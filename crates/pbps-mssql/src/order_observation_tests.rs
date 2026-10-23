//! DEC-1954.6 observations must not become refusals in active order consumers.
use super::Mssql;
use pbps_diff::order::{self, Baseline, Checks, graph, propose};
use pbps_model::operation::{
    ModuleEdits, ObjectKey, Operations, PartKey, PlanOwners, Requirement, Snapshot,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::*;

#[derive(Clone, Copy)]
enum Shape {
    Check,
    Computed,
    UnmanagedGrant,
}

fn assert_observational(shape: Shape) {
    let table: TableName = "dbo.t".parse().unwrap();
    let tid: Uid = "t_aaaaaa".parse().unwrap();
    let cid: Uid = "c_aaaaaa".parse().unwrap();
    let mut before = Schema::default();
    let mut t = Table::default();
    t.columns
        .insert("a".into(), Column::new("int".parse().unwrap()));
    match shape {
        Shape::Check => {
            t.checks.insert(
                "ck".into(),
                CheckConstraint {
                    expression: "a > 0".into(),
                },
            );
        }
        // A case-folded lexical blocker can stand although the connected
        // catalog's case-sensitive binding permits this retype (#1460).
        Shape::Computed => {
            t.columns
                .insert("A".into(), Column::new("int".parse().unwrap()));
            t.computed.insert(
                "b".into(),
                ComputedColumn {
                    expression: "A + 1".into(),
                    persisted: false,
                    not_null: false,
                },
            );
        }
        Shape::UnmanagedGrant => {}
    }
    before.tables.insert(table.clone(), t);
    let function = |body: &str, bound: bool| Module {
        kind: ModuleKind::Function,
        definition: format!(
            "(@x int) RETURNS int {} AS BEGIN RETURN {body} END",
            if bound { "WITH SCHEMABINDING" } else { "" }
        ),
        description: None,
    };
    let f: ModuleId = "dbo.f".parse().unwrap();
    let g: ModuleId = "dbo.g".parse().unwrap();
    before.modules.insert(f.clone(), function("@x", true));
    before
        .modules
        .insert(g.clone(), function("dbo.f(@x)", true));
    let mut ids = IdsFile::default();
    ids.tables.insert(tid, table.clone());
    ids.columns.insert(cid.clone(), table.column("a"));
    if matches!(shape, Shape::Computed) {
        ids.columns
            .insert("c_bbbbbb".parse().unwrap(), table.column("A"));
    }
    let mut after = before.clone();
    after
        .tables
        .get_mut(&table)
        .unwrap()
        .columns
        .get_mut("a")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    after.modules.insert(f.clone(), function("@x + 1", true));
    after.modules.insert(g.clone(), function("@x", false));
    let mut changes = vec![
        PlannedChange::new(Change::AlterModule {
            id: f.clone(),
            module: Box::new(after.modules[&f].clone()),
        }),
        PlannedChange::new(Change::AlterModule {
            id: g.clone(),
            module: Box::new(after.modules[&g].clone()),
        }),
        PlannedChange::new(Change::AlterColumnType {
            uid: cid,
            column: table.column("a"),
            from: "int".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
        }),
    ];
    if matches!(shape, Shape::UnmanagedGrant) {
        changes.push(PlannedChange::new(Change::Grant {
            role: "external_reader".into(),
            target: GrantTarget::Object("dbo.unmanaged_v".parse().unwrap()),
            permissions: [Permission::Select].into(),
        }));
    }
    let operations = Operations::of_plan(&changes, ModuleEdits::InPlace);
    let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &ids, &ids);
    let base = Snapshot {
        schema: &before,
        ids: &ids,
    };
    let end = Snapshot {
        schema: &after,
        ids: &ids,
    };
    let analysed = order::analyse(&operations, &owners, base, end, &Mssql);
    assert!(analysed.unowned.is_empty(), "{:?}", analysed.unowned);
    assert!(analysed.effects.iter().all(|fx| fx.symbolic.is_empty()));
    assert!(analysed.effects.iter().any(|fx| {
        fx.requires
            .iter()
            .any(|r| matches!(r, Requirement::BeforeRewrite(_)))
    }));
    let graph = graph(
        operations.as_slice(),
        &analysed.effects,
        &Baseline::of(base, &owners),
        &ModuleDeps::new(),
        Checks::ALL,
    );
    assert!(
        graph.unmet.iter().any(|u| match (&shape, &u.requirement) {
            (
                Shape::Check,
                Requirement::Absent(ObjectKey::Part {
                    part: PartKey::Check(name),
                    ..
                }),
            ) => name.raw() == "ck",
            (
                Shape::Computed,
                Requirement::Absent(ObjectKey::Part {
                    part: PartKey::Computed(name),
                    ..
                }),
            ) => name.raw() == "b",
            (Shape::UnmanagedGrant, Requirement::Is(ObjectKey::Module(id), _)) =>
                id == &"dbo.unmanaged_v".parse().unwrap(),
            _ => false,
        }),
        "{:?}",
        graph.unmet
    );
    let plan = propose(ChangeSet { changes });
    let result = order::before_rewrites(
        plan,
        pbps_diff::Side {
            schema: &before,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &after,
            ids: &ids,
        },
        &Mssql,
        &ModuleDeps::new(),
    )
    .unwrap();
    let at = |id: &ModuleId| {
        result
            .changes
            .iter()
            .position(
                |p| matches!(&p.change, Change::AlterModule { id: changed, .. } if changed == id),
            )
            .unwrap()
    };
    assert!(at(&g) < at(&f), "the real release edge still orders");
    let schedule = order::row_security::release_schedule(
        &result,
        pbps_diff::Side {
            schema: &before,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &after,
            ids: &ids,
        },
        &ModuleDeps::new(),
        &Mssql,
    )
    .unwrap();
    // Observational facts do not suppress real ordering failures. Conflicting
    // catalog constraints over the same release must still refuse a cycle.
    let mut cyclic = schedule.clone();
    for (before, after) in [(at(&f), at(&g)), (at(&g), at(&f))] {
        cyclic.edge(
            before,
            after,
            order::Source::Catalog,
            pbps_model::resolver::OrderReason::Binding,
        );
    }
    assert!(matches!(
        cyclic.settle(result.clone()),
        Err(order::Refusal::Cycle { .. })
    ));
    let settled = schedule.settle(result).unwrap();
    settled.proof.validate(&settled.changes).unwrap();
}

#[test]
fn a_check_rebuilt_around_a_retype_stays_observational_in_active_schedules() {
    assert_observational(Shape::Check);
}
#[test]
fn a_standing_computed_column_stays_observational_in_active_schedules() {
    assert_observational(Shape::Computed);
}
#[test]
fn a_grant_on_an_unmanaged_view_stays_observational_in_active_schedules() {
    assert_observational(Shape::UnmanagedGrant);
}
