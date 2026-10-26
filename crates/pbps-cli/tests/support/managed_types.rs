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

        // Equivalent SQL with different authored layout remains a difference:
        // the check is dropped and added again, in place (DEC-1635.8).
        std::fs::write(
            &choice,
            choice_yaml.replace("VALUE IN ('a','b')", "VALUE  IN ( 'a', 'b' )"),
        )
        .unwrap();
        d.commit();
        assert_eq!(halves(&plan()), ["Loosen", "Tighten"]);
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
        // Enum label changes are executable (DEC-1635.6, DEC-1635.7) and so
        // are a domain's NOT NULL, default and checks (DEC-1635.8); only the
        // base type and the collation below still refuse.
        let _ = &mood;
        for field in [0, 7] {
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
                message.contains(&name.shown().to_string()) && message.contains("slice 4b"),
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
fn a_type_move_applies_after_dropping_its_intermediate_holder_and_converges() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "type_move_release1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE SCHEMA dst; CREATE TYPE app.a AS ENUM ('moving'); CREATE TYPE dst.a AS ENUM ('holder')",
        );
        let d = Demo::new("type-move-release1635");
        succeeds(d.run(&["pull", "--db", db]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt both type holders",
        ]));
        let moving_oid = scalar(db, "SELECT 'app.a'::regtype::oid::bigint");
        std::fs::remove_file(d.dir.join("schema/app.a.type.yml")).unwrap();
        std::fs::remove_file(d.dir.join("schema/dst.a.type.yml")).unwrap();
        std::fs::write(
            d.dir.join("schema/dst.b.type.yml"),
            "enum: dst.b\nrenamed_from: app.a\nvalues: [moving]\n",
        )
        .unwrap();
        succeeds(d.run(&[
            "intent",
            "--drop-type",
            "dst.a",
            "release the intermediate name",
        ]));
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("move.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.changes.changes.len(), 2);
        assert!(matches!(
            saved.changes.changes[0].change,
            pbps_model::Change::DropType { .. }
        ));
        assert!(matches!(
            saved.changes.changes[1].change,
            pbps_model::Change::RenameType { .. }
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
            "rename,destructive",
        ]));
        assert_eq!(
            scalar(db, "SELECT 'dst.b'::regtype::oid::bigint"),
            moving_oid
        );
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let after: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(after.changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_type_move_refuses_kept_and_undeclared_intermediate_holders_without_applying() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for declared in [true, false] {
            let own = OwnDatabase::new(&server, "type_move_held1635");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE SCHEMA dst; CREATE TYPE app.a AS ENUM ('moving')",
            );
            if declared {
                on_server(db, "CREATE TYPE dst.a AS ENUM ('holder')");
            }
            let d = Demo::new("type-move-held1635");
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt managed types"]));
            if !declared {
                on_server(db, "CREATE TYPE dst.a AS ENUM ('external')");
            }
            let before = latest_snapshot(db);
            let moving_oid = scalar(db, "SELECT 'app.a'::regtype::oid::bigint");
            let holder_oid = scalar(db, "SELECT 'dst.a'::regtype::oid::bigint");
            std::fs::remove_file(d.dir.join("schema/app.a.type.yml")).unwrap();
            // Each reviewed revision has a free intermediate name. The target
            // still holds app.a, so the connected plan collapses both renames
            // and must refuse its held dst.a before producing an artifact.
            let middle = d.dir.join("schema/app.b.type.yml");
            std::fs::write(
                &middle,
                "enum: app.b\nrenamed_from: app.a\nvalues: [moving]\n",
            )
            .unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            std::fs::remove_file(&middle).unwrap();
            std::fs::write(
                d.dir.join("schema/dst.b.type.yml"),
                "enum: dst.b\nrenamed_from: app.b\nvalues: [moving]\n",
            )
            .unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let path = d.dir.join("held.plan.json");
            let sql = d.dir.join("held.sql");
            let out = d.run(&[
                "plan",
                "--db",
                db,
                "--out",
                path.to_str().unwrap(),
                "--sql",
                sql.to_str().unwrap(),
            ]);
            assert_ne!(code(&out), 0, "{}", stdout(&out));
            let message = stderr(&out);
            assert!(
                message.contains("dst.a") && message.contains("already occupied"),
                "{message}"
            );
            assert!(!path.exists() && !sql.exists());
            assert_eq!(
                scalar(db, "SELECT 'app.a'::regtype::oid::bigint"),
                moving_oid
            );
            assert_eq!(
                scalar(db, "SELECT 'dst.a'::regtype::oid::bigint"),
                holder_oid
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='dst' AND t.typname='b'"
                ),
                0
            );
            assert_eq!(latest_snapshot(db), before);
        }
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
fn rename_chains_keep_distinct_type_holders_through_apply_and_resumed_checks() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for resume in [false, true] {
            let own = OwnDatabase::new(&server, "rename_chain1635");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.a AS ENUM ('a'); CREATE TYPE app.b AS ENUM ('b'); CREATE DOMAIN app.da AS app.a[]; CREATE DOMAIN app.db AS app.b; CREATE TABLE app.t (a app.a, b app.b[], da app.da, db app.db)",
            );
            let d = Demo::new("rename-chain1635");
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            succeeds(d.run(&[
                "baseline",
                "--db",
                db,
                "--reason",
                "adopt distinct enum holders",
            ]));
            // Record two revisions without applying either. The connected
            // plan must perform both uid-preserving renames in one transaction.
            std::fs::remove_file(d.dir.join("schema/app.b.type.yml")).unwrap();
            std::fs::write(
                d.dir.join("schema/app.c.type.yml"),
                "enum: app.c\nrenamed_from: app.b\nvalues: [b]\n",
            )
            .unwrap();
            std::fs::write(
                d.dir.join("schema/app.db.type.yml"),
                "domain: app.db\ntype: app.c\n",
            )
            .unwrap();
            d.table("table: app.t\ncolumns:\n  a: {type: app.a}\n  b: {type: 'app.c[]'}\n  da: {type: app.da}\n  db: {type: app.db}\n");
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            std::fs::remove_file(d.dir.join("schema/app.a.type.yml")).unwrap();
            std::fs::write(
                d.dir.join("schema/app.b.type.yml"),
                "enum: app.b\nrenamed_from: app.a\nvalues: [a]\n",
            )
            .unwrap();
            std::fs::write(
                d.dir.join("schema/app.da.type.yml"),
                "domain: app.da\ntype: app.b[]\n",
            )
            .unwrap();
            d.table("table: app.t\ncolumns:\n  a: {type: app.b}\n  b: {type: 'app.c[]'}\n  da: {type: app.da}\n  db: {type: app.db}\n");
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let path = d.dir.join("chain.plan.json");
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved.changes.changes.len(), 2);
            assert!(
                matches!(&saved.changes.changes[0].change, pbps_model::Change::RenameType { from, to, .. } if from.name() == "b" && to.name() == "c")
            );
            if resume {
                // Multi-change plans are transactional. Exercise the type-pin
                // resume boundary directly across a committed prefix and a
                // new connection; the staged CLI's single-change resume is
                // covered by the adjacent cross-schema rename test.
                let pg = pbps_pg::Postgres::new();
                let statements: Vec<_> = saved
                    .changes
                    .changes
                    .iter()
                    .flat_map(|p| pg.fixture_emit(&p.change, p.strategy).unwrap())
                    .collect();
                let framing = pbps_dialect::Dialect::transaction_framing(
                    &pg,
                    &pbps_model::SessionZone::utc(),
                );
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let connect = || pbps_db::Conn::connect(pbps_db::Driver::Postgres, db);
                    let trusted = std::collections::BTreeSet::new();
                    let mut conn = connect().await.unwrap();
                    for completed in 0..=statements.len() {
                        if completed == 1 {
                            drop(conn);
                            conn = connect().await.unwrap();
                        }
                        pbps_pg::type_refs::declared::check(
                            &mut conn,
                            &saved.declared_type_pins,
                            &trusted,
                            pbps_pg::pins::Within::OwnTransaction,
                            completed == statements.len(),
                            &saved.changes,
                            &statements[..completed],
                        )
                        .await
                        .unwrap();
                        if let Some(statement) = statements.get(completed) {
                            conn.begin(&framing).await.unwrap();
                            conn.execute(&statement.sql).await.unwrap();
                            pbps_pg::type_refs::declared::vet(
                                &mut conn,
                                statement.declared_type.as_ref().unwrap(),
                                &trusted,
                            )
                            .await
                            .unwrap();
                            pbps_pg::type_refs::declared::check(
                                &mut conn,
                                &saved.declared_type_pins,
                                &trusted,
                                pbps_pg::pins::Within::CallersTransaction,
                                completed + 1 == statements.len(),
                                &saved.changes,
                                &statements[..=completed],
                            )
                            .await
                            .unwrap();
                            conn.commit(&framing).await.unwrap();
                        }
                    }
                    conn.execute("ALTER TYPE app.b ADD VALUE 'manual'")
                        .await
                        .unwrap();
                    assert!(
                        pbps_pg::type_refs::declared::check(
                            &mut conn,
                            &saved.declared_type_pins,
                            &trusted,
                            pbps_pg::pins::Within::OwnTransaction,
                            true,
                            &saved.changes,
                            &statements
                        )
                        .await
                        .is_err()
                    );
                });
            } else {
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
                succeeds(d.run(&["verify", "--db", db]));
                succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
                let after: pbps_model::SavedPlan =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert!(after.changes.is_empty());
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
fn bootstrap_refuses_unidentified_enums_and_domains_before_connecting_or_writing_sql() {
    for (name, yaml, create) in [
        (
            "mood",
            "enum: app.mood\nvalues: [ok]\n",
            "CREATE TYPE \"app\".\"mood\"",
        ),
        (
            "score",
            "domain: app.score\ntype: integer\n",
            "CREATE DOMAIN \"app\".\"score\"",
        ),
    ] {
        let d = Demo::new(&format!("bootstrap-unidentified-{name}1635"));
        std::fs::write(d.dir.join(format!("schema/app.{name}.type.yml")), yaml).unwrap();
        let script = d.dir.join("bootstrap.sql");
        std::fs::write(&script, "keep this script").unwrap();
        // No server can answer at this endpoint: the named identity refusal
        // must precede every connection, including --sql with a target.
        let unreachable = "host=127.0.0.1 port=1 user=postgres dbname=unused sslmode=disable";
        for args in [
            vec!["bootstrap"],
            vec!["bootstrap", "--db", unreachable],
            vec!["bootstrap", "--sql", script.to_str().unwrap()],
            vec![
                "bootstrap",
                "--sql",
                script.to_str().unwrap(),
                "--db",
                unreachable,
            ],
        ] {
            let out = d.run(&args);
            assert_ne!(code(&out), 0, "{args:?}: {}", stdout(&out));
            let error = stderr(&out);
            assert!(error.contains("identity file does not know"), "{error}");
            assert!(error.contains(&format!("type app.{name}")), "{error}");
            assert!(error.contains("pbps plan"), "{error}");
            assert_eq!(
                std::fs::read_to_string(&script).unwrap(),
                "keep this script"
            );
        }
        // Negative: after minting this type's identity, script generation
        // emits its CREATE even when no table references it.
        succeeds(d.run(&["plan", "--no-dev"]));
        succeeds(d.run(&["bootstrap", "--sql", script.to_str().unwrap()]));
        assert!(std::fs::read_to_string(&script).unwrap().contains(create));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn bootstrap_refuses_existing_declared_types_without_recording_a_failed_state() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for (name, yaml, ddl) in [
            (
                "mood",
                "enum: app.mood\nvalues: [ok]\n",
                "CREATE TYPE app.mood AS ENUM ('ok')",
            ),
            (
                "score",
                "domain: app.score\ntype: integer\n",
                "CREATE DOMAIN app.score AS integer",
            ),
        ] {
            let own = OwnDatabase::new(&server, &format!("bootstrap_nonempty_{name}1635"));
            let db = own.connection();
            on_server(db, &format!("CREATE SCHEMA app; {ddl}"));
            let d = Demo::new(&format!("bootstrap-nonempty-{name}1635"));
            std::fs::write(d.dir.join(format!("schema/app.{name}.type.yml")), yaml).unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
            let script = d.dir.join("bootstrap.sql");
            for sql in [false, true] {
                let mut args = vec!["bootstrap", "--db", db];
                if sql {
                    args.extend(["--sql", script.to_str().unwrap()]);
                }
                let out = d.run(&args);
                assert_ne!(code(&out), 0, "{}", stdout(&out));
                let error = stderr(&out);
                assert!(
                    error.contains("already has 1 of the declared object(s)"),
                    "{error}"
                );
                assert!(error.contains(&format!("type app.{name}")), "{error}");
                assert!(
                    error.contains("Bootstrap builds into an empty database"),
                    "{error}"
                );
                assert!(!error.contains("Bootstrap failure recorded"), "{error}");
                assert_eq!(
                    scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
                    0
                );
                assert_eq!(
                    scalar(
                        db,
                        &format!(
                            "SELECT count(*)::bigint FROM pg_type WHERE typnamespace='app'::regnamespace AND typname='{name}'"
                        )
                    ),
                    1
                );
                assert!(!script.exists(), "no script is written on refusal");
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_history_bootstraps_final_labels_connected_and_through_psql() {
    for (container, server) in [
        ("PBPS_TEST_PG_CONTAINER", server()),
        (
            "PBPS_TEST_PG_OLD_CONTAINER",
            std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
        ),
    ] {
        let source = OwnDatabase::new(&server, "label_history1635");
        on_server(source.connection(), "CREATE SCHEMA app");
        let d = Demo::new("label-history1635");
        let file = d.dir.join("schema/app.e.type.yml");
        std::fs::write(&file, "enum: app.e\nvalues: [sad, ok]\n").unwrap();
        d.table("table: app.t\ncolumns:\n  m: {type: app.e}\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", source.connection()]));
        on_server(source.connection(), "INSERT INTO app.t VALUES ('sad')");

        let annotated = "enum: app.e\nvalues: [{name: blue, renamed_from: sad}, ok]\n";
        std::fs::write(&file, annotated).unwrap();
        d.commit();
        let path = d.dir.join("label-history.plan.json");
        let rename = label_plan(&d, source.connection(), &path);
        assert_eq!(rename.changes.changes.len(), 1);
        assert!(matches!(
            rename.changes.changes[0].change,
            pbps_model::Change::RenameEnumValue { .. }
        ));
        succeeds(d.run(&[
            "apply",
            "--db",
            source.connection(),
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "rename",
        ]));
        assert_eq!(
            scalar(
                source.connection(),
                "SELECT count(*)::bigint FROM app.t WHERE m::text='blue'"
            ),
            1
        );
        assert!(
            label_plan(&d, source.connection(), &path)
                .changes
                .is_empty()
        );

        for scripted in [false, true] {
            let fresh = OwnDatabase::new(
                &server,
                if scripted {
                    "label_history_script1635"
                } else {
                    "label_history_connected1635"
                },
            );
            on_server(fresh.connection(), "CREATE SCHEMA app");
            if scripted {
                let path = d.dir.join("label-history-bootstrap.sql");
                succeeds(d.run(&["bootstrap", "--sql", path.to_str().unwrap()]));
                succeeds(psql_script_in(
                    container,
                    &fresh,
                    &std::fs::read_to_string(path).unwrap(),
                ));
                // SQL output creates no ledger; adopt the replay before planning.
                succeeds(d.run(&[
                    "baseline",
                    "--db",
                    fresh.connection(),
                    "--reason",
                    "adopt replayed bootstrap SQL",
                ]));
            } else {
                succeeds(d.run(&["bootstrap", "--db", fresh.connection()]));
            }
            assert_eq!(
                latest_snapshot(fresh.connection()).schema.types[&"app.e".parse().unwrap()],
                ManagedType::Enum {
                    values: vec!["blue".into(), "ok".into()]
                }
            );
            assert!(label_plan(&d, fresh.connection(), &path).changes.is_empty());
            assert_eq!(std::fs::read_to_string(&file).unwrap(), annotated);
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn bootstrap_creates_identified_enums_domains_and_their_table_and_records_all_of_them() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "bootstrap_types1635");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("bootstrap-types1635");
        std::fs::write(
            d.dir.join("schema/app.mood.type.yml"),
            "enum: app.mood\nvalues: [sad, ok]\n",
        )
        .unwrap();
        std::fs::write(
            d.dir.join("schema/app.score.type.yml"),
            "domain: app.score\ntype: integer\nchecks: {positive: 'VALUE > 0'}\n",
        )
        .unwrap();
        d.table("table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  mood: {type: app.mood}\n  score: {type: app.score}\nprimary_key: [id]\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        let ids: pbps_model::IdsFile =
            serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        let snapshot = latest_snapshot(db);
        let desired = pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema;
        assert_eq!(snapshot.kind, pbps_model::StateKind::Bootstrap);
        assert_eq!(snapshot.ids, ids);
        assert_eq!(snapshot.schema.types.len(), 2);
        assert_eq!(snapshot.schema.tables.len(), 1);
        assert_eq!(
            snapshot.declared,
            pbps_model::Declared::from_schema(&desired)
        );
        assert_eq!(
            snapshot.declared.overlay(&snapshot.schema).types,
            desired.types
        );
        succeeds(d.run(&["verify", "--db", db]));
        let path = d.dir.join("after-bootstrap.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let next: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(next.changes.is_empty(), "{:?}", next.changes);
        on_server(db, "INSERT INTO app.t VALUES (1, 'ok', 1)");
        assert!(try_on_server(db, "INSERT INTO app.t VALUES (2, 'ok', -1)").is_err());
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
            1
        );
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

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn type_renames_forward_bound_expressions_and_the_next_plan_is_clean() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for moved in [false, true] {
            let own = OwnDatabase::new(&server, "rename_text1635");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app; CREATE SCHEMA moved;
                CREATE TYPE app.mood AS ENUM ('sad', 'ok');
                CREATE DOMAIN app.score AS integer CHECK (VALUE > 0);
                CREATE DOMAIN app.kept AS app.mood DEFAULT 'ok'::app.mood CONSTRAINT kept_check CHECK (VALUE <> 'sad'::app.mood);
                CREATE DOMAIN app.kept_score AS app.score DEFAULT 1::app.score CONSTRAINT kept_score_check CHECK (VALUE <> 2::app.score);
                CREATE TABLE app.t (v app.mood DEFAULT 'ok'::app.mood, n app.score DEFAULT 1::app.score,
                    alias text DEFAULT ('app.mood'::regtype)::text,
                    g app.mood GENERATED ALWAYS AS ('ok'::app.mood) STORED,
                    gn app.score GENERATED ALWAYS AS (1::app.score) STORED,
                    CONSTRAINT allowed CHECK (v <> 'sad'::app.mood), CONSTRAINT allowed_n CHECK (n <> 2::app.score));
                CREATE POLICY p ON app.t USING (v <> 'sad'::app.mood) WITH CHECK (n <> 2::app.score);
                CREATE VIEW app.v AS SELECT 'ok'::app.mood AS v, 1::app.score AS n;
                CREATE INDEX ix ON app.t ((v = 'ok'::app.mood)) WHERE v <> 'sad'::app.mood;
                CREATE TABLE app.parts (v app.mood) PARTITION BY LIST (v);
                CREATE TABLE app.part PARTITION OF app.parts FOR VALUES IN ('ok');
                ALTER TABLE app.part ALTER COLUMN v SET DEFAULT 'ok'::app.mood;
                CREATE FUNCTION app.f(x boolean DEFAULT ('ok'::app.mood <> 'sad'::app.mood)) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN x AND (1::app.score <> 2::app.score);");
            if moved {
                on_server(
                    db,
                    "CREATE FUNCTION app.typed(x app.mood DEFAULT 'ok'::app.mood) RETURNS app.score LANGUAGE SQL IMMUTABLE RETURN 1::app.score",
                );
            }
            let d = Demo::new("rename-text1635");
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            // Bootstrap the pulled declarations to record their exact spelling,
            // independently of the engine's reformatting (decisions 207/208).
            on_server(db, "DROP SCHEMA app CASCADE; CREATE SCHEMA app");
            succeeds(d.run(&["bootstrap", "--db", db]));
            let before = latest_snapshot(db);
            let destination = if moved { "moved" } else { "app" };
            for (old, new, kind) in [("mood", "feeling", "enum"), ("score", "rating", "domain")] {
                let old_path = d.dir.join(format!("schema/app.{old}.type.yml"));
                let text = std::fs::read_to_string(&old_path).unwrap();
                std::fs::remove_file(old_path).unwrap();
                std::fs::write(
                    d.dir.join(format!("schema/{destination}.{new}.type.yml")),
                    text.replace(
                        &format!("{kind}: app.{old}"),
                        &format!("{kind}: {destination}.{new}\nrenamed_from: app.{old}"),
                    ),
                )
                .unwrap();
            }
            // Change structural references only; each expression keeps exactly
            // the spelling originally authored, including its old cast names.
            for entry in std::fs::read_dir(d.dir.join("schema")).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|e| e == "yml") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let text = text
                        .replace("type: app.mood", &format!("type: {destination}.feeling"))
                        .replace("type: app.score", &format!("type: {destination}.rating"));
                    if moved && text.starts_with("function: app.typed(") {
                        // Argument types are module identity, so their rename
                        // is an explicit module replacement (ADR-0009 §1).
                        let text = text
                            .replace("app.mood", "moved.feeling")
                            .replace("app.score", "moved.rating");
                        std::fs::remove_file(path).unwrap();
                        std::fs::write(
                            d.dir
                                .join("schema/app.typed%28moved.feeling%29.function.yml"),
                            text,
                        )
                        .unwrap();
                    } else {
                        std::fs::write(path, text).unwrap();
                    }
                }
            }
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let path = d.dir.join("rename.plan.json");
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let plan: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                plan.changes
                    .changes
                    .iter()
                    .filter(|p| matches!(p.change, pbps_model::Change::RenameType { .. }))
                    .count(),
                2
            );
            if !moved {
                assert_eq!(plan.changes.changes.len(), 2);
            } else {
                assert!(
                    plan.changes
                        .changes
                        .iter()
                        .any(|p| matches!(p.change, pbps_model::Change::DropModule { .. }))
                );
                assert!(
                    plan.changes
                        .changes
                        .iter()
                        .any(|p| matches!(p.change, pbps_model::Change::CreateModule { .. }))
                );
            }
            assert!(plan.changes.changes.iter().all(|p| matches!(
                p.change,
                pbps_model::Change::RenameType { .. }
                    | pbps_model::Change::DropModule { .. }
                    | pbps_model::Change::CreateModule { .. }
                    | pbps_model::Change::PublicExecution { .. }
            )));
            succeeds(d.run(&[
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&path),
                "--allow",
                if moved {
                    "rename,destructive"
                } else {
                    "rename"
                },
            ]));
            let after = latest_snapshot(db);
            assert_eq!(before.declared.expressions, after.declared.expressions);
            for (id, text) in &before.declared.modules {
                if id.name() != "typed" {
                    assert_eq!(after.declared.modules.get(id), Some(text));
                }
            }
            if moved {
                assert!(
                    after
                        .schema
                        .modules
                        .contains_key(&"app.typed(moved.feeling)".parse().unwrap())
                );
            }
            for name in ["app.kept", "app.kept_score"] {
                let name = name.parse().unwrap();
                assert_eq!(before.declared.types[&name], after.declared.types[&name]);
                assert_ne!(before.schema.types[&name], after.schema.types[&name]);
            }
            succeeds(d.run(&["verify", "--db", db]));
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let next: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(next.changes.is_empty(), "{:?}", next.changes);
            // A changed post-rename catalog still fails ordinary drift and the
            // next-plan checksum; the retained authored text cannot hide it.
            on_server(
                db,
                &format!("ALTER DOMAIN app.kept SET DEFAULT 'sad'::{destination}.feeling"),
            );
            assert_ne!(code(&d.run(&["verify", "--db", db])), 0);
            assert_ne!(code(&d.run(&["plan", "--db", db])), 0);
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn type_renames_refuse_default_and_check_edits_inside_the_plan_window() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for (rename_domain, edit) in [
            (
                false,
                "ALTER DOMAIN app.kept SET DEFAULT 'sad'::app.feeling",
            ),
            (
                false,
                "ALTER TABLE app.t ALTER COLUMN v SET DEFAULT 'sad'::app.feeling",
            ),
            (
                false,
                "ALTER TABLE app.t DROP CONSTRAINT allowed; ALTER TABLE app.t ADD CONSTRAINT allowed CHECK (v = 'sad'::app.feeling)",
            ),
            (
                true,
                "ALTER DOMAIN app.renamed SET DEFAULT 'sad'::app.feeling",
            ),
            (
                true,
                "ALTER DOMAIN app.renamed DROP CONSTRAINT kept_check; ALTER DOMAIN app.renamed ADD CONSTRAINT kept_check CHECK (VALUE = 'sad'::app.feeling)",
            ),
        ] {
            let own = OwnDatabase::new(&server, "rename_edit1635");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('sad', 'ok'); CREATE DOMAIN app.kept AS app.mood DEFAULT 'ok'::app.mood CONSTRAINT kept_check CHECK (VALUE <> 'sad'::app.mood); CREATE TABLE app.t(v app.mood DEFAULT 'ok'::app.mood, CONSTRAINT allowed CHECK (v <> 'sad'::app.mood))",
            );
            let d = Demo::new("rename-edit1635");
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt rename fixture"]));
            std::fs::rename(
                d.dir.join("schema/app.mood.type.yml"),
                d.dir.join("schema/app.feeling.type.yml"),
            )
            .unwrap();
            if rename_domain {
                let path = d.dir.join("schema/app.kept.type.yml");
                let text = std::fs::read_to_string(&path).unwrap().replace(
                    "domain: app.kept",
                    "domain: app.renamed\nrenamed_from: app.kept",
                );
                std::fs::remove_file(path).unwrap();
                std::fs::write(d.dir.join("schema/app.renamed.type.yml"), text).unwrap();
            }
            for entry in std::fs::read_dir(d.dir.join("schema")).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|e| e == "yml") {
                    let text = std::fs::read_to_string(&path)
                        .unwrap()
                        .replace(
                            "enum: app.mood",
                            "enum: app.feeling\nrenamed_from: app.mood",
                        )
                        .replace("type: app.mood", "type: app.feeling");
                    std::fs::write(path, text).unwrap();
                }
            }
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let path = d.dir.join("refuse.plan.json");
            succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                saved
                    .changes
                    .changes
                    .iter()
                    .filter(|p| matches!(p.change, pbps_model::Change::RenameType { .. }))
                    .count(),
                if rename_domain { 2 } else { 1 }
            );
            let edit = if rename_domain {
                format!(
                    "IF EXISTS (SELECT FROM pg_type WHERE typnamespace='app'::regnamespace AND typname='renamed') THEN {edit}; END IF"
                )
            } else {
                edit.to_owned()
            };
            on_server(
                db,
                &format!(
                    "CREATE FUNCTION app.edit() RETURNS event_trigger LANGUAGE plpgsql AS $body$ BEGIN IF current_setting('pbps.test_rename_edit', true) IS DISTINCT FROM 'on' THEN PERFORM set_config('pbps.test_rename_edit', 'on', true); {edit}; PERFORM set_config('pbps.test_rename_edit', 'off', true); END IF; END $body$; CREATE EVENT TRIGGER edit ON ddl_command_end WHEN TAG IN ('ALTER TYPE', 'ALTER DOMAIN') EXECUTE FUNCTION app.edit()"
                ),
            );
            let out = d.run(&[
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&path),
                "--allow",
                "rename",
            ]);
            assert_ne!(code(&out), 0, "hand edit was recorded: {edit}");
            assert!(
                stderr(&out).contains("changed since planning")
                    || stderr(&out).contains("moved while this plan was running"),
                "{}",
                stderr(&out)
            );
            on_server(db, "DROP EVENT TRIGGER edit; DROP FUNCTION app.edit()");
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_type WHERE typname='mood'"
                ),
                1
            );
            succeeds(d.run(&["verify", "--db", db]));
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn same_name_type_replacements_warn_and_distinguish_new_and_retained_dependents() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for array in [false, true] {
            for domain in [false, true] {
                for retained in [false, true] {
                    let own = OwnDatabase::new(&server, "replacement2321");
                    let db = own.connection();
                    let ty = if array { "app.mood[]" } else { "app.mood" };
                    on_server(
                        db,
                        "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('old')",
                    );
                    if retained {
                        let sql = if domain {
                            format!("CREATE DOMAIN app.choice AS {ty}")
                        } else {
                            format!("CREATE TABLE app.t (v {ty})")
                        };
                        on_server(db, &sql);
                    }
                    let old_oid = scalar(db, "SELECT 'app.mood'::regtype::oid::bigint");
                    let d = Demo::new("replacement2321");
                    succeeds(d.run(&["pull", "--db", db]));
                    d.commit();
                    succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt old type"]));
                    std::fs::remove_file(d.dir.join("schema/app.mood.type.yml")).unwrap();
                    succeeds(d.run(&[
                        "intent",
                        "--drop-type",
                        "app.mood",
                        "replace the type explicitly",
                    ]));
                    std::fs::write(
                        d.dir.join("schema/app.mood.type.yml"),
                        "enum: app.mood\nvalues: [new]\n",
                    )
                    .unwrap();
                    if domain {
                        std::fs::write(
                            d.dir.join("schema/app.choice.type.yml"),
                            format!("domain: app.choice\ntype: '{ty}'\n"),
                        )
                        .unwrap();
                    } else {
                        d.table(&format!("table: app.t\ncolumns:\n  v: {{type: '{ty}'}}\n"));
                    }
                    // Mint the new declaration's uid without asking a refused
                    // retained-dependent plan to publish identity as a side effect.
                    let ids_path = d.dir.join("schema.ids.json");
                    let ids: pbps_model::IdsFile =
                        serde_json::from_slice(&std::fs::read(&ids_path).unwrap()).unwrap();
                    let desired = pbps_load::load_schema_dir(&d.dir.join("schema"))
                        .unwrap()
                        .schema;
                    let resolved = pbps_diff::resolve(
                        &desired,
                        &ids,
                        &[],
                        &pbps_diff::Context {
                            operator: "test".into(),
                            today: "2026-10-24".into(),
                        },
                    )
                    .unwrap();
                    std::fs::write(&ids_path, serde_json::to_vec_pretty(&resolved.ids).unwrap())
                        .unwrap();
                    if !retained {
                        let offline = d.run(&["plan", "--no-dev"]);
                        succeeds(offline.clone());
                        assert!(
                            stderr(&offline).contains("different types to pbps"),
                            "{}",
                            stderr(&offline)
                        );
                    }
                    d.commit();
                    let path = d.dir.join("replacement.plan.json");
                    let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
                    if retained {
                        assert_ne!(code(&out), 0);
                        assert!(
                            stderr(&out).contains("app.mood")
                                && stderr(&out).contains("retained declaration"),
                            "{}",
                            stderr(&out)
                        );
                        assert!(!path.exists());
                        assert_eq!(
                            scalar(db, "SELECT 'app.mood'::regtype::oid::bigint"),
                            old_oid
                        );
                        continue;
                    }
                    succeeds(out.clone());
                    assert!(
                        stderr(&out).contains("different types to pbps")
                            && stderr(&out).contains("history does not carry over"),
                        "{}",
                        stderr(&out)
                    );
                    let json = d.run(&[
                        "plan",
                        "--db",
                        db,
                        "--out",
                        path.to_str().unwrap(),
                        "--format",
                        "json",
                    ]);
                    succeeds(json.clone());
                    let report: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
                    assert!(
                        report["findings"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|f| f["id"] == "plan.type-replacement"),
                        "{report}"
                    );
                    assert!(
                        !stderr(&json).contains("different types to pbps"),
                        "{}",
                        stderr(&json)
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
                        "destructive",
                    ]));
                    assert_ne!(
                        scalar(db, "SELECT 'app.mood'::regtype::oid::bigint"),
                        old_oid
                    );
                    let clean = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
                    succeeds(clean.clone());
                    assert!(!stderr(&clean).contains("different types to pbps"));
                    let next: pbps_model::SavedPlan =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    assert!(next.changes.is_empty());
                    succeeds(d.run(&["verify", "--db", db]));
                }
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn external_references_do_not_borrow_a_dropped_types_declared_identity() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        for array in ["", "[]"] {
            let own = OwnDatabase::new(&server, "external2321");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('old')",
            );
            let oid = scalar(db, "SELECT 'app.mood'::regtype::oid::bigint");
            let d = Demo::new("external2321");
            succeeds(d.run(&["pull", "--db", db]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt old type"]));
            std::fs::remove_file(d.dir.join("schema/app.mood.type.yml")).unwrap();
            succeeds(d.run(&[
                "intent",
                "--drop-type",
                "app.mood",
                "retire the declared identity",
            ]));
            d.table(&format!(
                "table: app.t\ncolumns:\n  v: {{type: 'app.mood{array}'}}\n"
            ));
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            let path = d.dir.join("external.plan.json");
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            succeeds(out.clone());
            assert!(!stderr(&out).contains("different types to pbps"));
            let plan: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(plan.declared_type_pins.is_empty());
            assert_eq!(plan.type_pins.len(), 1);
            assert_eq!(plan.type_pins[0].name, "mood");
            assert_eq!(plan.type_pins[0].oid, oid);
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='app' AND c.relname='t'"
                ),
                0
            );
        }
    }
}

