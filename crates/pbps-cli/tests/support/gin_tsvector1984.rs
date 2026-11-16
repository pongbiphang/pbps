//! A GIN index over a `tsvector` column through the whole tool (#1984, DEC-1984.1): the
//! pull holds it with no limitation and an empty re-plan, a bootstrap and a declared
//! create build the same index, and a declaration that names the default class, or a type
//! GIN has no class for, is refused by name. Each case runs on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout, succeeds,
};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.doc (id integer PRIMARY KEY, title text, tsv tsvector, d jsonb); \
    CREATE INDEX doc_tsv ON app.doc USING gin (tsv); \
    CREATE INDEX doc_d ON app.doc USING gin (d)";

/// Whether the index `name` of `app.doc` is the GIN index over `column` under the
/// column type's default class (the engine omits that class from the definition).
fn gin_over(connection: &str, name: &str, column: &str) -> bool {
    scalar(
        connection,
        &format!(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = 'app' AND indexname = '{name}' \
             AND indexdef LIKE '% USING gin ({column})'"
        ),
    ) == 1
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_gin_index_over_a_tsvector_pulls_re_plans_empty_and_bootstraps() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "gin-tsv-pull");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("gin-tsv-pull");
        let pulled = succeeds(d.run(&["pull", "--db", connection]));
        let table = std::fs::read_to_string(d.dir.join("schema/app.doc.yml")).unwrap();
        // Held, not left out: both indexes are declared, under no class at all.
        for held in ["doc_tsv:", "doc_d:", "method: gin", "tsv"] {
            assert!(
                table.contains(held),
                "`{held}` in:\n{table}\n{}",
                said(&pulled)
            );
        }
        assert!(!table.contains("tsvector_ops"), "{table}");
        // No index is named among what was left out (the pull's other notices are
        // about the roles of the engine, not the table).
        assert!(!said(&pulled).contains("doc_tsv"), "{}", said(&pulled));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        succeeds(d.run(&["validate"]));
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));

        // A bootstrap of those declarations into an empty database builds both.
        let fresh = OwnDatabase::new(url, "gin-tsv-boot");
        on_server(fresh.connection(), "CREATE SCHEMA app");
        succeeds(d.run(&["bootstrap", "--db", fresh.connection()]));
        assert!(gin_over(fresh.connection(), "doc_tsv", "tsv"));
        assert!(gin_over(fresh.connection(), "doc_d", "d"));
        let next = succeeds(d.run(&["plan", "--db", fresh.connection()]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", fresh.connection()]));
    });
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_declared_gin_index_over_a_tsvector_is_created_and_a_named_default_or_another_type_is_refused()
{
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "gin-tsv-declared");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.doc (id integer PRIMARY KEY, title text, \
             tsv tsvector)",
        );
        let d = Demo::new("gin-tsv-declared");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.doc.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        let declare = |keys: &str| {
            let text = format!(
                "{held}indexes:\n  doc_search:\n    method: gin\n    columns: [\"{keys}\"]\n"
            );
            std::fs::write(&path, text).unwrap();
        };
        // Refused by name, before any engine is asked: the default class spelled out,
        // another type's class, and types GIN has no class for.
        for (keys, expected) in [
            ("tsv tsvector_ops", "leave the class out"),
            (
                "tsv jsonb_path_ops",
                "takes only its default `tsvector_ops`",
            ),
            ("title", "only over `jsonb` or `tsvector` columns"),
        ] {
            declare(keys);
            let out = d.run(&["validate"]);
            assert_eq!(code(&out), 2, "{keys}: {}", said(&out));
            assert!(said(&out).contains(expected), "{keys}: {}", said(&out));
        }
        // The default, left out: planned, applied, verified, and a re-plan is empty.
        declare("tsv");
        succeeds(d.run(&["validate"]));
        d.commit();
        let plan = d.dir.join("plan.json");
        let sql = d.dir.join("plan.sql");
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            plan.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
        ]));
        let script = std::fs::read_to_string(&sql).unwrap();
        assert!(
            script.contains("USING gin") && !script.contains("tsvector_ops"),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert!(gin_over(connection, "doc_search", "tsv"));
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
    });
}
