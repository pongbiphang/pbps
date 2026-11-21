//! Reading the ledger.
//!
//! **A read is a read.** It never creates the ledger, never writes, and sets
//! nothing on the connection but the wait for another writer. Absent, empty and
//! unreadable are three answers and are never one:
//!
//! - no state table at all is [`LedgerError::NotInitialized`], whose remedy is
//!   `baseline` or `bootstrap`;
//! - a table with no entries is `Ok(None)` (or an empty list), a database that
//!   was initialized and pruned, or whose first record has not happened;
//! - an entry this build cannot read is [`LedgerError::BadEntry`] for the call
//!   that asked for that entry, and a row carrying its reason in a timeline.
//!
//! Something that is not an ordinary table in the state table's name is neither
//! absent nor present: it is refused, as [`is_initialized`] refuses it.
//!
//! Every function asks for the ledger first, so an absent one is
//! `NotInitialized` also where PostgreSQL would answer with the engine's "no such
//! table", which this engine cannot tell from any other error.

use pbps_db::DbError;
use pbps_db::ledger::{LedgerEntry, LedgerError, StateRevision, StoredEntry, TimelineEntry};
use pbps_db::sqlite::{Connection, Record, Value};
use pbps_model::{StateKind, Text};

use super::STATE_TABLE;
use super::busy::{BUSY_WAIT, explained, waiting};
use super::ddl::is_initialized;
use super::rows;

/// Runs one reader: asks whether there is a ledger to read, refuses to go on
/// when there is not, and holds the ledger's wait for all of its statements,
/// putting the caller's back afterwards.
fn reading<T>(
    c: &Connection,
    work: impl FnOnce() -> Result<T, LedgerError>,
) -> Result<T, LedgerError> {
    waiting(c, BUSY_WAIT, || {
        if is_initialized(c)? {
            work()
        } else {
            Err(LedgerError::NotInitialized)
        }
    })
}

/// Runs one read statement, naming a busy refusal for what it is.
fn run(c: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Record>, LedgerError> {
    Ok(c.query(sql, params).map_err(|e| explained(e, BUSY_WAIT))?)
}

/// The newest state, which is the environment's current baseline.
///
/// `Ok(None)` is a ledger with no entries: initialized, then pruned or never
/// recorded in. It is not the answer for a database with no ledger.
pub fn latest(c: &Connection) -> Result<Option<LedgerEntry>, LedgerError> {
    reading(c, || {
        let rows = run(
            c,
            &format!(
                "SELECT id, applied_at, state_json FROM {STATE_TABLE} ORDER BY id DESC LIMIT 1"
            ),
            &[],
        )?;
        rows.first().map(rows::entry).transpose()
    })
}

/// A selected historical entry, or the newest when `id` is `None`, before its
/// snapshot is decoded: the caller learns why it cannot be (a version, damage)
/// and not only that it cannot.
pub fn entry(c: &Connection, id: Option<i64>) -> Result<Option<StoredEntry>, LedgerError> {
    reading(c, || {
        let rows = match id {
            Some(id) => run(
                c,
                &format!("SELECT id, applied_at, state_json FROM {STATE_TABLE} WHERE id = ?1"),
                &[Value::Integer(id)],
            )?,
            None => run(
                c,
                &format!(
                    "SELECT id, applied_at, state_json FROM {STATE_TABLE} ORDER BY id DESC LIMIT 1"
                ),
                &[],
            )?,
        };
        Ok(rows.first().map(rows::stored).transpose()?)
    })
}

/// The commit evidence at or before the entry the caller already observed, read
/// from the projected columns so an obsolete snapshot is never decoded.
///
/// Only the kinds that establish a schema count: a failed attempt or a report of
/// data work names the checkout that reported it, not what was deployed
/// (DEC-2397.1, DEC-2402.1). The kinds are bound from `StateKind`, so a kind
/// added later is decided where it is defined.
pub fn deployed_revision(
    c: &Connection,
    observed_id: i64,
) -> Result<Option<StateRevision>, LedgerError> {
    reading(c, || {
        let kinds: Vec<Value> = StateKind::ALL
            .iter()
            .filter(|kind| kind.establishes_schema())
            .map(|kind| Value::Text(kind.as_str().to_owned()))
            .collect();
        let slots = (2..2 + kinds.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut params = vec![Value::Integer(observed_id)];
        params.extend(kinds);
        let rows = run(
            c,
            &format!(
                "SELECT id, git_sha FROM {STATE_TABLE} WHERE id <= ?1 AND kind IN ({slots}) \
             ORDER BY id DESC LIMIT 1"
            ),
            &params,
        )?;
        rows.first()
            .map(|row| {
                let wrong = |e: DbError| LedgerError::Db(e);
                Ok(StateRevision {
                    id: row.integer(0).map_err(wrong)?,
                    git_sha: row
                        .optional_text(1)
                        .map_err(wrong)?
                        .map(|sha| Text::from(sha.to_owned())),
                })
            })
            .transpose()
    })
}

/// Whether a plan with this checksum was applied and its entry is still kept.
///
/// A retained successful apply is the evidence for an expand, not a baseline or
/// another report that happens to name the same checksum (DEC-2402.1).
pub fn has_applied_plan(c: &Connection, checksum: &str) -> Result<bool, LedgerError> {
    reading(c, || {
        let rows = run(
            c,
            &format!(
                "SELECT id FROM {STATE_TABLE} WHERE kind = ?1 AND plan_checksum = ?2 \
             ORDER BY id DESC LIMIT 1"
            ),
            &[
                Value::Text(StateKind::Apply.as_str().to_owned()),
                Value::Text(checksum.to_owned()),
            ],
        )?;
        Ok(!rows.is_empty())
    })
}

/// The newest `limit` entries as a timeline, newest first.
///
/// It reads the projected columns and not the snapshot: a row this build cannot
/// read is still a row, and is carried with its reason so that one such row does
/// not erase the history above it. The snapshot is selected only for a row with
/// no projection, **in the same statement**, so a ledger whose rows all carry
/// one never transfers a snapshot at all. (PostgreSQL asks for them in a second
/// statement because a role may be denied the column and because a ledger from
/// before the projection exists; neither is true of this ledger, and one
/// statement leaves no window in which a concurrent prune could take a row
/// between two.)
pub fn timeline(c: &Connection, limit: u32) -> Result<Vec<TimelineEntry>, LedgerError> {
    reading(c, || {
        // `u32` into `i64` is never negative, which is what `LIMIT` would read as
        // "no limit".
        let rows = run(
            c,
            &format!(
                "SELECT {} FROM {STATE_TABLE} ORDER BY id DESC LIMIT ?1",
                rows::TIMELINE_COLUMNS
            ),
            &[Value::Integer(i64::from(limit))],
        )?;
        rows.iter()
            .map(|row| rows::timeline_entry(row).map_err(LedgerError::from))
            .collect()
    })
}