fn label_plan(d: &Demo, db: &str, path: &std::path::Path) -> pbps_model::SavedPlan {
    succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_additions_preserve_all_positions_through_apply_pull_and_repull() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for values in [
            "[first, sad, ok]",
            "[sad, middle, ok]",
            "[sad, ok, last]",
            "[sad, adjacent_one, adjacent_two, ok]",
            "[first, second, sad, middle_one, middle_two, ok, last]",
        ] {
            let own = OwnDatabase::new(&server, "label_positions1635");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('sad','ok')",
            );
            let d = Demo::new("label-positions1635");
            succeeds(d.run(&["pull", "--db", db, "--force"]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt enum order"]));
            let file = d.dir.join("schema/app.e.type.yml");
            std::fs::write(&file, format!("enum: app.e\nvalues: {values}\n")).unwrap();
            d.commit();
            let path = d.dir.join("labels.plan.json");
            let plan = label_plan(&d, db, &path);
            assert!(!plan.changes.is_empty());
            assert!(plan.changes.changes.iter().all(|p| matches!(
                p.change,
                pbps_model::Change::AddEnumValue { .. }
            ) && p.risks.is_empty()));
            succeeds(apply_plan(&d, db, &path, false));
            assert!(label_plan(&d, db, &path).changes.is_empty());
            let wanted = pbps_load::load_schema_dir(&d.dir.join("schema"))
                .unwrap()
                .schema
                .types;
            for _ in 0..2 {
                succeeds(d.run(&["pull", "--db", db, "--force"]));
                assert_eq!(
                    pbps_load::load_schema_dir(&d.dir.join("schema"))
                        .unwrap()
                        .schema
                        .types,
                    wanted
                );
            }
            assert!(label_plan(&d, db, &path).changes.is_empty());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_applied_label_chains_keep_annotations_when_adding_labels_and_converge() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "label_chain_round1");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('a','b')");
        let d = Demo::new("label-chain-round1");
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt enum before label chain",
        ]));
        let file = d.dir.join("schema/app.e.type.yml");
        std::fs::write(
            &file,
            "enum: app.e\nvalues: [{name: b, renamed_from: a}, {name: c, renamed_from: b}]\n",
        )
        .unwrap();
        d.commit();
        let path = d.dir.join("label-chain.plan.json");
        let chain = label_plan(&d, db, &path);
        assert_eq!(chain.changes.changes.len(), 2);
        assert!(
            chain
                .changes
                .changes
                .iter()
                .all(|step| matches!(step.change, pbps_model::Change::RenameEnumValue { .. }))
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
        assert!(label_plan(&d, db, &path).changes.is_empty());

        // Keep both annotations: b now names the first label's final value,
        // not a source that should be renamed to c again.
        std::fs::write(
            &file,
            "enum: app.e\nvalues: [{name: b, renamed_from: a}, {name: c, renamed_from: b}, d]\n",
        )
        .unwrap();
        d.commit();
        let added = label_plan(&d, db, &path);
        assert_eq!(added.changes.changes.len(), 1);
        assert!(
            matches!(&added.changes.changes[0].change, pbps_model::Change::AddEnumValue { value, .. } if value == "d")
        );
        succeeds(apply_plan(&d, db, &path, false));
        assert!(label_plan(&d, db, &path).changes.is_empty());
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        let pulled = pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema;
        assert_eq!(
            pulled.types[&"app.e".parse().unwrap()],
            ManagedType::Enum {
                values: vec!["b".into(), "c".into(), "d".into()]
            }
        );
        d.commit();
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_renames_keep_rows_and_rebuild_updated_checks_and_defaults() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for authored in [false, true] {
            let own = OwnDatabase::new(&server, "label_rename1635");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app");
            let d = Demo::new("label-rename1635");
            let file = d.dir.join("schema/app.e.type.yml");
            std::fs::write(&file, "enum: app.e\nvalues: [sad, ok, happy]\n").unwrap();
            d.table("table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  m: {type: app.e, default: \"'sad'\"}\nprimary_key: [id]\nchecks: {allowed: \"m <> 'happy'\"}\n");
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", db]));
            on_server(db, "INSERT INTO app.t(id,m) VALUES (1,'sad')");
            if !authored {
                succeeds(d.run(&["pull", "--db", db, "--force"]));
                d.commit();
                succeeds(d.run(&[
                    "baseline",
                    "--db",
                    db,
                    "--reason",
                    "compare adopted catalog expressions",
                ]));
            }
            // A retained check and default both bind the renamed constants.
            for path in std::fs::read_dir(d.dir.join("schema"))
                .unwrap()
                .map(|e| e.unwrap().path())
            {
                if path != file {
                    let text = std::fs::read_to_string(&path)
                        .unwrap()
                        .replace("sad", "blue")
                        .replace("happy", "joyful");
                    std::fs::write(path, text).unwrap();
                }
            }
            std::fs::write(&file, "enum: app.e\nvalues: [{name: blue, renamed_from: sad}, ok, {name: joyful, renamed_from: happy}]\n").unwrap();
            d.commit();
            let path = d.dir.join("rename-labels.plan.json");
            let plan = label_plan(&d, db, &path);
            assert_eq!(plan.changes.changes.len(), 5, "{:?}", plan.changes);
            assert_eq!(
                plan.changes
                    .changes
                    .iter()
                    .filter(|p| matches!(p.change, pbps_model::Change::RenameEnumValue { .. }))
                    .count(),
                2
            );
            assert!(
                plan.changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::DropCheck { .. }))
            );
            assert!(
                plan.changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::AddCheck { .. }))
            );
            assert!(
                plan.changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::AlterColumnDefault { .. }))
            );
            let denied = apply_plan(&d, db, &path, false);
            assert_ne!(code(&denied), 0);
            succeeds(d.run(&[
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&path),
                "--allow",
                "rename,constraint",
            ]));
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM app.t WHERE m::text='blue'"
                ),
                1
            );
            assert!(try_on_server(db, "INSERT INTO app.t VALUES (2,'joyful')").is_err());
            on_server(db, "INSERT INTO app.t(id) VALUES (3)");
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM app.t WHERE m::text='blue'"
                ),
                2
            );
            assert!(label_plan(&d, db, &path).changes.is_empty());
            std::fs::write(&file, "enum: app.e\nvalues: [blue, ok, joyful]\n").unwrap();
            d.commit();
            assert!(
                label_plan(&d, db, &path).changes.is_empty(),
                "provenance must survive removal of the one-shot annotations"
            );
            if !authored {
                // Pull adopts catalog text. Deliberate authored-layout changes
                // remain differences under DEC-1635.4; do not guess equality.
                succeeds(d.run(&["pull", "--db", db, "--force"]));
                let pulled = pbps_load::load_schema_dir(&d.dir.join("schema"))
                    .unwrap()
                    .schema;
                assert!(
                    matches!(&pulled.tables[&"app.t".parse().unwrap()].columns["m"].value_source, pbps_model::ValueSource::Default(text) if text.contains("'blue'"))
                );
                d.commit();
                let next = label_plan(&d, db, &path);
                assert!(
                    next.changes.is_empty(),
                    "authored={authored}: {:?}",
                    next.changes
                );
            }
        }
        let own = OwnDatabase::new(&server, "label_array_constants1635");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('sad','ok'); CREATE TABLE app.arrays (m app.e[] DEFAULT '{sad}'::app.e[], CONSTRAINT c CHECK (m <> '{sad}'::app.e[]))",
        );
        let d = Demo::new("label-array-constants1635");
        succeeds(d.run(&["pull", "--db", db]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt enum array defaults and checks",
        ]));
        for path in std::fs::read_dir(d.dir.join("schema"))
            .unwrap()
            .map(|e| e.unwrap().path())
        {
            let text = std::fs::read_to_string(&path)
                .unwrap()
                .replace("sad", "blue");
            std::fs::write(path, text).unwrap();
        }
        std::fs::write(
            d.dir.join("schema/app.e.type.yml"),
            "enum: app.e\nvalues: [{name: blue, renamed_from: sad}, ok]\n",
        )
        .unwrap();
        d.commit();
        let path = d.dir.join("array-constants.plan.json");
        assert_eq!(label_plan(&d, db, &path).changes.changes.len(), 4);
        succeeds(d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
            "--allow",
            "rename,constraint",
        ]));
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_renames_leave_unrelated_check_text_and_refuse_stale_bound_checks() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for stale in [false, true] {
            let own = OwnDatabase::new(&server, "label_no_rewrite2b");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app");
            let d = Demo::new("label-no-rewrite2b");
            let file = d.dir.join("schema/app.e.type.yml");
            std::fs::write(&file, "enum: app.e\nvalues: [sad, ok]\n").unwrap();
            let check = if stale {
                "m <> 'sad'"
            } else {
                "m IS NOT NULL AND 'sad' <> 'other'"
            };
            d.table(&format!(
                "table: app.t\ncolumns:\n  m: {{type: app.e}}\nchecks: {{kept: \"{check}\"}}\n"
            ));
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", db]));
            let initial = latest_snapshot(db);
            std::fs::write(
                &file,
                "enum: app.e\nvalues: [{name: blue, renamed_from: sad}, ok]\n",
            )
            .unwrap();
            d.commit();
            let path = d.dir.join("label-no-rewrite.plan.json");
            let plan = label_plan(&d, db, &path);
            assert_eq!(plan.changes.changes.len(), 1, "{:?}", plan.changes);
            assert!(matches!(
                plan.changes.changes[0].change,
                pbps_model::Change::RenameEnumValue { .. }
            ));
            let apply = || {
                d.run(&[
                    "apply",
                    "--db",
                    db,
                    "--plan",
                    path.to_str().unwrap(),
                    "--checksum",
                    &plan_checksum(&path),
                    "--allow",
                    "rename",
                ])
            };
            if stale {
                let result = apply();
                assert_ne!(
                    code(&result),
                    0,
                    "stale bound CHECK must never be silently recorded: {}",
                    stdout(&result)
                );
                assert!(stderr(&result).contains("app.t"), "{}", stderr(&result));
                let failed = latest_snapshot(db);
                assert_eq!(failed.kind, pbps_model::StateKind::Failed);
                assert_eq!(failed.schema, initial.schema);
                assert_eq!(failed.declared, initial.declared);
                assert_eq!(failed.ids, initial.ids);
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_enum WHERE enumtypid='app.e'::regtype AND enumlabel='sad'"
                    ),
                    1
                );
                let next = label_plan(&d, db, &path);
                assert_eq!(
                    next.changes.changes.len(),
                    1,
                    "the failed transaction leaves the rename pending"
                );
                let again = apply();
                assert_ne!(code(&again), 0);
                assert!(stderr(&again).contains("app.t"), "{}", stderr(&again));
            } else {
                succeeds(apply());
                let snapshot = latest_snapshot(db);
                let live =
                    &snapshot.schema.tables[&"app.t".parse().unwrap()].checks["kept"].expression;
                assert!(live.contains("'sad'::text"), "{live}");
                assert_eq!(
                    snapshot.declared.expressions.checks[&"app.t".parse().unwrap()]["kept"],
                    check
                );
                assert!(label_plan(&d, db, &path).changes.is_empty());
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_removals_reorders_missing_sources_and_swaps_refuse_without_sql() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "label_refusals1635");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('a','b')");
        let d = Demo::new("label-refusals1635");
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt labels"]));
        let file = d.dir.join("schema/app.e.type.yml");
        let path = d.dir.join("refused-labels.plan.json");
        for (values, expected) in [
            ("[{name: x, renamed_from: absent}, b]", "absent"),
            (
                "[{name: x, renamed_from: a}, {name: red, renamed_from: a}]",
                "two labels",
            ),
            (
                "[{name: b, renamed_from: a}, {name: a, renamed_from: b}]",
                "swap",
            ),
        ] {
            std::fs::write(&file, format!("enum: app.e\nvalues: {values}\n")).unwrap();
            d.commit();
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("app.e") && stderr(&out).contains(expected),
                "{}",
                stderr(&out)
            );
            assert!(!path.exists());
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_enum WHERE enumtypid='app.e'::regtype AND enumlabel IN ('a','b')"
                ),
                2
            );
            assert_eq!(
                scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
                1
            );
        }
        std::fs::write(
            &file,
            "enum: app.e\nvalues: [{name: b, renamed_from: a}, {name: c, renamed_from: b}]\n",
        )
        .unwrap();
        d.commit();
        let plan = label_plan(&d, db, &path);
        assert_eq!(plan.changes.changes.len(), 2);
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
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_new_label_uses_refuse_before_sql_and_the_same_uses_succeed_in_a_second_plan() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for ty in ["app.e", "app.e[]", "app.d"] {
            for kind in ["default", "check"] {
                let own = OwnDatabase::new(&server, "label_use1635");
                let db = own.connection();
                on_server(
                    db,
                    "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('old'); CREATE DOMAIN app.d AS app.e",
                );
                let d = Demo::new("label-use1635");
                succeeds(d.run(&["pull", "--db", db, "--force"]));
                d.commit();
                succeeds(d.run(&[
                    "baseline",
                    "--db",
                    db,
                    "--reason",
                    "adopt enum before evaluation",
                ]));
                let enum_file = d.dir.join("schema/app.e.type.yml");
                std::fs::write(&enum_file, "enum: app.e\nvalues: [old, new]\n").unwrap();
                let literal = if ty.ends_with("[]") {
                    "ARRAY['new'::app.e]"
                } else if ty == "app.d" {
                    "'new'::app.d"
                } else {
                    "'new'"
                };
                let mut table = format!(
                    "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: '{ty}'"
                );
                if kind == "default" {
                    table.push_str(&format!(", default: \"{literal}\""));
                }
                table.push_str("}\n");
                table.push_str("primary_key: [id]\n");
                match kind {
                    "check" => table.push_str(&format!(
                        "checks: {{allowed: \"{}\"}}\n",
                        if ty == "app.d" {
                            "m::app.e <> 'new'::app.e".to_owned()
                        } else {
                            format!("m <> {literal}")
                        }
                    )),
                    "default" => {}
                    _ => unreachable!(),
                }
                d.table(&table);
                // Offline planning mints new table/column identities, while
                // the new-label refusal itself must also apply offline.
                std::fs::write(&enum_file, "enum: app.e\nvalues: [old]\n").unwrap();
                succeeds(d.run(&["plan", "--no-dev"]));
                std::fs::write(&enum_file, "enum: app.e\nvalues: [old, new]\n").unwrap();
                d.commit();
                let path = d.dir.join("unsafe-label.plan.json");
                let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
                assert_ne!(code(&out), 0, "{ty} {kind}");
                assert!(
                    stderr(&out).contains("new")
                        && stderr(&out).contains("apply the label first, then the rest"),
                    "{ty} {kind}: {}",
                    stderr(&out)
                );
                assert!(!path.exists());
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_enum WHERE enumtypid='app.e'::regtype AND enumlabel='new'"
                    ),
                    0
                );
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_class WHERE relnamespace='app'::regnamespace AND relname='t'"
                    ),
                    0
                );
                assert_eq!(
                    scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
                    1
                );
                std::fs::remove_file(d.dir.join("schema/app.t.yml")).unwrap();
                // A refused plan cannot leave identities for removed files.
                let ids_file = d.dir.join("schema.ids.json");
                let mut ids: pbps_model::IdsFile =
                    serde_json::from_slice(&std::fs::read(&ids_file).unwrap()).unwrap();
                ids.tables.clear();
                ids.columns.clear();
                std::fs::write(
                    &ids_file,
                    format!("{}\n", serde_json::to_string_pretty(&ids).unwrap()),
                )
                .unwrap();
                d.commit();
                let first = label_plan(&d, db, &path);
                assert_eq!(first.changes.changes.len(), 1);
                succeeds(apply_plan(&d, db, &path, false));
                d.table(&table);
                succeeds(d.run(&["plan", "--no-dev"]));
                d.commit();
                assert!(!label_plan(&d, db, &path).changes.is_empty());
                let checksum = plan_checksum(&path);
                let out = d.run(&[
                    "apply",
                    "--db",
                    db,
                    "--plan",
                    path.to_str().unwrap(),
                    "--checksum",
                    &checksum,
                    "--allow",
                    "constraint",
                ]);
                succeeds(out);
                assert!(label_plan(&d, db, &path).changes.is_empty(), "{ty} {kind}");
            }
        }
        for base in ["app.e", "app.e[]"] {
            for kind in ["default", "check"] {
                let own = OwnDatabase::new(&server, "label_domain_use1635");
                let db = own.connection();
                on_server(db, "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('old')");
                let d = Demo::new("label-domain-use1635");
                succeeds(d.run(&["pull", "--db", db, "--force"]));
                d.commit();
                succeeds(d.run(&[
                    "baseline",
                    "--db",
                    db,
                    "--reason",
                    "adopt enum before domain evaluation",
                ]));
                let enum_file = d.dir.join("schema/app.e.type.yml");
                let domain_file = d.dir.join("schema/app.used.type.yml");
                let literal = if base.ends_with("[]") {
                    "ARRAY['new'::app.e]"
                } else {
                    "'new'::app.e"
                };
                let clause = if kind == "default" {
                    format!("default: \"{literal}\"\n")
                } else {
                    format!("checks: {{kept: \"VALUE <> {literal}\"}}\n")
                };
                let domain = format!("domain: app.used\ntype: '{base}'\n{clause}");
                std::fs::write(&domain_file, &domain).unwrap();
                succeeds(d.run(&["plan", "--no-dev"]));
                std::fs::write(&enum_file, "enum: app.e\nvalues: [old, new]\n").unwrap();
                d.commit();
                let path = d.dir.join("domain-label.plan.json");
                let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
                assert_ne!(code(&out), 0);
                assert!(
                    stderr(&out).contains("new")
                        && stderr(&out).contains("apply the label first, then the rest"),
                    "{}",
                    stderr(&out)
                );
                assert!(!path.exists());
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_enum WHERE enumtypid='app.e'::regtype AND enumlabel='new'"
                    ),
                    0
                );
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_type WHERE typnamespace='app'::regnamespace AND typname='used'"
                    ),
                    0
                );
                std::fs::remove_file(&domain_file).unwrap();
                let ids_file = d.dir.join("schema.ids.json");
                let mut ids: pbps_model::IdsFile =
                    serde_json::from_slice(&std::fs::read(&ids_file).unwrap()).unwrap();
                ids.types
                    .retain(|_, name| name.raw().to_string() != "app.used");
                std::fs::write(&ids_file, serde_json::to_vec_pretty(&ids).unwrap()).unwrap();
                d.commit();
                label_plan(&d, db, &path);
                succeeds(apply_plan(&d, db, &path, false));
                std::fs::write(&domain_file, &domain).unwrap();
                succeeds(d.run(&["plan", "--no-dev"]));
                d.commit();
                label_plan(&d, db, &path);
                succeeds(apply_plan(&d, db, &path, false));
                assert!(
                    label_plan(&d, db, &path).changes.is_empty(),
                    "{base} {kind}"
                );
            }
        }
        for (kind, definition) in [
            ("view", "SELECT 'new'::app.e AS m"),
            (
                "function",
                "() RETURNS app.e LANGUAGE sql AS $$ SELECT 'new'::app.e $$",
            ),
            (
                "function",
                "() RETURNS app.e LANGUAGE sql BEGIN ATOMIC SELECT 'new'::app.e; END",
            ),
        ] {
            let own = OwnDatabase::new(&server, "label_module1635");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('old')");
            let d = Demo::new("label-module1635");
            succeeds(d.run(&["pull", "--db", db, "--force"]));
            d.commit();
            succeeds(d.run(&[
                "baseline",
                "--db",
                db,
                "--reason",
                "adopt enum before module",
            ]));
            std::fs::write(
                d.dir.join("schema/app.e.type.yml"),
                "enum: app.e\nvalues: [old, new]\n",
            )
            .unwrap();
            let module_file = d.dir.join("schema/app.f.yml");
            let module = format!(
                "{kind}: app.f{}\ndefinition: |\n  {definition}\n",
                if kind == "function" { "()" } else { "" }
            );
            std::fs::write(&module_file, &module).unwrap();
            d.commit();
            let path = d.dir.join("module-label.plan.json");
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("apply the label first, then the rest"),
                "{}",
                stderr(&out)
            );
            assert!(!path.exists());
            std::fs::remove_file(&module_file).unwrap();
            d.commit();
            label_plan(&d, db, &path);
            succeeds(apply_plan(&d, db, &path, false));
            std::fs::write(&module_file, &module).unwrap();
            d.commit();
            label_plan(&d, db, &path);
            succeeds(apply_plan(&d, db, &path, false));
            assert!(label_plan(&d, db, &path).changes.is_empty());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_plpgsql_parameter_defaults_refuse_before_sql_and_succeed_after_a_label_only_plan() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for kind in ["function", "procedure"] {
            for default in ["DEFAULT 'new'", "DEFAULT 'new'::app.e", "= 'new'"] {
                let own = OwnDatabase::new(&server, "label_parameter1635");
                let db = own.connection();
                on_server(db, "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('old')");
                let d = Demo::new("label-parameter1635");
                succeeds(d.run(&["pull", "--db", db, "--force"]));
                d.commit();
                succeeds(d.run(&[
                    "baseline",
                    "--db",
                    db,
                    "--reason",
                    "adopt enum before parameter default",
                ]));
                std::fs::write(
                    d.dir.join("schema/app.e.type.yml"),
                    "enum: app.e\nvalues: [old, new]\n",
                )
                .unwrap();
                let (name, tail) = if kind == "function" {
                    (
                        "app.f",
                        "RETURNS app.e LANGUAGE plpgsql AS $$ BEGIN RETURN x; END; $$",
                    )
                } else {
                    ("app.p", "LANGUAGE plpgsql AS $$ BEGIN NULL; END; $$")
                };
                let module =
                    format!("{kind}: {name}(app.e)\ndefinition: |\n  (x app.e {default}) {tail}\n");
                let module_file = d.dir.join("schema/app.routine.yml");
                std::fs::write(&module_file, &module).unwrap();
                d.commit();
                let path = d.dir.join("parameter-label.plan.json");
                let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
                assert_ne!(code(&out), 0, "{kind}: {default}");
                assert!(
                    stderr(&out).contains(name)
                        && stderr(&out).contains("app.e")
                        && stderr(&out).contains("new")
                        && stderr(&out).contains("apply the label first, then the rest"),
                    "{kind}: {default}: {}",
                    stderr(&out)
                );
                assert!(!path.exists());
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_enum WHERE enumtypid='app.e'::regtype AND enumlabel='new'"
                    ),
                    0
                );
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_proc WHERE pronamespace='app'::regnamespace"
                    ),
                    0
                );
                assert_eq!(
                    scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
                    1
                );

                std::fs::remove_file(&module_file).unwrap();
                d.commit();
                let first = label_plan(&d, db, &path);
                assert!(
                    matches!(&first.changes.changes[..], [step] if matches!(step.change, pbps_model::Change::AddEnumValue { .. }))
                );
                succeeds(apply_plan(&d, db, &path, false));
                std::fs::write(&module_file, &module).unwrap();
                d.commit();
                let second = label_plan(&d, db, &path);
                assert!(
                    second
                        .changes
                        .changes
                        .iter()
                        .any(|step| matches!(step.change, pbps_model::Change::CreateModule { .. }))
                );
                assert!(
                    !second
                        .changes
                        .changes
                        .iter()
                        .any(|step| matches!(step.change, pbps_model::Change::AddEnumValue { .. }))
                );
                succeeds(apply_plan(&d, db, &path, false));
                if kind == "function" {
                    assert_eq!(
                        scalar(db, "SELECT count(*)::bigint WHERE app.f()::text='new'"),
                        1
                    );
                } else {
                    on_server(db, "CALL app.p()");
                }
                assert!(label_plan(&d, db, &path).changes.is_empty());
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_new_labels_admit_deferred_plpgsql_bodies_and_staged_add_and_rename_resume() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "label_plpgsql1635");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('old')");
        let d = Demo::new("label-plpgsql1635");
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "adopt enum before PL/pgSQL",
        ]));
        std::fs::write(
            d.dir.join("schema/app.e.type.yml"),
            "enum: app.e\nvalues: [old, new]\n",
        )
        .unwrap();
        std::fs::write(d.dir.join("schema/app.f.yml"), "function: app.f()\ndefinition: |\n  () RETURNS app.e LANGUAGE plpgsql AS $$ BEGIN RETURN 'new'::app.e; END $$\n").unwrap();
        d.commit();
        let path = d.dir.join("plpgsql-label.plan.json");
        label_plan(&d, db, &path);
        succeeds(apply_plan(&d, db, &path, false));
        assert!(label_plan(&d, db, &path).changes.is_empty());
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint WHERE app.f()::text='new'"),
            1
        );
        #[cfg(target_os = "linux")]
        for rename in [false, true] {
            let own = OwnDatabase::new(&server, "label_staged1635");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app");
            let d = Demo::new("label-staged1635");
            // Staged apply admits one logical change. Expression rebuilds
            // belong to the transactional fixture above (DEC-1635.6).
            if rename {
                d.table("table: app.t\ncolumns:\n  m: {type: app.e}\n");
            } else {
                d.table("table: app.t\ncolumns:\n  m: {type: app.e, default: \"'old'\"}\nchecks:\n  kept: m <> 'old'\n");
            }
            std::fs::write(
                d.dir.join("schema/app.e.type.yml"),
                "enum: app.e\nvalues: [old]\n",
            )
            .unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", db]));
            let file = d.dir.join("schema/app.e.type.yml");
            let yaml = if rename {
                "enum: app.e\nvalues: [{name: renamed, renamed_from: old}]\n"
            } else {
                "enum: app.e\nvalues: [old, staged]\n"
            };
            std::fs::write(&file, yaml).unwrap();
            d.commit();
            let path = d.dir.join("staged-label.plan.json");
            succeeds(d.run(&[
                "plan",
                "--db",
                db,
                "--staged",
                "--out",
                path.to_str().unwrap(),
            ]));
            let checksum = plan_checksum(&path);
            let mut args = vec![
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--staged",
            ];
            if rename {
                args.extend(["--allow", "rename"]);
            }
            let _checkpoint =
                super::abrupt_recovery::interrupt_after_type_checkpoint(db, &d, &args);
            args.push("--resume");
            succeeds(d.run(&args));
            succeeds(d.run(&["verify", "--db", db]));
            assert!(label_plan(&d, db, &path).changes.is_empty());
        }
    }
}

