//! The orders only scratch's own rules decide (DEC-2389.1): everything else
//! the schedule derives from what a part's text names. Each is measured on
//! PostgreSQL 16 and 18 and compiled there by `scratch_rules_compile_on_both_servers`.
use super::*;
use crate::generated::SET_EXPRESSION_ARRIVED_IN;
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
            options: Default::default(),
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
        // A string is data unless an OID-alias type reads it: the objects
        // whose strings equal each other's names are made in either order, and
        // the ones a cast names are made before it.
        vec![
            declared_type("app.a", domain_with("text", Some("'b'"), None)),
            declared_type("app.b", domain_with("text", Some("'a'"), None)),
        ],
        vec![
            declared_type("app.a", domain_with("text", None, Some("VALUE <> 'b'"))),
            declared_type("app.b", domain_with("text", None, Some("VALUE <> 'a'"))),
        ],
        vec![
            declared_type(
                "app.a",
                domain_with("text", Some("'app.b'::regtype::text"), None),
            ),
            declared_type("app.b", mood()),
        ],
        vec![
            module(
                "app.f(regtype)",
                ModuleKind::Function,
                "(x regtype DEFAULT 'app.b') RETURNS integer LANGUAGE sql RETURN 1",
            ),
            declared_type("app.b", mood()),
        ],
        vec![
            module("app.v", ModuleKind::View, "SELECT 'w' AS n"),
            module("app.w", ModuleKind::View, "SELECT 'v' AS n"),
            routine("g", "sql", " SELECT length('h') "),
            routine("h", "sql", " SELECT length('g') "),
            create("t", held_table()),
            function("f"),
        ],
        vec![
            module(
                "app.v",
                ModuleKind::View,
                "SELECT 'app.w'::regclass::oid AS o",
            ),
            module("app.w", ModuleKind::View, "SELECT 1 AS n"),
            create("t", held_table()),
            function("f"),
        ],
        // A name matched by accident closes no cycle (DEC-2389.4): `VALUE` is
        // not the domain of that name, a parameter is not the index of it.
        vec![
            declared_type(
                "app.positive",
                domain_with("integer", None, Some("VALUE > 0")),
            ),
            declared_type("app.value", domain_with("app.positive", None, None)),
        ],
        parameter_named_like_an_index(),
        // Round 4: the view `app.text` that reads a table whose column is of
        // type `text`, with the table held for its function (on 16).
        vec![
            create("t", held_table_with_text()),
            view_named_like_the_type(),
            function("f"),
        ],
        // A view named like the function a generation calls, and one named by
        // a qualified type spelling.
        vec![
            create("t", held_table()),
            module("app.f", ModuleKind::View, "SELECT * FROM app.t"),
            function("f"),
        ],
        // A type spelling is no call: the view `app.text` and a routine of
        // that name, reading the table whose column is of type `text`, follow
        // it (DEC-2389.4); a routine of the name that a generation or a default
        // calls still goes first.
        vec![create("t", text_typed()), view_named_like_the_type()],
        vec![
            create("t", text_typed()),
            routine("text", "sql", " SELECT count(*)::int FROM app.t "),
        ],
        vec![
            create("t", calling_text("generated")),
            routine("text", "sql", " SELECT $1 + 1 "),
        ],
        vec![
            create("t", calling_text("default")),
            routine("text", "sql", " SELECT $1 + 1 "),
        ],
        // A generation that only says the name of a routine reading its table.
        vec![
            create("t", saying_a_routine()),
            routine("f", "sql", " SELECT count(*)::int FROM app.t "),
        ],
        // A declared type is resolved at creation and a body or default is not.
        delayed_view(declaring(
            "r app.v%ROWTYPE; c CURSOR (p app.v) FOR SELECT 1;",
            "DECLARE x app.v.g%TYPE; BEGIN NULL; END;",
        )),
        delayed_view(declaring(
            "x int := (SELECT count(*) FROM app.v); c CURSOR FOR SELECT * FROM app.v;",
            "RETURN (SELECT count(*) FROM app.v)::int;",
        )),
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

