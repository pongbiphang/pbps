use super::{Demo, code, stderr, stdout};
use serde_json::{Value, json};

fn success(d: &Demo, args: &[&str]) -> String {
    let out = d.run(args);
    assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
    stdout(&out)
}

fn fixture(dialect: &str, name: &str) -> (Demo, Value, &'static str) {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let d = Demo::new(&format!("baseline1797-{name}"));
    d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  old: {{type: varchar(40)}}\n"));
    std::fs::write(d.dir.join("schema/reader.yml"), "role: reader\n").unwrap();
    success(&d, &["plan", "--no-dev"]);
    let ids: Value =
        serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
    let mut snapshot = json!({
        "version": pbps_model::state::OLDEST_READABLE_VERSION,
        "kind": "baseline", "operator": "offline-test",
        "schema": {"tables": {}, "roles": {"reader": {}}}, "ids": ids,
    });
    snapshot["schema"]["tables"][format!("{schema}.t")] = json!({"columns": {
        "id": {"type": "bigint", "nullable": false},
        "old": {"type": "varchar(40)", "nullable": true},
    }});
    (d, snapshot, schema)
}

fn spare_uid(ids: &Value, section: &str, prefix: &str) -> String {
    ["aaaaaa", "bbbbbb", "cccccc"]
        .into_iter()
        .map(|suffix| format!("{prefix}_{suffix}"))
        .find(|uid| ids[section].get(uid).is_none())
        .unwrap()
}

pub fn supported_files(dialect: &str) {
    let (d, mut snapshot, schema) = fixture(dialect, "valid");
    std::fs::remove_dir_all(d.dir.join(".git")).unwrap();
    let base = d.dir.join("base.json");
    let path = base.to_str().unwrap();
    for version in pbps_model::state::OLDEST_READABLE_VERSION..=pbps_model::state::CURRENT_VERSION {
        snapshot["version"] = json!(version);
        std::fs::write(&base, snapshot.to_string()).unwrap();
        let report: Value = serde_json::from_str(&success(
            &d,
            &["plan", "--base", path, "--check", "--format", "json"],
        ))
        .unwrap();
        assert_eq!(report["data"]["changes"], 0, "version {version}: {report}");
    }
    d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  old: {{type: varchar(40)}}\n  extra: {{type: int}}\n"));
    let added = success(&d, &["plan", "--base", path, "--no-dev"]);
    assert!(added.contains("add column extra"), "{added}");
    d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  renamed: {{type: varchar(40), renamed_from: old}}\n  extra: {{type: int}}\n"));
    let renamed = success(&d, &["plan", "--base", path, "--no-dev"]);
    assert!(
        renamed.contains("rename column old -> renamed"),
        "{renamed}"
    );
    assert!(!renamed.contains("drop column"), "{renamed}");
}

