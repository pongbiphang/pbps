//! Demand selection and portable creation parts (DEC-2469.4).
use crate::{Dialect, DialectError};
use pbps_model::object_ref::{ObjectRef, Root, Selector};
use pbps_model::operation::PartKey;
use pbps_model::{Change, Column, ColumnName, IdsFile, TableName, ValueSource};
use std::collections::{BTreeMap, BTreeSet};

mod inline;
pub mod inputs;
pub use inline::Inline;

/// Install these table parts after their columns' extracted NOT NULL. The
/// dialect already defines what blocks tightening; PKs additionally require
/// NOT NULL on engines that allow tightening under other indexes.
pub fn not_null_prerequisites(
    table: &pbps_model::Table,
    dialect: &(impl Dialect + ?Sized),
) -> BTreeMap<PartKey, BTreeSet<ColumnName>> {
    let mut prerequisites: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for (name, column) in &table.columns {
        if column.nullable {
            continue;
        }
        let mut blockers = crate::engine_effects::local_column_blockers(
            dialect,
            table,
            name.raw(),
            dialect.nullability_dependents(false),
            None,
        );
        if table
            .primary_key
            .as_ref()
            .is_some_and(|key| key.columns.contains(name))
        {
            blockers.push(PartKey::PrimaryKey);
        }
        for blocker in blockers {
            prerequisites
                .entry(blocker)
                .or_default()
                .insert(name.clone());
        }
    }
    prerequisites
}

/// Transient annotation intent, using the existing typed dependency graph.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub scratch: bool,
    pub references: &'a BTreeSet<ObjectRef>,
    pub dependencies: Option<&'a BTreeMap<ObjectRef, BTreeSet<ObjectRef>>>,
}
impl<'a> From<&'a BTreeSet<ObjectRef>> for Request<'a> {
    fn from(references: &'a BTreeSet<ObjectRef>) -> Self {
        Self {
            scratch: false,
            references,
            dependencies: None,
        }
    }
}

#[derive(Default)]
pub struct Demand {
    pub scratch: bool,
    pub inline: Inline,
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
    pub fn for_request(
        change: &Change,
        request: Request<'_>,
        dialect: &(impl Dialect + ?Sized),
    ) -> Self {
        let mut demand = Self::of(change, request.references);
        demand.scratch = request.scratch;
        if let Change::CreateTable { name, table, .. } = change {
            demand.inline = dialect.creation_inline(name, table);
            demand.inline.close(name, table, request, dialect);
            // A dependency on the atomic NOT NULL needs the whole column
            // installation. Provider-only references can use the inline group.
            for column in demand.inline.column_not_null.keys() {
                let reference = ObjectRef {
                    root: Root::Column(name.column(column)),
                    part: Selector::NotNull,
                };
                let ordered = request
                    .dependencies
                    .is_none_or(|graph| graph.get(&reference).is_some_and(|p| !p.is_empty()));
                if demand.not_null.contains(column) && ordered {
                    demand.columns.insert(column.clone());
                }
            }
        }
        demand
    }

