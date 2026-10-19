//! Creation parts expose relation names before generation binds (DEC-1503.1).
use super::*;
use pbps_model::Generated;
#[path = "../resolver/generated_fixtures.rs"]
mod cases;
use cases::shape;

pub(crate) fn ordinary(
    schema: &Schema,
    ids: &IdsFile,
    dialect: &dyn Dialect,
) -> Result<ChangeSet, String> {
    let empty = Schema::default();
    let before_ids = IdsFile::default();
    let cs = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &before_ids,
        },
        pbps_diff::Side { schema, ids },
        dialect,
        &pbps_model::Hints::default(),
    )
    .unwrap();
    super::schedule::schedule(
        cs,
        &super::schedule::Declarations {
            schema,
            sources: Sources {
                before: pbps_diff::Side {
                    schema: &empty,
                    ids: &before_ids,
                },
                after: ids,
            },
            module_deps: &ModuleDeps::new(),
        },
        &super::schedule::Facts::default(),
        dialect,
    )
    .map(|s| s.changes)
    .map_err(|e| e.to_string())
}

#[test]
#[allow(clippy::wildcard_enum_match_arm)]
fn generated_table_parts_keep_names_early_and_columns_in_declaration_order_on_every_version() {
    for case in ["literal", "reverse", "array", "collision"] {
        let schema = shape(case);
        let ids = identities(&schema);
        let unknown = ordinary(&schema, &ids, &pbps_pg::Postgres::new()).unwrap();
        for version in [160_000, 180_000] {
            let connected = ordinary(
                &schema,
                &ids,
                &pbps_pg::Postgres::new().with_server_version(version),
            )
            .unwrap();
            assert_eq!(unknown, connected, "{case}");
        }
        let a = TableName::new("app", "a");
        let mut columns = Vec::new();
        let mut table_at = None;
        let mut function_at = None;
        let mut last = 0;
        let mut view_at = 0;
        for (i, p) in unknown.changes.iter().enumerate() {
            match &p.change {
                Change::CreateTable { name, table, .. } if name == &a => {
                    table_at = Some(i);
                    columns.extend(table.columns.keys().cloned());
                }
                Change::AddColumn {
                    table, name, uid, ..
                } if table == &a => {
                    assert_eq!(Some(uid), ids.column_uid(&table.column(name)));
                    columns.push(name.clone());
                    last = i;
                }
                Change::CreateModule { id, .. } if id.name() == "f" => function_at = Some(i),
                Change::CreateModule { id, .. } if id.name() == "v" => view_at = i,
                _ => {}
            }
        }
        assert!(table_at.unwrap() < function_at.unwrap());
        assert!(function_at.unwrap() < last && last < view_at);
        assert_eq!(
            columns,
            schema.tables[&a]
                .columns
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn only_unsplittable_generated_tables_use_known_set_expression_capability() {
    for case in ["partition", "later", "prefix-later"] {
        let schema = shape(case);
        let ids = identities(&schema);
        for dialect in [
            pbps_pg::Postgres::new(),
            pbps_pg::Postgres::new().with_server_version(160_000),
        ] {
            let cs = ordinary(&schema, &ids, &dialect).unwrap();
            assert!(
                !cs.changes.iter().any(pbps_diff::order::generated::is_part),
                "{case}"
            );
        }
        let cs = ordinary(
            &schema,
            &ids,
            &pbps_pg::Postgres::new().with_server_version(180_000),
        )
        .unwrap();
        assert!(
            cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnExpression { .. })),
            "{case}"
        );
        assert!(
            !cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::AddColumn { .. })),
            "{case}"
        );
    }
}

#[test]
fn called_table_readers_require_full_shape_or_keep_the_named_hold() {
    for read in [
        "SELECT id FROM app.a LIMIT 1",
        "SELECT id * 2 FROM app.a LIMIT 1",
        "SELECT z FROM app.a LIMIT 1",
        "SELECT count(*)::integer FROM app.a",
        "SELECT (row_to_json(t)->>'id')::integer FROM ONLY app.a AS t",
        "SELECT 1 FROM app._a",
        "SELECT 1 FROM app._a_1",
        "TABLE app.a",
        "INSERT INTO app.a VALUES ($1, DEFAULT, 5) RETURNING id",
    ] {
        let mut schema = shape("plain");
        schema
            .modules
            .get_mut(&"app.f(integer)".parse().unwrap())
            .unwrap()
            .definition =
            format!("(integer) RETURNS integer LANGUAGE sql IMMUTABLE BEGIN ATOMIC {read}; END");
        let ids = identities(&schema);
        for pg in [
            pbps_pg::Postgres::new(),
            pbps_pg::Postgres::new().with_server_version(160_000),
        ] {
            let result = ordinary(&schema, &ids, &pg);
            let why = result.unwrap_err();
            assert!(
                why.contains("app.a") && why.contains("app.f"),
                "{read}: {why}"
            );
        }
        let cs = ordinary(
            &schema,
            &ids,
            &pbps_pg::Postgres::new().with_server_version(180_000),
        )
        .unwrap();
        assert!(
            cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnExpression { .. })),
            "{read}"
        );
        assert!(
            !cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::AddColumn { .. })),
            "{read}"
        );
    }
    // A name-independent callee keeps the primary split on every capability.
    let schema = shape("plain");
    for pg in [
        pbps_pg::Postgres::new(),
        pbps_pg::Postgres::new().with_server_version(160_000),
        pbps_pg::Postgres::new().with_server_version(180_000),
    ] {
        let cs = ordinary(&schema, &identities(&schema), &pg).unwrap();
        assert!(
            cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::AddColumn { .. }))
        );
    }
}