pub fn corrupt_files(dialect: &str) {
    let (d, snapshot, schema) = fixture(dialect, "corrupt");
    std::fs::remove_dir_all(d.dir.join(".git")).unwrap();
    // Resolution would mint a uid here. Baseline refusal must precede that
    // write as well as both already-existing output artifacts.
    d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  old: {{type: varchar(40)}}\n  pending: {{type: int}}\n"));
    let out = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    std::fs::write(&out, "existing reviewed plan\n").unwrap();
    std::fs::write(&sql, "existing reviewed SQL\n").unwrap();
    let held = [
        d.dir.join("schema.ids.json"),
        d.dir.join(format!("schema/{schema}.t.yml")),
        d.dir.join("schema/reader.yml"),
        d.dir.join("pbps.yml"),
        out.clone(),
        sql.clone(),
    ];
    let before: Vec<_> = held.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let base = d.dir.join("base.json");
    let table = format!("{schema}.t");
    for case in [
        "version",
        "duplicate-table",
        "duplicate-column",
        "duplicate-role",
        "wrong-kind",
        "orphan-column",
        "orphan-move",
        "tombstone",
        "missing-table",
        "missing-column",
        "missing-role",
        "outer-unknown",
        "ids-unknown",
        "schema-unknown",
        "future",
        "too-old",
        "malformed",
        "absent",
    ] {
        let mut bad = snapshot.clone();
        let column_uid = bad["ids"]["columns"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let column = bad["ids"]["columns"][&column_uid]
            .as_str()
            .unwrap()
            .to_owned();
        let expected = match case {
            "version" => {
                bad["ids"]["version"] = json!(999);
                "version 999".to_owned()
            }
            "duplicate-table" => {
                let uid = spare_uid(&bad["ids"], "tables", "t");
                bad["ids"]["tables"][uid] = json!(table);
                "both point at the name".into()
            }
            "duplicate-column" => {
                let uid = spare_uid(&bad["ids"], "columns", "c");
                bad["ids"]["columns"][uid] = json!(column);
                "both point at the name".into()
            }
            "duplicate-role" => {
                let uid = spare_uid(&bad["ids"], "roles", "r");
                bad["ids"]["roles"][uid] = json!("reader");
                "both point at the name".into()
            }
            "wrong-kind" => {
                bad["ids"]["tables"] = json!({column_uid: table});
                "prefix".into()
            }
            "orphan-column" => {
                bad["ids"]["columns"][&column_uid] = json!(format!("{schema}.absent.x"));
                "no live table entry".into()
            }
            "orphan-move" => {
                let uid = spare_uid(&bad["ids"], "tables", "t");
                bad["ids"]["moves"] = json!({uid: {"max_rows": 1, "allowed_at": "2026-10-12", "reason": "test", "operator": "test"}});
                "no live table entry".into()
            }
            "tombstone" => {
                bad["ids"]["tombstones"][&column_uid] = json!({"was": column, "dropped_at": "2026-10-12", "reason": "test", "operator": "test"});
                "both live and tombstoned".into()
            }
            "missing-table" => {
                bad["schema"]["tables"] = json!({});
                table.clone()
            }
            "missing-column" => {
                bad["schema"]["tables"][&table]["columns"]
                    .as_object_mut()
                    .unwrap()
                    .remove("old");
                format!("{table}.old")
            }
            "missing-role" => {
                bad["schema"]["roles"] = json!({});
                "reader".into()
            }
            "outer-unknown" => {
                bad["unknown"] = json!(true);
                "unknown field".into()
            }
            "ids-unknown" => {
                bad["ids"]["unknown"] = json!(true);
                "unknown field".into()
            }
            "schema-unknown" => {
                bad["schema"]["unknown"] = json!(true);
                "unknown field".into()
            }
            "future" => {
                bad["version"] = json!(999);
                "version 999".into()
            }
            "too-old" => {
                bad["version"] = json!(0);
                "version 0".into()
            }
            "malformed" | "absent" => "baseline file".into(),
            _ => unreachable!(),
        };
        if case == "absent" {
            std::fs::remove_file(&base).unwrap();
        } else {
            std::fs::write(
                &base,
                if case == "malformed" {
                    "not JSON".into()
                } else {
                    bad.to_string()
                },
            )
            .unwrap();
        }
        let input_before = if case == "absent" {
            None
        } else {
            Some(std::fs::read(&base).unwrap())
        };
        let result = d.run(&[
            "plan",
            "--no-dev",
            "--base",
            base.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
            "--format",
            "json",
        ]);
        assert_eq!(
            code(&result),
            1,
            "{case}: {}{}",
            stdout(&result),
            stderr(&result)
        );
        let report: Value = serde_json::from_str(&stdout(&result)).unwrap();
        assert_eq!(
            report["findings"][0]["id"], "baseline.unreadable",
            "{case}: {report}"
        );
        assert!(
            report["findings"][0]["message"]
                .as_str()
                .unwrap()
                .contains(&expected),
            "{case}: {report}"
        );
        for (path, bytes) in held.iter().zip(&before) {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                bytes,
                "{case}: {}",
                path.display()
            );
        }
        match input_before {
            Some(bytes) => assert_eq!(
                std::fs::read(&base).unwrap(),
                bytes,
                "{case}: baseline input changed"
            ),
            None => assert!(!base.exists(), "a refused missing baseline was created"),
        }
    }
}

pub fn corrupt_git(dialect: &str) {
    let (d, snapshot, _) = fixture(dialect, "git");
    let path = d.dir.join("schema.ids.json");
    let valid = std::fs::read(&path).unwrap();
    for case in ["version", "duplicate"] {
        let mut bad = snapshot["ids"].clone();
        if case == "version" {
            bad["version"] = json!(999);
        } else {
            let uid = spare_uid(&bad, "columns", "c");
            bad["columns"][uid] = bad["columns"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .clone();
        }
        std::fs::write(&path, bad.to_string()).unwrap();
        d.commit();
        std::fs::write(&path, &valid).unwrap();
        for args in [
            vec!["plan", "--check", "--format", "json"],
            vec!["plan", "--check", "--since", "HEAD", "--format", "json"],
            vec!["validate", "--since", "HEAD", "--format", "json"],
        ] {
            let out = d.run(&args);
            assert_ne!(code(&out), 0, "{case}: {}{}", stdout(&out), stderr(&out));
            let report: Value = serde_json::from_str(&stdout(&out)).unwrap();
            assert!(
                report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["id"] == "baseline.unreadable"
                        && f["message"].as_str().unwrap().contains("HEAD")),
                "{report}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), valid);
        }
        // Fixing the working file does not repair HEAD's evidence; committing
        // the repair does. No silent fallback or identity replacement.
        d.commit();
        success(&d, &["plan", "--check"]);
        success(&d, &["validate", "--since", "HEAD"]);
    }
}
