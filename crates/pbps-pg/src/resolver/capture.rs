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
pub use pbps_db::resolver::capture::{Assessment, Verdict};

/// Read coherent catalog inputs and release the transaction before returning.
/// This does not qualify executable content or a scratch runtime. The native
/// lifecycle must enclose it in its separately qualified input boundary and
/// discard the connection on cancellation; no publication path exists here.
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

/// Capture and seal in the same authorized read. The caller fixes the key
/// before this read starts; no method on a retained capture can rekey it.
pub async fn capture_identifying_sealed(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<(CapturedInputs, pbps_model::resolver::InputManifest), CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let manifest = captured.seal(key).map_err(|_| CaptureError::Incomplete)?;
    Ok((captured, manifest))
}

/// Seal scratch's fresh capture with the role map the qualified run made.
/// No caller-supplied text rewrite or retained-capture rekey entry is exposed.
pub async fn capture_identifying_sealed_with_roles(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    roles: &crate::resolver::authorization::RoleMap,
) -> Result<(CapturedInputs, pbps_model::resolver::InputManifest), CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let manifest = captured
        .seal_with_roles(key, Some(roles), None)
        .map_err(|_| CaptureError::Incomplete)?;
    Ok((captured, manifest))
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

/// Read and seal on behalf of the holder of catalog-read authority. A
/// previously captured result cannot be fingerprinted under a recipient's
/// chosen key: that would expose its private properties as a guessing oracle.
/// Runtime qualification is still separate; this is not verified plan evidence.
pub async fn capture_sealed(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<pbps_model::resolver::InputManifest, CaptureError> {
    capture_identifying_sealed(connection, scope, dropped, key)
        .await
        .map(|(_, manifest)| manifest)
}

/// Give the producer of a fresh coherent database read its native-input
/// capability separately from ordinary captured evidence (DEC-974.1).
///
/// The connection grants the same catalog-read authority needed to read this
/// source directly. A previously captured result cannot mint this capability.
/// Retain `RuntimeInputs` only inside the native lifecycle; do not hand it to
/// ordinary result consumers. It grants access to source-dependent operations,
/// not process/root/mapping qualification, which remains the lifecycle's job.
///
/// ```compile_fail,E0277
/// use pbps_pg::resolver::capture::{capture_with_runtime_inputs, CapturedInputs};
/// async fn acquire(captured: &mut CapturedInputs) {
///     let scope = captured.scope().clone();
///     let _ = capture_with_runtime_inputs(captured, &scope, &Default::default()).await;
/// }
/// ```
pub async fn capture_with_runtime_inputs(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
) -> Result<(CapturedInputs, RuntimeInputs), CaptureError> {
    let captured = capture_identifying(connection, scope, dropped).await?;
    let runtime = captured.runtime_inputs().map_err(CaptureError::Coverage)?;
    Ok((captured, runtime))
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

/// Source-bearing capability issued only to the producer of a fresh database
/// read by [`capture_with_runtime_inputs`], not recoverable from ordinary
/// [`CapturedInputs`]. The native lifecycle keeps it private (DEC-974.1).
/// Paths remain opaque, but resolution/open results depend on them: holders
/// are authorized to use these operations, so this is not an ordinary report
/// or a value to delegate to an untrusted result consumer.
///
/// A downstream caller cannot extract either source-bearing field:
/// ```compile_fail,E0616
/// use pbps_pg::resolver::capture::RuntimeInputs;
/// fn extract(inputs: RuntimeInputs) -> Vec<String> { inputs.libraries }
/// ```
/// ```compile_fail,E0616
/// use pbps_pg::resolver::capture::RuntimeInputs;
/// fn extract(inputs: RuntimeInputs) -> String { inputs.dynamic_library_path }
/// ```
/// Ordinary formatting cannot turn the transfer into a path report:
/// ```compile_fail,E0277
/// use pbps_pg::resolver::capture::RuntimeInputs;
/// fn report(inputs: RuntimeInputs) -> String { format!("{inputs:?}") }
/// ```
/// ```compile_fail,E0277
/// use pbps_pg::resolver::capture::RuntimeInputs;
/// fn report(inputs: RuntimeInputs) { let _ = serde_json::to_string(&inputs); }
/// ```
// Catalog capture is portable; only the Linux native lifecycle consumes these
// private fields. Keep the same opaque transfer on the other platforms too.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct RuntimeInputs {
    libraries: Vec<String>,
    dynamic_library_path: String,
}

#[cfg(target_os = "linux")]
mod native_inputs;
#[cfg(target_os = "linux")]
pub use native_inputs::{
    NativeLibrary, NativeLibraryReader, RuntimeResolution, native_library_candidates,
};

#[cfg(test)]
mod ordering_tests;

#[cfg(test)]
mod metadata_tests;
