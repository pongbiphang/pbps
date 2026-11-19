//! Which build wrote a persisted artifact, and the rule for moving a format
//! number (#2412, DEC-2412.2).
//!
//! A format number says which *release* an artifact belongs to. Before the
//! first release it is a label only, so what tells two development builds
//! apart is the fingerprint recorded next to it: [`plan::format_identity`] for a
//! saved plan, [`state::format_identity`] for a ledger snapshot. Each writer
//! records its own through one `stamp()`, and each reader asks [`Format::check`]
//! on the raw envelope before the strict parse, so no reader and no writer
//! compares the two itself.
//!
//! [`plan::format_identity`]: crate::plan::format_identity
//! [`state::format_identity`]: crate::state::format_identity

use std::sync::OnceLock;

use crate::{plan, state};

/// A persisted format whose artifacts record the build that wrote them.
pub struct Format {
    /// How a refusal names it: `plan` or `state`.
    pub name: &'static str,
    /// The number this build writes.
    pub current: u32,
    /// The value this build records next to the number. A function, because it
    /// is generated once per process on first use.
    identity: fn() -> &'static str,
}

/// The saved-plan file.
pub const PLAN: Format = Format {
    name: "plan",
    current: plan::CURRENT_VERSION,
    identity: plan::format_identity,
};

/// The ledger snapshot, in a row and in a state file.
pub const STATE: Format = Format {
    name: "state",
    current: state::CURRENT_VERSION,
    identity: state::format_identity,
};

/// How many digits of a fingerprint a message shows.
const SHOWN: usize = 12;

impl Format {
    /// The value this build records.
    pub fn identity(&self) -> &'static str {
        (self.identity)()
    }

    /// Refuses an artifact at this build's number that another build wrote.
    ///
    /// Only the current number is asked: an older or a newer one keeps its own
    /// diagnostic, and the value is not even generated for it. At the current
    /// number the three cases are three results: equal is accepted, different
    /// and absent are refused. An absent value cannot be told from a deleted
    /// one, and a deleted one would otherwise skip the whole check.
    pub fn check(&self, number: u32, recorded: Option<&str>) -> Result<(), ForeignBuild> {
        if number != self.current || recorded == Some(self.identity()) {
            return Ok(());
        }
        Err(ForeignBuild {
            format: self.name,
            number,
            recorded: recorded.map(str::to_owned),
            current: self.identity().to_owned(),
        })
    }

    /// The `version` and `format_fingerprint` members of a hand-written artifact at
    /// this build's number, for a test that builds JSON text (without a trailing
    /// comma). A test that wrote the number alone would be refused as another
    /// build's, which is the point of the check.
    #[doc(hidden)]
    pub fn stamp_members(&self) -> String {
        format!(
            "\"version\": {}, \"format_fingerprint\": \"{}\"",
            self.current,
            self.identity()
        )
    }

    /// [`check`](Self::check) on the raw text, before the strict parse.
    ///
    /// A file from another development build usually fails the strict parse
    /// ("unknown field"), which names a field and not the cause. Text that has
    /// no readable envelope is not this check's to judge and passes.
    pub fn check_raw(&self, raw: &str) -> Result<(), ForeignBuild> {
        #[derive(serde::Deserialize)]
        struct Envelope {
            version: u32,
            #[serde(default)]
            format_fingerprint: Option<String>,
        }
        match serde_json::from_str::<Envelope>(raw) {
            Ok(envelope) => self.check(envelope.version, envelope.format_fingerprint.as_deref()),
            Err(_) => Ok(()),
        }
    }
}

/// An artifact at this build's number that another development build wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignBuild {
    format: &'static str,
    number: u32,
    recorded: Option<String>,
    current: String,
}

impl ForeignBuild {
    /// The command that makes an artifact of this build, in the sentence the
    /// number refusals already use for it.
    pub fn remedy(&self) -> &'static str {
        if self.format == PLAN.name {
            "Recompute it with `pbps plan --db ... --out ...` on this build and review the new \
             artifact for approval."
        } else {
            "Re-record it with `pbps baseline --reason ...`; a state file is taken again from a \
             database this build recorded."
        }
    }
}

impl std::fmt::Display for ForeignBuild {
    /// Continues a sentence whose subject the caller names.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |text: &str| text.chars().take(SHOWN).collect::<String>();
        write!(
            f,
            "was written by a different development build of pbps ({} format {}, ",
            self.format, self.number
        )?;
        match &self.recorded {
            Some(recorded) => write!(
                f,
                "fingerprint {}",
                crate::escape::Escaped(&shown(recorded))
            )?,
            None => write!(f, "no fingerprint recorded")?,
        }
        write!(f, "; this build's fingerprint is {})", shown(&self.current))
    }
}

/// A released format's number and fingerprint, frozen at the release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frozen {
    pub version: u32,
    pub fingerprint: String,
}

