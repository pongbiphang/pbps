//! Settle binding releases through the general effect graph (DEC-2228.1).

use super::{Baseline, Because, Checks, Schedule, analyse, graph};
use pbps_dialect::Dialect;
use pbps_model::ModuleDeps;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Requirement, Snapshot};

/// Settle BeforeRewrite and the dependencies of its participants. Unrelated
/// steps retain their relative order, and later stages keep their own edges.
pub fn before_rewrites(
    changes: super::PlanningPlan,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    declared: &ModuleDeps,
) -> Result<super::PlanningPlan, String> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, after.ids)
        .map_err(|error| error.to_string())?;
    let owners = PlanOwners::new(names, before.ids, after.ids);
    let operations = Operations::of_plan(
        &changes.changes,
        if dialect.rebuilds_modules() {
            ModuleEdits::Rebuilt
        } else {
            ModuleEdits::InPlace
        },
    );
    let base = Snapshot {
        schema: before.schema,
        ids: before.ids,
    };
    let end = Snapshot {
        schema: after.schema,
        ids: after.ids,
    };
    let effects = analyse(&operations, &owners, base, end, dialect);
    if !effects.effects.iter().any(|fx| {
        fx.requires
            .iter()
            .any(|r| matches!(r, Requirement::BeforeRewrite(_)))
    }) {
        return Ok(changes);
    }
    if let Some((_, error)) = effects.unowned.first() {
        return Err(error.to_string());
    }
    let mut graph = graph(
        operations.as_slice(),
        &effects.effects,
        &Baseline::of(base, &owners),
        declared,
        Checks::ALL,
    );
    let mut moving = vec![false; changes.changes.len()];
    for edge in &graph.edges {
        if matches!(edge.because, Because::BeforeRewrite(_)) {
            moving[edge.before] = true;
            moving[edge.after] = true;
        }
    }
    if !moving.iter().any(|m| *m) {
        return Ok(changes);
    }
    // Only the release and its delayed rewrites may disturb this order.
    // Their dependents have to follow them, transitively. Do not let the
    // connected pass's computed-column edges extend or constrain that set.
    graph
        .edges
        .retain(|edge| !super::edges::connected_module_release(edge, operations.as_slice()));
    let mut outgoing = vec![Vec::new(); moving.len()];
    for edge in &graph.edges {
        outgoing[edge.before].push(edge.after);
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
    schedule
        .settle(changes.clone())
        .map(|settled| settled.changes)
        .map_err(|refusal| refusal.describe(&changes))
}
