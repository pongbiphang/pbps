use super::*;
use crate::generated::{column_names, selects_all, without_variables};
use crate::validate::computed_note_fold;
use indexmap::IndexMap;
use pbps_model::{
    ChangeSet, Column, ComputedColumn, IdsFile, Module, ModuleDeps, ModuleId, Uid, UidKind,
};
use pbps_model::{ModuleKind, PlannedChange, TableName, ValueSource};

fn name(table: &str) -> TableName {
    TableName::new("app", table)
}

fn int() -> Column {
    Column::new("int".parse().unwrap())
}

fn computed(expression: &str) -> ComputedColumn {
    ComputedColumn {
        expression: expression.into(),
        persisted: false,
        not_null: false,
    }
}

fn table_with(table: &str, spec: impl FnOnce(&mut pbps_model::Table)) -> Change {
    let mut made = pbps_model::Table {
        columns: IndexMap::from([("id".into(), int()), ("v".into(), int())]),
        ..Default::default()
    };
    spec(&mut made);
    Change::CreateTable {
        uid: Uid::derived(UidKind::Table, table, 0),
        name: name(table),
        table: Box::new(made),
        beside_default: None,
        moves_at_most: None,
    }
}

fn table(table: &str, calls: &[(&str, &str)]) -> Change {
    table_with(table, |t| {
        for (column, expression) in calls {
            t.computed.insert((*column).into(), computed(expression));
        }
    })
}

fn module(kind: ModuleKind, module: &str, definition: &str) -> Change {
    // As the loader names them: a view by its name, a function by a routine
    // with no arguments (SQL Server has no overloads).
    let id = match kind {
        ModuleKind::View => ModuleId::Named(name(module)),
        ModuleKind::Function | ModuleKind::Procedure | ModuleKind::Trigger => {
            ModuleId::Routine(pbps_model::RoutineId::new(name(module), Vec::new()))
        }
    };
    Change::CreateModule {
        id,
        module: Box::new(Module {
            kind,
            definition: definition.into(),
            description: None,
            options: Default::default(),
        }),
    }
}

fn function(function: &str, definition: &str) -> Change {
    module(ModuleKind::Function, function, definition)
}

fn view(view: &str, definition: &str) -> Change {
    module(ModuleKind::View, view, definition)
}

fn build(changes: Vec<Change>) -> Result<Reconstruction, ReconstructError> {
    // The identities the declarations record, as the planner holds them: the
    // split parts of a default name their column by it.
    let mut declared = pbps_model::Schema::default();
    for change in &changes {
        if let Change::CreateTable { name, table, .. } = change {
            declared.tables.insert(name.clone(), (**table).clone());
        }
    }
    let ids = pbps_diff::resolve(
        &declared,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "reconstruct-tests".into(),
            today: "2026-10-25".into(),
        },
    )
    .unwrap()
    .ids;
    let plan = pbps_diff::order::propose(ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    });
    Reconstruction::new(&Mssql, &plan, &ids, &ModuleDeps::new())
}

fn sql(reconstruction: &Reconstruction) -> Vec<String> {
    reconstruction
        .lowered
        .statements()
        .iter()
        .map(|s| s.sql.clone())
        .collect()
}

/// The position of the one statement holding `needle`.
fn at(statements: &[String], needle: &str) -> usize {
    let found: Vec<_> = statements
        .iter()
        .enumerate()
        .filter(|(_, s)| s.contains(needle))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(found.len(), 1, "{needle:?} in {statements:#?}");
    found[0]
}

const FUNCTION: &str = "(@x int) RETURNS int AS BEGIN RETURN @x + 1; END";

#[test]
fn a_computed_column_calling_a_declared_function_follows_it_and_leaves_the_table_bare() {
    // The ordinary plan order writes the column inline in CREATE TABLE, which
    // fails 4121 when the function does not exist yet (measured on 17.0).
    for changes in [
        vec![table("t", &[("c", "app.f(v)")]), function("f", FUNCTION)],
        vec![function("f", FUNCTION), table("t", &[("c", "app.f(v)")])],
    ] {
        let statements = sql(&build(changes).unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        assert!(
            !statements[create].contains(" AS ("),
            "{}",
            statements[create]
        );
        let routine = at(&statements, "FUNCTION [app].[f]");
        let column = at(&statements, "ADD [c] AS (app.f(v)");
        // The bare table and the function may come either way round; the
        // column needs both.
        assert!(create < column && routine < column, "{statements:#?}");
    }
}

#[test]
fn a_view_reading_a_computed_column_waits_for_the_function_that_column_calls() {
    // A view binds its columns when it is created, so it cannot precede the
    // column, and the column cannot precede the function it calls.
    let statements = sql(&build(vec![
        table("t", &[("c", "app.f(v)")]),
        view("w", "SELECT id, c FROM app.t"),
        function("f", FUNCTION),
    ])
    .unwrap());
    let routine = at(&statements, "FUNCTION [app].[f]");
    let column = at(&statements, "ADD [c] AS (app.f(v)");
    let reader = at(&statements, "VIEW [app].[w]");
    assert!(routine < column && column < reader, "{statements:#?}");
}

#[test]
fn a_schema_bound_function_reading_the_table_still_compiles_before_the_column_it_serves() {
    // Measured on 17.0: table, then function, then ADD computed compiles, so
    // the held part is the column and the table is never held (no Cycle).
    let statements = sql(&build(vec![
        table("t", &[("c", "app.f(id)")]),
        function(
            "f",
            "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT COUNT(*) FROM app.t WHERE id = @x); END",
        ),
    ])
    .unwrap());
    let create = at(&statements, "CREATE TABLE [app].[t]");
    let routine = at(&statements, "FUNCTION [app].[f]");
    let column = at(&statements, "ADD [c] AS (app.f(id)");
    assert!(create < routine && routine < column, "{statements:#?}");
}

#[test]
fn a_function_that_reads_the_column_it_is_called_by_is_refused_by_name() {
    let refused = build(vec![
        table("t", &[("c", "app.f(id)")]),
        function(
            "f",
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(c) FROM app.t WHERE id = @x); END",
        ),
    ])
    .expect_err("no order compiles both");
    assert!(
        matches!(&refused, ReconstructError::Cycle(subject) if subject.contains("app")),
        "{refused:?}"
    );
}

