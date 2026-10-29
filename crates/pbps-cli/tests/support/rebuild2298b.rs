//! Public CLI acceptance of recorded rebuilds on both supported PG majors.
use super::*;

#[test]
#[ignore = "needs live pg18"]
fn approved_rebuilds_copy_conversions_and_chains_atomically_on_pg18() {
    acceptance(&server());
    negatives(&server());
}
#[test]
#[ignore = "needs live pg16"]
fn approved_rebuilds_copy_conversions_and_chains_atomically_on_pg16() {
    acceptance(&old_server());
    negatives(&old_server());
}

fn write_tree(d: &Demo, schema: &pbps_model::Schema) {
    let root: pbps_model::TableName = "app.p".parse().unwrap();
    let partitions: Vec<_> = schema
        .tables
        .iter()
        .filter(|(_, t)| t.partition_of.as_ref().is_some_and(|p| p.parent == root))
        .collect();
    std::fs::write(
        d.dir.join("schema/app.p.yml"),
        pbps_load::render_partitioned(&root, &schema.tables[&root], &partitions, &[], None),
    )
    .unwrap();
}
fn approve(d: &Demo, cap: &str) {
    succeeds(d.run(&[
        "rebuild-table",
        "app.p",
        "--max-rows",
        cap,
        "--reason",
        "maintenance",
    ]));
    d.commit();
}
fn apply(d: &Demo, connection: &str, path: &std::path::Path) -> Output {
    d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &plan_checksum(path),
        "--allow",
        "destructive,rename,constraint,narrowing,revoke",
    ])
}

struct Reader<'a> {
    server: &'a str,
    name: String,
}
impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let _ = try_on_server(self.server, &format!("DROP ROLE IF EXISTS {}", self.name));
    }
}

