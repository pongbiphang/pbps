//! #2441 SQL repros exercise the shared #2444 census through both CLI paths.
use super::{
    ClusterRoles, Demo, OwnDatabase, approved_apply, as_role, code, json_output, on_server, scalar,
    stderr, stdout, succeeds, try_on_server,
};
use serde_json::Value;
use std::process::Command;

const ID: &str = "plan.unheld-object";
const ORDINARY: &str = "table: app.ordinary\ncolumns:\n  id: {type: integer}\n";

fn servers(mut check: impl FnMut(&str, &str)) {
    for (variable, container) in [
        ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_CONTAINER"),
        ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_CONTAINER"),
    ] {
        check(
            &std::env::var(variable).unwrap(),
            &std::env::var(container).unwrap(),
        );
    }
}

fn demo(connection: &str, slug: &str, statistics: bool) -> Demo {
    let d = Demo::new(slug);
    d.table(&format!(
        "{ORDINARY}{}",
        if statistics {
            "  other: {type: integer}\n"
        } else {
            ""
        }
    ));
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "before the census candidate",
    ]));
    d
}

fn report(d: &Demo, connection: &str) -> Value {
    let path = d.dir.join("census.plan.json");
    let out = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        path.to_str().unwrap(),
        "--format",
        "json",
    ]));
    assert!(
        !stderr(&out).contains("is not held by this model"),
        "{}",
        stderr(&out)
    );
    json_output(out)
}

fn messages(report: &Value) -> Vec<String> {
    let mut messages: Vec<_> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == ID)
        .map(|f| {
            assert_eq!(f["severity"], "warning");
            f["message"].as_str().unwrap().to_owned()
        })
        .collect();
    messages.sort();
    messages
}

fn both_paths(d: &Demo, connection: &str) -> Vec<String> {
    let report = report(d, connection);
    assert_eq!(report["data"]["changes"], 0, "{report}");
    let expected = messages(&report);
    let pulled = Demo::new("census2444-pull");
    let out = succeeds(pulled.run(&["pull", "--db", connection]));
    let mut warnings: Vec<_> = stderr(&out)
        .lines()
        .filter_map(|line| line.strip_prefix("warning: "))
        .filter(|line| {
            line.ends_with(
                "is not held by this model; it is left out of the pull and schema comparison",
            )
        })
        .map(str::to_owned)
        .collect();
    warnings.sort();
    assert_eq!(warnings, expected, "{}", stderr(&out));
    let human = succeeds(d.run(&["plan", "--db", connection]));
    for message in &expected {
        assert_eq!(
            stderr(&human).lines().filter(|line| line.trim_start().strip_prefix("warning: ") == Some(message.as_str())).count(),
            1,
        );
    }
    let saved = std::fs::read_to_string(d.dir.join("census.plan.json")).unwrap();
    assert!(!saved.contains(ID) && !saved.contains("is not held by this model"));
    expected
}

struct Cleanup {
    connection: String,
    sql: String,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = try_on_server(&self.connection, &self.sql);
    }
}