#[test]
fn a_default_and_a_check_wait_for_every_module() {
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.columns.get_mut("v").unwrap().value_source = ValueSource::Default("app.f(1)".into());
            t.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "app.f(v) > 0".into(),
                },
            );
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let create = at(&statements, "CREATE TABLE [app].[t]");
    assert!(
        !statements[create].contains("DEFAULT"),
        "{}",
        statements[create]
    );
    let routine = at(&statements, "FUNCTION [app].[f]");
    assert!(
        routine < at(&statements, "DEFAULT (app.f(1)"),
        "{statements:#?}"
    );
    assert!(
        routine < at(&statements, "CHECK (app.f(v) > 0"),
        "{statements:#?}"
    );
}

#[test]
fn a_key_over_a_computed_column_follows_the_column_and_other_keys_stay_with_the_table() {
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.computed.insert("c".into(), computed("app.f(v)"));
            t.unique.insert(
                "uq_c".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["c".into()],
                    storage_parameters: Default::default(),
                },
            );
            t.unique.insert(
                "uq_id".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                },
            );
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let routine = at(&statements, "FUNCTION [app].[f]");
    let column = at(&statements, "ADD [c] AS (app.f(v)");
    let over = at(&statements, "[uq_c]");
    assert!(routine < column && column < over, "{statements:#?}");
    assert!(at(&statements, "[uq_id]") < routine, "{statements:#?}");
}

#[test]
fn a_bootstrap_that_alters_or_drops_is_not_a_bootstrap() {
    let refused = build(vec![Change::DropTable {
        uid: Uid::derived(UidKind::Table, "t", 0),
        name: name("t"),
        detach_from: None,
    }])
    .expect_err("a drop is no bootstrap step");
    assert!(
        matches!(&refused, ReconstructError::Unsupported(op) if op == "drop_table"),
        "{refused:?}"
    );
}

#[test]
fn roles_grants_and_rows_create_no_step() {
    let reconstruction = build(vec![
        Change::CreateRole {
            uid: Uid::derived(UidKind::Role, "r", 0),
            name: "r".into(),
        },
        table("t", &[]),
    ])
    .unwrap();
    assert_eq!(reconstruction.len(), 1);
}

#[test]
fn the_created_map_names_the_step_of_every_table_and_module() {
    let reconstruction = build(vec![
        table("t", &[("c", "app.f(v)")]),
        function("f", FUNCTION),
        view("w", "SELECT id FROM app.t"),
    ])
    .unwrap();
    let created = reconstruction.created();
    let statements = sql(&reconstruction);
    for (declared, needle) in [
        ("t", "CREATE TABLE [app].[t]"),
        ("f", "FUNCTION [app].[f]"),
        ("w", "VIEW [app].[w]"),
    ] {
        assert_eq!(
            created.get(&object("app", declared)).copied(),
            Some(at(&statements, needle)),
            "{declared}"
        );
    }
    assert_eq!(
        created.len(),
        3,
        "a computed column is a child of its table"
    );
}

#[test]
fn only_a_failure_naming_the_statement_is_a_verdict_on_the_declaration() {
    let server = |code: &str| pbps_db::DbError::Driver {
        message: "x".into(),
        code: Some(code.into()),
    };
    for code in ["4121", "111", "262", "2760", "4513", "208"] {
        assert!(judged_the_statement(&server(code)), "{code}");
    }
    for code in ["1205", "1222", "701", "9002", "233"] {
        assert!(!judged_the_statement(&server(code)), "{code}");
    }
    assert!(!judged_the_statement(&pbps_db::DbError::Refused(
        "no server code".into()
    )));
}

#[test]
fn a_compiled_root_missing_from_its_capture_is_named() {
    let reconstruction = build(vec![
        table("t", &[]),
        function("f", FUNCTION),
        view("w", "SELECT id FROM app.t"),
    ])
    .unwrap();
    let all = [object("app", "t"), object("app", "f"), object("app", "w")];
    assert!(reconstruction.absent_from(&all.iter().collect()).is_empty());
    let without_the_function: BTreeSet<_> = [&all[0], &all[2]].into();
    assert_eq!(
        reconstruction.absent_from(&without_the_function),
        [&object("app", "f")]
    );
    // Nothing captured at all names every root, never reads as "none".
    assert_eq!(reconstruction.absent_from(&BTreeSet::new()).len(), 3);
}

#[test]
fn a_view_projecting_every_column_of_a_held_table_waits_for_the_column() {
    // A non-schema-bound view binds its `*` when it is created (measured on
    // 17.0), so one created before the column never exposes it, and a view
    // over it naming the column then fails 207.
    for star in [
        "SELECT * FROM app.t",
        "SELECT x.* FROM app.t AS x",
        "SELECT TOP 5 * FROM app.t",
        "SELECT id, * FROM app.t",
    ] {
        let statements = sql(&build(vec![
            table("t", &[("c", "app.f(v)")]),
            view("s", star),
            view("down", "SELECT id, c FROM app.s"),
            function("f", FUNCTION),
        ])
        .unwrap());
        let routine = at(&statements, "FUNCTION [app].[f]");
        let column = at(&statements, "ADD [c] AS (app.f(v)");
        let all = at(&statements, "VIEW [app].[s]");
        let down = at(&statements, "VIEW [app].[down]");
        assert!(
            routine < column && column < all && all < down,
            "{star}: {statements:#?}"
        );
    }
}

#[test]
fn an_inline_table_valued_function_projecting_every_column_waits_for_the_column() {
    let statements = sql(&build(vec![
        table("t", &[("c", "app.f(v)")]),
        function("tvf", "() RETURNS TABLE AS RETURN SELECT * FROM app.t"),
        function("f", FUNCTION),
    ])
    .unwrap());
    let column = at(&statements, "ADD [c] AS (app.f(v)");
    assert!(
        column < at(&statements, "FUNCTION [app].[tvf]"),
        "{statements:#?}"
    );
}

#[test]
fn a_scalar_function_the_column_calls_is_no_wildcard_reader_and_makes_no_cycle() {
    // `EXISTS (SELECT * ..)` projects no column list, and the column calls
    // scalar functions only, so the callee is never held after the column it
    // serves: a valid order exists (measured on 17.0) and it is found.
    let statements = sql(&build(vec![
        table("t", &[("c", "app.f(id)")]),
        function(
            "f",
            "(@x int) RETURNS int AS BEGIN RETURN (CASE WHEN EXISTS (SELECT * FROM app.t WHERE id = @x) THEN 1 ELSE 0 END); END",
        ),
    ])
    .unwrap());
    let create = at(&statements, "CREATE TABLE [app].[t]");
    let routine = at(&statements, "FUNCTION [app].[f]");
    let column = at(&statements, "ADD [c] AS (app.f(id)");
    assert!(create < routine && routine < column, "{statements:#?}");
}

