//! The constraint family (#2950).
//!
//! SQLite adds and drops, in place, only a check (by name) and drops a foreign
//! key (by name); a primary key and a unique constraint it cannot add (syntax
//! error) or drop ("constraint may not be dropped"), and it cannot add a foreign
//! key. Those are the table rebuild's, answered by name.
//!
//! `DROP CONSTRAINT` finds its constraint by name in the stored text, measured:
//! the name is compared with ASCII case folded, a quoted name (with a doubled
//! quote inside) finds the same name, a column-level `CONSTRAINT k` is found as
//! a table-level one is, and the first constraint of that name goes when two
//! share one. A constraint with no name, or a name the file does not hold, is
//! `no such constraint` at apply, inside the plan's transaction: the statement
//! cannot see the stored text, and a declaration names every constraint it
//! makes, so what is wrong is a baseline that is not the file's, which the
//! drift check and the apply's own guards own.

use pbps_model::{CheckConstraint, ConstraintChange, ObjectLocalName, TableName};

use super::{Form, Gate, Sql, allowed, closed, local, qualified, rebuild, statement};
use crate::dialect::Sqlite;
use crate::names;
use crate::validate::{self, unsupported};

pub(super) fn emit(dialect: &Sqlite, gate: Gate, change: &ConstraintChange) -> Sql {
    match change {
        ConstraintChange::AddCheck {
            table,
            name,
            constraint,
        } => add_check(dialect, gate, table, name, constraint),
        ConstraintChange::DropCheck { table, name } => {
            drop_constraint(dialect, gate, Form::DropCheck, table, name)
        }
        ConstraintChange::DropForeignKey { table, name } => {
            drop_constraint(dialect, gate, Form::DropForeignKey, table, name)
        }
        ConstraintChange::SetPrimaryKey { .. } => Err(rebuild("changing a table's primary key")),
        ConstraintChange::AddUnique { .. } | ConstraintChange::DropUnique { .. } => {
            Err(rebuild("adding or dropping a unique constraint"))
        }
        ConstraintChange::AddForeignKey { .. } => Err(rebuild("adding a foreign key")),
        ConstraintChange::SetExclusion { .. } => Err(unsupported("exclusion constraints")),
    }
}

/// `ADD CONSTRAINT "k" CHECK (..)`. SQLite checks every existing row and
/// refuses the statement (`constraint failed`, nothing changed) when one
/// violates it, and refuses a name any constraint of the table already holds,
/// whatever its kind and in any ASCII case.
fn add_check(
    dialect: &Sqlite,
    gate: Gate,
    table: &TableName,
    name: &ObjectLocalName,
    check: &CheckConstraint,
) -> Sql {
    allowed(dialect, gate, Form::AddCheck, "adding a check constraint")?;
    let mut found = Vec::new();
    validate::check_errors(&table.shown().to_string(), name, check, &mut found);
    if let Some(error) = found.into_iter().next() {
        return Err(error);
    }
    let sql = [
        "ALTER TABLE ",
        &qualified(table)?,
        " ADD CONSTRAINT ",
        &local(name.raw())?,
        " CHECK (",
        &closed(&check.expression),
        ")",
    ]
    .concat();
    Ok(vec![statement(sql)])
}

fn drop_constraint(
    dialect: &Sqlite,
    gate: Gate,
    form: Form,
    table: &TableName,
    name: &ObjectLocalName,
) -> Sql {
    allowed(dialect, gate, form, "dropping a constraint")?;
    // A name this tool made for an unnamed constraint is not one SQLite knows:
    // `DROP CONSTRAINT` would find nothing. Only the rebuild can change it.
    if names::is_synthetic(name.raw()) {
        return Err(rebuild(&format!(
            "dropping the constraint `{}`, which the file leaves unnamed and this tool named; \
             SQLite can drop a constraint only by a name it holds",
            name.shown()
        )));
    }
    let sql = [
        "ALTER TABLE ",
        &qualified(table)?,
        " DROP CONSTRAINT ",
        &local(name.raw())?,
    ]
    .concat();
    Ok(vec![statement(sql)])
}
