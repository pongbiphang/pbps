//! The integrity check: whether the ledger's tables are the ones this tool
//! creates (item 5 of #2950, DEC-2950.14).
//!
//! **A known layout, compared whole.** The recipe is three `CREATE TABLE`
//! statements and the engine keeps their text, so the check is a string equality
//! of each live table's stored text with [`Recipe::stored`], and everything else
//! the catalog holds under or beside a ledger name is a finding. Nothing is a list
//! of dangers: a trigger, an index, a column added or renamed, `WITHOUT ROWID`,
//! `STRICT`, a virtual table, a name in another case and a table of another shape
//! each change the text or add a catalog row, and so need no code of their own.
//!
//! **What the text cannot show** is stated in DEC-2950.14 and not guessed at here:
//! the rows (the lock's `proof` and `token`, the identifier, a reset of
//! `sqlite_sequence`) are data and not shape, and an edit made with
//! `writable_schema` that leaves the text alone is not seen.
//!
//! **Absent and partial are not problems.** A ledger table that is not there is
//! made by the next write ([`ensure_tables`](super::ensure_tables)), as the other
//! engines' checks leave it, so a database with no ledger, or with a state table
//! and no lock, has no problems and is not "clean" by this check either: the answer
//! is about what is there. An unreadable catalog is an error and never an empty
//! list.
//!
//! A read: it creates nothing and writes nothing. Under [`busy::waiting`], like
//! every function of the state layer.

use std::collections::BTreeSet;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, Value};
use pbps_model::escape::Escaped;

use super::busy::{self, BUSY_WAIT};
use super::ddl::{RECIPE, Recipe};
use crate::ident;

/// How many characters of a stored definition a finding quotes on each side of
/// the first difference: enough to recognise the change, not a second `CREATE`.
const EXCERPT_CHARS: usize = 36;

/// How the ledger's tables differ from the recipe they are created from, one line
/// each, naming the object; empty when they match or are not there yet.
///
/// An error when the catalog cannot be read, a busy database included.
pub fn ledger_problems(c: &Connection) -> Result<Vec<String>, DbError> {
    ledger_problems_within(c, BUSY_WAIT)
}

/// [`ledger_problems`] with the wait given, so a test need not wait five seconds.
pub(crate) fn ledger_problems_within(
    c: &Connection,
    wait: Duration,
) -> Result<Vec<String>, DbError> {
    busy::waiting(c, wait, || {
        problems(c).map_err(|e| busy::explained(e, wait))
    })
}

/// The refusal in front of every statement that writes to the ledger, given by
/// [`ensure_tables`](super::ensure_tables) (so by `record` and `lock`), `prune`,
/// `unlock` and `release`: a `DELETE` fires a trigger exactly as an `INSERT`
/// does. Two reads ask as well, `environment_identity` and `lock_holder`: each
/// answers from a one-row table, and a user's own table of that name and
/// columns would answer in its place. The reads of the state table do not.
pub(super) fn refuse_untrusted(c: &Connection, wait: Duration) -> Result<(), DbError> {
    let found = problems(c).map_err(|e| busy::explained(e, wait))?;
    if found.is_empty() {
        return Ok(());
    }
    Err(DbError::Refused(untrusted_ledger_message(&found)))
}

/// The refusal the writes give, and the text a diagnosis reports.
#[must_use]
pub fn untrusted_ledger_message(problems: &[String]) -> String {
    format!(
        "{}.\n\
         The ledger tables are not the ones this tool creates, so it will not write to them, nor \
         take the lock's holder or the environment's identifier from them: \
         a trigger on one would run with every write, and a table of another shape does not \
         keep what the ledger promises. If they were made by this tool and changed by hand, \
         restore them; otherwise rename or drop them and let this tool create its own. \
         Dropping the state table deletes the recorded history, so keep a copy of the file \
         first.",
        problems.join(";\n")
    )
}

/// One line for an object of a ledger name that is not an ordinary table. The
/// words are the refusal of `ensure_tables` for the same object, so a person reads
/// one sentence wherever it is found.
pub(super) fn stranger(name: &str, kind: &str) -> String {
    format!(
        "`{}` is {} {}, not an ordinary table",
        Escaped(name),
        if kind == "index" { "an" } else { "a" },
        Escaped(kind)
    )
}

