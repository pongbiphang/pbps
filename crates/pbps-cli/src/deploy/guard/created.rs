use pbps_model::ColumnName;
use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    Change, ChangeSet, ReplicaIdentity, Table, TableName, Uid, ValueSource,
    plan_names::{At, Holder, PlanNames},
};

use super::super::{Settled, index_as_declared, primary_key_as_declared};
use super::{Fields, PartChanges, TableChanges, parts::final_table};

/// Attach/detach adopt a declared shape, but neither acquires CREATE's
/// placement or replica-identity policy (DEC-1544.1, DEC-1545.1).
enum Kind<'a> {
    Create(&'a Uid),
    Attach,
    Detach,
}

pub(crate) struct Created<'a> {
    declared: &'a Table,
    finished: Table,
    kind: Kind<'a>,
    incompatible_defaults: BTreeSet<ColumnName>,
    identity: Option<&'a ReplicaIdentity>,
}

pub(crate) struct CreatedChanges<'a>(BTreeMap<TableName, Created<'a>>);

impl<'a> CreatedChanges<'a> {
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn of(
        changes: &'a ChangeSet,
        names: &PlanNames<'_>,
        scalars: &TableChanges<'a>,
        parts: &PartChanges<'a>,
    ) -> Self {
        let mut tables = BTreeMap::new();
        for (i, p) in changes.changes.iter().enumerate() {
            let shape = match &p.change {
                Change::CreateTable {
                    uid, name, table, ..
                } => Some((name, table.as_ref(), Kind::Create(uid), At::After(i))),
                Change::AttachPartition { table, shape, .. } => {
                    Some((table, shape.as_ref(), Kind::Attach, At::Before(i)))
                }
                Change::DetachPartition { table, shape, .. } => {
                    Some((table, shape.as_ref(), Kind::Detach, At::Before(i)))
                }
                // These changes establish or adopt a shape. A CREATE's
                // typed column parts complete its inventory below.
                _ => None,
            };
            if let Some((address, declared, kind, at)) = shape
                && let Some(name) = final_table(names, address, at)
            {
                let identity = match kind {
                    Kind::Create(uid) => scalars
                        .created_identity(uid)
                        .unwrap_or(declared.replica_identity.as_ref()),
                    Kind::Attach | Kind::Detach => declared.replica_identity.as_ref(),
                };
                tables.insert(
                    name,
                    Created {
                        declared,
                        finished: declared.clone(),
                        kind,
                        incompatible_defaults: BTreeSet::new(),
                        identity,
                    },
                );
            }
        }
        for (i, p) in changes.changes.iter().enumerate() {
            let at = At::Before(i);
            if let Change::AddColumn {
                table,
                name,
                column,
                ..
            } = &p.change
                && let Some(table_name) = final_table(names, table, at)
                && let Some(created) = tables.get_mut(&table_name)
                && matches!(created.kind, Kind::Create(_))
                && let Some(name) = final_column(names, table, name, At::After(i))
            {
                // A new table may be a prefix followed by ADD COLUMN
                // parts (DEC-1503.1). This adds only their declared names;
                // columns_after still checks each part's field promises.
                created.finished.columns.insert(name, (**column).clone());
            }
            if let Change::AlterColumnDefault { column, to, .. } = &p.change
                && let Some(table) = final_table(names, &column.table, at)
                && let Some(created) = tables.get_mut(&table)
                && let Some(column) = final_column(names, &column.table, &column.name, at)
                && let Some(value) = created.finished.columns.get_mut(&column)
            {
                let compatible = match &value.value_source {
                    ValueSource::None | ValueSource::Default(_) => {
                        value.value_source =
                            to.clone().map_or(ValueSource::None, ValueSource::Default);
                        true
                    }
                    ValueSource::Identity(_)
                    | ValueSource::Sequence(_)
                    | ValueSource::Generated(_) => to.is_none(),
                };
                if compatible {
                    created.incompatible_defaults.remove(&column);
                } else {
                    created.incompatible_defaults.insert(column);
                }
            }
            let own = match &p.change {
                Change::SetPartitionDefault {
                    table, column, to, ..
                } => Some((table, column, Some(to), None)),
                Change::SetPartitionNotNull {
                    table,
                    column,
                    not_null,
                    ..
                } => Some((table, column, None, Some(*not_null))),
                _ => None,
            };
            if let Some((table, column, default, not_null)) = own
                && let Some(table) = final_table(names, table, at)
                && let Some(created) = tables.get_mut(&table)
                && let Some(of) = created.finished.partition_of.as_mut()
                && let Some(column) = final_column(names, &of.parent, column, at)
            {
                let own = of.columns.entry(column).or_default();
                if let Some(default) = default {
                    own.default = default.clone();
                }
                if let Some(not_null) = not_null {
                    own.not_null = not_null;
                }
            }
        }
        for (name, created) in &mut tables {
            parts.created(name, &mut created.finished);
        }
        Self(tables)
    }

