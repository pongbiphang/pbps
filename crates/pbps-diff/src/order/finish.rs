//! The final stage for fresh deployment plans.

use crate::Side;
use pbps_dialect::{Dialect, slots::CatalogFacts};
use pbps_model::operation::{Captured, OwnerError};
use pbps_model::resolver::OrderingProof;
use pbps_model::{Change, ChangeSet, Finding, ModuleDeps, PlanMode};

pub use super::plan::{Standalone, StandaloneScheduled};
use super::{CanonicalPlan, DiffPlan, OrderedPlan, PlanningPlan};

/// Select the standalone route before running the shared scheduler.
pub fn standalone(plan: DiffPlan) -> PlanningPlan<Standalone> {
    super::plan::state(plan)
}

/// Declaration and binding facts for the final sequence.
pub struct Inputs<'a> {
    pub before: Side<'a>,
    pub after: Side<'a>,
    pub modules: &'a ModuleDeps,
    pub captured: &'a Captured,
}

/// Facts returned by the connected catalog pass.
pub struct Catalog<'a> {
    pub slots: Option<&'a CatalogFacts>,
    /// The stage trusts this flag from the rebuild's own verifier.
    pub verified: bool,
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct FinishError(#[from] OwnerError);

/// An engine-branded plan that has reached its route's final stage.
///
/// Until settlement lands, this value proves only that the final stage ran.
///
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish::FinishedPlan};
/// use pbps_dialect::MinimalDialect;
/// fn forge(plan: CanonicalPlan<MinimalDialect>) -> FinishedPlan<MinimalDialect> {
///     FinishedPlan::new(plan)
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish::FinishedPlan};
/// use pbps_dialect::MinimalDialect;
/// fn forge(plan: CanonicalPlan<MinimalDialect>) -> FinishedPlan<MinimalDialect> {
///     FinishedPlan { plan }
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::{Schedule, finish::FinishedPlan};
/// use pbps_dialect::MinimalDialect;
/// fn reorder(plan: FinishedPlan<MinimalDialect>) -> FinishedPlan<MinimalDialect> {
///     Schedule::new(plan.changes().changes.len()).settle(plan).unwrap().into_planning()
/// }
/// ```
///
/// ```
/// use pbps_diff::order::{CanonicalPlan, OrderedPlan, finish};
/// use pbps_dialect::MinimalDialect;
/// fn for_target(plan: CanonicalPlan<MinimalDialect>, inputs: finish::Inputs<'_>,
///               catalog: finish::Catalog<'_>) -> OrderedPlan<MinimalDialect> {
///     finish::connected(plan, inputs, &MinimalDialect, catalog).unwrap()
///         .ordered(pbps_model::PlanMode::Transactional, &MinimalDialect)
/// }
/// ```
pub struct FinishedPlan<D: ?Sized> {
    plan: CanonicalPlan<D>,
}

impl<D: ?Sized> FinishedPlan<D> {
    fn new(plan: CanonicalPlan<D>) -> Self {
        Self { plan }
    }

    pub fn changes(&self) -> &ChangeSet {
        self.plan.changes()
    }

    pub fn proof(&self) -> &OrderingProof {
        self.plan.proof()
    }

    pub(super) fn into_canonical(self) -> CanonicalPlan<D> {
        self.plan
    }
}

impl<D: ?Sized> Clone for FinishedPlan<D> {
    fn clone(&self) -> Self {
        Self::new(self.plan.clone())
    }
}

impl<D: ?Sized> std::ops::Deref for FinishedPlan<D> {
    type Target = ChangeSet;

    fn deref(&self) -> &ChangeSet {
        self.changes()
    }
}

impl<D: Dialect + ?Sized> FinishedPlan<D> {
    pub fn ordered(self, mode: PlanMode, dialect: &D) -> OrderedPlan<D> {
        OrderedPlan::from_finished(self, mode, dialect)
    }
}

/// Finish a declaration build after the shared ordering passes.
///
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish};
/// use pbps_dialect::MinimalDialect;
/// fn misroute(plan: CanonicalPlan<MinimalDialect>, inputs: finish::Inputs<'_>) {
///     let _ = finish::offline(plan, inputs, &MinimalDialect);
/// }
/// ```
pub fn offline<D: Dialect + ?Sized>(
    plan: PlanningPlan<StandaloneScheduled<D>>,
    _inputs: Inputs<'_>,
    _dialect: &D,
) -> Result<FinishedPlan<D>, FinishError> {
    Ok(FinishedPlan::new(super::plan::state(plan)))
}

/// Finish a connected plan after the last catalog ordering pass.
pub fn connected<D: Dialect + ?Sized>(
    plan: CanonicalPlan<D>,
    inputs: Inputs<'_>,
    dialect: &D,
    catalog: Catalog<'_>,
) -> Result<FinishedPlan<D>, FinishError> {
    #[cfg(debug_assertions)]
    if !catalog.verified {
        super::shadow::observe_scheduled(
            inputs.before,
            inputs.after,
            dialect,
            inputs.modules,
            plan.changes(),
            super::shadow::Stage::Final,
            Some(catalog.slots.unwrap_or(&CatalogFacts::default())),
            inputs.captured,
        )?;
    }
    #[cfg(not(debug_assertions))]
    let _ = (inputs, dialect, catalog);
    Ok(FinishedPlan::new(plan))
}

