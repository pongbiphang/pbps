//! Waiting for another writer, and saying so when the wait ran out.
//!
//! `pbps_db::sqlite::Connection::open` waits for nothing: whether and how long
//! to wait is the state layer's decision, and a second writer is refused at
//! once otherwise. That is wrong for the ledger. Measured with eight processes
//! creating it at once: with no wait seven of them failed `database is locked`,
//! with a two-second wait all eight succeeded. The connection has no getter for
//! its timeout, so every function here sets it for its own statements and puts
//! back what the pragma said before ([`waiting`]).

use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::Connection;

/// How long a ledger statement waits for another connection's write lock, and
/// for readers to leave before a commit in the journal mode where they block it.
pub const BUSY_WAIT: Duration = Duration::from_secs(5);

/// The primary result code of "busy": the extended codes 5, 261 (recovery), 517
/// (a stale snapshot) and 773 (timeout) all carry it in their low byte.
const BUSY: u32 = 5;

/// The extended code of a write refused because the transaction read before
/// another connection committed (`SQLITE_BUSY_SNAPSHOT`, WAL only). It comes at
/// once, with no wait, and nothing holds a lock when it does.
const STALE_SNAPSHOT: u32 = 517;

fn code(error: &DbError) -> Option<u32> {
    error.server_error_code()?.parse().ok()
}

/// Whether `error` is the engine saying another connection holds a lock.
#[must_use]
pub(crate) fn is_busy(error: &DbError) -> bool {
    code(error).is_some_and(|code| code & 0xff == BUSY)
}

/// Runs `work` with the wait set on `c`, and puts back the wait the caller had.
///
/// The wait is the connection's, not a statement's, so one left behind would
/// make every later statement on the handle wait where its caller chose not to.
/// A failure to put it back is reported unless `work` already failed, whose
/// error is the one that says what went wrong.
pub(crate) fn waiting<T, E: From<DbError>>(
    c: &Connection,
    wait: Duration,
    work: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let before = match c.query("PRAGMA busy_timeout", &[])?.first() {
        Some(row) => Duration::from_millis(u64::try_from(row.integer(0)?).unwrap_or(0)),
        None => {
            return Err(DbError::BadRow("`PRAGMA busy_timeout` returned no row".into()).into());
        }
    };
    c.set_busy_timeout(wait)?;
    let outcome = work();
    let restored = c.set_busy_timeout(before);
    match (outcome, restored) {
        (Ok(_), Err(error)) => Err(error.into()),
        (outcome, _) => outcome,
    }
}

/// `error`, with the sentence that tells a person what a busy refusal is **not**.
///
/// It does not say the wait ran out: the engine also refuses at once, with the
/// same code, where waiting would be a deadlock (a reader that asks to write
/// while a writer waits for readers to leave).
///
/// It is another connection's lock, possibly an application's own, so it is not
/// this tool's lock and `pbps unlock` does not clear it. The source stays in the
/// chain, so [`is_busy`] still answers for the result.
pub(crate) fn explained(error: DbError, wait: Duration) -> DbError {
    if !is_busy(&error) {
        return error;
    }
    // Not a wait that ran out: the lock may be long gone. Saying it was held
    // for the wait would send a person looking for a writer that is not there.
    if code(&error) == Some(STALE_SNAPSHOT) {
        return error.context(
            "the database was changed by another connection after this transaction first read \
             it, so SQLite refuses its write. Nothing is locked, and `pbps unlock` does not \
             apply: the transaction has to be started again",
        );
    }
    error.context(format!(
        "the database is busy: another connection holds one of SQLite's own locks, and a \
         statement of the ledger waits at most {} s for it. That is not this tool's lock, and \
         `pbps unlock` does not clear it",
        wait.as_secs_f64()
    ))
}
