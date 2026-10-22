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
        columns: IndexMap::from([("a".into(), Column::new("int".parse().unwrap()))]),
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
    let ordered = pbps_diff::order::reordered_operations(&ops, order);
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
            } if name.raw() == "c" && operation.source() == 1
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
                ("id".into(), Column::new("int".parse().unwrap())),
                ("pid".into(), Column::new("int".parse().unwrap())),
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
                ("id".into(), Value::Int(id)),
                ("pid".into(), parent.map_or(Value::Null, Value::Int)),
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
    // A table that references itself is a cycle of one: its rows keep the
    // order given, as the differ leaves them, whether the key stands or not
    // (DEC-1968.2). A key that stands between two tables wants the parent's
    // rows first: `rows_wait_only_for_a_key_that_stands_when_they_are_written`
    // in `pbps-diff`.
    assert_eq!(verdict(false), []);
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
        .changes
        .clone();
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
        let ordered = pbps_diff::order::reordered_operations(&self.ops, order);
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
            ("id".into(), Column::new(ty("int")).not_null()),
            ("a".into(), Column::new(ty(a))),
            ("b".into(), Column::new(ty(b))),
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
        columns: IndexMap::from([("x".into(), Column::new(ty(a)))]),
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
    let retype = plan.at(|c| matches!(c, Change::AlterColumnType { column, .. } if column.table == t() && column.name == "a".into()));
    for (drop, part) in [
        (
            plan.at(|c| matches!(c, Change::DropIndex { name, .. } if name.raw() == "ix_a")),
            PartKey::Index("ix_a".into()),
        ),
        (
            plan.at(|c| matches!(c, Change::DropUnique { name, .. } if name.raw() == "uq_a")),
            PartKey::Unique("uq_a".into()),
        ),
        (
            plan.at(|c| matches!(c, Change::DropForeignKey { name, .. } if name.raw() == "fk_x")),
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
        let retype = plan.at(
            |c| matches!(c, Change::AlterColumnType { column, .. } if column.name == "b".into()),
        );
        plan.effects[retype].requires.iter().any(|r| {
            matches!(
                r,
                pbps_model::operation::Requirement::Absent(ObjectKey::Part {
                    part: PartKey::Check(n),
                    ..
                }) if n.raw() == "ck_b"
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
    let ordered = pbps_diff::order::reordered_operations(&ops, order);
    let effects: Vec<_> = order.iter().map(|&i| shadowed.effects[i].clone()).collect();
    pbps_diff::order::verify(ordered.as_slice(), &effects, &baseline, &Default::default())
}

fn module_pair(caller: &str, alter_caller: bool) -> (Schema, Schema, ModuleId, ModuleId) {
    let callee = ModuleId::Named(TableName::new("app", "f"));
    let caller_id = ModuleId::Named(TableName::new("app", "a_caller"));
    let function = |text: &str| Module {
        kind: ModuleKind::Function,
        definition: text.into(),
        description: None,
    };
    let mut before = Schema::default();
    before.modules.insert(
        callee.clone(),
        function("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a END"),
    );
    if alter_caller {
        before.modules.insert(
            caller_id.clone(),
            function("(@a int) RETURNS int AS BEGIN RETURN @a END"),
        );
    }
    let mut after = before.clone();
    after.modules.insert(
        callee.clone(),
        function("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a + 1 END"),
    );
    after.modules.insert(caller_id.clone(), function(caller));
    (before, after, callee, caller_id)
}

#[test]
fn schema_bound_callers_require_their_callees_final_definition() {
    use pbps_diff::order::{Because, Checks, Schedule, Source, commute, graph};
    use pbps_model::operation::{Incarnation, Requirement};

    // The caller sorts first by name, so the differ's lexical ranks still
    // decide its correct order. The effects must independently agree with it.
    let bound = [
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN app.f(@a) END",
        "(@a int) RETURNS int with /* outer /* nested */ end */ schemabinding, RETURNS NULL ON NULL INPUT AS BEGIN RETURN [APP] . [F](@a) END",
        "(@a int) RETURNS int WITH EXECUTE AS OWNER, SCHEMABINDING AS BEGIN RETURN app.f(@a) END",
        "(@a int) RETURNS int WITH EXECUTE AS [SCHEMABINDING], SCHEMABINDING AS BEGIN RETURN app.f(@a) END",
    ];
    let independent = [
        "(@a int) RETURNS int AS BEGIN RETURN app.f(@a) END",
        "(@a int) RETURNS int /* WITH SCHEMABINDING */ AS BEGIN RETURN app.f(@a) END",
        "(@a int) RETURNS int AS BEGIN DECLARE @s nvarchar(100) = N'WITH SCHEMABINDING'; RETURN app.f(@a) END",
        "(@a int) RETURNS int AS BEGIN DECLARE @schemabinding int = 0; RETURN app.f(@a) END",
        "(@a int) RETURNS int AS BEGIN DECLARE @s int; SELECT @s = @a AS [WITH SCHEMABINDING]; RETURN app.f(@a) END",
        "(@a int) RETURNS int WITH EXECUTE AS [SCHEMABINDING] AS BEGIN RETURN app.f(@a) END",
        "(@a int) RETURNS int WITH EXECUTE AS \"WITH SCHEMABINDING\" AS BEGIN RETURN app.f(@a) END",
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN other.f(@a) END",
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN app.f_extra(@a) END",
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a /* app.f(@a) */ END",
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN DECLARE @s nvarchar(100) = N'app.f(@a)'; RETURN @a END",
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN app.a_caller(@a) END",
        // An alias's column spelled like the function, `app.f` under alias
        // `app`: no call, so no binding (#2192 review).
        "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT TOP 1 app.f FROM dbo.t AS app) END",
    ];
    for alter in [false, true] {
        for (texts, needs_callee) in [(&bound[..], true), (&independent[..], false)] {
            for definition in texts {
                let (before, after, callee, caller) = module_pair(definition, alter);
                let diffed = Diffed::new(&before, &after);
                assert_eq!(diffed.changes.len(), 2, "{definition}");
                let f = diffed.at(|c| matches!(c, Change::AlterModule { id, .. } if id == &callee));
                let g = diffed.at(|c| matches!(c,
                    Change::CreateModule { id, .. } | Change::AlterModule { id, .. } if id == &caller));
                let requirement =
                    Requirement::Is(ObjectKey::Module(callee.clone()), Incarnation::Final);
                assert_eq!(
                    diffed.effects[g].requires.contains(&requirement),
                    needs_callee,
                    "{definition}"
                );
                // Self-reference must never invent an edge from a step to itself.
                assert!(
                    !diffed.effects[g].requires.contains(&Requirement::Is(
                        ObjectKey::Module(caller),
                        Incarnation::Final
                    )),
                    "{definition}"
                );
                let own: Vec<usize> = (0..diffed.changes.len()).collect();
                assert_eq!(diffed.verify(&own), [], "{definition}");
                let graph = graph(
                    diffed.ops.as_slice(),
                    &diffed.effects,
                    &diffed.baseline,
                    &Default::default(),
                    Checks::ALL,
                );
                let mut reversed = own;
                reversed.swap(f, g);
                if needs_callee {
                    assert!(f < g, "{definition}");
                    assert!(diffed.verify(&reversed).iter().any(|v| matches!(v, Violation::Edge { edge, .. }
                        if edge.source == Source::Effect && edge.because == Because::Rewrites(ObjectKey::Module(callee.clone())))), "{definition}");
                    assert_eq!(commute::swapped(&graph, 2, f, g), None, "{definition}");
                    let ordered = pbps_diff::order::reordered_operations(&diffed.ops, &reversed);
                    let effects: Vec<_> = reversed
                        .iter()
                        .map(|&i| diffed.effects[i].clone())
                        .collect();
                    let backwards_graph = pbps_diff::order::graph(
                        ordered.as_slice(),
                        &effects,
                        &diffed.baseline,
                        &Default::default(),
                        Checks::ALL,
                    );
                    let backwards = pbps_model::ChangeSet {
                        changes: reversed
                            .iter()
                            .map(|&i| diffed.changes[i].clone())
                            .collect(),
                    };
                    let settled = Schedule::of_graph(&backwards_graph, ordered.as_slice())
                        .settle(pbps_diff::order::propose(backwards))
                        .unwrap();
                    assert_eq!(settled.changes.changes, diffed.changes, "{definition}");
                } else {
                    assert_eq!(diffed.verify(&reversed), [], "{definition}");
                    assert!(!commute::joined(&graph, 2, f, g), "{definition}");
                }
            }
        }
    }

    // A callee the plan leaves alone is already final in the baseline.
    let (before, mut after, callee, caller) = module_pair(bound[0], false);
    after
        .modules
        .insert(callee.clone(), before.modules[&callee].clone());
    let stable = Diffed::new(&before, &after);
    assert_eq!(stable.changes.len(), 1);
    assert!(stable.effects[0].requires.contains(&Requirement::Is(
        ObjectKey::Module(callee),
        Incarnation::Final
    )));
    assert_eq!(stable.verify(&[0]), []);

    // Unbound procedures can call one another; imposing Final on both
    // references would manufacture a cycle a valid plan does not have.
    let mut before = Schema::default();
    let procedure = |text: &str| Module {
        kind: ModuleKind::Procedure,
        definition: text.into(),
        description: None,
    };
    let f = ModuleId::Named(TableName::new("app", "f"));
    before
        .modules
        .insert(f.clone(), procedure("AS BEGIN RETURN END"));
    let mut after = before.clone();
    after
        .modules
        .insert(f, procedure("AS BEGIN EXEC app.a_caller END"));
    after
        .modules
        .insert(caller, procedure("AS BEGIN EXEC app.f END"));
    let unbound = Diffed::new(&before, &after);
    assert_eq!(unbound.verify(&[0, 1]), []);
    assert_eq!(unbound.verify(&[1, 0]), []);

    // Two edited schema-bound functions, each reading an alias's column
    // spelled like the other: no call either way, so no cycle, and both
    // orders verify (#2192 review).
    let function = |text: &str| Module {
        kind: ModuleKind::Function,
        definition: text.into(),
        description: None,
    };
    let (f, g) = (
        ModuleId::Named(TableName::new("app", "f")),
        ModuleId::Named(TableName::new("app", "g")),
    );
    let mut before = Schema::default();
    for id in [&f, &g] {
        before.modules.insert(
            id.clone(),
            function("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a END"),
        );
    }
    let mut after = before.clone();
    after.modules.insert(
        f,
        function("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT TOP 1 app.g FROM dbo.t AS app) END"),
    );
    after.modules.insert(
        g,
        function("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT TOP 1 app.f FROM dbo.t AS app) END"),
    );
    let aliased = Diffed::new(&before, &after);
    assert_eq!(aliased.changes.len(), 2);
    assert_eq!(aliased.verify(&[0, 1]), []);
    assert_eq!(aliased.verify(&[1, 0]), []);
}

#[test]
fn a_callers_binding_requirement_reads_its_own_payload() {
    use pbps_model::operation::{Incarnation, Requirement};

    let bound = "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN app.f(@a) END";
    let plain = "(@a int) RETURNS int AS BEGIN RETURN @a END";
    for (payload, ending, needs_callee) in [(bound, plain, true), (plain, bound, false)] {
        let (before, mut after, callee, caller) = module_pair(payload, false);
        let module = after.modules[&caller].clone();
        after.modules.get_mut(&caller).unwrap().definition = ending.into();
        let changes = vec![PlannedChange::new(Change::CreateModule {
            id: caller,
            module: Box::new(module),
        })];
        let ids = IdsFile::default();
        let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &ids, &ids);
        let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
        let fx = analyse(
            &ops,
            &owners,
            Snapshot {
                schema: &before,
                ids: &ids,
            },
            Snapshot {
                schema: &after,
                ids: &ids,
            },
            &Mssql,
        );
        assert!(fx.unowned.is_empty());
        assert_eq!(
            fx.effects[0].requires.contains(&Requirement::Is(
                ObjectKey::Module(callee),
                Incarnation::Final
            )),
            needs_callee
        );
    }
}

#[test]
fn case_distinct_callees_do_not_invent_a_schema_bound_cycle() {
    use pbps_model::operation::{Incarnation, Requirement};
    use pbps_model::resolver::{BoundSurface, ObjectIdentity, Surface, SurfaceResolution};

    let function = |body: &str| Module {
        kind: ModuleKind::Function,
        definition: format!("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN {body} END"),
        description: None,
    };
    let named = |name| ModuleId::Named(TableName::new("app", name));
    let (a, upper, lower) = (named("A"), named("F"), named("f"));
    let mut before = Schema::default();
    before.modules.insert(upper.clone(), function("@a"));
    before.modules.insert(a.clone(), function("app.F(@a)"));
    // The old f does not bind A, so A can be altered before f introduces
    // that binding; an already-bound baseline caller would block the edit.
    before.modules.insert(lower.clone(), function("@a"));
    let mut after = before.clone();
    after.modules.insert(a.clone(), function("app.F(@a) + 1"));
    after
        .modules
        .insert(lower.clone(), function("app.A(@a) + 1"));
    let ids = IdsFile::default();
    let evidence: Vec<_> = before
        .modules
        .keys()
        .map(|id| {
            let inputs = if id == &a {
                vec![Surface::Module(upper.clone())]
            } else if id == &lower {
                vec![Surface::Module(a.clone())]
            } else {
                vec![]
            };
            let bound = BoundSurface {
                object: ObjectIdentity {
                    class: "function".into(),
                    name: vec![id.shown().to_string()],
                    signature: vec![],
                },
                bindings: vec![],
                managed_inputs: inputs.into_iter().collect(),
            };
            let mut current = bound.clone();
            if id == &lower {
                current.managed_inputs.clear();
            }
            SurfaceResolution {
                surface: Surface::Module(id.clone()),
                current: Some(current),
                desired: Some(bound),
            }
        })
        .collect();
    let planned = pbps_diff::resolver::plan_operations(
        pbps_diff::Side {
            schema: &before,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &after,
            ids: &ids,
        },
        &Default::default(),
        &evidence,
        &Mssql,
    )
    .expect("the stable app.F is not the edited app.f");
    assert_eq!(planned.ordered.changes.changes.len(), 2);
    assert!(
        matches!(&planned.ordered.changes.changes[0].change, Change::AlterModule { id, .. } if id == &a)
    );
    assert!(
        matches!(&planned.ordered.changes.changes[1].change, Change::AlterModule { id, .. } if id == &lower)
    );
    let diffed = Diffed::new(&before, &after);
    let a_at = diffed.at(|change| matches!(change, Change::AlterModule { id, .. } if id == &a));
    let f_at = diffed.at(|change| matches!(change, Change::AlterModule { id, .. } if id == &lower));
    assert!(diffed.effects[a_at].requires.contains(&Requirement::Is(
        ObjectKey::Module(upper),
        Incarnation::Final
    )));
    assert!(!diffed.effects[a_at].requires.contains(&Requirement::Is(
        ObjectKey::Module(lower),
        Incarnation::Final
    )));
    assert_eq!(diffed.verify(&[a_at, f_at]), []);
    assert!(!diffed.verify(&[f_at, a_at]).is_empty());
}

#[test]
fn an_unambiguous_case_insensitive_callee_still_binds() {
    use pbps_model::operation::{Incarnation, Requirement};

    for kind in [ModuleKind::Function, ModuleKind::View] {
        let text = if kind == ModuleKind::Function {
            "APP.F(@a)"
        } else {
            "(SELECT TOP 1 n FROM APP.F)"
        };
        let (mut before, mut after, callee, _) = module_pair(
            &format!("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN {text} END"),
            false,
        );
        if kind == ModuleKind::View {
            for schema in [&mut before, &mut after] {
                schema.modules.insert(
                    callee.clone(),
                    Module {
                        kind,
                        definition: "SELECT 1 AS n".into(),
                        description: None,
                    },
                );
            }
            after.modules.get_mut(&callee).unwrap().definition = "SELECT 2 AS n".into();
        }
        let diffed = Diffed::new(&before, &after);
        let caller = diffed.at(|change| matches!(change, Change::CreateModule { .. }));
        let target =
            diffed.at(|change| matches!(change, Change::AlterModule { id, .. } if id == &callee));
        assert!(
            diffed.effects[caller].requires.contains(&Requirement::Is(
                ObjectKey::Module(callee),
                Incarnation::Final
            )),
            "{kind:?}"
        );
        assert_eq!(diffed.verify(&[target, caller]), []);
        assert!(!diffed.verify(&[caller, target]).is_empty());
    }
}

#[test]
fn an_exact_callee_spelling_wins_over_a_case_only_neighbour() {
    use pbps_model::operation::{Incarnation, Requirement};

    for kind in [ModuleKind::Function, ModuleKind::View] {
        for (schema, leaf, expected) in [
            ("app", "F", Some("F")),
            ("app", "f", Some("f")),
            ("APP", "F", None),
        ] {
            let call = if kind == ModuleKind::Function {
                format!("[{schema}].[{leaf}](@a)")
            } else {
                format!("(SELECT TOP 1 n FROM [{schema}].[{leaf}])")
            };
            let (mut before, mut after, lower, _) = module_pair(
                &format!("(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN {call} END"),
                false,
            );
            let upper = ModuleId::Named(TableName::new("app", "F"));
            for schema in [&mut before, &mut after] {
                if kind == ModuleKind::View {
                    schema.modules.insert(
                        lower.clone(),
                        Module {
                            kind,
                            definition: "SELECT 1 AS n".into(),
                            description: None,
                        },
                    );
                }
                schema
                    .modules
                    .insert(upper.clone(), schema.modules[&lower].clone());
            }
            // Only the caller is new: the requirements must select the exact
            // stable callee even without an edit to expose a false edge.
            after
                .modules
                .insert(lower.clone(), before.modules[&lower].clone());
            after
                .modules
                .insert(upper.clone(), before.modules[&upper].clone());
            let diffed = Diffed::new(&before, &after);
            assert_eq!(diffed.changes.len(), 1);
            for (id, spelling) in [(&upper, "F"), (&lower, "f")] {
                assert_eq!(
                    diffed.effects[0].requires.contains(&Requirement::Is(
                        ObjectKey::Module(id.clone()),
                        Incarnation::Final
                    )),
                    expected == Some(spelling),
                    "{kind:?}, call={call}, callee={spelling}"
                );
            }
            assert_eq!(diffed.verify(&[0]), []);
        }
    }
}
