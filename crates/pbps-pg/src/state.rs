//! The `__pbps_state` ledger and the `__pbps_lock` lock, in PostgreSQL
//! (SPEC §8.1).
//!
//! The types and the meaning live in `pbps_db::ledger`; only the statements are
//! here, for the same reason [`crate::catalog`]'s queries are: they are
//! PostgreSQL, they are T-SQL in `pbps_mssql::state`, and `pbps-db` is the
//! crate documented as holding no engine SQL. Nothing in this file interpolates
//! a user-supplied value into SQL — every one of them is bound.
//!
//! # Where this engine's ledger lives, and why it is `public`
//!
//! SPEC §8.1 puts the two tables in `dbo`, which is a SQL Server schema. The
//! counterpart here is `public`: the schema every PostgreSQL database is
//! created with, the one `dbo` corresponds to in every migration guide, and the
//! only schema this tool can name without inventing one.
//!
//! A dedicated `pbps` schema was the alternative and was rejected on what it
//! costs the deployment account. Creating a schema needs `CREATE` **on the
//! database**, which is granted by the database's owner and covers creating
//! *any* schema; creating the two tables in `public` needs `CREATE` on that one
//! schema. The narrower grant is the one a tool that argues against
//! `db_owner` should be asking for.
//!
//! Measured on 18.6, and it is why `doctor` asks: since PostgreSQL 15 the
//! `public` schema no longer carries `CREATE` for `PUBLIC` —
//! `nspacl = {pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}` —
//! so an ordinary deployment role has `USAGE` there and **not** `CREATE`, and
//! the first `bootstrap` fails at the ledger unless someone has granted it.
//!
//! # Why these two tables are not part of the managed set
//!
//! [`crate::catalog`]'s table query excludes them by name, so the tool never
//! introspects its own bookkeeping and never plans a change to it. Qualifying
//! that filter with [`LEDGER_SCHEMA`] — so that a project's own
//! `app.__pbps_state` stays visible — is #185, which was waiting on this module
//! to say where the ledger lives.

use pbps_db::ledger::{
    LOCK_TABLE_NAME, LedgerEntry, LedgerError, LockInfo, STATE_TABLE_NAME, TimelineEntry,
    TimelineStaged, TimelineState, ids_to_prune,
};
use pbps_db::{Conn, DbError, Param, Row};
use pbps_model::{StateSnapshot, Unreadable};

/// The schema this engine's ledger lives in. See the module header.
pub const LEDGER_SCHEMA: &str = "public";

/// The two tables, as this engine spells them.
///
/// The names are `pbps_db::ledger`'s, which is where the *meaning* of a ledger
/// row lives; the schema in front of them is this dialect's. Whoever has to
/// name these tables from outside — the pull's exclusion filter, `doctor`'s
/// permission questions — asks here rather than repeating the spelling.
pub const STATE_TABLE: &str = "public.__pbps_state";
pub const LOCK_TABLE: &str = "public.__pbps_lock";

/// `CREATE TABLE IF NOT EXISTS` rather than a catalog check and a branch:
/// creating the ledger must be safe to run on every command that writes one,
/// and re-running must not disturb the rows already there.
///
/// `GENERATED ALWAYS AS IDENTITY`, not `bigserial`. The dialect refuses
/// `serial` in a declaration (step 2, DECISIONS 245) because it is a spelling
/// the catalog never gives back; this tool's own DDL is held to the rule it
/// enforces.
///
/// `timestamp(3)`, not `timestamptz`. What is stored is a UTC wall clock, as
/// SQL Server's `DATETIME2(3)` is, and a `timestamptz` would be *rendered* in
/// whatever `TimeZone` the reading session happens to have — the same class of
/// setting-dependence [`crate::catalog`]'s canonical scope exists to close.
///
/// `clock_timestamp()`, not `now()`. **Measured**: `now()` is the transaction's
/// start time and does not move inside it, so two entries recorded in one apply
/// — a staged checkpoint and the entry that follows it — would carry the same
/// instant and read as simultaneous. `clock_timestamp()` is the statement's,
/// which is what SQL Server's `SYSUTCDATETIME()` gives the other ledger.
const CREATE_STATE: &str = "\
CREATE TABLE IF NOT EXISTS public.__pbps_state (
    id               bigint GENERATED ALWAYS AS IDENTITY
                     CONSTRAINT pk___pbps_state PRIMARY KEY,
    applied_at       timestamp(3)   NOT NULL DEFAULT (clock_timestamp() AT TIME ZONE 'UTC'),
    kind             varchar(16)    NOT NULL,
    git_sha          varchar(40)    NULL,
    plan_checksum    varchar(64)    NULL,
    state_json       text           NOT NULL,
    operator         varchar(128)   NOT NULL,
    reason           varchar(1000)  NULL,
    state_version    integer NULL,
    tables_count     integer NULL,
    modules_count    integer NULL,
    staged_completed integer NULL,
    staged_total     integer NULL
)";

/// Adds the five timeline columns to a `__pbps_state` created before issue
/// #103, if they are not there yet.
///
/// [`crate::state::migrate_timeline_columns`] sends this only once it has
/// already confirmed, from the catalog, that the columns are missing —
/// **measured** against 18.6: `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`,
/// sent by a role holding `SELECT`/`INSERT`/`DELETE` and not ownership, fails
/// with `must be owner of table t` even when every column already exists —
/// this engine checks ownership before it looks at `IF NOT EXISTS` at all,
/// unlike the other engine's guarded `IF COL_LENGTH(...) IS NULL` batch (see
/// `pbps_mssql::state::ADD_TIMELINE_COLUMNS`, which is why that one can skip
/// the Rust-side probe this one cannot). `IF NOT EXISTS` on each column is
/// still worth keeping, as belt beside the probe's suspenders: it is what
/// keeps a race against a concurrent migration idempotent.
const ADD_TIMELINE_COLUMNS: &str = "\
ALTER TABLE public.__pbps_state
    ADD COLUMN IF NOT EXISTS state_version    integer NULL,
    ADD COLUMN IF NOT EXISTS tables_count     integer NULL,
    ADD COLUMN IF NOT EXISTS modules_count    integer NULL,
    ADD COLUMN IF NOT EXISTS staged_completed integer NULL,
    ADD COLUMN IF NOT EXISTS staged_total     integer NULL";