#[test]
fn only_a_select_all_is_a_wildcard_and_a_count_or_a_product_is_not() {
    for wildcard in [
        "SELECT * FROM t",
        "select\n  *\nfrom t",
        "SELECT t.* FROM t",
        "SELECT [t].* FROM t",
        "SELECT DISTINCT * FROM t",
        "SELECT TOP 5 * FROM t",
        "SELECT TOP (5) PERCENT * FROM t",
        "SELECT *, a FROM t",
        "SELECT a, * FROM t",
        "SELECT COUNT(*) AS n, * FROM t",
    ] {
        assert!(selects_all(wildcard), "{wildcard}");
    }
    for plain in [
        "SELECT COUNT(*) FROM t",
        "SELECT COUNT( * ) FROM t",
        "SELECT a * b FROM t",
        "SELECT a*2 AS twice, MAX(b * 3) FROM t",
        "SELECT a * from_date FROM t",
        "SELECT (a + 1) * (b + 2) FROM t",
        "SELECT a FROM t",
        "SELECT a FROM t WHERE EXISTS (SELECT * FROM u)",
        "SELECT a FROM t WHERE NOT EXISTS ( SELECT * FROM u WHERE u.k = t.k)",
        "",
    ] {
        assert!(!selects_all(plain), "{plain}");
    }
}

fn persisted(expression: &str) -> ComputedColumn {
    ComputedColumn {
        expression: expression.into(),
        persisted: true,
        not_null: true,
    }
}

fn primary_key(column: &str) -> pbps_model::PrimaryKey {
    pbps_model::PrimaryKey {
        name: Some(format!("pk_{column}").into()),
        columns: vec![column.into()],
        storage_parameters: Default::default(),
    }
}

