use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    Change, ChangeSet, IndexPart, Part, PartAfter, PartDefinition, Schema, Table, TableName,
    plan_names::{At, Holder, PlanNames},
};

use super::super::{index_as_declared, primary_key_as_declared};
use super::Fields;

/// The primary key has no map name; names in different part namespaces never
/// exempt one another (DECISIONS 168).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot<'a> {
    PrimaryKey,
    Unique(&'a str),
    ForeignKey(&'a str),
    Check(&'a str),
    Index(&'a str),
    Computed(&'a str),
}

impl<'a> Slot<'a> {
    fn of(part: pbps_model::PartChange<'a>) -> Self {
        match part.part() {
            Part::PrimaryKey => Self::PrimaryKey,
            Part::Unique => Self::Unique(part.name.expect("named unique")),
            Part::ForeignKey => Self::ForeignKey(part.name.expect("named foreign key")),
            Part::Check => Self::Check(part.name.expect("named check")),
            Part::Index => Self::Index(part.name.expect("named index")),
            Part::Computed => Self::Computed(part.name.expect("named computed column")),
        }
    }

    fn index(part: &'a IndexPart) -> Self {
        match part {
            IndexPart::PrimaryKey => Self::PrimaryKey,
            IndexPart::Unique(name) => Self::Unique(name),
            IndexPart::Index(name) => Self::Index(name),
        }
    }

    fn name(self) -> &'a str {
        match self {
            Self::PrimaryKey => "",
            Self::Unique(n)
            | Self::ForeignKey(n)
            | Self::Check(n)
            | Self::Index(n)
            | Self::Computed(n) => n,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::PrimaryKey => "primary key",
            Self::Unique(_) => "unique",
            Self::ForeignKey(_) => "foreign key",
            Self::Check(_) => "check",
            Self::Index(_) => "index",
            Self::Computed(_) => "computed column",
        }
    }

    /// Layout is compared separately; equality here holds the complete catalog
    /// definition, including stored check/filter text (SPEC 7.6).
    fn read<'t>(self, table: &Fields<'t>) -> Option<PartDefinition<'t>> {
        Some(match self {
            Self::PrimaryKey => PartDefinition::PrimaryKey(table.primary_key.as_ref()?, false),
            Self::Unique(n) => PartDefinition::Unique(table.unique.get(n)?, false),
            Self::ForeignKey(n) => PartDefinition::ForeignKey(table.foreign_keys.get(n)?),
            Self::Check(n) => PartDefinition::Check(table.checks.get(n)?),
            Self::Index(n) => PartDefinition::Index(table.indexes.get(n)?, false),
            Self::Computed(n) => PartDefinition::Computed(table.computed.get(n)?),
        })
    }

    fn all(table: &Fields<'a>) -> BTreeSet<Self> {
        std::iter::once(Self::PrimaryKey)
            .chain(table.unique.keys().map(|n| Self::Unique(n)))
            .chain(table.foreign_keys.keys().map(|n| Self::ForeignKey(n)))
            .chain(table.checks.keys().map(|n| Self::Check(n)))
            .chain(table.indexes.keys().map(|n| Self::Index(n)))
            .chain(table.computed.keys().map(|n| Self::Computed(n.raw())))
            .collect()
    }
}

/// Last setting wins for each parameter, whether it sets or resets it. This
/// folds multiple statements without replaying each against the same baseline.
#[derive(Default)]
struct Parameters<'a>(BTreeMap<&'a str, Option<&'a str>>);

impl<'a> Parameters<'a> {
    fn extend(&mut self, set: &'a BTreeMap<String, String>, reset: &'a BTreeSet<String>) {
        self.0.extend(reset.iter().map(|k| (k.as_str(), None)));
        self.0
            .extend(set.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))));
    }

    fn apply(&self, parameters: &mut BTreeMap<String, String>) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => {
                    parameters.insert((*key).to_owned(), (*value).to_owned());
                }
                None => {
                    parameters.remove(*key);
                }
            }
        }
    }

    fn with<R>(
        &self,
        definition: PartDefinition<'_>,
        check: impl FnOnce(PartDefinition<'_>) -> R,
    ) -> R {
        if self.0.is_empty() {
            return check(definition);
        }
        match definition {
            PartDefinition::PrimaryKey(value, clustered) => {
                let mut value = value.clone();
                self.apply(&mut value.storage_parameters);
                check(PartDefinition::PrimaryKey(&value, clustered))
            }
            PartDefinition::Unique(value, clustered) => {
                let mut value = value.clone();
                self.apply(&mut value.storage_parameters);
                check(PartDefinition::Unique(&value, clustered))
            }
            PartDefinition::Index(value, clustered) => {
                let mut value = value.clone();
                self.apply(&mut value.storage_parameters);
                check(PartDefinition::Index(&value, clustered))
            }
            PartDefinition::ForeignKey(_)
            | PartDefinition::Check(_)
            | PartDefinition::Computed(_) => check(definition),
        }
    }
}

