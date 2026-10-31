//! DEFAULT rebuild obligations use the shared weave and release-gap effects.
use pbps_diff::order::catalog::{Dependent, Holds, Part};
use pbps_diff::{
    Side,
    order::{
        self,
        deployment::{
            Sources,
            schedule::{Declarations, Facts},
        },
    },
};
use pbps_model::operation::{
    ModuleEdits, ObjectKey, Operations, PlanOwners, Requirement, Snapshot,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::resolver::{OrderEdge, OrderReason};
use pbps_model::{
    Change, ChangeSet, IdsFile, Module, ModuleId, ModuleKind, PlannedChange, Schema, Table,
    TableName,
};
use pbps_mssql::Mssql;
use std::collections::BTreeMap;

fn fixture(
    both: bool,
) -> (
    Schema,
    Schema,
    IdsFile,
    ChangeSet,
    BTreeMap<ModuleId, Vec<Dependent>>,
) {
    let mut before = Schema::default();
    let mut ids = IdsFile::default();
    for (n, name) in [(1, "source"), (2, "t")] {
        let table = TableName::new("app", name);
        let mut shape = Table::default();
        let mut column = pbps_model::Column::new("int".parse().unwrap());
        if n == 2 {
            column.value_source = pbps_model::ValueSource::Default("app.f()".into());
        }
        shape.columns.insert("old".into(), column);
        before.tables.insert(table.clone(), shape);
        ids.tables
            .insert(format!("t_{n:06}").parse().unwrap(), table.clone());
        ids.columns
            .insert(format!("c_{n:06}").parse().unwrap(), table.column("old"));
    }
    let root = ModuleId::Named(TableName::new("app", "f"));
    let module = Module { kind: ModuleKind::Function,
        definition: "() RETURNS int WITH SCHEMABINDING AS BEGIN DECLARE @n int; SELECT @n=MAX(old) FROM app.source; RETURN ISNULL(@n,0); END".into(),
        description: None, options: Default::default() };
    before.modules.insert(root.clone(), module.clone());
    let mut after = before.clone();
    let mut changes = vec![PlannedChange::new(Change::DropModule {
        id: root.clone(),
        kind: ModuleKind::Function,
    })];
    for n in 1..=if both { 2 } else { 1 } {
        let uid: pbps_model::Uid = format!("c_{n:06}").parse().unwrap();
        let column = ids.columns[&uid].clone();
        after
            .tables
            .get_mut(&column.table)
            .unwrap()
            .columns
            .get_mut("old")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        let change: Change = serde_json::from_value(serde_json::json!({"op":"alter_column_type", "uid":uid, "column":column,"from":"int","to":"bigint","from_nullable":true,"to_nullable":true})).unwrap();
        changes.push(PlannedChange::new(change));
    }
    changes.push(PlannedChange::new(Change::CreateModule {
        id: root.clone(),
        module: Box::new(module),
    }));
    let found = BTreeMap::from([(
        root,
        vec![Dependent {
            described: "default for app.t.old".into(),
            holds: Holds::TablePart {
                table: TableName::new("app", "t"),
                part: Part::Default("old".into()),
            },
        }],
    )]);
    (before, after, ids, ChangeSet { changes }, found)
}

#[test]
fn sql_server_default_holders_have_catalog_proof_and_a_retype_release_gap() {
    for both in [false, true] {
        let (before, after, ids, changes, found) = fixture(both);
        let declared = Declarations {
            schema: &after,
            sources: Sources {
                before: Side {
                    schema: &before,
                    ids: &ids,
                },
                after: &ids,
            },
            module_deps: &Default::default(),
        };
        let done = order::deployment::schedule::schedule(
            order::propose(changes),
            &declared,
            &Facts::default().with_module_dependents(&found),
            &Mssql,
        )
        .unwrap();
        let plan = done.changes.changes();
        let at =
            |pred: fn(&Change) -> bool| plan.changes.iter().position(|p| pred(&p.change)).unwrap();
        let release = at(|c| matches!(c, Change::AlterColumnDefault { to: None, .. }));
        let drop = at(|c| matches!(c, Change::DropModule { .. }));
        let create = at(|c| matches!(c, Change::CreateModule { .. }));
        let restore = at(|c| matches!(c, Change::AlterColumnDefault { to: Some(_), .. }));
        assert_eq!(done.added, 2);
        assert!(release < drop && drop < create && create < restore);
        for (before, after, reason) in [
            (release, drop, OrderReason::Structural),
            (create, restore, OrderReason::Restoration),
        ] {
            assert!(
                done.changes.proof().edges().contains(&OrderEdge {
                    before,
                    after,
                    reason
                }),
                "{:?}",
                done.changes.proof()
            );
        }
        done.changes.proof().validate(plan).unwrap();
        let names = PlanNames::of(&plan.changes, &ids).unwrap();
        let owners = PlanOwners::new(names, &ids, &ids);
        let ops = Operations::of_plan(&plan.changes, ModuleEdits::InPlace);
        let base = Snapshot {
            schema: &before,
            ids: &ids,
        };
        let end = Snapshot {
            schema: &after,
            ids: &ids,
        };
        let analysis = order::analyse(&ops, &owners, base, end, &Mssql);
        assert!(analysis.unowned.is_empty(), "{:?}", analysis.unowned);
        let baseline = order::Baseline::of(base, &owners);
        assert_eq!(
            order::verify(
                ops.as_slice(),
                &analysis.effects,
                &baseline,
                &Default::default()
            ),
            []
        );
        if both {
            let retype = at(
                |c| matches!(c,Change::AlterColumnType { column,.. } if column.table.name == "t"),
            );
            assert!(release < retype && retype < restore);
            assert!(
                analysis.effects[retype]
                    .requires
                    .iter()
                    .any(|r| matches!(r, Requirement::ReleaseGap(ObjectKey::Default(_))))
            );
            let mut reversed = plan.clone();
            reversed.changes.swap(release, retype);
            let names = PlanNames::of(&reversed.changes, &ids).unwrap();
            let owners = PlanOwners::new(names, &ids, &ids);
            let ops = Operations::of_plan(&reversed.changes, ModuleEdits::InPlace);
            let a = order::analyse(&ops, &owners, base, end, &Mssql);
            assert!(
                !order::verify(
                    ops.as_slice(),
                    &a.effects,
                    &order::Baseline::of(base, &owners),
                    &Default::default()
                )
                .is_empty()
            );
        }
    }
}

#[test]
fn an_existing_default_change_is_split_once_and_no_facts_add_no_release() {
    let (before, mut after, ids, mut changes, found) = fixture(true);
    after
        .tables
        .get_mut(&TableName::new("app", "t"))
        .unwrap()
        .columns
        .get_mut("old")
        .unwrap()
        .value_source = pbps_model::ValueSource::Default("app.f() + 1".into());
    let default: Change = serde_json::from_value(serde_json::json!({"op":"alter_column_default", "uid":"c_000002", "column":"app.t.old", "from":"app.f()", "to":"app.f() + 1"})).unwrap();
    changes.changes.push(PlannedChange::new(default));
    let declared = Declarations {
        schema: &after,
        sources: Sources {
            before: Side {
                schema: &before,
                ids: &ids,
            },
            after: &ids,
        },
        module_deps: &Default::default(),
    };
    let done = order::deployment::schedule::schedule(
        order::propose(changes),
        &declared,
        &Facts::default().with_module_dependents(&found),
        &Mssql,
    )
    .unwrap();
    assert_eq!(
        done.changes
            .changes()
            .changes
            .iter()
            .filter(|p| matches!(p.change, Change::AlterColumnDefault { .. }))
            .count(),
        2
    );
    let (before, mut after, ids, mut changes, _) = fixture(true);
    after.tables.insert(
        TableName::new("app", "source"),
        before.tables[&TableName::new("app", "source")].clone(),
    );
    changes.changes.retain(
        |p| matches!(&p.change,Change::AlterColumnType { column,.. } if column.table.name == "t"),
    );
    let expected = changes.clone();
    let declared = Declarations {
        schema: &after,
        sources: Sources {
            before: Side {
                schema: &before,
                ids: &ids,
            },
            after: &ids,
        },
        module_deps: &Default::default(),
    };
    let done = order::deployment::schedule::schedule(
        order::propose(changes),
        &declared,
        &Facts::default().with_module_dependents(&BTreeMap::new()),
        &Mssql,
    )
    .unwrap();
    assert_eq!(done.changes.changes(), &expected);
    assert_eq!(done.added, 0);
}
