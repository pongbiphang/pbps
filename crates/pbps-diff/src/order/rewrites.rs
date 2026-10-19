//! Settle binding releases through the general effect graph (DEC-2228.1).

use super::{Baseline, Because, Checks, Schedule, analyse, graph};
use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Requirement, Snapshot};
use pbps_model::{ChangeSet, ModuleDeps};

/// Apply only orders requested by BeforeRewrite. Non-participants retain
/// their relative order, rather than letting an incomplete graph sort them.
pub fn before_rewrites(
    changes: ChangeSet,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &dyn Dialect,
    declared: &ModuleDeps,
) -> Result<ChangeSet, String> {
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
    let graph = graph(
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
