//! The sealed outcome of a plan's rehearsal (SPEC §9.3.1, #2379).
//!
//! A rehearsal runs a plan's statements on a scratch engine before the plan is
//! written. This is what the plan file says about it, so a reviewer holding
//! only the file can tell a plan that was rehearsed from one that was not, and
//! `apply` can display which it is.
//!
//! # Why it is not in the resolver evidence
//!
//! The rehearsal also runs for a plan that needed no resolver, and such a plan
//! carries no resolver evidence. The record is a field of the plan itself.
//!
//! # What is never recorded
//!
//! A rehearsal that failed produces no plan, so there is no `Failed` outcome.
//! A plan with no field was written where no scratch entry was configured, or
//! before the rehearsal existed: absence is "not asked", and `Off` is the one
//! way to say "asked and declined".

use crate::resolver::{ScratchLayout, ScratchSource};

/// How a plan was rehearsed, or that it was not.
///
/// Externally tagged on purpose: serde's internally tagged unit variant drops
/// the fields it does not know, where this one refuses them (the same trap
/// [`ScratchSource::Server`] documents).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Rehearsal {
    /// The plan's statements ran in order on a scratch engine and the result
    /// matched the declarations, up to the spellings listed.
    Converged {
        /// The `pbps.yml` resolver entry that supplied the scratch.
        entry: crate::Text,
        scratch: ScratchSource,
        layout: ScratchLayout,
        /// Differences that are only the engine's normalization showing
        /// through (SPEC §9.3.1), each as the engine stored it. They do not
        /// fail a rehearsal, so they are sealed beside the plan rather than
        /// lost with the terminal scrollback.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        spelling: Vec<crate::Text>,
        /// Whether the roles, grants and row-security policies were part of
        /// what ran. A record that skipped them must never read as a full
        /// rehearsal, so the field is required: a record without it is
        /// refused, not read as either answer.
        authorization: AuthorizationRehearsal,
    },
    /// The operator turned the rehearsal off for this run or environment.
    /// Never read as "rehearsed".
    Off,
}

/// Whether a converged rehearsal ran the plan's changes to roles, grants and
/// row-security policies (#2379).
///
/// Externally tagged, as [`Rehearsal`] is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorizationRehearsal {
    /// They ran, on roles made for the run in place of the target's, and were
    /// compared like everything else.
    Rehearsed,
    /// They did not, and the plan was rehearsed without them. The scratch
    /// account could not create the roles the run maps them to.
    NotRehearsed { reason: crate::Text },
}

