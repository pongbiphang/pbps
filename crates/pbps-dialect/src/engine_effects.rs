//! The parts of a step's engine effects that dialects share (#1954,
//! DEC-1954.3). Pure: they read the step and the baseline model.

use pbps_model::operation::{
    Effects, Operation, OwnerError, Owners, PartKey, Requirement, Snapshot, base_table,
};
use pbps_model::{Change, ColumnRef, IndexKey};

use crate::{Dialect, RetypeDependents};

/// The column a step drops, or renames away, with its table. A retype's
/// dependents are [`retype_blockers`]', by the dialect's policy.
#[allow(clippy::wildcard_enum_match_arm)]
fn column_of(change: &Change, renames_block: bool) -> Option<ColumnRef> {
    match change {
        Change::DropColumn { column, .. } => Some(column.clone()),
        Change::RenameColumn { table, from, .. } if renames_block => Some(table.column(from)),
        _ => None,
    }
}

/// Checks and filtered indexes whose text may name the column the step
/// drops (and, where `renames_block`, renames away): they stand in its way
/// until the plan drops them. A lexical over-approximation, by the dialect's
/// own lexer (DEC-174, DEC-1174.1).
pub fn text_dependents(
    dialect: &(impl Dialect + ?Sized),
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    renames_block: bool,
) -> Result<Effects, OwnerError> {
    let mut fx = Effects::default();
    let Some(column) = column_of(op.change(), renames_block) else {
        return Ok(fx);
    };
    let Some(table) = base_table(op, owners, before, &column.table)? else {
        return Ok(fx);
    };
    // A dependent the plan replaces under its own name with text that no
    // longer names the column does not stand in the way: its baseline
    // incarnation goes, and the requirement cannot tell it from the new one.
    let replaced = pbps_model::operation::final_table(after, &table);
    for (name, check) in &table.table.checks {
        if replaced
            .and_then(|t| t.checks.get(name))
            .is_some_and(|c| !dialect.may_name(&c.expression, column.name.raw()))
        {
            continue;
        }
        if dialect.may_name(&check.expression, column.name.raw()) {
            fx.requires.push(Requirement::Absent(
                table.part(PartKey::Check(name.clone())),
            ));
        }
    }
    for (name, index) in &table.table.indexes {
        let filtered = index
            .filter
            .as_deref()
            .is_some_and(|f| dialect.may_name(f, column.name.raw()));
        let expression = index.columns.iter().any(|c| match &c.key {
            IndexKey::Expression(text) => dialect.may_name(text, column.name.raw()),
            IndexKey::Column(_) => false,
        });
        let names = |index: &pbps_model::Index| {
            index
                .filter
                .as_deref()
                .is_some_and(|f| dialect.may_name(f, column.name.raw()))
                || index.columns.iter().any(|c| match &c.key {
                    IndexKey::Expression(text) => dialect.may_name(text, column.name.raw()),
                    IndexKey::Column(_) => false,
                })
        };
        if replaced
            .and_then(|t| t.indexes.get(name))
            .is_some_and(|i| !names(i))
        {
            continue;
        }
        if filtered || expression {
            fx.requires.push(Requirement::Absent(
                table.part(PartKey::Index(name.clone())),
            ));
        }
    }
    Ok(fx)
}