#[test]
fn enum_data_and_list_keys_keep_named_refusals_before_label_use_checks() {
    for ty in ["app.e", "app.e[]", "app.d"] {
        for partition in [false, true] {
            // A scalar declared enum keys a partitioned table (DEC-2346.1).
            if partition && ty == "app.e" {
                continue;
            }
            let d = Demo::new("enum-surfaces-deferred2346");
            let enum_file = d.dir.join("schema/app.e.type.yml");
            std::fs::write(&enum_file, "enum: app.e\nvalues: [old]\n").unwrap();
            std::fs::write(
                d.dir.join("schema/app.d.type.yml"),
                "domain: app.d\ntype: app.e\n",
            )
            .unwrap();
            succeeds(d.run(&["plan", "--no-dev"]));
            d.commit();
            std::fs::write(&enum_file, "enum: app.e\nvalues: [old, new]\n").unwrap();
            let tail = if partition {
                "partition_by: {list: [m]}\n"
            } else {
                "primary_key: [id]\ndata: {mode: ensure, rows: {1: {m: new}}}\n"
            };
            d.table(&format!("table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: '{ty}'}}\n{tail}"));
            let path = d.dir.join("refused.plan.json");
            // The connected route validates before opening any connection.
            // Offline previews are advisory on master; validate owns that check.
            let unreachable = "host=127.0.0.1 port=1 user=postgres dbname=unused sslmode=disable";
            for args in [
                vec!["validate"],
                vec!["plan", "--db", unreachable, "--out", path.to_str().unwrap()],
            ] {
                let out = d.run(&args);
                assert_ne!(code(&out), 0);
                let why = format!("{}\n{}", stdout(&out), stderr(&out));
                assert!(
                    why.contains("app.t")
                        && why.contains(if partition {
                            "partition key `m`"
                        } else {
                            "column `m`"
                        })
                        && (partition || why.contains(ty)),
                    "{why}"
                );
                assert!(why.contains("user-defined type reference"), "{why}");
                assert!(
                    !why.contains("apply the label first, then the rest"),
                    "{why}"
                );
                assert!(!path.exists());
            }
        }
    }
}

