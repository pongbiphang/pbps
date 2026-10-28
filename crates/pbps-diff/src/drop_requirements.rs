//! Requirements follow retired UIDs, including columns removed by a table drop.
use pbps_model::{Change, ChangeSet, DropGuard, IdsFile};

fn guards(
    changes: &ChangeSet,
    before: &IdsFile,
    after: &IdsFile,
) -> Result<Vec<Option<DropGuard>>, String> {
    if after.tombstones.values().all(|t| t.require.is_none()) {
        return Ok(vec![None; changes.changes.len()]);
    }
    use pbps_model::plan_names::{At, PlanNames};
    let names = PlanNames::of(&changes.changes, after).map_err(|e| e.to_string())?;
    let baseline = PlanNames::of(&[], before).map_err(|e| e.to_string())?;
    let mut guards = Vec::with_capacity(names.changes().len());
    for (index, step) in names.changes().iter().enumerate() {
        #[allow(clippy::wildcard_enum_match_arm)]
        let (uid, table_uid, detach_from, columns) = match &step.change {
            Change::DropColumn { uid, .. } => (
                uid,
                names
                    .column_at(uid, At::Before(index))
                    .map(|(owner, _)| owner),
                None,
                false,
            ),
            Change::DropTable {
                uid, detach_from, ..
            } => (uid, Some(uid), detach_from.clone(), true),
            _ => {
                guards.push(None);
                continue;
            }
        };
        let table_uid = table_uid.ok_or("the drop requirement has no owning table identity")?;
        let mut requirements = std::collections::BTreeMap::new();
        for (retired, tombstone) in &after.tombstones {
            let removed_child = columns
                && baseline
                    .column_at(retired, At::Start)
                    .is_some_and(|(owner, _)| owner == table_uid);
            if (retired == uid || removed_child)
                && let Some(requirement) = &tombstone.require
            {
                if &requirement.table_uid != table_uid {
                    return Err("the row requirement names a different table identity; record it again against the target".into());
                }
                requirements.insert(retired.clone(), requirement.clone());
            }
        }
        let guard = if requirements.is_empty() {
            None
        } else {
            let table = names
                .table_at(table_uid, At::Before(index))
                .ok_or("the drop requirement has no table holder at its operation")?;
            Some(DropGuard {
                table_uid: table_uid.clone(),
                table: table.clone(),
                detach_from,
                requirements,
            })
        };
        guards.push(guard);
    }
    Ok(guards)
}

pub fn assign(changes: &mut ChangeSet, before: &IdsFile, after: &IdsFile) -> Result<(), String> {
    let guards = guards(changes, before, after)?;
    for (step, guard) in changes.changes.iter_mut().zip(guards) {
        step.drop_guard = guard;
    }
    Ok(())
}

/// Recompute against the retained baseline: omitting a child guard from a
/// hand-written table drop cannot turn recorded intent into an unchecked drop.
pub fn validate(changes: &ChangeSet, before: &IdsFile, after: &IdsFile) -> Result<(), String> {
    for (step, guard) in changes.changes.iter().zip(guards(changes, before, after)?) {
        if step.drop_guard != guard {
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
    owner: usize,
    names: &pbps_model::plan_names::PlanNames<'_>,
    columns: &std::collections::BTreeSet<pbps_model::ColumnName>,
) -> Result<(), String> {
    use pbps_model::plan_names::At;
    let referenced: std::collections::BTreeSet<_> = names
        .columns_at(At::Start)
        .filter(|(uid, column)| {
            columns.contains(&column.name)
                && names
                    .column_at(uid, At::Start)
                    .is_some_and(|(owner, _)| owner == &guard.table_uid)
        })
        .map(|(uid, _)| uid)
        .collect();
    for (index, step) in names.changes().iter().enumerate() {
        #[allow(clippy::wildcard_enum_match_arm)]
        let uid = match &step.change {
            Change::RenameColumn { uid, .. }
            | Change::AlterColumnType { uid, .. }
            | Change::AlterColumnExpression { uid, .. } => Some(uid),
            Change::DropColumn { uid, .. } if matches!(&names.changes()[owner].change, Change::DropColumn { uid: own, .. } if own != uid) => {
                Some(uid)
            }
            _ => None,
        };
        if uid.is_some_and(|uid| {
            referenced.contains(uid)
                && names
                    .column_at(uid, At::Before(index))
                    .is_some_and(|(owner, _)| owner == &guard.table_uid)
        }) {
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
                        table_uid: "t_000001".parse().unwrap(),
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
        assign(&mut changes, &before, &after).unwrap();
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
        let requirement = after
            .tombstones
            .get_mut(&c)
            .unwrap()
            .require
            .as_mut()
            .unwrap();
        requirement.table_uid = "t_000002".parse().unwrap();
        assert!(
            assign(&mut changes, &before, &after)
                .unwrap_err()
                .contains("different table identity")
        );
        after
            .tombstones
            .get_mut(&c)
            .unwrap()
            .require
            .as_mut()
            .unwrap()
            .table_uid = "t_000001".parse().unwrap();
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
        assign(&mut changes, &before, &after).unwrap();
        assert!(changes.changes[0].drop_guard.is_none());
        validate(&changes, &before, &after).unwrap();
    }
}