/// The width of `__pbps_state.reason`, in the unit `varchar(n)` is measured in
/// on this engine: **characters**.
///
/// Not the same unit as the other ledger's, and the difference is the point.
/// `NVARCHAR(1000)` counts UTF-16 code units, so 1000 emoji do not fit there;
/// measured on 18.6, `varchar(4)` accepts four emoji and reports
/// `length = 4, octet_length = 16`. A truncation written in the other engine's
/// unit would cut a reason in half here for no reason at all.
pub const REASON_CHARS: usize = 1000;

/// Cuts a reason down to what the ledger column can hold.
///
/// Used on the best-effort audit paths, where a failed attempt that cannot be
/// recorded is the opposite of what the row exists for; a user-supplied
/// `--reason` is not truncated and fails loudly instead, since an audit text
/// silently shortened is a different kind of loss.
///
/// **Measured, and the reason this is not left to the engine**: a cast
/// truncates silently — `'😀😀😀😀😀'::varchar(4)` returns four emoji and no
/// warning — while an insert of the same value into a `varchar(4)` column
/// fails with `22001: value too long`. One engine, two answers, and the silent
/// one is the one a `::text` in the wrong place would reach.
pub fn truncate_reason(text: &str) -> String {
    text.chars().take(REASON_CHARS).collect()
}

/// The lock, whose one row is the gate.
///
/// A **table**, not `pg_advisory_lock`. The advisory lock is the obvious
/// PostgreSQL answer and it answers a different question: it is held by a
/// *session* and released when that session ends — measured, `pg_locks` shows
/// nothing of a `pg_try_advisory_lock(42)` once the connection that took it has
/// gone. SPEC §8.1's lock is the one thing here that must **survive** the
/// pipeline that took it, because a pipeline killed mid-apply is exactly when
/// the next one must not start; that is also why `pbps unlock` exists as a
/// command rather than as a timeout. A row in a table is what a dead process
/// leaves behind.
const CREATE_LOCK: &str = "\
CREATE TABLE IF NOT EXISTS public.__pbps_lock (
    id        integer NOT NULL
              CONSTRAINT pk___pbps_lock PRIMARY KEY
              CONSTRAINT ck___pbps_lock_single CHECK (id = 1),
    locked_by varchar(256) NOT NULL,
    locked_at timestamp(3) NOT NULL DEFAULT (clock_timestamp() AT TIME ZONE 'UTC')
)";

/// ISO 8601, rendered by the server and stored by the client as text — the
/// driver is used without a date library here for the same reason the other one
/// is: a dependency for one column is a dependency the audit has to read.
///
/// `to_char` with an explicit pattern, **not** `applied_at::text`. Measured on
/// 18.6: under `DateStyle = 'German, DMY'` the same value casts to
/// `31.08.2026 09:14:22.517`, which is not sortable as text, not parseable as
/// ISO 8601, and produced by an operator's own session setting rather than by
/// anything this tool did. The pattern here holds no locale-sensitive field
/// (`TM` is what would make one), so it renders the same under every
/// `DateStyle` and every `lc_time`.
const TIMESTAMP_PATTERN: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS.MS'";

/// One timestamp column, rendered. Written once and asked for by every
/// statement that returns a time, so that the pattern cannot drift between
/// two of them while both still look right.
fn rendered(column: &str) -> String {
    format!("to_char({column}, {TIMESTAMP_PATTERN}) AS {column}")
}

/// The newest entry. `LIMIT`, where the other dialect writes `TOP`.
///
/// Built rather than written out, like [`crate::catalog`]'s queries and for the
/// same reason: [`rendered`] is one pattern used by four statements, and
/// four copies of a `to_char` pattern would each look right on their own
/// while two of them drifted.
fn select_latest() -> String {
    let applied_at = rendered("applied_at");
    format!(
        "SELECT id, {applied_at}, state_json
  FROM {STATE_TABLE}
 ORDER BY id DESC LIMIT 1"
    )
}

fn select_history() -> String {
    let applied_at = rendered("applied_at");
    format!(
        "SELECT id, {applied_at}, state_json
  FROM {STATE_TABLE}
 ORDER BY id DESC LIMIT $1"
    )
}

/// The timeline reads the projected columns — including the five issue #103
/// added — and never `state_json`: a row this build cannot read, or may not
/// read, is still a row, and getting that without ever asking for
/// `state_json` is what lets `state list` succeed against a ledger whose
/// `state_json` this role cannot see (the sharp test issue #103 names). A row
/// from before the migration has `state_version IS NULL`; [`timeline`] asks
/// [`select_legacy_state_json`] for exactly those rows, in a second
/// statement, so the common case — every row already migrated — never sends
/// `state_json` at all.
fn select_timeline() -> String {
    let applied_at = rendered("applied_at");
    format!(
        "SELECT id, {applied_at}, kind, git_sha, plan_checksum, operator, reason,
                state_version, tables_count, modules_count, staged_completed, staged_total
  FROM {STATE_TABLE}
 ORDER BY id DESC LIMIT $1"
    )
}

/// [`select_timeline`] without the five columns a `__pbps_state` from before
/// issue #103 does not have.
///
/// `state list` is a read and must never require ownership of the table to
/// run (that is [`crate::doctor::Needed::LedgerCreation`]'s territory, and
/// widening a read into a migration would collide with the gap tracked
/// beside this change), so it cannot call [`ensure_tables`] to make the
/// columns appear. An unmigrated ledger is a known shape instead — "absent,
/// empty and unreadable are three different things," and this is a fourth
/// [`timeline`] already knows how to serve: every row on it is legacy, by
/// construction, so [`timeline`] sends this query rather than
/// [`select_timeline`] and then asks [`select_legacy_state_json`] for every
/// id it got back, exactly as it already does for the legacy rows a
/// partly-migrated ledger has (a round-1 review finding on #103's own PR:
/// the first version of this change only tested the migrated and the
/// partly-migrated shapes, never the one a real upgrade meets first).
fn select_timeline_unmigrated() -> String {
    let applied_at = rendered("applied_at");
    format!(
        "SELECT id, {applied_at}, kind, git_sha, plan_checksum, operator, reason
  FROM {STATE_TABLE}
 ORDER BY id DESC LIMIT $1"
    )
}