// ---- #1635 slice 3b: removing and reordering labels rebuilds the enum ----

const MOOD_SQL: &str = "CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
COMMENT ON TYPE app.mood IS 'how it feels';
CREATE TABLE app.u (m app.mood PRIMARY KEY);
INSERT INTO app.u VALUES ('ok'),('happy');
CREATE TABLE app.t (
  id int PRIMARY KEY,
  m app.mood NOT NULL DEFAULT 'ok' REFERENCES app.u (m),
  tags app.mood[],
  CONSTRAINT m_known CHECK (m IN ('ok','happy')));
CREATE INDEX t_ok ON app.t (id) WHERE m = 'ok';
INSERT INTO app.t VALUES (1,'ok','{ok,happy}'),(2,'happy',NULL);
CREATE TABLE app.events (
  id int NOT NULL,
  m app.mood DEFAULT 'ok',
  CONSTRAINT ev_known CHECK (m IN ('ok','happy'))) PARTITION BY LIST (id);
CREATE TABLE app.events_1 PARTITION OF app.events FOR VALUES IN (1);
INSERT INTO app.events VALUES (1,'happy');";

/// A project adopted from a database that holds `sql`.
fn adopted(server: &str, slug: &str, sql: &str) -> (OwnDatabase, Demo) {
    let own = OwnDatabase::new(server, slug);
    on_server(own.connection(), sql);
    let d = Demo::new(slug);
    succeeds(d.run(&["pull", "--db", own.connection(), "--force"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        own.connection(),
        "--reason",
        "adopt the enum before its rebuild",
    ]));
    (own, d)
}

fn declare_mood(d: &Demo, values: &str) {
    std::fs::write(
        d.dir.join("schema/app.mood.type.yml"),
        format!("enum: app.mood\nvalues: {values}\n"),
    )
    .unwrap();
    d.commit();
}

fn type_ids(d: &Demo) -> serde_json::Value {
    let ids: serde_json::Value =
        serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
    ids["types"].clone()
}

/// Whether `app.mood` holds exactly `labels`, in this order.
fn mood_is(db: &str, labels: &[&str]) -> bool {
    let ordered = labels
        .iter()
        .enumerate()
        .map(|(i, l)| format!("(n = {} AND enumlabel = '{l}')", i + 1))
        .collect::<Vec<_>>()
        .join(" OR ");
    scalar(
        db,
        &format!(
            "SELECT count(*)::bigint FROM (SELECT enumlabel, row_number() OVER (ORDER BY enumsortorder) AS n \
             FROM pg_enum WHERE enumtypid = 'app.mood'::regtype) x WHERE {ordered}"
        ),
    ) == labels.len() as i64
        && scalar(
            db,
            "SELECT count(*)::bigint FROM pg_enum WHERE enumtypid = 'app.mood'::regtype",
        ) == labels.len() as i64
}

fn no_replacement_left(db: &str) -> bool {
    scalar(
        db,
        "SELECT count(*)::bigint FROM pg_type WHERE typname LIKE 'pbps_rebuild_%'",
    ) == 0
}

/// Everything a rebuild's plan asks for beside its own class: the checks, keys
/// and indexes it restores are gated as they are in any plan.
const REBUILD_ALLOW: [&str; 6] = [
    "--allow",
    "type-rebuild",
    "--allow",
    "constraint",
    "--allow",
    "destructive",
];

