//! Name slots beside the effects (#1776 slice S1b-ii, DEC-1964.2).
//!
//! `pbps_model::operation::names` says which names each step claims and
//! releases; `pbps_dialect::slots` says which namespace holds each and whether
//! two spellings are one name. This module reads the two three ways, none of
//! which decides an order:
//!
//! - [`edges`]: release before claim, where the evidence names the holder a
//!   claim replaces. A name's first holder is one that only gives it up; its
//!   last is one that only takes it. Between them, an object that takes the
//!   name and gives it up again has no place the evidence fixes, and no edge:
//!   proposing one is S4's search (#1957), never position.
//! - [`verify`]: a replay of the order given, refusing a claim on a name
//!   another object still holds.
//! - the statement-level check inside it: a rename that passes through an
//!   intermediate name (PostgreSQL's `SET SCHEMA` before its `RENAME`, SQL
//!   Server's `TRANSFER` before its `sp_rename`) claims that name at that
//!   statement, from the operation's lowering.
//!
//! A cycle of handovers, a swap, never reaches here as an order: the differ
//! refuses it (`ColumnRenameCycle`, DEC-541.1; `TableRenameCycle`, #1966).

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_dialect::slots::{CatalogFacts, Same, SlotKey, same, slot};
use pbps_model::operation::{
    ObjectKey, Operation, OperationId, OwnerError, Owners, Snapshot, base_table_named, names,
};
use pbps_model::{Change, TableName};

/// One step's slots: what it claims and releases, each with its holder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotEffects {
    pub claims: Vec<(SlotKey, ObjectKey)>,
    pub releases: Vec<(SlotKey, ObjectKey)>,
    /// The names its statements pass through, which no other operation sees.
    pub intermediate: Vec<SlotKey>,
}

/// Every step's slots, and the steps whose names could not be read.
#[derive(Debug, Clone, Default)]
pub struct Slots {
    pub effects: Vec<SlotEffects>,
    pub unowned: Vec<(OperationId, OwnerError)>,
    /// Steps whose statements could not be lowered, so their intermediate
    /// names are not checked.
    pub unlowered: Vec<OperationId>,
}

/// Reads every step's names through the dialect's namespaces.
pub fn analyse(
    operations: &[Operation],
    owners: &dyn Owners,
    before: Snapshot<'_>,
    dialect: &dyn Dialect,
) -> Slots {
    let mut out = Slots::default();
    for op in operations {
        let mut fx = SlotEffects::default();
        match names(op, owners, before) {
            Ok(n) => {
                fx.claims = n
                    .claims
                    .iter()
                    .map(|c| (slot(dialect, &c.address), c.holder.clone()))
                    .collect();
                fx.releases = n
                    .releases
                    .iter()
                    .map(|r| (slot(dialect, &r.address), r.holder.clone()))
                    .collect();
                // A part that stays in its table's own namespace through a
                // move is released and claimed in one slot by one holder:
                // nothing changes hands.
                let kept: Vec<(SlotKey, ObjectKey)> = fx
                    .claims
                    .iter()
                    .filter(|c| fx.releases.contains(c))
                    .cloned()
                    .collect();
                fx.claims.retain(|c| !kept.contains(c));
                fx.releases.retain(|r| !kept.contains(r));
            }
            Err(error) => out.unowned.push((op.id(), error)),
        }
        if let Change::RenameTable { from, to, .. } = op.change()
            && from.schema != to.schema
        {
            match pbps_dialect::lower::lower(op, op.planned().strategy, dialect) {
                Ok(lowered) => {
                    fx.intermediate = lowered
                        .intermediate_tables()
                        .into_iter()
                        .map(|t| table_slot(dialect, t))
                        .collect();
                }
                Err(_) => out.unlowered.push(op.id()),
            }
        }
        out.effects.push(fx);
    }
    // A part the plan drops is gone before its table moves: the move neither
    // carries it out of its schema nor into the new one, its drop releases it
    // (review of #1346, as `rename_order` reads it).
    let dropped: BTreeSet<ObjectKey> = operations
        .iter()
        .zip(&out.effects)
        .filter(|(op, _)| !matches!(op.change(), Change::RenameTable { .. }))
        .flat_map(|(_, fx)| fx.releases.iter().map(|(_, h)| h.clone()))
        .filter(|h| matches!(h, ObjectKey::Part { .. }))
        .collect();
    for (op, fx) in operations.iter().zip(&mut out.effects) {
        if matches!(op.change(), Change::RenameTable { .. }) {
            fx.claims.retain(|(_, h)| !dropped.contains(h));
            fx.releases.retain(|(_, h)| !dropped.contains(h));
        }
    }
    out
}

