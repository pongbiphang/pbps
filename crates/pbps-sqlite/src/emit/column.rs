//! The column family (#2950).

use pbps_dialect::Created;
use pbps_model::{Column, ColumnChange, ColumnName, TableName, ValueSource};

use super::{
    Form, Gate, Sql, allows_under, closed, is_rowid_name, later, local, qualified, rebuild,
    statement,
};
use crate::dialect::Sqlite;
use crate::types;
use crate::validate::{self, not_built, unsupported};

pub(super) fn emit(dialect: &Sqlite, gate: Gate, change: &ColumnChange) -> Sql {
    match change {
        ColumnChange::AddColumn {
            table,
            name,
            column,
            ..
        } => add(table, name, column),
        ColumnChange::RenameColumn {
            table, from, to, ..
        } => rename(dialect, gate, table, from, to),
        ColumnChange::DropColumn { .. } => {
            Err(later("dropping a column (ALTER TABLE .. DROP COLUMN)"))
        }
        ColumnChange::AlterColumnNullability { .. } => Err(later(
            "making a column NOT NULL or nullable (ALTER TABLE .. ALTER COLUMN)",
        )),
        ColumnChange::AlterColumnType { .. } => Err(rebuild("changing a column's type")),
        ColumnChange::AlterColumnDefault { .. } => Err(rebuild("changing a column's default")),
        ColumnChange::AlterColumnExpression { .. }
        | ColumnChange::AddComputedColumn { .. }
        | ColumnChange::DropComputedColumn { .. } => {
            Err(not_built("generated and computed columns"))
        }
        ColumnChange::AlterColumnNumbering { .. } => Err(unsupported(
            "identity columns and columns owning a sequence; declare an `integer` primary key, which numbers itself",
        )),
        // Declaration metadata: nothing in the database records it.
        ColumnChange::SetColumnDeprecated { .. } => Ok(Vec::new()),
    }
}

/// One column as `CREATE TABLE` and `ADD COLUMN` write it: name, type, `NOT
/// NULL`, `DEFAULT`. The type is the normalized one, and the default is the one
/// form [`validate::default_value`] admits; `TRUE` and `FALSE` are text that
/// stays in the file, so they need the floor to read them as constants.
pub(super) fn definition(
    name: &ColumnName,
    column: &Column,
) -> Result<String, pbps_dialect::DialectError> {
    let ty = types::normalize(&column.ty)?;
    let mut sql = [local(name.raw())?, " ".to_owned(), ty.raw().to_string()].concat();
    if !column.nullable {
        sql.push_str(" NOT NULL");
    }
    if let Some(text) = declared_default(column)? {
        if !validate::default_value(&text) {
            return Err(validate::invalid(format!(
                "the default of `{}` is not one value SQLite reads whole",
                name.shown()
            )));
        }
        sql.push_str(" DEFAULT ");
        sql.push_str(&text);
    }
    Ok(sql)
}

/// The column's default as written, and the refusals for the sources SQLite has
/// no in-place form of: validation refuses them too, and a saved plan can still
/// carry one.
fn declared_default(column: &Column) -> Result<Option<String>, pbps_dialect::DialectError> {
    match &column.value_source {
        ValueSource::None => Ok(None),
        ValueSource::Default(text) => Ok(Some(closed(text))),
        ValueSource::Identity(_) | ValueSource::Sequence(_) => Err(unsupported(
            "identity columns and columns owning a sequence; declare an `integer` primary key, which numbers itself",
        )),
        ValueSource::Generated(_) => Err(not_built("generated and computed columns")),
    }
}

/// `ADD COLUMN`, written only when SQLite takes it without looking at the rows:
/// measured, it refuses a `NOT NULL` column without a non-NULL default (and takes
/// one only on an empty table, which a plan cannot know), a default that is
/// `CURRENT_TIMESTAMP` or any expression, and the names `rowid` and `oid`. Those
/// are the rebuild's, so no statement reaches apply to be refused there.
fn add(table: &TableName, name: &ColumnName, column: &Column) -> Sql {
    let at = qualified(table)?;
    let mut found = Vec::new();
    validate::column_errors(&table.shown().to_string(), name, column, &mut found);
    if let Some(error) = found.into_iter().next() {
        return Err(error);
    }
    let shown = format!("{}.{}", table.shown(), name.shown());
    if is_rowid_name(name.raw()) {
        return Err(rebuild(&format!(
            "adding the column `{shown}`: SQLite refuses to add a column named rowid, _rowid_ or oid"
        )));
    }
    let default = declared_default(column)?;
    if let Some(text) = &default
        && !validate::default_is_constant(text)
    {
        return Err(rebuild(&format!(
            "adding the column `{shown}` with the default `{text}`: ADD COLUMN refuses CURRENT_TIME, CURRENT_DATE, CURRENT_TIMESTAMP and every expression as a default"
        )));
    }
    let valued = default
        .as_deref()
        .is_some_and(|text| !text.eq_ignore_ascii_case("null"));
    if !column.nullable && !valued {
        return Err(not_built(format!(
            "adding the NOT NULL column `{shown}` without a default: SQLite refuses it unless the table is empty, which a plan cannot know. Give the column a default, or wait for the table rebuild (item 8 of #2950)"
        )));
    }
    let sql = [
        "ALTER TABLE ",
        &at,
        " ADD COLUMN ",
        &definition(name, column)?,
    ]
    .concat();
    Ok(vec![
        statement(sql).creating(Created::Column(table.clone(), name.clone())),
    ])
}

fn rename(
    dialect: &Sqlite,
    gate: Gate,
    table: &TableName,
    from: &ColumnName,
    to: &ColumnName,
) -> Sql {
    let at = qualified(table)?;
    if !allows_under(gate, dialect.floor(), Form::RenameColumn) {
        return Err(rebuild(&format!(
            "renaming a column needs SQLite {}, and this project's floor is {}",
            Form::RenameColumn.arrival(),
            dialect.floor()
        )));
    }
    if is_rowid_name(from.raw()) || is_rowid_name(to.raw()) {
        return Err(rebuild(
            "renaming a column to or from rowid, _rowid_ or oid, which SQLite refuses",
        ));
    }
    let sql = [
        "ALTER TABLE ",
        &at,
        " RENAME COLUMN ",
        &local(from.raw())?,
        " TO ",
        &local(to.raw())?,
    ]
    .concat();
    Ok(vec![statement(sql)])
}
