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
