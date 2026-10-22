//! The same identity/history contract on both live engines (DEC-1790.1).
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use std::process::Output;

fn ok(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}

fn schema(pg: bool) -> &'static str {
    if pg { "app" } else { "dbo" }
}

fn prepare(connection: &str, pg: bool) {
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    on_server(connection, &format!(
        "CREATE TABLE {}.country (code varchar(2) NOT NULL, label varchar(40) NOT NULL, CONSTRAINT pk_country PRIMARY KEY (code));
         INSERT INTO {}.country (code, label) VALUES ('TW', 'Taiwan'), ('JP', 'Japan'), ('NZ', 'New Zealand');",
        schema(pg), schema(pg)
    ));
}

fn rows(connection: &str, pg: bool) -> i64 {
    reference_rows(connection, pg, "country")
}

fn reference_rows(connection: &str, pg: bool, table: &str) -> i64 {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let driver = if pg { pbps_db::Driver::Postgres } else { pbps_db::Driver::Mssql };
        let mut c = pbps_db::Conn::connect(driver, connection).await.unwrap();
        c.query(&format!("SELECT CAST(COUNT(*) AS bigint) FROM {}.{table} WHERE (code = 'TW' AND label = 'Taiwan') OR (code = 'JP' AND label = 'Japan') OR (code = 'NZ' AND label = 'New Zealand')", schema(pg))).await.unwrap()[0].try_get_at::<i64>(0).unwrap().unwrap()
    })
}

fn history(d: &Demo, connection: &str) -> String {
    stdout(&ok(d.run(&[
        "state", "list", "--db", connection, "--format", "json",
    ])))
}

pub fn unchanged(server: &str, pg: bool) {
    for data in [false, true] {
        let slug = format!("pull_identity_unchanged_{data}");
        let own = OwnDatabase::new(server, &slug);
        let connection = own.connection();
        prepare(connection, pg);
        let d = Demo::new(&slug);
        let table = format!("{}.country", schema(pg));
        let mut args = vec!["pull", "--db", connection];
        if data {
            args.extend(["--data", &table]);
        }
        ok(d.run(&args));
        let ids_path = d.dir.join("schema.ids.json");
        // Preserve historical tombstones as well as live IDs, and do not
        // silently canonicalize the user's JSON when nothing changed.
        let mut ids: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ids_path).unwrap()).unwrap();
        ids["tombstones"] = serde_json::json!({"t_r3t1rd": {"was": format!("{}.retired", schema(pg)), "dropped_at": "2026-01-01", "reason": "retired reference list", "operator": "test"}});
        let bytes = serde_json::to_vec(&ids).unwrap();
        std::fs::write(&ids_path, &bytes).unwrap();
        d.commit();
        ok(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "adopt reference data",
        ]));
        let before = history(&d, connection);
        args.push("--force");
        for _ in 0..2 {
            ok(d.run(&args));
            assert_eq!(std::fs::read(&ids_path).unwrap(), bytes);
            ok(d.run(&["validate"]));
            let offline = ok(d.run(&["plan"]));
            assert!(
                stdout(&offline).contains("No changes"),
                "{}",
                stdout(&offline)
            );
            let plan = d.dir.join("plan.json");
            let sql = d.dir.join("plan.sql");
            let connected = ok(d.run(&[
                "plan",
                "--db",
                connection,
                "--out",
                plan.to_str().unwrap(),
                "--sql",
                sql.to_str().unwrap(),
            ]));
            assert!(
                stdout(&connected).contains("No changes"),
                "{}",
                stdout(&connected)
            );
            let script = std::fs::read_to_string(&sql).unwrap().to_uppercase();
            assert!(
                !script.contains("DROP TABLE") && !script.contains("CREATE TABLE"),
                "{script}"
            );
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
            ok(d.run(&[
                "apply",
                "--db",
                connection,
                "--plan",
                plan.to_str().unwrap(),
                "--checksum",
                &saved.wire_fingerprint(),
                "--allow",
                "destructive",
            ]));
            assert_eq!(rows(connection, pg), 3);
            assert_eq!(history(&d, connection), before);
        }
        if !data {
            // First row adoption on an already managed table must preserve
            // identity too. Recording a zero-SQL adoption is #1803's scope.
            ok(d.run(&["pull", "--db", connection, "--force", "--data", &table]));
            assert_eq!(std::fs::read(&ids_path).unwrap(), bytes);
            let plan = ok(d.run(&["plan", "--db", connection]));
            assert!(!stdout(&plan).contains("drop table"), "{}", stdout(&plan));
            assert!(!stdout(&plan).contains("create table"), "{}", stdout(&plan));
            assert_eq!(rows(connection, pg), 3);
            assert_eq!(history(&d, connection), before);
        }
    }
}

