//! The ledger's writes and its lock, once for every engine.
//!
//! As with the reads, each function sends the engine's own statement from its
//! [`LedgerSpec`]. The steps that are the engine's own work — creating and
//! migrating the tables, refusing a ledger it must not write through — are
//! called through [`LedgerSteps`] at the point the order requires them.

use pbps_model::StateSnapshot;

use super::read::require_initialized;
use super::spec::HeldLock;
use super::{
    LedgerSpec, LedgerSteps, Recoverable, bounded_operator, lock_holder, number, ordinary_params,
    text,
};
use crate::ledger::{LedgerError, LockInfo, ids_to_prune};
use crate::{Conn, DbError};

/// Appends one state to the ledger and returns its id.
///
/// The columns beside `state_json` are projected from the snapshot rather than
/// passed separately: they exist so `status` can filter without parsing JSON,
/// and a caller able to set them independently could make them lie. The five
/// issue #103 added follow the same rule (DECISIONS 435).
pub async fn record<S: LedgerSteps>(
    conn: &mut Conn,
    spec: &LedgerSpec,
    snapshot: &StateSnapshot,
) -> Result<i64, LedgerError> {
    snapshot
        .validate_data_work()
        .map_err(|e| LedgerError::BadEntry {
            id: 0,
            message: e.to_string(),
        })?;
    S::ensure_tables(conn).await?;
    let (unit, width) = spec.operator_clip;
    let snapshot = &*bounded_operator(snapshot, unit, width);
    let state_json = serde_json::to_string(snapshot).map_err(|e| LedgerError::BadEntry {
        id: 0,
        message: format!("the snapshot could not be serialized: {e}"),
    })?;

    // Two statements and not one: see [`ordinary_params`].
    let (params, staged) = ordinary_params(snapshot, &state_json);
    let sql = if staged {
        &spec.insert_state_staged
    } else {
        &spec.insert_state_not_staged
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
pub async fn prune<S: LedgerSteps>(
    conn: &mut Conn,
    spec: &LedgerSpec,
    keep: u32,
) -> Result<u64, LedgerError> {
    require_initialized(conn, spec).await?;
    S::refuse_untrusted(conn).await?;
    let ids: Vec<i64> = conn
        .query(&spec.select_ids)
        .await?
        .iter()
        .map(|row| number(row, "id"))
        .collect::<Result<_, _>>()?;

    // The policy is decided in `ids_to_prune` and tested without a server;
    // all that is left here is the delete. Because ids come from an identity
    // column and the list is newest-first, the prunable set is always a
    // contiguous tail, so one `<=` covers it exactly.
    let doomed = ids_to_prune(&ids, keep);
    let Some(highest) = doomed.first().copied() else {
        return Ok(0);
    };
    Ok(conn
        .execute_with(&spec.delete_up_to, &[highest.into()])
        .await?)
}

/// Takes the apply lock, or reports who already holds it.
///
/// The insert is the gate, not a read before it: a check-then-insert would
/// let two pipelines through the check together. The holder is read only
/// after the insert has said the lock is taken, and then only to name it.
pub async fn lock<S: LedgerSteps>(
    conn: &mut Conn,
    spec: &LedgerSpec,
    holder: &str,
) -> Result<(), LedgerError> {
    S::ensure_tables(conn).await?;
    let inserted = conn.execute_with(&spec.insert_lock, &[holder.into()]).await;
    match (spec.held_lock, inserted) {
        (HeldLock::ChangesNoRow, Ok(0)) => match lock_holder(conn, spec).await? {
            Some(info) => Err(LedgerError::Locked(info)),
            // The row was there when the insert ran and is gone now: whoever
            // held the lock released it in between. Reporting a holder this
            // call cannot name would be a sentence with a hole in it, and
            // claiming the lock would be a claim this call did not win, so it
            // is neither — the caller is told the lock was contended and can
            // try again.
            None => Err(LedgerError::Locked(LockInfo {
                locked_by:
                    "another operation, which released the lock while this one was reading it"
                        .to_owned(),
                locked_at: "just now".to_owned(),
                session_lookup: None,
            })),
        },
        (HeldLock::FailsTheInsert, Err(e)) => match lock_holder(conn, spec).await? {
            Some(info) => Err(LedgerError::Locked(info)),
            None => Err(LedgerError::Db(e)),
        },
        // A count of zero is not read where a held lock is a failure: there
        // the insert of one row either fails or took the lock, and a session
        // that reports no counts would otherwise be told it lost.
        (_, Ok(_)) => Ok(()),
        (HeldLock::ChangesNoRow, Err(e)) => Err(LedgerError::Db(e)),
    }
}

/// Releases the lock. `false` means it was not held.
pub async fn unlock<S: LedgerSteps>(conn: &mut Conn, spec: &LedgerSpec) -> Result<bool, DbError> {
    // The *lock* table, not the state table. Guarding on `is_initialized`
    // would mean a database whose state table had been dropped by hand
    // reported "not held" and left a live lock in place — with no command
    // able to clear it.
    //
    // The engine's refusal first (#396, #894): the DELETE writes through
    // whatever answers to the lock table's name, just as the lock's INSERT
    // would. An absent lock table has no problems, so it still answers "not
    // held" below.
    S::refuse_untrusted(conn).await?;
    let guard = Recoverable::take(conn, spec).await?;
    match conn.execute_with(&spec.delete_lock, &[]).await {
        Ok(n) => {
            guard.release(conn).await?;
            Ok(n > 0)
        }
        // The tolerated answer is an error, so it needs the rewind the reads
        // have: a `pbps unlock` against a database with no lock table must
        // not leave the caller's transaction dead on its way to saying "not
        // held".
        Err(e) if spec.is_missing_table(&e) => {
            guard.rewind(conn).await?;
            Ok(false)
        }
        Err(e) => {
            guard.rewind_quietly(conn).await;
            Err(e)
        }
    }
}

/// The application name this session carries, as the server holds it: the
/// lock must record the name a search of the server's session list will
/// match, and a server may hold a shorter one than the client sent (#1188).
pub async fn session_application_name(
    conn: &mut Conn,
    spec: &LedgerSpec,
) -> Result<String, DbError> {
    let rows = conn.query(&spec.session_application_name).await?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::BadRow("the application-name query returned no row".into()))?;
    text(row, "name")
}
