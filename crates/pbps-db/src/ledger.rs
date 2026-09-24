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

/// The tool's own two tables, by the names SPEC §8.1 gives them. They are
/// excluded from the managed set by the catalog queries, so the tool never
/// plans changes to itself.
///
/// **The name, not the qualified spelling.** These two are the same word on
/// every engine — they are this tool's, not the database's — but the schema
/// they live in is a dialect's answer: `dbo` on SQL Server (SPEC §8.1), and
/// `public` on PostgreSQL, which has no `dbo` and whose counterpart of it is
/// the schema every database is created with. So the qualified spelling lives
/// with the statements that use it (`pbps_mssql::state::STATE_TABLE`,
/// `pbps_pg::state::STATE_TABLE`), and each of those is tested to be its own
/// schema plus the name here. A single `dbo.`-qualified constant in this
/// dialect-free crate was the shape that could not survive a second engine.
pub const STATE_TABLE_NAME: &str = "__pbps_state";
pub const LOCK_TABLE_NAME: &str = "__pbps_lock";

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

    /// The four numbers a timeline row draws, or why this build could not read
    /// them.
    ///
    /// A `Result` rather than the data beside an optional reason: exactly one
    /// of the two is true of every row, and a struct able to hold both — or
    /// neither — needs a comment where a type does the same work.
    ///
    /// [`TimelineState`], not [`StateSnapshot`] (DECISIONS 435): the common
    /// path reads `state_version`, `tables_count`, `modules_count`,
    /// `staged_completed` and `staged_total` straight off the row and never
    /// touches `state_json`, so it never has the schema or the identity
    /// mapping in hand. A field typed `StateSnapshot` that was routinely
    /// empty everywhere but four numbers would be a type lying about what it
    /// holds; `TimelineState` can hold exactly what a timeline row needs and
    /// nothing a reader might reach for and not find.
    pub state: Result<TimelineState, pbps_model::Unreadable>,
}

/// The four numbers [`crate::TimelineEntry`] draws from a recorded state.
///
/// Read straight from the ledger's projected columns for a row recorded after
/// they existed; read out of a full [`StateSnapshot`] parse for one recorded
/// before (DECISIONS 435) — either way the timeline needs only these, never
/// the schema or the identity mapping the full snapshot also carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineState {
    /// The recorded state's own format version ([`StateSnapshot::version`]).
    pub version: u32,

    /// How many tables the recorded schema has.
    pub tables: usize,

    /// How many modules the recorded schema has.
    pub modules: usize,

    /// Present only for a [`pbps_model::StateKind::Staged`] checkpoint — the
    /// same absence rule [`pbps_model::StagedProgress`] documents.
    pub staged: Option<TimelineStaged>,
}

impl TimelineState {
    /// Reads the four numbers off a fully-parsed snapshot — the fallback path
    /// for a row recorded before the projected columns existed, or written by
    /// hand (DECISIONS 435).
    pub fn from_snapshot(snapshot: &StateSnapshot) -> Self {
        TimelineState {
            version: snapshot.version,
            tables: snapshot.schema.tables.len(),
            modules: snapshot.schema.modules.len(),
            staged: snapshot.staged.as_ref().map(|s| TimelineStaged {
                completed: s.completed,
                total: s.total,
            }),
        }
    }

    /// Builds from the ledger's own projected columns — the fast path for a
    /// row recorded after they existed (DECISIONS 435) — refusing first if
    /// `version` is not one this build reads.
    ///
    /// The only constructor that takes a bare `version: u32` rather than a
    /// parsed [`StateSnapshot`], and the only place
    /// [`pbps_model::check_readable_version`] is called from this crate: a
    /// row a newer pbps wrote populates these columns like any other, so
    /// without this check a reader on the projected path would present its
    /// counts as ordinary data instead of refusing it the way
    /// [`StateSnapshot::read_json`]'s fallback already does (a round-1 review
    /// finding on #103's own PR — the two paths had come to disagree about
    /// what "readable" means). Routing every projected row through this one
    /// function, rather than checking the version and calling a plain
    /// constructor beside it, is what keeps a third path from skipping the
    /// check the same way.
    pub fn from_projected(
        version: u32,
        tables: usize,
        modules: usize,
        staged: Option<TimelineStaged>,
    ) -> Result<Self, pbps_model::Unreadable> {
        pbps_model::check_readable_version(version)?;
        Ok(TimelineState {
            version,
            tables,
            modules,
            staged,
        })
    }
}

/// How far a staged checkpoint had got, without
/// [`pbps_model::StagedProgress::last_statement`] — nothing in `state list`
/// renders it, and a projected column exists for what a reader actually asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineStaged {
    pub completed: usize,
    pub total: usize,
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
        "this database has no `{STATE_TABLE_NAME}`: pbps has never recorded a state here.\n\
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

    /// A row a newer pbps wrote reaches the projected path as ordinary
    /// columns — `state_version` populated like any other row's — so
    /// `from_projected` is the one place standing between it and being
    /// presented as data this build understands. Refused identically to the
    /// JSON path's `StateSnapshot::read_json` (round-1 review finding on
    /// #103's own PR: the projected path had no version check at all).
    #[test]
    fn a_projected_version_this_build_does_not_read_is_refused_not_presented() {
        let too_new = pbps_model::state::CURRENT_VERSION + 1;
        let err = TimelineState::from_projected(too_new, 3, 1, None)
            .expect_err("a future version must be refused");
        assert!(matches!(err, pbps_model::Unreadable::UnsupportedVersion(_)));

        let too_old = pbps_model::state::OLDEST_READABLE_VERSION - 1;
        let err = TimelineState::from_projected(too_old, 3, 1, None)
            .expect_err("a version older than this build reads must be refused too");
        assert!(matches!(err, pbps_model::Unreadable::UnsupportedVersion(_)));
    }

    /// The negative case beside it: a row at a version this build reads is
    /// not refused, and its numbers come through unchanged.
    #[test]
    fn a_projected_version_this_build_reads_is_not_refused() {
        let state = TimelineState::from_projected(pbps_model::state::CURRENT_VERSION, 3, 1, None)
            .expect("a supported version must not be refused");
        assert_eq!(state.tables, 3);
        assert_eq!(state.modules, 1);
        assert_eq!(state.staged, None);
    }
}
