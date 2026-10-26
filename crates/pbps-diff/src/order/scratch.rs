//! Scratch's canonical source-mapped blueprint route. Both existing
//! settlements finish before lowering; their constraints stay separate.
use super::{Constraint, Schedule};
use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations, Origin};
use pbps_model::{Change, ChangeSet, PlannedChange, TableName};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Declared enums and domains: every table and routine that names one
    /// needs it first, and nothing a bootstrap makes is needed by a type.
    Types,
    Tables,
    Keys,
    Modules,
    Expressions,
}
#[derive(Debug, Clone)]
pub struct HeldTable {
    pub table: TableName,
    pub steps: std::ops::Range<usize>,
    pub generated: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct Part<M> {
    pub operation: usize,
    /// Assigned by Input, before either settlement.
    pub slot: usize,
    pub change: PlannedChange,
    pub phase: Phase,
    pub declaration: String,
    pub names: Vec<String>,
    pub calls: BTreeSet<String>,
    pub tables: BTreeSet<TableName>,
    pub metadata: M,
}
/// The real engine chooses an opaque input. A caller-defined engine gets a
/// distinct result brand; no generic scratch finalizer grants PlanView.
pub trait Engine: Dialect + Sized {
    type Input;
    type Metadata: Clone + std::fmt::Debug;
    fn blueprint(&self, input: Self::Input) -> Input<Self::Metadata>;
    fn phase(metadata: &mut Self::Metadata, phase: Phase);
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
/// The same initial generated expansion and settlement as reconstruction,
/// retaining the differ's sources while dropping its deployment-only metadata.
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
    super::generated::expand(&mut changes, ids, dependencies, dialect)?;
    super::generated::settle(&mut changes, &declared, dependencies, dialect)?;
    let edges = super::generated::constraints(&changes, &declared, dependencies, dialect);
    Ok(Prepared { changes, edges })
}
/// Logical blueprint facts supplied by the engine's opaque input. There is no
/// caller-supplied settlement or certificate conversion.
#[derive(Debug)]
pub struct Input<M> {
    prepared: Prepared,
    parts: Vec<Part<M>>,
    late: Vec<HeldTable>,
    functions: Vec<(usize, String)>,
    foreign: BTreeSet<usize>,
}
impl<M> Input<M> {
    pub fn new(
        prepared: Prepared,
        parts: Vec<Part<M>>,
        late: Vec<HeldTable>,
        functions: Vec<(usize, String)>,
        foreign: BTreeSet<usize>,
    ) -> Self {
        let parts = parts
            .into_iter()
            .enumerate()
            .map(|(slot, mut part)| {
                part.slot = slot;
                part
            })
            .collect();
        Self {
            prepared,
            parts,
            late,
            functions,
            foreign,
        }
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
    first: Vec<Part<E::Metadata>>,
    parts: Vec<Part<E::Metadata>>,
    operations: Operations,
    changes: ChangeSet,
}
impl<E: Engine> ScratchPlan<E> {
    pub fn first(&self) -> &[Part<E::Metadata>] {
        &self.first
    }
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
/// The complete scratch route: canonical function/phase compatibility, then
/// the existing generated constraints, never a union or an empty finalizer.
pub fn settle<E: Engine>(engine: &E, input: E::Input) -> Result<ScratchPlan<E>, String> {
    let Input {
        prepared,
        parts,
        late,
        functions,
        foreign,
    } = engine.blueprint(input);
    let source = &prepared.changes;
    let mut part_counts = vec![0; source.changes.len()];
    for part in &parts {
        part_counts[part.operation] += 1;
    }
    let operations = Operations::expanded(
        source.logical(),
        parts.iter().map(|part| {
            // Scratch decomposition has the same source and after-change
            // spelling as other splits, before either settlement moves it.
            let mut origin = source.origins()[part.operation].clone();
            if part_counts[part.operation] > 1
                && let Origin::Whole(source) = origin
            {
                origin = Origin::SplitPart(source);
            }
            (origin, part.change.clone())
        }),
        ModuleEdits::InPlace,
    );
    // Retain IDs assigned before either settlement; do not renumber split parts.
    let first = function_edges::<E>(parts, late, &functions, &foreign, source, engine)?;
    let mut schedule = Schedule::new(first.len());
    for edge in prepared.edges {
        let before = first.iter().rposition(|p| p.operation == edge.before);
        let after = first.iter().position(|p| p.operation == edge.after);
        if let (Some(before), Some(after)) = (before, after) {
            schedule.edge(before, after, edge.source, edge.reason);
        }
    }
    let names = names_for(source, &first);
    let settled = schedule
        .settle_raw(names.clone())
        .map_err(|e| e.describe(&names))?;
    let sequence = first.iter().map(|p| p.slot).collect::<Vec<_>>();
    let original = super::reordered_operations(&operations, &sequence);
    let operations = super::reordered_operations(&original, &settled.sequence);
    let parts = permute(first.clone(), &settled.sequence);
    let changes = ChangeSet {
        changes: parts.iter().map(|p| p.change.clone()).collect(),
    };
    Ok(ScratchPlan {
        first,
        parts,
        operations,
        changes,
    })
}
fn names_for<M>(source: &super::PlanningPlan, parts: &[Part<M>]) -> ChangeSet {
    ChangeSet {
        changes: parts
            .iter()
            .map(|p| PlannedChange::new(source.changes[p.operation].change.clone()))
            .collect(),
    }
}
fn permute<T>(items: Vec<T>, sequence: &[usize]) -> Vec<T> {
    let mut taken: Vec<_> = items.into_iter().map(Some).collect();
    sequence
        .iter()
        .map(|&at| taken[at].take().expect("settlement is a permutation"))
        .collect()
}

fn edge(schedule: &mut super::Schedule, before: usize, after: usize) {
    schedule.edge(
        before,
        after,
        super::Source::Lexical,
        pbps_model::resolver::OrderReason::Structural,
    );
}

/// Peers keep their input order, and a block follows the preceding block.
/// These are compatibility edges, not a new priority or a pre-sort.
fn block(schedule: &mut super::Schedule, previous: &mut Option<usize>, peers: &[usize]) {
    for &at in peers {
        if let Some(before) = *previous {
            edge(schedule, before, at);
        }
        *previous = Some(at);
    }
}

fn function_edges<E: Engine>(
    mut steps: Vec<Part<E::Metadata>>,
    late: Vec<HeldTable>,
    functions: &[(usize, String)],
    foreign: &BTreeSet<usize>,
    source: &super::PlanningPlan,
    engine: &E,
) -> Result<Vec<Part<E::Metadata>>, String> {
    let late: Vec<_> = late
        .into_iter()
        .filter_map(|table| {
            let called: Vec<usize> = functions
                .iter()
                .filter(|(_, name)| {
                    table
                        .generated
                        .iter()
                        .any(|text| engine.generation_may_call(text, name))
                })
                .map(|(at, _)| *at)
                .collect();
            (!called.is_empty()).then_some((table, called))
        })
        .collect();
    let names = |at: usize, name: &str| steps[at].calls.contains(name);
    let early = |at: usize| matches!(steps[at].phase, Phase::Tables | Phase::Keys);
    let keys: Vec<_> = foreign
        .iter()
        .copied()
        .filter(|&at| late.iter().any(|(table, _)| names(at, &table.table.name)))
        .collect();
    // The projection contains metadata positions only. Each subsequent order
    // is produced by Schedule, never by splicing this list around a callee.
    let mut modules: Vec<_> = (0..steps.len())
        .filter(|&at| steps[at].phase == Phase::Modules)
        .collect();
    let mut placed: BTreeSet<_> = modules.iter().copied().collect();
    let mut delayed = BTreeSet::new();
    for (table, called) in &late {
        let group: Vec<_> = table
            .steps
            .clone()
            .filter(|&at| early(at) && !keys.contains(&at))
            .collect();
        let point = modules
            .iter()
            .rposition(|at| {
                called.contains(at)
                    || (delayed.contains(at)
                        && steps[*at]
                            .names
                            .iter()
                            .any(|name| group.iter().any(|&own| names(own, name))))
            })
            .map_or(0, |position| position + 1);
        let mut held = BTreeSet::from([table.table.name.clone()]);
        let mut free = Vec::new();
        let mut readers = Vec::new();
        for &at in &modules[..point] {
            if held.iter().any(|name| {
                if name == &table.table.name {
                    steps[at].tables.contains(&table.table)
                } else {
                    names(at, name)
                }
            }) {
                if called.contains(&at) {
                    return Err(format!(
                        "table {} ({})",
                        table.table.shown(),
                        steps[at].declaration
                    ));
                }
                held.extend(steps[at].names.iter().cloned());
                readers.push(at);
            } else {
                free.push(at);
            }
        }
        let mut schedule = super::Schedule::new(steps.len());
        let mut previous = None;
        // The last callee (or a delayed name it uses) stays before the table.
        // Readers follow the complete group; later peers keep following those
        // readers, as the old hold did. Sink alone would not retain that barrier.
        block(&mut schedule, &mut previous, &free);
        block(&mut schedule, &mut previous, &group);
        block(&mut schedule, &mut previous, &readers);
        block(&mut schedule, &mut previous, &modules[point..]);
        placed.extend(group.iter().copied());
        delayed.extend(group);
        modules = schedule
            .sequence()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|at| placed.contains(at))
            .collect();
    }
    placed.extend(keys.iter().copied());
    for &at in &placed {
        steps[at].phase = Phase::Modules;
        E::phase(&mut steps[at].metadata, Phase::Modules);
    }

