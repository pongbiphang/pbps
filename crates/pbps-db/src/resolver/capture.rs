//! Connected capture reports, separate from semantic Schema and saved plans.
//!
//! The identities and failed conditions are safe for ordinary diagnostics.
//! Definitions, prerequisite properties and their guessing verifiers are not
//! part of this report (ADR-0016). The verifiers are keyed (DEC-952.1).

/// An engine object identified without a database-local object number.
/// `name` is a sequence of identifier components, never a dotted string:
/// a schema containing a dot must not alias a different qualified name.
/// `signature` holds the engine's logical input types or owning object.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ObjectIdentity {
    pub class: String,
    pub name: Vec<String>,
    pub signature: Vec<ObjectIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputChange {
    Added,
    Removed,
    Properties,
    Bindings,
    Environment,
    Version,
    Scope,
    Membership,
    Baseline,
}

/// No source, digest, canonical properties or equivalent verifier escapes
/// through the comparison API. A changed property requires fresh planning.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CaptureDifference {
    pub object: Option<ObjectIdentity>,
    pub change: InputChange,
}

/// What comparing one surface's observed target bindings with its bindings
/// compiled on scratch proves (ADR-0016 decision 2). Identities only: no
/// definition, property or verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The target already binds what a fresh creation would.
    Unaffected,
    /// A fresh creation binds differently; the object must be rebuilt.
    Rebuild,
    /// No sound answer: the condition names what is missing.
    Unresolved { condition: &'static str },
}

/// The comparison of every managed surface both captures hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessment {
    pub surfaces: std::collections::BTreeMap<ObjectIdentity, Verdict>,
    /// Routines whose body is runtime-bound: their header and defaults are
    /// compared, their body is outside this proof (ADR-0016 decision 3).
    pub runtime_bound: std::collections::BTreeSet<ObjectIdentity>,
}

/// A safe, named coverage refusal; no source or property value is included.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("target capture cannot cover {class}: {condition}")]
pub struct Uncovered {
    pub class: String,
    pub object: Option<ObjectIdentity>,
    pub condition: &'static str,
}

impl Uncovered {
    pub fn class(class: &str, condition: &'static str) -> Self {
        Self {
            class: class.into(),
            object: None,
            condition,
        }
    }
    pub fn object(object: &ObjectIdentity, condition: &'static str) -> Self {
        Self {
            class: object.class.clone(),
            object: Some(object.clone()),
            condition,
        }
    }
}

/// Source-free capture refusals shared by the engine adapters and CLI routing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("target input capture is not implemented for {engine}")]
    Unsupported { engine: &'static str },
    #[error(transparent)]
    Coverage(#[from] Uncovered),
    #[error("target capture requires its own transaction")]
    CallerTransaction,
    #[error("target capture cannot qualify this engine version")]
    Version,
    #[error("target capture could not read a required catalog")]
    Read,
    #[error("target capture received incomplete catalog input")]
    Incomplete,
    #[error("catalog changed while canonical definitions were rendered")]
    Changed,
    #[error("session inputs changed during target capture")]
    EnvironmentChanged,
    #[error("target capture could not close its read transaction")]
    Close,
}
