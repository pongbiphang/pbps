//! Private target-input capture components (ADR-0016; issue #612).
//!
//! Source and derived verifiers stay in memory. No saved-plan or publication
//! path is enabled by these components; the engine adapter must qualify the
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

pub use pbps_db::resolver::capture::{CaptureError, Uncovered};

mod manifest;
pub use manifest::CapturedInputs;

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
    let mut prepared = None;
    let read = read::owned(connection, |catalog, major| {
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
///     let _ = capture_with_runtime_inputs(captured, &scope).await;
/// }
/// ```
pub async fn capture_with_runtime_inputs(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
) -> Result<(CapturedInputs, RuntimeInputs), CaptureError> {
    let captured = capture(connection, scope).await?;
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
    let current = capture(connection, previous.scope()).await?;
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