fn check(slug: &str, sql: &str, kind: &str, identity: &str) {
    servers(|admin, container| {
        // Tablespaces are cluster objects; drop the database before the space.
        let _space = (slug == "tablespace").then(|| {
            let out = Command::new("docker")
                .args([
                    "exec",
                    container,
                    "install",
                    "-d",
                    "-o",
                    "postgres",
                    "-g",
                    "postgres",
                    "/tmp/pbps2444_tablespace",
                ])
                .output()
                .unwrap();
            succeeds(out);
            Cleanup {
                connection: admin.into(),
                sql: "DROP TABLESPACE IF EXISTS inv_space".into(),
            }
        });
        let own = OwnDatabase::new(admin, &format!("census2444-{slug}"));
        let connection = own.connection();
        let (base, candidate) = sql
            .split_once("-- Candidate added after the ordinary-table baseline.")
            .unwrap();
        on_server(connection, base);
        let d = demo(
            connection,
            &format!("census2444-{slug}"),
            slug == "statistics",
        );
        assert!(messages(&report(&d, connection)).is_empty());
        // The inherited repro uses psql's public path for CREATE EXTENSION;
        // CLI test connections deliberately start with an empty path.
        let candidate = if slug.starts_with("extensions_") || slug == "unsupported_comment" {
            format!("SET search_path = public; {candidate}")
        } else {
            candidate.to_owned()
        };
        on_server(connection, &candidate);
        let _subscription = (slug == "subscription").then(|| Cleanup {
            connection: connection.into(),
            sql: "DROP SUBSCRIPTION IF EXISTS inv_sub".into(),
        });
        let expected = both_paths(&d, connection);
        assert!(
            expected
                .iter()
                .any(|m| m.starts_with(&format!("{kind} `{identity}`"))),
            "{expected:?}"
        );
        let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        // Default warnings permit even an empty saved plan to be applied.
        succeeds(approved_apply(
            &d,
            connection,
            &d.dir.join("census.plan.json"),
            &[],
        ));
        assert_eq!(messages(&report(&d, connection)), expected);
        succeeds(d.run(&["verify", "--db", connection]));
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
        if slug == "large_object" {
            on_server(connection, "SELECT pg_catalog.lo_create(44)");
            assert!(
                both_paths(&d, connection)
                    .iter()
                    .any(|m| m.starts_with("large object `44`"))
            );
        }
        if slug == "extensions_pgcrypto" {
            let config = d.dir.join("pbps.yml");
            std::fs::write(
                &config,
                format!("dialect: postgres\npolicies:\n  rules:\n    {ID}: off\n"),
            )
            .unwrap();
            assert!(messages(&report(&d, connection)).is_empty());
            std::fs::write(&config, format!("dialect: postgres\npolicies:\n  suppress:\n    - rule: {ID}\n      reason: platform owns extensions\n")).unwrap();
            assert!(messages(&report(&d, connection)).is_empty());
            std::fs::write(
                &config,
                format!("dialect: postgres\npolicies:\n  rules:\n    {ID}: error\n"),
            )
            .unwrap();
            let path = d.dir.join("refused.json");
            let out = d.run(&[
                "plan",
                "--db",
                connection,
                "--format",
                "json",
                "--out",
                path.to_str().unwrap(),
            ]);
            assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
            let report = json_output(out);
            assert!(
                report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["id"] == ID && f["severity"] == "error")
            );
            assert!(!path.exists());
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_extensions_pgcrypto() {
    check(
        "extensions_pgcrypto",
        include_str!("../fixtures/census2444/extensions_pgcrypto.sql"),
        "extension",
        "pgcrypto",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_extensions_uuid() {
    check(
        "extensions_uuid",
        include_str!("../fixtures/census2444/extensions_uuid.sql"),
        "extension",
        "\"uuid-ossp\"",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_unsupported_comment() {
    check(
        "unsupported_comment",
        include_str!("../fixtures/census2444/unsupported_comment.sql"),
        "comment on extension",
        "pgcrypto",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_publication() {
    check(
        "publication",
        include_str!("../fixtures/census2444/publication.sql"),
        "publication",
        "inv_pub",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_subscription() {
    check(
        "subscription",
        include_str!("../fixtures/census2444/subscription.sql"),
        "subscription",
        "inv_sub",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_sequence_standalone() {
    check(
        "sequence_standalone",
        include_str!("../fixtures/census2444/sequence_standalone.sql"),
        "standalone sequence",
        "app.lonely",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_operator() {
    check(
        "operator",
        include_str!("../fixtures/census2444/operator.sql"),
        "operator",
        "app.===(integer,integer)",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_operator_class() {
    check(
        "operator_class",
        include_str!("../fixtures/census2444/operator_class.sql"),
        "operator class",
        "app.my_int_ops USING btree",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_statistics() {
    check(
        "statistics",
        include_str!("../fixtures/census2444/statistics.sql"),
        "extended statistics",
        "app.stat",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_event_trigger() {
    check(
        "event_trigger",
        include_str!("../fixtures/census2444/event_trigger.sql"),
        "event trigger",
        "inv_ddl",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_cast() {
    check(
        "cast",
        include_str!("../fixtures/census2444/cast.sql"),
        "cast",
        "(integer AS pg_catalog.xml)",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_collation() {
    check(
        "collation",
        include_str!("../fixtures/census2444/collation.sql"),
        "collation",
        "app.my_c",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_type_r() {
    check(
        "type_r",
        include_str!("../fixtures/census2444/type_r.sql"),
        "range type",
        "app.r",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_type_mr() {
    check(
        "type_mr",
        include_str!("../fixtures/census2444/type_mr.sql"),
        "multirange type",
        "app.mr",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_base_type() {
    check(
        "base_type",
        include_str!("../fixtures/census2444/base_type.sql"),
        "base type",
        "app.int42",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_transform() {
    check(
        "transform",
        include_str!("../fixtures/census2444/transform.sql"),
        "transform",
        "for integer language plpgsql",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_large_object() {
    check(
        "large_object",
        include_str!("../fixtures/census2444/large_object.sql"),
        "large object",
        "244100",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_names_tablespace() {
    check(
        "tablespace",
        include_str!("../fixtures/census2444/tablespace.sql"),
        "tablespace",
        "inv_space",
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_excludes_owned_sequences_used_types_and_extension_members() {
    servers(|admin, _| {
        let own = OwnDatabase::new(admin, "census2444-negatives");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.ordinary(id integer)",
        );
        let d = demo(connection, "census2444-negatives", false);
        assert!(
            messages(&report(&d, connection)).is_empty(),
            "system objects are not user objects"
        );
        on_server(
            connection,
            "CREATE TYPE app.r AS RANGE (subtype=integer,multirange_type_name=app.mr); CREATE TABLE app.held(id serial,span app.r,spans app.r[],multiple app.mr); CREATE EXTENSION hstore WITH SCHEMA app",
        );
        let found = both_paths(&d, connection);
        for excluded in [
            "standalone sequence `app.held_id_seq`",
            "range type `app.r`",
            "multirange type `app.mr`",
            "base type `app._r`",
            "base type `app.hstore`",
            "operator class `app.",
        ] {
            assert!(
                !found.iter().any(|m| m.starts_with(excluded)),
                "{excluded}: {found:?}"
            );
        }
        assert!(found.iter().any(|m| m.starts_with("extension `hstore`")));
        assert!(
            !found.iter().any(|m| m.starts_with("operator `app.")),
            "{found:?}"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn census_catalog_permission_failure_is_named_and_never_an_empty_list() {
    servers(|admin, _| {
        let role = format!("pbps_census2444_reader_{}", std::process::id());
        let _roles = ClusterRoles {
            server: admin.into(),
            names: vec![role.clone()],
        };
        on_server(
            admin,
            &format!("CREATE ROLE {role} LOGIN NOSUPERUSER PASSWORD 'trigger-test'"),
        );
        let own = OwnDatabase::new(admin, "census2444-permissions");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.ordinary(id integer)",
        );
        let d = demo(connection, "census2444-permissions", false);
        on_server(
            connection,
            &format!(
                "GRANT USAGE ON SCHEMA app,public TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA app,public TO {role}"
            ),
        );
        let reader = as_role(connection, &role);
        assert!(
            messages(&report(&d, &reader)).is_empty(),
            "column grants permit the ordinary non-superuser census"
        );
        on_server(
            connection,
            "REVOKE SELECT(oid,subdbid,subname) ON pg_catalog.pg_subscription FROM PUBLIC",
        );
        let _restore = Cleanup {
            connection: connection.into(),
            sql: "GRANT SELECT(oid,subdbid,subname) ON pg_catalog.pg_subscription TO PUBLIC".into(),
        };
        assert_eq!(
            scalar(
                connection,
                &format!(
                    "SELECT pg_catalog.has_column_privilege('{role}','pg_catalog.pg_subscription','oid','SELECT')::int::int8"
                )
            ),
            0
        );
        let pull = Demo::new("census2444-unreadable-pull");
        let failed = pull.run(&["pull", "--db", &reader]);
        assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
        for word in ["pg_subscription", "permission denied"] {
            assert!(stderr(&failed).contains(word), "{}", stderr(&failed));
        }
        let path = d.dir.join("unreadable.json");
        let failed = d.run(&[
            "plan",
            "--db",
            &reader,
            "--format",
            "json",
            "--out",
            path.to_str().unwrap(),
        ]);
        assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
        let report = json_output(failed);
        assert_eq!(report["result"], "unanswerable");
        let message = report.to_string();
        assert!(
            message.contains("pg_subscription") && message.contains("permission denied"),
            "{message}"
        );
        assert!(!path.exists());
    });
}