fn acceptance(server: &str) {
    let reader = Reader {
        server,
        name: format!("w2298b_reader_{}", std::process::id()),
    };
    on_server(
        server,
        &format!(
            "DROP ROLE IF EXISTS {}; CREATE ROLE {}",
            reader.name, reader.name
        ),
    );
    for kind in ["key", "type", "using", "chain", "composed_using", "default"] {
        let own = OwnDatabase::new(server, &format!("rebuild2298b_{kind}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.p(k integer NOT NULL,bucket integer NOT NULL,label text) PARTITION BY RANGE(k); CREATE TABLE app.p1 PARTITION OF app.p FOR VALUES FROM(0) TO(10); INSERT INTO app.p VALUES(1,2,'a'),(3,4,'b')",
        );
        if kind == "default" {
            on_server(connection, "UPDATE app.p SET bucket=14 WHERE k=3");
        }
        let dependents = matches!(kind, "key" | "type");
        if dependents {
            on_server(
                connection,
                &format!(
                    "ALTER TABLE app.p ADD CONSTRAINT pk_p PRIMARY KEY(k,bucket); CREATE INDEX p_label ON app.p(label); CREATE INDEX p1_label_own ON app.p1(label); CREATE TABLE app.lookup(id integer PRIMARY KEY); INSERT INTO app.lookup SELECT generate_series(0,10); ALTER TABLE app.p ADD CONSTRAINT outgoing FOREIGN KEY(k) REFERENCES app.lookup(id); CREATE TABLE app.ref(k integer,bucket integer, CONSTRAINT incoming FOREIGN KEY(k,bucket) REFERENCES app.p(k,bucket)); INSERT INTO app.ref VALUES(1,2),(3,4); CREATE VIEW app.v AS SELECT k,bucket FROM app.p; CREATE FUNCTION app.f() RETURNS bigint LANGUAGE SQL BEGIN ATOMIC SELECT sum(k)::bigint FROM app.p; END; CREATE FUNCTION app.tg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN NEW.label := NEW.label || ''!''; RETURN NEW; END'; CREATE TRIGGER label_trigger BEFORE INSERT ON app.p FOR EACH ROW EXECUTE FUNCTION app.tg(); GRANT SELECT ON app.p,app.v TO {}; GRANT EXECUTE ON FUNCTION app.f() TO {}",
                    reader.name, reader.name
                ),
            );
        }
        if dependents {
            on_server(
                connection,
                &format!("GRANT USAGE ON SCHEMA app TO {}", reader.name),
            );
        }
        let d = Demo::new(&format!("rebuild2298b-{kind}"));
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let source = latest_snapshot(connection);
        let mut desired = source.schema.clone();
        let root: pbps_model::TableName = "app.p".parse().unwrap();
        if kind == "type" || kind == "using" || kind == "composed_using" {
            desired
                .tables
                .get_mut(&root)
                .unwrap()
                .columns
                .get_mut("k")
                .unwrap()
                .ty = "bigint".parse().unwrap();
        } else {
            desired
                .tables
                .get_mut(&root)
                .unwrap()
                .partition_by
                .as_mut()
                .unwrap()
                .columns = vec!["bucket".into()];
        }
        if kind == "default" {
            desired.tables.insert(
                "app.fallback".parse().unwrap(),
                pbps_model::Table {
                    partition_of: Some(pbps_model::PartitionOf {
                        parent: root.clone(),
                        bound: pbps_model::PartitionBound::Default,
                        columns: Default::default(),
                    }),
                    ..Default::default()
                },
            );
        }
        write_tree(&d, &desired);
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "missing approval must refuse");
        if kind == "using" || kind == "composed_using" {
            succeeds(d.run(&["convert", "app.p.k", "--using", "k::bigint + 1"]));
        }
        approve(&d, "10");
        if kind == "chain" {
            desired
                .tables
                .get_mut(&root)
                .unwrap()
                .columns
                .get_mut("bucket")
                .unwrap()
                .ty = "bigint".parse().unwrap();
            write_tree(&d, &desired);
            approve(&d, "5");
        }
        if kind == "composed_using" {
            desired
                .tables
                .get_mut(&root)
                .unwrap()
                .columns
                .get_mut("k")
                .unwrap()
                .ty = "integer".parse().unwrap();
            write_tree(&d, &desired);
            succeeds(d.run(&["convert", "app.p.k", "--using", "k::integer * 2"]));
            approve(&d, "5");
        }
        for command in [
            vec!["snapshot", "--db", connection, "--force"],
            vec!["baseline", "--db", connection, "--reason", "retain source"],
        ] {
            succeeds(d.run(&command));
            assert_eq!(latest_snapshot(connection).ids.tables, source.ids.tables);
        }
        let path = d.dir.join("rebuild.json");
        let planned =
            succeeds(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
        assert!(
            stdout(&planned).contains("blocking rewrite"),
            "{}",
            stdout(&planned)
        );
        assert!(stdout(&planned).contains("WAL"));
        let plan: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(plan.version, 57);
        let refused = apply_plan(&d, connection, &path, false);
        assert_ne!(code(&refused), 0, "separate risk approval is required");
        succeeds(apply(&d, connection, &path));
        assert_eq!(scalar(connection, "SELECT count(*) FROM app.p"), 2);
        assert_eq!(
            scalar(
                connection,
                if kind == "default" {
                    "SELECT count(*) FROM app.p WHERE (k,bucket) IN ((1,2),(3,14))"
                } else if kind == "composed_using" {
                    "SELECT count(*) FROM app.p WHERE (k,bucket) IN ((4,2),(8,4))"
                } else if kind == "using" {
                    "SELECT count(*) FROM app.p WHERE (k,bucket) IN ((2,2),(4,4))"
                } else {
                    "SELECT count(*) FROM app.p WHERE (k,bucket) IN ((1,2),(3,4))"
                }
            ),
            2
        );
        let recorded = latest_snapshot(connection);
        assert_eq!(recorded.version, 35);
        assert_eq!(recorded.ids.tables, plan.ids.tables);
        if dependents {
            assert_eq!(scalar(connection, "SELECT count(*) FROM app.v"), 2);
            assert_eq!(scalar(connection, "SELECT app.f()"), 4);
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM app.ref r JOIN app.p p USING(k,bucket)"
                ),
                2
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_index WHERE indrelid IN ('app.p'::regclass,'app.p1'::regclass) AND indisvalid"
                ),
                5
            );
            assert_eq!(
                scalar(
                    connection,
                    &format!(
                        "SELECT count(*) FROM pg_roles WHERE rolname='{}' AND has_table_privilege(oid,'app.p','SELECT') AND has_table_privilege(oid,'app.v','SELECT') AND has_function_privilege(oid,'app.f()','EXECUTE')",
                        reader.name
                    )
                ),
                1
            );
            assert!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_statistic WHERE starelid='app.p'::regclass"
                ) > 0
            );
            on_server(connection, "INSERT INTO app.p VALUES(5,6,'c')");
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM app.p WHERE k=5 AND label='c!'"
                ),
                1
            );
        }
        succeeds(d.run(&["verify", "--db", connection]));
        let next = succeeds(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
        let empty: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(empty.changes.is_empty(), "{}", stdout(&next));
        succeeds(d.run(&["rebuild-table", "app.p", "--withdraw"]));
        let withdrawn = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&withdrawn), 0);
        assert!(
            stderr(&withdrawn).contains("already applied an approved rewrite"),
            "{}",
            stderr(&withdrawn)
        );
    }
}