/// The most parameters one bound statement may carry on this engine.
///
/// **Measured**, against the pinned image, not copied across from the other
/// dialect's `pbps_mssql::doctor::MAX_PARAMETERS`: a different protocol, a
/// different ceiling. PostgreSQL's extended protocol writes the Bind
/// message's parameter count as an `int16`, so 65,535 bound parameters are
/// accepted and 65,536 are refused (the live test
/// `a_query_may_bind_the_most_parameters_the_extended_protocol_represents`)
/// — unlike SQL Server's `sp_executesql` wrapper, nothing here spends
/// parameters of its own first, so this number is not adjusted down the way
/// the other dialect's is.
const MAX_PARAMETERS: usize = 65535;

/// `state_json` for exactly the rows [`timeline`] could not answer from the
/// projected columns. Never sent when there are none, so the common case
/// this issue exists to make cheap never runs it.
fn select_legacy_state_json(count: usize) -> String {
    let slots: Vec<String> = (1..=count).map(|i| format!("${i}")).collect();
    format!(
        "SELECT id, state_json FROM {STATE_TABLE} WHERE id IN ({})",
        slots.join(", ")
    )
}

/// `RETURNING id` is one round trip and cannot be confused by a trigger someone
/// added to the ledger — the reason the other dialect writes
/// `OUTPUT INSERTED.id` rather than reading a session-wide identity.
///
/// The last five values are the same projection [`CREATE_STATE`] documents,
/// computed from the snapshot being recorded rather than passed separately.
///
/// Two spellings, not one with an optional pair of parameters: `Param` has no
/// nullable-integer variant, and `staged_completed`/`staged_total` are NULL
/// on precisely the rows that are not a staged checkpoint. [`record`] chooses
/// between them by the snapshot's own `staged` field, never by anything a
/// caller supplies as text, so there is nothing here for
/// `no_statement_interpolates_a_value` to catch.
const INSERT_STATE_STAGED: &str = "\
INSERT INTO public.__pbps_state
    (kind, git_sha, plan_checksum, state_json, operator, reason,
     state_version, tables_count, modules_count, staged_completed, staged_total)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
RETURNING id";

const INSERT_STATE_NOT_STAGED: &str = "\
INSERT INTO public.__pbps_state
    (kind, git_sha, plan_checksum, state_json, operator, reason,
     state_version, tables_count, modules_count, staged_completed, staged_total)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL, NULL)
RETURNING id";

const SELECT_IDS: &str = "SELECT id FROM public.__pbps_state ORDER BY id DESC";

const DELETE_UP_TO: &str = "DELETE FROM public.__pbps_state WHERE id <= $1";

fn select_lock() -> String {
    let locked_at = rendered("locked_at");
    format!(
        "SELECT locked_by, {locked_at}
  FROM {LOCK_TABLE} WHERE id = 1"
    )
}

/// `ON CONFLICT (id) DO NOTHING`, and the zero rows it reports are the answer.
///
/// The other dialect inserts and reads the holder when the insert *fails*.
/// That shape cannot be ported: measured on 18.6, a failed statement aborts the
/// whole transaction — the `SELECT` that would name the holder comes back
/// `25P02: current transaction is aborted` — so a lock taken inside a
/// transaction would report the wrong thing, or nothing. `ON CONFLICT` is still
/// a gate: a second inserter blocks on the primary key's index until the first
/// commits and then does nothing, so exactly one caller sees a row count of 1.
/// If the first rolled back, the second gets the lock.
const INSERT_LOCK: &str =
    "INSERT INTO public.__pbps_lock (id, locked_by) VALUES (1, $1) ON CONFLICT (id) DO NOTHING";

const DELETE_LOCK: &str = "DELETE FROM public.__pbps_lock WHERE id = 1";

/// The cheapest statement that resolves the ledger and checks the permission to
/// read it without returning a row. See [`is_initialized`] for why it is a
/// statement at all.
const PROBE_STATE: &str = "SELECT 1 AS present FROM public.__pbps_state LIMIT 0";

/// Whether both tables are already there, asked of the catalog.
///
/// `pg_class` and `pg_namespace` are world-readable and this is a **join on
/// names**, not a name resolution: measured, `to_regclass('hidden.t')` raises
/// `42501` for a schema this role cannot enter, while a join like this one
/// answers about it. So this question can be asked by a role holding nothing
/// at all, and it cannot fail where the answer matters.
///
/// It asks exactly what `CREATE TABLE IF NOT EXISTS` asks — **any relation of
/// that name**, whatever its kind — because the point is to predict whether
/// that statement would do anything, and a probe that asked a narrower question
/// would send DDL the engine is about to skip.
///
/// This is not the question [`is_initialized`] asks and must not be confused
/// with it (DECISIONS 288): that one asks whether *this caller* has a ledger to
/// read, which only a statement can answer, and answering it from the catalog
/// would report a table this role cannot touch as one it can.
fn ledger_is_there() -> String {
    format!(
        "SELECT count(*)::int8 AS present
       FROM pg_catalog.pg_class c
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = '{LEDGER_SCHEMA}'
        AND c.relname IN ('{STATE_TABLE_NAME}', '{LOCK_TABLE_NAME}')"
    )
}

/// A savepoint held around one statement whose failure this code expects to
/// handle.
///
/// **On this engine a failed statement aborts the whole transaction**, so every
/// `Err(e) if ... => Ok(...)` arm below is a claim that the connection is still
/// usable — and without something to rewind to, that claim is false inside a
/// transaction: the caller's next statement is `25P02` and its `COMMIT` becomes
/// a `ROLLBACK`. The ledger is read and written inside the apply's own
/// transaction (DECISIONS 147), so the callers are real ones.
///
/// Taken by **trying** it: measured, `SAVEPOINT` outside a transaction block is
/// `25P01` and harms nothing, so one round trip both establishes the savepoint
/// and answers whether this call is in a transaction at all — where a separate
/// probe would cost the same round trip and tell it less. Outside a transaction
/// there is nothing to protect and nothing is taken.
struct Recoverable {
    inside_a_transaction: bool,
}

impl Recoverable {
    async fn take(conn: &mut Conn) -> Result<Self, DbError> {
        match conn.execute(SAVEPOINT).await {
            Ok(()) => Ok(Recoverable {
                inside_a_transaction: true,
            }),
            Err(e) if is_outside_a_transaction(&e) => Ok(Recoverable {
                inside_a_transaction: false,
            }),
            Err(e) => Err(e),
        }
    }

    /// The statement did what was wanted: end the savepoint and leave the
    /// caller's transaction exactly as deep as it was.
    async fn release(self, conn: &mut Conn) -> Result<(), DbError> {
        if self.inside_a_transaction {
            conn.execute(RELEASE_SAVEPOINT).await?;
        }
        Ok(())
    }

