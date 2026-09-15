//! Read-only environment discovery (ADR-0016 decision 4), not plan evidence.
//!
//! These are connected report shapes, like `doctor::Ask`. Engine crates own
//! the queries and candidate rules. Discovery cannot construct a qualified
//! resolver or authorize compilation, publication or apply.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Observation {
    Observed {
        value: String,
    },
    /// SQL NULL: absent, hidden or unsupported cannot be distinguished here.
    NotReported,
    Unknown {
        reason: String,
    },
}

impl Observation {
    pub fn reported(value: Option<&str>) -> Self {
        match value {
            Some(value) => Self::Observed {
                value: value.into(),
            },
            None => Self::NotReported,
        }
    }

    pub fn value(&self) -> Option<&str> {
        match self {
            Self::Observed { value } => Some(value),
            Self::NotReported | Self::Unknown { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Candidate {
    Suggested { image: String },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct Extension {
    pub name: String,
    pub schema: String,
    pub version: String,
}

/// A separate single-variant type prevents an advisory query from claiming
/// the verified compatibility reserved for a qualified backend/run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DiscoveryCompatibility {
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct Discovery {
    /// Observations describe this introspection session, not the future
    /// deployment's per-statement settings or authorization. No consistent
    /// evidence capture, fingerprint or compatibility comparison is implied.
    pub observations: BTreeMap<String, Observation>,
    /// PostgreSQL's installed extension inventory, without definitions or
    /// native-library identities. None means this inventory is not applicable
    /// to the engine; a failed inventory query returns an error instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<Extension>>,
    pub candidate: Candidate,
    pub compatibility: DiscoveryCompatibility,
    /// Conditions discovery cannot establish. These remain advisory to
    /// ordinary deployment readiness, never proof that resolution can run.
    pub qualification: BTreeMap<String, Observation>,
}

impl Discovery {
    pub fn unverified(
        observations: BTreeMap<String, Observation>,
        extensions: Option<Vec<Extension>>,
        candidate: Candidate,
    ) -> Self {
        let qualification = [
            ("engine-and-native-builds", "Qualify the actual backend's loaded and required executable content; reported versions alone do not prove compatibility."),
            ("deployment-context", "Qualify per-statement deployment settings, authorization, memberships, ownership and visibility; introspection-session observations are not that context."),
            ("candidate-compatibility", "Verify the running candidate against the required engine/product, versions, extensions, locale and settings; an image suggestion is not a match."),
            ("transport", "Qualify authenticated peer-verified evidence channels before accepting evidence or transferring declarations."),
            ("instance-separation", "Prove that the actual resolver backend is outside the target cluster or instance before scratch DDL."),
            ("execution-containment", "Qualify runtime-enforced network/filesystem containment, bounded resources and lifetime."),
            ("run-stability", "Qualify backend continuity and protection against relevant in-place changes throughout compilation."),
            ("private-input-handling", "If external definitions are needed, qualify private transport, logging, storage and confidential artifact handling, including the legacy-reader prerequisite (#594)."),
            ("binding-adapter", "Binding resolution is not implemented; discovery cannot supply evidence or replace existing planning protections."),
        ].into_iter().map(|(key, reason)| (key.into(), Observation::Unknown { reason: reason.into() })).collect();
        Self {
            observations,
            extensions,
            candidate,
            compatibility: DiscoveryCompatibility::Unverified,
            qualification,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreported_and_unknown_facts_cannot_be_read_as_observed_empty_values() {
        assert_eq!(Observation::reported(Some("")).value(), Some(""));
        for fact in [
            Observation::reported(None),
            Observation::Unknown {
                reason: "not qualified".into(),
            },
        ] {
            assert_eq!(fact.value(), None);
            assert!(serde_json::to_value(fact).unwrap().get("value").is_none());
        }
    }
}