fn foreign_key(from: &str, to: &str, key: &str) -> pbps_model::ForeignKey {
    pbps_model::ForeignKey {
        columns: vec![from.into()],
        references_table: name(to),
        references_columns: vec![key.into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    }
}

#[test]
fn a_primary_key_over_a_computed_column_follows_the_column_with_its_name_and_layout() {
    // The first CREATE cannot name a column that does not exist yet (1911,
    // measured on 17.0); the key is added after the column, spelled by the
    // same clause the CREATE would have used.
    for (clustered, layout) in [
        (None, "CLUSTERED"),
        (Some(pbps_model::Clustered::Heap), "NONCLUSTERED"),
    ] {
        let statements = sql(&build(vec![
            table_with("t", |t| {
                t.computed.insert("c".into(), persisted("app.f(v)"));
                t.primary_key = Some(primary_key("c"));
                t.clustered = clustered.clone();
            }),
            function("f", FUNCTION),
        ])
        .unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        assert!(
            !statements[create].contains("PRIMARY KEY"),
            "{}",
            statements[create]
        );
        let routine = at(&statements, "FUNCTION [app].[f]");
        let column = at(&statements, "ADD [c] AS (app.f(v)");
        let key = at(
            &statements,
            &format!("CONSTRAINT [pk_c] PRIMARY KEY {layout} ([c])"),
        );
        assert!(
            routine < column && column < key,
            "{layout}: {statements:#?}"
        );
    }
}

#[test]
fn a_primary_key_over_a_plain_column_stays_in_the_create() {
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.computed.insert("c".into(), persisted("app.f(v)"));
            t.primary_key = Some(primary_key("id"));
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let create = at(&statements, "PRIMARY KEY");
    assert_eq!(create, at(&statements, "CREATE TABLE [app].[t]"));
    assert!(
        statements[create].contains("CONSTRAINT [pk_id] PRIMARY KEY CLUSTERED ([id])"),
        "{}",
        statements[create]
    );
}

#[test]
fn a_foreign_key_from_or_to_a_computed_column_waits_for_every_module_and_the_key_it_references() {
    // Measured on 17.0: a key before its own computed column fails 1769 and a
    // key before the key it references fails 1776, whichever table the
    // declarations list first, and whether the differ split the key out of
    // its table or the table still holds it. `from` is the key's own column:
    // computed (the key's own end) or plain (only the referenced end is).
    for from in ["ref", "id"] {
        for child_first in [false, true] {
            for inline in [false, true] {
                let parent = table_with("parent", |t| {
                    t.computed.insert("k".into(), persisted("app.f(v)"));
                    t.primary_key = Some(primary_key("k"));
                });
                let fk = foreign_key(from, "parent", "k");
                let child = table_with("child", |t| {
                    t.computed.insert("ref".into(), persisted("app.f(v)"));
                    if inline {
                        t.foreign_keys.insert("fk_child".into(), fk.clone());
                    }
                });
                let mut changes = vec![parent, child];
                if child_first {
                    changes.reverse();
                }
                if !inline {
                    changes.push(Change::AddForeignKey {
                        table: name("child"),
                        name: "fk_child".into(),
                        constraint: Box::new(fk),
                    });
                }
                changes.push(function("f", FUNCTION));
                let statements = sql(&build(changes).unwrap());
                let routine = at(&statements, "FUNCTION [app].[f]");
                let key = at(&statements, "CONSTRAINT [pk_k] PRIMARY KEY");
                let reference = at(&statements, &format!("FOREIGN KEY ([{from}])"));
                let columns: Vec<_> = statements
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.contains("ADD [k] AS") || s.contains("ADD [ref] AS"))
                    .map(|(at, _)| at)
                    .collect();
                assert_eq!(columns.len(), 2, "{statements:#?}");
                assert!(
                    routine < columns[0].min(columns[1])
                        && columns[0].max(columns[1]) < reference
                        && key < reference,
                    "from={from} child_first={child_first} inline={inline}: {statements:#?}"
                );
            }
        }
    }
}

#[test]
fn a_foreign_key_from_a_computed_column_to_a_plain_key_waits_for_the_column() {
    // Only the key's own end is computed: its column is added after the
    // function, so the key cannot come before it.
    for inline in [false, true] {
        for plain_first in [false, true] {
            let plain = table_with("plain", |t| {
                t.primary_key = Some(primary_key("id"));
            });
            let fk = foreign_key("ref", "plain", "id");
            let child = table_with("child", |t| {
                t.computed.insert("ref".into(), persisted("app.f(v)"));
                if inline {
                    t.foreign_keys.insert("fk_child".into(), fk.clone());
                }
            });
            let mut changes = vec![plain, child];
            if !plain_first {
                changes.reverse();
            }
            if !inline {
                changes.push(Change::AddForeignKey {
                    table: name("child"),
                    name: "fk_child".into(),
                    constraint: Box::new(fk),
                });
            }
            changes.push(function("f", FUNCTION));
            let statements = sql(&build(changes).unwrap());
            let column = at(&statements, "ADD [ref] AS");
            let key = at(&statements, "FOREIGN KEY ([ref])");
            assert!(
                at(&statements, "FUNCTION [app].[f]") < column && column < key,
                "inline={inline} plain_first={plain_first}: {statements:#?}"
            );
        }
    }
}

#[test]
fn a_foreign_key_between_plain_columns_stays_with_the_tables() {
    let statements = sql(&build(vec![
        table("parent", &[("k", "app.f(v)")]),
        table_with("child", |t| {
            t.foreign_keys
                .insert("fk_plain".into(), foreign_key("id", "parent", "id"));
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let plain = at(&statements, "FOREIGN KEY ([id])");
    assert!(
        plain < at(&statements, "FUNCTION [app].[f]"),
        "{statements:#?}"
    );
}

fn reads(function: &str, column: &str) -> Change {
    function_named(
        function,
        &format!(
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT({column}) FROM app.t WHERE id = @x); END"
        ),
    )
}

fn function_named(function_name: &str, definition: &str) -> Change {
    function(function_name, definition)
}

#[test]
fn a_function_reading_a_column_declared_before_the_one_that_calls_it_follows_that_column() {
    // `a AS (id + 1)` declared before `c AS (f(id))`, with `f` reading `a`:
    // bare table, `a`, `f`, `c` compiles (measured on 17.0), and the columns
    // are added in declared order, so their positions are the target's.
    for functions_first in [false, true] {
        let table = table("t", &[("a", "id + 1"), ("c", "app.f(id)")]);
        let changes = if functions_first {
            vec![reads("f", "a"), table]
        } else {
            vec![table, reads("f", "a")]
        };
        let statements = sql(&build(changes).unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        let routine = at(&statements, "FUNCTION [app].[f]");
        let c = at(&statements, "ADD [c] AS");
        // `a` is before the first column that calls a planned function, so it
        // stays in the CREATE, and `f` that reads it needs nothing more.
        assert!(
            statements[create].contains("[a] AS (id + 1"),
            "{statements:#?}"
        );
        assert!(
            create < c && routine < c,
            "functions_first={functions_first}: {statements:#?}"
        );
    }
}

#[test]
fn a_function_reading_a_column_declared_after_the_one_that_calls_it_is_refused_with_its_remedy() {
    // `c AS (f(id))` is declared before `d AS (id + 1)` and `f` reads `d`.
    // `d` must exist before `f`, and `f` before `c`, but declared order is a
    // hard chain: positions are compared as reported, columns are appended
    // and cannot be reordered (measured on 17.0: `d` then `c` gives `d=3, c=4`
    // where the target has `c=3, d=4`), so no order both compiles and
    // reproduces them. It is refused by name, with the remedy.
    let refused = build(vec![
        table("t", &[("c", "app.f(id)"), ("d", "id + 1")]),
        reads("f", "d"),
    ])
    .expect_err("no order compiles and keeps the declared positions");
    let ReconstructError::Cycle(subject) = &refused else {
        panic!("{refused:?}");
    };
    assert!(subject.contains("app.f()"), "{refused}");
    assert!(
        refused
            .to_string()
            .contains("declare the column the function reads first"),
        "{refused}"
    );
}

#[test]
fn a_function_reading_the_column_that_calls_it_is_still_a_cycle() {
    // Measured on 17.0: the column cannot be added before the function, nor
    // the function created before the column it reads (207). Another column
    // of the table makes no difference to that.
    let own = build(vec![
        table("t", &[("c", "app.f(id)"), ("d", "id + 1")]),
        reads("f", "c"),
    ])
    .expect_err("f reads the column that calls it");
    assert!(
        matches!(&own, ReconstructError::Cycle(subject) if subject.contains("app.f()")),
        "{own:?}"
    );
}

#[test]
fn a_cycle_across_two_columns_is_refused_before_anything_compiles() {
    // Each function reads the other's column: a graph cycle, now found in the
    // route and not by the engine's 207.
    let refused = build(vec![
        table("t", &[("c1", "app.g1(id)"), ("c2", "app.g2(id)")]),
        reads("g1", "c2"),
        reads("g2", "c1"),
    ])
    .expect_err("each column needs the function that reads the other");
    assert!(
        matches!(&refused, ReconstructError::Cycle(_)),
        "{refused:?}"
    );
}

#[test]
fn two_computed_columns_calling_two_functions_follow_their_own_function_in_any_order() {
    for functions in [["f1", "f2"], ["f2", "f1"]] {
        let mut changes = vec![table("t", &[("c1", "app.f2(id)"), ("c2", "app.f1(id)")])];
        changes.extend(functions.map(|name| function(name, FUNCTION)));
        let statements = sql(&build(changes).unwrap());
        assert!(
            at(&statements, "FUNCTION [app].[f2]") < at(&statements, "ADD [c1] AS"),
            "{statements:#?}"
        );
        assert!(
            at(&statements, "FUNCTION [app].[f1]") < at(&statements, "ADD [c2] AS"),
            "{statements:#?}"
        );
    }
}

#[test]
fn a_key_over_two_computed_columns_follows_both() {
    for functions in [["f1", "f2"], ["f2", "f1"]] {
        let mut changes = vec![table_with("t", |t| {
            t.computed.insert("c1".into(), persisted("app.f1(v)"));
            t.computed.insert("c2".into(), persisted("app.f2(v)"));
            t.unique.insert(
                "uq_two".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["c1".into(), "c2".into()],
                    storage_parameters: Default::default(),
                },
            );
        })];
        changes.extend(functions.map(|name| function(name, FUNCTION)));
        let statements = sql(&build(changes).unwrap());
        let key = at(&statements, "[uq_two]");
        assert!(
            at(&statements, "ADD [c1] AS") < key && at(&statements, "ADD [c2] AS") < key,
            "{functions:?}: {statements:#?}"
        );
        assert!(
            at(&statements, "FUNCTION [app].[f1]") < at(&statements, "ADD [c1] AS")
                && at(&statements, "FUNCTION [app].[f2]") < at(&statements, "ADD [c2] AS"),
            "{functions:?}: {statements:#?}"
        );
    }
}

fn function_in(schema: &str, function_name: &str, definition: &str) -> Change {
    Change::CreateModule {
        id: ModuleId::Routine(pbps_model::RoutineId::new(
            TableName::new(schema, function_name),
            Vec::new(),
        )),
        module: Box::new(Module {
            kind: ModuleKind::Function,
            definition: definition.into(),
            description: None,
            options: Default::default(),
        }),
    }
}

const READS_ID: &str =
    "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(id) FROM app.t WHERE id = @x); END";

// The lexical reader and call tests, audited together (round 5). For each
// way a name can appear in a part's text without being a read or a call:

#[test]
fn a_function_is_no_reader_of_a_column_that_shares_its_name() {
    // The header of `f` says `f`; a part never reads or calls itself through
    // its declaration. Measured on 17.0: bare table, `f`, `ADD f AS (app.f(id))`.
    let statements = sql(&build(vec![
        table("t", &[("f", "app.f(id)")]),
        function("f", READS_ID),
    ])
    .unwrap());
    let create = at(&statements, "CREATE TABLE [app].[t]");
    let routine = at(&statements, "FUNCTION [app].[f]");
    let column = at(&statements, "ADD [f] AS");
    assert!(create < routine && routine < column, "{statements:#?}");
}

#[test]
fn a_parameter_a_local_a_string_and_a_comment_are_no_reads_of_a_column() {
    // A string literal and a comment are no code to the dialect's lexer; a
    // variable, a parameter and a temporary object are, and its name test
    // reads `c` out of `@c`, `@@c` and `#c` (measured), so they are dropped.
    let statements = sql(&build(vec![
        table("t", &[("c", "app.g(id)")]),
        function(
            "g",
            "(@c int) RETURNS int AS BEGIN DECLARE @c2 int = @c; -- c\n /* c */ RETURN (SELECT COUNT(id) FROM app.t WHERE id = @c AND @@c = 1 AND 'c' = 'c' AND 1 = (SELECT 1 FROM #c)); END",
        ),
    ])
    .unwrap());
    assert!(
        at(&statements, "FUNCTION [app].[g]") < at(&statements, "ADD [c] AS"),
        "{statements:#?}"
    );
}

#[test]
fn a_name_shared_with_the_column_by_an_alias_or_another_table_is_a_refusal_naming_the_remedy() {
    // Content-dependent: whether `o.c` is `t.c` is for the engine to say, and
    // a lexical test cannot. Reading it as `t.c` only ever adds a constraint,
    // so what it can cost is a refusal, and the refusal says what to rename.
    for text in [
        "SELECT COUNT(o.c) FROM app.other AS o JOIN app.t ON 1 = 1",
        "SELECT id AS c FROM app.t",
        "SELECT COUNT(c) FROM other.t",
    ] {
        let refused = build(vec![
            table("t", &[("c", "app.g(id)")]),
            function(
                "g",
                &format!(
                    "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM ({text}) AS q); END"
                ),
            ),
        ])
        .expect_err("g is read as reading the column that calls it");
        let ReconstructError::Cycle(subject) = &refused else {
            panic!("{refused:?}");
        };
        assert!(
            subject.contains("app.g()") && refused.to_string().contains("rename"),
            "{refused}"
        );
    }
}

#[test]
fn a_wildcard_from_another_table_over_holds_and_never_misorders() {
    // `o.*` projects another table's columns, and the view also names `t`:
    // it is read as projecting `t`'s too, so it follows the column. Later is
    // always a valid place for a view, so this costs no refusal.
    let statements = sql(&build(vec![
        table("t", &[("c", "app.f(id)")]),
        view("s", "SELECT o.* FROM app.other AS o, app.t"),
        function("f", FUNCTION),
    ])
    .unwrap());
    assert!(
        at(&statements, "FUNCTION [app].[f]") < at(&statements, "ADD [c] AS")
            && at(&statements, "ADD [c] AS") < at(&statements, "VIEW [app].[s]"),
        "{statements:#?}"
    );
}

#[test]
fn a_column_named_like_a_function_calls_no_function() {
    // `v + 1` uses the column `v`; the function `app.v` is called by name
    // `app.v(`. Measured on 17.0: bare table, `ADD c AS (v + 1)`, then the
    // function that reads `c`.
    let statements = sql(&build(vec![
        table("t", &[("c", "v + 1")]),
        function(
            "v",
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(c) FROM app.t WHERE id = @x); END",
        ),
    ])
    .unwrap());
    // Nothing calls a planned function, so the table is not split: `c` stays
    // in the CREATE, which precedes the function that reads it.
    let create = at(&statements, "CREATE TABLE [app].[t]");
    assert!(
        statements[create].contains("[c] AS (v + 1"),
        "{statements:#?}"
    );
    assert!(
        create < at(&statements, "FUNCTION [app].[v]"),
        "{statements:#?}"
    );
}

#[test]
fn a_function_of_the_same_name_in_another_schema_is_no_callee() {
    // `c` calls `other.f`; `app.f` shares its leaf name and reads `c`.
    let statements = sql(&build(vec![
        table("t", &[("c", "other.f(id)")]),
        function(
            "f",
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(c) FROM app.t WHERE id = @x); END",
        ),
        function_in("other", "f", FUNCTION),
    ])
    .unwrap());
    let other = at(&statements, "FUNCTION [other].[f]");
    let column = at(&statements, "ADD [c] AS");
    let reader = at(&statements, "FUNCTION [app].[f]");
    assert!(other < column && column < reader, "{statements:#?}");
}

#[test]
fn a_variable_name_is_not_a_name_in_the_code() {
    assert_eq!(
        without_variables("SELECT @c, @@rowcount, #t.c, x@y FROM #t WHERE c = @c2"),
        "SELECT  ,  ,  .c, x@y FROM   WHERE c =  "
    );
    // Not a token where it is part of a name: delimited, or after a letter.
    for named in [
        "[c#d] > 0",
        "\"c#d\" > 0",
        "[c]]#d] > 0",
        "c#d > 0",
        "[@c] > 0",
    ] {
        assert_eq!(without_variables(named), named, "{named}");
    }
}

#[test]
fn a_column_named_with_a_hash_is_read_by_its_whole_name() {
    // `[c#d]` is the column `c#d`, not `c` followed by a temporary name.
    // Every word is a candidate (a literal number too), so the test is of
    // what is, and is not, among them.
    let names = |text: &str| column_names(&Mssql, text).unwrap();
    for (text, column) in [
        ("[c#d] > 0", "c#d"),
        ("\"c#d\" > 0", "c#d"),
        ("c#d > 0", "c#d"),
        ("[c]]#d] > 0", "c]#d"),
    ] {
        let read = names(text);
        assert!(read.contains(column), "{text}: {read:?}");
        assert!(
            !read.contains("c") && !read.contains("d"),
            "{text}: {read:?}"
        );
    }
    // Negative: a real variable and a temporary object are still no column.
    let read = names("c > @d + #t.e");
    assert!(read.contains("c") && read.contains("e"), "{read:?}");
    assert!(!read.contains("d") && !read.contains("t"), "{read:?}");
}

#[test]
fn a_check_over_a_hash_named_computed_column_follows_the_column() {
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.computed.insert("c#d".into(), persisted("app.f(id)"));
            t.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "[c#d] > 0".into(),
                },
            );
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let create = at(&statements, "CREATE TABLE [app].[t]");
    assert!(!statements[create].contains("[c#d]"), "{statements:#?}");
    assert!(
        at(&statements, "ADD [c#d] AS") < at(&statements, "[ck]"),
        "{statements:#?}"
    );
}

#[test]
fn a_composite_key_over_a_late_and_an_inline_looking_column_follows_both() {
    // `c1 AS (f(id))` is late and `c2 AS (id + 1)` is declared after it, so it
    // is added after `c1` too: the key names both, and `c2` is no exception
    // that stays ahead of the key (a held group with no callee used to).
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.columns.get_mut("id").unwrap().nullable = false;
            t.computed.insert("c1".into(), persisted("app.f(id)"));
            t.computed.insert("c2".into(), persisted("id + 1"));
            t.unique.insert(
                "uq_both".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["c1".into(), "c2".into()],
                    storage_parameters: Default::default(),
                },
            );
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let routine = at(&statements, "FUNCTION [app].[f]");
    let first = at(&statements, "ADD [c1] AS");
    let second = at(&statements, "ADD [c2] AS");
    let key = at(&statements, "[uq_both]");
    assert!(
        routine < first && first < second && second < key,
        "{statements:#?}"
    );
}