    /// The statement failed and this code is handling that failure: put the
    /// transaction back where it was, so that "handled" means what it says.
    async fn rewind(self, conn: &mut Conn) -> Result<(), DbError> {
        if self.inside_a_transaction {
            conn.execute(ROLLBACK_TO_SAVEPOINT).await?;
        }
        Ok(())
    }

    /// The same, for a failure that is **not** being handled: the caller is
    /// about to be given the error, and this only leaves it able to run the
    /// diagnostics it wants to print. Its own failure is dropped, so it cannot
    /// replace the error that caused it.
    async fn rewind_quietly(self, conn: &mut Conn) {
        let _ = self.rewind(conn).await;
    }
}

/// How many of the two tables the catalog holds.
async fn ledger_present(conn: &mut Conn) -> Result<i64, DbError> {
    let rows = conn.query(&ledger_is_there()).await?;
    rows.first()
        .map(|row| number(row, "present"))
        .transpose()?
        .ok_or_else(|| missing("present"))
}

/// Creates the ledger and lock tables if they are not there yet.
///
/// **The DDL is not sent when there is nothing to create**, and that is a
/// permission question rather than an optimization. Measured on 18.6: a role
/// with `SELECT`, `INSERT` and `DELETE` on both ledger tables and no `CREATE`
/// on their schema gets `42501: permission denied for schema public` from
/// `CREATE TABLE IF NOT EXISTS public.__pbps_state` — the engine checks the
/// schema privilege before it notices the relation is already there. That is
/// exactly the least-privilege configuration SPEC §8.1 asks for and
/// [`crate::doctor`] reports as ready, and every `record` and every `lock` runs
/// this, so the whole deployment failed on a grant `doctor` had correctly said
/// was spent.
///
/// One batch, which on this engine is one transaction: PostgreSQL's DDL is
/// transactional, so the pair arrives whole or not at all. The other engine
/// can leave the ledger created and the lock missing, and its callers have to
/// cope; here that state is unreachable.
pub async fn ensure_tables(conn: &mut Conn) -> Result<(), DbError> {
    if ledger_present(conn).await? != 2 {
        let guard = Recoverable::take(conn).await?;
        let created = conn
            .execute(&format!("{CREATE_STATE};\n{CREATE_LOCK};"))
            .await;
        match created {
            Ok(()) => guard.release(conn).await?,
            // `IF NOT EXISTS` is not atomic against a concurrent creator:
            // measured, two sessions creating the same table at once leave
            // one of them with `23505` on `pg_type_typname_nsp_index` or
            // `42P07`, because the check and the create are two steps under
            // one lock the second session does not hold. Both mean the table
            // is there now, which is what the caller asked for.
            //
            // **Tolerating it is not enough**, twice over.
            //
            // The transaction: measured, the loser's *transaction* is aborted
            // by that error, so a bare success would hand back a connection
            // on which the caller's next statement is `25P02` — and `record`
            // runs inside the apply's transaction (DECISIONS 147), so the
            // whole deployment would fail on a race this arm claims to have
            // handled.
            //
            // And the claim itself: the SQLSTATE says a relation the DDL
            // creates was already there, **not** which one. Measured, a table
            // called `public.pk___pbps_state` — the name this DDL gives the
            // ledger's primary key, and an ordinary name for something else to
            // have — makes `CREATE TABLE IF NOT EXISTS public.__pbps_state`
            // fail with `42P07` while the ledger is still absent. So the
            // answer is not taken from the error at all: the catalog is asked
            // again, and only both tables being there is success. Otherwise
            // the original failure is the answer, with the name the engine
            // put in it.
            Err(e) if made_by_someone_else(&e) => {
                guard.rewind(conn).await?;
                if ledger_present(conn).await? != 2 {
                    return Err(e);
                }
            }
            // Anything else is still a failure — and the caller is left able
            // to report it: without the rewind its transaction is aborted, so
            // even the diagnostics it wants to run would come back `25P02`.
            Err(e) => {
                guard.rewind_quietly(conn).await;
                return Err(e);
            }
        }
    }
    // Both tables exist by this point, freshly created or already there —
    // either way a `__pbps_state` from before issue #103 still needs its
    // five timeline columns, and one just created by [`CREATE_STATE`] above
    // already has them, so this is cheap in the common case (DECISIONS 433).
    migrate_timeline_columns(conn).await
}

/// Whether the ledger's timeline-projection columns (issue #103) are already
/// there.
///
/// `pg_attribute` joined by name, like [`ledger_is_there`]: world-readable, so
/// this can be asked before [`migrate_timeline_columns`] decides whether to
/// send `ALTER TABLE` at all — which this engine's ownership rule makes
/// mandatory rather than an optimization (see that function's doc comment).
async fn timeline_columns_present(conn: &mut Conn) -> Result<bool, DbError> {
    let rows = conn.query(&timeline_columns_probe()).await?;
    let present: i64 = rows
        .first()
        .map(|row| number(row, "present"))
        .transpose()?
        .unwrap_or(0);
    Ok(present > 0)
}

fn timeline_columns_probe() -> String {
    format!(
        "SELECT count(*)::int8 AS present
           FROM pg_catalog.pg_attribute a
           JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = '{LEDGER_SCHEMA}' AND c.relname = '{STATE_TABLE_NAME}'
            AND a.attname = 'state_version' AND a.attnum > 0 AND NOT a.attisdropped"
    )
}

/// Adds the timeline columns to a `__pbps_state` from before issue #103, if
/// [`timeline_columns_present`] says they are not there yet.
///
/// The failure is rewritten rather than passed through: `must be owner of
/// table t` names neither the ledger nor what the `ALTER` was for, and
/// `doctor` does not yet ask for ownership of an *existing* ledger (only
/// [`crate::doctor::Needed::LedgerCreation`], while it does not exist yet) —
/// a gap reported alongside this change, not closed by it.
async fn migrate_timeline_columns(conn: &mut Conn) -> Result<(), DbError> {
    if timeline_columns_present(conn).await? {
        return Ok(());
    }
    let guard = Recoverable::take(conn).await?;
    match conn.execute(ADD_TIMELINE_COLUMNS).await {
        Ok(()) => {
            guard.release(conn).await?;
            Ok(())
        }
        Err(e) => {
            guard.rewind_quietly(conn).await;
            let code = e.server_error_code();
            Err(DbError::Driver {
                message: format!(
                    "public.__pbps_state is missing the timeline columns (state_version, \
                     tables_count, modules_count, staged_completed, staged_total) issue \
                     #103 added, and this role could not add them: {e}\n\
                     This is a one-time migration that needs ownership of \
                     public.__pbps_state — PostgreSQL authorizes ALTER TABLE by ownership, \
                     not by a grantable privilege — so a role holding only what `doctor` \
                     asks for today will meet this until that is fixed."
                ),
                code,
            })
        }
    }
}

