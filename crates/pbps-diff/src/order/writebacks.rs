//! Applications of existing ordering proposals and closed payload edits.
//! Callers supply facts; only order writes the resulting operation sequence.
use pbps_dialect::Dialect;
use pbps_model::{Change, Finding, IdsFile, PlannedChange, RoleName, TableName};
use std::collections::BTreeMap;

pub fn scatter(
    mut changes: super::PlanningPlan,
    placed: Vec<(Vec<usize>, Vec<usize>)>,
) -> super::PlanningPlan {
    let mut taken: Vec<Option<PlannedChange>> = std::mem::take(&mut changes.payload.changes)
        .into_iter()
        .map(Some)
        .collect();
    let mut at: Vec<usize> = (0..taken.len()).collect();
    for (slots, order) in placed {
        for (slot, from) in slots.into_iter().zip(order) {
            at[slot] = from;
        }
    }
    let origins = changes.origins.clone();
    changes.origins = at.iter().map(|&i| origins[i].clone()).collect();
    changes.payload.changes = at
        .into_iter()
        .map(|i| taken[i].take().expect("each change once"))
        .collect();
    changes.invalidate();
    changes
}

pub fn addressed(
    mut settled: super::Settled,
    spellings: &BTreeMap<usize, TableName>,
) -> super::PlanningPlan {
    for (part, from) in settled
        .changes
        .payload
        .changes
        .iter_mut()
        .zip(&settled.sequence)
    {
        if let Some(table) = spellings.get(from) {
            address(&mut part.change, table);
        }
    }
    settled.changes.refresh();
    settled.changes
}

// Only drops on a table are respelled; a logical operation cannot be replaced.
#[allow(clippy::wildcard_enum_match_arm)]
fn address(change: &mut Change, table: &TableName) {
    match change {
        Change::DropIndex { table: at, .. }
        | Change::DropUnique { table: at, .. }
        | Change::DropForeignKey { table: at, .. }
        | Change::DropCheck { table: at, .. }
        | Change::DropComputedColumn { table: at, .. }
        | Change::SetPrimaryKey {
            table: at,
            to: None,
            ..
        } => *at = table.clone(),
        _ => {}
    }
}

pub fn role_members(
    changes: &mut super::PlanningPlan,
    members: &BTreeMap<RoleName, Vec<RoleName>>,
) {
    for planned in &mut changes.payload.changes {
        if let Change::DropRole {
            name, members: of, ..
        } = &mut planned.change
        {
            *of = members.get(name).cloned().unwrap_or_default();
        }
    }
    changes.refresh();
}

pub fn creation_risks(
    changes: &mut super::PlanningPlan,
    ids: &IdsFile,
    dialect: &(impl Dialect + ?Sized),
) {
    let risks = super::generated::risks(changes, ids, dialect);
    for (part, risks) in changes.payload.changes.iter_mut().zip(risks) {
        part.risks = risks;
    }
    changes.refresh();
}

pub fn findings(changes: &mut super::PlanningPlan, findings: &[(usize, Finding)]) {
    for (at, finding) in findings {
        if let Some(part) = changes.payload.changes.get_mut(*at) {
            part.findings.push(finding.clone());
        }
    }
    changes.refresh();
}

/// Explicit logical planning input. Connected plans use it for the two scope
/// transitions absent from schema comparison, at their existing tail position.
#[derive(Debug, Clone)]
pub struct PlanningInput {
    changes: Vec<PlannedChange>,
}
impl PlanningInput {
    pub fn new(changes: Vec<PlannedChange>) -> Self {
        Self { changes }
    }
    pub fn append_to(&self, mut draft: super::PlanningPlan) -> super::PlanningPlan {
        for part in &self.changes {
            draft
                .origins
                .push(pbps_model::operation::Origin::Whole(draft.logical));
            draft.logical += 1;
            draft.payload.changes.push(part.clone());
        }
        draft.refresh();
        draft
    }
}

/// The connected scope input is the same non-executable logical builder.
pub type ScopeInput = PlanningInput;
