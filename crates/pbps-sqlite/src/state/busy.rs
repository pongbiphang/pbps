//! Waiting for another writer, and saying so when the wait ran out.
//!
//! `pbps_db::sqlite::Connection::open` waits for nothing: whether and how long
//! to wait is the state layer's decision, and a second writer is refused at
//! once otherwise. That is wrong for the ledger. Measured with eight processes
//! creating it at once: with no wait seven of them failed `database is locked`,
//! with a two-second wait all eight succeeded. The connection has no getter for
//! its timeout, so every function here sets it before it starts.

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

/// Sets the wait on `c`.
pub(super) fn wait_up_to(c: &Connection, wait: Duration) -> Result<(), DbError> {
    c.set_busy_timeout(wait)
}

/// `error`, with the sentence that tells a person what a busy refusal is **not**.
///
/// It is another connection's lock, possibly an application's own, so it is not
/// this tool's lock and `pbps unlock` does not clear it. The source stays in the
/// chain, so [`is_busy`] still answers for the result.
pub(super) fn explained(error: DbError, wait: Duration) -> DbError {
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
        "the database is busy: another connection has held SQLite's own write lock for more \
         than {} s. That is not this tool's lock, and `pbps unlock` does not clear it",
        wait.as_secs_f64()
    ))
}