#[test]
fn a_table_that_calls_no_planned_function_is_not_split() {
    // Nothing in the CREATE names a function that does not exist yet, so the
    // table is written whole, computed column, default, check and key included.
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.computed.insert("c".into(), computed("id + 1"));
            t.columns.get_mut("v").unwrap().value_source = ValueSource::Default("0".into());
            t.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "v >= 0".into(),
                },
            );
            t.primary_key = Some(primary_key("id"));
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    let create = at(&statements, "CREATE TABLE [app].[t]");
    assert!(
        statements[create].contains("[c] AS (id + 1"),
        "{statements:#?}"
    );
    assert!(statements[create].contains("DEFAULT (0"), "{statements:#?}");
    assert!(
        statements.iter().all(|s| !s.contains("ADD [c]")),
        "{statements:#?}"
    );
}

#[test]
fn ordinary_planning_still_refuses_a_computed_column_over_a_function_the_plan_creates() {
    // The split exists for the scratch route only: the ordinary plan never
    // receives such a table, because the differ refuses it first.
    let mut desired = pbps_model::Schema::default();
    desired.tables.insert(
        name("t"),
        pbps_model::Table {
            columns: IndexMap::from([("id".into(), int())]),
            computed: [("c".into(), computed("app.f(id)"))].into(),
            ..Default::default()
        },
    );
    desired.modules.insert(
        "app.f()".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            definition: FUNCTION.into(),
            description: None,
            options: Default::default(),
        },
    );
    let empty = pbps_model::Schema::default();
    let ids = pbps_diff::resolve(
        &desired,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "reconstruct-tests".into(),
            today: "2026-10-25".into(),
        },
    )
    .unwrap()
    .ids;
    let refused = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &ids,
        },
        &Mssql,
        &Default::default(),
    )
    .unwrap_err();
    assert!(
        format!("{refused:?}").contains("ComputedFunctionChanged"),
        "{refused:?}"
    );
}

