//! The comparison schema a restoration is diffed against.

use super::*;

fn table_by_uid<'a>(side: Side<'a>, uid: &Uid) -> Option<&'a Table> {
    side.schema.tables.get(side.ids.tables.get(uid)?)
}

fn ordinary_default<'a>(side: Side<'a>, uid: &Uid) -> Option<Option<&'a String>> {
    let at = side.ids.columns.get(uid)?;
    let column = side.schema.tables.get(&at.table)?.columns.get(&at.name)?;
    Some(match &column.value_source {
        ValueSource::Default(text) => Some(text),
        ValueSource::None
        | ValueSource::Identity(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => None,
    })
}

/// Compare logical restorations through the differ's own decisions (SPEC §8.2).
/// Parameter-only index edits do not replace a part; physical dependent rebuilds
/// must not turn them into new interface restrictions. Prepare once per plan.
pub fn restoration_comparison<'a>(
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &'a (impl Dialect + ?Sized),
) -> impl Fn(&Change, Option<&Uid>) -> bool + 'a {
    let renames = renames_of(base, declared);
    let changing_hands = names_changing_hands(base, declared);
    let index = Indexes {
        base: base.ids.index(),
        declared: declared.ids.index(),
    };
    let mut changed = BTreeSet::new();
    let mut partition_defaults = BTreeSet::new();
    for (uid, old) in &base.ids.tables {
        let Some(new) = declared.ids.tables.get(uid) else {
            continue;
        };
        let (Some(before), Some(after)) = (table_by_uid(base, uid), table_by_uid(declared, uid))
        else {
            continue;
        };
        let mut edits = Vec::new();
        let renamed = renames.apply(before, old);
        // A refusal is the plan's to report; this comparison only asks which
        // parts a plan would edit, so it has nowhere to put one.
        diff_constraints(new, &renamed, after, &mut edits, &mut Vec::new());
        if let (Some(was), Some(now)) = (&before.partition_of, &after.partition_of) {
            let was_columns =
                partition_columns_by_uid(&was.columns, &was.parent, &now.parent, &index);
            for column in declared
                .schema
                .tables
                .get(&now.parent)
                .into_iter()
                .flat_map(|t| t.columns.keys())
            {
                let existed = declared
                    .ids
                    .column_uid(&now.parent.column(column.clone()))
                    .is_some_and(|id| base.ids.columns.contains_key(id));
                if existed
                    && !defaults_differ(
                        was_columns.get(column).and_then(|c| c.default.as_ref()),
                        now.columns.get(column).and_then(|c| c.default.as_ref()),
                    )
                {
                    partition_defaults.insert((uid.clone(), column.clone()));
                }
            }
        }
        diff_computed(new, before, after, &mut edits);
        for edit in edits {
            if let Some(part) = edit.constraints() {
                changed.insert((uid.clone(), part.part(), part.name.map(str::to_owned)));
            }
        }
    }
    move |change: &Change, uid: Option<&Uid>| {
        if let Change::Column(ColumnChange::AlterColumnDefault { uid, .. }) = change {
            return ordinary_default(base, uid)
                .zip(ordinary_default(declared, uid))
                .is_some_and(|(b, d)| !defaults_differ(b, d));
        }
        if let Change::Partition(PartitionChange::SetPartitionDefault { column, .. }) = change {
            return uid.is_some_and(|u| partition_defaults.contains(&(u.clone(), column.clone())));
        }
        if let Some(id) = change.module_id() {
            return !trigger_changes_hands(id, &changing_hands)
                && base
                    .schema
                    .modules
                    .get(id)
                    .zip(declared.schema.modules.get(id))
                    .is_some_and(|(b, d)| module_delta(b, d, dialect) == ModuleDelta::Unchanged);
        }
        let Some(part) = change.constraints() else {
            return false;
        };
        let Some(uid) = uid else { return false };
        let (Some(before), Some(after)) = (table_by_uid(base, uid), table_by_uid(declared, uid))
        else {
            return false;
        };
        let present = |t: &Table| match part.part() {
            pbps_model::Part::PrimaryKey => t.primary_key.is_some(),
            pbps_model::Part::Computed => part.name.is_some_and(|n| t.computed.contains_key(n)),
            pbps_model::Part::Unique => part.name.is_some_and(|n| t.unique.contains_key(n)),
            pbps_model::Part::Check => part.name.is_some_and(|n| t.checks.contains_key(n)),
            pbps_model::Part::ForeignKey => {
                part.name.is_some_and(|n| t.foreign_keys.contains_key(n))
            }
            pbps_model::Part::Index => part.name.is_some_and(|n| t.indexes.contains_key(n)),
            pbps_model::Part::Exclusion => part.name.is_some_and(|n| t.exclude.contains_key(n)),
            // RLS policy changes have no consumer restoration exemption.
            pbps_model::Part::Policy => false,
        };
        present(before)
            && present(after)
            && !changed.contains(&(uid.clone(), part.part(), part.name.map(str::to_owned)))
    }
}