fn apply_rebuild(d: &Demo, db: &str, plan: &std::path::Path, extra: &[&str]) -> Output {
    let checksum = plan_checksum(plan);
    let mut args = vec![
        "apply",
        "--db",
        db,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    args.extend(REBUILD_ALLOW);
    args.extend(extra);
    d.run(&args)
}

fn rebuild_steps(plan: &pbps_model::SavedPlan) -> usize {
    plan.changes
        .changes
        .iter()
        .filter(|p| matches!(p.change, pbps_model::Change::EnumRebuild { .. }))
        .count()
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_removes_and_reorders_labels_under_the_same_uid_and_converges() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "rebuild_labels1635", MOOD_SQL);
        let db = own.connection();
        let uids = type_ids(&d);
        assert_eq!(uids.as_object().unwrap().len(), 1);
        // `sad` goes, and `happy` now sorts before `ok`.
        declare_mood(&d, "[happy, ok]");
        let path = d.dir.join("rebuild.plan.json");
        let plan = label_plan(&d, db, &path);
        // Create, the retypes of t.m, t.tags, u.m and the partitioned root's m
        // (never a partition), and the swap.
        assert_eq!(rebuild_steps(&plan), 6, "{:#?}", plan.changes);
        assert!(
            plan.changes
                .risks()
                .contains(&pbps_model::RiskClass::TypeRebuild)
        );
        assert!(plan.changes.changes.iter().any(|p| matches!(
            p.change,
            pbps_model::Change::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Swap {
                    comment: Some(_),
                    ..
                },
                ..
            }
        )));

        // Without the class: refused by name, and nothing ran.
        let checksum = plan_checksum(&path);
        let out = d.run(&[
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--allow",
            "constraint",
            "--allow",
            "destructive",
        ]);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("type-rebuild"), "{}", stderr(&out));
        assert!(mood_is(db, &["sad", "ok", "happy"]));
        assert!(no_replacement_left(db));

        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert!(no_replacement_left(db));
        // The comment, the rows, the defaults and the checks, the key from
        // enum to enum and the partial index all came back.
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_description WHERE objoid = 'app.mood'::regtype AND description = 'how it feels'"
            ),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE m = 'ok' AND tags @> ARRAY['happy']::app.mood[]"
            ),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.events WHERE m = 'happy'"
            ),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_attrdef WHERE adrelid IN ('app.t'::regclass, 'app.events'::regclass)"
            ),
            2
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_constraint WHERE conname IN ('m_known','ev_known') AND convalidated AND conrelid IN ('app.t'::regclass, 'app.events'::regclass)"
            ),
            2
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_constraint WHERE contype = 'f' AND conrelid = 'app.t'::regclass"
            ),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_indexes WHERE schemaname = 'app' AND indexname = 't_ok'"
            ),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_attribute WHERE attrelid IN ('app.t'::regclass, 'app.u'::regclass, 'app.events'::regclass, 'app.events_1'::regclass) AND atttypid IN ('app.mood'::regtype, 'app.mood[]'::regtype)"
            ),
            5
        );
        // The uid is the one the project recorded, and the next plan is clean.
        assert_eq!(type_ids(&d), uids);
        assert!(label_plan(&d, db, &path).changes.is_empty());
        for _ in 0..2 {
            succeeds(d.run(&["pull", "--db", db, "--force"]));
            let pulled = pbps_load::load_schema_dir(&d.dir.join("schema"))
                .unwrap()
                .schema;
            assert_eq!(
                pulled.types[&"app.mood".parse().unwrap()],
                ManagedType::Enum {
                    values: vec!["happy".into(), "ok".into()]
                }
            );
        }
        assert_eq!(type_ids(&d), uids);
        assert!(label_plan(&d, db, &path).changes.is_empty());

        // A reorder alone removes nothing and rebuilds just the same.
        declare_mood(&d, "[ok, happy]");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 6);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["ok", "happy"]));
        assert!(label_plan(&d, db, &path).changes.is_empty());
        assert_eq!(type_ids(&d), uids);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_refuses_rows_holding_a_removed_label_with_a_count_per_column() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood, tags app.mood[]);
            INSERT INTO app.t VALUES (1,'sad','{sad,ok,sad}'),(2,'sad',NULL),(3,'ok','{happy}');";
        let (own, d) = adopted(&server, "rebuild_rows1635", sql);
        let db = own.connection();
        declare_mood(&d, "[ok, happy]");
        let path = d.dir.join("rows.plan.json");
        let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            text.contains("2 rows of app.t.m holding a label this plan removes")
                && text.contains("2 elements of app.t.tags holding a label this plan removes")
                && text.contains("sad"),
            "{text}"
        );
        assert!(!path.exists());
        assert!(mood_is(db, &["sad", "ok", "happy"]));
        assert!(no_replacement_left(db));
        // Once no row holds the label, the same declaration plans.
        on_server(
            db,
            "UPDATE app.t SET m = 'ok', tags = NULL WHERE id IN (1, 2)",
        );
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 4);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["ok", "happy"]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_refuses_each_dependent_it_cannot_restore_by_name_before_any_sql() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let base = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood);
            INSERT INTO app.t VALUES (1,'ok');";
        for (slug, extra, expected) in [
            ("domain", "CREATE DOMAIN app.d AS app.mood;", "domain app.d"),
            (
                "signature",
                "CREATE FUNCTION app.f(x app.mood) RETURNS int LANGUAGE sql AS $$ SELECT 1 $$;",
                "as an argument",
            ),
            (
                "policy",
                "ALTER TABLE app.t ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY p ON app.t USING (m <> 'sad');",
                "a policy",
            ),
            (
                "generated_reader",
                "ALTER TABLE app.t ADD COLUMN g boolean GENERATED ALWAYS AS (m = 'ok') STORED;",
                "generated column",
            ),
            (
                "generated_enum",
                "ALTER TABLE app.t ADD COLUMN g app.mood GENERATED ALWAYS AS ('ok'::app.mood) STORED;",
                "generated column of this enum",
            ),
            (
                "composite",
                "CREATE TYPE app.pair AS (x app.mood); CREATE TABLE app.c (p app.pair);",
                "composite type",
            ),
            (
                "privileges",
                "REVOKE USAGE ON TYPE app.mood FROM PUBLIC;",
                "explicit privilege list",
            ),
        ] {
            let (own, d) = adopted(
                &server,
                &format!("rebuild_refuses_{slug}1635"),
                &format!("{base} {extra}"),
            );
            let db = own.connection();
            declare_mood(&d, "[ok, happy]");
            let path = d.dir.join("refused.plan.json");
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0, "{slug}");
            let text = format!("{}{}", stdout(&out), stderr(&out));
            assert!(text.contains(expected), "{slug}: {text}");
            assert!(!path.exists(), "{slug}");
            assert!(mood_is(db, &["sad", "ok", "happy"]), "{slug}");
            assert!(no_replacement_left(db), "{slug}");
            assert_eq!(
                scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
                1,
                "{slug}"
            );
        }
        // A type owned by another role would lose its owner in the copy.
        let role = format!("pbps_rebuild_owner_{}", std::process::id());
        on_server(
            &server,
            &format!("DROP ROLE IF EXISTS {role}; CREATE ROLE {role}"),
        );
        let (own, d) = adopted(&server, "rebuild_refuses_owner1635", base);
        let db = own.connection();
        on_server(db, &format!("ALTER TYPE app.mood OWNER TO {role}"));
        succeeds(d.run(&["baseline", "--db", db, "--reason", "owner changed"]));
        declare_mood(&d, "[ok, happy]");
        let out = d.run(&[
            "plan",
            "--db",
            db,
            "--out",
            d.dir.join("owner.plan.json").to_str().unwrap(),
        ]);
        let text = format!("{}{}", stdout(&out), stderr(&out));
        drop(own);
        on_server(&server, &format!("DROP ROLE IF EXISTS {role}"));
        assert_ne!(code(&out), 0);
        assert!(
            text.contains("owned by a role other than the one deploying"),
            "{text}"
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_rebuilds_views_bodies_and_triggers_over_its_columns() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL);
            INSERT INTO app.t VALUES (1,'ok'),(2,'happy');
            CREATE VIEW app.glad AS SELECT id, m FROM app.t WHERE m = 'happy'::app.mood;
            CREATE VIEW app.moods AS SELECT m FROM app.t;
            CREATE VIEW app.moods2 AS SELECT m FROM app.moods;
            CREATE FUNCTION app.n() RETURNS int LANGUAGE sql
              BEGIN ATOMIC SELECT count(*)::int FROM app.t WHERE m = 'ok'; END;
            CREATE FUNCTION app.s() RETURNS bigint LANGUAGE sql AS $$ SELECT count(*) FROM app.t $$;
            CREATE FUNCTION app.tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
            CREATE TRIGGER tr BEFORE UPDATE OF m ON app.t FOR EACH ROW EXECUTE FUNCTION app.tf();";
        let (own, d) = adopted(&server, "rebuild_modules1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        let path = d.dir.join("modules.plan.json");
        let plan = label_plan(&d, db, &path);
        let rebuilt: Vec<_> = plan
            .changes
            .changes
            .iter()
            .filter(|p| matches!(p.change, pbps_model::Change::AlterModule { .. }))
            .collect();
        // The views and the body that name a moved column, and the trigger,
        // are rebuilt in place; the view over a view comes down and goes back
        // beside them; the string-bodied function is left alone.
        let touched = plan
            .changes
            .changes
            .iter()
            .filter(|p| {
                matches!(
                    p.change,
                    pbps_model::Change::DropModule { .. } | pbps_model::Change::CreateModule { .. }
                )
            })
            .count();
        assert_eq!((rebuilt.len(), touched), (0, 10), "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.glad"), 1);
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.moods2"), 2);
        assert_eq!(scalar(db, "SELECT app.n()::bigint"), 1);
        assert_eq!(scalar(db, "SELECT app.s()"), 2);
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_trigger WHERE tgname = 'tr' AND NOT tgisinternal"
            ),
            1
        );
        on_server(db, "UPDATE app.t SET m = 'happy' WHERE id = 1");
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_after_a_rename_in_the_same_plan_maps_labels_by_their_final_names() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood, tags app.mood[]);
            INSERT INTO app.t VALUES (1,'sad','{sad,ok}'),(2,'ok',NULL);";
        let (own, d) = adopted(&server, "rebuild_rename1635", sql);
        let db = own.connection();
        // `sad` becomes `blue`; `happy` goes; the order changes too.
        declare_mood(&d, "[ok, {name: blue, renamed_from: sad}]");
        let path = d.dir.join("rename-rebuild.plan.json");
        let plan = label_plan(&d, db, &path);
        assert!(
            plan.changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::RenameEnumValue { .. }))
        );
        assert_eq!(rebuild_steps(&plan), 4);
        succeeds(apply_rebuild(&d, db, &path, &["--allow", "rename"]));
        assert!(mood_is(db, &["ok", "blue"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE m = 'blue' AND tags = ARRAY['blue','ok']::app.mood[]"
            ),
            1
        );
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.t WHERE m = 'ok'"),
            1
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

/// A staged rebuild of `app.mood` that has made the replacement and moved every
/// column to it, with the swap held back by a lock on the old type.
struct HeldAtItsSwap {
    child: std::process::Child,
    release: std::sync::mpsc::Sender<()>,
    gate: std::thread::JoinHandle<()>,
    args: Vec<String>,
}

impl HeldAtItsSwap {
    /// The process is killed and the run unlocked; the arguments that resume
    /// it come back.
    fn killed(mut self, d: &Demo, db: &str) -> Vec<String> {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.release.send(()).unwrap();
        self.gate.join().unwrap();
        succeeds(d.run(&["unlock", "--db", db]));
        self.args
    }

    /// The lock goes, and the run goes on to its swap and its closing read.
    fn released(mut self) -> std::process::ExitStatus {
        self.release.send(()).unwrap();
        self.gate.join().unwrap();
        self.child.wait().unwrap()
    }
}

fn staged_rebuild_held_at_its_swap(d: &Demo, db: &str, path: &std::path::Path) -> HeldAtItsSwap {
    succeeds(d.run(&[
        "plan",
        "--db",
        db,
        "--staged",
        "--out",
        path.to_str().unwrap(),
    ]));
    let checksum = plan_checksum(path);
    let args = [
        "apply",
        "--db",
        db,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "type-rebuild",
        "--staged",
    ];
    // A lock on the old type holds the swap's `DROP TYPE`, so the process
    // stands after the replacement exists and both columns have moved.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (ready, locked) = std::sync::mpsc::channel::<()>();
    let gate = {
        let db = db.to_owned();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &db)
                        .await
                        .unwrap();
                    conn.execute("BEGIN; COMMENT ON TYPE app.mood IS 'held'")
                        .await
                        .unwrap();
                    ready.send(()).unwrap();
                    held.recv().unwrap();
                    conn.execute("ROLLBACK").await.unwrap();
                });
        })
    };
    locked.recv().unwrap();
    let mut child = std::process::Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(args)
        .stdout(std::fs::File::create(d.dir.join("staged.stdout")).unwrap())
        .stderr(std::fs::File::create(d.dir.join("staged.stderr")).unwrap())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        let done = latest_snapshot(db).staged.map_or(0, |s| s.completed);
        if done >= 3 {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the apply ended early: {}{}",
            std::fs::read_to_string(d.dir.join("staged.stdout")).unwrap_or_default(),
            std::fs::read_to_string(d.dir.join("staged.stderr")).unwrap_or_default()
        );
        assert!(
            start.elapsed().as_secs() < 120,
            "the staged steps never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Mid-rebuild: the replacement and the old type both stand, and both
    // columns hold the replacement.
    assert_eq!(
        scalar(
            db,
            "SELECT count(*)::bigint FROM pg_type WHERE typname LIKE 'pbps_rebuild_%' AND typtype = 'e'"
        ),
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*)::bigint FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid OR t.typarray = a.atttypid WHERE a.attrelid = 'app.t'::regclass AND t.typname LIKE 'pbps_rebuild_%'"
        ),
        2
    );
    HeldAtItsSwap {
        child,
        release,
        gate,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_stages_and_resumes_across_its_steps() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int, m app.mood, tags app.mood[]);
            INSERT INTO app.t VALUES (1,'ok','{happy}'),(2,'happy',NULL);";
        let (own, d) = adopted(&server, "rebuild_staged1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        let path = d.dir.join("staged-rebuild.plan.json");
        let args = staged_rebuild_held_at_its_swap(&d, db, &path).killed(&d, db);
        let mut resume = args.clone();
        resume.push("--resume".into());
        succeeds(d.run(&resume.iter().map(String::as_str).collect::<Vec<_>>()));
        assert!(mood_is(db, &["happy", "ok"]));
        assert!(no_replacement_left(db));
        succeeds(d.run(&["verify", "--db", db]));
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_apply_refuses_a_dependent_created_after_the_plan_by_name_and_applies_nothing() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int, m app.mood, tags app.mood[]);
            INSERT INTO app.t VALUES (1,'ok','{happy}'),(2,'happy',NULL);";
        let (own, d) = adopted(&server, "rebuild_late1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        let applied = scalar(
            db,
            "SELECT count(*)::bigint FROM public.__pbps_state WHERE kind = 'apply'",
        );
        for staged in [false, true] {
            let path = d.dir.join("late.plan.json");
            let mut plan = vec!["plan", "--db", db, "--out", path.to_str().unwrap()];
            if staged {
                plan.push("--staged");
            }
            succeeds(d.run(&plan));
            // Another session adds what the plan never saw.
            on_server(db, "CREATE VIEW app.sneaky AS SELECT m FROM app.t");
            let checksum = plan_checksum(&path);
            let mut apply = vec![
                "apply",
                "--db",
                db,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
            ];
            apply.extend(REBUILD_ALLOW);
            if staged {
                apply.push("--staged");
            }
            let out = d.run(&apply);
            assert_ne!(code(&out), 0, "{}", stdout(&out));
            let text = format!("{}{}", stdout(&out), stderr(&out));
            assert!(
                text.contains("app.sneaky") && text.contains("after the plan"),
                "{text}"
            );
            // Nothing ran: the old type stands and no replacement was made. A
            // refusal inside the apply's transaction is recorded as the failed
            // attempt it is, as any other is; no applied state follows it.
            assert!(mood_is(db, &["sad", "ok", "happy"]), "{text}");
            assert!(no_replacement_left(db));
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM public.__pbps_state WHERE kind = 'apply'"
                ),
                applied,
                "{text}"
            );
            on_server(db, "DROP VIEW app.sneaky");
        }
        // Once the stranger is gone, the same plan applies.
        let path = d.dir.join("late.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
    }
}

/// Rewrites, by `edit`, the one declaration file under `schema/` that holds
/// `needle`, and commits.
fn redeclare(d: &Demo, needle: &str, edit: impl Fn(&str) -> String) {
    let files: Vec<_> = std::fs::read_dir(d.dir.join("schema"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains(needle)))
        .collect();
    assert_eq!(files.len(), 1, "{needle}: {files:?}");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let edited = edit(&text);
    assert_ne!(text, edited, "{needle}: the edit changed nothing in {text}");
    std::fs::write(&files[0], edited).unwrap();
    d.commit();
}

fn modules_in(plan: &pbps_model::SavedPlan) -> (usize, usize, usize) {
    let count = |f: fn(&pbps_model::Change) -> bool| {
        plan.changes.changes.iter().filter(|p| f(&p.change)).count()
    };
    (
        count(|c| matches!(c, pbps_model::Change::DropModule { .. })),
        count(|c| matches!(c, pbps_model::Change::CreateModule { .. })),
        count(|c| matches!(c, pbps_model::Change::AlterModule { .. })),
    )
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_applies_with_a_view_the_same_plan_redefines() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL);
            INSERT INTO app.t VALUES (1,'ok'),(2,'happy'),(3,'ok');
            CREATE VIEW app.glad AS SELECT id, m FROM app.t;";
        let (own, d) = adopted(&server, "rebuild_redefined_view1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        // The view's text changes in the same plan as the labels.
        redeclare(&d, "app.glad", |text| {
            text.replace("FROM app.t", "FROM app.t WHERE id > 1")
        });
        let path = d.dir.join("redefined-view.plan.json");
        let plan = label_plan(&d, db, &path);
        // Down before the retypes, back after the swap with its new text; never
        // restated in place, which would run with the old view standing.
        assert_eq!(modules_in(&plan), (1, 1, 0), "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_views WHERE schemaname = 'app' AND viewname = 'glad' AND definition LIKE '%id > 1%'"
            ),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.glad WHERE m = 'happy'"
            ),
            1
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.glad"), 2);
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_applies_with_a_routine_body_the_same_plan_changes() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL);
            INSERT INTO app.t VALUES (1,'ok'),(2,'happy'),(3,'ok');
            CREATE FUNCTION app.n() RETURNS int LANGUAGE sql
              BEGIN ATOMIC SELECT count(*)::int FROM app.t WHERE m = 'ok'; END;";
        let (own, d) = adopted(&server, "rebuild_redefined_body1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        // The body counts another label from now on.
        redeclare(&d, "app.n()", |text| {
            text.replace("'ok'::app.mood", "'happy'::app.mood")
        });
        let path = d.dir.join("redefined-body.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(modules_in(&plan), (1, 1, 0), "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(scalar(db, "SELECT app.n()::bigint"), 1);
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_applies_with_a_default_check_and_index_the_same_plan_changes() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL DEFAULT 'ok',
              CONSTRAINT m_known CHECK (m IN ('ok','happy')));
            CREATE INDEX t_m ON app.t (id) WHERE m = 'ok';
            INSERT INTO app.t VALUES (1,'happy'),(2,'happy');";
        let (own, d) = adopted(&server, "rebuild_redefined_parts1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        // The default, the check and the index filter all spell another label.
        redeclare(&d, "m_known", |text| {
            text.replace("'ok'::app.mood", "'happy'::app.mood")
        });
        let path = d.dir.join("redefined-parts.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 3);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        on_server(db, "INSERT INTO app.t (id) VALUES (3)");
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.t WHERE m = 'happy'"),
            3
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_indexes WHERE schemaname = 'app' AND indexname = 't_m' AND indexdef LIKE '%happy%'"
            ),
            1
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_applies_beside_a_table_and_column_rename_over_a_default_and_check() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL DEFAULT 'ok',
              CONSTRAINT m_known CHECK (m IN ('ok','happy')));
            CREATE INDEX t_m ON app.t (id) WHERE m = 'ok';
            INSERT INTO app.t VALUES (1,'happy'),(2,'ok');";
        let (own, d) = adopted(&server, "rebuild_renamed_parts1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        // The catalog still has `app.t.m` when the plan is made; the drops and
        // restores of its default, check and index are written for the names
        // the declarations leave.
        redeclare(&d, "table: app.t\n", |text| {
            text.replace("table: app.t\n", "table: app.tt\nrenamed_from: app.t\n")
                .replace("  m:\n", "  mood:\n    renamed_from: m\n")
                .replace("(m = ", "(mood = ")
        });
        // Identity for the renames is recorded locally and committed.
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = d.dir.join("renamed-parts.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 3, "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &["--allow", "rename"]));
        assert!(mood_is(db, &["happy", "ok"]));
        on_server(db, "INSERT INTO app.tt (id) VALUES (3)");
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.tt WHERE mood = 'ok'"),
            2
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

/// The check is judged by the engine against the replacement's label order, so
/// the plan-time probe, which reads the old order, must not decide it.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_adds_an_order_sensitive_check_judged_by_the_new_label_order() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL);
            INSERT INTO app.t VALUES (1,'happy');";
        let (own, d) = adopted(&server, "rebuild_order_check1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        // `happy` is above `ok` today and below it once the rebuild has run.
        redeclare(&d, "table: app.t\n", |text| {
            format!("{text}checks:\n  m_low: m <= 'ok'::app.mood\n")
        });
        let path = d.dir.join("order-check.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 3, "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_constraint WHERE conname = 'm_low'"
            ),
            1
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());

        // A check the stored data really violates under the new order is still
        // refused. The plan-time probe stands aside for a retyped column, so
        // the engine refuses it at apply: the transaction rolls back, the old
        // type stands and no apply is recorded.
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood NOT NULL);
            INSERT INTO app.t VALUES (1,'ok');";
        let (own, d) = adopted(&server, "rebuild_order_check_bad1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        redeclare(&d, "table: app.t\n", |text| {
            format!("{text}checks:\n  m_low: m <= 'happy'::app.mood\n")
        });
        let applied = scalar(
            db,
            "SELECT count(*)::bigint FROM public.__pbps_state WHERE kind = 'apply'",
        );
        let path = d.dir.join("order-check-bad.plan.json");
        label_plan(&d, db, &path);
        let out = apply_rebuild(&d, db, &path, &[]);
        assert_ne!(code(&out), 0, "{}", stdout(&out));
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(text.contains("m_low"), "{text}");
        assert!(mood_is(db, &["sad", "ok", "happy"]), "{text}");
        assert!(no_replacement_left(db));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            applied,
            "{text}"
        );
    }
}

