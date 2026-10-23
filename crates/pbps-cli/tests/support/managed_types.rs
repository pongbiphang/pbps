//! Declared shared types through pull, baseline and the plan refusal seam.
use super::*;
use pbps_model::{ManagedType, TableName};

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn missing_managed_types_refuse_pull_before_any_project_write() {
    fn tree(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.insert(path.clone(), None);
                files.extend(tree(&path));
            } else {
                files.insert(path.clone(), Some(std::fs::read(&path).unwrap()));
            }
        }
        files
    }
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "type_missing1635");
        let db = own.connection();
        let major = scalar(
            db,
            "SELECT current_setting('server_version_num')::bigint / 10000",
        );
        for (case, change, unrepresentable) in [
            ("dropped", "DROP TYPE app.mood", None),
            (
                "renamed",
                "ALTER TYPE app.mood RENAME TO _mood",
                Some("app._mood"),
            ),
            (
                "unrepresentable",
                "DROP TYPE app.mood; CREATE TYPE app._base AS ENUM ('ok');
                 CREATE DOMAIN app.mood AS app._base",
                Some("app._base"),
            ),
        ] {
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('ok');
                 CREATE TABLE app.t (id integer)",
            );
            let d = Demo::new(&format!("type-missing1635-{case}"));
            succeeds(d.run(&["pull", "--db", db]));
            let before = tree(&d.dir);
            on_server(db, change);
            // An unrelated new table would write a new file, while the changed
            // table and lost type would overwrite and remove existing files.
            on_server(
                db,
                "CREATE TABLE app.new (id integer); ALTER TABLE app.t ADD added integer",
            );
            for force in [true, false] {
                let mut args = vec!["pull", "--db", db];
                if force {
                    args.push("--force");
                }
                let out = d.run(&args);
                let message = stderr(&out);
                assert_ne!(code(&out), 0, "{case}: {}", stdout(&out));
                assert!(
                    message.contains("app.mood")
                        && message.contains("type lifecycle arrives in later slices of #1635"),
                    "{case}, force={force}: {message}"
                );
                if let Some(name) = unrepresentable {
                    assert!(message.contains(name), "{message}");
                    assert!(message.contains("cannot be represented"), "{message}");
                }
                if case == "unrepresentable" {
                    assert!(
                        message.lines().any(|line| {
                            line.contains("type `app.mood`")
                                && line.contains("is unrepresentable")
                                && line.contains("_base")
                        }),
                        "{message}"
                    );
                    assert!(
                        !message.contains("is absent from the pulled schema"),
                        "{message}"
                    );
                }
                assert_eq!(
                    tree(&d.dir),
                    before,
                    "{case}, force={force}: changed project tree"
                );
                eprintln!("PostgreSQL {major}: {case}, force={force}: {message}");
            }
            on_server(db, "DROP SCHEMA app CASCADE");
            // A readable definition at the same name still preserves its uid.
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('ok'); CREATE TABLE app.t (id integer)",
            );
            let out = d.run(&["pull", "--db", db]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("pull would overwrite"),
                "{}",
                stderr(&out)
            );
            assert_eq!(tree(&d.dir), before);
            succeeds(d.run(&["pull", "--db", db, "--force"]));
            assert_eq!(tree(&d.dir), before);
            on_server(db, "DROP SCHEMA app CASCADE");
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn unrepresentable_types_leave_unrelated_plans_usable_and_declared_dependents_refused() {
    use pbps_db::catalog::LimitationTarget;
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "type_omission1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app;
             CREATE TYPE app._mood AS ENUM ('ok');
             CREATE DOMAIN app.positive AS integer NOT NULL
                 CONSTRAINT positive_check CHECK (VALUE > 0);
             CREATE TABLE app.t (id integer);
             CREATE TABLE app.bad (mood text)",
        );
        let read = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, db)
                        .await
                        .unwrap();
                    pbps_pg::catalog::introspect(&mut conn).await.unwrap()
                })
        };
        let first = read();
        assert_eq!(first.schema.types.len(), 1);
        assert!(first.limitations.iter().any(|l| {
            l.target == LimitationTarget::Type(TableName::new("app", "_mood"))
                && l.detail.contains("_mood")
        }));
        // PG 18 also stores a NOT NULL constraint row; the actual inventory
        // excludes it and uses typnotnull, just as on PG 16 (DEC-1635.1).
        assert!(matches!(
            &first.schema.types[&"app.positive".parse().unwrap()],
            ManagedType::Domain { not_null: true, checks, .. }
                if checks.len() == 1 && checks.contains_key("positive_check")
        ));
        let not_null_rows = scalar(
            db,
            "SELECT count(*)::bigint FROM pg_catalog.pg_constraint WHERE contypid='app.positive'::regtype AND contype='n'",
        );
        let major = scalar(
            db,
            "SELECT current_setting('server_version_num')::bigint / 10000",
        );
        assert_eq!(not_null_rows, i64::from(major >= 17));
        eprintln!(
            "PostgreSQL {major}: domain NOT NULL rows={not_null_rows}; pulled not_null=true with one CHECK; _mood is a named limitation"
        );

        let d = Demo::new("type-omission1635");
        let out = d.run(&["pull", "--db", db]);
        assert!(stderr(&out).contains("_mood"), "{}", stderr(&out));
        succeeds(out);
        assert!(!d.dir.join("schema/app._mood.type.yml").exists());
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt representable objects",
        ]));
        let policy_plan = d.dir.join("unmanaged-type.plan.json");
        for policy in ["error", "warn", "ignore"] {
            std::fs::write(
                d.dir.join("pbps.yml"),
                format!("dialect: postgres\nunmanaged: {policy}\n"),
            )
            .unwrap();
            d.commit();
            let out = d.run(&["plan", "--db", db, "--out", policy_plan.to_str().unwrap()]);
            let message = stderr(&out);
            if policy == "error" {
                assert_ne!(code(&out), 0, "{message}");
                assert!(message.contains("`unmanaged: error`"), "{message}");
                assert!(message.contains("type app._mood ("), "{message}");
                assert!(!policy_plan.exists());
            } else {
                assert_eq!(code(&out), 0, "{message}");
                assert_eq!(
                    message.contains("are not declared and are left alone"),
                    policy == "warn",
                    "{message}"
                );
                if policy == "warn" {
                    assert!(
                        message.lines().any(|line| {
                            line.contains("are not declared and are left alone")
                                && line.contains("type app._mood (")
                        }),
                        "{message}"
                    );
                }
                assert!(policy_plan.exists());
                std::fs::remove_file(&policy_plan).unwrap();
            }
            let out = d.run(&["verify", "--db", db, "--format", "json"]);
            let message = stderr(&out);
            let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
            assert_eq!(
                code(&out),
                if policy == "error" { 2 } else { 0 },
                "{report}: {message}"
            );
            assert_eq!(
                report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| {
                        finding["id"] == "state.unmanaged-refused"
                            && finding["message"]
                                .as_str()
                                .is_some_and(|message| message.contains("type app._mood ("))
                    }),
                policy == "error",
                "{report}: {message}"
            );
            assert_eq!(
                message.contains("are not declared and are left alone"),
                policy == "warn",
                "{message}"
            );
            if policy == "warn" {
                assert!(
                    message.lines().any(|line| {
                        line.contains("are not declared and are left alone")
                            && line.contains("type app._mood (")
                    }),
                    "{message}"
                );
            }
            eprintln!(
                "PostgreSQL {major}: unmanaged {policy}: plan and verify name/refuse/warn/ignore app._mood as configured"
            );
        }
        d.table("table: app.t\ncolumns:\n  id: {type: integer}\n  added: {type: integer}\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let plan = d.dir.join("omission.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", plan.to_str().unwrap()]));
        succeeds(apply_plan(&d, db, &plan, false));
        succeeds(d.run(&["verify", "--db", db]));
        std::fs::remove_file(&plan).unwrap();

        // #1523 cannot round-trip this reference. Its existing whole-table
        // omission remains a relation limitation, never a dangling column.
        on_server(
            db,
            "ALTER TABLE app.bad ALTER COLUMN mood TYPE app._mood USING NULL::app._mood",
        );
        let pulled = read();
        assert!(
            !pulled
                .schema
                .tables
                .contains_key(&TableName::new("app", "bad"))
        );
        assert!(pulled.limitations.iter().any(|l| {
            l.target == LimitationTarget::Relation(TableName::new("app", "bad"))
                && l.detail.contains("_mood")
        }));
        let omitted = Demo::new("type-omission-pull1635");
        succeeds(omitted.run(&["pull", "--db", db]));
        assert!(!omitted.dir.join("schema/app.bad.yml").exists());
        let out = d.run(&["plan", "--db", db, "--out", plan.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.bad") && stderr(&out).contains("partial schema"),
            "{}",
            stderr(&out)
        );
        assert!(!plan.exists());
        on_server(
            db,
            "ALTER TABLE app.bad ALTER COLUMN mood TYPE text USING mood::text",
        );

        // A valid declared name can have an unrepresentable live definition.
        // Keep its identity in the managed refusal, instead of calling it gone.
        on_server(
            db,
            "DROP DOMAIN app.positive; CREATE DOMAIN app.positive AS app._mood",
        );
        let out = d.run(&["plan", "--db", db, "--out", plan.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        let message = stderr(&out);
        assert!(
            message.contains("app.positive")
                && message.contains("cannot be represented")
                && message.contains("partial schema"),
            "{message}"
        );
        assert!(!message.contains("absent in the target"), "{message}");
        assert!(!plan.exists());

        // #1523 can still hold a reference whose name is representable even
        // when its definition cannot be a type file. It stays explicitly
        // external, with a prerequisite pin and the named type limitation.
        on_server(db, "CREATE TABLE app.external_ref (mood app.positive)");
        let external = Demo::new("type-omission-external1635");
        std::fs::write(
            external.dir.join("schema/app.external_ref.yml"),
            "table: app.external_ref\ncolumns:\n  mood: {type: app.positive}\n",
        )
        .unwrap();
        succeeds(external.run(&["plan", "--no-dev"]));
        external.commit();
        succeeds(external.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "keep the existing external reference",
        ]));
        let external_plan = external.dir.join("external.plan.json");
        let out = external.run(&["plan", "--db", db, "--out", external_plan.to_str().unwrap()]);
        assert!(stderr(&out).contains("app.positive"), "{}", stderr(&out));
        succeeds(out);
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(external_plan).unwrap()).unwrap();
        assert!(saved.changes.is_empty());
        assert!(saved.ids.types.is_empty());
        assert!(
            saved
                .type_pins
                .iter()
                .any(|p| p.schema == "app" && p.name == "positive")
        );
        on_server(db, "DROP SCHEMA app CASCADE");
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_namespace WHERE nspname='app'"
            ),
            0
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn pulled_type_files_converge_and_each_type_change_is_refused_without_sql() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "managed1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad', 'ok');
            CREATE DOMAIN app.phone AS varchar(20) DEFAULT '0'
                CONSTRAINT phone_digits CHECK (VALUE ~ '^[0-9+ -]+$');
            CREATE TABLE app.t (mood app.mood, phone app.phone)",
        );
        let d = Demo::new("managed1635");
        succeeds(d.run(&["pull", "--db", db]));
        let mood_path = d.dir.join("schema/app.mood.type.yml");
        let phone_path = d.dir.join("schema/app.phone.type.yml");
        assert!(mood_path.is_file() && phone_path.is_file());
        let mood_text = std::fs::read_to_string(&mood_path).unwrap();
        let phone_text = std::fs::read_to_string(&phone_path).unwrap();
        let ids_text = std::fs::read_to_string(d.dir.join("schema.ids.json")).unwrap();
        let ids: pbps_model::IdsFile = serde_json::from_str(&ids_text).unwrap();
        assert_eq!(ids.types.len(), 2);
        let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
        assert_eq!(
            loaded.schema.tables[&"app.t".parse().unwrap()]
                .columns
                .len(),
            2
        );
        succeeds(d.run(&["fmt", "--check"]));
        // Recreate with SQL supplied by the fixture: slice one emits no type DDL.
        on_server(
            db,
            "DROP SCHEMA app CASCADE; CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad', 'ok');
            CREATE DOMAIN app.phone AS varchar(20) DEFAULT '0'
                CONSTRAINT phone_digits CHECK (VALUE ~ '^[0-9+ -]+$');
            CREATE TABLE app.t (mood app.mood, phone app.phone)",
        );
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        assert_eq!(std::fs::read_to_string(&mood_path).unwrap(), mood_text);
        assert_eq!(std::fs::read_to_string(&phone_path).unwrap(), phone_text);
        assert_eq!(
            std::fs::read_to_string(d.dir.join("schema.ids.json")).unwrap(),
            ids_text
        );
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt existing shared types",
        ]));
        let plan_path = d.dir.join("type.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", plan_path.to_str().unwrap()]));
        let plan: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&plan_path).unwrap()).unwrap();
        assert!(plan.changes.is_empty());
        std::fs::remove_file(&plan_path).unwrap();
        // First-time type adoption must also survive staged checkpoints for
        // an unrelated column change, even when no column uses the type.
        on_server(db, "CREATE TYPE app.unused AS ENUM ('x')");
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        d.commit();
        let table_path = d.dir.join("schema/app.t.yml");
        let mut table = pbps_load::load_table_file(&table_path).unwrap();
        table.table.columns.insert(
            "added".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        std::fs::write(
            &table_path,
            pbps_load::render_partitioned(&table.name, &table.table, &[], &[], None),
        )
        .unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&[
            "plan",
            "--db",
            db,
            "--staged",
            "--out",
            plan_path.to_str().unwrap(),
        ]));
        succeeds(apply_plan(&d, db, &plan_path, true));
        succeeds(d.run(&["verify", "--db", db]));
        std::fs::remove_file(&plan_path).unwrap();
        let mood = pbps_load::load_type_str(&mood_path, &mood_text).unwrap();
        let phone = pbps_load::load_type_str(&phone_path, &phone_text).unwrap();
        let mut variants = Vec::new();
        for labels in [
            vec!["sad", "ok", "happy"],
            vec!["sad"],
            vec!["ok", "sad"],
            vec!["blue", "ok"],
        ] {
            variants.push((
                mood_path.clone(),
                mood.name.clone(),
                ManagedType::Enum {
                    values: labels.into_iter().map(str::to_owned).collect(),
                },
            ));
        }
        for field in 0..8 {
            let mut changed = phone.definition.clone();
            let ManagedType::Domain {
                base,
                not_null,
                default,
                checks,
                collation,
            } = &mut changed
            else {
                unreachable!()
            };
            match field {
                0 => *base = "text".parse().unwrap(),
                1 => *not_null = true,
                2 => *default = None,
                3 => *default = Some("'1'".into()),
                4 => checks.clear(),
                5 => {
                    checks.insert("phone_digits".into(), "VALUE <> 'x'".into());
                }
                6 => {
                    checks.insert("extra".into(), "VALUE <> 'x'".into());
                }
                _ => *collation = Some("pg_catalog.\"C\"".parse().unwrap()),
            }
            variants.push((phone_path.clone(), phone.name.clone(), changed));
        }
        for (path, name, definition) in variants {
            std::fs::write(&path, pbps_load::render_type(&name, &definition)).unwrap();
            let out = d.run(&["plan", "--db", db, "--out", plan_path.to_str().unwrap()]);
            assert_ne!(code(&out), 0, "{}", stdout(&out));
            let message = stderr(&out);
            assert!(
                message.contains(&name.shown().to_string())
                    && message.contains("type lifecycle arrives in later slices"),
                "{message}"
            );
            assert!(!plan_path.exists());
            std::fs::write(&mood_path, &mood_text).unwrap();
            std::fs::write(&phone_path, &phone_text).unwrap();
        }
        // A removed declaration stays in the recorded scope and cannot drop a type.
        std::fs::remove_file(&mood_path).unwrap();
        let out = d.run(&["plan", "--db", db, "--out", plan_path.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.mood")
                && stderr(&out).contains("type lifecycle arrives in later slices"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            stderr(&out)
                .matches("type lifecycle arrives in later slices")
                .count(),
            1,
            "{}",
            stderr(&out)
        );
        assert!(!plan_path.exists());
        std::fs::write(&mood_path, &mood_text).unwrap();
        // A newly declared missing type must be refused before its reference gate.
        let missing = d.dir.join("schema/app.missing.type.yml");
        std::fs::write(&missing, "enum: app.missing\nvalues: [x]\n").unwrap();
        let out = d.run(&["plan", "--no-dev"]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.missing")
                && stderr(&out).contains("type lifecycle arrives in later slices"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_enum e JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE n.nspname = 'app' AND t.typname = 'mood'"
            ),
            2
        );
    }
}
