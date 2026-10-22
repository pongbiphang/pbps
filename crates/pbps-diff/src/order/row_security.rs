//! The creation boundary shared by ordinary and resolver schedules.

use super::{PlanningPlan, Schedule, Source};
use pbps_dialect::Dialect;
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
    let effects = super::analyse(&operations, &owners, before, after, dialect);
    if let Some((id, why)) = effects.unowned.first() {
        return Err(format!(
            "policy release operation {id} has unread effects: {why}"
        ));
    }
    let graph = super::graph(
        operations.as_slice(),
        &effects.effects,
        &super::Baseline::of(before, &owners),
        deps,
        super::Checks::ALL,
    );
    let mut schedule = Schedule::of_graph(&graph, operations.as_slice());
    for edge in super::generated::constraints(plan, after.schema, deps, dialect) {
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

/// Policies follow the structural/module/data build; flags follow policies.
/// These are setup phases, not inferred bindings inside predicate text.
pub fn edges<'a>(changes: impl IntoIterator<Item = &'a Change>) -> Vec<(usize, usize)> {
    let changes: Vec<_> = changes.into_iter().collect();
    let policies: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| matches!(c, Change::CreatePolicy { .. }).then_some(i))
        .collect();
    let flags: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| matches!(c, Change::SetRowSecurity { .. }).then_some(i))
        .collect();
    if policies.is_empty() && flags.is_empty() {
        return Vec::new();
    }
    let suppliers: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            (!matches!(
                c,
                Change::CreatePolicy { .. }
                    | Change::SetRowSecurity { .. }
                    | Change::SetDataMode { .. }
                    | Change::SetEnsureKeys { .. }
            ) && (c.table().is_some()
                || matches!(
                    c,
                    Change::CreateModule { .. }
                        | Change::AlterModule { .. }
                        | Change::PublicExecution { .. }
                )))
            .then_some(i)
        })
        .collect();
    let mut out = Vec::new();
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
    if let Change::DropColumn { uid, column } = change {
        Some((uid, column, "dropped"))
    } else if let Change::AlterColumnType { uid, column, .. } = change {
        Some((uid, column, "retyped"))
    } else {
        None
    }
}

pub fn policy_target_uid(change: &Change) -> Option<&Uid> {
    if let Change::DropTable { uid, .. }
    | Change::RenameTable { uid, .. }
    | Change::RenameColumn { uid, .. } = change
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
            Change::DropTable { .. } | Change::RenameTable { .. }
        ) {
            let table = names.table_at(uid, At::Start).ok_or_else(|| {
                release_refusal!("policy dependent table {uid} has no opening catalog identity")
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
                "{}; its table drop must precede the dependent change. Plan again.",
                release.refusal
            );
        }
    }
    Ok(())
}

mod validation;
pub use validation::validate;
