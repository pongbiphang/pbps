//! One column-input query for creation extraction and its ordering edges.
use crate::Dialect;
use pbps_model::ColumnChange;
use pbps_model::ConstraintChange;
use pbps_model::IndexChange;
use pbps_model::PartitionChange;
use pbps_model::TablePropertyChange;
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
        Change::Column(ColumnChange::AlterColumnDefault { column, .. })
        | Change::Column(ColumnChange::AlterColumnNullability { column, .. })
        | Change::Column(ColumnChange::AlterColumnExpression { column, .. }) => {
            columns.insert(column.name.clone());
        }
        Change::Partition(PartitionChange::SetPartitionDefault { column, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { column, .. }) => {
            columns.insert(column.clone());
        }
        Change::Constraint(ConstraintChange::SetPrimaryKey { to: Some(key), .. }) => {
            columns.extend(key.columns.iter().cloned())
        }
        Change::Constraint(ConstraintChange::AddUnique { constraint, .. }) => {
            columns.extend(constraint.columns.iter().cloned())
        }
        Change::Constraint(ConstraintChange::AddForeignKey {
            table: owner,
            constraint,
            ..
        }) => {
            if owner == table {
                columns.extend(constraint.columns.iter().cloned());
            }
            if &constraint.references_table == table {
                columns.extend(constraint.references_columns.iter().cloned());
            }
        }
        Change::Index(IndexChange::AddIndex { index, .. }) => {
            columns.extend(index.columns.iter().filter_map(|c| c.key.column()).cloned());
            columns.extend(index.include.iter().cloned());
        }
        Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => {}
        _ => return None,
    }
    let texts: Vec<&str> = match change {
        Change::Column(ColumnChange::AlterColumnDefault { to, .. }) => {
            to.as_deref().into_iter().collect()
        }
        Change::Partition(PartitionChange::SetPartitionDefault { to, fallback, .. }) => {
            to.as_deref().or(fallback.as_deref()).into_iter().collect()
        }
        Change::Column(ColumnChange::AlterColumnExpression { to, .. }) => vec![to],
        Change::Constraint(ConstraintChange::AddCheck { constraint, .. }) => {
            vec![&constraint.expression]
        }
        Change::Index(IndexChange::AddIndex { index, .. }) => index
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
