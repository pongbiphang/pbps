//! Private target-input capture components (ADR-0016; issue #612).
//!
//! Source stays in memory; a fresh authorized read can seal keyed fingerprints.
//! No saved-plan publication path is enabled by these components; the engine adapter must qualify the
//! requested read scope and the lifecycle must qualify non-snapshot inputs.

mod bindings;
mod logical;
mod nodes;
mod properties;
mod queries;
mod read;
mod render;

mod profile;
mod scope;

use pbps_db::resolver::capture::ObjectIdentity;
use std::collections::BTreeSet;

/// What a stored expression's node tree binds, by class (#1523, #2046).
///
/// `pg_depend` is no substitute: it omits the pinned built-ins, so it cannot
/// say which built-in function a CHECK or a DEFAULT calls, and a gate that
/// admits only built-in code has to know exactly that. The tree carries every
/// occurrence, and the decoder refuses a node or a field it does not know
/// instead of certifying a partial set (see `nodes`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Bound {
    /// Every function the tree names: calls, cast functions, an operator's
    /// own function (`opfuncid`).
    pub routines: BTreeSet<u32>,
    /// Every operator, which runs its `pg_operator.oprcode`.
    pub operators: BTreeSet<u32>,
    /// Every type: the type a coercion or a literal names.
    pub types: BTreeSet<u32>,
}

/// The objects a stored expression binds, or why the tree cannot be certified
/// complete. The reason is the decoder's category only: a tree can hold a
/// private literal.
pub(crate) fn bound_objects(text: &str, major: u32) -> Result<Bound, String> {
    let tree = nodes::decode(text, major)
        .map_err(|e| format!("the stored expression is not one this release can read ({e:?})"))?;
    let mut bound = Bound::default();
    for reference in nodes::references(&tree, major).map_err(|e| {
        format!("the stored expression has a reference this release cannot read ({e:?})")
    })? {
        // A relation, collation, role, constraint or operator class an
        // expression names is not code of its own: what runs is the function
        // or the operator that takes it, and that is listed above.
        match reference.class {
            nodes::ReferenceClass::Routine => bound.routines.insert(reference.oid),
            nodes::ReferenceClass::Operator => bound.operators.insert(reference.oid),
            nodes::ReferenceClass::Type => bound.types.insert(reference.oid),
            nodes::ReferenceClass::Relation
            | nodes::ReferenceClass::Collation
            | nodes::ReferenceClass::Role
            | nodes::ReferenceClass::Constraint
            | nodes::ReferenceClass::OperatorClass
            | nodes::ReferenceClass::OperatorFamily => false,
        };
    }
    Ok(bound)
}

/// A complete set of candidates sharing a name, or all names in a namespace.
/// The set includes every overload; signatures cannot filter its membership.
/// Capturing a set is not proof that a planner selected every required set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct CandidateSet {
    pub class: CandidateClass,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum CandidateClass {
    Relation,
    Routine,
    Type,
    Operator,
    Collation,
    OperatorClass,
    OperatorFamily,
    Cast,
    Extension,
}

impl CandidateClass {
    const ALL: [Self; 9] = [
        Self::Relation,
        Self::Routine,
        Self::Type,
        Self::Operator,
        Self::Collation,
        Self::OperatorClass,
        Self::OperatorFamily,
        Self::Cast,
        Self::Extension,
    ];

    /// The class a sealed predicate names by its catalog.
    fn of_catalog(catalog: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|class| class.catalog() == catalog)
    }

    fn catalog(self) -> &'static str {
        match self {
            Self::Relation => "pg_class",
            Self::Routine => "pg_proc",
            Self::Type => "pg_type",
            Self::Operator => "pg_operator",
            Self::Collation => "pg_collation",
            Self::OperatorClass => "pg_opclass",
            Self::OperatorFamily => "pg_opfamily",
            Self::Cast => "pg_cast",
            Self::Extension => "pg_extension",
        }
    }
}