impl Rehearsal {
    /// A record naming no entry or no image says nothing about what ran, and
    /// one that skipped authorization without saying why says nothing about it.
    pub fn is_complete(&self) -> bool {
        match self {
            Rehearsal::Converged {
                entry,
                scratch,
                authorization,
                ..
            } => {
                !entry.raw().is_empty()
                    && !matches!(scratch, ScratchSource::Docker { image } if image.raw().is_empty())
                    && !matches!(
                        authorization,
                        AuthorizationRehearsal::NotRehearsed { reason } if reason.raw().is_empty()
                    )
            }
            Rehearsal::Off => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn converged(spelling: &[&str]) -> Rehearsal {
        Rehearsal::Converged {
            entry: "ci".into(),
            scratch: ScratchSource::Docker {
                image: "postgres:18".into(),
            },
            layout: ScratchLayout::RunOwned,
            spelling: spelling.iter().map(|s| (*s).into()).collect(),
            authorization: AuthorizationRehearsal::Rehearsed,
        }
    }

    fn unrehearsed(reason: &str) -> Rehearsal {
        Rehearsal::Converged {
            entry: "ci".into(),
            scratch: ScratchSource::Server {},
            layout: ScratchLayout::Supplied,
            spelling: Vec::new(),
            authorization: AuthorizationRehearsal::NotRehearsed {
                reason: reason.into(),
            },
        }
    }

    #[test]
    fn a_record_round_trips_and_omits_empty_spelling() {
        for record in [
            converged(&[]),
            converged(&["check c on t"]),
            unrehearsed("the scratch account lacks CREATEROLE"),
            Rehearsal::Off,
        ] {
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(serde_json::from_value::<Rehearsal>(json).unwrap(), record);
        }
        assert_eq!(
            serde_json::to_value(converged(&[])).unwrap(),
            json!({"converged": {
                "entry": "ci",
                "scratch": {"kind": "docker", "image": "postgres:18"},
                "layout": "run-owned",
                "authorization": "rehearsed",
            }})
        );
        assert_eq!(
            serde_json::to_value(unrehearsed("no CREATEROLE")).unwrap()["converged"]["authorization"],
            json!({"not_rehearsed": {"reason": "no CREATEROLE"}})
        );
        assert_eq!(serde_json::to_value(Rehearsal::Off).unwrap(), json!("off"));
    }

    #[test]
    fn the_reader_refuses_what_it_does_not_know() {
        let ok = json!({"converged": {
            "entry": "ci",
            "scratch": {"kind": "server"},
            "layout": "supplied",
            "authorization": "rehearsed",
        }});
        assert!(serde_json::from_value::<Rehearsal>(ok.clone()).is_ok());
        let mut no_entry = ok.clone();
        no_entry["converged"]
            .as_object_mut()
            .unwrap()
            .remove("entry");
        let mut extra = ok.clone();
        extra["converged"]["verified"] = true.into();
        // Neither answer may be assumed: a record that does not say whether
        // authorization ran is refused, not read as rehearsed.
        let mut silent = ok.clone();
        silent["converged"]
            .as_object_mut()
            .unwrap()
            .remove("authorization");
        let mut unreasoned = ok.clone();
        unreasoned["converged"]["authorization"] = json!({"not_rehearsed": {}});
        for (bad, why) in [
            (json!("failed"), "an unknown outcome"),
            (json!({"failed": {}}), "an unknown outcome with a body"),
            (json!({"off": {}}), "Off with a body"),
            (json!("converged"), "Converged with no fields"),
            (no_entry, "Converged with no entry"),
            (extra, "an unknown key inside the record"),
            (silent, "a record that does not say about authorization"),
            (unreasoned, "authorization skipped with no reason field"),
            (
                json!({"converged": ok["converged"], "off": null}),
                "two outcomes",
            ),
        ] {
            assert!(serde_json::from_value::<Rehearsal>(bad).is_err(), "{why}");
        }
    }

    /// A hand edit of a plan file, applied to its JSON.
    type Edit = fn(&mut serde_json::Value);

    fn plan_with(record: Option<Rehearsal>) -> crate::SavedPlan {
        let mut plan = crate::SavedPlan::new(
            crate::PlanOrigin::Database,
            "postgres",
            "2026-10-25T00:00:00Z",
            crate::PlanBaseline {
                description: "prod as queried".into(),
                checksum: "00".repeat(32),
                database_collation: None,
                time_zone: None,
            },
            crate::ChangeSet::default(),
            crate::IdsFile::default(),
        );
        plan.rehearsal = record;
        plan
    }

    /// Plans written before the field existed, and plans written where no
    /// scratch entry is configured, keep their bytes: the field is omitted,
    /// not written as `null`.
    #[test]
    fn a_plan_without_a_record_keeps_its_bytes() {
        let plan = plan_with(None);
        let json = serde_json::to_value(&plan).unwrap();
        assert!(json.get("rehearsal").is_none(), "{json}");
        let back: crate::SavedPlan = serde_json::from_value(json).unwrap();
        assert_eq!(back.rehearsal, None);
        assert_eq!(back.wire_fingerprint(), plan.wire_fingerprint());
    }

    #[test]
    fn a_record_in_a_plan_round_trips() {
        for record in [converged(&[]), converged(&["check c on t"]), Rehearsal::Off] {
            let plan = plan_with(Some(record));
            let back: crate::SavedPlan =
                serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
            assert_eq!(back, plan);
            back.validate_analysis().unwrap();
        }
    }

    /// The fingerprint hashes the whole file, so a record edited by hand, or
    /// dropped, is a different plan from the one approved.
    #[test]
    fn editing_or_dropping_the_record_changes_the_plan_fingerprint() {
        let sealed = plan_with(Some(converged(&["check c on t"])));
        let fingerprint = sealed.wire_fingerprint();
        let original = serde_json::to_value(&sealed).unwrap();
        let edits: [(&str, Edit); 6] = [
            ("dropped", |v| {
                v.as_object_mut().unwrap().remove("rehearsal");
            }),
            ("turned off", |v| v["rehearsal"] = json!("off")),
            ("spelling edited", |v| {
                v["rehearsal"]["converged"]["spelling"][0] = "check c on u".into()
            }),
            ("spelling removed", |v| {
                v["rehearsal"]["converged"]
                    .as_object_mut()
                    .unwrap()
                    .remove("spelling");
            }),
            ("entry edited", |v| {
                v["rehearsal"]["converged"]["entry"] = "other".into()
            }),
            ("authorization edited", |v| {
                v["rehearsal"]["converged"]["authorization"] =
                    json!({"not_rehearsed": {"reason": "x"}})
            }),
        ];
        for (what, edit) in edits {
            let mut edited = original.clone();
            edit(&mut edited);
            let changed: crate::SavedPlan = serde_json::from_value(edited).unwrap();
            assert_ne!(changed.wire_fingerprint(), fingerprint, "{what}");
        }
    }

    #[test]
    fn a_plan_refuses_a_record_it_cannot_read() {
        let original = serde_json::to_value(plan_with(Some(Rehearsal::Off))).unwrap();
        for bad in [
            json!("failed"),
            json!({"converged": {"scratch": {"kind": "server"}, "layout": "supplied",
                "authorization": "rehearsed"}}),
            json!({"converged": {"entry": "ci", "scratch": {"kind": "server"},
                "layout": "supplied", "authorization": "rehearsed", "verified": true}}),
        ] {
            let mut json = original.clone();
            json["rehearsal"] = bad.clone();
            assert!(
                serde_json::from_value::<crate::SavedPlan>(json).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_record_naming_nothing_is_incomplete() {
        assert!(converged(&[]).is_complete());
        assert!(Rehearsal::Off.is_complete());
        let nameless = Rehearsal::Converged {
            entry: "".into(),
            scratch: ScratchSource::Server {},
            layout: ScratchLayout::Supplied,
            spelling: Vec::new(),
            authorization: AuthorizationRehearsal::Rehearsed,
        };
        let imageless = Rehearsal::Converged {
            entry: "ci".into(),
            scratch: ScratchSource::Docker { image: "".into() },
            layout: ScratchLayout::RunOwned,
            spelling: Vec::new(),
            authorization: AuthorizationRehearsal::Rehearsed,
        };
        let unexplained = unrehearsed("");
        assert!(!nameless.is_complete());
        assert!(!imageless.is_complete());
        assert!(!unexplained.is_complete());
        assert!(unrehearsed("no CREATEROLE").is_complete());
        // The plan reader refuses what the record alone would accept.
        for record in [nameless, imageless, unexplained] {
            assert_eq!(
                plan_with(Some(record)).validate_analysis(),
                Err(crate::resolver::EvidenceError::Incomplete)
            );
        }
    }
}
