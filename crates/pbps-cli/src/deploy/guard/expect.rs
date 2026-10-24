use pbps_model::ColumnName;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    Change, ChangeSet, PartitionBound, PartitionBy, PartitionColumn, Renames, ReplicaIdentity,
    Schema, SystemTime, Table, TableName, Uid,
};

use super::{Expr, Req, Source};

/// Field-sized requirements: retyping a column does not own its default,
/// identity or nullability, and Whole owns only one-sided presence (DECISIONS 173).
pub(crate) struct ColumnExpected<'a> {
    pub(super) ty: Req<&'a pbps_model::ColumnType>,
    pub(super) nullable: Req<bool>,
    pub(super) default: Expr<'a>,
    pub(super) deprecated: Req<Option<&'a str>>,
    pub(super) collation: Req<Option<&'a pbps_model::Collation>>,
    pub(super) generated_kind: Option<bool>,
    pub(super) generated_expression: Expr<'a>,
    pub(super) identity: Req<Option<&'a pbps_model::Identity>>,
    pub(super) sequence: Req<Option<&'a pbps_model::OwnedSequence>>,
    pub(super) description: Option<&'a str>,
}

impl<'a> ColumnExpected<'a> {
    pub(crate) fn keeping(
        was: &'a pbps_model::Column,
        changed: &BTreeSet<pbps_model::ColumnField>,
    ) -> Self {
        use pbps_model::{Column, ColumnField};
        let Column {
            ty,
            nullable,
            value_source,
            collation,
            description,
            deprecated,
        } = was;
        let source = Source::of(value_source);
        fn kept<T>(value: T, changed: bool) -> Req<T> {
            if changed {
                Req::Unpromised
            } else {
                Req::Keep(value)
            }
        }
        let expression = |value, field| {
            if changed.contains(&field) {
                Expr::Unpromised
            } else {
                Expr::Keep(value)
            }
        };
        Self {
            ty: kept(ty, changed.contains(&ColumnField::Type)),
            nullable: kept(*nullable, changed.contains(&ColumnField::Nullable)),
            default: expression(source.default, ColumnField::Default),
            deprecated: kept(
                deprecated.as_ref().map(pbps_model::Text::raw),
                changed.contains(&ColumnField::Deprecated),
            ),
            collation: kept(
                collation.as_ref(),
                changed.contains(&ColumnField::Collation),
            ),
            generated_kind: source.generated.map(|g| g.stored),
            generated_expression: expression(
                source.generated.map(|g| g.expression.as_str()),
                ColumnField::Generated,
            ),
            // A numbering change moves either (#1889); its promise holds the
            // source it gives the column at the close.
            identity: kept(source.identity, changed.contains(&ColumnField::Numbering)),
            sequence: kept(source.sequence, changed.contains(&ColumnField::Numbering)),
            description: description.as_ref().map(pbps_model::Text::raw),
        }
    }
}

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
    row_security: Option<Setting<'a, &'a pbps_model::RowSecurity>>,
    policies: Option<Setting<'a, ()>>,
}

