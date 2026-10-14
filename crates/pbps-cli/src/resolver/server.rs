//! The resolver's scratch run and the types its callers share (DEC-1528.1).
//!
//! A run compiles the managed declarations on a scratch server the operator
//! vouches for, either supplied (`kind: server`) or started for the run
//! (`kind: docker`, #1674), and seals what it read into the plan's evidence.
//! [`scratch`] is the run; this module holds what its callers and it share.

use crate::resolver::scope::PlannedGrant;
use pbps_db::resolver::ScratchNames;

pub(crate) mod engine;
mod producer;
pub use producer::{ProduceError, Rechecked, Refused, Unsealed};
mod resolution;
pub mod scratch;
mod transitions;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("the run-owned scratch resources could not be created on the scratch server")]
    Scratch,
    #[error("the analysis scope could not be qualified or changed under the run: {0}")]
    Scope(String),
    /// The scope was read in full and differs: an answered incompatibility,
    /// not a failed read (SPEC 9.8, DEC-1515.1).
    #[error("the resolver environment is incompatible with the target: {}", .0.join("; "))]
    Incompatible(Vec<String>),
    #[error("the desired bindings could not be resolved on this run: {0}")]
    Binding(String),
    /// A catalog read during resolution failed or was cut short: nothing was
    /// answered about the bindings, so it is not a [`Error::Binding`] verdict
    /// (SPEC 9.8, #1575).
    #[error("a catalog read during resolution failed: {0}")]
    Read(String),
    /// The resolver's scratch cannot be used as configured: it is the
    /// target, shares its cluster without a confined account, or is not
    /// empty (#1667, #1672). Nothing was written on it.
    #[error("the resolver's scratch cannot be used: {0}")]
    Unusable(String),
    /// The resolver's externals file failed on scratch, or what it staged is not
    /// the target's (#1673). Each finding names the object or statement.
    #[error("the resolver's externals file was refused: {}", .0.join("; "))]
    Externals(Vec<String>),
    /// The publication recheck found the target other than the run read
    /// it: another cluster, or a changed fact, authorization or captured
    /// object, each named (#1516). Nothing is published.
    #[error("the target changed after the resolver read it: {}", .0.join("; "))]
    Changed(Vec<String>),
}

/// What a run is asked to qualify: the schemas its plan writes to, the extras
/// the dialect appends after each on the write path, and the grants the plan
/// performs before its DDL. Pure data supplied by the caller.
#[derive(Debug, Clone, Default)]
pub struct ScopeRequest {
    pub schemas: Vec<String>,
    pub write_path_extras: Vec<String>,
    pub planned: Vec<PlannedGrant>,
}

/// What a run compiles and compares. Pure data supplied by the planner.
#[derive(Debug, Clone, Copy)]
pub struct BindingRequest<'a> {
    /// The plan from an empty schema to `desired`, as the differ produces it.
    pub bootstrap: &'a [pbps_model::Change],
    /// The declarations. Scratch's user schemas hold these and nothing else.
    pub desired: &'a pbps_model::Schema,
    /// The target's managed side, by the names pbps's own model gives it.
    pub base: &'a pbps_model::Schema,
}

/// Complete same-capture qualified plan. It contains only ordered typed
/// changes and keyed evidence; no raw target or scratch capture escapes.
pub struct ResolvedPlan {
    pub changes: pbps_model::ChangeSet,
    pub evidence: pbps_model::resolver::ResolverEvidence,
}

struct ProducerOutcome {
    compiled: engine::CompiledCapture,
    opening_build: String,
    surfaces: Vec<pbps_model::resolver::SurfaceResolution>,
}

pub(crate) fn generated_names() -> Result<ScratchNames, Error> {
    let suffix = || format!("{:032x}", rand::random::<u128>());
    ScratchNames::new(
        format!("pbps_scratch_{}", suffix()),
        format!("pbps_run_{}", suffix()),
        format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        ),
    )
    .map_err(|_| Error::Scratch)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod transitions_tests;
