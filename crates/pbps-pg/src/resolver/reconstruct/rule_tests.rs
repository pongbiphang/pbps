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
/// gives, and a partition keeps the default of its own.
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