pub fn additions(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "pull_identity_additions");
    let connection = own.connection();
    prepare(connection, pg);
    let d = Demo::new("pull-identity-additions");
    ok(d.run(&["pull", "--db", connection]));
    let path = d.dir.join("schema.ids.json");
    let before: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    on_server(
        connection,
        &format!(
            "ALTER TABLE {}.country ADD population bigint NULL; CREATE TABLE {}.region (id int NOT NULL);",
            schema(pg),
            schema(pg)
        ),
    );
    ok(d.run(&["pull", "--db", connection, "--force"]));
    let bytes = std::fs::read(&path).unwrap();
    let after: pbps_model::IdsFile = serde_json::from_slice(&bytes).unwrap();
    for (id, name) in &before.tables {
        assert_eq!(after.tables.get(id), Some(name));
    }
    for (id, name) in &before.columns {
        assert_eq!(after.columns.get(id), Some(name));
    }
    assert_eq!(after.tables.len(), before.tables.len() + 1);
    assert_eq!(after.columns.len(), before.columns.len() + 2);
    ok(d.run(&["pull", "--db", connection, "--force"]));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(rows(connection, pg), 3);

    // PostgreSQL validation leaves this entire table out; SQL Server reaches
    // the identity-format guard instead. Neither path may forget its IDs.
    let declaration = d.dir.join(format!("schema/{}.country.yml", schema(pg)));
    let yaml = std::fs::read(&declaration).unwrap();
    let bad = if pg { "\"bad.name\"" } else { "[bad.name]" };
    on_server(
        connection,
        &format!("ALTER TABLE {}.country ADD {bad} int NULL", schema(pg)),
    );
    let refused = d.run(&["pull", "--db", connection, "--force"]);
    assert_eq!(
        code(&refused),
        if pg { 2 } else { 1 },
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("bad.name"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    if pg {
        assert!(!declaration.exists());
        assert!(
            stderr(&refused).contains("drop-table"),
            "{}",
            stderr(&refused)
        );
    } else {
        assert_eq!(std::fs::read(&declaration).unwrap(), yaml);
    }
}

pub fn intent_and_drift(server: &str, pg: bool, drift: bool) {
    for rename in [false, true] {
        let slug = format!("pull_identity_intent_{drift}_{rename}");
        let own = OwnDatabase::new(server, &slug);
        let connection = own.connection();
        prepare(connection, pg);
        let d = Demo::new(&slug);
        ok(d.run(&["pull", "--db", connection]));
        d.commit();
        ok(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let before = history(&d, connection);
        let ids_path = d.dir.join("schema.ids.json");
        let ids = std::fs::read(&ids_path).unwrap();
        let old = format!("{}.country", schema(pg));
        let new = format!("{}.nation", schema(pg));
        let path = d.dir.join(format!("schema/{old}.yml"));
        if drift {
            let sql = if rename {
                if pg {
                    format!("ALTER TABLE {old} RENAME TO nation")
                } else {
                    format!("EXEC sp_rename '{old}', 'nation'")
                }
            } else {
                format!("DROP TABLE {old}")
            };
            on_server(connection, &sql);
            let out = d.run(&["pull", "--db", connection, "--force"]);
            assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out).contains("identity") || stderr(&out).contains("unchanged"),
                "{}",
                stderr(&out)
            );
            assert_eq!(std::fs::read(&ids_path).unwrap(), ids);
            assert!(!path.exists());
            assert_eq!(d.dir.join(format!("schema/{new}.yml")).exists(), rename);
            assert_eq!(history(&d, connection), before);
        } else if rename {
            let declaration = std::fs::read_to_string(&path).unwrap().replace(&old, &new);
            std::fs::write(d.dir.join(format!("schema/{new}.yml")), declaration).unwrap();
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        // No inference, even for a single vanished table and one new table.
        let unresolved = d.run(&["plan"]);
        assert_eq!(
            code(&unresolved),
            2,
            "{}{}",
            stdout(&unresolved),
            stderr(&unresolved)
        );
        assert_eq!(std::fs::read(&ids_path).unwrap(), ids);
        if rename {
            ok(d.run(&["rename-table", &old, &new]));
        } else {
            ok(d.run(&["drop-table", &old, "--reason", "retired reference list"]));
        }
        let intent_ids: pbps_model::IdsFile =
            serde_json::from_slice(&std::fs::read(&ids_path).unwrap()).unwrap();
        let old_ids: pbps_model::IdsFile = serde_json::from_slice(&ids).unwrap();
        if rename {
            let uid = old_ids.tables.keys().next().unwrap();
            assert_eq!(intent_ids.tables[uid].raw().to_string(), new);
        } else {
            let uid = old_ids.tables.keys().next().unwrap();
            assert_eq!(intent_ids.tombstones[uid].reason, "retired reference list");
        }
        ok(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("intent-plan.json");
        let out = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
        if drift {
            assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
            assert!(stderr(&out).contains("has drifted"), "{}", stderr(&out));
            assert!(!plan.exists());
        } else {
            let out = ok(out);
            assert!(
                stdout(&out).contains(if rename { "rename table" } else { "drop table" }),
                "{}",
                stdout(&out)
            );
            if !rename {
                let saved: pbps_model::SavedPlan =
                    serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
                let refused = d.run(&[
                    "apply",
                    "--db",
                    connection,
                    "--plan",
                    plan.to_str().unwrap(),
                    "--checksum",
                    &saved.wire_fingerprint(),
                ]);
                assert_ne!(code(&refused), 0);
                assert_eq!(rows(connection, pg), 3);
            }
        }
        assert_eq!(history(&d, connection), before);
        if !drift && rename {
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
            ok(d.run(&[
                "apply",
                "--db",
                connection,
                "--plan",
                plan.to_str().unwrap(),
                "--checksum",
                &saved.wire_fingerprint(),
                "--allow",
                "rename",
            ]));
            assert_eq!(reference_rows(connection, pg, "nation"), 3);
            let after = history(&d, connection);
            let renamed_ids = std::fs::read(&ids_path).unwrap();
            ok(d.run(&["pull", "--db", connection, "--force"]));
            assert_eq!(std::fs::read(&ids_path).unwrap(), renamed_ids);
            let next = ok(d.run(&["plan", "--db", connection]));
            assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
            assert_eq!(history(&d, connection), after);
        }
    }
}

pub fn batch_after_forced_pull(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "pull_identity_batch1791");
    let connection = own.connection();
    prepare(connection, pg);
    let table = format!("{}.country", schema(pg));
    for column in ["legacy_email", "legacy_phone"] {
        on_server(
            connection,
            &format!("ALTER TABLE {table} ADD {column} int NULL"),
        );
    }
    let d = Demo::new("pull-identity-batch1791");
    ok(d.run(&["pull", "--db", connection]));
    d.commit();
    ok(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    let before = history(&d, connection);
    let path = d.dir.join("schema.ids.json");
    let ids = std::fs::read(&path).unwrap();
    for column in ["legacy_email", "legacy_phone"] {
        on_server(
            connection,
            &format!("ALTER TABLE {table} DROP COLUMN {column}"),
        );
    }
    let out = d.run(&["pull", "--db", connection, "--force"]);
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    let email = format!("{table}.legacy_email");
    let phone = format!("{table}.legacy_phone");
    assert!(
        stderr(&out).contains(&format!(
            "pbps intent --drop {email} \"<why>\" --drop {phone} \"<why>\""
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(&path).unwrap(), ids);
    ok(d.run(&[
        "intent",
        "--drop",
        &email,
        "email retired",
        "--drop",
        &phone,
        "phone retired",
    ]));
    let recorded: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(
        recorded
            .tombstones
            .values()
            .any(|t| t.was == email && t.reason == "email retired")
    );
    assert!(
        recorded
            .tombstones
            .values()
            .any(|t| t.was == phone && t.reason == "phone retired")
    );
    ok(d.run(&["plan", "--check"]));
    assert_eq!(history(&d, connection), before);
}

pub fn column_history(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "pull_identity_column_history");
    let connection = own.connection();
    prepare(connection, pg);
    let table = format!("{}.country", schema(pg));
    on_server(
        connection,
        &format!("ALTER TABLE {table} ADD obsolete int NULL"),
    );
    let d = Demo::new("pull-identity-column-history");
    ok(d.run(&["pull", "--db", connection]));
    d.commit();
    ok(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    let ids_path = d.dir.join("schema.ids.json");
    let original: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(&ids_path).unwrap()).unwrap();
    let uid = original
        .column_uid(&format!("{table}.obsolete").parse().unwrap())
        .unwrap();
    let file = d.dir.join(format!("schema/{table}.yml"));
    for rename in [true, false] {
        let yaml = std::fs::read_to_string(&file).unwrap();
        let updated = if rename {
            yaml.replace("  obsolete:", "  deprecated:")
        } else {
            let mut in_column = false;
            yaml.lines()
                .filter(|line| {
                    if !line.starts_with("    ") {
                        in_column = line.starts_with("  deprecated:");
                    }
                    !in_column
                })
                .map(|line| format!("{line}\n"))
                .collect()
        };
        assert_ne!(updated, yaml);
        std::fs::write(&file, updated).unwrap();
        if rename {
            ok(d.run(&["rename", &format!("{table}.obsolete"), "deprecated"]));
        } else {
            ok(d.run(&[
                "drop",
                &format!("{table}.deprecated"),
                "--reason",
                "unused source field",
            ]));
        }
        d.commit();
        let plan = d.dir.join("column-plan.json");
        ok(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
        ok(d.run(&[
            "apply",
            "--db",
            connection,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &saved.wire_fingerprint(),
            "--allow",
            if rename { "rename" } else { "destructive" },
        ]));
        let bytes = std::fs::read(&ids_path).unwrap();
        let ids: pbps_model::IdsFile = serde_json::from_slice(&bytes).unwrap();
        if rename {
            assert_eq!(
                ids.columns[uid].raw().to_string(),
                format!("{table}.deprecated")
            );
        } else {
            assert_eq!(ids.tombstones[uid].reason, "unused source field");
        }
        let before = history(&d, connection);
        ok(d.run(&["pull", "--db", connection, "--force"]));
        assert_eq!(std::fs::read(&ids_path).unwrap(), bytes);
        let next = ok(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        assert_eq!(history(&d, connection), before);
        assert_eq!(rows(connection, pg), 3);
    }
}