    pub fn resolve_suffix(&mut self, name: &TableName, table: &pbps_model::Table) {
        self.columns = self.inline.suffix(name, table, self);
    }
    pub fn part(&self, table: &TableName, part: &PartKey) -> bool {
        self.parts.contains(part) && !self.inline.keeps_part(table, part)
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

    pub fn wants_default(&self, column: &ColumnName) -> bool {
        self.defaults.contains(column)
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
    engine: &(impl Dialect + ?Sized),
) -> Result<Vec<Change>, DialectError> {
    let dialect = engine.name();
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
                let full = (**table).clone();
                // A held engine recipe may return the original shape. Clamp
                // its physical cut against the same inline group here too.
                let selected = demand.inline.suffix(name, &full, demand);
                let mut moved_columns = BTreeSet::new();
                let columns = std::mem::take(&mut table.columns);
                for (column, mut spec) in columns {
                    let extract = selected.contains(&column);
                    if extract {
                        moved_columns.insert(column.clone());
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
                // Computed columns follow every ordinary column physically.
                // This is an ordinal prerequisite even for a constant expression.
                if !moved_columns.is_empty() {
                    for (column, computed) in std::mem::take(&mut table.computed) {
                        tail.push(Change::AddComputedColumn {
                            table: name.clone(),
                            name: column,
                            computed,
                        });
                    }
                }
                let mut facets = Vec::new();
                if let Some(key) = table.primary_key.take() {
                    facets.push((
                        PartKey::PrimaryKey,
                        Change::SetPrimaryKey {
                            table: name.clone(),
                            from: None,
                            to: Some(key),
                            nonclustered: !full.primary_key_is_clustered(),
                        },
                    ));
                }
                for (key, constraint) in std::mem::take(&mut table.unique) {
                    facets.push((
                        PartKey::Unique(key.clone()),
                        Change::AddUnique {
                            table: name.clone(),
                            clustered: full.unique_is_clustered(key.raw()),
                            name: key,
                            constraint,
                        },
                    ));
                }
                for (key, constraint) in std::mem::take(&mut table.foreign_keys) {
                    facets.push((
                        PartKey::ForeignKey(key.clone()),
                        Change::AddForeignKey {
                            table: name.clone(),
                            name: key,
                            constraint: Box::new(constraint),
                        },
                    ));
                }
                for (key, constraint) in std::mem::take(&mut table.checks) {
                    facets.push((
                        PartKey::Check(key.clone()),
                        Change::AddCheck {
                            table: name.clone(),
                            name: key,
                            constraint,
                        },
                    ));
                }
                for (key, index) in std::mem::take(&mut table.indexes) {
                    facets.push((
                        PartKey::Index(key.clone()),
                        Change::AddIndex {
                            table: name.clone(),
                            clustered: full.index_is_clustered(key.raw()),
                            name: key,
                            index: Box::new(index),
                        },
                    ));
                }
                let tightening = not_null_prerequisites(&full, engine);
                for (key, facet) in facets {
                    let affected = inputs::columns(&facet, name, &full, engine)
                        .map_or(!moved_columns.is_empty(), |columns| {
                            !columns.is_disjoint(&moved_columns)
                        });
                    let nullability = tightening.get(&key).is_some_and(|columns| {
                        columns.iter().any(|c| {
                            demand.not_null.contains(c)
                                && !demand
                                    .inline
                                    .contains(Root::Column(name.column(c)), Selector::NotNull)
                        })
                    });
                    if !demand.inline.keeps_part(name, &key)
                        && (demand.part(name, &key) || affected || nullability)
                    {
                        tail.push(facet);
                    } else {
                        match facet {
                            Change::SetPrimaryKey { to, .. } => table.primary_key = to,
                            Change::AddUnique {
                                name, constraint, ..
                            } => {
                                table.unique.insert(name, constraint);
                            }
                            Change::AddForeignKey {
                                name, constraint, ..
                            } => {
                                table.foreign_keys.insert(name, *constraint);
                            }
                            Change::AddCheck {
                                name, constraint, ..
                            } => {
                                table.checks.insert(name, constraint);
                            }
                            Change::AddIndex { name, index, .. } => {
                                table.indexes.insert(name, *index);
                            }
                            _ => unreachable!(
                                "creation facets use existing key/check/index installers"
                            ),
                        }
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
    demand.inline.check(&out, dialect)?;
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
    if demand.defaults.contains(name)
        && !demand.inline.keeps_default(table, name)
        && matches!(column.value_source, ValueSource::Default(_))
    {
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
    // Both engines require NOT NULL in the identity column's installation.
    if demand.not_null.contains(name)
        && !demand
            .inline
            .contains(Root::Column(table.column(name)), Selector::NotNull)
        && !column.nullable
        && !demand.inline.column_not_null.contains_key(name)
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
