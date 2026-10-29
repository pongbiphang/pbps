//! Pending boundary comparisons remain attached to the immutable proposal.
//! A settled permutation alone does not discharge a membership contract.
use super::membership;
use pbps_model::operation::{
    Effects, End, Lineage, ModuleEdits, Operation, OperationId, Operations, Origin, Snapshot,
    Standing,
    symbolic::{Clause, MoveKind, State},
};
use pbps_model::{Change, ChangeSet, Uid};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Boundary {
    pub child: Uid,
    pub parent: Uid,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Obligations {
    pending: BTreeSet<Boundary>,
    // Bind discharge to the actual immutable payload. Findings and risks do
    // not alter replay, but respelling, rewriting or reordering a Change does.
    checked: Option<Receipt>,
    modules: Option<ModuleEdits>,
    rebuild: bool,
}
#[derive(Debug, Clone)]
struct Receipt {
    steps: Vec<Change>,
    events: Vec<(OperationId, Origin)>,
}
impl Obligations {
    pub(super) fn parents(&self) -> BTreeSet<Uid> {
        self.pending.iter().map(|b| b.parent.clone()).collect()
    }
    pub(super) fn rebuilt(&self) -> bool {
        self.rebuild
    }
    pub(super) fn enable_rebuild(&mut self) {
        self.rebuild = true;
    }

    pub(super) fn require(
        &mut self,
        pending: impl IntoIterator<Item = Boundary>,
        modules: ModuleEdits,
    ) {
        self.pending.extend(pending);
        if !self.pending.is_empty() {
            self.modules = Some(modules);
            self.checked = None;
        }
    }
    pub(super) fn module_edits(&self) -> ModuleEdits {
        self.modules
            .expect("registered boundaries retain their engine's module policy")
    }
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    pub(super) fn ensure<S>(&self, plan: &super::PlanningPlan<S>) -> Result<(), String> {
        if self.pending.is_empty()
            || self.checked.as_ref().is_some_and(|checked| {
                let events = plan.identities.clone().unwrap_or_else(|| {
                    Operations::expanded(
                        plan.logical,
                        plan.origins
                            .iter()
                            .cloned()
                            .zip(plan.changes.iter().cloned()),
                        ModuleEdits::InPlace,
                    )
                });
                checked
                    .steps
                    .iter()
                    .eq(plan.changes.iter().map(|p| &p.change))
                    && checked
                        .events
                        .iter()
                        .zip(events.iter())
                        .all(|((id, origin), op)| *id == op.id() && origin == op.origin())
                    && plan.origins.iter().eq(events.iter().map(|op| op.origin()))
            })
        {
            Ok(())
        } else {
            let boundary = self.pending.first().unwrap();
            Err(format!(
                "membership boundary for child {} of parent {} is pending or changed after discharge; {}",
                boundary.child,
                boundary.parent,
                membership::REMEDY
            ))
        }
    }
    pub(super) fn discharge(
        &mut self,
        ops: &[Operation],
        fx: &[Effects],
        base: &State,
    ) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        for boundary in &self.pending {
            if !fx.iter().flat_map(|f| &f.symbolic).any(|c|
                matches!(c, Clause::Move(m) if m.child.uid() == &boundary.child && m.parent.uid() == &boundary.parent
                    && matches!(m.kind, MoveKind::Attach | MoveKind::Detach | MoveKind::Drop))) {
                return Err(format!("membership boundary for child {} of parent {} has no symbolic transfer; {}",
                    boundary.child, boundary.parent, membership::REMEDY));
            }
        }
        membership::project(ops, fx, base).map_err(|why| why.to_string())?;
        self.checked = Some(Receipt {
            steps: ops.iter().map(|op| op.change().clone()).collect(),
            events: ops
                .iter()
                .map(|op| (op.id(), op.origin().clone()))
                .collect(),
        });
        Ok(())
    }
}

/// Only UID continuity identifies a parent across an address change. The
/// differ registers a comparison without deciding on which side it belongs.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn meetings(
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    changes: &ChangeSet,
) -> BTreeSet<Boundary> {
    let mut result = BTreeSet::new();
    for planned in &changes.changes {
        let (child, parent, side) = match &planned.change {
            Change::AttachPartition { uid, parent, .. } => (uid, parent, after),
            Change::DetachPartition { uid, parent, .. } => (uid, parent, before),
            Change::DropTable {
                uid,
                detach_from: Some(parent),
                ..
            } => (uid, parent, before),
            _ => continue,
        };
        let parent_uid = if !matches!(planned.change, Change::AttachPartition { .. }) {
            before
                .ids
                .tables
                .get(child)
                .and_then(|n| before.schema.tables.get(n))
                .and_then(|t| t.partition_of.as_ref())
                .and_then(|of| before.ids.table_uid(&of.parent))
                // An IDs-only query scope still has the recorded base name;
                // this fallback supplies no transfer or execution authority.
                .or_else(|| before.ids.table_uid(parent))
        } else {
            side.ids.table_uid(parent)
        };
        let Some(parent_uid) = parent_uid else {
            continue;
        };
        if changes.changes.iter().any(|p| {
            if let Change::RenameTable { uid, .. } = &p.change {
                return uid == parent_uid;
            }
            let Some(uid) = p.change.parent_column_uid() else {
                return false;
            };
            [before, after].iter().any(|side| {
                side.ids
                    .columns
                    .get(uid)
                    .and_then(|at| side.ids.table_uid(&at.table))
                    == Some(parent_uid)
            })
        }) {
            result.insert(Boundary {
                child: child.clone(),
                parent: parent_uid.clone(),
            });
        }
    }
    result
}

