use super::*;
use indexmap::IndexMap;
use pbps_model::{
    ChangeSet, Column, ComputedColumn, IdsFile, Module, ModuleDeps, ModuleId, Uid, UidKind,
};

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
    let plan = pbps_diff::order::propose(ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    });
    Reconstruction::new(&Mssql, &plan, &IdsFile::default(), &ModuleDeps::new())
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
        assert!(create < routine && routine < column, "{statements:#?}");
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
        "SELECT a FROM t WHERE EXISTS (SELECT * FROM u)",
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
fn a_function_reading_one_computed_column_does_not_wait_for_a_column_that_calls_it() {
    // `d AS (id + 1)` and `c AS (f(id))`, with `f` reading `d`: bare table,
    // `d`, `f`, `c` compiles (measured on 17.0), though `f` reads a column of
    // the table whose other column must follow it. Held as one group that is
    // a cycle; held per column `d` calls nothing and is never held.
    for functions_first in [false, true] {
        let table = table("t", &[("c", "app.f(id)"), ("d", "id + 1")]);
        let changes = if functions_first {
            vec![reads("f", "d"), table]
        } else {
            vec![table, reads("f", "d")]
        };
        let statements = sql(&build(changes).unwrap());
        let create = at(&statements, "CREATE TABLE [app].[t]");
        let d = at(&statements, "ADD [d] AS");
        let routine = at(&statements, "FUNCTION [app].[f]");
        let c = at(&statements, "ADD [c] AS");
        assert!(
            create < d && d < routine && routine < c,
            "functions_first={functions_first}: {statements:#?}"
        );
    }
}

#[test]
fn a_function_reading_the_column_that_calls_it_is_still_a_cycle_per_column() {
    // Measured on 17.0: the column cannot be added before the function, nor
    // the function created before the column it reads (207). Another column
    // of the table makes no difference to that. (A cycle across two columns,
    // each function reading the other's column, is left to the engine: the
    // scratch route finds only a reader that its own column calls, and the
    // order it gives then fails to compile, naming the function: see the live
    // test.)
    let own = build(vec![
        table("t", &[("c", "app.f(id)"), ("d", "id + 1")]),
        reads("f", "c"),
    ])
    .expect_err("f reads the column that calls it");
    assert!(
        matches!(&own, ReconstructError::Cycle(subject) if subject.contains("app.t.c")),
        "{own:?}"
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
