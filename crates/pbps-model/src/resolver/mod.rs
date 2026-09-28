//! Source-free, versioned resolution evidence, beside semantic `Schema`.
//!
//! These are artifact values, not a claim that an arbitrary catalog capture
//! qualified a runtime. The connected producer still owns that qualification.
//! Every digest of captured input is keyed; no external definition belongs in
//! these types (DEC-952.1).

mod manifest;
pub use manifest::{
    Binding, CandidateSet, InputManifest, ManifestError, Membership, ObjectIdentity, Prerequisite,
    ReadScope, RoutineLookup,
};

mod ordering;
pub use ordering::{
    BoundSurface, OrderEdge, OrderError, OrderReason, OrderingProof, Surface, SurfaceResolution,
};

mod ownership;
pub use ownership::ObjectOwnership;

mod projection;
pub use projection::ObjectTransition;

mod evidence;
pub use evidence::{
    AuthorizationCondition, BindingCoverage, EvidenceError, EvidenceHandling, PlanAnalysis,
    Qualification, ResolverEvidence, ResolverRuntime,
};
