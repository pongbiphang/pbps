//! Retained whole-table holds and their phase compatibility, beside the
//! frozen scratch mover (DEC-1959.3).
use super::*;
use pbps_model::{
    Column, ForeignKey, Generated, IdsFile, Module, PrimaryKey, Schema, Table, TableName, Uid,
    UidKind,
};

fn schema(reference: bool, local_prefix: &str) -> Schema {
    let mut schema = Schema::default();
    for (name, function) in [("a", "f"), ("b", "h")] {
        let mut table = Table::default();
        table.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        let mut generation = Column::new("integer".parse().unwrap());
        generation.value_source = ValueSource::Generated(Generated {
            expression: format!(
                "app.{function}(z){}",
                if reference && name == "b" {
                    " + ('app.a'::regclass::oid::integer * 0)"
                } else {
                    ""
                }
            ),
            stored: true,
        });
        // A later input keeps the whole-table path on unknown/16 and selects
        // a full-shape placeholder on 17+. Both must retain the old order.
        table.columns.insert("g".into(), generation);
        table
            .columns
            .insert("z".into(), Column::new("integer".parse().unwrap()));
        table.primary_key = Some(PrimaryKey {
            name: Some(format!("{local_prefix}{name}_pk").into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table.checks.insert(
            format!("{local_prefix}{name}_positive").into(),
            pbps_model::CheckConstraint {
                expression: "g >= 0".into(),
            },
        );
        if name == "b" {
            table.foreign_keys.insert(
                "b_a".into(),
                ForeignKey {
                    columns: vec!["id".into()],
                    references_table: TableName::new("app", "a"),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
        }
        schema.tables.insert(TableName::new("app", name), table);
    }
    for (name, kind, definition) in [
        (
            "app.f(integer)",
            ModuleKind::Function,
            "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2",
        ),
        (
            "app.h(integer)",
            ModuleKind::Function,
            "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 3",
        ),
        ("app.reader", ModuleKind::View, "SELECT * FROM app.a"),
        ("app.consumer", ModuleKind::View, "SELECT g FROM app.reader"),
        ("app.other", ModuleKind::View, "SELECT g FROM app.b"),
        ("app.free", ModuleKind::View, "SELECT 7 AS n"),
    ] {
        schema.modules.insert(
            name.parse().unwrap(),
            Module {
                kind,
                description: None,
                definition: definition.into(),
            },
        );
    }
    schema
}

#[test]
fn reconstruction_fixture_local_names_keep_raw_prefixes_distinct_from_shown_lookalikes() {
    const RAW: &str = "part\nline\\slash\u{200b}";
    const SHOWN: &str = r"part\nline\\slash\u{200b}";
    let schema = schema(false, RAW);
    for name in ["a", "b"] {
        let table = &schema.tables[&TableName::new("app", name)];
        assert_eq!(
            table.primary_key.as_ref().unwrap().name,
            Some(pbps_model::ObjectLocalName::new(format!("{RAW}{name}_pk")))
        );
        assert_ne!(
            table.primary_key.as_ref().unwrap().name,
            Some(pbps_model::ObjectLocalName::new(format!(
                "{SHOWN}{name}_pk"
            )))
        );
        assert!(
            table
                .checks
                .contains_key(format!("{RAW}{name}_positive").as_str())
        );
        assert!(
            !table
                .checks
                .contains_key(format!("{SHOWN}{name}_positive").as_str())
        );
    }
}

fn bootstrap(schema: &Schema, seed: u32) -> (Vec<Change>, IdsFile) {
    let mut ids = IdsFile::default();
    let mut changes = Vec::new();
    for (name, table) in &schema.tables {
        let uid = Uid::derived(UidKind::Table, &name.raw().to_string(), seed);
        ids.tables.insert(uid.clone(), name.clone());
        for column in table.columns.keys() {
            let column = name.column(column);
            ids.columns.insert(
                Uid::derived(UidKind::Column, &column.raw().to_string(), seed),
                column,
            );
        }
        changes.push(Change::CreateTable {
            uid,
            name: name.clone(),
            table: Box::new(table.clone()),
            beside_default: None,
            moves_at_most: None,
        });
    }
    let mut modules: Vec<_> = schema
        .modules
        .iter()
        .map(|(id, module)| Change::CreateModule {
            id: id.clone(),
            module: Box::new(module.clone()),
        })
        .collect();
    let count = modules.len();
    modules.rotate_left(seed as usize % count);
    changes.extend(modules);
    (changes, ids)
}

#[test]
fn multiple_holds_and_phase_peers_match_the_old_metadata_for_32_uid_seeds() {
    for seed in 0..32 {
        for reference in [false, true] {
            let schema = schema(reference, "");
            let (changes, ids) = bootstrap(&schema, seed);
            for version in [None, Some(160_000), Some(170_000), Some(180_000)] {
                let pg = version.map_or_else(crate::Postgres::new, |v| {
                    crate::Postgres::new().with_server_version(v)
                });
                let reconstruction = Reconstruction::new(
                    &pg,
                    &crate::resolver::reconstruct::fixture_plan(&changes),
                    &ids,
                )
                .unwrap();
                assert!(
                    reconstruction
                        .steps
                        .iter()
                        .any(|s| s.declaration == "table app.a")
                );
                let mut cyclic = schema.clone();
                cyclic.modules.get_mut(&"app.f(integer)".parse().unwrap()).unwrap().definition =
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT g FROM app.a LIMIT 1 $$".into();
                let (cyclic, ids) = bootstrap(&cyclic, seed);
                // Unknown/old engines keep the named hold and refusal. Modern
                // placeholder behavior is pinned separately by #1503's tests.
                let why = Reconstruction::new(
                    &crate::Postgres::new(),
                    &crate::resolver::reconstruct::fixture_plan(&cyclic),
                    &ids,
                )
                .unwrap_err()
                .to_string();
                assert!(
                    why.contains("app.a") && why.contains("app.f(integer)"),
                    "{why}"
                );
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn held_tables_and_readers_compile_with_their_keys_on_both_servers() {
    use pbps_db::{Conn, Driver};
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(variable).expect("both PostgreSQL endpoints are required");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let version = crate::roles::server_version_num(&mut admin).await.unwrap();
        for reference in [false, true] {
            let schema = schema(reference, "");
            let (_, ids) = bootstrap(&schema, 0);
            let pg = crate::Postgres::new().with_server_version(version);
            let changes = pbps_diff::diff(
                pbps_diff::Side {
                    schema: &Schema::default(),
                    ids: &IdsFile::default(),
                },
                pbps_diff::Side {
                    schema: &schema,
                    ids: &ids,
                },
                &pg,
                &Default::default(),
            )
            .unwrap()
            .changes
            .clone()
            .into_iter()
            .map(|p| p.change)
            .collect::<Vec<_>>();
            // Use a dedicated database on the assigned fixture; no separate
            // producer or Docker scratch can touch another worker's server.
            let database = format!(
                "pbps_s7_function_edges_{}_{}",
                std::process::id(),
                u8::from(reference)
            );
            admin
                .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
                .await
                .unwrap();
            let result: Result<(), String> = async {
                let mut conn =
                    Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
                        .await
                        .map_err(|e| e.to_string())?;
                conn.execute("CREATE SCHEMA app")
                    .await
                    .map_err(|e| e.to_string())?;
                let mut reconstruction = Reconstruction::new(
                    &pg,
                    &crate::resolver::reconstruct::fixture_plan(&changes),
                    &ids,
                )
                .map_err(|e| e.to_string())?;
                reconstruction
                    .compile(&pg, &pbps_model::SessionZone::utc(), &mut conn)
                    .await
                    .map_err(|e| e.to_string())?;
                conn.execute(
                    "INSERT INTO app.a (id,z) VALUES (1,4); INSERT INTO app.b (id,z) VALUES (1,5)",
                )
                .await
                .map_err(|e| e.to_string())?;
                let rows = conn
                    .query(
                        "SELECT (SELECT g FROM app.consumer) AS a, (SELECT g FROM app.other) AS b",
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                assert_eq!(rows[0].try_get::<i32>("a").unwrap(), Some(8));
                assert_eq!(rows[0].try_get::<i32>("b").unwrap(), Some(15));
                assert!(
                    conn.execute("INSERT INTO app.b (id,z) VALUES (2,1)")
                        .await
                        .is_err(),
                    "the foreign key must have been installed"
                );
                assert!(
                    conn.execute("INSERT INTO app.a (id,z) VALUES (2,-1)")
                        .await
                        .is_err(),
                    "the deferred check must have been installed"
                );
                Ok(())
            }
            .await;
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            result.unwrap();
        }
    }
}
