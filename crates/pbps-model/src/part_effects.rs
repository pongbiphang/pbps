//! Dependency-bearing parts of a change, before any consumer chooses its policy.
//!
//! A removed binding, a rewritten declaration and an owner taking its parts away
//! are different facts. In particular, a partition can lose its own default and
//! still receive its parent's expression. Preserve that transition rather than
//! making each consumer recover it from `Change` (DEC-1773.1).

use crate::{Change, ColumnField, ColumnRef, ModuleId, ModuleKind, TableName, ValueSource};

/// Where a part's creation comes from. A table's inline default is not a separate
/// restoration step that can be woven around a routine replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Direct,
    Contained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Created,
    Removed,
    Rewritten,
    Renamed,
    Redefined(ColumnField),
}

/// The declaration transition, not an assertion about the prior catalog row.
/// `Partition { from: None, .. }` can still replace an inherited expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultChange<'a> {
    Declaration,
    Column {
        from: Option<&'a str>,
        to: Option<&'a str>,
    },
    Partition {
        from: Option<&'a str>,
        to: Option<&'a str>,
        fallback: Option<&'a str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    Table(&'a TableName),
    Column(ColumnRef),
    Default {
        column: ColumnRef,
        change: DefaultChange<'a>,
    },
    Generated(ColumnRef),
    Named {
        table: &'a TableName,
        kind: crate::Part,
        // Policy names share this borrowed dependency carrier; keep the
        // generic view raw without widening the index/constraint type scope.
        name: &'a str,
    },
    PrimaryKey(&'a TableName),
    Module {
        id: &'a ModuleId,
        kind: ModuleKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartEffect<'a> {
    pub part: Part<'a>,
    pub effect: Effect,
    pub origin: Origin,
}

fn direct(part: Part<'_>, effect: Effect) -> PartEffect<'_> {
    PartEffect {
        part,
        effect,
        origin: Origin::Direct,
    }
}

fn transition(from: bool, to: bool) -> Effect {
    match (from, to) {
        (_, false) => Effect::Removed,
        (false, true) => Effect::Created,
        (true, true) => Effect::Rewritten,
    }
}

impl Change {
    /// Dependency-part facts, with no consumer's removal/rebinding policy.
    ///
    /// This match is exhaustive: a new Change kind cannot compile until its
    /// dependency-part effects have been classified. Existing column-field
    /// facts come from `columns_redefined`, not a second field inventory.
    pub fn part_effects(&self) -> Vec<PartEffect<'_>> {
        let named =
            |table, kind, name, effect| vec![direct(Part::Named { table, kind, name }, effect)];
        let mut out = match self {
            Change::BuildPartitionedIndex(r) => vec![direct(
                match &r.definition {
                    crate::PartitionIndexDefinition::PrimaryKey(_) => Part::PrimaryKey(&r.parent.name),
                    crate::PartitionIndexDefinition::Index(_) => Part::Named {
                        table: &r.parent.name,
                        kind: crate::Part::Index,
                        name: r.name.raw(),
                    },
                    crate::PartitionIndexDefinition::Unique(_) => Part::Named {
                        table: &r.parent.name,
                        kind: crate::Part::Unique,
                        name: r.name.raw(),
                    },
                },
                Effect::Created,
            )],
Change::RebuildPartitionedTable(_) | Change::CopyTableRows(_) | Change::AnalyzeTableTree(_) => Vec::new(),
            Change::CreateTable { name, table, .. } => {
                let mut parts = vec![direct(Part::Table(name), Effect::Created)];
                let mut contained = |part| {
                    parts.push(PartEffect {
                        part,
                        effect: Effect::Created,
                        origin: Origin::Contained,
                    });
                };
                for column in table.columns.keys() {
                    contained(Part::Column(name.column(column)));
                }
                for column in table.expression_columns() {
                    if table
                        .columns
                        .get(column)
                        .is_some_and(|c| matches!(c.value_source, ValueSource::Generated(_)))
                    {
                        contained(Part::Generated(name.column(column)));
                    } else {
                        contained(Part::Default {
                            column: name.column(column),
                            change: DefaultChange::Declaration,
                        });
                    }
                }
                parts
            }
            Change::DropTable { name, .. } => vec![direct(Part::Table(name), Effect::Removed)],
            Change::RenameTable { from, .. } => vec![direct(Part::Table(from), Effect::Renamed)],
            Change::AddColumn {
                table,
                name,
                column,
                ..
            } => {
                let address = table.column(name);
                let mut parts = vec![direct(Part::Column(address.clone()), Effect::Created)];
                let expression = match column.value_source {
                    ValueSource::Default(_) => Some(Part::Default {
                        column: address,
                        change: DefaultChange::Declaration,
                    }),
                    ValueSource::Generated(_) => Some(Part::Generated(address)),
                    ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
                };
                if let Some(part) = expression {
                    parts.push(PartEffect {
                        part,
                        effect: Effect::Created,
                        origin: Origin::Contained,
                    });
                }
                parts
            }
            Change::DropColumn { column, .. } => {
                vec![direct(Part::Column(column.clone()), Effect::Removed)]
            }
            Change::RenameColumn { table, from, .. } => {
                vec![direct(Part::Column(table.column(from)), Effect::Renamed)]
            }
            Change::AlterColumnDefault {
                column, from, to, ..
            } => vec![direct(
                Part::Default {
                    column: column.clone(),
                    change: DefaultChange::Column {
                        from: from.as_deref(),
                        to: to.as_deref(),
                    },
                },
                transition(from.is_some(), to.is_some()),
            )],
            Change::SetPartitionDefault {
                table,
                column,
                from,
                to,
                fallback,
                ..
            } => vec![direct(
                Part::Default {
                    column: table.column(column),
                    change: DefaultChange::Partition {
                        from: from.as_deref(),
                        to: to.as_deref(),
                        fallback: fallback.as_deref(),
                    },
                },
                transition(from.is_some(), to.is_some() || fallback.is_some()),
            )],
            Change::AlterColumnExpression { column, .. } => {
                vec![direct(Part::Generated(column.clone()), Effect::Rewritten)]
            }
            Change::DropPolicy { table, name, .. } => {
                named(table, crate::Part::Policy, name, Effect::Removed)
            }
            Change::CreatePolicy { table, name, .. } => {
                named(table, crate::Part::Policy, name, Effect::Created)
            }
            Change::AddCheck { table, name, .. } => {
                named(table, crate::Part::Check, name.raw(), Effect::Created)
            }
            Change::DropCheck { table, name } => {
                named(table, crate::Part::Check, name.raw(), Effect::Removed)
            }
            Change::AddIndex { table, name, .. } => {
                named(table, crate::Part::Index, name.raw(), Effect::Created)
            }
            Change::DropIndex { table, name } => {
                named(table, crate::Part::Index, name.raw(), Effect::Removed)
            }
            Change::AddUnique { table, name, .. } => {
                named(table, crate::Part::Unique, name.raw(), Effect::Created)
            }
            Change::DropUnique { table, name } => {
                named(table, crate::Part::Unique, name.raw(), Effect::Removed)
            }
            Change::AddForeignKey { table, name, .. } => {
                named(table, crate::Part::ForeignKey, name.raw(), Effect::Created)
            }
            Change::DropForeignKey { table, name } => {
                named(table, crate::Part::ForeignKey, name.raw(), Effect::Removed)
            }
            Change::SetPrimaryKey {
                table, from, to, ..
            } => {
                // No prior key and no replacement is not a catalog removal.
                if from.is_none() && to.is_none() {
                    Vec::new()
                } else {
                    vec![direct(
                        Part::PrimaryKey(table),
                        transition(from.is_some(), to.is_some()),
                    )]
                }
            }
            Change::CreateModule { id, module } => vec![direct(
                Part::Module {
                    id,
                    kind: module.kind,
                },
                Effect::Created,
            )],
            Change::AlterModule { id, module } => vec![direct(
                Part::Module {
                    id,
                    kind: module.kind,
                },
                Effect::Rewritten,
            )],
            Change::DropModule { id, kind } => {
                vec![direct(Part::Module { id, kind: *kind }, Effect::Removed)]
            }
            // These kinds have no separate dependency-bearing definition in
            // this projection. Column field changes are appended below.
            Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            // A numbering source's sequence is no part this projection
            // names; PostgreSQL's drop impact finds it by its dependency on
            // the column (#1889).
            | Change::AlterColumnNumbering { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::AddEnumValue { .. }
            | Change::RenameEnumValue { .. }
            | Change::CreateType { .. }
            | Change::DropType { .. }
            | Change::RenameType { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => Vec::new(),
        };
        out.extend(
            self.columns_redefined()
                .into_iter()
                .map(|(column, field)| direct(Part::Column(column), Effect::Redefined(field))),
        );
        out
    }
}
