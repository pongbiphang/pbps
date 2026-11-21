//! What an engine hands the shared flow: its statements, its codes and the
//! few values the flow branches on.
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

use super::RowLimit;
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
    /// The lock's one row: `locked_by`, `locked_at` as text.
    pub select_lock: Statement,
    /// How a row limit is bound.
    pub row_limit: RowLimit,
    /// The server's code for "the table this statement names is not there".
    pub missing_table: &'static str,
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

    /// Every statement this spec can send, for the engine's tests of "the SQL
    /// here". A statement built from a count is sampled at three.
    ///
    /// The destructuring names every field, so a statement added to the spec
    /// does not compile until it is listed here or marked as not a statement:
    /// a rule asserted over this list cannot quietly stop covering one.
    #[must_use]
    pub fn statements(&self) -> SpecStatements {
        let LedgerSpec {
            probe_state,
            select_latest,
            select_by_id,
            select_history,
            select_applied_plan,
            select_deployed_revision,
            select_lock,
            row_limit: _,
            missing_table: _,
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
            ],
            lock: vec![select_lock.to_string()],
        }
    }
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

    fn no_lookup(_: &str) -> Option<String> {
        None
    }

    fn spec() -> LedgerSpec {
        LedgerSpec {
            probe_state: "state probe".into(),
            select_latest: "state latest".into(),
            select_by_id: "state by id".into(),
            select_history: String::from("state history").into(),
            select_applied_plan: "state applied plan".into(),
            select_deployed_revision: kinds,
            select_lock: "lock holder".into(),
            row_limit: RowLimit::I64,
            missing_table: "42P01",
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
                "state kinds(3)"
            ]
        );
        assert_eq!(listed.lock, ["lock holder"]);
        assert!(listed.lock.iter().all(|sql| !sql.starts_with("state")));
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
}
