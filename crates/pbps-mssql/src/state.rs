//! The `__pbps_state` ledger and the `__pbps_lock` lock, in T-SQL (SPEC §8.1).
//!
//! The types and the meaning live in `pbps_db::ledger`; only the statements are
//! here, for the same reason [`crate::catalog`]'s queries are: they are T-SQL,
//! and Phase 5's PostgreSQL crate will need its own. Nothing in this file
//! interpolates a user-supplied value into SQL — every one of them is bound.
//!
//! # Why these two tables are not part of the managed set
//!
//! [`crate::catalog`]'s table query excludes these two names — and only these
//! two, not the whole `__pbps_` prefix, which would also hide a project's own
//! table — so the tool never introspects its own bookkeeping and never plans a
//! change to it.
//! Without that, the first `plan` after a `snapshot` would propose dropping the
//! ledger — the declarations do not mention it.

use std::borrow::Cow;

use pbps_db::ledger::flow::{self, ClipUnit, HeldLock, LedgerSpec, LedgerSteps, RowLimit};
use pbps_db::ledger::{LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_db::{Conn, DbError};
use pbps_model::StateSnapshot;

use crate::catalog::get;

/// The schema this engine's ledger lives in (SPEC §8.1).
///
/// `dbo` is the schema every SQL Server database has and the one the spec
/// names. It is deliberately *not* treated as a managed schema — see
/// [`crate::doctor::Needed::LedgerCreation`] — and it is qualified into every
/// statement below rather than left to the login's own default schema, which
/// is a per-login setting and would put the ledger somewhere else.
pub const LEDGER_SCHEMA: &str = "dbo";

/// The two tables, as this engine spells them.
///
/// The names are `pbps_db::ledger`'s, which is where the *meaning* of a ledger
/// row lives; the schema in front of them is this dialect's. Whoever has to
/// name these tables from outside — the pull's exclusion filter, `doctor`'s
/// permission questions — asks here rather than repeating the spelling.
pub const STATE_TABLE: &str = "dbo.__pbps_state";
pub const LOCK_TABLE: &str = "dbo.__pbps_lock";

/// `IF OBJECT_ID` rather than `CREATE OR ALTER`: creating the ledger must be
/// safe to run on every command that writes one, and re-running must not
/// disturb the rows already there.
///
/// The last five columns are `state list`'s own projection (issue #103,
/// DECISIONS 435) — the same move SPEC §8.1 already made for
/// `kind`/`git_sha`/`plan_checksum`/`operator`/`reason`, so a reader of the
/// timeline can get counts without parsing `state_json`. Nullable, because a
/// row recorded before they existed has none; [`timeline`] falls back to
/// `state_json` for exactly that row rather than refusing the whole call.
const CREATE_STATE: &str = "\
IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL
BEGIN
    CREATE TABLE dbo.__pbps_state (
        id               BIGINT IDENTITY(1,1) NOT NULL
                         CONSTRAINT pk___pbps_state PRIMARY KEY,
        applied_at       DATETIME2(3)   NOT NULL
                         CONSTRAINT df___pbps_state_applied_at DEFAULT SYSUTCDATETIME(),
        kind             VARCHAR(16)    NOT NULL,
        git_sha          VARCHAR(40)    NULL,
        plan_checksum    CHAR(64)       NULL,
        state_json       NVARCHAR(MAX)  NOT NULL,
        operator         NVARCHAR(128)  NOT NULL,
        reason           NVARCHAR(1000) NULL,
        state_version    INT NULL,
        tables_count     INT NULL,
        modules_count    INT NULL,
        staged_completed INT NULL,
        staged_total     INT NULL
    );
END;";

/// Adds the five timeline columns to a `__pbps_state` created before issue
/// #103, if they are not there yet.
///
/// Guarded by `IF COL_LENGTH(...) IS NULL` rather than a probe asked of Rust
/// first: **measured** against the pinned server, a login holding `SELECT`,
/// `INSERT` and `DELETE` on the table and no `ALTER` runs this exact batch
/// against a table that already has the columns and it does nothing —
/// SQL Server evaluates the `IF` before it would need `ALTER` to act on the
/// `THEN`, unlike the other engine's `ALTER TABLE ... ADD COLUMN IF NOT
/// EXISTS`, which is refused by ownership before the `IF NOT EXISTS` is
/// looked at at all (see `pbps_pg::state::migrate_timeline_columns`, which
/// therefore cannot use this shape). So a deployment account that never needs
/// `ALTER` after the one-time migration never has to hold it — the same
/// reasoning [`CREATE_STATE`] already relies on for `IF OBJECT_ID(...) IS
/// NULL`.
const ADD_TIMELINE_COLUMNS: &str = "\
IF COL_LENGTH('dbo.__pbps_state', 'state_version') IS NULL
BEGIN
    ALTER TABLE dbo.__pbps_state ADD
        state_version    INT NULL,
        tables_count     INT NULL,
        modules_count    INT NULL,
        staged_completed INT NULL,
        staged_total     INT NULL;
END;";

/// The width of `__pbps_state.reason`, in the unit `NVARCHAR(n)` is measured
/// in: UTF-16 code units ([`ClipUnit::Utf16Units`]), not characters.
pub const REASON_UTF16_UNITS: usize = 1000;

/// Cuts a reason down to what the ledger column can hold; see
/// [`flow::truncate_reason`] for the unit and for when it is used.
pub fn truncate_reason(text: &str) -> String {
    flow::truncate_reason(text, ClipUnit::Utf16Units, REASON_UTF16_UNITS)
}

/// How `TOP (n)` is bound here: a signed 32-bit integer, saturating.
const ROW_LIMIT: RowLimit = RowLimit::SaturatingI32;

const CREATE_LOCK: &str = "\
IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NULL
BEGIN
    CREATE TABLE dbo.__pbps_lock (
        id        INT NOT NULL
                  CONSTRAINT pk___pbps_lock PRIMARY KEY
                  CONSTRAINT ck___pbps_lock_single CHECK (id = 1),
        locked_by NVARCHAR(256) NOT NULL,
        locked_at DATETIME2(3)  NOT NULL
                  CONSTRAINT df___pbps_lock_locked_at DEFAULT SYSUTCDATETIME()
    );
END;";

/// `CONVERT(..., 126)` is ISO 8601. The driver is built without the `chrono`
/// feature — a date library for one column would be a dependency the audit has
/// to read — so the server formats and the client stores the text. Measured:
/// style 126 leaves out a zero fraction, so `.000` reads `2026-09-23T00:01:13`
/// and `.001` reads `2026-09-23T00:01:13.001`. Nothing parses the text, so
/// the two widths are both the contract (#796).
const SELECT_LATEST: &str = "\
SELECT TOP (1) id, CONVERT(varchar(23), applied_at, 126) AS applied_at, state_json
  FROM dbo.__pbps_state
 ORDER BY id DESC;";

const SELECT_HISTORY: &str = "\
SELECT TOP (@P1) id, CONVERT(varchar(23), applied_at, 126) AS applied_at, state_json
  FROM dbo.__pbps_state
 ORDER BY id DESC;";

/// One entry by its id: the three columns [`SELECT_LATEST`] returns.
const SELECT_BY_ID: &str =
    "SELECT id, CONVERT(varchar(23), applied_at, 126) AS applied_at, state_json
               FROM dbo.__pbps_state WHERE id = @P1;";

/// The newest row of one kind that carries one plan checksum; both are bound.
const SELECT_APPLIED_PLAN: &str = "SELECT TOP (1) id FROM dbo.__pbps_state WHERE kind = @P1 AND plan_checksum = @P2 ORDER BY id DESC";

/// The newest row at or below an id whose kind is one of `kinds` bound
/// values. Built from the count alone, like [`select_legacy_state_json`].
fn select_deployed_revision(kinds: usize) -> String {
    let slots = (0..kinds)
        .map(|i| format!("@P{}", i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT TOP (1) id, git_sha FROM dbo.__pbps_state WHERE id <= @P1 AND kind IN ({slots}) ORDER BY id DESC"
    )
}

/// The timeline reads the projected columns — including the five issue #103
/// added — and never `state_json`: the whole point is that a row whose
/// recorded state this build cannot read, or may not read, is still a row,
/// and getting that without ever asking for `state_json` is what lets
/// `state list` succeed against a ledger whose `state_json` this login cannot
/// see (the sharp test issue #103 names). A row from before the migration has
/// `state_version IS NULL`; [`timeline`] asks [`select_legacy_state_json`]
/// for exactly those rows, in a second statement, so the common case —
/// every row already migrated — never sends `state_json` at all.
const SELECT_TIMELINE: &str = "\
SELECT TOP (@P1) id, CONVERT(varchar(23), applied_at, 126) AS applied_at,
       kind, git_sha, plan_checksum, operator, reason,
       state_version, tables_count, modules_count, staged_completed, staged_total
  FROM dbo.__pbps_state
 ORDER BY id DESC;";

/// `state_json` for exactly the rows [`timeline`] could not answer from the
/// projected columns. Never sent when there are none, so the common case
/// this issue exists to make cheap never runs it.
fn select_legacy_state_json(count: usize) -> String {
    let slots: Vec<String> = (1..=count).map(|i| format!("@P{i}")).collect();
    format!(
        "SELECT id, state_json FROM dbo.__pbps_state WHERE id IN ({});",
        slots.join(", ")
    )
}

/// [`SELECT_TIMELINE`] without the five columns a `__pbps_state` from before
/// issue #103 does not have.
///
/// `state list` is a read and must never require `ALTER` or ownership to run
/// (that is [`crate::doctor::Needed::LedgerCreation`]'s territory, and
/// widening a read into a migration would collide with the gap tracked beside
/// this change), so it cannot call [`ensure_tables`] to make the columns
/// appear. An unmigrated ledger is a known shape instead — "absent, empty and
/// unreadable are three different things," and this is a fourth `timeline`
/// already knows how to serve: every row on it is legacy, by construction, so
/// [`timeline`] sends this query rather than [`SELECT_TIMELINE`] and then asks
/// [`select_legacy_state_json`] for every id it got back, exactly as it
/// already does for the legacy rows a partly-migrated ledger has (a round-1
/// review finding on #103's own PR: the first version of this change only
/// tested the migrated and the partly-migrated shapes, never the one a real
/// upgrade meets first).
const SELECT_TIMELINE_UNMIGRATED: &str = "\
SELECT TOP (@P1) id, CONVERT(varchar(23), applied_at, 126) AS applied_at,
       kind, git_sha, plan_checksum, operator, reason
  FROM dbo.__pbps_state
 ORDER BY id DESC;";

/// Whether `dbo.__pbps_state` carries the five columns issue #103 added, read
/// without needing anything wider than what an ordinary read already needs.
///
/// The same `COL_LENGTH` expression [`ADD_TIMELINE_COLUMNS`]'s own guard
/// uses, in a bare `SELECT` rather than inside an `ALTER`'s `IF`: a metadata
/// function, not a statement that touches the table's rows, so it needs
/// nothing beyond what letting a login see the object at all already needs —
/// the same `SELECT`/`INSERT`/`DELETE` login [`ADD_TIMELINE_COLUMNS`]'s own
/// doc comment measures evaluating the identical expression with no `ALTER`
/// held.
const TIMELINE_COLUMNS_PROBE: &str = "SELECT CASE WHEN COL_LENGTH('dbo.__pbps_state', 'state_version') IS NULL \
     THEN 0 ELSE 1 END AS present;";

pub(crate) async fn timeline_columns_present(conn: &mut Conn) -> Result<bool, DbError> {
    let rows = conn.query(TIMELINE_COLUMNS_PROBE).await?;
    let present: i32 = get(&rows[0], "present")?;
    Ok(present != 0)
}

/// `OUTPUT INSERTED.id` rather than `SCOPE_IDENTITY()`: it is one round trip,
/// and it cannot be confused by a trigger someone added to the ledger.
///
/// The last five values are the same projection [`CREATE_STATE`] documents,
/// computed from the snapshot being recorded rather than passed separately —
/// a caller able to set them independently could make them disagree with
/// `state_json`, which is exactly what the five columns beside it already
/// exist to never do.
///
/// Two spellings, not one with an optional pair of parameters: `Param` has no
/// nullable-integer variant, and `staged_completed`/`staged_total` are NULL on
/// precisely the rows that are not a staged checkpoint. [`record`] chooses
/// between them by the snapshot's own `staged` field — never by anything a
/// caller supplies as text — so there is nothing for
/// `no_statement_interpolates_a_value` to catch in either.
const INSERT_STATE_STAGED: &str = "\
INSERT INTO dbo.__pbps_state
    (kind, git_sha, plan_checksum, state_json, operator, reason,
     state_version, tables_count, modules_count, staged_completed, staged_total)
OUTPUT INSERTED.id
VALUES (@P1, @P2, @P3, @P4, @P5, @P6, @P7, @P8, @P9, @P10, @P11);";

const INSERT_STATE_NOT_STAGED: &str = "\
INSERT INTO dbo.__pbps_state
    (kind, git_sha, plan_checksum, state_json, operator, reason,
     state_version, tables_count, modules_count, staged_completed, staged_total)
OUTPUT INSERTED.id
VALUES (@P1, @P2, @P3, @P4, @P5, @P6, @P7, @P8, @P9, NULL, NULL);";

const SELECT_IDS: &str = "SELECT id FROM dbo.__pbps_state ORDER BY id DESC;";

const DELETE_UP_TO: &str = "DELETE FROM dbo.__pbps_state WHERE id <= @P1;";

const SELECT_LOCK: &str = "\
SELECT locked_by, CONVERT(varchar(23), locked_at, 126) AS locked_at
  FROM dbo.__pbps_lock WHERE id = 1;";

const INSERT_LOCK: &str = "INSERT INTO dbo.__pbps_lock (id, locked_by) VALUES (1, @P1);";

const DELETE_LOCK: &str = "DELETE FROM dbo.__pbps_lock WHERE id = 1;";

/// The ordinary ledger tables' names as the catalog stores them, wherever that
/// differs from the ledger's exact spelling — one row per such table.
///
/// On a case-insensitive database `OBJECT_ID(N'dbo.__pbps_state')` resolves
/// to a project's own `dbo.__PBPS_STATE`, which the pull keeps and validation
/// allows because both reserve only the exact spelling. `CREATE_STATE`'s
/// `IF OBJECT_ID(…) IS NULL` then skips creating the ledger, and every read,
/// write and prune after it lands on the project's table (#894). The
/// protected table already carries this as its `spelled` fact (#884); these
/// two are asked the same way. `Latin1_General_BIN2` compares code points, and
/// the `|` appended to both sides keeps SQL Server from padding a trailing
/// space away: `OBJECT_ID` resolves the ledger's name to a project's
/// `dbo.[__pbps_state ]` on every collation, and a bare `<>` read the two as
/// equal (#954, measured on the pinned server). The `IS NOT NULL` comes first
/// because under `CONCAT_NULL_YIELDS_NULL OFF`, which a server's `user options`
/// can make the default, `NULL + N'|'` is `N'|'`: an absent table would read as
/// misspelt and refuse creating the ledger (measured).
/// An invisible table resolves to NULL and has no row either: that is
/// [`is_initialized`]'s question, not this one.
const MISSPELT_LEDGER: &str = "\
SELECT N'__pbps_state' AS ledger,
       OBJECT_NAME(OBJECT_ID(N'dbo.__pbps_state', N'U')) COLLATE DATABASE_DEFAULT AS found
 WHERE OBJECT_NAME(OBJECT_ID(N'dbo.__pbps_state', N'U')) IS NOT NULL
   AND (OBJECT_NAME(OBJECT_ID(N'dbo.__pbps_state', N'U')) + N'|') COLLATE Latin1_General_BIN2
       <> N'__pbps_state|'
UNION ALL
SELECT N'__pbps_lock' AS ledger,
       OBJECT_NAME(OBJECT_ID(N'dbo.__pbps_lock', N'U')) COLLATE DATABASE_DEFAULT AS found
 WHERE OBJECT_NAME(OBJECT_ID(N'dbo.__pbps_lock', N'U')) IS NOT NULL
   AND (OBJECT_NAME(OBJECT_ID(N'dbo.__pbps_lock', N'U')) + N'|') COLLATE Latin1_General_BIN2
       <> N'__pbps_lock|';";

/// Refuses when a ledger name resolves to a project's table spelled otherwise
/// (#894). Asked before every statement that writes the ordinary ledger:
/// [`ensure_tables`] for `record` and `lock`, and [`prune`] and [`unlock`]
/// before their DELETE.
async fn refuse_misspelt_ledger(conn: &mut Conn) -> Result<(), DbError> {
    let rows = conn.query(MISSPELT_LEDGER).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let found = rows
        .iter()
        .map(|row| {
            Ok(format!(
                "dbo.{} resolves to `dbo.{}`",
                get::<&str>(row, "ledger")?,
                get::<&str>(row, "found")?
            ))
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    Err(DbError::Refused(format!(
        "{}: a table the project names itself, which this database's collation folds onto the \
         ledger's name, so the ledger would be read and written through it. Rename that table, \
         or point pbps at a database where the ledger's names are free.",
        found.join("; ")
    )))
}

/// The application lock every statement that creates or reshapes a ledger
/// table holds first (DEC-898.1).
const LEDGER_DDL_LOCK: &str = "pbps: ledger ddl";

/// Wraps `body` so it runs holding [`LEDGER_DDL_LOCK`], owned by a transaction.
///
/// `IF OBJECT_ID(...) IS NULL` then `CREATE TABLE` is a check-then-act: two
/// pipelines initializing the same database both see no table, and the loser's
/// `CREATE` fails with Msg 2714 — measured, 37 of 300 calls across 150 races on
/// the pinned server (#898). Tolerating 2714 afterwards does not work here,
/// because [`record`] runs inside the apply's `XACT_ABORT ON` transaction, where
/// the error has already doomed it. Holding the lock, the loser waits for the
/// winner's transaction and then finds the table (DEC-898.1).
///
/// The transaction is this batch's own, or nests in the caller's, where the
/// lock is held until the caller's commit. The `CATCH` rolls back only a
/// transaction the batch opened itself: a caller's one is doomed by
/// `XACT_ABORT` and left to the caller, which rolls it back as it would for any
/// failed statement. `THROW` rethrows the original error number, so the codes
/// callers read (Msg 1088 in [`migration_error`]) are unchanged.
fn serialized(body: &str) -> String {
    format!(
        "\
DECLARE @outer INT = @@TRANCOUNT, @granted INT;
BEGIN TRY
    BEGIN TRANSACTION;
    EXEC @granted = sys.sp_getapplock @Resource = N'{LEDGER_DDL_LOCK}',
        @LockMode = N'Exclusive', @LockOwner = N'Transaction';
    IF @granted < 0
        THROW 50000, N'pbps could not take the lock that serializes creating its ledger tables.', 1;
{body}
    COMMIT TRANSACTION;
END TRY
BEGIN CATCH
    IF @outer = 0 AND @@TRANCOUNT > 0 ROLLBACK TRANSACTION;
    THROW;
END CATCH;"
    )
}

/// Creates the ledger and lock tables if they are not there yet, and migrates
/// a `__pbps_state` from before issue #103 to carry the timeline columns.
///
/// Both run under [`serialized`], so two pipelines reaching an empty database
/// at once both succeed (#898).
pub async fn ensure_tables(conn: &mut Conn) -> Result<(), DbError> {
    refuse_misspelt_ledger(conn).await?;
    conn.execute(&serialized(&format!("{CREATE_STATE}\n{CREATE_LOCK}")))
        .await?;
    migrate_timeline_columns(conn).await
}

/// Runs [`ADD_TIMELINE_COLUMNS`], and turns a failure into one that names the
/// ledger, the columns and the right this needs.
///
/// The driver's own message for a denied `ALTER` — measured, Msg 1088,
/// "Cannot find the object ... because it does not exist or you do not have
/// permissions" — names neither. `doctor` asks for this right before migration,
/// but callers that deploy without running it still need an actionable error.
async fn migrate_timeline_columns(conn: &mut Conn) -> Result<(), DbError> {
    // The same check-then-act as the creates: two pipelines reaching a
    // pre-#103 ledger at once would both add the columns.
    conn.execute(&serialized(ADD_TIMELINE_COLUMNS))
        .await
        .map_err(migration_error)
}

fn migration_error(e: DbError) -> DbError {
    let code = e.server_error_code();
    // Msg 1088 is the measured refusal for this ALTER (DECISIONS 435).
    // A transport failure says nothing about the login's rights.
    let guidance = if code.as_deref() == Some("1088") {
        "This is a one-time migration that needs ALTER on dbo.__pbps_state. \
             Run `pbps doctor` with the same `--db` or `--env` target as the failed \
             command and obtain the ALTER right it reports before retrying."
    } else {
        "The timeline-column migration failed. Investigate the original database \
             or connection error in the error chain before retrying."
    };
    e.context(format!(
        "dbo.__pbps_state is missing the timeline columns (state_version, \
                 tables_count, modules_count, staged_completed, staged_total) issue #103 \
                 added, and this login could not add them.\n\
                 {guidance}"
    ))
}

/// The cheapest statement that resolves the ledger and checks the permission to
/// read it without returning a row. See [`is_initialized`] for why it is a
/// statement at all.
///
/// `id`, not a bare `1`: **measured** against the pinned server, once any
/// column of a table carries a column-level `DENY`, `SELECT TOP (0) 1 AS
/// present FROM t` — naming no column at all — is refused with the same Msg
/// 230 a query naming the denied column would get, even though it reads
/// nothing. Naming a real, always-granted column (issue #103's sharp test
/// denies only `state_json`) is what keeps this probe answering the question
/// it exists to answer rather than "is every column of this table readable".
const PROBE_STATE: &str = "SELECT TOP (0) id AS present FROM dbo.__pbps_state;";

/// This engine's ledger, as the shared flow reads it (DEC-2988.1).
///
/// No savepoint: a failed statement here leaves the transaction as it was, so
/// a tolerated failure needs nothing to rewind to.
static LEDGER: LedgerSpec = LedgerSpec {
    dialect: crate::types::DIALECT,
    probe_state: Cow::Borrowed(PROBE_STATE),
    select_latest: Cow::Borrowed(SELECT_LATEST),
    select_by_id: Cow::Borrowed(SELECT_BY_ID),
    select_history: Cow::Borrowed(SELECT_HISTORY),
    select_applied_plan: Cow::Borrowed(SELECT_APPLIED_PLAN),
    select_deployed_revision,
    select_timeline: Cow::Borrowed(SELECT_TIMELINE),
    select_timeline_unmigrated: Cow::Borrowed(SELECT_TIMELINE_UNMIGRATED),
    select_legacy_state_json,
    insert_state_staged: Cow::Borrowed(INSERT_STATE_STAGED),
    insert_state_not_staged: Cow::Borrowed(INSERT_STATE_NOT_STAGED),
    select_ids: Cow::Borrowed(SELECT_IDS),
    delete_up_to: Cow::Borrowed(DELETE_UP_TO),
    select_lock: Cow::Borrowed(SELECT_LOCK),
    insert_lock: Cow::Borrowed(INSERT_LOCK),
    delete_lock: Cow::Borrowed(DELETE_LOCK),
    session_application_name: Cow::Borrowed(SESSION_APPLICATION_NAME),
    row_limit: ROW_LIMIT,
    max_parameters: crate::doctor::MAX_PARAMETERS,
    operator_clip: (ClipUnit::Utf16Units, pbps_db::OPERATOR_CHARS),
    // A plain `INSERT` of the one row: a held lock is the primary key's
    // refusal, and the transaction outlives it, so the holder can be read
    // after the failure.
    held_lock: HeldLock::FailsTheInsert,
    missing_table: INVALID_OBJECT_NAME,
    select_denied: SELECT_DENIED_ON_COLUMN,
    savepoint: None,
    session_lookup,
};

/// The steps of the shared flow that are this engine's own work.
struct Steps;

impl LedgerSteps for Steps {
    async fn ensure_tables(conn: &mut Conn) -> Result<(), DbError> {
        ensure_tables(conn).await
    }

    async fn refuse_untrusted(conn: &mut Conn) -> Result<(), DbError> {
        refuse_misspelt_ledger(conn).await
    }

    async fn timeline_columns_present(conn: &mut Conn) -> Result<bool, DbError> {
        timeline_columns_present(conn).await
    }
}

/// Whether this database has a ledger at all.
///
/// Every read has to ask first: querying a table that does not exist fails with
/// "invalid object name", which tells the user nothing about what to do. The
/// answer they need is "run baseline or bootstrap", and only this distinction
/// can produce it.
///
/// # Why the statement is attempted rather than the catalog asked
///
/// This asked `OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL`, and it was wrong
/// for exactly the reason [`INVALID_OBJECT_NAME`] gives about the lock table:
/// measured against the pinned server, a principal with no permission on an
/// existing `__pbps_state` gets NULL from `OBJECT_ID` and 0 from
/// `HAS_PERMS_BY_NAME`, the same answers an absent table gives. So "not
/// authorized to look" arrived at every caller as "there is no ledger" — a
/// `doctor` that says `uninitialized`, an `explain` that offers `bootstrap`,
/// and a `state list` that reports an empty history. The statement separates
/// them: 208 when the table is absent, 229 when it is there and hidden, and
/// every other failure stays a failure (DECISIONS 219).
pub async fn is_initialized(conn: &mut Conn) -> Result<bool, DbError> {
    flow::is_initialized(conn, &LEDGER).await
}

/// The newest state, which is the environment's current baseline.
///
/// `Ok(None)` means the ledger exists but is empty — a database that was
/// initialized and then pruned to nothing. `Err(NotInitialized)` means there is
/// no ledger at all. The two need different remedies, so they are different
/// answers.
pub async fn latest(conn: &mut Conn) -> Result<Option<LedgerEntry>, LedgerError> {
    flow::latest(conn, &LEDGER).await
}

/// Commit evidence at or before the snapshot the caller already observed.
/// Reading projected metadata avoids decoding an obsolete historical snapshot.
pub async fn deployed_revision(
    conn: &mut Conn,
    observed_id: i64,
) -> Result<Option<pbps_db::ledger::StateRevision>, LedgerError> {
    flow::deployed_revision(conn, &LEDGER, observed_id).await
}

/// A retained successful apply is the evidence for an expand, not a baseline
/// or another report that happens to name the same checksum (DEC-2402.1).
pub async fn has_applied_plan(conn: &mut Conn, checksum: &str) -> Result<bool, LedgerError> {
    flow::has_applied_plan(conn, &LEDGER, checksum).await
}

/// A selected historical entry, or the newest when no id is supplied.
/// This is a read: it never creates or migrates the ledger.
pub async fn entry(
    conn: &mut Conn,
    id: Option<i64>,
) -> Result<Option<pbps_db::ledger::StoredEntry>, LedgerError> {
    flow::entry(conn, &LEDGER, id).await
}

/// The most recent `limit` entries, newest first.
///
/// Every one of them has to be readable: a caller asking for entries wants the
/// states, and half a state is not one. [`timeline`] is the other question.
pub async fn history(conn: &mut Conn, limit: u32) -> Result<Vec<LedgerEntry>, LedgerError> {
    flow::history(conn, &LEDGER, limit).await
}

/// The most recent `limit` rows as a timeline, newest first.
///
/// A row whose recorded state this build cannot read is carried with its
/// reason rather than failing the call: an environment upgraded across a
/// state-format change keeps rows older than `OLDEST_READABLE_VERSION`, and
/// one of them must not erase the history above it (DECISIONS 218).
///
/// [`SELECT_TIMELINE`] never asks for `state_json` (DECISIONS 435): a second
/// query, [`select_legacy_state_json`], asks for it only for the rows that
/// predate the migration — `state_version IS NULL` — and only for those ids.
/// In the common case, once a ledger's rows are all migrated, that second
/// query is never sent, which is the whole point of issue #103 and the
/// property its sharp test pins: a fully-migrated ledger whose `state_json`
/// this login may not see still answers `state list`.
pub async fn timeline(conn: &mut Conn, limit: u32) -> Result<Vec<TimelineEntry>, LedgerError> {
    flow::timeline::<Steps>(conn, &LEDGER, limit).await
}

/// Appends one state to the ledger and returns its id.
///
/// The columns beside `state_json` are projected from the snapshot rather than
/// passed separately: they exist so `status` can filter without parsing JSON,
/// and a caller able to set them independently could make them lie. The five
/// issue #103 added follow the same rule (DECISIONS 435).
pub async fn record(conn: &mut Conn, snapshot: &StateSnapshot) -> Result<i64, LedgerError> {
    flow::record::<Steps>(conn, &LEDGER, snapshot).await
}

/// Deletes all but the `keep` newest entries; returns how many went.
pub async fn prune(conn: &mut Conn, keep: u32) -> Result<u64, LedgerError> {
    flow::prune::<Steps>(conn, &LEDGER, keep).await
}

/// Takes the apply lock, or reports who already holds it.
///
/// The insert is the gate, not a read before it: a check-then-insert would
/// let two pipelines through the check together. The read only happens after
/// the insert has already failed, and then only to name the holder.
pub async fn lock(conn: &mut Conn, holder: &str) -> Result<(), LedgerError> {
    flow::lock::<Steps>(conn, &LEDGER, holder).await
}

/// SQL Server's "Invalid object name": the table this statement names is not
/// there. **Not** the code for one that is there and may not be read — that is
/// 229, "permission was denied", and keeping the two apart is the whole point
/// of asking by error number.
///
/// # Why the statement is attempted rather than the catalog asked
///
/// The obvious guard is `OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NULL`, and it
/// is wrong: SQL Server's metadata-visibility rules hide an object from a
/// principal with no permission on it, so `OBJECT_ID` answers NULL for a table
/// that exists and holds a live lock. That turned "not authorized to look" into
/// "no lock", which is the one direction this tool must never round in. The
/// ledger table had the same guard and the same fault; [`is_initialized`] now
/// attempts a statement for this reason too.
///
/// `HAS_PERMS_BY_NAME` does not separate them either — measured against a real
/// server, it answers **0** both for an absent table and for one hidden this
/// way. The statement itself is the only thing that does: 208 for absent, 229
/// for denied, and every other failure stays a failure.
const INVALID_OBJECT_NAME: &str = "208";

/// SQL Server's "SELECT permission was denied on the column", **measured**
/// against the pinned server: Msg 230. Table-level denial is 229, the code
/// beside [`INVALID_OBJECT_NAME`] above — and by the time [`timeline`] would
/// meet this one, [`is_initialized`]'s table-level probe has already
/// succeeded, so 230 is specifically a principal refused `state_json` and
/// nothing else on the ledger, the shape issue #103's sharp test builds.
const SELECT_DENIED_ON_COLUMN: &str = "230";

/// Releases the lock. `false` means it was not held.
pub async fn unlock(conn: &mut Conn) -> Result<bool, DbError> {
    flow::unlock::<Steps>(conn, &LEDGER).await
}

/// The application name this session carries, as the server holds it, so the
/// lock records the name `sys.dm_exec_sessions.program_name` will show
/// (#1188).
pub async fn session_application_name(conn: &mut Conn) -> Result<String, DbError> {
    flow::session_application_name(conn, &LEDGER).await
}

const SESSION_APPLICATION_NAME: &str = "SELECT APP_NAME() AS name;";

/// How an operator finds a lock holder's sessions here, from what
/// `locked_by` recorded; `None` when it recorded no application name.
///
/// Measured on 17.0.4075.5: without `VIEW SERVER STATE` a login sees only its
/// own row of `sys.dm_exec_sessions`, so an empty result from such a login
/// says nothing about the holder. Whether the holder has gone stays the
/// operator's call (DECISIONS 285).
#[must_use]
pub fn session_lookup(locked_by: &str) -> Option<String> {
    let name = pbps_db::holder_application_name(locked_by)?;
    Some(format!(
        "While the holder is connected, its sessions are listed by:\n  \
         SELECT session_id, login_name, host_name, host_process_id, login_time \
         FROM sys.dm_exec_sessions WHERE program_name = N'{}';\n\
         A login without VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE on SQL Server 2022 \
         and later) sees only its own session, so no row from it proves nothing. No row from \
         one that has it means no session carries that name now; it does not show the \
         process has exited, so confirm that on its host.",
        name.replace('\'', "''")
    ))
}

/// Who holds the deployment lock, if anyone.
///
/// A missing lock table answers `None` rather than failing: nothing can be
/// holding a lock that does not exist. That is deliberately *not* the same as
/// the table being unreadable, which stays an error — callers report the two
/// differently, and "I could not look" must never be flattened into "nothing
/// there". See [`INVALID_OBJECT_NAME`] for why that distinction cannot be made by
/// asking the catalog first.
pub async fn lock_holder(conn: &mut Conn) -> Result<Option<LockInfo>, DbError> {
    flow::lock_holder(conn, &LEDGER).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::Param;

    #[test]
    fn only_permission_migration_failures_recommend_obtaining_rights() {
        for code in [Some("1088"), Some("1205"), None] {
            let original = "original driver failure";
            let error = migration_error(DbError::Driver {
                diagnostic: None,
                message: original.into(),
                code: code.map(str::to_owned),
            });
            assert_eq!(error.server_error_code().as_deref(), code);
            let message = error.to_string();
            assert!(!message.contains(original));
            assert_eq!(
                std::error::Error::source(&error).unwrap().to_string(),
                original
            );
            assert_eq!(message.contains("right it reports"), code == Some("1088"));
            assert_eq!(
                message.contains("with the same `--db` or `--env` target as the failed command"),
                code == Some("1088")
            );
            assert_eq!(
                message.contains("Investigate the original"),
                code != Some("1088")
            );
        }
    }

    /// A count too large for `TOP` must become "as many as it can", never a
    /// small number and never a negative one: wrapping would silently shorten
    /// the timeline, which reads as "that is all there is".
    #[test]
    fn a_count_above_the_signed_range_saturates_rather_than_wrapping() {
        let bound = |limit| match ROW_LIMIT.param(limit) {
            Param::I32(n) => n,
            Param::I64(_) | Param::Str(_) | Param::OptStr(_) => {
                panic!("`TOP` is bound as a 32-bit integer")
            }
        };
        assert_eq!(bound(1), 1);
        assert_eq!(bound(i32::MAX as u32), i32::MAX);
        assert_eq!(bound(i32::MAX as u32 + 1), i32::MAX);
        assert_eq!(bound(u32::MAX), i32::MAX);
    }

    /// The constant and the DDL have to agree, or the truncation is measured
    /// against a width the column does not have.
    #[test]
    fn the_reason_width_is_the_one_the_ddl_declares() {
        assert!(
            CREATE_STATE.contains(&format!(
                "reason           NVARCHAR({REASON_UTF16_UNITS}) NULL"
            )),
            "{CREATE_STATE}"
        );
    }

    /// A character count is the natural cut and the wrong one: 1000 emoji are
    /// 2000 units, and the audit insert for a failed apply was refused with
    /// Msg 2628 — the one row that exists to say what went wrong, dropped
    /// because of how it was said.
    #[test]
    fn a_reason_is_cut_by_utf16_units_and_never_inside_a_character() {
        let ascii = "x".repeat(REASON_UTF16_UNITS);
        assert_eq!(truncate_reason(&ascii), ascii);
        assert_eq!(truncate_reason(&format!("{ascii}y")), ascii);

        let emoji = "😀".repeat(REASON_UTF16_UNITS);
        let cut = truncate_reason(&emoji);
        assert_eq!(cut.encode_utf16().count(), REASON_UTF16_UNITS);
        assert_eq!(cut.chars().count(), REASON_UTF16_UNITS / 2);

        // An odd unit left over cannot hold half a pair: the cut falls short
        // by one rather than splitting the character.
        let odd = format!("x{emoji}");
        let cut = truncate_reason(&odd);
        assert_eq!(cut.encode_utf16().count(), REASON_UTF16_UNITS - 1);
        assert!(cut.chars().all(|c| c == 'x' || c == '😀'));
    }
    use pbps_db::ledger::{LOCK_TABLE_NAME, STATE_TABLE_NAME};

    /// The two table names in the SQL must be the ones `pbps-db` documents and
    /// the ones the catalog query excludes; a mismatch would have the tool
    /// planning changes to its own ledger.
    ///
    /// The qualified spellings are this crate's and the bare names are
    /// `pbps-db`'s, so the assertion is the join of the two rather than a
    /// literal: a schema edited here and not there would otherwise leave two
    /// constants that each look right on their own.
    #[test]
    fn the_statements_name_the_documented_tables() {
        assert_eq!(STATE_TABLE, format!("{LEDGER_SCHEMA}.{STATE_TABLE_NAME}"));
        assert_eq!(LOCK_TABLE, format!("{LEDGER_SCHEMA}.{LOCK_TABLE_NAME}"));
        for sql in [CREATE_STATE, ADD_TIMELINE_COLUMNS, TIMELINE_COLUMNS_PROBE] {
            assert!(sql.contains("__pbps_state"), "{sql}");
        }
        assert!(CREATE_LOCK.contains("__pbps_lock"));
        // What the shared flow sends comes from the spec's own list, which
        // names every field: a statement added there is covered here.
        let spec = LEDGER.statements();
        for sql in &spec.state {
            assert!(sql.contains(STATE_TABLE), "{sql}");
            assert!(!sql.contains(LOCK_TABLE_NAME), "{sql}");
        }
        for sql in &spec.lock {
            assert!(sql.contains(LOCK_TABLE), "{sql}");
            assert!(!sql.contains(STATE_TABLE_NAME), "{sql}");
        }
    }

    /// The spec hands the shared flow this module's own statements and codes,
    /// and each of its statements binds exactly the values the flow sends:
    /// a slot too few is a refused query, a slot too many reads a value that
    /// was never bound.
    #[test]
    fn the_spec_carries_this_engines_statements_and_binds_every_value() {
        assert_eq!(LEDGER.probe_state, PROBE_STATE);
        assert_eq!(LEDGER.select_latest, SELECT_LATEST);
        assert_eq!(LEDGER.select_history, SELECT_HISTORY);
        assert_eq!(LEDGER.select_lock, SELECT_LOCK);
        assert_eq!(LEDGER.select_timeline, SELECT_TIMELINE);
        assert_eq!(
            LEDGER.select_timeline_unmigrated,
            SELECT_TIMELINE_UNMIGRATED
        );
        assert_eq!(LEDGER.insert_state_staged, INSERT_STATE_STAGED);
        assert_eq!(LEDGER.insert_state_not_staged, INSERT_STATE_NOT_STAGED);
        assert_eq!(LEDGER.select_ids, SELECT_IDS);
        assert_eq!(LEDGER.delete_up_to, DELETE_UP_TO);
        assert_eq!(LEDGER.insert_lock, INSERT_LOCK);
        assert_eq!(LEDGER.delete_lock, DELETE_LOCK);
        assert_eq!(LEDGER.missing_table, "208");
        assert_eq!(LEDGER.select_denied, "230");
        assert_eq!(LEDGER.row_limit, RowLimit::SaturatingI32);
        assert_eq!(LEDGER.max_parameters, crate::doctor::MAX_PARAMETERS);
        assert_eq!(LEDGER.operator_clip, (ClipUnit::Utf16Units, 128));
        assert!(
            LEDGER.savepoint.is_none(),
            "a failed statement leaves the transaction usable here"
        );

        assert_eq!(slots(&LEDGER.probe_state), 0);
        assert_eq!(slots(&LEDGER.select_latest), 0);
        assert_eq!(slots(&LEDGER.select_lock), 0);
        assert_eq!(slots(&LEDGER.select_by_id), 1);
        assert_eq!(slots(&LEDGER.select_history), 1);
        assert_eq!(slots(&LEDGER.select_applied_plan), 2);
        assert_eq!(
            (LEDGER.select_deployed_revision)(3),
            format!(
                "SELECT TOP (1) id, git_sha FROM {STATE_TABLE} WHERE id <= @P1 AND kind IN (@P2, @P3, @P4) \
                 ORDER BY id DESC"
            )
        );
        assert_eq!(slots(&(LEDGER.select_deployed_revision)(1)), 2);
        assert_eq!(slots(&LEDGER.select_timeline), 1);
        assert_eq!(slots(&LEDGER.select_timeline_unmigrated), 1);
        assert_eq!(slots(&(LEDGER.select_legacy_state_json)(3)), 3);
        assert_eq!(slots(&LEDGER.insert_state_staged), 11);
        assert_eq!(slots(&LEDGER.insert_state_not_staged), 9);
        assert_eq!(slots(&LEDGER.select_ids), 0);
        assert_eq!(slots(&LEDGER.delete_up_to), 1);
        assert_eq!(slots(&LEDGER.insert_lock), 1);
        assert_eq!(slots(&LEDGER.delete_lock), 0);
        assert_eq!(slots(&LEDGER.session_application_name), 0);
        // No value is written into a statement: none of them quotes anything.
        let spec = LEDGER.statements();
        for sql in spec.state.iter().chain(&spec.lock) {
            assert!(!sql.contains('\''), "{sql}");
        }
    }

    /// The flow reads a held lock where the insert reports it. Here the
    /// insert has nothing that steps aside, so a held lock is the key's
    /// refusal — and a spec that said otherwise would return that refusal as
    /// a failure with no holder named.
    #[test]
    fn a_held_lock_is_read_where_this_insert_reports_it() {
        assert_eq!(
            LEDGER.insert_lock,
            "INSERT INTO dbo.__pbps_lock (id, locked_by) VALUES (1, @P1);"
        );
        assert_eq!(LEDGER.held_lock, HeldLock::FailsTheInsert);
    }

    /// How many distinct `@Pn` a statement binds, which must be `@P1..=@Pn`.
    fn slots(sql: &str) -> usize {
        let count = (1..=20).take_while(|n| has_slot(sql, *n)).count();
        assert!(
            (count + 1..=20).all(|n| !has_slot(sql, n)),
            "the slots of this statement are not contiguous: {sql}"
        );
        count
    }

    /// `@P1` is not found inside `@P10`.
    fn has_slot(sql: &str, n: usize) -> bool {
        let slot = format!("@P{n}");
        sql.match_indices(&slot)
            .any(|(at, _)| !sql[at + slot.len()..].starts_with(|c: char| c.is_ascii_digit()))
    }

    /// Every user-supplied value reaches the server as a parameter. A `'` in an
    /// operator's name or a drop reason must not be able to end a statement.
    #[test]
    fn no_statement_interpolates_a_value() {
        for sql in [
            SELECT_HISTORY,
            SELECT_TIMELINE,
            SELECT_TIMELINE_UNMIGRATED,
            INSERT_STATE_STAGED,
            INSERT_STATE_NOT_STAGED,
            DELETE_UP_TO,
            INSERT_LOCK,
            DELETE_LOCK,
            SELECT_BY_ID,
            SELECT_APPLIED_PLAN,
            &select_legacy_state_json(3),
            &select_deployed_revision(3),
        ] {
            assert!(
                !sql.contains("'{") && !sql.contains("{}"),
                "a format placeholder in ledger SQL means a value is being pasted: {sql}"
            );
        }
        assert!(
            INSERT_STATE_STAGED.contains("@P11"),
            "all eleven values are bound"
        );
        assert!(
            INSERT_STATE_NOT_STAGED.contains("@P9")
                && INSERT_STATE_NOT_STAGED.contains("NULL, NULL"),
            "the nine bound values plus the two literal NULLs"
        );
    }

    /// One placeholder per id asked about, in order — the shape
    /// [`timeline`]'s fallback relies on to keep a returned row matched to
    /// the id that asked for it.
    #[test]
    fn the_legacy_query_binds_one_placeholder_per_id() {
        assert_eq!(
            select_legacy_state_json(3),
            "SELECT id, state_json FROM dbo.__pbps_state WHERE id IN (@P1, @P2, @P3);"
        );
        assert_eq!(
            select_legacy_state_json(1),
            "SELECT id, state_json FROM dbo.__pbps_state WHERE id IN (@P1);"
        );
    }
}