/// What the last release froze. Written only by the release step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Released {
    pub tag: String,
    pub plan: Frozen,
    pub state: Frozen,
    pub schema_set: Frozen,
}

/// Reads the record file's text: `{"released": null}`, or `{"released": {...}}`.
///
/// One parser for the loader below and for the release step's own check of what
/// it wrote, so the two cannot come to accept different files.
pub fn parse_record(text: &str) -> Result<Option<Released>, serde_json::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        released: Option<Released>,
    }
    serde_json::from_str::<Record>(text).map(|record| record.released)
}

/// The last release's frozen formats, or `None` while no release exists.
///
/// The data file sits beside this module so that the crate that reads it owns
/// it, and the schema-set rule of a later change reads it through here too.
pub fn released() -> Option<&'static Released> {
    static RECORD: OnceLock<Option<Released>> = OnceLock::new();
    RECORD
        .get_or_init(|| {
            parse_record(include_str!("released_formats.json")).expect(
                "released_formats.json is read by a test and edited only by the release step",
            )
        })
        .as_ref()
}

/// The increment rule, a pure function of what is compared.
///
/// With nothing released, nothing is asserted about the number: before the
/// first release it is a label and the fingerprint tells builds apart. After a
/// release, the number moves exactly when the fingerprint does, and by one,
/// both ways, so a bump that changes nothing fails as well as a change that
/// forgot its bump.
pub fn check_increment(
    name: &str,
    number: u32,
    fingerprint: &str,
    frozen: Option<&Frozen>,
) -> Result<(), String> {
    let Some(frozen) = frozen else {
        return Ok(());
    };
    let (wanted, why) = if fingerprint == frozen.fingerprint {
        (frozen.version, "has not changed since the release")
    } else {
        (frozen.version + 1, "has changed since the release")
    };
    if number == wanted {
        return Ok(());
    }
    Err(format!(
        "the {name} format {why}, so its number must be {wanted} (the release froze {}); it is {number}",
        frozen.version
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fingerprint no build records: all `f`, which a SHA-256 of this
    /// format's schema is not.
    fn another_build() -> String {
        "f".repeat(64)
    }

    fn is_digest(text: &str) -> bool {
        text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn each_format_records_one_digest_and_they_are_not_the_same_value() {
        for format in [&PLAN, &STATE] {
            assert!(is_digest(format.identity()), "{}", format.name);
        }
        assert_ne!(PLAN.identity(), STATE.identity());
    }

    #[test]
    fn the_same_number_and_fingerprint_is_accepted() {
        for format in [&PLAN, &STATE] {
            assert_eq!(
                format.check(format.current, Some(format.identity())),
                Ok(())
            );
        }
    }

    #[test]
    fn the_same_number_with_another_fingerprint_is_refused_naming_both_builds() {
        let foreign = PLAN
            .check(PLAN.current, Some(&"ab".repeat(32)))
            .expect_err("another development build");
        assert_eq!(
            foreign.to_string(),
            format!(
                "was written by a different development build of pbps (plan format {}, \
                 fingerprint abababababab; this build's fingerprint is {})",
                PLAN.current,
                &PLAN.identity()[..SHOWN]
            )
        );
        assert!(foreign.remedy().contains("pbps plan --db ... --out ..."));
        let foreign = STATE
            .check(STATE.current, Some(&"ab".repeat(32)))
            .expect_err("another development build");
        assert!(foreign.to_string().contains("state format"), "{foreign}");
        assert!(foreign.remedy().contains("pbps baseline --reason ..."));
    }

    /// Absent, different and equal are three results. If an absent value were
    /// accepted, deleting the field would skip the whole check.
    #[test]
    fn the_same_number_with_no_fingerprint_is_refused_and_says_so() {
        for format in [&PLAN, &STATE] {
            let foreign = format.check(format.current, None).expect_err("no value");
            assert!(
                foreign.to_string().contains("no fingerprint recorded"),
                "{foreign}"
            );
            assert!(!foreign.to_string().contains("fingerprint ab"), "{foreign}");
        }
    }

    /// An older or a newer number keeps the diagnostic it has today, which is
    /// the callers': this check neither refuses it nor generates the value.
    #[test]
    fn another_number_is_not_this_checks_to_judge() {
        for format in [&PLAN, &STATE] {
            for number in [format.current - 1, format.current + 1] {
                assert_eq!(format.check(number, None), Ok(()));
                assert_eq!(format.check(number, Some(&another_build())), Ok(()));
            }
        }
    }

    /// Before the strict parse: a file from another development build usually
    /// holds a field this build does not know, and must be refused for its
    /// build rather than for that field.
    #[test]
    fn the_raw_check_reads_the_envelope_and_not_the_rest() {
        let number = PLAN.current;
        let unknown = format!(
            r#"{{"version":{number},"invented":true,"format_fingerprint":"{}"}}"#,
            another_build()
        );
        assert!(PLAN.check_raw(&unknown).is_err());
        let missing = format!(r#"{{"version":{number},"invented":true}}"#);
        assert!(PLAN.check_raw(&missing).is_err());
        let same = format!(
            r#"{{"version":{number},"invented":true,"format_fingerprint":"{}"}}"#,
            PLAN.identity()
        );
        assert_eq!(PLAN.check_raw(&same), Ok(()));
        // Negative: text with no readable envelope is the callers' to refuse.
        let duplicate = format!(r#"{{"version":{number},"version":{}}}"#, number + 1);
        for text in ["", "{", "null", "[]", r#"{"version":"x"}"#, &duplicate] {
            assert_eq!(PLAN.check_raw(text), Ok(()), "{text}");
        }
        assert_eq!(
            PLAN.check_raw(&format!(r#"{{"version":{}}}"#, number + 1)),
            Ok(())
        );
    }

    #[test]
    fn a_hostile_fingerprint_is_cut_and_shown_escaped() {
        let foreign = PLAN
            .check(
                PLAN.current,
                Some("\u{1b}[31m\nred\u{202e}rest of a very long line"),
            )
            .expect_err("another build");
        let shown = foreign.to_string();
        assert!(
            !shown.contains('\u{1b}') && !shown.contains('\n') && !shown.contains('\u{202e}'),
            "{shown:?}"
        );
        assert!(!shown.contains("long line"), "{shown}");
    }

    #[test]
    fn the_stamp_members_name_the_number_and_the_value() {
        assert_eq!(
            STATE.stamp_members(),
            format!(
                r#""version": {}, "format_fingerprint": "{}""#,
                STATE.current,
                STATE.identity()
            )
        );
    }

    fn frozen(version: u32, fingerprint: &str) -> Frozen {
        Frozen {
            version,
            fingerprint: fingerprint.to_owned(),
        }
    }

    #[test]
    fn nothing_is_asserted_about_the_number_while_nothing_is_released() {
        for number in [0, 1, PLAN.current, u32::MAX] {
            assert_eq!(
                check_increment("plan", number, &another_build(), None),
                Ok(())
            );
        }
    }

    #[test]
    fn an_unchanged_fingerprint_keeps_the_released_number() {
        let released = frozen(7, &another_build());
        assert_eq!(
            check_increment("plan", 7, &another_build(), Some(&released)),
            Ok(())
        );
        // Negative: a bump that changed nothing, and a step back.
        for number in [8, 6] {
            let error = check_increment("plan", number, &another_build(), Some(&released))
                .expect_err("the number moved without a change");
            assert!(
                error.contains("has not changed") && error.contains("must be 7"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_changed_fingerprint_moves_the_number_by_one() {
        let released = frozen(7, &"0".repeat(64));
        assert_eq!(
            check_increment("state", 8, &another_build(), Some(&released)),
            Ok(())
        );
        // Negative: a change that forgot its bump, and one that jumped.
        for number in [7, 9] {
            let error = check_increment("state", number, &another_build(), Some(&released))
                .expect_err("the number does not follow the change");
            assert!(
                error.contains("has changed") && error.contains("must be 8"),
                "{error}"
            );
        }
    }

    #[test]
    fn this_build_follows_the_increment_rule_against_the_released_record() {
        let released = released();
        for (format, frozen) in [
            (&PLAN, released.map(|r| &r.plan)),
            (&STATE, released.map(|r| &r.state)),
        ] {
            assert_eq!(
                check_increment(format.name, format.current, format.identity(), frozen),
                Ok(())
            );
        }
    }

    /// The release step is the only writer of this file, so a malformed one is
    /// a mistake of that step and is caught here, not at the first read.
    #[test]
    fn the_released_record_is_well_formed() {
        if let Some(released) = released() {
            assert!(!released.tag.is_empty());
            for frozen in [&released.plan, &released.state, &released.schema_set] {
                assert!(
                    frozen.version >= 1 && is_digest(&frozen.fingerprint),
                    "{frozen:?}"
                );
            }
        }
        let text = include_str!("released_formats.json");
        assert!(
            serde_json::from_str::<serde_json::Value>(text)
                .unwrap()
                .get("released")
                .is_some()
        );
        // Negative: the shape refuses an unnamed format and an unknown member.
        let record = r#"{"tag":"v1","plan":{"version":1,"fingerprint":"x"},"state":{"version":1,"fingerprint":"x"}}"#;
        assert!(serde_json::from_str::<Released>(record).is_err());
        let extra = r#"{"version":1,"fingerprint":"x","note":"y"}"#;
        assert!(serde_json::from_str::<Frozen>(extra).is_err());
    }
}
