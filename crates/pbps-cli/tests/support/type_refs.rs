//! User-type enablement through the binary, measured on both supported majors.
use super::*;

fn servers() -> [String; 2] {
    [
        server(),
        std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    ]
}

fn declarations(extra: &str) -> String {
    format!(
        "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n{extra}primary_key: [id]\n"
    )
}

fn plan(d: &Demo, connection: &str) -> PathBuf {
    let path = d.dir.join("user-types.plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    path
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn existing_enum_domain_and_composite_columns_round_trip_as_scalars_and_arrays() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_roundtrip");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE SCHEMA \"App\";
            CREATE TYPE app.mood AS ENUM ('sad', 'happy');
            CREATE DOMAIN app.positive AS integer CHECK (VALUE > 0);
            CREATE TYPE app.pair AS (n integer, s text);
            CREATE TYPE \"App\".\"Mood\" AS ENUM ('ok');",
        );
        let d = Demo::new("refs-roundtrip");
        d.table(&declarations("  mood: {type: app.mood, default: \"'happy'::app.mood\"}\n  moods: {type: 'app.mood[]', default: \"'{}'::app.mood[]\"}\n  n: {type: app.positive}\n  ns: {type: 'app.positive[]'}\n  pair: {type: app.pair}\n  pairs: {type: 'app.pair[]'}\n  quoted: {type: '\"App\".\"Mood\"'}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        let script = d.dir.join("bootstrap.sql");
        succeeds(d.run(&["bootstrap", "--sql", script.to_str().unwrap()]));
        let sql = std::fs::read_to_string(&script).unwrap();
        assert!(
            sql.find("DO $pbps$").unwrap() < sql.find("CREATE TABLE").unwrap(),
            "{sql}"
        );
        assert!(sql.contains("\"app\".\"mood\"[]"), "{sql}");
        on_server(db, &sql);
        // The script builds the declarations; baseline adopts the resulting table.
        succeeds(d.run(&["baseline", "--db", db, "--reason", "adopt script"]));
        succeeds(d.run(&["verify", "--db", db]));
        let path = plan(&d, db);
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(saved.changes.is_empty(), "{:?}", saved.changes);
        assert_eq!(saved.type_pins.len(), 4);
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        succeeds(d.run(&["validate"]));
        let next = plan(&d, db);
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(next).unwrap()).unwrap();
        assert!(saved.changes.is_empty(), "{:?}", saved.changes);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn missing_and_shell_types_refuse_before_bootstrap_creates_a_ledger_or_table() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_missing");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("refs-missing");
        d.table(&declarations("  mood: {type: app.missing}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        for (setup, message) in [
            ("", "does not exist"),
            ("CREATE TYPE app.missing", "shell type"),
        ] {
            if !setup.is_empty() {
                on_server(db, setup);
            }
            let out = d.run(&["bootstrap", "--db", db]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("app.missing") && stderr(&out).contains(message),
                "{}{}",
                stdout(&out),
                stderr(&out)
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_catalog.pg_class WHERE relname IN ('__pbps_state','t')"
                ),
                0
            );
        }
        let script = d.dir.join("missing.sql");
        succeeds(d.run(&["bootstrap", "--sql", script.to_str().unwrap()]));
        let err = try_on_server(db, &std::fs::read_to_string(script).unwrap()).unwrap_err();
        assert!(err.contains("app.missing"), "{err}");
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_class WHERE relname='t'"
            ),
            0
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn enum_label_domain_constraint_and_composite_changes_invalidate_saved_type_pins() {
    for server in servers() {
        for (name, setup, mutate) in [
            (
                "enum",
                "CREATE TYPE app.value AS ENUM ('a')",
                "ALTER TYPE app.value ADD VALUE 'b'",
            ),
            (
                "domain",
                "CREATE DOMAIN app.value AS integer",
                "ALTER DOMAIN app.value ADD CONSTRAINT pos CHECK (VALUE > 0) NOT VALID",
            ),
            (
                "composite",
                "CREATE TYPE app.value AS (a integer)",
                "ALTER TYPE app.value ADD ATTRIBUTE b text",
            ),
            (
                "rename",
                "CREATE TYPE app.value AS ENUM ('a')",
                "ALTER TYPE app.value RENAME TO renamed",
            ),
            (
                "recreate",
                "CREATE TYPE app.value AS ENUM ('a')",
                "DROP TYPE app.value; CREATE TYPE app.value AS ENUM ('a')",
            ),
        ] {
            let own = OwnDatabase::new(&server, &format!("refs_pin_{name}"));
            let db = own.connection();
            on_server(db, &format!("CREATE SCHEMA app; {setup}"));
            let d = Demo::new(&format!("refs-pin-{name}"));
            d.table(&declarations(""));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", db]));
            d.table(&declarations("  value: {type: app.value}\n"));
            succeeds(d.run(&["plan"]));
            d.commit();
            let path = plan(&d, db);
            on_server(db, mutate);
            let out = apply_plan(&d, db, &path, false);
            assert_ne!(code(&out), 0, "{name}");
            let reason = if name == "rename" {
                "does not exist"
            } else {
                "changed since planning"
            };
            assert!(
                stderr(&out).contains("app.value") && stderr(&out).contains(reason),
                "{}{}",
                stdout(&out),
                stderr(&out)
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_catalog.pg_attribute WHERE attrelid='app.t'::regclass AND attname='value'"
                ),
                0
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn adding_a_domain_over_rows_checks_builtin_code_and_the_saved_trust_list() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_trust");
        let db = own.connection();
        let role = format!("pbps_refs_owner_{}", std::process::id());
        on_server(
            &server,
            &format!("DROP ROLE IF EXISTS {role}; CREATE ROLE {role}"),
        );
        on_server(
            db,
            &format!(
                "CREATE SCHEMA app; GRANT USAGE ON SCHEMA app TO {role};
            CREATE DOMAIN app.value AS integer DEFAULT 1 CHECK (VALUE > 0);
            ALTER DOMAIN app.value OWNER TO {role};"
            ),
        );
        let d = Demo::new("refs-trust");
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1), (2)");
        d.table(&declarations(
            "  value: {type: app.value, nullable: false}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains(&role),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: postgres\ntrusted_roles: [{role}]\n"),
        )
        .unwrap();
        let path = plan(&d, db);
        std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();
        let out = approved_apply(&d, db, &path, &["--allow", "not-null"]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("trusted_roles changed"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: postgres\ntrusted_roles: [{role}]\n"),
        )
        .unwrap();
        succeeds(approved_apply(&d, db, &path, &["--allow", "not-null"]));
        assert_eq!(scalar(db, "SELECT sum(value)::bigint FROM app.t"), 2);
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\ntrusted_roles: [pbps_role_that_does_not_exist]\n",
        )
        .unwrap();
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("do not exist"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        drop(own);
        on_server(&server, &format!("DROP ROLE {role}"));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_user_domain_check_is_never_evaluated_by_planning_an_add_over_rows() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_inert");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
            BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            CREATE DOMAIN app.trap AS integer CHECK (app.seen(VALUE));",
        );
        let d = Demo::new("refs-inert");
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1), (2)");
        d.table(&declarations("  value: {type: app.trap}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        let before = scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state");
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.seen(integer)")
                && stderr(&out).contains("not built into PostgreSQL"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
            before
        );
        on_server(db, "DROP DOMAIN app.trap");
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.trap") && stderr(&out).contains("does not exist"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
            before
        );
        on_server(
            db,
            "CREATE DOMAIN app.trap AS integer CHECK (app.seen(VALUE))",
        );
        // Empty tables do not evaluate a fill and need no input-code gate.
        on_server(db, "TRUNCATE app.t");
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_nonnull_composite_with_null_attributes_satisfies_column_not_null() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_composite_null");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE TYPE app.pair AS (n integer, s text)",
        );
        let d = Demo::new("refs-composite-null");
        d.table(&declarations("  pair: {type: app.pair}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(
            db,
            "INSERT INTO app.t VALUES (1, ROW(NULL,NULL)::app.pair), (2, ROW(1,NULL)::app.pair)",
        );
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.t WHERE pair IS NULL"),
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE pair IS NOT DISTINCT FROM NULL"
            ),
            0
        );
        d.table(&declarations("  pair: {type: app.pair, nullable: false}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(approved_apply(&d, db, &path, &["--allow", "not-null"]));
        succeeds(d.run(&["verify", "--db", db]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_domain_keeps_a_null_default_while_enum_composite_and_array_null_defaults_are_erased() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_null_default");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE DOMAIN app.value AS integer DEFAULT 7 CHECK (VALUE IS NULL OR VALUE > 0); CREATE TYPE app.mood AS ENUM ('ok')",
        );
        let d = Demo::new("refs-null-default");
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1)");
        d.table(&declarations(
            "  value: {type: app.value, default: 'NULL'}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        assert_eq!(
            scalar(db, "SELECT count(*)::bigint FROM app.t WHERE value IS NULL"),
            1
        );
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty(), "the domain NULL default converges");
        on_server(db, "CREATE FUNCTION app.never(integer) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'nested CHECK ran'; END $$;
            CREATE DOMAIN app.trap AS integer CHECK (app.never(VALUE));
            CREATE TYPE app.pair AS (v app.trap); CREATE DOMAIN app.holder AS app.pair");
        d.table(&declarations("  value: {type: app.value, default: 'NULL'}\n  holder: {type: app.holder, default: 'NULL'}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        let fields = [
            ("enum_value", "app.mood"),
            ("composite_value", "app.pair"),
            ("array_value", "app.value[]"),
        ];
        let mut normal = "  value: {type: app.value, default: 'NULL'}\n  holder: {type: app.holder, default: 'NULL'}\n".to_owned();
        for (column, ty) in fields {
            normal.push_str(&format!("  {column}: {{type: '{ty}'}}\n"));
        }
        d.table(&declarations(&normal));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        for (column, ty) in fields {
            d.table(&declarations(&normal.replace(
                &format!("  {column}: {{type: '{ty}'}}\n"),
                &format!("  {column}: {{type: '{ty}', default: 'NULL'}}\n"),
            )));
            succeeds(d.run(&["plan"]));
            d.commit();
            // Apply the spelling to the engine, then ask the connected planner.
            on_server(
                db,
                &format!("ALTER TABLE app.t ALTER COLUMN {column} SET DEFAULT NULL"),
            );
            assert_eq!(
                scalar(
                    db,
                    &format!(
                        "SELECT count(*)::bigint FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.t'::regclass AND a.attname='{column}'"
                    )
                ),
                0,
                "{ty} erases NULL on {server}"
            );
            let out = d.run(&["plan", "--db", db]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("NULL default the engine does not keep")
                    && stderr(&out).contains(column),
                "{}{}",
                stdout(&out),
                stderr(&out)
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn partition_null_defaults_keep_domains_but_erase_enum_and_composite_constants() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_partition_null");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('ok');
            CREATE TYPE app.pair AS (n integer);
            CREATE DOMAIN app.value AS integer DEFAULT 7 CHECK (VALUE > 0)",
        );
        let d = Demo::new("refs-partition-null");
        let base = declarations(
            "  enum_value: {type: app.mood, default: \"'ok'::app.mood\"}\n  composite_value: {type: app.pair, default: \"'(1)'::app.pair\"}\n  domain_value: {type: app.value}\n",
        );
        let table = |overrides: &str| {
            format!(
                "{base}partition_by: [id]\npartitions:\n  low:\n    from: ['0']\n    to: ['10']\n{overrides}"
            )
        };
        d.table(&table(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        let domain = "    columns:\n      domain_value: {default: 'NULL'}\n";
        d.table(&table(domain));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty());
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.low'::regclass AND a.attname='domain_value'"
            ),
            1
        );
        for column in ["enum_value", "composite_value"] {
            d.table(&table(&format!(
                "{domain}      {column}: {{default: 'NULL'}}\n"
            )));
            succeeds(d.run(&["plan"]));
            d.commit();
            on_server(
                db,
                &format!("ALTER TABLE app.low ALTER COLUMN {column} SET DEFAULT NULL"),
            );
            assert_eq!(
                scalar(
                    db,
                    &format!(
                        "SELECT count(*)::bigint FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.low'::regclass AND a.attname='{column}'"
                    )
                ),
                0
            );
            let out = d.run(&["plan", "--db", db]);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("NULL default the engine does not keep")
                    && stderr(&out).contains(&format!("app.low.{column}")),
                "{}{}",
                stdout(&out),
                stderr(&out)
            );
        }
    }
}
