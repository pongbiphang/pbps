//! The create-only policy contract; standing mutations arrive with #1173.

use anyhow::bail;
use pbps_model::{
    Change, ChangeSet, IdsFile, TableName, Uid,
    plan_names::{At, Holder, PlanNames},
};
use std::collections::{BTreeMap, BTreeSet};

/// The exact policy dependents read for each edited column or dropped table. An absent entry
/// is unread, while an empty vector records a successful empty catalog read.
pub(crate) type PolicyDependents = BTreeMap<Uid, Vec<pbps_pg::modules::PolicyDependent>>;

/// The column changes whose old catalog bindings a policy can hold.
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
    if let Change::DropTable { uid, .. } = change {
        Some(uid)
    } else {
        column_edit(change).map(|(uid, _, _)| uid)
    }
}

/// A whole-table removal releases this policy before a referenced column edit.
/// Keeping the named refusal beside the edge lets apply check the saved order
/// without repairing or extending the approved plan.
pub(crate) struct PolicyRelease {
    pub before: usize,
    pub after: usize,
    pub refusal: String,
}

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
        let (subject, what, dropped_table) = if let Change::DropTable { .. } = &step.change {
            let table = names.table_at(uid, At::Start).ok_or_else(|| {
                anyhow::anyhow!("policy dependent table {uid} has no opening catalog identity")
            })?;
            (table.shown().to_string(), "dropped", Some(table))
        } else {
            let (_, _, what) =
                column_edit(&step.change).expect("a policy target is a table or column");
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

/// A checksum authenticates the bytes, not a forged creation origin or an
/// ignored policy hidden inside CREATE TABLE's descriptive payload.
pub(crate) fn validate(
    changes: &ChangeSet,
    ids: &IdsFile,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<()> {
    if !changes.changes.iter().any(|step| {
        matches!(
            &step.change,
            Change::CreatePolicy { .. } | Change::SetRowSecurity { .. }
        ) || if let Change::CreateTable { table, .. } = &step.change {
            table.has_row_security()
        } else if let Change::AttachPartition { shape, .. }
        | Change::DetachPartition { shape, .. } = &step.change
        {
            shape.has_row_security()
        } else {
            false
        }
    }) {
        return Ok(());
    }
    let mut created = BTreeMap::new();
    for (at, step) in changes.changes.iter().enumerate() {
        if let Change::CreateTable {
            uid, name, table, ..
        } = &step.change
        {
            if table.has_row_security() {
                if dialect.name() != "postgres"
                    || table.partition_by.is_some()
                    || table.partition_of.is_some()
                {
                    bail!(
                        "{}: row-security policies and flags require an ordinary PostgreSQL table",
                        name.shown()
                    );
                }
                if let Some(problem) = dialect.validate_table(name, table).into_iter().next() {
                    bail!("{}: {problem}", name.shown());
                }
                if table.data.is_some() {
                    bail!(
                        "{}: data cannot be declared with row-security flags or policies",
                        name.shown()
                    );
                }
            }
            if created.insert(uid, (at, name, table)).is_some() {
                bail!(
                    "{}: duplicate table creation cannot establish a policy origin",
                    name.shown()
                );
            }
        } else if let Change::AttachPartition { table, shape, .. }
        | Change::DetachPartition { table, shape, .. } = &step.change
            && shape.has_row_security()
        {
            bail!(
                "{}: row security on partition trees is unsupported",
                table.shown()
            );
        }
    }
    let names = PlanNames::of(&changes.changes, ids)?;
    let mut policies: BTreeMap<&Uid, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut flags = BTreeSet::new();
    let owner = |table: &TableName, at: usize| -> anyhow::Result<&Uid> {
        let Holder::Tracked(uid) = names.holder(table, At::Before(at)) else {
            bail!(
                "{}: created row-security origin has no preceding table creation",
                table.shown()
            );
        };
        let Some((start, _, _)) = created.get(uid) else {
            bail!(
                "{}: standing-table row-security planning arrives with #1173",
                table.shown()
            );
        };
        if *start >= at {
            bail!(
                "{}: row-security operation precedes its table creation",
                table.shown()
            );
        }
        Ok(uid)
    };
    for (at, step) in changes.changes.iter().enumerate() {
        if let Change::CreatePolicy {
            table,
            name,
            policy,
            ..
        } = &step.change
        {
            let uid = owner(table, at)?;
            let (_, _, shape) = created[uid];
            if dialect.name() != "postgres" || shape.row_policies.get(name) != Some(policy) {
                bail!(
                    "{} policy `{}`: created origin or payload does not match CREATE TABLE",
                    table.shown(),
                    pbps_model::escape::Escaped(name)
                );
            }
            let problems = policy.problems();
            if !problems.is_empty() {
                bail!(
                    "{} policy `{}`: {}",
                    table.shown(),
                    pbps_model::escape::Escaped(name),
                    problems.join("; ")
                );
            }
            if flags.contains(uid) || policies.entry(uid).or_default().insert(name, at).is_some() {
                bail!(
                    "{} policy `{}`: duplicate policy or policy after row-security flags",
                    table.shown(),
                    pbps_model::escape::Escaped(name)
                );
            }
        } else if let Change::SetRowSecurity {
            uid,
            table,
            from,
            to,
            ..
        } = &step.change
        {
            let actual = owner(table, at)?;
            let (_, _, shape) = created[actual];
            if dialect.name() != "postgres"
                || actual != uid
                || !from.is_default()
                || to.is_default()
                || *to != shape.row_security
                || !flags.insert(actual)
            {
                bail!(
                    "{}: row-security created origin, UID or flag payload is inconsistent",
                    table.shown()
                );
            }
            if policies.get(actual).map_or(0, BTreeMap::len) != shape.row_policies.len() {
                bail!("{}: row-security flags precede its policies", table.shown());
            }
        }
    }
    for (uid, (_, name, shape)) in created {
        if policies.get(uid).map_or(0, BTreeMap::len) != shape.row_policies.len()
            || flags.contains(uid) != !shape.row_security.is_default()
        {
            bail!(
                "{}: CREATE TABLE's row-security payload has missing explicit operations; regenerate the plan",
                name.shown()
            );
        }
        if !ids.tables.contains_key(uid) || ids.tables.get(uid) != names.table_at(uid, At::End) {
            bail!(
                "{}: created table UID is absent from plan identities",
                name.shown()
            );
        }
    }
    Ok(())
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
    use pbps_model::{PlannedChange, RowPolicy, RowSecurity, RowSecurityOrigin, Table};

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
