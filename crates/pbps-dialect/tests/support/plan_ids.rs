// Test fixtures predating PlanNames specify changes without a final ids file.
// Materialize that missing fixture input here, never in product code. The
// index's own acceptance tests use explicit ids independent of this helper.
use pbps_model::{Change, ChangeSet, ColumnRef, IdsFile, Uid, UidKind};
use pbps_model::RoleChange;
use pbps_model::TypeValueChange;
use pbps_model::ColumnChange;
use pbps_model::PartitionChange;
use pbps_model::TablePropertyChange;
use pbps_model::TableChange;
use std::collections::{BTreeMap, BTreeSet};

// Only identity-bearing variants supply ids; every other fixture change leaves them alone.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn ids(changes: &ChangeSet) -> IdsFile {
    let mut ids = IdsFile::default();
    let mut known = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for p in &changes.changes {
        let initial = match &p.change {
            Change::Table(TableChange::CreateTable { uid, name, .. }) => {
                known.insert(name.clone());
                seen.insert(uid.clone());
                None
            }
            Change::Table(TableChange::RenameTable { uid, from, to, .. }) => {
                known.extend([from.clone(), to.clone()]);
                Some((uid, from))
            }
            Change::Table(TableChange::DropTable { uid, name, .. }) => Some((uid, name)),
            Change::Partition(PartitionChange::AttachPartition { uid, table, .. })
            | Change::Partition(PartitionChange::DetachPartition { uid, table, .. })
            | Change::TableProperty(TablePropertyChange::SetTablePersistence { uid, table, .. })
            | Change::TableProperty(TablePropertyChange::SetStorageParameters { uid, table, .. })
            | Change::TableProperty(TablePropertyChange::SetReplicaIdentity { uid, table, .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { uid, table, .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { uid, table, .. }) => Some((uid, table)),
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
            ids.tables.insert(Uid::derived(UidKind::Table, &name.raw().to_string(), 0), name.clone());
        }
    }
    let mut columns: BTreeMap<Uid, (Uid, pbps_model::ColumnName)> = BTreeMap::new();
    let mut generated = BTreeSet::new();
    for p in &changes.changes {
        let mut inline = None;
        match &p.change {
            Change::Table(TableChange::CreateTable { uid, name, table, .. }) => {
                ids.tables.insert(uid.clone(), name.clone());
                inline = Some((uid, table.as_ref()));
            }
            Change::Table(TableChange::RenameTable { uid, to, .. }) => { ids.tables.insert(uid.clone(), to.clone()); }
            Change::Table(TableChange::DropTable { uid, .. }) => {
                ids.tables.remove(uid);
                columns.retain(|_, (owner, _)| owner != uid);
            }
            Change::Partition(PartitionChange::AttachPartition { uid, .. }) => { columns.retain(|_, (owner, _)| owner != uid); }
            Change::Partition(PartitionChange::DetachPartition { uid, shape, .. }) => { inline = Some((uid, shape.as_ref())); }
            Change::Role(RoleChange::CreateRole { uid, name }) | Change::Role(RoleChange::RenameRole { uid, to: name, .. }) => {
                ids.roles.insert(uid.clone(), name.clone());
            }
            Change::Role(RoleChange::DropRole { uid, .. }) => { ids.roles.remove(uid); }
            _ => {}
        }
        if let Some((owner, shape)) = inline {
            for name in shape.columns.keys() {
                let raw_name = name.raw();
                let uid = Uid::derived(UidKind::Column, &format!("{owner}.{raw_name}"), 0);
                generated.insert(uid.clone());
                columns.insert(uid, (owner.clone(), name.clone()));
            }
        }
        let column = match &p.change {
            Change::Column(ColumnChange::AddColumn { uid, table, name, .. }) => Some((uid, table.column(name), Some(name))),
            Change::Column(ColumnChange::RenameColumn { uid, table, from, to, .. }) => Some((uid, table.column(from), Some(to))),
            Change::Column(ColumnChange::DropColumn { uid, column }) => Some((uid, column.clone(), None)),
            Change::TypeValue(TypeValueChange::EnumRebuild { step: pbps_model::EnumRebuildStep::Retype { column_uid: uid, column, .. }, .. })
            | Change::Column(ColumnChange::AlterColumnType { uid, column, .. })
            | Change::Column(ColumnChange::AlterColumnNullability { uid, column, .. })
            | Change::Column(ColumnChange::AlterColumnDefault { uid, column, .. })
            | Change::Column(ColumnChange::AlterColumnExpression { uid, column, .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { uid, column, .. }) => Some((uid, column.clone(), Some(&column.name))),
            _ => None,
        };
        if let Some((uid, address, after)) = column {
            let owner = ids.tables.iter().find(|(_, name)| **name == address.table)
                .unwrap_or_else(|| panic!("fixture has no owner for {address}" , address = address.raw())).0.clone();
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

#[test]
fn fixture_table_identities_use_raw_name_seeds() {
    let part = "a\nb\\c\u{200b}";
    let name = pbps_model::TableName::new("app", part);
    let changes = ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(Change::Column(ColumnChange::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(), table: name.clone(), name: part.into(),
            column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
        }))],
    };
    let actual = ids(&changes);
    assert_eq!(actual.table_uid(&name), Some(&Uid::derived(UidKind::Table, &format!("app.{part}"), 0)));
    assert_eq!(actual.column_uid(&name.column(part)), Some(&"c_aaaaaa".parse().unwrap()));
    assert!(actual.table_uid(&pbps_model::TableName::new("app", r"a\nb\c\u{200b}")).is_none());
}