#[test]
fn a_seed_row_for_a_table_with_a_split_computed_column_makes_no_cycle() {
    // Reconstruction never runs a row, but `prepare` settles it first: the
    // seed must follow every column part, a split computed column included,
    // and no part may be ordered after it.
    let seed = Change::InsertRow {
        table: name("t"),
        key_column: "id".into(),
        identity_key: false,
        key: pbps_model::RowKey::from("1"),
        defaults: Default::default(),
        types: Default::default(),
        row: Default::default(),
    };
    for functions_first in [false, true] {
        let table = table_with("t", |t| {
            t.columns.get_mut("id").unwrap().nullable = false;
            t.primary_key = Some(primary_key("id"));
            t.computed.insert("c".into(), computed("app.f(id)"));
            t.unique.insert(
                "uq_c".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["c".into()],
                    storage_parameters: Default::default(),
                },
            );
        });
        let mut changes = if functions_first {
            vec![function("f", FUNCTION), table]
        } else {
            vec![table, function("f", FUNCTION)]
        };
        changes.push(seed.clone());
        let statements = sql(&build(changes).unwrap());
        assert!(
            statements.iter().all(|s| !s.contains("INSERT")),
            "{statements:#?}"
        );
        assert!(
            at(&statements, "FUNCTION [app].[f]") < at(&statements, "ADD [c] AS"),
            "{statements:#?}"
        );
    }
}

