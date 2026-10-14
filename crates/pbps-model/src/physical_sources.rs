//! Physical column continuity is independent of managed column UID lifetime
//! (DEC-1875.1).
//!
//! A partition owns no column UIDs, but it has catalog columns before DETACH
//! and after ATTACH. The baseline supplies that relation; the plan supplies
//! explicit creation, removal and rename intent. No catalog allocation or
//! SQL semantics are inferred here.

use std::collections::BTreeMap;

use crate::plan_names::{At, Holder, PlanNames};
use crate::{Change, ColumnRef, IdsFile, Schema, TableName, Uid};

/// Evidence about a column's physical origin, rather than its managed scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source<A> {
    Stored(A),
    Created { step: usize },
    Retired,
    Unavailable,
}

/// Where a catalog column is now, and which typed change can address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Column(Uid),
    Partition { parent: TableName },
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnAt {
    /// The table identity survives a change in managed column ownership.
    pub table_uid: Uid,
    pub address: ColumnRef,
    pub owner: Owner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located<A = ColumnAt> {
    Present(A),
    Retired,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
#[error("physical column sources: {0}")]
pub struct SourceError(String);

// This is not a second managed identity. A creation event distinguishes two
// physical columns even if a typed plan reuses the same address or UID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Stored(ColumnRef),
    Created { step: usize, slot: Slot },
}

type Slot = (Uid, String);
type Events<V> = Vec<(usize, Option<V>)>;

#[derive(Debug)]
struct History<K, V>(BTreeMap<K, Events<V>>);

impl<K: Ord, V> Default for History<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Ord, V> History<K, V> {
    fn put(&mut self, key: K, boundary: usize, value: Option<V>) {
        self.0.entry(key).or_default().push((boundary, value));
    }

    // An unknown history and a known removal deliberately have different
    // answers. A reader may skip a newly created source, never missing proof.
    fn get(&self, key: &K, boundary: usize) -> Option<&Option<V>> {
        let events = self.0.get(key)?;
        let n = events.partition_point(|(at, _)| *at <= boundary);
        n.checked_sub(1).map(|i| &events[i].1)
    }
}

/// Sparse histories tied to the same immutable plan as the managed index.
/// A rewrite must discard both views before mutating their borrowed changes.
#[derive(Debug)]
pub struct PhysicalSources<'p> {
    names: PlanNames<'p>,
    holders: History<Slot, Origin>,
    positions: History<Origin, Slot>,
    partitions: History<Uid, Uid>,
    managed: History<Origin, Uid>,
    table_births: History<Uid, usize>,
}

struct Build<'a, 'p> {
    view: &'a mut PhysicalSources<'p>,
    columns: BTreeMap<Slot, Origin>,
    parents: BTreeMap<Uid, Uid>,
}

impl<'p> PhysicalSources<'p> {
    pub fn of(
        names: PlanNames<'p>,
        baseline: &Schema,
        baseline_ids: &IdsFile,
    ) -> Result<Self, SourceError> {
        baseline_ids
            .validate()
            .map_err(|error| SourceError(error.to_string()))?;
        let changes = names.changes();
        let initial_columns: Vec<_> = names
            .columns_at(At::Start)
            .map(|(uid, column)| (uid.clone(), column))
            .collect();
        let mut view = Self {
            names,
            holders: History::default(),
            positions: History::default(),
            partitions: History::default(),
            managed: History::default(),
            table_births: History::default(),
        };
        let mut build = Build {
            view: &mut view,
            columns: BTreeMap::new(),
            parents: BTreeMap::new(),
        };
        // Start identity is sufficient evidence for an ordinary column even
        // when the connected reader is checking an old object now missing
        // from the catalog. Its eventual catalog lookup must still fail.
        for (uid, column) in initial_columns {
            let owner = build.owner(&column.table, At::Start)?;
            let origin = Origin::Stored(column.clone());
            build.insert(0, (owner, column.name.clone()), origin.clone())?;
            if baseline_ids
                .column_uid(&column)
                .is_some_and(|before| before != &uid)
            {
                return Err(SourceError(format!(
                    "baseline identity for {column} disagrees with the plan"
                )));
            }
            build.view.managed.put(origin, 0, Some(uid.clone()));
        }
        for (name, table) in &baseline.tables {
            let Ok(owner) = build.owner(name, At::Start) else {
                continue;
            };
            if baseline_ids
                .table_uid(name)
                .is_some_and(|before| before != &owner)
            {
                return Err(SourceError(format!(
                    "baseline identity for {name} disagrees with the plan"
                )));
            }
            let shape = if let Some(partition) = &table.partition_of {
                let Ok(parent) = build.owner(&partition.parent, At::Start) else {
                    continue;
                };
                build.parent(0, owner.clone(), Some(parent.clone()));
                let Some(parent) = baseline.tables.get(&partition.parent) else {
                    continue;
                };
                parent
            } else {
                table
            };
            for column in shape.columns.keys().chain(shape.computed.keys()) {
                let address = name.column(column);
                build.insert(
                    0,
                    (owner.clone(), column.clone()),
                    Origin::Stored(address.clone()),
                )?;
                if table.partition_of.is_none()
                    && let Some(uid) = baseline_ids.column_uid(&address)
                {
                    build
                        .view
                        .managed
                        .put(Origin::Stored(address), 0, Some(uid.clone()));
                }
            }
        }
        for (step, planned) in changes.iter().enumerate() {
            build.apply(step, &planned.change)?;
        }
        Ok(view)
    }

