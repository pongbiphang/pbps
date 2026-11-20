//! The creation facts a plan's effects state are edges beside the lines
//! (DEC-2469.10), so a line that reverses one is a cycle.
use super::*;
use pbps_model::ModuleChange;

fn trigger() -> Module {
    Module {
        kind: ModuleKind::Trigger,
        definition: "AFTER INSERT AS SELECT 1".into(),
        description: None,
        options: Default::default(),
    }
}

/// A table and a trigger on it, in the order the differ gives: table first.
fn table_and_trigger() -> (Fixture, Vec<Change>) {
    let mut f = Fixture::new();
    f.after.modules.clear();
    f.after
        .modules
        .insert("app.t.trg".parse().unwrap(), trigger());
    let mut changes = f.creates();
    changes[0] = Change::Module(ModuleChange::CreateModule {
        id: "app.t.trg".parse().unwrap(),
        module: Box::new(trigger()),
    });
    changes.reverse();
    (f, changes)
}

/// A plan the way a project with no extended annotation reaches the passes:
/// no contract is built for it.
fn unannotated(changes: Vec<Change>) -> PlanningPlan {
    propose(ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    })
}

#[test]
fn a_creation_fact_is_an_edge_only_where_the_pass_settles_by_it() {
    let (f, changes) = table_and_trigger();
    let goal = dependencies(&[("module::app.t.trg", &["table::app.t"])]);
    let plan = f.plan(changes, &ObjectDeps::default(), &goal).unwrap();
    let contract = plan.annotations.as_ref().unwrap();
    let (edges, lines) = contract.settling(&plan).unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.source == Source::Effect && (e.before, e.after) == (0, 1)),
        "{edges:?}"
    );
    assert_eq!(
        lines[&(0, 1)].iter().collect::<Vec<_>>(),
        ["annotation `module::app.t.trg` depends on `table::app.t`"]
    );
    // Negative: what is not a pass's own settlement still reads the lines alone.
    let lines_only = plan.annotation_edges().unwrap();
    assert!(lines_only.iter().all(|e| e.source == Source::Annotation));
}

#[test]
fn a_line_that_puts_a_trigger_before_its_table_is_a_cycle_that_names_the_line() {
    let (f, changes) = table_and_trigger();
    let goal = dependencies(&[("table::app.t", &["module::app.t.trg"])]);
    let plan = f.plan(changes, &ObjectDeps::default(), &goal).unwrap();
    let refusal = Schedule::new(2).settle(plan.clone()).unwrap_err();
    let Refusal::Cycle { members, edges, .. } = &refusal else {
        panic!("{refusal:?}")
    };
    assert_eq!(members, &[0, 1]);
    assert!(edges.iter().any(|e| e.source == Source::Effect));
    let text = refusal.describe(plan.changes());
    assert!(
        text.starts_with("these steps wait on one another: "),
        "{text}"
    );
    assert!(
        text.ends_with("line you wrote: annotation `table::app.t` depends on `module::app.t.trg`"),
        "{text}"
    );
}

#[test]
fn a_line_that_agrees_with_the_fact_leaves_the_order_as_it_was() {
    let (f, changes) = table_and_trigger();
    let goal = dependencies(&[("module::app.t.trg", &["table::app.t"])]);
    let plan = f
        .plan(changes.clone(), &ObjectDeps::default(), &goal)
        .unwrap();
    let annotated = Schedule::new(2).settle(plan).unwrap().sequence;
    // Negative: the plan with no line at all is ordered by the same two steps.
    let plain = unannotated(changes);
    assert!(plain.annotations.is_none());
    assert_eq!(Schedule::new(2).settle(plain).unwrap().sequence, [0, 1]);
    assert_eq!(annotated, [0, 1]);
}

#[test]
fn a_plan_with_no_annotation_states_no_creation_fact() {
    // The trigger is given before its table: without a line nothing reads the
    // fact, so the order is the one given.
    let (_, mut changes) = table_and_trigger();
    changes.reverse();
    let plain = unannotated(changes);
    assert!(plain.annotations.is_none());
    assert_eq!(Schedule::new(2).settle(plain).unwrap().sequence, [0, 1]);
}

#[test]
fn an_effect_edge_a_pair_already_has_is_not_told_twice() {
    let given = ChangeSet {
        changes: f_creates().into_iter().map(PlannedChange::new).collect(),
    };
    let told = |effect: bool| {
        let mut schedule = Schedule::new(2);
        schedule.edge(0, 1, Source::Lexical, OrderReason::Structural);
        if effect {
            schedule.edge(0, 1, Source::Effect, OrderReason::Structural);
        }
        schedule.edge(1, 0, Source::Annotation, OrderReason::Structural);
        schedule.sequence().unwrap_err().describe(&given)
    };
    // The text a table's own part had before the facts were edges.
    assert_eq!(told(true), told(false));
    assert!(!told(true).contains("Effect"));
    // Negative: where nothing else orders the pair, the effect edge is told.
    let mut alone = Schedule::new(2);
    alone.edge(0, 1, Source::Effect, OrderReason::Structural);
    alone.edge(1, 0, Source::Annotation, OrderReason::Structural);
    assert!(
        alone
            .sequence()
            .unwrap_err()
            .describe(&given)
            .contains("(structural, Effect)")
    );
}

fn f_creates() -> Vec<Change> {
    Fixture::new().creates()
}
