//! The ledger's reads, once for every engine.
//!
//! Each function takes the engine's [`LedgerSpec`] and sends that engine's
//! own statement; what is here is the order of the questions and what each
//! answer means. A read never creates or migrates the ledger.

use std::collections::HashMap;

use pbps_model::{StateKind, StateSnapshot, Unreadable};

use super::{
    LedgerSpec, LedgerSteps, ProjectedRow, Recoverable, entry_from_row, number, optional_text,
    projected_row, projected_row_unmigrated, stored_entry_from_row, text,
};
use crate::ledger::{
    LedgerEntry, LedgerError, LockInfo, StateRevision, StoredEntry, TimelineEntry, TimelineState,
};
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

pub(super) async fn require_initialized(
    conn: &mut Conn,
    spec: &LedgerSpec,
) -> Result<(), LedgerError> {
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

/// The most recent `limit` rows as a timeline, newest first.
///
/// A row whose recorded state this build cannot read is carried with its
/// reason rather than failing the call: an environment upgraded across a
/// state-format change keeps rows older than `OLDEST_READABLE_VERSION`, and
/// one of them must not erase the history above it (DECISIONS 218).
///
/// The timeline statement never asks for `state_json` (DECISIONS 435): a
/// second query, [`LedgerSpec::select_legacy_state_json`], asks for it only
/// for the rows that predate the migration — `state_version IS NULL` — and
/// only for those ids. In the common case, once a ledger's rows are all
/// migrated, that second query is never sent, which is the property issue
/// #103's sharp test pins: a fully-migrated ledger whose `state_json` this
/// session may not see still answers `state list`.
pub async fn timeline<S: LedgerSteps>(
    conn: &mut Conn,
    spec: &LedgerSpec,
    limit: u32,
) -> Result<Vec<TimelineEntry>, LedgerError> {
    require_initialized(conn, spec).await?;
    // `state list` is a read and must not need the rights a migration needs,
    // so it cannot call `ensure_tables` to make an unmigrated ledger's
    // columns appear — it asks the shape instead and sends the query that
    // matches (round-1 review finding on #103's own PR: this is the only
    // reader of the state table's shape, and both branches meet at
    // `legacy_ids` below whichever one ran).
    let limit = [spec.row_limit.param(limit)];
    let projected: Vec<ProjectedRow> = if S::timeline_columns_present(conn).await? {
        let rows = conn.query_with(&spec.select_timeline, &limit).await?;
        rows.iter().map(projected_row).collect::<Result<_, _>>()?
    } else {
        let rows = conn
            .query_with(&spec.select_timeline_unmigrated, &limit)
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
    let mut legacy: HashMap<i64, Result<TimelineState, Unreadable>> = HashMap::new();
    // In pieces of at most `max_parameters` ids, not one statement for every
    // legacy id: this query binds one parameter per id, and until a deployer
    // has run a deployment after upgrading, every row is legacy, so a
    // long-lived ledger's ordinary `state list` is exactly the case that
    // could reach the server's parameter ceiling in a single request (a
    // review finding on #103's own PR — the pre-#103 query needed only its
    // limit parameter and had no such ceiling).
    for chunk in legacy_ids.chunks(spec.max_parameters.max(1)) {
        let sql = (spec.select_legacy_state_json)(chunk.len());
        let params: Vec<Param<'_>> = chunk.iter().map(|&id| id.into()).collect();
        // Under the engine's guard, because the denial below is *handled*:
        // where a failed statement aborts a caller's open transaction,
        // returning `Ok` over an aborted transaction turns its next statement
        // into a failure and its `COMMIT` into a silent rollback (#361). See
        // [`Recoverable`].
        let guard = Recoverable::take(conn, spec).await?;
        let answer = conn.query_with(&sql, &params).await;
        match &answer {
            Ok(_) => guard.release(conn).await?,
            Err(e) if spec.is_select_denied(e) => guard.rewind(conn).await?,
            Err(_) => guard.rewind_quietly(conn).await,
        }
        match answer {
            Ok(rows) => {
                for row in &rows {
                    let id: i64 = number(row, "id")?;
                    let state_json = text(row, "state_json")?;
                    // `read_json`, the same reader the pre-migration timeline
                    // used: version before shape, and the failure carried
                    // rather than returned (DECISIONS 222).
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
            // a denial is not sensitive to which row is read; a later chunk
            // still asks for itself.
            Err(e) if spec.is_select_denied(&e) => {
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
            git_sha: r.git_sha.map(pbps_model::Text::from),
            plan_checksum: r.plan_checksum,
            operator: r.operator,
            reason: r.reason.map(pbps_model::Text::from),
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
