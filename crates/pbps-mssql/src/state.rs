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

use pbps_db::ledger::{
    LedgerEntry, LedgerError, LockInfo, TimelineEntry, TimelineStaged, TimelineState, ids_to_prune,
};
use pbps_db::{Conn, DbError, Param};
use pbps_model::{StateSnapshot, Unreadable};

use crate::catalog::{get, opt};

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
/// in: UTF-16 code units, not characters. Measured, not assumed — an
/// `NVARCHAR(4)` refused two emoji plus one ASCII letter with Msg 2628.
pub const REASON_UTF16_UNITS: usize = 1000;

/// Cuts a reason down to what the ledger column can hold.
///
/// Counts `char::len_utf16`, because a Rust `char` above U+FFFF occupies two
/// units and a count of characters could hand the column twice its width. The
/// cut never splits a character, so what remains is valid text. Used on the
/// best-effort audit paths, where a failed attempt that cannot be recorded is
/// the opposite of what the row exists for; a user-supplied `--reason` is not
/// truncated and fails loudly instead, since an audit text silently shortened
/// is a different kind of loss.
pub fn truncate_reason(text: &str) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= REASON_UTF16_UNITS
        })
        .collect()
}

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
/// to read — so the server formats and the client stores the text.
const SELECT_LATEST: &str = "\
SELECT TOP (1) id, CONVERT(varchar(23), applied_at, 126) AS applied_at, state_json
  FROM dbo.__pbps_state
 ORDER BY id DESC;";

const SELECT_HISTORY: &str = "\
SELECT TOP (@P1) id, CONVERT(varchar(23), applied_at, 126) AS applied_at, state_json
  FROM dbo.__pbps_state
 ORDER BY id DESC;";

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