    pub fn names(&self) -> &PlanNames<'p> {
        &self.names
    }

    pub fn table_source(&self, table: &TableName, at: At) -> Source<TableName> {
        let owner = match self.names.holder(table, at) {
            Holder::Tracked(uid) => uid,
            Holder::Vacant => return Source::Retired,
            Holder::Unmanaged => return Source::Unavailable,
        };
        match self.table_births.get(owner, self.names.boundary(at)) {
            Some(Some(step)) => Source::Created { step: *step },
            Some(None) => Source::Retired,
            None => match self.names.table_at(owner, At::Start) {
                Some(name) => Source::Stored(name.clone()),
                None => Source::Unavailable,
            },
        }
    }

    pub fn source(&self, column: &ColumnRef, at: At) -> Source<ColumnRef> {
        let owner = match self.names.holder(&column.table, at) {
            Holder::Tracked(uid) => uid,
            Holder::Vacant => return Source::Retired,
            Holder::Unmanaged => return Source::Unavailable,
        };
        match self.holders.get(
            &(owner.clone(), column.name.clone()),
            self.names.boundary(at),
        ) {
            Some(Some(Origin::Stored(stored))) => Source::Stored(stored.clone()),
            Some(Some(Origin::Created { step, .. })) => Source::Created { step: *step },
            Some(None) => Source::Retired,
            None => Source::Unavailable,
        }
    }

    /// Follow the original table, refusing to bind a dependent to a replacement.
    pub fn locate_table(&self, stored: &TableName, at: At) -> Located<TableName> {
        let Holder::Tracked(uid) = self.names.holder(stored, At::Start) else {
            return Located::Unavailable;
        };
        let Some(name) = self.names.table_at(uid, at) else {
            return Located::Retired;
        };
        match self.table_source(name, at) {
            Source::Stored(original) if original == *stored => Located::Present(name.clone()),
            Source::Stored(_) | Source::Created { .. } | Source::Retired => Located::Retired,
            Source::Unavailable => Located::Unavailable,
        }
    }

    /// Follow one original catalog column, never the next holder of its name.
    pub fn locate(&self, stored: &ColumnRef, at: At) -> Located {
        let slot = match self
            .positions
            .get(&Origin::Stored(stored.clone()), self.names.boundary(at))
        {
            Some(Some(slot)) => slot,
            Some(None) => return Located::Retired,
            None => return Located::Unavailable,
        };
        let Some(table) = self.names.table_at(&slot.0, at) else {
            return Located::Retired;
        };
        let address = table.column(&slot.1);
        let owner = match self.partitions.get(&slot.0, self.names.boundary(at)) {
            Some(Some(parent)) => match self.names.table_at(parent, at) {
                Some(parent) => Owner::Partition {
                    parent: parent.clone(),
                },
                None => Owner::Unavailable,
            },
            Some(None) | None => match self.names.column_holder(&address, at) {
                Holder::Tracked(uid) => Owner::Column(uid.clone()),
                Holder::Unmanaged | Holder::Vacant => match self
                    .managed
                    .get(&Origin::Stored(stored.clone()), self.names.boundary(at))
                {
                    Some(Some(uid)) => Owner::Column(uid.clone()),
                    Some(None) | None => Owner::Unavailable,
                },
            },
        };
        Located::Present(ColumnAt {
            table_uid: slot.0.clone(),
            address,
            owner,
        })
    }
}

impl Build<'_, '_> {
    fn owner(&self, name: &TableName, at: At) -> Result<Uid, SourceError> {
        match self.view.names.holder(name, at) {
            Holder::Tracked(uid) => Ok(uid.clone()),
            Holder::Unmanaged | Holder::Vacant => Err(SourceError(format!(
                "no managed table holds {name} at {at:?}"
            ))),
        }
    }

    fn insert(&mut self, boundary: usize, slot: Slot, origin: Origin) -> Result<(), SourceError> {
        if let Some(held) = self.columns.get(&slot) {
            if boundary == 0 && held == &origin {
                return Ok(());
            }
            return Err(SourceError(format!(
                "column {}.{} has two physical sources at boundary {boundary}",
                slot.0, slot.1
            )));
        }
        self.columns.insert(slot.clone(), origin.clone());
        self.view
            .holders
            .put(slot.clone(), boundary, Some(origin.clone()));
        self.view.positions.put(origin, boundary, Some(slot));
        Ok(())
    }

