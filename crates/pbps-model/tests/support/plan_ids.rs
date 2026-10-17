//! Saved-plan identities for schema-only guard and dependency fixtures.
//! Explicit UIDs remain authoritative: invalid reuse is a fixture error, never
//! a reason for production to fall back to an observed same-named object.

use pbps_model::{Change, ChangeSet, ColumnRef, IdsFile, Schema, TableName, Uid, UidKind};
use std::collections::{BTreeMap, BTreeSet};

type Event<A> = (usize, Uid, Option<A>, Option<A>);

#[test]
fn baseline_fixture_table_identities_use_raw_name_seeds() {
    let part = "a\nb\\c\u{200b}";
    let name = TableName::new("app", part);
    let mut before = Schema::default();
    before.tables.insert(name.clone(), Default::default());
    let actual = ids(&ChangeSet::default(), &before);
    assert_eq!(
        actual.table_uid(&name),
        Some(&Uid::derived(UidKind::Table, &format!("app.{part}"), 0))
    );
    assert!(
        actual
            .table_uid(&TableName::new("app", r"a\nb\c\u{200b}"))
            .is_none()
    );
}

// This test-data builder only supplies identities; unrelated changes need no
// fixture action. PlanNames still validates every explicit source and result.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn ids(changes: &ChangeSet, before: &Schema) -> IdsFile {
    let mut tables: Vec<Event<TableName>> = Vec::new();
    let mut columns: Vec<Event<ColumnRef>> = Vec::new();
    let mut roles: Vec<Event<String>> = Vec::new();
    let mut scopes = Vec::new();
    for (i, p) in changes.changes.iter().enumerate() {
        match &p.change {
            Change::CreateTable { uid, name, .. } => {
                tables.push((i, uid.clone(), None, Some(name.clone())))
            }
            Change::DropTable { uid, name, .. } => {
                tables.push((i, uid.clone(), Some(name.clone()), None))
            }
            Change::RenameTable { uid, from, to, .. } => {
                tables.push((i, uid.clone(), Some(from.clone()), Some(to.clone())))
            }
            Change::SetTablePersistence { uid, table, .. }
            | Change::SetStorageParameters { uid, table, .. }
            | Change::SetReplicaIdentity { uid, table, .. }
            | Change::SetPartitionDefault { uid, table, .. }
            | Change::SetPartitionNotNull { uid, table, .. }
            | Change::AttachPartition { uid, table, .. }
            | Change::DetachPartition { uid, table, .. } => {
                tables.push((i, uid.clone(), Some(table.clone()), Some(table.clone())))
            }
            Change::AddColumn {
                uid, table, name, ..
            } => columns.push((i, uid.clone(), None, Some(table.column(name)))),
            Change::DropColumn { uid, column } => {
                columns.push((i, uid.clone(), Some(column.clone()), None))
            }
            Change::RenameColumn {
                uid,
                table,
                from,
                to,
                ..
            } => columns.push((
                i,
                uid.clone(),
                Some(table.column(from)),
                Some(table.column(to)),
            )),
            Change::AlterColumnType { uid, column, .. }
            | Change::AlterColumnNullability { uid, column, .. }
            | Change::AlterColumnDefault { uid, column, .. }
            | Change::AlterColumnExpression { uid, column, .. }
            | Change::AlterColumnNumbering { uid, column, .. }
            | Change::SetColumnDeprecated { uid, column, .. } => {
                columns.push((i, uid.clone(), Some(column.clone()), Some(column.clone())))
            }
            Change::CreateRole { uid, name } => {
                roles.push((i, uid.clone(), None, Some(name.clone())))
            }
            Change::DropRole { uid, name, .. } => {
                roles.push((i, uid.clone(), Some(name.clone()), None))
            }
            Change::RenameRole { uid, from, to } => {
                roles.push((i, uid.clone(), Some(from.clone()), Some(to.clone())))
            }
            _ => {}
        }
        match &p.change {
            Change::AttachPartition { table, .. } => scopes.push((i, table, None)),
            Change::CreateTable { name, table, .. } => scopes.push((i, name, Some(table.as_ref()))),
            Change::DetachPartition { table, shape, .. } => {
                scopes.push((i, table, Some(shape.as_ref())))
            }
            _ => {}
        }
    }
    let initial = |events: &[Event<TableName>]| {
        let mut seed = BTreeMap::new();
        for (_, uid, from, _) in events {
            seed.entry(uid.clone()).or_insert_with(|| from.clone());
        }
        seed.into_iter()
            .filter_map(|(uid, name)| name.map(|name| (uid, name)))
            .collect::<BTreeMap<_, _>>()
    };
    let mut start = initial(&tables);
    let mentioned: BTreeSet<_> = tables
        .iter()
        .flat_map(|(_, _, from, to)| from.iter().chain(to))
        .cloned()
        .collect();
    for name in before.tables.keys().chain(
        columns
            .iter()
            .flat_map(|(_, _, from, to)| from.iter().chain(to).map(|c| &c.table)),
    ) {
        if !mentioned.contains(name) {
            start
                .entry(Uid::derived(UidKind::Table, &name.raw().to_string(), 0))
                .or_insert_with(|| name.clone());
        }
    }
    let at = |position: usize| {
        let mut held = start.clone();
        for (i, uid, _, to) in &tables {
            if *i >= position {
                break;
            }
            match to {
                Some(to) => {
                    held.insert(uid.clone(), to.clone());
                }
                None => {
                    held.remove(uid);
                }
            }
        }
        held
    };
    let end = at(changes.changes.len());
    let mut ids = IdsFile {
        tables: end.clone(),
        ..Default::default()
    };
    let owner = |name: &TableName, position| {
        at(position)
            .into_iter()
            .find_map(|(uid, n)| (n == *name).then_some(uid))
    };
    let mut affected = BTreeSet::new();
    let mut column_end = BTreeMap::new();
    for (i, uid, from, to) in &columns {
        for address in from.iter().chain(to) {
            let table = owner(&address.table, *i).expect("fixture column owner");
            affected.insert((table, address.name.clone()));
        }
        column_end.insert(
            uid.clone(),
            to.as_ref()
                .map(|c| (owner(&c.table, *i).unwrap(), c.name.clone())),
        );
    }
    // Untouched columns in a baseline are real identities too. A staged
    // closing baseline may already carry the final spelling of its table.
    for (name, table) in &before.tables {
        let Some(uid) = owner(name, 0).or_else(|| owner(name, changes.changes.len())) else {
            continue;
        };
        if table.partition_of.is_some() {
            continue;
        }
        for column in table.columns.keys() {
            if !affected.contains(&(uid.clone(), column.clone())) {
                column_end.insert(
                    Uid::derived(UidKind::Column, &format!("{uid}/{column}"), 0),
                    Some((uid.clone(), column.clone())),
                );
            }
        }
    }
    for (i, table, shape) in scopes {
        let Some(uid) = owner(table, i + 1) else {
            continue;
        };
        match shape {
            None => column_end.retain(|_, address| address.as_ref().is_none_or(|(t, _)| t != &uid)),
            Some(shape) => {
                for name in shape.columns.keys() {
                    if !affected.contains(&(uid.clone(), name.clone())) {
                        column_end.insert(
                            Uid::derived(UidKind::Column, &format!("{uid}/{name}"), 0),
                            Some((uid.clone(), name.clone())),
                        );
                    }
                }
            }
        }
    }
    for (uid, address) in column_end {
        if let Some((table, name)) = address
            && let Some(table) = end.get(&table)
        {
            ids.columns.insert(uid, table.column(name));
        }
    }
    for (_, uid, _, to) in roles {
        match to {
            Some(to) => {
                ids.roles.insert(uid, to);
            }
            None => {
                ids.roles.remove(&uid);
            }
        }
    }
    ids
}
