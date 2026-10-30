//! Local creation prerequisites share the engine recipe's may-read scan.
use super::*;
use pbps_model::ColumnName;

/// None retains a whole-shape boundary. Named constraints and local scalar
/// expressions instead require only the columns they name or may read.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn columns(
    change: &Change,
    table: &TableName,
    full: &pbps_model::Table,
    dialect: &(impl Dialect + ?Sized),
) -> Option<BTreeSet<ColumnName>> {
    let mut columns = BTreeSet::new();
    match change {
        Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnNullability { column, .. }
        | Change::AlterColumnExpression { column, .. } => {
            columns.insert(column.name.clone());
        }
        Change::SetPartitionDefault { column, .. } | Change::SetPartitionNotNull { column, .. } => {
            columns.insert(column.clone());
        }
        Change::SetPrimaryKey { to: Some(key), .. } => columns.extend(key.columns.iter().cloned()),
        Change::AddUnique { constraint, .. } => columns.extend(constraint.columns.iter().cloned()),
        Change::AddForeignKey {
            table: owner,
            constraint,
            ..
        } => {
            if owner == table {
                columns.extend(constraint.columns.iter().cloned());
            }
            if &constraint.references_table == table {
                columns.extend(constraint.references_columns.iter().cloned());
            }
        }
        Change::AddIndex { index, .. } => {
            columns.extend(index.columns.iter().filter_map(|c| c.key.column()).cloned());
            columns.extend(index.include.iter().cloned());
        }
        Change::AddCheck { .. } | Change::SetReplicaIdentity { .. } => {}
        _ => return None,
    }
    for text in expressions(change) {
        if dialect.generation_names_table(text, table)
            && !dialect.generation_table_name_only(text, table)
        {
            return None;
        }
        columns.extend(
            full.columns
                .keys()
                .chain(full.computed.keys())
                .filter(|column| dialect.generation_may_read_column(text, column))
                .cloned(),
        );
    }
    Some(columns)
}
