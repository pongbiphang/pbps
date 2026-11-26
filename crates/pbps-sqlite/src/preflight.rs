//! Checks before a plan runs (item 6 of #2950, DEC-2950.15).
//!
//! What the other engines ask before an apply's first statement is mostly about
//! a server: roles, editions, session settings, a catalog another connection
//! can change. None of that exists here. What is left is small and real, and is
//! asked of the connection and the file, never of the rows (the data probes are
//! not built, DEC-2950.15):
//!
//! - **the connection's own settings** that decide what the plan's statements
//!   mean ([`connection_problems`]);
//! - **the library against the plan** ([`library_problems`]);
//! - **the ledger** (`state::ledger_problems`, item 5d), asked again here so a
//!   caller has one answer;
//! - **whether the file can be written** ([`writable`]), apply only;
//! - **what a run on this library does not prove** when the project's floor is
//!   older ([`floor_notes`]), a note and not a refusal.
//!
//! Every function is a read, or a write that is rolled back and leaves nothing
//! behind. An answer that cannot be read is an error and never an empty list.

use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::Connection;
use pbps_model::ChangeSet;

use crate::emit::Form;
use crate::state::{self, BUSY_WAIT, explained_as, waiting};
use crate::{Version, catalog};

/// When the check runs: `plan` reads and writes nothing, so it does not ask
/// whether the file can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Plan,
    Apply,
}

/// What the checks found: `problems` refuse, one line each, and `notes` are
/// printed and refuse nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

/// One numeric connection setting the plan's statements depend on, the value it must
/// have, and what goes wrong otherwise (each measured on the bundled library).
struct Setting {
    pragma: &'static str,
    wanted: i64,
    why: &'static str,
}

/// The defaults of the bundled library are the `wanted` values, and `foreign_keys`
/// is on because the library is built with it. The connection is this tool's own,
/// so nothing outside the process changes these: the read is the guard for a
/// wiring that sets one, and for a library that is not the bundled one, as the
/// connection module's own read-back of `foreign_keys` is.
const SETTINGS: [Setting; 6] = [
    Setting {
        pragma: "foreign_keys",
        wanted: 1,
        why: "the ledger and the plan's constraints rely on it",
    },
    Setting {
        pragma: "legacy_alter_table",
        wanted: 0,
        why: "a rename would leave a view and a trigger naming the old name, and every statement \
              would still succeed",
    },
    Setting {
        pragma: "ignore_check_constraints",
        wanted: 0,
        why: "a CHECK would not be enforced on writes, and the ledger's widths rest on CHECKs",
    },
    Setting {
        pragma: "defer_foreign_keys",
        wanted: 0,
        why: "a violation would fail at the commit, not at its statement",
    },
    Setting {
        pragma: "query_only",
        wanted: 0,
        why: "every write would be refused",
    },
    Setting {
        pragma: "writable_schema",
        wanted: 0,
        why: "a statement could edit the catalog",
    },
];

/// How the connection differs from the one the plan's statements are written
/// for, one line each; empty when it does not.
pub fn connection_problems(c: &Connection) -> Result<Vec<String>, DbError> {
    let mut found = Vec::new();
    for setting in &SETTINGS {
        let rows = c.query(&format!("PRAGMA {}", setting.pragma), &[])?;
        let Some(row) = rows.first() else {
            return Err(DbError::BadRow(format!(
                "`PRAGMA {}` returned no row",
                setting.pragma
            )));
        };
        let value = row.integer(0)?;
        if value != setting.wanted {
            found.push(format!(
                "`PRAGMA {}` reads {value} on this connection and this tool needs {}: {}",
                setting.pragma, setting.wanted, setting.why
            ));
        }
    }
    found.extend(journal_off(c)?);
    Ok(found)
}

/// The line for a connection whose rollback journal is off, if it is.
///
/// SQLite documents a rollback as undefined with `journal_mode = OFF`. Measured:
/// a small rolled-back transaction is dropped from the page cache, and one large
/// enough to have written pages reports success and leaves them in the file. So
/// a failed apply could keep part of what ran, and the probe of [`writable`]
/// rests on a rollback too.
fn journal_off(c: &Connection) -> Result<Option<String>, DbError> {
    let rows = c.query("PRAGMA journal_mode", &[])?;
    let Some(row) = rows.first() else {
        return Err(DbError::BadRow(
            "`PRAGMA journal_mode` returned no row".into(),
        ));
    };
    Ok(row.text(0)?.eq_ignore_ascii_case("off").then(|| {
        "`PRAGMA journal_mode` reads off on this connection and this tool needs a journal: \
         without one a rollback is not reliable, so a failed apply could keep part of what ran"
            .to_owned()
    }))
}

/// What the library's version cannot do that the plan or the reader needs, one
/// line each.
///
/// The floor gates only text kept in the file (DEC-2950.4), because the in-place
/// forms are run by the library that is linked. That is what makes this check
/// unable to fail with the bundled library; it is the guard that decision says
/// comes back if statements are ever run on another one.
pub fn library_problems(c: &Connection, changes: &ChangeSet) -> Result<Vec<String>, DbError> {
    Ok(library_lines(c.version()?, changes))
}