#[derive(Default)]
struct Effect<'a> {
    after: Option<PartAfter<'a>>,
    parameters: Parameters<'a>,
    storage_order: Option<usize>,
}

#[derive(Default)]
struct Parts<'a> {
    effects: BTreeMap<Slot<'a>, Effect<'a>>,
    layout: Vec<(Slot<'a>, PartAfter<'a>)>,
}

pub(crate) struct PartChanges<'a>(BTreeMap<TableName, Parts<'a>>);

/// Resolve at the statement boundary before asking for the surviving endpoint.
/// A retired UID must not lend its effects to the next occupant of its name.
pub(super) fn final_table(names: &PlanNames<'_>, table: &TableName, at: At) -> Option<TableName> {
    match names.holder(table, at) {
        Holder::Tracked(uid) => names.table_at(uid, At::End).cloned(),
        Holder::Unmanaged => Some(table.clone()),
        Holder::Vacant => None,
    }
}

impl<'a> PartChanges<'a> {
    /// CREATE's payload and separately emitted parts describe one final shape.
    /// Replacements supersede the payload; settings apply to that surviving
    /// definition, in the same fold used for existing tables.
    pub(super) fn created(&self, name: &TableName, table: &mut Table) {
        let Some(parts) = self.0.get(name) else {
            return;
        };
        let fields = Fields::of(table);
        let expected_holder = parts.holder(holder(&fields));
        // Own the selector before mutating the table it may borrow from.
        let selector = match expected_holder {
            Some(Slot::PrimaryKey) => None,
            Some(Slot::Unique(n)) => Some(pbps_model::Clustered::Unique(n.to_owned())),
            Some(Slot::Index(n)) => Some(pbps_model::Clustered::Index(n.to_owned())),
            None => Some(pbps_model::Clustered::Heap),
            Some(Slot::ForeignKey(_) | Slot::Check(_) | Slot::Computed(_)) => {
                unreachable!("only index-backed parts hold rows")
            }
        };
        for (slot, effect) in &parts.effects {
            if let Some(after) = effect.after {
                match after {
                    PartAfter::Standing(definition) => {
                        let name = slot.name().to_owned();
                        match definition {
                            PartDefinition::PrimaryKey(value, _) => {
                                table.primary_key = Some(value.clone())
                            }
                            PartDefinition::Unique(value, _) => {
                                table.unique.insert(name, value.clone());
                            }
                            PartDefinition::ForeignKey(value) => {
                                table.foreign_keys.insert(name, value.clone());
                            }
                            PartDefinition::Check(value) => {
                                table.checks.insert(name, value.clone());
                            }
                            PartDefinition::Index(value, _) => {
                                table.indexes.insert(name, value.clone());
                            }
                            PartDefinition::Computed(value) => {
                                table.computed.insert(name.into(), value.clone());
                            }
                        }
                    }
                    PartAfter::Gone(_) => match slot {
                        Slot::PrimaryKey => table.primary_key = None,
                        Slot::Unique(n) => {
                            table.unique.remove(*n);
                        }
                        Slot::ForeignKey(n) => {
                            table.foreign_keys.remove(*n);
                        }
                        Slot::Check(n) => {
                            table.checks.remove(*n);
                        }
                        Slot::Index(n) => {
                            table.indexes.remove(*n);
                        }
                        Slot::Computed(n) => {
                            table.computed.remove(*n);
                        }
                    },
                }
            }
            let parameters = match slot {
                Slot::PrimaryKey => table
                    .primary_key
                    .as_mut()
                    .map(|p| &mut p.storage_parameters),
                Slot::Unique(n) => table.unique.get_mut(*n).map(|p| &mut p.storage_parameters),
                Slot::Index(n) => table.indexes.get_mut(*n).map(|p| &mut p.storage_parameters),
                Slot::ForeignKey(_) | Slot::Check(_) | Slot::Computed(_) => None,
            };
            if let Some(parameters) = parameters {
                effect.parameters.apply(parameters);
            }
        }
        table.clustered = selector;
    }