#[test]
fn a_name_equal_under_some_catalog_collation_is_read() {
    // The catalog collation decides what binds, and pbps cannot read it
    // offline: on an accent-, width- or kana-insensitive catalog `cafe`
    // binds `[café]`. A read that may be the column defers the check after
    // it, the order that always compiles (#2408).
    for (pending, read) in [
        ("café", "cafe"),
        ("cafe", "café"),
        ("cafe", "ｃａｆｅ"),
        ("ｃａｆｅ", "cafe"),
        ("ひらがな", "ヒラガナ"),
        ("ヒラガナ", "ひらがな"),
        ("kıt", "KIT"),
        ("CAFÉ", "cafe"),
    ] {
        let statements = sql(&build(vec![
            table_with("t", |t| {
                t.computed.insert(pending.into(), persisted("app.f(v)"));
                t.checks.insert(
                    "ck".into(),
                    pbps_model::CheckConstraint {
                        expression: format!("[{read}] > 0"),
                    },
                );
            }),
            function("f", FUNCTION),
        ])
        .unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        assert!(
            !statements[create].contains("[ck]"),
            "{pending}/{read}: {statements:#?}"
        );
        assert!(
            at(&statements, &format!("ADD [{pending}] AS")) < at(&statements, "[ck]"),
            "{pending}/{read}: {statements:#?}"
        );
    }
}

#[test]
fn a_name_that_is_equal_under_no_collation_is_no_read() {
    // An unrelated ASCII column, and the name only inside a literal or a
    // comment, still leave the check in the CREATE.
    for expression in [
        "[cafe2] > 0",
        "v > 0 AND N'café' <> N'x'",
        "v > 0 /* café */",
        "v > 0 AND [kafe] > 0",
    ] {
        let statements = sql(&build(vec![
            table_with("t", |t| {
                t.columns.insert("cafe2".into(), int());
                t.columns.insert("kafe".into(), int());
                t.computed.insert("café".into(), persisted("app.f(v)"));
                t.checks.insert(
                    "ck".into(),
                    pbps_model::CheckConstraint {
                        expression: expression.into(),
                    },
                );
            }),
            function("f", FUNCTION),
        ])
        .unwrap());
        // Written with the bare table, not held for the column.
        assert!(
            at(&statements, "[ck]") < at(&statements, "ADD [café] AS"),
            "{expression}: {statements:#?}"
        );
    }
}

#[test]
fn a_wildcard_inside_exists_is_no_whole_shape_read_whatever_the_select_list_opens_with() {
    for exists in [
        "EXISTS (SELECT TOP (1) * FROM t)",
        "EXISTS (SELECT TOP 1 * FROM t)",
        "EXISTS (SELECT DISTINCT * FROM t)",
        "EXISTS (SELECT ALL * FROM t)",
        "EXISTS (SELECT TOP 10 PERCENT WITH TIES * FROM t ORDER BY a)",
        "EXISTS (SELECT TOP (1) PERCENT * FROM t)",
        "NOT EXISTS ( SELECT DISTINCT TOP (2) * FROM t WHERE a = 1)",
        "EXISTS (SELECT TOP (1) t.* FROM t)",
        "EXISTS (SELECT TOP (1) *, 1 FROM t)",
        // A parenthesis in a delimited name is no syntax.
        "EXISTS (SELECT [a(b], * FROM t)",
        "EXISTS (SELECT \"a)b\", * FROM t)",
    ] {
        assert!(!selects_all(exists), "{exists}");
    }
    // Negative: the wildcard is the query's own, not the subquery's.
    for wildcard in [
        "SELECT TOP (1) * FROM t",
        "SELECT DISTINCT * FROM t WHERE EXISTS (SELECT 1 FROM u)",
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u)",
        "SELECT a FROM (SELECT TOP (1) * FROM t) AS d",
        "SELECT a FROM (SELECT * FROM t) AS d WHERE EXISTS (SELECT 1)",
        "SELECT a FROM t JOIN (SELECT TOP (1) * FROM u) AS d ON 1 = 1",
        "SELECT COUNT(*), TOP (1) * FROM t",
    ] {
        assert!(selects_all(wildcard), "{wildcard}");
    }
}

#[test]
fn a_function_with_an_exists_over_a_top_wildcard_makes_no_cycle() {
    for select in [
        "TOP (1) *",
        "DISTINCT *",
        "TOP 10 PERCENT WITH TIES *",
        "TOP (1) PERCENT t.*",
    ] {
        let statements = sql(&build(vec![
            table("t", &[("c", "app.f(id)")]),
            function(
                "f",
                &format!(
                    "(@x int) RETURNS int AS BEGIN RETURN (CASE WHEN EXISTS (SELECT {select} FROM app.t WHERE id = @x) THEN 1 ELSE 0 END); END"
                ),
            ),
        ])
        .unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        let routine = at(&statements, "FUNCTION [app].[f]");
        let column = at(&statements, "ADD [c] AS (app.f(id)");
        assert!(
            create < routine && routine < column,
            "{select}: {statements:#?}"
        );
    }
}

#[test]
fn a_function_called_by_a_name_equal_under_some_collation_is_called() {
    // The same comparison, for the function a column calls: `app.cafe(..)`
    // binds `app.[café]` on an accent-insensitive catalog, and the column then
    // has to follow the function (#2408).
    for (declared, called) in [("café", "cafe"), ("cafe", "café"), ("cafe", "ｃａｆｅ")] {
        let statements = sql(&build(vec![
            table("t", &[("c", &format!("app.[{called}](id)"))]),
            function(declared, FUNCTION),
        ])
        .unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        assert!(
            !statements[create].contains(" AS ("),
            "{declared}/{called}: {statements:#?}"
        );
        assert!(
            at(&statements, &format!("FUNCTION [app].[{declared}]"))
                < at(&statements, "ADD [c] AS"),
            "{declared}/{called}: {statements:#?}"
        );
    }
}

#[test]
fn a_module_reading_a_column_by_an_equal_name_waits_for_it() {
    // A view binds the table's columns when it is created (207 otherwise,
    // measured), so the one that names `cafe` for `[café]` follows the column.
    let statements = sql(&build(vec![
        view("v", "SELECT cafe FROM app.t"),
        table("t", &[("café", "app.g(v)")]),
        function("g", FUNCTION),
    ])
    .unwrap());
    assert!(
        at(&statements, "ADD [café] AS") < at(&statements, "VIEW [app].[v]"),
        "{statements:#?}"
    );
}

#[test]
fn a_letter_the_fold_leaves_alone_may_still_bind() {
    // A letter that is not in the fold's table of measured bindings may still
    // bind under some catalog collation (ȼ binds c on one of two measured).
    // A name holding one, on either side, may be the column: the check
    // follows it (#2408).
    for (pending, read) in [
        ("ŋarn", "narn"),
        ("narn", "ŋarn"),
        ("ŧest", "TEST"),
        ("ȼafe", "cafe"),
        ("cafe", "ŋ"),
    ] {
        let statements = sql(&build(vec![
            table_with("t", |t| {
                t.computed.insert(pending.into(), persisted("app.f(v)"));
                t.checks.insert(
                    "ck".into(),
                    pbps_model::CheckConstraint {
                        expression: format!("[{read}] > 0"),
                    },
                );
            }),
            function("f", FUNCTION),
        ])
        .unwrap());
        assert!(
            at(&statements, &format!("ADD [{pending}] AS")) < at(&statements, "[ck]"),
            "{pending}/{read}: {statements:#?}"
        );
    }
}

#[test]
fn an_unrelated_ascii_name_is_no_read_of_an_ascii_column() {
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.columns.insert("other".into(), int());
            t.computed.insert("cafe".into(), persisted("app.f(v)"));
            t.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "[other] > 0 AND N'ł' <> N'x'".into(),
                },
            );
        }),
        function("f", FUNCTION),
    ])
    .unwrap());
    assert!(
        at(&statements, "[ck]") < at(&statements, "ADD [cafe] AS"),
        "{statements:#?}"
    );
}

