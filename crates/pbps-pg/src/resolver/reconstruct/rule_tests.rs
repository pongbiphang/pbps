//! The orders only scratch's own rules decide (DEC-2389.1): everything else
//! the schedule derives from what a part's text names. Each is measured on
//! PostgreSQL 16 and 18 and compiled there by `scratch_rules_compile_on_both_servers`.
use super::*;
use pbps_model::{
    Column, Generated, Index, IndexColumn, IndexKey, Module, PartitionBound, PartitionBy,
    PartitionColumn, PartitionOf, Table, TableName, Uid, UidKind,
};

fn int() -> Column {
    Column::new("integer".parse().unwrap())
}

fn generated(expression: &str) -> Column {
    let mut column = int();
    column.value_source = ValueSource::Generated(Generated {
        expression: expression.into(),
        stored: true,
    });
    column
}

fn index(filter: Option<&str>) -> Index {
    keyed("a", false, filter)
}

fn keyed(column: &str, unique: bool, filter: Option<&str>) -> Index {
    Index {
        columns: vec![IndexColumn {
            key: IndexKey::Column(column.into()),
            descending: false,
            opclass: None,
        }],
        include: Vec::new(),
        unique,
        filter: filter.map(str::to_owned),
        method: Default::default(),
        storage_parameters: Default::default(),
    }
}

fn create(name: &str, table: Table) -> Change {
    Change::CreateTable {
        uid: Uid::derived(UidKind::Table, name, 0),
        name: TableName::new("app", name),
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    }
}

fn module(id: &str, kind: ModuleKind, definition: &str) -> Change {
    Change::CreateModule {
        id: id.parse().unwrap(),
        module: Box::new(Module {
            kind,
            description: None,
            definition: definition.into(),
        }),
    }
}

fn function(name: &str) -> Change {
    module(
        &format!("app.{name}(integer)"),
        ModuleKind::Function,
        "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
    )
}

pub(super) fn order(changes: &[Change], version: Option<i64>) -> Vec<String> {
    let pg = version.map_or_else(crate::Postgres::new, |v| {
        crate::Postgres::new().with_server_version(v)
    });
    super::tests::build_reconstruction(&pg, changes)
        .unwrap()
        .steps
        .iter()
        .map(|step| step.declaration.clone())
        .collect()
}

fn at(order: &[String], declaration: &str) -> usize {
    order
        .iter()
        .position(|d| d == declaration)
        .unwrap_or_else(|| panic!("{declaration} missing from {order:?}"))
}

/// A table whose generation reads a column declared after it is held whole
/// on a server without `SET EXPRESSION`, after the function it calls.
pub(super) fn held_table() -> Table {
    let mut table = Table::default();
    table.columns.insert("id".into(), int());
    table.columns.insert("a".into(), int());
    table.columns.insert("g".into(), generated("app.f(z)"));
    table.columns.insert("z".into(), int());
    table.indexes.insert("t_ix".into(), index(None));
    table
}

/// An index is a relation a reader can name by an OID-alias literal, and the
/// engine refuses the reader while the index is absent (42P01 on 16 and 18).
/// The table that keeps the index is held back by its function, so the reader
/// follows the table though nothing else names it.
#[test]
fn a_reader_naming_an_index_follows_the_table_that_keeps_it() {
    let changes = [
        create("t", held_table()),
        module(
            "app.v",
            ModuleKind::View,
            "SELECT 'app.t_ix'::regclass::oid AS o",
        ),
        module("app.w", ModuleKind::View, "SELECT 1 AS n"),
        function("f"),
    ];
    for version in [None, Some(160_000)] {
        let order = order(&changes, version);
        assert!(
            at(&order, "function app.f(integer)") < at(&order, "table app.t"),
            "{order:?}"
        );
        assert!(
            at(&order, "table app.t") < at(&order, "view app.v"),
            "{order:?}"
        );
        // Negative: a view that names nothing of the table keeps its place.
        assert!(
            at(&order, "view app.w") < at(&order, "table app.t"),
            "{order:?}"
        );
    }
}

