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

/// A column deprecated twice: a synthetic pair of writes to one definition,
/// whose final state depends on which runs last.
fn deprecated_twice() -> Plan {
    let mut after = Schema::default();
    after.tables.insert(t("t"), table_with(&["c"]));
    let column = |reason: &str| {
        p(Change::SetColumnDeprecated {
            uid: uid(UidKind::Column, "t.c"),
            column: t("t").column("c"),
            reason: Some(reason.parse().unwrap()),
        })
    };
    let ids = Plan::ids(&["t"], &[("t", "c")]);
    Plan {
        logical: vec![column("first"), column("second")],
        before: after.clone(),
        after,
        before_ids: ids.clone(),
        after_ids: ids,
    }
}

#[test]
fn two_writes_to_one_resource_swapped_from_their_base_order_are_refused() {
    let plan = deprecated_twice();
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
    use super::shadow::{Shadowed, judge};
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
    let (lines, fails) = judge(alone.as_slice(), &report);
    assert!(!fails, "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("unprovided ")),
        "{lines:?}"
    );
}

#[test]
fn an_obligation_some_order_could_meet_but_this_one_meets_too_late_fails_the_run() {
    use super::shadow::{Shadowed, judge};
    let plan = create_then_add_column();
    let (ops, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    let swapped = ops.reordered(&[1, 0]);
    let report = Shadowed {
        effects: vec![fx[1].clone(), fx[0].clone()],
        unowned: Vec::new(),
    }
    .report(&swapped, &baseline, &ModuleDeps::new());
    let (lines, fails) = judge(swapped.as_slice(), &report);
    assert!(fails, "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("unmet ")), "{lines:?}");
}

#[test]
fn a_dependent_that_stands_throughout_is_recorded_and_one_the_plan_removes_late_fails() {
    use super::shadow::{Shadowed, judge};
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
    let (lines, fails) = judge(ops.as_slice(), &report);
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
    let (lines, fails) = judge(swapped.as_slice(), &report);
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
