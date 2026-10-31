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