    pub(crate) fn get(&self, name: &TableName) -> Option<&Created<'a>> {
        self.0.get(name)
    }
}

fn final_column(
    names: &PlanNames<'_>,
    table: &TableName,
    column: &ColumnName,
    at: At,
) -> Option<ColumnName> {
    match names.column_holder(&table.column(column), at) {
        Holder::Tracked(uid) => names
            .column_at(uid, At::End)
            .map(|(_, name)| name.to_owned()),
        Holder::Unmanaged => Some(column.to_owned()),
        Holder::Vacant => None,
    }
}

impl Created<'_> {
    pub(crate) fn table(&self) -> &Table {
        self.declared
    }
    pub(crate) fn attached(&self) -> bool {
        matches!(self.kind, Kind::Attach)
    }
    pub(crate) fn detached(&self) -> bool {
        matches!(self.kind, Kind::Detach)
    }

    pub(crate) fn check(
        &self,
        dialect: &dyn pbps_dialect::Dialect,
        now_name: &TableName,
        now: &Fields<'_>,
        settled: Settled,
        moved: &mut Vec<String>,
    ) {
        let declared = self.declared;
        // No policy operation can own these fields yet (DEC-1172.1).
        if &declared.row_security != now.row_security || &declared.row_policies != now.row_policies
        {
            moved.push(format!(
                "{} row-security flags or policies are not what this plan declares",
                now_name.shown()
            ));
        }
        let mut named = |kind: &str, was: BTreeSet<&str>, now: BTreeSet<&str>| {
            for extra in now.difference(&was) {
                moved.push(format!(
                    "{now_name} {kind} `{extra}` is there, and this plan declares no such \
                     {kind}",
                    now_name = now_name.shown(),
                    extra = pbps_model::escape::Escaped(extra),
                ));
            }
            // Only once every statement has run: a foreign key is split
            // out of the `CREATE` into a change of its own, and at a
            // checkpoint it may not have been added yet.
            if settled.whole() {
                for missing in was.difference(&now) {
                    moved.push(format!(
                        "{now_name} {kind} `{missing}` is not there, and this plan's \
                         `CREATE TABLE` declares it",
                        now_name = now_name.shown(),
                        missing = pbps_model::escape::Escaped(missing),
                    ));
                }
            }
        };
        // The complete inventory includes typed ADD COLUMN parts without
        // granting ownership of any other arriving column.
        named(
            "column",
            self.finished.columns.keys().map(ColumnName::raw).collect(),
            now.columns.keys().map(ColumnName::raw).collect(),
        );
        named(
            "unique",
            self.finished.unique.keys().map(String::as_str).collect(),
            now.unique.keys().map(String::as_str).collect(),
        );
        named(
            "foreign key",
            self.finished
                .foreign_keys
                .keys()
                .map(String::as_str)
                .collect(),
            now.foreign_keys.keys().map(String::as_str).collect(),
        );
        named(
            "check",
            self.finished.checks.keys().map(String::as_str).collect(),
            now.checks.keys().map(String::as_str).collect(),
        );
        named(
            "index",
            self.finished.indexes.keys().map(String::as_str).collect(),
            now.indexes.keys().map(String::as_str).collect(),
        );
        // Created inside the `CREATE TABLE`, never split out of it, so the
        // payload names every one (#1174 review).
        named(
            "computed column",
            self.finished.computed.keys().map(ColumnName::raw).collect(),
            now.computed.keys().map(ColumnName::raw).collect(),
        );
        // A column's own promise, in the fields the catalog reads back
        // unchanged — the type once normalized, measured: the live
        // created-table test declares bare `decimal`, `char`, `float` and
        // `nvarchar` so a raw comparison would refuse it again
        // (DECISIONS 185, 186).
        for (n, was) in &declared.columns {
            let Some(now) = now.columns.get(n) else {
                continue;
            };
            let finished = &self.finished.columns[n];
            let as_declared = (!self.incompatible_defaults.contains(n)
                && super::declared_column(dialect, finished, now))
                || (!settled.whole() && super::declared_column(dialect, was, now));
            if !as_declared {
                moved.push(format!(
                    "{now_name} column `{n}` is not the one this plan's `CREATE TABLE` \
                     declares",
                    now_name = now_name.shown(),
                    n = n.shown(),
                ));
            }
        }
        if matches!(self.kind, Kind::Create(_)) {
            super::created_place(declared, now, now_name, moved);
        }
        // A partition's own defaults and NOT NULLs (#1578), as a column's:
        // NOT NULL exactly, a default by presence, the engine respelling
        // its text. One read back that the plan does not declare is
        // somebody else's too.
        let own = |partition: Option<&pbps_model::PartitionOf>| {
            partition
                .map(|of| {
                    of.columns
                        .iter()
                        .map(|(n, c)| (n.clone(), (c.default.is_some(), c.not_null)))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default()
        };
        // An attached table's parent and range, which no other part of
        // the shape names: another session can detach it and attach it
        // elsewhere between two reads (#1545).
        let place = |partition: Option<&pbps_model::PartitionOf>| {
            partition.map(|of| (of.parent.clone(), of.bound.clone()))
        };
        if self.attached()
            && place(declared.partition_of.as_ref()) != place(now.partition_of.as_ref())
        {
            moved.push(format!(
                "{now_name} is not attached to the parent and range this plan declares",
                now_name = now_name.shown()
            ));
        }
        // With the defaults this plan sets after the `CREATE`, as a
        // column's above.
        let mut finished = own(self.finished.partition_of.as_ref());
        finished.retain(|_, held| *held != (false, false));
        if settled.whole() && finished != own(now.partition_of.as_ref()) {
            moved.push(format!(
                "{now_name}'s own column defaults or NOT NULLs are not the ones this plan's \
                 `CREATE TABLE` declares",
                now_name = now_name.shown()
            ));
        }
        // A computed column is held to its persistence, and to NOT NULL
        // where it declares one; its expression comes back respelled, as
        // a check's does (DEC-1174.1).
        for (n, was) in &self.finished.computed {
            if let Some(now) = now.computed.get(n)
                && !(now.persisted == was.persisted && (!was.not_null || now.not_null))
            {
                moved.push(format!(
                    "{now_name} computed column `{n}` is not the one this plan's `CREATE \
                     TABLE` declares",
                    now_name = now_name.shown(),
                    n = n.shown(),
                ));
            }
        }
        // And what those parts *are*, where the declaration says it
        // without the engine's help. Only structure: a check is nothing
        // but an expression and SQL Server rewrites it (167), and an
        // index's filter is one too, so those two fields are left to the
        // name comparison above (DECISIONS 183).
        for (n, was) in &self.finished.unique {
            if let Some(now) = now.unique.get(n)
                && was != now
            {
                moved.push(format!(
                    "{now_name} unique `{n}` is not the one this plan's `CREATE TABLE` \
                     declares",
                    now_name = now_name.shown()
                ));
            }
        }
        // A foreign key is nothing but structure — the columns, the
        // parent and the two referential actions — so all of it is
        // comparable. The differ normally splits its definition into an
        // AddForeignKey change (182); a carried CREATE payload must answer
        // for the same structure instead of relying on that split.
        for (n, now) in now.foreign_keys {
            if let Some(was) = self.finished.foreign_keys.get(n)
                && was != now
            {
                moved.push(format!(
                    "{now_name} foreign key `{n}` is not the one this plan adds",
                    now_name = now_name.shown()
                ));
            }
        }
        for (n, now) in now.indexes {
            if let Some(was) = self.finished.indexes.get(n)
                && !index_as_declared(was, now)
            {
                moved.push(format!(
                    "{now_name} index `{n}` is not the one this plan's `CREATE TABLE` \
                     declares",
                    now_name = now_name.shown()
                ));
            }
        }
        if let (Some(was), Some(now)) = (&self.finished.primary_key, &now.primary_key)
            && !primary_key_as_declared(was, now)
        {
            moved.push(format!(
                "{now_name} primary key is not the one this plan's `CREATE TABLE` declares",
                now_name = now_name.shown()
            ));
        }
        if self.finished.primary_key.is_none() && now.primary_key.is_some() {
            moved.push(format!(
                "{now_name} has a primary key, and this plan declares none",
                now_name = now_name.shown()
            ));
        }
        // Which index holds the rows (#1178). A staged checkpoint can land
        // between the `CREATE TABLE` and the `ALTER` that adds the
        // clustered constraint or index, where the table is still a
        // heap; that is the plan in progress, not movement, so the layout
        // is held once the object it names is there.
        if !super::parts::created_layout(&self.finished, now, settled.whole()) {
            moved.push(format!(
                "{now_name} is not clustered the way this plan's `CREATE TABLE` declares",
                now_name = now_name.shown()
            ));
        }
        // The replica identity is the `CREATE`'s last statement (#1444):
        // until then the table reads as the default, which is the plan in
        // progress; any other identity is the one it ends with. Where the
        // resolver split it out of the `CREATE` to follow its index, the
        // split setting is that one (#1467 review).
        let target = self.identity;
        if (settled.whole() || now.replica_identity.is_some())
            && target != now.replica_identity.as_ref()
        {
            moved.push(format!(
                "{now_name} replica identity is not the one this plan's `CREATE TABLE` \
                 declares",
                now_name = now_name.shown()
            ));
        }
        // In the `CREATE` itself, so held from the first read that finds
        // the table (#1443).
        if declared.unlogged != now.unlogged {
            moved.push(format!(
                "{now_name} is not {} as this plan's `CREATE TABLE` declares",
                if declared.unlogged {
                    "unlogged"
                } else {
                    "logged"
                },
                now_name = now_name.shown()
            ));
        }
        // In the `CREATE` itself, period and versioning, so held from the
        // first read that finds the table (#1176): another session's
        // retention or versioning change after it is not the plan's.
        if &declared.system_time != now.system_time {
            moved.push(format!(
                "{now_name} period or system versioning is not what this plan's `CREATE \
                 TABLE` declares",
                now_name = now_name.shown()
            ));
        }
        // In the `CREATE` itself (`WITH`), so held from the first read
        // that finds the table (#1441).
        if &declared.storage_parameters != now.storage_parameters {
            moved.push(format!(
                "{now_name} storage parameters are not the ones this plan's `CREATE TABLE` \
                 declares",
                now_name = now_name.shown()
            ));
        }
        if settled.whole() && self.finished.primary_key.is_some() && now.primary_key.is_none() {
            moved.push(format!(
                "{now_name} has no primary key, and this plan's `CREATE TABLE` declares one",
                now_name = now_name.shown()
            ));
        }
    }
}