#[test]
fn a_callee_in_another_script_than_latin_makes_no_cycle_with_the_column_that_calls_it() {
    // The column calls `app.[計算]`, which is planned; another function reads
    // the column. A name with a letter the fold leaves alone must not make
    // every function of the schema a callee, which would put `h` both before
    // and after the column.
    let statements = sql(&build(vec![
        table("t", &[("c", "app.[計算](id)")]),
        function("計算", FUNCTION),
        function(
            "h",
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(c) FROM app.t WHERE id = @x); END",
        ),
    ])
    .unwrap());
    let column = at(&statements, "ADD [c] AS");
    assert!(
        at(&statements, "FUNCTION [app].[計算]") < column
            && column < at(&statements, "FUNCTION [app].[h]"),
        "{statements:#?}"
    );
}

#[test]
fn a_function_in_another_script_than_latin_makes_no_cycle_with_the_column_that_calls_it() {
    // The function names the table and a column of it written in Chinese; the
    // pending column calls it. A letter that no collation equates with `c`
    // must not make the function a reader of `c`: that would put it both
    // before and after the column.
    let statements = sql(&build(vec![
        table_with("t", |t| {
            t.columns.insert("名稱".into(), int());
            t.computed.insert("c".into(), persisted("app.g(id)"));
        }),
        function(
            "g",
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(名稱) FROM app.t WHERE id = @x); END",
        ),
    ])
    .unwrap());
    assert!(
        at(&statements, "FUNCTION [app].[g]") < at(&statements, "ADD [c] AS"),
        "{statements:#?}"
    );
}

#[test]
fn a_letter_the_catalog_binds_to_ascii_is_resolved_by_the_fold() {
    // Measured on Latin1_General_100_CI_AI and SQL_Latin1_General_CP1_CI_AI:
    // each of these binds to its mapping, upper case too.
    for (letter, mapped) in [
        ("ł", "l"),
        ("Ł", "l"),
        ("æ", "ae"),
        ("Æ", "ae"),
        ("ß", "ss"),
        ("ø", "o"),
        ("Ø", "o"),
        ("đ", "d"),
        ("Đ", "d"),
        ("ð", "d"),
        ("Ð", "d"),
        ("þ", "th"),
        ("Þ", "th"),
        ("œ", "oe"),
        ("Œ", "oe"),
        ("ħ", "h"),
        ("Ħ", "h"),
        ("ŀ", "l"),
        ("Ŀ", "l"),
        ("ı", "i"),
        ("ĸ", "k"),
        ("ƒ", "f"),
        ("Ƒ", "f"),
        ("ǥ", "g"),
        ("Ǥ", "g"),
        ("ɨ", "i"),
        ("Ɨ", "i"),
    ] {
        assert_eq!(computed_note_fold(letter), mapped, "{letter}");
    }
    // Negative: the engine binds none of these to ASCII (measured), so the
    // fold leaves them, and they stay "unresolved" for the parts.
    for letter in ["ŋ", "Ŋ", "ŧ", "Ŧ", "ꞁ", "ȼ", "Ƀ"] {
        assert_eq!(
            computed_note_fold(letter),
            letter.to_lowercase(),
            "{letter}"
        );
    }
    assert_eq!(computed_note_fold("ŁÓDŹ"), "lodz");
}

#[test]
fn a_module_reading_a_column_by_a_mapped_letter_waits_for_it() {
    // `lodz` binds `[łódź]` on an accent-insensitive catalog, and a view binds
    // its columns when it is created (207 otherwise, measured).
    for (pending, read) in [("łódź", "lodz"), ("lodz", "łódź"), ("straße", "strasse")] {
        let statements = sql(&build(vec![
            view("v", &format!("SELECT [{read}] FROM app.t")),
            table("t", &[(pending, "app.g(v)")]),
            function("g", FUNCTION),
        ])
        .unwrap());
        assert!(
            at(&statements, &format!("ADD [{pending}] AS")) < at(&statements, "VIEW [app].[v]"),
            "{pending}/{read}: {statements:#?}"
        );
    }
}

#[test]
fn a_call_by_a_mapped_letter_finds_the_function() {
    for (declared, called) in [("łódź", "lodz"), ("lodz", "łódź"), ("straße", "strasse")] {
        let statements = sql(&build(vec![
            table("t", &[("c", &format!("app.[{called}](id)"))]),
            function(declared, FUNCTION),
        ])
        .unwrap());
        assert!(
            at(&statements, &format!("FUNCTION [app].[{declared}]"))
                < at(&statements, "ADD [c] AS"),
            "{declared}/{called}: {statements:#?}"
        );
    }
}

#[test]
fn an_unrelated_name_is_no_read_by_a_module_beside_a_mapped_letter() {
    // The view names `other`, not the pending `łódź`: no edge, so it is
    // created with the bare table, before the column.
    let statements = sql(&build(vec![
        view("v", "SELECT other FROM app.t"),
        table_with("t", |t| {
            t.columns.insert("other".into(), int());
            t.computed.insert("łódź".into(), persisted("app.g(v)"));
        }),
        function("g", FUNCTION),
    ])
    .unwrap());
    assert!(
        at(&statements, "VIEW [app].[v]") < at(&statements, "ADD [łódź] AS"),
        "{statements:#?}"
    );
}