    pub(crate) fn of(changes: &'a ChangeSet, names: &PlanNames<'_>) -> Self {
        let mut tables: BTreeMap<TableName, Parts<'a>> = BTreeMap::new();
        for (i, change) in changes.changes.iter().enumerate() {
            if let Some(part) = change.change.constraints()
                && let Some(table) = final_table(names, part.table, At::Before(i))
            {
                let parts = tables.entry(table).or_default();
                let slot = Slot::of(part);
                // A replacement installs its own parameters, discarding any
                // setting made on the previous definition (DECISIONS 169).
                parts.effects.insert(
                    slot,
                    Effect {
                        after: Some(part.after),
                        ..Default::default()
                    },
                );
                parts.layout.push((slot, part.after));
            }
            if let Change::SetIndexStorageParameters {
                table,
                target,
                set,
                reset,
                ..
            } = &change.change
                && let Some(table) = final_table(names, table, At::Before(i))
            {
                let effect = tables
                    .entry(table)
                    .or_default()
                    .effects
                    .entry(Slot::index(target))
                    .or_default();
                effect.parameters.extend(set, reset);
                effect.storage_order.get_or_insert(i);
            }
        }
        Self(tables)
    }

    pub(crate) fn kept(
        &self,
        name: &TableName,
        was: &Fields<'_>,
        now: &Fields<'_>,
        pending: Option<&Table>,
        moved: &mut Vec<String>,
    ) {
        let parts = self.0.get(name);
        for slot in Slot::all(was).union(&Slot::all(now)) {
            if parts.is_some_and(|parts| parts.effects.contains_key(slot)) {
                continue;
            }
            let (before, after) = (slot.read(was), slot.read(now));
            // Later CREATE statements may add a part; once checkpointed its
            // definition is kept exactly, even if its declaration is opaque.
            if *slot != Slot::PrimaryKey
                && before.is_none()
                && after.is_some()
                && pending.is_some_and(|declared| slot.read(&Fields::of(declared)).is_some())
            {
                continue;
            }
            if before != after {
                moved.push(if *slot == Slot::PrimaryKey {
                    format!("{name} has a different primary key", name = name.shown())
                } else {
                    format!(
                        "{name} {} `{}` is not as the plan left it",
                        slot.kind(),
                        pbps_model::escape::Escaped(slot.name()),
                        name = name.shown()
                    )
                });
            }
        }
    }

    pub(crate) fn storage(
        &self,
        name: &TableName,
        was: &Fields<'_>,
        now: &Fields<'_>,
        whole: bool,
        moved: &mut Vec<String>,
    ) {
        let Some(parts) = self.0.get(name) else {
            return;
        };
        let mut settings: Vec<_> = parts
            .effects
            .iter()
            .filter_map(|(slot, effect)| {
                // A replacement's complete declared definition is checked by
                // `after`; only a setting on a kept part derives from the read.
                (effect.after.is_none())
                    .then_some(effect.storage_order)
                    .flatten()
                    .map(|order| (order, slot, effect))
            })
            .collect();
        settings.sort_by_key(|(order, _, _)| *order);
        for (_, slot, effect) in settings {
            let Some(before) = slot.read(was) else {
                continue;
            };
            let after = slot.read(now);
            let held = effect
                .parameters
                .with(before, |expected| after == Some(expected))
                || (!whole && after == Some(before));
            if !held {
                let what = if *slot == Slot::PrimaryKey {
                    slot.kind().to_owned()
                } else {
                    format!(
                        "{} `{}`",
                        slot.kind(),
                        pbps_model::escape::Escaped(slot.name())
                    )
                };
                moved.push(format!("{name} {what} is not the one this plan leaves: its index storage parameters set, and nothing else changed", name = name.shown()));
            }
        }
    }

    pub(crate) fn layout(
        &self,
        name: &TableName,
        was: &Fields<'_>,
        now: &Fields<'_>,
        moved: &mut Vec<String>,
    ) {
        let (before, after) = (holder(was), holder(now));
        if before == after {
            return;
        }
        let parts = self.0.get(name);
        let expected = parts.map_or(before, |parts| parts.holder(before));
        let touched = parts.is_some_and(|parts| {
            parts
                .layout
                .iter()
                .any(|(slot, _)| before == Some(*slot) || after == Some(*slot))
        });
        if !touched || !(after.is_none() || after == expected) {
            moved.push(format!("{name} is clustered differently from the table the plan was approved over, and no change of this plan moves its clustered index there", name = name.shown()));
        }
    }

