//! What an engine hands the shared flow: its statements, its codes, the few
//! values the flow branches on, and the steps that are the engine's own.
//!
//! # Why whole statements
//!
//! A statement here is the engine's finished text, not something the flow
//! assembles from a table name, a placeholder and a limit clause (DEC-2988.1).
//! The engines differ in shape, not in spelling — `TOP (n)` goes before the
//! column list and `LIMIT n` after the ordering, `ONLY` exists on one of them
//! — so fragments would put an engine's grammar in this crate, and a whole
//! text keeps each statement reviewable beside the measurement that explains
//! it, in the crate documented as owning that engine's SQL.

use std::borrow::Cow;

use super::{ClipUnit, RowLimit};
use crate::{Conn, DbError};

/// One statement's text. Borrowed where the engine writes it out, owned where
/// the engine builds it once from a shared piece (PostgreSQL renders every
/// timestamp through one pattern).
pub type Statement = Cow<'static, str>;

/// One engine's ledger, as the shared flow needs it.
///
/// Every statement names its table qualified and binds every value; each
/// engine's own tests assert that over [`LedgerSpec::statements`].
pub struct LedgerSpec {
    /// The dialect name of the engine that owns this ledger (`mssql`, `postgres`, …).
    pub dialect: &'static str,
    /// Resolves the ledger and checks the permission to read it, returning no
    /// row. A statement and not a catalog question: on both engines only the
    /// statement separates "absent" from "there and unreadable".
    pub probe_state: Statement,
    /// The newest row: `id`, `applied_at` as text, `state_json`.
    pub select_latest: Statement,
    /// The same three columns of the row whose `id` is the one bound value.
    pub select_by_id: Statement,
    /// The same three columns of the newest rows; the one bound value is the
    /// row limit, bound as [`LedgerSpec::row_limit`] says.
    pub select_history: Statement,
    /// The `id` of the newest row of a kind (first value) that carries a plan
    /// checksum (second value).
    pub select_applied_plan: Statement,
    /// `id` and `git_sha` of the newest row at or below an id (first value)
    /// whose kind is one of the next `kinds` values. Built from the count,
    /// never from a kind's text.
    pub select_deployed_revision: fn(kinds: usize) -> String,
    /// The newest rows as the timeline shows them, without `state_json`; the
    /// one bound value is the row limit.
    pub select_timeline: Statement,
    /// The same rows of a ledger whose timeline columns are not there yet.
    pub select_timeline_unmigrated: Statement,
    /// `id` and `state_json` of the rows whose ids are the `ids` bound values.
    /// Built from the count, never from an id.
    pub select_legacy_state_json: fn(ids: usize) -> String,
    /// The insert of a row with staged progress (eleven values), returning
    /// its `id`.
    pub insert_state_staged: Statement,
    /// The insert of a row without it (nine values), returning its `id`.
    pub insert_state_not_staged: Statement,
    /// Every `id`, newest first.
    pub select_ids: Statement,
    /// Deletes the rows at or below the one bound id.
    pub delete_up_to: Statement,
    /// The lock's one row: `locked_by`, `locked_at` as text.
    pub select_lock: Statement,
    /// Takes the lock for the holder that is the one bound value.
    pub insert_lock: Statement,
    /// Releases it.
    pub delete_lock: Statement,
    /// The application name the server holds for this session, as `name`.
    pub session_application_name: Statement,
    /// How a row limit is bound.
    pub row_limit: RowLimit,
    /// The most values one statement may bind: the legacy `state_json` query
    /// is sent in pieces of at most this many ids.
    pub max_parameters: usize,
    /// The unit and the width the `operator` column is counted in.
    pub operator_clip: (ClipUnit, usize),
    /// What [`LedgerSpec::insert_lock`] does when the lock is already held.
    pub held_lock: HeldLock,
    /// The server's code for "the table this statement names is not there".
    pub missing_table: &'static str,
    /// The server's code for "this session may not read that", as the legacy
    /// `state_json` query can meet it.
    pub select_denied: &'static str,
    /// The savepoint the engine needs around a statement whose failure the
    /// flow handles; `None` where a failed statement leaves the transaction
    /// usable.
    pub savepoint: Option<Savepoint>,
    /// How an operator finds a lock holder's sessions, from what `locked_by`
    /// recorded.
    pub session_lookup: fn(locked_by: &str) -> Option<String>,
}

/// The texts of a [`LedgerSpec`], by the table each one must name.
pub struct SpecStatements {
    pub state: Vec<String>,
    pub lock: Vec<String>,
}

impl LedgerSpec {
    /// Whether a failure means "that table does not exist".
    #[must_use]
    pub fn is_missing_table(&self, e: &DbError) -> bool {
        e.server_error_code().as_deref() == Some(self.missing_table)
    }

    /// Whether a failure means "this session may not read that".
    #[must_use]
    pub fn is_select_denied(&self, e: &DbError) -> bool {
        e.server_error_code().as_deref() == Some(self.select_denied)
    }