/// Append findings to the reviewed payload while keeping every step at its position.
pub fn record_findings<D: ?Sized>(
    mut plan: FinishedPlan<D>,
    findings: &[(usize, Finding)],
) -> FinishedPlan<D> {
    super::writebacks::findings(&mut plan.plan, findings);
    plan
}

/// Remap resolver evidence and operation IDs while keeping every step at its position.
pub fn remap_resolver<D: ?Sized>(
    before: &CanonicalPlan<D>,
    plan: FinishedPlan<D>,
    describe: impl Fn(&Change) -> String,
) -> Result<FinishedPlan<D>, String> {
    super::resolver_remap::finish(before, plan.plan, describe).map(FinishedPlan::new)
}

/// Carry a fixture's intermediate order to the conversion without another observation.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn finish_fixture<D: Dialect + ?Sized>(plan: CanonicalPlan<D>) -> FinishedPlan<D> {
    FinishedPlan::new(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Schedule, Source, propose};
    use pbps_dialect::MinimalDialect;
    use pbps_model::finding::Severity;
    use pbps_model::resolver::OrderReason;
    use pbps_model::{PlannedChange, RoleChange};

    fn changes() -> ChangeSet {
        ChangeSet {
            changes: [("r_aaaaaa", "reader"), ("r_aaaaab", "writer")]
                .into_iter()
                .map(|(uid, name)| {
                    PlannedChange::new(Change::Role(RoleChange::CreateRole {
                        uid: uid.parse().unwrap(),
                        name: name.into(),
                    }))
                })
                .collect(),
        }
    }

    #[test]
    fn final_stage_preserves_the_complete_change_sequence() {
        let intermediate = super::super::plan::state(propose(changes()));
        let before = intermediate.changes().clone();
        let proof = intermediate.proof().clone();
        let origins = intermediate.origins().to_vec();
        let side = Side {
            schema: &Default::default(),
            ids: &Default::default(),
        };
        let finished = offline(
            intermediate,
            Inputs {
                before: side,
                after: side,
                modules: &Default::default(),
                captured: &Default::default(),
            },
            &MinimalDialect,
        )
        .unwrap();
        assert_eq!(finished.changes(), &before);
        let ordered = finished.ordered(PlanMode::Transactional, &MinimalDialect);
        assert_eq!(ordered.changes(), &before);
        assert_eq!(ordered.proof(), &proof);
        assert_eq!(ordered.origins(), origins);
        assert_eq!(
            serde_json::to_vec(ordered.changes()).unwrap(),
            serde_json::to_vec(&before).unwrap()
        );

        let mut reversed = before.clone();
        reversed.changes.reverse();
        assert_ne!(reversed, before);
        assert_eq!(
            ordered.proof().validate(&reversed),
            Err(pbps_model::resolver::OrderError::Changed)
        );
    }

    #[test]
    fn recording_findings_keeps_every_change_at_its_position() {
        let before = changes();
        let plan = super::super::plan::canonical(propose(before.clone()), &MinimalDialect).unwrap();
        let note = Finding::new("test.note", Severity::Warning, "example note", None);
        let finished = record_findings(
            finish_fixture(plan),
            &[(1, note.clone()), (before.changes.len(), note.clone())],
        );
        let mut expected = before.clone();
        expected.changes[1].findings.push(note);
        assert_eq!(finished.changes(), &expected);
        assert!(finished.proof().validate(&expected).is_ok());
        assert_eq!(
            finished.proof().validate(&before),
            Err(pbps_model::resolver::OrderError::Changed)
        );
    }

    #[test]
    fn remapping_resolver_evidence_keeps_every_change_at_its_position() {
        let before = changes();
        let mut schedule = Schedule::new(before.changes.len());
        schedule.edge(0, 1, Source::Effect, OrderReason::Structural);
        let resolver = super::super::plan::canonical(
            schedule
                .settle(propose(before.clone()))
                .unwrap()
                .into_planning(),
            &MinimalDialect,
        )
        .unwrap();
        let current =
            super::super::plan::canonical(propose(before.clone()), &MinimalDialect).unwrap();
        let remapped = remap_resolver(&resolver, finish_fixture(current), Change::subject).unwrap();
        assert_eq!(remapped.changes(), &before);
        assert_eq!(remapped.proof(), resolver.proof());

        let mut reversed = before;
        reversed.changes.reverse();
        let wrong = super::super::plan::canonical(propose(reversed), &MinimalDialect).unwrap();
        let error = remap_resolver(&resolver, finish_fixture(wrong), Change::subject)
            .err()
            .expect("an existing resolver edge is backward");
        assert!(error.contains("which the resolver orders after it"));
    }
}
