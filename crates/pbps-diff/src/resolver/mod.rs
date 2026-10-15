//! Pure resolution planning. Engine adapters supply observed identities; this
//! module adds typed rebuilds and orders the resulting graph. No SQL or I/O.

mod assess;
mod graph;
mod prepare;

#[cfg(test)]
mod part_effect_tests;

// Count actual classifier calls in tests, including a caller that accidentally
// bypasses a pass cache, rather than measuring a synthetic cache-only loop.
#[inline]
fn part_effects(change: &pbps_model::Change) -> Vec<pbps_model::part_effects::PartEffect<'_>> {
    #[cfg(test)]
    part_effect_tests::record_projection();
    change.part_effects()
}

pub use assess::{Answer, Assessment, assess};

use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners};
use pbps_model::resolver::{OrderEdge, OrderingProof, Surface, SurfaceResolution};
use pbps_model::{ChangeSet, IdsFile};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the ordinary typed plan cannot be constructed: {0:?}")]
    Diff(Vec<String>),
    #[error("resolver evidence does not cover the managed surface {0:?}")]
    Coverage(Surface),
    #[error("resolver rebuild has no declared identity or definition: {0:?}")]
    Definition(Surface),
    #[error("resolver ordering contains an unsupported dependency cycle")]
    Cycle,
    #[error("resolver ordering refers to a change outside the plan")]
    Edge,
    #[error("resolver ordering cannot split this data-bearing operation: {0:?}")]
    Unsupported(Surface),
    /// The ordinary plan is the differ's executable order, so its names are
    /// followed step by step; a plan whose names cannot be is refused rather
    /// than ordered by guessed owners (#1871).
    #[error("the ordinary plan's names cannot be followed step by step: {0}")]
    Names(String),
}

/// The spelling a resolution names a base surface by: its own where the plan
/// keeps it, and the one its removal uses after the plan's renames where the
/// plan removes it. The producer and [`plan`]'s coverage check share it.
///
/// `base_names` is the base identity file's index, built once by a caller
/// that asks about every surface (#1870).
pub fn removal_spelling(
    surface: &Surface,
    base_names: &pbps_model::IdsIndex<'_>,
    desired_ids: &IdsFile,
    base_surfaces: &BTreeSet<Surface>,
    desired_surfaces: &BTreeSet<Surface>,
) -> Surface {
    prepare::removal_spelling(
        surface,
        base_names,
        desired_ids,
        base_surfaces,
        desired_surfaces,
    )
}

/// Output for the evidence seal. The final emitter receives `changes` exactly
/// as ordered here; it has no reason to choose another order.
pub struct Ordered {
    pub changes: ChangeSet,
    pub proof: OrderingProof,
}

/// The typed plan resolver planning starts from: [`crate::diff`] without
/// ADR-0013's candidate rebuilds, which the resolver's evidence replaces
/// (DEC-1515.1). Every step that derives from the ordinary plan on this path
/// uses it, so the scope a run qualifies and the plan it seals agree.
pub fn ordinary(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
) -> Result<ChangeSet, Vec<crate::DiffError>> {
    crate::schema_diff::rebuilding_by(
        base,
        desired,
        dialect,
        hints,
        &BTreeSet::new(),
        crate::schema_diff::Rebinding::Evidence,
        crate::schema_diff::Screen::Text,
        None,
    )
}

/// Derive ordinary changes (including their existing grant and PUBLIC restore
/// obligations), add the engine-proven expression rebuilds, then order them.
/// Ordinary planning never calls this path and keeps its existing order.
///
/// A module ADR-0013's candidate test would rebuild is rebuilt here only
/// when the evidence says its binding changes or one of its managed inputs
/// is replaced (DEC-1515.1).
pub fn plan(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &dyn Dialect,
) -> Result<Ordered, Error> {
    plan_operations(base, desired, hints, resolution, dialect).map(|planned| planned.ordered)
}

/// [`plan`], with the operations its steps are (#1961) and the logical plan
/// they came from.
pub struct Planned {
    pub ordered: Ordered,
    /// One per step of `ordered`, in its order.
    pub operations: Operations,
    /// The ordinary plan the resolver started from: what an operation's
    /// [`pbps_model::operation::Source::Change`] counts positions in, and the
    /// order a step's names are in force in.
    pub logical: ChangeSet,
}

