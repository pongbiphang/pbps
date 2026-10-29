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

use crate::plan_names::{At, Holder, PlanNames};
use crate::{ColumnRef, Schema, Table, TableName, TypeRef};

/// The declaration chain from `table` through its root, inclusive.
///
/// Like serial grant resolution (DEC-2202.1), this can run before partition
/// validation: a missing ancestor or a cycle supplies no column ownership facts.
pub fn ancestor_chain<'a>(
    table: &TableName,
    tables: &'a BTreeMap<TableName, Table>,
) -> Option<Vec<(&'a TableName, &'a Table)>> {
    let mut next = table;
    let mut chain = Vec::new();
    for _ in 0..tables.len() {
        let (name, table) = tables.get_key_value(next)?;
        chain.push((name, table));
        if let Some(of) = &table.partition_of {
            next = &of.parent;
        } else {
            return Some(chain);
        }
    }
    None
}

/// The direct partition links of a schema, by parent and child.
/// Column ownership at arbitrary depth uses `ancestor_chain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partitions {
    parents: BTreeMap<TableName, TableName>,
    held: BTreeMap<TableName, Vec<TableName>>,
    /// Each partitioned parent's key columns, by the user-defined type a
    /// scalar column names, as the schema spells it (DEC-2346.1). A bound's
    /// labels are the labels of these types.
    keys: BTreeMap<TableName, Vec<Option<TypeRef>>>,
}