/// What an `ALTER COLUMN` that retypes, recollates, tightens or relaxes a
/// column requires gone first, by the dialect's own answer
/// ([`RetypeDependents::of_retype`], `Dialect::nullability_dependents`): the
/// one the differ rebuilds the dependents by (`recreate_retyped_dependents`),
/// so the effects require exactly what the plan drops (DEC-1998.1).
///
/// - keys and indexes: the primary key, a unique constraint or an index with
///   the column in its key or its `INCLUDE`;
/// - checks, filtered and expression indexes: those whose text may name the
///   column, by the lexer;
/// - foreign keys: one over the column, and one from any table that
///   references it.
///
/// A dependent the plan replaces under its own name with text that no longer
/// names the column is skipped, as in [`text_dependents`].
#[allow(clippy::wildcard_enum_match_arm)]
pub fn retype_blockers(
    dialect: &(impl Dialect + ?Sized),
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
) -> Result<Effects, OwnerError> {
    let mut fx = Effects::default();
    let (uid, column, dependents) = match op.change() {
        Change::AlterColumnType {
            uid,
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            from_collation,
            to_collation,
            ..
        } => (
            uid,
            column,
            RetypeDependents::of_retype(
                dialect,
                (from, to),
                from_collation != to_collation,
                (from_nullable != to_nullable).then_some(*to_nullable),
            ),
        ),
        Change::AlterColumnNullability {
            uid,
            column,
            to_nullable,
            ..
        } => (uid, column, dialect.nullability_dependents(*to_nullable)),
        _ => return Ok(fx),
    };
    let Some(table) = base_table(op, owners, before, &column.table)? else {
        return Ok(fx);
    };
    // The column as the baseline spells it: the step may name it after a
    // rename.
    let Some(name) = before.ids.columns.get(uid).map(|c| c.name.raw()) else {
        return Ok(fx);
    };
    let t = table.table;
    let replaced = pbps_model::operation::final_table(after, &table);
    let gone = local_column_blockers(dialect, t, name, dependents, replaced);
    for part in gone {
        fx.requires.push(Requirement::Absent(table.part(part)));
    }
    if dependents.foreign_keys {
        for other_name in before.schema.tables.keys() {
            let Some(other) = pbps_model::operation::base_table_named(owners, before, other_name)
            else {
                continue;
            };
            for (n, fk) in &other.table.foreign_keys {
                let local = other_name == table.name && fk.columns.iter().any(|c| c.raw() == name);
                let referenced = fk.references_table == *table.name
                    && fk.references_columns.iter().any(|c| c.raw() == name);
                if local || referenced {
                    fx.requires.push(Requirement::Absent(
                        other.part(PartKey::ForeignKey(n.clone())),
                    ));
                }
            }
        }
    }
    Ok(fx)
}

/// Local table parts that must be absent for an ALTER COLUMN. Creation uses
/// the same exact key/INCLUDE and lexical predicate matching as engine effects;
/// a final replacement may release only a text dependency (DEC-1998.1).
pub fn local_column_blockers(
    dialect: &(impl Dialect + ?Sized),
    t: &pbps_model::Table,
    name: &str,
    dependents: RetypeDependents,
    replaced: Option<&pbps_model::Table>,
) -> Vec<PartKey> {
    let mut gone: Vec<PartKey> = Vec::new();
    if dependents.keys_and_indexes {
        if t.primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.iter().any(|c| c.raw() == name))
        {
            gone.push(PartKey::PrimaryKey);
        }
        for (n, u) in &t.unique {
            if u.columns.iter().any(|c| c.raw() == name) {
                gone.push(PartKey::Unique(n.clone()));
            }
        }
        for (n, index) in &t.indexes {
            let keyed = index
                .columns
                .iter()
                .any(|c| matches!(&c.key, IndexKey::Column(k) if k.raw() == name));
            if keyed || index.include.iter().any(|c| c.raw() == name) {
                gone.push(PartKey::Index(n.clone()));
            }
        }
    }
    if dependents.checks {
        for (n, check) in &t.checks {
            let off = replaced
                .and_then(|r| r.checks.get(n))
                .is_some_and(|c| !dialect.may_name(&c.expression, name));
            if !off && dialect.may_name(&check.expression, name) {
                gone.push(PartKey::Check(n.clone()));
            }
        }
    }
    if dependents.filtered_indexes {
        let names = |index: &pbps_model::Index| {
            index
                .filter
                .as_deref()
                .is_some_and(|f| dialect.may_name(f, name))
                || index.columns.iter().any(|c| match &c.key {
                    IndexKey::Expression(text) => dialect.may_name(text, name),
                    IndexKey::Column(_) => false,
                })
        };
        for (n, index) in &t.indexes {
            let off = replaced
                .and_then(|r| r.indexes.get(n))
                .is_some_and(|i| !names(i));
            if !off && names(index) && !gone.contains(&PartKey::Index(n.clone())) {
                gone.push(PartKey::Index(n.clone()));
            }
        }
    }
    gone
}
