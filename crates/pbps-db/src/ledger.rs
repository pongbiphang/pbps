//! The shapes of the in-database ledger and lock (SPEC §8.1).
//!
//! # What lives here and what does not
//!
//! The types, the errors and the pruning policy are dialect-agnostic and live
//! here. The SQL that reads and writes them is T-SQL and lives in
//! `pbps_mssql::state`, beside the catalog queries — the same split `pull`
//! already uses, and the one that lets Phase 5's `pbps-pg` supply its own
//! statements without redefining what a ledger entry *is*.
//!
//! # Why the whole snapshot, and why the columns duplicate it
//!
//! `state_json` holds the entire [`StateSnapshot`]; the columns beside it
//! (`kind`, `git_sha`, `plan_checksum`, `operator`, `reason`) are a projection
//! of the same values so that `status` and `state prune` can filter and sort
//! without parsing JSON in the database. They are written *from* the snapshot at
//! record time and never edited separately, which is the only arrangement in
//! which they cannot come to disagree.
//!
//! # The trust model
//!
//! This ledger protects against mistakes and process disorder, **not** against
//! deliberate tampering: whoever can change the schema by hand can change this
//! table too. The audit baseline is git plus the CI logs; permissions should
//! give only the deployment account write access here.

use pbps_model::StateSnapshot;

use crate::DbError;

/// The tool's own tables. They are excluded from the managed set by the catalog
/// queries, so the tool never plans changes to itself.
pub const STATE_TABLE: &str = "dbo.__pbps_state";
pub const LOCK_TABLE: &str = "dbo.__pbps_lock";

/// One row of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub id: i64,

    /// When the server recorded it, ISO 8601 (`2026-08-31T09:14:22.517`).
    ///
    /// The **server's** clock, not the client's: a CI runner's clock says
    /// nothing about the environment, and two runners disagreeing would make
    /// the history unorderable.
    pub applied_at: String,

    pub snapshot: StateSnapshot,
}

/// One ledger row as a *timeline* reads it: the projected columns always, and
/// the recorded state only if this build can read it.
///
/// Separate from [`LedgerEntry`] because the two answer different questions.
/// An entry is the state itself, and a reader that cannot parse it has nothing
/// to work with — `latest` is right to refuse. A timeline is the list of what
/// happened, and every row of it exists whether or not this build understands
/// the snapshot inside: an environment upgraded across a state-format change
/// keeps rows older than `OLDEST_READABLE_VERSION`, and letting one of them
/// erase the history above it would answer "when was this database last
/// applied to?" with an error (SPEC §14.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    pub id: i64,

    /// The server's clock, as [`LedgerEntry::applied_at`].
    pub applied_at: String,

    /// The `kind` column, which is projected beside `state_json` precisely so
    /// it can be read without parsing it.
    pub kind: String,

    pub git_sha: Option<String>,
    pub plan_checksum: Option<String>,
    pub operator: String,
    pub reason: Option<String>,

    /// The recorded state, when this build can read it.
    pub snapshot: Option<StateSnapshot>,

    /// Why it could not be read, when it could not. Exactly one of this and
    /// `snapshot` is set, which is what keeps "not readable" from arriving as
    /// "nothing was recorded".
    pub unreadable: Option<String>,
}

/// Who holds `__pbps_lock`, and since when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub locked_by: String,
    pub locked_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Db(#[from] DbError),

    /// This database has never been bootstrapped, baselined or snapshotted, so
    /// there is nothing to compare against and no history to read.
    #[error(
        "this database has no `{STATE_TABLE}`: pbps has never recorded a state here.\n\
         Run `pbps baseline --db ... --reason ...` to adopt the database as it stands, \
         or `pbps bootstrap --db ...` to build it from the declarations."
    )]
    NotInitialized,

    /// Another pipeline is applying. This is the whole point of the lock: two
    /// concurrent applies against one environment produce a schema neither plan
    /// describes.
    #[error(
        "another operation holds the lock: `{}` since {}.\n\
         Wait for it to finish. If it died without releasing, `pbps unlock --db ...` clears it.",
        .0.locked_by,
        .0.locked_at
    )]
    Locked(LockInfo),

    /// `state_json` did not parse. Written by an older version, or edited by
    /// hand — either way the row cannot be trusted and must not be guessed at.
    #[error("ledger entry #{id} is unreadable: {message}")]
    BadEntry { id: i64, message: String },
}

/// How many entries `state prune` keeps by default.
///
/// Snapshots are the backup as well as the baseline, so the default errs
/// generously: a schema snapshot is kilobytes, and the cost of having thrown
/// away the one that answered "what did this look like in March" is unbounded.
pub const DEFAULT_KEEP: u32 = 50;

/// Which ids survive a prune that keeps `keep` newest entries.
///
/// Extracted from the SQL so the boundary conditions are testable without a
/// server: `keep = 0` is the one that would quietly empty the ledger.
pub fn ids_to_prune(all_ids_newest_first: &[i64], keep: u32) -> Vec<i64> {
    all_ids_newest_first
        .iter()
        .skip(keep.max(1) as usize)
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruning_keeps_the_newest() {
        assert_eq!(ids_to_prune(&[9, 8, 7, 6, 5], 2), vec![7, 6, 5]);
        assert_eq!(ids_to_prune(&[9, 8], 5), Vec::<i64>::new());
        assert_eq!(ids_to_prune(&[], 3), Vec::<i64>::new());
    }

    /// `--keep 0` would leave the environment with no baseline at all, which is
    /// drift detection switched off rather than history cleaned up. The newest
    /// entry is never prunable.
    #[test]
    fn pruning_never_removes_the_current_baseline() {
        assert_eq!(ids_to_prune(&[9, 8, 7], 0), vec![8, 7]);
        assert_eq!(ids_to_prune(&[9], 0), Vec::<i64>::new());
    }
}