/// A plain index is made with its table, so it is a name of that step, and
/// an index that holds a predicate is a step of its own, after every module
/// and every generated expression, so that each counts it among the names
/// made resolvable after it (and the engine has the routine it calls).
#[test]
fn an_index_that_holds_a_predicate_follows_every_module_and_generation() {
    let mut t = Table::default();
    t.columns.insert("a".into(), int());
    t.indexes.insert("t_plain".into(), index(None));
    t.indexes
        .insert("t_pred".into(), index(Some("a > app.f(1)")));
    let mut u = Table::default();
    u.columns.insert("id".into(), int());
    u.columns.insert("g".into(), generated("app.f(id)"));
    let changes = [
        create("t", t),
        create("u", u),
        function("f"),
        module("app.v", ModuleKind::View, "SELECT 1 AS n"),
    ];
    let order = order(&changes, Some(180_000));
    for judged in ["function app.f(integer)", "view app.v", "column app.u.g"] {
        assert!(
            at(&order, judged) < at(&order, "index t_pred on app.t"),
            "{order:?}"
        );
    }
    // Negative: the plain index is no step, and the predicate one does not
    // hold the table back.
    assert!(!order.iter().any(|d| d == "index t_plain on app.t"));
    assert!(at(&order, "table app.t") < at(&order, "function app.f(integer)"));
}

fn declared_type(name: &str, definition: pbps_model::ManagedType) -> Change {
    Change::CreateType {
        uid: Uid::derived(UidKind::Type, name, 0),
        name: name.parse().unwrap(),
        definition: Box::new(definition),
    }
}

fn mood() -> pbps_model::ManagedType {
    pbps_model::ManagedType::Enum {
        values: vec!["sad".into(), "happy".into()],
    }
}

fn domain_over(base: &str) -> pbps_model::ManagedType {
    pbps_model::ManagedType::Domain {
        base: base.parse().unwrap(),
        not_null: false,
        default: None,
        checks: Default::default(),
        collation: None,
    }
}

/// The engine refuses a column, a routine or a domain over a type that does
/// not exist (`42704`, DEC-2407.1), so a created type precedes each change that
/// names it, a domain over another type included, wherever the plan listed
/// them.
#[test]
fn a_created_type_precedes_what_names_it() {
    let mut typed = Table::default();
    typed
        .columns
        .insert("m".into(), Column::new("app.moody".parse().unwrap()));
    let changes = [
        create("t", typed),
        module(
            "app.f(integer)",
            ModuleKind::Function,
            "(i app.mood) RETURNS integer LANGUAGE sql RETURN 1",
        ),
        declared_type("app.moody", domain_over("app.mood")),
        declared_type("app.mood", mood()),
    ];
    let order = order(&changes, None);
    for (type_, reader) in [
        ("type app.mood", "type app.moody"),
        ("type app.moody", "table app.t"),
        ("type app.mood", "function app.f(integer)"),
    ] {
        assert!(at(&order, type_) < at(&order, reader), "{order:?}");
    }
    assert_eq!(order.len(), changes.len(), "{order:?}");
}

/// A parent's `SET DEFAULT` reaches every partition and replaces its own
/// (measured on 16 and 18), so a partition's own default follows it even
/// when the parent's waits for a function and the partition's does not.
pub(super) fn partitions(parent_default: &str, own: &str) -> [Change; 3] {
    let mut parent = Table::default();
    let mut column = int();
    column.value_source = ValueSource::Default(parent_default.into());
    parent.columns.insert("a".into(), column);
    parent.partition_by = Some(PartitionBy {
        external: false,
        columns: vec!["a".into()],
        strategy: Default::default(),
    });
    let child = Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", "p"),
            bound: PartitionBound::Default,
            columns: [(
                "a".into(),
                PartitionColumn {
                    default: Some(own.into()),
                    not_null: false,
                },
            )]
            .into(),
        }),
        ..Table::default()
    };
    [create("p", parent), create("c", child), function("f")]
}

