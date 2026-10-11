use super::{Demo, code, stderr, stdout};
use pbps_model::IdsFile;

const BLANK: &[&str] = &["", "   ", "\t", "\n\t ", "\u{3000}"];
const MEANINGFUL: &str = " \t核准 REG-2026-042: \"PII\"\n ";

fn success(d: &Demo, args: &[&str]) {
    let out = d.run(args);
    assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
}

fn refused(d: &Demo, args: &[&str]) {
    assert_refused(d.run(args), args);
}

fn assert_refused(out: std::process::Output, args: &[&str]) {
    assert_eq!(code(&out), 2, "{args:?}: {}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("reason"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("empty") || stderr(&out).contains("whitespace"),
        "{}",
        stderr(&out)
    );
}

fn declaration(schema: &str, old: bool) -> String {
    format!(
        "table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n{}primary_key: [id]\n",
        if old {
            "  old: {type: varchar(40)}\n"
        } else {
            ""
        }
    )
}

pub fn offline(dialect: &str) {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    for command in ["drop", "drop-table", "drop-role"] {
        let d = Demo::new(&format!("reasons1796-{command}"));
        d.table(&declaration(schema, true));
        let role = d.dir.join("schema/reader.yml");
        if command == "drop-role" {
            std::fs::write(&role, "role: reader\n").unwrap();
        }
        success(&d, &["plan", "--no-dev"]);
        d.commit();
        let target = match command {
            "drop" => {
                d.table(&declaration(schema, false));
                format!("{schema}.t.old")
            }
            "drop-table" => {
                std::fs::remove_file(d.dir.join(format!("schema/{schema}.t.yml"))).unwrap();
                format!("{schema}.t")
            }
            "drop-role" => {
                std::fs::remove_file(role).unwrap();
                "reader".into()
            }
            _ => unreachable!(),
        };
        let ids_path = d.dir.join("schema.ids.json");
        let before = std::fs::read(&ids_path).unwrap();
        for reason in BLANK {
            refused(&d, &[command, &target, "--reason", reason]);
            assert_eq!(std::fs::read(&ids_path).unwrap(), before);
            let blocked = d.run(&["plan", "--check"]);
            assert_eq!(
                code(&blocked),
                2,
                "{}{}",
                stdout(&blocked),
                stderr(&blocked)
            );
            assert!(stderr(&blocked).contains(command), "{}", stderr(&blocked));
            assert_eq!(std::fs::read(&ids_path).unwrap(), before);
        }
        success(&d, &[command, &target, "--reason", MEANINGFUL]);
        let ids: IdsFile = serde_json::from_slice(&std::fs::read(&ids_path).unwrap()).unwrap();
        assert_eq!(
            ids.tombstones.len(),
            if command == "drop-table" { 3 } else { 1 }
        );
        assert!(ids.tombstones.values().all(|t| t.reason == MEANINGFUL));
        assert!(ids.tombstones.values().any(|t| t.was == target));
        if command == "drop-table" {
            for suffix in ["id", "old"] {
                assert!(
                    ids.tombstones
                        .values()
                        .any(|t| t.was == format!("{schema}.t.{suffix}"))
                );
            }
        }
        success(&d, &["plan", "--check"]);
    }
}

pub fn before_io(dialect: &str) {
    let d = Demo::new("reasons1796-noio");
    // A missing project proves argument validation happens before discovery.
    let missing = d.dir.join("missing-project");
    let connection = if dialect == "postgres" {
        "host=127.0.0.1 port=1 user=unused dbname=unused sslmode=disable"
    } else {
        "Server=127.0.0.1,1;User Id=unused;Password=unused;TrustServerCertificate=true"
    };
    let missing_project_refuses = |args: &[&str]| {
        let out = std::process::Command::new(super::BIN)
            .arg("--project")
            .arg(&missing)
            .args(args)
            .output()
            .unwrap();
        assert_refused(out, args);
    };
    for reason in BLANK {
        for (command, target) in [
            ("drop", "app.t.old"),
            ("drop-table", "app.t"),
            ("drop-role", "reader"),
        ] {
            missing_project_refuses(&[command, target, "--reason", reason]);
        }
        // The valid project and unreachable target separately pin refusal
        // before opening a connection, not merely before recording a row.
        refused(&d, &["baseline", "--db", connection, "--reason", reason]);
        missing_project_refuses(&["baseline", "--db", connection, "--reason", reason]);
    }
    assert!(!missing.exists());
    assert!(!d.dir.join("schema.ids.json").exists());
}

pub fn connected(dialect: &str, connection: &str) {
    let d = Demo::new("reasons1796-ledger");
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    d.table(&declaration(schema, false));
    success(&d, &["plan", "--no-dev"]);
    d.commit();
    success(&d, &["bootstrap", "--db", connection]);
    let history = || {
        let out = d.run(&["state", "list", "--db", connection, "--format", "json"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        serde_json::from_str::<serde_json::Value>(&stdout(&out)).unwrap()["data"]["entries"].clone()
    };
    let before = history();
    assert_eq!(before.as_array().unwrap().len(), 1);
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    for reason in BLANK {
        refused(&d, &["baseline", "--db", connection, "--reason", reason]);
        assert_eq!(history(), before);
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    }
    success(
        &d,
        &["baseline", "--db", connection, "--reason", MEANINGFUL],
    );
    let after = history();
    assert_eq!(after.as_array().unwrap().len(), 2);
    assert_eq!(after[0]["kind"], "baseline");
    assert_eq!(after[0]["reason"], MEANINGFUL);
    assert_eq!(after[1], before[0]);
}
