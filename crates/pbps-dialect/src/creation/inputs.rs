//! One column-input query for creation extraction and its ordering edges.
use crate::Dialect;
use pbps_model::{Change, ColumnName, Table, TableName};
use std::collections::BTreeSet;

pub fn reads<'a>(
    texts: impl IntoIterator<Item = &'a str>,
    table: &TableName,
    full: &Table,
    dialect: &(impl Dialect + ?Sized),
) -> Option<BTreeSet<ColumnName>> {
    let mut columns = BTreeSet::new();
    for text in texts {
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

/// None retains a whole-shape boundary; named inputs need only their columns.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn columns(
    change: &Change,
    table: &TableName,
    full: &Table,
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
    let texts: Vec<&str> = match change {
        Change::AlterColumnDefault { to, .. } => to.as_deref().into_iter().collect(),
        Change::SetPartitionDefault { to, fallback, .. } => {
            to.as_deref().or(fallback.as_deref()).into_iter().collect()
        }
        Change::AlterColumnExpression { to, .. } => vec![to],
        Change::AddCheck { constraint, .. } => vec![&constraint.expression],
        Change::AddIndex { index, .. } => index
            .filter
            .as_deref()
            .into_iter()
            .chain(index.columns.iter().filter_map(|c| c.key.expression()))
            .collect(),
        _ => Vec::new(),
    };
    columns.extend(reads(texts, table, full, dialect)?);
    Some(columns)
}