#[test]
fn composite_consumers_and_views_wait_for_the_complete_table() {
    for case in ["composite-check", "array", "collision"] {
        let schema = shape(case);
        let cs = ordinary(&schema, &identities(&schema), &pbps_pg::Postgres::new()).unwrap();
        let complete = cs
            .changes
            .iter()
            .rposition(
                |p| matches!(&p.change, Change::AddColumn { table, .. } if table.name == "a"),
            )
            .unwrap();
        let view = cs
            .changes
            .iter()
            .position(|p| matches!(&p.change, Change::CreateModule { id, .. } if id.name() == "v"))
            .unwrap();
        let consumer = cs
            .changes
            .iter()
            .position(|p| matches!(&p.change, Change::CreateTable { name, .. } if name.name == "b"))
            .unwrap();
        assert!(
            complete < view && complete < consumer,
            "{case}: {:?}",
            cs.changes
        );
    }
    let mut reverse = shape("reverse");
    reverse
        .tables
        .get_mut(&TableName::new("app", "b"))
        .unwrap()
        .checks
        .insert(
            "a_shape".into(),
            pbps_model::CheckConstraint {
                expression: "(ROW(1,2,3,4)::app.a).id > 0".into(),
            },
        );
    let why = ordinary(&reverse, &identities(&reverse), &pbps_pg::Postgres::new()).unwrap_err();
    assert!(
        why.contains("wait on one another") && why.contains("app.a") && why.contains("app.b"),
        "{why}"
    );
}

