//! Demand selection and portable creation parts (DEC-2469.4).
use crate::DialectError;
use pbps_model::object_ref::{ObjectRef, Root, Selector};
use pbps_model::operation::PartKey;
use pbps_model::{Change, Column, ColumnName, IdsFile, TableName, ValueSource};
use std::collections::BTreeSet;

#[derive(Default)]
pub struct Demand {
    /// Requested column roots; the engine recipe resolves these into its
    /// legal physical suffix before expose consumes the exact selection.
    pub columns: BTreeSet<ColumnName>,
    pub generation: BTreeSet<ColumnName>,
    pub required_columns: BTreeSet<ColumnName>,
    defaults: BTreeSet<ColumnName>,
    not_null: BTreeSet<ColumnName>,
    parts: BTreeSet<PartKey>,
}
impl Demand {
    pub fn of(change: &Change, references: &BTreeSet<ObjectRef>) -> Self {
        let mut demand = Self::default();
        let Some(table) = change.table() else {
            return demand;
        };
        for reference in references {
            match (&reference.root, &reference.part) {
                (Root::Column(column), selector) if &column.table == table => match selector {
                    Selector::Whole => {
                        demand.columns.insert(column.name.clone());
                    }
                    Selector::Generation => {
                        demand.generation.insert(column.name.clone());
                    }
                    Selector::Default => {
                        demand.defaults.insert(column.name.clone());
                    }
                    Selector::NotNull => {
                        demand.not_null.insert(column.name.clone());
                    }
                    Selector::Sequence | Selector::Part(_) => {}
                },
                (Root::Table(owner), Selector::Part(part)) if owner == table => {
                    demand.parts.insert(part.clone());
                }
                _ => {}
            }
        }
        demand
    }
    /// Resolve the physical suffix once, before the engine decides which
    /// expressions and shape must remain together (DEC-2469.4).
    pub fn column_suffix<'a>(
        &self,
        columns: impl Iterator<Item = &'a ColumnName>,
    ) -> BTreeSet<ColumnName> {
        columns
            .skip_while(|column| !self.columns.contains(*column))
            .cloned()
            .collect()
    }

    pub fn generation(&self, column: &ColumnName) -> bool {
        self.generation.contains(column) || self.columns.contains(column)
    }

    pub fn not_null(&self, column: &ColumnName) -> bool {
        self.not_null.contains(column)
    }
}

