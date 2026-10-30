use super::*;
use crate::order::{PlanningPlan, Refusal, Schedule, propose};
use pbps_model::operation::Snapshot;
use pbps_model::{ChangeSet, Column, IdsFile, Module, ModuleKind, PlannedChange, Schema, Table};

fn dependencies(pairs: &[(&str, &[&str])]) -> ObjectDeps {
    ObjectDeps(
        pairs
            .iter()
            .map(|(owner, providers)| {
                (
                    owner.parse().unwrap(),
                    providers.iter().map(|p| p.parse().unwrap()).collect(),
                )
            })
            .collect(),
    )
}
fn view() -> Module {
    Module {
        kind: ModuleKind::View,
        definition: "SELECT 1 AS x".into(),
        description: None,
        options: Default::default(),
    }
}
struct Fixture {
    before: Schema,
    after: Schema,
    base: IdsFile,
    ids: IdsFile,
}
impl Fixture {
    fn new() -> Self {
        let mut after = Schema::default();
        after.tables.insert(
            "app.t".parse().unwrap(),
            Table {
                columns: [("x".into(), Column::new("integer".parse().unwrap()))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        after.modules.insert("app.f".parse().unwrap(), view());
        let ids = IdsFile {
            tables: [("t_aaaaaa".parse().unwrap(), "app.t".parse().unwrap())].into(),
            columns: [("c_aaaaaa".parse().unwrap(), "app.t.x".parse().unwrap())].into(),
            ..Default::default()
        };
        Self {
            before: Schema::default(),
            after,
            base: IdsFile::default(),
            ids,
        }
    }
    fn creates(&self) -> Vec<Change> {
        vec![
            Change::CreateModule {
                id: "app.f".parse().unwrap(),
                module: Box::new(view()),
            },
            Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "app.t".parse().unwrap(),
                table: Box::new(self.after.tables[&"app.t".parse().unwrap()].clone()),
                beside_default: None,
                moves_at_most: None,
            },
        ]
    }
    fn plan(
        &self,
        changes: Vec<Change>,
        base: &ObjectDeps,
        goal: &ObjectDeps,
    ) -> Result<PlanningPlan, String> {
        self.plan_with_modules(
            changes,
            base,
            goal,
            pbps_model::operation::ModuleEdits::Rebuilt,
        )
    }
    fn plan_with_modules(
        &self,
        changes: Vec<Change>,
        base: &ObjectDeps,
        goal: &ObjectDeps,
        modules: pbps_model::operation::ModuleEdits,
    ) -> Result<PlanningPlan, String> {
        propose(ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        })
        .with_annotations(
            Snapshot {
                schema: &self.before,
                ids: &self.base,
            },
            Snapshot {
                schema: &self.after,
                ids: &self.ids,
            },
            base,
            goal,
            modules,
        )
    }
}

#[test]
fn explicit_intent_displaces_a_text_guess_but_never_a_typed_or_catalog_fact() {
    let f = Fixture::new();
    let goal = dependencies(&[("app.f", &["table::app.t"])]);
    let plan = f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap();
    for source in [Source::Inferred, Source::Guess] {
        let mut schedule = Schedule::new(2);
        schedule.edge(0, 1, source, OrderReason::Structural);
        let settled = schedule.settle(plan.clone()).unwrap();
        assert_eq!(settled.sequence, [1, 0]);
        assert_eq!(settled.proof.edges().len(), 1);
    }
    for source in [
        Source::Effect,
        Source::Catalog,
        Source::Binding,
        Source::Execution,
        Source::Lexical,
    ] {
        let mut schedule = Schedule::new(2);
        schedule.edge(0, 1, source, OrderReason::Structural);
        assert!(
            matches!(schedule.settle(plan.clone()), Err(Refusal::Cycle { .. })),
            "{source:?}"
        );
    }
    let mut guesses = Schedule::new(3);
    guesses.edge(0, 1, Source::Inferred, OrderReason::Structural);
    guesses.edge(1, 2, Source::Inferred, OrderReason::Structural);
    let mut changes = f.creates();
    changes.insert(
        1,
        Change::CreateModule {
            id: "app.middle".parse().unwrap(),
            module: Box::new(view()),
        },
    );
    let mut f = f;
    f.after
        .modules
        .insert("app.middle".parse().unwrap(), view());
    assert_eq!(
        guesses
            .settle(f.plan(changes, &ObjectDeps::default(), &goal).unwrap())
            .unwrap()
            .sequence,
        [1, 2, 0]
    );
}

#[test]
fn empty_and_legacy_only_intent_preserve_the_compatibility_schedule() {
    let f = Fixture::new();
    let raw = propose(ChangeSet {
        changes: f.creates().into_iter().map(PlannedChange::new).collect(),
    });
    let annotated = f
        .plan(f.creates(), &ObjectDeps::default(), &ObjectDeps::default())
        .unwrap();
    let mut schedule = Schedule::new(2);
    schedule.edge(0, 1, Source::Inferred, OrderReason::Structural);
    let original = schedule.settle(raw).unwrap();
    let result = schedule.settle(annotated).unwrap();
    assert_eq!(
        serde_json::to_vec(original.changes.changes()).unwrap(),
        serde_json::to_vec(result.changes.changes()).unwrap()
    );
    assert_eq!(original.proof, result.proof);
    assert_eq!(result.sequence, [0, 1]);
}

#[test]
fn parts_use_exposed_boundaries_and_otherwise_hold_the_indivisible_group() {
    let mut f = Fixture::new();
    f.after
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["x"]
        .value_source = pbps_model::ValueSource::Default("1".into());
    let goal = dependencies(&[("default::app.t.x", &["app.f"])]);
    let whole = f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap();
    assert_eq!(Schedule::new(2).settle(whole).unwrap().sequence, [0, 1]);
    let mut changes = f.creates();
    let Change::CreateTable { table, .. } = &mut changes[1] else {
        panic!()
    };
    table.columns["x"].value_source = pbps_model::ValueSource::None;
    changes.push(Change::AlterColumnDefault {
        uid: "c_aaaaaa".parse().unwrap(),
        column: "app.t.x".parse().unwrap(),
        from: None,
        to: Some("1".into()),
    });
    let goal = dependencies(&[
        ("default::app.t.x", &["app.f"]),
        ("app.f", &["table::app.t"]),
    ]);
    let split = f.plan(changes, &ObjectDeps::default(), &goal).unwrap();
    let edges = split.annotation_edges().unwrap();
    assert!(edges.iter().any(|e| (e.before, e.after) == (1, 0)));
    assert!(edges.iter().any(|e| (e.before, e.after) == (0, 2)));
    assert!(!edges.iter().any(|e| (e.before, e.after) == (0, 1)));
    assert_eq!(Schedule::new(3).settle(split).unwrap().sequence, [1, 0, 2]);
    let indivisible = f.plan(f.creates(), &ObjectDeps::default(), &goal).unwrap();
    let refusal = Schedule::new(2).settle(indivisible.clone()).unwrap_err();
    assert!(
        matches!(&refusal, Refusal::Cycle { members, edges, .. } if members==&[0,1] && edges.iter().all(|e| e.source==Source::Annotation))
    );
    let text = refusal.describe(indivisible.changes());
    assert!(text.contains("app.t") && text.contains("app.f"), "{text}");
}

#[test]
fn annotation_edits_on_standing_objects_create_no_ddl_or_fake_provider() {
    let mut f = Fixture::new();
    f.before = f.after.clone();
    f.base = f.ids.clone();
    let goal = dependencies(&[("app.f", &["table::app.t"])]);
    let plan = f.plan(Vec::new(), &ObjectDeps::default(), &goal).unwrap();
    assert!(plan.annotation_edges().unwrap().is_empty());
    assert!(
        Schedule::new(0)
            .settle(plan)
            .unwrap()
            .changes
            .changes
            .is_empty()
    );
    let f = Fixture::new();
    let incomplete = f
        .plan(vec![f.creates()[0].clone()], &ObjectDeps::default(), &goal)
        .unwrap();
    let Err(Refusal::Unmet(obligations)) = Schedule::new(1).settle(incomplete) else {
        panic!()
    };
    assert!(obligations[0].refusal.contains("table::app.t"));
    assert!(obligations[0].refusal.contains("module::app.f"));
}

#[test]
fn baseline_edges_end_the_old_incarnation_without_gating_the_reused_name() {
    let mut f = Fixture::new();
    f.before = f.after.clone();
    f.base = f.ids.clone();
    f.ids.tables = [("t_bbbbbb".parse().unwrap(), "app.t".parse().unwrap())].into();
    f.ids.columns = [("c_bbbbbb".parse().unwrap(), "app.t.x".parse().unwrap())].into();
    let old = dependencies(&[("app.f", &["table::app.t"])]);
    let table = f.after.tables[&"app.t".parse().unwrap()].clone();
    let changes = vec![
        Change::DropModule {
            id: "app.f".parse().unwrap(),
            kind: ModuleKind::View,
        },
        Change::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            detach_from: None,
        },
        Change::CreateTable {
            uid: "t_bbbbbb".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        },
        Change::CreateModule {
            id: "app.f".parse().unwrap(),
            module: Box::new(view()),
        },
    ];
    let removed = f
        .plan(changes.clone(), &old, &ObjectDeps::default())
        .unwrap();
    let edges = removed.annotation_edges().unwrap();
    assert_eq!(
        edges
            .iter()
            .map(|e| (e.before, e.after))
            .collect::<Vec<_>>(),
        [(0, 1)]
    );
    let replaced = f.plan(changes, &old, &old).unwrap();
    let edges = replaced.annotation_edges().unwrap();
    assert_eq!(
        edges
            .iter()
            .map(|e| (e.before, e.after))
            .collect::<Vec<_>>(),
        [(0, 1), (2, 3)]
    );
}