fn table_slot(dialect: &dyn Dialect, table: &TableName) -> SlotKey {
    slot(
        dialect,
        &pbps_model::operation::Address {
            kind: pbps_model::operation::NameKind::Table,
            schema: table.schema.clone(),
            table: None,
            name: table.name.clone(),
            signature: None,
        },
    )
}

/// An edge a name's handover implies: `before` gives the name up, `after`
/// takes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotEdge {
    pub before: usize,
    pub after: usize,
    pub slot: SlotKey,
}

/// The slots `a` and `b` are one name in, by the facts at hand: equal slots
/// always, and alike spellings where the catalog says so.
fn groups(slots: &Slots, facts: Option<&CatalogFacts>) -> BTreeMap<SlotKey, SlotKey> {
    // Each slot's representative: itself, or the least slot the catalog
    // reports alike with it.
    let mut all: BTreeSet<&SlotKey> = BTreeSet::new();
    for fx in &slots.effects {
        all.extend(fx.claims.iter().map(|(s, _)| s));
        all.extend(fx.releases.iter().map(|(s, _)| s));
    }
    let mut rep: BTreeMap<SlotKey, SlotKey> =
        all.iter().map(|s| ((*s).clone(), (*s).clone())).collect();
    if let Some(facts) = facts {
        // Union by the least key, to a fixed point: the alike pairs are few.
        let mut changed = true;
        while changed {
            changed = false;
            for (a, b) in &facts.alike {
                let (Some(ra), Some(rb)) = (rep.get(a).cloned(), rep.get(b).cloned()) else {
                    continue;
                };
                if ra != rb {
                    let least = ra.clone().min(rb.clone());
                    for r in rep.values_mut() {
                        if *r == ra || *r == rb {
                            r.clone_from(&least);
                        }
                    }
                    changed = true;
                }
            }
        }
    }
    rep
}

