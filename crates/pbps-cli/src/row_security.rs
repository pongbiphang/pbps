//! The create-only policy contract; standing mutations arrive with #1173.

use pbps_model::IdsFile;
#[cfg(test)]
use pbps_model::{Change, ChangeSet, Uid};

pub(crate) use pbps_diff::order::row_security::{PolicyDependents, column_edit, policy_target_uid};

#[cfg(test)]
pub(crate) fn policy_releases(
    changes: &ChangeSet,
    ids: &IdsFile,
    found: &PolicyDependents,
) -> anyhow::Result<Vec<pbps_diff::order::row_security::PolicyRelease>> {
    Ok(pbps_diff::order::row_security::policy_releases(
        changes, ids, found,
    )?)
}

pub(crate) fn check_policy_releases<D: ?Sized>(
    plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
    ids: &IdsFile,
    found: &PolicyDependents,
) -> anyhow::Result<()> {
    let changes = pbps_dialect::PlanPayload::changes(plan);
    Ok(pbps_diff::order::row_security::check_releases(
        changes, ids, found,
    )?)
}

/// Test observations use the same create-only contract as admission.
#[cfg(test)]
pub(crate) fn validate(
    changes: &ChangeSet,
    ids: &IdsFile,
    dialect: &dyn pbps_dialect::Dialect,
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
    use pbps_diff::order::row_security::check_releases as check_policy_releases;
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
        use pbps_diff::order::{ArtifactView, admit};
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
        let checksum = wire.wire_fingerprint();
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
            let checksum = incomplete.wire_fingerprint();
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
        let mut dropping_created = good.clone();
        if let Change::CreatePolicy {
            table,
            name,
            policy,
            ..
        } = &good.changes[1].change
        {
            dropping_created
                .changes
                .push(PlannedChange::new(Change::DropPolicy {
                    table: table.clone(),
                    name: name.clone(),
                    policy: policy.clone(),
                }));
        }
        assert!(
            validate(&dropping_created, &ids, &pbps_pg::Postgres::new())
                .unwrap_err()
                .to_string()
                .contains("requires a standing table")
        );
        let mut blank = good.clone();
        blank.changes.truncate(1);
        assert!(
            validate(&blank, &ids, &pbps_pg::Postgres::new())
                .unwrap_err()
                .to_string()
                .contains("missing explicit operations")
        );
    }

    #[test]
    fn forged_policy_origins_are_refused_in_both_directions_by_table_identity() {
        let pg = pbps_pg::Postgres::new();
        let (created, ids) = fixture();
        for at in [1, 2] {
            let mut forged = created.clone();
            let (Change::CreatePolicy { origin, .. } | Change::SetRowSecurity { origin, .. }) =
                &mut forged.changes[at].change
            else {
                unreachable!("fixture contains only policy and flag steps");
            };
            *origin = RowSecurityOrigin::Standing;
            assert!(
                validate(&forged, &ids, &pg)
                    .unwrap_err()
                    .to_string()
                    .contains("forged row-security origin")
            );
        }
        let mut standing = created.clone();
        standing.changes.remove(0);
        for step in &mut standing.changes {
            let (Change::CreatePolicy { origin, .. } | Change::SetRowSecurity { origin, .. }) =
                &mut step.change
            else {
                unreachable!("fixture contains only policy and flag steps");
            };
            *origin = RowSecurityOrigin::Standing;
        }
        validate(&standing, &ids, &pg).unwrap();
        for at in [0, 1] {
            let mut forged = standing.clone();
            let (Change::CreatePolicy { origin, .. } | Change::SetRowSecurity { origin, .. }) =
                &mut forged.changes[at].change
            else {
                unreachable!("fixture contains only policy and flag steps");
            };
            *origin = RowSecurityOrigin::Created;
            assert!(
                validate(&forged, &ids, &pg)
                    .unwrap_err()
                    .to_string()
                    .contains("forged row-security origin")
            );
        }
        if let Change::SetRowSecurity { uid, .. } = &mut standing.changes[1].change {
            *uid = "t_117301".parse().unwrap();
        }
        assert!(validate(&standing, &ids, &pg).is_err());
    }

    #[test]
    fn policy_origins_follow_renamed_owners_and_not_reused_table_spellings() {
        let (mut changes, mut ids) = fixture();
        let uid = ids.tables.keys().next().unwrap().clone();
        let old = ids.tables[&uid].clone();
        let renamed = TableName::new("app", "renamed");
        changes.changes.insert(
            1,
            PlannedChange::new(Change::RenameTable {
                uid: uid.clone(),
                from: old.clone(),
                to: renamed.clone(),
                defaults: vec![],
            }),
        );
        for step in &mut changes.changes[2..] {
            let (Change::CreatePolicy { table, .. } | Change::SetRowSecurity { table, .. }) =
                &mut step.change
            else {
                unreachable!("fixture contains only policy and flag steps");
            };
            *table = renamed.clone();
        }
        ids.tables.insert(uid, renamed);
        validate(&changes, &ids, &pbps_pg::Postgres::new()).unwrap();
        let standing_uid: Uid = "t_117301".parse().unwrap();
        let before = TableName::new("app", "standing");
        ids.tables.insert(standing_uid.clone(), old.clone());
        changes.changes.insert(
            2,
            PlannedChange::new(Change::RenameTable {
                uid: standing_uid,
                from: before,
                to: old.clone(),
                defaults: vec![],
            }),
        );
        let mut forged = changes.changes[3].clone();
        if let Change::CreatePolicy { table, .. } = &mut forged.change {
            *table = old;
        }
        changes.changes.push(forged);
        assert!(
            validate(&changes, &ids, &pbps_pg::Postgres::new())
                .unwrap_err()
                .to_string()
                .contains("forged row-security origin")
        );
    }

    #[test]
    fn both_saved_admission_paths_rederive_standing_and_replacement_risks() {
        use pbps_model::{PlanBaseline, PlanOrigin, RiskClass, SavedPlan};
        let pg = pbps_pg::Postgres::new();
        let (mut changes, ids) = fixture();
        changes.changes.remove(0);
        for step in &mut changes.changes {
            let (Change::CreatePolicy { origin, .. } | Change::SetRowSecurity { origin, .. }) =
                &mut step.change
            else {
                unreachable!("fixture contains only policy and flag steps");
            };
            *origin = RowSecurityOrigin::Standing;
        }
        let Change::CreatePolicy {
            table,
            name,
            policy,
            ..
        } = &changes.changes[0].change
        else {
            unreachable!("fixture starts with a policy creation");
        };
        let (table, name, policy) = (table.clone(), name.clone(), policy.clone());
        changes.changes.insert(
            0,
            PlannedChange::new(Change::DropPolicy {
                table,
                name,
                policy,
            }),
        );
        let risks = pbps_diff::order::generated::risks(&changes, &ids, &pg);
        for (step, risks) in changes.changes.iter_mut().zip(risks) {
            step.risks = risks;
        }
        let plan = SavedPlan::new(
            PlanOrigin::Database,
            "postgres",
            "2026-10-22T00:00:00Z",
            PlanBaseline {
                description: "test".into(),
                checksum: "0".repeat(64),
                time_zone: None,
                database_collation: None,
            },
            changes,
            ids,
        );
        crate::validate_saved_plan(&plan, &pg).unwrap();
        pbps_diff::order::validate_saved_plan(&plan, &pg).unwrap();
        for at in 0..3 {
            let mut bad = plan.clone();
            bad.changes.changes[at].risks.insert(RiskClass::Destructive);
            assert!(
                crate::validate_saved_plan(&bad, &pg)
                    .unwrap_err()
                    .to_string()
                    .contains("inconsistent risks")
            );
            assert!(
                pbps_diff::order::validate_saved_plan(&bad, &pg)
                    .unwrap_err()
                    .to_string()
                    .contains("inconsistent risks")
            );
        }
        for risk in [RiskClass::Revoke, RiskClass::RowSecurity] {
            let mut bad = plan.clone();
            bad.changes.changes[0].risks.remove(&risk);
            assert!(
                crate::validate_saved_plan(&bad, &pg)
                    .unwrap_err()
                    .to_string()
                    .contains("inconsistent risks")
            );
            assert!(
                pbps_diff::order::validate_saved_plan(&bad, &pg)
                    .unwrap_err()
                    .to_string()
                    .contains("inconsistent risks")
            );
        }
    }

    #[test]
    fn standing_policy_delivery_refuses_while_created_policy_sql_is_unchanged() {
        use pbps_dialect::PlanDialect as _;
        use pbps_diff::order::singleton;
        use pbps_model::PlanMode;
        let pg = pbps_pg::Postgres::new();
        let (created, _) = fixture();
        for step in &created.changes[1..] {
            let created_plan = singleton(step.clone(), PlanMode::Transactional, &pg);
            assert!(!pg.emit(&created_plan, 0).unwrap().is_empty());
            let mut standing = step.change.clone();
            let (Change::CreatePolicy { origin, .. } | Change::SetRowSecurity { origin, .. }) =
                &mut standing
            else {
                unreachable!("fixture contains only policy and flag steps");
            };
            *origin = RowSecurityOrigin::Standing;
            let pg_standing = singleton(
                PlannedChange::new(standing.clone()),
                PlanMode::Transactional,
                &pg,
            );
            let mssql_standing = singleton(
                PlannedChange::new(standing),
                PlanMode::Transactional,
                &pbps_mssql::Mssql,
            );
            assert!(
                pg.emit(&pg_standing, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("arrives with later PRs of #1173")
            );
            assert!(
                pbps_mssql::Mssql
                    .emit(&mssql_standing, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("PostgreSQL row-security")
            );
        }
        let drop = Change::DropPolicy {
            table: TableName::new("app", "docs"),
            name: "p".into(),
            policy: RowPolicy::default(),
        };
        let pg_drop = singleton(
            PlannedChange::new(drop.clone()),
            PlanMode::Transactional,
            &pg,
        );
        let mssql_drop = singleton(
            PlannedChange::new(drop),
            PlanMode::Transactional,
            &pbps_mssql::Mssql,
        );
        assert!(
            pg.emit(&pg_drop, 0)
                .unwrap_err()
                .to_string()
                .contains("arrives with later PRs of #1173")
        );
        assert!(pbps_mssql::Mssql.emit(&mssql_drop, 0).is_err());
    }
}

#[cfg(test)]
mod live_tests;
