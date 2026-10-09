//! Which table is whose partition (#1699).
//!
//! A partition's column has no name of its own: its columns, types included,
//! are its parent's, and the engine recurses every change to a partitioned
//! parent's column into each partition. A `ChangeSet` names that change by the
//! parent alone, while a partition's own default, NOT NULL, index or check is
//! named by the partition. So a lookup keyed by `(table, column)` asks the
//! wrong key wherever the two meet, unless it maps the partition's column to
//! its parent's first.
//!
//! The plan does not carry the relation: each consumer derives it from the
//! schema it already holds — the declarations, or a read of the database —
//! through this one type, rather than re-deriving or guessing it locally
//! (DEC-1699.1).

use std::collections::BTreeMap;

use crate::{ColumnRef, Schema, TableName};

/// The partitions of a schema, by parent, and each partition's parent.
///
/// A tree is one level deep: a partitioned table that is itself a partition
/// is not one pbps holds (DEC-1170.1), so a parent is never a partition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partitions {
    parents: BTreeMap<TableName, TableName>,
    held: BTreeMap<TableName, Vec<TableName>>,
}

impl Partitions {
    /// The relation as `schema` states it.
    pub fn of(schema: &Schema) -> Self {
        let mut this = Self::default();
        for (name, table) in &schema.tables {
            if let Some(of) = &table.partition_of {
                this.parents.insert(name.clone(), of.parent.clone());
                this.held
                    .entry(of.parent.clone())
                    .or_default()
                    .push(name.clone());
            }
        }
        this
    }

    /// The parent `table` is a partition of.
    pub fn parent(&self, table: &TableName) -> Option<&TableName> {
        self.parents.get(table)
    }

    /// The partitions `parent` holds, in name order.
    pub fn of_parent(&self, parent: &TableName) -> &[TableName] {
        self.held.get(parent).map_or(&[], Vec::as_slice)
    }

    /// `table`, then every partition it holds: each table a change to
    /// `table`'s columns reaches.
    pub fn holding<'a>(&'a self, table: &'a TableName) -> impl Iterator<Item = &'a TableName> {
        std::iter::once(table).chain(self.of_parent(table))
    }

    /// The column `column` is: its parent's where `column.table` is a
    /// partition, and itself otherwise. Every lookup of a column by its table
    /// goes through this, so a change to the parent's column and a
    /// partition's own entry on it meet at one key.
    pub fn column(&self, column: &ColumnRef) -> ColumnRef {
        match self.parent(&column.table) {
            Some(parent) => ColumnRef::new(parent.clone(), column.name.clone()),
            None => column.clone(),
        }
    }

    /// The relation without `tables` as partitions: the ones a plan
    /// attaches, detaches or drops, which are a partition on one side of it
    /// and not the other. A parent's change reaches such a table only on one
    /// side of its own transition, so a consumer that reads the relation off
    /// one schema at plan time and another at apply would disagree about it
    /// (#1728 review).
    pub fn without<'a>(mut self, tables: impl IntoIterator<Item = &'a TableName>) -> Self {
        for table in tables {
            if let Some(parent) = self.parents.remove(table)
                && let Some(held) = self.held.get_mut(&parent)
            {
                held.retain(|p| p != table);
            }
        }
        self
    }

    /// Whether this schema has no partition at all.
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PartitionBound, PartitionOf, Table};

    fn name(n: &str) -> TableName {
        n.parse().unwrap()
    }

    fn partition(parent: &str) -> Table {
        Table {
            partition_of: Some(PartitionOf {
                parent: name(parent),
                bound: PartitionBound::Default,
                columns: BTreeMap::new(),
            }),
            ..Table::default()
        }
    }

    fn schema() -> Schema {
        let mut s = Schema::default();
        s.tables.insert(name("app.p"), Table::default());
        s.tables.insert(name("app.p_b"), partition("app.p"));
        s.tables.insert(name("app.p_a"), partition("app.p"));
        s.tables.insert(name("app.t"), Table::default());
        s
    }

    #[test]
    fn a_partitions_column_is_its_parents() {
        let p = Partitions::of(&schema());
        assert_eq!(
            p.column(&ColumnRef::new(name("app.p_a"), "v")),
            ColumnRef::new(name("app.p"), "v")
        );
        assert_eq!(p.parent(&name("app.p_b")), Some(&name("app.p")));
        // Negative: a parent's and a plain table's columns are their own.
        for own in ["app.p", "app.t"] {
            let c = ColumnRef::new(name(own), "v");
            assert_eq!(p.column(&c), c);
            assert_eq!(p.parent(&name(own)), None);
        }
    }

    #[test]
    fn a_change_to_a_parents_column_reaches_each_of_its_partitions() {
        let p = Partitions::of(&schema());
        let parent = name("app.p");
        let held: Vec<&TableName> = p.holding(&parent).collect();
        assert_eq!(held, [&name("app.p"), &name("app.p_a"), &name("app.p_b")]);
        // Negative: a plain table, and a partition, reach only themselves.
        for alone in ["app.t", "app.p_a"] {
            let n = name(alone);
            assert_eq!(p.holding(&n).collect::<Vec<_>>(), [&n]);
        }
        assert!(Partitions::of(&Schema::default()).is_empty());
        assert!(!p.is_empty());
    }
}
