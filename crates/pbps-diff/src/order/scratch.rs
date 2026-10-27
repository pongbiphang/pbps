//! Scratch's canonical source-mapped blueprint route. The new tables are split
//! into the parts the engine creates one at a time, and one `Schedule`
//! settles the constraints `prepare` states over them (DEC-2389.1).
use super::{Constraint, Schedule};
use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations};
use pbps_model::{Change, ChangeSet, PlannedChange};

/// One operation of the prepared plan as the engine compiles it. An
/// operation is at most one part, and one the engine does not compile is none.
#[derive(Debug, Clone)]
pub struct Part<M> {
    pub operation: usize,
    pub change: PlannedChange,
    pub metadata: M,
}
/// The real engine chooses an opaque input. A caller-defined engine gets a
/// distinct result brand; no generic scratch finalizer grants PlanView.
pub trait Engine: Dialect + Sized {
    type Input;
    type Metadata: Clone + std::fmt::Debug;
    fn blueprint(&self, input: Self::Input) -> Input<Self::Metadata>;
}
#[derive(Debug, Clone)]
pub struct Prepared {
    changes: super::PlanningPlan,
    edges: Vec<Constraint>,
}
impl Prepared {
    pub fn changes(&self) -> &super::PlanningPlan {
        &self.changes
    }
    pub fn edges(&self) -> &[Constraint] {
        &self.edges
    }
}
/// The generated expansion of reconstruction, with every new table split as
/// the engine's scratch parts say, and the constraints on the result. It
/// retains the differ's sources while dropping its deployment-only metadata.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn prepare<S: super::RouteState<D>, D: Dialect + ?Sized>(
    input: &super::PlanningPlan<S>,
    ids: &pbps_model::IdsFile,
    dependencies: &pbps_model::ModuleDeps,
    dialect: &D,
) -> Result<Prepared, String> {
    let mut declared = pbps_model::Schema::default();
    for p in &input.changes {
        match &p.change {
            Change::CreateTable { name, table, .. } => {
                declared.tables.insert(name.clone(), (**table).clone());
            }
            Change::CreateModule { id, module } => {
                declared.modules.insert(id.clone(), (**module).clone());
            }
            _ => {}
        }
    }
    let payload = ChangeSet {
        changes: input
            .changes
            .iter()
            .map(|p| PlannedChange::new(p.change.clone()))
            .collect(),
    };
    let proof = pbps_model::resolver::OrderingProof::new(&payload, Default::default()).unwrap();
    let mut changes =
        super::PlanningPlan::new(payload, input.origins.clone(), input.logical, proof);
    changes.boundaries = input.boundaries.clone();
    super::generated::expand_in(&mut changes, ids, dependencies, dialect, true)?;
    let edges = super::generated::constraints_in(&changes, &declared, dependencies, dialect, true);
    Ok(Prepared { changes, edges })
}
/// Logical blueprint facts supplied by the engine's opaque input. There is no
/// caller-supplied settlement or certificate conversion.
#[derive(Debug)]
pub struct Input<M> {
    prepared: Prepared,
    parts: Vec<Part<M>>,
}
impl<M> Input<M> {
    pub fn new(prepared: Prepared, parts: Vec<Part<M>>) -> Self {
        Self { prepared, parts }
    }
}
/// Completed scratch input, branded by its engine. It cannot become the
/// general PlanView and cannot certify an external payload copy.
///
/// ```compile_fail
/// fn recertify<E: pbps_diff::order::scratch::Engine>(
///     plan: &pbps_diff::order::scratch::ScratchPlan<E>) {
///     let _: &dyn pbps_diff::order::PlanView<E> = plan;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ScratchPlan<E: Engine> {
    parts: Vec<Part<E::Metadata>>,
    operations: Operations,
    changes: ChangeSet,
}
impl<E: Engine> ScratchPlan<E> {
    pub fn parts(&self) -> &[Part<E::Metadata>] {
        &self.parts
    }
}
impl<E: Engine> pbps_dialect::PlanPayload for ScratchPlan<E> {
    fn changes(&self) -> &ChangeSet {
        &self.changes
    }
    fn mode(&self) -> pbps_model::PlanMode {
        pbps_model::PlanMode::Transactional
    }
    fn operations(&self) -> Operations {
        self.operations.clone()
    }
}
/// The complete scratch route: one schedule over the constraints `prepare`
/// stated, each between the two operations it names. A cycle is the
/// schedule's named refusal, and an order is its lowest position first among
/// what is ready, so a part waits only where a constraint holds it.
pub fn settle<E: Engine>(engine: &E, input: E::Input) -> Result<ScratchPlan<E>, String> {
    let Input { prepared, parts } = engine.blueprint(input);
    let source = &prepared.changes;
    let mut at = vec![None; source.changes.len()];
    for (position, part) in parts.iter().enumerate() {
        assert!(
            at[part.operation].replace(position).is_none(),
            "an operation is at most one part"
        );
    }
    let operations = Operations::expanded(
        source.logical(),
        parts.iter().map(|part| {
            (
                source.origins()[part.operation].clone(),
                part.change.clone(),
            )
        }),
        ModuleEdits::InPlace,
    );
    let mut schedule = Schedule::new(source.changes.len());
    for edge in prepared.edges {
        schedule.edge(edge.before, edge.after, edge.source, edge.reason);
    }
    let names = ChangeSet {
        changes: source
            .changes
            .iter()
            .map(|p| PlannedChange::new(p.change.clone()))
            .collect(),
    };
    let settled = schedule
        .settle_raw(names.clone())
        .map_err(|e| e.describe(&names))?;
    let sequence: Vec<usize> = settled
        .sequence
        .iter()
        .filter_map(|&operation| at[operation])
        .collect();
    let operations = super::reordered_operations(&operations, &sequence);
    let parts = permute(parts, &sequence);
    let changes = ChangeSet {
        changes: parts.iter().map(|p| p.change.clone()).collect(),
    };
    Ok(ScratchPlan {
        parts,
        operations,
        changes,
    })
}
fn permute<T>(items: Vec<T>, sequence: &[usize]) -> Vec<T> {
    let mut taken: Vec<_> = items.into_iter().map(Some).collect();
    sequence
        .iter()
        .map(|&at| taken[at].take().expect("settlement is a permutation"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_dialect::PlanPayload;
    use pbps_model::operation::{Origin, Source as LogicalSource, Spelling};
    use pbps_model::plan_names::At;
    use pbps_model::resolver::OrderReason;

    impl Engine for pbps_dialect::MinimalDialect {
        type Input = Input<usize>;
        type Metadata = usize;
        fn blueprint(&self, input: Self::Input) -> Input<usize> {
            input
        }
    }
    fn role(name: &str) -> PlannedChange {
        PlannedChange::new(Change::CreateRole {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Role, name, 0),
            name: name.into(),
        })
    }
    /// One part for each of `operations`, the metadata being the operation.
    fn parts(changes: &[PlannedChange], operations: &[usize]) -> Vec<Part<usize>> {
        operations
            .iter()
            .map(|&operation| Part {
                operation,
                change: changes[operation].clone(),
                metadata: operation,
            })
            .collect()
    }
    fn prepared(changes: &[PlannedChange], edges: &[(usize, usize)]) -> Prepared {
        let source = super::super::propose(ChangeSet {
            changes: changes.to_vec(),
        });
        let mut prepared = prepare(
            &source,
            &Default::default(),
            &Default::default(),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        prepared.edges = edges
            .iter()
            .map(|&(before, after)| Constraint {
                before,
                after,
                source: super::super::Source::Lexical,
                reason: OrderReason::Structural,
            })
            .collect();
        prepared
    }
    #[test]
    fn a_constraint_moves_a_part_and_the_operations_keep_their_original_ids() {
        let changes = vec![role("one"), role("two"), role("three")];
        let plan = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(prepared(&changes, &[(2, 0)]), parts(&changes, &[0, 1, 2])),
        )
        .unwrap();
        // The lowest position that is ready goes first: `one` waits for
        // `three`, and `two` does not wait for either.
        assert_eq!(
            plan.parts().iter().map(|p| p.metadata).collect::<Vec<_>>(),
            [1, 2, 0]
        );
        let operations = plan.operations();
        for (operation, source) in operations.iter().zip([1, 2, 0]) {
            assert_eq!(operation.id().to_string(), format!("{source}.0"));
            assert_eq!(operation.origin(), &Origin::Whole(source));
            assert_eq!(
                operation.origin().spelling(),
                Spelling::At(At::Before(source))
            );
        }
        assert_eq!(plan.changes().changes.len(), 3);
    }
    #[test]
    fn an_order_through_an_operation_that_is_no_part_is_kept() {
        let changes = vec![role("one"), role("two"), role("three")];
        // `two` is compiled by no part, but `three` still precedes `one`
        // through it.
        let plan = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(
                prepared(&changes, &[(2, 1), (1, 0)]),
                parts(&changes, &[0, 2]),
            ),
        )
        .unwrap();
        assert_eq!(
            plan.parts().iter().map(|p| p.metadata).collect::<Vec<_>>(),
            [2, 0]
        );
        // Negative: with no constraint the order is the one given.
        let plan = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(prepared(&changes, &[]), parts(&changes, &[0, 2])),
        )
        .unwrap();
        assert_eq!(
            plan.parts().iter().map(|p| p.metadata).collect::<Vec<_>>(),
            [0, 2]
        );
    }
    #[test]
    fn scratch_parts_keep_the_prepared_logical_source_and_existing_roles() {
        let changes = vec![role("other"), role("split")];
        for (origin, spelling) in [
            (Origin::Whole(1), Spelling::At(At::Before(1))),
            (Origin::SplitPart(1), Spelling::At(At::After(1))),
            (
                Origin::Teardown(LogicalSource::Change(1)),
                Origin::Teardown(LogicalSource::Change(1)).spelling(),
            ),
            (
                Origin::Restoration(LogicalSource::Change(1)),
                Origin::Restoration(LogicalSource::Change(1)).spelling(),
            ),
        ] {
            let mut prepared = prepared(&changes, &[]);
            // Existing expansion roles must survive any settlement.
            prepared.changes.origins[1] = origin.clone();
            let plan = settle(
                &pbps_dialect::MinimalDialect,
                Input::new(prepared, parts(&changes, &[0, 1])),
            )
            .unwrap();
            let operations = plan.operations();
            let operation = operations.iter().nth(1).unwrap();
            assert_eq!(operation.origin(), &origin);
            assert_eq!(operation.origin().source(), LogicalSource::Change(1));
            assert_eq!(operation.origin().spelling(), spelling);
        }
    }
    #[test]
    fn a_generated_cycle_is_refused_with_its_original_change_names() {
        let changes = vec![role("reader"), role("writer")];
        let why = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(
                prepared(&changes, &[(0, 1), (1, 0)]),
                parts(&changes, &[0, 1]),
            ),
        )
        .unwrap_err();
        assert!(why.contains("reader") && why.contains("writer"), "{why}");
    }
    #[test]
    #[should_panic(expected = "an operation is at most one part")]
    fn two_parts_of_one_operation_are_refused() {
        let changes = vec![role("one")];
        let _ = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(prepared(&changes, &[]), parts(&changes, &[0, 0])),
        );
    }
}
