//! Name slots beside the effects (#1776 slices S1b-ii and S4, DEC-1964.2,
//! DEC-1957.1).
//!
//! `pbps_model::operation::names` says which names each step claims and
//! releases; `pbps_dialect::slots` says which namespace holds each and whether
//! two spellings are one name. This module reads the two three ways:
//!
//! - [`edges`]: release before claim, where the evidence names the holder a
//!   claim replaces. A name's first holder is one that only gives it up; its
//!   last is one that only takes it. Between them, an object that takes the
//!   name and gives it up again has no place the evidence fixes, and no edge:
//!   proposing one is a search's, never position's. A catalog occupant the
//!   plan takes away at one step is a first holder too ([`carried`]), and a
//!   move passing through a name holds it between its statements. The edges
//!   are `Identity` edges a schedule settles ([`schedule`], [`handovers`]);
//!   a connected PostgreSQL plan orders its carried moves by them.
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
use pbps_dialect::slots::{
    CatalogFacts, Same, SlotKey, baseline_names, names, same, slots as slots_of,
};
use pbps_model::operation::{
    ObjectKey, Operation, OperationId, OwnerError, Owners, Snapshot, base_table_named,
};
use pbps_model::{Change, TableName};

/// One step's slots: what it claims and releases, each with its holder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotEffects {
    pub claims: Vec<(SlotKey, ObjectKey)>,
    pub releases: Vec<(SlotKey, ObjectKey)>,
    /// A partition's arrival inherits these names from a standing module.
    /// Its arrival does not make that module an independent new holder.
    pub inherited: Vec<(SlotKey, ObjectKey)>,
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
        match names(dialect, op, owners, before, operations) {
            Ok(n) => {
                fx.claims = n
                    .claims
                    .iter()
                    .flat_map(|c| {
                        slots_of(dialect, &c.address)
                            .into_iter()
                            .map(|s| (s, c.holder.clone()))
                    })
                    .collect();
                fx.releases = n
                    .releases
                    .iter()
                    .flat_map(|r| {
                        slots_of(dialect, &r.address)
                            .into_iter()
                            .map(|s| (s, r.holder.clone()))
                    })
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
                if matches!(
                    op.change(),
                    Change::CreateTable { .. } | Change::AttachPartition { .. }
                ) {
                    fx.inherited = fx
                        .claims
                        .iter()
                        .filter(|(_, h)| matches!(h, ObjectKey::Module(_)))
                        .cloned()
                        .collect();
                }
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
                        .filter_map(|t| table_slot(dialect, t))
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

fn table_slot(dialect: &dyn Dialect, table: &TableName) -> Option<SlotKey> {
    // A table's name is one slot in its schema.
    slots_of(
        dialect,
        &pbps_model::operation::Address {
            kind: pbps_model::operation::NameKind::Table,
            schema: table.schema.clone(),
            table: None,
            name: table.name.clone(),
            signature: None,
        },
    )
    .into_iter()
    .next()
}

/// An edge a name's handover implies: `before` gives the name up, `after`
/// takes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotEdge {
    pub before: usize,
    pub after: usize,
    pub slot: SlotKey,
    /// Whether a catalog occupant is the holder that gives the name up or
    /// takes it, so the edge is the catalog's evidence, not the plan's.
    pub catalog: bool,
}

/// What one catalog occupant does in the plan: the one step that takes it
/// away from its name, and the name it lands on where that step moves its
/// table to another schema (#1957, DEC-1957.1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Carried {
    /// Its position in the facts' occupants.
    pub occupant: usize,
    pub slot: SlotKey,
    pub step: usize,
    pub lands: Option<SlotKey>,
}

