//! Policy teardown and setup shared by ordinary and resolver schedules.

use super::{PlanningPlan, Schedule, Source};
use pbps_dialect::Dialect;
use pbps_model::ColumnChange;
use pbps_model::GrantChange;
use pbps_model::ModuleChange;
use pbps_model::PartitionChange;
use pbps_model::PolicyChange;
use pbps_model::RowChange;
use pbps_model::TableChange;
use pbps_model::resolver::OrderReason;
use pbps_model::{
    Change, ChangeSet, IdsFile, ModuleDeps, Uid,
    plan_names::{At, PlanNames},
};
use std::collections::BTreeMap;

/// Catalog facts name the owner drop that releases each policy binding.
/// Refusals travel beside the edge to preserve the saved-order diagnostic.
pub struct PolicyRelease {
    pub before: usize,
    pub after: usize,
    pub refusal: String,
}

/// Settle the existing final policy phase without exposing a raw write-back.
/// Earlier stages keep their own proofs; this stage records only its edges.
///
/// ```compile_fail
/// fn reorder(raw: pbps_model::ChangeSet) {
///     let _ = pbps_diff::order::row_security::settle(
///         raw, pbps_diff::order::Schedule::new(0), &[],
///     );
/// }
/// ```
pub fn settle(
    plan: PlanningPlan,
    mut schedule: Schedule,
    releases: &[PolicyRelease],
) -> Result<PlanningPlan, String> {
    for release in releases {
        schedule.edge(
            release.before,
            release.after,
            Source::Catalog,
            OrderReason::Structural,
        );
    }
    for (before, after) in edges(plan.changes.iter().map(|p| &p.change)) {
        schedule.edge(before, after, Source::Effect, OrderReason::Structural);
    }
    schedule
        .settle(plan.clone())
        .map(|settled| settled.into_planning())
        .map_err(|why| {
            format!(
                "{}{}",
                why.describe(&plan),
                if releases.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n{}",
                        releases
                            .iter()
                            .map(|r| r.refusal.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                }
            )
        })
}

/// Rebuild the same structural, generated-table and name-slot constraints
/// only when a catalog policy release requires a new teardown order.
pub fn release_schedule(
    plan: &PlanningPlan,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    deps: &ModuleDeps,
    dialect: &(impl Dialect + ?Sized),
) -> Result<Schedule, String> {
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
    let operations = Operations::of_plan(&plan.changes, ModuleEdits::Rebuilt);
    let names = pbps_model::plan_names::PlanNames::of(&plan.changes, after.ids)
        .map_err(|why| why.to_string())?;
    let owners = PlanOwners::new(names, before.ids, after.ids);
    let before = Snapshot {
        schema: before.schema,
        ids: before.ids,
    };
    let after = Snapshot {
        schema: after.schema,
        ids: after.ids,
    };
    let effects = super::analyse_captured(
        &operations,
        &owners,
        before,
        after,
        dialect,
        None,
        plan.captured(),
    )
    .map_err(|e| e.to_string())?;
    let step_effects = effects.effects().map_err(|unread| {
        format!(
            "policy release operation {} has unread effects: {}",
            unread.id, unread.error
        )
    })?;
    let graph = super::graph(
        operations.as_slice(),
        step_effects,
        &super::Baseline::of(before, &owners).map_err(|e| e.to_string())?,
        deps,
        super::Checks::ALL,
    );
    let mut schedule = Schedule::of_graph(&graph, operations.as_slice());
    for edge in super::generated::plan_constraints(plan, after.schema, deps, dialect, false) {
        schedule.edge(edge.before, edge.after, edge.source, edge.reason);
    }
    let slots = super::slots::analyse(operations.as_slice(), &owners, before, dialect);
    if !slots.unowned.is_empty() || !slots.unlowered.is_empty() {
        return Err("policy release has unread name-slot effects".into());
    }
    super::slots::schedule(&mut schedule, &super::slots::edges(&slots, None, &[]));
    Ok(schedule)
}

