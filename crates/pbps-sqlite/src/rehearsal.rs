//! The rehearsal's engine half (item 6 of #2950, DEC-2950.16): the scratch the
//! plan's statements run on, and the checks the statements do not make.
//!
//! The rehearsal itself is the engine-neutral `rehearse_in` of the command line:
//! it runs the plan as an apply runs it on a scratch, reads the scratch back and
//! compares it with the declarations, so it cannot disagree with `plan` about what
//! a difference is. What is SQLite's is here.
//!
//! **The scratch is a copy of the database's own pages** ([`scratch`]), made in
//! this process, in memory, on the linked library: nothing is started, nothing is
//! configured, and nothing is vouched for. The schema is exact (a function, a
//! collation or a virtual-table module the library lacks cannot be replayed from
//! text and is copied all the same) and the rows are there, so the engine itself
//! refuses what depends on them.
//!
//! **What the copy holds of the ledger** is whatever the file holds: the state
//! table's rows, the lock row, the environment identifier. Nothing reads them. A
//! rehearsal runs the plan's own statements and never `record`, `lock` or
//! `release`, which are an apply's, and the scratch is dropped when the rehearsal
//! ends. A lock taken on the copy would not be the file's (the sidecar is not
//! copied).
//!
//! **What a statement does not check** ([`resolution`]): a view that names what is
//! gone, a default calling a function that does not exist, a foreign key to a
//! column that is no key. SQLite accepts them when they are declared and refuses
//! them when they are used, so a plan that leaves one behind runs every statement
//! and ends with a database that fails later. Asked before the plan and again
//! after it on the copy, so only what the plan broke is the plan's.

use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::Connection;
use pbps_model::escape::Escaped;

use crate::state::{BUSY_WAIT, waiting};

/// The most a copy may take, in bytes: 512 MiB. A larger database is refused, and
/// the rehearsal is turned off by the operator, never skipped by the tool.
pub const SCRATCH_LIMIT: u64 = 512 * 1024 * 1024;

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// What every refusal ends with: the one way out, and what it records.
const WAY_OUT: &str = "Turn the rehearsal off with `--no-rehearse`; the plan is then recorded as \
                       not rehearsed.";

/// A copy of `target` for the rehearsal to run on, or the reason there is none.
///
/// `Refused` above `limit` (the size is read first, then enforced again on what
/// is copied: a database that grows past it during the copy is refused and the
/// partial copy dropped), and a busy database is the library's busy code with the
/// sentence that says what to do. `target` is not changed: no setting, no wait, no
/// transaction, no byte of the file.
pub fn scratch(target: &Connection, limit: u64) -> Result<Connection, DbError> {
    scratch_within(target, limit, BUSY_WAIT)
}

/// [`scratch`] with the wait given, so a test need not wait five seconds.
pub(crate) fn scratch_within(
    target: &Connection,
    limit: u64,
    wait: Duration,
) -> Result<Connection, DbError> {
    let library = target.version()?;
    let what = format!(
        "the rehearsal runs the plan on a copy of this database made in this process, in \
         memory, on SQLite {library}"
    );
    let size = waiting(target, wait, || target.size())?;
    if size > limit {
        return Err(DbError::Refused(format!(
            "{what}, and the database is {} where a copy may be {} at most, so this plan \
             cannot be rehearsed. {WAY_OUT}",
            mib(size),
            mib(limit)
        )));
    }
    target
        .copy_to_memory(limit, wait)
        .map_err(|error| explain_copy(error, &what, limit, wait))
}

