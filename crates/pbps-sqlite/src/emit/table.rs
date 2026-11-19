//! The table family: create, drop, rename (#2950).

use pbps_dialect::Created;
use pbps_model::{
    CheckConstraint, ForeignKey, ObjectLocalName, PrimaryKey, ReferentialAction, Table,
    TableChange, TableName, Uid, UniqueConstraint,
};

use super::{Sql, closed, column, index, local, qualified, statement};
use crate::validate::{self, SCHEMA, unsupported};
use crate::{ident, names};

pub(super) fn emit(change: &TableChange) -> Sql {
    match change {
        TableChange::CreateTable {
            name,
            table,
            beside_default,
            ..
        } => {
            // `moves_at_most` is ignored without a `beside_default`, so it is
            // not read here either.
            if beside_default.is_some() {
                return Err(unsupported("partitions"));
            }
            create(name, table)
        }
        TableChange::DropTable {
            name, detach_from, ..
        } => {
            if detach_from.is_some() {
                return Err(unsupported("partitions"));
            }
            Ok(vec![statement(["DROP TABLE ", &qualified(name)?].concat())])
        }
        TableChange::RenameTable {
            uid,
            from,
            to,
            defaults,
        } => rename(uid, from, to, defaults),
    }
}

/// `CREATE TABLE` with every constraint inline, then one `CREATE INDEX` per
/// index. SQLite cannot add a key or a foreign key to a table in place, so the
/// constraints are part of the statement that makes the table, written as named
/// table constraints: a constraint's name exists only in the stored text, and a
/// table-level clause is what `DROP CONSTRAINT` can find later.
fn create(name: &TableName, table: &Table) -> Sql {
    // The declaration's own refusals, repeated: a saved plan can carry a table
    // validation never saw.
    if let Some(error) = validate::table(name, table).into_iter().next() {
        return Err(error);
    }
    let mut parts = Vec::new();
    for (column_name, def) in &table.columns {
        parts.push(column::definition(column_name, def)?);
    }
    if let Some(key) = &table.primary_key {
        parts.push(primary_key(key)?);
    }
    for (constraint, unique) in &table.unique {
        parts.push(unique_clause(constraint, unique)?);
    }
    for (constraint, check) in &table.checks {
        parts.push(check_clause(constraint, check)?);
    }
    for (constraint, key) in &table.foreign_keys {
        parts.push(foreign_key_clause(constraint, key)?);
    }
    let sql = [
        "CREATE TABLE ",
        &qualified(name)?,
        " (\n    ",
        &parts.join(",\n    "),
        "\n)",
    ]
    .concat();
    let mut first = statement(sql);
    first.creates.push(Created::Table(name.clone()));
    let mut out = vec![first];
    for (index_name, def) in &table.indexes {
        out.push(index::create(name, index_name, def)?);
    }
    Ok(out)
}

fn names_list(columns: &[pbps_model::ColumnName]) -> Result<String, pbps_dialect::DialectError> {
    let quoted: Result<Vec<_>, _> = columns.iter().map(|c| local(c.raw())).collect();
    Ok(["(", &quoted?.join(", "), ")"].concat())
}

fn named(constraint: Option<&ObjectLocalName>) -> Result<String, pbps_dialect::DialectError> {
    Ok(match constraint {
        Some(constraint) => ["CONSTRAINT ", &local(constraint.raw())?, " "].concat(),
        None => String::new(),
    })
}

fn primary_key(key: &PrimaryKey) -> Result<String, pbps_dialect::DialectError> {
    Ok([
        &named(key.name.as_ref())?,
        "PRIMARY KEY ",
        &names_list(&key.columns)?,
    ]
    .concat())
}

fn unique_clause(
    constraint: &ObjectLocalName,
    unique: &UniqueConstraint,
) -> Result<String, pbps_dialect::DialectError> {
    Ok([
        &named(Some(constraint))?,
        "UNIQUE ",
        &names_list(&unique.columns)?,
    ]
    .concat())
}

fn check_clause(
    constraint: &ObjectLocalName,
    check: &CheckConstraint,
) -> Result<String, pbps_dialect::DialectError> {
    Ok([
        &named(Some(constraint))?,
        "CHECK (",
        &closed(&check.expression),
        ")",
    ]
    .concat())
}

fn action(action: ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

/// The parent is written bare: a qualified parent is a syntax error.
fn foreign_key_clause(
    constraint: &ObjectLocalName,
    key: &ForeignKey,
) -> Result<String, pbps_dialect::DialectError> {
    if key.references_table.schema != SCHEMA {
        return Err(unsupported(format!(
            "a schema other than `{SCHEMA}` (a foreign key references `{}`)",
            key.references_table.shown()
        )));
    }
    Ok([
        &named(Some(constraint))?,
        "FOREIGN KEY ",
        &names_list(&key.columns)?,
        " REFERENCES ",
        &local(&key.references_table.name)?,
        " ",
        &names_list(&key.references_columns)?,
        " ON DELETE ",
        action(key.on_delete),
        " ON UPDATE ",
        action(key.on_update),
    ]
    .concat())
}

/// The name a table holds between the two statements of a case-only rename.
/// The identity makes it the table's own; it starts `__pbps_`, which is not a
/// ledger name and which no declaration is refused for, and SQLite refuses the
/// first statement, atomically, if any table, view or index already holds it.
pub(super) fn intermediate(uid: &Uid, schema: &str) -> TableName {
    TableName::new(schema, ["__pbps_rename_", uid.as_str()].concat())
}

/// A rename. The new name is written bare (`RENAME TO main.u` is a syntax
/// error). Two names that differ only in ASCII case are one name to SQLite,
/// which refuses the direct rename as an existing table, so such a rename is two
/// statements through a name in between, and the statements record it, so the
/// plan sees a name that is in neither the baseline nor the declaration.
fn rename(uid: &Uid, from: &TableName, to: &TableName, defaults: &[pbps_model::ColumnName]) -> Sql {
    if !defaults.is_empty() {
        return Err(unsupported(
            "renaming the generated default constraints of a table, which are SQL Server's",
        ));
    }
    if names::is_ledger(&to.name) || ident::has_reserved_prefix(&to.name) {
        return Err(unsupported(format!(
            "naming a table `{}`: the name is SQLite's or the ledger's own",
            to.shown()
        )));
    }
    // Both ends in `main`.
    let old = qualified(from)?;
    qualified(to)?;
    let step = |at: &str, name: &TableName| -> Result<String, pbps_dialect::DialectError> {
        Ok(["ALTER TABLE ", at, " RENAME TO ", &local(&name.name)?].concat())
    };
    if ident::same(&from.name, &to.name) && from.name != to.name {
        let between = intermediate(uid, &from.schema);
        let first = statement(step(&old, &between)?).renaming(from.clone(), between.clone());
        let second = statement(step(&qualified(&between)?, to)?).renaming(between, to.clone());
        return Ok(vec![first, second]);
    }
    Ok(vec![
        statement(step(&old, to)?).renaming(from.clone(), to.clone()),
    ])
}
