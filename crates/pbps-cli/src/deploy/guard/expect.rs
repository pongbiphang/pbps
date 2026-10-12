use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    Change, ChangeSet, PartitionBound, PartitionBy, ReplicaIdentity, SystemTime, Table, TableName,
    Uid,
};

use super::Req;

type Storage = BTreeMap<String, String>;
type StorageSetting<'a> = (&'a Storage, &'a BTreeSet<String>);
type Setting<'a, T> = (T, &'a TableName);
pub(super) type Place = (Option<(TableName, PartitionBound)>, Option<PartitionBy>);

#[derive(Default)]
struct TablePlan<'a> {
    renamed: Option<(&'a TableName, &'a TableName)>,
    identity: Option<Setting<'a, Option<&'a ReplicaIdentity>>>,
    storage: Option<Setting<'a, StorageSetting<'a>>>,
    persistence: Option<Setting<'a, bool>>,
}

pub(crate) struct TableChanges<'a>(BTreeMap<&'a Uid, TablePlan<'a>>);

pub(crate) struct Expected<'a> {
    pub(super) identity: Req<Option<&'a ReplicaIdentity>>,
    pub(super) storage: Req<Cow<'a, Storage>>,
    pub(super) persistence: Req<bool>,
    pub(super) system_time: Req<Option<&'a SystemTime>>,
    pub(super) place: Req<Place>,
}

impl<'a> TableChanges<'a> {
    pub(crate) fn of(plan: &'a ChangeSet) -> Self {
        let mut tables: BTreeMap<&Uid, TablePlan<'_>> = BTreeMap::new();
        for p in &plan.changes {
            match &p.change {
                Change::RenameTable { uid, from, to, .. } => {
                    tables.entry(uid).or_default().renamed = Some((from, to));
                }
                Change::SetReplicaIdentity { uid, table, to } => {
                    tables.entry(uid).or_default().identity = Some((to.as_ref(), table));
                }
                Change::SetStorageParameters {
                    uid,
                    table,
                    set,
                    reset,
                } => {
                    tables.entry(uid).or_default().storage = Some(((set, reset), table));
                }
                Change::SetTablePersistence {
                    uid,
                    table,
                    unlogged,
                } => {
                    tables.entry(uid).or_default().persistence = Some((*unlogged, table));
                }
                // These families retain their legacy comparisons. Listing
                // them makes a new change require an explicit scalar policy.
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::DetachPartition { .. }
                | Change::AttachPartition { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnExpression { .. }
                | Change::AddComputedColumn { .. }
                | Change::DropComputedColumn { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPartitionDefault { .. }
                | Change::SetPartitionNotNull { .. }
                | Change::SetIndexStorageParameters { .. }
                | Change::SetPrimaryKey { .. }
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
        }
        // A rename alone supplies no scalar requirement. Retaining those
        // entries would make every touched table scan every rename even
        // when the plan changes no scalar at all.
        tables
            .retain(|_, p| p.identity.is_some() || p.storage.is_some() || p.persistence.is_some());
        Self(tables)
    }

    // Settings before and after a rename share a UID, not necessarily a name.
    // Keep each field's execution address: this also preserves the old lookup
    // on synthetic plans whose settings disagree about an unrenamed address.
    fn setting<T: Copy>(
        &self,
        name: &TableName,
        now_name: &TableName,
        field: impl Fn(&TablePlan<'a>) -> Option<Setting<'a, T>>,
    ) -> Option<T> {
        self.0.values().find_map(|plan| {
            let (value, ran_under) = field(plan)?;
            let ours = match plan.renamed {
                Some((from, to)) => from == name && to == now_name,
                None => ran_under == name && name == now_name,
            };
            ours.then_some(value)
        })
    }

    pub(crate) fn created_identity(&self, uid: &Uid) -> Option<Option<&'a ReplicaIdentity>> {
        self.0.get(uid)?.identity.map(|(to, _)| to)
    }

    pub(crate) fn expect<'b>(
        &'b self,
        name: &TableName,
        now_name: &TableName,
        was: &'b Table,
        whole: bool,
        key_handoff: Option<&BTreeMap<String, Option<String>>>,
    ) -> Expected<'b> {
        let identity = Req::at_close(
            was.replica_identity.as_ref(),
            self.setting(name, now_name, |p| p.identity),
            whole,
        );
        let persistence = Req::at_close(
            was.unlogged,
            self.setting(name, now_name, |p| p.persistence),
            whole,
        );
        let kept = Cow::Borrowed(&was.storage_parameters);
        let storage = match self.setting(name, now_name, |p| p.storage) {
            None => Req::Keep(kept),
            Some((set, reset)) => {
                let mut planned = was.storage_parameters.clone();
                planned.retain(|k, _| !reset.contains(k));
                planned.extend(set.iter().map(|(k, v)| (k.clone(), v.clone())));
                if whole {
                    Req::Planned(Cow::Owned(planned))
                } else {
                    Req::PlannedOrKeep(Cow::Owned(planned), kept)
                }
            }
        };
        let mut place = place(was);
        // A key whose column names change hands has no undo. Compare its
        // baseline brought forward with the unrewound later read (#1692).
        if let (Some(by), Some(net)) = (&mut place.1, key_handoff) {
            for column in &mut by.columns {
                if let Some((now, _)) = net
                    .iter()
                    .find(|(_, was)| was.as_deref() == Some(column.as_str()))
                {
                    *column = now.clone();
                }
            }
        }
        Expected {
            identity,
            storage,
            persistence,
            system_time: Req::Keep(was.system_time.as_ref()),
            place: Req::Keep(place),
        }
    }
}

pub(super) fn place(table: &Table) -> Place {
    (
        table
            .partition_of
            .as_ref()
            .map(|of| (of.parent.clone(), of.bound.clone())),
        table.partition_by.clone(),
    )
}
