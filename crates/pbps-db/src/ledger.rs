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
    /// How to find the holder's sessions on this engine, and what that search
    /// cannot prove: the dialect's text, built from the application name
    /// [`LockHolder`] recorded. `None` for a holder recorded without one, by
    /// an older pbps or by hand (#1188).
    pub session_lookup: Option<String>,
}

impl LockInfo {
    fn session_lookup_suffix(&self) -> String {
        self.session_lookup
            .as_deref()
            .map(|lookup| format!("\n{lookup}"))
            .unwrap_or_default()
    }
}

/// The widest `locked_by` either engine's lock table holds, in characters.
pub const LOCKED_BY_CHARS: usize = 256;

/// The widest `operator` either engine's ledger holds: `varchar(128)` on
/// PostgreSQL, `NVARCHAR(128)` on SQL Server (#1205).
pub const OPERATOR_CHARS: usize = 128;

/// `text` cut to at most `width` UTF-16 code units, on a character boundary.
///
/// UTF-16 code units because that is how SQL Server's `NVARCHAR(n)` measures
/// a value, and a character outside the Basic Multilingual Plane takes two;
/// PostgreSQL's `varchar(n)` counts characters, never more than code units,
/// so one bound fits both. Never inside a surrogate pair: SQL Server refuses
/// a split one as "Invalid UTF-16 data" rather than storing half a character.
#[must_use]
pub fn clip_utf16(text: &str, width: usize) -> String {
    let mut room = width;
    text.chars()
        .take_while(|c| {
            let fits = c.len_utf16() <= room;
            if fits {
                room -= c.len_utf16();
            }
            fits
        })
        .collect()
}

/// Which process took the lock, as `locked_by` records it (#1188).
///
/// The lock deliberately outlives the process that took it and `pbps unlock`
/// is the human override (DECISIONS 285), so the row has to give the operator
/// something to check before overriding: the host, the process, the CI job,
/// and the application name the holder's sessions carry, which the engine's
/// session list can be searched by. It goes into `locked_by` rather than new
/// columns because every reader of the lock already shows that text, and a
/// new column is ledger DDL for no gain in what a human can check
/// (DEC-1188.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder<'a> {
    pub operator: &'a str,
    pub host: Option<&'a str>,
    pub pid: u32,
    pub ci_job: Option<&'a str>,
    /// What the holder's session reports as its application name, read back
    /// from the engine rather than assumed: a connection string may name its
    /// own, and the engine may shorten it.
    pub application_name: &'a str,
}

const APPLICATION_NAME_MARKER: &str = "application_name ";
const CONTEXT_CHARS: usize = 64;

impl LockHolder<'_> {
    /// `operator [host h, pid p, CI job j, application_name "a"]`, within
    /// [`LOCKED_BY_CHARS`]. The application name is JSON-quoted so it can be
    /// read back exactly ([`holder_application_name`]); what gives way to the
    /// width is the operator's name, then the job, then the host, and only
    /// last the application name — dropped whole rather than cut, since a cut
    /// name would find nothing, and the lock must still be taken.
    ///
    /// Width is counted in UTF-16 code units, which is how SQL Server's
    /// `NVARCHAR(256)` measures it: a character outside the Basic
    /// Multilingual Plane takes two. PostgreSQL's `varchar(256)` counts
    /// characters, never more than code units, so the same budget fits both.
    #[must_use]
    pub fn render(&self) -> String {
        let width = |text: &str| text.encode_utf16().count();
        let clip = |text: &str| text.chars().take(CONTEXT_CHARS).collect::<String>();
        let suffix = |host: bool, job: bool, name: bool| {
            let mut parts = Vec::new();
            if let Some(h) = self.host.filter(|_| host) {
                parts.push(format!("host {}", clip(h)));
            }
            parts.push(format!("pid {}", self.pid));
            if let Some(j) = self.ci_job.filter(|_| job) {
                parts.push(format!("CI job {}", clip(j)));
            }
            if name {
                parts.push(format!(
                    "{APPLICATION_NAME_MARKER}{}",
                    serde_json::Value::from(self.application_name)
                ));
            }
            format!(" [{}]", parts.join(", "))
        };
        let suffix = [
            (true, true, true),
            (true, false, true),
            (false, false, true),
            (false, false, false),
        ]
        .into_iter()
        .map(|(host, job, name)| suffix(host, job, name))
        .find(|s| width(s) <= LOCKED_BY_CHARS)
        .unwrap_or_else(|| suffix(false, false, false));
        let operator = clip_utf16(
            self.operator,
            LOCKED_BY_CHARS.saturating_sub(width(&suffix)),
        );
        format!("{operator}{suffix}")
    }
}

