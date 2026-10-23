//! Declared shared types through pull, baseline and the plan refusal seam.
use super::*;
use pbps_model::{ManagedType, TableName};

fn latest_snapshot(db: &str) -> pbps_model::StateSnapshot {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, db)
                .await
                .unwrap();
            let rows = conn
                .query("SELECT state_json FROM public.__pbps_state ORDER BY id DESC LIMIT 1")
                .await
                .unwrap();
            pbps_model::StateSnapshot::read_json(
                rows[0].try_get::<&str>("state_json").unwrap().unwrap(),
            )
            .unwrap()
        })
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_expression_records_converge_without_masking_layout_changes_or_live_drift() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "domain_record1635");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("domain-record1635");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        let positive = d.dir.join("schema/app.positive.type.yml");
        let choice = d.dir.join("schema/app.choice.type.yml");
        let choice_yaml = "domain: app.choice\ntype: text\ndefault: \"'a'\"\nchecks:\n  allowed: \"VALUE IN ('a','b')\"\n";
        std::fs::write(
            &positive,
            "domain: app.positive\ntype: integer\nchecks: {positive: 'VALUE > 0'}\n",
        )
        .unwrap();
        std::fs::write(&choice, choice_yaml).unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("domains.plan.json");
        let plan = || {
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            serde_json::from_slice::<pbps_model::SavedPlan>(&std::fs::read(&path).unwrap()).unwrap()
        };
        assert_eq!(plan().changes.changes.len(), 2);
        succeeds(apply_plan(&d, db, &path, false));
        assert!(plan().changes.is_empty());
        let snapshot = latest_snapshot(db);
        let desired = pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema;
        for (name, definition) in &desired.types {
            assert_eq!(snapshot.declared.types.get(name), Some(definition));
            assert_ne!(
                &snapshot.schema.types[name], definition,
                "engine must respell this fixture"
            );
        }
        assert_eq!(
            snapshot.declared.overlay(&snapshot.schema).types,
            desired.types
        );
        succeeds(d.run(&["verify", "--db", db]));

        // Equivalent SQL with different authored layout remains a slice-4 difference.
        std::fs::write(
            &choice,
            choice_yaml.replace("VALUE IN ('a','b')", "VALUE  IN ( 'a', 'b' )"),
        )
        .unwrap();
        d.commit();
        let changed = d.run(&["plan", "--db", db]);
        assert_ne!(code(&changed), 0);
        assert!(
            stderr(&changed).contains("app.choice")
                && stderr(&changed).contains("domain checks changed"),
            "{}",
            stderr(&changed)
        );
        // Restore the source declaration before the separate offline rename
        // preview, which compares git declarations rather than target state.
        std::fs::write(&choice, choice_yaml).unwrap();
        d.commit();
        std::fs::remove_file(&choice).unwrap();
        std::fs::write(
            d.dir.join("schema/app.selected.type.yml"),
            choice_yaml.replace(
                "domain: app.choice",
                "domain: app.selected\nrenamed_from: app.choice",
            ),
        )
        .unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        assert!(
            plan()
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::RenameType { .. }))
        );
        succeeds(d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "rename",
        ]));
        assert!(plan().changes.is_empty());
        let renamed = latest_snapshot(db);
        assert!(
            !renamed
                .declared
                .types
                .contains_key(&"app.choice".parse().unwrap())
        );
        assert_eq!(
            renamed.declared.types[&"app.selected".parse().unwrap()],
            snapshot.declared.types[&"app.choice".parse().unwrap()]
        );

        // Seal an unapplied write so apply must check drift before statements.
        d.table("table: app.t\ncolumns:\n  value: {type: app.selected}\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        assert!(!plan().changes.is_empty());
        // Both planning and the sealed apply must see the raw catalog drift.
        on_server(
            db,
            "ALTER DOMAIN app.selected DROP CONSTRAINT allowed; ALTER DOMAIN app.selected ADD CONSTRAINT allowed CHECK (VALUE = 'a')",
        );
        let drift = apply_plan(&d, db, &path, false);
        assert_ne!(code(&drift), 0);
        assert!(
            stderr(&drift).contains("no longer the database this plan was computed against")
                && stderr(&drift).contains("plan baseline:"),
            "{}",
            stderr(&drift)
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='app' AND c.relname='t'"
            ),
            0
        );
        let drift = d.run(&["plan", "--db", db]);
        assert_ne!(code(&drift), 0);
        assert!(stderr(&drift).contains("drifted"), "{}", stderr(&drift));
        assert_eq!(latest_snapshot(db).declared, renamed.declared);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn adopted_domains_and_older_states_keep_engine_expression_text() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "domain_adoption1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE DOMAIN app.choice AS text DEFAULT 'a' CONSTRAINT allowed CHECK (VALUE IN ('a','b'))",
        );
        let d = Demo::new("domain-adoption1635");
        succeeds(d.run(&["pull", "--db", db]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt existing domain"]));
        let snapshot = latest_snapshot(db);
        assert!(snapshot.declared.types.is_empty());
        let desired = pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema;
        assert_eq!(desired.types, snapshot.schema.types);
        let path = d.dir.join("adopted.plan.json");
        let clean = || {
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let plan: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(plan.changes.is_empty());
        };
        clean();
        on_server(
            db,
            "UPDATE public.__pbps_state SET state_version = 30, state_json = jsonb_set(state_json::jsonb, '{version}', '30')::text",
        );
        assert_eq!(latest_snapshot(db).version, 30);
        assert!(latest_snapshot(db).declared.types.is_empty());
        clean();
        succeeds(d.run(&["verify", "--db", db]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn declared_type_lifecycles_converge_and_vetting_is_atomic() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "lifecycle1635");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("lifecycle1635");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        let mood = d.dir.join("schema/app.mood.type.yml");
        let phone = d.dir.join("schema/app.phone.type.yml");
        std::fs::write(&mood, "enum: app.mood\nvalues: [sad, ok, happy]\n").unwrap();
        std::fs::write(&phone, "domain: app.phone\ntype: varchar(20)\ncollation: pg_catalog.\"C\"\nnot_null: true\ndefault: \"'0'\"\nchecks:\n  phone_digits: \"VALUE ~ '^[0-9+ -]+$'\"\n").unwrap();
        let wrapped = d.dir.join("schema/app.mood_domain.type.yml");
        std::fs::write(&wrapped, "domain: app.mood_domain\ntype: app.mood\n").unwrap();
        d.table("table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  mood: {type: app.mood}\n  phone: {type: app.phone}\nprimary_key: [id]\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("lifecycle.plan.json");
        let plan = || {
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            serde_json::from_slice::<pbps_model::SavedPlan>(&std::fs::read(&path).unwrap()).unwrap()
        };
        let first = plan();
        assert_eq!(first.declared_type_pins.len(), 3);
        assert!(
            first
                .declared_type_pins
                .iter()
                .all(|pin| pin.before.is_none())
        );
        let table_at = first
            .changes
            .changes
            .iter()
            .position(|p| matches!(p.change, pbps_model::Change::CreateTable { .. }))
            .unwrap();
        assert!(
            first
                .changes
                .changes
                .iter()
                .take(table_at)
                .filter(|p| matches!(p.change, pbps_model::Change::CreateType { .. }))
                .count()
                == 3,
            "{:?}",
            first.changes
        );
        succeeds(apply_plan(&d, db, &path, false));
        assert!(plan().changes.is_empty());
        let pulled = Demo::new("lifecycle-repull1635");
        succeeds(pulled.run(&["pull", "--db", db]));
        let readback = pbps_load::load_schema_dir(&pulled.dir.join("schema"))
            .unwrap()
            .schema;
        for pin in &first.declared_type_pins {
            assert!(pbps_pg::type_refs::declared::matches_declaration(
                &pin.definition,
                &readback.types[&pin.name]
            ));
        }
        on_server(db, "INSERT INTO app.t VALUES (7, 'ok', '123')");
        std::fs::remove_file(&mood).unwrap();
        std::fs::write(
            d.dir.join("schema/app.feeling.type.yml"),
            "enum: app.feeling\nrenamed_from: app.mood\nvalues: [sad, ok, happy]\n",
        )
        .unwrap();
        std::fs::write(&wrapped, "domain: app.mood_domain\ntype: app.feeling\n").unwrap();
        d.table("table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  mood: {type: app.feeling}\n  phone: {type: app.phone}\nprimary_key: [id]\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let renamed = plan();
        assert!(
            renamed
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::RenameType { .. }))
        );
        assert!(
            !renamed
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::AlterColumnType { .. }))
        );
        succeeds(d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "rename",
        ]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE id=7 AND mood='ok' AND phone='123'"
            ),
            1
        );
        assert!(plan().changes.is_empty());

        // Before-definition drift is named before any statement can run.
        on_server(db, "ALTER TYPE app.feeling ADD VALUE 'manual'");
        let out = apply_plan(&d, db, &path, false);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("app.feeling"), "{}", stderr(&out));
        on_server(db, "DROP SCHEMA app CASCADE");

        // A new domain's code is bound and vetted in its creating transaction.
        let bad = Demo::new("bad-domain1635");
        bad.commit();
        succeeds(bad.run(&["baseline", "--db", db, "--reason", "empty database"]));
        on_server(
            db,
            "CREATE SCHEMA app; CREATE FUNCTION app.unreviewed(integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 > 0",
        );
        std::fs::write(
            bad.dir.join("schema/app.bad.type.yml"),
            "domain: app.bad\ntype: integer\nchecks:\n  unsafe: app.unreviewed(VALUE)\n",
        )
        .unwrap();
        bad.table("table: app.t\ncolumns:\n  value: {type: app.bad}\n");
        succeeds(bad.run(&["plan", "--no-dev"]));
        bad.commit();
        let bad_path = bad.dir.join("bad.plan.json");
        succeeds(bad.run(&["plan", "--db", db, "--out", bad_path.to_str().unwrap()]));
        let out = apply_plan(&bad, db, &bad_path, false);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("app.bad"), "{}", stderr(&out));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_type WHERE typname='bad'"
            ),
            0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_class WHERE relname='t'"
            ),
            0
        );
        // Immediate bootstrap shares the same transactional post-DDL vet.
        let boot_db = OwnDatabase::new(&server, "bootstrap_declared1635");
        let boot = Demo::new("bootstrap-declared1635");
        on_server(boot_db.connection(), "CREATE SCHEMA app");
        std::fs::write(
            boot.dir.join("schema/app.mood.type.yml"),
            "enum: app.mood\nvalues: [ok]\n",
        )
        .unwrap();
        std::fs::write(
            boot.dir.join("schema/app.positive.type.yml"),
            "domain: app.positive\ntype: integer\ndefault: '1'\nchecks: {positive: 'VALUE > 0'}\n",
        )
        .unwrap();
        boot.table(
            "table: app.t\ncolumns:\n  mood: {type: app.mood}\n  value: {type: app.positive}\n",
        );
        succeeds(boot.run(&["plan", "--no-dev"]));
        boot.commit();
        succeeds(boot.run(&["bootstrap", "--db", boot_db.connection()]));
        succeeds(boot.run(&["verify", "--db", boot_db.connection()]));
    }
}

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
                    message.contains("app.mood") && message.contains("pull cannot record a drop"),
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
fn pulled_type_files_converge_and_type_attribute_changes_are_refused_without_sql() {
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
                message.contains(&name.shown().to_string()) && message.contains("slice 3"),
                "{message}"
            );
            assert!(!plan_path.exists());
            std::fs::write(&mood_path, &mood_text).unwrap();
            std::fs::write(&phone_path, &phone_text).unwrap();
        }
        // Removed declarations still require recorded human drop intent.
        std::fs::remove_file(&mood_path).unwrap();
        let out = d.run(&["plan", "--db", db, "--out", plan_path.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("app.mood"), "{}", stderr(&out));
        assert!(!plan_path.exists());
        std::fs::write(&mood_path, &mood_text).unwrap();
        // Offline previews can now mint identities for declared creations;
        // the connected plan supplies the applicable, checksum-pinned evidence.
        let missing = d.dir.join("schema/app.missing.type.yml");
        std::fs::write(&missing, "enum: app.missing\nvalues: [x]\n").unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_enum e JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE n.nspname = 'app' AND t.typname = 'mood'"
            ),
            2
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn pg_type_names_refuse_row_types_and_admit_indexes_and_sequences_before_emission() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for (kind, holder, collision) in [
            ("table", "CREATE TABLE app.x (id integer)", true),
            (
                "partition",
                "CREATE TABLE app.x (id integer) PARTITION BY RANGE(id)",
                true,
            ),
            ("view", "CREATE VIEW app.x AS SELECT 1 AS id", true),
            (
                "matview",
                "CREATE MATERIALIZED VIEW app.x AS SELECT 1 AS id",
                true,
            ),
            ("composite", "CREATE TYPE app.x AS (id integer)", true),
            (
                "index",
                "CREATE TABLE app.holder (id integer); CREATE INDEX x ON app.holder(id)",
                false,
            ),
            ("sequence", "CREATE SEQUENCE app.x", false),
        ] {
            for domain in [false, true] {
                let own = OwnDatabase::new(&server, "type_slot1635");
                let db = own.connection();
                on_server(db, "CREATE SCHEMA app");
                let d = Demo::new("type-slot1635");
                d.commit();
                succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
                on_server(db, holder);
                std::fs::write(
                    d.dir.join("schema/app.x.type.yml"),
                    if domain {
                        "domain: app.x\ntype: integer\n"
                    } else {
                        "enum: app.x\nvalues: [ok]\n"
                    },
                )
                .unwrap();
                succeeds(d.run(&["plan", "--no-dev"]));
                d.commit();
                let path = d.dir.join("slot.plan.json");
                let sql = d.dir.join("slot.sql");
                let out = d.run(&[
                    "plan",
                    "--db",
                    db,
                    "--out",
                    path.to_str().unwrap(),
                    "--sql",
                    sql.to_str().unwrap(),
                ]);
                if collision {
                    assert_ne!(code(&out), 0, "{kind} domain={domain}: {}", stdout(&out));
                    assert!(stderr(&out).contains("app.x"), "{kind}: {}", stderr(&out));
                    assert!(
                        !path.exists() && !sql.exists(),
                        "a refused plan emitted an artifact"
                    );
                } else {
                    succeeds(out);
                    succeeds(apply_plan(&d, db, &path, false));
                }
            }
        }
        // The inverse claim must also see a live pg_type without a pg_class.
        let own = OwnDatabase::new(&server, "inverse_slot1635");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE TYPE app.x AS ENUM ('ok')");
        let d = Demo::new("inverse-slot1635");
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "leave external type unmanaged",
        ]));
        d.table("table: app.x\ncolumns:\n  id: {type: integer}\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("inverse.plan.json");
        let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("app.x"), "{}", stderr(&out));
        assert!(!path.exists());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn type_drops_follow_the_last_table_and_table_names_can_handoff_to_types() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for handoff in [false, true] {
            let own = OwnDatabase::new(&server, "type_drop1635");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('ok'); CREATE TABLE app.t (v app.mood)",
            );
            // PostgreSQL itself refuses a type still used by a column.
            assert!(try_on_server(db, "DROP TYPE app.mood").is_err());
            let d = Demo::new("type-drop1635");
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt table and type"]));
            std::fs::remove_file(d.dir.join("schema/app.t.yml")).unwrap();
            std::fs::remove_file(d.dir.join("schema/app.mood.type.yml")).unwrap();
            if handoff {
                std::fs::write(
                    d.dir.join("schema/app.t.type.yml"),
                    "enum: app.t\nvalues: [ok]\n",
                )
                .unwrap();
            }
            let absent_intent = d.run(&["plan", "--no-dev"]);
            assert_ne!(code(&absent_intent), 0);
            succeeds(d.run(&[
                "intent",
                "--drop-table",
                "app.t",
                "table retired",
                "--drop-type",
                "app.mood",
                "type retired",
            ]));
            d.commit();
            let path = d.dir.join("drop.plan.json");
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let table_at = saved
                .changes
                .changes
                .iter()
                .position(|p| matches!(p.change, pbps_model::Change::DropTable { .. }))
                .unwrap();
            let type_at = saved
                .changes
                .changes
                .iter()
                .position(|p| matches!(p.change, pbps_model::Change::DropType { .. }))
                .unwrap();
            assert!(table_at < type_at);
            if handoff {
                let claim = saved
                    .changes
                    .changes
                    .iter()
                    .position(|p| matches!(p.change, pbps_model::Change::CreateType { .. }))
                    .unwrap();
                assert!(table_at < claim);
            }
            let out = apply_plan(&d, db, &path, false);
            assert_ne!(code(&out), 0, "a destructive change needs approval");
            succeeds(d.run(&[
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&path),
                "--allow",
                "destructive",
            ]));
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let after: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(after.changes.is_empty());
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_type WHERE typname='mood'"
                ),
                0
            );
            if handoff {
                // Exercise the opposite class inversion: DropType is class
                // 18, but its slot release must precede a class-7 table.
                std::fs::remove_file(d.dir.join("schema/app.t.type.yml")).unwrap();
                d.table("table: app.t\ncolumns:\n  id: {type: integer}\n");
                succeeds(d.run(&[
                    "intent",
                    "--drop-type",
                    "app.t",
                    "return the name to a table",
                ]));
                d.commit();
                succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
                let returned: pbps_model::SavedPlan =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert!(matches!(
                    returned.changes.changes[0].change,
                    pbps_model::Change::DropType { .. }
                ));
                succeeds(d.run(&[
                    "apply",
                    "--db",
                    db,
                    "--plan",
                    path.to_str().unwrap(),
                    "--checksum",
                    &plan_checksum(&path),
                    "--allow",
                    "destructive",
                ]));
                succeeds(d.run(&["verify", "--db", db]));
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn staged_type_creation_vets_before_commit_and_resume_accepts_a_vetted_step() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "staged_type1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE FUNCTION app.unsafe(integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 > 0",
        );
        let d = Demo::new("staged-type1635");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty managed scope"]));
        std::fs::write(
            d.dir.join("schema/app.bad.type.yml"),
            "domain: app.bad\ntype: integer\nchecks:\n  unsafe: app.unsafe(VALUE)\n",
        )
        .unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("staged.plan.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            db,
            "--staged",
            "--out",
            path.to_str().unwrap(),
        ]));
        let out = apply_plan(&d, db, &path, true);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("app.bad"), "{}", stderr(&out));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_type WHERE typname='bad'"
            ),
            0,
            "vetting must precede the creating step's commit"
        );

        // The process-loss harness can stop after a creating statement's
        // checkpoint even when this one-change staged plan has no next SQL.
        #[cfg(target_os = "linux")]
        {
            let own = OwnDatabase::new(&server, "resume_created1635");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app");
            let d = Demo::new("resume-created1635");
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
            std::fs::write(d.dir.join("schema/app.good.type.yml"), "domain: app.good\ntype: integer\nnot_null: true\ndefault: '1'\nchecks: {positive: 'VALUE > 0'}\n").unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let path = d.dir.join("created.plan.json");
            succeeds(d.run(&[
                "plan",
                "--db",
                db,
                "--staged",
                "--out",
                path.to_str().unwrap(),
            ]));
            let checksum = plan_checksum(&path);
            let args = [
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--staged",
            ];
            let checkpoint = super::abrupt_recovery::interrupt_after_type_checkpoint(db, &d, &args);
            assert!(
                checkpoint
                    .schema
                    .types
                    .contains_key(&"app.good".parse().unwrap())
            );
            assert!(
                checkpoint
                    .ids
                    .types
                    .values()
                    .any(|name| name == &"app.good".parse().unwrap())
            );
            let mut resume = args.to_vec();
            resume.push("--resume");
            succeeds(d.run(&resume));
            succeeds(d.run(&["verify", "--db", db]));
        }

        let own = OwnDatabase::new(&server, "resume_type1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE SCHEMA moved; CREATE TYPE app.old AS ENUM ('ok'); CREATE TABLE app.t (v app.old); INSERT INTO app.t VALUES ('ok')",
        );
        let d = Demo::new("resume-type1635");
        succeeds(d.run(&["pull", "--db", db]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt enum and dependent table",
        ]));
        std::fs::remove_file(d.dir.join("schema/app.old.type.yml")).unwrap();
        std::fs::write(
            d.dir.join("schema/moved.final.type.yml"),
            "enum: moved.final\nrenamed_from: app.old\nvalues: [ok]\n",
        )
        .unwrap();
        d.table("table: app.t\ncolumns:\n  v: {type: moved.final}\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("resume.plan.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            db,
            "--staged",
            "--out",
            path.to_str().unwrap(),
        ]));
        on_server(
            db,
            "CREATE FUNCTION app.stop_rename() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS (SELECT 1 FROM pg_type WHERE typnamespace='moved'::regnamespace AND typname='old') THEN RAISE EXCEPTION 'interrupt after the vetted schema move'; END IF; END $$; CREATE EVENT TRIGGER stop_rename ON ddl_command_start WHEN TAG IN ('ALTER TYPE') EXECUTE FUNCTION app.stop_rename()",
        );
        let apply = |resume: bool| {
            let checksum = plan_checksum(&path);
            let mut args = vec![
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--allow",
                "rename",
                "--staged",
            ];
            if resume {
                args.push("--resume");
            }
            d.run(&args)
        };
        let interrupted = apply(false);
        assert_ne!(code(&interrupted), 0);
        assert!(
            stderr(&interrupted).contains("interrupt after the vetted schema move"),
            "{}",
            stderr(&interrupted)
        );
        assert_eq!(latest_snapshot(db).staged.as_ref().unwrap().completed, 1);
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_type WHERE typnamespace='moved'::regnamespace AND typname='old'"
            ),
            1
        );
        on_server(
            db,
            "DROP EVENT TRIGGER stop_rename; DROP FUNCTION app.stop_rename()",
        );
        succeeds(apply(true));
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.t WHERE v='ok'"),
            1
        );
        succeeds(d.run(&["verify", "--db", db]));
    }
}

