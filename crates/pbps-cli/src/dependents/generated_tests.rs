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
    for case in ["partition", "later"] {
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
fn generation_functions_may_read_prefix_columns_but_suffix_and_whole_row_cycles_are_named() {
    for (read, accepts) in [
        ("SELECT id FROM app.a LIMIT 1", true),
        ("SELECT id * 2 FROM app.a LIMIT 1", true),
        ("SELECT z FROM app.a LIMIT 1", false),
        ("SELECT count(*)::integer FROM app.a", false),
        ("SELECT (row_to_json(a)->>'id')::integer FROM app.a", false),
        (
            "SELECT (row_to_json(t)->>'id')::integer FROM ONLY app.a AS t",
            false,
        ),
        ("SELECT t.id FROM ONLY app.a AS t LIMIT 1", true),
    ] {
        let mut schema = shape("plain");
        schema
            .modules
            .get_mut(&"app.f(integer)".parse().unwrap())
            .unwrap()
            .definition =
            format!("(integer) RETURNS integer LANGUAGE sql IMMUTABLE BEGIN ATOMIC {read}; END");
        let result = ordinary(&schema, &identities(&schema), &pbps_pg::Postgres::new());
        if accepts {
            assert!(result.is_ok(), "{result:?}");
        } else {
            let why = result.unwrap_err();
            assert!(why.contains("app.a") && why.contains("app.f"), "{why}");
        }
    }
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
                let dialect = crate::engine::planning_dialect(&mut conn).await?;
                let schema = shape(case);
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
                conn.execute("INSERT INTO app.a (id,z) VALUES (3,5)").await?;
                let rows = conn.query("SELECT id,g,z FROM app.a ORDER BY id").await?;
                let row = rows.last().unwrap();
                assert_eq!(row.try_get::<i32>("g")?.unwrap(), if case == "later" { 10 } else { 6 }, "{case}");
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