/// [`plan`] and the operations it orders. [`plan`] is this, so the order and
/// the changes are the same: the operations are built beside them from where
/// each step came from, and are never part of the sealed plan.
pub fn plan_operations(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &dyn Dialect,
) -> Result<Planned, Error> {
    let (logical, prepared) = prepare(base, desired, hints, resolution, dialect)?;
    let operations = Operations::expanded(
        logical.changes.len(),
        prepared
            .origins
            .iter()
            .cloned()
            .zip(prepared.changes.changes.iter().cloned()),
        if dialect.rebuilds_modules() {
            ModuleEdits::Rebuilt
        } else {
            ModuleEdits::InPlace
        },
    );
    // Who owns each step's table comes from where the step came from, read
    // against the logical plan's own name history (#1871): the graph keeps
    // no copy of that history.
    let names = pbps_model::plan_names::PlanNames::of(&logical.changes, desired.ids)
        .map_err(|e| Error::Names(e.to_string()))?;
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let edges = graph::constraints(
        &prepared.changes,
        prepared.ordinary,
        &prepared.origins,
        &owners,
        resolution,
        &hints.module_deps,
        base,
        desired,
    );
    #[cfg(any(test, debug_assertions))]
    crate::order::shadow::observe_constraints(
        base,
        desired,
        dialect,
        hints,
        &logical,
        &operations,
        &edges,
    );
    let (ordered, sequence) = ordered(prepared.changes, edges)?;
    let operations = operations.reordered(&sequence);
    #[cfg(any(test, debug_assertions))]
    crate::order::shadow::observe(
        base,
        desired,
        dialect,
        &hints.module_deps,
        &logical,
        &operations,
        crate::order::shadow::Stage::Differ,
        None,
    );
    Ok(Planned {
        ordered,
        operations,
        logical,
    })
}

/// The ordinary plan and the resolver's input built from it: its splits,
/// and the rebuilds the evidence appends.
#[allow(clippy::wildcard_enum_match_arm)]
fn prepare(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &dyn Dialect,
) -> Result<(ChangeSet, prepare::Prepared), Error> {
    prepare::coverage(base, desired, resolution)?;
    let ordinary = ordinary(base, desired, dialect, hints)
        .map_err(|e| Error::Diff(e.into_iter().map(|e| e.to_string()).collect()))?;
    let rebuilds = prepare::rebuilds(
        &ordinary,
        resolution,
        dialect.rebuilds_modules(),
        base.ids,
        desired.ids,
    );
    let modules = rebuilds
        .iter()
        .filter_map(|s| match s {
            Surface::Module(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    let ordinary = crate::schema_diff::rebuilding_by(
        base,
        desired,
        dialect,
        hints,
        &modules,
        crate::schema_diff::Rebinding::Evidence,
        crate::schema_diff::Screen::Text,
        None,
    )
    .map_err(|e| Error::Diff(e.into_iter().map(|e| e.to_string()).collect()))?;
    let prepared = prepare::changes(
        &ordinary,
        base.schema,
        desired.schema,
        desired.ids,
        base.ids,
        &rebuilds,
        hints,
        dialect,
    )?;
    Ok((ordinary, prepared))
}

#[cfg(test)]
fn order(changes: ChangeSet, edges: BTreeSet<OrderEdge>) -> Result<Ordered, Error> {
    ordered(changes, edges).map(|(ordered, _)| ordered)
}

/// The ordered plan and the positions it took its steps from.
fn ordered(changes: ChangeSet, edges: BTreeSet<OrderEdge>) -> Result<(Ordered, Vec<usize>), Error> {
    let n = changes.changes.len();
    let sequence = sequence(n, &edges)?;
    let mut positions = vec![0; n];
    let ordered = ChangeSet {
        changes: sequence
            .iter()
            .enumerate()
            .map(|(position, &i)| {
                positions[i] = position;
                changes.changes[i].clone()
            })
            .collect(),
    };
    let proof = OrderingProof::new(
        &ordered,
        edges
            .into_iter()
            .map(|e| OrderEdge {
                before: positions[e.before],
                after: positions[e.after],
                reason: e.reason,
            })
            .collect(),
    )
    .map_err(|_| Error::Edge)?;
    Ok((
        Ordered {
            changes: ordered,
            proof,
        },
        sequence,
    ))
}

/// The order of `n` steps under `edges`, as positions into them.
fn sequence(n: usize, edges: &BTreeSet<OrderEdge>) -> Result<Vec<usize>, Error> {
    let mut incoming = vec![0usize; n];
    let mut outgoing = vec![BTreeSet::new(); n];
    for edge in edges {
        if edge.before >= n || edge.after >= n {
            return Err(Error::Edge);
        }
        if outgoing[edge.before].insert(edge.after) {
            incoming[edge.after] += 1;
        }
    }
    // Original position is the deterministic tie-break. It preserves the
    // differ's identity/data order wherever no dependency requires movement.
    let mut ready: BTreeSet<_> = (0..n).filter(|&i| incoming[i] == 0).collect();
    let mut sequence = Vec::with_capacity(n);
    while let Some(i) = ready.pop_first() {
        sequence.push(i);
        for &after in &outgoing[i] {
            incoming[after] -= 1;
            if incoming[after] == 0 {
                ready.insert(after);
            }
        }
    }
    if sequence.len() != n {
        return Err(Error::Cycle);
    }
    Ok(sequence)
}

#[cfg(test)]
mod tests;
