//! Pure resolution planning. Engine adapters supply observed identities; this
//! module adds typed rebuilds and orders the resulting graph. No SQL or I/O.

mod assess;
mod graph;
use crate::order::resolver_prepare as prepare;

#[cfg(test)]
mod part_effect_tests;

// Count actual classifier calls in tests, including a caller that accidentally
// bypasses a pass cache, rather than measuring a synthetic cache-only loop.
#[inline]
pub(crate) fn part_effects(
    change: &pbps_model::Change,
) -> Vec<pbps_model::part_effects::PartEffect<'_>> {
    #[cfg(test)]
    part_effect_tests::record_projection();
    change.part_effects()
}

pub use assess::{Answer, Assessment, assess};

use crate::order::resolver_finish::settle;
use crate::order::{Baseline, Checks, Schedule, graph};
#[cfg(test)]
use crate::order::{Source, analyse};
use pbps_dialect::Dialect;
use pbps_model::escape::Escaped;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
#[cfg(test)]
use pbps_model::resolver::OrderReason;
use pbps_model::resolver::{Surface, SurfaceResolution};
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
    #[error("resolver ordering contains an unsupported dependency cycle: {0}")]
    Cycle(String),
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
pub struct Ordered<S = crate::order::Draft> {
    pub changes: crate::order::PlanningPlan<S>,
    /// Proof observation retained for existing evidence consumers;
    /// the immutable plan holds its own matching proof inside order.
    pub proof: pbps_model::resolver::OrderingProof,
}