pub(crate) struct TableChanges<'a> {
    scalars: BTreeMap<Uid, TablePlan<'a>>,
    parents: BTreeSet<TableName>,
    parent_effects: BTreeMap<TableName, Vec<(usize, OwnEffect<'a>)>>,
    own_effects: BTreeMap<TableName, Vec<(usize, OwnEffect<'a>)>>,
}

#[derive(Clone, Copy)]
enum OwnEffect<'a> {
    Drop(&'a ColumnName),
    Default(&'a ColumnName, bool),
    // Relaxing the parent changes no kept local NOT NULL, but it still
    // owns that field between statements (DEC-1581.1).
    NotNull(&'a ColumnName, Option<bool>),
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

pub(crate) struct OwnExpected<'a>(pub(super) BTreeMap<ColumnName, Own<'a>>);

pub(crate) struct OwnNames<'a, 'p> {
    pub(crate) parent: Option<&'a TableName>,
    pub(crate) handoff: Option<&'a BTreeMap<ColumnName, Option<ColumnName>>>,
    pub(crate) undo: &'a Renames,
    pub(crate) index: &'a pbps_model::plan_names::PlanNames<'p>,
}

pub(crate) struct Implied<'a>(pub(super) BTreeMap<&'a TableName, Req<bool>>);

impl<'a> Implied<'a> {
    pub(crate) fn names(&self) -> impl Iterator<Item = &'a TableName> + '_ {
        self.0.keys().copied()
    }
}

pub(crate) struct Expected<'a> {
    pub(super) row_security: Req<&'a pbps_model::RowSecurity>,
    pub(super) row_policies: Req<&'a BTreeMap<String, pbps_model::RowPolicy>>,
    pub(super) identity: Req<Option<&'a ReplicaIdentity>>,
    pub(super) storage: Req<Cow<'a, Storage>>,
    pub(super) persistence: Req<bool>,
    pub(super) system_time: Req<Option<&'a SystemTime>>,
    pub(super) place: Req<Place>,
}

impl<'a> TableChanges<'a> {
    pub(crate) fn of(plan: &'a ChangeSet, names: &pbps_model::plan_names::PlanNames<'_>) -> Self {
        let mut tables: BTreeMap<Uid, TablePlan<'_>> = BTreeMap::new();
        let mut parents = BTreeSet::new();
        let mut parent_effects: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut own_effects: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (at, p) in plan.changes.iter().enumerate() {
            match &p.change {
                Change::RebuildPartitionedTable(_)
                | Change::BuildPartitionedIndex(_)
                | Change::CopyTableRows(_)
                | Change::AnalyzeTableTree(_) => {}
                Change::RenameTable { uid, from, to, .. } => {
                    tables.entry(uid.clone()).or_default().renamed = Some((from, to));
                }
                Change::SetRowSecurity { uid, table, to, .. } => {
                    tables.entry(uid.clone()).or_default().row_security = Some((to, table));
                }
                Change::DropPolicy { table, .. } | Change::CreatePolicy { table, .. } => {
                    if let pbps_model::plan_names::Holder::Tracked(uid) =
                        names.holder(table, pbps_model::plan_names::At::Before(at))
                    {
                        tables.entry(uid.clone()).or_default().policies = Some(((), table));
                    }
                }
                Change::SetReplicaIdentity { uid, table, to } => {
                    tables.entry(uid.clone()).or_default().identity = Some((to.as_ref(), table));
                }
                Change::SetStorageParameters {
                    uid,
                    table,
                    set,
                    reset,
                } => {
                    tables.entry(uid.clone()).or_default().storage = Some(((set, reset), table));
                }
                Change::SetTablePersistence {
                    uid,
                    table,
                    unlogged,
                } => {
                    tables.entry(uid.clone()).or_default().persistence = Some((*unlogged, table));
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
                | Change::AlterColumnNumbering { .. }
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
                | Change::PublicExecution { .. } => {}
            }
        }
        use pbps_model::plan_names::{At, Holder};
        let endpoint = |table: &TableName, at: usize| {
            let Holder::Tracked(uid) = names.holder(table, At::Before(at)) else {
                return None;
            };
            names.table_at(uid, At::End).cloned()
        };
        let forward = |effects: BTreeMap<&TableName, Vec<(usize, OwnEffect<'a>)>>| {
            let mut projected: BTreeMap<TableName, Vec<_>> = BTreeMap::new();
            for (table, effects) in effects {
                for (at, effect) in effects {
                    if let Some(table) = endpoint(table, at) {
                        projected.entry(table).or_default().push((at, effect));
                    }
                }
            }
            projected
        };
        let parents: BTreeSet<TableName> = parents.into_iter().cloned().collect();
        // Reach children observed under either endpoint of the parent UID.
        let mut parents = parents;
        for (at, p) in plan.changes.iter().enumerate() {
            for table in p.change.objects() {
                if parents.contains(table)
                    && let Some(end) = endpoint(table, at)
                {
                    parents.insert(end);
                }
            }
        }
        let parent_effects = forward(parent_effects);
        let own_effects = forward(own_effects);
        // A rename alone supplies no scalar requirement. Retaining those
        // entries would make every touched table scan every rename even
        // when the plan changes no scalar at all.
        tables.retain(|_, p| {
            p.identity.is_some()
                || p.storage.is_some()
                || p.persistence.is_some()
                || p.row_security.is_some()
                || p.policies.is_some()
        });
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
        names: OwnNames<'_, '_>,
    ) -> OwnExpected<'b> {
        let OwnNames {
            parent,
            handoff,
            undo,
            index,
        } = names;
        let mut kept: BTreeMap<ColumnName, Own<'b>> = was
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
        let column_at = |column: &ColumnName, at, boundary| {
            use pbps_model::plan_names::{At, Holder};
            let Some(parent) = parent else {
                return Some(column.to_owned());
            };
            let owner = match index.holder(parent, At::End) {
                Holder::Tracked(owner) => owner,
                Holder::Unmanaged => return Some(column.to_owned()),
                Holder::Vacant => return None,
            };
            let before = index.table_at(owner, At::Before(at))?;
            match index.column_holder(&before.column(column), At::Before(at)) {
                Holder::Tracked(uid) => index.column_at(uid, boundary).map(|(_, n)| n.to_owned()),
                Holder::Unmanaged => Some(column.to_owned()),
                Holder::Vacant => None,
            }
        };
        for (at, effect) in effects {
            // Field effects belong to the column held when the step runs.
            // Project before undoing observation names: a retired column's
            // default must not erase its replacement's kept own default.
            // DROP removes the UID's baseline slot even when it was renamed
            // before retirement. Its end address no longer exists, and its
            // execution spelling need not be the baseline spelling.
            let column = match effect {
                OwnEffect::Drop(column) => {
                    let Some(column) = column_at(column, *at, pbps_model::plan_names::At::Start)
                    else {
                        continue;
                    };
                    column
                }
                OwnEffect::Default(column, _) | OwnEffect::NotNull(column, _) => {
                    let Some(column) = column_at(column, *at, pbps_model::plan_names::At::End)
                    else {
                        continue;
                    };
                    match (handoff, parent) {
                        (None, Some(parent)) => undo.column(parent, &column),
                        _ => column,
                    }
                }
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
        key_handoff: Option<&BTreeMap<ColumnName, Option<ColumnName>>>,
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
                if let Some((now, _)) = net.iter().find(|(_, was)| was.as_ref() == Some(column)) {
                    *column = now.clone();
                }
            }
        }
        Expected {
            row_security: Req::at_close(
                &was.row_security,
                self.setting(name, now_name, |p| p.row_security),
                whole,
            ),
            // Parts::kept compares every untouched policy exactly; Parts::after
            // compares each touched policy structurally, including both clauses.
            row_policies: if self.setting(name, now_name, |p| p.policies).is_some() {
                Req::Unpromised
            } else {
                Req::Keep(&was.row_policies)
            },
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

/// Requirements are addressed through the pinned UID history, then rendered
/// under the final spelling. A vacated name never lends its old occupant's
/// requirements to a replacement (DECISIONS 474).
pub(crate) struct ColumnChanges<'a> {
    changed: BTreeMap<TableName, BTreeMap<ColumnName, BTreeSet<pbps_model::ColumnField>>>,
    pub(super) presence: BTreeMap<pbps_model::ColumnRef, pbps_model::Presence>,
    pub(super) promises:
        BTreeMap<(pbps_model::ColumnRef, pbps_model::ColumnField), pbps_model::ColumnPromise<'a>>,
    pub(crate) renamed: BTreeMap<TableName, BTreeMap<ColumnName, ColumnName>>,
    pub(crate) handoffs: BTreeMap<TableName, BTreeMap<ColumnName, Option<ColumnName>>>,
    pub(crate) partition_handoffs: BTreeMap<TableName, BTreeMap<ColumnName, Option<ColumnName>>>,
    replaced: BTreeSet<pbps_model::ColumnRef>,
}

impl<'a> ColumnChanges<'a> {
    pub(crate) fn of(
        changes: &'a ChangeSet,
        names: &pbps_model::plan_names::PlanNames<'_>,
    ) -> Self {
        use pbps_model::{
            ColumnField, ColumnRef, Presence,
            plan_names::{At, Holder},
        };
        let mut this = Self {
            changed: BTreeMap::new(),
            presence: BTreeMap::new(),
            promises: BTreeMap::new(),
            renamed: BTreeMap::new(),
            handoffs: BTreeMap::new(),
            partition_handoffs: BTreeMap::new(),
            replaced: BTreeSet::new(),
        };
        // A spelling without a current column (DROP or the old end of a
        // rename) still belongs to its known table. Only the table's UID may
        // carry that vacancy through a later table rename.
        let spelling = |column: &ColumnRef, at| {
            let Holder::Tracked(owner) = names.holder(&column.table, at) else {
                return None;
            };
            Some(names.table_at(owner, At::End)?.column(&column.name))
        };
        let surviving = |column: &ColumnRef, at| {
            let Holder::Tracked(uid) = names.column_holder(column, at) else {
                return None;
            };
            let (owner, name) = names.column_at(uid, At::End)?;
            Some(names.table_at(owner, At::End)?.column(name))
        };
        let mut touched_names: BTreeMap<TableName, BTreeSet<ColumnName>> = BTreeMap::new();
        for (i, p) in changes.changes.iter().enumerate() {
            for (column, field) in p.change.columns_redefined() {
                let final_column = if field == ColumnField::Whole {
                    spelling(&column, At::Before(i))
                } else {
                    surviving(&column, At::Before(i))
                };
                if let Some(column) = final_column {
                    if field == ColumnField::Whole {
                        touched_names
                            .entry(column.table.clone())
                            .or_default()
                            .insert(column.name.clone());
                    }
                    this.changed
                        .entry(column.table)
                        .or_default()
                        .entry(column.name)
                        .or_default()
                        .insert(field);
                }
            }
            for (column, _) in p.change.columns_after() {
                if let Some(column) = spelling(&column, At::After(i)) {
                    let present =
                        matches!(names.column_holder(&column, At::End), Holder::Tracked(_));
                    this.presence.insert(
                        column,
                        if present {
                            Presence::Present
                        } else {
                            Presence::Absent
                        },
                    );
                }
            }
            // Last promise wins for this identity, even if it is renamed
            // again. A column removed later has no final definition to hold.
            for (column, promise) in p.change.columns_promised() {
                if let Some(column) = surviving(&column, At::After(i)) {
                    this.promises.insert((column, promise.field()), promise);
                }
            }
        }
        for (table, columns) in touched_names {
            let Holder::Tracked(owner) = names.holder(&table, At::End) else {
                continue;
            };
            let Some(original) = names.table_at(owner, At::Start) else {
                continue;
            };
            let mut net = BTreeMap::new();
            let (mut hands, mut replaced) = (false, false);
            for column in columns {
                let address = table.column(&column);
                let before = names.column_holder(&original.column(&column), At::Start);
                let after = names.column_holder(&address, At::End);
                let held = match after {
                    Holder::Tracked(uid) => names
                        .column_at(uid, At::Start)
                        .map(|(_, name)| name.to_owned()),
                    Holder::Unmanaged | Holder::Vacant => None,
                };
                if let Holder::Tracked(was) = before
                    && let Holder::Tracked(now) = after
                    && was != now
                {
                    if names.column_at(was, At::End).is_some() {
                        hands = true;
                    } else {
                        replaced = true;
                        this.replaced.insert(address);
                    }
                }
                if let Some(from) = &held
                    && from != &column
                {
                    this.renamed
                        .entry(table.clone())
                        .or_default()
                        .insert(column.clone(), from.clone());
                }
                net.insert(column, held);
            }
            if hands || replaced {
                this.partition_handoffs.insert(table.clone(), net.clone());
            }
            if hands {
                this.handoffs.insert(table, net);
            }
        }
        this
    }
}

/// Derivation chooses the observed identity; comparison only checks fields.
pub(super) struct ColumnObservation<'a> {
    pub(super) name: ColumnName,
    pub(super) present: Req<bool>,
    pub(super) fields: Option<ColumnExpected<'a>>,
}

pub(crate) struct ColumnsExpected<'a>(pub(super) Vec<ColumnObservation<'a>>);

impl ColumnChanges<'_> {
    pub(crate) fn expect<'b>(
        &self,
        name: &TableName,
        was: &'b Table,
        now: &Table,
    ) -> ColumnsExpected<'b> {
        use pbps_model::ColumnField;
        let mut baseline: BTreeMap<_, _> =
            was.columns.iter().map(|(n, c)| (n.clone(), c)).collect();
        // Lookup order and reporting order differ: preserve the catalog's
        // IndexMap order, including the legacy repeated two-sided findings.
        let mut order: Vec<_> = was.columns.keys().collect();
        if let Some(net) = self.handoffs.get(name) {
            order.retain(|column| !net.contains_key(*column));
            baseline.retain(|column, _| !net.contains_key(column));
            for (column, from) in net {
                if let Some(held) = from.as_ref().and_then(|from| was.columns.get(from)) {
                    baseline.insert(column.clone(), held);
                    order.push(column);
                }
            }
        }
        let empty = BTreeSet::new();
        let changed = |column: &ColumnName| {
            self.changed
                .get(name)
                .and_then(|c| c.get(column))
                .unwrap_or(&empty)
        };
        ColumnsExpected(
            order
                .into_iter()
                .chain(now.columns.keys())
                .map(|column| {
                    let mut fields = changed(column).clone();
                    let from = self.renamed.get(name).and_then(|r| r.get(column));
                    // A newly added UID has no baseline source. A surviving UID
                    // only takes the retired holder's name in the read spanning
                    // its rename; later reads already hold that survivor, and
                    // after DROP alone its source still exists (DECISIONS 474).
                    let replaced = self.replaced.contains(&name.column(column))
                        && from.is_none_or(|from| {
                            baseline.contains_key(from) && !now.columns.contains_key(from)
                        });
                    let kept = baseline
                        .get(column)
                        .copied()
                        .filter(|_| !replaced)
                        .or_else(|| {
                            let from = from?;
                            if now.columns.contains_key(from) {
                                return None;
                            }
                            fields.extend(changed(from));
                            baseline.get(from).copied()
                        });
                    ColumnObservation {
                        name: column.clone(),
                        present: if fields.contains(&ColumnField::Whole) {
                            Req::Unpromised
                        } else {
                            Req::Keep(kept.is_some())
                        },
                        fields: kept.map(|was| ColumnExpected::keeping(was, &fields)),
                    }
                })
                .collect(),
        )
    }
}
