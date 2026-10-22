//! Couple final risk classification, proof and stable expansion identities.
use super::Schedule;
use crate::resolver::{Error, Ordered};
use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations, Origin};
use pbps_model::resolver::OrderingProof;
use pbps_model::{ChangeSet, IdsFile};

pub(crate) fn finish<D: Dialect + ?Sized>(
    mut ordered: Ordered,
    sequence: &[usize],
    logical: usize,
    origins: Vec<Origin>,
    ids: &IdsFile,
    dialect: &D,
    module_edits: ModuleEdits,
) -> Result<(Ordered<super::Canonical<D>>, Operations), Error> {
    // Settlement can put a validator after seed rows. Preparation's empty
    // context cannot certify that final position (DEC-1503.4); seal the same
    // risks saved-plan validation will derive, including in operation copies.
    let risks = crate::order::generated::risks(&ordered.changes.payload, ids, dialect);
    for (part, risks) in ordered.changes.payload.changes.iter_mut().zip(risks) {
        part.risks = risks;
    }
    ordered.proof = OrderingProof::new(&ordered.changes.payload, ordered.proof.edges().clone())
        .map_err(|_| Error::Edge)?;
    ordered.changes.proof = ordered.proof.clone();
    ordered.changes.origins = sequence.iter().map(|&at| origins[at].clone()).collect();
    ordered.changes.logical = logical;
    // Keep IDs assigned in expansion order even when settlement moves parts.
    let mut classified = vec![None; sequence.len()];
    for (&from, part) in sequence.iter().zip(&ordered.changes.payload.changes) {
        classified[from] = Some(part.clone());
    }
    let operations = Operations::expanded(
        logical,
        origins.into_iter().zip(
            classified
                .into_iter()
                .map(|part| part.expect("settlement is a permutation of the prepared changes")),
        ),
        module_edits,
    );
    let operations = reordered_operations(&operations, sequence);
    ordered.changes.identities = Some(operations.clone());
    Ok((
        Ordered {
            changes: super::plan::canonical(ordered.changes, dialect),
            proof: ordered.proof,
        },
        operations,
    ))
}

/// Preserve the operation IDs assigned in expansion order. This produces a
/// draft observation collection, not an execution certificate.
pub fn reordered_operations(operations: &Operations, sequence: &[usize]) -> Operations {
    let mut seen = vec![false; operations.len()];
    assert_eq!(sequence.len(), seen.len(), "not a permutation");
    Operations::from_operations(sequence.iter().map(|&i| {
        assert!(!std::mem::replace(&mut seen[i], true), "not a permutation");
        operations.as_slice()[i].clone()
    }))
}

/// The ordered plan and the positions it took its steps from. The order the
/// steps arrive in is the tie-break: it preserves the differ's identity and
/// data order wherever no dependency requires movement (DEC-1956.2). A cycle
/// is refused by name (DEC-1956.3).
pub(crate) fn settle(
    changes: ChangeSet,
    schedule: &Schedule,
) -> Result<(Ordered, Vec<usize>), Error> {
    use crate::order::Refusal;
    let described = changes.clone();
    let settled = schedule
        .settle(super::propose(changes))
        .map_err(|refusal| match refusal {
            Refusal::Outside { .. } | Refusal::Mismatch { .. } => Error::Edge,
            Refusal::Cycle { .. } | Refusal::Unmet(_) | Refusal::Kept(_) => {
                Error::Cycle(refusal.describe(&described))
            }
        })?;
    Ok((
        Ordered {
            changes: settled.changes,
            proof: settled.proof,
        },
        settled.sequence,
    ))
}