#[test]
fn a_partitions_own_default_follows_its_parents_when_the_parents_waits() {
    let order = order(&partitions("app.f(1)", "7"), None);
    assert!(
        at(&order, "the default of app.p.a") < at(&order, "the default of app.c.a"),
        "{order:?}"
    );
    // Negative: with nothing waiting, the order is the one the plan gives.
    let order = super::rule_tests::order(&partitions("1", "7"), None);
    assert!(
        at(&order, "the default of app.p.a") < at(&order, "the default of app.c.a"),
        "{order:?}"
    );
}

/// A replica identity on an index over a late column follows that index
/// (measured: `42704` while the index is absent), with the column before both.
pub(super) fn identity_table() -> Table {
    let mut table = Table::default();
    table.columns.insert("id".into(), int());
    table
        .columns
        .insert("g".into(), generated("app.f(id)").not_null());
    table.indexes.insert("t_g".into(), keyed("g", true, None));
    table.replica_identity = Some(pbps_model::ReplicaIdentity::Index("t_g".into()));
    table
}

#[test]
fn a_replica_identity_follows_the_late_index_it_names() {
    let order = order(
        &[create("t", identity_table()), function("f")],
        Some(180_000),
    );
    let (g, index, identity) = (
        at(&order, "column app.t.g"),
        at(&order, "index t_g on app.t"),
        at(&order, "replica identity of app.t"),
    );
    assert!(g < index && index < identity, "{order:?}");
}

/// The fixtures above compile on 16 and 18, in the order the schedule
/// gives, and a partition keeps the default of its own. A routine that is
/// resolved when it is created follows what it names, and one that is not
/// does not wait for it.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn scratch_rules_compile_on_both_servers() {
    use pbps_db::{Conn, Driver};
    let mut predicate = Table::default();
    predicate.columns.insert("a".into(), int());
    predicate
        .indexes
        .insert("t_pred".into(), index(Some("a > app.f(1)")));
    let fixtures = [
        vec![
            create("t", held_table()),
            module(
                "app.v",
                ModuleKind::View,
                "SELECT 'app.t_ix'::regclass::oid AS o",
            ),
            function("f"),
        ],
        vec![
            create("t", predicate),
            function("f"),
            module("app.v", ModuleKind::View, "SELECT 1 AS n"),
        ],
        partitions("app.f(1)", "7").to_vec(),
        vec![create("t", identity_table()), function("f")],
        // Routines resolved late call one another: no order is needed between
        // them, and where one is resolved at creation only that one follows.
        vec![
            create("t", defaulted()),
            plpgsql("f", "g"),
            plpgsql("g", "f"),
        ],
        vec![create("t", defaulted()), sql("f", "g"), plpgsql("g", "f")],
        literal_readers().to_vec(),
        header_readers(),
        // A late body names the table, and an index of it, that waits for it.
        vec![
            create("t", held_table()),
            routine(
                "f",
                "plpgsql",
                " BEGIN REINDEX INDEX app.t_ix; \
                  RETURN (SELECT count(*) FROM app.t)::int; END ",
            ),
        ],
    ];
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(variable).expect("both PostgreSQL endpoints are required");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let version = crate::roles::server_version_num(&mut admin).await.unwrap();
        let pg = crate::Postgres::new().with_server_version(version);
        for (at, changes) in fixtures.iter().enumerate() {
            let database = format!("pbps_s2389_rules_{}_{at}", std::process::id());
            admin
                .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
                .await
                .unwrap();
            let result: Result<String, String> = async {
                let mut conn =
                    Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
                        .await
                        .map_err(|e| e.to_string())?;
                conn.execute("CREATE SCHEMA app")
                    .await
                    .map_err(|e| e.to_string())?;
                let mut reconstruction =
                    super::tests::build_reconstruction(&pg, changes).map_err(|e| e.to_string())?;
                reconstruction
                    .compile(&pg, &pbps_model::SessionZone::utc(), &mut conn)
                    .await
                    .map_err(|e| e.to_string())?;
                let rows = conn
                    .query(
                        "SELECT coalesce((SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef \
                         WHERE adrelid = to_regclass('app.c')), '') AS own",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(rows[0]
                    .try_get::<&str>("own")
                    .unwrap()
                    .unwrap_or("")
                    .to_owned())
            }
            .await;
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            let own = result.unwrap_or_else(|e| panic!("fixture {at} on {version}: {e}"));
            assert_eq!(
                own,
                if at == 2 { "7" } else { "" },
                "fixture {at} on {version}"
            );
        }
    }
}

