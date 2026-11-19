//! The SQL for the changes SQLite makes in place (#2950, item 3).
//!
//! `emit` turns one [`Change`] into its statements, and is the only place a
//! change becomes SQLite text. A change has one of four answers, and the match
//! over the changes names every variant, so a new kind of change is a compile
//! error here and not a silent gap:
//!
//! - **In place**: its statements. `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE`
//!   (`RENAME`, `ADD`, `RENAME` and `DROP COLUMN`, `ALTER COLUMN .. SET` and
//!   `DROP NOT NULL`, `ADD` and `DROP CONSTRAINT`), `CREATE` and `DROP INDEX`,
//!   `CREATE` and `DROP VIEW`.
//! - **Needs the table rebuilt**: `NotBuilt`, by name. SQLite cannot change a
//!   column's type or default, or add a key, in place. Never an empty success and
//!   never a statement the library would refuse at apply.
//! - **Cannot occur**: validation refuses the declaration (`Unsupported` or
//!   `NotBuilt`, as it does). The emitter still answers, because a saved plan can
//!   carry any change.
//!
//! # Names
//!
//! The object a statement creates, alters or drops is written `main."name"`.
//! Measured: a bare `ALTER TABLE t` or `DROP TABLE t` acts on a TEMP table named
//! `t` if one exists. `REFERENCES`, the new name of a rename, `ON <table>` of an
//! index, and the text of a view or a check are written bare: a qualified parent
//! in `REFERENCES` is a syntax error, and the rest is not ours to qualify.
//!
//! # What the floor gates
//!
//! The version floor is the oldest SQLite an application opens the file with. It
//! gates text that is written into the file and stays there (`DEFAULT TRUE`),
//! not a statement that only the tool's own bundled library executes and that
//! leaves old syntax behind: measured, a file altered by 3.54.1 with each in-place
//! form opens, passes `integrity_check` and enforces its constraints on 3.46.1,
//! and the stored `CREATE` text equals what a rebuild by hand stores, with no
//! newer token. [`allows_under`] is the one place that decision lives (DEC-2950.4).

use pbps_dialect::{DialectError, Statement};
use pbps_model::{Change, ConstraintChange, PlannedChange, TableChange, TableName};

use crate::dialect::Sqlite;
use crate::validate::{SCHEMA, not_built, unsupported};
use crate::{Version, ident, lexis};

mod column;
mod constraint;
mod index;
mod module;
mod refused;
mod table;
#[cfg(test)]
mod tests;

/// What a change emits.
pub(crate) type Sql = Result<Vec<Statement>, DialectError>;

/// The in-place `ALTER` forms whose arrival version is kept, with the version
/// that brought each. Whether the floor gates them is [`GATE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    RenameColumn,
    DropColumn,
    SetNotNull,
    DropNotNull,
    AddCheck,
    DropCheck,
    DropForeignKey,
}

impl Form {
    #[cfg(test)]
    pub(crate) const ALL: [Form; 7] = [
        Form::RenameColumn,
        Form::DropColumn,
        Form::SetNotNull,
        Form::DropNotNull,
        Form::AddCheck,
        Form::DropCheck,
        Form::DropForeignKey,
    ];

    /// The SQLite release that brought the form (SQLite's release notes; the
    /// last five are the 3.53.0 release of 2026-04-09, measured to be syntax
    /// errors on 3.46.1).
    pub(crate) const fn arrival(self) -> Version {
        match self {
            Form::RenameColumn => Version::new(3, 25, 0),
            Form::DropColumn => Version::new(3, 35, 0),
            Form::SetNotNull
            | Form::DropNotNull
            | Form::AddCheck
            | Form::DropCheck
            | Form::DropForeignKey => Version::new(3, 53, 0),
        }
    }
}

/// What the floor gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Only text written verbatim into the file: the in-place forms are always
    /// available, because the library that runs them is the one pbps links.
    Text,
    /// Every in-place form, by the version that brought it. Comes back if pbps
    /// ever executes statements on a library other than the bundled one.
    #[allow(
        dead_code,
        reason = "the setting that comes back; only the tests run it"
    )]
    EveryForm,
}

/// **The decision**, in one place (DEC-2950.4).
pub(crate) const GATE: Gate = Gate::Text;

/// The first release that reads `TRUE` and `FALSE` as constants. Before it they
/// are names, so a default written `TRUE` would mean the text `'TRUE'`, and a
/// check or a view that says `TRUE` would name a column that is not there.
pub(crate) const BOOLEAN_WORDS: Version = Version::new(3, 23, 0);

/// Whether a project that targets `floor` may have `form` written in place.
#[must_use]
pub(crate) fn allows_under(gate: Gate, floor: Version, form: Form) -> bool {
    match gate {
        Gate::Text => true,
        Gate::EveryForm => floor >= form.arrival(),
    }
}

/// Emits one change for a project on `dialect`'s floor.
pub(crate) fn emit(dialect: &Sqlite, change: &Change) -> Sql {
    emit_under(GATE, dialect, change)
}

/// Emits the change at `position` of a plan.
///
/// The differ takes a new table's foreign keys out of its `CreateTable` and
/// plans each as an `AddForeignKey`, because other engines add them once every
/// table exists. SQLite has no statement that adds one, and needs none: a
/// `CREATE TABLE` may reference a table that is not there yet (measured). So
/// the keys the plan adds to a table it creates are written inside that
/// table's `CREATE`, and each such `AddForeignKey` writes nothing of its own.
/// One on a table the plan does not create is still the rebuild's.
pub(crate) fn emit_at(dialect: &Sqlite, changes: &[PlannedChange], position: usize) -> Sql {
    let change = &changes[position].change;
    if let Change::Constraint(ConstraintChange::AddForeignKey { table, .. }) = change
        && changes.iter().any(|planned| {
            matches!(&planned.change,
                Change::Table(TableChange::CreateTable { name, .. }) if name == table)
        })
    {
        return Ok(Vec::new());
    }
    if let Change::Table(TableChange::CreateTable { name, .. }) = change
        && added(changes, name).next().is_some()
    {
        let mut whole = change.clone();
        if let Change::Table(TableChange::CreateTable { table, .. }) = &mut whole {
            table.foreign_keys.extend(added(changes, name));
        }
        return emit(dialect, &whole);
    }
    emit(dialect, change)
}