    let mut schedule = super::Schedule::new(steps.len());
    let mut previous = None;
    for phase in [
        Phase::Types,
        Phase::Tables,
        Phase::Keys,
        Phase::Modules,
        Phase::Expressions,
    ] {
        if phase == Phase::Modules {
            block(&mut schedule, &mut previous, &modules);
            block(&mut schedule, &mut previous, &keys);
        } else {
            let peers: Vec<_> = (0..steps.len())
                .filter(|&at| steps[at].phase == phase)
                .collect();
            block(&mut schedule, &mut previous, &peers);
        }
    }
    // Phase compatibility is settled before generated-part constraints, which
    // may legitimately move a key across a phase boundary (DEC-1959.3).
    let names = names_for(source, &steps);
    let settled = schedule
        .settle_raw(names.clone())
        .map_err(|e| e.describe(&names))?;
    Ok(permute(steps, &settled.sequence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_dialect::PlanPayload;
    use pbps_model::operation::{Source as LogicalSource, Spelling};
    use pbps_model::plan_names::At;
    use pbps_model::resolver::OrderReason;

    impl Engine for pbps_dialect::MinimalDialect {
        type Input = Input<usize>;
        type Metadata = usize;
        fn blueprint(&self, input: Self::Input) -> Input<usize> {
            input
        }
        fn phase(_metadata: &mut usize, _phase: Phase) {}
    }
    fn role(name: &str) -> PlannedChange {
        PlannedChange::new(Change::CreateRole {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Role, name, 0),
            name: name.into(),
        })
    }
    fn parts(changes: &[PlannedChange], phases: &[Phase]) -> Vec<Part<usize>> {
        phases
            .iter()
            .enumerate()
            .map(|(slot, &phase)| Part {
                operation: slot.min(changes.len() - 1),
                slot: usize::MAX,
                change: changes[slot.min(changes.len() - 1)].clone(),
                phase,
                declaration: format!("part {slot}"),
                names: Vec::new(),
                calls: BTreeSet::new(),
                tables: BTreeSet::new(),
                metadata: slot,
            })
            .collect()
    }
    #[test]
    fn equal_parts_keep_their_original_ids_when_phase_order_moves_them() {
        let change = role("one");
        let source = super::super::propose(ChangeSet {
            changes: vec![change.clone()],
        });
        let prepared = prepare(
            &source,
            &Default::default(),
            &Default::default(),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let plan = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(
                prepared,
                parts(
                    &[change],
                    &[Phase::Expressions, Phase::Tables, Phase::Modules],
                ),
                Vec::new(),
                Vec::new(),
                BTreeSet::new(),
            ),
        )
        .unwrap();
        assert_eq!(
            plan.parts().iter().map(|p| p.metadata).collect::<Vec<_>>(),
            [1, 2, 0]
        );
        let ids = plan
            .operations()
            .iter()
            .map(|op| (op.id().source(), op.id().part()))
            .collect::<Vec<_>>();
        assert_eq!(ids, [(0, 1), (0, 2), (0, 0)]);
        for operation in plan.operations().iter() {
            assert_eq!(operation.origin(), &Origin::SplitPart(0));
            assert_eq!(operation.origin().source(), LogicalSource::Change(0));
            assert_eq!(operation.origin().spelling(), Spelling::At(At::After(0)));
        }
        assert_eq!(plan.changes().changes.len(), 3);
    }
    #[test]
    fn unsplit_operations_keep_their_whole_origins_when_phases_move_them() {
        let changes = vec![role("one"), role("two")];
        let source = super::super::propose(ChangeSet {
            changes: changes.clone(),
        });
        let prepared = prepare(
            &source,
            &Default::default(),
            &Default::default(),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let plan = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(
                prepared,
                parts(&changes, &[Phase::Modules, Phase::Tables]),
                Vec::new(),
                Vec::new(),
                BTreeSet::new(),
            ),
        )
        .unwrap();
        let operations = plan.operations();
        for (operation, source) in operations.iter().zip([1, 0]) {
            assert_eq!(operation.id().to_string(), format!("{source}.0"));
            assert_eq!(operation.origin(), &Origin::Whole(source));
            assert_eq!(
                operation.origin().spelling(),
                Spelling::At(At::Before(source))
            );
        }
        assert_eq!(operations.len(), 2);
    }
    #[test]
    fn scratch_splits_keep_the_prepared_logical_source_and_existing_roles() {
        let source = super::super::propose(ChangeSet {
            changes: vec![role("other"), role("split")],
        });
        let mut schedule = Schedule::new(2);
        schedule.edge(1, 0, super::super::Source::Effect, OrderReason::Structural);
        let source = schedule.settle(source).unwrap().into_planning();
        assert_eq!(source.origins(), [Origin::Whole(1), Origin::Whole(0)]);
        for (origin, expected) in [
            (Origin::Whole(1), Origin::SplitPart(1)),
            (Origin::SplitPart(1), Origin::SplitPart(1)),
            (
                Origin::Teardown(LogicalSource::Change(1)),
                Origin::Teardown(LogicalSource::Change(1)),
            ),
            (
                Origin::Restoration(LogicalSource::Change(1)),
                Origin::Restoration(LogicalSource::Change(1)),
            ),
        ] {
            let mut prepared = prepare(
                &source,
                &Default::default(),
                &Default::default(),
                &pbps_dialect::MinimalDialect,
            )
            .unwrap();
            // Existing expansion roles must survive any further blueprint split.
            prepared.changes.origins[0] = origin;
            let plan = settle(
                &pbps_dialect::MinimalDialect,
                Input::new(
                    prepared,
                    parts(&source.changes[..1], &[Phase::Expressions, Phase::Tables]),
                    Vec::new(),
                    Vec::new(),
                    BTreeSet::new(),
                ),
            )
            .unwrap();
            let operations = plan.operations();
            assert_eq!(operations.len(), 2);
            for (operation, part) in operations.iter().zip([1, 0]) {
                assert_eq!(operation.id().to_string(), format!("1.{part}"));
                assert_eq!(operation.origin(), &expected);
                assert_eq!(operation.origin().source(), LogicalSource::Change(1));
                assert_eq!(operation.origin().spelling(), expected.spelling());
            }
        }
    }
    #[test]
    fn a_generated_cycle_is_refused_with_its_original_change_names() {
        let changes = vec![role("reader"), role("writer")];
        let source = super::super::propose(ChangeSet {
            changes: changes.clone(),
        });
        let mut prepared = prepare(
            &source,
            &Default::default(),
            &Default::default(),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        prepared.edges = [(0, 1), (1, 0)]
            .into_iter()
            .map(|(before, after)| Constraint {
                before,
                after,
                source: super::super::Source::Effect,
                reason: OrderReason::Structural,
            })
            .collect();
        let why = settle(
            &pbps_dialect::MinimalDialect,
            Input::new(
                prepared,
                parts(&changes, &[Phase::Tables, Phase::Modules]),
                Vec::new(),
                Vec::new(),
                BTreeSet::new(),
            ),
        )
        .unwrap_err();
        assert!(why.contains("reader") && why.contains("writer"), "{why}");
    }
}