/// A routine over `body`, in `language`, named `name(integer)`.
fn routine(name: &str, language: &str, body: &str) -> Change {
    module(
        &format!("app.{name}(integer)"),
        ModuleKind::Function,
        &format!("(integer) RETURNS integer LANGUAGE {language} IMMUTABLE AS $${body}$$"),
    )
}

fn plpgsql(name: &str, callee: &str) -> Change {
    routine(
        name,
        "plpgsql",
        &format!(" BEGIN RETURN app.{callee}($1); END "),
    )
}

fn sql(name: &str, callee: &str) -> Change {
    routine(name, "sql", &format!(" SELECT app.{callee}($1) "))
}

/// A table whose plain default calls `app.f`.
pub(super) fn defaulted() -> Table {
    let mut table = Table::default();
    let mut column = int();
    column.value_source = ValueSource::Default("app.f(1)".into());
    table.columns.insert("a".into(), column);
    table
}

/// Two PL/pgSQL routines that call each other are created in either order
/// (measured on 16 and 18: the body is parsed and resolves nothing), so a
/// table whose default calls one must not make them wait on one another.
#[test]
fn routines_that_call_each_other_late_are_no_cycle() {
    let changes = [
        create("t", defaulted()),
        plpgsql("f", "g"),
        plpgsql("g", "f"),
    ];
    for version in [None, Some(160_000)] {
        let order = order(&changes, version);
        assert_eq!(order.len(), 4, "{order:?}");
        // What the engine does need still holds: the default calls `f`.
        assert!(
            at(&order, "function app.f(integer)") < at(&order, "the default of app.t.a"),
            "{order:?}"
        );
    }
}

/// Negative: bodies the engine resolves at creation (`LANGUAGE sql`, measured
/// the same with a string body, `BEGIN ATOMIC` and `RETURN`) that call each
/// other leave no order, and stay the schedule's named refusal.
#[test]
fn routines_that_call_each_other_at_creation_stay_a_named_cycle() {
    let pg = crate::Postgres::new();
    let refused = |changes: &[Change]| match super::tests::build_reconstruction(&pg, changes) {
        Err(ReconstructError::Cycle(message)) => message,
        other => panic!("expected a cycle, got {:?}", other.map(|r| r.steps.len())),
    };
    let message = refused(&[create("t", defaulted()), sql("f", "g"), sql("g", "f")]);
    assert!(
        message.contains("app.f(integer)") && message.contains("app.g(integer)"),
        "{message}"
    );
    // Only one side resolves at creation: no cycle, and that side follows.
    let order = order(
        &[create("t", defaulted()), sql("f", "g"), plpgsql("g", "f")],
        None,
    );
    assert!(
        at(&order, "function app.g(integer)") < at(&order, "function app.f(integer)"),
        "{order:?}"
    );
}

pub(super) fn header_readers() -> Vec<Change> {
    let header = module(
        "app.f(integer)",
        ModuleKind::Function,
        "(i integer DEFAULT app.g(1)) RETURNS integer LANGUAGE plpgsql \
         AS $$ BEGIN RETURN app.g(i); END $$",
    );
    vec![create("t", defaulted()), header, plpgsql("g", "f")]
}

