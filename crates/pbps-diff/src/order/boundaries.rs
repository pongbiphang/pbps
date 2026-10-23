//! Pending boundary comparisons remain attached to the immutable proposal.
//! A settled permutation alone does not discharge a membership contract.
use super::membership;
use pbps_model::operation::{
    Effects, ModuleEdits, Operation, OperationId, Operations, Origin,
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
}
#[derive(Debug, Clone)]
struct Receipt {
    steps: Vec<Change>,
    events: Vec<(OperationId, Origin)>,
}
impl Obligations {
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
        let parent_uid = side.ids.table_uid(parent);
        let Some(parent_uid) = parent_uid else {
            continue;
        };
        if changes.changes.iter().any(|p| {
            if let Change::RenameTable { uid, .. } = &p.change {
                return uid == parent_uid;
            }
            let uid = match &p.change {
                Change::AddColumn { uid, .. }
                | Change::DropColumn { uid, .. }
                | Change::RenameColumn { uid, .. }
                | Change::AlterColumnType { uid, .. }
                | Change::AlterColumnNullability { uid, .. }
                | Change::AlterColumnDefault { uid, .. }
                | Change::AlterColumnNumbering { uid, .. } => uid,
                _ => return false,
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

#[cfg(test)]
mod tests;
