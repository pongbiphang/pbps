//! The ledger's reads, once for every engine.
//!
//! Each function takes the engine's [`LedgerSpec`] and sends that engine's
//! own statement; what is here is the order of the questions and what each
//! answer means. A read never creates or migrates the ledger.

use pbps_model::StateKind;

use super::{
    LedgerSpec, Recoverable, entry_from_row, number, optional_text, stored_entry_from_row, text,
};
use crate::ledger::{LedgerEntry, LedgerError, LockInfo, StateRevision, StoredEntry};
use crate::{Conn, DbError, Param, Row};

/// The rows of one statement, or `None` when the table it names is not there.
///
/// Under the engine's guard, because the tolerated answer is an error: where
/// an error inside a transaction takes the transaction with it, "not there"
/// must not leave the caller's transaction dead on its way to being reported.
/// Every other failure stays a failure — "I could not look" is never
/// flattened into "nothing there".
async fn rows_unless_missing(
    conn: &mut Conn,
    spec: &LedgerSpec,
    statement: &str,
) -> Result<Option<Vec<Row>>, DbError> {
    let guard = Recoverable::take(conn, spec).await?;
    match conn.query(statement).await {
        Ok(rows) => {
            guard.release(conn).await?;
            Ok(Some(rows))
        }
        Err(e) if spec.is_missing_table(&e) => {
            guard.rewind(conn).await?;
            Ok(None)
        }
        Err(e) => {
            guard.rewind_quietly(conn).await;
            Err(e)
        }
    }
}

/// Whether this database has a ledger at all.
///
/// Every read of an entry has to ask first: querying a table that does not
/// exist fails with the engine's "no such table", which tells the user nothing
/// about what to do. The answer they need is "run baseline or bootstrap", and
/// only this distinction can produce it. Each engine's own `is_initialized`
/// records why the question is a statement and not a catalog lookup there.
pub async fn is_initialized(conn: &mut Conn, spec: &LedgerSpec) -> Result<bool, DbError> {
    Ok(rows_unless_missing(conn, spec, &spec.probe_state)
        .await?
        .is_some())
}

async fn require_initialized(conn: &mut Conn, spec: &LedgerSpec) -> Result<(), LedgerError> {
    if is_initialized(conn, spec).await? {
        Ok(())
    } else {
        Err(LedgerError::NotInitialized)
    }
}

/// The newest state, which is the environment's current baseline.
///
/// `Ok(None)` means the ledger exists but is empty — a database that was
/// initialized and then pruned to nothing. `Err(NotInitialized)` means there is
/// no ledger at all. The two need different remedies, so they are different
/// answers.
pub async fn latest(
    conn: &mut Conn,
    spec: &LedgerSpec,
) -> Result<Option<LedgerEntry>, LedgerError> {
    require_initialized(conn, spec).await?;
    let rows = conn.query(&spec.select_latest).await?;
    rows.first().map(entry_from_row).transpose()
}

/// Commit evidence at or before the snapshot the caller already observed.
/// Reading projected metadata avoids decoding an obsolete historical snapshot.
///
/// The caller observed that snapshot through a read that already asked
/// whether there is a ledger, so this one does not ask again.
pub async fn deployed_revision(
    conn: &mut Conn,
    spec: &LedgerSpec,
    observed_id: i64,
) -> Result<Option<StateRevision>, LedgerError> {
    let allowed: Vec<StateKind> = StateKind::ALL
        .iter()
        .copied()
        .filter(|kind| kind.establishes_schema())
        .collect();
    let mut params: Vec<Param<'_>> = vec![observed_id.into()];
    params.extend(allowed.iter().map(|kind| Param::from(kind.as_str())));
    let rows = conn
        .query_with(&(spec.select_deployed_revision)(allowed.len()), &params)
        .await?;
    rows.first()
        .map(|row| {
            Ok(StateRevision {
                id: number(row, "id")?,
                git_sha: optional_text(row, "git_sha")?.map(pbps_model::Text::from),
            })
        })
        .transpose()
}

/// A retained successful apply is the evidence for an expand, not a baseline
/// or another report that happens to name the same checksum (DEC-2402.1).
pub async fn has_applied_plan(
    conn: &mut Conn,
    spec: &LedgerSpec,
    checksum: &str,
) -> Result<bool, LedgerError> {
    let rows = conn
        .query_with(
            &spec.select_applied_plan,
            &[StateKind::Apply.as_str().into(), checksum.into()],
        )
        .await?;
    Ok(!rows.is_empty())
}

/// A selected historical entry, or the newest when no id is supplied.
pub async fn entry(
    conn: &mut Conn,
    spec: &LedgerSpec,
    id: Option<i64>,
) -> Result<Option<StoredEntry>, LedgerError> {
    require_initialized(conn, spec).await?;
    let rows = match id {
        Some(id) => conn.query_with(&spec.select_by_id, &[id.into()]).await?,
        None => conn.query(&spec.select_latest).await?,
    };
    rows.first().map(stored_entry_from_row).transpose()
}

/// The most recent `limit` entries, newest first.
///
/// Every one of them has to be readable: a caller asking for entries wants the
/// states, and half a state is not one. The timeline is the other question.
pub async fn history(
    conn: &mut Conn,
    spec: &LedgerSpec,
    limit: u32,
) -> Result<Vec<LedgerEntry>, LedgerError> {
    require_initialized(conn, spec).await?;
    let rows = conn
        .query_with(&spec.select_history, &[spec.row_limit.param(limit)])
        .await?;
    rows.iter().map(entry_from_row).collect()
}

/// Who holds the deployment lock, if anyone.
///
/// A missing lock table answers `None` rather than failing: nothing can be
/// holding a lock that does not exist. That is deliberately *not* the same as
/// the table being unreadable, which stays an error — callers report the two
/// differently.
pub async fn lock_holder(conn: &mut Conn, spec: &LedgerSpec) -> Result<Option<LockInfo>, DbError> {
    let Some(rows) = rows_unless_missing(conn, spec, &spec.select_lock).await? else {
        return Ok(None);
    };
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let locked_by = text(row, "locked_by")?;
    Ok(Some(LockInfo {
        session_lookup: (spec.session_lookup)(&locked_by),
        locked_by,
        locked_at: text(row, "locked_at")?,
    }))
}
