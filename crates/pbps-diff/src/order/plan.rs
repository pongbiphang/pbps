//! Emission authority belongs to order, never to a mutable draft (DEC-1959.5).

use pbps_dialect::PlanPayload;
use pbps_model::resolver::OrderingProof;
use pbps_model::{ChangeSet, PlanMode, PlannedChange};

pub(super) mod sealed {
    pub trait Sealed {}
}

/// An immutable plan accepted by real engine emitters. Implementations are
/// sealed here so a draft or a caller-defined view cannot become executable.
///
/// ```compile_fail
/// struct Unordered(pbps_model::ChangeSet);
/// impl pbps_dialect::PlanPayload for Unordered {
///     fn changes(&self) -> &pbps_model::ChangeSet { &self.0 }
///     fn mode(&self) -> pbps_model::PlanMode { pbps_model::PlanMode::Transactional }
/// }
/// impl pbps_diff::order::PlanView for Unordered {}
/// ```
pub trait PlanView: PlanPayload + sealed::Sealed {}

/// An immutable intermediate result. Ordering another stage may replace it,
/// but neither it nor a cloned payload can be passed to an engine.
///
/// ```compile_fail
/// fn execute(plan: &pbps_diff::order::PlanningPlan) {
///     let _: &dyn pbps_diff::order::PlanView = plan;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct PlanningPlan {
    pub(super) payload: ChangeSet,
    pub(super) origins: Vec<pbps_model::operation::Origin>,
    pub(super) logical: usize,
    pub(super) proof: OrderingProof,
}

impl PlanningPlan {
    pub(super) fn new(
        payload: ChangeSet,
        origins: Vec<pbps_model::operation::Origin>,
        logical: usize,
        proof: OrderingProof,
    ) -> Self {
        assert_eq!(payload.changes.len(), origins.len());
        Self {
            payload,
            origins,
            logical,
            proof,
        }
    }
    pub fn changes(&self) -> &ChangeSet {
        &self.payload
    }
    pub fn origins(&self) -> &[pbps_model::operation::Origin] {
        &self.origins
    }
    pub fn logical(&self) -> usize {
        self.logical
    }
    pub fn proof(&self) -> &OrderingProof {
        &self.proof
    }

    pub(super) fn refresh(&mut self) {
        self.proof = OrderingProof::new(&self.payload, self.proof.edges().clone())
            .expect("a payload-only edit preserves the settled positions");
    }
    pub(super) fn invalidate(&mut self) {
        // This is an unsealed intermediate, not recertification. Structural
        // changes invalidate the preceding stage's positional proof; the next
        // existing settlement records only its own edges, never their union.
        self.proof = OrderingProof::new(&self.payload, Default::default()).unwrap();
    }
}
impl std::ops::Deref for PlanningPlan {
    type Target = ChangeSet;
    fn deref(&self) -> &ChangeSet {
        &self.payload
    }
}
impl PartialEq for PlanningPlan {
    fn eq(&self, other: &Self) -> bool {
        self.payload == other.payload
            && self.origins == other.origins
            && self.logical == other.logical
    }
}
impl PartialEq<ChangeSet> for PlanningPlan {
    fn eq(&self, other: &ChangeSet) -> bool {
        self.payload == *other
    }
}
impl PartialEq<PlanningPlan> for ChangeSet {
    fn eq(&self, other: &PlanningPlan) -> bool {
        *self == other.payload
    }
}

/// Starts a logical planning input, granting no emission authority. External
/// draft copies can enter planning again, but must still take the complete
/// canonical route before execution; an empty settlement cannot certify them.
pub fn propose(payload: ChangeSet) -> PlanningPlan {
    let logical = payload.changes.len();
    let origins = (0..logical)
        .map(pbps_model::operation::Origin::Whole)
        .collect();
    let proof = OrderingProof::new(&payload, Default::default()).unwrap();
    PlanningPlan::new(payload, origins, logical, proof)
}

/// Moves an intermediate out of an adapter, preserving the old empty-on-error
/// behavior of mem::take without exposing a Default execution certificate.
pub fn take(plan: &mut PlanningPlan) -> PlanningPlan {
    std::mem::replace(plan, propose(ChangeSet::default()))
}

/// A raw snapshot cannot enter settlement without starting new, non-executable
/// logical input, and its immutable intermediate cannot be edited in place.
///
/// ```compile_fail
/// fn raw(draft: pbps_model::ChangeSet) {
///     let _ = pbps_diff::order::Schedule::new(draft.changes.len()).settle(draft);
/// }
/// ```
/// ```compile_fail
/// fn mutate(plan: &mut pbps_diff::order::PlanningPlan) {
///     plan.changes.reverse();
/// }
/// ```
/// A plan whose complete canonical ordering route has finished.
/// Intermediate settlements never grant this execution authority.
///
/// The former public raw settlement could recertify an external reorder,
/// erasing the original constraints with an empty schedule (DEC-1959.5).
/// This external-consumer counterexample must remain a compiler error:
///
/// ```compile_fail
/// use pbps_diff::order::{OrderedPlan, PlanView, Schedule, propose};
/// fn recertify(ordered: &OrderedPlan) {
///     let mut external_copy = ordered.changes().clone();
///     external_copy.changes.reverse();
///     let recertified = Schedule::new(external_copy.changes.len())
///         .settle(propose(external_copy)).unwrap().into_planning();
///     let _: &dyn PlanView = &recertified;
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::OrderedPlan;
/// let plan = OrderedPlan::new(todo!());
/// ```
///
/// ```compile_fail
/// fn edit(plan: &mut pbps_diff::order::OrderedPlan) {
///     plan.changes().changes.reverse();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct OrderedPlan {
    changes: ChangeSet,
    origins: Vec<pbps_model::operation::Origin>,
    proof: OrderingProof,
    mode: PlanMode,
}