fn library_lines(library: Version, changes: &ChangeSet) -> Vec<String> {
    let mut found = Vec::new();
    if library < catalog::NEEDED {
        found.push(format!(
            "reading a SQLite catalog needs {} (`PRAGMA table_list`), and this library is {library}",
            catalog::NEEDED
        ));
    }
    let mut seen: Vec<Form> = Vec::new();
    for planned in &changes.changes {
        let Some(form) = Form::of(&planned.change) else {
            continue;
        };
        if seen.contains(&form) {
            continue;
        }
        seen.push(form);
        if library < form.arrival() {
            found.push(format!(
                "{} needs SQLite {}, and this library is {library}",
                form.words(),
                form.arrival()
            ));
        }
    }
    found
}

/// Whether the connection can write the file: the first write of an apply
/// creates the journal beside it, and in a write-protected directory that fails
/// with the engine's `attempt to write a readonly database`, which says nothing
/// about the directory.
///
/// Asked by creating a table of a random name inside a savepoint and rolling the
/// savepoint back, so nothing of the application's is written, not even to the
/// value it already has. Measured in a write-protected directory, this fails with
/// the same code as the first real write (extended code 1544), and in a writable
/// one it leaves the file's bytes and the directory as they were.
///
/// `Refused` when the file cannot be written (the sentence says why); any other
/// error, a busy database included, is the engine's.
pub fn writable(c: &Connection) -> Result<(), DbError> {
    writable_within(c, BUSY_WAIT)
}

/// What the rolled-back write probe established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteProbe {
    Writable,
    ReadOnlyDirectory { code: u32 },
    ReadOnly { code: u32 },
    JournalOff(String),
}

/// [`writable`] with the wait given, so a test need not wait five seconds.
pub(crate) fn writable_within(c: &Connection, wait: Duration) -> Result<(), DbError> {
    match probe_writable_within(c, wait, &[])? {
        WriteProbe::Writable => Ok(()),
        WriteProbe::JournalOff(line) => Err(DbError::Refused(line)),
        WriteProbe::ReadOnlyDirectory { code } => Err(refuse_readonly(code, true)),
        WriteProbe::ReadOnly { code } => Err(refuse_readonly(code, false)),
    }
}

/// Runs the rolled-back write probe plus `extra_writes` inside one rolled-back
/// transaction, returning a typed classification.
pub(crate) fn probe_writable_within(
    c: &Connection,
    wait: Duration,
    extra_writes: &[&str],
) -> Result<WriteProbe, DbError> {
    waiting(c, wait, || {
        // The probe is undone by a rollback, which is not reliable without a journal.
        if let Some(line) = journal_off(c)? {
            return Ok(WriteProbe::JournalOff(line));
        }
        let name = probe_name(c)?;
        // The apply's own framing: the write lock is taken at once, so a second
        // writer is named as busy and not as an unwritable file.
        let outside = !c.in_transaction();
        if outside && let Err(error) = c.execute_batch("BEGIN IMMEDIATE") {
            return classify_write_error(error, wait);
        }
        let tried = c.execute_batch("SAVEPOINT pbps_probe").and_then(|()| {
            for sql in extra_writes {
                c.execute_batch(sql)?;
            }
            c.execute_batch(&format!("CREATE TABLE main.{name} (x)"))
        });
        // However it went, undone: inside the caller's transaction only the
        // savepoint, outside it the transaction this function opened. Its own
        // error cannot replace the one that caused it.
        let undone = c.execute_batch(if outside {
            "ROLLBACK"
        } else {
            "ROLLBACK TO pbps_probe; RELEASE pbps_probe"
        });
        match (tried, undone) {
            // The probe's table needs a page, and a database at its page limit
            // with none free refuses that page, not the write: a plan that only
            // renames, or writes where there is room, still commits there
            // (measured). So it is no answer to this question, and the plan is
            // not refused for it. Only at that limit: the same code comes from
            // a volume or a quota with no room, where the first real write
            // fails too, and that stays the engine's error. In a
            // write-protected directory the journal fails first (measured), so
            // that refusal is not lost.
            (Err(error), Ok(())) if is_full(&error) && at_the_page_limit(c)? => {
                Ok(WriteProbe::Writable)
            }
            (Err(error), _) => classify_write_error(error, wait),
            (Ok(()), Err(error)) => Err(busy(error, wait)),
            (Ok(()), Ok(())) => Ok(WriteProbe::Writable),
        }
    })
}

/// A name no object holds, made by the library's own randomness.
fn probe_name(c: &Connection) -> Result<String, DbError> {
    let rows = c.query("SELECT lower(hex(randomblob(8)))", &[])?;
    match rows.first() {
        Some(row) => Ok(format!("__pbps_probe_{}", row.text(0)?)),
        None => Err(DbError::BadRow("`randomblob` returned no row".into())),
    }
}

