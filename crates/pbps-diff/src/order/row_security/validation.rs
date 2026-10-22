//! The unchanged create-only RLS admission contract from master 34b1b1db.
use super::super::AdmissionError;
use pbps_model::{
    Change, ChangeSet, IdsFile, TableName, Uid,
    plan_names::{At, Holder, PlanNames},
};
use std::collections::{BTreeMap, BTreeSet};

macro_rules! bail {
    ($($arg:tt)*) => { return Err(AdmissionError::message(format!($($arg)*))) };
}

/// A checksum authenticates the bytes, not a forged creation origin or an
/// ignored policy hidden inside CREATE TABLE's descriptive payload.
pub fn validate(
    changes: &ChangeSet,
    ids: &IdsFile,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Result<(), AdmissionError> {
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
    let names = PlanNames::of(&changes.changes, ids).map_err(AdmissionError::from_error)?;
    let mut policies: BTreeMap<&Uid, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut flags = BTreeSet::new();
    let owner = |table: &TableName, at: usize| -> Result<&Uid, AdmissionError> {
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