    pub(crate) fn after(&self, schema: &Schema, moved: &mut Vec<String>) {
        for (name, parts) in &self.0 {
            let Some(table) = schema.tables.get(name) else {
                continue;
            };
            let table = &Fields::of(table);
            for (slot, effect) in &parts.effects {
                let Some(after) = effect.after else {
                    continue;
                };
                let (kind, part_name) = (slot.kind(), slot.name());
                match after {
                    PartAfter::Standing(definition) => {
                        if effect.parameters.with(definition, |definition| {
                            part_as_planned(definition, table, part_name)
                        }) == Some(false)
                        {
                            moved.push(format!("{name} {kind} `{part_name}` is there, and is not the one this plan adds", name = name.shown(), part_name = pbps_model::escape::Escaped(part_name)));
                        } else if slot.read(table).is_none() {
                            moved.push(format!(
                                "{name} {kind} `{part_name}` is not there, and this plan writes it",
                                name = name.shown(),
                                part_name = pbps_model::escape::Escaped(part_name),
                            ));
                        }
                    }
                    PartAfter::Gone(_) => {
                        if slot.read(table).is_some() {
                            moved.push(format!("{name} {kind} `{part_name}` is still there, and this plan removes it", name = name.shown(), part_name = pbps_model::escape::Escaped(part_name)));
                        }
                    }
                }
            }
        }
    }
}

impl Parts<'_> {
    fn holder<'a>(&'a self, mut holder: Option<Slot<'a>>) -> Option<Slot<'a>> {
        for (slot, effect) in &self.layout {
            if holder == Some(*slot) {
                holder = None;
            }
            if clustered(*effect) {
                holder = Some(*slot);
            }
        }
        holder
    }
}

pub(super) fn created_layout(expected: &Table, now: &Fields<'_>, whole: bool) -> bool {
    let expected = Fields::of(expected);
    let target = holder(&expected);
    let built = target.is_none_or(|slot| slot.read(now).is_some());
    !(built || whole) || target == holder(now)
}

fn holder<'a>(table: &Fields<'a>) -> Option<Slot<'a>> {
    match table.clustered {
        None if table.primary_key.is_some() => Some(Slot::PrimaryKey),
        Some(pbps_model::Clustered::Unique(n)) => Some(Slot::Unique(n)),
        Some(pbps_model::Clustered::Index(n)) => Some(Slot::Index(n)),
        None | Some(pbps_model::Clustered::Heap) => None,
    }
}

fn clustered(after: PartAfter<'_>) -> bool {
    match after {
        PartAfter::Standing(
            PartDefinition::PrimaryKey(_, clustered)
            | PartDefinition::Unique(_, clustered)
            | PartDefinition::Index(_, clustered),
        ) => clustered,
        PartAfter::Standing(
            PartDefinition::ForeignKey(_) | PartDefinition::Check(_) | PartDefinition::Computed(_),
        )
        | PartAfter::Gone(_) => false,
    }
}

/// Whether a part read back is the one the plan adds it as, or `None` where
/// the definition has nothing a read-back can be held to: a check is nothing
/// but an expression and SQL Server rewrites it (167), so its name is all
/// that is comparable (DECISIONS 183).
///
/// `None` as well where the part is not there at all: absent and wrong are
/// two different findings, and the presence check answers for the first.
fn part_as_planned(
    planned: pbps_model::PartDefinition<'_>,
    table: &Fields<'_>,
    name: &str,
) -> Option<bool> {
    use pbps_model::PartDefinition;
    // The layout is held with the rest: which index is clustered is read
    // back as the table's selector, so a key the engine built nonclustered
    // where the plan built it clustered is movement, not the plan's result
    // (#1178).
    Some(match planned {
        PartDefinition::PrimaryKey(was, clustered) => {
            primary_key_as_declared(was, table.primary_key.as_ref()?)
                && (holder(table) == Some(Slot::PrimaryKey)) == clustered
        }
        // A unique constraint is nothing but its columns, its index's
        // storage parameters (#1442) and its layout.
        PartDefinition::Unique(was, clustered) => {
            table.unique.get(name)? == was
                && (holder(table) == Some(Slot::Unique(name))) == clustered
        }
        // And a foreign key nothing but structure — the columns, the parent
        // and the two referential actions — so all of it is comparable (182).
        PartDefinition::ForeignKey(was) => table.foreign_keys.get(name)? == was,
        PartDefinition::Check(_) => return None,
        PartDefinition::Index(was, clustered) => {
            index_as_declared(was, table.indexes.get(name)?)
                && (holder(table) == Some(Slot::Index(name))) == clustered
        }
        // Held to its persistence, which the catalog reads back as given.
        // The expression is respelled (`([a]*(2))`), as a check's is, so it
        // is not compared here (#1174).
        PartDefinition::Computed(was) => {
            let now = table.computed.get(name)?;
            now.persisted == was.persisted && (!was.not_null || now.not_null)
        }
    })
}