impl OrderedPlan {
    pub(super) fn new(plan: PlanningPlan, mode: PlanMode) -> Self {
        Self {
            changes: plan.payload,
            origins: plan.origins,
            proof: plan.proof,
            mode,
        }
    }

    pub fn changes(&self) -> &ChangeSet {
        &self.changes
    }

    pub fn mode(&self) -> PlanMode {
        self.mode
    }

    pub fn proof(&self) -> &OrderingProof {
        &self.proof
    }
    pub fn origins(&self) -> &[pbps_model::operation::Origin] {
        &self.origins
    }
}

impl PlanPayload for OrderedPlan {
    fn changes(&self) -> &ChangeSet {
        &self.changes
    }
    fn mode(&self) -> PlanMode {
        self.mode
    }
}
impl sealed::Sealed for OrderedPlan {}
impl PlanView for OrderedPlan {}

/// The real single-operation route used by emitter fixtures. A singleton has
/// no inter-operation permutation; the same schedule still checks its shape.
/// This cannot recertify a caller-reordered collection of operations.
pub fn singleton(change: PlannedChange, mode: PlanMode) -> OrderedPlan {
    let settled = super::Schedule::new(1)
        .settle(propose(ChangeSet {
            changes: vec![change],
        }))
        .expect("a single operation has no ordering cycle");
    OrderedPlan::new(settled.changes, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Schedule, Source};
    use pbps_model::resolver::OrderReason;
    use pbps_model::{Change, PlannedChange};

    #[test]
    fn an_intermediate_keeps_the_exact_settled_sequence_and_proof() {
        let changes = ChangeSet {
            changes: ["before", "after"]
                .into_iter()
                .map(|name| {
                    PlannedChange::new(Change::CreateRole {
                        uid: "r_aaaaaa".parse().unwrap(),
                        name: name.into(),
                    })
                })
                .collect(),
        };
        let mut schedule = Schedule::new(2);
        schedule.edge(1, 0, Source::Effect, OrderReason::Structural);
        let ordered = schedule.settle(propose(changes)).unwrap().into_planning();
        ordered.proof().validate(ordered.changes()).unwrap();
        assert_eq!(ordered.changes().changes[0].change.subject(), "role after");
        let mut draft = ordered.changes().clone();
        draft.changes.reverse();
        assert_ne!(&draft, ordered.changes());
        assert_eq!(ordered.proof().edges().len(), 1);
    }
    #[test]
    fn successive_settlements_preserve_sources_without_unioning_constraints() {
        use pbps_model::operation::Origin;
        let input = propose(ChangeSet {
            changes: ["a", "b", "c"]
                .into_iter()
                .map(|name| {
                    PlannedChange::new(Change::CreateRole {
                        uid: pbps_model::Uid::derived(pbps_model::UidKind::Role, name, 0),
                        name: name.into(),
                    })
                })
                .collect(),
        });
        let mut first = Schedule::new(3);
        first.edge(2, 0, Source::Effect, OrderReason::Structural);
        let first = first.settle(input).unwrap().into_planning();
        assert_eq!(
            first.origins(),
            &[Origin::Whole(1), Origin::Whole(2), Origin::Whole(0)]
        );
        let mut second = Schedule::new(3);
        second.edge(2, 0, Source::Lexical, OrderReason::Identity);
        let second = second.settle(first).unwrap().into_planning();
        assert_eq!(
            second.origins(),
            &[Origin::Whole(2), Origin::Whole(0), Origin::Whole(1)]
        );
        assert_eq!(second.logical(), 3);
        assert_eq!(second.proof().edges().len(), 1);
        assert_eq!(
            second.proof().edges().first().unwrap().reason,
            OrderReason::Identity
        );
        second.proof().validate(second.changes()).unwrap();
    }

    #[test]
    fn public_companions_rejoin_their_original_sources_after_settlement() {
        use pbps_model::operation::Origin;
        let id: pbps_model::ModuleId = "app.f()".parse().unwrap();
        let mut input = propose(ChangeSet {
            changes: vec![
                PlannedChange::new(Change::CreateModule {
                    id,
                    module: Box::new(pbps_model::Module {
                        kind: pbps_model::ModuleKind::Function,
                        definition: "() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$".into(),
                        description: None,
                    }),
                }),
                PlannedChange::new(Change::PublicExecution {
                    routine: "app.f()".parse().unwrap(),
                    access: pbps_model::PublicAccess::Revoked,
                    origin: pbps_model::RoutineOrigin::Created,
                }),
                PlannedChange::new(Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "reader".into(),
                }),
            ],
        });
        let decisions = crate::order::take_public_execution(&mut input);
        let mut schedule = Schedule::new(2);
        schedule.edge(1, 0, Source::Effect, OrderReason::Structural);
        let mut input = schedule.settle(input).unwrap().into_planning();
        crate::order::settle_public_execution(&mut input, decisions);
        assert_eq!(
            input.origins(),
            &[Origin::Whole(2), Origin::Whole(0), Origin::Whole(1)]
        );
        assert_eq!(input.logical(), 3);
        assert!(matches!(
            &input.changes[2].change,
            Change::PublicExecution { .. }
        ));
        input.proof().validate(input.changes()).unwrap();
    }
}
