//! Scratch's canonical source-mapped blueprint route. The new tables are split
//! into the parts the engine creates one at a time, and one `Schedule`
//! settles the constraints `prepare` states over them (DEC-2389.1).
use super::{Constraint, Schedule};
mod auxiliary;
mod fill;
mod trial;
pub use fill::{FillDependency, FillNode, FillProgram, FillWork};
/// Scratch-only provenance; operation IDs stay in the prepared coordinate space.
#[derive(Debug, Clone)]
pub struct DroppedGuess {
    pub edge: Constraint,
    pub members: Vec<usize>,
    pub backward_only: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRule {
    Inferred,
    Annotation,
    Structural,
}
#[derive(Debug, Clone)]
pub struct EdgeWhy {
    pub edge: Constraint,
    pub rule: EdgeRule,
}

pub use auxiliary::{Auxiliary, Node, Step};
use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations};
use pbps_model::{Change, ChangeSet, PlannedChange};
pub use trial::{Attempt, Progress, TrialProgram, Trials};

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
    fn trial_order(&self) -> bool {
        false
    }
}
#[derive(Debug, Clone)]
pub struct Prepared {
    changes: super::PlanningPlan,
    edges: Vec<Constraint>,
    why: Vec<EdgeWhy>,
}
impl Prepared {
    pub fn changes(&self) -> &super::PlanningPlan {
        &self.changes
    }
    pub fn edges(&self) -> &[Constraint] {
        &self.edges
    }
    #[cfg(feature = "test-fixtures")]
    pub fn without_guesses(mut self) -> Self {
        self.edges.retain(|e| e.source != super::Source::Guess);
        self.why.retain(|w| w.edge.source != super::Source::Guess);
        self
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
    let mut edges =
        super::generated::constraints_in(&changes, &declared, dependencies, dialect, true);
    let mut why: Vec<_> = edges
        .iter()
        .map(|edge| EdgeWhy {
            edge: edge.clone(),
            rule: if edge.source == super::Source::Guess {
                EdgeRule::Inferred
            } else {
                EdgeRule::Structural
            },
        })
        .collect();
    if dialect.scratch_text_edges_are_guesses() {
        // Explicit identities must not pass through the inferred same-leaf
        // exclusion (DEC-2465.1); no general Source/Requirement rule changes.
        let module_at = |id: &pbps_model::ModuleId| {
            changes.changes.iter().position(
                |p| matches!(&p.change, Change::CreateModule { id: found, .. } if found == id),
            )
        };
        for (owner, providers) in dependencies {
            for provider in providers {
                if let (Some(before), Some(after)) = (module_at(provider), module_at(owner)) {
                    let edge = Constraint {
                        before,
                        after,
                        source: super::Source::Lexical,
                        reason: pbps_model::resolver::OrderReason::Structural,
                    };
                    why.push(EdgeWhy {
                        edge: edge.clone(),
                        rule: EdgeRule::Annotation,
                    });
                    edges.push(edge);
                }
            }
        }
    }
    Ok(Prepared {
        changes,
        edges,
        why,
    })
}
/// Logical blueprint facts supplied by the engine's opaque input. There is no
/// caller-supplied settlement or certificate conversion.
#[derive(Debug)]
pub struct Input<M> {
    prepared: Prepared,
    parts: Vec<Part<M>>,
    auxiliary: Vec<Auxiliary<M>>,
}
impl<M> Input<M> {
    pub fn new(prepared: Prepared, parts: Vec<Part<M>>) -> Self {
        Self {
            prepared,
            parts,
            auxiliary: Vec::new(),
        }
    }

    /// Scratch-only nodes, not changes or borrowed managed origins.
    pub fn with_auxiliary(mut self, auxiliary: Vec<Auxiliary<M>>) -> Self {
        self.auxiliary = auxiliary;
        self
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
    trial: Option<TrialProgram<E>>,
    steps: Vec<Step<E::Metadata>>,
}
impl<E: Engine> ScratchPlan<E> {
    /// The execution sequence within compile ranges. Auxiliary positions are
    /// hard boundaries: a trial of managed groups cannot overtake or retry
    /// them unless their executor explicitly opts in (DEC-2376.1).
    pub fn steps(&self) -> &[Step<E::Metadata>] {
        &self.steps
    }

    pub fn parts(&self) -> &[Part<E::Metadata>] {
        &self.parts
    }
    pub fn trial(&self) -> Option<&TrialProgram<E>> {
        self.trial.as_ref()
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
    let Input {
        prepared,
        parts,
        auxiliary,
    } = engine.blueprint(input);
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
    let names = ChangeSet {
        changes: source
            .changes
            .iter()
            .map(|p| PlannedChange::new(p.change.clone()))
            .collect(),
    };
    let mut dropped = Vec::new();
    let (settled, steps) = auxiliary::settle(&prepared, &at, &auxiliary, &mut dropped)?;
    let sequence: Vec<usize> = settled
        .iter()
        .filter_map(|&operation| at[operation])
        .collect();
    let trial = if engine.trial_order() {
        let sources: Vec<_> = sequence.iter().map(|&part| parts[part].operation).collect();
        let mut program = TrialProgram::project(&prepared.edges, &sources, &names)?;
        program.set_trace(prepared.why, dropped, sources, &names);
        Some(program)
    } else {
        None
    };
    let operations = super::reordered_operations(&operations, &sequence);
    let parts = permute(parts, &sequence);
    let changes = ChangeSet {
        changes: parts.iter().map(|p| p.change.clone()).collect(),
    };
    Ok(ScratchPlan {
        parts,
        operations,
        changes,
        trial,
        steps,
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
    mod auxiliary_tests;
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

#[cfg(test)]
mod trial_tests;