/// Release before claim, where the evidence names whom a claim replaces.
pub fn edges(slots: &Slots, facts: Option<&CatalogFacts>) -> Vec<SlotEdge> {
    let rep = groups(slots, facts);
    // Per name: each holder's claims and releases, by step.
    type Steps = (Vec<usize>, Vec<usize>);
    let mut by_slot: BTreeMap<&SlotKey, BTreeMap<&ObjectKey, Steps>> = BTreeMap::new();
    for (i, fx) in slots.effects.iter().enumerate() {
        for (s, h) in &fx.claims {
            by_slot
                .entry(&rep[s])
                .or_default()
                .entry(h)
                .or_default()
                .0
                .push(i);
        }
        for (s, h) in &fx.releases {
            by_slot
                .entry(&rep[s])
                .or_default()
                .entry(h)
                .or_default()
                .1
                .push(i);
        }
    }
    let mut out = BTreeSet::new();
    for (s, holders) in &by_slot {
        for (holder, (claims, releases)) in holders {
            let others = holders.iter().filter(|(h, _)| *h != holder);
            if claims.is_empty() {
                // The name's first holder: every other claim waits for it to
                // give the name up.
                for (_, (their_claims, _)) in others {
                    for &r in releases {
                        for &c in their_claims {
                            if r != c {
                                out.insert(SlotEdge {
                                    before: r,
                                    after: c,
                                    slot: (*s).clone(),
                                });
                            }
                        }
                    }
                }
            } else if releases.is_empty() {
                // The name's last holder: it takes the name once every other
                // holder has given it up.
                for (_, (_, their_releases)) in others {
                    for &r in their_releases {
                        for &c in claims {
                            if r != c {
                                out.insert(SlotEdge {
                                    before: r,
                                    after: c,
                                    slot: (*s).clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Why the replay refused a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotViolation {
    /// A step claims a name another object still holds, and that object
    /// gives it up later in the plan: another order would have it free.
    Held {
        operation: OperationId,
        slot: SlotKey,
        holder: String,
    },
    /// A step claims a name another object holds and the plan never gives
    /// up: no order makes it free. The declarations collide, which is the
    /// loader's to refuse (`check_constraint_names`), not an order's.
    Contested {
        operation: OperationId,
        slot: SlotKey,
        holder: String,
    },
    /// A statement of a step passes through a name another object holds.
    IntermediateHeld {
        operation: OperationId,
        slot: SlotKey,
        holder: String,
    },
    /// An edge the handovers imply runs the other way.
    Edge {
        before: OperationId,
        after: OperationId,
        slot: SlotKey,
    },
}

/// A comparison the replay could not make: the two spellings may be one name
/// under the engine's rule, and no fact says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub operation: OperationId,
    pub claimed: SlotKey,
    pub held: SlotKey,
}

/// Who holds a slot during the replay.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Occupier {
    Managed(ObjectKey),
    /// A catalog occupant, by its kind and the table it belongs to.
    Catalog {
        kind: String,
        owner: Option<ObjectKey>,
        /// The owner's column it belongs to, by the column's key and the
        /// baseline's name.
        column: Option<(ObjectKey, String)>,
    },
}

impl Occupier {
    fn describe(&self) -> String {
        match self {
            Occupier::Managed(key) => format!("{key:?}"),
            Occupier::Catalog { kind, .. } => format!("an unmanaged {kind}"),
        }
    }
}

/// Replays the order given and refuses a claim on a name still held.
///
/// The names held at the start are those a step gives up before taking them,
/// and the catalog's occupants. A name no step gives up and no fact reports
/// is not tracked: the desired state, which the loader checks, holds it once.
pub fn verify(
    operations: &[Operation],
    slots: &Slots,
    dialect: &dyn Dialect,
    facts: Option<&CatalogFacts>,
    owners: &dyn Owners,
    before: Snapshot<'_>,
) -> (Vec<SlotViolation>, Vec<Skipped>) {
    let mut violations = Vec::new();
    let mut skipped = Vec::new();
    let mut held: Vec<(SlotKey, Occupier)> = Vec::new();
    // First holders: a step that gives a name up before it takes it held the
    // name at the start.
    let mut seen: BTreeSet<(&SlotKey, &ObjectKey)> = BTreeSet::new();
    for fx in &slots.effects {
        for (s, h) in &fx.releases {
            if seen.insert((s, h)) {
                held.push((s.clone(), Occupier::Managed(h.clone())));
            }
        }
        for (s, h) in &fx.claims {
            seen.insert((s, h));
        }
    }
    if let Some(facts) = facts {
        for o in facts.occupants.iter().filter(|o| !declared(before, o)) {
            let table = o
                .owner
                .as_ref()
                .and_then(|t| base_table_named(owners, before, t));
            let column = table
                .as_ref()
                .zip(o.column.as_ref())
                .and_then(|(t, c)| t.column(owners, c).map(|k| (k, c.clone())));
            held.push((
                o.slot.clone(),
                Occupier::Catalog {
                    kind: o.kind.clone(),
                    owner: table.map(|t| t.table_key()),
                    column,
                },
            ));
        }
    }
    let claim = |held: &[(SlotKey, Occupier)],
                 operation: OperationId,
                 s: &SlotKey,
                 holder: Option<&ObjectKey>,
                 skipped: &mut Vec<Skipped>|
     -> Option<(String, bool)> {
        for (h, who) in held {
            if matches!(who, Occupier::Managed(k) if Some(k) == holder) {
                continue;
            }
            match same(dialect, facts, s, h) {
                Same::Yes => {
                    let gives_up = match who {
                        Occupier::Managed(k) => slots
                            .effects
                            .iter()
                            .any(|fx| fx.releases.iter().any(|(rs, rh)| rs == h && rh == k)),
                        // An occupant leaves with its table or its column;
                        // one no step takes away holds the name whatever the
                        // order.
                        Occupier::Catalog { .. } => operations.iter().any(|op| {
                            let key = op.key(owners).ok();
                            leaves(op, key.as_ref(), who) || moves(op, key.as_ref(), who)
                        }),
                    };
                    return Some((who.describe(), gives_up));
                }
                Same::Unknown => skipped.push(Skipped {
                    operation,
                    claimed: s.clone(),
                    held: h.clone(),
                }),
                Same::No => {}
            }
        }
        None
    };
    for (op, fx) in operations.iter().zip(&slots.effects) {
        for (s, h) in &fx.releases {
            held.retain(|(hs, who)| !(hs == s && *who == Occupier::Managed(h.clone())));
        }
        // A catalog occupant goes with its table: out with a drop, and into
        // the new schema with a move, where it claims its name again.
        let mut landed = Vec::new();
        let key = op.key(owners).ok();
        held.retain(|(_, who)| !leaves(op, key.as_ref(), who));
        if let Change::RenameTable { from, to, .. } = op.change()
            && from.schema != to.schema
        {
            let (moving, staying): (Vec<_>, Vec<_>) = std::mem::take(&mut held)
                .into_iter()
                .partition(|(_, who)| moves(op, key.as_ref(), who));
            held = staying;
            for (mut s, who) in moving {
                if let pbps_dialect::slots::Namespace::Relations(schema) = &mut s.namespace {
                    schema.clone_from(&to.schema);
                }
                landed.push((s, who));
            }
        }
        for s in &fx.intermediate {
            if let Some((holder, _)) = claim(&held, op.id(), s, None, &mut skipped) {
                violations.push(SlotViolation::IntermediateHeld {
                    operation: op.id(),
                    slot: s.clone(),
                    holder,
                });
            }
        }
        let refused =
            |operation: OperationId, slot: &SlotKey, (holder, gives_up): (String, bool)| {
                if gives_up {
                    SlotViolation::Held {
                        operation,
                        slot: slot.clone(),
                        holder,
                    }
                } else {
                    SlotViolation::Contested {
                        operation,
                        slot: slot.clone(),
                        holder,
                    }
                }
            };
        for (s, who) in landed {
            if let Some(found) = claim(&held, op.id(), &s, None, &mut skipped) {
                violations.push(refused(op.id(), &s, found));
            }
            held.push((s, who));
        }
        for (s, h) in &fx.claims {
            if let Some(found) = claim(&held, op.id(), s, Some(h), &mut skipped) {
                violations.push(refused(op.id(), s, found));
            }
            held.push((s.clone(), Occupier::Managed(h.clone())));
        }
    }
    for e in edges(slots, facts) {
        if e.before >= e.after {
            violations.push(SlotViolation::Edge {
                before: operations[e.before].id(),
                after: operations[e.after].id(),
                slot: e.slot,
            });
        }
    }
    (violations, skipped)
}

/// Whether the baseline's declarations name a catalog occupant: a table, a
/// view or a table's named part, whose name the plan's own claims and releases
/// already track. The reads return every object at the names they ask, so
/// the managed ones are among them.
fn declared(before: Snapshot<'_>, o: &pbps_dialect::slots::Occupant) -> bool {
    let pbps_dialect::slots::Namespace::Relations(schema) = &o.slot.namespace else {
        return false;
    };
    let name = &o.slot.name;
    let here = TableName::new(schema.clone(), name.clone());
    if let Some(owner) = o.owner.as_ref().and_then(|t| before.schema.tables.get(t)) {
        return owner
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.name.as_ref() == Some(name))
            || owner.unique.contains_key(name)
            || owner.indexes.contains_key(name)
            || owner.checks.contains_key(name)
            || owner.foreign_keys.contains_key(name);
    }
    before.schema.tables.contains_key(&here)
        || before
            .schema
            .modules
            .contains_key(&pbps_model::ModuleId::Named(here))
}

/// Whether `op`, whose own key is `key`, takes a catalog occupant out of its
/// schema: its table dropped, or a column's own occupant (an owned sequence, a
/// default constraint) at the first step that may drop or rename it, which is
/// its column dropped, renamed, retyped or given another default or
/// numbering, or its table renamed with that default (`defaults`). Whether
/// that step really gives the name up is the engine's walk (`object_order`,
/// `order_carried_moves`); reading it as given up can let a misorder pass,
/// never refuse a valid plan (DEC-1964.2).
fn leaves(op: &Operation, key: Option<&ObjectKey>, who: &Occupier) -> bool {
    let Occupier::Catalog { owner, column, .. } = who else {
        return false;
    };
    let owned = owner.is_some() && owner.as_ref() == key;
    if let Change::DropTable { .. } = op.change() {
        return owned;
    }
    let Some((column, column_name)) = column else {
        return false;
    };
    if let Change::RenameTable { defaults, .. } = op.change() {
        return owned && defaults.contains(column_name);
    }
    matches!(
        op.change(),
        Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnNumbering { .. }
    ) && key
        .and_then(column_of)
        .is_some_and(|k| Some(k) == column_of(column))
}

/// The column a key is about: a column's own key, or its default's, which is
/// what a default change acts on.
fn column_of(key: &ObjectKey) -> Option<&pbps_model::operation::ColumnKey> {
    match key {
        ObjectKey::Column(c) | ObjectKey::Default(c) => Some(c),
        ObjectKey::Table(_)
        | ObjectKey::Role(_)
        | ObjectKey::Module(_)
        | ObjectKey::NotNull(_)
        | ObjectKey::Part { .. }
        | ObjectKey::Row { .. }
        | ObjectKey::Grant { .. } => None,
    }
}

/// Whether `op` moves a catalog occupant's table to another schema, which
/// carries the occupant there.
fn moves(op: &Operation, key: Option<&ObjectKey>, who: &Occupier) -> bool {
    let Occupier::Catalog { owner, .. } = who else {
        return false;
    };
    matches!(op.change(), Change::RenameTable { from, to, .. } if from.schema != to.schema)
        && owner.is_some()
        && owner.as_ref() == key
}
