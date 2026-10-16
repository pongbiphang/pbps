//! The verifier and the graph, on the negatives the issue names and the
//! guard checks that show each one needs what it uses (#1954).

use std::collections::BTreeSet;

use indexmap::IndexMap;
use pbps_model::operation::{
    Effects, Incarnation, ModuleEdits, ObjectKey, Operation, Operations, Origin, PlanOwners,
    Requirement, Resource, Snapshot, Source as Origin_, effects,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, Column, ColumnRef, ColumnType, ForeignKey, IdsFile, Module, ModuleDeps, ModuleId,
    ModuleKind, PlannedChange, ReferentialAction, Row, RowKey, Schema, Table, TableName, Uid,
    UidKind, Value,
};

use super::edges::{Baseline, Because, Checks, graph};
use super::verify::{Violation, verify, verify_with};

fn uid(kind: UidKind, name: &str) -> Uid {
    Uid::derived(kind, name, 0)
}
fn t(name: &str) -> TableName {
    TableName::new("app", name)
}
fn int() -> ColumnType {
    "int".parse().unwrap()
}
fn p(change: Change) -> PlannedChange {
    PlannedChange::new(change)
}
fn table_with(columns: &[&str]) -> Table {
    let mut cols = IndexMap::new();
    for c in columns {
        cols.insert((*c).to_owned(), Column::new(int()));
    }
    Table {
        columns: cols,
        ..Table::default()
    }
}

/// A plan, its two ends and their identity files: everything a verification
/// needs.
struct Plan {
    logical: Vec<PlannedChange>,
    before: Schema,
    after: Schema,
    before_ids: IdsFile,
    after_ids: IdsFile,
}

impl Plan {
    fn ids(tables: &[&str], columns: &[(&str, &str)]) -> IdsFile {
        let mut ids = IdsFile::default();
        for name in tables {
            ids.tables.insert(uid(UidKind::Table, name), t(name));
        }
        for (table, column) in columns {
            ids.columns.insert(
                uid(UidKind::Column, &format!("{table}.{column}")),
                t(table).column(*column),
            );
        }
        ids
    }

    /// Every operation's effects, in the logical order, and the baseline.
    fn analysed(&self, modules: ModuleEdits) -> (Operations, Vec<Effects>, Baseline) {
        let ops = Operations::of_plan(&self.logical, modules);
        let names = PlanNames::of(&self.logical, &self.after_ids).unwrap();
        let owners = PlanOwners::new(names, &self.before_ids, &self.after_ids);
        let (before, after) = (
            Snapshot {
                schema: &self.before,
                ids: &self.before_ids,
            },
            Snapshot {
                schema: &self.after,
                ids: &self.after_ids,
            },
        );
        let fx: Vec<Effects> = ops
            .iter()
            .map(|op| effects(op, &owners, before, after).unwrap())
            .collect();
        let baseline = Baseline::of(before, &owners);
        (ops, fx, baseline)
    }
}

fn run(
    ops: &Operations,
    fx: &[Effects],
    baseline: &Baseline,
    order: &[usize],
    declared: &ModuleDeps,
    checks: Checks,
) -> Vec<Violation> {
    let reordered = ops.reordered(order);
    let effects: Vec<Effects> = order.iter().map(|&i| fx[i].clone()).collect();
    verify_with(reordered.as_slice(), &effects, baseline, declared, checks)
}

fn create_then_add_column() -> Plan {
    let (tid, cid) = (uid(UidKind::Table, "t"), uid(UidKind::Column, "t.c"));
    let after_table = table_with(&["c"]);
    let mut after = Schema::default();
    after.tables.insert(t("t"), after_table);
    Plan {
        logical: vec![
            p(Change::CreateTable {
                uid: tid,
                name: t("t"),
                table: Box::new(table_with(&[])),
                beside_default: None,
                moves_at_most: None,
            }),
            p(Change::AddColumn {
                uid: cid,
                table: t("t"),
                name: "c".into(),
                column: Box::new(Column::new(int())),
            }),
        ],
        before: Schema::default(),
        after,
        before_ids: IdsFile::default(),
        after_ids: Plan::ids(&["t"], &[("t", "c")]),
    }
}

/// Parent and child tables, the child's key into the parent, both dropped.
fn two_dropped_tables() -> Plan {
    let mut child = table_with(&["pid"]);
    child.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["pid".into()],
            references_table: t("parent"),
            references_columns: vec!["id".into()],
            on_delete: ReferentialAction::default(),
            on_update: ReferentialAction::default(),
        },
    );
    let mut before = Schema::default();
    before.tables.insert(t("parent"), table_with(&["id"]));
    before.tables.insert(t("child"), child);
    let drop = |name: &str| {
        p(Change::DropTable {
            uid: uid(UidKind::Table, name),
            name: t(name),
            detach_from: None,
        })
    };
    Plan {
        // The child first: its key is gone before the table it points at.
        logical: vec![drop("child"), drop("parent")],
        before,
        after: Schema::default(),
        before_ids: Plan::ids(&["parent", "child"], &[("parent", "id"), ("child", "pid")]),
        after_ids: IdsFile::default(),
    }
}

#[test]
fn an_add_column_before_its_create_table_is_refused_by_the_requirement_it_misses() {
    let plan = create_then_add_column();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL),
        [],
        "the order the differ makes is accepted"
    );
    let violations = run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            Violation::Unmet { requirement: Requirement::Exists(ObjectKey::Table(_)), operation }
                if operation.source() == 1
        )),
        "{violations:?}"
    );
}

#[test]
fn two_dropped_tables_with_the_referenced_one_first_are_refused_by_the_baseline_s_key() {
    let plan = two_dropped_tables();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL),
        [],
        "the child first is accepted"
    );
    let violations = run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            Violation::StillPresent { key: ObjectKey::Part { .. }, operation }
                if operation.source() == 1
        )),
        "{violations:?}"
    );
    // Guard: without the baseline model nothing says the key stands, neither
    // to the effects of the drop nor to the replay, and the negative passes
    // wrongly.
    let blind = Plan {
        before: Schema::default(),
        ..two_dropped_tables()
    };
    let (ops, fx, _) = blind.analysed(ModuleEdits::InPlace);
    let ignored = Checks {
        baseline: false,
        ..Checks::ALL
    };
    assert_eq!(
        run(&ops, &fx, &Baseline::empty(), &[1, 0], &none, ignored),
        []
    );
}

fn view(name: &str) -> (ModuleId, Module) {
    (
        ModuleId::Named(t(name)),
        Module {
            kind: ModuleKind::View,
            definition: "SELECT 1 AS id".into(),
            description: None,
        },
    )
}

#[test]
fn a_broken_depends_on_is_refused_as_declared() {
    let ((input_id, input), (dependent_id, dependent)) = (view("v1"), view("v2"));
    let logical = vec![
        p(Change::CreateModule {
            id: input_id.clone(),
            module: Box::new(input),
        }),
        p(Change::CreateModule {
            id: dependent_id.clone(),
            module: Box::new(dependent),
        }),
    ];
    let plan = Plan {
        logical,
        before: Schema::default(),
        after: Schema::default(),
        before_ids: IdsFile::default(),
        after_ids: IdsFile::default(),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let declared: ModuleDeps = [(dependent_id, BTreeSet::from([input_id]))].into();
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1], &declared, Checks::ALL),
        []
    );
    let violations = run(&ops, &fx, &baseline, &[1, 0], &declared, Checks::ALL);
    assert!(
        violations
            .iter()
            .any(|v| matches!(v, Violation::BrokenDeclared { .. })),
        "{violations:?}"
    );
    // Not declared, nothing orders two views that name nothing of each other.
    assert_eq!(
        run(
            &ops,
            &fx,
            &baseline,
            &[1, 0],
            &ModuleDeps::new(),
            Checks::ALL
        ),
        []
    );
}

/// Two rows inserted into one table: a synthetic pair of writes to the
/// table's values, whose final state depends on which runs last.
fn two_rows_of_one_table() -> Plan {
    let mut after = Schema::default();
    after.tables.insert(t("t"), table_with(&["id"]));
    let ids = Plan::ids(&["t"], &[("t", "id")]);
    Plan {
        logical: vec![row("t", "1"), row("t", "2")],
        before: after.clone(),
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

#[test]
fn two_writes_to_one_resource_swapped_from_their_base_order_are_refused() {
    let plan = two_rows_of_one_table();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    let violations = run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            Violation::Edge { edge, .. } if matches!(edge.because, Because::WriteWrite(_))
        )),
        "{violations:?}"
    );
    // Guard: without the write-write rule nothing orders the pair.
    let ignored = Checks {
        write_write: false,
        ..Checks::ALL
    };
    assert_eq!(run(&ops, &fx, &baseline, &[1, 0], &none, ignored), []);
}

fn row(table: &str, key: &str) -> PlannedChange {
    p(Change::InsertRow {
        table: t(table),
        key_column: "id".into(),
        identity_key: false,
        key: RowKey::from(key),
        row: [("id".to_owned(), Value::Text(key.into()))]
            .into_iter()
            .collect::<Row>(),
        defaults: Default::default(),
        types: Default::default(),
    })
}

