use super::*;
use crate::order::{Baseline, Checks, graph, propose};
use pbps_model::operation::{Effects, ModuleEdits, Operations};
use pbps_model::*;

fn policy(table: &str, name: &str, origin: RowSecurityOrigin) -> Change {
    Change::CreatePolicy {
        table: table.parse().unwrap(),
        name: name.into(),
        policy: RowPolicy {
            using: Some("app.allowed(id)".into()),
            ..Default::default()
        },
        origin,
    }
}

fn drop_policy() -> Change {
    Change::DropPolicy {
        table: TableName::new("app", "t"),
        name: "p".into(),
        policy: RowPolicy {
            using: Some("app.allowed(id)".into()),
            ..Default::default()
        },
    }
}

fn flags(table: &str, origin: RowSecurityOrigin) -> Change {
    Change::SetRowSecurity {
        uid: if table == "app.other" {
            "t_117302"
        } else {
            "t_117301"
        }
        .parse()
        .unwrap(),
        table: table.parse().unwrap(),
        from: RowSecurity::default(),
        to: RowSecurity {
            enabled: true,
            forced: true,
        },
        origin,
    }
}

// Start with the dependent first: the differ's sort cannot mask a missing
// phase edge. Exercise both the CLI helper and the resolver's graph entry.
fn settles_before(first: Change, second: Change) {
    let reversed = ChangeSet {
        changes: vec![
            PlannedChange::new(second.clone()),
            PlannedChange::new(first.clone()),
        ],
    };
    let expected = vec![first, second];
    let settled = settle(propose(reversed.clone()), Schedule::new(2), &[]).unwrap();
    assert_eq!(
        settled
            .changes
            .iter()
            .map(|p| p.change.clone())
            .collect::<Vec<_>>(),
        expected
    );
    let operations = Operations::of_plan(&reversed.changes, ModuleEdits::Rebuilt);
    // Phase edges must exist without expression bindings supplied by effects.
    let graph = graph(
        operations.as_slice(),
        &vec![Effects::default(); operations.len()],
        &Baseline::empty(),
        &ModuleDeps::new(),
        Checks::ALL,
    );
    let settled = Schedule::of_graph(&graph, operations.as_slice())
        .settle(propose(reversed))
        .unwrap()
        .into_planning();
    assert_eq!(
        settled
            .changes
            .iter()
            .map(|p| p.change.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn policy_drop_only_plans_release_modules_and_columns_before_teardown() {
    settles_before(
        drop_policy(),
        Change::DropModule {
            id: "app.allowed(integer)".parse().unwrap(),
            kind: ModuleKind::Function,
        },
    );
    settles_before(
        drop_policy(),
        Change::DropColumn {
            uid: "c_117301".parse().unwrap(),
            column: TableName::new("app", "t").column("id"),
        },
    );
    settles_before(
        drop_policy(),
        Change::AlterColumnType {
            uid: "c_117301".parse().unwrap(),
            column: TableName::new("app", "t").column("id"),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        },
    );
}

#[test]
fn policy_setup_follows_modules_and_columns_for_both_origins() {
    let module = Box::new(Module {
        kind: ModuleKind::Function,
        definition: "SELECT true".into(),
        description: None,
        options: Default::default(),
    });
    let suppliers = [
        Change::CreateModule {
            id: "app.allowed(integer)".parse().unwrap(),
            module: module.clone(),
        },
        Change::AlterModule {
            id: "app.allowed(integer)".parse().unwrap(),
            module,
        },
        Change::PublicExecution {
            routine: "app.allowed(integer)".parse().unwrap(),
            access: PublicAccess::Revoked,
            origin: RoutineOrigin::Rebuilt,
        },
        Change::AddColumn {
            uid: "c_117301".parse().unwrap(),
            table: TableName::new("app", "t"),
            name: "id".into(),
            column: Box::new(Column::new("integer".parse().unwrap())),
        },
    ];
    for origin in [RowSecurityOrigin::Created, RowSecurityOrigin::Standing] {
        for supplier in &suppliers {
            settles_before(supplier.clone(), policy("app.t", "p", origin));
            settles_before(supplier.clone(), flags("app.t", origin));
        }
    }
}

#[test]
fn replacement_teardown_and_row_deletes_finish_before_policy_flags() {
    let installed = policy("app.t", "p", RowSecurityOrigin::Standing);
    let changed_flags = flags("app.t", RowSecurityOrigin::Standing);
    settles_before(drop_policy(), installed.clone());
    settles_before(drop_policy(), changed_flags.clone());
    settles_before(installed, changed_flags.clone());
    settles_before(
        Change::DeleteRow {
            table: TableName::new("app", "other"),
            key_column: "id".into(),
            key: "1".into(),
            cause: pbps_model::change::DeleteCause::Undeclared,
            row: [("id".into(), Cell::Value(Value::Int(1)))].into(),
            types: [("id".into(), "integer".parse().unwrap())].into(),
            dropped: Default::default(),
            after_types: Default::default(),
            key_type: Some("integer".parse().unwrap()),
        },
        changed_flags,
    );
}

#[test]
fn independent_policy_names_and_separate_table_flags_have_no_phase_edge() {
    for pair in [
        [
            policy("app.t", "p", RowSecurityOrigin::Standing),
            policy("app.t", "q", RowSecurityOrigin::Standing),
        ],
        [
            policy("app.t", "p", RowSecurityOrigin::Standing),
            policy("app.other", "p", RowSecurityOrigin::Standing),
        ],
        [
            flags("app.t", RowSecurityOrigin::Standing),
            flags("app.other", RowSecurityOrigin::Standing),
        ],
    ] {
        assert!(edges(pair.iter()).is_empty());
        for reversed in [false, true] {
            let mut changes = pair.clone();
            if reversed {
                changes.reverse();
            }
            let plan = propose(ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            });
            assert_eq!(settle(plan.clone(), Schedule::new(2), &[]).unwrap(), plan);
        }
    }
}
