//! A routine rebuild must release its ordinary DEFAULT callers in the typed plan.
use super::*;
fn ok(o: Output) {
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}
fn widen(path: &std::path::Path, name: &str) {
    for e in std::fs::read_dir(path).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            widen(&p, name);
        } else {
            let old = std::fs::read_to_string(&p).unwrap();
            if old.contains(&format!("table: app.{name}\n")) {
                assert!(old.contains("type: int"), "{old}");
                std::fs::write(p, old.replacen("type: int", "type: bigint", 1)).unwrap();
            }
        }
    }
}
#[test]
#[ignore = "needs live mssql"]
fn default_holders_survive_routine_rebuilds_with_and_without_writer_contracts() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    for writer in [false, true] {
        for (rebuild, both) in [(true, false), (true, true), (false, true)] {
            let own = OwnDatabase::new(&server, "default2455");
            let connection = own.connection();
            on_server(connection, "CREATE SCHEMA app");
            on_server(
                connection,
                "CREATE TABLE app.source (old int NULL); INSERT app.source VALUES (7)",
            );
            on_server(
                connection,
                "CREATE FUNCTION app.f() RETURNS int WITH SCHEMABINDING AS BEGIN DECLARE @n int; SELECT @n=MAX(old) FROM app.source; RETURN ISNULL(@n,0); END",
            );
            on_server(
                connection,
                "CREATE TABLE app.t (old int CONSTRAINT user_default DEFAULT (app.f())); INSERT app.t VALUES (3)",
            );
            let d = Demo::new("default-holders2455");
            ok(d.run(&["pull", "--db", connection]));
            d.commit();
            ok(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
            if writer {
                std::fs::write(
                    d.dir.join("pbps.yml"),
                    "dialect: mssql\nconsumers:\n  writer: {writes: [app.t.old]}\n",
                )
                .unwrap();
            }
            if rebuild {
                widen(&d.dir.join("schema"), "source");
            }
            if both {
                widen(&d.dir.join("schema"), "t");
            }
            let plan = d.dir.join("default.json");
            let sql = d.dir.join("default.sql");
            ok(d.run(&[
                "plan",
                "--db",
                connection,
                "--out",
                plan.to_str().unwrap(),
                "--sql",
                sql.to_str().unwrap(),
            ]));
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
            // Apply first: reverting capture must expose the native 3729 witness,
            // rather than fail a shape assertion before executing the offered plan.
            let applied = d.run(&[
                "apply",
                "--db",
                connection,
                "--plan",
                plan.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&plan),
                "--allow",
                "destructive,revoke",
            ]);
            if code(&applied) != 0 {
                on_server(
                    connection,
                    "IF TYPE_NAME((SELECT user_type_id FROM sys.columns WHERE object_id=OBJECT_ID('app.source') AND name='old')) <> 'int' OR TYPE_NAME((SELECT user_type_id FROM sys.columns WHERE object_id=OBJECT_ID('app.t') AND name='old')) <> 'int' THROW 51000,'failed rebuild did not roll back its types',1; IF (SELECT COUNT(*) FROM sys.default_constraints WHERE parent_object_id=OBJECT_ID('app.t')) <> 1 OR (SELECT COUNT(*) FROM app.t WHERE old=3) <> 1 THROW 51000,'failed rebuild changed its DEFAULT or data',1;",
                );
            }
            ok(applied);
            ok(d.run(&["verify", "--db", connection]));
            let rendered = std::fs::read_to_string(&sql).unwrap();
            let text = serde_json::to_string(&json).unwrap();
            assert_eq!(
                text.matches("alter_column_default").count(),
                if rebuild { 2 } else { 0 },
                "{text}"
            );
            assert_eq!(
                rendered.matches("DROP FUNCTION").count(),
                usize::from(rebuild),
                "{rendered}"
            );
            on_server(
                connection,
                "IF (SELECT COUNT(*) FROM sys.default_constraints WHERE parent_object_id=OBJECT_ID('app.t')) <> 1 THROW 51000,'DEFAULT not restored once',1; IF (SELECT COUNT(*) FROM app.t WHERE old=3) <> 1 THROW 51000,'existing row changed',1; INSERT app.t DEFAULT VALUES; IF (SELECT COUNT(*) FROM app.t WHERE old=7) <> 1 THROW 51000,'restored DEFAULT did not call the function',1;",
            );
            on_server(
                connection,
                &format!(
                    "IF TYPE_NAME((SELECT user_type_id FROM sys.columns WHERE object_id=OBJECT_ID('app.t') AND name='old')) <> '{}' THROW 51000,'wrong caller type',1;",
                    if both { "bigint" } else { "int" }
                ),
            );
            ok(d.run(&["plan", "--db", connection]));
        }
    }
}