/// The header of a PL/pgSQL routine is resolved when it is created (a
/// parameter default calling an absent function is `42883`), unlike its body.
#[test]
fn a_late_routine_still_follows_what_its_header_names() {
    // `g` calls `f` only in its body, so `f` may keep waiting for `g`.
    let order = order(&header_readers(), None);
    assert!(
        at(&order, "function app.g(integer)") < at(&order, "function app.f(integer)"),
        "{order:?}"
    );
}

/// The same holds for a table: a PL/pgSQL body that reads the table of a
/// generated column the routine itself computes is created before the table
/// (measured: the body resolves nothing), where a `LANGUAGE sql` body that
/// reads it leaves no order, which stays the schedule's named refusal.
#[test]
fn a_late_body_reading_a_table_does_not_follow_it() {
    let table = held_table();
    let reader = |language: &str, body: &str| routine("f", language, body);
    let pg = crate::Postgres::new();
    for version in [None, Some(160_000)] {
        let changes = [
            create("t", table.clone()),
            reader(
                "plpgsql",
                " BEGIN RETURN (SELECT count(*) FROM app.t)::int; END ",
            ),
        ];
        let order = order(&changes, version);
        // The generated column, or on a server that cannot set an expression
        // later the whole table, is the last step of the table.
        let last = order.iter().rposition(|d| d.contains("app.t")).unwrap();
        assert!(at(&order, "function app.f(integer)") < last, "{order:?}");
        let pg = version.map_or_else(|| pg.clone(), |v| pg.clone().with_server_version(v));
        let changes = [
            create("t", table.clone()),
            reader("sql", " SELECT count(*)::int FROM app.t "),
        ];
        assert!(
            matches!(
                super::tests::build_reconstruction(&pg, &changes),
                Err(ReconstructError::Cycle(_))
            ),
            "{version:?}"
        );
    }
}

/// A `LANGUAGE sql` body is code to the engine, so the table and the index
/// its own literals name (`'app.t_ix'::regclass`) must exist when it is created
/// (measured: 42P01 on 16 and 18), though the whole body is one string. The
/// routine is declared first here, and follows both.
pub(super) fn literal_readers() -> [Change; 3] {
    let mut table = Table::default();
    table.columns.insert("a".into(), int());
    table.indexes.insert("t_ix".into(), index(None));
    let reader = routine(
        "f",
        "sql",
        " SELECT 'app.t_ix'::regclass::oid::int + 'app.u'::regclass::oid::int ",
    );
    let mut other = Table::default();
    other.columns.insert("a".into(), int());
    [reader, create("t", table), create("u", other)]
}

#[test]
fn a_sql_body_naming_an_index_and_a_table_by_literal_follows_both() {
    let eager = order(&literal_readers(), None);
    for named in ["table app.t", "table app.u"] {
        assert!(
            at(&eager, named) < at(&eager, "function app.f(integer)"),
            "{eager:?}"
        );
    }
    // Negative: the same body in PL/pgSQL names them late, so the plan's own
    // order stands.
    let [_, t, u] = literal_readers();
    let late = routine(
        "f",
        "plpgsql",
        " BEGIN RETURN 'app.t_ix'::regclass::oid::int + 'app.u'::regclass::oid::int; END ",
    );
    let lazy = order(&[late, t, u], None);
    assert!(
        at(&lazy, "function app.f(integer)") < at(&lazy, "table app.t"),
        "{lazy:?}"
    );
}

/// A PL/pgSQL body that names an index resolves it only when it runs
/// (measured), so it does not follow the table that keeps the index,
/// which here waits for the routine its generated column calls. A body
/// resolved at creation does (`a_reader_naming_an_index_follows_the_table_that_keeps_it`).
#[test]
fn a_late_body_naming_an_index_does_not_follow_the_table_keeping_it() {
    let body = " BEGIN REINDEX INDEX app.t_ix; RETURN 1; END ";
    let order = order(
        &[create("t", held_table()), routine("f", "plpgsql", body)],
        Some(160_000),
    );
    assert!(
        at(&order, "function app.f(integer)") < at(&order, "table app.t"),
        "{order:?}"
    );
}