async fn timeline_columns_present(conn: &mut Conn) -> Result<bool, DbError> {
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

/// Creates the ledger and lock tables if they are not there yet, and migrates
/// a `__pbps_state` from before issue #103 to carry the timeline columns.
pub async fn ensure_tables(conn: &mut Conn) -> Result<(), DbError> {
    conn.execute(CREATE_STATE).await?;
    conn.execute(CREATE_LOCK).await?;
    migrate_timeline_columns(conn).await
}

/// Runs [`ADD_TIMELINE_COLUMNS`], and turns a failure into one that names the
/// ledger, the columns and the right this needs.
///
/// The driver's own message for a denied `ALTER` — measured, Msg 1088,
/// "Cannot find the object ... because it does not exist or you do not have
/// permissions" — names neither. `doctor` does not yet ask for `ALTER` on an
/// existing ledger (it asks only while the table is being created,
/// [`crate::doctor::Needed::LedgerCreation`]), so an operator meeting this for
/// the first time has no readiness check that would have warned them; that
/// gap is reported beside this change, not closed by it.
async fn migrate_timeline_columns(conn: &mut Conn) -> Result<(), DbError> {
    conn.execute(ADD_TIMELINE_COLUMNS).await.map_err(|e| {
        let code = e.server_error_code();
        DbError::Driver {
            message: format!(
                "dbo.__pbps_state is missing the timeline columns (state_version, \
                 tables_count, modules_count, staged_completed, staged_total) issue #103 \
                 added, and this login could not add them: {e}\n\
                 This is a one-time migration that needs ALTER on dbo.__pbps_state; it is \
                 not part of the ordinary deployment grant, so a login holding only what \
                 `doctor` asks for today will meet this until that is fixed."
            ),
            code,
        }
    })
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
/// for exactly the reason [`is_missing_table`] gives about the lock table:
/// measured against the pinned server, a principal with no permission on an
/// existing `__pbps_state` gets NULL from `OBJECT_ID` and 0 from
/// `HAS_PERMS_BY_NAME`, the same answers an absent table gives. So "not
/// authorized to look" arrived at every caller as "there is no ledger" — a
/// `doctor` that says `uninitialized`, an `explain` that offers `bootstrap`,
/// and a `state list` that reports an empty history. The statement separates
/// them: 208 when the table is absent, 229 when it is there and hidden, and
/// every other failure stays a failure (DECISIONS 219).
pub async fn is_initialized(conn: &mut Conn) -> Result<bool, DbError> {
    match conn.query(PROBE_STATE).await {
        Ok(_) => Ok(true),
        Err(e) if is_missing_table(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The newest state, which is the environment's current baseline.
///
/// `Ok(None)` means the ledger exists but is empty — a database that was
/// initialized and then pruned to nothing. `Err(NotInitialized)` means there is
/// no ledger at all. The two need different remedies, so they are different
/// answers.
pub async fn latest(conn: &mut Conn) -> Result<Option<LedgerEntry>, LedgerError> {
    if !is_initialized(conn).await? {
        return Err(LedgerError::NotInitialized);
    }
    let rows = conn.query(SELECT_LATEST).await?;
    rows.first().map(entry_from_row).transpose()
}

/// The most recent `limit` entries, newest first.
///
/// Every one of them has to be readable: a caller asking for entries wants the
/// states, and half a state is not one. [`timeline`] is the other question.
pub async fn history(conn: &mut Conn, limit: u32) -> Result<Vec<LedgerEntry>, LedgerError> {
    if !is_initialized(conn).await? {
        return Err(LedgerError::NotInitialized);
    }
    let rows = conn
        .query_with(SELECT_HISTORY, &[top(limit).into()])
        .await?;
    rows.iter().map(entry_from_row).collect()
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
    if !is_initialized(conn).await? {
        return Err(LedgerError::NotInitialized);
    }
    // `state list` is a read and must not require `ALTER` to run, so it
    // cannot call `ensure_tables` to make an unmigrated ledger's columns
    // appear — it asks the shape instead, with a probe that needs nothing
    // wider than an ordinary read already needs, and sends the query that
    // matches (round-1 review finding on #103's own PR: `timeline` is the
    // only reader of `dbo.__pbps_state`'s shape, and both branches meet at
    // `legacy_ids` below whichever one ran).
    let projected: Vec<ProjectedRow> = if timeline_columns_present(conn).await? {
        let rows = conn
            .query_with(SELECT_TIMELINE, &[top(limit).into()])
            .await?;
        rows.iter().map(projected_row).collect::<Result<_, _>>()?
    } else {
        let rows = conn
            .query_with(SELECT_TIMELINE_UNMIGRATED, &[top(limit).into()])
            .await?;
        rows.iter()
            .map(projected_row_unmigrated)
            .collect::<Result<_, _>>()?
    };

    let legacy_ids: Vec<i64> = projected
        .iter()
        .filter(|r| r.state.is_none())
        .map(|r| r.id)
        .collect();
    let mut legacy: std::collections::HashMap<i64, Result<TimelineState, Unreadable>> =
        std::collections::HashMap::new();
    // In pieces of at most `crate::doctor::MAX_PARAMETERS` ids, not one
    // statement for every legacy id: this query binds one parameter per id,
    // and until a deployer has run a deployment after upgrading, every row
    // is legacy, so a long-lived ledger's ordinary `state list` is exactly
    // the case that could reach the server's own ceiling in a single
    // request (a round-1 review finding on #103's own PR — the pre-#103
    // query needed only its `@P1` limit parameter and had no such ceiling).
    for chunk in legacy_ids.chunks(crate::doctor::MAX_PARAMETERS) {
        let sql = select_legacy_state_json(chunk.len());
        let params: Vec<Param<'_>> = chunk.iter().map(|&id| id.into()).collect();
        match conn.query_with(&sql, &params).await {
            Ok(rows) => {
                for row in &rows {
                    let id: i64 = get(row, "id")?;
                    let state_json: &str = get(row, "state_json")?;
                    // `read_json`, the same reader the pre-migration
                    // `timeline_from_row` used: version before shape, and the
                    // failure carried rather than returned (DECISIONS 222).
                    let state = StateSnapshot::read_json(state_json)
                        .map(|s| TimelineState::from_snapshot(&s));
                    legacy.insert(id, state);
                }
            }
            // Denied specifically, never a hard failure of the whole call:
            // this is the fallback query, run only for rows a fresh ledger
            // never has, and DECISIONS 218's promise — a row this build
            // cannot read is carried, not thrown — extends to a row this
            // build was refused, not only one it could not parse (DECISIONS
            // 433). Every legacy id in this chunk shares one answer, because
            // a column-level DENY is not sensitive to which row is read; a
            // later chunk still asks for itself; a `DENY` on the table does
            // not change between one statement and the next.
            Err(e) if is_select_denied(&e) => {
                let denied = Err(Unreadable::Denied(e.to_string()));
                for id in chunk {
                    legacy.insert(*id, denied.clone());
                }
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(projected
        .into_iter()
        .map(|r| TimelineEntry {
            id: r.id,
            applied_at: r.applied_at,
            kind: r.kind,
            git_sha: r.git_sha,
            plan_checksum: r.plan_checksum,
            operator: r.operator,
            reason: r.reason,
            state: match r.state {
                // A version this build does not read, refused before the
                // JSON fallback ever runs — see [`ProjectedRow`]'s doc
                // comment on why this is neither `Some(Ok(_))` nor `None`.
                Some(Err(e)) => Err(e),
                Some(Ok(state)) => Ok(state),
                // The fallback query answered for every id it was asked
                // about; a legacy id missing from its answer means the row
                // left the ledger between the two queries (a concurrent
                // `state prune`), not that it was ever malformed or denied.
                None => legacy.remove(&r.id).unwrap_or_else(|| {
                    Err(Unreadable::Malformed(
                        "this row's state_json could not be re-read; it may have been \
                         pruned while the timeline was being read"
                            .to_owned(),
                    ))
                }),
            },
        })
        .collect())
}

/// `TOP (n)` takes a signed integer, and the count is unsigned.
///
/// Saturating rather than `as`, which wraps: `--limit 4294967295` became `-1`
/// and the server refused the query, so a number too large to mean anything
/// turned into a failure rather than into "all of them" (DECISIONS 217).
fn top(limit: u32) -> i32 {
    i32::try_from(limit).unwrap_or(i32::MAX)
}

/// A count or version as the `INT` columns hold it: saturating, like [`top`]
/// — a table, module or staged-statement count that does not fit `i32` is not
/// a real schema, and a wrapped negative would misreport the count rather
/// than merely cap it.
fn saturating_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Appends one state to the ledger and returns its id.
///
/// The columns beside `state_json` are projected from the snapshot rather than
/// passed separately: they exist so `status` can filter without parsing JSON,
/// and a caller able to set them independently could make them lie. The five
/// issue #103 added follow the same rule (DECISIONS 435).
pub async fn record(conn: &mut Conn, snapshot: &StateSnapshot) -> Result<i64, LedgerError> {
    ensure_tables(conn).await?;
    let state_json = serde_json::to_string(snapshot).map_err(|e| LedgerError::BadEntry {
        id: 0,
        message: format!("the snapshot could not be serialized: {e}"),
    })?;

    let kind = snapshot.kind.as_str();
    let state_version = saturating_i32(u64::from(snapshot.version));
    let tables_count = saturating_i32(snapshot.schema.tables.len() as u64);
    let modules_count = saturating_i32(snapshot.schema.modules.len() as u64);

    let mut params: Vec<Param<'_>> = vec![
        kind.into(),
        snapshot.git_sha.as_deref().into(),
        snapshot.plan_checksum.as_deref().into(),
        state_json.as_str().into(),
        snapshot.operator.as_str().into(),
        snapshot.reason.as_deref().into(),
        state_version.into(),
        tables_count.into(),
        modules_count.into(),
    ];
    // `staged_completed`/`staged_total` cannot be bound as NULL through
    // `Param` — it has no nullable-integer variant, and adding one would mean
    // updating the binder `pbps_db::Param` matches in *both* drivers for two
    // columns that are NULL on precisely the rows that are not staged. Two
    // static statements, chosen by whether this snapshot has staged progress
    // at all, cost nothing `no_statement_interpolates_a_value` would catch —
    // neither text is built from a value — and need no new binder variant.
    let sql = match &snapshot.staged {
        Some(progress) => {
            params.push(saturating_i32(progress.completed as u64).into());
            params.push(saturating_i32(progress.total as u64).into());
            INSERT_STATE_STAGED
        }
        None => INSERT_STATE_NOT_STAGED,
    };
    let rows = conn.query_with(sql, &params).await?;
    match rows.first() {
        Some(row) => Ok(get(row, "id")?),
        None => Err(LedgerError::Db(DbError::BadRow(
            "the ledger insert returned no id".into(),
        ))),
    }
}

/// Deletes all but the `keep` newest entries; returns how many went.
pub async fn prune(conn: &mut Conn, keep: u32) -> Result<u64, LedgerError> {
    if !is_initialized(conn).await? {
        return Err(LedgerError::NotInitialized);
    }
    let ids: Vec<i64> = conn
        .query(SELECT_IDS)
        .await?
        .iter()
        .map(|row| get::<i64>(row, "id"))
        .collect::<Result<_, _>>()?;

    // The policy is decided in `pbps-db` and tested without a server; all that
    // is left here is the delete. Because ids come from an IDENTITY and the list
    // is newest-first, the prunable set is always a contiguous tail, so one
    // `<=` covers it exactly.
    let doomed = ids_to_prune(&ids, keep);
    let Some(highest) = doomed.first().copied() else {
        return Ok(0);
    };
    Ok(conn.execute_with(DELETE_UP_TO, &[highest.into()]).await?)
}

/// Takes the apply lock, or reports who already holds it.
pub async fn lock(conn: &mut Conn, holder: &str) -> Result<(), LedgerError> {
    ensure_tables(conn).await?;
    // The insert is the gate, not the preceding read: a check-then-insert would
    // let two pipelines through the check together. The read only happens after
    // the insert has already failed, and then only to name the holder.
    match conn.execute_with(INSERT_LOCK, &[holder.into()]).await {
        Ok(_) => Ok(()),
        Err(e) => match lock_holder(conn).await? {
            Some(info) => Err(LedgerError::Locked(info)),
            None => Err(LedgerError::Db(e)),
        },
    }
}

/// SQL Server's "Invalid object name": the table this statement names is not
/// there. **Not** the code for one that is there and may not be read — that is
/// 229, "permission was denied", and keeping the two apart is the whole point
/// of asking by error number.
const INVALID_OBJECT_NAME: &str = "208";

/// Whether a failure means "that table does not exist".
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
fn is_missing_table(e: &DbError) -> bool {
    e.server_error_code().as_deref() == Some(INVALID_OBJECT_NAME)
}

/// SQL Server's "SELECT permission was denied on the column", **measured**
/// against the pinned server: Msg 230. Table-level denial is 229, the code
/// beside [`INVALID_OBJECT_NAME`] above — and by the time [`timeline`] would
/// meet this one, [`is_initialized`]'s table-level probe has already
/// succeeded, so 230 is specifically a principal refused `state_json` and
/// nothing else on the ledger, the shape issue #103's sharp test builds.
const SELECT_DENIED_ON_COLUMN: &str = "230";

fn is_select_denied(e: &DbError) -> bool {
    e.server_error_code().as_deref() == Some(SELECT_DENIED_ON_COLUMN)
}

/// Releases the lock. `false` means it was not held.
pub async fn unlock(conn: &mut Conn) -> Result<bool, DbError> {
    // The *lock* table, not the state table. Guarding on `is_initialized` meant
    // a database whose `__pbps_state` had been dropped by hand reported "not
    // held" and left a live lock in place — with no command able to clear it.
    match conn.execute_with(DELETE_LOCK, &[]).await {
        Ok(n) => Ok(n > 0),
        Err(e) if is_missing_table(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Who holds the deployment lock, if anyone.
///
/// A missing lock table answers `None` rather than failing: nothing can be
/// holding a lock that does not exist. That is deliberately *not* the same as
/// the table being unreadable, which stays an error — callers report the two
/// differently, and "I could not look" must never be flattened into "nothing
/// there". See [`is_missing_table`] for why that distinction cannot be made by
/// asking the catalog first.
pub async fn lock_holder(conn: &mut Conn) -> Result<Option<LockInfo>, DbError> {
    let rows = match conn.query(SELECT_LOCK).await {
        Ok(rows) => rows,
        Err(e) if is_missing_table(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    match rows.first() {
        Some(row) => Ok(Some(LockInfo {
            locked_by: get::<&str>(row, "locked_by")?.to_owned(),
            locked_at: get::<&str>(row, "locked_at")?.to_owned(),
        })),
        None => Ok(None),
    }
}

fn entry_from_row(row: &pbps_db::Row) -> Result<LedgerEntry, LedgerError> {
    let id: i64 = get(row, "id")?;
    let state_json: &str = get(row, "state_json")?;
    // `from_json`, not `from_str` then `check_version`: an entry written by an
    // older pbps is refused by its version and the remedy that goes with it,
    // rather than by whichever field of its older shape serde reached first.
    let snapshot = StateSnapshot::from_json(state_json)
        .map_err(|message| LedgerError::BadEntry { id, message })?;
    Ok(LedgerEntry {
        id,
        // NULL is impossible: the column is NOT NULL with a default. Treating a
        // surprise as an empty string would put a blank date on a status screen,
        // so it is reported instead.
        applied_at: opt::<&str>(row, "applied_at")?
            .ok_or_else(|| DbError::BadRow("`applied_at` is unexpectedly NULL".into()))?
            .to_owned(),
        snapshot,
    })
}

/// A ledger row's projected columns, without `state_json` (DECISIONS 435):
/// [`SELECT_TIMELINE`] never asks for it. `state` is `None` exactly when this
/// row predates issue #103's migration — `state_version IS NULL` — and
/// [`timeline`] must still ask [`select_legacy_state_json`] for it.
///
/// `Some(Err(_))` is a third shape, distinct from `None`: a row whose columns
/// *are* populated but whose version this build does not read — a row a
/// newer pbps wrote. That is not "legacy" and must never reach the JSON
/// fallback, which would find nothing wrong with a `state_json` this build
/// simply has not been asked to parse; refusing it here, before the fallback
/// even runs, is what [`pbps_db::ledger::TimelineState::from_projected`]
/// makes unavoidable (a round-1 review finding on #103's own PR).
struct ProjectedRow {
    id: i64,
    applied_at: String,
    kind: String,
    git_sha: Option<String>,
    plan_checksum: Option<String>,
    operator: String,
    reason: Option<String>,
    state: Option<Result<TimelineState, Unreadable>>,
}

/// A count or a version read back from an `INT` column. Negative is
/// impossible by construction — [`record`] only ever writes what
/// [`saturating_i32`] produces — so it is reported rather than clamped: a
/// negative here means the row and this reader have gone out of step, not
/// that the count was merely large.
fn as_count(n: i32, column: &str) -> Result<usize, DbError> {
    usize::try_from(n).map_err(|_| DbError::BadRow(format!("`{column}` is {n}, not a count")))
}

fn as_version(n: i32, column: &str) -> Result<u32, DbError> {
    u32::try_from(n).map_err(|_| DbError::BadRow(format!("`{column}` is {n}, not a version")))
}

fn projected_row(row: &pbps_db::Row) -> Result<ProjectedRow, LedgerError> {
    let id: i64 = get(row, "id")?;

    // `state_version` is the migration's own marker: written by every
    // `record` from here on, and NULL on every row from before it. It is the
    // one column this test asks about rather than `tables_count` or the
    // staged pair, because those three project independently of whether a
    // row is staged and would each need their own "is this really absent"
    // rule; `state_version` has only one meaning either way.
    let state = match opt::<i32>(row, "state_version")? {
        None => None,
        Some(version) => {
            let version = as_version(version, "state_version")?;
            // The version is checked before any other projected column is
            // even read, not only before `TimelineState` is built from them
            // (a round-3 review finding on #103's own PR): `from_projected`
            // already refuses an unsupported version, but `tables`/`modules`
            // below were decoded *before* that call, so a newer pbps that
            // wrote both an unsupported version and a count this build
            // cannot parse would fail the whole `timeline()` call rather
            // than refusing just that row — the one ordering the JSON
            // fallback never got wrong (`read_json` checks the version
            // before it touches the rest of the document at all). This is a
            // second call to the same `check_readable_version` `from_projected`
            // already calls, not a second check that could disagree with
            // it — `from_projected` stays the only constructor, so `.expect`
            // below documents a call that cannot fail rather than skips one.
            Some(match pbps_model::check_readable_version(version) {
                Err(e) => Err(e),
                Ok(()) => {
                    let tables = as_count(get(row, "tables_count")?, "tables_count")?;
                    let modules = as_count(get(row, "modules_count")?, "modules_count")?;
                    // `(Some, Some)` and `(None, None)` are the only two pairs
                    // `record` ever writes — it always writes both columns
                    // together or neither, driven from the JSON side's
                    // `Option<StagedProgress>`, where `completed`/`total` are
                    // one field, not two, and cannot come apart (a round-4
                    // review finding on #103's own PR). This diff's two
                    // independently nullable columns can still represent a
                    // pair no write path produces — a hand-edited row, or a
                    // migration gone wrong — and the catch-all this fixes
                    // used to read that mismatch as "not staged" the same as
                    // a genuine `(None, None)`, silently losing whatever
                    // progress the row actually carried rather than saying
                    // the row could not be read. Refused as `Malformed`
                    // instead, by the same reasoning `read_json`'s strict
                    // parse already refuses an inconsistent JSON document.
                    let staged = match (
                        opt::<i32>(row, "staged_completed")?,
                        opt::<i32>(row, "staged_total")?,
                    ) {
                        (Some(completed), Some(total)) => Ok(Some(TimelineStaged {
                            completed: as_count(completed, "staged_completed")?,
                            total: as_count(total, "staged_total")?,
                        })),
                        // A migrated row that is not a staged checkpoint: both
                        // are NULL together, which is what "not
                        // mid-deployment" means (`pbps_model::StagedProgress`'s
                        // own doc comment).
                        (None, None) => Ok(None),
                        (Some(completed), None) => Err(Unreadable::Malformed(format!(
                            "`staged_completed` is {completed} but `staged_total` is NULL"
                        ))),
                        (None, Some(total)) => Err(Unreadable::Malformed(format!(
                            "`staged_total` is {total} but `staged_completed` is NULL"
                        ))),
                    };
                    match staged {
                        Ok(staged) => Ok(TimelineState::from_projected(
                            version, tables, modules, staged,
                        )
                        .expect("the version was already checked as readable above")),
                        Err(e) => Err(e),
                    }
                }
            })
        }
    };

    ledger_row(row, id, state)
}

/// A row from [`SELECT_TIMELINE_UNMIGRATED`]: the six columns a `__pbps_state`
/// from before issue #103 has, and nothing else — `state` is always `None`,
/// because a table with no timeline columns has no row that could be
/// anything but legacy.
fn projected_row_unmigrated(row: &pbps_db::Row) -> Result<ProjectedRow, LedgerError> {
    let id: i64 = get(row, "id")?;
    ledger_row(row, id, None)
}

/// The six columns every shape of `__pbps_state` this crate reads has, shared
/// by [`projected_row`] and [`projected_row_unmigrated`] so the two never
/// drift on how one of them is read.
fn ledger_row(
    row: &pbps_db::Row,
    id: i64,
    state: Option<Result<TimelineState, Unreadable>>,
) -> Result<ProjectedRow, LedgerError> {
    Ok(ProjectedRow {
        id,
        applied_at: opt::<&str>(row, "applied_at")?
            .ok_or_else(|| DbError::BadRow("`applied_at` is unexpectedly NULL".into()))?
            .to_owned(),
        // The projected columns, which are what makes an unreadable row still
        // a row worth showing.
        kind: get::<&str>(row, "kind")?.to_owned(),
        git_sha: opt::<&str>(row, "git_sha")?.map(str::to_owned),
        plan_checksum: opt::<&str>(row, "plan_checksum")?.map(str::to_owned),
        operator: get::<&str>(row, "operator")?.to_owned(),
        reason: opt::<&str>(row, "reason")?.map(str::to_owned),
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count too large for `TOP` must become "as many as it can", never a
    /// small number and never a negative one: wrapping would silently shorten
    /// the timeline, which reads as "that is all there is".
    #[test]
    fn a_count_above_the_signed_range_saturates_rather_than_wrapping() {
        assert_eq!(top(1), 1);
        assert_eq!(top(i32::MAX as u32), i32::MAX);
        assert_eq!(top(i32::MAX as u32 + 1), i32::MAX);
        assert_eq!(top(u32::MAX), i32::MAX);
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
        for sql in [
            CREATE_STATE,
            ADD_TIMELINE_COLUMNS,
            TIMELINE_COLUMNS_PROBE,
            SELECT_LATEST,
            SELECT_HISTORY,
            SELECT_TIMELINE,
            SELECT_TIMELINE_UNMIGRATED,
            INSERT_STATE_STAGED,
            INSERT_STATE_NOT_STAGED,
            SELECT_IDS,
            DELETE_UP_TO,
        ] {
            assert!(sql.contains("__pbps_state"), "{sql}");
        }
        assert!(select_legacy_state_json(2).contains("__pbps_state"));
        for sql in [CREATE_LOCK, SELECT_LOCK, INSERT_LOCK, DELETE_LOCK] {
            assert!(sql.contains("__pbps_lock"), "{sql}");
        }
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
            &select_legacy_state_json(3),
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

    /// A count too large for `i32` saturates rather than wrapping — the same
    /// rule [`top`] follows, applied to what `record` writes.
    #[test]
    fn a_count_too_large_for_the_column_saturates() {
        assert_eq!(saturating_i32(0), 0);
        assert_eq!(saturating_i32(i32::MAX as u64), i32::MAX);
        assert_eq!(saturating_i32(i32::MAX as u64 + 1), i32::MAX);
        assert_eq!(saturating_i32(u64::MAX), i32::MAX);
    }

    /// A negative value in a column this build only ever writes non-negative
    /// numbers to is reported rather than clamped to zero — clamping would
    /// read as "no tables" about a row that is actually corrupted.
    #[test]
    fn a_negative_count_or_version_is_reported_not_clamped() {
        assert!(as_count(-1, "tables_count").is_err());
        assert_eq!(as_count(5, "tables_count").unwrap(), 5);
        assert!(as_version(-1, "state_version").is_err());
        assert_eq!(as_version(7, "state_version").unwrap(), 7);
    }
}
