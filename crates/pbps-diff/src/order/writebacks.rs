//! Applications of existing ordering proposals and closed payload edits.
//! Callers supply facts; only order writes the resulting operation sequence.
use pbps_dialect::Dialect;
use pbps_model::ColumnChange;
use pbps_model::ConstraintChange;
use pbps_model::IndexChange;
use pbps_model::RoleChange;
use pbps_model::RowChange;
use pbps_model::{Change, Finding, IdsFile, PlannedChange, RoleName, TableName};
use std::collections::BTreeMap;

pub fn scatter<S>(
    mut changes: super::PlanningPlan<S>,
    placed: Vec<(Vec<usize>, Vec<usize>)>,
) -> super::PlanningPlan<S> {
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
    if let Some(identities) = &changes.identities {
        changes.identities = Some(super::reordered_operations(identities, &at));
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

pub fn addressed<S>(
    mut settled: super::Settled<S>,
    spellings: &BTreeMap<usize, TableName>,
) -> super::PlanningPlan<S> {
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
        Change::Index(IndexChange::DropIndex { table: at, .. })
        | Change::Constraint(ConstraintChange::DropUnique { table: at, .. })
        | Change::Constraint(ConstraintChange::DropForeignKey { table: at, .. })
        | Change::Constraint(ConstraintChange::DropCheck { table: at, .. })
        | Change::Column(ColumnChange::DropComputedColumn { table: at, .. })
        | Change::Constraint(ConstraintChange::SetPrimaryKey {
            table: at,
            to: None,
            ..
        }) => *at = table.clone(),
        _ => {}
    }
}

pub fn role_members(
    changes: &mut super::PlanningPlan,
    members: &BTreeMap<RoleName, Vec<RoleName>>,
) {
    for planned in &mut changes.payload.changes {
        if let Change::Role(RoleChange::DropRole {
            name, members: of, ..
        }) = &mut planned.change
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

pub fn findings<S>(changes: &mut super::PlanningPlan<S>, findings: &[(usize, Finding)]) {
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
    pub fn append_to<S>(&self, draft: super::PlanningPlan<S>) -> super::PlanningPlan {
        let plain = draft.boundaries.is_empty()
            && draft.identities.is_none()
            && draft.annotations.is_none()
            && draft.captured.is_none();
        let mut draft: super::PlanningPlan = if plain {
            // Preserve the existing re-proposal route for untracked inputs.
            super::PlanningPlan::new(draft.payload, draft.origins, draft.logical, draft.proof)
        } else {
            super::plan::state(draft)
        };
        draft.boundaries.append_names(&self.changes);
        let prior = draft.payload.changes.len();
        for part in &self.changes {
            draft
                .origins
                .push(pbps_model::operation::Origin::Whole(draft.logical));
            draft.logical += 1;
            draft.payload.changes.push(part.clone());
        }
        if let Some(identities) = &draft.identities {
            draft.allocator.reserve(identities);
            let added = draft.allocator.allocate(
                draft.origins[prior..]
                    .iter()
                    .cloned()
                    .zip(draft.payload.changes[prior..].iter().cloned()),
                if draft.boundaries.is_empty() {
                    pbps_model::operation::ModuleEdits::InPlace
                } else {
                    draft.boundaries.module_edits()
                },
            );
            draft.identities = Some(pbps_model::operation::Operations::from_operations(
                identities.iter().chain(added.iter()).cloned(),
            ));
        }
        draft.refresh();
        draft
    }
}

/// Only the existing zero-SQL scope transitions may retain producer provenance.
/// Arbitrary logical additions restart an unqualified draft through PlanningInput.
/// Scope additions still need the shared stages; they cannot directly finalize.
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish};
/// use pbps_dialect::MinimalDialect;
/// fn skip_stages(plan: CanonicalPlan<MinimalDialect>, inputs: finish::Inputs<'_>) {
///     let scope = pbps_diff::order::writebacks::ScopeInput::new(vec![]).unwrap();
///     let pending = scope.append_to(plan, &pbps_dialect::MinimalDialect);
///     let _ = finish::connected(pending, inputs, &MinimalDialect,
///         finish::Catalog { slots: None, membership: None });
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ScopeInput {
    changes: Vec<PlannedChange>,
}
impl ScopeInput {
    pub fn new(changes: Vec<PlannedChange>) -> Option<Self> {
        changes
            .iter()
            .all(|p| {
                matches!(
                    p.change,
                    Change::Row(RowChange::SetDataMode { .. })
                        | Change::Row(RowChange::SetEnsureKeys { .. })
                )
            })
            .then_some(Self { changes })
    }
    pub fn append_to<S: super::RouteState<D>, D: Dialect + ?Sized>(
        &self,
        plan: super::PlanningPlan<S>,
        _dialect: &D,
    ) -> super::PlanningPlan<S::Input> {
        let input = PlanningInput::new(self.changes.clone()).append_to(plan);
        super::plan::state(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::RoleChange;
    use pbps_model::RowChange;
    use pbps_model::data::DataMode;

    fn takeover() -> PlannedChange {
        PlannedChange::new(Change::Row(RowChange::SetDataMode {
            table: TableName::new("app", "holdings"),
            from: None,
            to: Some(DataMode::Exact),
        }))
    }

    fn keys() -> PlannedChange {
        PlannedChange::new(Change::Row(RowChange::SetEnsureKeys {
            table: TableName::new("app", "country"),
            from: ["TW"].into_iter().map(Into::into).collect(),
            to: ["TW", "JP"].into_iter().map(Into::into).collect(),
        }))
    }

    fn role() -> PlannedChange {
        PlannedChange::new(Change::Role(RoleChange::CreateRole {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Role),
            name: "reader".into(),
        }))
    }

    #[test]
    fn scope_input_admits_exactly_the_two_zero_sql_scope_kinds() {
        assert!(ScopeInput::new(vec![]).is_some());
        assert!(ScopeInput::new(vec![takeover(), keys()]).is_some());
        // A change with SQL would reach the resolver's sealed plan without
        // the stages that order and check it.
        assert!(ScopeInput::new(vec![role()]).is_none());
        // One foreign change refuses the whole input, wherever it stands.
        assert!(ScopeInput::new(vec![takeover(), role(), keys()]).is_none());
        assert!(ScopeInput::new(vec![role(), takeover()]).is_none());
    }

    #[test]
    fn scope_changes_follow_the_resolvers_changes_in_the_order_given() {
        let resolved = crate::order::plan::canonical(
            crate::order::propose(pbps_model::ChangeSet {
                changes: vec![role()],
            }),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let scope = ScopeInput::new(vec![takeover(), keys()]).unwrap();
        let appended = scope.append_to(resolved, &pbps_dialect::MinimalDialect);
        let kinds: Vec<_> = appended
            .payload
            .changes
            .iter()
            .map(|planned| {
                (
                    matches!(planned.change, Change::Role(RoleChange::CreateRole { .. })),
                    matches!(planned.change, Change::Row(RowChange::SetDataMode { .. })),
                    matches!(planned.change, Change::Row(RowChange::SetEnsureKeys { .. })),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (true, false, false),
                (false, true, false),
                (false, false, true)
            ]
        );
    }
}
