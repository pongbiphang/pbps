//! The create-only policy contract; standing mutations arrive with #1173.

use anyhow::bail;
use pbps_model::{
    Change, ChangeSet, IdsFile, Uid,
    plan_names::{At, PlanNames},
};
use std::collections::BTreeMap;

/// Exact policy dependents of edited columns and dropped or renamed tables.
/// An absent entry is unread; an empty vector records a successful empty read.
pub(crate) type PolicyDependents = BTreeMap<Uid, Vec<pbps_pg::modules::PolicyDependent>>;

/// Destructive column edits shared with engine probes. Rename dependencies
/// are read separately, without invoking drop/retype-only probes.
pub(crate) fn column_edit(change: &Change) -> Option<(&Uid, &pbps_model::ColumnRef, &'static str)> {
    if let Change::DropColumn { uid, column } = change {
        Some((uid, column, "dropped"))
    } else if let Change::AlterColumnType { uid, column, .. } = change {
        Some((uid, column, "retyped"))
    } else {
        None
    }
}

pub(crate) fn policy_target_uid(change: &Change) -> Option<&Uid> {
    if let Change::DropTable { uid, .. }
    | Change::RenameTable { uid, .. }
    | Change::RenameColumn { uid, .. } = change
    {
        Some(uid)
    } else {
        column_edit(change).map(|(uid, _, _)| uid)
    }
}

pub(crate) use pbps_diff::order::row_security::PolicyRelease;

pub(crate) fn policy_releases(
    changes: &ChangeSet,
    ids: &IdsFile,
    found: &PolicyDependents,
) -> anyhow::Result<Vec<PolicyRelease>> {
    let names = PlanNames::of(&changes.changes, ids)?;
    let mut edges = Vec::new();
    for (at, step) in changes.changes.iter().enumerate() {
        let Some(uid) = policy_target_uid(&step.change) else {
            continue;
        };
        let policies = found.get(uid).ok_or_else(|| {
            anyhow::anyhow!("policy dependents of {uid} were not read from PostgreSQL")
        })?;
        if policies.is_empty() {
            continue;
        }
        let (subject, what, dropped_table) = if matches!(
            &step.change,
            Change::DropTable { .. } | Change::RenameTable { .. }
        ) {
            let table = names.table_at(uid, At::Start).ok_or_else(|| {
                anyhow::anyhow!("policy dependent table {uid} has no opening catalog identity")
            })?;
            if matches!(&step.change, Change::DropTable { .. }) {
                (table.shown().to_string(), "dropped", Some(table))
            } else {
                (table.shown().to_string(), "renamed", None)
            }
        } else {
            let what = if matches!(&step.change, Change::RenameColumn { .. }) {
                "renamed"
            } else {
                column_edit(&step.change)
                    .expect("a policy target is a table or column")
                    .2
            };
            let (owner, name) = names.column_at(uid, At::Start).ok_or_else(|| {
                anyhow::anyhow!("policy dependent column {uid} has no opening catalog identity")
            })?;
            let table = names.table_at(owner, At::Start).ok_or_else(|| {
                anyhow::anyhow!("policy dependent column {uid} has no opening table identity")
            })?;
            (table.column(name).shown().to_string(), what, None)
        };
        for policy in policies {
            // DROP TABLE takes its own policies automatically, including
            // normal attribute references in its own predicates.
            if dropped_table == Some(&policy.table) {
                continue;
            }
            let refusal = format!(
                "{} cannot be {what}: policy `{}` on {} depends on it; planning this policy change arrives with #1173",
                subject,
                pbps_model::escape::Escaped(&policy.name),
                policy.table.shown()
            );
            // Compare opening UID identities, not a later holder of the same
            // spelling. An individual standing policy is never removed here.
            let Some(before) = changes.changes.iter().position(|p| {
                if let Change::DropTable { uid, .. } = &p.change {
                    names.table_at(uid, At::Start) == Some(&policy.table)
                } else {
                    false
                }
            }) else {
                bail!("{refusal}");
            };
            edges.push(PolicyRelease {
                before,
                after: at,
                refusal,
            });
        }
    }
    Ok(edges)
}

pub(crate) fn check_policy_releases(
    changes: &ChangeSet,
    ids: &IdsFile,
    found: &PolicyDependents,
) -> anyhow::Result<()> {
    for release in policy_releases(changes, ids, found)? {
        if release.before >= release.after {
            bail!(
                "{}; its table drop must precede the dependent change. Plan again.",
                release.refusal
            );
        }
    }
    Ok(())
}