/// `app.parent` as the partition test pulls it, with a check of the
/// partition's own under its entry in the parent's file (DEC-1577.1).
fn partition_check(d: &Demo, expression: &str) {
    std::fs::write(
        d.dir.join("schema/app.parent.yml"),
        format!(
            "table: app.parent\ncolumns:\n  id: {{type: integer, nullable: false}}\n  \
             m: {{type: app.mood, nullable: false}}\npartition_by: [id]\npartitions:\n  \
             child:\n    from: ['1']\n    to: ['2']\n    checks:\n      m_low: {expression}\n"
        ),
    )
    .unwrap();
    d.commit();
}

/// The rebuild retypes the parent's column and the engine recurses that into
/// every partition, so a check added on a *partition* in the same plan is judged
/// under the new label order too, and the plan-time probe, which carries the
/// partition map the apply carries, does not refuse it against the old one
/// (#2406).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_partition_check_is_judged_by_the_new_label_order() {
    const PARTITIONED: &str = "CREATE SCHEMA app;
        CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
        CREATE TABLE app.parent (id int NOT NULL, m app.mood NOT NULL) PARTITION BY RANGE (id);
        CREATE TABLE app.child PARTITION OF app.parent FOR VALUES FROM (1) TO (2);";
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        // `happy` is above `ok` today and below it once the rebuild has run.
        let (own, d) = adopted(
            &server,
            "rebuild_partition_check1635",
            &format!("{PARTITIONED} INSERT INTO app.parent VALUES (1,'happy');"),
        );
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        partition_check(&d, "m <= 'ok'::app.mood");
        let path = d.dir.join("partition-check.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 3, "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_constraint \
                 WHERE conname = 'm_low' AND conrelid = 'app.child'::regclass"
            ),
            1
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());

        // A partition check the stored data really violates under the new
        // order is still refused: the plan-time probe stands aside for a
        // retyped column, so the engine refuses it at apply, the transaction
        // rolls back and the old type stands.
        let (own, d) = adopted(
            &server,
            "rebuild_partition_check_bad1635",
            &format!("{PARTITIONED} INSERT INTO app.parent VALUES (1,'ok');"),
        );
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        partition_check(&d, "m <= 'happy'::app.mood");
        let path = d.dir.join("partition-check-bad.plan.json");
        label_plan(&d, db, &path);
        let out = apply_rebuild(&d, db, &path, &[]);
        assert_ne!(code(&out), 0, "{}", stdout(&out));
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(text.contains("m_low"), "{text}");
        assert!(mood_is(db, &["sad", "ok", "happy"]), "{text}");
        assert!(no_replacement_left(db));
    }
}

/// A column the plan drops, with intent, no longer holds the old type when the
/// swap runs (classes 5 and 6 come before the create and the swap), so it is
/// neither retyped nor refused as a blocker, and the default, check and index
/// that stood on it go with it and are not restored.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_drops_a_column_and_its_parts_in_the_plan_that_removes_a_label() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int PRIMARY KEY, m app.mood DEFAULT 'sad', n app.mood,
              CONSTRAINT m_known CHECK (m IN ('sad','ok')));
            CREATE INDEX t_m ON app.t (id) WHERE m = 'sad';
            INSERT INTO app.t VALUES (1,'sad','ok'),(2,'sad','happy');";
        let (own, d) = adopted(&server, "rebuild_dropped_col1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        redeclare(&d, "table: app.t\n", |text| {
            text.replace(
                "  m:\n    type: app.mood\n    default: \"'sad'::app.mood\"\n",
                "",
            )
            .replace(
                "checks:\n  m_known: \"(m = ANY (ARRAY['sad'::app.mood, 'ok'::app.mood]))\"\n\n",
                "",
            )
            .replace(
                "indexes:\n  t_m:\n    columns: [id]\n    where: \"(m = 'sad'::app.mood)\"\n",
                "",
            )
        });
        // Without the intent the drop is not planned at all.
        succeeds(d.run(&["intent", "--drop", "app.t.m", "column retired"]));
        d.commit();
        let path = d.dir.join("dropped-column.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 3, "{:#?}", plan.changes);
        assert!(
            plan.changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::DropColumn { .. })),
            "{:#?}",
            plan.changes
        );
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'm' AND NOT attisdropped"
            ),
            0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_constraint WHERE conname = 'm_known'"
            ),
            0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_indexes WHERE indexname = 't_m'"
            ),
            0
        );
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.t WHERE n = 'happy'"),
            1
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

/// The same for a table the plan drops: its column, default, check and index
/// go with it. A partitioned table cannot be dropped by a plan yet, so its
/// partitions never reach the census this way.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_drops_a_table_holding_the_enum_in_the_plan_that_removes_a_label() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.keep (id int PRIMARY KEY, n app.mood);
            CREATE TABLE app.gone (id int PRIMARY KEY, m app.mood DEFAULT 'sad', tags app.mood[],
              CONSTRAINT gone_known CHECK (m IN ('sad','ok')));
            CREATE INDEX gone_m ON app.gone (id) WHERE m = 'sad';
            INSERT INTO app.keep VALUES (1,'ok'),(2,'happy');
            INSERT INTO app.gone VALUES (1,'sad','{sad}'),(2,'sad',NULL);";
        let (own, d) = adopted(&server, "rebuild_dropped_table1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        std::fs::remove_file(d.dir.join("schema/app.gone.yml")).unwrap();
        succeeds(d.run(&["intent", "--drop-table", "app.gone", "table retired"]));
        d.commit();
        let path = d.dir.join("dropped-table.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(rebuild_steps(&plan), 3, "{:#?}", plan.changes);
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_class WHERE relname = 'gone' AND relnamespace = 'app'::regnamespace"
            ),
            0
        );
        assert!(no_replacement_left(db));
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_rebuilds_a_view_over_a_partitions_column_and_refuses_one_it_cannot() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        // The view reads the partition's column and shows text: nothing in its
        // output is the enum, and the root's retype is what it stands in the way of.
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.parent (id int NOT NULL, m app.mood NOT NULL) PARTITION BY LIST (id);
            CREATE TABLE app.child PARTITION OF app.parent FOR VALUES IN (1);
            INSERT INTO app.parent VALUES (1,'ok'),(1,'happy');
            CREATE VIEW app.shown AS SELECT m::text AS m FROM app.child;";
        let (own, d) = adopted(&server, "rebuild_partition_view1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        let path = d.dir.join("partition-view.plan.json");
        let plan = label_plan(&d, db, &path);
        assert_eq!(modules_in(&plan), (1, 1, 0), "{:#?}", plan.changes);
        // Another session adds a second reader the plan never saw: refused by
        // name before any statement, not by the engine halfway through.
        on_server(
            db,
            "CREATE VIEW app.sneaky AS SELECT m::text AS m FROM app.child",
        );
        let out = apply_rebuild(&d, db, &path, &[]);
        assert_ne!(code(&out), 0, "{}", stdout(&out));
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            text.contains("app.sneaky") && text.contains("after the plan"),
            "{text}"
        );
        assert!(mood_is(db, &["sad", "ok", "happy"]), "{text}");
        assert!(no_replacement_left(db));
        on_server(db, "DROP VIEW app.sneaky");
        succeeds(apply_rebuild(&d, db, &path, &[]));
        assert!(mood_is(db, &["happy", "ok"]));
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.shown"), 2);
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.shown WHERE m = 'ok'"),
            1
        );
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_rebuild_staged_closing_read_refuses_a_moved_column_another_session_retyped() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE TYPE app.mood AS ENUM ('sad','ok','happy');
            CREATE TABLE app.t (id int, m app.mood, tags app.mood[]);
            INSERT INTO app.t VALUES (1,'ok','{happy}'),(2,'happy',NULL);";
        let (own, d) = adopted(&server, "rebuild_staged_retyped1635", sql);
        let db = own.connection();
        declare_mood(&d, "[happy, ok]");
        let path = d.dir.join("staged-retyped.plan.json");
        let held = staged_rebuild_held_at_its_swap(&d, db, &path);
        // Between the column's retype and the closing read, another session
        // moves it on to text. The swap has nothing to refuse it by: the old
        // type no longer holds the column.
        on_server(
            db,
            "ALTER TABLE app.t ALTER COLUMN m TYPE text USING m::text",
        );
        assert!(!held.released().success());
        let text = [
            std::fs::read_to_string(d.dir.join("staged.stdout")).unwrap_or_default(),
            std::fs::read_to_string(d.dir.join("staged.stderr")).unwrap_or_default(),
        ]
        .concat();
        assert!(
            text.contains("app.t")
                && text.contains("`m`")
                && text.contains("does not have the type this plan gives it"),
            "{text}"
        );
        // The column the session left alone was held to its type too, and
        // passed: only the one it moved is named.
        assert!(!text.contains("`tags`"), "{text}");
    }
}

// ---- #2346 PR 1: a declared scalar enum keys a partitioned table ----

/// A LIST- or RANGE-partitioned `app.ev` keyed by the declared enum `app.mood`
/// (`values`), with `partitions` as the YAML under `partitions:`.
fn enum_keyed(d: &Demo, list: bool, values: &str, partitions: &str) {
    std::fs::write(
        d.dir.join("schema/app.mood.type.yml"),
        format!("enum: app.mood\nvalues: {values}\n"),
    )
    .unwrap();
    let key = if list { "{list: [m]}" } else { "[m]" };
    std::fs::write(
        d.dir.join("schema/app.ev.yml"),
        format!(
            "table: app.ev\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: app.mood, nullable: false}}\nprimary_key: [id, m]\npartition_by: {key}\npartitions:\n{partitions}"
        ),
    )
    .unwrap();
}

/// The bound of the partition named `name`, as the engine deparses it.
fn bound_of(db: &str, name: &str) -> String {
    text_of(
        db,
        &format!(
            "SELECT pg_catalog.pg_get_expr(relpartbound, oid) FROM pg_class WHERE oid = 'app.{name}'::regclass"
        ),
    )
}

