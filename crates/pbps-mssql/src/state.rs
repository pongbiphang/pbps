//! The `__pbps_state` ledger and the `__pbps_lock` lock, in T-SQL (SPEC §8.1).
//!
//! The types and the meaning live in `pbps_db::ledger`; only the statements are
//! here, for the same reason [`crate::catalog`]'s queries are: they are T-SQL,
//! and Phase 4's PostgreSQL crate will need its own. Nothing in this file
//! interpolates a user-supplied value into SQL — every one of them is bound.
//!
//! # Why these two tables are not part of the managed set
//!
//! [`crate::catalog`]'s table query excludes anything named `__pbps_*`, so the
//! tool never introspects its own bookkeeping and never plans a change to it.
//! Without that, the first `plan` after a `snapshot` would propose dropping the
//! ledger — the declarations do not mention it.

use pbps_db::ledger::{LedgerEntry, LedgerError, LockInfo, ids_to_prune};
use pbps_db::{Conn, DbError};
use pbps_model::StateSnapshot;

use crate::catalog::{get, opt};

/// `IF OBJECT_ID` rather than `CREATE OR ALTER`: creating the ledger must be
/// safe to run on every command that writes one, and re-running must not
/// disturb the rows already there.
const CREATE_STATE: &str = "\
IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL
BEGIN
    CREATE TABLE dbo.__pbps_state (
        id            BIGINT IDENTITY(1,1) NOT NULL
                      CONSTRAINT pk___pbps_state PRIMARY KEY,
        applied_at    DATETIME2(3)   NOT NULL
                      CONSTRAINT df___pbps_state_applied_at DEFAULT SYSUTCDATETIME(),
        kind          VARCHAR(16)    NOT NULL,
        git_sha       VARCHAR(40)    NULL,
        plan_checksum CHAR(64)       NULL,
        state_json    NVARCHAR(MAX)  NOT NULL,
        operator      NVARCHAR(128)  NOT NULL,
        reason        NVARCHAR(1000) NULL
    );
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

/// `OUTPUT INSERTED.id` rather than `SCOPE_IDENTITY()`: it is one round trip,
/// and it cannot be confused by a trigger someone added to the ledger.
const INSERT_STATE: &str = "\
INSERT INTO dbo.__pbps_state (kind, git_sha, plan_checksum, state_json, operator, reason)
OUTPUT INSERTED.id
VALUES (@P1, @P2, @P3, @P4, @P5, @P6);";

const SELECT_IDS: &str = "SELECT id FROM dbo.__pbps_state ORDER BY id DESC;";

const DELETE_UP_TO: &str = "DELETE FROM dbo.__pbps_state WHERE id <= @P1;";

const SELECT_LOCK: &str = "\
SELECT locked_by, CONVERT(varchar(23), locked_at, 126) AS locked_at
  FROM dbo.__pbps_lock WHERE id = 1;";

const INSERT_LOCK: &str = "INSERT INTO dbo.__pbps_lock (id, locked_by) VALUES (1, @P1);";

const DELETE_LOCK: &str = "DELETE FROM dbo.__pbps_lock WHERE id = 1;";

/// Creates the ledger and lock tables if they are not there yet.
pub async fn ensure_tables(conn: &mut Conn) -> Result<(), DbError> {
    conn.execute(CREATE_STATE).await?;
    conn.execute(CREATE_LOCK).await
}

/// Whether this database has a ledger at all.
///
/// Every read has to ask first: querying a table that does not exist fails with
/// "invalid object name", which tells the user nothing about what to do. The
/// answer they need is "run baseline or bootstrap", and only this distinction
/// can produce it.
pub async fn is_initialized(conn: &mut Conn) -> Result<bool, DbError> {
    let rows = conn
        .query("SELECT CASE WHEN OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL THEN 0 ELSE 1 END AS present;")
        .await?;
    let present: i32 = match rows.first() {
        Some(row) => get(row, "present")?,
        None => return Err(DbError::BadRow("`present` returned no row".into())),
    };
    Ok(present == 1)
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
pub async fn history(conn: &mut Conn, limit: u32) -> Result<Vec<LedgerEntry>, LedgerError> {
    if !is_initialized(conn).await? {
        return Err(LedgerError::NotInitialized);
    }
    let limit = limit as i32;
    let rows = conn.query_with(SELECT_HISTORY, &[limit.into()]).await?;
    rows.iter().map(entry_from_row).collect()
}

/// Appends one state to the ledger and returns its id.
///
/// The columns beside `state_json` are projected from the snapshot rather than
/// passed separately: they exist so `status` can filter without parsing JSON,
/// and a caller able to set them independently could make them lie.
pub async fn record(conn: &mut Conn, snapshot: &StateSnapshot) -> Result<i64, LedgerError> {
    ensure_tables(conn).await?;
    let state_json = serde_json::to_string(snapshot).map_err(|e| LedgerError::BadEntry {
        id: 0,
        message: format!("the snapshot could not be serialized: {e}"),
    })?;

    let kind = snapshot.kind.as_str();
    let rows = conn
        .query_with(
            INSERT_STATE,
            &[
                kind.into(),
                snapshot.git_sha.as_deref().into(),
                snapshot.plan_checksum.as_deref().into(),
                state_json.as_str().into(),
                snapshot.operator.as_str().into(),
                snapshot.reason.as_deref().into(),
            ],
        )
        .await?;
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
/// "no lock", which is the one direction this tool must never round in.
///
/// `HAS_PERMS_BY_NAME` does not separate them either — measured against a real
/// server, it answers **0** both for an absent table and for one hidden this
/// way. The statement itself is the only thing that does: 208 for absent, 229
/// for denied, and every other failure stays a failure.
fn is_missing_table(e: &DbError) -> bool {
    e.server_error_code().as_deref() == Some(INVALID_OBJECT_NAME)
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
    let snapshot: StateSnapshot =
        serde_json::from_str(state_json).map_err(|e| LedgerError::BadEntry {
            id,
            message: e.to_string(),
        })?;
    snapshot
        .check_version()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The constant and the DDL have to agree, or the truncation is measured
    /// against a width the column does not have.
    #[test]
    fn the_reason_width_is_the_one_the_ddl_declares() {
        assert!(
            CREATE_STATE.contains(&format!(
                "reason        NVARCHAR({REASON_UTF16_UNITS}) NULL"
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
    use pbps_db::ledger::{LOCK_TABLE, STATE_TABLE};

    /// The two table names in the SQL must be the ones `pbps-db` documents and
    /// the ones the catalog query excludes; a mismatch would have the tool
    /// planning changes to its own ledger.
    #[test]
    fn the_statements_name_the_documented_tables() {
        assert_eq!(STATE_TABLE, "dbo.__pbps_state");
        assert_eq!(LOCK_TABLE, "dbo.__pbps_lock");
        for sql in [
            CREATE_STATE,
            SELECT_LATEST,
            SELECT_HISTORY,
            INSERT_STATE,
            SELECT_IDS,
            DELETE_UP_TO,
        ] {
            assert!(sql.contains("__pbps_state"), "{sql}");
        }
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
            INSERT_STATE,
            DELETE_UP_TO,
            INSERT_LOCK,
            DELETE_LOCK,
        ] {
            assert!(
                !sql.contains("'{") && !sql.contains("{}"),
                "a format placeholder in ledger SQL means a value is being pasted: {sql}"
            );
        }
        assert!(INSERT_STATE.contains("@P6"), "all six values are bound");
    }
}