/// Catalog selection stays with the caller; only order writes its constraints.
pub fn module_constraints(
    schedule: &mut Schedule,
    edges: Vec<super::expand::Order>,
) -> Result<(), String> {
    for edge in edges {
        let (super::expand::Step::Plan(before), super::expand::Step::Plan(after)) =
            (edge.before, edge.after)
        else {
            return Err("module catalog edge names an unexpanded operation".into());
        };
        schedule.edge(before, after, edge.source, edge.reason);
    }
    Ok(())
}

/// Policy drops precede module/column teardown; installs follow the build,
/// and flags follow policies. These phases do not infer predicate bindings.
pub fn edges<'a>(changes: impl IntoIterator<Item = &'a Change>) -> Vec<(usize, usize)> {
    let changes: Vec<_> = changes.into_iter().collect();
    let policies: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            matches!(c, Change::Policy(PolicyChange::CreatePolicy { .. })).then_some(i)
        })
        .collect();
    let flags: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            matches!(c, Change::Policy(PolicyChange::SetRowSecurity { .. })).then_some(i)
        })
        .collect();
    let drops: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            matches!(c, Change::Policy(PolicyChange::DropPolicy { .. })).then_some(i)
        })
        .collect();
    if drops.is_empty() && policies.is_empty() && flags.is_empty() {
        return Vec::new();
    }
    let suppliers: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            (!matches!(
                c,
                Change::Policy(PolicyChange::CreatePolicy { .. })
                    | Change::Policy(PolicyChange::SetRowSecurity { .. })
                    | Change::Row(RowChange::SetDataMode { .. })
                    | Change::Row(RowChange::SetEnsureKeys { .. })
            ) && (c.table().is_some()
                || matches!(
                    c,
                    Change::Module(ModuleChange::CreateModule { .. })
                        | Change::Module(ModuleChange::AlterModule { .. })
                        | Change::Permission(GrantChange::PublicExecution { .. })
                )))
            .then_some(i)
        })
        .collect();
    let mut out = Vec::new();
    for (later, change) in changes.iter().enumerate() {
        if matches!(
            change,
            Change::Module(ModuleChange::DropModule { .. })
                | Change::Column(ColumnChange::DropColumn { .. })
                | Change::Column(ColumnChange::AlterColumnType { .. })
        ) {
            out.extend(drops.iter().map(|&earlier| (earlier, later)));
        }
    }
    for &later in policies.iter().chain(&flags) {
        out.extend(suppliers.iter().map(|&earlier| (earlier, later)));
    }
    for &flag in &flags {
        out.extend(policies.iter().map(|&policy| (policy, flag)));
    }
    out
}

macro_rules! release_refusal {
    ($($arg:tt)*) => { super::AdmissionError::message(format!($($arg)*)) };
}
macro_rules! release_bail {
    ($($arg:tt)*) => { return Err(release_refusal!($($arg)*)) };
}

/// Exact policy dependents of edited columns and dropped or renamed tables.
/// An absent entry is unread; an empty vector records a successful empty read.
pub type PolicyDependents = BTreeMap<Uid, Vec<super::catalog::PolicyDependent>>;

/// Destructive column edits shared with engine probes. Rename dependencies
/// are read separately, without invoking drop/retype-only probes.
pub fn column_edit(change: &Change) -> Option<(&Uid, &pbps_model::ColumnRef, &'static str)> {
    if let Change::Column(ColumnChange::DropColumn { uid, column }) = change {
        Some((uid, column, "dropped"))
    } else if let Change::Column(ColumnChange::AlterColumnType { uid, column, .. }) = change {
        Some((uid, column, "retyped"))
    } else {
        None
    }
}

pub fn policy_target_uid(change: &Change) -> Option<&Uid> {
    if let Change::Table(TableChange::DropTable { uid, .. })
    | Change::Table(TableChange::RenameTable { uid, .. })
    | Change::Column(ColumnChange::RenameColumn { uid, .. }) = change
    {
        Some(uid)
    } else {
        column_edit(change).map(|(uid, _, _)| uid)
    }
}

