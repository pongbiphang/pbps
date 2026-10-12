use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    Change, ChangeSet, PartitionBound, PartitionBy, PartitionColumn, Renames, ReplicaIdentity,
    Schema, SystemTime, Table, TableName, Uid,
};

use super::{Expr, Req};

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

pub(crate) struct TableChanges<'a> {
    scalars: BTreeMap<&'a Uid, TablePlan<'a>>,
    parents: BTreeSet<&'a TableName>,
    parent_effects: BTreeMap<&'a TableName, Vec<(usize, OwnEffect<'a>)>>,
    own_effects: BTreeMap<&'a TableName, Vec<(usize, OwnEffect<'a>)>>,
}

#[derive(Clone, Copy)]
enum OwnEffect<'a> {
    Drop(&'a str),
    Default(&'a str, bool),
    // Relaxing the parent changes no kept local NOT NULL, but it still
    // owns that field between statements (DEC-1581.1).
    NotNull(&'a str, Option<bool>),
}

impl OwnEffect<'_> {
    fn column(&self) -> &str {
        match self {
            Self::Drop(column) | Self::Default(column, _) | Self::NotNull(column, _) => column,
        }
    }
}

#[derive(Clone)]
pub(super) struct Own<'a> {
    pub(super) default: Expr<'a>,
    pub(super) not_null: Req<bool>,
}

impl Default for Own<'_> {
    fn default() -> Self {
        Self {
            default: Expr::Keep(None),
            not_null: Req::Keep(false),
        }
    }
}

pub(crate) struct OwnExpected<'a>(pub(super) BTreeMap<String, Own<'a>>);

pub(crate) struct OwnNames<'a> {
    pub(crate) parent: Option<&'a TableName>,
    pub(crate) handoff: Option<&'a BTreeMap<String, Option<String>>>,
    pub(crate) undo: &'a Renames,
}

pub(crate) struct Implied<'a>(pub(super) BTreeMap<&'a TableName, Req<bool>>);