/// The application name a [`LockHolder`] recorded, or `None` when `locked_by`
/// was not written by one.
///
/// The marker is searched from the end: the name is JSON-quoted, so an
/// unescaped `application_name "` cannot occur inside it, while an operator's
/// name before it may say anything at all.
#[must_use]
pub fn holder_application_name(locked_by: &str) -> Option<String> {
    let quoted = &locked_by[locked_by.rfind(&format!("{APPLICATION_NAME_MARKER}\""))?..];
    let quoted = quoted
        .strip_prefix(APPLICATION_NAME_MARKER)?
        .strip_suffix(']')?;
    serde_json::from_str::<String>(quoted).ok()
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
         Wait for it to finish. If it died without releasing, `pbps unlock --db ...` clears it.{}",
        .0.locked_by,
        .0.locked_at,
        .0.session_lookup_suffix()
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

    fn holder(operator: &str, application_name: &str) -> LockHolder<'static> {
        LockHolder {
            operator: Box::leak(operator.to_owned().into_boxed_str()),
            host: Some("build-7"),
            pid: 4242,
            ci_job: Some("GitHub Actions run 99 attempt 2"),
            application_name: Box::leak(application_name.to_owned().into_boxed_str()),
        }
    }

    /// #1188: the row names the process, not only the person, and the
    /// application name reads back exactly.
    #[test]
    fn a_rendered_holder_names_the_process_and_its_application_name_reads_back() {
        let rendered = holder("leon", "pbps/4242/0a1b2c3d").render();
        assert_eq!(
            rendered,
            "leon [host build-7, pid 4242, CI job GitHub Actions run 99 attempt 2, \
             application_name \"pbps/4242/0a1b2c3d\"]"
        );
        assert_eq!(
            holder_application_name(&rendered).as_deref(),
            Some("pbps/4242/0a1b2c3d")
        );
    }

    /// An operator's name may say anything, and an application name may hold
    /// quotes and the marker itself; neither may be misread as the other.
    #[test]
    fn the_application_name_survives_a_hostile_operator_and_its_own_quotes() {
        for (operator, name) in [
            ("x application_name \"evil\"]", "pbps/1/2"),
            ("leon", "a \"quoted\" application_name \"name\"] too"),
            ("", "plain"),
            ("leon", ""),
        ] {
            let rendered = holder(operator, name).render();
            assert_eq!(
                holder_application_name(&rendered).as_deref(),
                Some(name),
                "{rendered}"
            );
        }
    }

    /// The row fits the lock table's `locked_by` whatever it is given, and
    /// what gives way is the operator's name, then the job and the host, but
    /// never the application name the session lookup needs.
    #[test]
    fn a_long_holder_fits_the_column_and_keeps_its_application_name() {
        let name = "n".repeat(128);
        let long = LockHolder {
            operator: &"o".repeat(300),
            host: Some(&"h".repeat(300)),
            pid: u32::MAX,
            ci_job: Some(&"j".repeat(300)),
            application_name: &name,
        }
        .render();
        assert!(
            long.encode_utf16().count() <= LOCKED_BY_CHARS,
            "{}",
            long.len()
        );
        assert_eq!(
            holder_application_name(&long).as_deref(),
            Some(name.as_str())
        );
        let short = holder(&"o".repeat(300), "pbps/1/2").render();
        assert!(short.encode_utf16().count() <= LOCKED_BY_CHARS);
        assert!(
            short.contains("host build-7") && short.contains("CI job"),
            "{short}"
        );
    }

    /// SQL Server's `NVARCHAR(256)` counts UTF-16 code units, so a name
    /// outside the Basic Multilingual Plane takes twice the room it seems to;
    /// and a name that JSON-escapes to more than the column holds cannot be
    /// recorded whole. Either way the row still fits and the lock is taken: a
    /// name that fits reads back exactly, one that cannot is left out whole
    /// rather than cut into a name no session carries.
    #[test]
    fn the_width_is_the_columns_own_measure_and_the_row_always_fits() {
        let emoji = "\u{1F600}".repeat(64);
        for (operator, name, kept) in [
            ("\u{1F600}".repeat(300), "pbps/1/2".to_owned(), true),
            ("leon".to_owned(), emoji.clone(), true),
            ("leon".to_owned(), "\"".repeat(128), false),
            ("leon".to_owned(), "\u{1}".repeat(128), false),
        ] {
            let rendered = holder(&operator, &name).render();
            assert!(
                rendered.encode_utf16().count() <= LOCKED_BY_CHARS,
                "{} code units: {rendered}",
                rendered.encode_utf16().count()
            );
            assert!(rendered.contains("pid 4242"), "{rendered}");
            assert_eq!(
                holder_application_name(&rendered),
                kept.then_some(name.clone()),
                "{rendered}"
            );
        }
    }

    /// #1205: the bound is UTF-16 code units, the measure SQL Server's
    /// `NVARCHAR` uses, cut on a character boundary so no surrogate pair is
    /// split.
    #[test]
    fn a_clip_counts_utf16_code_units_and_never_splits_a_character() {
        let emoji = "\u{1F600}";
        assert_eq!(
            clip_utf16(&"a".repeat(300), OPERATOR_CHARS),
            "a".repeat(128)
        );
        assert_eq!(
            clip_utf16(&emoji.repeat(100), OPERATOR_CHARS),
            emoji.repeat(64)
        );
        assert_eq!(clip_utf16(&format!("a{emoji}"), 2), "a");
        assert_eq!(clip_utf16("short", OPERATOR_CHARS), "short");
        assert_eq!(clip_utf16("anything", 0), "");
    }

    /// A holder written by hand or by an older pbps has no application name,
    /// and that reads as none rather than as a guess.
    #[test]
    fn a_holder_without_an_application_name_has_none() {
        for locked_by in [
            "pipeline-one",
            "leon [host h, pid 1]",
            "leon [application_name \"unterminated]",
            "leon [application_name \"x\"] trailing",
        ] {
            assert_eq!(holder_application_name(locked_by), None, "{locked_by}");
        }
    }

    #[test]
    fn a_locked_refusal_carries_the_session_lookup_only_when_there_is_one() {
        let mut info = LockInfo {
            locked_by: "leon".into(),
            locked_at: "2026-09-28T01:02:03.000".into(),
            session_lookup: None,
        };
        let bare = LedgerError::Locked(info.clone()).to_string();
        assert!(bare.ends_with("clears it."), "{bare}");
        info.session_lookup = Some("SELECT 1 -- find it".into());
        let found = LedgerError::Locked(info).to_string();
        assert!(
            found.ends_with("clears it.\nSELECT 1 -- find it"),
            "{found}"
        );
    }

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