pub fn policy_releases(
    changes: &ChangeSet,
    ids: &IdsFile,
    found: &PolicyDependents,
) -> Result<Vec<PolicyRelease>, super::AdmissionError> {
    let names = PlanNames::of(&changes.changes, ids).map_err(super::AdmissionError::from_error)?;
    let mut edges = Vec::new();
    for (at, step) in changes.changes.iter().enumerate() {
        let Some(uid) = policy_target_uid(&step.change) else {
            continue;
        };
        let policies = found.get(uid).ok_or_else(|| {
            release_refusal!("policy dependents of {uid} were not read from PostgreSQL")
        })?;
        if policies.is_empty() {
            continue;
        }
        let (subject, what, dropped_table) = if matches!(
            &step.change,
            Change::Table(TableChange::DropTable { .. })
                | Change::Table(TableChange::RenameTable { .. })
        ) {
            let table = names.table_at(uid, At::Start).ok_or_else(|| {
                release_refusal!("policy dependent table {uid} has no opening catalog identity")
            })?;
            if matches!(&step.change, Change::Table(TableChange::DropTable { .. })) {
                (table.shown().to_string(), "dropped", Some(table))
            } else {
                (table.shown().to_string(), "renamed", None)
            }
        } else {
            let what = if matches!(
                &step.change,
                Change::Column(ColumnChange::RenameColumn { .. })
            ) {
                "renamed"
            } else {
                column_edit(&step.change)
                    .expect("a policy target is a table or column")
                    .2
            };
            let (owner, name) = names.column_at(uid, At::Start).ok_or_else(|| {
                release_refusal!("policy dependent column {uid} has no opening catalog identity")
            })?;
            let table = names.table_at(owner, At::Start).ok_or_else(|| {
                release_refusal!("policy dependent column {uid} has no opening table identity")
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
                "{} cannot be {what}: policy `{}` on {} depends on it; explicitly remove or replace that policy in the declarations",
                subject,
                pbps_model::escape::Escaped(&policy.name),
                policy.table.shown()
            );
            // A vacated spelling must not release a different table's policy.
            // Resolve each explicit removal at its statement boundary, then
            // compare the opening catalog identity used by pg_depend.
            let Some(before) = changes.changes.iter().enumerate().position(|(i, p)| {
                if let Change::Table(TableChange::DropTable { uid, .. }) = &p.change {
                    names.table_at(uid, At::Start) == Some(&policy.table)
                } else if let Change::Policy(PolicyChange::DropPolicy { table, name, .. }) =
                    &p.change
                {
                    name == &policy.name
                        && if let pbps_model::plan_names::Holder::Tracked(uid) =
                            names.holder(table, At::Before(i))
                        {
                            names.table_at(uid, At::Start) == Some(&policy.table)
                        } else {
                            false
                        }
                } else {
                    false
                }
            }) else {
                release_bail!("{refusal}");
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

pub fn check_releases(
    changes: &ChangeSet,
    ids: &IdsFile,
    found: &PolicyDependents,
) -> Result<(), super::AdmissionError> {
    for release in policy_releases(changes, ids, found)? {
        if release.before >= release.after {
            release_bail!(
                "{}; its policy or table drop must precede the dependent change. Plan again.",
                release.refusal
            );
        }
    }
    Ok(())
}

/// RLS transitions must commit together with their closing guard. Include the
/// descriptive CREATE TABLE payload so edited artifacts cannot hide a policy.
pub fn requires_transaction(changes: &ChangeSet) -> bool {
    changes.changes.iter().any(|step| {
        matches!(step.change, Change::Policy(_))
            || if let Change::Table(TableChange::CreateTable { table, .. }) = &step.change {
                table.has_row_security()
            } else if let Change::Partition(PartitionChange::AttachPartition { shape, .. })
            | Change::Partition(PartitionChange::DetachPartition { shape, .. }) =
                &step.change
            {
                shape.has_row_security()
            } else {
                false
            }
    })
}

mod validation;
pub use validation::validate;

#[cfg(test)]
mod tests;