fn changes_of(plan: &pbps_model::SavedPlan) -> Vec<String> {
    plan.changes
        .changes
        .iter()
        .map(|p| {
            format!("{:?}", p.change)
                .split([' ', '{', '('])
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_list_and_range_partition_keys_converge_with_their_type_in_one_plan() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for list in [true, false] {
            let own = OwnDatabase::new(&server, "enum_key2346");
            let db = own.connection();
            on_server(db, "CREATE SCHEMA app");
            let d = Demo::new("enum-key2346");
            succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
            let (sad, ok, happy) = if list {
                (
                    "  ev_sad: {in: [sad]}\n",
                    "  ev_ok: {in: [ok]}\n",
                    "  ev_happy: {in: [happy]}\n",
                )
            } else {
                (
                    "  ev_sad: {from: [minvalue], to: [ok]}\n",
                    "  ev_ok: {from: [ok], to: [happy]}\n",
                    "  ev_happy: {from: [happy], to: [maxvalue]}\n",
                )
            };
            enum_keyed(&d, list, "[sad, ok, happy]", &format!("{sad}{ok}"));
            let path = connected_artifact(&d, db, false);
            // The type, the table keyed by it and its partitions are one plan
            // and one apply.
            let plan: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let kinds = changes_of(&plan);
            assert_eq!(
                kinds.iter().filter(|k| *k == "CreateType").count(),
                1,
                "{kinds:?}"
            );
            assert_eq!(
                kinds.iter().filter(|k| *k == "CreateTable").count(),
                3,
                "{kinds:?}"
            );
            succeeds(approved_apply(&d, db, &path, &[]));
            let again = d.dir.join("again.plan.json");
            assert!(label_plan(&d, db, &again).changes.is_empty());
            succeeds(d.run(&["verify", "--db", db]));
            on_server(db, "INSERT INTO app.ev VALUES (1, 'sad'), (2, 'ok')");
            let routed = |name: &str| {
                scalar(
                    db,
                    &format!("SELECT count(*) FROM app.ev WHERE tableoid = 'app.{name}'::regclass"),
                )
            };
            assert_eq!((routed("ev_sad"), routed("ev_ok")), (1, 1));

            // A partition is added: its creation alone.
            enum_keyed(&d, list, "[sad, ok, happy]", &format!("{sad}{ok}{happy}"));
            succeeds(d.run(&["plan"]));
            d.commit();
            let added = label_plan(&d, db, &again);
            assert_eq!(changes_of(&added), ["CreateTable"], "{added:?}");
            succeeds(approved_apply(&d, db, &again, &[]));
            assert!(label_plan(&d, db, &again).changes.is_empty());

            // And one is dropped, detached first.
            enum_keyed(&d, list, "[sad, ok, happy]", &format!("{sad}{ok}"));
            succeeds(d.run(&["drop-table", "app.ev_happy", "--reason", "retired"]));
            succeeds(d.run(&["plan"]));
            d.commit();
            let dropped = label_plan(&d, db, &again);
            assert_eq!(changes_of(&dropped), ["DropTable"], "{dropped:?}");
            succeeds(approved_apply(&d, db, &again, &["--allow", "destructive"]));
            assert!(label_plan(&d, db, &again).changes.is_empty());
            succeeds(d.run(&["verify", "--db", db]));
        }
    }
}

/// The tree of [`enum_keyed`] on an empty database of `server`, applied.
fn enum_keyed_tree(
    server: &str,
    slug: &str,
    list: bool,
    values: &str,
    partitions: &str,
) -> (OwnDatabase, Demo) {
    let own = OwnDatabase::new(server, slug);
    on_server(own.connection(), "CREATE SCHEMA app");
    let d = Demo::new(slug);
    succeeds(d.run(&[
        "baseline",
        "--db",
        own.connection(),
        "--reason",
        "empty database",
    ]));
    enum_keyed(&d, list, values, partitions);
    let path = connected_artifact(&d, own.connection(), false);
    succeeds(approved_apply(&d, own.connection(), &path, &[]));
    (own, d)
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_rename_moves_the_bounds_of_its_partitions_without_detach_or_recreate() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for list in [true, false] {
            let (sad, ok) = if list {
                ("  ev_sad: {in: [sad]}\n", "  ev_ok: {in: [ok]}\n")
            } else {
                (
                    "  ev_sad: {from: [minvalue], to: [ok]}\n",
                    "  ev_ok: {from: [ok], to: [maxvalue]}\n",
                )
            };
            let (own, d) = enum_keyed_tree(
                &server,
                "enum_key_rename2346",
                list,
                "[sad, ok, happy]",
                &format!("{sad}{ok}"),
            );
            let db = own.connection();
            on_server(db, "INSERT INTO app.ev VALUES (1, 'sad'), (2, 'ok')");
            // `sad` becomes `blue`, in the type and in the bound that names it.
            // The partition that follows the rename takes a check of its own
            // in the same plan, so the plan touches it as well.
            let check = "checks: {not_ok: \"m <> 'ok'\"}";
            let (blue, ok) = if list {
                (
                    format!("  ev_sad: {{in: [blue], {check}}}\n"),
                    ok.to_owned(),
                )
            } else {
                (
                    format!("  ev_sad: {{from: [minvalue], to: [ok], {check}}}\n"),
                    "  ev_ok: {from: [ok], to: [maxvalue]}\n".to_owned(),
                )
            };
            enum_keyed(
                &d,
                list,
                "[{name: blue, renamed_from: sad}, ok, happy]",
                &format!("{blue}{ok}"),
            );
            succeeds(d.run(&["plan"]));
            d.commit();
            let path = d.dir.join("rename.plan.json");
            let plan = label_plan(&d, db, &path);
            // The rename and the check: the engine renames the label inside
            // the bound, and no detach, attach or recreate follows.
            let mut kinds = changes_of(&plan);
            kinds.sort();
            assert_eq!(kinds, ["AddCheck", "RenameEnumValue"], "{plan:?}");
            succeeds(approved_apply(
                &d,
                db,
                &path,
                &["--allow", "rename,constraint"],
            ));
            assert!(label_plan(&d, db, &path).changes.is_empty());
            succeeds(d.run(&["verify", "--db", db]));
            if list {
                assert!(
                    bound_of(db, "ev_sad").contains("'blue'"),
                    "{}",
                    bound_of(db, "ev_sad")
                );
            }
            assert_eq!(
                scalar(db, "SELECT count(*) FROM app.ev WHERE m = 'blue'"),
                1
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*) FROM app.ev WHERE tableoid = 'app.ev_sad'::regclass"
                ),
                1
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_key_bounds_name_declared_labels_and_not_labels_added_in_the_same_plan() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for list in [true, false] {
            // What stands, and the partition whose bound would name `happy`.
            let (standing, later) = if list {
                (
                    "  ev_sad: {in: [sad]}\n  ev_ok: {in: [ok]}\n",
                    "  ev_happy: {in: [happy]}\n",
                )
            } else {
                (
                    "  ev_sad: {from: [minvalue], to: [ok]}\n",
                    "  ev_ok: {from: [ok], to: [happy]}\n",
                )
            };
            let (own, d) =
                enum_keyed_tree(&server, "enum_key_labels2346", list, "[sad, ok]", standing);
            let db = own.connection();
            let path = d.dir.join("labels.plan.json");

            // A bound that names no declared label is refused where the
            // declarations are read, naming the partition and the label.
            let bad = if list {
                "  ev_bad: {in: [nonesuch]}\n"
            } else {
                "  ev_bad: {from: [nonesuch], to: [maxvalue]}\n"
            };
            enum_keyed(&d, list, "[sad, ok]", &format!("{standing}{bad}"));
            let unreachable = "host=127.0.0.1 port=1 user=postgres dbname=unused sslmode=disable";
            for args in [
                vec!["validate"],
                vec!["plan", "--db", unreachable, "--out", path.to_str().unwrap()],
            ] {
                let out = d.run(&args);
                assert_ne!(code(&out), 0);
                let why = format!("{}\n{}", stdout(&out), stderr(&out));
                assert!(
                    why.contains("app.ev_bad")
                        && why.contains("`nonesuch`")
                        && why.contains("not a label of `app.mood`"),
                    "{why}"
                );
                assert!(!path.exists());
            }

            // A label added in this plan cannot bound a partition in it.
            enum_keyed(&d, list, "[sad, ok, happy]", &format!("{standing}{later}"));
            let out = d.run(&["plan"]);
            assert_ne!(code(&out), 0);
            let why = format!("{}\n{}", stdout(&out), stderr(&out));
            assert!(
                why.contains(&format!(
                    "new enum label `happy` is used by app.{}",
                    if list { "ev_happy" } else { "ev_ok" }
                )) && why.contains("apply the label first, then the rest"),
                "{why}"
            );
            assert!(!path.exists());

            // The two-plan split succeeds: the label, then the partition.
            enum_keyed(&d, list, "[sad, ok, happy]", standing);
            succeeds(d.run(&["plan"]));
            d.commit();
            let labels = label_plan(&d, db, &path);
            assert_eq!(changes_of(&labels), ["AddEnumValue"], "{labels:?}");
            succeeds(approved_apply(&d, db, &path, &[]));
            enum_keyed(&d, list, "[sad, ok, happy]", &format!("{standing}{later}"));
            succeeds(d.run(&["plan"]));
            d.commit();
            let partition = label_plan(&d, db, &path);
            assert_eq!(changes_of(&partition), ["CreateTable"], "{partition:?}");
            succeeds(approved_apply(&d, db, &path, &[]));
            assert!(label_plan(&d, db, &path).changes.is_empty());
            succeeds(d.run(&["verify", "--db", db]));
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_attach_to_an_enum_keyed_parent_skips_its_scan_as_a_built_in_key_does() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for list in [true, false] {
            let own = OwnDatabase::new(&server, "enum_attach2346");
            let db = own.connection().to_owned();
            on_server(&db, "CREATE SCHEMA app");
            let d = Demo::new("enum-attach2346");
            let (standing, bounds) = if list {
                (
                    "  ev_sad: {in: [sad]}\n",
                    [
                        ("u_st", "{in: [ok]}", "ok"),
                        ("u_tx", "{in: [happy]}", "happy"),
                    ],
                )
            } else {
                (
                    "  ev_sad: {from: [minvalue], to: [ok]}\n",
                    [
                        ("u_st", "{from: [ok], to: [happy]}", "ok"),
                        ("u_tx", "{from: [happy], to: [maxvalue]}", "happy"),
                    ],
                )
            };
            enum_keyed(&d, list, "[sad, ok, happy]", standing);
            for (name, _, _) in &bounds {
                std::fs::write(
                    d.dir.join(format!("schema/app.{name}.yml")),
                    format!(
                        "table: app.{name}\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: app.mood, nullable: false}}\nprimary_key: [id, m]\n"
                    ),
                )
                .unwrap();
            }
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", &db]));
            for (name, _, label) in &bounds {
                on_server(
                    &db,
                    &format!(
                        "INSERT INTO app.{name} (id, m) SELECT g, '{label}' FROM generate_series(1, 1000) g"
                    ),
                );
            }
            let scans = |name: &str| {
                scalar(
                    &db,
                    &format!(
                        "SELECT seq_scan FROM pg_stat_user_tables WHERE relid = 'app.{name}'::regclass"
                    ),
                )
            };
            let mut deltas = Vec::new();
            let mut partitions = String::from(standing);
            for (name, bound, _) in &bounds {
                let staged = *name == "u_st";
                std::fs::remove_file(d.dir.join(format!("schema/app.{name}.yml"))).unwrap();
                partitions.push_str(&format!("  {name}: {bound}\n"));
                enum_keyed(&d, list, "[sad, ok, happy]", &partitions);
                let plan = connected_artifact(&d, &db, staged);
                let shown = std::fs::read_to_string(&plan).unwrap();
                // The plan carries the key, which the staged form writes its
                // CHECK over (the transactional form attaches by one statement).
                assert!(shown.contains("\"key\": [\n"), "{name}: {shown}");
                let before = scans(name);
                let flags: &[&str] = if staged {
                    &["--staged", "--allow", "constraint"]
                } else {
                    &["--allow", "constraint"]
                };
                succeeds(approved_apply(&d, &db, &plan, flags));
                deltas.push(scans(name) - before);
                assert_eq!(
                    scalar(
                        &db,
                        &format!(
                            "SELECT count(*) FROM pg_inherits WHERE inhrelid = 'app.{name}'::regclass"
                        )
                    ),
                    1,
                    "{name}"
                );
                assert_eq!(
                    scalar(
                        &db,
                        "SELECT count(*) FROM pg_constraint WHERE conname LIKE 'pbps_attach_%'"
                    ),
                    0
                );
            }
            // The staged attach reads its table once, for the VALIDATE; a
            // CHECK that did not imply the bound would make the attach read it
            // again, and the transactional form reads it once, for the attach.
            assert_eq!(
                deltas[0], deltas[1],
                "staged and transactional scans: {deltas:?}"
            );
            succeeds(d.run(&["verify", "--db", &db]));
            let again = d.dir.join("again.plan.json");
            assert!(label_plan(&d, &db, &again).changes.is_empty());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn other_user_types_keep_refusing_as_partition_keys_before_any_staged_plan() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let own = OwnDatabase::new(&server, "enum_key_refused2346");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE TYPE app.ext AS ENUM ('a', 'b'); \
             CREATE TYPE app.pair AS (a integer, b integer)",
        );
        let d = Demo::new("enum-key-refused2346");
        std::fs::write(
            d.dir.join("schema/app.mood.type.yml"),
            "enum: app.mood\nvalues: [sad, ok]\n",
        )
        .unwrap();
        std::fs::write(
            d.dir.join("schema/app.dm.type.yml"),
            "domain: app.dm\ntype: app.mood\n",
        )
        .unwrap();
        std::fs::write(
            d.dir.join("schema/app.dt.type.yml"),
            "domain: app.dt\ntype: text\n",
        )
        .unwrap();
        let path = d.dir.join("refused.plan.json");
        // A domain over an enum, a domain over text, an external enum, a
        // composite, an array of the declared enum.
        for ty in ["app.dm", "app.dt", "app.ext", "app.pair", "app.mood[]"] {
            for list in [true, false] {
                let key = if list { "{list: [m]}" } else { "[m]" };
                d.table(&format!(
                    "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: '{ty}', nullable: false}}\nprimary_key: [id, m]\npartition_by: {key}\n"
                ));
                for args in [
                    vec!["validate"],
                    vec![
                        "plan",
                        "--db",
                        db,
                        "--staged",
                        "--out",
                        path.to_str().unwrap(),
                    ],
                ] {
                    let out = d.run(&args);
                    assert_ne!(code(&out), 0, "{ty} {args:?}");
                    let why = format!("{}\n{}", stdout(&out), stderr(&out));
                    assert!(
                        why.contains("app.t")
                            && why.contains(
                                "partition key `m` has a user-defined type reference, which is not supported as a partition key"
                            ),
                        "{ty} {args:?}: {why}"
                    );
                    assert!(!path.exists());
                }
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_label_removal_or_reorder_of_an_enum_that_keys_a_partitioned_table_is_refused_by_name() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for list in [true, false] {
            let partitions = if list {
                "  ev_ok: {in: [ok]}\n  ev_happy: {in: [happy]}\n"
            } else {
                "  ev_ok: {from: [ok], to: [happy]}\n  ev_happy: {from: [happy], to: [maxvalue]}\n"
            };
            let (own, d) = enum_keyed_tree(
                &server,
                "enum_key_rebuild2346",
                list,
                "[sad, ok, happy]",
                partitions,
            );
            let db = own.connection();
            let path = d.dir.join("rebuild.plan.json");
            for values in ["[ok, happy]", "[happy, ok, sad]"] {
                enum_keyed(&d, list, values, partitions);
                d.commit();
                let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
                assert_ne!(code(&out), 0, "{values}");
                let why = format!("{}\n{}", stdout(&out), stderr(&out));
                assert!(
                    why.contains("type `app.mood`")
                        && why.contains("app.ev.m is a partition key of this enum")
                        && why.contains("rebuild the partitioned table"),
                    "{values}: {why}"
                );
                assert!(!path.exists());
            }
            // The key's labels can still be added to and renamed.
            enum_keyed(&d, list, "[sad, ok, happy, glad]", partitions);
            d.commit();
            let added = label_plan(&d, db, &path);
            assert_eq!(changes_of(&added), ["AddEnumValue"], "{added:?}");
        }
    }
}

/// A bound is declared in the labels the plan leaves, and the pre-flight asks
/// the rows as they are before it runs, so the bound goes back through the
/// plan's renames first (DEC-2346.1). `b` becomes `c`, `a` becomes `b`, and a
/// partition takes `b`: the DEFAULT row that is `b` now is `c` after, and
/// does not move; one that is `a` now is `b` after, and would.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_declared_enum_bound_is_asked_of_the_rows_in_the_labels_they_hold() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for (row, moves) in [("b", false), ("a", true)] {
            let (own, d) = enum_keyed_tree(
                &server,
                "enum_key_asked2346",
                true,
                "[a, b]",
                "  ev_rest: default\n",
            );
            let db = own.connection();
            on_server(db, &format!("INSERT INTO app.ev VALUES (1, '{row}')"));
            enum_keyed(
                &d,
                true,
                "[{name: b, renamed_from: a}, {name: c, renamed_from: b}]",
                "  ev_rest: default\n  ev_new: {in: [b]}\n",
            );
            succeeds(d.run(&["plan"]));
            d.commit();
            let path = d.dir.join("asked.plan.json");
            let plan = label_plan(&d, db, &path);
            let mut kinds = changes_of(&plan);
            kinds.sort();
            assert_eq!(
                kinds,
                ["CreateTable", "RenameEnumValue", "RenameEnumValue"],
                "{plan:?}"
            );
            let out = approved_apply(&d, db, &path, &["--allow", "rename"]);
            let said = format!("{}{}", stdout(&out), stderr(&out));
            if moves {
                // Refused before a statement, with the remedy the move has.
                assert_ne!(code(&out), 0, "{said}");
                assert!(
                    said.contains("rows of app.ev_rest are among the values of app.ev_new")
                        && said.contains("no move of them was allowed")
                        && said.contains("pbps move-rows app.ev_new --max-rows"),
                    "{said}"
                );
                assert_eq!(
                    scalar(db, "SELECT count(*) FROM pg_enum WHERE enumlabel = 'c'"),
                    0
                );
            } else {
                assert_eq!(code(&out), 0, "{said}");
                // Renamed in place, and where it was: the DEFAULT.
                assert_eq!(
                    text_of(
                        db,
                        "SELECT m::text || ' in ' || tableoid::regclass::text FROM app.ev WHERE id = 1"
                    ),
                    "c in app.ev_rest"
                );
                assert_eq!(scalar(db, "SELECT count(*) FROM app.ev_new"), 0);
                assert!(label_plan(&d, db, &path).changes.is_empty());
                succeeds(d.run(&["verify", "--db", db]));
            }
        }
    }
}

// ---- #1635 slice 4a: a declared domain changes in place ----

/// The domain `app.code`, as a declaration file writes it.
fn code_domain(not_null: bool, default: Option<&str>, checks: &[(&str, &str)]) -> String {
    let mut yaml = String::from("domain: app.code\ntype: text\n");
    if not_null {
        yaml.push_str("not_null: true\n");
    }
    if let Some(default) = default {
        yaml.push_str(&format!(
            "default: {}\n",
            serde_json::to_string(default).unwrap()
        ));
    }
    if !checks.is_empty() {
        yaml.push_str("checks:\n");
        for (name, text) in checks {
            yaml.push_str(&format!(
                "  {name}: {}\n",
                serde_json::to_string(text).unwrap()
            ));
        }
    }
    yaml
}

/// The check `c1` of [`CODE_SQL`] in the engine's spelling, which an adopted
/// domain is compared by until it is applied from a declaration.
const C1: &str = "(VALUE <> ''::text)";

const CODE_SQL: &str = "CREATE SCHEMA app;
CREATE DOMAIN app.code AS text DEFAULT 'a' CONSTRAINT c1 CHECK (VALUE <> '');
CREATE TABLE app.t (id integer PRIMARY KEY, v app.code);
INSERT INTO app.t VALUES (1, 'abc'), (2, 'xyz');";

/// The halves of a plan's domain alterations, in plan order: `Loosen` and
/// `Tighten`, and any other change by its name.
#[allow(clippy::wildcard_enum_match_arm)] // Any other change is named by its variant.
fn halves(plan: &pbps_model::SavedPlan) -> Vec<String> {
    plan.changes
        .changes
        .iter()
        .map(|p| match &p.change {
            pbps_model::Change::AlterDomain { half, .. } => format!("{half:?}"),
            other => format!("{other:?}")
                .split([' ', '{', '('])
                .next()
                .unwrap()
                .to_owned(),
        })
        .collect()
}

fn write_code(d: &Demo, yaml: &str) {
    std::fs::write(d.dir.join("schema/app.code.type.yml"), yaml).unwrap();
    d.commit();
}

fn apply_allowing(
    d: &Demo,
    db: &str,
    path: &std::path::Path,
    allow: &[&str],
) -> std::process::Output {
    let checksum = plan_checksum(path);
    let mut args = vec![
        "apply",
        "--db",
        db,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    for class in allow {
        args.extend(["--allow", class]);
    }
    d.run(&args)
}

/// No domain check stands `NOT VALID`: pbps leaves none behind.
fn no_unvalidated_domain_check(db: &str) -> bool {
    scalar(
        db,
        "SELECT count(*) FROM pg_constraint WHERE contypid <> 0 AND NOT convalidated",
    ) == 0
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_alterations_converge_in_place_keep_the_uid_and_record_the_authored_text() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_alter4a", CODE_SQL);
        let db = own.connection();
        let uids = type_ids(&d);
        let authored = "VALUE <>  '' AND length(VALUE) < 9";
        let path = d.dir.join("domain.plan.json");
        // (what is declared, the halves it plans, the classes it needs)
        let steps: Vec<(String, Vec<&str>, Vec<&str>)> = vec![
            // A changed check is a drop and an add under its name.
            (
                code_domain(false, Some("'a'"), &[("c1", authored)]),
                vec!["Loosen", "Tighten"],
                vec!["constraint"],
            ),
            (
                code_domain(
                    false,
                    Some("'a'"),
                    &[("c1", authored), ("c2", "VALUE <> 'x'")],
                ),
                vec!["Tighten"],
                vec!["constraint"],
            ),
            (
                code_domain(false, Some("'a'"), &[("c1", authored)]),
                vec!["Loosen"],
                vec![],
            ),
            (
                code_domain(false, Some("'b'"), &[("c1", authored)]),
                vec!["Loosen"],
                vec![],
            ),
            (
                code_domain(false, None, &[("c1", authored)]),
                vec!["Loosen"],
                vec![],
            ),
            (
                code_domain(true, None, &[("c1", authored)]),
                vec!["Tighten"],
                vec!["not-null"],
            ),
            (
                code_domain(false, None, &[("c1", authored)]),
                vec!["Loosen"],
                vec![],
            ),
        ];
        for (yaml, expected, allow) in steps {
            write_code(&d, &yaml);
            let plan = label_plan(&d, db, &path);
            assert_eq!(halves(&plan), expected, "{yaml}\n{plan:?}");
            succeeds(apply_allowing(&d, db, &path, &allow));
            assert!(label_plan(&d, db, &path).changes.is_empty(), "{yaml}");
            assert!(no_unvalidated_domain_check(db));
            assert_eq!(type_ids(&d), uids, "the uid never changes");
            succeeds(d.run(&["verify", "--db", db]));
        }
        // The text recorded for the next plan is the authored spelling, the
        // double space included, and not the engine's.
        let snapshot = latest_snapshot(db);
        let recorded = snapshot
            .declared
            .types
            .get(&"app.code".parse().unwrap())
            .expect("the domain's record");
        let ManagedType::Domain { checks, .. } = recorded else {
            panic!("{recorded:?}")
        };
        let c1: pbps_model::ObjectLocalName = "c1".into();
        assert_eq!(checks.get(&c1).map(String::as_str), Some(authored));
        assert_eq!(scalar(db, "SELECT count(*) FROM app.t"), 2);
    }
}

/// The engine's own answer, kept: the plan is valid and the apply is refused
/// where the rows are, with nothing changed (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_tightening_over_a_violating_row_rolls_the_apply_back_and_keeps_the_old_check() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_violate4a", CODE_SQL);
        let db = own.connection();
        let path = d.dir.join("violate.plan.json");
        write_code(
            &d,
            &code_domain(false, Some("'a'"), &[("c1", "length(VALUE) < 3")]),
        );
        let plan = label_plan(&d, db, &path);
        assert_eq!(halves(&plan), ["Loosen", "Tighten"]);
        let before = latest_snapshot(db);
        let out = apply_allowing(&d, db, &path, &["constraint"]);
        assert_ne!(code(&out), 0);
        let said = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            said.contains("violate the new constraint") && said.contains("table \"t\""),
            "{said}"
        );
        // Whole: the old check stands, validated, and no new one.
        assert_eq!(
            text_of(
                db,
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
                 WHERE contypid = 'app.code'::regtype AND conname = 'c1'"
            ),
            "CHECK ((VALUE <> ''::text))"
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_constraint WHERE contypid = 'app.code'::regtype"
            ),
            1
        );
        assert!(no_unvalidated_domain_check(db));
        assert_eq!(latest_snapshot(db).declared, before.declared);
        // The plan stands as it was: the rows are the author's to fix.
        assert_eq!(halves(&label_plan(&d, db, &path)), ["Loosen", "Tighten"]);
    }
}

