//! Appending an entry to the ledger and pruning it.

use std::borrow::Cow;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::ledger::{LedgerError, ids_to_prune};
use pbps_db::sqlite::{Connection, Value};
use pbps_model::StateSnapshot;

use super::busy::{self, BUSY_WAIT};
use super::ddl::{ensure_tables_within, is_initialized};
use super::{OPERATOR_CHARS, REASON_CHARS, STATE_TABLE};

/// The widths of the other bounded columns, in characters, as the recipe's
/// `CHECK`s have them.
const KIND_CHARS: usize = 16;
const GIT_SHA_CHARS: usize = 40;
const CHECKSUM_CHARS: usize = 64;

/// The snapshot with its operator cut to [`OPERATOR_CHARS`], cloned only when it
/// has to be.
///
/// The operator is git's `user.name`, which nothing bounds, and the insert of a
/// longer one fails the `CHECK` on `apply`, after the statements ran. Cut in the
/// snapshot and not only in the bound column, so `state_json` and its projection
/// agree on what was recorded.
fn bounded_operator(snapshot: &StateSnapshot) -> Cow<'_, StateSnapshot> {
    if snapshot.operator.chars().count() <= OPERATOR_CHARS {
        return Cow::Borrowed(snapshot);
    }
    let mut bounded = snapshot.clone();
    bounded.operator = snapshot.operator.chars().take(OPERATOR_CHARS).collect();
    Cow::Owned(bounded)
}

/// Refuses a value longer than its column holds, counted here in characters.
///
/// The column's `CHECK` cannot be the bound: `length()` stops at an embedded
/// NUL, so a text with one passes it however long it is (measured). A value the
/// user supplied is refused and never cut, as the `CHECK` would refuse it.
fn fits(what: &str, value: Option<&str>, most: usize) -> Result<(), LedgerError> {
    let Some(found) = value.map(|v| v.chars().count()).filter(|n| *n > most) else {
        return Ok(());
    };
    Err(LedgerError::Db(DbError::Refused(format!(
        "the {what} to record is {found} characters and the ledger holds {most}; nothing was \
         recorded"
    ))))
}

/// A count as the `INTEGER` columns hold it. It cannot exceed `i64` on a real
/// schema; saturating keeps a wrapped negative from misreporting a count.
fn count(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::Text(text.to_owned()))
}

/// The ledger's `INSERT`. `RETURNING id` is one statement and cannot be confused
/// by a trigger someone added to the ledger, which a `last_insert_rowid()` read
/// after it could. The last five values are the projection of the snapshot, so
/// they cannot disagree with `state_json`, and a value that is absent is `NULL`:
/// the staged pair is `NULL` on every row that is not a staged checkpoint.
pub(super) const INSERT_STATE: &str = "\
INSERT INTO main.__pbps_state
    (kind, git_sha, plan_checksum, state_json, operator, reason,
     state_version, tables_count, modules_count, staged_completed, staged_total)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
RETURNING id";

/// Appends one state to the ledger and returns its id.
///
/// The columns beside `state_json` are projected from the snapshot and never
/// passed separately: they exist so a list can be drawn without parsing the JSON,
/// and a caller able to set them could make them lie.
///
/// It runs plain statements and never `BEGIN`: inside an apply's transaction it
/// is rolled back with it, and outside one it is a statement of its own, atomic
/// as any is. (The ledger is created first when it is not there, under
/// [`ensure_tables`](super::ensure_tables)'s savepoint.)
pub fn record(c: &Connection, snapshot: &StateSnapshot) -> Result<i64, LedgerError> {
    record_within(c, snapshot, BUSY_WAIT)
}

/// [`record`] with the wait given, so a test need not wait five seconds.
pub(crate) fn record_within(
    c: &Connection,
    snapshot: &StateSnapshot,
    wait: Duration,
) -> Result<i64, LedgerError> {
    busy::waiting(c, wait, || append(c, snapshot, wait))
}

fn append(c: &Connection, snapshot: &StateSnapshot, wait: Duration) -> Result<i64, LedgerError> {
    snapshot
        .validate_data_work()
        .map_err(|e| LedgerError::BadEntry {
            id: 0,
            message: e.to_string(),
        })?;
    ensure_tables_within(c, wait)?;
    let snapshot = &*bounded_operator(snapshot);
    let state_json = serde_json::to_string(snapshot).map_err(|e| LedgerError::BadEntry {
        id: 0,
        message: format!("the snapshot could not be serialized: {e}"),
    })?;
    let (completed, total) = match &snapshot.staged {
        Some(progress) => (count(progress.completed), count(progress.total)),
        None => (Value::Null, Value::Null),
    };
    let git_sha = snapshot.git_sha.as_ref().map(pbps_model::Text::raw);
    let reason = snapshot.reason.as_ref().map(pbps_model::Text::raw);
    fits("kind", Some(snapshot.kind.as_str()), KIND_CHARS)?;
    fits("git revision", git_sha, GIT_SHA_CHARS)?;
    fits(
        "plan checksum",
        snapshot.plan_checksum.as_deref(),
        CHECKSUM_CHARS,
    )?;
    fits("reason", reason, REASON_CHARS)?;
    let params = [
        Value::Text(snapshot.kind.as_str().to_owned()),
        text(git_sha),
        text(snapshot.plan_checksum.as_deref()),
        Value::Text(state_json),
        Value::Text(snapshot.operator.clone()),
        text(reason),
        Value::Integer(i64::from(snapshot.version)),
        count(snapshot.schema.tables.len()),
        count(snapshot.schema.modules.len()),
        completed,
        total,
    ];
    let rows = c
        .query(INSERT_STATE, &params)
        .map_err(|e| busy::explained(e, wait))?;
    match rows.first() {
        Some(row) => Ok(row.integer(0)?),
        None => Err(LedgerError::Db(DbError::BadRow(
            "the ledger insert returned no id".into(),
        ))),
    }
}

/// Deletes all but the `keep` newest entries and returns how many went.
///
/// `keep = 0` keeps one: the newest entry is the baseline, and the policy of
/// which ids survive is `pbps_db`'s and is tested without a database. Because ids
/// only grow (`AUTOINCREMENT`) and the list is newest first, what goes is always
/// a contiguous tail, so one `<=` covers it exactly, and an entry recorded
/// between the read and the delete is newer than the boundary and is not touched.
pub fn prune(c: &Connection, keep: u32) -> Result<u64, LedgerError> {
    busy::waiting(c, BUSY_WAIT, || delete_all_but(c, keep, BUSY_WAIT))
}

fn delete_all_but(c: &Connection, keep: u32, wait: Duration) -> Result<u64, LedgerError> {
    if !is_initialized(c)? {
        return Err(LedgerError::NotInitialized);
    }
    let ids: Vec<i64> = c
        .query(
            &format!("SELECT id FROM {STATE_TABLE} ORDER BY id DESC"),
            &[],
        )
        .map_err(|e| busy::explained(e, wait))?
        .iter()
        .map(|row| row.integer(0))
        .collect::<Result<_, _>>()?;
    let Some(highest) = ids_to_prune(&ids, keep).first().copied() else {
        return Ok(0);
    };
    Ok(c.execute(
        &format!("DELETE FROM {STATE_TABLE} WHERE id <= ?1"),
        &[Value::Integer(highest)],
    )
    .map_err(|e| busy::explained(e, wait))?)
}
