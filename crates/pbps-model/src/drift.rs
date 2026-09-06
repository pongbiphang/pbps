//! The drift report: what the database has that its recorded state does not
//! (SPEC §8.2, §9.4).
//!
//! # Why the difference is a `ChangeSet`
//!
//! Drift is a comparison of two states, which is exactly what the differ
//! already does — so the report carries the changes that would turn the
//! **recorded** state into the **live** one. A reader gets the same vocabulary
//! they read plans in, `--format json` is typed rather than prose, and the
//! `on_drift` hook receives something a script can branch on instead of a
//! message it has to grep.
//!
//! Reading it backwards is deliberate too: "the database has grown a column"
//! is what happened, and phrasing it as "the plan would drop it" would put a
//! remedy into a report whose whole job is to describe, not prescribe.
//!
//! # Why `unmanaged` sits outside the changes
//!
//! An unmanaged table is somebody else's: pbps never touches it, so its
//! presence is not drift. It is listed anyway, because "pbps sees three tables
//! here it does not manage" is exactly the context a reader needs before
//! deciding what the changes mean.

use crate::change::ChangeSet;
use crate::name::TableName;

pub const CURRENT_VERSION: u32 = 1;

/// The recorded state the live database was compared against.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct DriftBaseline {
    /// The ledger row, so an operator can find it: `SELECT * FROM
    /// dbo.__pbps_state WHERE id = ...`.
    pub entry_id: i64,

    /// When that state was recorded, by the server's clock.
    pub applied_at: String,

    pub checksum: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct DriftReport {
    pub version: u32,

    /// The environment's name, or a redacted description of the connection.
    /// Never a connection string — this JSON goes to a hook, and from there
    /// wherever the hook sends it.
    pub environment: String,

    /// `YYYY-MM-DDTHH:MM:SSZ`, by the checking machine's clock.
    pub checked_at: String,

    pub baseline: DriftBaseline,

    /// The fingerprint of the database as it is now. Equal to the baseline's
    /// exactly when there is no drift.
    pub live_checksum: String,

    /// What the live database has that the recorded state does not.
    ///
    /// Opaque to the published envelope schema (SPEC §9.8): the change model is
    /// the tool's own vocabulary, not a format anyone writes, and deriving
    /// `JsonSchema` across it would publish — and freeze — every variant of
    /// `Change` as a contract. The report carries its own `version` for a
    /// consumer that reads inside it.
    #[schemars(with = "serde_json::Value")]
    pub changes: ChangeSet,

    /// Tables in the database that pbps does not manage. Informational: their
    /// presence is not drift.
    ///
    /// Described as strings in the published schema because that is what they
    /// are on the wire: `TableName` serializes through `into = "String"`, so a
    /// derived description of its two fields would document a shape this JSON
    /// never has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(with = "Vec<String>")]
    pub unmanaged: Vec<TableName>,

    /// Differences the differ found but has no [`Change`](crate::Change) for —
    /// an `IDENTITY` that was altered by hand, say.
    ///
    /// Carried *in* the report rather than raised as a separate outcome. They
    /// are drift: the database was reached, the catalog was read, and the two
    /// states are not the same. A parallel path for them meant the `on_drift`
    /// hook never ran for this class, so a scheduled alert missed exactly the
    /// drift nobody can fix by re-running `plan`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexpressible: Vec<String>,
}

impl DriftReport {
    /// Whether anything moved.
    ///
    /// The checksum is the authority, not the change list. A difference the
    /// differ cannot phrase — a construct the model does not carry — would
    /// leave `changes` empty while the two states are plainly not the same, and
    /// "no drift" is the answer that must never be given by accident.
    pub fn has_drift(&self) -> bool {
        self.live_checksum != self.baseline.checksum
            || !self.changes.is_empty()
            || !self.unexpressible.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, PlannedChange};
    use crate::uid::Uid;

    fn report(live_checksum: &str) -> DriftReport {
        DriftReport {
            version: CURRENT_VERSION,
            environment: "prod".into(),
            checked_at: "2026-08-31T09:00:00Z".into(),
            baseline: DriftBaseline {
                entry_id: 7,
                applied_at: "2026-08-30T22:14:01.123".into(),
                checksum: "abc".into(),
            },
            live_checksum: live_checksum.into(),
            changes: ChangeSet::default(),
            unmanaged: Vec::new(),
            unexpressible: Vec::new(),
        }
    }

    /// A difference the differ has no `Change` for is still drift. It is the
    /// case `has_drift`'s own doc anticipates, and until it was carried in the
    /// report it travelled down a parallel path that skipped the `on_drift`
    /// hook — so the alert missed exactly the drift nobody can fix by rerunning
    /// `plan`.
    #[test]
    fn an_unexpressible_difference_is_drift() {
        let mut r = report("abc");
        assert!(!r.has_drift(), "the premise: matching checksum, no changes");
        r.unexpressible
            .push("the IDENTITY of dbo.t.id changed".into());
        assert!(r.has_drift());
    }

    #[test]
    fn matching_checksums_with_no_changes_is_no_drift() {
        assert!(!report("abc").has_drift());
    }

    #[test]
    fn a_different_checksum_is_drift() {
        assert!(report("def").has_drift());
    }

    /// The hottest false-negative path: the states differ in a way the differ
    /// cannot phrase, so `changes` is empty. The checksum still catches it.
    #[test]
    fn drift_the_differ_cannot_express_is_still_drift() {
        let r = report("def");
        assert!(r.changes.is_empty());
        assert!(r.has_drift());
    }

    /// And the other direction: a change the differ did phrase, against a
    /// checksum that somehow agreed. Neither signal is trusted alone.
    #[test]
    fn a_change_is_drift_even_when_the_checksums_agree() {
        let mut r = report("abc");
        r.changes
            .changes
            .push(PlannedChange::new(Change::DropTable {
                uid: "t_a1b2c3".parse::<Uid>().unwrap(),
                name: "dbo.customer".parse().unwrap(),
            }));
        assert!(r.has_drift());
    }

    /// Somebody else's table is not drift; that is what makes coexisting with
    /// other tooling possible at all.
    #[test]
    fn an_unmanaged_table_is_not_drift() {
        let mut r = report("abc");
        r.unmanaged.push("dbo.other_tool".parse().unwrap());
        assert!(!r.has_drift());
    }

    #[test]
    fn round_trips_through_json() {
        let mut r = report("def");
        r.changes
            .changes
            .push(PlannedChange::new(Change::DropTable {
                uid: "t_a1b2c3".parse::<Uid>().unwrap(),
                name: "dbo.customer".parse().unwrap(),
            }));
        r.unmanaged.push("dbo.other_tool".parse().unwrap());
        let back: DriftReport = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }
}
