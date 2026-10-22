//! Emission authority belongs to order, never to a mutable draft (DEC-1959.5).

use pbps_dialect::PlanPayload;
use pbps_model::resolver::OrderingProof;
use pbps_model::{ChangeSet, PlanMode};

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
/// impl pbps_diff::order::PlanView<pbps_dialect::MinimalDialect> for Unordered {}
/// ```
pub trait PlanView<D: ?Sized>: PlanPayload + sealed::Sealed {}

/// Draft inputs have no execution authority, including after arbitrary settlement.
#[derive(Debug, Clone)]
pub struct Draft;
/// The differ's immutable result has completed its existing sort/rename stages.
#[derive(Debug, Clone)]
pub struct Differ;
/// Raw copies never acquire the differ's completed-stage provenance.
/// ```compile_fail
/// let raw = pbps_diff::order::propose(pbps_model::ChangeSet::default());
/// let _: pbps_diff::order::DiffPlan = raw;
/// ```
pub type DiffPlan = PlanningPlan<Differ>;

mod route_state {
    pub trait Sealed<D: ?Sized> {}
}
/// Which immutable inputs may complete the shared route for `D`. A raw
/// proposal stays non-executable; a canonical continuation retains its engine.
/// The sealed state/dialect pair prevents external implementations from
/// assigning a different engine to an existing producer's provenance.
pub trait RouteState<D: pbps_dialect::Dialect + ?Sized>:
    route_state::Sealed<D> + Clone + std::fmt::Debug
{
    type Output: Clone + std::fmt::Debug;
    type Input: Clone + std::fmt::Debug;
}
impl<D: ?Sized> route_state::Sealed<D> for Draft {}
impl<D: pbps_dialect::Dialect + ?Sized> RouteState<D> for Draft {
    type Output = Draft;
    type Input = Draft;
}
impl<D: ?Sized> route_state::Sealed<D> for Differ {}
impl<D: pbps_dialect::Dialect + ?Sized> RouteState<D> for Differ {
    type Output = Canonical<D>;
    type Input = Continuing<D>;
}
impl<D: ?Sized> route_state::Sealed<D> for Canonical<D> {}
impl<D: pbps_dialect::Dialect + ?Sized> RouteState<D> for Canonical<D> {
    type Output = Canonical<D>;
    type Input = Continuing<D>;
}

/// A scope-annotated producer must finish the shared stages again, retaining
/// its engine while it has no direct execution authority (DEC-1959.5).
pub struct Continuing<D: ?Sized>(std::marker::PhantomData<fn(&D) -> &D>);
impl<D: ?Sized> Clone for Continuing<D> {
    fn clone(&self) -> Self {
        Self(std::marker::PhantomData)
    }
}
impl<D: ?Sized> std::fmt::Debug for Continuing<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Continuing")
    }
}
impl<D: ?Sized> route_state::Sealed<D> for Continuing<D> {}
impl<D: pbps_dialect::Dialect + ?Sized> RouteState<D> for Continuing<D> {
    type Output = Canonical<D>;
    type Input = Continuing<D>;
}

pub(crate) fn differ(plan: PlanningPlan) -> DiffPlan {
    state(plan)
}
pub(super) fn state<S, T>(plan: PlanningPlan<S>) -> PlanningPlan<T> {
    let mut done = PlanningPlan::new(plan.payload, plan.origins, plan.logical, plan.proof);
    done.identities = plan.identities;
    done
}

