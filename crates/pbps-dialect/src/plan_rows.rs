//! Raw plan facts shared by preflight projections. SQL and engine assignment
//! rules belong to the callers, not to this walk.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::ColumnName;
use pbps_model::plan_names::{At, Holder, PlanNames};
use pbps_model::{
    Cell, Change, Collation, Column, ColumnRef, ColumnType, ForeignKey, Row, RowKey, Table,
    TableName, Uid, Value,
};

/// The raw input an engine projection needs. A new Change kind must be
/// classified once in `PlanFacts::of`, even when it has no row effect.
#[derive(Debug, Clone, Copy)]
pub enum Event<'p> {
    CreateTable {
        name: &'p TableName,
        table: &'p Table,
    },
    DropTable {
        name: &'p TableName,
    },
    RenameColumn {
        uid: &'p Uid,
        table: &'p TableName,
        from: &'p ColumnName,
    },
    DropColumn {
        uid: &'p Uid,
        column: &'p ColumnRef,
    },
    AddColumn {
        table: &'p TableName,
        name: &'p ColumnName,
        column: &'p Column,
    },
    AlterColumnType {
        column: &'p ColumnRef,
        from: &'p ColumnType,
        to: &'p ColumnType,
        from_collation: &'p Option<Collation>,
        to_collation: &'p Option<Collation>,
    },
    AlterColumnExpression {
        column: &'p ColumnRef,
    },
    AddComputedColumn {
        table: &'p TableName,
        name: &'p ColumnName,
    },
    DropComputedColumn {
        table: &'p TableName,
    },
    AddForeignKey {
        table: &'p TableName,
        name: &'p pbps_model::ObjectLocalName,
        constraint: &'p ForeignKey,
    },
    DropForeignKey {
        table: &'p TableName,
        name: &'p pbps_model::ObjectLocalName,
    },
    AttachPartition {
        table: &'p TableName,
        parent: &'p TableName,
    },
    InsertRow {
        table: &'p TableName,
        key_column: &'p ColumnName,
        key: &'p RowKey,
        row: &'p Row,
        defaults: &'p BTreeMap<ColumnName, String>,
        types: &'p BTreeMap<ColumnName, ColumnType>,
    },
    UpdateRow {
        table: &'p TableName,
        key_column: &'p ColumnName,
        key: &'p RowKey,
        columns: &'p BTreeMap<ColumnName, (Cell, Cell)>,
        unchanged: &'p BTreeMap<ColumnName, Cell>,
        types: &'p BTreeMap<ColumnName, ColumnType>,
        after_types: &'p BTreeMap<ColumnName, ColumnType>,
    },
    DeleteRow {
        table: &'p TableName,
        key_column: &'p ColumnName,
        key: &'p RowKey,
        types: &'p BTreeMap<ColumnName, ColumnType>,
        after_types: &'p BTreeMap<ColumnName, ColumnType>,
    },
}

#[derive(Debug)]
pub struct PlanFacts<'p> {
    names: &'p PlanNames<'p>,
    events: Vec<(usize, Event<'p>)>,
    removed: BTreeMap<Removed, usize>,
}