const PROBE_SQL: &str = "CREATE SCHEMA app;
CREATE SEQUENCE app.probe_seq;
CREATE FUNCTION app.probe(text) RETURNS boolean LANGUAGE sql
  AS $$ SELECT nextval('app.probe_seq') > 0 $$;
CREATE DOMAIN app.code AS text;
CREATE TABLE app.t (id integer PRIMARY KEY, v app.code);
INSERT INTO app.t VALUES (1, 'a'), (2, 'b');";

/// The pinned negative: a check that calls a function of the project is
/// refused by the apply's gate, and the function never ran. Added plain, the
/// check is evaluated over every row inside the `ALTER`, before anything can
/// vet it; added `NOT VALID` it is bound and evaluated nowhere until the vet
/// has read it (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_check_calling_a_project_function_is_vetted_before_it_runs_and_leaves_nothing() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_probe4a", PROBE_SQL);
        let db = own.connection();
        let path = d.dir.join("probe.plan.json");
        let probed = |db: &str| {
            scalar(
                db,
                "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM app.probe_seq",
            )
        };
        assert_eq!(probed(db), 0);
        write_code(
            &d,
            &code_domain(false, None, &[("probe_it", "app.probe(VALUE)")]),
        );
        let plan = label_plan(&d, db, &path);
        assert_eq!(halves(&plan), ["Tighten"]);
        let out = apply_allowing(&d, db, &path, &["constraint"]);
        assert_ne!(code(&out), 0);
        let said = format!("{}{}", stdout(&out), stderr(&out));
        assert!(said.contains("app.probe"), "{said}");
        // Nothing left behind, and the function never ran.
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_constraint WHERE contypid = 'app.code'::regtype"
            ),
            0
        );
        assert_eq!(probed(db), 0, "the check ran before it was vetted: {said}");
    }
}

/// Refused where the plan is made, naming the column and the engine's reason,
/// for each container that holds the domain (DEC-1635.8). A domain nothing
/// holds that way tightens, and so does one a plan loosens beside.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_tightening_is_refused_by_name_while_an_array_or_composite_of_the_domain_is_a_column() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        for (holder, column) in [
            ("x app.code[]", "t.x"),
            ("x app.pair", "t.x"),
            ("x app.pair[]", "t.x"),
        ] {
            let sql = format!(
                "{CODE_SQL} CREATE TYPE app.pair AS (a app.code, b integer); ALTER TABLE app.t ADD COLUMN {holder};"
            );
            let (own, d) = adopted(&server, "domain_holders4a", &sql);
            let db = own.connection();
            let path = d.dir.join("holders.plan.json");
            // A loosening is not the engine's concern.
            write_code(&d, &code_domain(false, None, &[("c1", C1)]));
            let loosen = label_plan(&d, db, &path);
            assert_eq!(halves(&loosen), ["Loosen"], "{holder}");
            std::fs::remove_file(&path).unwrap();
            // A tightening is.
            write_code(
                &d,
                &code_domain(false, None, &[("c1", C1), ("c2", "VALUE <> 'x'")]),
            );
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0, "{holder}");
            let said = format!("{}{}", stdout(&out), stderr(&out));
            assert!(
                said.contains("domain app.code cannot take a new check or NOT NULL")
                    && said.contains(&format!("app.{column}"))
                    && said.contains("cannot alter type \"code\" because column")
                    && said.contains("slice 4b"),
                "{holder}: {said}"
            );
            assert!(!path.exists());
            // NOT NULL is refused alike.
            write_code(&d, &code_domain(true, None, &[("c1", C1)]));
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0, "{holder}");
            assert!(!path.exists());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_base_type_or_collation_change_is_refused_by_name_and_points_at_the_rebuild() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_base4a", CODE_SQL);
        let db = own.connection();
        let path = d.dir.join("base.plan.json");
        for yaml in [
            "domain: app.code\ntype: varchar(20)\ndefault: \"'a'\"\nchecks:\n  c1: \"VALUE <> ''\"\n",
            "domain: app.code\ntype: text\ncollation: pg_catalog.\"C\"\ndefault: \"'a'\"\nchecks:\n  c1: \"VALUE <> ''\"\n",
        ] {
            write_code(&d, yaml);
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0, "{yaml}");
            let said = format!("{}{}", stdout(&out), stderr(&out));
            assert!(
                said.contains("app.code")
                    && said.contains("rebuilding it (#1635 slice 4b)")
                    && !said.contains("require slice 4,"),
                "{said}"
            );
            assert!(!path.exists());
        }
    }
}

/// A standing `NOT VALID` check compares equal to its declaration: the plan
/// is empty, and says what the engine has not confirmed (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_standing_not_valid_domain_check_is_equal_to_its_declaration_and_warned_about() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = format!(
            "{CODE_SQL} ALTER DOMAIN app.code ADD CONSTRAINT c2 CHECK (length(VALUE) < 9) NOT VALID;"
        );
        let (own, d) = adopted(&server, "domain_not_valid4a", &sql);
        let db = own.connection();
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_constraint WHERE contypid = 'app.code'::regtype AND NOT convalidated"
            ),
            1
        );
        let out = succeeds(d.run(&["plan", "--db", db, "--format", "json"]));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        let finding = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == "plan.domain-check-not-valid")
            .unwrap_or_else(|| panic!("{report}"));
        assert_eq!(finding["severity"], "warning", "{finding}");
        let text = finding.to_string();
        assert!(
            text.contains("`c2`")
                && text.contains("app.code")
                && text.contains(
                    "ALTER DOMAIN \\\"app\\\".\\\"code\\\" VALIDATE CONSTRAINT \\\"c2\\\""
                ),
            "{text}"
        );
        // Otherwise empty: the check is the declaration's.
        let path = d.dir.join("not-valid.plan.json");
        assert!(label_plan(&d, db, &path).changes.is_empty());
        // The remedy is by hand, and silences the warning.
        on_server(db, "ALTER DOMAIN app.code VALIDATE CONSTRAINT c2");
        let out = succeeds(d.run(&["plan", "--db", db, "--format", "json"]));
        assert!(!stdout(&out).contains("plan.domain-check-not-valid"));
    }
}

/// A layout-only edit of a check is a difference, as DEC-1635.4 requires: the
/// text recorded is the text applied.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_layout_only_edit_of_a_domain_check_stays_a_difference() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_layout4a", CODE_SQL);
        let db = own.connection();
        let path = d.dir.join("layout.plan.json");
        write_code(
            &d,
            &code_domain(false, Some("'a'"), &[("c1", "VALUE  <>  ''")]),
        );
        let plan = label_plan(&d, db, &path);
        assert_eq!(halves(&plan), ["Loosen", "Tighten"]);
        succeeds(apply_allowing(&d, db, &path, &["constraint"]));
        assert!(label_plan(&d, db, &path).changes.is_empty());
    }
}

/// A check altered by hand between the plan and the apply is drift, refused
/// before any statement (DEC-1635.4's before-definition).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_domain_check_altered_by_hand_between_plan_and_apply_is_refused_as_drift() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_drift4a", CODE_SQL);
        let db = own.connection();
        let path = d.dir.join("drift.plan.json");
        write_code(
            &d,
            &code_domain(false, Some("'a'"), &[("c1", "length(VALUE) < 9")]),
        );
        let plan = label_plan(&d, db, &path);
        assert_eq!(halves(&plan), ["Loosen", "Tighten"]);
        on_server(
            db,
            "ALTER DOMAIN app.code DROP CONSTRAINT c1; \
             ALTER DOMAIN app.code ADD CONSTRAINT c1 CHECK (VALUE <> 'zz')",
        );
        let out = apply_allowing(&d, db, &path, &["constraint"]);
        assert_ne!(code(&out), 0);
        let said = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            said.contains("is no longer the database this plan was computed against"),
            "{said}"
        );
        assert_eq!(
            text_of(
                db,
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
                 WHERE contypid = 'app.code'::regtype AND conname = 'c1'"
            ),
            "CHECK ((VALUE <> 'zz'::text))"
        );
    }
}

/// The tightening is one statement, and so one staged step: the checks are
/// added `NOT VALID`, vetted and validated inside the transaction of that
/// step, and a resume past it finds the domain as the step left it
/// (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_domain_tightening_is_one_step_and_a_resume_accepts_it() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_staged4a", CODE_SQL);
        let db = own.connection();
        let path = d.dir.join("staged-domain.plan.json");
        write_code(
            &d,
            &code_domain(
                false,
                Some("'a'::text"),
                &[("c1", C1), ("c2", "length(VALUE) < 9")],
            ),
        );
        succeeds(d.run(&[
            "plan",
            "--db",
            db,
            "--staged",
            "--out",
            path.to_str().unwrap(),
        ]));
        let plan: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(halves(&plan), ["Tighten"]);
        let checksum = plan_checksum(&path);
        let mut args = vec![
            "apply",
            "--db",
            db,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--staged",
            "--allow",
            "constraint",
        ];
        let _checkpoint = super::abrupt_recovery::interrupt_after_type_checkpoint(db, &d, &args);
        // The step committed whole: added, vetted, and validated.
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_constraint \
                 WHERE contypid = 'app.code'::regtype AND conname = 'c2' AND convalidated"
            ),
            1
        );
        args.push("--resume");
        succeeds(d.run(&args));
        succeeds(d.run(&["verify", "--db", db]));
        assert!(label_plan(&d, db, &path).changes.is_empty());
        assert!(no_unvalidated_domain_check(db));
    }
}

/// A loosening goes in before the plan's other writes and a tightening after
/// them: the column this plan adds takes a default the old check refused, so
/// the check has to be gone first (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_loosening_comes_before_the_columns_the_plan_adds_and_a_tightening_after() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let sql = "CREATE SCHEMA app;
            CREATE DOMAIN app.code AS text CONSTRAINT short CHECK (length(VALUE) < 5);
            CREATE TABLE app.t (id integer PRIMARY KEY, v app.code);
            INSERT INTO app.t VALUES (1, 'abc');";
        let (own, d) = adopted(&server, "domain_order4a", sql);
        let db = own.connection();
        let path = d.dir.join("order.plan.json");
        // The new column's default is longer than the old check allows.
        let table = |extra: &str| {
            d.table(&format!(
                "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  v: {{type: app.code}}\n{extra}primary_key: [id]\n"
            ));
        };
        write_code(
            &d,
            &code_domain(false, None, &[("short", "length(VALUE) < 9")]),
        );
        table("  w: {type: app.code, default: \"'abcdefg'\"}\n");
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = label_plan(&d, db, &path);
        let kinds = halves(&plan);
        let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
        assert!(
            at("Loosen") < at("AddColumn") && at("AddColumn") < at("Tighten"),
            "{kinds:?}"
        );
        succeeds(apply_allowing(&d, db, &path, &["constraint"]));
        assert!(label_plan(&d, db, &path).changes.is_empty());
        assert_eq!(text_of(db, "SELECT w FROM app.t WHERE id = 1"), "abcdefg");
    }
}

/// The alteration names the domain by the spelling the plan leaves it: the
/// rename runs first, and both halves find it there (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_renamed_domain_is_altered_under_its_final_name() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        let (own, d) = adopted(&server, "domain_renamed4a", CODE_SQL);
        let db = own.connection();
        let path = d.dir.join("renamed.plan.json");
        let uids = type_ids(&d);
        std::fs::remove_file(d.dir.join("schema/app.code.type.yml")).unwrap();
        std::fs::write(
            d.dir.join("schema/app.codes.type.yml"),
            "domain: app.codes\nrenamed_from: app.code\ntype: text\nnot_null: true\ndefault: \"'a'::text\"\nchecks:\n  c1: \"length(VALUE) < 9\"\n",
        )
        .unwrap();
        d.table("table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  v: {type: app.codes}\nprimary_key: [id]\n");
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = label_plan(&d, db, &path);
        let kinds = halves(&plan);
        assert_eq!(kinds, ["RenameType", "Loosen", "Tighten"], "{kinds:?}");
        succeeds(apply_allowing(
            &d,
            db,
            &path,
            &["constraint", "not-null", "rename"],
        ));
        assert!(label_plan(&d, db, &path).changes.is_empty());
        assert_eq!(
            scalar(db, "SELECT count(*) FROM pg_type WHERE typname = 'code'"),
            0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_constraint \
                 WHERE contypid = 'app.codes'::regtype AND conname = 'c1' AND convalidated"
            ),
            1
        );
        assert_eq!(
            type_ids(&d).as_object().unwrap().len(),
            uids.as_object().unwrap().len()
        );
        assert!(no_unvalidated_domain_check(db));
    }
}

/// A domain over an enum, as a plan that adds a label meets it: the default or
/// check the alteration introduces names the label (DEC-1635.8).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_new_label_in_what_a_domain_alteration_introduces_is_refused_until_the_label_is_applied() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        // (what the database holds, what is declared, the classes the split needs)
        let cases: [(&str, &str, &[&str]); 3] = [
            ("", "domain: app.d\ntype: app.e\ndefault: \"'new'\"\n", &[]),
            (
                "",
                "domain: app.d\ntype: app.e\nchecks:\n  c: \"VALUE <> 'new'\"\n",
                &["constraint"],
            ),
            (
                " CONSTRAINT c CHECK (VALUE <> 'old')",
                "domain: app.d\ntype: app.e\nchecks:\n  c: \"VALUE <> 'new'\"\n",
                &["constraint"],
            ),
        ];
        for (n, (held, declared, allow)) in cases.into_iter().enumerate() {
            let (own, d) = adopted(
                &server,
                "domain_label4a",
                &format!(
                    "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('old'); \
                     CREATE DOMAIN app.d AS app.e{held}; \
                     CREATE TABLE app.t (id integer PRIMARY KEY, m app.d);"
                ),
            );
            let db = own.connection();
            let enum_file = d.dir.join("schema/app.e.type.yml");
            let domain_file = d.dir.join("schema/app.d.type.yml");
            let pulled = std::fs::read_to_string(&domain_file).unwrap();
            std::fs::write(&enum_file, "enum: app.e\nvalues: [old, new]\n").unwrap();
            std::fs::write(&domain_file, declared).unwrap();
            d.commit();
            let path = d.dir.join("domain-label.plan.json");
            let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            assert_ne!(code(&out), 0, "case {n}");
            assert!(
                stderr(&out).contains("app.d")
                    && stderr(&out).contains("new")
                    && stderr(&out).contains("apply the label first, then the rest"),
                "case {n}: {}",
                stderr(&out)
            );
            assert!(!path.exists());
            let labels = "SELECT count(*)::bigint FROM pg_enum \
                          WHERE enumtypid = 'app.e'::regtype AND enumlabel = 'new'";
            assert_eq!(scalar(db, labels), 0, "refused before any SQL");

            // The two-plan split: the label first, then the domain.
            std::fs::write(&domain_file, &pulled).unwrap();
            d.commit();
            let plan = label_plan(&d, db, &path);
            assert_eq!(halves(&plan), ["AddEnumValue"], "case {n}");
            succeeds(apply_allowing(&d, db, &path, &[]));
            assert_eq!(scalar(db, labels), 1);
            std::fs::write(&domain_file, declared).unwrap();
            d.commit();
            let plan = label_plan(&d, db, &path);
            assert!(!halves(&plan).is_empty(), "case {n}");
            succeeds(apply_allowing(&d, db, &path, allow));
            assert!(label_plan(&d, db, &path).changes.is_empty(), "case {n}");
            assert!(no_unvalidated_domain_check(db));
            succeeds(d.run(&["verify", "--db", db]));
        }
    }
}

/// A label renamed in the same plan is usable by the alteration only once the
/// rename has run: the half that sets a default or adds a check follows it.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_domain_default_or_check_naming_a_renamed_label_runs_after_the_rename() {
    for server in [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ] {
        // (what the database holds, what is declared, the classes it needs)
        let cases: [(&str, &str, &[&str]); 3] = [
            (
                " DEFAULT 'a'",
                "domain: app.d\ntype: app.e\ndefault: \"'b'\"\n",
                &["rename"],
            ),
            (
                "",
                "domain: app.d\ntype: app.e\ndefault: \"'b'\"\n",
                &["rename"],
            ),
            (
                " CONSTRAINT c CHECK (VALUE <> 'a')",
                "domain: app.d\ntype: app.e\nchecks:\n  c: \"VALUE <> 'b'\"\n",
                &["rename", "constraint"],
            ),
        ];
        for (n, (held, declared, allow)) in cases.into_iter().enumerate() {
            let (own, d) = adopted(
                &server,
                "domain_rename4a",
                &format!(
                    "CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('a', 'keep'); \
                     CREATE DOMAIN app.d AS app.e{held}; \
                     CREATE TABLE app.t (id integer PRIMARY KEY, m app.d);"
                ),
            );
            let db = own.connection();
            std::fs::write(
                d.dir.join("schema/app.e.type.yml"),
                "enum: app.e\nvalues: [{name: b, renamed_from: a}, keep]\n",
            )
            .unwrap();
            std::fs::write(d.dir.join("schema/app.d.type.yml"), declared).unwrap();
            d.commit();
            let path = d.dir.join("domain-rename.plan.json");
            let plan = label_plan(&d, db, &path);
            let kinds = halves(&plan);
            let rename = kinds.iter().position(|k| k == "RenameEnumValue").unwrap();
            // The half that introduces the label is the last of the two.
            let after = kinds
                .iter()
                .rposition(|k| k == "Loosen" || k == "Tighten")
                .unwrap();
            assert!(rename < after, "case {n}: {kinds:?}");
            succeeds(apply_allowing(&d, db, &path, allow));
            assert!(label_plan(&d, db, &path).changes.is_empty(), "case {n}");
            assert!(no_unvalidated_domain_check(db));
        }
    }
}