/// The foreign keys a plan adds to the table `to`.
fn added<'a>(
    changes: &'a [PlannedChange],
    to: &'a TableName,
) -> impl Iterator<Item = (pbps_model::ObjectLocalName, pbps_model::ForeignKey)> + 'a {
    changes.iter().filter_map(move |planned| {
        if let Change::Constraint(ConstraintChange::AddForeignKey {
            table,
            name,
            constraint,
        }) = &planned.change
            && table == to
        {
            return Some((name.clone(), (**constraint).clone()));
        }
        None
    })
}

/// [`emit`] under a stated gate, so each form's test runs under both.
pub(crate) fn emit_under(gate: Gate, dialect: &Sqlite, change: &Change) -> Sql {
    let statements = lowered(gate, dialect, change)?;
    kept_text(dialect, &statements)?;
    Ok(statements)
}

/// What a statement writes into the file must mean to the floor's SQLite what
/// it means to the one that runs it. `TRUE` and `FALSE` do not: before
/// [`BOOLEAN_WORDS`] they are names. Every statement is read here, after it is
/// written, so a default, a check, an index's filter and a view's body are held
/// alike and a new statement cannot forget to ask. The read is of code only:
/// a string, a quoted name and a comment are not the words.
fn kept_text(dialect: &Sqlite, statements: &[Statement]) -> Result<(), DialectError> {
    if dialect.at_least(BOOLEAN_WORDS) {
        return Ok(());
    }
    for word in ["TRUE", "FALSE"] {
        if statements
            .iter()
            .any(|statement| lexis::has_bare_word(&statement.sql, word))
        {
            return Err(not_built(format!(
                "`{word}` in text kept in the file, for a project whose floor is {}: SQLite reads \
                 TRUE and FALSE as constants from {BOOLEAN_WORDS}, and as names before it (write 1 \
                 or 0, or quote a column of that name)",
                dialect.floor()
            )));
        }
    }
    Ok(())
}

fn lowered(gate: Gate, dialect: &Sqlite, change: &Change) -> Sql {
    match change {
        Change::Table(change) => table::emit(change),
        Change::TableProperty(change) => refused::table_property(change),
        Change::Partition(change) => refused::partition(change),
        Change::Column(change) => column::emit(dialect, gate, change),
        Change::Constraint(change) => constraint::emit(dialect, gate, change),
        Change::Index(change) => index::emit(change),
        Change::Policy(change) => refused::policy(change),
        Change::Row(change) => refused::row(change),
        Change::Module(change) => module::emit(change),
        Change::Permission(change) => refused::permission(change),
        Change::TypeValue(change) => refused::type_value(change),
        Change::Role(change) => refused::role(change),
        Change::Type(change) => refused::declared_type(change),
        Change::Extension(change) => refused::extension(change),
        Change::Search(change) => refused::search(change),
    }
}

/// The answer for what needs the table rebuilt.
pub(crate) fn rebuild(what: &str) -> DialectError {
    not_built(format!("the table rebuild (item 8 of #2950): {what}"))
}

/// Whether the gate in force lets the project's floor have `form`. What it does
/// not is the rebuild's, said by name and never a statement the library would
/// take or refuse by luck.
pub(crate) fn allowed(
    dialect: &Sqlite,
    gate: Gate,
    form: Form,
    what: &str,
) -> Result<(), DialectError> {
    if allows_under(gate, dialect.floor(), form) {
        return Ok(());
    }
    Err(rebuild(&format!(
        "{what} needs SQLite {}, and this project's floor is {}",
        form.arrival(),
        dialect.floor()
    )))
}

/// `main."name"`: a table, index or view in the one schema this engine holds.
pub(crate) fn qualified(name: &TableName) -> Result<String, DialectError> {
    if name.schema != SCHEMA {
        return Err(unsupported(format!(
            "a schema other than `{SCHEMA}` (`{}`); declare it as `{SCHEMA}.<name>`",
            name.shown()
        )));
    }
    Ok(["main.", &local(&name.name)?].concat())
}

/// A name as SQLite reads it back as the same name.
pub(crate) fn local(name: &str) -> Result<String, DialectError> {
    ident::quote(name)
}

/// A statement, terminated.
pub(crate) fn statement(sql: String) -> Statement {
    Statement::new(sql + ";")
}

/// The text with its edges trimmed of SQLite white space, and a line feed added
/// when it ends in a `--` comment, so that what the emitter writes after it is
/// not read as part of the comment.
pub(crate) fn closed(text: &str) -> String {
    let text = text.trim_matches(lexis::is_space);
    match lexis::pieces(text).last() {
        Some(lexis::Piece::Comment(comment)) if comment.starts_with("--") => [text, "\n"].concat(),
        _ => text.to_owned(),
    }
}

/// The names `ADD`, `DROP` and `RENAME COLUMN` refuse on any table (3.54.0).
pub(crate) fn is_rowid_name(name: &str) -> bool {
    ["rowid", "_rowid_", "oid"]
        .iter()
        .any(|special| special.eq_ignore_ascii_case(name))
}
