//! Bringing a table's constraints forward through a plan's renames.
//!
//! A constraint names its columns, and a foreign key names another table and
//! that table's columns. All of those are spelled the old way on the side that
//! was read before the renames ran, and the new way in the declarations — so
//! any comparison between the two sides has to bring one of them forward
//! first, or a rename looks like a change to every constraint standing around
//! it.
//!
//! Measured on SQL Server 2025: `sp_rename` on a column carries the primary
//! key (including one a foreign key references), the unique constraints, and
//! an index's key and `INCLUDE` columns; `sp_rename` on a table carries the
//! `references_table` of every child's foreign key. Those are the parts
//! [`Renames::apply`] rewrites, and they are the only ones it may.

use crate::name::{ColumnRef, TableName};
use crate::schema::Table;
use std::borrow::Cow;
use std::collections::BTreeMap;

/// The new name a plan gives each renamed table and column, keyed by the old.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renames {
    tables: BTreeMap<TableName, TableName>,
    columns: BTreeMap<ColumnRef, String>,
}

impl Renames {
    /// A table renamed by this plan. Both names are the whole qualified name.
    pub fn rename_table(&mut self, from: TableName, to: TableName) {
        self.tables.insert(from, to);
    }

    /// A column renamed by this plan, under the table's **pre-rename** name —
    /// which is how every base-side constraint spells it.
    pub fn rename_column(&mut self, from: ColumnRef, to: impl Into<String>) {
        self.columns.insert(from, to.into());
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.columns.is_empty()
    }

    /// The name this plan leaves the table under, or the one it was given.
    pub fn table(&self, t: &TableName) -> TableName {
        self.tables.get(t).unwrap_or(t).clone()
    }

    /// The name this plan leaves the column under.
    pub fn column(&self, table: &TableName, name: &str) -> String {
        self.columns
            .get(&ColumnRef::new(table.clone(), name))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    fn column_list(&self, table: &TableName, names: &[String]) -> Vec<String> {
        names.iter().map(|n| self.column(table, n)).collect()
    }

    /// The table with every constraint the engine carries through a rename
    /// spelled as this plan leaves it.
    ///
    /// A check constraint's expression and a filtered index's predicate are
    /// **not** rewritten. They are opaque text this tool never parses, and the
    /// engine refuses `sp_rename` on a column either of them names at all
    /// (15336 for the check, 5074 with 4922 behind it for the index,
    /// measured), so the drop and re-add such a rename forces is the only way
    /// it can happen — not a spelling to be reconciled away.
    ///
    /// `name` is the table's **pre-rename** name, because that is what the
    /// column map is keyed by.
    pub fn apply<'a>(&self, table: &'a Table, name: &TableName) -> Cow<'a, Table> {
        if self.is_empty() {
            return Cow::Borrowed(table);
        }
        let mut t = table.clone();
        if let Some(pk) = t.primary_key.as_mut() {
            pk.columns = self.column_list(name, &pk.columns);
        }
        for u in t.unique.values_mut() {
            u.columns = self.column_list(name, &u.columns);
        }
        for ix in t.indexes.values_mut() {
            for c in &mut ix.columns {
                c.name = self.column(name, &c.name);
            }
            ix.include = self.column_list(name, &ix.include);
        }
        for f in t.foreign_keys.values_mut() {
            f.columns = self.column_list(name, &f.columns);
            // The referenced columns belong to the referenced table, so they
            // are looked up under *its* pre-rename name — the map is keyed by
            // the old spelling — before the table itself is brought forward.
            f.references_columns = self.column_list(&f.references_table, &f.references_columns);
            f.references_table = self.table(&f.references_table);
        }
        Cow::Owned(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ForeignKey, Index, IndexColumn, PrimaryKey, UniqueConstraint};

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn sample() -> Table {
        let mut table = Table {
            primary_key: Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
            }),
            ..Default::default()
        };
        table.unique.insert(
            "uq".into(),
            UniqueConstraint {
                columns: vec!["tag".into()],
            },
        );
        table.indexes.insert(
            "ix".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "tag".into(),
                    descending: false,
                }],
                include: vec!["note".into()],
                unique: false,
                filter: Some("[tag] IS NOT NULL".into()),
            },
        );
        table.foreign_keys.insert(
            "fk".into(),
            ForeignKey {
                columns: vec!["pid".into()],
                references_table: t("dbo.parent"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        table
    }

    #[test]
    fn an_empty_map_borrows_the_table_it_was_given() {
        let table = sample();
        let out = Renames::default().apply(&table, &t("dbo.t"));
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn every_carried_part_is_spelled_as_the_plan_leaves_it() {
        let mut r = Renames::default();
        r.rename_column(ColumnRef::new(t("dbo.t"), "id"), "row_id");
        r.rename_column(ColumnRef::new(t("dbo.t"), "tag"), "label");
        r.rename_column(ColumnRef::new(t("dbo.t"), "note"), "remark");
        r.rename_column(ColumnRef::new(t("dbo.parent"), "id"), "parent_id");
        r.rename_table(t("dbo.parent"), t("dbo.ancestor"));

        let table = sample();
        let out = r.apply(&table, &t("dbo.t"));
        assert_eq!(out.primary_key.as_ref().unwrap().columns, ["row_id"]);
        assert_eq!(out.unique["uq"].columns, ["label"]);
        assert_eq!(out.indexes["ix"].columns[0].name, "label");
        assert_eq!(out.indexes["ix"].include, ["remark"]);
        assert_eq!(out.foreign_keys["fk"].references_table, t("dbo.ancestor"));
        assert_eq!(out.foreign_keys["fk"].references_columns, ["parent_id"]);
    }

    /// The negative case, and the one that matters: a filtered index's
    /// predicate is left exactly as it was, because the engine will not
    /// perform a rename it names and the drop-and-add is therefore real.
    #[test]
    fn a_filter_predicate_is_never_rewritten() {
        let mut r = Renames::default();
        r.rename_column(ColumnRef::new(t("dbo.t"), "tag"), "label");
        let table = sample();
        let out = r.apply(&table, &t("dbo.t"));
        assert_eq!(
            out.indexes["ix"].filter.as_deref(),
            Some("[tag] IS NOT NULL"),
            "the predicate is opaque text, and a rename it names is refused"
        );
    }

    /// A column of another table with the same name is not touched: the map is
    /// keyed by the whole [`ColumnRef`], not by the bare name.
    #[test]
    fn a_rename_on_one_table_does_not_reach_another() {
        let mut r = Renames::default();
        r.rename_column(ColumnRef::new(t("dbo.other"), "tag"), "label");
        let table = sample();
        let out = r.apply(&table, &t("dbo.t"));
        assert_eq!(out.unique["uq"].columns, ["tag"]);
    }
}