#[test]
fn row_writes_to_different_tables_with_no_key_between_them_get_no_edge() {
    let mut after = Schema::default();
    after.tables.insert(t("a"), table_with(&["id"]));
    after.tables.insert(t("b"), table_with(&["id"]));
    let ids = Plan::ids(&["a", "b"], &[("a", "id"), ("b", "id")]);
    let plan = Plan {
        logical: vec![row("a", "x"), row("b", "y")],
        before: after.clone(),
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let g = graph(
        ops.as_slice(),
        &fx,
        &baseline,
        &ModuleDeps::new(),
        Checks::ALL,
    );
    assert!(g.edges.is_empty(), "{:?}", g.edges);
    let none = ModuleDeps::new();
    for order in [[0, 1], [1, 0]] {
        assert_eq!(run(&ops, &fx, &baseline, &order, &none, Checks::ALL), []);
    }
    // Negative: two rows of one table do keep their base order.
    let mut one = Schema::default();
    one.tables.insert(t("a"), table_with(&["id"]));
    let ids = Plan::ids(&["a"], &[("a", "id")]);
    let same = Plan {
        logical: vec![row("a", "x"), row("a", "y")],
        before: one.clone(),
        after: one,
        before_ids: ids.clone(),
        after_ids: ids,
    };
    let (ops, fx, baseline) = same.analysed(ModuleEdits::InPlace);
    let g = graph(
        ops.as_slice(),
        &fx,
        &baseline,
        &ModuleDeps::new(),
        Checks::ALL,
    );
    let pairs: BTreeSet<(usize, usize)> = g.edges.iter().map(|e| (e.before, e.after)).collect();
    assert_eq!(pairs, BTreeSet::from([(0, 1)]), "{:?}", g.edges);
}

/// The routine a rebuild drops and creates again, and a step that needs the
/// new one: its `PUBLIC` execute is restated on the object it is on.
fn rebuilt_routine() -> (Operations, Vec<Effects>, Baseline) {
    let id: ModuleId = "app.f(integer)".parse().unwrap();
    let module = Module {
        kind: ModuleKind::Function,
        definition: "CREATE FUNCTION app.f(a integer) RETURNS integer AS 'SELECT a' \
                     LANGUAGE sql;"
            .into(),
        description: None,
    };
    let ModuleId::Routine(routine) = id.clone() else {
        unreachable!("declared as a routine")
    };
    let logical = vec![p(Change::PublicExecution {
        routine,
        access: pbps_model::PublicAccess::Revoked,
        origin: pbps_model::RoutineOrigin::Rebuilt,
    })];
    let source = Origin_::Rebinding(pbps_model::resolver::Surface::Module(id.clone()));
    let ops = Operations::expanded(
        1,
        [
            (Origin::Whole(0), logical[0].clone()),
            (
                Origin::Teardown(source.clone()),
                p(Change::DropModule {
                    id: id.clone(),
                    kind: ModuleKind::Function,
                }),
            ),
            (
                Origin::Restoration(source),
                p(Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                }),
            ),
        ],
        ModuleEdits::InPlace,
    );
    let mut schema = Schema::default();
    schema.modules.insert(id, module);
    let ids = IdsFile::default();
    let names = PlanNames::of(&logical, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let snapshot = Snapshot {
        schema: &schema,
        ids: &ids,
    };
    let fx = ops
        .iter()
        .map(|op| effects(op, &owners, snapshot, snapshot).unwrap())
        .collect();
    (ops, fx, Baseline::of(snapshot, &owners))
}

#[test]
fn a_step_that_needs_the_rebuilt_definition_before_the_rebuild_is_refused_by_the_incarnation() {
    let (ops, fx, baseline) = rebuilt_routine();
    let none = ModuleDeps::new();
    // The teardown, the restoration, then what needs the new definition.
    assert_eq!(
        run(&ops, &fx, &baseline, &[1, 2, 0], &none, Checks::ALL),
        []
    );
    // It needs the plan's instance, and runs while the baseline's stands.
    let violations = run(&ops, &fx, &baseline, &[0, 1, 2], &none, Checks::ALL);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            Violation::Unmet {
                requirement: Requirement::Is(ObjectKey::Module(_), Incarnation::Final),
                ..
            }
        )),
        "{violations:?}"
    );
    // Guard: with the two instances told apart by nothing, the baseline's
    // stands for the plan's and the negative passes wrongly.
    let ignored = Checks {
        incarnations: false,
        ..Checks::ALL
    };
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1, 2], &none, ignored), []);
}

