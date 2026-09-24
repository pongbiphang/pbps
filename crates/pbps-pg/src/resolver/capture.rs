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

/// A safe, named coverage refusal; no source or property value is included.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("PostgreSQL capture cannot cover {class}: {condition}")]
pub struct Uncovered {
    pub class: String,
    pub object: Option<ObjectIdentity>,
    pub condition: &'static str,
}

impl Uncovered {
    fn class(class: &str, condition: &'static str) -> Self {
        Self {
            class: class.into(),
            object: None,
            condition,
        }
    }
    fn object(object: &ObjectIdentity, condition: &'static str) -> Self {
        Self {
            class: object.class.clone(),
            object: Some(object.clone()),
            condition,
        }
    }
}

mod manifest;
pub use manifest::CapturedInputs;
pub use read::Failure as CaptureError;

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

/// Private transfer to the native lifecycle, not a report or saved artifact.
/// Paths can come from retained source, so this has no Debug or Serialize.
pub struct RuntimeInputs {
    pub libraries: Vec<String>,
    pub dynamic_library_path: String,
}