fn negatives(server: &str) {
    for case in [
        "bounds",
        "null",
        "cap",
        "cast",
        "sibling",
        "publication",
        "unmanaged",
        "late_dependency",
        "late_rows",
        "rollback",
        "admission",
        "staged",
    ] {
        let own = OwnDatabase::new(server, &format!("rebuild2298b_{case}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.p(k integer NOT NULL,bucket integer NOT NULL,label text) PARTITION BY RANGE(k); CREATE TABLE app.p1 PARTITION OF app.p FOR VALUES FROM(0) TO(10); INSERT INTO app.p VALUES(1,2,'a'),(3,4,'b')",
        );
        let d = Demo::new(&format!("rebuild2298b-{case}"));
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let source = latest_snapshot(connection);
        let source_oid = scalar(connection, "SELECT 'app.p'::regclass::oid::bigint");
        let root: pbps_model::TableName = "app.p".parse().unwrap();
        let mut desired = source.schema.clone();
        desired
            .tables
            .get_mut(&root)
            .unwrap()
            .partition_by
            .as_mut()
            .unwrap()
            .columns = vec!["bucket".into()];
        if case == "bounds" {
            desired
                .tables
                .get_mut(&"app.p1".parse().unwrap())
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .bound = pbps_model::PartitionBound::Range {
                from: vec![pbps_model::BoundDatum::Value("0".into())],
                to: vec![pbps_model::BoundDatum::Value("3".into())],
            };
        }
        if matches!(case, "cast" | "sibling" | "rollback") {
            let parent = desired.tables.get_mut(&root).unwrap();
            parent.partition_by.as_mut().unwrap().columns = vec!["k".into()];
            parent.columns.get_mut("k").unwrap().ty = "bigint".parse().unwrap();
            if case == "rollback" {
                parent.primary_key = Some(pbps_model::PrimaryKey {
                    name: Some("pk_p".into()),
                    columns: vec!["k".into()],
                    storage_parameters: Default::default(),
                });
            }
        }
        if case == "null" {
            on_server(connection, "UPDATE app.p SET label=NULL WHERE k=1");
            desired
                .tables
                .get_mut(&root)
                .unwrap()
                .columns
                .get_mut("label")
                .unwrap()
                .nullable = false;
        }
        write_tree(&d, &desired);
        if matches!(case, "cast" | "sibling" | "rollback") {
            succeeds(d.run(&[
                "convert",
                "app.p.k",
                "--using",
                if case == "cast" {
                    "k::bigint / 0"
                } else if case == "sibling" {
                    "k::bigint + bucket"
                } else {
                    "k::bigint % 2"
                },
            ]));
        }
        approve(
            &d,
            if case == "cap" {
                "1"
            } else if case == "late_rows" {
                "2"
            } else {
                "10"
            },
        );
        if case == "publication" {
            on_server(
                connection,
                "CREATE PUBLICATION pbps_rebuild_publication FOR TABLE app.p",
            );
        }
        if case == "unmanaged" {
            on_server(
                connection,
                "CREATE VIEW app.unmanaged AS SELECT * FROM app.p",
            );
        }
        let path = d.dir.join("rebuild.json");
        std::fs::write(&path, b"existing artifact").unwrap();
        let planned = d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]);
        if matches!(
            case,
            "bounds" | "null" | "cap" | "cast" | "sibling" | "publication" | "unmanaged"
        ) {
            assert_ne!(code(&planned), 0, "{case}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"existing artifact",
                "{case}"
            );
            if !matches!(case, "cast" | "sibling") {
                assert!(
                    stderr(&planned).contains(if case == "unmanaged" {
                        "unmanaged"
                    } else if case == "publication" {
                        "publication"
                    } else {
                        "max_rows"
                    }),
                    "{case}: {}",
                    stderr(&planned)
                );
            }
        } else {
            succeeds(planned);
            if case == "late_dependency" {
                on_server(
                    connection,
                    "CREATE VIEW app.unmanaged AS SELECT * FROM app.p",
                );
            }
            if case == "admission" {
                let original = std::fs::read(&path).unwrap();
                for mutation in 0..3 {
                    let mut plan: pbps_model::SavedPlan =
                        serde_json::from_slice(&original).unwrap();
                    if mutation < 2 {
                        plan.changes.changes.retain(|p| {
                            !if mutation == 0 {
                                matches!(p.change, pbps_model::Change::CopyTableRows(_))
                            } else {
                                matches!(p.change, pbps_model::Change::AnalyzeTableTree(_))
                            }
                        });
                    } else {
                        for p in &mut plan.changes.changes {
                            if let pbps_model::Change::CopyTableRows(c) = &mut p.change {
                                c.max_rows += 1;
                            }
                        }
                    }
                    std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
                    assert_ne!(
                        code(&apply(&d, connection, &path)),
                        0,
                        "edited rebuild {mutation}"
                    );
                }
            } else if case == "staged" {
                let refused = d.run(&["plan", "--db", connection, "--staged"]);
                assert_ne!(code(&refused), 0);
                assert!(
                    stderr(&refused).contains("transactional"),
                    "{}",
                    stderr(&refused)
                );
            } else {
                let result = if case == "late_rows" {
                    concurrent_row(&d, connection, &path)
                } else {
                    apply(&d, connection, &path)
                };
                assert_ne!(
                    code(&result),
                    0,
                    "{case}: {}{}",
                    stdout(&result),
                    stderr(&result)
                );
                assert!(
                    stderr(&result).contains(match case {
                        "late_rows" => "rebuild observes 3 rows",
                        "late_dependency" => "unmanaged",
                        _ => "whole plan was rolled back",
                    }),
                    "{case}: {}",
                    stderr(&result)
                );
            }
        }
        assert_eq!(
            scalar(connection, "SELECT 'app.p'::regclass::oid::bigint"),
            source_oid,
            "{case}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.p WHERE (k,bucket) IN ((1,2),(3,4))"
            ),
            2,
            "{case}"
        );
        assert_eq!(
            latest_snapshot(connection).ids.tables,
            source.ids.tables,
            "{case}"
        );
    }
}

