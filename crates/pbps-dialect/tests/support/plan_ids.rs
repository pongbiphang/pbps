// Test fixtures predating PlanNames specify changes without a final ids file.
// Materialize that missing fixture input here, never in product code. The
// index's own acceptance tests use explicit ids independent of this helper.
use pbps_model::{Change, ChangeSet, ColumnRef, IdsFile, Uid, UidKind};
use std::collections::{BTreeMap, BTreeSet};

// Only identity-bearing variants supply ids; every other fixture change leaves them alone.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn ids(changes: &ChangeSet) -> IdsFile {
    let mut ids = IdsFile::default();
    let mut known = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for p in &changes.changes {
        let initial = match &p.change {
            Change::CreateTable { uid, name, .. } => {
                known.insert(name.clone());
                seen.insert(uid.clone());
                None
            }
            Change::RenameTable { uid, from, to, .. } => {
                known.extend([from.clone(), to.clone()]);
                Some((uid, from))
            }
            Change::DropTable { uid, name, .. } => Some((uid, name)),
            Change::AttachPartition { uid, table, .. }
            | Change::DetachPartition { uid, table, .. }
            | Change::SetTablePersistence { uid, table, .. }
            | Change::SetStorageParameters { uid, table, .. }
            | Change::SetReplicaIdentity { uid, table, .. }
            | Change::SetPartitionDefault { uid, table, .. }
            | Change::SetPartitionNotNull { uid, table, .. } => Some((uid, table)),
            _ => None,
        };
        if let Some((uid, name)) = initial {
            known.insert(name.clone());
            if seen.insert(uid.clone()) {
                ids.tables.insert(uid.clone(), name.clone());
            }
        }
    }
    for p in &changes.changes {
        if let Some(name) = p.change.table()
            && !known.contains(name)
        {
            ids.tables.insert(Uid::derived(UidKind::Table, &name.to_string(), 0), name.clone());
        }
    }
    let mut columns: BTreeMap<Uid, (Uid, String)> = BTreeMap::new();
    let mut generated = BTreeSet::new();
    for p in &changes.changes {
        let mut inline = None;
        match &p.change {
            Change::CreateTable { uid, name, table, .. } => {
                ids.tables.insert(uid.clone(), name.clone());
                inline = Some((uid, table.as_ref()));
            }
            Change::RenameTable { uid, to, .. } => { ids.tables.insert(uid.clone(), to.clone()); }
            Change::DropTable { uid, .. } => {
                ids.tables.remove(uid);
                columns.retain(|_, (owner, _)| owner != uid);
            }
            Change::AttachPartition { uid, .. } => { columns.retain(|_, (owner, _)| owner != uid); }
            Change::DetachPartition { uid, shape, .. } => { inline = Some((uid, shape.as_ref())); }
            Change::CreateRole { uid, name } | Change::RenameRole { uid, to: name, .. } => {
                ids.roles.insert(uid.clone(), name.clone());
            }
            Change::DropRole { uid, .. } => { ids.roles.remove(uid); }
            _ => {}
        }
        if let Some((owner, shape)) = inline {
            for name in shape.columns.keys() {
                let uid = Uid::derived(UidKind::Column, &format!("{owner}.{name}"), 0);
                generated.insert(uid.clone());
                columns.insert(uid, (owner.clone(), name.clone()));
            }
        }
        let column = match &p.change {
            Change::AddColumn { uid, table, name, .. } => Some((uid, table.column(name), Some(name.as_str()))),
            Change::RenameColumn { uid, table, from, to, .. } => Some((uid, table.column(from), Some(to.as_str()))),
            Change::DropColumn { uid, column } => Some((uid, column.clone(), None)),
            Change::AlterColumnType { uid, column, .. }
            | Change::AlterColumnNullability { uid, column, .. }
            | Change::AlterColumnDefault { uid, column, .. }
            | Change::AlterColumnExpression { uid, column, .. }
            | Change::SetColumnDeprecated { uid, column, .. } => Some((uid, column.clone(), Some(column.name.as_str()))),
            _ => None,
        };
        if let Some((uid, address, after)) = column {
            let owner = ids.tables.iter().find(|(_, name)| **name == address.table)
                .unwrap_or_else(|| panic!("fixture has no owner for {address}" )).0.clone();
            columns.retain(|old, (held, name)| !(generated.contains(old) && *held == owner && *name == address.name));
            if let Some(name) = after {
                columns.insert(uid.clone(), (owner, name.into()));
            } else {
                columns.remove(uid);
            }
        }
    }
    ids.columns = columns.into_iter().map(|(uid, (owner, name))| {
        (uid, ColumnRef { table: ids.tables[&owner].clone(), name })
    }).collect();
    ids
}