fn reference_rows(path: &std::path::Path, rows: &str, constant: bool) {
    for e in std::fs::read_dir(path).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            reference_rows(&p, rows, constant);
        } else {
            let old = std::fs::read_to_string(&p).unwrap();
            if old.contains("table: app.t\n") {
                let shape = old.split("\ndata:").next().unwrap().trim_end();
                let shape = shape
                    .lines()
                    .map(|line| {
                        if constant && line.trim_start().starts_with("default:") {
                            line.replacen(line.trim_start(), "default: '7'", 1)
                        } else {
                            line.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(p, format!("{shape}\ndata:\n  mode: exact\n  rows:\n{rows}"))
                    .unwrap();
            }
        }
    }
}

#[test]
#[ignore = "needs live mssql"]
fn reference_inserts_and_updates_take_the_restored_function_default() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    for update in [false, true] {
        let own = OwnDatabase::new(&server, "default_rows2455");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        on_server(
            db,
            "CREATE TABLE app.source (old int NULL); INSERT app.source VALUES (7)",
        );
        on_server(
            db,
            "CREATE FUNCTION app.f() RETURNS int WITH SCHEMABINDING AS BEGIN DECLARE @n int; SELECT @n=MAX(old) FROM app.source; RETURN ISNULL(@n,0); END",
        );
        on_server(
            db,
            "CREATE TABLE app.t (id int NOT NULL PRIMARY KEY, old int CONSTRAINT user_default DEFAULT (app.f())); INSERT app.t VALUES (1,3)",
        );
        let d = Demo::new("default-rows2455");
        ok(d.run(&["pull", "--db", db]));
        reference_rows(&d.dir.join("schema"), "    1: {old: 3}\n", false);
        d.commit();
        ok(d.run(&["baseline", "--db", db, "--reason", "adopt"]));
        widen(&d.dir.join("schema"), "source");
        // A constant replacement gives the connected reader a definite
        // SET DEFAULT update; the other case retains the opaque function
        // default for an omitted-column insert, whose value must be 7 too.
        let rows = if update {
            "    1: {}\n    2: {}\n    3: {old: 23}\n"
        } else {
            "    1: {old: 3}\n    2: {}\n    3: {old: 23}\n"
        };
        reference_rows(&d.dir.join("schema"), rows, update);
        let path = d.dir.join("rows.json");
        ok(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let changes = &saved.changes.changes;
        let restore = changes
            .iter()
            .position(|p| {
                matches!(
                    p.change,
                    pbps_model::Change::AlterColumnDefault { to: Some(_), .. }
                )
            })
            .unwrap();
        let insert = changes.iter().position(|p| matches!(&p.change, pbps_model::Change::InsertRow { defaults, .. } if defaults.contains_key("old"))).unwrap();
        assert!(restore < insert, "{saved:?}");
        if update {
            let write = changes.iter().position(|p| matches!(&p.change, pbps_model::Change::UpdateRow { columns, .. } if columns.values().any(|(_, to)| matches!(to, pbps_model::Cell::Default(_))))).unwrap();
            assert!(restore < write, "{saved:?}");
        }
        ok(d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "destructive,revoke,data-update",
        ]));
        let first = if update { 7 } else { 3 };
        on_server(
            db,
            &format!(
                "IF (SELECT COUNT(*) FROM app.t WHERE id=1 AND old={first}) <> 1 OR (SELECT COUNT(*) FROM app.t WHERE id=2 AND old=7) <> 1 OR (SELECT COUNT(*) FROM app.t WHERE id=3 AND old=23) <> 1 OR EXISTS (SELECT 1 FROM app.t WHERE old IS NULL) THROW 51000,'reference writes missed the restored DEFAULT',1; IF (SELECT COUNT(*) FROM sys.default_constraints WHERE parent_object_id=OBJECT_ID('app.t')) <> 1 THROW 51000,'DEFAULT not restored once',1;"
            ),
        );
        ok(d.run(&["verify", "--db", db]));
    }
}