/// What a failed copy is told to the operator as: a refusal from the copy itself
/// is a database that grew past the limit after its size was read, and the busy
/// code is a database that was not free for the whole wait.
fn explain_copy(error: DbError, what: &str, limit: u64, wait: Duration) -> DbError {
    if matches!(error, DbError::Refused(_)) {
        return DbError::Refused(format!(
            "{what}, and the database grew past {} while it was copied. Run again, or \
             {WAY_OUT}",
            mib(limit)
        ));
    }
    if error.server_error_code().as_deref() == Some("5") {
        return error.context(format!(
            "{what}, and the database stayed busy for {} s while it was copied: another \
             connection is writing it, or this connection is inside a write transaction. Run \
             again when it is free. {WAY_OUT}",
            wait.as_secs_f64()
        ));
    }
    error
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Detail {
    /// The engine's own words for what failed.
    Failed(String),
    /// A count of rows.
    Rows(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Unresolved {
    what: &'static str,
    name: String,
    detail: Detail,
}

/// What does not resolve in a database now. Compare two with [`Resolution::new_since`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution(Vec<Unresolved>);

/// Selects from every view, evaluates every column default and checks the foreign
/// keys, on `scratch`. Nothing is written. An engine refusal is a finding, not an
/// error: it is the thing being asked.
pub fn resolution(scratch: &Connection) -> Result<Resolution, DbError> {
    let mut found = Vec::new();
    let views = scratch.query(
        "SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'view' ORDER BY name",
        &[],
    )?;
    for view in &views {
        let name = view.text(0)?;
        let sql = format!("SELECT * FROM main.{} LIMIT 0", quote(name));
        if let Err(error) = scratch.query(&sql, &[]) {
            found.push(failed("view", name.to_owned(), &error));
        }
    }
    // A virtual table's columns are not asked of (its module may be missing), nor
    // a shadow table's; a generated column has no default.
    let defaults = scratch.query(
        "SELECT t.name, x.name, x.dflt_value FROM pragma_table_list AS t, \
         pragma_table_xinfo(t.name, 'main') AS x WHERE t.schema = 'main' AND t.type = 'table' \
         AND x.hidden = 0 AND x.dflt_value IS NOT NULL ORDER BY t.name, x.cid",
        &[],
    )?;
    for column in &defaults {
        let (table, name, default) = (column.text(0)?, column.text(1)?, column.text(2)?);
        // A line feed before the closing parenthesis, so a default that ends in a
        // `--` comment does not swallow it.
        if let Err(error) = scratch.query(&format!("SELECT ({default}\n)"), &[]) {
            found.push(failed("default", format!("{table}.{name}"), &error));
        }
    }
    match scratch.query("PRAGMA foreign_key_check", &[]) {
        Err(error) => found.push(failed("foreign keys", String::new(), &error)),
        Ok(rows) => {
            let mut counts: std::collections::BTreeMap<(String, String), u64> = Default::default();
            for row in &rows {
                *counts
                    .entry((row.text(0)?.to_owned(), row.text(2)?.to_owned()))
                    .or_default() += 1;
            }
            for ((table, parent), rows) in counts {
                found.push(Unresolved {
                    what: "foreign key",
                    name: format!("{table} -> {parent}"),
                    detail: Detail::Rows(rows),
                });
            }
        }
    }
    Ok(Resolution(found))
}

fn failed(what: &'static str, name: String, error: &DbError) -> Unresolved {
    Unresolved {
        what,
        name,
        detail: Detail::Failed(error.to_string()),
    }
}

fn quote(name: &str) -> String {
    ["\"", &name.replace('"', "\"\""), "\""].concat()
}

impl Resolution {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One line for each thing that fails now and did not fail, in the same words,
    /// before: what the plan broke. A view that fails because the library lacks an
    /// application's function fails the same way before the plan ran, and is not
    /// the plan's fault; a view that fails for another reason after, or that was
    /// fine before, is. Foreign-key rows count only the ones added.
    #[must_use]
    pub fn new_since(&self, before: &Resolution) -> Vec<String> {
        let mut lines = Vec::new();
        for item in &self.0 {
            let earlier = before
                .0
                .iter()
                .find(|b| b.what == item.what && b.name == item.name);
            match (&item.detail, earlier.map(|b| &b.detail)) {
                (Detail::Failed(now), Some(Detail::Failed(was))) if now == was => {}
                (Detail::Failed(now), _) => lines.push(match item.what {
                    "view" => format!(
                        "the view `{}` cannot be selected from after the plan: {}",
                        Escaped(&item.name),
                        Escaped(now)
                    ),
                    "default" => format!(
                        "the default of `{}` cannot be evaluated after the plan: {}",
                        Escaped(&item.name),
                        Escaped(now)
                    ),
                    _ => format!(
                        "the foreign keys cannot be checked after the plan: {}",
                        Escaped(now)
                    ),
                }),
                (Detail::Rows(now), Some(Detail::Rows(was))) if now <= was => {}
                (Detail::Rows(now), was) => {
                    let new = now
                        - was.map_or(0, |was| match was {
                            Detail::Rows(n) => *n,
                            Detail::Failed(_) => 0,
                        });
                    lines.push(format!(
                        "{new} more row(s) have no parent in `{}` after the plan",
                        Escaped(&item.name)
                    ));
                }
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests;