    /// Every statement this spec can send, for the engine's tests of "the SQL
    /// here". A statement built from a count is sampled at three.
    ///
    /// The destructuring names every field, so a statement added to the spec
    /// does not compile until it is listed here or marked as not a statement:
    /// a rule asserted over this list cannot quietly stop covering one.
    #[must_use]
    pub fn statements(&self) -> SpecStatements {
        let LedgerSpec {
            dialect: _,
            probe_state,
            select_latest,
            select_by_id,
            select_history,
            select_applied_plan,
            select_deployed_revision,
            select_timeline,
            select_timeline_unmigrated,
            select_legacy_state_json,
            insert_state_staged,
            insert_state_not_staged,
            select_ids,
            delete_up_to,
            select_lock,
            insert_lock,
            delete_lock,
            // It names no table.
            session_application_name: _,
            row_limit: _,
            max_parameters: _,
            operator_clip: _,
            held_lock: _,
            missing_table: _,
            select_denied: _,
            // Its statements name no table.
            savepoint: _,
            session_lookup: _,
        } = self;
        SpecStatements {
            state: vec![
                probe_state.to_string(),
                select_latest.to_string(),
                select_by_id.to_string(),
                select_history.to_string(),
                select_applied_plan.to_string(),
                select_deployed_revision(3),
                select_timeline.to_string(),
                select_timeline_unmigrated.to_string(),
                select_legacy_state_json(3),
                insert_state_staged.to_string(),
                insert_state_not_staged.to_string(),
                select_ids.to_string(),
                delete_up_to.to_string(),
            ],
            lock: vec![
                select_lock.to_string(),
                insert_lock.to_string(),
                delete_lock.to_string(),
            ],
        }
    }
}

/// What the lock's insert does when another holder is already there.
///
/// The engines differ in the statement they can write, and so in where the
/// answer arrives; which one applies is a fact about the statement, so it
/// travels with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldLock {
    /// The insert succeeds and changes no row. Any failure of it is a real
    /// failure and is returned as it is.
    ChangesNoRow,
    /// The insert fails. A failure with a holder to name is "locked"; one
    /// with no holder is returned as it is.
    FailsTheInsert,
}

/// The steps the flow calls and does not contain (DEC-2988.1).
///
/// Each is the engine's own work, found by measurement on that engine: how
/// the ledger is created and migrated, what makes a ledger unfit to write
/// through, and how the shape of the state table is asked.
pub trait LedgerSteps {
    /// Creates the ledger's tables when they are absent and brings an older
    /// ledger to the current shape. Called before every write that may be the
    /// first.
    fn ensure_tables(conn: &mut Conn) -> impl Future<Output = Result<(), DbError>> + Send;
    /// Refuses a ledger this engine must not delete through. Called before
    /// the deletes of `prune` and `unlock`; an absent ledger is not refused.
    fn refuse_untrusted(conn: &mut Conn) -> impl Future<Output = Result<(), DbError>> + Send;
    /// Whether the state table has the timeline's columns. Asked by a read,
    /// so it must need no more than a read does.
    fn timeline_columns_present(
        conn: &mut Conn,
    ) -> impl Future<Output = Result<bool, DbError>> + Send;
}

/// The statements of a savepoint, and the code that says there was no
/// transaction to take one in.
#[derive(Debug, Clone, Copy)]
pub struct Savepoint {
    pub take: &'static str,
    /// Ends the savepoint and keeps what ran under it.
    pub release: &'static str,
    /// Undoes what ran under it. The marker stays until `release`.
    pub rewind: &'static str,
    /// The server's code for `take` outside a transaction.
    pub outside_a_transaction: &'static str,
}

/// A savepoint held around one statement whose failure this code expects to
/// handle.
///
/// **On an engine that supplies a [`Savepoint`], a failed statement aborts the
/// whole transaction**, so every `Err(e) if ... => Ok(...)` arm in the flow is
/// a claim that the connection is still usable — and without something to
/// rewind to, that claim is false inside a transaction: on PostgreSQL the
/// caller's next statement is `25P02` and its `COMMIT` becomes a `ROLLBACK`.
/// The ledger is read and written inside the apply's own transaction
/// (DECISIONS 147), so the callers are real ones.
///
/// Taken by **trying** it: measured on PostgreSQL, `SAVEPOINT` outside a
/// transaction block is `25P01` and harms nothing, so one round trip both
/// establishes the savepoint and answers whether this call is in a transaction
/// at all — where a separate probe would cost the same round trip and tell it
/// less. Outside a transaction there is nothing to protect and nothing is
/// taken.
///
/// An engine with no [`Savepoint`] holds nothing and sends nothing: there a
/// failed statement leaves the transaction as it was.
pub struct Recoverable {
    held: Option<Savepoint>,
}

