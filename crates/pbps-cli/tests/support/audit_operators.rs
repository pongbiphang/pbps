use super::{BIN, Demo, code, stderr, stdout};
use std::process::{Command, Output};

const BLANK: &[&str] = &["", "   ", "\t\r\n", "\u{3000}", "\u{a0}"];
const APPROVER: &str = " \t核准者 ci-approver\n ";

fn command(d: &Demo, args: &[&str]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("--project")
        .arg(&d.dir)
        .current_dir(&d.dir)
        .args(args)
        .env_remove("PBPS_OPERATOR")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

fn success(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}

fn refused(out: Output) {
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("PBPS_OPERATOR"), "{}", stderr(&out));
    assert!(stderr(&out).contains("whitespace-only"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

fn schema(dialect: &str) -> &'static str {
    if dialect == "postgres" { "app" } else { "dbo" }
}

fn table(schema: &str, old: bool) -> String {
    format!(
        "table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n{}primary_key: [id]\n",
        if old { "  old: {type: bigint}\n" } else { "" }
    )
}

fn intent_fixture(dialect: &str, slug: &str) -> Demo {
    let d = Demo::new(slug);
    d.table(&table(schema(dialect), true));
    success(command(&d, &["plan", "--no-dev"]).output().unwrap());
    d.table(&table(schema(dialect), false));
    d
}

fn recorded_operator(d: &Demo) -> String {
    let ids: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
    assert_eq!(ids.tombstones.len(), 1);
    ids.tombstones.values().next().unwrap().operator.clone()
}

pub fn explicit(dialect: &str) {
    let d = intent_fixture(dialect, "operator2330-explicit");
    let column = format!("{}.t.old", schema(dialect));
    success(
        command(&d, &["drop", &column, "--reason", "approved cleanup"])
            .env("PBPS_OPERATOR", APPROVER)
            .env("USER", "runner-account")
            .output()
            .unwrap(),
    );
    assert_eq!(recorded_operator(&d), APPROVER);
}

pub fn unset(dialect: &str) {
    for (slug, git, user, expected) in [
        (
            "git",
            Some(" git-person "),
            Some("runner-account"),
            "git-person",
        ),
        ("blank-git", Some(" \t "), Some("local-user"), "local-user"),
        ("user", None, Some(" local-user "), " local-user "),
        ("empty-user", None, Some(""), ""),
        ("blank-user", None, Some(" \t "), " \t "),
        ("unknown", None, None, "unknown"),
    ] {
        let d = intent_fixture(dialect, &format!("operator2330-unset-{slug}"));
        match git {
            Some(name) => d.git(&["config", "user.name", name]),
            None => d.git(&["config", "--unset", "user.name"]),
        }
        let column = format!("{}.t.old", schema(dialect));
        let mut cmd = command(&d, &["drop", &column, "--reason", "approved cleanup"]);
        match user {
            Some(name) => {
                cmd.env("USER", name);
            }
            None => {
                cmd.env_remove("USER");
            }
        }
        success(cmd.output().unwrap());
        assert_eq!(recorded_operator(&d), expected, "{slug}");
    }
}

pub fn before_io(dialect: &str) {
    let d = Demo::new("operator2330-noio");
    d.table(&table(schema(dialect), false));
    let config = std::fs::read(d.dir.join("pbps.yml")).unwrap();
    let declaration =
        std::fs::read(d.dir.join(format!("schema/{}.t.yml", schema(dialect)))).unwrap();
    let missing = d.dir.join("missing-project");
    let connection = if dialect == "postgres" {
        "host=127.0.0.1 port=1 user=unused dbname=unused sslmode=disable"
    } else {
        "Server=127.0.0.1,1;User Id=unused;Password=unused;TrustServerCertificate=true"
    };
    let checksum = "0".repeat(64);
    let column = format!("{}.t.old", schema(dialect));
    let commands: &[&[&str]] = &[
        &[
            "apply",
            "--db",
            connection,
            "--plan",
            "absent.json",
            "--checksum",
            &checksum,
        ],
        &[
            "apply",
            "--db",
            connection,
            "--plan",
            "absent.json",
            "--checksum",
            &checksum,
            "--staged",
        ],
        &[
            "apply",
            "--db",
            connection,
            "--plan",
            "absent.json",
            "--checksum",
            &checksum,
            "--staged",
            "--resume",
        ],
        &[
            "baseline",
            "--db",
            connection,
            "--reason",
            "approved adoption",
        ],
        &["snapshot", "--db", connection, "--force"],
        &["bootstrap", "--db", connection, "--sql", "absent.sql"],
        &["plan", "--db", connection],
        &["pull", "--db", connection, "--force"],
        &[
            "init",
            "--from",
            "prod",
            "--url-env",
            "PBPS_OPERATOR_TEST_TARGET",
        ],
        &["drop", &column, "--reason", "approved cleanup"],
        &["intent", "--drop", &column, "approved cleanup"],
    ];
    let usage = command(&d, &["baseline", "--db", connection, "--reason", " "])
        .output()
        .unwrap();
    assert_eq!(code(&usage), 2);
    for value in BLANK {
        for args in commands {
            refused(
                command(&d, args)
                    .env("PBPS_OPERATOR", value)
                    .env("PBPS_OPERATOR_TEST_TARGET", connection)
                    .output()
                    .unwrap(),
            );
            refused(
                Command::new(BIN)
                    .current_dir(&d.dir)
                    .arg("--project")
                    .arg(&missing)
                    .args(*args)
                    .env("PBPS_OPERATOR", value)
                    .env("PBPS_OPERATOR_TEST_TARGET", connection)
                    .output()
                    .unwrap(),
            );
        }
    }
    assert_eq!(std::fs::read(d.dir.join("pbps.yml")).unwrap(), config);
    assert_eq!(
        std::fs::read(d.dir.join(format!("schema/{}.t.yml", schema(dialect)))).unwrap(),
        declaration
    );
    for path in [
        "schema.ids.json",
        "absent.json",
        "absent.sql",
        "missing-project",
    ] {
        assert!(!d.dir.join(path).exists(), "{path}");
    }
    // Nonblank input must reach ordinary command processing, rather than
    // passing because every configured operator is refused.
    let control = command(&d, &["baseline", "--db", connection, "--reason", "adopt"])
        .env("PBPS_OPERATOR", "ci-approver")
        .output()
        .unwrap();
    assert_eq!(
        code(&control),
        1,
        "{}{}",
        stdout(&control),
        stderr(&control)
    );
    assert!(!stderr(&control).contains("PBPS_OPERATOR"));
}

pub fn connected(dialect: &str, connection: &str) {
    let d = Demo::new("operator2330-ledger");
    let schema = schema(dialect);
    d.table(&table(schema, false));
    success(command(&d, &["plan", "--no-dev"]).output().unwrap());
    d.commit();
    let history = || {
        let out = success(
            command(
                &d,
                &["state", "list", "--db", connection, "--format", "json"],
            )
            .output()
            .unwrap(),
        );
        serde_json::from_str::<serde_json::Value>(&stdout(&out)).unwrap()["data"]["entries"].clone()
    };
    success(
        command(&d, &["bootstrap", "--db", connection])
            .env("PBPS_OPERATOR", "bootstrap-approver")
            .output()
            .unwrap(),
    );
    assert_eq!(history()[0]["operator"], "bootstrap-approver");
    d.table(&table(schema, true));
    success(command(&d, &["plan", "--no-dev"]).output().unwrap());
    d.commit();
    let plan = d.dir.join("plan.json");
    let plan_arg = plan.to_str().unwrap();
    success(
        command(&d, &["plan", "--db", connection, "--out", plan_arg])
            .output()
            .unwrap(),
    );
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    let checksum = saved.wire_fingerprint();
    let before = history();
    let writers: &[&[&str]] = &[
        &[
            "apply",
            "--db",
            connection,
            "--plan",
            plan_arg,
            "--checksum",
            &checksum,
        ],
        &[
            "baseline",
            "--db",
            connection,
            "--reason",
            "approved adoption",
        ],
        &["snapshot", "--db", connection, "--force"],
        &["bootstrap", "--db", connection],
    ];
    for value in BLANK {
        for args in writers {
            refused(
                command(&d, args)
                    .env("PBPS_OPERATOR", value)
                    .output()
                    .unwrap(),
            );
            assert_eq!(history(), before);
        }
    }
    for (args, approver) in [
        (writers[0], "apply-approver"),
        (writers[1], "baseline-approver"),
        (writers[2], "snapshot-approver"),
    ] {
        success(
            command(&d, args)
                .env("PBPS_OPERATOR", approver)
                .output()
                .unwrap(),
        );
        assert_eq!(history()[0]["operator"], approver);
    }
    success(command(&d, writers[2]).output().unwrap());
    assert_eq!(history()[0]["operator"], "demo");
}
