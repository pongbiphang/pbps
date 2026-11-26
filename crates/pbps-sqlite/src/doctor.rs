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
    if let Some(path) = path.as_deref() {
        let shown = |p: &Path| Escaped(&p.display().to_string()).to_string();
        if !asked(path, false)? {
            gaps.push(PermissionGap {
                permission: "WRITE",
                securable: format!("database file `{}`", shown(path)),
                why: "the operating system does not let this process write the file".to_owned(),
            });
        }
        let directory = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !asked(directory, true)? {
            gaps.push(PermissionGap {
                permission: "WRITE",
                securable: format!("database directory `{}`", shown(directory)),
                why: "SQLite creates its journal beside the database file, and the operating \
                      system does not let this process create a file in this directory"
                    .to_owned(),
            });
        }
    }

    Ok(Permissions {
        gaps,
        ..Permissions::default()
    })
}

/// The operating system's answer, with "nothing to ask on this platform" read
/// as no gap found and everything else that is not an answer as an error.
fn asked(path: &Path, directory: bool) -> Result<bool, DbError> {
    match may_write(path, directory) {
        Ok(answer) => Ok(answer.unwrap_or(true)),
        // Nothing was learnt about the file a deployment would open.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(DbError::Refused(format!(
                "`{}` was moved or removed while the database was open; what this process may \
                 do to it could not be read",
                Escaped(&path.display().to_string())
            )))
        }
        Err(error) => Err(DbError::Refused(format!(
            "what this process may do to `{}` could not be read: {error}",
            Escaped(&path.display().to_string())
        ))),
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
