//! The parts of a step's engine effects that dialects share (#1954,
//! DEC-1954.3). Pure: they read the step and the baseline model.

use pbps_model::operation::{
    Effects, Operation, OwnerError, Owners, PartKey, Requirement, Snapshot, base_table,
};
use pbps_model::{Change, ColumnRef, IndexKey};

use crate::Dialect;

/// The column a step drops, or retypes, or renames away, with its table.
#[allow(clippy::wildcard_enum_match_arm)]
fn column_of(change: &Change, renames_block: bool) -> Option<ColumnRef> {
    match change {
        Change::DropColumn { column, .. } => Some(column.clone()),
        Change::AlterColumnType { column, .. } if renames_block => Some(column.clone()),
        Change::RenameColumn { table, from, .. } if renames_block => Some(table.column(from)),
        _ => None,
    }
}

/// Checks and filtered indexes whose text may name the column the step
/// drops (and, where `renames_block`, retypes or renames away): they stand in
/// its way until the plan drops them. A lexical over-approximation, by the
/// dialect's own lexer (DEC-174, DEC-1174.1).
pub fn text_dependents(
    dialect: &dyn Dialect,
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
            .is_some_and(|c| !dialect.may_name(&c.expression, &column.name))
        {
            continue;
        }
        if dialect.may_name(&check.expression, &column.name) {
            fx.requires.push(Requirement::Absent(
                table.part(PartKey::Check(name.clone())),
            ));
        }
    }
    for (name, index) in &table.table.indexes {
        let filtered = index
            .filter
            .as_deref()
            .is_some_and(|f| dialect.may_name(f, &column.name));
        let expression = index.columns.iter().any(|c| match &c.key {
            IndexKey::Expression(text) => dialect.may_name(text, &column.name),
            IndexKey::Column(_) => false,
        });
        let names = |index: &pbps_model::Index| {
            index
                .filter
                .as_deref()
                .is_some_and(|f| dialect.may_name(f, &column.name))
                || index.columns.iter().any(|c| match &c.key {
                    IndexKey::Expression(text) => dialect.may_name(text, &column.name),
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