    fn remove(&mut self, boundary: usize, slot: &Slot) -> Option<Origin> {
        let origin = self.columns.remove(slot);
        self.view.holders.put(slot.clone(), boundary, None);
        if let Some(origin) = &origin {
            self.view.positions.put(origin.clone(), boundary, None);
        }
        origin
    }

    fn parent(&mut self, boundary: usize, table: Uid, parent: Option<Uid>) {
        for (slot, origin) in &self.columns {
            if slot.0 != table {
                continue;
            }
            // Scope entry/exit does not change the physical source. It does
            // terminate the baseline ordinary UID, even if a later DETACH
            // introduces another UID under exactly the same spelling.
            let uid = if parent.is_none() {
                let at = boundary.checked_sub(1).map_or(At::Start, At::After);
                self.view.names.table_at(&table, at).and_then(|name| {
                    match self.view.names.column_holder(&name.column(&slot.1), at) {
                        Holder::Tracked(uid) => Some(uid.clone()),
                        Holder::Unmanaged | Holder::Vacant => None,
                    }
                })
            } else {
                None
            };
            self.view.managed.put(origin.clone(), boundary, uid);
        }
        match &parent {
            Some(parent) => {
                self.parents.insert(table.clone(), parent.clone());
            }
            None => {
                self.parents.remove(&table);
            }
        }
        self.view.partitions.put(table, boundary, parent);
    }

    fn reached(&self, table: &Uid) -> Vec<Uid> {
        std::iter::once(table.clone())
            .chain(
                self.parents
                    .iter()
                    .filter(|(_, parent)| *parent == table)
                    .map(|(child, _)| child.clone()),
            )
            .collect()
    }

    fn create(&mut self, step: usize, slot: Slot) -> Result<(), SourceError> {
        self.insert(step + 1, slot.clone(), Origin::Created { step, slot })
    }

    // This classification concerns physical continuity only. Managed name
    // transitions stay exhaustive in PlanNames; the consumers have no replay.
    fn apply(&mut self, step: usize, change: &Change) -> Result<(), SourceError> {
        let before = At::Before(step);
        let boundary = step + 1;
        match change {
            Change::CreateTable { uid, table, .. } => {
                self.view
                    .table_births
                    .put(uid.clone(), boundary, Some(step));
                let columns: Vec<String> = if let Some(partition) = &table.partition_of {
                    let parent = self.owner(&partition.parent, before)?;
                    let columns = self
                        .columns
                        .keys()
                        .filter(|(owner, _)| *owner == parent)
                        .map(|(_, column)| column.clone())
                        .collect();
                    self.parent(boundary, uid.clone(), Some(parent));
                    columns
                } else {
                    table
                        .columns
                        .keys()
                        .chain(table.computed.keys())
                        .cloned()
                        .collect()
                };
                for column in columns {
                    self.create(step, (uid.clone(), column))?;
                }
            }
            Change::DropTable { uid, .. } => {
                self.view.table_births.put(uid.clone(), boundary, None);
                let columns: Vec<_> = self
                    .columns
                    .keys()
                    .filter(|(owner, _)| owner == uid)
                    .cloned()
                    .collect();
                for column in columns {
                    self.remove(boundary, &column);
                }
                self.parent(boundary, uid.clone(), None);
            }
            Change::AttachPartition { uid, parent, .. } => {
                let parent = self.owner(parent, before)?;
                self.parent(boundary, uid.clone(), Some(parent));
            }
            Change::DetachPartition { uid, .. } => self.parent(boundary, uid.clone(), None),
            Change::AddColumn { table, name, .. }
            | Change::AddComputedColumn { table, name, .. } => {
                let table = self.owner(table, before)?;
                for owner in self.reached(&table) {
                    self.create(step, (owner, name.clone()))?;
                }
            }
            Change::DropColumn { column, .. } => {
                let table = self.owner(&column.table, before)?;
                for owner in self.reached(&table) {
                    self.remove(boundary, &(owner, column.name.clone()));
                }
            }
            Change::DropComputedColumn { table, name, .. } => {
                let table = self.owner(table, before)?;
                for owner in self.reached(&table) {
                    self.remove(boundary, &(owner, name.clone()));
                }
            }
            Change::RenameColumn {
                table, from, to, ..
            } => {
                let table = self.owner(table, before)?;
                for owner in self.reached(&table) {
                    let old = (owner.clone(), from.clone());
                    let Some(origin) = self.remove(boundary, &old) else {
                        return Err(SourceError(format!(
                            "source of column {}.{} is unavailable before change {step}",
                            old.0, old.1
                        )));
                    };
                    self.insert(boundary, (owner, to.clone()), origin)?;
                }
            }
            Change::RenameTable { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
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
            | Change::PublicExecution { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
