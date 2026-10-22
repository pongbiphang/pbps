//! Connected refusal regressions for DEC-2084.1 (#2069, #2078).
use crate::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use std::process::Output;

pub const CASES: &[&str] = &["entity\n\u{1b}[31m\u{9b}\\\u{202e}名前", "名前_ünï❤️"];

pub fn ok(o: Output) {
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

pub fn refused(o: Output, name: &str, phrase: &str) {
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let text = stderr(&o);
    let shown = pbps_model::escape::Escaped(name).to_string();
    assert!(text.contains(phrase), "{text:?}");
    assert!(text.contains(&shown), "missing {shown:?} in {text:?}");
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
    if name != shown {
        // A pasteable ByHand statement deliberately contains the SQL identity raw.
        let prose = text.split("\n\n    ").next().unwrap();
        assert!(!prose.contains(name), "{text:?}");
        assert!(
            !text.contains(&pbps_model::escape::Escaped(&shown).to_string()),
            "{text:?}"
        );
    }
}

// JSON leaves C1 literal, but YAML requires its escaped spelling even in quotes.
pub fn yaml_scalar(text: &str) -> String {
    serde_json::to_string(text)
        .unwrap()
        .replace('\u{9b}', r"\u009b")
}

pub fn quote(name: &str, pg: bool) -> String {
    if pg {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        format!("[{}]", name.replace(']', "]]"))
    }
}

pub fn drift_and_ledger(server: &str, pg: bool) {
    for (i, name) in CASES.iter().enumerate() {
        let own = OwnDatabase::new(server, &format!("step5_drift_{i}"));
        let connection = own.connection();
        let schema = if pg { "app" } else { "dbo" };
        if pg {
            on_server(connection, "CREATE SCHEMA app");
        }
        let table = pbps_model::TableName::new(schema, *name);
        let qualified = format!("{schema}.{}", quote(name, pg));
        let column = quote(name, pg);
        on_server(
            connection,
            &format!("CREATE TABLE {qualified} ({column} int NULL)"),
        );
        let d = Demo::new(&format!("step5-drift-{i}"));
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: {}\nenvironments:\n  {}:\n    url_env: PBPS_STEP5_CONNECTION\n",
                if pg { "postgres" } else { "mssql" },
                yaml_scalar(name)
            ),
        )
        .unwrap();

        ok(d.run(&["pull", "--db", connection]));
        d.commit();
        ok(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
        let mut spec = loaded.schema.tables[&table].clone();
        spec.columns.insert(
            "extra".into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        let files = pbps_load::schema_files(&d.dir.join("schema")).unwrap();
        assert_eq!(files.len(), 1);
        std::fs::write(&files[0], pbps_load::fmt::render(&table, &spec, &[], None)).unwrap();
        ok(d.run(&["plan"]));
        d.commit();
        let path = d.dir.join("plan.json");
        ok(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
        let plan: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let checksum = plan.wire_fingerprint();
        let alter = if pg { "TYPE bigint" } else { "bigint NULL" };
        on_server(
            connection,
            &format!("ALTER TABLE {qualified} ALTER COLUMN {column} {alter}"),
        );
        let verify = d.run(&["verify", "--db", connection]);
        assert_eq!(code(&verify), 2, "{}{}", stdout(&verify), stderr(&verify));
        let text = stdout(&verify);
        let shown = pbps_model::escape::Escaped(name).to_string();
        assert!(text.contains(&shown), "{text:?}");
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        refused(
            d.run_with_env(
                &[
                    "apply",
                    "--env",
                    name,
                    "--plan",
                    path.to_str().unwrap(),
                    "--checksum",
                    &checksum,
                ],
                &[("PBPS_STEP5_CONNECTION", connection)],
            ),
            name,
            "no longer the database",
        );
        // Empty is a real connected ledger state, not a failed read or an absent ledger.
        let ledger_schema = if pg { "public" } else { "dbo" };
        on_server(
            connection,
            &format!("DELETE FROM {ledger_schema}.__pbps_state"),
        );
        refused(
            d.run_with_env(
                &[
                    "apply",
                    "--env",
                    name,
                    "--plan",
                    path.to_str().unwrap(),
                    "--checksum",
                    &checksum,
                ],
                &[("PBPS_STEP5_CONNECTION", connection)],
            ),
            name,
            "has a ledger but no entries",
        );
    }
}
