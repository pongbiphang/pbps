//! Numbering sources (identities and sequences) and the order they release in.

use super::*;

/// The columns a plan puts an identity on that had none (#1889).
pub(super) fn identities_put_on<'a>(
    changes: impl Iterator<Item = &'a Change>,
) -> BTreeSet<ColumnRef> {
    changes
        .filter_map(|c| {
            if let Change::Column(ColumnChange::AlterColumnNumbering {
                column,
                from: ValueSource::None | ValueSource::Default(_) | ValueSource::Sequence(_),
                to: ValueSource::Identity(_),
                ..
            }) = c
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Renames, from and to, that wait on each other's names.
type RenameCycle = Vec<(TableName, TableName)>;

/// The owned sequences a plan gives up, renames, or turns into an identity
/// on a column it neither tightens nor retypes, each with its place in the
/// chain of names they free: zero for one that takes no name another frees,
/// and one more than the change freeing the name it takes otherwise. A cycle
/// of renames has no order in which each name is free when taken, so each
/// cycle is returned too, as its renames, to be refused by name (#1930
/// review).
pub(super) fn sequence_release_depths<'a>(
    changes: impl Iterator<Item = &'a Change>,
    held_back: &BTreeSet<ColumnRef>,
) -> (BTreeMap<ColumnRef, usize>, BTreeSet<RenameCycle>) {
    let mut released: BTreeMap<(String, String), ColumnRef> = BTreeMap::new();
    let mut claims: BTreeMap<ColumnRef, Option<(String, String)>> = BTreeMap::new();
    let mut renamed: BTreeMap<ColumnRef, (TableName, TableName)> = BTreeMap::new();
    for c in changes {
        let Change::Column(ColumnChange::AlterColumnNumbering {
            column,
            from: ValueSource::Sequence(was),
            to,
            ..
        }) = c
        else {
            continue;
        };
        let at = |name: &String| (column.table.schema.clone(), name.clone());
        let claim = match to {
            ValueSource::None => None,
            ValueSource::Sequence(now)
                if was.name.is_some() && now.name.is_some() && was.name != now.name =>
            {
                now.name.as_ref().map(at)
            }
            ValueSource::Identity(_) if !held_back.contains(column) => None,
            ValueSource::Sequence(_)
            | ValueSource::Identity(_)
            | ValueSource::Default(_)
            | ValueSource::Generated(_) => continue,
        };
        if let Some(name) = &was.name {
            released.insert(at(name), column.clone());
            if let Some((schema, to)) = &claim {
                renamed.insert(
                    column.clone(),
                    (TableName::new(schema, name), TableName::new(schema, to)),
                );
            }
        }
        claims.insert(column.clone(), claim);
    }
    let mut depths = BTreeMap::new();
    let mut cycles = BTreeSet::new();
    for start in claims.keys() {
        let mut depth = 0;
        let mut path = vec![start];
        let mut at = start;
        while let Some(Some(name)) = claims.get(at)
            && let Some(next) = released.get(name)
        {
            if let Some(from) = path.iter().position(|seen| *seen == next) {
                // Back to a member already walked: the renames from there
                // on wait on each other. Sorted, so each cycle is one entry
                // whichever member the walk started from.
                let mut cycle: Vec<_> = path[from..]
                    .iter()
                    .filter_map(|c| renamed.get(*c).cloned())
                    .collect();
                cycle.sort();
                cycles.insert(cycle);
                break;
            }
            depth += 1;
            path.push(next);
            at = next;
        }
        depths.insert(start.clone(), depth);
    }
    (depths, cycles)
}

/// The columns a plan puts a numbering source on, an identity or an owned
/// sequence they were not numbered by (#1889).
pub(super) fn numbering_put_on<'a>(
    changes: impl Iterator<Item = &'a Change>,
) -> BTreeSet<ColumnRef> {
    changes
        .filter_map(|c| {
            if let Change::Column(ColumnChange::AlterColumnNumbering {
                column, from, to, ..
            }) = c
                && pbps_model::NumberingKind::installed(from, to)
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect()
}

/// The tables a plan puts a numbering source on and both deletes and
/// inserts rows of: the deletes may remove the rows holding NULL, or the
/// greatest value, which must go before the source is put on, and the
/// inserts leave the numbered column out, so they follow it (#1930 review).
pub(super) fn numbering_after_deletes<'a>(
    changes: impl Iterator<Item = &'a Change> + Clone,
) -> BTreeSet<TableName> {
    let inserted = tables_with_inserts(changes.clone());
    let deleted: BTreeSet<TableName> = changes
        .clone()
        .filter_map(|c| {
            if let Change::Row(RowChange::DeleteRow { table, .. }) = c {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect();
    numbering_put_on(changes)
        .into_iter()
        .map(|column| column.table)
        .filter(|t| inserted.contains(t) && deleted.contains(t))
        .collect()
}

/// The rows of another table that reference one whose inserts follow an
/// identity put on it, at the end of the deletes' class: they are written in
/// class 11, before the rows they may reference exist, and the foreign key
/// refuses them. Refused by name; the row changes go in a plan of their own
/// first (#1930 review).
pub(super) fn refuse_rows_referencing_late_inserts(
    declared: Side<'_>,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let late = numbering_after_deletes(changes.iter());
    if late.is_empty() {
        return;
    }
    // A key the plan adds, a created table's among them, is added in class
    // 13, after every row: nothing enforces it while they are written
    // (#1930 review).
    let added: BTreeSet<(&TableName, &str)> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Constraint(ConstraintChange::AddForeignKey { table, name, .. }) = c {
                Some((table, name.raw()))
            } else {
                None
            }
        })
        .collect();
    let mut seen = BTreeSet::new();
    for change in changes {
        let (Change::Row(RowChange::InsertRow { table, .. })
        | Change::Row(RowChange::UpdateRow { table, .. })) = change
        else {
            continue;
        };
        let Some(t) = declared.schema.tables.get(table) else {
            continue;
        };
        for (name, fk) in &t.foreign_keys {
            if added.contains(&(table, name.raw())) {
                continue;
            }
            if fk.references_table != *table
                && late.contains(&fk.references_table)
                && seen.insert((table.clone(), fk.references_table.clone()))
            {
                errs.push(DiffError::RowsReferenceLateIdentityRows {
                    table: table.clone(),
                    referenced: fk.references_table.clone(),
                });
            }
        }
    }
}
