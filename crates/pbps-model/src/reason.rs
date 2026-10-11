//! A human's explanation for new audit intent, separate from historical text.

use std::{fmt, str::FromStr};

/// A reason must explain something; whitespace alone cannot supply intent.
/// Historical tombstones and snapshots keep `String` so records written by
/// older versions remain readable (DEC-1796.1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Reason(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an audit reason must not be empty or whitespace-only")]
pub struct ReasonError;

impl Reason {
    /// Preserve meaningful input verbatim: trimming decides validity only.
    pub fn new(value: impl Into<String>) -> Result<Self, ReasonError> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(ReasonError)
        } else {
            Ok(Self(value))
        }
    }
}

impl TryFrom<String> for Reason {
    type Error = ReasonError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl FromStr for Reason {
    type Err = ReasonError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<Reason> for String {
    fn from(value: Reason) -> Self {
        value.0
    }
}

impl AsRef<str> for Reason {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meaningful_reasons_preserve_every_character() {
        for text in [
            "x",
            " x ",
            "刪除舊欄位",
            "REG-2026-042: \"PII\"",
            "\tapproved\n",
        ] {
            let reason: Reason = text.parse().unwrap();
            assert_eq!(reason.as_ref(), text);
            assert_eq!(reason.to_string(), text);
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(serde_json::from_str::<Reason>(&json).unwrap(), reason);
            assert_eq!(String::from(reason), text);
        }
    }

    #[test]
    fn blank_reasons_cannot_be_constructed_or_deserialized() {
        for text in ["", " ", "   ", "\t", "\n\t ", "\u{3000}", "\u{a0}"] {
            assert_eq!(Reason::new(text), Err(ReasonError));
            assert_eq!(text.parse::<Reason>(), Err(ReasonError));
            let json = serde_json::to_string(text).unwrap();
            assert!(serde_json::from_str::<Reason>(&json).is_err());
        }
    }

    #[test]
    fn historical_blank_reasons_remain_readable() {
        let tombstone: crate::Tombstone = serde_json::from_value(serde_json::json!({
            "was": "app.t.old", "dropped_at": "2026-10-11",
            "operator": "old-client", "reason": " "
        }))
        .unwrap();
        assert_eq!(tombstone.reason, " ");
        let mut snapshot = crate::StateSnapshot::new(
            crate::StateKind::Baseline,
            crate::Schema::default(),
            crate::IdsFile::default(),
            "old-client",
        );
        snapshot.reason = Some(String::new());
        let back: crate::StateSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(back.reason.as_deref(), Some(""));
    }
}
