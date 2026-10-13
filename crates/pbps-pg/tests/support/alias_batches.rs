//! Bound the planner's tree without changing the engine's key comparisons.
use super::{Conn, Driver, conn_str, text, ty};
use pbps_model::{Column, PrimaryKey, RowKey, RowScope, Schema, Table, TableName};
use std::collections::{BTreeMap, BTreeSet};

struct Database {
    admin: Conn,
    conn: Conn,
    name: String,
}

impl Database {
    async fn new(server: &str, tag: &str) -> Self {
        let name = format!("pbps_alias1805_{tag}_{}", std::process::id());
        let mut admin = Conn::connect(Driver::Postgres, server).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let base = server
            .split_whitespace()
            .filter(|p| !p.starts_with("dbname="))
            .collect::<Vec<_>>()
            .join(" ");
        let conn = Conn::connect(Driver::Postgres, &format!("{base} dbname={name}"))
            .await
            .unwrap();
        Self { admin, conn, name }
    }

    async fn drop(mut self) {
        drop(self.conn);
        self.admin
            .execute(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .await
            .unwrap();
    }
}

fn servers() -> [String; 2] {
    [
        conn_str(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("the PostgreSQL 16 fixture"),
    ]
}

fn schema(kind: &str) -> (TableName, Schema) {
    let name = TableName::new("public", "product");
    let mut table = Table::default();
    table
        .columns
        .insert("code".into(), Column::new(ty(kind)).not_null());
    table.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".into()],
        storage_parameters: Default::default(),
    });
    let mut schema = Schema::default();
    schema.tables.insert(name.clone(), table);
    (name, schema)
}

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18; see scripts/live-tests-pg.sh"]
async fn ten_thousand_aliases_keep_every_requested_row_at_the_default_stack_limit() {
    for server in servers() {
        let mut db = Database::new(&server, "large").await;
        assert_eq!(text(&mut db.conn, "SHOW max_stack_depth").await, "2MB");
        db.conn.execute("CREATE TABLE public.product(code varchar(32) PRIMARY KEY); INSERT INTO public.product SELECT 'SKU_' || n FROM generate_series(1,10000) n;").await.unwrap();
        let (name, schema) = schema("varchar(32)");
        let mut known: BTreeSet<RowKey> = (1..=10000)
            .map(|n| RowKey::from(format!("SKU_{n}")))
            .collect();
        known.insert(RowKey::from("missing"));
        let mut outcomes = Vec::new();
        for scope in [
            RowScope::Every {
                known: known.clone(),
            },
            RowScope::Keys(known.clone()),
        ] {
            outcomes.push(
                pbps_pg::catalog::read_rows(
                    &mut db.conn,
                    &schema,
                    &BTreeMap::from([(name.clone(), scope)]),
                )
                .await,
            );
        }
        // Keep cleanup ahead of the regression assertion, including old-code
        // stack errors: an aborted read must not leave an owned database.
        db.drop().await;
        for outcome in outcomes {
            let seen = outcome.unwrap_or_else(|e| panic!("large alias read: {e:?}"));
            let table = &seen[&name];
            assert_eq!(table.rows.len(), 10000);
            assert_eq!(table.aliases.len(), 10000);
            for n in 1..=10000 {
                let key = RowKey::from(format!("SKU_{n}"));
                assert_eq!(table.aliases.get(&key), Some(&key));
                assert!(table.rows.contains_key(&key));
            }
            assert!(!table.aliases.contains_key(&RowKey::from("missing")));
        }
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL 16 and 18; see scripts/live-tests-pg.sh"]
async fn aliases_in_different_batches_still_refuse_a_collision() {
    for server in servers() {
        let mut db = Database::new(&server, "collision").await;
        db.conn.execute("CREATE TABLE public.product(code integer PRIMARY KEY); INSERT INTO public.product VALUES (1),(2);").await.unwrap();
        let (name, schema) = schema("integer");
        // Valid numeric spellings absent from the table, lexically between
        // 01 and 1: their absence must not become invented aliases.
        let mut known: BTreeSet<RowKey> = (2000..3000)
            .map(|n| RowKey::from(format!("0{n}")))
            .collect();
        known.extend([RowKey::from("01"), RowKey::from("1")]);
        let query =
            pbps_pg::rows::query(&name, &schema.tables[&name], &RowScope::Keys(known.clone()))
                .unwrap()
                .unwrap();
        let queries: Vec<_> = query.aliases.into_iter().collect();
        let first = queries
            .iter()
            .position(|sql| sql.contains("E'01' AS requested"))
            .unwrap();
        let second = queries
            .iter()
            .position(|sql| sql.contains("E'1' AS requested"))
            .unwrap();
        let mut outcomes = Vec::new();
        for collision in [true, false] {
            let mut requested = known.clone();
            if !collision {
                requested.remove(&RowKey::from("1"));
                requested.insert(RowKey::from("2"));
            }
            for scope in [
                RowScope::Every {
                    known: requested.clone(),
                },
                RowScope::Keys(requested.clone()),
            ] {
                outcomes.push((
                    collision,
                    requested.clone(),
                    pbps_pg::catalog::read_rows(
                        &mut db.conn,
                        &schema,
                        &BTreeMap::from([(name.clone(), scope)]),
                    )
                    .await,
                ));
            }
        }
        let mut invalid = known.clone();
        invalid.insert(RowKey::from("not_an_integer"));
        let failed = pbps_pg::catalog::read_rows(
            &mut db.conn,
            &schema,
            &BTreeMap::from([(name.clone(), RowScope::Every { known: invalid })]),
        )
        .await;
        let usable = db.conn.query("SELECT 1").await;
        db.drop().await;
        let error = failed.expect_err("a failed later batch is not a partial answer");
        assert!(
            matches!(error, pbps_pg::rows::RowsError::Read { source, .. }
            if source.server_error_code().as_deref() == Some("22P02"))
        );
        assert!(
            usable.is_ok(),
            "the failed read must roll back its transaction"
        );
        assert_ne!(
            first, second,
            "the collision must cross a real batch boundary"
        );
        for (collision, requested, outcome) in outcomes {
            let seen = outcome.unwrap();
            let table = &seen[&name];
            assert_eq!(table.aliases.len(), 2);
            assert_eq!(table.aliases[&RowKey::from("01")], RowKey::from("1"));
            if collision {
                assert_eq!(
                    table.conflict(&requested),
                    Some((RowKey::from("01"), RowKey::from("1"), RowKey::from("1")))
                );
            } else {
                assert_eq!(table.aliases[&RowKey::from("2")], RowKey::from("2"));
                assert_eq!(table.conflict(&requested), None);
            }
        }
    }
}
