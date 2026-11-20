//! The apply lock: a row, and an operating-system lock that proves its holder is
//! alive (item 5 of #2950, DEC-2950.13).
//!
//! # Why both
//!
//! A row is what a dead process leaves behind, which is the point of the lock on
//! the other engines (DECISIONS 285): a run killed in the middle must not be
//! followed silently by the next. On this engine a transaction cut short is rolled
//! back by SQLite itself (measured: nothing of it survives, the file opens at once
//! and is consistent), so a dead holder's row guards nothing and only costs a
//! `pbps unlock`. The operating system can say whether the holder is alive: an
//! exclusive lock on a file is released when its process ends, however it ends
//! (measured by `a_killed_holders_lock_is_released_and_its_row_is_cleared`).
//!
//! So `lock` writes the row **and** takes that lock on a sidecar file beside the
//! database, `<database>-pbps-lock`, and keeps it in the [`LockGuard`] it returns.
//! A later `lock` that finds the row tries the file lock without blocking: it
//! cannot, so the holder is alive and the run is refused naming it; it can, so the
//! holder is gone, its row is cleared and replaced by this run's, and the result
//! says whose row it was.
//!
//! # Order, and why two runs cannot both win
//!
//! The file lock is taken **first**, and the row is written second, in one write
//! unit: `DELETE` of a stale row (`RETURNING` what it was), then the gated
//! `INSERT`. At most one run holds the file lock, and a run that does not has been
//! refused before it reaches the row, so two runs that find the same stale row
//! cannot both clear it and both win. The delete is the unit's first statement, a
//! write, so the unit takes the write lock at once and cannot fail on a stale read.
//! A run that takes the file lock and then fails or loses releases it.
//!
//! The row is deleted **before** the file lock is released, not after: released
//! first, another run could take the file, replace the row, and have it deleted by
//! the run that is leaving. The row carries a random token so even then a run
//! deletes only its own.
//!
//! # When there is no proof
//!
//! The row alone is used, and a found row is only ever cleared by `pbps unlock`
//! (never two holders), where the sidecar cannot be opened or created, where the
//! system reports locks unsupported or fails in another way, and for a database
//! with no file (in memory, or a private temporary one), which no other process
//! can reach. The row records which it was (`proof`), so a run that **can** take
//! the file lock never clears a row whose holder could not: it had no proof, and
//! the file lock this run holds says nothing about it. The result names the mode,
//! so a message can say why a stale row was not cleared.
//!
//! # The sidecar
//!
//! It is never deleted. Deleting a locked file and making it again leaves two
//! files and two holders on Unix, and no rule that deletes it only when it is free
//! can be made safe against a run opening it at that moment. It is empty and
//! stays beside the database. Its name is resolved from the connection's own file
//! name (`PRAGMA database_list`), which SQLite has made absolute with symbolic
//! links followed (measured), so two spellings of one file meet on one sidecar. Two
//! hard links to one file are two paths to SQLite as well, and do not.
//!
//! A network file system cannot be detected portably and is not tried: where the
//! system reports the lock taken, it is believed.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use pbps_db::DbError;
use pbps_db::ledger::{LOCK_TABLE_NAME, LOCKED_BY_CHARS, LedgerError, LockInfo};
use pbps_db::sqlite::{Connection, Value};
use pbps_model::escape::Escaped;

use super::LOCK_TABLE;
use super::busy::{BUSY_WAIT, explained, waiting};
use super::ddl::{ensure_tables_within, occupants, refuse_a_stranger};
use super::write::fits;

/// What is appended to the database's path to name the sidecar. It cannot meet
/// SQLite's own `-journal`, `-wal` or `-shm`.
pub const LOCK_FILE_SUFFIX: &str = "-pbps-lock";

/// Why a lock has no liveness proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOnly {
    /// The database has no file: it is in memory or private, and no other process
    /// can open it.
    NoFile,
    /// The sidecar cannot be used, in the system's or this tool's words.
    Unavailable(String),
}

impl RowOnly {
    /// The reason, as a clause.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::NoFile => "the database has no file for another process to open".to_owned(),
            Self::Unavailable(why) => why.clone(),
        }
    }
}

/// How a lock is backed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proof {
    /// The operating system's lock on the sidecar is held by this run.
    File,
    /// The row alone.
    RowOnly(RowOnly),
}