#[test]
fn declared_type_bootstrap_scripts_admit_enums_and_refuse_unvetted_domain_code() {
    let d = Demo::new("type-bootstrap1635");
    std::fs::write(
        d.dir.join("schema/app.mood.type.yml"),
        "enum: app.mood\nvalues: [ok]\n",
    )
    .unwrap();
    d.table("table: app.t\ncolumns:\n  v: {type: app.mood}\n");
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let script = d.dir.join("bootstrap.sql");
    succeeds(d.run(&["bootstrap", "--sql", script.to_str().unwrap()]));
    let sql = std::fs::read_to_string(&script).unwrap();
    assert!(sql.find("CREATE TYPE").unwrap() < sql.find("CREATE TABLE \"app\".\"t\"").unwrap());
    for (name, clause) in [
        ("with_default", "default: '1'\n"),
        ("with_check", "checks: {positive: 'VALUE > 0'}\n"),
    ] {
        std::fs::write(
            d.dir.join(format!("schema/app.{name}.type.yml")),
            format!("domain: app.{name}\ntype: integer\n{clause}"),
        )
        .unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let before = std::fs::read(&script).unwrap();
        let out = d.run(&["bootstrap", "--sql", script.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("DEC-2181.1"), "{}", stderr(&out));
        assert_eq!(std::fs::read(&script).unwrap(), before);
    }
}

#[test]
fn fmt_keeps_a_pending_type_rename_and_removes_only_absorbed_intent() {
    let d = Demo::new("type-fmt1635");
    let old = d.dir.join("schema/app.old.type.yml");
    let new = d.dir.join("schema/app.new.type.yml");
    std::fs::write(&old, "enum: app.old\nvalues: [ok]\n").unwrap();
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    std::fs::remove_file(&old).unwrap();
    let pending = "enum: app.new\nrenamed_from: app.old\nvalues: [ok]\n";
    std::fs::write(&new, pending).unwrap();
    let before = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    succeeds(d.run(&["fmt"]));
    assert!(
        std::fs::read_to_string(&new)
            .unwrap()
            .contains("renamed_from: app.old")
    );
    assert_eq!(
        std::fs::read(d.dir.join("schema.ids.json")).unwrap(),
        before
    );
    succeeds(d.run(&["plan", "--no-dev"]));
    succeeds(d.run(&["fmt"]));
    assert!(
        !std::fs::read_to_string(&new)
            .unwrap()
            .contains("renamed_from")
    );
    // An unknown target is unresolved intent; formatting neither guesses
    // identity nor writes an ids entry even when the source is misspelled.
    let future = d.dir.join("schema/app.future.type.yml");
    let assigned = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    std::fs::write(
        &future,
        "enum: app.future\nrenamed_from: app.missing\nvalues: [ok]\n",
    )
    .unwrap();
    succeeds(d.run(&["fmt"]));
    assert!(
        std::fs::read_to_string(&future)
            .unwrap()
            .contains("renamed_from: app.missing")
    );
    assert_eq!(
        std::fs::read(d.dir.join("schema.ids.json")).unwrap(),
        assigned
    );
}
