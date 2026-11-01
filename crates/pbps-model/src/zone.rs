//! The time zone a target reads offset-less date/time values in (DEC-1916.1).
//!
//! A `timestamptz` or `timetz` written without an offset — `2026-02-01`, a
//! `data:` value, a default, a partition bound — is read in the session's
//! `TimeZone`, and so is the text the engine renders back. pbps used to pin
//! that to UTC; it now reads it in the target database's own zone, the one a
//! fresh session of the deploying connection gets before any pin: server,
//! then database, then role setting, then the connection's startup options.
//! `plan --db` records it, and every pin and read scope replays it.

/// The `TimeZone` text a target reported for a fresh session.
///
/// Held as the engine spelled it and replayed byte for byte, never parsed or
/// normalised. The engine's own spellings are not portable to reason about:
/// measured on 18.6, `SET TimeZone = '+08:00'` is a POSIX zone eight hours
/// *west* of UTC, `<+08>-08` is one eight hours east, and `localtime` is
/// whatever the server's system zone is. Replaying the exact text is the one
/// reading that is the target's by construction. So two spellings of one zone
/// (`UTC` and `Etc/UTC`) compare unequal, and a check that compares them
/// refuses: a spelling difference costs a DBA one `ALTER`, while a real
/// difference accepted would store every offset-less value at another instant.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct SessionZone(String);

/// Why a text cannot be a [`SessionZone`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZoneError {
    #[error("the time zone is empty")]
    Empty,
    #[error("the time zone {0:?} contains a control character")]
    Control(String),
}

impl SessionZone {
    /// The zone as the engine reported it.
    ///
    /// # Errors
    /// An empty text, or one with a control character: neither is a zone any
    /// engine reports, and either replayed into a pin would be a statement
    /// pbps did not mean to write.
    pub fn new(text: impl Into<String>) -> Result<Self, ZoneError> {
        let text = text.into();
        if text.is_empty() {
            return Err(ZoneError::Empty);
        }
        if text.chars().any(char::is_control) {
            return Err(ZoneError::Control(text));
        }
        Ok(Self(text))
    }

    /// UTC, for a read whose rendering must not depend on any session's zone:
    /// one compared only with another read, on a server whose own zone may
    /// differ (the resolver's captures of the target and of its scratch).
    #[must_use]
    pub fn utc() -> Self {
        Self("UTC".to_owned())
    }

    /// UTC, for a path that has no target to ask.
    ///
    /// The offline paths — a preview plan, a target-less `bootstrap --sql` —
    /// keep the zone they always had until #1918 gives them the declared one
    /// or a refusal. Named rather than spelled at each of them, so that change
    /// is one search.
    #[must_use]
    pub fn offline_utc() -> Self {
        Self::utc()
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionZone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SessionZone {
    type Error = ZoneError;

    fn try_from(text: String) -> Result<Self, ZoneError> {
        Self::new(text)
    }
}

impl From<SessionZone> for String {
    fn from(zone: SessionZone) -> String {
        zone.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zone_keeps_the_engines_spelling_byte_for_byte() {
        for text in ["Asia/Taipei", "Etc/UTC", "<+08>-08", "+08:00", "localtime"] {
            assert_eq!(SessionZone::new(text).unwrap().as_str(), text);
        }
    }

    #[test]
    fn two_spellings_of_one_zone_are_not_equal() {
        assert_ne!(
            SessionZone::new("UTC").unwrap(),
            SessionZone::new("Etc/UTC").unwrap()
        );
    }

    #[test]
    fn an_empty_zone_is_refused() {
        assert_eq!(SessionZone::new(""), Err(ZoneError::Empty));
    }

    #[test]
    fn a_zone_with_a_control_character_is_refused() {
        assert!(matches!(
            SessionZone::new("UTC\n; DROP TABLE t"),
            Err(ZoneError::Control(_))
        ));
    }

    #[test]
    fn a_saved_zone_that_is_empty_does_not_deserialize() {
        assert!(serde_json::from_str::<SessionZone>("\"\"").is_err());
        assert_eq!(
            serde_json::from_str::<SessionZone>("\"Asia/Taipei\"").unwrap(),
            SessionZone::new("Asia/Taipei").unwrap()
        );
    }
}