impl<'p> PlanFacts<'p> {
    pub fn of(names: &'p PlanNames<'p>) -> Self {
        let mut events = Vec::new();
        let mut removed = BTreeMap::new();
        for (at, p) in names.changes().iter().enumerate() {
            let event = match &p.change {
                Change::CreateTable { name, table, .. } => Event::CreateTable { name, table },
                Change::DropTable { name, .. } => Event::DropTable { name },
                Change::RenameColumn {
                    uid, table, from, ..
                } => Event::RenameColumn { uid, table, from },
                Change::DropColumn { uid, column } => Event::DropColumn { uid, column },
                Change::AddColumn {
                    table,
                    name,
                    column,
                    ..
                } => Event::AddColumn {
                    table,
                    name,
                    column,
                },
                Change::AlterColumnType {
                    column,
                    from,
                    to,
                    from_collation,
                    to_collation,
                    ..
                } => Event::AlterColumnType {
                    column,
                    from,
                    to,
                    from_collation,
                    to_collation,
                },
                Change::AlterColumnExpression { column, .. } => {
                    Event::AlterColumnExpression { column }
                }
                Change::AddComputedColumn { table, name, .. } => {
                    Event::AddComputedColumn { table, name }
                }
                Change::DropComputedColumn { table, .. } => Event::DropComputedColumn { table },
                Change::AddForeignKey {
                    table,
                    name,
                    constraint,
                } => Event::AddForeignKey {
                    table,
                    name,
                    constraint,
                },
                Change::DropForeignKey { table, name } => Event::DropForeignKey { table, name },
                Change::AttachPartition { table, parent, .. } => {
                    Event::AttachPartition { table, parent }
                }
                Change::InsertRow {
                    table,
                    key_column,
                    key,
                    row,
                    defaults,
                    types,
                    ..
                } => Event::InsertRow {
                    table,
                    key_column,
                    key,
                    row,
                    defaults,
                    types,
                },
                Change::UpdateRow {
                    table,
                    key_column,
                    key,
                    columns,
                    unchanged,
                    types,
                    after_types,
                    ..
                } => Event::UpdateRow {
                    table,
                    key_column,
                    key,
                    columns,
                    unchanged,
                    types,
                    after_types,
                },
                Change::DeleteRow {
                    table,
                    key_column,
                    key,
                    types,
                    after_types,
                    ..
                } => Event::DeleteRow {
                    table,
                    key_column,
                    key,
                    types,
                    after_types,
                },
                Change::RenameTable { .. }
                | Change::DetachPartition { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnNumbering { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::SetIndexStorageParameters { .. }
                | Change::SetTablePersistence { .. }
                | Change::SetStorageParameters { .. }
                | Change::DropPolicy { .. }
                | Change::CreatePolicy { .. }
                | Change::SetRowSecurity { .. }
                | Change::SetReplicaIdentity { .. }
                | Change::SetPartitionDefault { .. }
                | Change::SetPartitionNotNull { .. }
                | Change::SetDataMode { .. }
                | Change::SetEnsureKeys { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => continue,
            };
            let removal = match event {
                Event::DropTable { name } => Some((name, None)),
                Event::DropForeignKey { table, name } => Some((table, Some(name.clone()))),
                Event::CreateTable { .. }
                | Event::RenameColumn { .. }
                | Event::DropColumn { .. }
                | Event::AddColumn { .. }
                | Event::AlterColumnType { .. }
                | Event::AlterColumnExpression { .. }
                | Event::AddComputedColumn { .. }
                | Event::DropComputedColumn { .. }
                | Event::AddForeignKey { .. }
                | Event::AttachPartition { .. }
                | Event::InsertRow { .. }
                | Event::UpdateRow { .. }
                | Event::DeleteRow { .. } => None,
            };
            if let Some((table, constraint)) = removal {
                let stored_table = names
                    .stored(table, At::Before(at))
                    .unwrap_or_else(|| table.clone());
                removed
                    .entry(Removed {
                        stored_table,
                        constraint,
                    })
                    .or_insert(at);
            }
            events.push((at, event));
        }
        Self {
            names,
            events,
            removed,
        }
    }

    /// Steps [0, step). The whole-plan endpoint is `changes().len()`.
    pub fn before(&self, step: usize) -> View<'_, 'p> {
        assert!(
            step <= self.names.changes().len(),
            "preflight boundary outside plan"
        );
        View { facts: self, step }
    }

    pub fn end(&self) -> View<'_, 'p> {
        self.before(self.names.changes().len())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct View<'v, 'p> {
    facts: &'v PlanFacts<'p>,
    step: usize,
}

/// A removed table releases every key it holds. Otherwise only these named
/// keys are removed; an empty set does not mean the table is absent.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    Table,
    Keys(Vec<pbps_model::ObjectLocalName>),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Removed {
    pub stored_table: TableName,
    pub constraint: Option<pbps_model::ObjectLocalName>,
}

impl<'p> View<'_, 'p> {
    fn at(&self) -> At {
        // PlanNames' Before names an actual change. A prefix may also end
        // after the last change, including the empty plan's sole boundary.
        if self.step == self.facts.names.changes().len() {
            At::End
        } else {
            At::Before(self.step)
        }
    }

    pub fn events(&self) -> impl Iterator<Item = (usize, Event<'p>)> + '_ {
        self.facts
            .events
            .iter()
            .copied()
            .take_while(|(at, _)| *at < self.step)
    }

    pub fn table(&self, name: &TableName) -> Option<TableName> {
        match self.facts.names.holder(name, self.at()) {
            Holder::Tracked(uid) => self.facts.names.table_at(uid, At::Start).cloned(),
            Holder::Unmanaged => Some(name.clone()),
            Holder::Vacant => None,
        }
    }

    pub fn created(&self, name: &TableName) -> bool {
        match self.facts.names.holder(name, self.at()) {
            Holder::Tracked(uid) => self.facts.names.table_at(uid, At::Start).is_none(),
            Holder::Unmanaged | Holder::Vacant => false,
        }
    }

    pub fn renamed_columns(
        &self,
        table: &TableName,
    ) -> impl Iterator<Item = (&'p Uid, &'p ColumnName, &'p ColumnName)> + use<'p> {
        self.facts.names.renamed_columns_at(table, self.at())
    }

    pub fn column(&self, column: &ColumnRef) -> Option<ColumnRef> {
        self.column_as(column, column)
    }

    /// The caller supplies an inherited column's naming source; deciding
    /// which relation reaches a child is an engine question.
    pub fn column_as(&self, column: &ColumnRef, source: &ColumnRef) -> Option<ColumnRef> {
        let stored = self
            .renamed_columns(&source.table)
            .find(|(_, _, to)| *to == &source.name)
            .map_or(&source.name, |(_, from, _)| from);
        Some(self.table(&column.table)?.column(stored))
    }

    pub fn removed(&self) -> impl Iterator<Item = &Removed> {
        self.facts
            .removed
            .iter()
            .filter(|(_, at)| **at < self.step)
            .map(|(removed, _)| removed)
    }

    pub fn removed_from(&self, stored_child: &TableName) -> Removal {
        let mut keys = Vec::new();
        for removed in self.removed() {
            if &removed.stored_table == stored_child {
                match &removed.constraint {
                    None => return Removal::Table,
                    Some(key) => {
                        keys.push(key.clone());
                    }
                }
            }
        }
        Removal::Keys(keys)
    }

    /// Record row effects once, with assignment and NULL syntax supplied by
    /// the engine. A dialect may leave an omitted NULL unrendered and read it
    /// later through `typed`; that omission is never an unknown default.
    pub fn writes<V, M: Default>(
        &self,
        mut render: impl FnMut(Assignment<'_>, &mut M) -> Option<V>,
        is_null: impl Fn(&Cell) -> bool,
    ) -> BTreeMap<TableName, RowWrites<V, M>> {
        let mut tables: BTreeMap<TableName, RowWrites<V, M>> = BTreeMap::new();
        for (_, event) in self.events() {
            match event {
                Event::InsertRow {
                    table,
                    key_column,
                    key,
                    row,
                    defaults,
                    types,
                } => {
                    let moved = tables.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    moved
                        .typed
                        .insert(key.clone(), types.keys().cloned().collect());
                    // The declared key is explicit even for an identity key;
                    // `types` carries non-key columns only (ADR-0004).
                    let value = Value::Text(key.as_str().to_owned());
                    let sql = render(
                        Assignment {
                            table,
                            key,
                            column: key_column,
                            cell: WrittenCell::Value(&value),
                            ty: None,
                        },
                        &mut moved.metadata,
                    );
                    moved.insert(key, key_column, sql);
                    for (column, cell) in WrittenCell::cells(row, defaults, types) {
                        if column == key_column
                            && matches!(
                                cell,
                                WrittenCell::OmittedNull | WrittenCell::EngineAssigned
                            )
                        {
                            continue;
                        }
                        let sql = render(
                            Assignment {
                                table,
                                key,
                                column,
                                cell,
                                ty: types.get(column),
                            },
                            &mut moved.metadata,
                        );
                        if sql.is_some()
                            || matches!(cell, WrittenCell::Value(_) | WrittenCell::Default(_))
                        {
                            moved.insert(key, column, sql);
                        }
                    }
                }
                Event::UpdateRow {
                    table,
                    key_column,
                    key,
                    columns,
                    unchanged,
                    types,
                    after_types,
                } => {
                    let moved = tables.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    for (column, cell) in unchanged {
                        if is_null(cell) {
                            moved
                                .held_null
                                .entry(key.clone())
                                .or_default()
                                .insert(column.clone());
                        }
                    }
                    for (column, (_, after)) in columns {
                        let cell = match after {
                            Cell::Value(value) => WrittenCell::Value(value),
                            Cell::Default(default) => WrittenCell::Default(default),
                        };
                        let sql = render(
                            Assignment {
                                table,
                                key,
                                column,
                                cell,
                                ty: after_types.get(column).or_else(|| types.get(column)),
                            },
                            &mut moved.metadata,
                        );
                        moved.update(key, column, sql);
                    }
                }
                Event::DeleteRow {
                    table,
                    key_column,
                    key,
                    ..
                } => {
                    let moved = tables.entry(table.clone()).or_default();
                    moved.delete(key_column, key);
                }
                Event::CreateTable { .. }
                | Event::DropTable { .. }
                | Event::RenameColumn { .. }
                | Event::DropColumn { .. }
                | Event::AddColumn { .. }
                | Event::AlterColumnType { .. }
                | Event::AlterColumnExpression { .. }
                | Event::AddComputedColumn { .. }
                | Event::DropComputedColumn { .. }
                | Event::AddForeignKey { .. }
                | Event::DropForeignKey { .. }
                | Event::AttachPartition { .. } => {}
            }
        }
        tables
    }
}

/// Raw assignment context. The dialect decides how the destination type and
/// collation convert a default, without making row bookkeeping engine-owned.
pub struct Assignment<'p> {
    pub table: &'p TableName,
    pub key: &'p RowKey,
    pub column: &'p ColumnName,
    pub cell: WrittenCell<'p>,
    pub ty: Option<&'p ColumnType>,
}

/// Raw omission semantics. An engine-assigned cell is not an omitted NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrittenCell<'p> {
    Value(&'p Value),
    Default(&'p str),
    OmittedNull,
    EngineAssigned,
}

impl<'p> WrittenCell<'p> {
    /// Every cell described by an insert, once. A column absent from all
    /// three inputs remains engine-assigned when queried with `inserted`.
    pub fn cells(
        row: &'p Row,
        defaults: &'p BTreeMap<ColumnName, String>,
        types: &'p BTreeMap<ColumnName, ColumnType>,
    ) -> impl Iterator<Item = (&'p ColumnName, Self)> {
        row.0
            .keys()
            .chain(defaults.keys())
            .chain(types.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(move |column| (column, Self::inserted(row, defaults, types, column)))
    }

    pub fn inserted(
        row: &'p Row,
        defaults: &'p BTreeMap<ColumnName, String>,
        types: &BTreeMap<ColumnName, ColumnType>,
        column: &ColumnName,
    ) -> Self {
        if let Some(value) = row.0.get(column) {
            Self::Value(value)
        } else if let Some(default) = defaults.get(column) {
            Self::Default(default)
        } else if types.contains_key(column) {
            Self::OmittedNull
        } else {
            Self::EngineAssigned
        }
    }
}

/// Shared bookkeeping over values supplied by a dialect. Foreign keys compare
/// tuples, so cells stay together by row key (DECISIONS 121). This container
/// neither renders nor interprets V; engine-specific metadata remains M.
#[derive(Debug)]
pub struct RowWrites<V, M = ()> {
    pub key_column: ColumnName,
    pub deleted: BTreeSet<RowKey>,
    pub updated: BTreeMap<RowKey, BTreeMap<ColumnName, Option<V>>>,
    pub inserted: BTreeMap<RowKey, BTreeMap<ColumnName, V>>,
    /// Non-key columns the insert can spell. A missing column is assigned by
    /// the engine; a present but unwritten one is an omitted NULL.
    pub typed: BTreeMap<RowKey, BTreeSet<ColumnName>>,
    /// Unknown assignments stay visible even where no value can be rendered.
    pub unprobeable: BTreeMap<ColumnName, BTreeSet<RowKey>>,
    /// Unchanged NULL siblings can exempt the entire FK tuple (DECISIONS 349).
    pub held_null: BTreeMap<RowKey, BTreeSet<ColumnName>>,
    pub metadata: M,
}

impl<V, M: Default> Default for RowWrites<V, M> {
    fn default() -> Self {
        Self {
            key_column: ColumnName::default(),
            deleted: BTreeSet::new(),
            updated: BTreeMap::new(),
            inserted: BTreeMap::new(),
            typed: BTreeMap::new(),
            unprobeable: BTreeMap::new(),
            held_null: BTreeMap::new(),
            metadata: M::default(),
        }
    }
}

impl<V, M> RowWrites<V, M> {
    /// A deleted row contributes neither a surviving reference nor an arrival
    /// from its earlier writes. Keep other rows' unknown assignments visible.
    pub fn delete(&mut self, key_column: &ColumnName, row: &RowKey) {
        self.key_column = key_column.to_owned();
        self.deleted.insert(row.clone());
        self.updated.remove(row);
        self.inserted.remove(row);
        self.typed.remove(row);
        self.held_null.remove(row);
        self.unprobeable.retain(|_, rows| {
            rows.remove(row);
            !rows.is_empty()
        });
    }

    /// Keep an unknown assignment visible: treating it as an untouched cell
    /// would compare the stored reference instead (DECISIONS 124, 165).
    pub fn update(&mut self, row: &RowKey, column: &ColumnName, value: Option<V>) {
        self.remember_unknown(row, column, &value);
        self.updated
            .entry(row.clone())
            .or_default()
            .insert(column.to_owned(), value);
    }

    pub fn insert(&mut self, row: &RowKey, column: &ColumnName, value: Option<V>) {
        self.remember_unknown(row, column, &value);
        if let Some(value) = value {
            self.inserted
                .entry(row.clone())
                .or_default()
                .insert(column.to_owned(), value);
        }
    }

    fn remember_unknown(&mut self, row: &RowKey, column: &ColumnName, value: &Option<V>) {
        if value.is_none() {
            self.unprobeable
                .entry(column.to_owned())
                .or_default()
                .insert(row.clone());
        }
    }

    pub fn has_writes(&self) -> bool {
        !self.inserted.is_empty() || !self.updated.is_empty()
    }

    pub fn gone_keys<'a>(&'a self, columns: &[ColumnName]) -> BTreeSet<&'a RowKey> {
        self.deleted
            .iter()
            .chain(
                self.updated
                    .iter()
                    .filter(|(_, cells)| cells.keys().any(|c| columns.contains(c)))
                    .map(|(key, _)| key),
            )
            .collect()
    }

    pub fn nulls_of(
        &self,
        row: &RowKey,
        stored_name: &impl Fn(&ColumnName) -> Option<ColumnName>,
        is_null: impl Fn(&V) -> bool,
    ) -> BTreeSet<ColumnName> {
        let mut out = BTreeSet::new();
        if let Some(columns) = self.updated.get(row) {
            out.extend(
                columns
                    .iter()
                    .filter(|(_, value)| value.as_ref().is_some_and(&is_null))
                    .filter_map(|(c, _)| stored_name(c)),
            );
        }
        if let Some(columns) = self.inserted.get(row) {
            out.extend(
                columns
                    .iter()
                    .filter(|(_, value)| is_null(value))
                    .filter_map(|(c, _)| stored_name(c)),
            );
        }
        if let Some(columns) = self.held_null.get(row) {
            out.extend(columns.iter().filter_map(stored_name));
        }
        out
    }
}

#[cfg(test)]
mod tests;