/// A lock this run holds. It owns the operating-system lock, which is released
/// when it is [released](release) or dropped, and when the process ends however
/// it ends. Dropping it without releasing it leaves the row, which the next run
/// clears as stale when it can prove the holder gone.
///
/// The wiring keeps it for the command: the connection of a command that took the
/// lock holds it until `unlock`, so the lock lives exactly as long as the command
/// does. There is no global state.
#[derive(Debug)]
pub struct LockGuard {
    file: Option<File>,
    token: String,
    proof: Proof,
}

impl LockGuard {
    /// How this lock is backed.
    #[must_use]
    pub fn proof(&self) -> &Proof {
        &self.proof
    }
}

/// A lock taken.
#[derive(Debug)]
pub struct Taken {
    /// Held by the caller until [`release`].
    pub guard: LockGuard,
    /// The row this run cleared as a dead holder's, if it did, so the caller can
    /// print one line saying whose.
    pub cleared: Option<LockInfo>,
}

/// The name of the sidecar of the database file `database`.
#[must_use]
pub fn sidecar_path(database: &Path) -> PathBuf {
    let mut name = OsString::from(database);
    name.push(LOCK_FILE_SUFFIX);
    PathBuf::from(name)
}

/// What trying the operating-system lock came to.
enum Attempt {
    /// This run holds it.
    Held(File),
    /// Another holds it: the holder is alive.
    Alive,
    /// It cannot be used.
    Without(RowOnly),
}

/// The file `main` was opened from, as SQLite names it, or `None` for a database
/// with none.
fn database_file(c: &Connection) -> Result<Option<PathBuf>, DbError> {
    let rows = c.query(
        "SELECT file FROM pragma_database_list WHERE name = 'main'",
        &[],
    )?;
    match rows.first() {
        None => Err(DbError::BadRow(
            "`pragma_database_list` does not list `main`".into(),
        )),
        Some(row) => {
            let file = row.text(0)?;
            Ok((!file.is_empty()).then(|| PathBuf::from(file)))
        }
    }
}

fn attempt(c: &Connection) -> Result<Attempt, DbError> {
    let Some(database) = database_file(c)? else {
        return Ok(Attempt::Without(RowOnly::NoFile));
    };
    let path = sidecar_path(&database);
    let shown = path.display().to_string();
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            return Ok(Attempt::Without(RowOnly::Unavailable(format!(
                "the lock file `{}` cannot be opened: {error}",
                Escaped(&shown)
            ))));
        }
    };
    match file.try_lock() {
        Ok(()) => Ok(Attempt::Held(file)),
        Err(TryLockError::WouldBlock) => Ok(Attempt::Alive),
        // Unsupported, or failed in some other way: not "held", and not to be
        // trusted as "free" either.
        Err(TryLockError::Error(error)) => Ok(Attempt::Without(RowOnly::Unavailable(format!(
            "the system does not lock `{}`: {error}",
            Escaped(&shown)
        )))),
    }
}

/// What the lock table's row says.
struct Row {
    locked_by: String,
    locked_at: String,
    proof: String,
}

/// The `proof` column's words.
const PROOF_FILE: &str = "file";
const PROOF_ROW: &str = "row";

/// The row, if there is one. An absent lock table is no row; a name held by
/// anything but a table is refused, as everywhere; an unreadable table is an
/// error and never "nothing there".
fn read_row(c: &Connection) -> Result<Option<Row>, DbError> {
    let held = occupants(c, &[LOCK_TABLE_NAME])?;
    refuse_a_stranger(&held)?;
    if held.is_empty() {
        return Ok(None);
    }
    let rows = c.query(
        &format!("SELECT locked_by, locked_at, proof FROM {LOCK_TABLE} WHERE id = 1"),
        &[],
    )?;
    rows.first()
        .map(|row| {
            Ok(Row {
                locked_by: row.text(0)?.to_owned(),
                locked_at: row.text(1)?.to_owned(),
                proof: row.text(2)?.to_owned(),
            })
        })
        .transpose()
}