/// Bodies the engine resolves at creation (`LANGUAGE sql`, measured the same
/// with a string body, `BEGIN ATOMIC` and `RETURN`) that call each other have
/// no order. Each edge rests on a name in the other's text, a guess that gives
/// way where it closes a cycle (DEC-2389.4), so the plan's order is built and
/// the engine names the routine it lacks (`scratch_refusals_name_what_is_missing`).
#[test]
fn routines_that_call_each_other_at_creation_fall_back_to_the_plans_order() {
    let changes = [create("t", defaulted()), sql("f", "g"), sql("g", "f")];
    let built = order(&changes, None);
    assert!(
        at(&built, "function app.f(integer)") < at(&built, "function app.g(integer)"),
        "{built:?}"
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
/// reads it has no order on a server that holds the table whole. The edge from
/// the table to it is a guess and gives way (DEC-2389.4): the plan's order is
/// built, with the routine ahead of the generation it computes.
#[test]
fn a_late_body_reading_a_table_does_not_follow_it() {
    let table = held_table();
    let reader = |language: &str, body: &str| routine("f", language, body);
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
        let changes = [
            create("t", table.clone()),
            reader("sql", " SELECT count(*)::int FROM app.t "),
        ];
        let sql = self::order(&changes, version);
        let last = sql.iter().rposition(|d| d.contains("app.t")).unwrap();
        assert!(at(&sql, "function app.f(integer)") < last, "{sql:?}");
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

/// A PL/pgSQL routine over `declare` (its `DECLARE` section) and `body`.
fn declaring(declare: &str, body: &str) -> Change {
    routine(
        "r",
        "plpgsql",
        &format!(" DECLARE {declare} BEGIN {body} RETURN 1; END "),
    )
}

/// The table is held back by the function its generated column calls, and the
/// view reads that column, so it waits for the function too; the routine is
/// planned before that function. `r` follows `v` exactly when its text names it.
fn delayed_view(routine: Change) -> Vec<Change> {
    let view = module("app.v", ModuleKind::View, "SELECT g FROM app.t");
    vec![create("t", held_table()), view, routine, function("f")]
}

/// What a PL/pgSQL routine declares is resolved when it is created, unlike its
/// body (measured on 16.15 and 18.6, every object absent): the type of a
/// variable (`app.v`, `app.v%ROWTYPE`, `app.v.g%TYPE`, an array of it, in a
/// nested block too) and of a cursor's parameter are `42704`/`42P01`.
#[test]
fn a_declared_type_in_a_late_routine_follows_what_it_names() {
    let nested = ("", "DECLARE x app.v%ROWTYPE; BEGIN NULL; END;");
    let declared = [
        ("r app.v%ROWTYPE;", ""),
        ("r app.v;", ""),
        ("r app.v[];", ""),
        ("x app.v.g%TYPE;", ""),
        ("x CONSTANT app.v := NULL;", ""),
        ("c CURSOR (p app.v) FOR SELECT 1;", ""),
        nested,
    ];
    for version in [None, Some(160_000)] {
        for (declare, body) in declared {
            let order = order(&delayed_view(declaring(declare, body)), version);
            assert!(
                at(&order, "view app.v") < at(&order, "function app.r(integer)"),
                "{declare} {body} on {version:?}: {order:?}"
            );
        }
    }
}

/// Negative: what a routine only runs stays late, the declaration's defaults,
/// a cursor's query and a variable's name included (measured: created with
/// every object absent), so the routine keeps its place before the view.
#[test]
fn a_late_body_naming_a_view_does_not_follow_it() {
    let late = [
        ("", "RETURN (SELECT count(*) FROM app.v)::int;"),
        ("x int := (SELECT count(*) FROM app.v);", ""),
        ("c CURSOR FOR SELECT * FROM app.v;", ""),
        ("v int := 1;", ""),
        ("x text := 'a'::app.v;", ""),
        ("a ALIAS FOR $1;", "PERFORM app.v;"),
    ];
    for version in [None, Some(160_000)] {
        for (declare, body) in late {
            let order = order(&delayed_view(declaring(declare, body)), version);
            assert!(
                at(&order, "function app.r(integer)") < at(&order, "view app.v"),
                "{declare} {body} on {version:?}: {order:?}"
            );
        }
    }
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

fn domain_with(base: &str, default: Option<&str>, check: Option<&str>) -> pbps_model::ManagedType {
    pbps_model::ManagedType::Domain {
        base: base.parse().unwrap(),
        not_null: false,
        default: default.map(str::to_owned),
        checks: check
            .map(|c| ("c".into(), c.to_owned()))
            .into_iter()
            .collect(),
        collation: None,
    }
}

fn defaulting(ty: &str, default: &str) -> Table {
    let mut table = Table::default();
    let mut column = Column::new(ty.parse().unwrap());
    column.value_source = ValueSource::Default(default.into());
    table.columns.insert("c".into(), column);
    table
}

/// What a string says is data unless an OID-alias type reads it (DEC-2389.3,
/// measured on 16.15 and 18.6, every named object absent): a domain
/// `AS text DEFAULT 'b'` or a view selecting `'b'` is created with no `b`. So
/// domains, views and routines whose strings equal one another's names are no
/// cycle, though the engine creates them in either order.
#[test]
fn a_string_equal_to_an_objects_name_is_data_and_no_cycle() {
    let domains = [
        [
            declared_type("app.a", domain_with("text", Some("'b'"), None)),
            declared_type("app.b", domain_with("text", Some("'a'"), None)),
        ],
        [
            declared_type("app.a", domain_with("text", None, Some("VALUE <> 'b'"))),
            declared_type("app.b", domain_with("text", None, Some("VALUE <> 'a'"))),
        ],
        // The one that names `d2` as a base is no cycle with the one whose
        // default only says `d3`.
        [
            declared_type("app.d2", domain_with("text", Some("'d3'"), None)),
            declared_type("app.d3", domain_with("app.d2", None, None)),
        ],
    ];
    for version in [None, Some(160_000)] {
        for changes in &domains {
            assert_eq!(order(changes, version).len(), 2, "{changes:?}");
        }
    }
    // The table that holds the modules back keeps the modules' own edges.
    let modules = [
        module("app.v", ModuleKind::View, "SELECT 'w' AS n"),
        module("app.w", ModuleKind::View, "SELECT 'v' AS n"),
        routine("f", "sql", " SELECT length('g') "),
        routine("g", "sql", " SELECT length('f') "),
        create("t", held_table()),
        function("h"),
    ];
    assert!(at(&order(&modules, None), "view app.v") < at(&order(&modules, None), "view app.w"));
}

/// Negative: where an OID-alias type reads a string the engine resolves it
/// when it creates the change (measured: `42704`, `42P01`, `42883`), so it
/// names its object, in each spelling that reads it and in a parameter's
/// default.
#[test]
fn a_string_an_oid_alias_type_reads_still_orders() {
    let reads = [
        "'app.b'::regtype::text",
        "(regtype 'app.b')::text",
        "regtype('app.b')::text",
        "CAST('app.b' AS regtype)::text",
        "'app.b'::pg_catalog.regtype::text",
    ];
    for version in [None, Some(160_000)] {
        for default in reads {
            let changes = [
                declared_type("app.a", domain_with("text", Some(default), None)),
                declared_type("app.b", mood()),
            ];
            let order = order(&changes, version);
            assert!(
                at(&order, "type app.b") < at(&order, "type app.a"),
                "{default}: {order:?}"
            );
        }
        // A routine's parameter of such a type, in a header.
        let header = |parameter: &str| {
            [
                module(
                    "app.f(regtype)",
                    ModuleKind::Function,
                    &format!("(x {parameter}) RETURNS integer LANGUAGE sql RETURN 1"),
                ),
                declared_type("app.b", mood()),
            ]
        };
        let typed = order(&header("regtype DEFAULT 'app.b'"), version);
        assert!(
            at(&typed, "type app.b") < at(&typed, "function app.f(regtype)"),
            "{typed:?}"
        );
        let text = order(&header("text DEFAULT 'app.b'"), version);
        assert!(
            at(&text, "function app.f(regtype)") < at(&text, "type app.b"),
            "{text:?}"
        );
        // The same strings in a module, behind a table held for its function.
        let held = |view: &str| {
            [
                module("app.v", ModuleKind::View, view),
                module("app.w", ModuleKind::View, "SELECT 1 AS n"),
                create("t", held_table()),
                function("f"),
            ]
        };
        let named = order(&held("SELECT 'app.w'::regclass::oid AS o"), version);
        assert!(
            at(&named, "view app.w") < at(&named, "view app.v"),
            "{named:?}"
        );
        let data = order(&held("SELECT 'app.w' AS o"), version);
        assert!(
            at(&data, "view app.v") < at(&data, "view app.w"),
            "{data:?}"
        );
    }
}

/// A string equal to an index's name is data as well: the view that selects
/// it keeps its place before the table that is held back (and
/// `a_reader_naming_an_index_follows_the_table_that_keeps_it` is the cast).
#[test]
fn a_string_equal_to_an_indexs_name_does_not_follow_its_table() {
    let changes = [
        module("app.v", ModuleKind::View, "SELECT 'app.t_ix' AS o"),
        create("t", held_table()),
        function("f"),
    ];
    let order = order(&changes, None);
    assert!(
        at(&order, "view app.v") < at(&order, "table app.t"),
        "{order:?}"
    );
}

/// The premise of reading the strings of a text alone: no declared column or
/// domain is of an OID-alias type, so none reads the string of a default
/// without the cast showing in that default's text.
#[test]
fn no_declared_column_or_domain_is_of_an_oid_alias_type() {
    let pg = crate::Postgres::new();
    for changes in [
        vec![create("t", defaulting("regtype", "'app.b'"))],
        vec![declared_type("app.a", domain_with("regclass", None, None))],
    ] {
        assert!(super::tests::build_reconstruction(&pg, &changes).is_err());
    }
}

fn saying_a_routine() -> Table {
    let mut table = Table::default();
    table.columns.insert("a".into(), int());
    table
        .columns
        .insert("g".into(), generated("a + length('f')"));
    table
}

/// A generated column whose expression only says the name of a routine that
/// reads its table is no call of it: the routine would wait for the whole
/// table and the column for the routine, a cycle the engine does not have.
#[test]
fn a_generation_saying_a_routines_name_does_not_wait_for_it() {
    let mut table = saying_a_routine();
    let reader = routine("f", "sql", " SELECT count(*)::int FROM app.t ");
    for version in [None, Some(160_000)] {
        let order = order(&[create("t", table.clone()), reader.clone()], version);
        assert_eq!(order.len(), 3, "{order:?}");
        assert!(
            at(&order, "column app.t.g") < at(&order, "function app.f(integer)"),
            "{order:?}"
        );
    }
    // Two tables whose generations only say each other's names.
    let saying = |other: &str| {
        let mut t = Table::default();
        t.columns.insert("a".into(), int());
        t.columns
            .insert("g".into(), generated(&format!("a + length('{other}')")));
        t
    };
    assert_eq!(
        order(&[create("t", saying("u")), create("u", saying("t"))], None).len(),
        2
    );
    // Negative: a cast to an OID-alias type names the routine, and then the
    // table is held for it, though the routine's text reading the table is the
    // guess that gives way.
    let named = generated("a + length('app.f(integer)'::regprocedure::text)");
    table.columns.insert("g".into(), named);
    let order = order(&[create("t", table), reader], None);
    assert!(
        at(&order, "function app.f(integer)") < at(&order, "table app.t"),
        "{order:?}"
    );
}

/// An index named `id` on the held table, and a routine whose parameter is
/// `id`: the parameter is read as the index (a name matched in a text), which
/// closes a cycle with the routine the table waits for (round 2, on 16).
fn parameter_named_like_an_index() -> Vec<Change> {
    let mut table = held_table();
    table.indexes.clear();
    table.indexes.insert("id".into(), keyed("id", false, None));
    // The view is first, so only its edge keeps it behind the table.
    vec![
        module(
            "app.v",
            ModuleKind::View,
            "SELECT 'app.id'::regclass::oid AS o",
        ),
        create("t", table),
        module(
            "app.f(integer)",
            ModuleKind::Function,
            "(id integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN id",
        ),
    ]
}

/// A guess that closes a cycle gives way, so the plan's order stands there
/// (DEC-2389.4): `VALUE` in a check is not the domain `app.value`, and a
/// parameter is not the index of that name, though the edges their names draw
/// would close a cycle with an edge the engine does need.
#[test]
fn a_name_matched_by_accident_does_not_close_a_cycle() {
    let positive = declared_type(
        "app.positive",
        domain_with("integer", None, Some("VALUE > 0")),
    );
    let value = declared_type("app.value", domain_with("app.positive", None, None));
    for version in [None, Some(160_000)] {
        let order = order(&[positive.clone(), value.clone()], version);
        assert!(
            at(&order, "type app.positive") < at(&order, "type app.value"),
            "{order:?}"
        );
        let order = order_of_parameter(version);
        // The table is held whole for the routine, which keeps the table's
        // guess from reaching it.
        if version == Some(160_000) {
            assert!(
                at(&order, "function app.f(integer)") < at(&order, "table app.t"),
                "{order:?}"
            );
        }
        // Negative: the view names the index in a cast, a guess that is in no
        // cycle, so it still follows the table.
        assert!(
            at(&order, "table app.t") < at(&order, "view app.v"),
            "{order:?}"
        );
    }
}

fn order_of_parameter(version: Option<i64>) -> Vec<String> {
    order(&parameter_named_like_an_index(), version)
}

/// The same input gives the same order, whichever cycle it fell back from.
#[test]
fn the_fallback_gives_the_same_order_twice() {
    let inputs = [
        parameter_named_like_an_index(),
        vec![
            create("t", held_table_with_text()),
            view_named_like_the_type(),
            function("f"),
        ],
        vec![
            create("t", held_table()),
            function("f"),
            sql("g", "h"),
            sql("h", "g"),
        ],
        vec![
            declared_type("app.a", domain_with("text", Some("'b'"), None)),
            declared_type("app.b", domain_with("text", Some("'a'"), None)),
        ],
    ];
    for changes in inputs {
        for version in [None, Some(160_000), Some(180_000)] {
            assert_eq!(order(&changes, version), order(&changes, version));
        }
    }
}

/// Negative: a cycle of edges the engine needs by construction stays the
/// schedule's named refusal, guesses or none: two routines each declared to
/// need the other.
#[test]
fn a_cycle_of_declared_dependencies_stays_the_named_refusal() {
    let changes = [create("t", held_table()), function("f"), function("g")];
    let ids =
        |name: &str| -> pbps_model::ModuleId { format!("app.{name}(integer)").parse().unwrap() };
    let deps: pbps_model::ModuleDeps =
        [(ids("f"), [ids("g")].into()), (ids("g"), [ids("f")].into())].into();
    let refused = Reconstruction::with_dependencies(
        &crate::Postgres::new(),
        &crate::resolver::reconstruct::fixture_plan(&changes),
        &crate::resolver::reconstruct::fixture_ids(&changes),
        &deps,
    );
    match refused {
        Err(ReconstructError::Cycle(message)) => assert!(
            message.contains("app.f(integer)") && message.contains("app.g(integer)"),
            "{message}"
        ),
        other => panic!(
            "expected the named cycle, got {:?}",
            other.map(|r| r.steps.len())
        ),
    }
}

/// What the guesses leave to the engine it refuses by name, with the way out
/// (DEC-2389.4): routines that need each other, and a reader of a table that
/// a server which cannot replace an expression holds back for the routine it
/// reads through.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn scratch_refusals_name_what_is_missing() {
    use pbps_db::{Conn, Driver};
    let fixtures = [
        (
            vec![create("t", defaulted()), sql("f", "g"), sql("g", "f")],
            "function app.g(integer)",
        ),
        (
            vec![
                create("t", held_table()),
                routine("f", "sql", " SELECT count(*)::int FROM app.t "),
            ],
            "app.t",
        ),
    ];
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(variable).expect("both PostgreSQL endpoints are required");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let version = crate::roles::server_version_num(&mut admin).await.unwrap();
        let pg = crate::Postgres::new().with_server_version(version);
        for (at, (changes, missing)) in fixtures.iter().enumerate() {
            // A server that can replace an expression creates the table bare,
            // and the routine reading it compiles.
            if at == 1 && version >= SET_EXPRESSION_ARRIVED_IN {
                continue;
            }
            let database = format!("pbps_s2389_refuse_{}_{at}", std::process::id());
            admin
                .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
                .await
                .unwrap();
            let refused: Result<(), String> = async {
                let mut conn =
                    Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
                        .await
                        .map_err(|e| e.to_string())?;
                conn.execute("CREATE SCHEMA app")
                    .await
                    .map_err(|e| e.to_string())?;
                super::tests::build_reconstruction(&pg, changes)
                    .map_err(|e| e.to_string())?
                    .compile(&pg, &pbps_model::SessionZone::utc(), &mut conn)
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            let why = refused.expect_err("the engine has no order for it");
            assert!(
                why.contains("does not exist")
                    && why.contains(missing)
                    && why.contains("`depends_on:`"),
                "fixture {at}: {why}"
            );
        }
    }
}

/// `app.t(x text)`: a column whose type spelling is the name of a view or a
/// routine, which no type spelling calls.
fn text_typed() -> Table {
    let mut table = Table::default();
    table
        .columns
        .insert("x".into(), Column::new("text".parse().unwrap()));
    table
}

fn view_named_like_the_type() -> Change {
    module("app.text", ModuleKind::View, "SELECT x FROM app.t")
}

/// A table whose generated column, or default, calls the routine `app.text`.
fn calling_text(kind: &str) -> Table {
    let mut table = Table::default();
    table.columns.insert("id".into(), int());
    let mut column = int();
    column.value_source = if kind == "generated" {
        ValueSource::Generated(Generated {
            expression: "app.text(id)".into(),
            stored: true,
        })
    } else {
        ValueSource::Default("app.text(1)".into())
    };
    table.columns.insert("c".into(), column);
    table
}

/// Only an expression that calls a routine makes the call edge the engine
/// needs (DEC-2389.4). A column's type spelling does not: the type `text` of
/// `app.t(x text)` is no call of the view `app.text` that reads the table, and
/// a stated view-before-table edge would have put the view first (round 3).
#[test]
fn a_type_spelling_is_no_call_of_a_view_or_routine_named_like_it() {
    for version in [None, Some(160_000), Some(180_000)] {
        let order = order(
            &[create("t", text_typed()), view_named_like_the_type()],
            version,
        );
        assert!(
            at(&order, "table app.t") < at(&order, "view app.text"),
            "{order:?}"
        );
        // A routine of that name, called nowhere, orders nothing hard either.
        let reader = routine("text", "sql", " SELECT count(*)::int FROM app.t ");
        let order = self::order(&[create("t", text_typed()), reader], version);
        assert!(
            at(&order, "table app.t") < at(&order, "function app.text(integer)"),
            "{order:?}"
        );
        // A function is no type: where it is named by a type spelling alone it
        // is no reference, so a routine of that name that reads nothing keeps
        // its place after the table (round 4).
        let idle = routine("text", "sql", " SELECT $1 + 1 ");
        let order = self::order(&[create("t", text_typed()), idle], version);
        assert!(
            at(&order, "table app.t") < at(&order, "function app.text(integer)"),
            "{order:?}"
        );
    }
}

/// Negative: a routine called by an expression of the table, a generation or
/// a default, is the engine's need whatever it is named, `text` included, and
/// still goes first.
#[test]
fn a_routine_an_expression_calls_still_precedes_it_though_named_like_a_type() {
    for version in [None, Some(160_000), Some(180_000)] {
        for kind in ["generated", "default"] {
            let order = order(
                &[
                    create("t", calling_text(kind)),
                    routine("text", "sql", " SELECT $1 + 1 "),
                ],
                version,
            );
            // The step that carries the expression is the table's last: the
            // whole table, the generated column or the default.
            let carrying = order.iter().rposition(|d| d.contains("app.t")).unwrap();
            assert!(
                at(&order, "function app.text(integer)") < carrying,
                "{kind} on {version:?}: {order:?}"
            );
        }
    }
}

/// The kind of edge, in the schedule scratch settles, from the module of
/// `kind` named `name` to the generated expression of `changes`, or to the
/// table's first step when none is generated.
fn call_edge(changes: &[Change], kind: ModuleKind, name: &str) -> Option<pbps_diff::order::Source> {
    let pg = crate::Postgres::new().with_server_version(180_000);
    let plan = crate::resolver::reconstruct::fixture_plan(changes);
    let ids = crate::resolver::reconstruct::fixture_ids(changes);
    let prepared =
        pbps_diff::order::scratch::prepare(&plan, &ids, &pbps_model::ModuleDeps::new(), &pg)
            .unwrap();
    let steps = &prepared.changes().changes;
    let provider = steps
        .iter()
        .position(|p| {
            matches!(&p.change, Change::CreateModule { id, module }
                if module.kind == kind && id.name() == name)
        })
        .unwrap();
    let carrying = steps
        .iter()
        .position(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
        .or_else(|| {
            steps
                .iter()
                .position(|p| matches!(p.change, Change::CreateTable { .. }))
        })
        .unwrap();
    prepared
        .edges()
        .iter()
        .find(|c| c.before == provider && c.after == carrying)
        .map(|c| c.source)
}

/// A stated call edge needs a routine as the provider and an expression as the
/// text (DEC-2389.4): a function the generated expression calls holds the table
/// behind it, while a view, a procedure or a function that only a type spelling
/// names is a guess.
#[test]
fn a_call_edge_is_stated_only_for_a_function_an_expression_calls() {
    use pbps_diff::order::Source;
    let called = [
        create("t", held_table()),
        function("f"),
        module("app.f", ModuleKind::View, "SELECT * FROM app.t"),
        module(
            "app.f(text)",
            ModuleKind::Procedure,
            "(text) LANGUAGE sql AS $$ SELECT 1 $$",
        ),
    ];
    assert_eq!(
        call_edge(&called, ModuleKind::Function, "f"),
        Some(Source::Lexical)
    );
    assert_eq!(
        call_edge(&called, ModuleKind::View, "f"),
        Some(Source::Guess)
    );
    assert_eq!(
        call_edge(&called, ModuleKind::Procedure, "f"),
        Some(Source::Guess)
    );
    // Only the type spelling `text` names the function, which nothing calls,
    // and a function is no type: no edge at all (round 4).
    let spelled = [create("t", text_typed()), function("text")];
    assert_eq!(call_edge(&spelled, ModuleKind::Function, "text"), None);
}

/// A name is matched by the position it occurs in (DEC-2389.4). A built-in type
/// spelling names nothing a declaration made, so `x text` makes no edge with a
/// view `app.text`; a spelling qualified by its schema names a user-defined
/// type, the row type of that view among them (measured on 16 and 18: absent,
/// `42704`), and it does.
#[test]
fn a_built_in_type_spelling_names_nothing_and_a_qualified_one_names_what_it_spells() {
    use pbps_diff::order::Source;
    let builtin = [create("t", text_typed()), view_named_like_the_type()];
    assert_eq!(call_edge(&builtin, ModuleKind::View, "text"), None);
    // Negative: `app.t(x app.text)` needs the row type of the view.
    let mut qualified = Table::default();
    qualified
        .columns
        .insert("x".into(), Column::new("app.text".parse().unwrap()));
    let reading = [
        create("t", qualified),
        module("app.text", ModuleKind::View, "SELECT 1 AS n"),
    ];
    assert_eq!(
        call_edge(&reading, ModuleKind::View, "text"),
        Some(Source::Guess)
    );
    for version in [None, Some(160_000), Some(180_000)] {
        let order = self::order(&reading, version);
        assert!(
            at(&order, "view app.text") < at(&order, "table app.t"),
            "{order:?}"
        );
    }
}

/// A function is no type, so a type spelling that is qualified, and so is read,
/// still does not name it: the enum `app.f` of a column and the function
/// `app.f(integer)` share a name and no edge.
#[test]
fn a_function_is_not_named_by_a_type_spelling() {
    let mut typed = Table::default();
    typed
        .columns
        .insert("x".into(), Column::new("app.f".parse().unwrap()));
    let changes = [
        create("t", typed),
        declared_type("app.f", mood()),
        function("f"),
    ];
    assert_eq!(call_edge(&changes, ModuleKind::Function, "f"), None);
}

/// `app.t(x text, g integer GENERATED ALWAYS AS (app.f(z)) STORED, z integer)`,
/// a view `app.text` that reads it and an independent `app.f(integer)` (round
/// 4, on 16): the type `text` names no view, and where a name does collide the
/// guess against the order given goes first.
fn held_table_with_text() -> Table {
    let mut table = held_table();
    table
        .columns
        .insert("x".into(), Column::new("text".parse().unwrap()));
    table
}

#[test]
fn a_view_named_like_a_builtin_type_follows_the_table_it_reads() {
    let changes = [
        create("t", held_table_with_text()),
        view_named_like_the_type(),
        function("f"),
    ];
    for version in [None, Some(160_000), Some(180_000)] {
        let order = self::order(&changes, version);
        assert!(
            at(&order, "table app.t") < at(&order, "view app.text"),
            "{version:?}: {order:?}"
        );
    }
}

/// A guess-only cycle whose forward guess is the real one: a view named like
/// the function a generation calls (`app.f`) reads the table, which waits for
/// that function. Nothing but the name collides, so no rule about positions
/// removes the guess against the order, and the view keeps its place after the
/// table only because that guess goes first (round 4).
#[test]
fn a_view_named_like_a_called_function_still_follows_the_table_it_reads() {
    let changes = [
        create("t", held_table()),
        module("app.f", ModuleKind::View, "SELECT * FROM app.t"),
        function("f"),
    ];
    for version in [None, Some(160_000), Some(180_000)] {
        let order = self::order(&changes, version);
        assert!(
            at(&order, "table app.t") < at(&order, "view app.f"),
            "{version:?}: {order:?}"
        );
    }
}

/// Whether the schedule scratch settles states a guess from a step of the table
/// `from` to a step of the table `to`.
fn guesses_between(changes: &[Change], from: &str, to: &str) -> bool {
    let pg = crate::Postgres::new().with_server_version(180_000);
    let plan = crate::resolver::reconstruct::fixture_plan(changes);
    let ids = crate::resolver::reconstruct::fixture_ids(changes);
    let prepared =
        pbps_diff::order::scratch::prepare(&plan, &ids, &pbps_model::ModuleDeps::new(), &pg)
            .unwrap();
    let steps = &prepared.changes().changes;
    let of = |at: usize, table: &str| steps[at].change.table().is_some_and(|t| t.name == table);
    prepared.edges().iter().any(|c| {
        c.source == pbps_diff::order::Source::Guess && of(c.before, from) && of(c.after, to)
    })
}

/// The rule for indexes reads no type spelling: an index is no type. A table
/// `u(x text)` does not follow an index of another table that is named `text`,
/// where a table whose expression casts to that index (`'app.text'::regclass`)
/// still does.
#[test]
fn an_index_is_not_named_by_a_type_spelling() {
    let mut keeping = Table::default();
    keeping.columns.insert("a".into(), int());
    keeping
        .indexes
        .insert("text".into(), keyed("a", false, None));
    let mut naming = Table::default();
    let mut column = int();
    column.value_source = ValueSource::Default("'app.text'::regclass::oid::int".into());
    naming.columns.insert("n".into(), column);
    assert!(!guesses_between(
        &[create("t", keeping.clone()), create("u", text_typed())],
        "t",
        "u"
    ));
    assert!(guesses_between(
        &[create("t", keeping), create("u", naming)],
        "t",
        "u"
    ));
}
