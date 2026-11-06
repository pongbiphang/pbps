//! Catalog diagnostics stay with the command, including failed reads (#1739).
use super::{Demo, OwnDatabase, code, on_server, server, stderr, stdout, try_on_server};
use serde_json::Value;
use std::process::Output;

const SUPERUSER: &str = "role `postgres` is a superuser";

fn servers() -> [(String, String); 2] {
    [
        ("18".into(), server()),
        ("16".into(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()),
    ]
}

fn report(out: &Output, exit: i32) -> Value {
    assert_eq!(code(out), exit, "{}{}", stdout(out), stderr(out));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

fn warnings(report: &Value, id: &str, message: &str) -> usize {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| {
            f["id"] == id
                && f["severity"] == "warning"
                && f["message"].as_str().unwrap().contains(message)
        })
        .count()
}

fn project(connection: &str, slug: &str) -> Demo {
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (id integer NOT NULL PRIMARY KEY)",
    );
    let d = Demo::new(slug);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nunmanaged: ignore\n",
    )
    .unwrap();
    let out = d.run(&["pull", "--db", connection]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    d.commit();
    let out = d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "catalog warning fixture",
    ]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    d
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn catalog_warnings_follow_plan_and_verify_reports_in_both_formats() {
    for (version, server) in servers() {
        let slug = format!("catalog1739-{version}");
        let own = OwnDatabase::new(&server, &slug);
        let conn = own.connection();
        let d = project(conn, &slug);
        for command in ["plan", "verify"] {
            let out = d.run(&[command, "--db", conn, "--format", "json"]);
            let json = report(&out, 0);
            assert_eq!(json["result"], "ok", "{json}");
            assert_eq!(warnings(&json, "catalog.warning", SUPERUSER), 1, "{json}");
            assert_eq!(stderr(&out), "", "{version}: {command}");

            // Human diagnostics retain their text and are emitted once per read.
            let out = d.run(&[command, "--db", conn]);
            assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            assert_eq!(
                stderr(&out)
                    .matches(&format!("warning: {SUPERUSER}"))
                    .count(),
                1
            );
        }

        on_server(conn, "ALTER TABLE app.t ADD COLUMN outside_pbps integer");
        let out = d.run(&["plan", "--db", conn, "--format", "json"]);
        let json = report(&out, 1);
        assert_eq!(json["result"], "unanswerable", "{json}");
        assert_eq!(warnings(&json, "catalog.warning", SUPERUSER), 1, "{json}");
        assert_eq!(
            json["findings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["id"] == "plan.failed")
                .count(),
            1
        );
        assert!(!stderr(&out).contains("warning:"), "{}", stderr(&out));
        let out = d.run(&["plan", "--db", conn]);
        assert_eq!(code(&out), 1);
        assert_eq!(
            stderr(&out)
                .matches(&format!("warning: {SUPERUSER}"))
                .count(),
            1
        );
    }
}

// Declared before the database guard: its owned grants disappear with the
// database before the cluster role is removed, including during unwinding.
struct Role(String, String);
impl Drop for Role {
    fn drop(&mut self) {
        let _ = try_on_server(&self.0, &format!("DROP ROLE {}", self.1));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn catalog_permission_warnings_follow_the_managed_scope_and_its_refusals() {
    for (version, server) in servers() {
        let role = Role(
            server.clone(),
            format!("catalog1739_{version}_{}", std::process::id()),
        );
        on_server(&server, &format!("CREATE ROLE {}", role.1));
        let slug = format!("catalog1739-grants-{version}");
        let own = OwnDatabase::new(&server, &slug);
        let conn = own.connection();
        let d = project(conn, &slug);
        on_server(
            conn,
            &format!(
                "GRANT USAGE ON SCHEMA app TO {0}; GRANT SELECT ON app.t TO {0} WITH GRANT OPTION",
                role.1
            ),
        );

        // An unmanaged recipient is advisory, not a managed permission warning.
        let out = d.run(&["plan", "--db", conn, "--format", "json"]);
        let json = report(&out, 0);
        assert_eq!(
            warnings(&json, "catalog.unexpressible-permission", ""),
            0,
            "{json}"
        );
        assert_eq!(stderr(&out), "");

        // Adopt its ordinary grant first; then widen it outside pbps.
        on_server(
            conn,
            &format!("REVOKE GRANT OPTION FOR SELECT ON app.t FROM {}", role.1),
        );
        std::fs::write(
            d.dir.join("schema/recipient.role.yml"),
            format!(
                "role: {}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n",
                role.1
            ),
        )
        .unwrap();
        let out = d.run(&["plan", "--no-dev"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        d.commit();
        let out = d.run(&["baseline", "--db", conn, "--reason", "adopt recipient"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        on_server(
            conn,
            &format!("GRANT SELECT ON app.t TO {} WITH GRANT OPTION", role.1),
        );
        for (command, exit) in [("plan", 1), ("verify", 2)] {
            let out = d.run(&[command, "--db", conn, "--format", "json"]);
            let json = report(&out, exit);
            assert_eq!(
                warnings(
                    &json,
                    "catalog.unexpressible-permission",
                    "WITH GRANT OPTION"
                ),
                1,
                "{json}"
            );
            assert!(!stderr(&out).contains("warning:"), "{}", stderr(&out));
            let out = d.run(&[command, "--db", conn]);
            assert_eq!(code(&out), exit, "{}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out)
                    .lines()
                    .any(|line| line.starts_with("warning:") && line.contains("WITH GRANT OPTION")),
                "{}",
                stderr(&out)
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn catalog_warnings_survive_a_row_read_failure_and_disappear_when_not_observed() {
    for (version, server) in servers() {
        let role = Role(
            server.clone(),
            format!("catalog1739_read_{version}_{}", std::process::id()),
        );
        on_server(
            &server,
            &format!("CREATE ROLE {} LOGIN PASSWORD 'catalog1739'", role.1),
        );
        let slug = format!("catalog1739-read-{version}");
        let own = OwnDatabase::new(&server, &slug);
        let conn = own.connection();
        let d = project(conn, &slug);
        let path = d.dir.join("schema/app.t.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("{table}\ndata:\n  mode: ensure\n  rows:\n    1: {{}}\n"),
        )
        .unwrap();
        d.commit();
        on_server(conn, "INSERT INTO app.t VALUES (1)");
        let out = d.run(&["baseline", "--db", conn, "--reason", "watch declared rows"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        on_server(
            conn,
            &format!(
                "GRANT USAGE ON SCHEMA app TO {0}; GRANT SELECT ON public.__pbps_state TO {0}",
                role.1
            ),
        );
        let reader = format!(
            "{} user={} password=catalog1739",
            conn.split_whitespace()
                .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                .collect::<Vec<_>>()
                .join(" "),
            role.1
        );
        let out = d.run(&["verify", "--db", &reader, "--format", "json"]);
        let json = report(&out, 1);
        assert_eq!(
            json["findings"][0]["id"], "environment.unreadable",
            "{json}"
        );
        assert!(
            json["findings"][0]["message"]
                .as_str()
                .unwrap()
                .contains("cannot read the declared rows back"),
            "{json}"
        );
        assert_eq!(
            warnings(&json, "catalog.warning", "PUBLIC holds USAGE"),
            1,
            "{json}"
        );
        assert!(!stderr(&out).contains("warning:"), "{}", stderr(&out));
        // A failure before the catalog read has no observed catalog warnings.
        on_server(
            conn,
            &format!("REVOKE SELECT ON public.__pbps_state FROM {}", role.1),
        );
        let out = d.run(&["verify", "--db", &reader, "--format", "json"]);
        let json = report(&out, 1);
        assert_eq!(warnings(&json, "catalog.warning", ""), 0, "{json}");
    }
}