/// The savepoint [`ensure_tables`] takes, named for what takes it: a caller
/// that already holds one of its own keeps it, because a `SAVEPOINT` of the
/// same name shadows rather than replaces, and the `RELEASE` below ends only
/// this one.
const SAVEPOINT: &str = "SAVEPOINT pbps_ensure_tables";
const ROLLBACK_TO_SAVEPOINT: &str = "ROLLBACK TO SAVEPOINT pbps_ensure_tables";
const RELEASE_SAVEPOINT: &str = "RELEASE SAVEPOINT pbps_ensure_tables";

/// `no_active_sql_transaction`: this connection is in autocommit, so there is
/// no savepoint to take and nothing an error here could abort.
const NO_ACTIVE_TRANSACTION: &str = "25P01";

fn is_outside_a_transaction(e: &DbError) -> bool {
    e.server_error_code().as_deref() == Some(NO_ACTIVE_TRANSACTION)
}

/// Whether this database has a ledger at all.
///
/// Every read has to ask first: querying a table that does not exist fails with
/// `relation "public.__pbps_state" does not exist`, which tells the user
/// nothing about what to do. The answer they need is "run baseline or
/// bootstrap", and only this distinction can produce it.
///
/// # Why the statement is attempted rather than the catalog asked
///
/// The obvious guard is `to_regclass('public.__pbps_state') IS NULL`, and it
/// cannot separate the three answers. **Measured on 18.6**, as a role with no
/// rights on the schema, `SELECT to_regclass('hidden.t')` does not return NULL
/// — it raises `42501: permission denied for schema hidden`. So the guard has
/// to handle an error anyway, and where it does not raise it is silent about
/// the difference between "no table" and "a table you may not read".
///
/// The statement separates them, and this engine says so in the code rather
/// than in the sentence: `42P01` when the relation is not there, `42501` when
/// it is there and this role may not read it — or when the schema itself is
/// closed to it. Only the first is `false`; the second stays an error, because
/// "I could not look" reported as "there is no ledger" is the direction that
/// ends in `bootstrap` against a database that already has one. (The other
/// engine needs the same distinction and gets it from 208 against 229 — that
/// one had to be found by measurement, because its catalog *hides* an object a
/// login has no permission on and answers NULL for it, DECISIONS 219.)
pub async fn is_initialized(conn: &mut Conn) -> Result<bool, DbError> {
    // Under a savepoint, because the answer this call is *for* — `42P01`, there
    // is no ledger — is an error, and an error inside a transaction takes the
    // transaction with it. See [`Recoverable`]. Every reader below goes through
    // here, so this is where the four of them get that protection.
    let guard = Recoverable::take(conn).await?;
    match conn.query(PROBE_STATE).await {
        Ok(_) => {
            guard.release(conn).await?;
            Ok(true)
        }
        Err(e) if is_missing_table(&e) => {
            guard.rewind(conn).await?;
            Ok(false)
        }
        Err(e) => {
            guard.rewind_quietly(conn).await;
            Err(e)
        }
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
    let rows = conn.query(&select_latest()).await?;
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
        .query_with(&select_history(), &[rows_wanted(limit).into()])
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
/// [`select_timeline`] never asks for `state_json` (DECISIONS 433): a second
/// query, [`select_legacy_state_json`], asks for it only for the rows that
/// predate the migration — `state_version IS NULL` — and only for those ids.
/// In the common case, once a ledger's rows are all migrated, that second
/// query is never sent, which is the property issue #103's sharp test pins: a
/// fully-migrated ledger whose `state_json` this role may not see still
/// answers `state list`.
pub async fn timeline(conn: &mut Conn, limit: u32) -> Result<Vec<TimelineEntry>, LedgerError> {
    if !is_initialized(conn).await? {
        return Err(LedgerError::NotInitialized);
    }
    // `state list` is a read and must not require ownership to run, so it
    // cannot call `ensure_tables` to make an unmigrated ledger's columns
    // appear — it asks the shape instead, with the same catalog probe
    // `migrate_timeline_columns` already uses, and sends the query that
    // matches (round-1 review finding on #103's own PR: `timeline` is the
    // only reader of `public.__pbps_state`'s shape, and both branches meet
    // at `legacy_ids` below whichever one ran).
    let projected: Vec<ProjectedRow> = if timeline_columns_present(conn).await? {
        let rows = conn
            .query_with(&select_timeline(), &[rows_wanted(limit).into()])
            .await?;
        rows.iter().map(projected_row).collect::<Result<_, _>>()?
    } else {
        let rows = conn
            .query_with(&select_timeline_unmigrated(), &[rows_wanted(limit).into()])
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
    // In pieces of at most `MAX_PARAMETERS` ids, not one statement for every
    // legacy id: this query binds one parameter per id, and until a deployer
    // has run a deployment after upgrading, every row is legacy, so a
    // long-lived ledger's ordinary `state list` is exactly the case that
    // could reach the extended protocol's own parameter-count ceiling in a
    // single request (a round-2 review finding on #103's own PR — the
    // pre-#103 query needed only its `$1` limit parameter and had no such
    // ceiling; the fix mirrors `pbps_mssql::state::timeline`'s identical
    // chunking of the same shape, one dialect apart).
    for chunk in legacy_ids.chunks(MAX_PARAMETERS) {
        let sql = select_legacy_state_json(chunk.len());
        let params: Vec<Param<'_>> = chunk.iter().map(|&id| id.into()).collect();
        match conn.query_with(&sql, &params).await {
            Ok(rows) => {
                for row in &rows {
                    let id: i64 = number(row, "id")?;
                    let state_json = text(row, "state_json")?;
                    // `read_json`, the same reader the pre-migration
                    // `timeline_from_row` used: version before shape, and the
                    // failure carried rather than returned (DECISIONS 222).
                    let state = StateSnapshot::read_json(&state_json)
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
            // a permission denial is not sensitive to which row is read; a
            // later chunk still asks for itself.
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

/// `LIMIT` takes a `bigint`, and the count is a `u32`.
///
/// Every value fits, so there is nothing to saturate and nothing to check —
/// which is worth a line only because the other dialect's `TOP` takes a signed
/// 32-bit integer, where `--limit 4294967295` wrapped to `-1` and the server
/// refused the query (DECISIONS 217). Two engines, one count, and the
/// conversion that is a hazard there is total here.
fn rows_wanted(limit: u32) -> i64 {
    i64::from(limit)
}

/// A count or version as the `integer` columns hold it: saturating — a
/// table, module or staged-statement count that does not fit `i32` is not a
/// real schema, and a wrapped negative would misreport the count rather than
/// merely cap it.
fn saturating_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Appends one state to the ledger and returns its id.
///
/// The columns beside `state_json` are projected from the snapshot rather than
/// passed separately: they exist so `status` can filter without parsing JSON,
/// and a caller able to set them independently could make them lie. The five
/// issue #103 added follow the same rule (DECISIONS 433).
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
    // `Param` — see the doc comment on `INSERT_STATE_STAGED`.
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
        Some(row) => Ok(number(row, "id")?),
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
        .map(|row| number(row, "id"))
        .collect::<Result<_, _>>()?;

    // The policy is decided in `pbps-db` and tested without a server; all that
    // is left here is the delete. Because ids come from an identity column and
    // the list is newest-first, the prunable set is always a contiguous tail,
    // so one `<=` covers it exactly.
    let doomed = ids_to_prune(&ids, keep);
    let Some(highest) = doomed.first().copied() else {
        return Ok(0);
    };
    Ok(conn.execute_with(DELETE_UP_TO, &[highest.into()]).await?)
}

/// Takes the apply lock, or reports who already holds it.
///
/// The insert is the gate, not the read that follows it: a check-then-insert
/// would let two pipelines through the check together. The read only happens
/// once the insert has reported that it changed nothing, and then only to name
/// the holder.
pub async fn lock(conn: &mut Conn, holder: &str) -> Result<(), LedgerError> {
    ensure_tables(conn).await?;
    if conn.execute_with(INSERT_LOCK, &[holder.into()]).await? > 0 {
        return Ok(());
    }
    match lock_holder(conn).await? {
        Some(info) => Err(LedgerError::Locked(info)),
        // The row was there when the insert ran and is gone now: whoever held
        // the lock released it in between. Reporting a holder this call cannot
        // name would be a sentence with a hole in it, and claiming the lock
        // would be a claim this call did not win, so it is neither — the caller
        // is told the lock was contended and can try again.
        None => Err(LedgerError::Locked(LockInfo {
            locked_by: "another operation, which released the lock while this one was reading it"
                .to_owned(),
            locked_at: "just now".to_owned(),
        })),
    }
}

/// `undefined_table`: the relation this statement names is not there. **Not**
/// the code for one that is there and may not be read — that is `42501`,
/// `insufficient_privilege`, and keeping the two apart is the whole point of
/// asking by SQLSTATE.
const UNDEFINED_TABLE: &str = "42P01";

/// `duplicate_table` and `unique_violation`: what a concurrent creator leaves
/// behind. See [`ensure_tables`].
const DUPLICATE_TABLE: &str = "42P07";
const UNIQUE_VIOLATION: &str = "23505";

/// Whether a failure means "that table does not exist".
///
/// A schema that is not there answers the same way — measured, a reference to
/// `no_such_schema.t` is `42P01: relation "no_such_schema.t" does not exist`,
/// not `3F000` — which is the right answer for a caller asking whether the
/// ledger is there: it is not, and `bootstrap` or `baseline` is what makes both
/// the schema's absence and the ledger's visible.
fn is_missing_table(e: &DbError) -> bool {
    e.server_error_code().as_deref() == Some(UNDEFINED_TABLE)
}

/// `insufficient_privilege`: this engine's one code for "you may not touch
/// this", table-level and column-level alike — unlike the other engine, which
/// has 229 and 230 for the two. By the time [`timeline`]'s fallback query
/// could meet this, [`is_initialized`]'s probe and [`select_timeline`] have
/// already succeeded against this same table, so a 42501 here can only be
/// about `state_json` — the shape issue #103's sharp test builds.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

fn is_select_denied(e: &DbError) -> bool {
    e.server_error_code().as_deref() == Some(INSUFFICIENT_PRIVILEGE)
}

/// Whether a failure means "somebody else created it first".
fn made_by_someone_else(e: &DbError) -> bool {
    matches!(
        e.server_error_code().as_deref(),
        Some(DUPLICATE_TABLE | UNIQUE_VIOLATION)
    )
}

/// Releases the lock. `false` means it was not held.
pub async fn unlock(conn: &mut Conn) -> Result<bool, DbError> {
    // The *lock* table, not the state table. Guarding on `is_initialized` would
    // mean a database whose `__pbps_state` had been dropped by hand reported
    // "not held" and left a live lock in place — with no command able to clear
    // it.
    let guard = Recoverable::take(conn).await?;
    match conn.execute_with(DELETE_LOCK, &[]).await {
        Ok(n) => {
            guard.release(conn).await?;
            Ok(n > 0)
        }
        // The tolerated answer is an error, so it needs the rewind the others
        // do: a `pbps unlock` against a database with no lock table must not
        // leave the caller's transaction dead on its way to saying "not held".
        Err(e) if is_missing_table(&e) => {
            guard.rewind(conn).await?;
            Ok(false)
        }
        Err(e) => {
            guard.rewind_quietly(conn).await;
            Err(e)
        }
    }
}

/// Who holds the deployment lock, if anyone.
///
/// A missing lock table answers `None` rather than failing: nothing can be
/// holding a lock that does not exist. That is deliberately *not* the same as
/// the table being unreadable, which stays an error — callers report the two
/// differently, and "I could not look" must never be flattened into "nothing
/// there".
pub async fn lock_holder(conn: &mut Conn) -> Result<Option<LockInfo>, DbError> {
    let guard = Recoverable::take(conn).await?;
    let rows = match conn.query(&select_lock()).await {
        Ok(rows) => {
            guard.release(conn).await?;
            rows
        }
        Err(e) if is_missing_table(&e) => {
            guard.rewind(conn).await?;
            return Ok(None);
        }
        Err(e) => {
            guard.rewind_quietly(conn).await;
            return Err(e);
        }
    };
    match rows.first() {
        Some(row) => Ok(Some(LockInfo {
            locked_by: text(row, "locked_by")?,
            locked_at: text(row, "locked_at")?,
        })),
        None => Ok(None),
    }
}

fn entry_from_row(row: &Row) -> Result<LedgerEntry, LedgerError> {
    let id: i64 = number(row, "id")?;
    let state_json = text(row, "state_json")?;
    // `from_json`, not `from_str` then `check_version`: an entry written by an
    // older pbps is refused by its version and the remedy that goes with it,
    // rather than by whichever field of its older shape serde reached first.
    // This is the reader #74 added, and there is no second path to it.
    let snapshot = StateSnapshot::from_json(&state_json)
        .map_err(|message| LedgerError::BadEntry { id, message })?;
    Ok(LedgerEntry {
        id,
        applied_at: text(row, "applied_at")?,
        snapshot,
    })
}

/// A ledger row's projected columns, without `state_json` (DECISIONS 433):
/// [`select_timeline`] never asks for it. `state` is `None` exactly when this
/// row predates issue #103's migration — `state_version IS NULL` — and
/// [`timeline`] must still ask [`select_legacy_state_json`] for it.
///
/// `Some(Err(_))` is a third shape, distinct from `None`: a row whose columns
/// *are* populated but whose version this build does not read — a row a
/// newer pbps wrote. That is not "legacy" and must never reach the JSON
/// fallback, which would find nothing wrong with a `state_json` this build
/// simply has not been asked to parse; refusing it here, before the fallback
/// even runs, is what
/// [`pbps_db::ledger::TimelineState::from_projected`] makes unavoidable (a
/// round-1 review finding on #103's own PR).
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

fn optional_i32(row: &Row, column: &str) -> Result<Option<i32>, DbError> {
    row.try_get::<i32>(column)
}

fn required_i32(row: &Row, column: &str) -> Result<i32, DbError> {
    optional_i32(row, column)?.ok_or_else(|| missing(column))
}

/// A count or a version read back from an `integer` column. Negative is
/// impossible by construction — [`record`] only ever writes what
/// `saturating_i32` produces — so it is reported rather than clamped: a
/// negative here means the row and this reader have gone out of step, not
/// that the count was merely large.
fn as_count(n: i32, column: &str) -> Result<usize, DbError> {
    usize::try_from(n).map_err(|_| DbError::BadRow(format!("`{column}` is {n}, not a count")))
}

fn as_version(n: i32, column: &str) -> Result<u32, DbError> {
    u32::try_from(n).map_err(|_| DbError::BadRow(format!("`{column}` is {n}, not a version")))
}

fn projected_row(row: &Row) -> Result<ProjectedRow, LedgerError> {
    let id: i64 = number(row, "id")?;

    // `state_version` is the migration's own marker: written by every
    // `record` from here on, and NULL on every row from before it. See
    // `pbps_mssql::state::projected_row` for why this one column, rather than
    // `tables_count` or the staged pair, is what decides the fallback.
    let state = match optional_i32(row, "state_version")? {
        None => None,
        Some(version) => {
            let tables = as_count(required_i32(row, "tables_count")?, "tables_count")?;
            let modules = as_count(required_i32(row, "modules_count")?, "modules_count")?;
            let staged = match (
                optional_i32(row, "staged_completed")?,
                optional_i32(row, "staged_total")?,
            ) {
                (Some(completed), Some(total)) => Some(TimelineStaged {
                    completed: as_count(completed, "staged_completed")?,
                    total: as_count(total, "staged_total")?,
                }),
                // A migrated row that is not a staged checkpoint: both are
                // NULL together, which is what "not mid-deployment" means
                // (`pbps_model::StagedProgress`'s own doc comment).
                _ => None,
            };
            Some(TimelineState::from_projected(
                as_version(version, "state_version")?,
                tables,
                modules,
                staged,
            ))
        }
    };

    ledger_row(row, id, state)
}

/// A row from [`select_timeline_unmigrated`]: the six columns a
/// `__pbps_state` from before issue #103 has, and nothing else — `state` is
/// always `None`, because a table with no timeline columns has no row that
/// could be anything but legacy.
fn projected_row_unmigrated(row: &Row) -> Result<ProjectedRow, LedgerError> {
    let id: i64 = number(row, "id")?;
    ledger_row(row, id, None)
}

/// The six columns every shape of `__pbps_state` this crate reads has,
/// shared by [`projected_row`] and [`projected_row_unmigrated`] so the two
/// never drift on how one of them is read.
fn ledger_row(
    row: &Row,
    id: i64,
    state: Option<Result<TimelineState, Unreadable>>,
) -> Result<ProjectedRow, LedgerError> {
    Ok(ProjectedRow {
        id,
        applied_at: text(row, "applied_at")?,
        // The projected columns, which are what makes an unreadable row still
        // a row worth showing.
        kind: text(row, "kind")?,
        git_sha: optional_text(row, "git_sha")?,
        plan_checksum: optional_text(row, "plan_checksum")?,
        operator: text(row, "operator")?,
        reason: optional_text(row, "reason")?,
        state,
    })
}

/// A column the ledger declares `NOT NULL` that came back NULL, or one the
/// statement did not return at all.
///
/// Reported rather than defaulted: an empty string here would put a blank date
/// on a status screen or an empty operator in an audit record, which is the
/// silent wrong answer in the one place that exists to be read after the fact.
fn missing(column: &str) -> DbError {
    DbError::BadRow(format!(
        "the ledger row has no `{column}`, which means the statement and this code have gone out \
         of step"
    ))
}

fn text(row: &Row, column: &str) -> Result<String, DbError> {
    optional_text(row, column)?.ok_or_else(|| missing(column))
}

fn optional_text(row: &Row, column: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(column)?.map(str::to_owned))
}

fn number(row: &Row, column: &str) -> Result<i64, DbError> {
    row.try_get::<i64>(column)?.ok_or_else(|| missing(column))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every statement this module can send, in one list, so that a rule
    /// asserted about "the SQL here" cannot quietly stop covering a statement
    /// somebody adds. Three of them are built rather than written, which is
    /// why the list holds `String`s.
    fn state_statements() -> Vec<String> {
        [
            CREATE_STATE,
            ADD_TIMELINE_COLUMNS,
            INSERT_STATE_STAGED,
            INSERT_STATE_NOT_STAGED,
            SELECT_IDS,
            DELETE_UP_TO,
            PROBE_STATE,
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([
            select_latest(),
            select_history(),
            select_timeline(),
            select_timeline_unmigrated(),
            select_legacy_state_json(3),
        ])
        .collect()
    }

    fn lock_statements() -> Vec<String> {
        [CREATE_LOCK, INSERT_LOCK, DELETE_LOCK]
            .into_iter()
            .map(str::to_owned)
            .chain([select_lock()])
            .collect()
    }

    fn every_statement() -> Vec<String> {
        let mut all = state_statements();
        all.extend(lock_statements());
        all
    }

    /// A count larger than the other engine's `TOP` can hold is an ordinary
    /// number here. The test is the boundary the other dialect gets wrong, run
    /// against the conversion that cannot.
    #[test]
    fn every_count_a_caller_can_ask_for_survives_the_conversion() {
        assert_eq!(rows_wanted(1), 1);
        assert_eq!(rows_wanted(i32::MAX as u32), i64::from(i32::MAX));
        assert_eq!(rows_wanted(i32::MAX as u32 + 1), i64::from(i32::MAX) + 1);
        assert_eq!(rows_wanted(u32::MAX), i64::from(u32::MAX));
    }

    /// The constant and the DDL have to agree, or the truncation is measured
    /// against a width the column does not have.
    #[test]
    fn the_reason_width_is_the_one_the_ddl_declares() {
        assert!(
            CREATE_STATE.contains(&format!("reason           varchar({REASON_CHARS})  NULL")),
            "{CREATE_STATE}"
        );
    }

    /// This engine counts a `varchar`'s width in characters, so an emoji costs
    /// one and the whole declared width is usable. The SQL Server counterpart
    /// cuts the same text in half, and a shared helper would have to be wrong
    /// on one of the two engines.
    #[test]
    fn a_reason_is_cut_by_characters_and_never_inside_one() {
        let ascii = "x".repeat(REASON_CHARS);
        assert_eq!(truncate_reason(&ascii), ascii);
        assert_eq!(truncate_reason(&format!("{ascii}y")), ascii);

        let emoji = "😀".repeat(REASON_CHARS);
        assert_eq!(truncate_reason(&emoji), emoji);
        assert_eq!(truncate_reason(&format!("{emoji}😀")), emoji);

        // A cut never splits a character, so what remains is still text.
        let cut = truncate_reason(&format!("{emoji}x"));
        assert_eq!(cut.chars().count(), REASON_CHARS);
        assert!(cut.chars().all(|c| c == '😀'));
    }

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
        for sql in state_statements() {
            assert!(sql.contains(STATE_TABLE), "{sql}");
        }
        for sql in lock_statements() {
            assert!(sql.contains(LOCK_TABLE), "{sql}");
        }
    }

    /// Every statement names its table with the schema in front of it. An
    /// unqualified `__pbps_state` would resolve through whatever `search_path`
    /// the session has — and this crate's own reads pin that path to empty,
    /// which would leave the ledger unreachable from the very sessions that
    /// read the catalog (DECISIONS 254).
    #[test]
    fn no_statement_leaves_its_table_to_the_search_path() {
        for sql in every_statement() {
            for bare in [STATE_TABLE_NAME, LOCK_TABLE_NAME] {
                for (at, _) in sql.match_indices(bare) {
                    // A constraint name (`pk___pbps_state`) contains the table
                    // name and is not a reference to the table.
                    let before = &sql[..at];
                    assert!(
                        before.ends_with(&format!("{LEDGER_SCHEMA}.")) || before.ends_with('_'),
                        "`{bare}` is named without its schema in: {sql}"
                    );
                }
            }
        }
    }

    /// Every user-supplied value reaches the server as a parameter. A `'` in an
    /// operator's name or a drop reason must not be able to end a statement.
    #[test]
    fn no_statement_interpolates_a_value() {
        for sql in every_statement() {
            assert!(
                !sql.contains("'{") && !sql.contains("{}"),
                "a format placeholder in ledger SQL means a value is being pasted: {sql}"
            );
        }
        assert!(
            INSERT_STATE_STAGED.contains("$11"),
            "all eleven values are bound"
        );
        assert!(
            INSERT_STATE_NOT_STAGED.contains("$9")
                && INSERT_STATE_NOT_STAGED.contains("NULL, NULL"),
            "the nine bound values plus the two literal NULLs"
        );
    }

    /// One placeholder per id asked about, in order.
    #[test]
    fn the_legacy_query_binds_one_placeholder_per_id() {
        assert_eq!(
            select_legacy_state_json(3),
            format!("SELECT id, state_json FROM {STATE_TABLE} WHERE id IN ($1, $2, $3)")
        );
        assert_eq!(
            select_legacy_state_json(1),
            format!("SELECT id, state_json FROM {STATE_TABLE} WHERE id IN ($1)")
        );
    }

    /// A count too large for `i32` saturates rather than wrapping.
    #[test]
    fn a_count_too_large_for_the_column_saturates() {
        assert_eq!(saturating_i32(0), 0);
        assert_eq!(saturating_i32(i32::MAX as u64), i32::MAX);
        assert_eq!(saturating_i32(i32::MAX as u64 + 1), i32::MAX);
        assert_eq!(saturating_i32(u64::MAX), i32::MAX);
    }

    /// A negative value in a column this build only ever writes non-negative
    /// numbers to is reported rather than clamped to zero.
    #[test]
    fn a_negative_count_or_version_is_reported_not_clamped() {
        assert!(as_count(-1, "tables_count").is_err());
        assert_eq!(as_count(5, "tables_count").unwrap(), 5);
        assert!(as_version(-1, "state_version").is_err());
        assert_eq!(as_version(7, "state_version").unwrap(), 7);
    }

    /// The timestamp is rendered by a pattern and never cast, on every
    /// statement that returns one. A `::text` here would read differently in a
    /// session whose `DateStyle` is not this one's.
    #[test]
    fn every_timestamp_is_rendered_by_a_pattern_that_no_setting_moves() {
        for sql in [
            select_latest(),
            select_history(),
            select_timeline(),
            select_timeline_unmigrated(),
            select_lock(),
        ] {
            assert!(
                sql.contains("to_char(") && sql.contains("YYYY-MM-DD\"T\"HH24:MI:SS.MS"),
                "{sql}"
            );
            assert!(!sql.contains("::text"), "{sql}");
            // `TM` is what makes `to_char` read `lc_time`; nothing here may.
            assert!(!sql.contains("TM"), "{sql}");
        }
    }
}
