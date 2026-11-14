use super::*;
use pbps_model::object_ref::{ObjectDeps, ObjectEdges, ObjectRef};
use pbps_model::{
    Change, ChangeSet, Column, IdsFile, Module, ModuleKind, PlanBaseline, PlanOrigin,
    PlannedChange, SavedPlan, StateKind, StateSnapshot, Table,
};

fn reference(s: &str) -> ObjectRef {
    s.parse().unwrap()
}

fn edges(rows: &[(&str, &[&str])]) -> ObjectEdges {
    ObjectEdges::from_deps(&ObjectDeps(
        rows.iter()
            .map(|(owner, targets)| {
                (
                    reference(owner),
                    targets.iter().map(|t| reference(t)).collect(),
                )
            })
            .collect(),
    ))
    .unwrap()
}

/// A schema declaring the given views, and one table with a column, a domain
/// and a role: one endpoint of each kind an annotation can name.
fn schema(views: &[&str], with_the_rest: bool) -> Schema {
    let mut schema = Schema::default();
    for view in views {
        schema.modules.insert(
            view.parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            },
        );
    }
    if with_the_rest {
        let mut table = Table::default();
        table
            .columns
            .insert("x".into(), Column::new("integer".parse().unwrap()));
        schema.tables.insert("app.t".parse().unwrap(), table);
        schema.types.insert(
            "app.d".parse().unwrap(),
            pbps_model::ManagedType::Domain {
                base: "integer".parse().unwrap(),
                not_null: false,
                default: None,
                checks: Default::default(),
                collation: None,
            },
        );
        schema.roles.insert("reader".into(), Default::default());
    }
    schema
}

fn plan(deps: ObjectEdges, dropping: &[&str]) -> SavedPlan {
    let mut plan = SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "2026-11-12T00:00:00Z",
        PlanBaseline {
            description: "d".into(),
            checksum: "c".into(),
            database_collation: None,
            time_zone: None,
        },
        ChangeSet {
            changes: dropping
                .iter()
                .map(|id| {
                    PlannedChange::new(Change::DropModule {
                        id: id.parse().unwrap(),
                        kind: ModuleKind::View,
                    })
                })
                .collect(),
        },
        IdsFile::default(),
    );
    plan.object_deps = deps;
    plan
}

fn recorded(deps: ObjectEdges) -> StateSnapshot {
    let mut state = StateSnapshot::new(
        StateKind::Apply,
        Schema::default(),
        IdsFile::default(),
        "operator",
    );
    state.object_deps = deps;
    state
}

#[test]
fn a_module_being_dropped_keeps_its_recorded_edges_in_a_checkpoint() {
    // `app.old` has no declaration left, so the plan holds no edge for it; the
    // recorded state does, and the module is still in the checkpoint's schema.
    let kept = dependency_hints_for_schema(
        &recorded(edges(&[("module::app.old", &["module::app.base"])])),
        &plan(ObjectEdges::default(), &["app.old"]),
        &schema(&["app.old", "app.base"], false),
    );
    assert_eq!(kept, edges(&[("module::app.old", &["module::app.base"])]));
    // Negative: once the drop has run, the module is not in the schema and the
    // edge is not claimed.
    let gone = dependency_hints_for_schema(
        &recorded(edges(&[("module::app.old", &["module::app.base"])])),
        &plan(ObjectEdges::default(), &["app.old"]),
        &schema(&["app.base"], false),
    );
    assert!(gone.is_empty(), "{gone:?}");
}

#[test]
fn a_surviving_owner_takes_the_plans_edges_and_not_the_recorded_ones() {
    let views = schema(&["app.v", "app.w", "app.base"], false);
    let state = recorded(edges(&[("module::app.v", &["module::app.w"])]));
    // The declaration dropped an edge and added another: the plan is the truth.
    let kept = dependency_hints_for_schema(
        &state,
        &plan(edges(&[("module::app.v", &["module::app.base"])]), &[]),
        &views,
    );
    assert_eq!(kept, edges(&[("module::app.v", &["module::app.base"])]));
    // Intentional removal: no plan edge means no edge, whatever was recorded.
    let removed = dependency_hints_for_schema(&state, &plan(ObjectEdges::default(), &[]), &views);
    assert!(removed.is_empty(), "{removed:?}");
    // Negative: a dropped owner does not take the plan's edges instead.
    let dropped = dependency_hints_for_schema(
        &state,
        &plan(
            edges(&[("module::app.v", &["module::app.base"])]),
            &["app.v"],
        ),
        &views,
    );
    assert_eq!(dropped, edges(&[("module::app.v", &["module::app.w"])]));
}

#[test]
fn an_edge_is_kept_only_when_the_schema_declares_both_endpoints_of_every_kind() {
    let wanted = edges(&[
        (
            "module::app.v",
            &[
                "module::app.w",
                "table::app.t",
                "column::app.t.x",
                "type::app.d",
                "role::reader",
            ],
        ),
        ("table::app.t", &["module::app.v"]),
    ]);
    let all = schema(&["app.v", "app.w"], true);
    let state = recorded(ObjectEdges::default());
    let kept = dependency_hints_for_schema(&state, &plan(wanted.clone(), &[]), &all);
    assert_eq!(kept, wanted);
    // Each endpoint kind, missing in turn, takes its edge with it and no other.
    // A column goes with its table, so removing the table removes both
    // endpoints; every other kind removes only itself.
    let missing: [(&[&str], Schema); 5] = [
        (&["module::app.w"], schema(&["app.v"], true)),
        (&["table::app.t", "column::app.t.x"], {
            let mut s = all.clone();
            s.tables.clear();
            s
        }),
        (&["column::app.t.x"], {
            let mut s = all.clone();
            s.tables.values_mut().next().unwrap().columns.clear();
            s
        }),
        (&["type::app.d"], {
            let mut s = all.clone();
            s.types.clear();
            s
        }),
        (&["role::reader"], {
            let mut s = all.clone();
            s.roles.clear();
            s
        }),
    ];
    for (gone, without) in missing {
        let kept = dependency_hints_for_schema(&state, &plan(wanted.clone(), &[]), &without);
        let touches = |edge: &pbps_model::object_ref::ObjectEdge| {
            gone.iter()
                .any(|g| edge.owner == reference(g) || edge.target == reference(g))
        };
        let expected = ObjectEdges::from_edges(wanted.iter().filter(|e| !touches(e)).cloned());
        assert!(expected.len() < wanted.len(), "{gone:?} is in an edge");
        assert_eq!(kept, expected, "{gone:?}");
    }
    // A checkpoint with nothing recorded and nothing planned holds nothing.
    assert!(
        dependency_hints_for_schema(&state, &plan(ObjectEdges::default(), &[]), &all).is_empty()
    );
}
