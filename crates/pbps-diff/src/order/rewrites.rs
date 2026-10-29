//! Settle binding releases through the general effect graph (DEC-2228.1).

use super::{Baseline, Because, Checks, Schedule, graph};
use pbps_dialect::{Dialect, slots::CatalogFacts};
use pbps_model::operation::{
    Incarnation, ModuleEdits, ObjectKey, Operations, PlanOwners, Requirement, Snapshot,
};
use pbps_model::{Change, ModuleDeps};

/// Settle BeforeRewrite and the dependencies of its participants. Unrelated
/// steps retain their relative order, and later stages keep their own edges.
pub fn before_rewrites(
    changes: super::PlanningPlan,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    declared: &ModuleDeps,
) -> Result<super::PlanningPlan, String> {
    settle_requirements(changes, before, after, dialect, declared, None, &[])
}

/// Settle requirements refined by connected name comparisons before the final
/// observer. Reuse the release scheduler so callers and their prerequisites
/// move together, while unrelated steps and catalog drop precedence stay put.
pub fn catalog_requirements<S: Clone>(
    changes: super::PlanningPlan<S>,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    declared: &ModuleDeps,
    catalog: &CatalogFacts,
    precedence: &[(Change, Change)],
) -> Result<super::PlanningPlan<S>, String> {
    settle_requirements(
        changes,
        before,
        after,
        dialect,
        declared,
        Some(catalog),
        precedence,
    )
}

fn settle_requirements<S: Clone>(
    changes: super::PlanningPlan<S>,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    declared: &ModuleDeps,
    catalog: Option<&CatalogFacts>,
    precedence: &[(Change, Change)],
) -> Result<super::PlanningPlan<S>, String> {
    let names = pbps_model::plan_names::PlanNames::of(
        &changes.boundaries.names(&changes).changes,
        after.ids,
    )
    .map_err(|error| error.to_string())?;
    let owners = PlanOwners::new(names, before.ids, after.ids);
    let operations = (!changes.boundaries.is_empty())
        .then_some(&changes.identities)
        .and_then(|identities| identities.as_ref())
        .map(|ops| ops.with_steps(&changes.changes))
        .unwrap_or_else(|| {
            Operations::of_plan(
                &changes.changes,
                if dialect.rebuilds_modules() {
                    ModuleEdits::Rebuilt
                } else {
                    ModuleEdits::InPlace
                },
            )
        });
    let base = Snapshot {
        schema: before.schema,
        ids: before.ids,
    };
    let end = Snapshot {
        schema: after.schema,
        ids: after.ids,
    };
    let effects = super::shadow::analyse_planning(
        &operations,
        &owners,
        base,
        end,
        dialect,
        catalog,
        &changes,
    );
    if !effects.effects.iter().any(|fx| {
        fx.requires.iter().any(|r| {
            matches!(r, Requirement::BeforeRewrite(_))
                || (catalog.is_some()
                    && matches!(r, Requirement::Is(ObjectKey::Module(_), Incarnation::Final)))
        })
    }) && changes.boundaries.is_empty()
    {
        return Ok(changes);
    }
    if let Some((_, error)) = effects.unowned.first() {
        return Err(error.to_string());
    }
    let mut graph = graph(
        operations.as_slice(),
        &effects.effects,
        &Baseline::of(base, &owners).with_symbolic(effects.symbolic.clone()),
        declared,
        Checks::ALL,
    );
    let mut moving = vec![false; changes.changes.len()];
    for edge in &graph.edges {
        let final_callee = catalog.is_some()
            && matches!(&edge.because,
                Because::Creates(key @ ObjectKey::Module(_))
                | Because::Rewrites(key @ ObjectKey::Module(_))
                    if effects.effects[edge.after].requires
                        .contains(&Requirement::Is(key.clone(), Incarnation::Final)));
        if matches!(edge.because, Because::BeforeRewrite(_)) || final_callee {
            moving[edge.before] = true;
            moving[edge.after] = true;
        }
    }
    if !changes.boundaries.is_empty() {
        for (at, fx) in effects.effects.iter().enumerate() {
            moving[at] |= !fx.symbolic.is_empty();
        }
    }
    if !moving.iter().any(|m| *m) && changes.boundaries.is_empty() {
        return Ok(changes);
    }
    // Only the release, its delayed rewrites and connected callees may disturb this order.
    // Their dependents have to follow them, transitively. Do not let the
    // connected pass's computed-column edges extend or constrain that set.
    graph
        .edges
        .retain(|edge| !super::edges::connected_module_release(edge, operations.as_slice()));
    let at = |change: &Change| changes.changes.iter().position(|p| &p.change == change);
    let precedence: Vec<_> = precedence
        .iter()
        .filter_map(|(before, after)| Some((at(before)?, at(after)?)))
        .collect();
    let mut outgoing = vec![Vec::new(); moving.len()];
    for edge in &graph.edges {
        outgoing[edge.before].push(edge.after);
    }
    for &(before, after) in &precedence {
        outgoing[before].push(after);
    }
    let mut pending: Vec<_> = (0..moving.len()).filter(|&at| moving[at]).collect();
    while let Some(at) = pending.pop() {
        for &dependent in &outgoing[at] {
            if !moving[dependent] {
                moving[dependent] = true;
                pending.push(dependent);
            }
        }
    }
    // Incoming prerequisites still constrain a participant. Edges between
    // unrelated steps belong to their current or later ordering stage.
    graph
        .edges
        .retain(|edge| moving[edge.before] || moving[edge.after]);
    let mut schedule = Schedule::of_graph(&graph, operations.as_slice());
    for &(before, after) in &precedence {
        schedule.edge(
            before,
            after,
            super::Source::Catalog,
            pbps_model::resolver::OrderReason::Structural,
        );
    }
    let mut previous = None;
    for (at, moves) in moving.into_iter().enumerate() {
        if !moves {
            if let Some(previous) = previous {
                schedule.keep(
                    previous,
                    at,
                    format!(
                        "cannot order the dependency release without moving `{}` after `{}`; split the revision into separate plans",
                        pbps_model::Text::from(changes.changes[previous].change.subject()).shown(),
                        pbps_model::Text::from(changes.changes[at].change.subject()).shown(),
                    ),
                );
            }
            previous = Some(at);
        }
    }
    let mut settled = schedule
        .settle(changes.clone())
        .map_err(|refusal| refusal.describe(&changes))?;
    if !settled.changes.boundaries.is_empty() {
        let operations = super::reordered_operations(&operations, &settled.sequence);
        settled.changes.identities = Some(operations);
    }
    Ok(settled.changes)
}