/// Retain the engine's legal generation groups, then expose requested portable
/// facets using existing Changes. No caller mutates an already lowered group.
// Only the creation/function transitions below grant this capability.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn expose(
    parts: Vec<Change>,
    ids: &IdsFile,
    demand: &Demand,
    dialect: &'static str,
) -> Result<Vec<Change>, DialectError> {
    let empty: BTreeSet<_> = parts
        .iter()
        .filter_map(|change| {
            if let Change::CreateTable {
                name,
                beside_default: None,
                ..
            } = change
            {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();
    let identities = ids.index();
    let uid = |table: &TableName, column: &ColumnName| {
        identities
            .column_uid(&table.column(column))
            .cloned()
            .ok_or_else(|| DialectError::Invalid {
                dialect,
                message: format!(
                    "{} has no recorded identity for its demanded creation facet",
                    table.column(column).shown()
                ),
            })
    };
    let mut out = Vec::new();
    for mut change in parts {
        let mut tail = Vec::new();
        match &mut change {
            Change::CreateTable {
                uid: table_uid,
                name,
                table,
                beside_default,
                ..
            } if beside_default.is_none() => {
                let mut moved = false;
                let columns = std::mem::take(&mut table.columns);
                for (column, mut spec) in columns {
                    let extract = demand.columns.contains(&column);
                    moved |= extract;
                    if extract {
                        let column_uid = uid(name, &column)?;
                        let mut facets = Vec::new();
                        split_column(name, &column, &column_uid, &mut spec, demand, &mut facets);
                        tail.push(Change::AddColumn {
                            uid: column_uid,
                            table: name.clone(),
                            name: column,
                            column: Box::new(spec),
                        });
                        tail.extend(facets);
                    } else {
                        if demand.defaults.contains(&column) || demand.not_null.contains(&column) {
                            split_column(
                                name,
                                &column,
                                &uid(name, &column)?,
                                &mut spec,
                                demand,
                                &mut tail,
                            );
                        }
                        table.columns.insert(column, spec);
                    }
                }
                if moved {
                    for (column, computed) in std::mem::take(&mut table.computed) {
                        tail.push(Change::AddComputedColumn {
                            table: name.clone(),
                            name: column,
                            computed,
                        });
                    }
                }
                if table.primary_key.as_ref().is_some_and(|key| {
                    moved
                        || demand.parts.contains(&PartKey::PrimaryKey)
                        || key.columns.iter().any(|c| demand.not_null.contains(c))
                }) {
                    tail.push(Change::SetPrimaryKey {
                        table: name.clone(),
                        from: None,
                        nonclustered: !table.primary_key_is_clustered(),
                        to: table.primary_key.take(),
                    });
                }
                for (key, constraint) in std::mem::take(&mut table.unique) {
                    if moved || demand.parts.contains(&PartKey::Unique(key.clone())) {
                        tail.push(Change::AddUnique {
                            table: name.clone(),
                            clustered: table.unique_is_clustered(key.raw()),
                            name: key,
                            constraint,
                        });
                    } else {
                        table.unique.insert(key, constraint);
                    }
                }
                for (key, constraint) in std::mem::take(&mut table.foreign_keys) {
                    if moved || demand.parts.contains(&PartKey::ForeignKey(key.clone())) {
                        tail.push(Change::AddForeignKey {
                            table: name.clone(),
                            name: key,
                            constraint: Box::new(constraint),
                        });
                    } else {
                        table.foreign_keys.insert(key, constraint);
                    }
                }
                for (key, constraint) in std::mem::take(&mut table.checks) {
                    if moved || demand.parts.contains(&PartKey::Check(key.clone())) {
                        tail.push(Change::AddCheck {
                            table: name.clone(),
                            name: key,
                            constraint,
                        });
                    } else {
                        table.checks.insert(key, constraint);
                    }
                }
                for (key, index) in std::mem::take(&mut table.indexes) {
                    if moved || demand.parts.contains(&PartKey::Index(key.clone())) {
                        tail.push(Change::AddIndex {
                            table: name.clone(),
                            clustered: table.index_is_clustered(key.raw()),
                            name: key,
                            index: Box::new(index),
                        });
                    } else {
                        table.indexes.insert(key, index);
                    }
                }
                let identity_late = match &table.replica_identity {
                    Some(pbps_model::ReplicaIdentity::Index(key)) => {
                        !table.indexes.contains_key(key)
                    }
                    Some(pbps_model::ReplicaIdentity::Unique(key)) => {
                        !table.unique.contains_key(key)
                    }
                    Some(pbps_model::ReplicaIdentity::PrimaryKey) => table.primary_key.is_none(),
                    _ => false,
                };
                if identity_late {
                    tail.push(Change::SetReplicaIdentity {
                        uid: table_uid.clone(),
                        table: name.clone(),
                        to: table.replica_identity.take(),
                    });
                }
            }
            // A standing table may contain rows. ADD COLUMN DEFAULT can populate
            // them in the same statement; preserve that engine-owned group.
            Change::AddColumn {
                uid,
                table,
                name,
                column,
            } if empty.contains(table) => split_column(table, name, uid, column, demand, &mut tail),
            _ => {}
        }
        out.push(change);
        out.extend(tail);
    }
    Ok(out)
}
fn split_column(
    table: &TableName,
    name: &ColumnName,
    uid: &pbps_model::Uid,
    column: &mut Column,
    demand: &Demand,
    tail: &mut Vec<Change>,
) {
    if demand.defaults.contains(name) && matches!(column.value_source, ValueSource::Default(_)) {
        let ValueSource::Default(default) =
            std::mem::replace(&mut column.value_source, ValueSource::None)
        else {
            unreachable!()
        };
        tail.push(Change::AlterColumnDefault {
            uid: uid.clone(),
            column: table.column(name),
            from: None,
            to: Some(default),
        });
    }
    // Identity and its NOT NULL are one physical installation on SQL Server.
    if demand.not_null.contains(name)
        && !column.nullable
        && !matches!(column.value_source, ValueSource::Identity(_))
    {
        column.nullable = true;
        tail.push(Change::AlterColumnNullability {
            uid: uid.clone(),
            column: table.column(name),
            ty: column.ty.clone(),
            to_nullable: false,
            collation: column.collation.clone(),
            through_external_partitions: false,
        });
    }
}
