//! Pure resolution planning. Engine adapters supply observed identities; this
//! module adds typed rebuilds and orders the resulting graph. No SQL or I/O.

mod graph;
mod prepare;

use pbps_dialect::Dialect;
use pbps_model::ChangeSet;
use pbps_model::resolver::{OrderEdge, OrderingProof, Surface, SurfaceResolution};
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
}

/// The spelling a resolution names a base surface by: its own where the plan
/// keeps it, and the one its removal uses after the plan's renames where the
/// plan removes it. The producer and [`plan`]'s coverage check share it.
pub fn removal_spelling(
    surface: &Surface,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    base_surfaces: &BTreeSet<Surface>,
    desired_surfaces: &BTreeSet<Surface>,
) -> Surface {
    prepare::removal_spelling(surface, base, desired, base_surfaces, desired_surfaces)
}

/// Output for the evidence seal. The final emitter receives `changes` exactly
/// as ordered here; it has no reason to choose another order.
pub struct Ordered {
    pub changes: ChangeSet,
    pub proof: OrderingProof,
}

/// Derive ordinary changes (including their existing grant and PUBLIC restore
/// obligations), add the engine-proven expression rebuilds, then order them.
/// Ordinary planning never calls this path and keeps its existing order.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn plan(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    hints: &pbps_model::Hints,
    resolution: &[SurfaceResolution],
    dialect: &dyn Dialect,
) -> Result<Ordered, Error> {
    prepare::coverage(base, desired, resolution)?;
    let ordinary = crate::diff(base, desired, dialect, hints)
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
    let ordinary = crate::diff_rebuilding(base, desired, dialect, hints, &modules)
        .map_err(|e| Error::Diff(e.into_iter().map(|e| e.to_string()).collect()))?;
    let (changes, ordinary) = prepare::changes(
        &ordinary,
        base.schema,
        desired.schema,
        desired.ids,
        base.ids,
        &rebuilds,
        hints,
        dialect,
    )?;
    let edges = graph::constraints(
        &changes,
        ordinary,
        resolution,
        &hints.module_deps,
        base,
        desired,
    );
    order(changes, edges)
}

fn order(changes: ChangeSet, edges: BTreeSet<OrderEdge>) -> Result<Ordered, Error> {
    let n = changes.changes.len();
    let mut incoming = vec![0usize; n];
    let mut outgoing = vec![BTreeSet::new(); n];
    for edge in &edges {
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
    Ok(Ordered {
        changes: ordered,
        proof,
    })
}

#[cfg(test)]
mod tests;
