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

/// A step whose effects cannot be read here: a rebuild root, which the
/// effects refuse because a root must be expanded before it is ordered.
fn unreadable() -> Change {
    let root: pbps_model::RebuildPartitionedTable = serde_json::from_value(serde_json::json!({
        "table": "app.p",
        "intent": {
            "source": {"tables": {}, "columns": {}},
            "replacement": {"tables": {}, "columns": {}},
            "replacement_root": "t_dddddd",
            "continues": {},
            "target_checksum": "x",
            "max_rows": 1,
            "allowed_at": "2026-01-01T00:00:00Z",
            "reason": "test",
            "operator": "test"
        },
        "before": {},
        "after": {}
    }))
    .unwrap();
    Change::Partition(pbps_model::PartitionChange::RebuildPartitionedTable(
        Box::new(root),
    ))
}

#[test]
fn a_step_whose_creation_facts_cannot_be_read_refuses_the_annotated_plan_naming_it() {
    let (mut f, mut changes) = table_and_trigger();
    // The rebuilt table stands in the baseline and in the goal.
    let parent = f.after.tables[&"app.t".parse().unwrap()].clone();
    for schema in [&mut f.before, &mut f.after] {
        schema
            .tables
            .insert("app.p".parse().unwrap(), parent.clone());
    }
    for ids in [&mut f.base, &mut f.ids] {
        ids.tables
            .insert("t_dddddd".parse().unwrap(), "app.p".parse().unwrap());
        ids.columns
            .insert("c_dddddd".parse().unwrap(), "app.p.x".parse().unwrap());
    }
    changes.push(unreadable());
    let goal = dependencies(&[("module::app.t.trg", &["table::app.t"])]);
    let plan = f
        .plan(changes.clone(), &ObjectDeps::default(), &goal)
        .unwrap();
    let refusal = Schedule::new(3).settle(plan.clone()).unwrap_err();
    let text = refusal.describe(plan.changes());
    assert!(text.contains("the creation facts of step"), "{text}");
    assert!(text.contains("app.p"), "{text}");
    assert!(text.contains("logical root must be expanded"), "{text}");
    assert!(text.contains("cannot be read"), "{text}");
    // Negative: a plan with no annotation computes no fact, so the same step
    // reads as nothing there and the plan keeps its order.
    let plain = unannotated(changes);
    assert_eq!(Schedule::new(3).settle(plain).unwrap().sequence, [0, 1, 2]);
    // Negative: without the unreadable step the annotated plan is accepted.
    let (f, changes) = table_and_trigger();
    let readable = f.plan(changes, &ObjectDeps::default(), &goal).unwrap();
    assert_eq!(Schedule::new(2).settle(readable).unwrap().sequence, [0, 1]);
}