fn snapshot(side: crate::Side<'_>) -> Snapshot<'_> {
    Snapshot {
        schema: side.schema,
        ids: side.ids,
    }
}

/// Relations whose stored columns the connected seam must read: a parent whose
/// columns change in the same plan as its membership, and every table that
/// stands in its family at the baseline. This is a catalog query scope, never
/// the target of a recursive write.
pub fn catalog_tables(
    changes: &ChangeSet,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
) -> BTreeSet<Uid> {
    let parents: BTreeSet<_> = meetings(before, after, changes)
        .into_iter()
        .map(|b| b.parent)
        .collect();
    let lineage = Lineage::of(snapshot(before), snapshot(after));
    lineage
        .families()
        .filter(|(p, f)| parents.contains(p.uid()) && f.present(End::Base))
        .flat_map(|(p, f)| {
            let held = f
                .children()
                .filter(|c| f.standing(c, End::Base) != Standing::Absent);
            std::iter::once(p).chain(held).map(|r| r.uid().clone())
        })
        .collect()
}

#[cfg(test)]
mod tests;

/// Bind prototype inputs only where the ordinary effect contract already
/// determines a unique writer chain. This does not add ordering preferences:
/// incomparable writers remain the symbolic compiler's named fork.
pub(super) fn bind(ops: &Operations, effects: &mut [Effects], baseline: &super::Baseline) {
    use pbps_model::operation::symbolic::{Key, Version};
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<Key, Vec<(usize, usize)>> = BTreeMap::new();
    for (at, effect) in effects.iter().enumerate() {
        for (clause, c) in effect.symbolic.iter().enumerate() {
            if let Clause::Recurse(w) = c {
                groups.entry(w.key.clone()).or_default().push((at, clause));
            }
        }
    }
    if groups.values().all(|g| g.len() < 2) {
        return;
    }
    let concrete: Vec<_> = effects
        .iter()
        .cloned()
        .map(|mut f| {
            f.symbolic.clear();
            f
        })
        .collect();
    let graph = super::graph(
        ops.as_slice(),
        &concrete,
        baseline,
        &Default::default(),
        super::Checks::ALL,
    );
    let mut reach = BTreeMap::new();
    for (key, group) in groups.into_iter().filter(|(_, g)| g.len() > 1) {
        for &(at, _) in &group {
            reach.entry(at).or_insert_with(|| {
                super::commute::reaches(&graph, ops.len(), at, super::commute::Direction::Forward)
            });
        }
        // A strict total reach relation is precisely one existing chain.
        // A fork or cycle supplies no unique predecessor and stays unbound.
        if !group.iter().enumerate().all(|(i, a)| {
            group[i + 1..]
                .iter()
                .all(|b| reach[&a.0].contains(&b.0) ^ reach[&b.0].contains(&a.0))
        }) {
            continue;
        }
        let mut ordered = group;
        ordered.sort_by(|a, b| reach[&b.0].contains(&a.0).cmp(&reach[&a.0].contains(&b.0)));
        let Some(base) = baseline
            .symbolic
            .cells
            .get(&(key.parent.clone(), key.property.clone()))
        else {
            continue;
        };
        let mut version = base.version;
        let mut value = base.value.clone();
        for (at, clause) in ordered {
            let Clause::Recurse(w) = &mut effects[at].symbolic[clause] else {
                unreachable!()
            };
            w.input = version;
            w.from = value;
            value = w.to.clone();
            version = Version::After(ops.as_slice()[at].id());
        }
    }
}

/// Complete engine merge tables from the declared version values, including
/// intermediate releases. This indexes descriptions, not trial schedules.
pub(super) fn transfers(
    effects: &mut [Effects],
    base: &State,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) {
    use pbps_model::operation::symbolic::{
        Boundary as TransferBoundary, Fact, TransferPolicy, Value,
    };
    use std::collections::BTreeMap;
    let mut values: BTreeMap<(_, _), Vec<Value>> = BTreeMap::new();
    let mut insert = |key, value: &Fact<Value>| {
        if let Ok(v) = value.known() {
            let list = values.entry(key).or_default();
            if !list.contains(v) {
                list.push(v.clone());
            }
        }
    };
    for (key, cell) in &base.cells {
        insert(key.clone(), &cell.value);
    }
    for clause in effects.iter().flat_map(|f| &f.symbolic) {
        if let Clause::Recurse(w) = clause {
            for value in [&w.from, &w.to] {
                insert((w.key.parent.clone(), w.key.property.clone()), value);
            }
        }
    }
    for effect in effects {
        let mut errors = Vec::new();
        for clause in &mut effect.symbolic {
            let Clause::Move(m) = clause else { continue };
            if m.kind != MoveKind::Attach {
                continue;
            }
            let TransferBoundary::All(transfers) = &mut m.boundary else {
                continue;
            };
            for t in transfers {
                if !matches!(t.policy, TransferPolicy::Merge(_)) {
                    continue;
                }
                let parents = values
                    .get(&(m.parent.clone(), t.parent.clone()))
                    .map_or(&[][..], Vec::as_slice);
                let candidates = values
                    .get(&(m.child.clone(), t.from.clone()))
                    .map_or(&[][..], Vec::as_slice);
                match dialect.transfer_policy(t, parents, candidates) {
                    Ok(policy) => t.policy = policy,
                    Err(reason) => errors.push(Clause::Require(Fact::Unsupported {
                        sqlstate: None,
                        reason,
                    })),
                }
            }
        }
        effect.symbolic.extend(errors);
    }
}
