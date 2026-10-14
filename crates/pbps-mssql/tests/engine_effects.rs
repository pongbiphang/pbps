//! SQL Server's engine effects, through the operation-level verifier (#1954).

use indexmap::IndexMap;
use pbps_diff::order::{Baseline, Violation, analyse};
use pbps_model::operation::{ModuleEdits, ObjectKey, Operations, PartKey, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, Column, ComputedColumn, IdsFile, Module, ModuleId, ModuleKind, PlannedChange, Schema,
    Table, TableName, Uid, UidKind,
};
use pbps_mssql::Mssql;

fn t() -> TableName {
    TableName::new("dbo", "t")
}

fn setup() -> (Schema, IdsFile, ModuleId, Module) {
    let id = ModuleId::Named(TableName::new("dbo", "f"));
    let module = Module {
        kind: ModuleKind::Function,
        definition: "CREATE FUNCTION dbo.f(@a int) RETURNS int AS BEGIN RETURN @a END".into(),
        description: None,
    };
    let mut table = Table {
        columns: IndexMap::from([("a".to_owned(), Column::new("int".parse().unwrap()))]),
        ..Table::default()
    };
    table.computed.insert(
        "c".into(),
        ComputedColumn {
            expression: "dbo.f(a)".into(),
            persisted: false,
            not_null: false,
        },
    );
    let mut schema = Schema::default();
    schema.tables.insert(t(), table);
    schema.modules.insert(id.clone(), module.clone());
    let mut ids = IdsFile::default();
    ids.tables.insert(Uid::derived(UidKind::Table, "t", 0), t());
    ids.columns
        .insert(Uid::derived(UidKind::Column, "t.a", 0), t().column("a"));
    (schema, ids, id, module)
}

fn violations(logical: Vec<Change>, order: &[usize]) -> Vec<Violation> {
    let (schema, ids, _, _) = setup();
    let logical: Vec<PlannedChange> = logical.into_iter().map(PlannedChange::new).collect();
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&logical, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let snapshot = Snapshot {
        schema: &schema,
        ids: &ids,
    };
    let baseline = Baseline::of(snapshot, &owners);
    let shadowed = analyse(&ops, &owners, snapshot, snapshot, &Mssql);
    assert!(shadowed.unowned.is_empty(), "{:?}", shadowed.unowned);
    let ordered = ops.reordered(order);
    let effects: Vec<_> = order.iter().map(|&i| shadowed.effects[i].clone()).collect();
    pbps_diff::order::verify(ordered.as_slice(), &effects, &baseline, &Default::default())
}

#[test]
fn altering_a_function_while_a_computed_column_calling_it_stands_is_refused_by_name() {
    let (_, _, id, module) = setup();
    let computed = || ComputedColumn {
        expression: "dbo.f(a)".into(),
        persisted: false,
        not_null: false,
    };
    let plan = || {
        vec![
            Change::DropComputedColumn {
                table: t(),
                name: "c".into(),
                computed: computed(),
            },
            Change::AlterModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            },
            Change::AddComputedColumn {
                table: t(),
                name: "c".into(),
                computed: computed(),
            },
        ]
    };
    // The computed column out of the way, the function altered, the column
    // back: SQL Server accepts it.
    assert_eq!(violations(plan(), &[0, 1, 2]), []);
    // The alter first, while the column stands (3729).
    let refused = violations(plan(), &[1, 0, 2]);
    assert!(
        refused.iter().any(|v| matches!(
            v,
            Violation::StillPresent {
                key: ObjectKey::Part { part: PartKey::Computed(name), .. },
                operation,
            } if name == "c" && operation.source() == 1
        )),
        "{refused:?}"
    );
    // And the column back before the alter would call the old function.
    let late = violations(plan(), &[0, 2, 1]);
    assert!(!late.is_empty(), "{late:?}");
}

#[test]
fn rows_written_while_a_foreign_key_is_rebuilt_do_not_wait_for_it() {
    use pbps_model::{
        ForeignKey, PlannedChange as P, PrimaryKey, ReferentialAction, Row, RowKey, Value,
    };
    let table = |with_key: bool| {
        let mut table = Table {
            columns: IndexMap::from([
                ("id".to_owned(), Column::new("int".parse().unwrap())),
                ("pid".to_owned(), Column::new("int".parse().unwrap())),
            ]),
            primary_key: Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            }),
            ..Table::default()
        };
        if with_key {
            table.foreign_keys.insert(
                "fk".into(),
                ForeignKey {
                    columns: vec!["pid".into()],
                    references_table: t(),
                    references_columns: vec!["id".into()],
                    on_delete: ReferentialAction::default(),
                    on_update: ReferentialAction::default(),
                },
            );
        }
        table
    };
    let (mut schema, mut ids) = (Schema::default(), IdsFile::default());
    schema.tables.insert(t(), table(true));
    ids.tables.insert(Uid::derived(UidKind::Table, "t", 0), t());
    for c in ["id", "pid"] {
        ids.columns.insert(
            Uid::derived(UidKind::Column, &format!("t.{c}"), 0),
            t().column(c),
        );
    }
    let insert = |id: i64, parent: Option<i64>| {
        P::new(Change::InsertRow {
            table: t(),
            key_column: "id".into(),
            identity_key: false,
            key: RowKey::from(id.to_string().as_str()),
            row: [
                ("id".to_owned(), Value::Int(id)),
                ("pid".to_owned(), parent.map_or(Value::Null, Value::Int)),
            ]
            .into_iter()
            .collect::<Row>(),
            defaults: Default::default(),
            types: Default::default(),
        })
    };
    let key = table(true).foreign_keys["fk"].clone();
    let verdict = |rebuilt: bool| {
        let mut logical = vec![insert(1, Some(2)), insert(2, None)];
        if rebuilt {
            logical.insert(
                0,
                P::new(Change::DropForeignKey {
                    table: t(),
                    name: "fk".into(),
                }),
            );
            logical.push(P::new(Change::AddForeignKey {
                table: t(),
                name: "fk".into(),
                constraint: Box::new(key.clone()),
            }));
        }
        let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
        let names = PlanNames::of(&logical, &ids).unwrap();
        let owners = PlanOwners::new(names, &ids, &ids);
        let snapshot = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        let baseline = Baseline::of(snapshot, &owners);
        let shadowed = analyse(&ops, &owners, snapshot, snapshot, &Mssql);
        pbps_diff::order::verify(
            ops.as_slice(),
            &shadowed.effects,
            &baseline,
            &Default::default(),
        )
    };
    // Rebuilt around the rows: 1 -> 2 is written before 2 exists, and fine.
    assert_eq!(verdict(true), []);
    // Negative: a key that stands throughout wants its parent row first.
    assert!(!verdict(false).is_empty());
}