/// What a refusal tells a person about the holder, beyond the row.
///
/// SQLite keeps no list of sessions, so there is nothing to search; what can be
/// said is whether this tool could tell the holder is alive, and what to do when
/// it could not.
fn lookup(row_proof: &str, ours: Option<&Proof>) -> String {
    let confirm = "Confirm on the host named above that the process has exited before \
                   `pbps unlock --db ...`; SQLite rolls back a transaction a dead process left, \
                   so what it was doing is not left half done";
    match (row_proof, ours) {
        // Whatever this run could prove, this holder never could.
        (PROOF_ROW, _) => format!(
            "This lock was taken without a liveness check, so only `pbps unlock` clears it. \
             {confirm}."
        ),
        (_, Some(Proof::File)) => "That holder is alive: it holds the operating-system lock on \
             the lock file beside the database. Wait for it to finish. A holder that is gone is \
             cleared by the next run itself."
            .to_owned(),
        (_, Some(Proof::RowOnly(why))) => format!(
            "This run cannot tell whether the holder is alive, because {}. {confirm}.",
            why.reason()
        ),
        (_, None) => format!(
            "If the holder is gone, the next run clears this row itself once it can take the \
             lock file; otherwise {confirm}."
        ),
    }
}

fn info(row: Row, ours: Option<&Proof>) -> LockInfo {
    let session_lookup = Some(lookup(&row.proof, ours));
    LockInfo {
        locked_by: row.locked_by,
        locked_at: row.locked_at,
        session_lookup,
    }
}

/// The refusal for a lock held by someone, named from the row.
///
/// A row gone between the attempt and this read is a holder that released in
/// between, and is reported as contention without a name, as on the other engines:
/// a sentence with a hole in it would claim a holder this call cannot name, and
/// claiming the lock would claim one this call did not win.
fn locked(c: &Connection, ours: Option<&Proof>) -> LedgerError {
    match read_row(c) {
        Ok(Some(row)) => LedgerError::Locked(info(row, ours)),
        Ok(None) => LedgerError::Locked(LockInfo {
            locked_by: "another operation, which is taking or releasing the lock".to_owned(),
            locked_at: "just now".to_owned(),
            session_lookup: None,
        }),
        Err(error) => LedgerError::Db(error),
    }
}

/// What the write unit came to.
enum Written {
    Won {
        token: String,
        cleared: Option<(String, String)>,
    },
    /// A row is there that this run may not clear.
    Lost,
}

/// Clears a stale row (only when this run holds the proof, and only one that had
/// it) and inserts this run's, in one unit.
fn write_row(
    c: &Connection,
    holder: &str,
    proof: &Proof,
    wait: std::time::Duration,
) -> Result<Written, DbError> {
    let (word, clears) = match proof {
        Proof::File => (PROOF_FILE, true),
        Proof::RowOnly(_) => (PROOF_ROW, false),
    };
    super::ddl::in_a_unit(c, || {
        let cleared = if clears {
            c.query(
                &format!(
                    "DELETE FROM {LOCK_TABLE} WHERE id = 1 AND proof = '{PROOF_FILE}' \
                     RETURNING locked_by, locked_at"
                ),
                &[],
            )?
            .first()
            .map(|row| Ok::<_, DbError>((row.text(0)?.to_owned(), row.text(1)?.to_owned())))
            .transpose()?
        } else {
            None
        };
        let rows = c.query(
            &format!(
                "INSERT INTO {LOCK_TABLE} (id, locked_by, proof, token) \
                 VALUES (1, ?1, ?2, lower(hex(randomblob(8)))) \
                 ON CONFLICT (id) DO NOTHING RETURNING token"
            ),
            &[Value::Text(holder.to_owned()), Value::Text(word.to_owned())],
        )?;
        match rows.first() {
            Some(row) => Ok(Written::Won {
                token: row.text(0)?.to_owned(),
                cleared,
            }),
            None => Ok(Written::Lost),
        }
    })
    .map_err(|e| explained(e, wait))
}

/// Takes the lock for `holder`, the already-rendered text of who holds it.
///
/// Creates the ledger first when it is not there, as the other engines do. Returns
/// the guard to keep until [`release`], and the row it cleared, if any. A lock held
/// by a live holder, or by one whose liveness cannot be proved, is
/// `LedgerError::Locked` naming it from the row; a busy database is a `DbError`
/// that says it is not this tool's lock.
pub fn lock(c: &Connection, holder: &str) -> Result<Taken, LedgerError> {
    lock_within(c, holder, BUSY_WAIT)
}