/// A table name as the argument of a pragma statement, which takes no bound
/// parameter. [`ident::quote`] refuses the empty name, which SQLite accepts for
/// a table and the pragma reads (measured), so it is written out here: a table
/// of that name must not hide a key. A NUL ends the statement, so a name with
/// one is an error and never a question about some other table.
fn pragma_name(name: &str) -> Result<String, DbError> {
    if name.is_empty() {
        return Ok("\"\"".to_owned());
    }
    ident::quote(name).map_err(|_| {
        DbError::BadRow(format!(
            "the table name `{}` cannot be written into a statement",
            Escaped(name)
        ))
    })
}

fn problems(c: &Connection) -> Result<Vec<String>, DbError> {
    let names: Vec<&str> = RECIPE.iter().map(|r| r.name).collect();
    let slots = (1..=names.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let params: Vec<Value> = names.iter().map(|n| Value::Text((*n).to_owned())).collect();
    // Every row of the file's own catalog at a ledger name or on a ledger table,
    // and nothing from another schema: `main` is where the statements put the
    // ledger, and a `temp` object cannot be seen by another connection.
    let rows = c.query(
        &format!(
            "SELECT type, name, tbl_name, sql FROM main.sqlite_schema \
             WHERE name COLLATE NOCASE IN ({slots}) OR tbl_name COLLATE NOCASE IN ({slots}) \
             ORDER BY name, type"
        ),
        &params,
    )?;
    let mut found = Vec::new();
    for row in &rows {
        let (kind, name, on) = (row.text(0)?, row.text(1)?, row.text(2)?);
        let sql = row.optional_text(3)?;
        if let Some(recipe) = RECIPE.iter().find(|r| ident::same(r.name, name)) {
            if kind != "table" {
                found.push(stranger(name, kind));
            } else if sql != Some(recipe.stored().as_str()) {
                found.push(differs(name, recipe, sql.unwrap_or_default()));
            }
        } else if sql.is_some() {
            // An index or a trigger on a ledger table. An index with no text is the
            // engine's own for a `UNIQUE` or `PRIMARY KEY` clause, which the table's
            // text shows.
            found.push(format!(
                "`{}` is {} {} on `{}`, which this tool did not create",
                Escaped(name),
                if kind == "index" { "an" } else { "a" },
                Escaped(kind),
                Escaped(on)
            ));
        }
    }
    // A foreign key onto a ledger table makes a `DELETE` there (the prune, the
    // unlock) delete the referring rows or fail, and no ledger text shows it. The
    // pragma does not load a virtual table's module, so a table of a module this
    // library lacks answers with no keys and does not fail the question on an
    // application's file (measured, and a test keeps it so).
    //
    // The statement form, once per table, and not `pragma_foreign_key_list(...)`
    // in a `SELECT`: a `FROM` name is an ordinary table first, so an application
    // table named `pragma_foreign_key_list` (in `main`, `temp` or an attached
    // file) makes the table-valued form fail, and with it every gated operation
    // on a genuine ledger. No object an application can create changes what the
    // statement form returns. It is asked of `main` by name: unqualified, a
    // `TEMP` table of the child's name would answer in its place, with no key.
    let children = c.query(
        "SELECT name FROM main.sqlite_schema WHERE type = 'table' ORDER BY name",
        &[],
    )?;
    for child in &children {
        let child = child.text(0)?;
        let mut onto = BTreeSet::new();
        for key in c.query(
            &format!("PRAGMA main.foreign_key_list({})", pragma_name(child)?),
            &[],
        )? {
            let parent = key.text(2)?;
            if RECIPE.iter().any(|r| ident::same(r.name, parent)) {
                onto.insert(parent.to_owned());
            }
        }
        for parent in onto {
            found.push(format!(
                "`{}` has a foreign key onto `{}`, so a delete there would delete or refuse its rows",
                Escaped(child),
                Escaped(&parent)
            ));
        }
    }
    Ok(found)
}

/// Where a stored definition first departs from the recipe's, with a few
/// characters of each, so a person can see what changed without two statements.
fn differs(name: &str, recipe: &Recipe, found: &str) -> String {
    let wanted = recipe.stored();
    let at = found
        .chars()
        .zip(wanted.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let excerpt = |text: &str| -> String {
        let from = at.saturating_sub(EXCERPT_CHARS / 4);
        let cut: String = text.chars().skip(from).take(EXCERPT_CHARS).collect();
        let more = text.chars().count() > from + EXCERPT_CHARS;
        format!(
            "{}{cut}{}",
            if from > 0 { "…" } else { "" },
            if more { "…" } else { "" }
        )
    };
    format!(
        "`{}` is not defined as this tool creates it: from character {} its definition reads \
         `{}` where `{}` is expected",
        Escaped(name),
        at + 1,
        Escaped(&excerpt(found)),
        Escaped(&excerpt(&wanted))
    )
}
