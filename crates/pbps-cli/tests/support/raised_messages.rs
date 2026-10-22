//! Exercise the actual returned guard sentence, after the CLI's SQL context.

use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use std::process::Output;

fn ok(out: Output) {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
}

fn assert_message(text: &str, expected: &str, pg: bool) {
    if pg {
        assert!(text.ends_with(&format!(": {expected}\n")), "{text:?}");
    } else {
        let returned = text
            .rsplit_once("Token error: '")
            .expect("the driver frame")
            .1;
        let message = returned
            .split_once("' on server ")
            .expect("the server frame")
            .0;
        assert_eq!(message, expected);
    }
}

pub fn check(server: &str, pg: bool) {
    for (name, shown) in [
        ("guard\n\u{1b}[31m", r"guard\n\u{1b}[31m"),
        ("guard", "guard"),
    ] {
        for staged in [false, true] {
            let own = OwnDatabase::new(server, "raised2161");
            let connection = own.connection();
            let d = Demo::new("raised2161");
            let schema = if pg { "app" } else { "dbo" };
            let qualified = if pg {
                format!("app.\"{name}\"")
            } else {
                format!("[dbo].[{name}]")
            };
            if pg {
                on_server(
                    connection,
                    &format!(
                        "CREATE SCHEMA app; CREATE TABLE {qualified}(code varchar(20) NOT NULL PRIMARY KEY, label varchar(50) NOT NULL); \
                     CREATE FUNCTION app.skip() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;"
                    ),
                );
            } else {
                on_server(
                    connection,
                    &format!(
                        "CREATE TABLE {qualified}(code varchar(20) NOT NULL PRIMARY KEY, label varchar(50) NOT NULL)"
                    ),
                );
            }
            // Pull before adding the trigger: block-scalar ESC handling is a
            // separate pull bug (#2186), not the guard this test must reach.
            ok(d.run(&["pull", "--db", connection]));
            let definition = if pg {
                on_server(
                    connection,
                    &format!(
                        "CREATE TRIGGER skip BEFORE INSERT ON {qualified} FOR EACH ROW EXECUTE FUNCTION app.skip()"
                    ),
                );
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                        .await
                        .unwrap();
                    let rows = conn.query("SELECT pg_get_triggerdef(oid) AS definition FROM pg_catalog.pg_trigger WHERE tgname = 'skip' AND NOT tgisinternal").await.unwrap();
                    rows[0].try_get::<&str>("definition").unwrap().unwrap()
                        .strip_prefix("CREATE TRIGGER skip ").unwrap().to_owned()
                })
            } else {
                let definition = format!("AFTER INSERT AS DELETE FROM {qualified};");
                on_server(
                    connection,
                    &format!(
                        "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON; EXEC(N'CREATE TRIGGER dbo.skip ON {qualified} {definition}')"
                    ),
                );
                definition
            };
            std::fs::write(
                d.dir.join("schema/skip.trigger.yml"),
                format!(
                    "trigger: {schema}.skip\non: {}\ndefinition: {}\n",
                    serde_json::to_string(&format!("{schema}.{name}")).unwrap(),
                    serde_json::to_string(&definition).unwrap(),
                ),
            )
            .unwrap();
            let table = std::fs::read_dir(d.dir.join("schema"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| std::fs::read_to_string(path).unwrap().starts_with("table:"))
                .unwrap();
            let declared = format!(
                "{}data:\n  mode: exact\n  rows: {{}}\n",
                std::fs::read_to_string(&table).unwrap()
            );
            std::fs::write(&table, &declared).unwrap();
            ok(d.run(&["plan", "--no-dev"]));
            d.commit();
            ok(d.run(&[
                "baseline",
                "--db",
                connection,
                "--reason",
                "adopt the recorded trigger",
            ]));
            std::fs::write(
                &table,
                declared.replace("rows: {}", "rows:\n    x: {label: New}"),
            )
            .unwrap();
            let path = d.dir.join("plan.json");
            let mut args = vec!["plan", "--db", connection, "--out", path.to_str().unwrap()];
            if staged {
                args.push("--staged");
            }
            ok(d.run(&args));
            let plan: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let checksum = plan.wire_fingerprint();
            let mut args = vec![
                "apply",
                "--db",
                connection,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
            ];
            if staged {
                args.push("--staged");
            }
            let out = d.run(&args);
            assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
            let dash = if pg { "--" } else { "—" };
            let expected = format!(
                "{schema}.{shown} row `x` is not what this plan wrote once the statement had run {dash} a \
                trigger on the table, another writer inside it, or a value the engine stores \
                differently from the way it is declared. Nothing was applied; `pbps plan --db` \
                says which."
            );
            assert_message(&stderr(&out), &expected, pg);
            if !pg {
                for format in ["human", "json"] {
                    let out = d.run(&["plan", "--dev", connection, "--format", format]);
                    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
                    assert_message(&stderr(&out), &expected, false);
                    if format == "json" {
                        let report: serde_json::Value =
                            serde_json::from_str(&stdout(&out)).unwrap();
                        assert_message(
                            report["findings"][0]["message"].as_str().unwrap(),
                            &expected,
                            false,
                        );
                    }
                }
            }
        }
    }
}