/// The catalog's occupants as holders, for [`edges`]: each one the plan
/// takes away at exactly one step. An occupant no step takes away holds its
/// name whatever the order, which the replay reports. A step that drops or
/// moves its table, or drops its column, takes it away for certain; one
/// that only changes its column may (`leaves`). With no certain step, one
/// that several steps may take away, a column's default given another
/// default and then renamed, has no step the evidence fixes, and no edge:
/// which one really frees it is the engine's walk to say. Occupants at a name a
/// baseline object holds are that object, tracked by its own claims and
/// releases, as in [`verify`].
pub fn carried(
    operations: &[Operation],
    facts: &CatalogFacts,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    dialect: &dyn Dialect,
) -> Vec<Carried> {
    let declared: BTreeSet<SlotKey> = baseline_names(dialect, owners, before)
        .into_iter()
        .flat_map(|named| slots_of(dialect, &named.address))
        .collect();
    let keys: Vec<Option<ObjectKey>> = operations.iter().map(|op| op.key(owners).ok()).collect();
    let mut out = Vec::new();
    for (k, o) in facts.occupants.iter().enumerate() {
        if declared.contains(&o.slot) {
            continue;
        }
        let table = o
            .owner
            .as_ref()
            .and_then(|t| base_table_named(owners, before, t));
        let column = table
            .as_ref()
            .zip(o.column.as_ref())
            .and_then(|(t, c)| t.column(owners, c).map(|key| (key, c.clone())));
        let who = Occupier::Catalog {
            kind: o.kind.clone(),
            owner: table.map(|t| t.table_key()),
            column,
        };
        // A step that moves or drops the occupant's table, or drops its
        // column, takes it away on every engine. The other column changes
        // `leaves` reads as freeing it may not: a column rename keeps a
        // PostgreSQL sequence's name (review of #2119). A table has at most
        // one move or drop, and a column one drop, so the step that frees
        // the occupant's own name is decided by kind, not by position: its
        // table's move, which carries it out of the schema whatever its
        // column does after; else its table's drop; else its column's drop;
        // else the one step that may. A column dropped before the move frees
        // the name earlier still, so the edge holds the claim longer than it
        // must, and never too short.
        let possible: Vec<usize> = operations
            .iter()
            .zip(&keys)
            .enumerate()
            .filter(|(_, (op, key))| {
                leaves(op, key.as_ref(), &who) || moves(op, key.as_ref(), &who)
            })
            .map(|(i, _)| i)
            .collect();
        let ranked: [&dyn Fn(usize) -> bool; 3] = [
            &|i| moves(&operations[i], keys[i].as_ref(), &who),
            &|i| matches!(operations[i].change(), Change::DropTable { .. }),
            &|i| matches!(operations[i].change(), Change::DropColumn { .. }),
        ];
        let steps: Vec<usize> = ranked
            .iter()
            .map(|kind| possible.iter().copied().filter(|&i| kind(i)).collect())
            .find(|steps: &Vec<usize>| !steps.is_empty())
            .unwrap_or_else(|| possible.clone());
        let [step] = steps[..] else { continue };
        // As in the replay, an occupant the step takes out of its schema
        // does not land anywhere: a default renamed with its table's move.
        let key = keys[step].as_ref();
        let lands = if let Change::RenameTable { from, to, .. } = operations[step].change()
            && from.schema != to.schema
            && !leaves(&operations[step], key, &who)
            && moves(&operations[step], key, &who)
        {
            let mut landed = o.slot.clone();
            if let pbps_dialect::slots::Namespace::Relations(schema) = &mut landed.namespace {
                schema.clone_from(&to.schema);
            }
            Some(landed)
        } else {
            None
        };
        out.push(Carried {
            occupant: k,
            slot: o.slot.clone(),
            step,
            lands,
        });
    }
    out
}