/// The typed plan resolver planning starts from: [`crate::diff`] without
/// ADR-0013's candidate rebuilds, which the resolver's evidence replaces
/// (DEC-1515.1). Every step that derives from the ordinary plan on this path
/// uses it, so the scope a run qualifies and the plan it seals agree.
pub fn ordinary(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &pbps_model::Hints,
) -> Result<crate::order::DiffPlan, Vec<crate::DiffError>> {
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
pub fn plan<D: Dialect + ?Sized>(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &D,
) -> Result<Ordered<crate::order::Canonical<D>>, Error> {
    plan_operations(base, desired, hints, resolution, dialect).map(|planned| planned.ordered)
}

/// [`plan`], with the operations its steps are (#1961) and the logical plan
/// they came from.
pub struct Planned<D: ?Sized> {
    pub ordered: Ordered<crate::order::Canonical<D>>,
    /// One per step of `ordered`, in its order.
    pub operations: Operations,
    /// The ordinary plan the resolver started from: what an operation's
    /// [`pbps_model::operation::Source::Change`] counts positions in, and the
    /// order a step's names are in force in.
    pub logical: crate::order::DiffPlan,
}

/// [`plan`] and the operations it orders. [`plan`] is this, so the order and
/// the changes are the same: the operations are built beside them from where
/// each step came from, and are never part of the sealed plan.
pub fn plan_operations<D: Dialect + ?Sized>(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &D,
) -> Result<Planned<D>, Error> {
    let (logical, prepared) = prepare(base, desired, hints, resolution, dialect)?;
    let module_edits = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let operations = Operations::expanded(
        logical.changes.len(),
        prepared
            .origins
            .iter()
            .cloned()
            .zip(prepared.changes.changes.iter().cloned()),
        module_edits,
    );
    // Who owns each step's table comes from where the step came from, read
    // against the logical plan's own name history (#1871): the graph keeps
    // no copy of that history.
    let names = pbps_model::plan_names::PlanNames::of(&logical.changes, desired.ids)
        .map_err(|e| Error::Names(e.to_string()))?;
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let schedule = scheduled(
        &prepared.changes,
        &prepared.origins,
        &operations,
        &owners,
        resolution,
        &hints.module_deps,
        base,
        desired,
        dialect,
    );
    let (ordered, sequence) = settle(prepared.changes, &schedule)?;
    let (ordered, operations) = crate::order::resolver_finish::finish(
        ordered,
        &sequence,
        &logical,
        prepared.origins,
        desired.ids,
        dialect,
        module_edits,
    )?;
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

/// Every constraint the prepared steps are held to, in one schedule: the
/// effects an ordinary plan's order is checked against, read in every build
/// because the resolver's order is made from them; the name slots' handovers;
/// the `depends_on:` annotations; and the two kinds only the resolver's
/// evidence states (DEC-1968.1, DEC-1968.2).
#[allow(clippy::too_many_arguments)]
fn scheduled(
    changes: &ChangeSet,
    origins: &[pbps_model::operation::Origin],
    operations: &Operations,
    owners: &PlanOwners<'_>,
    resolution: &[SurfaceResolution],
    declared: &pbps_model::ModuleDeps,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
) -> Schedule {
    let (before, after) = (
        Snapshot {
            schema: base.schema,
            ids: base.ids,
        },
        Snapshot {
            schema: desired.schema,
            ids: desired.ids,
        },
    );
    let baseline = Baseline::of(before, owners);
    let shadowed =
        crate::order::analyse_with_catalog(operations, owners, before, after, dialect, None);
    let effect_graph = graph(
        operations.as_slice(),
        &shadowed.effects,
        &baseline,
        declared,
        Checks::ALL,
    );
    let mut schedule = Schedule::of_graph(&effect_graph, operations.as_slice());
    let slots = crate::order::slots::analyse(operations.as_slice(), owners, before, dialect);
    // The resolver reads no catalog: the names are the plan's own.
    crate::order::slots::schedule(
        &mut schedule,
        &crate::order::slots::edges(&slots, None, &[]),
    );
    for c in graph::constraints(changes, origins, owners, resolution, base, desired) {
        schedule.edge(c.before, c.after, c.source, c.reason);
    }
    for c in crate::order::generated::constraints(changes, desired.schema, declared, dialect) {
        schedule.edge(c.before, c.after, c.source, c.reason);
    }
    // Unknown effects do not establish independence: keep every pair rather
    // than inventing edges that could contradict known effects (DEC-2158.1).
    for (id, _) in &shadowed.unowned {
        let i = operations
            .iter()
            .position(|op| op.id() == *id)
            .expect("an unowned operation belongs to the analysed plan");
        for j in 0..operations.len() {
            if i != j {
                schedule.keep(
                    i.min(j),
                    i.max(j),
                    format!(
                        "cannot reorder `{}` across `{}`: the latter's names have no recorded owner",
                        Escaped(&changes.changes[j].change.subject()),
                        Escaped(&changes.changes[i].change.subject()),
                    ),
                );
            }
        }
    }
    schedule
}

/// [`plan_operations`]' ordering over a plan that is its own logical plan,
/// each step whole: the shape a test builds by hand.
#[cfg(test)]
fn order_logical(
    changes: ChangeSet,
    resolution: &[SurfaceResolution],
    declared: &pbps_model::ModuleDeps,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
) -> Result<Ordered, Error> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, desired.ids)
        .map_err(|e| Error::Names(e.to_string()))?;
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let origins: Vec<_> = (0..changes.changes.len())
        .map(pbps_model::operation::Origin::Whole)
        .collect();
    let operations = Operations::expanded(
        changes.changes.len(),
        origins.iter().cloned().zip(changes.changes.iter().cloned()),
        if dialect.rebuilds_modules() {
            ModuleEdits::Rebuilt
        } else {
            ModuleEdits::InPlace
        },
    );
    let schedule = scheduled(
        &changes,
        &origins,
        &operations,
        &owners,
        resolution,
        declared,
        base,
        desired,
        dialect,
    );
    settle(changes, &schedule).map(|(ordered, _)| ordered)
}

/// The ordinary plan and the resolver's input built from it: its splits,
/// and the rebuilds the evidence appends.
#[allow(clippy::wildcard_enum_match_arm)]
fn prepare(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &(impl Dialect + ?Sized),
) -> Result<(crate::order::DiffPlan, prepare::Prepared), Error> {
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

/// [`settle`] over edges stated by hand: the shape the tests of the
/// schedule's refusals build.
#[cfg(test)]
fn order(
    changes: ChangeSet,
    edges: BTreeSet<pbps_model::resolver::OrderEdge>,
) -> Result<Ordered, Error> {
    let mut schedule = Schedule::new(changes.changes.len());
    for edge in edges {
        let source = match edge.reason {
            OrderReason::Binding => Source::Binding,
            OrderReason::Authorization => Source::Execution,
            OrderReason::Structural
            | OrderReason::Identity
            | OrderReason::Data
            | OrderReason::Restoration => Source::Effect,
        };
        schedule.edge(edge.before, edge.after, source, edge.reason);
    }
    settle(changes, &schedule).map(|(ordered, _)| ordered)
}

#[cfg(test)]
mod tests;
