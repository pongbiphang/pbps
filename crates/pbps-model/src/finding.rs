//! A policy finding: what an analyzer said about a declaration or a change
//! ([ADR-0008](../../../docs/ADR-0008-policies.md)).
//!
//! # Why this is not a [`RiskClass`](crate::RiskClass)
//!
//! A risk class is the closed set the deployment gate reads, and `--allow`
//! names its members. A finding needs three things a gate class has not: an
//! id a project can re-weight, a severity the project decides, and a
//! suppression. Putting those on the risk enum would turn every lint into a
//! flag. So findings live **beside** risks on a planned change, and beside the
//! declarations for the rules `validate` runs, and neither reads the other.

use std::fmt;
use std::str::FromStr;

/// How seriously a finding is taken — the same three levels the findings
/// envelope carries, with the same meaning: only `error` changes an exit code.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Severity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(Severity::Error),
            "warning" => Ok(Severity::Warning),
            "note" => Ok(Severity::Note),
            other => Err(format!(
                "unknown severity `{other}`; one of error, warning, note"
            )),
        }
    }
}

/// One thing a rule found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// The rule's id (`naming.column`, `change.expand-contract`): stable, and
    /// what a `policies:` block re-weights or suppresses by.
    pub id: String,
    pub severity: Severity,
    pub message: String,
    /// The object it is about, as a suppression names it: `dbo.customer`,
    /// `role app_reader`. `None` for a finding about the whole plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl Finding {
    pub fn new(
        id: impl Into<String>,
        severity: Severity,
        message: impl Into<String>,
        subject: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            severity,
            message: message.into(),
            subject,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities_read_back_from_their_own_spelling_only() {
        assert_eq!("error".parse::<Severity>().unwrap(), Severity::Error);
        assert_eq!("Warning".parse::<Severity>().unwrap(), Severity::Warning);
        assert!("fatal".parse::<Severity>().is_err());
        assert!("off".parse::<Severity>().is_err(), "off is not a severity");
    }

    #[test]
    fn a_finding_round_trips_and_omits_an_absent_subject() {
        let f = Finding::new("naming.table", Severity::Warning, "x", None);
        let json = serde_json::to_string(&f).unwrap();
        assert!(!json.contains("subject"), "{json}");
        let back: Finding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}