impl<'a> Implied<'a> {
    pub(crate) fn names(&self) -> impl Iterator<Item = &'a TableName> + '_ {
        self.0.keys().copied()
    }
}

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
        let mut parents = BTreeSet::new();
        let mut parent_effects: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut own_effects: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (at, p) in plan.changes.iter().enumerate() {
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
                Change::RenameColumn { table, .. } => {
                    parents.insert(table);
                }
                Change::DropColumn { column, .. } => {
                    parents.insert(&column.table);
                    parent_effects
                        .entry(&column.table)
                        .or_default()
                        .push((at, OwnEffect::Drop(&column.name)));
                }
                Change::AlterColumnNullability {
                    column,
                    to_nullable,
                    ..
                } => {
                    if !to_nullable {
                        parents.insert(&column.table);
                    }
                    parent_effects.entry(&column.table).or_default().push((
                        at,
                        OwnEffect::NotNull(&column.name, (!to_nullable).then_some(false)),
                    ));
                }
                Change::AlterColumnType {
                    column,
                    from_nullable,
                    to_nullable,
                    ..
                } => {
                    if from_nullable != to_nullable {
                        if !to_nullable {
                            parents.insert(&column.table);
                        }
                        parent_effects.entry(&column.table).or_default().push((
                            at,
                            OwnEffect::NotNull(&column.name, (!to_nullable).then_some(false)),
                        ));
                    }
                }
                Change::AlterColumnDefault { column, .. } => {
                    // A parent's SET/DROP DEFAULT replaces every child's
                    // override. A later own setting restores it (#1610).
                    parent_effects
                        .entry(&column.table)
                        .or_default()
                        .push((at, OwnEffect::Default(&column.name, false)));
                }
                Change::SetPartitionDefault {
                    table, column, to, ..
                } => {
                    own_effects
                        .entry(table)
                        .or_default()
                        .push((at, OwnEffect::Default(column, to.is_some())));
                }
                Change::SetPartitionNotNull {
                    table,
                    column,
                    not_null,
                    ..
                } => {
                    own_effects
                        .entry(table)
                        .or_default()
                        .push((at, OwnEffect::NotNull(column, Some(*not_null))));
                }
                // These families retain their legacy comparisons. Listing
                // them makes a new change require an explicit table policy.
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::DetachPartition { .. }
                | Change::AttachPartition { .. }
                | Change::AddColumn { .. }
                | Change::AlterColumnExpression { .. }
                | Change::AddComputedColumn { .. }
                | Change::DropComputedColumn { .. }
                | Change::SetColumnDeprecated { .. }
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
        }
        // A rename alone supplies no scalar requirement. Retaining those
        // entries would make every touched table scan every rename even
        // when the plan changes no scalar at all.
        tables
            .retain(|_, p| p.identity.is_some() || p.storage.is_some() || p.persistence.is_some());
        Self {
            scalars: tables,
            parents,
            parent_effects,
            own_effects,
        }
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
        self.scalars.values().find_map(|plan| {
            let (value, ran_under) = field(plan)?;
            let ours = match plan.renamed {
                Some((from, to)) => from == name && to == now_name,
                None => ran_under == name && name == now_name,
            };
            ours.then_some(value)
        })
    }

    pub(crate) fn created_identity(&self, uid: &Uid) -> Option<Option<&'a ReplicaIdentity>> {
        self.scalars.get(uid)?.identity.map(|(to, _)| to)
    }

    pub(crate) fn implied<'b>(
        &self,
        before: &'b Schema,
        after: &'b Schema,
        named: &BTreeSet<&TableName>,
    ) -> Implied<'b> {
        // Include arrivals so they cannot inherit an exemption from their
        // parent's change. Their expected presence still comes from before.
        Implied(
            before
                .tables
                .iter()
                .chain(&after.tables)
                .filter(|(name, table)| {
                    !named.contains(name)
                        && table
                            .partition_of
                            .as_ref()
                            .is_some_and(|of| self.parents.contains(&of.parent))
                })
                .map(|(name, _)| (name, Req::Keep(before.tables.contains_key(name))))
                .collect(),
        )
    }

    pub(crate) fn own<'b>(
        &self,
        name: &TableName,
        was: &'b Table,
        whole: bool,
        names: OwnNames<'_>,
    ) -> OwnExpected<'b> {
        let OwnNames {
            parent,
            handoff,
            undo,
        } = names;
        let mut kept: BTreeMap<String, Own<'b>> = was
            .partition_of
            .iter()
            .flat_map(|of| &of.columns)
            .map(|(name, PartitionColumn { default, not_null })| {
                (
                    name.clone(),
                    Own {
                        default: Expr::Keep(default.as_deref()),
                        not_null: Req::Keep(*not_null),
                    },
                )
            })
            .collect();
        if let Some(net) = handoff {
            // Undo cannot distinguish two successive holders of a name.
            // Keep both requirements under the names the plan leaves.
            let mut forwarded: BTreeMap<_, _> = kept
                .iter()
                .filter(|(name, _)| !net.contains_key(*name))
                .map(|(name, own)| (name.clone(), own.clone()))
                .collect();
            for (now, was) in net {
                if let Some(own) = was.as_ref().and_then(|was| kept.get(was)) {
                    forwarded.insert(now.clone(), own.clone());
                }
            }
            kept = forwarded;
        }
        let mut effects: Vec<_> = self
            .own_effects
            .get(name)
            .into_iter()
            .flatten()
            .chain(
                parent
                    .and_then(|p| self.parent_effects.get(p))
                    .into_iter()
                    .flatten(),
            )
            .collect();
        effects.sort_by_key(|(at, _)| *at);
        for (_, effect) in effects {
            let column = match (handoff, parent) {
                (None, Some(parent)) => undo.column(parent, effect.column()),
                _ => effect.column().to_owned(),
            };
            match effect {
                OwnEffect::Drop(_) if whole => {
                    // A forwarded map already dropped the old identity;
                    // this spelling can now belong to another column.
                    if handoff.is_none() {
                        kept.remove(&column);
                    }
                }
                OwnEffect::Drop(_) => {
                    kept.insert(
                        column,
                        Own {
                            default: Expr::Unpromised,
                            not_null: Req::Unpromised,
                        },
                    );
                }
                OwnEffect::Default(_, has) => {
                    kept.entry(column).or_default().default = if whole {
                        Expr::Planned(*has)
                    } else {
                        Expr::Unpromised
                    };
                }
                OwnEffect::NotNull(_, planned) => {
                    let own = kept.entry(column).or_default();
                    match (whole, planned) {
                        (true, Some(value)) => own.not_null = Req::Planned(*value),
                        (false, _) => own.not_null = Req::Unpromised,
                        (true, None) => {}
                    }
                }
            }
        }
        OwnExpected(kept)
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
