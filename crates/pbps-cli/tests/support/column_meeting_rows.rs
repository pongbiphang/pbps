//! Row-validation contributions at certified column/membership boundaries.
use super::*;
fn refusal(output: &Output, label: &str) {
    assert!(
        matches!(code(output), 1 | 2),
        "{label}: {}{}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stderr(output).contains("unrepresentable")
            || stderr(output).contains("cannot be represented")
            || stderr(output).contains("identity")
            || stderr(output).contains("cannot share")
            || stderr(output).contains("separate plans")
            || stderr(output).contains("NOT NULL"),
        "{label}: {}",
        stderr(output)
    );
}
pub(super) fn rows(server: &str) {
    for (kind, ty, cell, _alter, target) in [
        (
            "narrow",
            "bigint",
            "100000",
            "ALTER TABLE app.p ALTER COLUMN d TYPE smallint",
            "smallint",
        ),
        (
            "notnull",
            "integer",
            "NULL",
            "ALTER TABLE app.p ALTER COLUMN d SET NOT NULL",
            "integer",
        ),
    ] {
        for motion in ["detach", "drop", "attach"] {
            let own = OwnDatabase::new(server, &format!("moves2296_rows_{kind}_{motion}"));
            let db = own.connection();
            let setup = format!(
                "CREATE SCHEMA app; CREATE TABLE app.p(k integer NOT NULL,d {ty}) PARTITION BY RANGE(k); CREATE TABLE app.b PARTITION OF app.p FOR VALUES FROM(0) TO(10); {} INSERT INTO app.b VALUES(1,1);INSERT INTO app.a VALUES(11,{cell})",
                if motion == "attach" {
                    format!("CREATE TABLE app.a(k integer NOT NULL,d {ty});")
                } else {
                    "CREATE TABLE app.a PARTITION OF app.p FOR VALUES FROM(10) TO(20);".into()
                }
            );
            on_server(db, &setup);
            let d = Demo::new(&format!("moves2296-rows-{kind}-{motion}"));
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            succeeds(d.run(&[
                "baseline",
                "--db",
                db,
                "--reason",
                "adopt row contribution fixture",
            ]));
            let parent = format!(
                "table: app.p\ncolumns:\n  k: {{type: integer, nullable: false}}\n  d: {{type: {target}{}}}\npartition_by: [k]\npartitions:\n  b: {{from: [0], to: [10]}}\n{}",
                if kind == "notnull" {
                    ", nullable: false"
                } else {
                    ""
                },
                if motion == "attach" {
                    "  a: {from: [10], to: [20]}\n"
                } else {
                    ""
                }
            );
            std::fs::write(d.dir.join("schema/app.p.yml"), parent).unwrap();
            if motion == "detach" {
                std::fs::write(d.dir.join("schema/app.a.yml"),format!("table: app.a\ncolumns:\n  k: {{type: integer, nullable: false}}\n  d: {{type: {ty}}}\n")).unwrap();
            }
            if motion == "drop" {
                succeeds(d.run(&[
                    "drop-table",
                    "app.a",
                    "--reason",
                    "remove the bad contribution",
                ]));
            }
            if motion == "attach" {
                std::fs::remove_file(d.dir.join("schema/app.a.yml")).unwrap();
            }
            // Real resolution, without constructing an executable plan in a fixture.
            let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
            let id_path = d.dir.join("schema.ids.json");
            let old: pbps_model::IdsFile =
                serde_json::from_slice(&std::fs::read(&id_path).unwrap()).unwrap();
            let ids = pbps_diff::resolve(
                &loaded.schema,
                &old,
                &[],
                &pbps_diff::Context {
                    operator: "fixture".into(),
                    today: "2026-10-24".into(),
                },
            )
            .unwrap()
            .ids;
            std::fs::write(id_path, serde_json::to_vec_pretty(&ids).unwrap()).unwrap();
            d.commit();
            let path = d.dir.join("plan.json");
            let sql = d.dir.join("plan.sql");
            let planned = d.run(&[
                "plan",
                "--db",
                db,
                "--out",
                path.to_str().unwrap(),
                "--sql",
                sql.to_str().unwrap(),
            ]);
            if motion == "attach" && code(&planned) != 0 {
                refusal(&planned, kind);
                continue;
            }
            succeeds(planned);
            let text = std::fs::read_to_string(sql).unwrap();
            println!("ROW-SCOPE {kind}/{motion}: {text}");
            let applied = approved_apply(
                &d,
                db,
                &path,
                &[
                    "--allow",
                    "narrowing",
                    "--allow",
                    "not-null",
                    "--allow",
                    "destructive",
                    "--allow",
                    "constraint",
                ],
            );
            if motion == "attach" {
                assert_eq!(
                    code(&applied),
                    1,
                    "{kind}: {}{}",
                    stdout(&applied),
                    stderr(&applied)
                );
                assert!(
                    stderr(&applied).contains("NULLs")
                        || stderr(&applied).contains("cannot become"),
                    "{}",
                    stderr(&applied)
                );
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*) FROM pg_catalog.pg_inherits WHERE inhrelid='app.a'::regclass"
                    ),
                    0
                );
                assert_eq!(
                    text_of(
                        db,
                        "SELECT pg_catalog.format_type(atttypid,atttypmod) FROM pg_catalog.pg_attribute WHERE attrelid='app.p'::regclass AND attname='d'"
                    ),
                    ty
                );
                assert_eq!(scalar(db, "SELECT count(*) FROM ONLY app.a"), 1);
            } else {
                succeeds(applied);
                assert!(
                    text.find(if motion == "drop" {
                        "DROP TABLE"
                    } else {
                        "DETACH PARTITION"
                    })
                    .unwrap()
                        < text
                            .find(if kind == "narrow" {
                                "TYPE smallint"
                            } else {
                                "SET NOT NULL"
                            })
                            .unwrap(),
                    "{kind}/{motion}: {text}"
                );
                succeeds(d.run(&["verify", "--db", db]));
                let empty = d.dir.join("empty.json");
                succeeds(d.run(&["plan", "--db", db, "--out", empty.to_str().unwrap()]));
                let saved: pbps_model::SavedPlan =
                    serde_json::from_slice(&std::fs::read(empty).unwrap()).unwrap();
                assert!(saved.changes.is_empty());
            }
        }
    }
}