/// This marker is produced only by the complete declaration/catalog route.
pub struct Canonical<D: ?Sized>(std::marker::PhantomData<fn(&D) -> &D>);
impl<D: ?Sized> Clone for Canonical<D> {
    fn clone(&self) -> Self {
        Self(std::marker::PhantomData)
    }
}
impl<D: ?Sized> std::fmt::Debug for Canonical<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Canonical")
    }
}
/// A completed route may continue through the existing order-owned connected
/// stages and annotations. Its immutable payload cannot be replaced by a draft.
/// Even running the shared stages on copied raw input cannot impersonate
/// the differ's preceding sort and rename stages.
/// ```compile_fail
/// use pbps_diff::{Side, order::{deployment::{Sources, schedule::{Declarations, Facts, schedule}}, propose}};
/// fn copied_input(dialect: &pbps_dialect::MinimalDialect) {
/// let schema = pbps_model::Schema::default();
/// let ids = pbps_model::IdsFile::default();
/// let result = schedule(propose(pbps_model::ChangeSet::default()), &Declarations {
///     schema: &schema, sources: Sources { before: Side { schema: &schema, ids: &ids }, after: &ids },
///     module_deps: &Default::default(),
/// }, &Facts::default(), dialect).unwrap();
/// let _ = result.changes.ordered(pbps_model::PlanMode::Transactional, dialect);
/// }
/// ```
pub type CanonicalPlan<D> = PlanningPlan<Canonical<D>>;

pub(super) fn canonical<D: pbps_dialect::Dialect + ?Sized>(
    plan: PlanningPlan,
    _dialect: &D,
) -> CanonicalPlan<D> {
    let identities = plan.identities.or_else(|| {
        Some(pbps_model::operation::Operations::expanded(
            plan.logical,
            plan.origins
                .iter()
                .cloned()
                .zip(plan.payload.changes.iter().cloned()),
            pbps_model::operation::ModuleEdits::InPlace,
        ))
    });
    let mut done = PlanningPlan::new(plan.payload, plan.origins, plan.logical, plan.proof);
    done.identities = identities;
    done
}
impl<D: pbps_dialect::Dialect + ?Sized> CanonicalPlan<D> {
    pub fn ordered(self, mode: PlanMode, dialect: &D) -> OrderedPlan<D> {
        OrderedPlan::from_canonical(self, mode, dialect)
    }
}

/// An immutable intermediate result. Ordering another stage may replace it,
/// but neither it nor a cloned payload can be passed to an engine.
///
/// ```compile_fail
/// fn execute(plan: &pbps_diff::order::PlanningPlan) {
///     let _: &dyn pbps_diff::order::PlanView<pbps_dialect::MinimalDialect> = plan;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct PlanningPlan<S = Draft> {
    state: std::marker::PhantomData<S>,
    pub(super) payload: ChangeSet,
    pub(super) origins: Vec<pbps_model::operation::Origin>,
    pub(super) logical: usize,
    pub(super) proof: OrderingProof,
    pub(super) identities: Option<pbps_model::operation::Operations>,
}