/// The primary result code of a write that needed a page the database may not
/// grow by (`SQLITE_FULL`).
const FULL: u32 = 13;

/// Whether the database stands at `max_page_count` with no free page, which is
/// when a new table cannot be had and an existing page still can be written.
/// The pragma without a value reads the limit and does not set it.
fn at_the_page_limit(c: &Connection) -> Result<bool, DbError> {
    let integer = |sql: &str| -> Result<i64, DbError> {
        match c.query(sql, &[])?.first() {
            Some(row) => row.integer(0),
            None => Err(DbError::BadRow(format!("`{sql}` returned no row"))),
        }
    };
    Ok(integer("PRAGMA main.freelist_count")? == 0
        && integer("PRAGMA main.page_count")? >= integer("PRAGMA main.max_page_count")?)
}

/// A busy refusal of the probe, in the probe's name: it is not a statement of
/// the ledger that waited.
fn busy(error: DbError, wait: Duration) -> DbError {
    explained_as(error, wait, "the check that the file can be written")
}

fn is_full(error: &DbError) -> bool {
    error
        .server_error_code()
        .and_then(|code| code.parse::<u32>().ok())
        .is_some_and(|code| code & 0xff == FULL)
}

/// The primary result code of a write refused because the database is read-only,
/// and the extended code of the directory case (the journal cannot be made).
const READONLY: u32 = 8;
const READONLY_DIRECTORY: u32 = 1544;

fn refuse_readonly(code: u32, directory: bool) -> DbError {
    DbError::Refused(if directory {
        format!(
            "SQLite can open this database but cannot write it (code {code}): the directory \
             that holds the file is write-protected, so the journal beside it cannot be made. \
             The first statement of the apply would fail the same way"
        )
    } else {
        format!(
            "SQLite can open this database but cannot write it (code {code}): the file, its \
             directory or the connection is read-only. The first statement of the apply would \
             fail the same way"
        )
    })
}

fn classify_write_error(error: DbError, wait: Duration) -> Result<WriteProbe, DbError> {
    let Some(code) = error
        .server_error_code()
        .and_then(|code| code.parse::<u32>().ok())
    else {
        return Err(error);
    };
    if code & 0xff != READONLY {
        return Err(busy(error, wait));
    }
    Ok(if code == READONLY_DIRECTORY {
        WriteProbe::ReadOnlyDirectory { code }
    } else {
        WriteProbe::ReadOnly { code }
    })
}

/// What a run on this library does not prove when the project's floor is older.
///
/// The floor gates only text this tool writes into the file, and of that only
/// `TRUE` and `FALSE` (DEC-2950.4, #2965). A declared default, check, index
/// filter or view body can keep syntax newer than the floor, and a library at the
/// floor cannot open the file then. Nothing run here on the newer library can
/// show it.
pub fn floor_notes(library: Version, floor: Version) -> Vec<String> {
    if floor >= library {
        return Vec::new();
    }
    vec![format!(
        "this run uses SQLite {library} and the project's floor is {floor}: text this tool \
         writes into the file is checked against the floor only for TRUE and FALSE, so a \
         declared default, check, index filter or view body can keep syntax newer than \
         {floor} (`DEFAULT (1_000)` needs 3.46), which a library at the floor cannot open"
    )]
}

/// What a plan is checked against before it runs.
///
/// The connection, the library and the ledger at both stages; whether the file
/// can be written at apply only; the floor note at both.
pub fn before_apply(
    c: &Connection,
    changes: &ChangeSet,
    floor: Version,
    stage: Stage,
) -> Result<Verdict, DbError> {
    verdict_for(c, c.version()?, changes, floor, stage)
}

/// [`before_apply`] with the library's version given, so a test can ask what an
/// older library would be told.
fn verdict_for(
    c: &Connection,
    library: Version,
    changes: &ChangeSet,
    floor: Version,
    stage: Stage,
) -> Result<Verdict, DbError> {
    let mut verdict = Verdict::default();
    verdict.problems.extend(connection_problems(c)?);
    verdict.problems.extend(library_lines(library, changes));
    verdict.problems.extend(state::ledger_problems(c)?);
    if stage == Stage::Apply {
        match writable(c) {
            Ok(()) => {}
            // The journal's line is the connection's too; said once.
            Err(DbError::Refused(line)) if verdict.problems.contains(&line) => {}
            Err(DbError::Refused(line)) => verdict.problems.push(line),
            Err(other) => return Err(other),
        }
    }
    verdict.notes.extend(floor_notes(library, floor));
    Ok(verdict)
}

/// The refusal an apply gives when [`before_apply`] found problems.
#[must_use]
pub fn refusal(problems: &[String]) -> String {
    format!(
        "{}.\nNothing was changed. Fix what is named, or run against a database where it \
         does not apply, and run again.",
        problems.join(";\n")
    )
}

#[cfg(test)]
mod tests;