/// Actual retained roots and complete candidate predicates to read. This is
/// an input request, never a caller-supplied completeness certificate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CaptureScope {
    pub retained: BTreeSet<ObjectIdentity>,
    pub candidates: BTreeSet<CandidateSet>,
}

/// A dropped routine's declared signature as the plan's `DROP` spells it,
/// with the write path that `DROP` runs under and the kind it must be (`f`
/// or `p`). The capture identifies it in its own snapshot, so the routine a
/// plan drops is read coherently with the candidates it is assessed against
/// (#1124, #1126, #1148).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct DroppedSignature {
    pub spelled: String,
    pub path: String,
    pub kind: &'static str,
}

pub use pbps_db::resolver::capture::{CaptureError, Uncovered};

mod manifest;
mod ownership;
pub use manifest::{BindingRecord, CapturedInputs, CompiledCapture, SealError};

/// The canonical property rule every sealed PostgreSQL prerequisite uses.
pub(crate) const INPUT_RULE: &str = properties::RULE;
pub use ownership::RecordedOwnership;

mod assess;
pub use assess::{Managed, Paths, assess, managed_scope, scope};

mod apply;
pub use apply::{capture_closing, recheck_opening};

mod staged;
pub use pbps_db::resolver::capture::{Assessment, Verdict};
pub use staged::{StageError, Staged, compare as compare_staged};

/// Read coherent catalog inputs and release the transaction before returning.
/// The caller discards the connection on cancellation; no publication path
/// exists here.
pub async fn capture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
) -> Result<CapturedInputs, CaptureError> {
    capture_identifying(connection, scope, &BTreeSet::new()).await
}

/// [`capture`], also identifying each dropped signature in the same snapshot.
pub async fn capture_identifying(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
) -> Result<CapturedInputs, CaptureError> {
    let mut prepared = None;
    let read = read::owned(connection, dropped, |catalog, major| {
        let mut result = scope::prepare(catalog, major, scope)?;
        let selection = std::mem::take(&mut result.render);
        prepared = Some(result);
        Ok(selection)
    })
    .await?;
    manifest::finish(
        read,
        prepared.ok_or(CaptureError::Incomplete)?,
        scope.clone(),
    )
    .map_err(CaptureError::Coverage)
}

/// Qualify recorded ownership while the final coherent read still holds its
/// private relkind/prokind/dependency facts. The key and Schema/IdsFile are
/// fixed at this producer entry; the returned capture has no rekey method.
pub async fn capture_identifying_qualified(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    roles: Option<&crate::resolver::authorization::RoleMap>,
    recorded: &RecordedOwnership<'_>,
) -> Result<(CapturedInputs, pbps_model::resolver::InputManifest), CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let ownership = ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    let manifest = captured
        .seal_with_roles(key, roles, Some(&ownership))
        .map_err(|_| CaptureError::Incomplete)?;
    Ok((captured, manifest))
}

/// Fix the key, role map and recorded identity roots at the authorized
/// scratch read. The private result exposes no raw or rekey operation.
pub async fn capture_identifying_for_plan(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    roles: &crate::resolver::authorization::RoleMap,
    recorded: &RecordedOwnership<'_>,
) -> Result<CompiledCapture, CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let ownership = ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    Ok(CompiledCapture::new(captured, key, roles, ownership))
}

/// A new owned snapshot is required on every recheck; cached rows cannot
/// answer whether candidate additions or property changes invalidated inputs.
pub async fn recapture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    previous: &CapturedInputs,
) -> Result<
    (
        CapturedInputs,
        Vec<pbps_db::resolver::capture::CaptureDifference>,
    ),
    CaptureError,
> {
    let signatures = previous.dropped().keys().cloned().collect();
    let current = capture_identifying(connection, previous.scope(), &signatures).await?;
    let differences = previous.compare(&current);
    Ok((current, differences))
}

#[cfg(test)]
mod tests;

mod baseline;

mod session;

#[cfg(test)]
mod ordering_tests;

#[cfg(test)]
mod metadata_tests;
