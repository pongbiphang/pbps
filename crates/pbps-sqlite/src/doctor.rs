//! Readiness questions for a SQLite file (`pbps doctor`, SPEC §14.1, #2950).
//!
//! SQLite has no accounts or SQL `GRANT`s: what stops a deployment on a file is
//! whether this process may write the file and create a journal beside it.
//! `doctor` writes nothing and tries nothing (DECISIONS 38), so each is asked:
//! the connection for what it knows, the operating system for the rest. No
//! lock is taken, so the check can run beside a writer. The apply's preflight
//! is what tries, before it changes anything.
//!
//! Absent, empty and unreadable stay three answers: an absent ledger is not a
//! gap, and a file that is gone or whose journal is off is an error.

use std::path::Path;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::doctor::{PermissionGap, Permissions};
use pbps_db::ledger::{LOCK_TABLE_NAME, STATE_TABLE_NAME};
use pbps_db::sqlite::{Connection, may_write};
use pbps_model::escape::Escaped;

use crate::ident;
use crate::preflight::journal_off;
use crate::state::{BUSY_WAIT, LOCK_TABLE, STATE_TABLE, database_file, occupants, waiting};

/// What a deployment on this file needs and this process lacks.
pub fn permissions(c: &Connection) -> Result<Permissions, DbError> {
    permissions_within(c, BUSY_WAIT)
}

pub(crate) fn permissions_within(c: &Connection, wait: Duration) -> Result<Permissions, DbError> {
    let mut gaps = Vec::new();
    let path = match database_file(c)? {
        Ok(path) => path,
        Err(why) => return Err(DbError::Refused(why.reason())),
    };

    // Absent ledger tables are not a gap; non-table occupants are left to
    // `ledger_problems` (`ledger.untrusted`) so they stay exit-2 findings.
    for qualified in existing_ledger_tables(c, wait)? {
        waiting(c, wait, || {
            c.query(&format!("SELECT 1 FROM {qualified} LIMIT 0"), &[])
        })?;
    }

    // The apply refuses a connection with no journal, so it is not ready.
    if let Some(line) = journal_off(c)? {
        return Err(DbError::Refused(line));
    }
    if query_only(c)? {
        gaps.push(PermissionGap {
            permission: "WRITE",
            securable: "the database connection".to_owned(),
            why: "`PRAGMA query_only` is on, so this connection is read-only; deployments \
                  write through it"
                .to_owned(),
        });
    }

    // The file is not opened a second time to ask: closing any descriptor of
    // a file drops this process's POSIX locks on it, SQLite's own included.
    let mut unasked = Vec::new();
    if let Some(path) = path.as_deref() {
        // Only the files this journal mode writes: a `-wal` file left beside
        // a rollback-journal database is not written, and may be read-only.
        let kept: &[&str] = if write_ahead(c)? {
            &["-wal", "-shm"]
        } else {
            &["-journal"]
        };
        ask_the_system(path, kept, may_write, &mut gaps, &mut unasked)?;
    }

    Ok(Permissions {
        gaps,
        unasked,
        ..Permissions::default()
    })
}

/// Asks `may` about the file and its directory. Three answers stay three: no
/// is a gap, "this platform cannot be asked" is said and never read as yes,
/// and a question that failed is an error.
fn ask_the_system(
    path: &Path,
    kept: &[&str],
    may: impl Fn(&Path, bool) -> std::io::Result<Option<bool>>,
    gaps: &mut Vec<PermissionGap>,
    unasked: &mut Vec<PermissionGap>,
) -> Result<(), DbError> {
    let shown = |p: &Path| Escaped(&p.display().to_string()).to_string();
    let directory = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let asked = |p: &Path, is_directory: bool| {
        may(p, is_directory).map_err(|error| {
            DbError::Refused(if error.kind() == std::io::ErrorKind::NotFound {
                // Nothing was learnt about the file a deployment would open.
                format!(
                    "`{}` was moved or removed while the database was open; what this \
                     process may do to it could not be read",
                    shown(p)
                )
            } else {
                format!(
                    "what this process may do to `{}` could not be read: {error}",
                    shown(p)
                )
            })
        })
    };
    let file = asked(path, false)?;
    let beside = asked(directory, true)?;
    if file == Some(false) {
        gaps.push(PermissionGap {
            permission: "WRITE",
            securable: format!("database file `{}`", shown(path)),
            why: "the operating system does not let this process write the file".to_owned(),
        });
    }
    // SQLite also writes the files its journal mode keeps beside the database
    // (`kept`): a commit in WAL mode goes to the `-wal` file. Each is asked
    // when it exists; one that does not is created in the directory.
    for suffix in kept {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let kept = std::path::PathBuf::from(name);
        match may(&kept, false) {
            Ok(Some(false)) => gaps.push(PermissionGap {
                permission: "WRITE",
                securable: format!("journal file `{}`", shown(&kept)),
                why: "SQLite writes this file when it commits, and the operating system \
                      does not let this process write it"
                    .to_owned(),
            }),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                asked(&kept, false)?;
            }
        }
    }
    if beside == Some(false) {
        gaps.push(PermissionGap {
            permission: "WRITE",
            securable: format!("database directory `{}`", shown(directory)),
            why: "SQLite creates its journal beside the database file, and the operating \
                  system does not let this process create a file in this directory"
                .to_owned(),
        });
    }
    if file.is_none() || beside.is_none() {
        unasked.push(PermissionGap {
            permission: "WRITE",
            securable: format!("database file `{}` and its directory", shown(path)),
            why: "this platform answers only an attempt, and this command tries nothing; \
                  `apply` tries before it changes anything"
                .to_owned(),
        });
    }
    Ok(())
}

/// Whether this connection journals to a write-ahead log.
fn write_ahead(c: &Connection) -> Result<bool, DbError> {
    match c.query("PRAGMA journal_mode", &[])?.first() {
        Some(row) => Ok(row.text(0)?.eq_ignore_ascii_case("wal")),
        None => Err(DbError::BadRow(
            "`PRAGMA journal_mode` returned no row".into(),
        )),
    }
}

fn query_only(c: &Connection) -> Result<bool, DbError> {
    let rows = c.query("PRAGMA query_only", &[])?;
    match rows.first() {
        Some(row) => Ok(row.integer(0)? != 0),
        None => Err(DbError::BadRow(
            "`PRAGMA query_only` returned no row".into(),
        )),
    }
}

fn existing_ledger_tables(c: &Connection, wait: Duration) -> Result<Vec<&'static str>, DbError> {
    let held = waiting(c, wait, || {
        occupants(c, &[STATE_TABLE_NAME, LOCK_TABLE_NAME])
    })?;
    Ok([
        (STATE_TABLE_NAME, STATE_TABLE),
        (LOCK_TABLE_NAME, LOCK_TABLE),
    ]
    .into_iter()
    .filter(|(name, _)| {
        held.iter()
            .any(|(found, kind)| kind == "table" && ident::same(found, name))
    })
    .map(|(_, qualified)| qualified)
    .collect())
}

#[cfg(test)]
mod tests;