#[test]
fn generation_without_a_planned_function_or_only_a_comment_is_not_expanded() {
    let mut schema = shape("plain");
    schema
        .tables
        .get_mut(&TableName::new("app", "a"))
        .unwrap()
        .columns
        .get_mut("g")
        .unwrap()
        .value_source = ValueSource::Generated(Generated {
        expression: "id * 2 /* app.f(id) */".into(),
        stored: true,
    });
    let ids = identities(&schema);
    let cs = ordinary(&schema, &ids, &pbps_pg::Postgres::new()).unwrap();
    assert!(!cs.changes.iter().any(pbps_diff::order::generated::is_part));
    schema.modules.remove(&"app.f(integer)".parse().unwrap());
    schema
        .tables
        .get_mut(&TableName::new("app", "a"))
        .unwrap()
        .columns
        .get_mut("g")
        .unwrap()
        .value_source = ValueSource::Generated(Generated {
        expression: "app.missing(id)".into(),
        stored: true,
    });
    assert!(
        !ordinary(&schema, &ids, &pbps_pg::Postgres::new())
            .unwrap()
            .changes
            .iter()
            .any(pbps_diff::order::generated::is_part)
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn generated_table_parts_apply_with_the_declared_column_order_values_and_names() {
    use pbps_db::{Conn, Driver};
    for server in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(server).expect("both PostgreSQL endpoints are required");
        for case in [
            "literal",
            "reverse",
            "array",
            "collision",
            "partition",
            "later",
            "rows",
            "prefix-later",
            "prefix-later-offline",
            "prefix-only",
            "prefix-forward",
            "composite-check",
        ] {
            let database = format!("pbps_1503_{:016x}", rand::random::<u64>());
            let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
            admin
                .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
                .await
                .unwrap();
            let result: anyhow::Result<()> = async {
                let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}")).await?;
                conn.execute("CREATE SCHEMA app").await?;
                let dialect: Box<dyn Dialect> = if case.ends_with("-offline") {
                    Box::new(pbps_pg::Postgres::new())
                } else {
                    crate::engine::planning_dialect(&mut conn).await?
                };
                let shape_case = case.strip_suffix("-offline").unwrap_or(case);
                let schema = shape(shape_case);
                let cs = ordinary(&schema, &identities(&schema), dialect.as_ref()).map_err(anyhow::Error::msg)?;
                conn.execute("BEGIN").await?;
                for p in &cs.changes {
                    for statement in dialect.emit(&p.change, p.strategy)? {
                        conn.execute(&statement.sql).await?;
                    }
                }
                if case == "rows" {
                    let seeded = conn.query("SELECT g,z FROM app.a WHERE id=1").await?;
                    assert_eq!(seeded[0].try_get::<i32>("g")?, Some(2));
                    assert_eq!(seeded[0].try_get::<i32>("z")?, Some(4));
                }
                let input = if shape_case.starts_with("prefix-") { "b" } else { "z" };
                conn.execute(&format!("INSERT INTO app.a (id,{input}) VALUES (3,5)")).await?;
                let rows = conn.query(&format!("SELECT id,g,{input} FROM app.a ORDER BY id")).await?;
                let row = rows.last().unwrap();
                assert_eq!(row.try_get::<i32>("g")?.unwrap(), if case == "later" { 10 } else { 6 }, "{case}");
                if shape_case.starts_with("prefix-") {
                    let rows = conn.query("SELECT a FROM app.a WHERE id=3").await?;
                    assert_eq!(rows[0].try_get::<i32>("a")?, Some(if shape_case == "prefix-only" { 6 } else { 10 }), "{case}");
                    let has_parts = cs.changes.iter().any(pbps_diff::order::generated::is_part);
                    let placeholder = cs.changes.iter().any(|p| matches!(p.change, Change::AlterColumnExpression { .. }));
                    let version = conn.query("SHOW server_version_num").await?;
                    let modern = version[0].try_get::<&str>("server_version_num")?.unwrap().parse::<i64>()? >= pbps_pg::generated::SET_EXPRESSION_ARRIVED_IN;
                    let suffix_input = shape_case == "prefix-later";
                    assert_eq!(has_parts, !suffix_input || (modern && !case.ends_with("-offline")), "{case}");
                    assert_eq!(placeholder, suffix_input && modern && !case.ends_with("-offline"), "{case}");
                }
                let columns = conn.query("SELECT attname::text AS name FROM pg_catalog.pg_attribute WHERE attrelid='app.a'::regclass AND attnum>0 AND NOT attisdropped ORDER BY attnum").await?;
                assert_eq!(columns.iter().map(|r| r.try_get::<&str>("name").unwrap().unwrap()).collect::<Vec<_>>(),
                    schema.tables[&TableName::new("app", "a")].columns.keys().map(pbps_model::ColumnName::raw).collect::<Vec<_>>(), "{case}");
                if case == "collision" {
                    let rows = conn.query("SELECT t.typname::text AS name FROM pg_catalog.pg_type r JOIN pg_catalog.pg_type t ON t.oid=r.typarray WHERE r.oid='app.a'::regtype").await?;
                    assert_eq!(rows[0].try_get::<&str>("name")?.unwrap(), "_a_1");
                }
                conn.execute("COMMIT").await?;
                Ok(())
            }.await;
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            result.unwrap_or_else(|e| panic!("{case}: {e:#}"));
        }
    }
}

pub(crate) fn identities(schema: &Schema) -> IdsFile {
    pbps_diff::resolve(
        schema,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "issue-1503".into(),
            today: "2026-10-19".into(),
        },
    )
    .unwrap()
    .ids
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn called_table_readers_use_complete_shapes_and_old_servers_keep_the_hold() {
    use pbps_db::{Conn, Driver};
    for server in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(server).unwrap();
        for case in ["called-table", "called-insert"] {
            let database = format!("pbps_1503_{:016x}", rand::random::<u64>());
            let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
            admin
                .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
                .await
                .unwrap();
            let result: anyhow::Result<()> = async {
                let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}")).await?;
                conn.execute("CREATE SCHEMA app").await?;
                let pg = crate::engine::planning_dialect(&mut conn).await?;
                let version = conn.query("SHOW server_version_num").await?;
                let modern = version[0].try_get::<&str>("server_version_num")?.unwrap().parse::<i64>()? >= 170_000;
                let schema = shape(case);
                let ids = identities(&schema);
                assert!(ordinary(&schema, &ids, &pbps_pg::Postgres::new()).is_err(), "offline keeps the named hold");
                let result = ordinary(&schema, &ids, pg.as_ref());
                if !modern {
                    let why = result.unwrap_err();
                    assert!(why.contains("app.a") && why.contains("app.f"), "{why}");
                    return Ok(());
                }
                let cs = result.map_err(anyhow::Error::msg)?;
                assert!(cs.changes.iter().any(|p| matches!(p.change, Change::AlterColumnExpression { .. })));
                assert!(!cs.changes.iter().any(|p| matches!(p.change, Change::AddColumn { .. })));
                for p in &cs.changes { for sql in pg.emit(&p.change, p.strategy)? { conn.execute(&sql.sql).await?; } }
                let count = conn.query("SELECT count(*)::int4 AS n FROM information_schema.columns WHERE table_schema='app' AND table_name='v'").await?;
                assert_eq!(count[0].try_get::<i32>("n")?, Some(3));
                Ok(())
            }.await;
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            result.unwrap_or_else(|e| panic!("{case}: {e:#}"));
        }
    }
}
