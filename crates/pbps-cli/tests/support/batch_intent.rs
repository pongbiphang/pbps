use super::{Demo, code, stderr, stdout};
use pbps_model::IdsFile;

fn success(d: &Demo, args: &[&str]) {
    let out = d.run(args);
    assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
}

fn ids(d: &Demo) -> Vec<u8> {
    std::fs::read(d.dir.join("schema.ids.json")).unwrap()
}

fn table(d: &Demo, schema: &str, columns: &[&str]) {
    let fields: String = columns
        .iter()
        .map(|name| format!("  '{name}': {{type: varchar(40)}}\n"))
        .collect();
    d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n{fields}primary_key: [id]\n"));
}

pub fn complete(dialect: &str) {
    ambiguous_column_and_companion_remedy(dialect);
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    for scenario in ["drops", "renames", "mixed"] {
        let d = Demo::new(&format!("batch1791-{scenario}"));
        table(&d, schema, &["legacy_email", "Legacy Phone"]);
        success(&d, &["plan", "--no-dev"]);
        d.commit();
        let before = ids(&d);
        let old: IdsFile = serde_json::from_slice(&before).unwrap();
        let email = format!("{schema}.t.legacy_email");
        let phone = format!("{schema}.t.Legacy Phone");
        let email_uid = old.column_uid(&email.parse().unwrap()).unwrap();
        let phone_uid = old.column_uid(&phone.parse().unwrap()).unwrap();
        table(
            &d,
            schema,
            match scenario {
                "drops" => &[],
                "renames" => &["email", "Phone Current"],
                _ => &["email"],
            },
        );
        let human = d.run(&["plan", "--check"]);
        assert_eq!(code(&human), 2, "{}{}", stdout(&human), stderr(&human));
        assert!(stderr(&human).contains("pbps intent"), "{}", stderr(&human));
        let json = d.run(&["plan", "--check", "--format", "json"]);
        assert_eq!(code(&json), 2, "{}{}", stdout(&json), stderr(&json));
        let value: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
        let remedy = value["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == "identity.batch-required")
            .unwrap()["remedy"]
            .as_str()
            .unwrap();
        assert!(stderr(&human).contains(remedy));
        if scenario == "drops" {
            let line = remedy
                .lines()
                .find(|line| line.trim_start().starts_with("pbps intent "))
                .unwrap();
            assert!(line.contains("\"\""));
            assert!(!line.contains("<why>"));
            let names = [&email, &phone];
            let unedited = d.run(&["intent", "--drop", names[0], "", "--drop", names[1], ""]);
            assert_eq!(code(&unedited), 2);
            assert!(stderr(&unedited).contains("empty"));
            assert_eq!(ids(&d), before);
        }
        let unreachable = if dialect == "postgres" {
            "host=127.0.0.1 port=1 user=unused dbname=unused sslmode=disable"
        } else {
            "Server=127.0.0.1,1;User Id=unused;Password=unused;TrustServerCertificate=true"
        };
        let connected = d.run(&["plan", "--db", unreachable, "--format", "json"]);
        assert_eq!(
            code(&connected),
            2,
            "{}{}",
            stdout(&connected),
            stderr(&connected)
        );
        let connected: serde_json::Value = serde_json::from_str(&stdout(&connected)).unwrap();
        assert_eq!(connected["findings"], value["findings"]);
        let first = if scenario == "drops" {
            vec!["drop", email.as_str(), "--reason", "email retired"]
        } else {
            vec!["rename", email.as_str(), "email"]
        };
        let single = d.run(&first);
        assert_eq!(code(&single), 2, "{}{}", stdout(&single), stderr(&single));
        assert!(
            stderr(&single).contains("valid but not recorded"),
            "{}",
            stderr(&single)
        );
        assert!(
            stderr(&single).contains(&format!(
                "--{} {email}",
                if scenario == "drops" {
                    "drop"
                } else {
                    "rename"
                }
            )),
            "{}",
            stderr(&single)
        );
        assert!(stderr(&single).contains(&phone), "{}", stderr(&single));
        assert_eq!(ids(&d), before);

        let first_flag = if scenario == "drops" {
            "--drop"
        } else {
            "--rename"
        };
        let first_value = if scenario == "drops" {
            " email retired "
        } else {
            "email"
        };
        let second_flag = if scenario == "renames" {
            "--rename"
        } else {
            "--drop"
        };
        let second_value = if scenario == "renames" {
            "Phone Current"
        } else {
            "phone retention expired"
        };
        let batch = [
            "intent",
            first_flag,
            &email,
            first_value,
            second_flag,
            &phone,
            second_value,
        ];
        success(&d, &batch);
        let after: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
        if scenario == "drops" {
            assert_eq!(after.tombstones[email_uid].reason, first_value);
        } else {
            assert_eq!(after.columns[email_uid].name, "email".into());
        }
        if scenario == "renames" {
            assert_eq!(after.columns[phone_uid].name, "Phone Current".into());
        } else {
            assert_eq!(after.tombstones[phone_uid].reason, second_value);
        }
        let recorded = ids(&d);
        if scenario == "drops" {
            success(&d, &batch);
        } else {
            // DEC-403.1: an old annotation may be absorbed, but a current CLI
            // rename that no longer changes identity is an unused decision.
            let repeat = d.run(&batch);
            assert_eq!(code(&repeat), 2, "{}{}", stdout(&repeat), stderr(&repeat));
            assert!(stderr(&repeat).contains("matches nothing"));
        }
        assert_eq!(ids(&d), recorded);
        success(&d, &["plan", "--check"]);
    }
}

pub fn atomic_refusals(dialect: &str) {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let d = Demo::new("batch1791-negative");
    table(&d, schema, &["a", "b", "c"]);
    success(&d, &["plan", "--no-dev"]);
    d.commit();
    let before = ids(&d);
    let a = format!("{schema}.t.a");
    let b = format!("{schema}.t.b");
    let c = format!("{schema}.t.c");
    let kept = format!("{schema}.t.id");
    table(&d, schema, &[]);
    for args in [
        vec!["intent", "--drop", &a, "approved", "--drop", &b, "\u{3000}"],
        vec!["intent", "--drop", &a, "approved", "--drop", &b, "approved"],
        vec![
            "intent",
            "--drop",
            &a,
            "approved",
            "--drop",
            &b,
            "approved",
            "--drop",
            &c,
            "approved",
            "--drop",
            &kept,
            "still declared",
        ],
    ] {
        let out = d.run(&args);
        assert_eq!(code(&out), 2, "{args:?}: {}{}", stdout(&out), stderr(&out));
        assert_eq!(ids(&d), before);
    }
    table(&d, schema, &["new", "c"]);
    let conflict = d.run(&["intent", "--rename", &a, "new", "--rename", &b, "new"]);
    assert_eq!(
        code(&conflict),
        2,
        "{}{}",
        stdout(&conflict),
        stderr(&conflict)
    );
    assert!(
        stderr(&conflict).contains("nothing says which one"),
        "{}",
        stderr(&conflict)
    );
    assert!(!stderr(&conflict).contains("valid but not recorded"));
    assert_eq!(ids(&d), before);

    let missing = d.dir.join("missing");
    for args in [
        vec!["intent"],
        vec!["intent", "--drop", &a],
        vec!["intent", "--drop", &a, "approved", "--drop", &b],
        vec!["intent", "--drop", &a, "approved", "--drop", &b, " "],
        vec!["intent", "--drop", "unqualified", "approved"],
        vec!["intent", "--rename-table", "app.old", "unqualified"],
        vec!["intent", "--drop", &a, "--rename", &b, "new"],
    ] {
        let out = std::process::Command::new(super::BIN)
            .arg("--project")
            .arg(&missing)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(code(&out), 2, "{args:?}: {}{}", stdout(&out), stderr(&out));
        assert!(!stderr(&out).contains("pbps.yml"), "{}", stderr(&out));
        assert_eq!(ids(&d), before);
        assert!(!missing.exists());
    }
}

pub fn tables_and_roles(dialect: &str) {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let d = Demo::new("batch1791-objects");
    table(&d, schema, &[]);
    let role = d.dir.join("schema/role.yml");
    std::fs::write(&role, "role: reader\n").unwrap();
    success(&d, &["plan", "--no-dev"]);
    d.commit();
    let old: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
    let from = format!("{schema}.t");
    let to = format!("{schema}.Renamed");
    d.table(&format!(
        "table: {to}\ncolumns:\n  id: {{type: bigint, nullable: false}}\nprimary_key: [id]\n"
    ));
    std::fs::write(&role, "role: Auditor\n").unwrap();
    success(
        &d,
        &[
            "intent",
            "--rename-table",
            &from,
            &to,
            "--rename-role",
            "reader",
            "Auditor",
        ],
    );
    let renamed: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
    assert_eq!(
        renamed.tables[old.tables.keys().next().unwrap()]
            .raw()
            .to_string(),
        to
    );
    assert_eq!(
        renamed.roles[old.roles.keys().next().unwrap()],
        "Auditor".into()
    );
    d.commit();
    std::fs::remove_file(d.dir.join(format!("schema/{schema}.t.yml"))).unwrap();
    std::fs::remove_file(role).unwrap();
    success(
        &d,
        &[
            "intent",
            "--drop-table",
            &to,
            "table retired",
            "--drop-role",
            "Auditor",
            "access retired",
        ],
    );
    let dropped: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
    assert!(dropped.tables.is_empty() && dropped.columns.is_empty() && dropped.roles.is_empty());
    assert_eq!(
        dropped.tombstones[old.tables.keys().next().unwrap()].reason,
        "table retired"
    );
    assert_eq!(
        dropped.tombstones[old.roles.keys().next().unwrap()].reason,
        "access retired"
    );
    success(&d, &["plan", "--check"]);
}

pub fn annotations_and_single_edits(dialect: &str) {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let d = Demo::new("batch1791-annotations");
    table(&d, schema, &["a", "b", "c"]);
    success(&d, &["plan", "--no-dev"]);
    d.commit();
    let old: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
    d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  next_a: {{type: varchar(40), renamed_from: a}}\n  c: {{type: varchar(40)}}\nprimary_key: [id]\n"));
    success(
        &d,
        &["intent", "--drop", &format!("{schema}.t.b"), "retire b"],
    );
    let recorded: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
    let uid = old
        .column_uid(&format!("{schema}.t.a").parse().unwrap())
        .unwrap();
    assert_eq!(recorded.columns[uid].name, "next_a".into());
    success(&d, &["plan", "--check"]);
    table(&d, schema, &["next_a"]);
    success(
        &d,
        &["drop", &format!("{schema}.t.c"), "--reason", "retire c"],
    );
    success(&d, &["plan", "--check"]);
}

// One vanished/new column plus a companion role exercises the complete batch
// without changing DEC-1791.1's threshold for offering batch remedies.
fn ambiguous_column_and_companion_remedy(dialect: &str) {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let d = Demo::new("batch2332-ambiguous");
    table(&d, schema, &["old"]);
    let role = d.dir.join("schema/reader.yml");
    std::fs::write(&role, "role: reader\n").unwrap();
    success(&d, &["plan", "--no-dev"]);
    d.commit();
    table(&d, schema, &["new"]);
    std::fs::remove_file(role).unwrap();
    let before = ids(&d);
    let human = d.run(&["plan", "--check"]);
    let json = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&human), 2);
    assert_eq!(code(&json), 2);
    let json: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let remedy = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "identity.batch-required")
        .unwrap()["remedy"]
        .as_str()
        .unwrap();
    assert!(stderr(&human).contains(remedy));
    assert!(remedy.contains("--drop-role reader \"\""));
    let old = format!("{schema}.t.old");
    assert!(remedy.contains(&format!("--drop {old} \"\"")));
    let out = d.run(&["intent", "--drop-role", "reader", ""]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("empty"));
    assert_eq!(ids(&d), before);
    success(
        &d,
        &[
            "intent",
            "--drop-role",
            "reader",
            "role retired",
            "--drop",
            &old,
            "old retired",
        ],
    );
    let after: IdsFile = serde_json::from_slice(&ids(&d)).unwrap();
    assert!(
        after
            .tombstones
            .values()
            .any(|t| t.was == old && t.reason == "old retired")
    );
    assert!(
        after
            .column_uid(&format!("{schema}.t.new").parse().unwrap())
            .is_some()
    );
    success(&d, &["plan", "--check"]);
}