/// The slots `a` and `b` are one name in, by the facts at hand: equal slots
/// always, and alike spellings where the catalog says so.
fn groups(
    slots: &Slots,
    facts: Option<&CatalogFacts>,
    carried: &[Carried],
) -> BTreeMap<SlotKey, SlotKey> {
    // Each slot's representative: itself, or the least slot the catalog
    // reports alike with it.
    let mut all: BTreeSet<&SlotKey> = BTreeSet::new();
    for fx in &slots.effects {
        all.extend(fx.claims.iter().map(|(s, _)| s));
        all.extend(fx.releases.iter().map(|(s, _)| s));
    }
    for c in carried {
        all.insert(&c.slot);
        all.extend(&c.lands);
    }
    for fx in &slots.effects {
        all.extend(&fx.intermediate);
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

/// Who holds a name, for the edges: an object of the plan, a catalog
/// occupant by its position in the facts, or a step passing through a name
/// between two of its statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Holder<'a> {
    Managed(&'a ObjectKey),
    Catalog(usize),
    Passing(usize),
}

/// Release before claim, where the evidence names whom a claim replaces:
/// the plan's own names, the names a step passes through between its
/// statements, and the catalog's occupants as the first holders of theirs
/// (`carried`, DEC-1957.1).
pub fn edges(slots: &Slots, facts: Option<&CatalogFacts>, carried: &[Carried]) -> Vec<SlotEdge> {
    handovers_of(slots, facts, carried, true)
}

/// [`edges`], with the names steps pass through as holders where `passing`.
fn handovers_of(
    slots: &Slots,
    facts: Option<&CatalogFacts>,
    carried: &[Carried],
    passing: bool,
) -> Vec<SlotEdge> {
    let rep = groups(slots, facts, carried);
    // Per name: each holder's claims and releases, by step.
    type Steps = (Vec<usize>, Vec<usize>);
    let mut by_slot: BTreeMap<&SlotKey, BTreeMap<Holder<'_>, Steps>> = BTreeMap::new();
    let released: BTreeSet<_> = slots.effects.iter().flat_map(|fx| &fx.releases).collect();
    for (i, fx) in slots.effects.iter().enumerate() {
        for claimed in &fx.claims {
            let (s, h) = claimed;
            // An inherited clone belongs to its standing parent trigger.
            // Releasing that trigger must precede a competing claim even
            // when the partition would otherwise create the clone first.
            // Replay still sees the actual, operation-local claim below.
            if fx.inherited.contains(claimed) && released.contains(claimed) {
                continue;
            }
            by_slot
                .entry(&rep[s])
                .or_default()
                .entry(Holder::Managed(h))
                .or_default()
                .0
                .push(i);
        }
        for (s, h) in &fx.releases {
            by_slot
                .entry(&rep[s])
                .or_default()
                .entry(Holder::Managed(h))
                .or_default()
                .1
                .push(i);
        }
    }
    // A step that passes through a name takes it and gives it up again: a
    // holder between the name's first and last, which waits for the first
    // and which the last waits for, as a borrower does.
    for (i, fx) in slots.effects.iter().enumerate().filter(|_| passing) {
        for s in &fx.intermediate {
            let steps = by_slot
                .entry(&rep[s])
                .or_default()
                .entry(Holder::Passing(i))
                .or_default();
            steps.0.push(i);
            steps.1.push(i);
        }
    }
    // An occupant holds its name from the start and gives it up at its
    // step; carried to another schema, it takes the name it lands on there.
    for c in carried {
        by_slot
            .entry(&rep[&c.slot])
            .or_default()
            .entry(Holder::Catalog(c.occupant))
            .or_default()
            .1
            .push(c.step);
        if let Some(lands) = &c.lands {
            by_slot
                .entry(&rep[lands])
                .or_default()
                .entry(Holder::Catalog(c.occupant))
                .or_default()
                .0
                .push(c.step);
        }
    }
    let mut out = BTreeSet::new();
    for (s, holders) in &by_slot {
        for (holder, (claims, releases)) in holders {
            let others = holders.iter().filter(|(h, _)| *h != holder);
            let catalog = |other: &Holder<'_>| {
                matches!(holder, Holder::Catalog(_)) || matches!(other, Holder::Catalog(_))
            };
            if claims.is_empty() {
                // The name's first holder: every other claim waits for it to
                // give the name up.
                for (other, (their_claims, _)) in others {
                    for &r in releases {
                        for &c in their_claims {
                            if r != c {
                                out.insert(SlotEdge {
                                    before: r,
                                    after: c,
                                    slot: (*s).clone(),
                                    catalog: catalog(other),
                                });
                            }
                        }
                    }
                }
            } else if releases.is_empty() {
                // The name's last holder: it takes the name once every other
                // holder has given it up.
                for (other, (_, their_releases)) in others {
                    for &r in their_releases {
                        for &c in claims {
                            if r != c {
                                out.insert(SlotEdge {
                                    before: r,
                                    after: c,
                                    slot: (*s).clone(),
                                    catalog: catalog(other),
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

/// The handover edges as a schedule's: a name changing hands is `Identity`
/// (DEC-1956.1), from the plan's names (`Effect`) or a catalog occupant's
/// (`Catalog`).
pub fn schedule(schedule: &mut super::Schedule, edges: &[SlotEdge]) {
    for e in edges {
        let source = if e.catalog {
            super::Source::Catalog
        } else {
            super::Source::Effect
        };
        schedule.edge(
            e.before,
            e.after,
            source,
            pbps_model::resolver::OrderReason::Identity,
        );
    }
}

/// A plan's name handovers as a schedule over its changes, from the plan's
/// names and the catalog's occupants (#1957, DEC-1957.1), for a pass that
/// orders by them. Every other edge of the plan is the caller's to keep: a
/// release build computes no effects, so what the schedule may move is the
/// caller's rule (DEC-1957.2). `Err` where the plan's names cannot be read.
pub fn handovers(
    changes: &[pbps_model::PlannedChange],
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    facts: &CatalogFacts,
) -> Result<super::Schedule, String> {
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners};
    let modes = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let operations = Operations::of_plan(changes, modes);
    let names =
        pbps_model::plan_names::PlanNames::of(changes, desired.ids).map_err(|e| e.to_string())?;
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let before = Snapshot {
        schema: base.schema,
        ids: base.ids,
    };
    let ops = operations.as_slice();
    let slots = analyse(ops, &owners, before, dialect);
    let held = carried(ops, facts, &owners, before, dialect);
    let mut out = super::Schedule::new(changes.len());
    schedule(&mut out, &edges(&slots, Some(facts), &held));
    Ok(out)
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
    // And what the baseline holds unchanged at a name a step claims, or
    // passes through: a table that stays where a move stops on its way
    // (review of #2052). Only those names are asked: a name nothing claims
    // cannot be refused.
    let wanted: BTreeSet<&SlotKey> = slots
        .effects
        .iter()
        .flat_map(|fx| fx.claims.iter().map(|(s, _)| s).chain(&fx.intermediate))
        .collect();
    let baseline: Vec<(SlotKey, ObjectKey)> = baseline_names(dialect, owners, before)
        .into_iter()
        .flat_map(|named| {
            slots_of(dialect, &named.address)
                .into_iter()
                .map(move |s| (s, named.holder.clone()))
        })
        .collect();
    // A baseline name may be the claimed one under the engine's comparison
    // and not by its spelling, so the comparison selects it: a name it may
    // be is seeded too, and the claim then records what it cannot tell
    // (review of #2052).
    for (s, h) in &baseline {
        let holder = Occupier::Managed(h.clone());
        if wanted
            .iter()
            .any(|w| same(dialect, facts, w, s) != Same::No)
            && !held.iter().any(|(hs, who)| hs == s && *who == holder)
        {
            held.push((s.clone(), holder));
        }
    }
    // The catalog's rows for names the baseline's own objects hold are those
    // objects, tracked by their own claims and releases: a trigger, a view,
    // a routine, a table or a part (review of #2052).
    let declared: BTreeSet<&SlotKey> = baseline.iter().map(|(s, _)| s).collect();
    if let Some(facts) = facts {
        for o in facts
            .occupants
            .iter()
            .filter(|o| !declared.contains(&o.slot))
        {
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
    // The occupants and the names steps pass through are the replay's above,
    // not edges here: it refuses a claim on one still held, and an edge would
    // name the same fault twice.
    for e in handovers_of(slots, facts, &[], false) {
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
        | ObjectKey::Rows { .. }
        | ObjectKey::ColumnOrder(_)
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
