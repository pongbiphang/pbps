//! Baseline schema bindings must be released before a callee is rewritten.

#[cfg(test)]
use crate::certified_fixture::FixtureEmission as _;

use pbps_diff::{
    Side,
    order::{self, Baseline, Violation},
};
use pbps_model::operation::{
    Effects, ModuleEdits, ObjectKey, Operations, PlanOwners, Requirement, Snapshot,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, ChangeSet, CheckConstraint, Column, IdsFile, Module, ModuleId, ModuleKind,
    PlannedChange, RowKey, Schema, Table, TableName, Uid, UidKind, Value,
};
use pbps_mssql::Mssql;

fn id(name: &str) -> ModuleId {
    ModuleId::Named(TableName::new("app", name))
}
fn function(bound: bool, body: &str) -> Module {
    Module {
        kind: ModuleKind::Function,
        definition: format!(
            "(@x int) RETURNS int {} AS BEGIN RETURN {body} END",
            if bound { "WITH SCHEMABINDING" } else { "" }
        ),
        description: None,
        options: Default::default(),
    }
}
fn alter(name: &str, module: &Module) -> Change {
    Change::AlterModule {
        id: id(name),
        module: Box::new(module.clone()),
    }
}
fn plan(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}
fn ends(new_g: Module) -> (Schema, Schema) {
    let mut before = Schema::default();
    for (name, body) in [("f", "@x"), ("g", "app.f(@x)"), ("h", "@x + 10")] {
        before.modules.insert(id(name), function(true, body));
    }
    let mut after = before.clone();
    after.modules.insert(id("f"), function(true, "@x + 1"));
    after.modules.insert(id("g"), new_g);
    (before, after)
}
fn analysed(
    before: &Schema,
    after: &Schema,
    plan: &ChangeSet,
) -> (Operations, Vec<Effects>, Baseline) {
    let ids = IdsFile::default();
    let names = PlanNames::of(&plan.changes, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let ops = Operations::of_plan(&plan.changes, ModuleEdits::InPlace);
    let base = Snapshot {
        schema: before,
        ids: &ids,
    };
    let end = Snapshot {
        schema: after,
        ids: &ids,
    };
    let analysed = order::analyse(&ops, &owners, base, end, &Mssql);
    assert!(analysed.unowned.is_empty(), "{:?}", analysed.unowned);
    (ops, analysed.effects, Baseline::of(base, &owners))
}
fn settle(before: &Schema, after: &Schema, plan: ChangeSet) -> Result<ChangeSet, String> {
    let ids = IdsFile::default();
    order::before_rewrites(
        pbps_diff::order::propose(plan),
        Side {
            schema: before,
            ids: &ids,
        },
        Side {
            schema: after,
            ids: &ids,
        },
        &Mssql,
        &Default::default(),
    )
    .map(|plan| plan.changes().clone())
}
fn release() -> Requirement {
    Requirement::BeforeRewrite(ObjectKey::Module(id("f")))
}

#[test]
fn releasing_a_baseline_binding_precedes_the_old_callees_rewrite() {
    for new_g in [
        function(false, "app.f(@x)"),
        function(true, "app.h(@x)"),
        function(true, "@x"),
    ] {
        let (before, after) = ends(new_g);
        let backwards = plan(vec![
            alter("f", &after.modules[&id("f")]),
            alter("g", &after.modules[&id("g")]),
        ]);
        let (ops, fx, base) = analysed(&before, &after, &backwards);
        assert!(fx[1].requires.contains(&release()));
        let rejected = order::verify(ops.as_slice(), &fx, &base, &Default::default());
        assert!(
            rejected.iter().any(|v| matches!(v, Violation::Edge { .. })),
            "{rejected:?}"
        );
        let expected = plan(vec![
            backwards.changes[1].change.clone(),
            backwards.changes[0].change.clone(),
        ]);
        assert_eq!(settle(&before, &after, backwards).unwrap(), expected);
        let (ops, fx, base) = analysed(&before, &after, &expected);
        assert_eq!(
            order::verify(ops.as_slice(), &fx, &base, &Default::default()),
            []
        );
    }
}

#[test]
fn a_split_caller_releases_its_binding_at_teardown_and_rebinds_after_the_rewrite() {
    let (before, after) = ends(function(true, "app.f(@x) + 1"));
    let backwards = plan(vec![
        alter("f", &after.modules[&id("f")]),
        Change::DropModule {
            id: id("g"),
            kind: ModuleKind::Function,
        },
        Change::CreateModule {
            id: id("g"),
            module: Box::new(after.modules[&id("g")].clone()),
        },
    ]);
    let (ops, fx, base) = analysed(&before, &after, &backwards);
    assert!(fx[1].requires.contains(&release()));
    assert!(!fx[2].requires.contains(&release()));
    assert!(!order::verify(ops.as_slice(), &fx, &base, &Default::default()).is_empty());
    let expected = ChangeSet {
        changes: [1, 0, 2].map(|at| backwards.changes[at].clone()).to_vec(),
    };
    assert_eq!(settle(&before, &after, backwards).unwrap(), expected);
    let (ops, fx, base) = analysed(&before, &after, &expected);
    assert_eq!(
        order::verify(ops.as_slice(), &fx, &base, &Default::default()),
        []
    );
}

#[test]
fn only_an_actual_old_binding_that_the_step_releases_adds_before_rewrite() {
    for old in [
        function(false, "app.f(@x)"),
        function(true, "other.f(@x)"),
        function(true, "@x /* app.f(@x) */"),
        function(true, "@x + LEN(N'app.f(@x)')"),
        function(true, "app.g(@x)"),
    ] {
        let (mut before, after) = ends(function(false, "@x"));
        before.modules.insert(id("g"), old);
        let (_, fx, _) = analysed(
            &before,
            &after,
            &plan(vec![alter("g", &after.modules[&id("g")])]),
        );
        assert!(
            !fx[0]
                .requires
                .iter()
                .any(|r| matches!(r, Requirement::BeforeRewrite(_)))
        );
    }
    let (mut before, after) = ends(function(true, "app.f(@x) + 1"));
    let (_, fx, _) = analysed(
        &before,
        &after,
        &plan(vec![alter("g", &after.modules[&id("g")])]),
    );
    assert!(
        !fx[0].requires.contains(&release()),
        "a retained binding was not released"
    );
    before.modules.remove(&id("g"));
    let (_, fx, _) = analysed(
        &before,
        &after,
        &plan(vec![Change::CreateModule {
            id: id("g"),
            module: Box::new(after.modules[&id("g")].clone()),
        }]),
    );
    assert!(
        !fx[0].requires.contains(&release()),
        "a new caller has no baseline binding"
    );
}

#[test]
fn baseline_bindings_preserve_case_and_allow_only_unambiguous_folding() {
    for (call, neighbours, expected) in [
        ("app.F(@x)", true, "F"),
        ("APP.F(@x)", false, "f"),
        ("APP.F(@x)", true, "none"),
    ] {
        let (mut before, mut after) = ends(function(false, "@x"));
        before.modules.insert(id("g"), function(true, call));
        if neighbours {
            before.modules.insert(id("F"), function(true, "@x"));
            after.modules.insert(id("F"), function(true, "@x"));
        }
        let (_, fx, _) = analysed(
            &before,
            &after,
            &plan(vec![alter("g", &after.modules[&id("g")])]),
        );
        let released: Vec<_> = fx[0]
            .requires
            .iter()
            .filter_map(|r| {
                if let Requirement::BeforeRewrite(ObjectKey::Module(id)) = r {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            released,
            if expected == "none" {
                vec![]
            } else {
                vec![id(expected)]
            }
        );
    }
}

#[test]
fn binding_release_settle_keeps_unrelated_steps_outside_its_scope() {
    let (before, mut after) = ends(function(false, "@x"));
    after.modules.insert(id("m"), function(true, "@x + 1"));
    after.modules.insert(id("n"), function(true, "app.m(@x)"));
    let mut before = before;
    before.modules.insert(id("m"), function(true, "@x"));
    let changes = plan(vec![
        alter("f", &after.modules[&id("f")]),
        Change::CreateModule {
            id: id("n"),
            module: Box::new(after.modules[&id("n")].clone()),
        },
        alter("m", &after.modules[&id("m")]),
        alter("g", &after.modules[&id("g")]),
    ]);
    let expected = ChangeSet {
        changes: [1, 2, 3, 0].map(|at| changes.changes[at].clone()).to_vec(),
    };
    // This stage must neither repair nor refuse the unrelated m/n order.
    assert_eq!(settle(&before, &after, changes).unwrap(), expected);
}

#[test]
fn plans_without_before_rewrite_keep_identical_steps_sql_and_sealed_bytes() {
    let table = TableName::new("app", "t");
    let mut ids = IdsFile::default();
    ids.tables
        .insert(Uid::derived(UidKind::Table, "t", 0), table.clone());
    ids.columns
        .insert(Uid::derived(UidKind::Column, "t.id", 0), table.column("id"));
    let mut before = Schema::default();
    before.tables.insert(
        table.clone(),
        Table {
            columns: [("id".into(), Column::new("int".parse().unwrap()))].into(),
            ..Default::default()
        },
    );
    let mut after = before.clone();
    after.modules.insert(id("plain"), function(false, "@x"));
    let check = CheckConstraint {
        expression: "id > 0".into(),
    };
    after
        .tables
        .get_mut(&table)
        .unwrap()
        .checks
        .insert("positive".into(), check.clone());
    // Deliberately not class order: activating a general sort on this path
    // would change an approved plan even though it releases no binding.
    let original = plan(vec![
        Change::CreateModule {
            id: id("plain"),
            module: Box::new(after.modules[&id("plain")].clone()),
        },
        Change::AddCheck {
            table: table.clone(),
            name: "positive".into(),
            constraint: check,
        },
        Change::InsertRow {
            table,
            key_column: "id".into(),
            identity_key: false,
            key: RowKey::from("1"),
            row: [("id".into(), Value::Int(1))].into_iter().collect(),
            defaults: Default::default(),
            types: Default::default(),
        },
    ]);
    let result = order::before_rewrites(
        pbps_diff::order::propose(original.clone()),
        Side {
            schema: &before,
            ids: &ids,
        },
        Side {
            schema: &after,
            ids: &ids,
        },
        &Mssql,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(result, original);
    let sql = |plan: &ChangeSet| {
        plan.changes
            .iter()
            .flat_map(|p| Mssql.fixture_emit(&p.change, p.strategy).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(sql(&result), sql(&original));
    let seal = |changes| {
        pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Preview,
            "mssql",
            "2026-10-19T00:00:00Z",
            pbps_model::PlanBaseline {
                description: "baseline".into(),
                checksum: "unchanged".into(),
                database_collation: None,
                time_zone: None,
            },
            changes,
            ids.clone(),
        )
    };
    let (old, new) = (seal(original), seal(result.changes().clone()));
    assert_eq!(
        serde_json::to_vec(&old).unwrap(),
        serde_json::to_vec(&new).unwrap()
    );
    assert_eq!(old.wire_fingerprint(), new.wire_fingerprint());
}

#[test]
fn incompatible_binding_releases_are_refused_with_the_cycle_members_named() {
    let (before, mut after) = ends(function(true, "app.h(@x)"));
    after.modules.insert(id("h"), function(true, "app.f(@x)"));
    let changes = plan(
        ["f", "h", "g"]
            .map(|name| alter(name, &after.modules[&id(name)]))
            .to_vec(),
    );
    let refusal = settle(&before, &after, changes).unwrap_err();
    for text in ["wait on one another", "app.f", "app.g", "app.h"] {
        assert!(refusal.contains(text), "{refusal}");
    }
}

#[path = "support/binding_release_plan.rs"]
mod binding_release_plan;

#[test]
fn binding_releases_leave_computed_column_rebuilds_for_the_connected_stage() {
    for callee in ["z", "f"] {
        let (before, after, ids, input) = binding_release_plan::combined(callee);
        let expected = ChangeSet {
            changes: [1, 0, 2, 3, 4].map(|i| input.changes[i].clone()).to_vec(),
        };
        let result = order::before_rewrites(
            pbps_diff::order::propose(input),
            Side {
                schema: &before,
                ids: &ids,
            },
            Side {
                schema: &after,
                ids: &ids,
            },
            &Mssql,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            result, expected,
            "the connected stage owns the {callee}/computed pair"
        );
    }
}

#[test]
fn binding_release_dependents_follow_the_rewrite_transitively() {
    let (mut before, mut after) = ends(function(false, "@x"));
    before.modules.remove(&id("h"));
    for (name, body) in [
        ("h", "app.f(@x)"),
        ("i", "app.h(@x)"),
        ("u", "@x"),
        ("v", "@x"),
    ] {
        after.modules.insert(id(name), function(true, body));
    }
    let create = |name: &str| Change::CreateModule {
        id: id(name),
        module: Box::new(after.modules[&id(name)].clone()),
    };
    let changes = plan(vec![
        alter("f", &after.modules[&id("f")]),
        create("h"),
        create("u"),
        create("i"),
        create("v"),
        alter("g", &after.modules[&id("g")]),
    ]);
    let expected = ChangeSet {
        changes: [2, 4, 5, 0, 1, 3]
            .map(|i| changes.changes[i].clone())
            .to_vec(),
    };
    assert_eq!(settle(&before, &after, changes).unwrap(), expected);
    let (ops, effects, base) = analysed(&before, &after, &expected);
    assert_eq!(
        order::verify(ops.as_slice(), &effects, &base, &Default::default()),
        []
    );
}

#[cfg(test)]
mod certified_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-diff/tests/support/emission.rs"
    ));
}
