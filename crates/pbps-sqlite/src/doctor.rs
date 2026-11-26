//! Readiness questions for a SQLite file (`pbps doctor`, SPEC §14.1, #2950).
//!
//! SQLite has no accounts or SQL `GRANT`s: deployment readiness depends on OS
//! permissions on the database file and its directory (journal and lock sidecar)
//! and readability/writability of existing ledger tables. Absent, empty and
//! unreadable stay three answers: an absent ledger is not a gap, and a busy,
//! unlinked, unreadable or `journal_mode = OFF` database returns `Err`.

use std::path::Path;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::doctor::{PermissionGap, Permissions};
use pbps_db::ledger::{LOCK_TABLE_NAME, STATE_TABLE_NAME};
use pbps_db::sqlite::Connection;
use pbps_model::escape::Escaped;

use crate::ident;
use crate::preflight::{WriteProbe, probe_writable_within};
use crate::state::{BUSY_WAIT, LOCK_TABLE, STATE_TABLE, database_file, occupants, waiting};

/// `SQLITE_READONLY_DBMOVED`.
const READONLY_MOVED: u32 = 1032;

/// Checks database file access, journal-directory writability, and access to
/// existing ledger/lock tables. Uses [`probe_writable_within`], which runs
/// inside a rolled-back transaction and refuses if `journal_mode = OFF`.
pub fn permissions(c: &Connection) -> Result<Permissions, DbError> {
    permissions_within(c, BUSY_WAIT)
}

pub(crate) fn permissions_within(c: &Connection, wait: Duration) -> Result<Permissions, DbError> {
    let mut gaps = Vec::new();
    let path = match database_file(c)? {
        Ok(path) => path,
        Err(why) => return Err(DbError::Refused(why.reason())),
    };
    // The file is not opened a second time to ask: closing any descriptor of
    // a file drops this process's POSIX locks on it, SQLite's own included.
    // SQLite answers through the probe below.

    // Absent ledger tables are not a gap; non-table occupants are left to
    // `ledger_problems` (`ledger.untrusted`) so they stay exit-2 findings.
    let tables = existing_ledger_tables(c, wait)?;
    for &qualified in &tables {
        waiting(c, wait, || {
            c.query(&format!("SELECT 1 FROM {qualified} LIMIT 0"), &[])
        })?;
    }

    let extra_sql: Vec<String> = tables
        .iter()
        .map(|table| format!("DELETE FROM {table} WHERE 0"))
        .collect();
    let extra_refs: Vec<&str> = extra_sql.iter().map(String::as_str).collect();
    match probe_writable_within(c, wait, &extra_refs)? {
        WriteProbe::Writable => {}
        WriteProbe::JournalOff(line) => return Err(DbError::Refused(line)),
        WriteProbe::ReadOnlyDirectory { code } => {
            let securable = path
                .as_deref()
                .and_then(Path::parent)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map_or_else(
                    || "the database directory".to_owned(),
                    |d| format!("database directory `{}`", Escaped(&d.display().to_string())),
                );
            gaps.push(PermissionGap {
                permission: "WRITE",
                securable,
                why: format!(
                    "SQLite cannot create its journal beside the database file (code {code}); \
                     deployments need write access to this directory"
                ),
            });
        }
        // The file was moved or removed under the open connection: nothing
        // was learnt about the file a deployment would open.
        WriteProbe::ReadOnly { code } if code == READONLY_MOVED => {
            return Err(DbError::Refused(format!(
                "the database file was moved or removed while it was open (SQLite code {code}); \
                 what this process may do to it could not be read"
            )));
        }
        WriteProbe::ReadOnly { code } => gaps.push(PermissionGap {
            permission: "WRITE",
            securable: path.as_deref().map_or_else(
                || "the database connection".to_owned(),
                |p| format!("database file `{}`", Escaped(&p.display().to_string())),
            ),
            why: format!(
                "SQLite cannot write to this database (code {code}): the file, its \
                 directory or the connection (`PRAGMA query_only`) is read-only"
            ),
        }),
    }

    Ok(Permissions {
        gaps,
        ..Permissions::default()
    })
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