#[test]
fn explicit_cycles_are_named_with_every_reference_leg() {
    let f = Fixture::new();
    let cycle = dependencies(&[("app.f", &["table::app.t"]), ("table::app.t", &["app.f"])]);
    let text = f
        .plan(f.creates(), &ObjectDeps::default(), &cycle)
        .unwrap_err();
    for name in ["module::app.f", "table::app.t"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(text.contains("cycle"), "{text}");
}

#[test]
fn legacy_projection_is_hard_and_does_not_filter_equal_leaf_names() {
    let creates: Vec<_> = ["left.v", "right.v"]
        .into_iter()
        .map(|id| {
            PlannedChange::new(Change::CreateModule {
                id: id.parse().unwrap(),
                module: Box::new(view()),
            })
        })
        .collect();
    let ops = pbps_model::operation::Operations::of_plan(
        &creates,
        pbps_model::operation::ModuleEdits::Rebuilt,
    );
    let deps = [(
        "left.v".parse().unwrap(),
        ["right.v".parse().unwrap()].into(),
    )]
    .into();
    let edges = legacy(ops.as_slice(), &deps);
    assert_eq!(edges.len(), 1);
    assert_eq!(
        (edges[0].before, edges[0].after, edges[0].source),
        (1, 0, Source::Annotation)
    );
    let cs = ChangeSet { changes: creates };
    let schema = Schema {
        modules: [
            ("left.v".parse().unwrap(), view()),
            ("right.v".parse().unwrap(), view()),
        ]
        .into(),
        ..Default::default()
    };
    let generated =
        crate::order::generated::constraints(&cs, &schema, &deps, &pbps_dialect::MinimalDialect);
    assert!(
        generated
            .iter()
            .any(|e| e.source == Source::Annotation && (e.before, e.after) == (1, 0))
    );
}

mod continuations;

#[test]
fn every_reference_kind_uses_the_same_boundary_projection() {
    for owner in [
        "table::app.t",
        "column::app.t.x",
        "default::app.t.x",
        "generation::app.t.x",
        "sequence::app.t.x",
        "not_null::app.t.x",
        "type::app.d",
        "default::type::app.d",
        "check::type::app.d.positive",
        "not_null::type::app.d",
        "index::app.t.ix",
        "unique::app.t.uq",
        "primary_key::app.t",
        "foreign_key::app.t.fk",
        "check::app.t.ck",
        "policy::app.t.p",
        "row_security::app.t",
        "role::reader",
        "module::app.f(integer)",
        "index::module::app.v.ix",
    ] {
        let owner: ObjectRef = owner.parse().unwrap();
        let provider: ObjectRef = "module::app.provider".parse().unwrap();
        let mut index = Index::default();
        index.record(owner.clone(), 0, Transition::Produces);
        index.record(provider.clone(), 1, Transition::Produces);
        let annotation = Annotation {
            owner,
            provider: provider.clone(),
        };
        let edges = index
            .project(&[], std::slice::from_ref(&annotation))
            .unwrap();
        assert_eq!(
            edges
                .iter()
                .map(|e| (e.before, e.after, e.source))
                .collect::<Vec<_>>(),
            [(1, 0, Source::Annotation)]
        );
        index.boundaries.remove(&provider);
        index.standing.insert(provider.clone());
        index.record(provider, 1, Transition::Ends);
        assert_eq!(
            index.project(&[], &[annotation]),
            Err(ProjectionError::Missing(0))
        );
    }
}

mod survival;