fn concurrent_row(d: &Demo, connection: &str, path: &std::path::Path) -> Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut writer = rt
        .block_on(pbps_db::Conn::connect(
            pbps_db::Driver::Postgres,
            connection,
        ))
        .unwrap();
    rt.block_on(writer.execute("BEGIN; INSERT INTO app.p VALUES(5,6,'writer')"))
        .unwrap();
    let child = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args([
            "apply",
            "--db",
            connection,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(path),
            "--allow",
            "destructive,rename,constraint,narrowing,revoke",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while scalar(
        connection,
        "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'LOCK TABLE%ACCESS EXCLUSIVE%'",
    ) == 0
    {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "apply never waited for its source-tree lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    rt.block_on(writer.execute("COMMIT")).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn rebuild2520_probe_cannot_execute_declared_expressions() {
    for server in [server(), old_server()] {
        let own = OwnDatabase::new(&server, "rebuild2520");
        on_server(own.connection(), "CREATE SCHEMA app");
        let d = Demo::new("rebuild2520");
        let before = "table: app.p\ncolumns:\n  k: {type: integer, nullable: false}\n  bucket: {type: integer, nullable: false}\n  payload: {type: integer}\npartition_by: [k]\npartitions:\n  p1: {from: ['0'], to: ['10']}\n";
        std::fs::write(d.dir.join("schema/app.p.yml"), before).unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", own.connection()]));
        on_server(own.connection(), "INSERT INTO app.p(k,bucket) VALUES(1,1)");
        let desired = before.replace("partition_by: [k]", "partition_by: [bucket]");
        let payload = "0)) PARTITION BY RANGE (bucket); COMMIT; CREATE TABLE public.marker2520(id integer); SELECT ((0";
        let with_default = desired.replace(
            "  payload: {type: integer}",
            &format!("  payload: {{type: integer, default: {payload:?}}}"),
        );
        std::fs::write(d.dir.join("schema/app.p.yml"), with_default).unwrap();
        let refused = d.run(&[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "1",
            "--reason",
            "hardening proof",
        ]);
        assert_ne!(code(&refused), 0);
        assert!(stderr(&refused).contains("app.p"), "{}", stderr(&refused));
        assert!(
            stderr(&refused).contains("plain copied columns"),
            "{}",
            stderr(&refused)
        );
        let payload = "k >= 0); COMMIT; CREATE TABLE public.marker2520(id integer); SELECT (true";
        let after = format!("{desired}checks:\n  allowed: {payload:?}\n");
        std::fs::write(d.dir.join("schema/app.p.yml"), after).unwrap();
        succeeds(d.run(&[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "1",
            "--reason",
            "hardening proof",
        ]));
        d.commit();
        let path = d.dir.join("unsafe.plan.json");
        let out = d.run(&[
            "plan",
            "--db",
            own.connection(),
            "--no-rehearse",
            "--out",
            path.to_str().unwrap(),
        ]);
        assert_eq!(
            scalar(
                own.connection(),
                "SELECT (pg_catalog.to_regclass('public.marker2520') IS NOT NULL)::integer::bigint"
            ),
            0,
            "rebuild probe ran a trailing statement on the target"
        );
        // The factory strips CHECKs from the probe's bare tables. The CHECK
        // belongs to the approved plan, which this test deliberately never applies.
        succeeds(out);
        assert!(path.exists());
        assert_eq!(scalar(own.connection(), "SELECT count(*) FROM app.p"), 1);
        std::fs::write(
            d.dir.join("schema/app.p.yml"),
            format!("{desired}checks:\n  allowed: \"';'::text = ';'::text\"\n"),
        )
        .unwrap();
        approve(&d, "1");
        succeeds(d.run(&[
            "plan",
            "--db",
            own.connection(),
            "--no-rehearse",
            "--out",
            path.to_str().unwrap(),
        ]));
    }
}