/// [`lock`] with the wait given, so a test need not wait five seconds.
pub(crate) fn lock_within(
    c: &Connection,
    holder: &str,
    wait: std::time::Duration,
) -> Result<Taken, LedgerError> {
    // Counted here in characters: the column's `CHECK` cannot be the bound.
    fits("lock holder", Some(holder), LOCKED_BY_CHARS)?;
    waiting(c, wait, || {
        ensure_tables_within(c, wait)?;
        let (file, proof) = match attempt(c)? {
            Attempt::Held(file) => (Some(file), Proof::File),
            Attempt::Without(why) => (None, Proof::RowOnly(why)),
            Attempt::Alive => return Err(locked(c, Some(&Proof::File))),
        };
        // From here `file` is dropped, and the operating-system lock with it, on
        // every way out but a won lock.
        match write_row(c, holder, &proof, wait)? {
            Written::Won { token, cleared } => Ok(Taken {
                // Nothing to say about a holder that is gone, so no lookup.
                cleared: cleared.map(|(locked_by, locked_at)| LockInfo {
                    locked_by,
                    locked_at,
                    session_lookup: None,
                }),
                guard: LockGuard { file, token, proof },
            }),
            Written::Lost => Err(locked(c, Some(&proof))),
        }
    })
}

/// Releases a lock this run holds: deletes its own row, then lets go of the
/// operating-system lock, in that order and whether or not the delete worked.
///
/// `true` when the row was this run's and is gone; `false` when it was not there
/// to delete (a person ran `pbps unlock`) or was another run's, which is left
/// alone. A failed delete leaves the row, and the lock file is released all the
/// same: the next run then finds a stale row and clears it.
pub fn release(c: &Connection, mut guard: LockGuard) -> Result<bool, DbError> {
    let deleted = waiting(c, BUSY_WAIT, || {
        if occupants(c, &[LOCK_TABLE_NAME])?.is_empty() {
            return Ok(false);
        }
        Ok(c.execute(
            &format!("DELETE FROM {LOCK_TABLE} WHERE id = 1 AND token = ?1"),
            &[Value::Text(guard.token.clone())],
        )
        .map_err(|e| explained(e, BUSY_WAIT))?
            > 0)
    });
    if let Some(file) = guard.file.take() {
        // The lock goes with the handle in any case; unlocking first only makes the
        // order explicit.
        let _ = file.unlock();
    }
    deleted
}

/// Clears the lock whoever holds it: what `pbps unlock` does for a holder that is
/// gone, and what a run falls back to when it has no guard to [release].
///
/// `false` when no lock was held, or no lock table exists. It does not need the
/// operating-system lock, and it never touches the sidecar, which a live holder
/// may have locked.
pub fn unlock(c: &Connection) -> Result<bool, DbError> {
    waiting(c, BUSY_WAIT, || {
        let held = occupants(c, &[LOCK_TABLE_NAME])?;
        refuse_a_stranger(&held)?;
        if held.is_empty() {
            return Ok(false);
        }
        Ok(
            c.execute(&format!("DELETE FROM {LOCK_TABLE} WHERE id = 1"), &[])
                .map_err(|e| explained(e, BUSY_WAIT))?
                > 0,
        )
    })
}

/// Who holds the lock, if anyone.
///
/// An absent lock table is `None`: nothing can hold a lock that does not exist.
/// An unreadable one, including a busy database, stays an error, because "I could
/// not look" must never read as "nothing there".
pub fn lock_holder(c: &Connection) -> Result<Option<LockInfo>, DbError> {
    waiting(c, BUSY_WAIT, || {
        Ok(read_row(c)
            .map_err(|e| explained(e, BUSY_WAIT))?
            .map(|row| info(row, None)))
    })
}

/// What the session calls itself in the lock's row. There is no server to cut or
/// rename it, so there is nothing to read back.
pub fn session_application_name(_c: &Connection) -> Result<String, DbError> {
    Ok("pbps-sqlite".to_owned())
}

/// How a person finds a holder's sessions, from the text `locked_by` recorded;
/// `None` when it recorded no application name. SQLite has no sessions to list,
/// so this says so and says what can be done instead.
#[must_use]
pub fn session_lookup(locked_by: &str) -> Option<String> {
    pbps_db::holder_application_name(locked_by)?;
    Some(lookup(PROOF_FILE, None))
}