impl Recoverable {
    pub async fn take(conn: &mut Conn, spec: &LedgerSpec) -> Result<Self, DbError> {
        let Some(savepoint) = spec.savepoint else {
            return Ok(Recoverable { held: None });
        };
        match conn.execute(savepoint.take).await {
            Ok(()) => Ok(Recoverable {
                held: Some(savepoint),
            }),
            Err(e) if e.server_error_code().as_deref() == Some(savepoint.outside_a_transaction) => {
                Ok(Recoverable { held: None })
            }
            Err(e) => Err(e),
        }
    }

    /// The statement did what was wanted: end the savepoint and leave the
    /// caller's transaction exactly as deep as it was.
    pub async fn release(self, conn: &mut Conn) -> Result<(), DbError> {
        if let Some(savepoint) = self.held {
            conn.execute(savepoint.release).await?;
        }
        Ok(())
    }

    /// The statement failed and this code is handling that failure: put the
    /// transaction back where it was, so that "handled" means what it says.
    pub async fn rewind(self, conn: &mut Conn) -> Result<(), DbError> {
        if let Some(savepoint) = self.held {
            conn.execute(savepoint.rewind).await?;
        }
        // The rewind keeps the marker; release it to restore the caller's
        // original depth and expose any older savepoint of the same name.
        self.release(conn).await
    }

    /// The same, for a failure that is **not** being handled: the caller is
    /// about to be given the error, and this only leaves it able to run the
    /// diagnostics it wants to print. Its own failure is dropped, so it cannot
    /// replace the error that caused it.
    pub async fn rewind_quietly(self, conn: &mut Conn) {
        let _ = self.rewind(conn).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(count: usize) -> String {
        format!("state kinds({count})")
    }

    fn legacy(ids: usize) -> String {
        format!("state legacy({ids})")
    }

    fn no_lookup(_: &str) -> Option<String> {
        None
    }

    fn spec() -> LedgerSpec {
        LedgerSpec {
            dialect: "postgres",
            probe_state: "state probe".into(),
            select_latest: "state latest".into(),
            select_by_id: "state by id".into(),
            select_history: String::from("state history").into(),
            select_applied_plan: "state applied plan".into(),
            select_deployed_revision: kinds,
            select_timeline: "state timeline".into(),
            select_timeline_unmigrated: "state timeline unmigrated".into(),
            select_legacy_state_json: legacy,
            insert_state_staged: "state insert staged".into(),
            insert_state_not_staged: "state insert".into(),
            select_ids: "state ids".into(),
            delete_up_to: "state delete".into(),
            select_lock: "lock holder".into(),
            insert_lock: "lock take".into(),
            delete_lock: "lock release".into(),
            session_application_name: "session name".into(),
            row_limit: RowLimit::I64,
            max_parameters: 2,
            operator_clip: (ClipUnit::Characters, 8),
            held_lock: HeldLock::ChangesNoRow,
            missing_table: "42P01",
            select_denied: "42501",
            savepoint: None,
            session_lookup: no_lookup,
        }
    }

    /// Every statement is listed under the table it reads, the built one at a
    /// count that shows its slots, and no state statement is filed under the
    /// lock.
    #[test]
    fn every_statement_is_listed_under_its_table() {
        let listed = spec().statements();
        assert_eq!(
            listed.state,
            [
                "state probe",
                "state latest",
                "state by id",
                "state history",
                "state applied plan",
                "state kinds(3)",
                "state timeline",
                "state timeline unmigrated",
                "state legacy(3)",
                "state insert staged",
                "state insert",
                "state ids",
                "state delete"
            ]
        );
        assert_eq!(listed.lock, ["lock holder", "lock take", "lock release"]);
        assert!(listed.lock.iter().all(|sql| !sql.starts_with("state")));
        let all = [listed.state, listed.lock].concat();
        assert!(
            !all.contains(&"session name".to_owned()),
            "the session's name is read from no table"
        );
    }

    /// Only the spec's own code means "the table is not there". A different
    /// code, and a failure that carries none, stay failures: "I could not
    /// look" must never read as "nothing there".
    #[test]
    fn only_the_specs_code_means_a_missing_table() {
        let spec = spec();
        let server = |code: Option<&str>| DbError::Driver {
            diagnostic: None,
            message: "server sentence".into(),
            code: code.map(str::to_owned),
        };
        assert!(spec.is_missing_table(&server(Some("42P01"))));
        assert!(spec.is_missing_table(&server(Some("42P01")).context("guidance")));
        assert!(!spec.is_missing_table(&server(Some("42501"))));
        assert!(!spec.is_missing_table(&server(None)));
        assert!(!spec.is_missing_table(&DbError::BadRow("no code at all".into())));
    }

    /// A denial is its own code: a missing table is not one, and neither is a
    /// failure with no code.
    #[test]
    fn only_the_specs_code_means_a_denied_read() {
        let spec = spec();
        let server = |code: Option<&str>| DbError::Driver {
            diagnostic: None,
            message: "server sentence".into(),
            code: code.map(str::to_owned),
        };
        assert!(spec.is_select_denied(&server(Some("42501"))));
        assert!(!spec.is_select_denied(&server(Some("42P01"))));
        assert!(!spec.is_select_denied(&server(None)));
    }
}
