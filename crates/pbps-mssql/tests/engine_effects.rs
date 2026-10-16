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
    let (schema, _, _, _) = setup();
    violations_ending_in(&schema, logical, order)
}

/// [`violations`] for a plan that ends in `after` rather than where it began.
fn violations_ending_in(after: &Schema, logical: Vec<Change>, order: &[usize]) -> Vec<Violation> {
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
    let ending = Snapshot {
        schema: after,
        ids: &ids,
    };
    let shadowed = analyse(&ops, &owners, snapshot, ending, &Mssql);
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

#[test]
fn a_computed_column_rewritten_off_a_function_does_not_hold_the_function_back() {
    let (schema, _, id, module) = setup();
    let computed = |expression: &str| ComputedColumn {
        expression: expression.into(),
        persisted: false,
        not_null: false,
    };
    let plan = |new: &str| {
        vec![
            Change::DropComputedColumn {
                table: t(),
                name: "c".into(),
                computed: computed("dbo.f(a)"),
            },
            Change::AddComputedColumn {
                table: t(),
                name: "c".into(),
                computed: computed(new),
            },
            Change::AlterModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            },
        ]
    };
    let ending = |new: &str| {
        let mut after = schema.clone();
        after
            .tables
            .get_mut(&t())
            .unwrap()
            .computed
            .insert("c".into(), computed(new));
        after
    };
    // Rewritten to read the column alone, the function is free to change
    // once the old column is gone, after the new one is back.
    assert_eq!(
        violations_ending_in(&ending("a + 1"), plan("a + 1"), &[0, 1, 2]),
        []
    );
    // Negative: rewritten to read the function again, the column back before
    // the alter would call the old function.
    let late = violations_ending_in(&ending("dbo.f(a)"), plan("dbo.f(a)"), &[0, 1, 2]);
    assert!(!late.is_empty(), "{late:?}");
}

// The engine effects #1998 found too narrow, on the differ's own plans: the
// pipeline's order verifies clean, and an order that leaves a dependent the
// engine refuses in place is refused by name.

fn ty(s: &str) -> pbps_model::ColumnType {
    s.parse().unwrap()
}

fn ctx() -> pbps_diff::Context {
    pbps_diff::Context {
        operator: "test".into(),
        today: "2026-10-16".into(),
    }
}

/// The differ's plan from `base` to `desired` on SQL Server, the effects of
/// its steps, and the baseline.
struct Diffed {
    changes: Vec<PlannedChange>,
    ops: Operations,
    effects: Vec<pbps_model::operation::Effects>,
    baseline: Baseline,
}

impl Diffed {
    fn new(base: &Schema, desired: &Schema) -> Diffed {
        let base_ids = pbps_diff::resolve(base, &IdsFile::default(), &[], &ctx())
            .expect("resolve the base")
            .ids;
        let ids = pbps_diff::resolve(desired, &base_ids, &[], &ctx())
            .expect("resolve the desired state")
            .ids;
        let changes = pbps_diff::diff(
            pbps_diff::Side {
                schema: base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: desired,
                ids: &ids,
            },
            &Mssql,
            &pbps_model::Hints::default(),
        )
        .expect("diff")
        .changes;
        let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
        let names = PlanNames::of(&changes, &ids).unwrap();
        let owners = PlanOwners::new(names, &base_ids, &ids);
        let (before, after) = (
            Snapshot {
                schema: base,
                ids: &base_ids,
            },
            Snapshot {
                schema: desired,
                ids: &ids,
            },
        );
        let baseline = Baseline::of(before, &owners);
        let effects = analyse(&ops, &owners, before, after, &Mssql).effects;
        Diffed {
            changes,
            ops,
            effects,
            baseline,
        }
    }

    fn at(&self, what: impl Fn(&Change) -> bool) -> usize {
        self.changes
            .iter()
            .position(|c| what(&c.change))
            .unwrap_or_else(|| panic!("not in the plan: {:#?}", self.changes))
    }

