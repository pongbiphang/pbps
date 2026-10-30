//! Pending boundary comparisons remain attached to the immutable proposal.
//! A settled permutation alone does not discharge a membership contract (DEC-2296.10).
use super::membership;
use pbps_model::operation::{
    Effects, End, Lineage, ModuleEdits, Operation, Operations, Snapshot, Standing,
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
    logical_names: Option<ChangeSet>,
}
#[derive(Debug, Clone)]
struct Receipt {
    steps: Vec<Change>,
    events: Vec<Operation>,
    // Transient intent still constrains the order this receipt discharges.
    annotations: Vec<super::Constraint>,
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
        logical: &ChangeSet,
    ) {
        self.pending.extend(pending);
        if !self.pending.is_empty() {
            self.modules = Some(modules);
            self.logical_names.get_or_insert_with(|| logical.clone());
            self.checked = None;
        }
    }
    pub(super) fn names<'a>(&'a self, current: &'a ChangeSet) -> &'a ChangeSet {
        self.logical_names.as_ref().unwrap_or(current)
    }
    pub(super) fn append_names(&mut self, parts: &[pbps_model::PlannedChange]) {
        if let Some(names) = &mut self.logical_names {
            names.changes.extend_from_slice(parts);
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
        if self.pending.is_empty() {
            return Ok(());
        }
        let annotations = plan.annotation_edges()?;
        if self.checked.as_ref().is_some_and(|checked| {
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
            checked.annotations == annotations
                && checked
                    .steps
                    .iter()
                    .eq(plan.changes.iter().map(|p| &p.change))
                && checked.events.len() == events.len()
                && checked
                    .events
                    .iter()
                    .zip(events.iter())
                    .all(|(old, op)| old.same_input_binding(op))
                && plan.origins.iter().eq(events.iter().map(|op| op.origin()))
        }) {
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
        annotations: &[super::Constraint],
    ) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if annotations.iter().any(|edge| edge.before >= edge.after) {
            return Err("an annotation boundary is unsatisfied at membership replay".into());
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
            events: ops.to_vec(),
            annotations: annotations.to_vec(),
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
pub(super) fn bind(
    ops: &Operations,
    effects: &mut [Effects],
    baseline: &super::Baseline,
    annotations: &[super::Constraint],
) {
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
    let mut graph = super::graph(
        ops.as_slice(),
        &concrete,
        baseline,
        &Default::default(),
        super::Checks::ALL,
    );
    // Annotation edges qualify this same writer chain; they do not add a
    // settlement or let an incomparable pair borrow the current permutation.
    graph
        .edges
        .extend(annotations.iter().map(|edge| super::Edge {
            before: edge.before,
            after: edge.after,
            source: edge.source,
            because: super::edges::Because::Declared,
        }));
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

/// Revalidate the actual continuation without adding a step, edge, proof
/// union or settlement. Only this immutable receipt can cross certification.
pub(super) fn replay<S>(
    plan: &mut super::PlanningPlan<S>,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
    stored: Option<&pbps_model::operation::MembershipCatalog>,
) -> Result<(), String> {
    use pbps_model::operation::{PlanOwners, Snapshot};
    use pbps_model::physical_sources::{Located, PhysicalSources};
    use pbps_model::plan_names::{At, PlanNames};
    if plan.boundaries.is_empty() {
        return Ok(());
    }
    let operations = plan
        .identities
        .as_ref()
        .map(|ops| ops.with_steps(&plan.changes))
        .unwrap_or_else(|| {
            Operations::expanded(
                plan.logical,
                plan.origins
                    .iter()
                    .cloned()
                    .zip(plan.changes.iter().cloned()),
                plan.boundaries.module_edits(),
            )
        });
    let names = PlanNames::of(&plan.boundaries.names(plan).changes, after.ids)
        .map_err(|e| e.to_string())?;
    let owners = PlanOwners::new(names, before.ids, after.ids);
    let annotations = plan.annotation_edges()?;
    let effects = super::shadow::analyse_scoped(
        &operations,
        &owners,
        Snapshot {
            schema: before.schema,
            ids: before.ids,
        },
        Snapshot {
            schema: after.schema,
            ids: after.ids,
        },
        dialect,
        None,
        stored,
        true,
        &annotations,
    );
    if let Some((_, error)) = effects.unowned.first() {
        return Err(error.to_string());
    }
    // The full ordered physical column list is checked at ATTACH, not against
    // either endpoint's parent shape. PhysicalSources retains source order.
    let actual = PlanNames::of(&plan.changes, after.ids).map_err(|e| e.to_string())?;
    let physical = PhysicalSources::of(actual, before.schema, before.ids)
        .map_err(|e| format!("{e}; {}", membership::REMEDY))?;
    for (at, op) in operations.iter().enumerate() {
        if let Change::AttachPartition { table, parent, .. } = op.change() {
            if !matches!(physical.members_at(parent, At::Before(at)), Located::Present(members) if !members.contains(table))
            {
                return Err(format!(
                    "{} has no complete standalone membership at its attachment boundary; {}",
                    table.shown(),
                    membership::REMEDY
                ));
            }
            match (
                physical.columns_at(parent, At::Before(at)),
                physical.columns_at(table, At::Before(at)),
            ) {
                (Located::Present(expected), Located::Present(candidate))
                    if expected == candidate => {}
                _ => {
                    return Err(format!(
                        "{} does not match {} in physical column order at its attachment boundary; {}",
                        table.shown(),
                        parent.shown(),
                        membership::REMEDY
                    ));
                }
            }
        }
        if let Change::DetachPartition {
            table,
            parent,
            shape,
            ..
        } = op.change()
        {
            if !matches!(physical.members_at(parent, At::Before(at)), Located::Present(members) if members.contains(table))
            {
                return Err(format!(
                    "{} has no complete attached membership at its detachment boundary; {}",
                    table.shown(),
                    membership::REMEDY
                ));
            }
            let expected: Vec<_> = shape
                .columns
                .keys()
                .chain(shape.computed.keys())
                .cloned()
                .collect();
            if physical.columns_at(table, At::Before(at)) != Located::Present(expected) {
                return Err(format!(
                    "{} does not retain its declared columns in physical order at its detachment boundary; {}",
                    table.shown(),
                    membership::REMEDY
                ));
            }
        }
    }
    plan.boundaries.discharge(
        operations.as_slice(),
        &effects.effects,
        &effects.symbolic,
        &annotations,
    )?;
    lock_window(plan, before, after);
    plan.identities = Some(operations);
    Ok(())
}

/// Describe the existing transaction's lock window; never an ordering rule.
fn lock_window<S>(
    plan: &mut super::PlanningPlan<S>,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
) {
    use pbps_model::change::ColumnProperty;
    use pbps_model::operation::symbolic::Attribute;
    let id = "plan.partition-alter-attach-lock";
    if !plan.changes.iter().any(|p| {
        matches!(&p.change, Change::AttachPartition { uid, .. }
        if plan.boundaries.pending.iter().any(|b| &b.child == uid))
    }) || plan
        .changes
        .iter()
        .any(|p| p.findings.iter().any(|f| f.id == id))
    {
        return;
    }
    let first = plan.changes.iter().enumerate().find_map(|(at, p)| {
        let uid = p.change.parent_column_uid()?;
        let transitions = p.change.column_transitions();
        // Retuning an owned sequence changes only Numbering, not the table.
        // A column statement with no changed value can still hold its lock.
        if !transitions.is_empty()
            && transitions
                .iter()
                .all(|t| t.property == ColumnProperty::Column(Attribute::Numbering))
        {
            return None;
        }
        let parent = [before.ids, after.ids]
            .into_iter()
            .find_map(|ids| ids.columns.get(uid).and_then(|c| ids.table_uid(&c.table)))?;
        plan.boundaries
            .pending
            .iter()
            .any(|b| &b.parent == parent)
            .then_some(at)
    });
    if let Some(at) = first {
        super::writebacks::findings(
            plan,
            &[(
                at,
                pbps_model::Finding::new(
                    id,
                    pbps_model::Severity::Note,
                    "This plan holds ACCESS EXCLUSIVE on the parent tree from the first parent ALTER until commit; the attach validation scan runs inside that lock window. This can hold the tree lock longer than separate plans. You may split the column and partition changes into separate plans.",
                    None,
                ),
            )],
        );
    }
}
