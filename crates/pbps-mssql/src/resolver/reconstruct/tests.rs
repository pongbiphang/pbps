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