    /// The plan's own order with step `moved` put right after step `after`.
    fn moved_after(&self, moved: usize, after: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.changes.len()).filter(|&i| i != moved).collect();
        let at = order.iter().position(|&i| i == after).unwrap();
        order.insert(at + 1, moved);
        order
    }

    fn verify(&self, order: &[usize]) -> Vec<Violation> {
        let ordered = self.ops.reordered(order);
        let effects: Vec<_> = order.iter().map(|&i| self.effects[i].clone()).collect();
        pbps_diff::order::verify(
            ordered.as_slice(),
            &effects,
            &self.baseline,
            &Default::default(),
        )
    }
}

fn still_present(violations: &[Violation], part: &PartKey) -> bool {
    violations.iter().any(|v| {
        matches!(v, Violation::StillPresent { key: ObjectKey::Part { part: p, .. }, .. } if p == part)
    })
}

/// `dbo.t(id, a, b)` with an index and a unique constraint over `a`, a check
/// on `b`, and `dbo.c` holding a foreign key into `t(a)`; `a` and `b` typed
/// as given.
fn retyped(a: &str, b: &str) -> Schema {
    use pbps_model::{CheckConstraint, ForeignKey, Index, IndexColumn, UniqueConstraint};
    let mut table = Table {
        columns: IndexMap::from([
            ("id".to_owned(), Column::new(ty("int")).not_null()),
            ("a".to_owned(), Column::new(ty(a))),
            ("b".to_owned(), Column::new(ty(b))),
        ]),
        ..Table::default()
    };
    table.indexes.insert(
        "ix_a".into(),
        Index {
            columns: vec![IndexColumn::column("a")],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    table.unique.insert(
        "uq_a".into(),
        UniqueConstraint {
            columns: vec!["a".into()],
            storage_parameters: Default::default(),
        },
    );
    table.checks.insert(
        "ck_b".into(),
        CheckConstraint {
            expression: "b <> ''".into(),
        },
    );
    let mut child = Table {
        columns: IndexMap::from([("x".to_owned(), Column::new(ty(a)))]),
        ..Table::default()
    };
    child.foreign_keys.insert(
        "fk_x".into(),
        ForeignKey {
            columns: vec!["x".into()],
            references_table: t(),
            references_columns: vec!["a".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    let mut schema = Schema::default();
    schema.tables.insert(t(), table);
    schema.tables.insert(TableName::new("dbo", "c"), child);
    schema
}

/// #1998 items 5 and 12: `int` to `bigint` changes the storage type, so
/// every key, index and foreign key over the column must be out of its way,
/// as the differ rebuilds them.
#[test]
fn a_storage_type_change_waits_for_every_key_and_index_over_its_column() {
    let plan = Diffed::new(&retyped("int", "varchar(10)"), &{
        let mut s = retyped("bigint", "varchar(10)");
        // The child's column follows, so the key still matches.
        s.tables
            .get_mut(&TableName::new("dbo", "c"))
            .unwrap()
            .columns["x"]
            .ty = ty("bigint");
        s
    });
    let own: Vec<usize> = (0..plan.changes.len()).collect();
    assert_eq!(plan.verify(&own), []);
    let retype = plan.at(|c| matches!(c, Change::AlterColumnType { column, .. } if column.table == t() && column.name == "a"));
    for (drop, part) in [
        (
            plan.at(|c| matches!(c, Change::DropIndex { name, .. } if name == "ix_a")),
            PartKey::Index("ix_a".into()),
        ),
        (
            plan.at(|c| matches!(c, Change::DropUnique { name, .. } if name == "uq_a")),
            PartKey::Unique("uq_a".into()),
        ),
        (
            plan.at(|c| matches!(c, Change::DropForeignKey { name, .. } if name == "fk_x")),
            PartKey::ForeignKey("fk_x".into()),
        ),
    ] {
        let late = plan.verify(&plan.moved_after(drop, retype));
        assert!(still_present(&late, &part), "{part:?}: {late:?}");
    }
}

/// #1998 item 5, the other half: widening a `varchar` keeps its checks and
/// indexes on SQL Server, so the retype requires none of them gone; a change
/// of the base type does require the check gone.
#[test]
fn a_varchar_widening_requires_no_check_gone_and_a_base_type_change_does() {
    let check_gone = |to: &str| {
        let plan = Diffed::new(&retyped("int", "varchar(10)"), &retyped("int", to));
        let own: Vec<usize> = (0..plan.changes.len()).collect();
        assert_eq!(plan.verify(&own), [], "{to}");
        let retype =
            plan.at(|c| matches!(c, Change::AlterColumnType { column, .. } if column.name == "b"));
        plan.effects[retype].requires.iter().any(|r| {
            matches!(
                r,
                pbps_model::operation::Requirement::Absent(ObjectKey::Part {
                    part: PartKey::Check(n),
                    ..
                }) if n == "ck_b"
            )
        })
    };
    assert!(!check_gone("varchar(20)"));
    assert!(check_gone("nvarchar(10)"));
}

/// #1998 item 13: a check whose text names a computed column stands in the
/// way of dropping it.
#[test]
fn dropping_a_computed_column_waits_for_a_check_that_names_it() {
    let (mut schema, _, _, _) = setup();
    schema.tables.get_mut(&t()).unwrap().checks.insert(
        "ck_c".into(),
        pbps_model::CheckConstraint {
            expression: "c > 0".into(),
        },
    );
    let mut after = schema.clone();
    let table = after.tables.get_mut(&t()).unwrap();
    table.checks.clear();
    let computed = table.computed.remove("c").unwrap();
    let plan = vec![
        Change::DropCheck {
            table: t(),
            name: "ck_c".into(),
        },
        Change::DropComputedColumn {
            table: t(),
            name: "c".into(),
            computed,
        },
    ];
    let ordered = violations_from(&schema, &after, plan.clone(), &[0, 1]);
    assert_eq!(ordered, []);
    let early = violations_from(&schema, &after, plan, &[1, 0]);
    assert!(
        still_present(&early, &PartKey::Check("ck_c".into())),
        "{early:?}"
    );
}

/// #1998 item 8: an index over a computed column the plan adds follows the
/// addition: the column has no uid, only its part of the table.
#[test]
fn an_index_over_a_computed_column_follows_its_addition() {
    let (schema, _, _, _) = setup();
    let computed = ComputedColumn {
        expression: "a + 1".into(),
        persisted: true,
        not_null: false,
    };
    let index = pbps_model::Index {
        columns: vec![pbps_model::IndexColumn::column("d")],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let mut after = schema.clone();
    let table = after.tables.get_mut(&t()).unwrap();
    table.computed.insert("d".into(), computed.clone());
    table.indexes.insert("ix_d".into(), index.clone());
    let plan = vec![
        Change::AddComputedColumn {
            table: t(),
            name: "d".into(),
            computed,
        },
        Change::AddIndex {
            table: t(),
            name: "ix_d".into(),
            index: Box::new(index),
            clustered: false,
        },
    ];
    assert_eq!(violations_from(&schema, &after, plan.clone(), &[0, 1]), []);
    assert!(!violations_from(&schema, &after, plan, &[1, 0]).is_empty());
}

/// [`violations`] from `before` to `after`, under the setup's ids.
fn violations_from(
    before: &Schema,
    after: &Schema,
    logical: Vec<Change>,
    order: &[usize],
) -> Vec<Violation> {
    let (_, ids, _, _) = setup();
    let logical: Vec<PlannedChange> = logical.into_iter().map(PlannedChange::new).collect();
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&logical, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let (b, a) = (
        Snapshot {
            schema: before,
            ids: &ids,
        },
        Snapshot {
            schema: after,
            ids: &ids,
        },
    );
    let baseline = Baseline::of(b, &owners);
    let shadowed = analyse(&ops, &owners, b, a, &Mssql);
    let ordered = ops.reordered(order);
    let effects: Vec<_> = order.iter().map(|&i| shadowed.effects[i].clone()).collect();
    pbps_diff::order::verify(ordered.as_slice(), &effects, &baseline, &Default::default())
}
