//! The column family (#2950).

use pbps_dialect::Created;
use pbps_model::{Column, ColumnChange, ColumnName, TableName, ValueSource};

use super::{
    Form, Gate, Sql, allowed, closed, is_rowid_name, local, qualified, rebuild, statement,
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
        ColumnChange::DropColumn { column, .. } => drop(dialect, gate, column),
        ColumnChange::AlterColumnNullability {
            column,
            to_nullable,
            through_external_partitions,
            ..
        } => nullability(
            dialect,
            gate,
            column,
            *to_nullable,
            *through_external_partitions,
        ),
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
/// NULL`, `DEFAULT`. The type is the normalized one (and for the reserved
/// [`types::UNTYPED`] nothing: that is the one place the word is turned into no
/// type, and the only place a type's text enters SQL), and the default is the one
/// form [`validate::default_value`] admits; `TRUE` and `FALSE` are text that
/// stays in the file, so they need the floor to read them as constants.
pub(super) fn definition(
    name: &ColumnName,
    column: &Column,
) -> Result<String, pbps_dialect::DialectError> {
    let ty = types::normalize(&column.ty)?;
    let mut sql = local(name.raw())?;
    // The reserved word stands for no type: written, it would be a NUMERIC
    // column.
    if !types::is_untyped(&ty) {
        sql.push(' ');
        sql.push_str(&ty.raw().to_string());
    }
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
    let constant = default.as_deref().map(validate::default_constant);
    if let (Some(text), Some(None)) = (&default, &constant) {
        return Err(rebuild(&format!(
            "adding the column `{shown}` with the default `{text}`: ADD COLUMN refuses CURRENT_TIME, CURRENT_DATE, CURRENT_TIMESTAMP and every expression as a default"
        )));
    }
    let valued = constant
        .as_ref()
        .and_then(Option::as_ref)
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
    allowed(dialect, gate, Form::RenameColumn, "renaming a column")?;
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

/// `ALTER TABLE .. DROP COLUMN`. SQLite refuses it for a column in an index, a
/// key, a table check, a foreign key, a view, a trigger or a generated column,
/// and for the last column of a table (measured), and the statement cannot see
/// any of that. The order does for the dependents: the shared effects require
/// each gone first and the views torn down around the drop (`ordering`), and the
/// keys and unique constraints, which SQLite cannot drop in place, are the
/// rebuild's own refusals. The last column is the one refusal left to the
/// engine (see the test that pins it).
fn drop(dialect: &Sqlite, gate: Gate, column: &pbps_model::ColumnRef) -> Sql {
    allowed(dialect, gate, Form::DropColumn, "dropping a column")?;
    let sql = [
        "ALTER TABLE ",
        &qualified(&column.table)?,
        " DROP COLUMN ",
        &local(column.name.raw())?,
    ]
    .concat();
    Ok(vec![statement(sql)])
}

/// `ALTER COLUMN .. SET NOT NULL` or `DROP NOT NULL`. `SET` scans the table and
/// is refused, with the table unchanged, when a row holds NULL: the engine's
/// refusal at apply, inside the plan's transaction, which the shared risk class
/// `NotNull` already puts in front of a reviewer. No other dependent blocks
/// either form (measured: indexes, views, checks, keys, either side of a
/// foreign key).
fn nullability(
    dialect: &Sqlite,
    gate: Gate,
    column: &pbps_model::ColumnRef,
    to_nullable: bool,
    through_external_partitions: bool,
) -> Sql {
    if through_external_partitions {
        return Err(unsupported("partitions"));
    }
    let (form, tail, what) = if to_nullable {
        (
            Form::DropNotNull,
            " DROP NOT NULL",
            "making a column nullable",
        )
    } else {
        (
            Form::SetNotNull,
            " SET NOT NULL",
            "making a column NOT NULL",
        )
    };
    allowed(dialect, gate, form, what)?;
    let sql = [
        "ALTER TABLE ",
        &qualified(&column.table)?,
        " ALTER COLUMN ",
        &local(column.name.raw())?,
        tail,
    ]
    .concat();
    Ok(vec![statement(sql)])
}