#[test]
fn the_effects_of_an_operation_read_its_own_payload_and_not_the_desired_table() {
    // A bare table the resolver splits off creates no check: the later step
    // does (DEC-1961.1). Effects are read the same way.
    let mut full = table_with(&["id"]);
    full.checks.insert(
        "k".into(),
        pbps_model::CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    let mut after = Schema::default();
    after.tables.insert(t("t"), full);
    let ids = Plan::ids(&["t"], &[("t", "id")]);
    let logical = vec![p(Change::CreateTable {
        uid: uid(UidKind::Table, "t"),
        name: t("t"),
        table: Box::new(table_with(&["id"])),
        beside_default: None,
        moves_at_most: None,
    })];
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&logical, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let snapshot = Snapshot {
        schema: &after,
        ids: &ids,
    };
    let op: &Operation = &ops.as_slice()[0];
    let fx = effects(op, &owners, snapshot, snapshot).unwrap();
    assert!(
        !fx.creates
            .iter()
            .any(|r| matches!(r, Resource::Object(ObjectKey::Part { .. }, _))),
        "{fx:?}"
    );
    assert!(
        fx.creates
            .iter()
            .any(|r| matches!(r, Resource::Object(ObjectKey::Table(_), Incarnation::Final)))
    );
}

#[test]
fn a_clean_order_verifies_clean() {
    let plan = create_then_add_column();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(
        verify(ops.as_slice(), &fx, &baseline, &ModuleDeps::new()),
        []
    );
    let _: Option<ColumnRef> = None;
}

#[test]
fn an_obligation_nothing_can_provide_is_recorded_and_does_not_fail_the_run() {
    use super::shadow::{Shadowed, Stage, judge};
    // The column is added to a table the baseline lacks and no operation
    // creates: the input disagrees with itself, and no order could meet it.
    let mut plan = create_then_add_column();
    plan.logical.remove(0);
    let (alone, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let report = Shadowed {
        effects: vec![fx[0].clone()],
        unowned: Vec::new(),
    }
    .report(&alone, &baseline, &ModuleDeps::new());
    assert!(!report.violations.is_empty(), "the replay still sees it");
    let (lines, fails) = judge(alone.as_slice(), &report, Stage::Differ);
    assert!(!fails, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("unprovided ")),
        "{lines:?}"
    );
}

#[test]
fn an_obligation_some_order_could_meet_but_this_one_meets_too_late_fails_the_run() {
    use super::shadow::{Shadowed, Stage, judge};
    let plan = create_then_add_column();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let swapped = ops.reordered(&[1, 0]);
    let report = Shadowed {
        effects: vec![fx[1].clone(), fx[0].clone()],
        unowned: Vec::new(),
    }
    .report(&swapped, &baseline, &ModuleDeps::new());
    let (lines, fails) = judge(swapped.as_slice(), &report, Stage::Differ);
    assert!(fails, "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("unmet ")), "{lines:?}");
}

#[test]
fn a_dependent_that_stands_throughout_is_recorded_and_one_the_plan_removes_late_fails() {
    use super::shadow::{Shadowed, Stage, judge};
    // Only the parent is dropped: the child's key into it stands in the
    // baseline and nothing removes it, so no order could satisfy the drop.
    let mut standing = two_dropped_tables();
    standing.logical.remove(0);
    let (ops, fx, baseline) = standing.analysed(ModuleEdits::InPlace);
    let report = Shadowed {
        effects: fx,
        unowned: Vec::new(),
    }
    .report(&ops, &baseline, &ModuleDeps::new());
    assert!(!report.violations.is_empty(), "the replay still sees it");
    let (lines, fails) = judge(ops.as_slice(), &report, Stage::Differ);
    assert!(!fails, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("unprovided ")),
        "{lines:?}"
    );
    // The same drop with the child's own drop after it is an order that could
    // have been right.
    let both = two_dropped_tables();
    let (ops, fx, baseline) = both.analysed(ModuleEdits::InPlace);
    let swapped = ops.reordered(&[1, 0]);
    let report = Shadowed {
        effects: vec![fx[1].clone(), fx[0].clone()],
        unowned: Vec::new(),
    }
    .report(&swapped, &baseline, &ModuleDeps::new());
    let (lines, fails) = judge(swapped.as_slice(), &report, Stage::Differ);
    assert!(fails, "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("present ")), "{lines:?}");
}

/// `t` with `id` and one more column, the plan renaming it from `a` to `b`;
/// the column keeps its identity across the two names.
fn renamed_default(after_default: Option<&str>) -> Plan {
    use pbps_model::ValueSource;
    let table = |name: &str, default: Option<&str>| {
        let mut table = table_with(&["id"]);
        let mut column = Column::new(int());
        if let Some(text) = default {
            column.value_source = ValueSource::Default(text.into());
        }
        table.columns.insert(name.into(), column);
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), table("a", Some("1")));
    after.tables.insert(t("t"), table("b", after_default));
    let moved = uid(UidKind::Column, "t.a");
    let named = |column: &str| {
        let mut ids = Plan::ids(&["t"], &[("t", "id")]);
        ids.columns.insert(moved.clone(), t("t").column(column));
        ids
    };
    Plan {
        logical: vec![
            p(Change::RenameColumn {
                uid: moved.clone(),
                table: t("t"),
                from: "a".into(),
                to: "b".into(),
                table_was: None,
            }),
            row("t", "1"),
        ],
        before,
        after,
        before_ids: named("a"),
        after_ids: named("b"),
    }
}

#[test]
fn a_renamed_column_keeps_its_default_so_a_row_insert_waits_for_nothing() {
    let default_must_go = |plan: &Plan| {
        let (_, fx, _) = plan.analysed(ModuleEdits::InPlace);
        fx[1]
            .requires
            .iter()
            .any(|r| matches!(r, Requirement::Absent(ObjectKey::Default(_))))
    };
    assert!(!default_must_go(&renamed_default(Some("1"))));
    // Negative: the same rename that also drops the default does make the
    // row wait for the drop.
    assert!(default_must_go(&renamed_default(None)));
}

/// `par` with a partition `pt` attached in the plan, whose own default and
/// NOT NULL on `note` are then set: ops 0 (attach), 1 (default), 2 (NOT NULL).
fn attached_partition() -> Plan {
    use pbps_model::{PartitionBound, PartitionColumn, PartitionOf};
    let mut before = Schema::default();
    before.tables.insert(t("par"), table_with(&["id", "note"]));
    before.tables.insert(t("pt"), table_with(&["id", "note"]));
    let mut member = table_with(&[]);
    member.partition_of = Some(PartitionOf {
        parent: t("par"),
        bound: PartitionBound::Default,
        columns: [(
            "note".to_owned(),
            PartitionColumn {
                default: Some("5".into()),
                not_null: true,
            },
        )]
        .into_iter()
        .collect(),
    });
    let mut after = Schema::default();
    after.tables.insert(t("par"), table_with(&["id", "note"]));
    after.tables.insert(t("pt"), member.clone());
    let pt = uid(UidKind::Table, "pt");
    Plan {
        logical: vec![
            p(Change::AttachPartition {
                uid: pt.clone(),
                table: t("pt"),
                parent: t("par"),
                bound: PartitionBound::Default,
                key: Vec::new(),
                shape: Box::new(member),
            }),
            p(Change::SetPartitionDefault {
                uid: pt.clone(),
                table: t("pt"),
                parent: t("par"),
                column: "note".into(),
                from: None,
                to: Some("5".into()),
                fallback: None,
            }),
            p(Change::SetPartitionNotNull {
                uid: pt,
                table: t("pt"),
                column: "note".into(),
                not_null: true,
            }),
        ],
        before,
        after,
        before_ids: Plan::ids(
            &["par", "pt"],
            &[("par", "id"), ("par", "note"), ("pt", "id"), ("pt", "note")],
        ),
        after_ids: Plan::ids(&["par", "pt"], &[("par", "id"), ("par", "note")]),
    }
}

#[test]
fn a_partitions_default_and_not_null_on_one_column_do_not_order_each_other() {
    let plan = attached_partition();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    let g = graph(ops.as_slice(), &fx, &baseline, &none, Checks::ALL);
    assert!(
        !g.edges
            .iter()
            .any(|e| (e.before, e.after) == (1, 2) || (e.before, e.after) == (2, 1)),
        "{:?}",
        g.edges
    );
    for order in [[0, 1, 2], [0, 2, 1]] {
        assert_eq!(
            run(&ops, &fx, &baseline, &order, &none, Checks::ALL),
            [],
            "{order:?}"
        );
    }
    // Negative: the entry they both address is the attach's, and a step that
    // runs before it is still refused.
    let early = run(&ops, &fx, &baseline, &[2, 0, 1], &none, Checks::ALL);
    assert!(
        early.iter().any(|v| matches!(
            v,
            Violation::Edge { before, after, edge }
                if before.source() == 0
                    && after.source() == 2
                    && matches!(edge.because, Because::Creates(ObjectKey::Column(_)))
        )),
        "{early:?}"
    );
}

#[test]
fn a_column_gaining_a_default_and_a_deprecation_runs_them_in_either_order() {
    let mut x = Column::new(int());
    x.value_source = pbps_model::ValueSource::Default("1".into());
    let mut table = table_with(&[]);
    table.columns.insert("x".into(), x);
    let mut after = Schema::default();
    after.tables.insert(t("t"), table.clone());
    let bare = Table {
        columns: [("x".to_owned(), Column::new(int()))].into_iter().collect(),
        ..Table::default()
    };
    let (tid, cid) = (uid(UidKind::Table, "t"), uid(UidKind::Column, "t.x"));
    let plan = Plan {
        logical: vec![
            p(Change::CreateTable {
                uid: tid,
                name: t("t"),
                table: Box::new(bare),
                beside_default: None,
                moves_at_most: None,
            }),
            p(Change::AlterColumnDefault {
                uid: cid.clone(),
                column: t("t").column("x"),
                from: None,
                to: Some("1".into()),
            }),
            p(Change::SetColumnDeprecated {
                uid: cid,
                column: t("t").column("x"),
                reason: Some("old".into()),
            }),
        ],
        before: Schema::default(),
        after,
        before_ids: IdsFile::default(),
        after_ids: Plan::ids(&["t"], &[("t", "x")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    for order in [[0, 1, 2], [0, 2, 1]] {
        assert_eq!(
            run(&ops, &fx, &baseline, &order, &none, Checks::ALL),
            [],
            "{order:?}"
        );
    }
    // Negative: the deprecation still waits for the column to exist.
    assert!(!run(&ops, &fx, &baseline, &[2, 0, 1], &none, Checks::ALL).is_empty());
}

#[test]
fn a_new_replica_identity_index_does_not_order_the_old_one_s_parameter_change() {
    use pbps_model::IndexPart;
    use pbps_model::{Index, IndexColumn, IndexKey, IndexMethod, ReplicaIdentity};
    let index = || Index {
        columns: vec![IndexColumn {
            key: IndexKey::Column("a".into()),
            descending: false,
            opclass: None,
        }],
        include: vec![],
        unique: true,
        filter: None,
        method: IndexMethod::Btree,
        storage_parameters: Default::default(),
    };
    let mut was = table_with(&["a"]);
    was.indexes.insert("ix_old".into(), index());
    was.replica_identity = Some(ReplicaIdentity::Index("ix_old".into()));
    let mut will = was.clone();
    will.indexes.insert("ix_new".into(), index());
    will.replica_identity = Some(ReplicaIdentity::Index("ix_new".into()));
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), was);
    after.tables.insert(t("t"), will);
    let plan = Plan {
        logical: vec![
            p(Change::SetIndexStorageParameters {
                table: t("t"),
                target: IndexPart::Index("ix_old".into()),
                method: IndexMethod::Btree,
                set: [("fillfactor".to_owned(), "70".to_owned())]
                    .into_iter()
                    .collect(),
                reset: Default::default(),
            }),
            p(Change::AddIndex {
                table: t("t"),
                name: "ix_new".into(),
                index: Box::new(index()),
                clustered: false,
            }),
            p(Change::SetReplicaIdentity {
                uid: uid(UidKind::Table, "t"),
                table: t("t"),
                to: Some(ReplicaIdentity::Index("ix_new".into())),
            }),
        ],
        before,
        after,
        before_ids: Plan::ids(&["t"], &[("t", "a")]),
        after_ids: Plan::ids(&["t"], &[("t", "a")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    for order in [[0, 1, 2], [1, 2, 0], [1, 0, 2]] {
        assert_eq!(
            run(&ops, &fx, &baseline, &order, &none, Checks::ALL),
            [],
            "{order:?}"
        );
    }
    // Negative: the switch to the new index still follows its creation.
    assert!(!run(&ops, &fx, &baseline, &[2, 1, 0], &none, Checks::ALL).is_empty());
}

#[test]
fn a_diagnostic_is_cut_inside_no_character() {
    // "é" is two bytes: byte 3 of "aaé" falls inside it.
    for max in 0..8 {
        let mut text = "aaéé".to_owned();
        super::shadow::clip(&mut text, max);
        assert!(
            text.len() <= max && "aaéé".starts_with(&text),
            "{max}: {text}"
        );
    }
    let mut short = "ab".to_owned();
    super::shadow::clip(&mut short, 10);
    assert_eq!(short, "ab");
}

#[test]
fn a_replica_identity_moves_to_a_standing_index_before_the_old_one_is_dropped() {
    use pbps_model::{Index, IndexColumn, IndexKey, IndexMethod, ReplicaIdentity};
    let index = || Index {
        columns: vec![IndexColumn {
            key: IndexKey::Column("a".into()),
            descending: false,
            opclass: None,
        }],
        include: vec![],
        unique: true,
        filter: None,
        method: IndexMethod::Btree,
        storage_parameters: Default::default(),
    };
    let mut was = table_with(&[]);
    let mut a = Column::new(int());
    a.nullable = false;
    was.columns.insert("a".into(), a);
    was.indexes.insert("ix_old".into(), index());
    was.indexes.insert("ix_new".into(), index());
    was.replica_identity = Some(ReplicaIdentity::Index("ix_old".into()));
    let mut will = was.clone();
    will.indexes.remove("ix_old");
    will.replica_identity = Some(ReplicaIdentity::Index("ix_new".into()));
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), was);
    after.tables.insert(t("t"), will);
    let plan = Plan {
        logical: vec![
            p(Change::SetReplicaIdentity {
                uid: uid(UidKind::Table, "t"),
                table: t("t"),
                to: Some(ReplicaIdentity::Index("ix_new".into())),
            }),
            p(Change::DropIndex {
                table: t("t"),
                name: "ix_old".into(),
            }),
        ],
        before,
        after,
        before_ids: Plan::ids(&["t"], &[("t", "a")]),
        after_ids: Plan::ids(&["t"], &[("t", "a")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: the old index is dropped under the identity that still
    // binds it.
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

fn permissions() -> std::collections::BTreeSet<pbps_model::Permission> {
    Default::default()
}

#[test]
fn a_revoke_reads_the_module_that_stands_so_it_may_precede_an_in_place_alteration() {
    let (id, module) = view("v");
    let mut changed = module.clone();
    changed.definition = "SELECT 2 AS id".into();
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.modules.insert(id.clone(), module);
    after.modules.insert(id.clone(), changed.clone());
    let plan = Plan {
        logical: vec![
            p(Change::Revoke {
                role: "r".into(),
                target: pbps_model::GrantTarget::Object(t("v")),
                permissions: permissions(),
            }),
            p(Change::AlterModule {
                id,
                module: Box::new(changed),
            }),
        ],
        before,
        after,
        before_ids: IdsFile::default(),
        after_ids: IdsFile::default(),
    };
    let none = ModuleDeps::new();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    for order in [[0, 1], [1, 0]] {
        assert_eq!(
            run(&ops, &fx, &baseline, &order, &none, Checks::ALL),
            [],
            "{order:?}"
        );
    }
    // Negative: a module the plan drops is gone before a revoke that runs
    // after it.
    let (id, module) = view("v");
    let mut before = Schema::default();
    before.modules.insert(id.clone(), module);
    let dropped = Plan {
        logical: vec![
            p(Change::Revoke {
                role: "r".into(),
                target: pbps_model::GrantTarget::Object(t("v")),
                permissions: permissions(),
            }),
            p(Change::DropModule {
                id,
                kind: ModuleKind::View,
            }),
        ],
        before,
        after: Schema::default(),
        before_ids: IdsFile::default(),
        after_ids: IdsFile::default(),
    };
    let (ops, fx, baseline) = dropped.analysed(ModuleEdits::InPlace);
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

/// `c(id, pid)` keyed by `id`, optionally with `pid -> id` on itself.
fn self_referencing(with_key: bool) -> Table {
    let mut table = table_with(&["id", "pid"]);
    table.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    if with_key {
        table.foreign_keys.insert(
            "fk".into(),
            ForeignKey {
                columns: vec!["pid".into()],
                references_table: t("c"),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::default(),
                on_update: ReferentialAction::default(),
            },
        );
    }
    table
}

fn child_row(id: i64, parent: Option<i64>) -> PlannedChange {
    p(Change::InsertRow {
        table: t("c"),
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
}

fn rows_of_c(before_key: bool, after_key: bool, logical: Vec<PlannedChange>) -> Plan {
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("c"), self_referencing(before_key));
    after.tables.insert(t("c"), self_referencing(after_key));
    let ids = Plan::ids(&["c"], &[("c", "id"), ("c", "pid")]);
    Plan {
        logical,
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

#[test]
fn rows_wait_only_for_a_key_that_stands_when_they_are_written() {
    let none = ModuleDeps::new();
    // The key is added after the rows it checks: 1 -> 2 and 2 -> NULL, the
    // child first, as the ids give it.
    let adds = rows_of_c(
        false,
        true,
        vec![
            child_row(1, Some(2)),
            child_row(2, None),
            p(Change::AddForeignKey {
                table: t("c"),
                name: "fk".into(),
                constraint: Box::new(self_referencing(true).foreign_keys["fk"].clone()),
            }),
        ],
    );
    let (ops, fx, baseline) = adds.analysed(ModuleEdits::InPlace);
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1, 2], &none, Checks::ALL),
        []
    );
    // Negative: a key that stands throughout wants the parent row first.
    let stands = rows_of_c(true, true, vec![child_row(2, None), child_row(1, Some(2))]);
    let (ops, fx, baseline) = stands.analysed(ModuleEdits::InPlace);
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

#[test]
fn a_row_value_names_a_row_only_through_the_referenced_primary_key() {
    // `code` is unique but is not the key rows are named by: the referenced
    // value 2 need not be the row keyed 2.
    let mut table = self_referencing(false);
    table.columns.insert("code".into(), Column::new(int()));
    table.unique.insert(
        "uq_code".into(),
        pbps_model::UniqueConstraint {
            columns: vec!["code".into()],
            storage_parameters: Default::default(),
        },
    );
    table.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["pid".into()],
            references_table: t("c"),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::default(),
            on_update: ReferentialAction::default(),
        },
    );
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("c"), table.clone());
    after.tables.insert(t("c"), table);
    let ids = Plan::ids(&["c"], &[("c", "id"), ("c", "pid"), ("c", "code")]);
    let plan = Plan {
        logical: vec![child_row(1, Some(2)), child_row(2, None)],
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(
        run(
            &ops,
            &fx,
            &baseline,
            &[0, 1],
            &ModuleDeps::new(),
            Checks::ALL
        ),
        []
    );
}

#[test]
fn a_replica_identity_target_made_usable_by_the_plan_is_switched_to_last() {
    use pbps_model::{Index, IndexColumn, IndexKey, IndexMethod, ReplicaIdentity};
    let index = |column: &str| Index {
        columns: vec![IndexColumn {
            key: IndexKey::Column(column.into()),
            descending: false,
            opclass: None,
        }],
        include: vec![],
        unique: true,
        filter: None,
        method: IndexMethod::Btree,
        storage_parameters: Default::default(),
    };
    let nullable = |not_null: bool| {
        let mut table = table_with(&[]);
        let mut a = Column::new(int());
        a.nullable = !not_null;
        let mut b = Column::new(int());
        b.nullable = false;
        table.columns.insert("a".into(), a);
        table.columns.insert("b".into(), b);
        table.indexes.insert("ix_new".into(), index("a"));
        table
    };
    let mut was = nullable(false);
    was.indexes.insert("ix_old".into(), index("b"));
    was.replica_identity = Some(ReplicaIdentity::Index("ix_old".into()));
    let mut will = nullable(true);
    will.replica_identity = Some(ReplicaIdentity::Index("ix_new".into()));
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), was);
    after.tables.insert(t("t"), will);
    let tid = uid(UidKind::Table, "t");
    let switch = |to| {
        p(Change::SetReplicaIdentity {
            uid: tid.clone(),
            table: t("t"),
            to: Some(to),
        })
    };
    let plan = Plan {
        // The differ's order: FULL, drop the old index, tighten, then switch.
        logical: vec![
            switch(ReplicaIdentity::Full),
            p(Change::DropIndex {
                table: t("t"),
                name: "ix_old".into(),
            }),
            p(Change::AlterColumnNullability {
                uid: uid(UidKind::Column, "t.a"),
                column: t("t").column("a"),
                ty: int(),
                to_nullable: false,
                collation: None,
                through_external_partitions: false,
            }),
            switch(ReplicaIdentity::Index("ix_new".into())),
        ],
        before,
        after,
        before_ids: Plan::ids(&["t"], &[("t", "a"), ("t", "b")]),
        after_ids: Plan::ids(&["t"], &[("t", "a"), ("t", "b")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(
        run(
            &ops,
            &fx,
            &baseline,
            &[0, 1, 2, 3],
            &ModuleDeps::new(),
            Checks::ALL
        ),
        []
    );
    // Negative (#1998 item 4): switched before the tightening, the identity
    // would select an index over a nullable column.
    assert!(
        !run(
            &ops,
            &fx,
            &baseline,
            &[0, 1, 3, 2],
            &ModuleDeps::new(),
            Checks::ALL
        )
        .is_empty()
    );
}

/// `t(id, n)` keyed by `id`, `n` nullable before and NOT NULL after.
fn tightened_column(logical: Vec<PlannedChange>) -> Plan {
    let table = |nullable: bool| {
        let mut table = table_with(&["id"]);
        let mut n = Column::new(int());
        n.nullable = nullable;
        table.columns.insert("n".into(), n);
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), table(true));
    after.tables.insert(t("t"), table(false));
    let ids = Plan::ids(&["t"], &[("t", "id"), ("t", "n")]);
    Plan {
        logical,
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

fn tighten() -> PlannedChange {
    p(Change::AlterColumnNullability {
        uid: uid(UidKind::Column, "t.n"),
        column: t("t").column("n"),
        ty: int(),
        to_nullable: false,
        collation: None,
        through_external_partitions: false,
    })
}

#[test]
fn a_tightening_followed_by_a_non_null_write_is_a_valid_order() {
    let insert = p(Change::InsertRow {
        table: t("t"),
        key_column: "id".into(),
        identity_key: false,
        key: RowKey::from("1"),
        row: [
            ("id".to_owned(), Value::Int(1)),
            ("n".to_owned(), Value::Int(5)),
        ]
        .into_iter()
        .collect::<Row>(),
        defaults: Default::default(),
        types: Default::default(),
    });
    let plan = tightened_column(vec![tighten(), insert]);
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(
        run(
            &ops,
            &fx,
            &baseline,
            &[0, 1],
            &ModuleDeps::new(),
            Checks::ALL
        ),
        []
    );
}

#[test]
fn a_recomputed_expression_runs_before_the_tightening_that_would_refuse_its_nulls() {
    let recompute = p(Change::AlterColumnExpression {
        uid: uid(UidKind::Column, "t.n"),
        column: t("t").column("n"),
        from: "1".into(),
        to: "2".into(),
    });
    let plan = tightened_column(vec![recompute, tighten()]);
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

#[test]
fn a_long_chain_of_annotated_modules_is_ordered_without_scanning_every_pair() {
    let n = 800;
    let modules: Vec<(ModuleId, Module)> = (0..n).map(|i| view(&format!("v{i}"))).collect();
    let logical: Vec<PlannedChange> = modules
        .iter()
        .map(|(id, module)| {
            p(Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            })
        })
        .collect();
    let declared: ModuleDeps = (1..n)
        .map(|i| {
            (
                modules[i].0.clone(),
                BTreeSet::from([modules[i - 1].0.clone()]),
            )
        })
        .collect();
    let plan = Plan {
        logical,
        before: Schema::default(),
        after: Schema::default(),
        before_ids: IdsFile::default(),
        after_ids: IdsFile::default(),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let order: Vec<usize> = (0..n).collect();
    assert_eq!(
        run(&ops, &fx, &baseline, &order, &declared, Checks::ALL),
        []
    );
    // Negative: the chain still orders, reversed.
    let reversed: Vec<usize> = (0..n).rev().collect();
    assert!(!run(&ops, &fx, &baseline, &reversed, &declared, Checks::ALL).is_empty());
}

/// `t(a)` with a computed column `c` over `a`, a view `v`, and a plan that
/// drops `c` and then does `second`, whose requirement that `c` is gone is
/// added by hand (the SQL Server effects are in another crate).
fn computed_then(second: Change, id: Option<ModuleId>) -> (Operations, Vec<Effects>, Baseline) {
    use pbps_model::ComputedColumn;
    use pbps_model::operation::PartKey;
    let computed = ComputedColumn {
        expression: "[a]*2".into(),
        persisted: false,
        not_null: false,
    };
    let mut table = table_with(&["a"]);
    table.computed.insert("c".into(), computed.clone());
    let mut before = Schema::default();
    before.tables.insert(t("t"), table);
    if let Some(id) = &id {
        before.modules.insert(id.clone(), view("v").1);
    }
    let mut after = Schema::default();
    after.tables.insert(t("t"), table_with(&["a"]));
    if let Some(id) = &id {
        after.modules.insert(id.clone(), view("v").1);
    }
    let plan = Plan {
        logical: vec![
            p(Change::DropComputedColumn {
                table: t("t"),
                name: "c".into(),
                computed,
            }),
            p(second),
        ],
        before,
        after,
        before_ids: Plan::ids(&["t"], &[("t", "a")]),
        after_ids: Plan::ids(&["t"], &[]),
    };
    let (ops, mut fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let names = PlanNames::of(&plan.logical, &plan.after_ids).unwrap();
    let owners = PlanOwners::new(names, &plan.before_ids, &plan.after_ids);
    let snapshot = Snapshot {
        schema: &plan.before,
        ids: &plan.before_ids,
    };
    let key = pbps_model::operation::base_table_named(&owners, snapshot, &t("t"))
        .unwrap()
        .part(PartKey::Computed("c".into()));
    fx[1].requires.push(Requirement::Absent(key));
    (ops, fx, baseline)
}

#[test]
fn a_function_change_ahead_of_a_computed_column_is_connected_and_a_column_change_is_not() {
    use super::shadow::{Shadowed, Stage, judge};
    let judged_at =
        |stage: Stage, ops: &Operations, fx: &[Effects], baseline: &Baseline, order: &[usize]| {
            let reordered = ops.reordered(order);
            let effects: Vec<Effects> = order.iter().map(|&i| fx[i].clone()).collect();
            let report = Shadowed {
                effects,
                unowned: Vec::new(),
            }
            .report(&reordered, baseline, &ModuleDeps::new());
            judge(reordered.as_slice(), &report, stage)
        };
    let judged = |ops: &Operations, fx: &[Effects], baseline: &Baseline, order: &[usize]| {
        judged_at(Stage::Differ, ops, fx, baseline, order)
    };
    // The function's alteration ahead of the column's drop: the connected
    // pass moves it, so it is recorded and does not fail.
    let (id, module) = view("v");
    let (ops, fx, baseline) = computed_then(
        Change::AlterModule {
            id: id.clone(),
            module: Box::new(module),
        },
        Some(id),
    );
    let (lines, fails) = judged(&ops, &fx, &baseline, &[1, 0]);
    assert!(!fails, "{lines:?}");
    assert!(
        lines.iter().any(|l| l.starts_with("connected ")),
        "{lines:?}"
    );
    // Negative: on the order that runs nothing moves it afterwards, so the
    // same order fails there.
    let (lines, fails) = judged_at(Stage::Final, &ops, &fx, &baseline, &[1, 0]);
    assert!(fails, "{lines:?}");
    assert!(
        lines.iter().all(|l| !l.starts_with("connected ")),
        "{lines:?}"
    );
    // Negative: the same requirement on a column drop is an order the
    // differ itself makes, and fails.
    let (ops, fx, baseline) = computed_then(
        Change::DropColumn {
            uid: uid(UidKind::Column, "t.a"),
            column: t("t").column("a"),
        },
        None,
    );
    let (lines, fails) = judged(&ops, &fx, &baseline, &[1, 0]);
    assert!(fails, "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("present ")), "{lines:?}");
}

fn update_pid(id: i64, parent: i64) -> PlannedChange {
    use pbps_model::Cell;
    p(Change::UpdateRow {
        table: t("c"),
        key_column: "id".into(),
        key: RowKey::from(id.to_string().as_str()),
        columns: [(
            "pid".to_owned(),
            (Cell::Value(Value::Null), Cell::Value(Value::Int(parent))),
        )]
        .into_iter()
        .collect(),
        unchanged: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: Default::default(),
    })
}

#[test]
fn two_standing_rows_updated_to_reference_each_other_are_not_a_cycle() {
    let plan = rows_of_c(true, true, vec![update_pid(1, 2), update_pid(2, 1)]);
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    let g = graph(ops.as_slice(), &fx, &baseline, &none, Checks::ALL);
    assert!(!g.has_cycle(2), "{:?}", g.edges);
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: an update of a row the plan inserts still follows the insert.
    let plan = rows_of_c(true, true, vec![child_row(1, None), update_pid(1, 1)]);
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

#[test]
fn a_primary_key_over_a_column_the_plan_tightens_follows_the_tightening() {
    let mut was = table_with(&["id"]);
    let mut n = Column::new(int());
    n.nullable = true;
    was.columns.insert("n".into(), n);
    let mut will = was.clone();
    will.columns.get_mut("n").unwrap().nullable = false;
    will.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["n".into()],
        storage_parameters: Default::default(),
    });
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), was);
    after.tables.insert(t("t"), will.clone());
    let ids = Plan::ids(&["t"], &[("t", "id"), ("t", "n")]);
    let plan = Plan {
        logical: vec![
            tighten(),
            p(Change::SetPrimaryKey {
                table: t("t"),
                from: None,
                to: will.primary_key,
                nonclustered: false,
            }),
        ],
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

#[test]
fn a_report_that_cannot_be_written_is_an_error_and_not_an_empty_report() {
    let dir = std::env::temp_dir().join(format!("pbps-shadow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A directory is not a file the report can be appended to.
    assert!(super::shadow::write_report(&dir, "line\n").is_err());
    let file = dir.join("report.log");
    super::shadow::write_report(&file, "a\n").unwrap();
    super::shadow::write_report(&file, "b\n").unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_dropped_column_takes_its_default_with_it_so_a_later_insert_is_valid() {
    use pbps_model::ValueSource;
    let mut was = table_with(&["id"]);
    let mut d = Column::new(int());
    d.value_source = ValueSource::Default("1".into());
    was.columns.insert("d".into(), d);
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), was);
    after.tables.insert(t("t"), table_with(&["id"]));
    let insert = p(Change::InsertRow {
        table: t("t"),
        key_column: "id".into(),
        identity_key: false,
        key: RowKey::from("1"),
        row: [("id".to_owned(), Value::Int(1))]
            .into_iter()
            .collect::<Row>(),
        defaults: Default::default(),
        types: Default::default(),
    });
    let plan = Plan {
        logical: vec![
            p(Change::DropColumn {
                uid: uid(UidKind::Column, "t.d"),
                column: t("t").column("d"),
            }),
            insert,
        ],
        before,
        after,
        before_ids: Plan::ids(&["t"], &[("t", "id"), ("t", "d")]),
        after_ids: Plan::ids(&["t"], &[("t", "id")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: the insert before the drop would take the column's default.
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

#[test]
fn the_scheduled_observer_accepts_the_right_order_and_refuses_a_wrong_one() {
    use super::shadow::{Stage, observe_scheduled};
    // The observer panics on a violation unless a shadow file collects them,
    // so the property is only observable without one.
    if std::env::var_os("PBPS_ORDER_SHADOW").is_some() {
        return;
    }
    let plan = two_dropped_tables();
    let run = |order: &[usize], stage: Stage| {
        let changes = pbps_model::ChangeSet {
            changes: order.iter().map(|&i| plan.logical[i].clone()).collect(),
        };
        std::panic::catch_unwind(|| {
            observe_scheduled(
                crate::Side {
                    schema: &plan.before,
                    ids: &plan.before_ids,
                },
                crate::Side {
                    schema: &plan.after,
                    ids: &plan.after_ids,
                },
                &pbps_dialect::MinimalDialect,
                &ModuleDeps::new(),
                &changes,
                stage,
                None,
            );
        })
        .is_ok()
    };
    assert!(run(&[0, 1], Stage::Final), "the child first is accepted");
    assert!(run(&[0, 1], Stage::Differ));
    assert!(!run(&[1, 0], Stage::Final), "the parent first is refused");
    assert!(!run(&[1, 0], Stage::Differ));
}

/// `t.id` retyped to `bigint` and its numbering source changed from `from`
/// to `to`, the numbering listed first.
fn numbered_and_retyped(from: pbps_model::ValueSource, to: pbps_model::ValueSource) -> Plan {
    let table = |source: pbps_model::ValueSource, ty: &str| {
        let mut table = Table::default();
        let mut column = Column::new(ty.parse().unwrap());
        column.nullable = false;
        column.value_source = source;
        table.columns.insert("id".into(), column);
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), table(from.clone(), "int"));
    after.tables.insert(t("t"), table(to.clone(), "bigint"));
    let id = uid(UidKind::Column, "t.id");
    let ids = Plan::ids(&["t"], &[("t", "id")]);
    Plan {
        logical: vec![
            p(Change::AlterColumnNumbering {
                uid: id.clone(),
                column: t("t").column("id"),
                ty: "bigint".parse().unwrap(),
                from,
                to,
            }),
            p(Change::AlterColumnType {
                uid: id,
                column: t("t").column("id"),
                from: int(),
                to: "bigint".parse().unwrap(),
                from_nullable: false,
                to_nullable: false,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
            }),
        ],
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

#[test]
fn a_numbering_source_given_up_goes_before_the_retype_and_one_put_on_goes_after() {
    use pbps_model::{Identity, ValueSource};
    let identity = ValueSource::Identity(Identity::new(1, 1));
    let none = ModuleDeps::new();
    // Given up for a default the new type takes (DEC-1889.1, rank -3 before -1).
    let plan = numbered_and_retyped(identity.clone(), ValueSource::Default("0".into()));
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL),
        [],
        "the numbering first is accepted"
    );
    // An identity turned into an owned sequence goes first too (rank -3).
    let serial = ValueSource::Sequence(pbps_model::OwnedSequence::serial());
    let plan = numbered_and_retyped(identity.clone(), serial);
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL),
        [],
        "an identity turned into a sequence goes before the retype"
    );
    // Negative: put on a column the plan retypes, the sequence is created
    // `AS` the final type, so the numbering first is refused and the retype
    // first accepted.
    let plan = numbered_and_retyped(ValueSource::None, identity);
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    assert!(
        !run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL).is_empty(),
        "a source put on before the retype is refused"
    );
    assert_eq!(run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL), []);
}

#[test]
fn a_key_the_baseline_lacks_is_ended_after_the_step_that_makes_it() {
    // Three steps over a key the baseline does not hold: 0 makes it, 1 ends it,
    // 2 makes it again, as a default dropped and set around a module change.
    let (ops, fx, baseline) = attached_partition().analysed(ModuleEdits::InPlace);
    let key = fx
        .iter()
        .flat_map(|e| e.creates.iter().chain(&e.removes))
        .find_map(|r| match r {
            Resource::Object(key, _) | Resource::Column(key) => {
                (baseline.holds(key) == Some(false)).then(|| key.clone())
            }
        })
        .expect("a key the plan makes and the baseline lacks");
    let make = Effects {
        creates: vec![Resource::Object(key.clone(), Incarnation::Final)],
        ..Effects::default()
    };
    let end = Effects {
        removes: vec![Resource::Object(key.clone(), Incarnation::Base)],
        ..Effects::default()
    };
    let fx = [make.clone(), end, make];
    let none = ModuleDeps::new();
    // The second making is a later incarnation, so it follows the ending.
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1, 2], &none, Checks::ALL),
        []
    );
    // Negative: the ending cannot come before the making it ends.
    let reversed = run(&ops, &fx, &baseline, &[1, 0, 2], &none, Checks::ALL);
    assert!(
        reversed.iter().any(|v| matches!(v, Violation::Edge { .. })),
        "{reversed:?}"
    );
    // Negative: with the baseline guard off the key counts as held by it, and
    // the ending must precede the making.
    let held = Checks {
        baseline: false,
        ..Checks::ALL
    };
    assert!(!run(&ops, &fx, &baseline, &[0, 1, 2], &none, held).is_empty());
}

/// `par.v` has its default changed from `1` to `2`, and the standing
/// partition `pt` is set to the parent's text (op 1) and to a default of its
/// own (op 2): ops 0 (parent), 1 (take back), 2 (own).
fn parent_default_changes_under_a_partition() -> Plan {
    use pbps_model::{PartitionBound, PartitionColumn, PartitionOf, ValueSource};
    let parent = |default: &str| {
        let mut table = table_with(&[]);
        let mut column = Column::new(int());
        column.value_source = ValueSource::Default(default.into());
        table.columns.insert("v".into(), column);
        table
    };
    let member = |own: Option<&str>| {
        let mut table = table_with(&[]);
        table.partition_of = Some(PartitionOf {
            parent: t("par"),
            bound: PartitionBound::Default,
            columns: own
                .map(|d| {
                    (
                        "v".to_owned(),
                        PartitionColumn {
                            default: Some(d.into()),
                            not_null: false,
                        },
                    )
                })
                .into_iter()
                .collect(),
        });
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("par"), parent("1"));
    before.tables.insert(t("pt"), member(None));
    after.tables.insert(t("par"), parent("2"));
    after.tables.insert(t("pt"), member(Some("9")));
    let ids = Plan::ids(&["par", "pt"], &[("par", "v")]);
    let pt = uid(UidKind::Table, "pt");
    let set = |to: Option<&str>| {
        p(Change::SetPartitionDefault {
            uid: pt.clone(),
            table: t("pt"),
            parent: t("par"),
            column: "v".into(),
            from: None,
            to: to.map(Into::into),
            fallback: Some("2".into()),
        })
    };
    Plan {
        logical: vec![
            p(Change::AlterColumnDefault {
                uid: uid(UidKind::Column, "par.v"),
                column: t("par").column("v"),
                from: Some("1".into()),
                to: Some("2".into()),
            }),
            set(None),
            set(Some("9")),
        ],
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

#[test]
fn a_partition_taking_back_its_parents_default_does_not_wait_for_the_parents_change() {
    let (ops, fx, baseline) =
        parent_default_changes_under_a_partition().analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    // The take-back writes the parent's own text, which the parent's
    // recursion writes again whichever runs first (DEC-1954.9).
    for order in [[0, 1, 2], [1, 0, 2]] {
        assert_eq!(
            run(&ops, &fx, &baseline, &order, &none, Checks::ALL),
            [],
            "{order:?}"
        );
    }
    // Negative: a default of the partition's own is overwritten by the
    // parent's change, so it still follows it.
    let early = run(&ops, &fx, &baseline, &[2, 0, 1], &none, Checks::ALL);
    assert!(!early.is_empty(), "{early:?}");
}

#[test]
fn a_partition_created_beside_a_parent_column_add_runs_in_either_order() {
    use pbps_model::{PartitionBound, PartitionOf};
    let member = {
        let mut table = table_with(&[]);
        table.partition_of = Some(PartitionOf {
            parent: t("par"),
            bound: PartitionBound::Default,
            columns: Default::default(),
        });
        table
    };
    let mut after = Schema::default();
    after.tables.insert(t("par"), table_with(&["id", "extra"]));
    after.tables.insert(t("pt"), member.clone());
    let plan = Plan {
        logical: vec![
            p(Change::CreateTable {
                uid: uid(UidKind::Table, "par"),
                name: t("par"),
                table: Box::new(table_with(&["id"])),
                beside_default: None,
                moves_at_most: None,
            }),
            p(Change::CreateTable {
                uid: uid(UidKind::Table, "pt"),
                name: t("pt"),
                table: Box::new(member),
                beside_default: None,
                moves_at_most: None,
            }),
            p(Change::AddColumn {
                uid: uid(UidKind::Column, "par.extra"),
                table: t("par"),
                name: "extra".into(),
                column: Box::new(Column::new(int())),
            }),
        ],
        before: Schema::default(),
        after,
        before_ids: IdsFile::default(),
        after_ids: Plan::ids(&["par", "pt"], &[("par", "id"), ("par", "extra")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    // The parent's column add recurses into a partition created before it, and
    // a partition created after copies the column (DEC-1954.9).
    for order in [[0, 1, 2], [0, 2, 1]] {
        assert_eq!(
            run(&ops, &fx, &baseline, &order, &none, Checks::ALL),
            [],
            "{order:?}"
        );
    }
    // Negative: the parent table itself must exist first.
    let early = run(&ops, &fx, &baseline, &[1, 0, 2], &none, Checks::ALL);
    assert!(!early.is_empty(), "{early:?}");
}

/// The effect graph over `order`, settled with either tie-break: the
/// positions in `order` that the settled plan takes, in turn.
fn settled(
    ops: &Operations,
    fx: &[Effects],
    baseline: &Baseline,
    order: &[usize],
    hoist: bool,
) -> Vec<usize> {
    let reordered = ops.reordered(order);
    let effects: Vec<Effects> = order.iter().map(|&i| fx[i].clone()).collect();
    let g = graph(
        reordered.as_slice(),
        &effects,
        baseline,
        &ModuleDeps::new(),
        Checks::ALL,
    );
    let mut schedule = super::schedule::Schedule::of_graph(&g, reordered.as_slice());
    if hoist {
        schedule = schedule.hoisting();
    }
    let changes = pbps_model::ChangeSet {
        changes: reordered.iter().map(|o| o.planned().clone()).collect(),
    };
    let settled = schedule.settle(changes).unwrap();
    settled.sequence.iter().map(|&i| order[i]).collect()
}

/// #1956: two writes of one resource keep their base order whichever
/// tie-break settles them, from either order they arrive in.
#[test]
fn two_writes_to_one_resource_settle_in_their_base_order_under_either_tie_break() {
    let plan = two_rows_of_one_table();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    for hoist in [false, true] {
        for order in [[0, 1], [1, 0]] {
            assert_eq!(
                settled(&ops, &fx, &baseline, &order, hoist),
                [0, 1],
                "{order:?} hoist={hoist}"
            );
        }
    }
}

/// #1956: whatever order a plan arrives in, settling its effect graph gives
/// an order the replay accepts, and settling that again changes nothing.
#[test]
fn every_settled_order_passes_the_replay_and_settles_to_itself() {
    let none = ModuleDeps::new();
    for plan in [
        create_then_add_column(),
        two_dropped_tables(),
        two_rows_of_one_table(),
    ] {
        let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
        let n = ops.as_slice().len();
        let orders: Vec<Vec<usize>> = if n == 2 {
            vec![vec![0, 1], vec![1, 0]]
        } else {
            vec![(0..n).collect()]
        };
        for order in orders {
            for hoist in [false, true] {
                let out = settled(&ops, &fx, &baseline, &order, hoist);
                assert_eq!(
                    run(&ops, &fx, &baseline, &out, &none, Checks::ALL),
                    [],
                    "{order:?} -> {out:?}"
                );
                assert_eq!(settled(&ops, &fx, &baseline, &out, hoist), out);
            }
        }
        // Negative: the order the plan arrives in is refused where it is
        // not the settled one.
        if n == 2 {
            assert_ne!(run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL), []);
        }
    }
}

/// DEC-1956.1: the reason of an effect edge is the effect's, not the
/// source's; a write-write pair is identity on an object and data on values
/// or rows; a search's proposal is a handover.
#[test]
fn each_edge_maps_to_one_sealed_reason_by_the_effect_behind_it() {
    use super::edges::{Edge, Source};
    use super::schedule::reason;
    use pbps_model::resolver::OrderReason;
    let plan = two_rows_of_one_table();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let g = graph(
        ops.as_slice(),
        &fx,
        &baseline,
        &ModuleDeps::new(),
        Checks::ALL,
    );
    let ww = g
        .edges
        .iter()
        .find(|e| matches!(e.because, Because::WriteWrite(_)))
        .expect("two rows of one table are two writes of it");
    assert_eq!(reason(ww, ops.as_slice()), OrderReason::Data);
    // The table's key, as the effects hold it: no caller can make one up.
    let table = fx[0]
        .requires
        .iter()
        .find_map(|r| {
            if let Requirement::Exists(key @ ObjectKey::Table(_)) = r {
                Some(key.clone())
            } else {
                None
            }
        })
        .expect("a row requires its table");
    let edge = |source, because| Edge {
        before: 0,
        after: 1,
        source,
        because,
    };
    let of = |source, because| reason(&edge(source, because), ops.as_slice());
    assert_eq!(
        of(
            Source::Effect,
            Because::WriteWrite(Resource::Object(table.clone(), Incarnation::Final))
        ),
        OrderReason::Identity
    );
    assert_eq!(
        of(
            Source::Effect,
            Because::WriteWrite(Resource::Column(table.clone()))
        ),
        OrderReason::Data
    );
    assert_eq!(
        of(Source::Effect, Because::Creates(table.clone())),
        OrderReason::Structural
    );
    assert_eq!(
        of(Source::Effect, Because::Replaces(table.clone())),
        OrderReason::Identity
    );
    // The source decides only where it is the whole story.
    assert_eq!(
        of(Source::Binding, Because::Creates(table.clone())),
        OrderReason::Binding
    );
    assert_eq!(
        of(Source::Search, Because::Creates(table.clone())),
        OrderReason::Identity
    );
    assert_eq!(
        of(Source::Declared, Because::Declared),
        OrderReason::Structural
    );
    // Negative: a catalog edge is read by its effect like any other.
    assert_eq!(
        of(Source::Catalog, Because::Writes(table)),
        OrderReason::Data
    );
}

// The effects #1998 found too narrow: each order below was verified clean
// although the engine refuses it, or it runs to a different result.

/// `t(id, n)` keyed by `id`, under the check `ck` the baseline and the
/// desired state give as here, with one row `1` updated as `update` says.
fn checked(was: &str, will: &str, update: pbps_model::Cell) -> Plan {
    let table = |check: &str| {
        let mut table = table_with(&["id", "n"]);
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table.checks.insert(
            "ck".into(),
            pbps_model::CheckConstraint {
                expression: check.into(),
            },
        );
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), table(was));
    after.tables.insert(t("t"), table(will));
    let ids = Plan::ids(&["t"], &[("t", "id"), ("t", "n")]);
    let mut logical = vec![p(Change::UpdateRow {
        table: t("t"),
        key_column: "id".into(),
        key: RowKey::from("1"),
        columns: [(
            "n".to_owned(),
            (pbps_model::Cell::Value(Value::Int(5)), update),
        )]
        .into_iter()
        .collect(),
        unchanged: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: Default::default(),
    })];
    if was != will {
        logical.insert(
            0,
            p(Change::DropCheck {
                table: t("t"),
                name: "ck".into(),
            }),
        );
        logical.push(p(Change::AddCheck {
            table: t("t"),
            name: "ck".into(),
            constraint: pbps_model::CheckConstraint {
                expression: will.into(),
            },
        }));
    }
    Plan {
        logical,
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

/// #1998 item 1: a check replaced under its own name stands in the way of a
/// row the old one would refuse, as a dropped one does.
#[test]
fn a_row_update_waits_for_a_check_replaced_under_its_name_and_not_for_one_kept() {
    let none = ModuleDeps::new();
    let minus = pbps_model::Cell::Value(Value::Int(-1));
    let plan = checked("n > 0", "n < 0", minus.clone());
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    // Drop, update, add: the row is written while no check stands.
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1, 2], &none, Checks::ALL),
        []
    );
    // Negative: the update before the old check's drop.
    let early = run(&ops, &fx, &baseline, &[1, 0, 2], &none, Checks::ALL);
    assert!(
        early.iter().any(|v| matches!(
            v,
            Violation::StillPresent {
                key: ObjectKey::Part {
                    part: pbps_model::operation::PartKey::Check(n),
                    ..
                },
                ..
            } if n == "ck"
        )),
        "{early:?}"
    );
    // A check the plan keeps is no obligation of the row's.
    let kept = checked("n > 0", "n > 0", pbps_model::Cell::Value(Value::Int(7)));
    let (_, fx, _) = kept.analysed(ModuleEdits::InPlace);
    assert!(
        !fx[0]
            .requires
            .iter()
            .any(|r| matches!(r, Requirement::Absent(_))),
        "{:?}",
        fx[0].requires
    );
}

/// #1998 item 1, the comparison the differ makes: a unique constraint over a
/// column the plan renames, or one whose storage parameters alone change,
/// is kept and not replaced, so a row written beside it waits for nothing.
#[test]
fn a_unique_constraint_kept_through_a_rename_or_a_parameter_change_is_not_replaced() {
    let unique = |column: &str, fill: Option<&str>| pbps_model::UniqueConstraint {
        columns: vec![column.into()],
        storage_parameters: fill
            .map(|f| [("fillfactor".to_owned(), f.to_owned())].into())
            .unwrap_or_default(),
    };
    let requires_absent =
        |was: pbps_model::UniqueConstraint, will: pbps_model::UniqueConstraint, renamed: bool| {
            let table = |u: pbps_model::UniqueConstraint, n: &str| {
                let mut table = table_with(&["id", n]);
                table.unique.insert("u".into(), u);
                table
            };
            let now = if renamed { "m" } else { "n" };
            let (mut before, mut after) = (Schema::default(), Schema::default());
            before.tables.insert(t("t"), table(was, "n"));
            after.tables.insert(t("t"), table(will, now));
            let column = uid(UidKind::Column, "t.n");
            let ids = |name: &str| {
                let mut ids = Plan::ids(&["t"], &[("t", "id")]);
                ids.columns.insert(column.clone(), t("t").column(name));
                ids
            };
            let plan = Plan {
                logical: vec![row("t", "1")],
                before,
                after,
                before_ids: ids("n"),
                after_ids: ids(now),
            };
            let (_, fx, _) = plan.analysed(ModuleEdits::InPlace);
            fx[0].requires.iter().any(|r| {
                matches!(
                    r,
                    Requirement::Absent(ObjectKey::Part {
                        part: pbps_model::operation::PartKey::Unique(_),
                        ..
                    })
                )
            })
        };
    assert!(!requires_absent(unique("n", None), unique("m", None), true));
    assert!(!requires_absent(
        unique("n", Some("70")),
        unique("n", Some("90")),
        false
    ));
    // Negative: one moved to another column is replaced.
    assert!(requires_absent(
        unique("n", None),
        unique("id", None),
        false
    ));
}

/// #1998 item 2: `SET n = DEFAULT` writes the default the plan leaves, so it
/// follows the step that installs it.
#[test]
fn a_row_updated_to_its_default_follows_the_default_it_takes() {
    use pbps_model::ValueSource;
    let mut plan = checked("n > 0", "n > 0", pbps_model::Cell::Default("3".into()));
    plan.after
        .tables
        .get_mut(&t("t"))
        .unwrap()
        .columns
        .get_mut("n")
        .unwrap()
        .value_source = ValueSource::Default("3".into());
    plan.logical.insert(
        0,
        p(Change::AlterColumnDefault {
            uid: uid(UidKind::Column, "t.n"),
            column: t("t").column("n"),
            from: None,
            to: Some("3".into()),
        }),
    );
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: the update before the default is set.
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

/// #1998 items 3 and 9: a new foreign key validates the child's values
/// against the parent's rows, so it follows a parent row the plan inserts.
#[test]
fn a_foreign_key_added_follows_the_parent_rows_the_plan_writes() {
    let parent = {
        let mut table = table_with(&["id"]);
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table
    };
    let key = ForeignKey {
        columns: vec!["pid".into()],
        references_table: t("p"),
        references_columns: vec!["id".into()],
        on_delete: ReferentialAction::default(),
        on_update: ReferentialAction::default(),
    };
    let mut child = table_with(&["pid"]);
    let mut before = Schema::default();
    before.tables.insert(t("p"), parent.clone());
    before.tables.insert(t("c"), child.clone());
    child.foreign_keys.insert("fk".into(), key.clone());
    let mut after = Schema::default();
    after.tables.insert(t("p"), parent);
    after.tables.insert(t("c"), child);
    let ids = Plan::ids(&["p", "c"], &[("p", "id"), ("c", "pid")]);
    let plan = Plan {
        logical: vec![
            p(Change::InsertRow {
                table: t("p"),
                key_column: "id".into(),
                identity_key: false,
                key: RowKey::from("1"),
                row: [("id".to_owned(), Value::Int(1))]
                    .into_iter()
                    .collect::<Row>(),
                defaults: Default::default(),
                types: Default::default(),
            }),
            p(Change::AddForeignKey {
                table: t("c"),
                name: "fk".into(),
                constraint: Box::new(key),
            }),
        ],
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: the key before the parent row it would check against.
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

/// #1998 item 6: a key the baseline lacks, made and then ended by the plan,
/// is gone after the ending: a step that needs the plan's instance cannot
/// follow it.
#[test]
fn a_key_the_plan_makes_and_ends_is_not_found_after_its_ending() {
    let (ops, fx, baseline) = attached_partition().analysed(ModuleEdits::InPlace);
    let key = fx
        .iter()
        .flat_map(|e| e.creates.iter().chain(&e.removes))
        .find_map(|r| match r {
            Resource::Object(key, _) | Resource::Column(key) => {
                (baseline.holds(key) == Some(false)).then(|| key.clone())
            }
        })
        .expect("a key the plan makes and the baseline lacks");
    let make = Effects {
        creates: vec![Resource::Object(key.clone(), Incarnation::Final)],
        ..Effects::default()
    };
    let end = Effects {
        removes: vec![Resource::Object(key.clone(), Incarnation::Base)],
        ..Effects::default()
    };
    let need = Effects {
        requires: vec![Requirement::Is(key.clone(), Incarnation::Final)],
        ..Effects::default()
    };
    let none = ModuleDeps::new();
    let replay = |order: [usize; 3]| {
        let fx = [make.clone(), end.clone(), need.clone()];
        let at: Vec<usize> = order.to_vec();
        let reordered = ops.reordered(&at);
        let effects: Vec<Effects> = at.iter().map(|&i| fx[i].clone()).collect();
        super::verify::verify_with(
            reordered.as_slice(),
            &effects,
            &baseline,
            &none,
            Checks::ALL,
        )
    };
    // Made, needed, ended.
    assert!(
        !replay([0, 2, 1])
            .iter()
            .any(|v| matches!(v, Violation::Unmet { .. })),
        "{:?}",
        replay([0, 2, 1])
    );
    // Negative: needed after its ending.
    assert!(
        replay([0, 1, 2]).iter().any(|v| matches!(
            v,
            Violation::Unmet {
                requirement: Requirement::Is(k, Incarnation::Final),
                ..
            } if *k == key
        )),
        "{:?}",
        replay([0, 1, 2])
    );
}

/// #1998 item 7: a default dropped from a column the plan renames reads the
/// column under the name it spells, so it stays on its side of the rename.
#[test]
fn a_default_dropped_by_the_old_name_runs_before_the_rename() {
    let mut plan = renamed_default(None);
    plan.logical = vec![
        p(Change::AlterColumnDefault {
            uid: uid(UidKind::Column, "t.a"),
            column: t("t").column("a"),
            from: Some("1".into()),
            to: None,
        }),
        p(Change::RenameColumn {
            uid: uid(UidKind::Column, "t.a"),
            table: t("t"),
            from: "a".into(),
            to: "b".into(),
            table_was: None,
        }),
    ];
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: renamed first, the drop would spell a name that is gone.
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

/// #1998 item 10: a default turned into an identity is removed by a change
/// of its own, and the numbering, whose `from` the differ gives as `None`,
/// still goes after it.
#[test]
fn a_numbering_put_on_a_column_with_a_default_follows_the_default_s_removal() {
    use pbps_model::{Identity, ValueSource};
    let table = |source: ValueSource| {
        let mut table = table_with(&[]);
        let mut n = Column::new(int());
        n.nullable = false;
        n.value_source = source;
        table.columns.insert("n".into(), n);
        table
    };
    let identity = ValueSource::Identity(Identity::new(1, 1));
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before
        .tables
        .insert(t("t"), table(ValueSource::Default("0".into())));
    after.tables.insert(t("t"), table(identity.clone()));
    let ids = Plan::ids(&["t"], &[("t", "n")]);
    let plan = Plan {
        logical: vec![
            p(Change::AlterColumnDefault {
                uid: uid(UidKind::Column, "t.n"),
                column: t("t").column("n"),
                from: Some("0".into()),
                to: None,
            }),
            p(Change::AlterColumnNumbering {
                uid: uid(UidKind::Column, "t.n"),
                column: t("t").column("n"),
                ty: int(),
                from: ValueSource::None,
                to: identity,
            }),
        ],
        before,
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    // Negative: the identity put on while the default stands.
    assert!(!run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL).is_empty());
}

/// #1998 item 4's limit: a column the plan creates NOT NULL is NOT NULL
/// from its creation, which produces no NOT NULL fact, so a replica identity
/// switched to an index over it requires none (#2111 review).
#[test]
fn a_replica_identity_switched_to_an_index_over_a_new_not_null_column_verifies_clean() {
    use pbps_model::{Index, IndexColumn, IndexKey, IndexMethod, ReplicaIdentity};
    let index = Index {
        columns: vec![IndexColumn {
            key: IndexKey::Column("a".into()),
            descending: false,
            opclass: None,
        }],
        include: vec![],
        unique: true,
        filter: None,
        method: IndexMethod::Btree,
        storage_parameters: Default::default(),
    };
    let mut a = Column::new(int());
    a.nullable = false;
    let mut will = table_with(&["id"]);
    will.columns.insert("a".into(), a.clone());
    will.indexes.insert("ix".into(), index.clone());
    will.replica_identity = Some(ReplicaIdentity::Index("ix".into()));
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), table_with(&["id"]));
    after.tables.insert(t("t"), will);
    let plan = Plan {
        logical: vec![
            p(Change::AddColumn {
                uid: uid(UidKind::Column, "t.a"),
                table: t("t"),
                name: "a".into(),
                column: Box::new(a),
            }),
            p(Change::AddIndex {
                table: t("t"),
                name: "ix".into(),
                index: Box::new(index),
                clustered: false,
            }),
            p(Change::SetReplicaIdentity {
                uid: uid(UidKind::Table, "t"),
                table: t("t"),
                to: Some(ReplicaIdentity::Index("ix".into())),
            }),
        ],
        before,
        after,
        before_ids: Plan::ids(&["t"], &[("t", "id")]),
        after_ids: Plan::ids(&["t"], &[("t", "id"), ("t", "a")]),
    };
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let none = ModuleDeps::new();
    assert_eq!(
        run(&ops, &fx, &baseline, &[0, 1, 2], &none, Checks::ALL),
        []
    );
    // Negative: the switch before the index it selects.
    assert!(!run(&ops, &fx, &baseline, &[0, 2, 1], &none, Checks::ALL).is_empty());
/// `t(a)` with a default, and a plan that takes the default off and then
/// either renames the column or retypes it. Ops 0 (the removal) and 1.
fn default_taken_off(then_rename: bool) -> Plan {
    use pbps_model::ValueSource;
    let with_default = |name: &str, default: Option<&str>, ty: &str| {
        let mut table = table_with(&["id"]);
        let mut column = Column::new(ty.parse().unwrap());
        if let Some(text) = default {
            column.value_source = ValueSource::Default(text.into());
        }
        table.columns.insert(name.into(), column);
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before
        .tables
        .insert(t("t"), with_default("a", Some("1"), "int"));
    let moved = uid(UidKind::Column, "t.a");
    let named = |column: &str| {
        let mut ids = Plan::ids(&["t"], &[("t", "id")]);
        ids.columns.insert(moved.clone(), t("t").column(column));
        ids
    };
    let removal = p(Change::AlterColumnDefault {
        uid: moved.clone(),
        column: t("t").column("a"),
        from: Some("1".into()),
        to: None,
    });
    let (second, later, ids) = if then_rename {
        after.tables.insert(t("t"), with_default("b", None, "int"));
        (
            p(Change::RenameColumn {
                uid: moved.clone(),
                table: t("t"),
                from: "a".into(),
                to: "b".into(),
                table_was: None,
            }),
            "b",
            named("b"),
        )
    } else {
        after
            .tables
            .insert(t("t"), with_default("a", None, "bigint"));
        (
            p(Change::AlterColumnType {
                uid: moved.clone(),
                column: t("t").column("a"),
                from: "int".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
            }),
            "a",
            named("a"),
        )
    };
    let _ = later;
    Plan {
        logical: vec![removal, second],
        before,
        after,
        before_ids: named("a"),
        after_ids: ids,
    }
}

/// A removal that spells the column by its old name goes before the rename
/// that moves the name; a retype of the same column is no such move, and the
/// default goes before it by `Absent` alone (DEC-1168.1, DEC-1968.2).
#[test]
fn a_base_spelled_reader_precedes_the_rename_of_what_it_names_and_not_a_retype() {
    let none = ModuleDeps::new();
    let rewrite_edges = |plan: &Plan| -> Vec<(usize, usize)> {
        let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
        graph(ops.as_slice(), &fx, &baseline, &none, Checks::ALL)
            .edges
            .iter()
            .filter(|e| matches!(e.because, Because::Rewrites(_)))
            .map(|e| (e.before, e.after))
            .collect()
    };
    let renamed = default_taken_off(true);
    assert_eq!(rewrite_edges(&renamed), [(0, 1)]);
    // The order that runs the rename first is refused by that edge.
    let (ops, fx, baseline) = renamed.analysed(ModuleEdits::InPlace);
    let violations = run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            Violation::Edge { edge, .. } if matches!(edge.because, Because::Rewrites(_))
        )),
        "{violations:?}"
    );
    // Negative: a retype of the same column draws no rewrite edge, either
    // way, so the removal is ordered by the default alone.
    let retyped = default_taken_off(false);
    assert_eq!(rewrite_edges(&retyped), []);
    let (ops, fx, baseline) = retyped.analysed(ModuleEdits::InPlace);
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
}

/// The default of `t.a` taken off under the name the rename gave the column,
/// `b`: the removal is the plan's second step, so it spells the name the
/// column has once the rename has run (DEC-1968.2, review of #2120).
fn default_taken_off_after_the_rename() -> Plan {
    use pbps_model::ValueSource;
    let table = |name: &str, default: Option<&str>| {
        let mut table = table_with(&["id"]);
        let mut column = Column::new(int());
        if let Some(text) = default {
            column.value_source = ValueSource::Default(text.into());
        }
        table.columns.insert(name.into(), column);
        table
    };
    let (mut before, mut after) = (Schema::default(), Schema::default());
    before.tables.insert(t("t"), table("a", Some("1")));
    after.tables.insert(t("t"), table("b", None));
    let moved = uid(UidKind::Column, "t.a");
    let named = |column: &str| {
        let mut ids = Plan::ids(&["t"], &[("t", "id")]);
        ids.columns.insert(moved.clone(), t("t").column(column));
        ids
    };
    Plan {
        logical: vec![
            p(Change::RenameColumn {
                uid: moved.clone(),
                table: t("t"),
                from: "a".into(),
                to: "b".into(),
                table_was: None,
            }),
            p(Change::AlterColumnDefault {
                uid: moved.clone(),
                column: t("t").column("b"),
                from: Some("1".into()),
                to: None,
            }),
        ],
        before,
        after,
        before_ids: named("a"),
        after_ids: named("b"),
    }
}

#[test]
fn a_final_spelled_reader_of_the_base_follows_the_rename_it_names_the_column_by() {
    let none = ModuleDeps::new();
    let plan = default_taken_off_after_the_rename();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let rewrites: Vec<(usize, usize)> = graph(ops.as_slice(), &fx, &baseline, &none, Checks::ALL)
        .edges
        .iter()
        .filter(|e| matches!(e.because, Because::Rewrites(_)))
        .map(|e| (e.before, e.after))
        .collect();
    assert_eq!(rewrites, [(0, 1)]);
    // The removal first would name a column that has no such name yet.
    assert_eq!(run(&ops, &fx, &baseline, &[0, 1], &none, Checks::ALL), []);
    let violations = run(&ops, &fx, &baseline, &[1, 0], &none, Checks::ALL);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            Violation::Edge { edge, .. } if matches!(edge.because, Because::Rewrites(_))
        )),
        "{violations:?}"
    );
}

/// A column renamed and retyped in one plan: the retype names the column by
/// its new name, and the foreign key into it is the baseline's, spelled by
/// the old one (review of #2120).
#[test]
fn a_retype_of_a_renamed_column_still_waits_for_the_foreign_key_into_it() {
    use pbps_model::ValueSource;
    let _ = ValueSource::None;
    let mut before = Schema::default();
    before.tables.insert(t("p"), table_with(&["id"]));
    let mut child = table_with(&["pid"]);
    child.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["pid".into()],
            references_table: t("p"),
            references_columns: vec!["id".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    before.tables.insert(t("c"), child.clone());
    let mut after = Schema::default();
    after.tables.insert(t("p"), table_with(&["key"]));
    child.foreign_keys.clear();
    after.tables.insert(t("c"), child);
    let moved = uid(UidKind::Column, "p.id");
    let mut before_ids = Plan::ids(&["p", "c"], &[("c", "pid")]);
    before_ids
        .columns
        .insert(moved.clone(), t("p").column("id"));
    let mut after_ids = Plan::ids(&["p", "c"], &[("c", "pid")]);
    after_ids
        .columns
        .insert(moved.clone(), t("p").column("key"));
    let plan = Plan {
        logical: vec![
            p(Change::RenameColumn {
                uid: moved.clone(),
                table: t("p"),
                from: "id".into(),
                to: "key".into(),
                table_was: None,
            }),
            p(Change::AlterColumnType {
                uid: moved,
                column: t("p").column("key"),
                from: int(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
            }),
            p(Change::DropForeignKey {
                table: t("c"),
                name: "fk".into(),
            }),
        ],
        before,
        after,
        before_ids,
        after_ids,
    };
    let (ops, fx, _) = plan.analysed(ModuleEdits::InPlace);
    let waits = |x: &Effects| {
        x.requires
            .iter()
            .any(|r| matches!(r, Requirement::Absent(ObjectKey::Part { .. })))
    };
    assert!(waits(&fx[1]), "{:?}", fx[1].requires);
    // Negative: a retype of a column no foreign key names waits for none.
    assert!(!waits(&fx[0]), "{:?}", fx[0].requires);
    let _ = ops;
}