/// Admission and the transitional CLI consumer share the same RLS checks.
pub(crate) fn validate(
    changes: &ChangeSet,
    ids: &IdsFile,
    dialect: &dyn pbps_dialect::Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> anyhow::Result<()> {
    Ok(pbps_diff::order::row_security::validate(
        changes, ids, dialect,
    )?)
}

pub(crate) fn binding_notices(schema: &pbps_model::Schema) -> Vec<String> {
    let mut notices = Vec::new();
    for (table, shape) in &schema.tables {
        for (name, policy) in &shape.row_policies {
            if policy.using.is_some() || policy.with_check.is_some() {
                notices.push(format!(
                    "{} policy `{}`: predicate bindings are not recorded; binding verification arrives with #595",
                    table.shown(), pbps_model::escape::Escaped(name)
                ));
            }
        }
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{PlannedChange, RowPolicy, RowSecurity, RowSecurityOrigin, Table, TableName};

    fn fixture() -> (ChangeSet, IdsFile) {
        let uid: Uid = "t_117200".parse().unwrap();
        let table = TableName::new("app", "docs");
        let policy = RowPolicy {
            using: Some("true".into()),
            ..Default::default()
        };
        let flags = RowSecurity {
            enabled: true,
            forced: true,
        };
        let mut ids = IdsFile::default();
        ids.tables.insert(uid.clone(), table.clone());
        let changes = vec![
            Change::CreateTable {
                uid: uid.clone(),
                name: table.clone(),
                table: Box::new(Table {
                    row_security: flags,
                    row_policies: [("p".into(), policy.clone())].into(),
                    ..Default::default()
                }),
                beside_default: None,
                moves_at_most: None,
            },
            Change::CreatePolicy {
                table: table.clone(),
                name: "p".into(),
                policy,
                origin: RowSecurityOrigin::Created,
            },
            Change::SetRowSecurity {
                uid,
                table,
                from: Default::default(),
                to: flags,
                origin: RowSecurityOrigin::Created,
            },
        ];
        (
            ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            },
            ids,
        )
    }

    #[test]
    fn saved_admission_keeps_complete_rls_bytes_and_refuses_missing_policy_operations() {
        use pbps_diff::order::admit;
        use pbps_model::{PlanBaseline, PlanOrigin, SavedPlan};

        let (changes, ids) = fixture();
        let dialect = pbps_pg::Postgres::new();
        let wire = SavedPlan::new(
            PlanOrigin::Database,
            "postgres",
            "2026-10-22T00:00:00Z",
            PlanBaseline {
                description: "RLS admission fixture".into(),
                checksum: "baseline".into(),
                database_collation: None,
                time_zone: None,
            },
            changes,
            ids,
        );
        let path = std::path::Path::new("plan.json");
        let bytes = serde_json::to_vec(&wire).unwrap();
        let checksum = wire.checksum();
        let admitted = admit(
            serde_json::from_slice(&bytes).unwrap(),
            &dialect,
            path,
            &checksum,
        )
        .unwrap();
        assert_eq!(serde_json::to_vec(admitted.wire()).unwrap(), bytes);
        assert_eq!(admitted.checksum(), checksum);
        for missing in 0..3 {
            let mut incomplete = wire.clone();
            incomplete.changes.changes.remove(missing);
            let refused = validate(&incomplete.changes, &incomplete.ids, &dialect).unwrap_err();
            let expected = refused.to_string();
            assert_eq!(format!("{refused:#}"), expected);
            let checksum = incomplete.checksum();
            let refused = admit(incomplete, &dialect, path, &checksum).unwrap_err();
            assert_eq!(
                format!("{:#}", anyhow::Error::new(refused)),
                expected,
                "missing operation {missing}"
            );
        }
    }

    #[test]
    fn a_same_named_replacement_drop_does_not_release_an_original_policy() {
        let input = TableName::new("app", "input");
        let owner = TableName::new("app", "owner");
        let kept = TableName::new("app", "kept");
        let input_uid: Uid = "t_117200".parse().unwrap();
        let original: Uid = "t_117201".parse().unwrap();
        let replacement: Uid = "t_117202".parse().unwrap();
        let target: Uid = "c_117200".parse().unwrap();
        let mut ids = IdsFile::default();
        ids.tables.insert(input_uid, input.clone());
        ids.tables.insert(original.clone(), kept.clone());
        let mut plan = ChangeSet {
            changes: vec![
                Change::DropColumn {
                    uid: target.clone(),
                    column: input.column("target"),
                },
                Change::RenameTable {
                    uid: original.clone(),
                    from: owner.clone(),
                    to: kept.clone(),
                    defaults: vec![],
                },
                Change::CreateTable {
                    uid: replacement.clone(),
                    name: owner.clone(),
                    table: Box::default(),
                    beside_default: None,
                    moves_at_most: None,
                },
                Change::DropTable {
                    uid: replacement.clone(),
                    name: owner.clone(),
                    detach_from: None,
                },
            ]
            .into_iter()
            .map(PlannedChange::new)
            .collect(),
        };
        let found = [
            (
                target,
                vec![pbps_pg::modules::PolicyDependent {
                    table: owner,
                    name: "original_policy".into(),
                }],
            ),
            (replacement, vec![]),
            (original.clone(), vec![]),
        ]
        .into();
        let refused = policy_releases(&plan, &ids, &found)
            .err()
            .unwrap()
            .to_string();
        assert!(
            refused.contains("original_policy") && refused.contains("arrives with #1173"),
            "{refused}"
        );
        plan.changes.push(PlannedChange::new(Change::DropTable {
            uid: original.clone(),
            name: kept,
            detach_from: None,
        }));
        ids.tables.remove(&original);
        let edges = policy_releases(&plan, &ids, &found).unwrap();
        assert_eq!((edges[0].before, edges[0].after), (4, 0));
        assert!(check_policy_releases(&plan, &ids, &found).is_err());
    }

    #[test]
    fn rename_refusals_follow_opening_column_identity_and_exact_owner_release() {
        let before = TableName::new("app", "before");
        let after = TableName::new("app", "after");
        let owner = TableName::new("app", "owner");
        let table_uid: Uid = "t_117210".parse().unwrap();
        let column_uid: Uid = "c_117210".parse().unwrap();
        let owner_uid: Uid = "t_117211".parse().unwrap();
        let mut ids = IdsFile::default();
        ids.tables.insert(table_uid.clone(), after.clone());
        ids.tables.insert(owner_uid.clone(), owner.clone());
        ids.columns
            .insert(column_uid.clone(), after.column("renamed"));
        let mut plan = ChangeSet {
            changes: vec![
                Change::RenameTable {
                    uid: table_uid.clone(),
                    from: before,
                    to: after.clone(),
                    defaults: vec![],
                },
                Change::RenameColumn {
                    uid: column_uid.clone(),
                    table: after,
                    from: "original".into(),
                    to: "renamed".into(),
                    table_was: Some(TableName::new("app", "before")),
                },
            ]
            .into_iter()
            .map(PlannedChange::new)
            .collect(),
        };
        let mut found: PolicyDependents = [(table_uid, vec![])].into();
        assert!(
            policy_releases(&plan, &ids, &found)
                .err()
                .unwrap()
                .to_string()
                .contains("were not read")
        );
        found.insert(column_uid.clone(), vec![]);
        assert!(policy_releases(&plan, &ids, &found).unwrap().is_empty());
        found.insert(
            column_uid,
            vec![pbps_pg::modules::PolicyDependent {
                table: owner.clone(),
                name: "gate".into(),
            }],
        );
        let error = policy_releases(&plan, &ids, &found)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("app.before.original cannot be renamed")
                && error.contains("policy `gate`")
                && error.contains("arrives with #1173"),
            "{error}"
        );
        ids.tables.remove(&owner_uid);
        found.insert(owner_uid.clone(), vec![]);
        plan.changes.insert(
            0,
            PlannedChange::new(Change::DropTable {
                uid: owner_uid,
                name: owner,
                detach_from: None,
            }),
        );
        let edges = policy_releases(&plan, &ids, &found).unwrap();
        assert_eq!((edges[0].before, edges[0].after), (0, 2));
        check_policy_releases(&plan, &ids, &found).unwrap();
        plan.changes.rotate_left(1);
        assert!(
            check_policy_releases(&plan, &ids, &found)
                .unwrap_err()
                .to_string()
                .contains("its table drop must precede")
        );
    }

    #[test]
    fn complete_policy_creation_is_accepted_and_forged_origins_are_refused() {
        let (good, ids) = fixture();
        validate(&good, &ids, &pbps_pg::Postgres::new()).unwrap();
        let mut wrong_ids = ids.clone();
        wrong_ids
            .tables
            .values_mut()
            .for_each(|name| *name = TableName::new("app", "wrong"));
        assert!(validate(&good, &wrong_ids, &pbps_pg::Postgres::new()).is_err());
        assert!(validate(&good, &ids, &pbps_mssql::Mssql).is_err());
        for bad in 0..9 {
            let mut changes = good.clone();
            match bad {
                0 => {
                    changes.changes.remove(0);
                }
                1 => {
                    changes.changes.remove(1);
                }
                2 => {
                    changes.changes.remove(2);
                }
                3 => changes.changes.swap(0, 1),
                4 => changes.changes.swap(1, 2),
                5 => changes.changes.push(changes.changes[1].clone()),
                6 => {
                    if let Change::CreatePolicy { name, .. } = &mut changes.changes[1].change {
                        *name = "extra".into();
                    }
                }
                7 => {
                    if let Change::SetRowSecurity { uid, .. } = &mut changes.changes[2].change {
                        *uid = "t_117201".parse().unwrap();
                    }
                }
                _ => {
                    if let Change::SetRowSecurity { from, .. } = &mut changes.changes[2].change {
                        from.enabled = true;
                    }
                }
            }
            assert!(
                validate(&changes, &ids, &pbps_pg::Postgres::new()).is_err(),
                "case {bad}"
            );
        }
        let mut blank = good.clone();
        blank.changes.truncate(1);
        assert!(
            validate(&blank, &ids, &pbps_pg::Postgres::new())
                .unwrap_err()
                .to_string()
                .contains("missing explicit operations")
        );
    }
}

#[cfg(test)]
mod live_tests;
