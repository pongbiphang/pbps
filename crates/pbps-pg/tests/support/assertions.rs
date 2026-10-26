//! ADR-0019 §7, qualified on both supported PostgreSQL fixture builds.
use super::*;
use pbps_db::query::{EvidenceCell, QueryEvidence};
use pbps_pg::assertions::{AssertionResult, run};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(10);

struct Fixture {
    admin: Conn,
    base: String,
    database: String,
    role: String,
    assertion: String,
}

fn settings(base: &str, overrides: &[(&str, &str)]) -> String {
    assert!(!base.contains("://"));
    let mut parts: Vec<_> = base
        .split_whitespace()
        .filter(|p| {
            !overrides
                .iter()
                .any(|(k, _)| p.starts_with(&format!("{k}=")))
        })
        .map(str::to_owned)
        .collect();
    parts.extend(overrides.iter().map(|(k, v)| format!("{k}={v}")));
    parts.join(" ")
}

fn servers() -> Vec<String> {
    vec![
        conn_str(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ]
}

impl Fixture {
    async fn new(base: String, tag: &str) -> Self {
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        let database = format!("assert1165_{tag}_{}", std::process::id());
        let role = format!("assert1165_role_{tag}_{}", std::process::id());
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
            .await
            .unwrap();
        admin.execute(&format!("DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'assertion-only-1165'")).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {database}"))
            .await
            .unwrap();
        let target = settings(&base, &[("dbname", &database)]);
        let mut admin = Conn::connect(Driver::Postgres, &target).await.unwrap();
        admin.execute(&format!("CREATE SCHEMA app; CREATE TABLE app.orders(id integer, total integer); INSERT INTO app.orders VALUES (1,1164); REVOKE CREATE ON SCHEMA public FROM PUBLIC; GRANT USAGE ON SCHEMA app TO {role}; GRANT SELECT ON app.orders TO {role}; CREATE FUNCTION app.definer_write() RETURNS integer LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS 'BEGIN INSERT INTO app.orders VALUES (2,1); RETURN 1; END'; REVOKE ALL ON FUNCTION app.definer_write() FROM PUBLIC; GRANT EXECUTE ON FUNCTION app.definer_write() TO {role};")).await.unwrap();
        let assertion = settings(
            &base,
            &[
                ("dbname", &database),
                ("user", &role),
                ("password", "assertion-only-1165"),
            ],
        );
        Self {
            admin,
            base,
            database,
            role,
            assertion,
        }
    }

    async fn run(&self, sql: &str) -> AssertionResult {
        run(&self.assertion, sql, DEADLINE).await
    }

    async fn finish(self) {
        drop(self.admin);
        let mut admin = Conn::connect(Driver::Postgres, &self.base).await.unwrap();
        admin
            .execute(&format!("DROP DATABASE {} WITH (FORCE)", self.database))
            .await
            .unwrap();
        admin
            .execute(&format!("DROP ROLE {}", self.role))
            .await
            .unwrap();
    }
}

fn violation(result: AssertionResult) -> QueryEvidence {
    let AssertionResult::Violation(evidence) = result else {
        panic!("expected violation, got {result:?}");
    };
    evidence
}

fn sql_error(result: AssertionResult, code: &str) {
    let AssertionResult::SqlError(error) = result else {
        panic!("expected SQLSTATE {code}, got {result:?}");
    };
    assert_eq!(
        error.server_error_code().as_deref(),
        Some(code),
        "{error:?}"
    );
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn only_a_completed_zero_row_query_passes() {
    for server in servers() {
        let f = Fixture::new(server, "empty").await;
        let result = f
            .run("SELECT id, total FROM app.orders WHERE total <> 1164")
            .await;
        let AssertionResult::Pass(evidence) = result else {
            panic!("{result:?}")
        };
        assert_eq!(evidence.columns.len(), 2);
        assert_eq!(evidence.columns[0].name, "id");
        assert_eq!(evidence.columns[0].type_name, "pg_catalog.int4");
        assert!(evidence.rows.is_empty());
        assert!(!evidence.truncated);
        let evidence = violation(
            f.run("SELECT id,total FROM app.orders WHERE total <> 999")
                .await,
        );
        assert_eq!(
            evidence.rows[0][1],
            EvidenceCell::Text {
                value: "1164".into(),
                truncated: false
            }
        );
        for sql in [
            "SELECT NULL",
            "SELECT ''",
            "SELECT false",
            "SELECT count(*) FROM app.orders WHERE false",
        ] {
            let evidence = violation(f.run(sql).await);
            assert_eq!(evidence.rows.len(), 1, "{sql}");
            assert!(!evidence.truncated);
        }
        assert_eq!(
            violation(f.run("SELECT NULL::integer").await).rows[0][0],
            EvidenceCell::Null
        );
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn arbitrary_column_types_remain_visible_violation_cells() {
    for server in servers() {
        let mut f = Fixture::new(server, "types").await;
        f.admin.execute("CREATE TYPE app.colour AS ENUM ('red'); CREATE DOMAIN app.amount AS integer; CREATE TYPE app.pair AS (n integer, note text)").await.unwrap();
        let evidence = violation(f.run("SELECT 7::smallint AS small, 8::bigint AS big, 9::oid AS oid, 1.5::real AS real, 2.5::double precision AS double, true AS bool, 'text'::text AS text, ARRAY[1,2] AS array, ROW(1,'x') AS record, '{}'::jsonb AS json, '2026-10-26'::date AS date, 3::numeric AS numeric, 'point(1,2)'::text AS example, point(1,2) AS point, NULL::bytea AS null_bytes, 'abc'::bytea AS bytes, 'red'::app.colour AS colour, 4::app.amount AS domain, ROW(1,'x')::app.pair AS pair").await);
        assert_eq!(evidence.rows.len(), 1);
        assert_eq!(evidence.columns.len(), 19);
        for (index, value) in ["7", "8", "9", "1.5", "2.5", "true", "text"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                evidence.rows[0][index],
                EvidenceCell::Text {
                    value: (*value).into(),
                    truncated: false
                }
            );
        }
        for index in [7, 8, 9, 10, 11, 13, 15] {
            assert_eq!(evidence.rows[0][index], EvidenceCell::Unrenderable);
        }
        assert_eq!(evidence.rows[0][14], EvidenceCell::Null);
        assert_eq!(
            evidence.rows[0][16],
            EvidenceCell::Text {
                value: "red".into(),
                truncated: false
            }
        );
        // The protocol reports a domain's base type; a composite stays opaque.
        assert_eq!(
            evidence.rows[0][17],
            EvidenceCell::Text {
                value: "4".into(),
                truncated: false
            }
        );
        assert_eq!(evidence.rows[0][18], EvidenceCell::Unrenderable);
        assert_eq!(evidence.columns[18].type_name, "app.pair");
        assert_eq!(evidence.columns[7].type_name, "pg_catalog._int4");
        assert_eq!(evidence.columns[8].type_name, "pg_catalog.record");
        assert!(matches!(
            f.run("SELECT NULL::app.colour, NULL::app.pair, ARRAY[1], point(1,2) WHERE false")
                .await,
            AssertionResult::Pass(_)
        ));

        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn commands_and_empty_files_are_invalid_instead_of_empty_passes() {
    for server in servers() {
        let f = Fixture::new(server, "shape").await;
        for sql in [
            "",
            "-- comment only",
            "SET application_name = 'assert1165'",
            "DO 'BEGIN NULL; END'",
        ] {
            let result = f.run(sql).await;
            assert!(
                matches!(result, AssertionResult::InvalidResult { .. }),
                "{sql}: {result:?}"
            );
        }
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn zero_column_queries_are_invalid_even_when_they_return_rows() {
    for server in servers() {
        let f = Fixture::new(server, "zerocol").await;
        for sql in [
            "SELECT WHERE false",
            "SELECT FROM generate_series(1,1)",
            "SELECT FROM generate_series(1,1000)",
        ] {
            let result = f.run(sql).await;
            let AssertionResult::InvalidResult { message } = result else {
                panic!("zero columns were not invalid: {sql}: {result:?}");
            };
            assert!(message.contains("at least one output column"), "{message}");
            assert!(message.contains("SELECT 1 ..."), "{message}");
        }
        assert!(matches!(
            f.run("SELECT 1 WHERE false").await,
            AssertionResult::Pass(_)
        ));
        assert!(matches!(
            f.run("SELECT 1 FROM generate_series(1,1)").await,
            AssertionResult::Violation(_)
        ));
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn multiple_statements_are_refused_by_the_extended_protocol() {
    for server in servers() {
        let f = Fixture::new(server, "multiple").await;
        for sql in [
            "SELECT 1; SELECT 2",
            "COMMIT; SELECT app.definer_write()",
            "ROLLBACK; INSERT INTO app.orders VALUES (2,2)",
        ] {
            sql_error(f.run(sql).await, "42601");
        }
        // Semicolons in SQL literals and trailing terminators are not split.
        assert!(matches!(
            f.run("SELECT ';' WHERE false;").await,
            AssertionResult::Pass(_)
        ));
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn standalone_commit_and_rollback_are_boundary_violations_before_the_next_file() {
    for server in servers() {
        let mut f = Fixture::new(server, "boundary").await;
        // Keep the next transaction READ ONLY too: the identity must catch
        // replacement, rather than relying solely on the read-only setting.
        f.admin
            .execute(&format!(
                "ALTER ROLE {} SET default_transaction_read_only = on",
                f.role
            ))
            .await
            .unwrap();
        for sql in [
            "COMMIT",
            "ROLLBACK",
            "COMMIT AND CHAIN",
            "ROLLBACK AND CHAIN",
        ] {
            let result = f.run(sql).await;
            assert!(
                matches!(result, AssertionResult::BoundaryViolation),
                "{sql}: {result:?}"
            );
            sql_error(f.run("SELECT app.definer_write()").await, "25006");
            assert!(matches!(
                f.run("SELECT 1 WHERE false").await,
                AssertionResult::Pass(_)
            ));
        }
        let rows = f
            .admin
            .query("SELECT count(*)::integer AS count FROM app.orders")
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<i32>("count").unwrap(), Some(1));
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn the_runner_first_read_prevents_a_leading_read_write_switch() {
    for server in servers() {
        let f = Fixture::new(server, "firstread").await;
        sql_error(f.run("SET TRANSACTION READ WRITE").await, "25001");
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn the_assertion_login_is_checked_before_any_user_sql() {
    for server in servers() {
        let f = Fixture::new(server, "login").await;
        let admin = settings(&f.base, &[("dbname", &f.database)]);
        let result = run(&admin, "SELECT 1 WHERE false", DEADLINE).await;
        let AssertionResult::Unanswerable(error) = result else {
            panic!("admin accepted: {result:?}")
        };
        assert!(error.to_string().contains("non-administrator"), "{error:?}");
        let masked_admin = format!("{admin} options='-c role={}'", f.role);
        let result = run(&masked_admin, "SELECT 1 WHERE false", DEADLINE).await;
        let AssertionResult::Unanswerable(error) = result else {
            panic!("an administrator hidden behind SET ROLE was accepted: {result:?}");
        };
        assert!(error.to_string().contains("non-administrator"), "{error:?}");
        let result = f.run("SELECT current_user::text, session_user::text").await;
        let evidence = violation(result);
        assert_eq!(
            evidence.rows[0][0],
            EvidenceCell::Text {
                value: f.role.clone(),
                truncated: false
            }
        );
        assert_eq!(evidence.rows[0][0], evidence.rows[0][1]);
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn read_only_blocks_a_definer_write_that_privileges_alone_allow() {
    for server in servers() {
        let mut f = Fixture::new(server, "definer").await;
        // Positive control: the same separate login can write through the
        // SECURITY DEFINER function outside the runner-owned transaction.
        let mut plain = Conn::connect(Driver::Postgres, &f.assertion).await.unwrap();
        plain.query("SELECT app.definer_write()").await.unwrap();
        drop(plain);
        f.admin
            .execute("DELETE FROM app.orders WHERE id = 2")
            .await
            .unwrap();
        sql_error(f.run("SELECT app.definer_write()").await, "25006");
        sql_error(
            f.run("INSERT INTO app.orders VALUES (3,3) RETURNING id")
                .await,
            "25006",
        );
        let mut plain = Conn::connect(Driver::Postgres, &f.assertion).await.unwrap();
        assert_eq!(
            plain
                .execute("INSERT INTO app.orders VALUES (3,3)")
                .await
                .unwrap_err()
                .server_error_code()
                .as_deref(),
            Some("42501")
        );
        assert_eq!(
            plain
                .execute("CREATE TABLE app.forbidden(n integer)")
                .await
                .unwrap_err()
                .server_error_code()
                .as_deref(),
            Some("42501")
        );
        plain
            .execute("CREATE TEMP TABLE helper(n integer); INSERT INTO helper VALUES (1)")
            .await
            .unwrap();
        drop(plain);
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn row_and_byte_limits_close_the_stream_without_collecting_the_result() {
    for server in servers() {
        let f = Fixture::new(server, "bounded").await;
        // Beyond the first 1,000 rows the query sleeps. Draining/collecting it
        // instead of closing the stream would exceed this test's deadline.
        let result = run(&f.assertion, "SELECT n, CASE WHEN n > 1000 THEN pg_sleep(30)::text ELSE '' END FROM generate_series(1,1000000) n", Duration::from_secs(3)).await;
        let evidence = violation(result);
        assert_eq!(evidence.rows.len(), 20);
        assert!(evidence.truncated);
        let evidence = violation(
            f.run("SELECT repeat('€', 1000000) AS large, 'later' AS later")
                .await,
        );
        assert!(evidence.truncated);
        assert_eq!(evidence.rows.len(), 1);
        let EvidenceCell::Text { value, truncated } = &evidence.rows[0][0] else {
            panic!("missing text")
        };
        assert!(*truncated);
        assert!(value.len() <= 65536);
        assert_eq!(value.len() % 3, 0);
        assert!(
            evidence
                .rows
                .iter()
                .flatten()
                .filter_map(|c| match c {
                    EvidenceCell::Text { value, .. } => Some(value.len()),
                    EvidenceCell::Null | EvidenceCell::Unrenderable => None,
                })
                .sum::<usize>()
                <= 65536
        );
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn sql_errors_timeout_and_transport_loss_never_become_passes() {
    for server in servers() {
        let f = Fixture::new(server, "errors").await;
        sql_error(f.run("SELECT 1 / 0").await, "22012");
        sql_error(
            f.run("SELECT CASE WHEN n < 3 THEN n ELSE 1 / (n-n) END FROM generate_series(1,10) n")
                .await,
            "22012",
        );
        sql_error(f.run("SELECT * FROM app.missing").await, "42P01");
        let result = run(
            &f.assertion,
            "SELECT pg_sleep(30)",
            Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(result, AssertionResult::Unanswerable(_)),
            "{result:?}"
        );
        let result = f.run("SELECT pg_terminate_backend(pg_backend_pid())").await;
        assert!(
            matches!(result, AssertionResult::Unanswerable(_)),
            "{result:?}"
        );
        // A fresh file cannot inherit a timed-out/terminated/aborted session.
        assert!(matches!(
            f.run("SELECT 1 WHERE false").await,
            AssertionResult::Pass(_)
        ));
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn a_deadline_stops_the_assertion_backend_instead_of_leaving_it_running() {
    for server in servers() {
        let mut f = Fixture::new(server, "backendtimeout").await;
        let assertion = f.assertion.clone();
        let task = tokio::spawn(async move {
            run(&assertion, "SELECT pg_sleep(30)", Duration::from_secs(1)).await
        });
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let rows = f
                    .admin
                    .query(&format!(
                        "SELECT pid FROM pg_catalog.pg_stat_activity WHERE datname = '{}' \
                     AND usename = '{}' AND state = 'active' AND query = 'SELECT pg_sleep(30)'",
                        f.database, f.role
                    ))
                    .await
                    .unwrap();
                if let Some(row) = rows.first() {
                    break row.try_get::<i32>("pid").unwrap().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the sleeping assertion backend was never observed");
        let result = task.await.unwrap();
        assert!(
            matches!(result, AssertionResult::Unanswerable(_)),
            "{result:?}"
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let rows = f.admin.query(&format!(
                    "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_stat_activity WHERE pid = {pid}) AS present"
                )).await.unwrap();
                if rows[0].try_get::<bool>("present").unwrap() == Some(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the timed-out assertion backend kept running");
        f.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
async fn types_without_binary_output_are_invalid_before_execution() {
    for server in servers() {
        let mut f = Fixture::new(server, "binarysender").await;
        f.admin
            .execute(
                "CREATE DOMAIN app.no_binary AS pg_catalog.aclitem; \
            CREATE DOMAIN app.no_binary_array AS app.no_binary[]",
            )
            .await
            .unwrap();
        for (shape, sql, column, type_name) in [
            (
                "value",
                "SELECT 'postgres=arwdDxt/postgres'::aclitem AS blocked",
                "blocked",
                "aclitem",
            ),
            (
                "null",
                "SELECT NULL::aclitem AS blocked",
                "blocked",
                "aclitem",
            ),
            (
                "empty",
                "SELECT NULL::aclitem AS blocked WHERE false",
                "blocked",
                "aclitem",
            ),
            (
                "domain",
                "SELECT NULL::app.no_binary AS domain_value",
                "domain_value",
                "aclitem",
            ),
            (
                "array",
                "SELECT ARRAY[NULL::aclitem] AS array_value",
                "array_value",
                "_aclitem",
            ),
            (
                "array of domains",
                "SELECT NULL::app.no_binary[] AS nested",
                "nested",
                "_no_binary",
            ),
            (
                "domain over array of domains",
                "SELECT NULL::app.no_binary_array AS nested",
                "nested",
                "_no_binary",
            ),
            // Division by zero would be a SQL error if binding/execution were reached.
            (
                "never executed",
                "SELECT 1/0 AS other, NULL::aclitem AS blocked",
                "blocked",
                "aclitem",
            ),
        ] {
            let result = f.run(sql).await;
            println!("{shape}: {result:?}");
            let AssertionResult::InvalidResult { message } = result else {
                panic!("{shape}: {result:?}");
            };
            assert!(
                message.contains(column) && message.contains(type_name),
                "{shape}: {message}"
            );
            assert!(
                message.contains("cast it to text, e.g. `col::text`"),
                "{message}"
            );
        }
        assert!(matches!(
            f.run("SELECT NULL::aclitem::text WHERE false").await,
            AssertionResult::Pass(_)
        ));
        assert!(matches!(
            f.run("SELECT NULL::app.no_binary[]::text WHERE false")
                .await,
            AssertionResult::Pass(_)
        ));
        assert_eq!(
            violation(
                f.run("SELECT 'postgres=arwdDxt/postgres'::aclitem::text")
                    .await
            )
            .rows[0][0],
            EvidenceCell::Text {
                value: "postgres=arwdDxt/postgres".into(),
                truncated: false
            }
        );
        // Catalog permissions are local to this disposable database. A failed
        // runner lookup is not the user's SQL error and cannot establish pass.
        f.admin
            .execute("REVOKE SELECT ON pg_catalog.pg_type FROM PUBLIC")
            .await
            .unwrap();
        let result = f.run("SELECT 1 WHERE false").await;
        let AssertionResult::Unanswerable(error) = result else {
            panic!("{result:?}");
        };
        assert_eq!(error.server_error_code().as_deref(), Some("42501"));
        f.finish().await;
    }
}
