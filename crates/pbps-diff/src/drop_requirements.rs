//! Requirements follow retired UIDs, including columns removed by a table drop.
use pbps_model::{Change, ChangeSet, DropGuard, IdsFile};

pub fn for_change(change: &Change, before: &IdsFile, after: &IdsFile) -> Option<DropGuard> {
    #[allow(clippy::wildcard_enum_match_arm)]
    let (uid, table, detach_from, columns) = match change {
        Change::DropColumn { uid, column } => (uid, &column.table, None, false),
        Change::DropTable {
            uid,
            name,
            detach_from,
        } => (uid, name, detach_from.clone(), true),
        _ => return None,
    };
    let owner = before.tables.get(uid).unwrap_or(table);
    let mut requirements = std::collections::BTreeMap::new();
    for (retired, tombstone) in &after.tombstones {
        if (retired == uid
            || (columns
                && before
                    .columns
                    .get(retired)
                    .is_some_and(|c| &c.table == owner)))
            && let Some(requirement) = &tombstone.require
        {
            requirements.insert(retired.clone(), requirement.clone());
        }
    }
    (!requirements.is_empty()).then(|| DropGuard {
        table: table.clone(),
        detach_from,
        requirements,
    })
}

pub fn assign(changes: &mut ChangeSet, before: &IdsFile, after: &IdsFile) {
    for step in &mut changes.changes {
        step.drop_guard = for_change(&step.change, before, after);
    }
}

/// Recompute against the retained baseline: omitting a child guard from a
/// hand-written table drop cannot turn recorded intent into an unchecked drop.
pub fn validate(changes: &ChangeSet, before: &IdsFile, after: &IdsFile) -> Result<(), String> {
    for step in &changes.changes {
        if step.drop_guard != for_change(&step.change, before, after) {
            return Err("the drop requirements disagree with the recorded identities; recompute and review the plan".into());
        }
    }
    for rebuild in after.rebuilds.values() {
        if rebuild
            .source
            .tables
            .keys()
            .chain(rebuild.source.columns.keys())
            .any(|uid| {
                (before.tables.contains_key(uid) || before.columns.contains_key(uid))
                    && after
                        .tombstones
                        .get(uid)
                        .is_some_and(|t| t.require.is_some())
            })
        {
            return Err(
                "row requirements on rebuild-source identities are unsupported; split the plan"
                    .into(),
            );
        }
    }
    Ok(())
}

pub fn check_columns(
    guard: &DropGuard,
    owner: &Change,
    changes: &[pbps_model::PlannedChange],
    columns: &std::collections::BTreeSet<pbps_model::ColumnName>,
) -> Result<(), String> {
    for step in changes {
        #[allow(clippy::wildcard_enum_match_arm)]
        let changed = match &step.change {
            Change::RenameColumn {
                table, from, to, ..
            } if table == &guard.table => columns.contains(from) || columns.contains(to),
            Change::AlterColumnType { column, .. }
            | Change::AlterColumnExpression { column, .. }
                if column.table == guard.table =>
            {
                columns.contains(&column.name)
            }
            Change::DropColumn { uid, column }
                if column.table == guard.table
                    && matches!(owner, Change::DropColumn { uid: own, .. } if own != uid) =>
            {
                columns.contains(&column.name)
            }
            _ => false,
        };
        if changed {
            return Err(format!(
                "the row requirement of {} names a column changed by this plan; split the plan",
                guard.table.shown()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{DropRequirement, PlannedChange, Tombstone};

    #[test]
    fn a_table_drop_keeps_every_removed_column_requirement_by_uid() {
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        let t: pbps_model::Uid = "t_000001".parse().unwrap();
        let c: pbps_model::Uid = "c_000001".parse().unwrap();
        let mut before = IdsFile::default();
        before.tables.insert(t.clone(), table.clone());
        before.columns.insert(c.clone(), table.column("old"));
        let mut after = IdsFile::default();
        for (uid, was) in [(t.clone(), "app.t"), (c.clone(), "app.t.old")] {
            after.tombstones.insert(
                uid,
                Tombstone {
                    was: was.into(),
                    dropped_at: "2026-10-28".into(),
                    reason: "copied".into(),
                    operator: "reviewer".into(),
                    require: Some(DropRequirement {
                        predicate: "old IS NULL".into(),
                        required_at: "2026-10-28".into(),
                        operator: "reviewer".into(),
                    }),
                },
            );
        }
        let mut changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::DropTable {
                uid: t,
                name: table,
                detach_from: None,
            })],
        };
        assign(&mut changes, &before, &after);
        assert_eq!(
            changes.changes[0]
                .drop_guard
                .as_ref()
                .unwrap()
                .requirements
                .len(),
            2
        );
        validate(&changes, &before, &after).unwrap();
        changes.changes[0]
            .drop_guard
            .as_mut()
            .unwrap()
            .requirements
            .remove(&c);
        assert!(
            validate(&changes, &before, &after)
                .unwrap_err()
                .contains("disagree")
        );
        for tombstone in after.tombstones.values_mut() {
            tombstone.require = None;
        }
        assign(&mut changes, &before, &after);
        assert!(changes.changes[0].drop_guard.is_none());
        validate(&changes, &before, &after).unwrap();
    }
}
