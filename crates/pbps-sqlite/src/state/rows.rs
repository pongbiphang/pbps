//! Reading one ledger row back: what a statement returned, as the shapes of
//! `pbps_db::ledger`.
//!
//! Three kinds of failure are kept apart, because each has a different remedy
//! and a different reach:
//!
//! - a statement that returned the wrong columns (too few, or NULL where the
//!   table says `NOT NULL`) means the statement and this code have gone out of
//!   step. It would show on every row, so it is **thrown**: [`DbError::BadRow`].
//! - a snapshot this build cannot read (a version outside the readable range, or
//!   text that does not parse) is a fact about **one row**, and a timeline carries
//!   it with that row ([`Unreadable`]) so one such row never erases the history
//!   above it.
//! - a projected column that holds the wrong kind of value (text in a count) is
//!   also one row's: SQLite does not enforce a column's type, so a hand-edited
//!   row can hold it, and a drifted statement could not.

use pbps_db::DbError;
use pbps_db::ledger::{LedgerError, StoredEntry, TimelineEntry, TimelineStaged, TimelineState};
use pbps_db::sqlite::Record;
use pbps_model::{StateSnapshot, Text, Unreadable};

/// The columns [`TIMELINE_COLUMNS`] returns.
pub(super) const TIMELINE_WIDTH: usize = 13;

/// What `timeline` selects, in the order [`timeline_entry`] reads them. The last
/// is `state_json` for a row with no projection, and NULL for every other: the
/// common case never transfers the snapshot at all.
pub(super) const TIMELINE_COLUMNS: &str = "id, applied_at, kind, git_sha, plan_checksum, operator, \
     reason, state_version, tables_count, modules_count, staged_completed, staged_total, \
     CASE WHEN state_version IS NULL THEN state_json END";

/// `row` must have `width` columns. A narrower row is a statement that drifted.
fn width(row: &Record, width: usize) -> Result<(), DbError> {
    if row.len() == width {
        return Ok(());
    }
    Err(DbError::BadRow(format!(
        "the ledger statement returned {} columns where this code reads {width}; the statement \
         and this code have gone out of step",
        row.len()
    )))
}

/// A column the ledger declares `NOT NULL` that came back NULL, or of another
/// type. Reported and never defaulted: an empty string would put a blank date on
/// a status screen or an empty operator in an audit record, in the one place that
/// exists to be read after the fact.
fn missing(column: &str, cause: DbError) -> DbError {
    cause.context(format!(
        "the ledger row has no usable `{column}`, which means the statement and this code have \
         gone out of step or the row was edited by hand"
    ))
}

fn text(row: &Record, index: usize, column: &str) -> Result<String, DbError> {
    row.text(index)
        .map(str::to_owned)
        .map_err(|e| missing(column, e))
}

fn optional_text(row: &Record, index: usize, column: &str) -> Result<Option<String>, DbError> {
    row.optional_text(index)
        .map(|value| value.map(str::to_owned))
        .map_err(|e| missing(column, e))
}

fn integer(row: &Record, index: usize, column: &str) -> Result<i64, DbError> {
    row.integer(index).map_err(|e| missing(column, e))
}

/// The three columns that make a ledger row an entry.
pub(super) fn stored(row: &Record) -> Result<StoredEntry, DbError> {
    width(row, 3)?;
    Ok(StoredEntry {
        id: integer(row, 0, "id")?,
        applied_at: text(row, 1, "applied_at")?,
        state_json: text(row, 2, "state_json")?,
    })
}

/// A stored entry whose snapshot is read, or `BadEntry` naming why not.
pub(super) fn entry(row: &Record) -> Result<pbps_db::ledger::LedgerEntry, LedgerError> {
    let stored = stored(row)?;
    let id = stored.id;
    stored.read().map_err(|error| LedgerError::BadEntry {
        id,
        message: error.to_string(),
    })
}

/// One row of a timeline.
pub(super) fn timeline_entry(row: &Record) -> Result<TimelineEntry, DbError> {
    width(row, TIMELINE_WIDTH)?;
    Ok(TimelineEntry {
        id: integer(row, 0, "id")?,
        applied_at: text(row, 1, "applied_at")?,
        kind: text(row, 2, "kind")?,
        git_sha: optional_text(row, 3, "git_sha")?.map(Text::from),
        plan_checksum: optional_text(row, 4, "plan_checksum")?,
        operator: text(row, 5, "operator")?,
        reason: optional_text(row, 6, "reason")?.map(Text::from),
        state: projected_state(row)?,
    })
}

/// A projected column as an integer, or why this row's value is not one.
fn cell(row: &Record, index: usize, column: &str) -> Result<Option<i64>, Unreadable> {
    row.optional_integer(index)
        .map_err(|_| Unreadable::Malformed(format!("`{column}` does not hold an integer")))
}

/// A projected count: absent beside a `state_version`, negative and too large
/// are each this row's, and `record` never writes any of them.
fn count(row: &Record, index: usize, column: &str) -> Result<usize, Unreadable> {
    match cell(row, index, column)? {
        Some(n) => usize::try_from(n)
            .map_err(|_| Unreadable::Malformed(format!("`{column}` is {n}, not a count"))),
        None => Err(Unreadable::Malformed(format!(
            "`{column}` is NULL beside a `state_version`"
        ))),
    }
}

/// The four numbers a timeline row draws, from the projection when the row has
/// one and from its own JSON when it does not.
///
/// `state_version` is the marker: `record` writes it on every row. The version is
/// checked **before any other projected column is read**, so a newer build's row
/// that also holds a value this build cannot parse is refused as the version it
/// is. The JSON fallback is for a row a person wrote or a ledger from another
/// source; for it the version check is `read_json`'s, the same reader
/// `entry` uses.
fn projected_state(row: &Record) -> Result<Result<TimelineState, Unreadable>, DbError> {
    let version = match cell(row, 7, "state_version") {
        Ok(version) => version,
        Err(unreadable) => return Ok(Err(unreadable)),
    };
    let Some(version) = version else {
        let json = optional_text(row, 12, "state_json")?.ok_or_else(|| {
            DbError::BadRow(
                "a ledger row with no `state_version` has no `state_json` to read it from".into(),
            )
        })?;
        return Ok(StateSnapshot::read_json(&json).map(|s| TimelineState::from_snapshot(&s)));
    };
    Ok(projection(row, version))
}

fn projection(row: &Record, version: i64) -> Result<TimelineState, Unreadable> {
    let version = u32::try_from(version).map_err(|_| {
        Unreadable::Malformed(format!("`state_version` is {version}, not a version"))
    })?;
    pbps_model::check_readable_version(version)?;
    let tables = count(row, 8, "tables_count")?;
    let modules = count(row, 9, "modules_count")?;
    // `(Some, Some)` and `(None, None)` are the only two pairs `record` writes.
    // A half pair is refused as malformed and not read as "not staged".
    let staged = match (
        cell(row, 10, "staged_completed")?,
        cell(row, 11, "staged_total")?,
    ) {
        (Some(_), Some(_)) => Some(TimelineStaged {
            completed: count(row, 10, "staged_completed")?,
            total: count(row, 11, "staged_total")?,
        }),
        (None, None) => None,
        (Some(_), None) => {
            return Err(Unreadable::Malformed(
                "`staged_completed` is set but `staged_total` is NULL".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(Unreadable::Malformed(
                "`staged_total` is set but `staged_completed` is NULL".into(),
            ));
        }
    };
    TimelineState::from_projected(version, tables, modules, staged)
}
