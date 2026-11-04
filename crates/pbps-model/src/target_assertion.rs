//! What a plan records of the reviewed assertions run against its target
//! (ADR-0019 §7, DEC-2401.4).
//!
//! The record is an observation made while the plan was computed. Nothing in
//! it is run again at `apply`, and a row added after `plan` is not caught
//! there; only constraints and recorded drop preconditions hold at apply.
//!
//! # What is never recorded
//!
//! A violation, an error, an invalid or unreadable result, an admission
//! refusal and a timeout all write no plan, so [`AssertionOutcome`] has no
//! variant for them. Absence of the field means "no assertion was asked for",
//! never "all passed".

/// How one assertion ended. Only a pass reaches a written plan.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AssertionOutcome {
    /// A complete, readable result with a tabular shape and zero rows.
    Pass,
}

impl AssertionOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            AssertionOutcome::Pass => "pass",
        }
    }
}

/// One assertion file, as it was when the plan was computed.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TargetAssertion {
    /// The project-relative path with `/` separators.
    pub path: crate::Text,
    /// Lowercase hex SHA-256 of the file's bytes. A file edited after the plan
    /// has another hash, so a replan differs from this record.
    pub sha256: String,
    pub outcome: AssertionOutcome,
}

impl TargetAssertion {
    /// A record a reader can trust to name a file and its content. The wire
    /// form is untrusted: an empty path or a hash of the wrong shape says
    /// nothing about any file.
    pub fn is_complete(&self) -> bool {
        !self.path.is_empty()
            && self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, sha256: &str) -> TargetAssertion {
        TargetAssertion {
            path: path.into(),
            sha256: sha256.into(),
            outcome: AssertionOutcome::Pass,
        }
    }

    #[test]
    fn a_record_names_a_file_and_a_lowercase_sha256() {
        assert!(record("checks/a.sql", &"a".repeat(64)).is_complete());
        assert!(!record("", &"a".repeat(64)).is_complete());
        assert!(!record("a.sql", &"a".repeat(63)).is_complete());
        assert!(!record("a.sql", &"A".repeat(64)).is_complete());
        assert!(!record("a.sql", &"g".repeat(64)).is_complete());
    }

    #[test]
    fn only_a_pass_can_be_read_back() {
        let ok = r#"{"path":"a.sql","sha256":"00","outcome":"pass"}"#;
        assert!(serde_json::from_str::<TargetAssertion>(ok).is_ok());
        for outcome in ["violation", "sql_error", "unanswerable", ""] {
            let json = format!(r#"{{"path":"a.sql","sha256":"00","outcome":"{outcome}"}}"#);
            assert!(
                serde_json::from_str::<TargetAssertion>(&json).is_err(),
                "{outcome}"
            );
        }
        let extra = r#"{"path":"a.sql","sha256":"00","outcome":"pass","rows":0}"#;
        assert!(serde_json::from_str::<TargetAssertion>(extra).is_err());
    }
}