impl<S> PlanningPlan<S> {
    pub(super) fn new(
        payload: ChangeSet,
        origins: Vec<pbps_model::operation::Origin>,
        logical: usize,
        proof: OrderingProof,
    ) -> Self {
        assert_eq!(payload.changes.len(), origins.len());
        Self {
            state: std::marker::PhantomData,
            payload,
            origins,
            logical,
            proof,
            identities: None,
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
impl<S> std::ops::Deref for PlanningPlan<S> {
    type Target = ChangeSet;
    fn deref(&self) -> &ChangeSet {
        &self.payload
    }
}
impl<S, T> PartialEq<PlanningPlan<T>> for PlanningPlan<S> {
    fn eq(&self, other: &PlanningPlan<T>) -> bool {
        self.payload == other.payload
            && self.origins == other.origins
            && self.logical == other.logical
    }
}
impl<S> PartialEq<ChangeSet> for PlanningPlan<S> {
    fn eq(&self, other: &ChangeSet) -> bool {
        self.payload == *other
    }
}
impl<S> PartialEq<PlanningPlan<S>> for ChangeSet {
    fn eq(&self, other: &PlanningPlan<S>) -> bool {
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
pub fn take<S>(plan: &mut PlanningPlan<S>) -> PlanningPlan<S> {
    let empty = propose(ChangeSet::default());
    std::mem::replace(
        plan,
        PlanningPlan::new(empty.payload, empty.origins, empty.logical, empty.proof),
    )
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
/// fn recertify(ordered: &OrderedPlan<pbps_dialect::MinimalDialect>) {
///     let mut external_copy = ordered.changes().clone();
///     external_copy.changes.reverse();
///     let recertified = Schedule::new(external_copy.changes.len())
///         .settle(propose(external_copy)).unwrap().into_planning();
///     let _: &dyn PlanView<pbps_dialect::MinimalDialect> = &recertified;
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::OrderedPlan;
/// let plan = OrderedPlan::new(todo!(), pbps_model::PlanMode::Transactional,
///                             &pbps_dialect::MinimalDialect);
/// ```
///
/// ```compile_fail
/// fn edit(plan: &mut pbps_diff::order::OrderedPlan<pbps_dialect::MinimalDialect>) {
///     plan.changes().changes.reverse();
/// }
/// ```
#[derive(Debug)]
pub struct OrderedPlan<D: ?Sized> {
    identities: pbps_model::operation::Operations,
    changes: ChangeSet,
    origins: Vec<pbps_model::operation::Origin>,
    proof: OrderingProof,
    mode: PlanMode,
    dialect: &'static str,
    // Invariance prevents erasing or substituting the checker type.
    brand: std::marker::PhantomData<fn(&D) -> &D>,
}

impl<D: ?Sized> Clone for OrderedPlan<D> {
    fn clone(&self) -> Self {
        Self {
            identities: self.identities.clone(),
            changes: self.changes.clone(),
            origins: self.origins.clone(),
            proof: self.proof.clone(),
            mode: self.mode,
            dialect: self.dialect,
            brand: std::marker::PhantomData,
        }
    }
}

impl<D: ?Sized> OrderedPlan<D> {
    // A singleton fixture has no inter-operation permutation. Its private
    // constructor stays under the same fixture gate as the public adapter.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub(super) fn new(plan: PlanningPlan, mode: PlanMode, dialect: &D) -> Self
    where
        D: pbps_dialect::Dialect,
    {
        Self {
            identities: plan.identities.unwrap_or_else(|| {
                pbps_model::operation::Operations::expanded(
                    plan.logical,
                    plan.origins
                        .iter()
                        .cloned()
                        .zip(plan.payload.changes.iter().cloned()),
                    pbps_model::operation::ModuleEdits::InPlace,
                )
            }),
            changes: plan.payload,
            origins: plan.origins,
            proof: plan.proof,
            mode,
            dialect: dialect.name(),
            brand: std::marker::PhantomData,
        }
    }

    fn from_canonical(plan: CanonicalPlan<D>, mode: PlanMode, dialect: &D) -> Self
    where
        D: pbps_dialect::Dialect,
    {
        Self {
            identities: plan.identities.unwrap_or_else(|| {
                pbps_model::operation::Operations::expanded(
                    plan.logical,
                    plan.origins
                        .iter()
                        .cloned()
                        .zip(plan.payload.changes.iter().cloned()),
                    pbps_model::operation::ModuleEdits::InPlace,
                )
            }),
            changes: plan.payload,
            origins: plan.origins,
            proof: plan.proof,
            mode,
            dialect: dialect.name(),
            brand: std::marker::PhantomData,
        }
    }

    pub(super) fn dialect(&self) -> &'static str {
        self.dialect
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

impl<D: ?Sized> PlanPayload for OrderedPlan<D> {
    fn operations(&self) -> pbps_model::operation::Operations {
        self.identities.with_steps(&self.changes.changes)
    }
    fn changes(&self) -> &ChangeSet {
        &self.changes
    }
    fn mode(&self) -> PlanMode {
        self.mode
    }
}
impl<D: ?Sized> sealed::Sealed for OrderedPlan<D> {}
impl<D: ?Sized> PlanView<D> for OrderedPlan<D> {}

/// A single-operation fixture, available only in test builds or through the
/// non-default `test-fixtures` feature used by engine dev-dependencies.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn singleton<D: pbps_dialect::Dialect + ?Sized>(
    change: pbps_model::PlannedChange,
    mode: PlanMode,
    dialect: &D,
) -> OrderedPlan<D> {
    let settled = super::Schedule::new(1)
        .settle(propose(ChangeSet {
            changes: vec![change],
        }))
        .expect("a single operation has no ordering cycle");
    OrderedPlan::new(settled.changes, mode, dialect)
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