impl Partitions {
    /// The relation as `schema` states it.
    pub fn of(schema: &Schema) -> Self {
        let mut this = Self::default();
        for (name, table) in &schema.tables {
            if let Some(by) = &table.partition_by {
                this.keys.insert(
                    name.clone(),
                    by.columns
                        .iter()
                        .map(|column| {
                            table
                                .columns
                                .get(column)
                                .filter(|c| !c.ty.is_array())
                                .and_then(|c| c.ty.type_ref().cloned())
                        })
                        .collect(),
                );
            }
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

    /// The user-defined type of each key column of the partitioned `parent`,
    /// in key order, where the column is a scalar reference to one.
    pub fn key_types(&self, parent: &TableName) -> &[Option<TypeRef>] {
        self.keys.get(parent).map_or(&[], Vec::as_slice)
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

    /// Bring a baseline relation to final parent names by identity. Partition
    /// addresses stay as read: existing consumers match those separately.
    /// Chains and temporary names must follow one UID, not a spelling that
    /// another parent takes over later in the plan.
    pub fn renamed(self, names: &PlanNames<'_>) -> Self {
        let to = |table: TableName| match names.holder(&table, At::Start) {
            Holder::Tracked(uid) => names.table_at(uid, At::End).cloned().unwrap_or(table),
            Holder::Unmanaged | Holder::Vacant => table,
        };
        Self {
            parents: self
                .parents
                .into_iter()
                .map(|(p, of)| (p, to(of)))
                .collect(),
            held: self
                .held
                .into_iter()
                .map(|(of, held)| (to(of), held))
                .collect(),
            keys: self
                .keys
                .into_iter()
                .map(|(parent, keys)| (to(parent), keys))
                .collect(),
        }
    }

    /// Spell both ends of an existing relation at another plan boundary.
    /// This changes names only, without adding relations created by attach.
    /// A name handed to another UID never receives its old holder's relation.
    pub fn at(&self, names: &PlanNames<'_>, from: At, to: At) -> Self {
        let address = |table: &TableName| match names.holder(table, from) {
            Holder::Tracked(uid) => names.table_at(uid, to).cloned(),
            Holder::Unmanaged => Some(table.clone()),
            Holder::Vacant => None,
        };
        let mut result = Self::default();
        for (child, parent) in &self.parents {
            if let (Some(child), Some(parent)) = (address(child), address(parent)) {
                result.parents.insert(child.clone(), parent.clone());
                result.held.entry(parent).or_default().push(child);
            }
        }
        for children in result.held.values_mut() {
            children.sort();
        }
        for (parent, keys) in &self.keys {
            if let Some(parent) = address(parent) {
                result.keys.insert(parent, keys.clone());
            }
        }
        result
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
    fn parents_swapped_through_a_temporary_name_keep_their_own_partitions() {
        let mut s = schema();
        s.tables.insert(name("app.q"), Table::default());
        s.tables.insert(name("app.q_1"), partition("app.q"));
        let p_uid = crate::Uid::derived(crate::UidKind::Table, "p", 0);
        let q_uid = crate::Uid::derived(crate::UidKind::Table, "q", 0);
        let rename = |uid: &crate::Uid, from: &str, to: &str| {
            crate::PlannedChange::new(crate::Change::RenameTable {
                uid: uid.clone(),
                from: name(from),
                to: name(to),
                defaults: vec![],
            })
        };
        let changes = [
            rename(&p_uid, "app.p", "app.tmp"),
            rename(&q_uid, "app.q", "app.p"),
            rename(&p_uid, "app.tmp", "app.q"),
        ];
        let after = crate::IdsFile {
            tables: [(p_uid, name("app.q")), (q_uid, name("app.p"))].into(),
            ..crate::IdsFile::default()
        };
        let names = PlanNames::of(&changes, &after).unwrap();
        let moved = Partitions::of(&s).renamed(&names);
        assert_eq!(moved.parent(&name("app.p_a")), Some(&name("app.q")));
        assert_eq!(moved.parent(&name("app.q_1")), Some(&name("app.p")));
        assert!(moved.of_parent(&name("app.tmp")).is_empty());
        let original = Partitions::of(&s);
        let middle = original.at(&names, At::Start, At::Before(2));
        assert_eq!(middle.parent(&name("app.p_a")), Some(&name("app.tmp")));
        assert_eq!(middle.parent(&name("app.q_1")), Some(&name("app.p")));
        assert_eq!(middle.at(&names, At::Before(2), At::End), moved);
        assert_eq!(moved.at(&names, At::End, At::Start), original);
    }

    /// DEC-2346.1: a parent keeps the user-defined types of its key columns,
    /// scalar ones only, under the name the plan leaves it, and the relation
    /// at another boundary carries them with it.
    #[test]
    fn a_parents_key_types_follow_the_parent_and_name_only_scalar_references() {
        let mut parent = Table::default();
        for (column, ty) in [("id", "integer"), ("m", "app.mood"), ("tags", "app.mood[]")] {
            parent
                .columns
                .insert(column.into(), crate::Column::new(ty.parse().unwrap()));
        }
        parent.partition_by = Some(crate::PartitionBy {
            strategy: crate::PartitionStrategy::Range,
            columns: vec!["m".into(), "id".into(), "tags".into(), "gone".into()],
            external: false,
        });
        let mut s = schema();
        s.tables.insert(name("app.p"), parent);
        let partitions = Partitions::of(&s);
        let mood: TypeRef = "app.mood".parse().unwrap();
        // Positions that are no scalar reference, or no column, name nothing;
        // an ordinary table is no parent at all.
        assert_eq!(
            partitions.key_types(&name("app.p")),
            [Some(mood.clone()), None, None, None]
        );
        assert!(partitions.key_types(&name("app.t")).is_empty());
        // The parent is renamed: its key types go with it.
        let uid = crate::Uid::derived(crate::UidKind::Table, "p", 0);
        let changes = [crate::PlannedChange::new(crate::Change::RenameTable {
            uid: uid.clone(),
            from: name("app.p"),
            to: name("app.q"),
            defaults: vec![],
        })];
        let after = crate::IdsFile {
            tables: [(uid, name("app.q"))].into(),
            ..crate::IdsFile::default()
        };
        let names = PlanNames::of(&changes, &after).unwrap();
        let moved = partitions.clone().renamed(&names);
        assert_eq!(moved.key_types(&name("app.q"))[0], Some(mood));
        assert!(moved.key_types(&name("app.p")).is_empty());
        assert_eq!(moved.at(&names, At::End, At::Start), partitions);
    }

    #[test]
    fn boundary_relations_follow_renamed_children_and_never_their_replacements() {
        use crate::{Change, IdsFile, PlannedChange, Uid, UidKind};
        let original = Partitions::of(&schema());
        let child = Uid::derived(UidKind::Table, "child", 0);
        let next = Uid::derived(UidKind::Table, "next", 0);
        let changes = [
            Change::RenameTable {
                uid: child.clone(),
                from: name("app.p_a"),
                to: name("app.renamed"),
                defaults: vec![],
            },
            Change::DropTable {
                uid: child.clone(),
                name: name("app.renamed"),
                detach_from: Some(name("app.p")),
            },
            Change::CreateTable {
                uid: next.clone(),
                name: name("app.renamed"),
                table: Box::new(Table::default()),
                beside_default: None,
                moves_at_most: None,
            },
        ]
        .map(PlannedChange::new);
        let ids = IdsFile {
            tables: [(next, name("app.renamed"))].into(),
            ..Default::default()
        };
        let names = PlanNames::of(&changes, &ids).unwrap();
        let middle = original.at(&names, At::Start, At::Before(1));
        assert_eq!(middle.parent(&name("app.renamed")), Some(&name("app.p")));
        assert_eq!(
            middle.of_parent(&name("app.p")),
            [name("app.p_b"), name("app.renamed")]
        );
        assert_eq!(middle.parent(&name("app.p_a")), None);
        let end = original.at(&names, At::Start, At::End);
        assert_eq!(end.parent(&name("app.renamed")), None);
        assert_eq!(end.of_parent(&name("app.p")), [name("app.p_b")]);
    }

    /// A read before a plan's renames, brought under the names they leave:
    /// each parent moves once, a chain included (#1751 review).
    #[test]
    fn a_renamed_parent_holds_its_partitions_under_its_new_name() {
        let mut s = schema();
        s.tables.insert(name("app.q"), Table::default());
        s.tables.insert(name("app.q_1"), partition("app.q"));
        // `q` to `r`, then `p` to `q`: each once.
        let (p, q, r) = (name("app.p"), name("app.q"), name("app.r"));
        let p_uid = crate::Uid::derived(crate::UidKind::Table, "p", 0);
        let q_uid = crate::Uid::derived(crate::UidKind::Table, "q", 0);
        let changes = [
            crate::PlannedChange::new(crate::Change::RenameTable {
                uid: q_uid.clone(),
                from: q.clone(),
                to: r.clone(),
                defaults: vec![],
            }),
            crate::PlannedChange::new(crate::Change::RenameTable {
                uid: p_uid.clone(),
                from: p.clone(),
                to: q.clone(),
                defaults: vec![],
            }),
        ];
        let after = crate::IdsFile {
            tables: [(p_uid, q.clone()), (q_uid, r.clone())].into(),
            ..crate::IdsFile::default()
        };
        let names = PlanNames::of(&changes, &after).unwrap();
        let moved = Partitions::of(&s).renamed(&names);
        assert_eq!(moved.of_parent(&q), [name("app.p_a"), name("app.p_b")]);
        assert_eq!(moved.of_parent(&r), [name("app.q_1")]);
        assert_eq!(moved.parent(&name("app.q_1")), Some(&r));
        assert_eq!(
            moved.column(&ColumnRef::new(name("app.p_a"), "v")),
            ColumnRef::new(q.clone(), "v")
        );
        // Negative: the old name holds nothing, and no rename moves nothing.
        assert!(moved.of_parent(&p).is_empty());
        assert_eq!(
            Partitions::of(&s).renamed(&PlanNames::of(&[], &crate::IdsFile::default()).unwrap()),
            Partitions::of(&s)
        );
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
