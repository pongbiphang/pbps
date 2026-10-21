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

struct UntrustedRole {
    server: String,
    name: String,
}

impl UntrustedRole {
    fn new(server: &str, slug: &str) -> Self {
        let name = format!("pbps_refs_{slug}_{}", std::process::id());
        on_server(server, &format!("CREATE ROLE {name}"));
        Self {
            server: server.into(),
            name,
        }
    }
}

impl Drop for UntrustedRole {
    fn drop(&mut self) {
        let _ = try_on_server(&self.server, &format!("DROP ROLE {}", self.name));
    }
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
        succeeds(d.run(&["bootstrap", "--db", db]));
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
        let out = d.run(&["bootstrap", "--sql", script.to_str().unwrap()]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("bootstrap --sql cannot validate"),
            "{}",
            stderr(&out)
        );
        assert!(!script.exists());
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_class WHERE relname='t'"
            ),
            0
        );
    }
}

fn bootstrap_script_boundary(server: &str) {
    let role = UntrustedRole::new(server, "bootstrap_script");
    let own = OwnDatabase::new(server, "refs_script");
    let db = own.connection();
    on_server(
        db,
        &format!(
            "CREATE SCHEMA app; CREATE DOMAIN app.positive AS integer DEFAULT 1 CHECK (VALUE > 0); ALTER DOMAIN app.positive OWNER TO {}",
            role.name
        ),
    );
    let d = Demo::new("refs-script");
    opt_in(&d, db, false);
    let config = d.dir.join("pbps.yml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}trusted_roles: [{}]\n", role.name)).unwrap();
    let typed = declarations("  n: {type: app.positive, default: \"'1'::app.positive\"}\n");
    d.table(&typed);
    succeeds(d.run(&["plan"]));
    d.commit();
    for connected in [true, false] {
        for existing in [false, true] {
            let script = d.dir.join(format!("bootstrap-{connected}-{existing}.sql"));
            let previous = b"keep the previous script\n";
            if existing {
                std::fs::write(&script, previous).unwrap();
            }
            let mut args = vec!["bootstrap", "--sql", script.to_str().unwrap()];
            if connected {
                args.extend(["--db", db]);
            }
            let out = d.run(&args);
            assert_ne!(
                code(&out),
                0,
                "referenced-type script must refuse; connected={connected}; script_exists={}",
                script.exists()
            );
            let why = stderr(&out);
            assert!(
                why.contains("bootstrap --sql cannot validate")
                    && why.contains("connected bootstrap without --sql")
                    && why.contains("plan/apply against the target"),
                "{why}"
            );
            if existing {
                assert_eq!(std::fs::read(&script).unwrap(), previous);
            } else {
                assert!(!script.exists());
            }
        }
    }
    assert_eq!(
        scalar(
            db,
            "SELECT count(*)::bigint FROM pg_catalog.pg_class WHERE relname IN ('__pbps_state','t')"
        ),
        0
    );

    // Reference-free scripts still carry the existing prerequisite guard.
    let plain = OwnDatabase::new(server, "refs_script_plain");
    on_server(plain.connection(), "CREATE SCHEMA app");
    let plain_demo = Demo::new("refs-script-plain");
    opt_in(&plain_demo, plain.connection(), false);
    let config = plain_demo.dir.join("pbps.yml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}trusted_roles: [{}]\n", role.name)).unwrap();
    plain_demo.table(&declarations(""));
    succeeds(plain_demo.run(&["plan"]));
    plain_demo.commit();
    for connected in [false, true] {
        let script = plain_demo.dir.join(format!("plain-{connected}.sql"));
        let mut args = vec!["bootstrap", "--sql", script.to_str().unwrap()];
        if connected {
            args.extend(["--db", plain.connection()]);
        }
        succeeds(plain_demo.run(&args));
        let sql = std::fs::read_to_string(&script).unwrap();
        assert!(sql.contains("CREATE TABLE"), "{sql}");
        assert!(sql.contains("pg_catalog.pg_roles"), "{sql}");
        assert!(sql.contains(&role.name), "{sql}");
    }
    succeeds(plain_demo.run(&["verify", "--db", plain.connection()]));

    // Owner trust still admits connected execution of a builtin-only domain.
    succeeds(d.run(&["bootstrap", "--db", db]));
    succeeds(d.run(&["verify", "--db", db]));
}

#[test]
#[ignore = "needs live PostgreSQL 16"]
fn bootstrap_scripts_refuse_type_references_on_postgres_16() {
    bootstrap_script_boundary(&std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"));
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn bootstrap_scripts_refuse_type_references_on_postgres_18() {
    bootstrap_script_boundary(&server());
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
        // No DEFAULT means this empty table runs no CHECK, but admission uses
        // one rule regardless of row presence (DEC-1523.4).
        on_server(db, "TRUNCATE app.t");
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.seen(integer)")
                && stderr(&out).contains("not built into PostgreSQL")
                && stderr(&out).contains("trusted_roles")
                && stderr(&out).contains("source review and pins"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);

        // Owner trust is the remedy when the CHECK itself is built in.
        let role = UntrustedRole::new(&server, "empty_check");
        on_server(
            db,
            &format!(
                "DROP DOMAIN app.trap;
                 CREATE DOMAIN app.trap AS integer CHECK (VALUE > 0);
                 ALTER DOMAIN app.trap OWNER TO {}",
                role.name
            ),
        );
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains(&role.name)
                && stderr(&out).contains("trusted_roles")
                && stderr(&out).contains("pbps.yml"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: postgres\ntrusted_roles: [{}]\n", role.name),
        )
        .unwrap();
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        drop(own);
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
        opt_in(&d, db, true);
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

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_domains_default_populates_a_new_primary_key_without_a_null_projection() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_default_key");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE DOMAIN app.default_key AS integer DEFAULT 1;
             CREATE TABLE app.measure (id integer); INSERT INTO app.measure VALUES (1);
             ALTER TABLE app.measure ADD COLUMN n app.default_key NOT NULL;
             ALTER TABLE app.measure ADD PRIMARY KEY (n)",
        );
        assert_eq!(scalar(db, "SELECT n::bigint FROM app.measure"), 1);
        on_server(db, "DROP TABLE app.measure");
        let d = Demo::new("refs-default-key");
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1)");
        d.table(
            &declarations("  n: {type: app.default_key, nullable: false}\n")
                .replace("primary_key: [id]", "primary_key: [n]"),
        );
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(approved_apply(
            &d,
            db,
            &path,
            &["--allow", "destructive,not-null,constraint"],
        ));
        assert_eq!(scalar(db, "SELECT n::bigint FROM app.t"), 1);
        succeeds(d.run(&["verify", "--db", db]));
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn fast_domain_defaults_evaluate_on_empty_tables_and_require_trusted_owners() {
    for server in servers() {
        let role = UntrustedRole::new(&server, "empty_default");
        for user_function in [false, true] {
            let own = OwnDatabase::new(&server, "refs_empty_default");
            let db = own.connection();
            let default = if user_function {
                "app.evil()"
            } else {
                "pg_catalog.abs(-7)"
            };
            on_server(
                db,
                &format!(
                    "CREATE SCHEMA app; CREATE SEQUENCE app.runs;
                 CREATE FUNCTION app.evil() RETURNS integer LANGUAGE plpgsql IMMUTABLE AS $$
                 BEGIN PERFORM nextval('app.runs'); RETURN 1; END $$;
                 CREATE DOMAIN app.trap AS integer DEFAULT {default};
                 ALTER FUNCTION app.evil() OWNER TO {owner};
                 ALTER DOMAIN app.trap OWNER TO {owner};
                 GRANT USAGE ON SCHEMA app TO {owner};
                 CREATE TABLE app.measure (id integer);
                 ALTER TABLE app.measure ADD COLUMN value app.trap",
                    owner = role.name
                ),
            );
            assert_eq!(scalar(db, "SELECT count(*) FROM app.measure"), 0);
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*) FROM pg_catalog.pg_attribute WHERE attrelid = 'app.measure'::regclass AND attname = 'value' AND atthasmissing"
                ),
                1,
                "the fast default is stored even without rows"
            );
            assert_eq!(
                scalar(db, "SELECT is_called::integer::bigint FROM app.runs"),
                i64::from(user_function),
                "the immutable default runs as the deployer before any row exists"
            );
            on_server(
                db,
                "DROP TABLE app.measure; ALTER SEQUENCE app.runs RESTART WITH 1",
            );

            let d = Demo::new("refs-empty-default");
            d.keyed(db);
            d.table(&declarations(""));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", db]));
            d.table(&declarations("  value: {type: app.trap}\n"));
            succeeds(d.run(&["plan"]));
            d.commit();
            let refused = d.run(&["plan", "--db", db]);
            assert_ne!(
                code(&refused),
                0,
                "the empty-table plan admitted an untrusted domain default"
            );
            let reason = if user_function {
                "not built into PostgreSQL"
            } else {
                &role.name
            };
            assert!(
                stderr(&refused).contains(reason),
                "{}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert_eq!(scalar(db, "SELECT count(*) FROM app.t"), 0);
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*) FROM pg_catalog.pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'value' AND NOT attisdropped"
                ),
                0
            );
            assert_eq!(
                scalar(db, "SELECT is_called::integer::bigint FROM app.runs"),
                0
            );
            if user_function {
                continue;
            }

            let config = d.dir.join("pbps.yml");
            let mut trusted = std::fs::read_to_string(&config).unwrap();
            trusted.push_str(&format!("\ntrusted_roles: [{}]\n", role.name));
            std::fs::write(config, trusted).unwrap();
            let path = plan(&d, db);
            succeeds(approved_apply(&d, db, &path, &[]));
            succeeds(d.run(&["verify", "--db", db]));
            assert_eq!(scalar(db, "SELECT count(*) FROM app.t"), 0);
            assert_eq!(
                scalar(
                    db,
                    "SELECT (attmissingval::text = '{7}')::integer::bigint FROM pg_catalog.pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'value'"
                ),
                1
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_domain_default_backfills_foreign_keys_but_a_column_default_overrides_it() {
    for server in servers() {
        for explicit_default in [false, true] {
            let own = OwnDatabase::new(&server, "refs_default_fk");
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app; CREATE DOMAIN app.parent_id AS integer DEFAULT 1",
            );
            let d = Demo::new("refs-default-fk");
            d.table(&declarations(""));
            let parent = "table: app.parent\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\ndata:\n  mode: exact\n  rows:\n";
            let parent_path = d.dir.join("schema/app.parent.yml");
            std::fs::write(&parent_path, format!("{parent}    1: {{}}\n    2: {{}}\n")).unwrap();
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", db]));
            on_server(db, "INSERT INTO app.t VALUES (10)");

            // Measure the two fills in the statement order: the domain's
            // own DEFAULT references the deleted row, while an explicit
            // column DEFAULT selects the survivor. Each connection drops
            // its transaction so the measurement leaves the fixture intact.
            let default = if explicit_default { " DEFAULT 2" } else { "" };
            let measured = try_on_server(
                db,
                &format!(
                    "BEGIN; ALTER TABLE app.t ADD COLUMN pid app.parent_id{default};
                         DELETE FROM app.parent WHERE id = 1;
                         ALTER TABLE app.t ADD CONSTRAINT fk_parent
                         FOREIGN KEY (pid) REFERENCES app.parent(id); ROLLBACK"
                ),
            );
            if explicit_default {
                measured.expect("the column DEFAULT selects the surviving parent");
            } else {
                let error = measured.expect_err("the domain DEFAULT selects the deleted parent");
                assert!(error.contains("foreign key"), "{error}");
            }
            assert_eq!(scalar(db, "SELECT count(*) FROM app.parent"), 2);

            d.table(&format!(
                    "{}foreign_keys:\n  fk_parent:\n    columns: [pid]\n    references: app.parent(id)\n",
                    declarations(&format!(
                        "  pid: {{type: app.parent_id{}}}\n",
                        if explicit_default { ", default: '2'" } else { "" }
                    ))
                ));
            std::fs::write(&parent_path, format!("{parent}    2: {{}}\n")).unwrap();
            succeeds(d.run(&["plan"]));
            d.commit();
            let path = d.dir.join("domain-default-fk.plan.json");
            let planned = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
            succeeds(planned);
            let applied = approved_apply(&d, db, &path, &["--allow", "data-delete,constraint"]);
            if explicit_default {
                succeeds(applied);
                assert_eq!(scalar(db, "SELECT pid::bigint FROM app.t"), 2);
                succeeds(d.run(&["verify", "--db", db]));
            } else {
                assert_ne!(
                    code(&applied),
                    0,
                    "{}{}",
                    stdout(&applied),
                    stderr(&applied)
                );
                assert!(
                    stderr(&applied).contains("cannot evaluate")
                        && stderr(&applied).contains("nothing has been changed"),
                    "{}{}",
                    stdout(&applied),
                    stderr(&applied)
                );
                assert_eq!(scalar(db, "SELECT count(*) FROM app.parent"), 2);
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*) FROM pg_catalog.pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'pid' AND NOT attisdropped"
                    ),
                    0
                );
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_null_generated_composite_evaluates_no_attribute_domain_check() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_generated_null");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app;
                 CREATE TABLE app.calls (n integer);
                 CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
                 BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
                 CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
                 CREATE TYPE app.pair AS (n app.checked);
                 CREATE TABLE app.measure (id integer);
                 INSERT INTO app.measure VALUES (1), (2);
                 ALTER TABLE app.measure ADD COLUMN g app.pair
                     GENERATED ALWAYS AS (NULL::app.pair) STORED",
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.measure WHERE g IS NOT DISTINCT FROM NULL"
            ),
            2
        );
        on_server(db, "DROP TABLE app.measure");
        let d = Demo::new("refs-generated-null");
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1), (2)");
        let generated =
            "  g: {type: app.pair, generated: {expression: 'NULL::app.pair', stored: true}}\n";
        d.table(&declarations(generated));
        succeeds(d.run(&["plan"]));
        d.commit();
        opt_in(&d, db, true);
        let path = plan(&d, db);
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE g IS NOT DISTINCT FROM NULL"
            ),
            2
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty());
        opt_in(&d, db, false);
        d.table(&declarations(&format!(
            "{generated}  h: {{type: app.pair, default: 'ROW(1)::app.pair'}}\n"
        )));
        succeeds(d.run(&["plan"]));
        d.commit();
        let refused = d.run(&["plan", "--db", db]);
        assert_ne!(code(&refused), 0);
        assert!(
            stderr(&refused).contains("app.seen"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        on_server(db, "SELECT ROW(1)::app.pair");
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 1);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn routine_overloads_and_relations_have_distinct_dependency_identities() {
    for server in servers() {
        for managed in ["overload", "relation", "dependency"] {
            let own = OwnDatabase::new(&server, &format!("refs_identity_{managed}"));
            let db = own.connection();
            on_server(
                db,
                "CREATE SCHEMA app;
                 CREATE FUNCTION app.f(v text) RETURNS boolean LANGUAGE sql IMMUTABLE
                     RETURN length(v) > 0;
                 CREATE FUNCTION app.f(v integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                     RETURN v > 0;
                 CREATE TABLE app.f (id integer);
                 CREATE DOMAIN app.value AS text CHECK (app.f(VALUE))",
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_catalog.pg_proc WHERE oid IN ('app.f(text)'::regprocedure, 'app.f(integer)'::regprocedure)"
                ),
                2
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_catalog.pg_depend WHERE classid='pg_catalog.pg_constraint'::regclass AND refclassid='pg_catalog.pg_proc'::regclass AND refobjid='app.f(text)'::regprocedure AND objid IN (SELECT oid FROM pg_catalog.pg_constraint WHERE contypid='app.value'::regtype)"
                ),
                1
            );
            let dependency = scalar(db, "SELECT 'app.f(text)'::regprocedure::oid::bigint");
            // Neither a different overload nor a same-named relation owns this dependency.
            on_server(db, "DROP FUNCTION app.f(integer); DROP TABLE app.f");
            let d = Demo::new(&format!("refs-identity-{managed}"));
            d.keyed(db);
            d.table(&declarations(if managed == "dependency" {
                "  value: {type: app.value}\n"
            } else {
                ""
            }));
            let other = if managed == "relation" {
                "table: app.f\ncolumns:\n  id: {type: integer}\n".to_owned()
            } else {
                let arg = if managed == "overload" {
                    "integer"
                } else {
                    "text"
                };
                let body = if managed == "overload" {
                    "v > 0"
                } else {
                    "length(v) > 0"
                };
                format!(
                    "function: app.f({arg})\npublic_execute: true\ndefinition: (v {arg}) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN {body}\n"
                )
            };
            std::fs::write(d.dir.join("schema/other.yml"), other).unwrap();
            succeeds(d.run(&["plan"]));
            d.commit();
            let bootstrap = d.run(&["bootstrap", "--db", db]);
            if managed == "dependency" {
                assert_ne!(code(&bootstrap), 0);
                assert!(
                    stderr(&bootstrap)
                        .contains("depends on managed function `app.f(pg_catalog.text)`"),
                    "{}{}",
                    stdout(&bootstrap),
                    stderr(&bootstrap)
                );
                assert_eq!(
                    scalar(
                        db,
                        "SELECT count(*)::bigint FROM pg_catalog.pg_class WHERE relname IN ('t','__pbps_state')"
                    ),
                    0
                );
            } else {
                succeeds(bootstrap);
                // The reference now needs a reviewed plan; the unrelated
                // overload/relation must still not count as its dependency.
                opt_in(&d, db, true);
                d.table(&declarations("  value: {type: app.value}\n"));
                succeeds(d.run(&["plan"]));
                d.commit();
                let reviewed = plan(&d, db);
                succeeds(apply_plan(&d, db, &reviewed, false));
                succeeds(d.run(&["verify", "--db", db]));
                let next: pbps_model::SavedPlan =
                    serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
                assert!(next.changes.is_empty(), "{managed}: {:?}", next.changes);
            }
            assert_eq!(
                scalar(db, "SELECT 'app.f(text)'::regprocedure::oid::bigint"),
                dependency
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_coercions_keep_null_defaults_on_enum_columns_and_partition_overrides() {
    let setup = "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('ok');
        CREATE DOMAIN app.checked_mood AS app.mood CHECK (VALUE IS NULL OR VALUE = 'ok')";
    let retained = [
        "NULL::app.checked_mood",
        "(NULL::app.checked_mood)::app.mood",
        "CAST(CAST(NULL AS app.checked_mood) AS app.mood)",
    ];
    let erased = [
        "NULL",
        "NULL::app.mood::app.mood",
        "CAST(CAST(NULL AS app.mood) AS app.mood)",
    ];
    // Measure both majors before asking pbps to admit the declaration.
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_cast_measure");
        let db = own.connection();
        on_server(db, setup);
        on_server(
            db,
            "CREATE TABLE app.measure (id integer, mood app.mood) PARTITION BY RANGE (id);
             CREATE TABLE app.low PARTITION OF app.measure FOR VALUES FROM (0) TO (10)",
        );
        for (expressions, count) in [(&retained[..], 2), (&erased[..], 0)] {
            for expr in expressions {
                on_server(
                    db,
                    &format!(
                        "ALTER TABLE app.measure ALTER COLUMN mood SET DEFAULT {expr};
                    ALTER TABLE app.low ALTER COLUMN mood SET DEFAULT {expr}"
                    ),
                );
                assert_eq!(scalar(db, "SELECT count(*)::bigint FROM pg_catalog.pg_attrdef d
                    JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum
                    WHERE d.adrelid IN ('app.measure'::regclass, 'app.low'::regclass) AND a.attname='mood'"), count,
                    "{expr} on {server}");
            }
        }
    }
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_cast_default");
        let db = own.connection();
        on_server(db, setup);
        let d = Demo::new("refs-cast-default");
        let table = |parent: &str, partition: &str| {
            format!(
                "{}partition_by: [id]\npartitions:\n  low:\n    from: ['0']\n    to: ['10']\n    columns:\n      override_mood: {{default: '{partition}'}}\n",
                declarations(&format!(
                    "  mood: {{type: app.mood, default: '{parent}'}}\n  override_mood: {{type: app.mood, default: \"'ok'::app.mood\"}}\n"
                ))
            )
        };
        d.table(&table(retained[0], retained[0]));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        for (index, expr) in retained.into_iter().enumerate() {
            if index > 0 {
                d.table(&table(expr, expr));
                succeeds(d.run(&["plan"]));
                d.commit();
            }
            let path = plan(&d, db);
            succeeds(apply_plan(&d, db, &path, false));
            succeeds(d.run(&["verify", "--db", db]));
            let next: pbps_model::SavedPlan =
                serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
            assert!(next.changes.is_empty(), "{expr}: {:?}", next.changes);
        }
        for expr in erased {
            for (parent, partition, name) in [
                (expr, retained[0], "app.t.mood"),
                (retained[0], expr, "app.low.override_mood"),
            ] {
                d.table(&table(parent, partition));
                succeeds(d.run(&["plan"]));
                d.commit();
                let out = d.run(&["plan", "--db", db]);
                assert_ne!(code(&out), 0, "{expr} at {name} on {server}");
                assert!(
                    stderr(&out).contains(name)
                        && stderr(&out).contains("NULL default the engine does not keep"),
                    "{}",
                    stderr(&out)
                );
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn empty_array_defaults_evaluate_no_element_domain_check() {
    let forms = [
        "ARRAY[]::app.checked[]",
        "ARRAY[ ]::app.checked[]",
        "CAST(ARRAY[] AS app.checked[])",
        "'{}'::app.checked[]",
        "CAST('{}' AS app.checked[])",
    ];
    for server in servers() {
        let role = UntrustedRole::new(&server, "empty_element");
        let own = OwnDatabase::new(&server, "refs_empty_array");
        let db = own.connection();
        on_server(
            db,
            &format!(
                "CREATE SCHEMA app; GRANT USAGE ON SCHEMA app TO {};
            CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(text) RETURNS boolean LANGUAGE plpgsql AS $$
            BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            ALTER FUNCTION app.seen(text) OWNER TO {};
            CREATE DOMAIN app.checked AS text CHECK (app.seen(VALUE));
            CREATE TABLE app.measure (id integer); INSERT INTO app.measure VALUES (1), (2)",
                role.name, role.name
            ),
        );
        // Measure the engine before testing admission: none of these empty
        // values coerces an element, while both non-empty controls do.
        for expr in forms {
            on_server(
                db,
                &format!("ALTER TABLE app.measure ADD COLUMN items app.checked[] DEFAULT {expr}"),
            );
            assert_eq!(
                scalar(db, "SELECT count(*)::bigint FROM app.calls"),
                0,
                "{expr} on {server}"
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM app.measure WHERE cardinality(items)=0"
                ),
                2
            );
            on_server(db, "ALTER TABLE app.measure DROP COLUMN items");
        }
        for expr in ["ARRAY['x']::app.checked[]", "'{x}'::app.checked[]"] {
            on_server(db, &format!("SELECT {expr}"));
            assert!(
                scalar(db, "SELECT count(*)::bigint FROM app.calls") > 0,
                "{expr}"
            );
            on_server(db, "DELETE FROM app.calls");
        }
        let d = Demo::new("refs-empty-array");
        d.keyed(db);
        let table = |extra: &str, overrides: &str| {
            format!(
                "{}partition_by: [id]\npartitions:\n  low:\n    from: ['0']\n    to: ['100']\n{overrides}",
                declarations(extra)
            )
        };
        d.table(&table("", ""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1), (2)");
        let mut columns = String::new();
        let mut overrides = String::from("    columns:\n");
        for (index, expr) in forms.into_iter().enumerate() {
            let expr_yaml = serde_json::to_string(expr).unwrap();
            columns.push_str(&format!("  items{index}: {{type: 'app.checked[]', default: {expr_yaml}}}\n  override{index}: {{type: 'app.checked[]'}}\n"));
            overrides.push_str(&format!(
                "      override{index}: {{default: {expr_yaml}}}\n"
            ));
            d.table(&table(&columns, &overrides));
            succeeds(d.run(&["plan"]));
            d.commit();
            opt_in(&d, db, true);
            let path = plan(&d, db);
            succeeds(apply_plan(&d, db, &path, false));
            on_server(
                db,
                &format!("INSERT INTO app.low (id) VALUES ({})", index + 3),
            );
            assert_eq!(
                scalar(
                    db,
                    &format!(
                        "SELECT count(*)::bigint FROM app.t WHERE cardinality(items{index})=0"
                    )
                ),
                index as i64 + 3
            );
            assert_eq!(
                scalar(
                    db,
                    &format!(
                        "SELECT count(*)::bigint FROM app.low WHERE cardinality(override{index})=0"
                    )
                ),
                1
            );
            assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
            succeeds(d.run(&["verify", "--db", db]));
            let next: pbps_model::SavedPlan =
                serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
            assert!(next.changes.is_empty(), "{expr}: {:?}", next.changes);
        }
        opt_in(&d, db, false);
        for expr in ["ARRAY['x']::app.checked[]", "'{x}'::app.checked[]"] {
            let extra = format!(
                "{columns}  refused: {{type: 'app.checked[]', default: {}}}\n",
                serde_json::to_string(expr).unwrap()
            );
            d.table(&table(&extra, &overrides));
            succeeds(d.run(&["plan"]));
            d.commit();
            let out = d.run(&["plan", "--db", db]);
            assert_ne!(code(&out), 0, "{expr} on {server}");
            assert!(
                stderr(&out).contains("app.seen") && stderr(&out).contains(&role.name),
                "{}",
                stderr(&out)
            );
            assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_catalog.pg_attribute WHERE attrelid='app.t'::regclass AND attname='refused' AND NOT attisdropped"
                ),
                0
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn stored_generated_empty_arrays_evaluate_no_element_domain_check() {
    let setup = |role: &str| {
        format!(
            "CREATE SCHEMA app; GRANT USAGE ON SCHEMA app TO {role};
            CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(text) RETURNS boolean LANGUAGE plpgsql AS $$
            BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            ALTER FUNCTION app.seen(text) OWNER TO {role};
            CREATE DOMAIN app.checked AS text CHECK (app.seen(VALUE))"
        )
    };
    // Measure both majors before the admission assertion can fail on either one.
    for server in servers() {
        let role = UntrustedRole::new(&server, "generated_measure");
        let own = OwnDatabase::new(&server, "refs_generated_measure");
        let db = own.connection();
        on_server(db, &setup(&role.name));
        on_server(
            db,
            "CREATE TABLE app.measure (id integer); INSERT INTO app.measure VALUES (1), (2);
            ALTER TABLE app.measure ADD COLUMN items app.checked[]
                GENERATED ALWAYS AS ('{}'::app.checked[]) STORED",
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.measure WHERE cardinality(items)=0"
            ),
            2
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_type t
                JOIN pg_catalog.pg_type a ON a.oid=t.typarray
                JOIN pg_catalog.pg_roles r ON r.oid=t.typowner
                WHERE t.oid='app.checked'::regtype AND a.typowner=t.typowner
                  AND r.rolname=current_user"
            ),
            1
        );
        on_server(db, "SELECT '{x}'::app.checked[]");
        assert!(scalar(db, "SELECT count(*)::bigint FROM app.calls") > 0);
        let major = scalar(db, "SELECT current_setting('server_version_num')::bigint") / 10000;
        eprintln!(
            "PostgreSQL {major}: a stored generated empty array evaluates no element CHECK; a nonempty array evaluates it"
        );
    }
    for server in servers() {
        let role = UntrustedRole::new(&server, "generated_fill");
        let own = OwnDatabase::new(&server, "refs_generated_empty");
        let db = own.connection();
        on_server(db, &setup(&role.name));
        let d = Demo::new("refs-generated-empty");
        d.keyed(db);
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1), (2)");
        let generated = "  items: {type: 'app.checked[]', generated: {expression: \"'{}'::app.checked[]\", stored: true}}\n";
        d.table(&declarations(generated));
        succeeds(d.run(&["plan"]));
        d.commit();
        opt_in(&d, db, true);
        let path = plan(&d, db);
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE cardinality(items)=0"
            ),
            2
        );
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty());
        opt_in(&d, db, false);
        d.table(&declarations(&format!(
            "{generated}  refused: {{type: 'app.checked[]', generated: {{expression: \"'{{x}}'::app.checked[]\", stored: true}}}}\n"
        )));
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.seen") && stderr(&out).contains(&role.name),
            "{}",
            stderr(&out)
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_attribute
                WHERE attrelid='app.t'::regclass AND attname='refused' AND NOT attisdropped"
            ),
            0
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn empty_arrays_still_evaluate_outer_domain_checks_and_keep_array_type_ownership() {
    for server in servers() {
        let role = UntrustedRole::new(&server, "empty_outer");
        let own = OwnDatabase::new(&server, "refs_empty_outer");
        let db = own.connection();
        on_server(db, &format!("CREATE SCHEMA app; GRANT USAGE ON SCHEMA app TO {};
            CREATE TABLE app.calls (kind text);
            CREATE FUNCTION app.element_seen(text) RETURNS boolean LANGUAGE plpgsql AS $$
            BEGIN INSERT INTO app.calls VALUES ('element'); RETURN true; END $$;
            CREATE DOMAIN app.checked AS text CHECK (app.element_seen(VALUE));
            CREATE FUNCTION app.outer_seen(app.checked[]) RETURNS boolean LANGUAGE plpgsql AS $$
            BEGIN INSERT INTO app.calls VALUES ('outer'); RETURN true; END $$;
            ALTER FUNCTION app.outer_seen(app.checked[]) OWNER TO {};
            CREATE DOMAIN app.outer_array AS app.checked[] CHECK (app.outer_seen(VALUE));
            CREATE TABLE app.measure (id integer); INSERT INTO app.measure VALUES (1), (2);
            ALTER TABLE app.measure ADD COLUMN items app.outer_array DEFAULT ARRAY[]::app.checked[]", role.name, role.name));
        assert!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.calls WHERE kind='outer'"
            ) > 0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.calls WHERE kind='element'"
            ),
            0
        );
        on_server(db, "DELETE FROM app.calls");
        let d = Demo::new("refs-empty-outer");
        d.keyed(db);
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        on_server(db, "INSERT INTO app.t VALUES (1), (2)");
        d.table(&declarations(
            "  items: {type: app.outer_array, default: 'ARRAY[]::app.checked[]'}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("app.outer_seen") && stderr(&out).contains(&role.name),
            "{}",
            stderr(&out)
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        on_server(
            db,
            &format!("ALTER DOMAIN app.checked OWNER TO {}", role.name),
        );
        // PostgreSQL changes the array's owner along with the element type's.
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_type e JOIN pg_catalog.pg_type a ON a.oid=e.typarray WHERE e.oid='app.checked'::regtype AND a.typowner=e.typowner"
            ),
            1
        );
        d.table(&declarations(
            "  items: {type: 'app.checked[]', default: 'ARRAY[]::app.checked[]'}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains(&role.name) && stderr(&out).contains("owned by"),
            "{}",
            stderr(&out)
        );
        let config = d.dir.join("pbps.yml");
        let mut trusted = std::fs::read_to_string(&config).unwrap();
        trusted.push_str(&format!("trusted_roles: [{}]\n", role.name));
        std::fs::write(config, trusted).unwrap();
        opt_in(&d, db, true);
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM app.t WHERE cardinality(items)=0"
            ),
            2
        );
        assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.calls"), 0);
        succeeds(d.run(&["verify", "--db", db]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn polymorphic_index_defaults_survive_multiple_domain_layers() {
    let setup = "CREATE SCHEMA app;
        CREATE TYPE app.mood AS ENUM ('ok');
        CREATE TYPE app.pair AS (n integer, s text);
        CREATE DOMAIN app.mood_domain AS app.mood;
        CREATE DOMAIN app.pair_domain AS app.pair;
        CREATE DOMAIN app.moods_domain AS app.mood[];
        CREATE DOMAIN app.int_range AS int4range;
        CREATE DOMAIN app.int_multirange AS int4multirange;
        CREATE DOMAIN app.mood_d2 AS app.mood_domain;
        CREATE DOMAIN app.pair_d2 AS app.pair_domain;
        CREATE DOMAIN app.moods_d2 AS app.moods_domain;
        CREATE DOMAIN app.range_d2 AS app.int_range;
        CREATE DOMAIN app.multirange_d2 AS app.int_multirange";
    let columns = [
        ("mood", "app.mood"),
        ("pair", "app.pair"),
        ("moods", "app.mood[]"),
        ("pairs", "app.pair[]"),
        ("dm", "app.mood_domain"),
        ("dp", "app.pair_domain"),
        ("da", "app.moods_domain"),
        ("dr", "app.int_range"),
        ("dmr", "app.int_multirange"),
        ("de2", "app.mood_d2"),
        ("dc2", "app.pair_d2"),
        ("da2", "app.moods_d2"),
        ("dr2", "app.range_d2"),
        ("dm2", "app.multirange_d2"),
    ];
    // Measure both majors before relying on pbps's classification of a key.
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_index_measure");
        let db = own.connection();
        on_server(db, setup);
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_type
            WHERE oid='app.mood_d2'::regtype AND typbasetype='app.mood_domain'::regtype"
            ),
            1
        );
        on_server(
            db,
            &format!(
                "CREATE TABLE app.measure ({})",
                columns
                    .iter()
                    .map(|(name, ty)| format!("{name} {ty}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        for (name, _) in columns {
            on_server(
                db,
                &format!("CREATE INDEX ix_{name} ON app.measure ({name})"),
            );
        }
        for (names, input, count) in [
            ("'ix_mood','ix_dm','ix_de2'", "anyenum", 3),
            ("'ix_pair','ix_dp','ix_dc2'", "record", 3),
            ("'ix_moods','ix_pairs','ix_da','ix_da2'", "anyarray", 4),
            ("'ix_dr','ix_dr2'", "anyrange", 2),
            ("'ix_dmr','ix_dm2'", "anymultirange", 2),
        ] {
            assert_eq!(
                scalar(
                    db,
                    &format!(
                        "SELECT count(*)::bigint FROM pg_catalog.pg_index i
                JOIN pg_catalog.pg_class c ON c.oid=i.indexrelid
                JOIN pg_catalog.pg_opclass oc ON oc.oid=i.indclass[0]
                WHERE i.indrelid='app.measure'::regclass AND c.relname IN ({names})
                  AND oc.opcdefault AND oc.opcintype='pg_catalog.{input}'::regtype"
                    )
                ),
                count
            );
        }
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_cast
                WHERE castsource IN ('app.mood'::regtype, 'app.pair'::regtype,
                                     'app.mood[]'::regtype, 'app.pair[]'::regtype,
                                     'pg_catalog.int4range'::regtype, 'pg_catalog.int4multirange'::regtype)
                  AND casttarget IN ('pg_catalog.anyenum'::regtype,
                                     'pg_catalog.record'::regtype, 'pg_catalog.anyarray'::regtype,
                                     'pg_catalog.anyrange'::regtype, 'pg_catalog.anymultirange'::regtype)"
            ),
            0
        );
        on_server(
            db,
            "CREATE INDEX ix_image ON app.measure (pair pg_catalog.record_image_ops)",
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_catalog.pg_index i
            JOIN pg_catalog.pg_opclass oc ON oc.oid=i.indclass[0]
            WHERE i.indexrelid='app.ix_image'::regclass AND NOT oc.opcdefault
              AND oc.opcintype='pg_catalog.record'::regtype"
            ),
            1
        );
        let major = scalar(db, "SELECT current_setting('server_version_num')::bigint") / 10000;
        eprintln!(
            "PostgreSQL {major}: enum/domain keys use anyenum, composite/domain keys use record, array/domain keys use anyarray, range/multirange domain keys use anyrange/anymultirange; record_image_ops is nondefault"
        );
    }
    for server in servers() {
        let own = OwnDatabase::new(&server, "refs_index_cli");
        let db = own.connection();
        on_server(db, setup);
        let d = Demo::new("refs-index-roundtrip");
        let mut extra = String::new();
        let mut indexes = String::from("indexes:\n");
        for (name, ty) in columns {
            extra.push_str(&format!("  {name}: {{type: '{ty}'}}\n"));
            indexes.push_str(&format!("  ix_{name}: {{columns: [{name}]}}\n"));
        }
        d.table(&format!("{}{indexes}", declarations(&extra)));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        succeeds(d.run(&["verify", "--db", db]));
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty(), "{:?}", next.changes);
        // A connected apply must also read back an enum index it just built.
        d.table(&format!(
            "{}{indexes}  ix_mood_id: {{columns: [mood, id]}}\n",
            declarations(&extra)
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        succeeds(apply_plan(&d, db, &path, false));
        succeeds(d.run(&["verify", "--db", db]));
        succeeds(d.run(&["pull", "--db", db, "--force"]));
        succeeds(d.run(&["validate"]));
        let pulled = std::fs::read_to_string(d.dir.join("schema/app.t.yml")).unwrap();
        for (name, _) in columns {
            assert!(pulled.contains(&format!("ix_{name}")), "{pulled}");
        }
        let next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(plan(&d, db)).unwrap()).unwrap();
        assert!(next.changes.is_empty(), "{:?}", next.changes);
        // The polymorphic input type does not make every record class default.
        on_server(
            db,
            "DROP INDEX app.ix_pair; CREATE INDEX ix_pair ON app.t (pair pg_catalog.record_image_ops)",
        );
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("record_image_ops"),
            "{}",
            stderr(&out)
        );
    }
}

/// The setting belongs to the review policy, not to the measured fill shape.
fn opt_in(d: &Demo, db: &str, enabled: bool) {
    if !std::fs::read_to_string(d.dir.join("pbps.yml"))
        .unwrap()
        .contains("environments:")
    {
        d.keyed(db);
    }
    let path = d.dir.join("pbps.yml");
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text
        .lines()
        .filter(|line| !line.starts_with("pin_type_routines:"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{text}\npin_type_routines: {enabled}\n")).unwrap();
}

fn refused_plan(d: &Demo, db: &str, message: &str) {
    let out = d.run(&["plan", "--db", db]);
    assert_ne!(code(&out), 0, "{}", stdout(&out));
    assert!(stderr(&out).contains(message), "{}", stderr(&out));
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn potential_type_paths_are_checked_even_without_an_added_column() {
    for server in servers() {
        let role = UntrustedRole::new(&server, "potential");
        let own = OwnDatabase::new(&server, "potential");
        let db = own.connection();
        on_server(
            db,
            &format!(
                "CREATE SCHEMA app; CREATE DOMAIN app.checked AS integer CHECK (VALUE > 0); ALTER DOMAIN app.checked OWNER TO {}; CREATE TABLE app.t (id integer PRIMARY KEY, n app.checked)",
                role.name
            ),
        );
        let d = Demo::new("potential");
        d.table(&declarations("  n: {type: app.checked}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "existing table"]));
        // No changes at all: eligibility belongs to the referenced type.
        opt_in(&d, db, true);
        refused_plan(&d, db, "trusted_roles");
        on_server(db, "ALTER DOMAIN app.checked OWNER TO CURRENT_USER");
        let path = plan(&d, db);
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(saved.changes.is_empty());
        on_server(
            db,
            &format!(
                "CREATE TYPE app.free AS ENUM ('ok'); ALTER TYPE app.free OWNER TO {}",
                role.name
            ),
        );
        d.table(&declarations(
            "  n: {type: app.checked}\n  free: {type: app.free}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let admitted = plan(&d, db);
        succeeds(apply_plan(&d, db, &admitted, false));
    }
}

fn root_demo(db: &str, slug: &str, root: &str) -> Demo {
    on_server(
        db,
        &format!(
            "CREATE SCHEMA app; CREATE SEQUENCE app.calls; {root}; CREATE DOMAIN app.checked AS varchar CHECK (app.root(VALUE))"
        ),
    );
    let d = Demo::new(slug);
    d.keyed(db);
    d.table(&declarations(""));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", db]));
    on_server(db, "INSERT INTO app.t VALUES (1)");
    d.table(&declarations(
        "  n: {type: app.checked, default: \"'X'\"}\n",
    ));
    succeeds(d.run(&["plan"]));
    d.commit();
    d
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn any_language_roots_require_source_review_and_the_saved_opt_in() {
    for server in servers() {
        for (index, body) in [
            "LANGUAGE plpgsql AS $$ BEGIN RETURN lower($1) = 'x'; END $$",
            "LANGUAGE sql AS $$ SELECT lower($1) = 'x' $$",
            "LANGUAGE sql RETURN lower($1) = 'x'",
        ]
        .into_iter()
        .enumerate()
        {
            let own = OwnDatabase::new(&server, &format!("review_{index}"));
            let db = own.connection();
            let d = root_demo(
                db,
                "source-review",
                &format!("CREATE FUNCTION app.root(varchar) RETURNS boolean {body}"),
            );
            refused_plan(&d, db, "pin_type_routines");
            opt_in(&d, db, true);
            let path = plan(&d, db);
            let saved: pbps_model::SavedPlan =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(saved.type_routines.len(), 1);
            assert!(saved.type_routine_pins.is_some());
            let source = &saved.type_routines[0].definition;
            assert!(source.contains("lower"), "{source}");
            for format in ["text", "json"] {
                let mut args = vec!["explain", "--plan", path.to_str().unwrap()];
                if format == "json" {
                    args.extend(["--format", "json"]);
                }
                let out = d.run(&args);
                assert_eq!(code(&out), 0, "{}", stderr(&out));
                assert!(stdout(&out).contains("lower"));
                assert!(stdout(&out).contains("whatever they call"));
            }
            opt_in(&d, db, false);
            let changed = apply_plan(&d, db, &path, false);
            assert_ne!(code(&changed), 0);
            assert!(
                stderr(&changed).contains("pin_type_routines changed"),
                "{}",
                stderr(&changed)
            );
            opt_in(&d, db, true);
            succeeds(apply_plan(&d, db, &path, false));
            succeeds(d.run(&["verify", "--db", db]));
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn io_coercion_roots_require_source_review_on_postgres_18() {
    io_coercion_roots_require_source_review(&server());
}

#[test]
#[ignore = "needs live PostgreSQL 16"]
fn io_coercion_roots_require_source_review_on_postgres_16() {
    io_coercion_roots_require_source_review(
        &std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    );
}

fn io_coercion_roots_require_source_review(server: &str) {
    let own = OwnDatabase::new(server, "io_review");
    let db = own.connection();
    on_server(
        db,
        "CREATE SCHEMA app; CREATE SEQUENCE app.calls;
         CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
           BEGIN PERFORM nextval('app.calls'); RETURN true; END $$;
         CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
         CREATE DOMAIN app.outer_text AS text DEFAULT '{1}'
           CHECK (VALUE::app.checked[] IS NOT NULL);",
    );
    let d = Demo::new("io-review");
    d.keyed(db);
    d.table(&declarations(""));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", db]));
    on_server(db, "INSERT INTO app.t VALUES (1)");
    d.table(&declarations("  n: {type: app.outer_text}\n"));
    succeeds(d.run(&["plan"]));
    d.commit();
    refused_plan(&d, db, "pin_type_routines");
    assert_eq!(
        scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
        0
    );
    opt_in(&d, db, true);
    let path = plan(&d, db);
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved.type_routines.len(), 1);
    assert!(saved.type_routines[0].definition.contains("nextval"));
    assert!(saved.type_routine_pins.is_some());
    assert_eq!(
        scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
        0
    );
    let out = d.run(&["explain", "--plan", path.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("nextval"));
    succeeds(apply_plan(&d, db, &path, false));
    assert_eq!(
        scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
        1
    );
    succeeds(d.run(&["verify", "--db", db]));
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn superuser_owned_root_replacements_are_pinned_separately_from_replaceable_callees() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "root_pin");
        let db = own.connection();
        let d = root_demo(
            db,
            "root-pin",
            "CREATE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN RETURN true; END $$",
        );
        opt_in(&d, db, true);
        let path = plan(&d, db);
        on_server(
            db,
            "CREATE OR REPLACE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN PERFORM 1; RETURN true; END $$",
        );
        let out = apply_plan(&d, db, &path, false);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("reviewed type routines changed"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_attribute WHERE attrelid='app.t'::regclass AND attname='n' AND NOT attisdropped"
            ),
            0
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_unchanged_opaque_root_has_a_changed_callee_refused_before_any_statement() {
    for server in servers() {
        for staged in [false, true] {
            let role = UntrustedRole::new(&server, "callee");
            let own = OwnDatabase::new(&server, "callee_pins");
            let db = own.connection();
            let d = root_demo(
                db,
                "callee-pin",
                "CREATE FUNCTION app.leaf(varchar) RETURNS boolean LANGUAGE sql AS $$ SELECT true $$; CREATE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN RETURN app.leaf($1); END $$",
            );
            on_server(
                db,
                &format!(
                    "GRANT USAGE ON SCHEMA app TO {}; ALTER FUNCTION app.leaf(varchar) OWNER TO {}",
                    role.name, role.name
                ),
            );
            opt_in(&d, db, true);
            let path = d.dir.join("callee.json");
            let mut args = vec!["plan", "--db", db, "--out", path.to_str().unwrap()];
            if staged {
                args.push("--staged");
            }
            succeeds(d.run(&args));
            let before = text_scalar(
                db,
                "SELECT pg_get_functiondef('app.root(varchar)'::regprocedure)",
            );
            on_server(
                db,
                "CREATE OR REPLACE FUNCTION app.leaf(varchar) RETURNS boolean LANGUAGE sql AS $$ SELECT length($1) >= 0 $$",
            );
            assert_eq!(
                before,
                text_scalar(
                    db,
                    "SELECT pg_get_functiondef('app.root(varchar)'::regprocedure)"
                )
            );
            let out = apply_plan(&d, db, &path, staged);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("pinned routines changed"),
                "{}",
                stderr(&out)
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_attribute WHERE attrelid='app.t'::regclass AND attname='n' AND NOT attisdropped"
                ),
                0
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_scoped_path_overload_created_during_a_statement_is_detected_before_commit() {
    for server in servers() {
        let role = UntrustedRole::new(&server, "overload");
        let own = OwnDatabase::new(&server, "scoped_overload");
        let db = own.connection();
        let d = root_demo(
            db,
            "scoped-overload",
            "CREATE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN RETURN lower($1) = 'x'; END $$",
        );
        opt_in(&d, db, true);
        let path = plan(&d, db);
        // The event trigger is superuser-held: its new overload appears after
        // the pre-statement check and is replaceable by an ordinary role.
        on_server(
            db,
            &format!(
                r#"GRANT USAGE ON SCHEMA app TO {role};
            CREATE FUNCTION app.redirect() RETURNS event_trigger LANGUAGE plpgsql AS $outer$
            BEGIN
              IF TG_TAG = 'ALTER TABLE' THEN
                EXECUTE 'CREATE FUNCTION app.lower(varchar) RETURNS text LANGUAGE plpgsql AS $inner$ BEGIN PERFORM nextval(''app.calls''); RETURN ''x''; END $inner$';
                EXECUTE 'ALTER FUNCTION app.lower(varchar) OWNER TO {role}';
              END IF;
            END $outer$;
            CREATE EVENT TRIGGER redirect ON ddl_command_start EXECUTE FUNCTION app.redirect()"#,
                role = role.name
            ),
        );
        let root = text_scalar(
            db,
            "SELECT pg_get_functiondef('app.root(varchar)'::regprocedure)",
        );
        let out = apply_plan(&d, db, &path, false);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("DEC-319.1"), "{}", stderr(&out));
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            1
        );
        assert_eq!(
            root,
            text_scalar(
                db,
                "SELECT pg_get_functiondef('app.root(varchar)'::regprocedure)"
            )
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_attribute WHERE attrelid='app.t'::regclass AND attname='n' AND NOT attisdropped"
            ),
            0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_proc WHERE pronamespace='app'::regnamespace AND proname='lower'"
            ),
            0
        );
    }
}

fn text_scalar(db: &str, sql: &str) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, db)
            .await
            .unwrap();
        conn.query(sql).await.unwrap()[0]
            .try_get_at::<&str>(0)
            .unwrap()
            .unwrap()
            .to_owned()
    })
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn staged_transactional_steps_rollback_changed_roots_and_concurrent_indexes_detect_them_after_commit()
 {
    for server in servers() {
        for online in [false, true] {
            let own = OwnDatabase::new(&server, "staged_root");
            let db = own.connection();
            let original = "CREATE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN RETURN true; END $$";
            let d = root_demo(db, "staged-root", original);
            opt_in(&d, db, true);
            let initial = plan(&d, db);
            succeeds(apply_plan(&d, db, &initial, false));
            d.table(&format!(
                "{}strategy: {{online: {online}}}\nindexes:\n  ix_n: {{columns: [n]}}\n",
                declarations("  n: {type: app.checked, default: \"'X'\"}\n")
            ));
            succeeds(d.run(&["plan"]));
            d.commit();
            let path = d.dir.join("staged.json");
            succeeds(d.run(&[
                "plan",
                "--db",
                db,
                "--out",
                path.to_str().unwrap(),
                "--staged",
            ]));
            on_server(
                db,
                r#"CREATE FUNCTION app.replace_root() RETURNS event_trigger LANGUAGE plpgsql AS $outer$
                BEGIN
                  IF TG_TAG = 'CREATE INDEX' THEN
                    EXECUTE 'CREATE OR REPLACE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $inner$ BEGIN PERFORM nextval(''app.calls''); RETURN true; END $inner$';
                  END IF;
                END $outer$;
                CREATE EVENT TRIGGER replace_root ON ddl_command_start EXECUTE FUNCTION app.replace_root()"#,
            );
            let out = apply_plan(&d, db, &path, true);
            assert_ne!(code(&out), 0);
            assert!(
                stderr(&out).contains("reviewed type routines changed"),
                "{}",
                stderr(&out)
            );
            assert_eq!(
                scalar(
                    db,
                    "SELECT count(*)::bigint FROM pg_class WHERE oid=to_regclass('app.ix_n')"
                ),
                i64::from(online)
            );
            if online {
                assert!(stderr(&out).contains("committed"), "{}", stderr(&out));
                let checksum = plan_checksum(&path);
                let resume = || {
                    d.run(&[
                        "apply",
                        "--db",
                        db,
                        "--plan",
                        path.to_str().unwrap(),
                        "--checksum",
                        &checksum,
                        "--staged",
                        "--resume",
                    ])
                };
                let refused = resume();
                assert_ne!(code(&refused), 0);
                assert!(
                    stderr(&refused).contains("reviewed type routines changed"),
                    "{}",
                    stderr(&refused)
                );
                on_server(
                    db,
                    &original.replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1),
                );
                succeeds(resume());
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn immediate_bootstrap_requires_reviewed_plans_for_user_written_type_roots() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "bootstrap_review");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE FUNCTION app.root(integer) RETURNS boolean LANGUAGE sql AS $$ SELECT true $$; CREATE DOMAIN app.checked AS integer CHECK(app.root(VALUE))",
        );
        let d = Demo::new("bootstrap-review");
        opt_in(&d, db, true);
        d.table(&declarations("  n: {type: app.checked}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = d.run(&["bootstrap", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("immediate bootstrap cannot run type routines"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_class WHERE oid=to_regclass('app.t')"
            ),
            0
        );
    }
}

fn partition_review_declarations(own: &str) -> String {
    format!(
        "{}partition_by: [id]\npartitions:\n  low:\n    from: ['0']\n    to: ['10']\n{own}",
        declarations("  n: {type: 'app.checked[]', default: \"'{}'::app.checked[]\"}\n")
    )
}

fn partition_review_demo(db: &str) -> Demo {
    on_server(
        db,
        "CREATE SCHEMA app; CREATE SEQUENCE app.calls;
         CREATE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$
         BEGIN PERFORM nextval('app.calls'); RETURN true; END $$;
         CREATE DOMAIN app.checked AS varchar CHECK(app.root(VALUE))",
    );
    let d = Demo::new("partition-source-review");
    opt_in(&d, db, true);
    d.table(&partition_review_declarations(""));
    succeeds(d.run(&["plan"]));
    d.commit();
    d
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn partition_default_parsing_runs_domain_checks_only_after_source_approval() {
    let mut observed = Vec::new();
    for server in servers() {
        let own = OwnDatabase::new(&server, "partition_source_review");
        let db = own.connection();
        let d = partition_review_demo(db);
        on_server(
            db,
            "CREATE TABLE app.t(id integer PRIMARY KEY, n app.checked[] DEFAULT '{}'::app.checked[]) PARTITION BY RANGE(id);
             CREATE TABLE app.low PARTITION OF app.t FOR VALUES FROM (0) TO (10)",
        );
        succeeds(d.run(&[
            "baseline",
            "--db",
            db,
            "--reason",
            "existing partition tree",
        ]));
        d.table(&partition_review_declarations(
            "    columns:\n      n: {default: \"'{x}'::app.checked[]\"}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        opt_in(&d, db, false);
        refused_plan(&d, db, "pin_type_routines");
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            0
        );
        opt_in(&d, db, true);
        let path = d.dir.join("partition-review.plan.json");
        let out = d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]);
        succeeds(out);
        observed.push((
            text_scalar(db, "SELECT current_setting('server_version')"),
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
        ));
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.type_routines.len(), 1);
        assert!(saved.type_routines[0].definition.contains("nextval"));
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            1
        );
        succeeds(d.run(&["verify", "--db", db]));
    }
    assert!(
        observed.iter().all(|(_, called)| *called == 0),
        "before source approval: {observed:?}"
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn bootstrap_refuses_before_partition_default_parsing_can_advance_a_sequence() {
    let mut observed = Vec::new();
    for server in servers() {
        let own = OwnDatabase::new(&server, "bootstrap_partition_review");
        let db = own.connection();
        let d = partition_review_demo(db);
        d.table(&partition_review_declarations(
            "    columns:\n      n: {default: \"'{x}'::app.checked[]\"}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let rehearsal = d.run(&["plan", "--dev", db]);
        assert_ne!(code(&rehearsal), 0);
        assert!(
            stderr(&rehearsal).contains("dev rehearsal is not supported"),
            "{}",
            stderr(&rehearsal)
        );
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            0
        );
        let out = d.run(&["bootstrap", "--db", db]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("immediate bootstrap cannot run type routines"),
            "{}",
            stderr(&out)
        );
        observed.push((
            text_scalar(db, "SELECT current_setting('server_version')"),
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
        ));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*)::bigint FROM pg_class WHERE relname IN ('t','low','__pbps_state')"
            ),
            0
        );
    }
    assert!(
        observed.iter().all(|(_, called)| *called == 0),
        "before bootstrap refusal: {observed:?}"
    );
}

fn builtin_partition_probe(bootstrap: bool) {
    let mut observed = Vec::new();
    for server in servers() {
        // Stable is also non-immutable; immutable built-ins retain the probe.
        for (check, unchecked) in [
            ("pg_catalog.nextval('app.calls') > 0", true),
            ("pg_catalog.current_setting('server_version') <> ''", true),
            ("pg_catalog.length(VALUE) > 0", false),
        ] {
            let own = OwnDatabase::new(&server, "builtin_partition_probe");
            let db = own.connection();
            on_server(
                db,
                &format!(
                    "CREATE SCHEMA app; CREATE SEQUENCE app.calls;
                     CREATE DOMAIN app.checked AS varchar CHECK ({check});
                     CREATE TABLE app.t(id integer PRIMARY KEY, n app.checked[] DEFAULT '{{}}'::app.checked[]) PARTITION BY RANGE(id);
                     CREATE TABLE app.low PARTITION OF app.t FOR VALUES FROM (0) TO (10)"
                ),
            );
            let d = Demo::new("builtin-partition-probe");
            d.table(&partition_review_declarations(""));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&[
                "baseline",
                "--db",
                db,
                "--reason",
                "existing partition tree",
            ]));
            d.table(&partition_review_declarations(
                "    columns:\n      n: {default: \"'{x}'::app.checked[]\"}\n",
            ));
            succeeds(d.run(&["plan"]));
            d.commit();
            assert_eq!(
                scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
                0
            );
            let output = if bootstrap {
                // This command cannot build over an existing managed table.
                // Its earlier probe must not change a sequence before refusal.
                let out = d.run(&["bootstrap", "--db", db]);
                assert_ne!(code(&out), 0, "{}", stdout(&out));
                assert!(stderr(&out).contains("already has"), "{}", stderr(&out));
                out
            } else {
                succeeds(d.run(&["plan", "--db", db]))
            };
            let message = stderr(&output);
            observed.push((
                text_scalar(db, "SELECT current_setting('server_version')"),
                check,
                scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
                message
                    .contains("a referenced input path can run non-immutable code before approval"),
                unchecked,
                message,
            ));
        }
    }
    assert!(
        observed.iter().all(|(_, _, called, _, _, _)| *called == 0),
        "a pre-approval probe changed a sequence: {observed:?}"
    );
    assert!(
        observed.iter().all(
            |(_, _, _, warning, unchecked, message)| warning == unchecked
                && message.contains("not checked before the plan") == *unchecked
        ),
        "only non-immutable paths remain unchecked: {observed:?}"
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn plan_probes_do_not_run_nextval_in_domain_checks_and_only_probe_immutable_paths() {
    builtin_partition_probe(false);
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn bootstrap_probes_do_not_run_nextval_in_domain_checks_and_only_probe_immutable_paths() {
    builtin_partition_probe(true);
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn reviewed_domain_defaults_execute_at_ddl_time_but_sql_running_builtins_stay_refused() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "default_review");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE SEQUENCE app.calls; CREATE FUNCTION app.fill() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('app.calls'); RETURN 7; END $$; CREATE DOMAIN app.checked AS integer DEFAULT app.fill()",
        );
        let d = Demo::new("default-review");
        d.keyed(db);
        d.table(&declarations(""));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", db]));
        // Even without rows, ADD COLUMN evaluates this non-volatile expression
        // once. Mark the fixture IMMUTABLE to exercise fast-default evaluation.
        on_server(db, "ALTER FUNCTION app.fill() IMMUTABLE");
        d.table(&declarations("  n: {type: app.checked}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        refused_plan(&d, db, "pin_type_routines");
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            0
        );
        opt_in(&d, db, true);
        let path = plan(&d, db);
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.type_routines[0].name, "fill");
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            0
        );
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            1
        );
        on_server(
            db,
            "CREATE DOMAIN app.sql_trap AS varchar CHECK (pg_catalog.query_to_xml('SELECT 1',true,false,'') IS NOT NULL)",
        );
        d.table(&declarations(
            "  n: {type: app.checked}\n  trap: {type: app.sql_trap}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        refused_plan(&d, db, "runs SQL or code named by its arguments");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn set_default_and_create_parse_nested_domain_literals_while_stored_values_do_not_recheck() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "definition_input");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE SEQUENCE app.calls;
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN PERFORM nextval('app.calls'); RETURN true; END $$;
            CREATE DOMAIN app.checked AS integer CHECK(app.seen(VALUE));
            CREATE TYPE app.pair AS (n app.checked)");
        for sql in [
            "CREATE TABLE app.m (n app.pair DEFAULT '(1)')",
            "ALTER TABLE app.m ALTER COLUMN n SET DEFAULT '(2)'",
            "CREATE TABLE app.a (n app.checked[] DEFAULT '{1}')",
            "ALTER TABLE app.a ALTER COLUMN n SET DEFAULT '{2}'",
        ] {
            on_server(db, "SELECT setval('app.calls',1,false)");
            on_server(db, sql);
            assert_eq!(
                scalar(
                    db,
                    "SELECT last_value::bigint FROM app.calls WHERE is_called"
                ),
                1,
                "{sql}"
            );
        }
        on_server(
            db,
            "CREATE TABLE app.stored (n app.checked); INSERT INTO app.stored VALUES (1),(2)",
        );
        for sql in [
            "UPDATE app.stored SET n=n",
            "ALTER TABLE app.stored ADD CHECK (n>0)",
            "CREATE INDEX direct ON app.stored(n)",
        ] {
            on_server(db, "SELECT setval('app.calls',1,false)");
            on_server(db, sql);
            assert_eq!(
                scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
                0,
                "{sql}"
            );
        }
        for sql in [
            "ALTER TABLE app.stored ADD CHECK ((n::integer)::app.checked>0)",
            "CREATE INDEX recast ON app.stored(((n::integer)::app.checked))",
        ] {
            on_server(db, "SELECT setval('app.calls',1,false)");
            on_server(db, sql);
            assert_eq!(
                scalar(
                    db,
                    "SELECT last_value::bigint FROM app.calls WHERE is_called"
                ),
                2,
                "{sql}"
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn generated_expression_replacement_rechecks_domains_on_18_and_is_refused_on_16() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "expression_input");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE SEQUENCE app.calls;
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('app.calls'); RETURN true; END $$;
            CREATE DOMAIN app.checked AS integer CHECK(app.seen(VALUE));
            CREATE TABLE app.t (id integer PRIMARY KEY, n app.checked GENERATED ALWAYS AS (id*2) STORED);
            INSERT INTO app.t (id) VALUES (1),(2)");
        let major = scalar(db, "SELECT current_setting('server_version_num')::bigint") / 10000;
        on_server(db, "SELECT setval('app.calls',1,false)");
        let measured = try_on_server(
            db,
            "ALTER TABLE app.t ALTER COLUMN n SET EXPRESSION AS (id*3)",
        );
        if major == 16 {
            assert!(measured.is_err());
            assert_eq!(
                scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
                0
            );
        } else {
            measured.unwrap();
            assert_eq!(scalar(db, "SELECT last_value::bigint FROM app.calls"), 2);
        }
        let d = Demo::new("expression-input");
        d.keyed(db);
        d.table(&declarations(&format!(
            "  n: {{type: app.checked, generated: {{expression: 'id*{}', stored: true}}}}\n",
            if major == 16 { 2 } else { 3 }
        )));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "generated fixture"]));
        d.table(&declarations(
            "  n: {type: app.checked, generated: {expression: 'id*4', stored: true}}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        on_server(db, "SELECT setval('app.calls',1,false)");
        refused_plan(&d, db, "pin_type_routines");
        assert_eq!(
            scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
            0
        );
        opt_in(&d, db, true);
        if major == 16 {
            refused_plan(&d, db, "PostgreSQL 17");
        } else {
            let path = plan(&d, db);
            let saved: pbps_model::SavedPlan =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(
                saved
                    .changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::AlterColumnExpression { .. }))
            );
            assert!(
                !saved
                    .changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::AddColumn { .. }))
            );
            succeeds(approved_apply(&d, db, &path, &["--allow", "narrowing"]));
            assert_eq!(scalar(db, "SELECT last_value::bigint FROM app.calls"), 2);
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn existing_type_eligibility_covers_metadata_scans_rebuilds_and_baseline_only_references() {
    for server in servers() {
        let role = UntrustedRole::new(&server, "all_operations");
        for case in [
            "default",
            "drop-default",
            "not-null",
            "check",
            "index",
            "rename",
            "drop",
            "create",
        ] {
            let own = OwnDatabase::new(&server, "operation_eligibility");
            let db = own.connection();
            on_server(
                db,
                &format!(
                    "CREATE SCHEMA app; CREATE DOMAIN app.checked AS integer CHECK(VALUE>0); ALTER DOMAIN app.checked OWNER TO {}; CREATE TABLE app.t(id integer PRIMARY KEY,n app.checked DEFAULT 7); INSERT INTO app.t VALUES(1,7)",
                    role.name
                ),
            );
            let d = Demo::new("operation-eligibility");
            d.table(&declarations("  n: {type: app.checked, default: '7'}\n"));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "existing column"]));
            match case {
                "default"=>d.table(&declarations("  n: {type: app.checked, default: '8'}\n")),
                "drop-default"=>d.table(&declarations("  n: {type: app.checked}\n")),
                "not-null"=>d.table(&declarations("  n: {type: app.checked, default: '7', nullable: false}\n")),
                "check"=>d.table(&format!("{}checks:\n  positive: '(n::integer)::app.checked > 0'\n",declarations("  n: {type: app.checked, default: '7'}\n"))),
                "index"=>d.table(&format!("{}indexes:\n  recast:\n    keys:\n      - expression: '(n::integer)::app.checked'\n",declarations("  n: {type: app.checked, default: '7'}\n"))),
                "rename"=>{ d.table(&declarations("  renamed: {type: app.checked, default: '7'}\n"));succeeds(d.run(&["rename","app.t.n","renamed"])); },
                "drop"=>{d.table(&declarations(""));succeeds(d.run(&["drop","app.t.n","--reason","no longer needed"]));},
                "create"=>std::fs::write(d.dir.join("schema/new.yml"),"table: app.new\ncolumns:\n  n: {type: app.checked}\n").unwrap(),
                _=>unreachable!(),
            }
            succeeds(d.run(&["plan"]));
            d.commit();
            refused_plan(&d, db, "trusted_roles");
            let config = d.dir.join("pbps.yml");
            let text = std::fs::read_to_string(&config).unwrap();
            std::fs::write(config, format!("{text}trusted_roles: [{}]\n", role.name)).unwrap();
            let path = plan(&d, db);
            let saved: pbps_model::SavedPlan =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(
                !saved
                    .changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::AddColumn { .. })),
                "{case}"
            );
            succeeds(approved_apply(
                &d,
                db,
                &path,
                &[
                    "--allow",
                    "constraint",
                    "--allow",
                    "not-null",
                    "--allow",
                    "narrowing",
                    "--allow",
                    "rename",
                    "--allow",
                    "destructive",
                ],
            ));
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_existing_reference_cannot_be_unpinned_from_a_saved_default_change() {
    for server in servers() {
        let own = OwnDatabase::new(&server, "baseline_pin");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; CREATE DOMAIN app.checked AS varchar CHECK(length(VALUE)>0); CREATE TABLE app.t(id integer PRIMARY KEY,n app.checked DEFAULT 'X')",
        );
        let d = Demo::new("baseline-pin");
        d.table(&declarations(
            "  n: {type: app.checked, default: \"'X'\"}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "existing reference"]));
        d.table(&declarations(
            "  n: {type: app.checked, default: \"'Y'\"}\n",
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = plan(&d, db);
        let mut saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            saved
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::AlterColumnDefault { .. }))
        );
        assert!(
            !saved
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::AddColumn { .. }))
        );
        saved.type_pins.clear();
        std::fs::write(&path, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();
        let out = apply_plan(&d, db, &path, false);
        assert_ne!(code(&out), 0, "{}", stdout(&out));
        assert!(
            stderr(&out).contains("no type pin for `app.checked`"),
            "{}",
            stderr(&out)
        );
    }
}

fn empty_apply_checks_referenced_type_prerequisites(server: &str) {
    let own = OwnDatabase::new(server, "refs_empty_apply");
    let db = own.connection();
    let root = "CREATE OR REPLACE FUNCTION app.root(varchar) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('app.calls'); RETURN true; END $$";
    on_server(
        db,
        &format!(
            "CREATE SCHEMA app; CREATE SEQUENCE app.calls; {root}; CREATE DOMAIN app.checked AS varchar CHECK (app.root(VALUE)); CREATE TABLE app.t(id integer PRIMARY KEY,n app.checked)"
        ),
    );
    let d = Demo::new("refs-empty-apply");
    opt_in(&d, db, true);
    d.table(&declarations("  n: {type: app.checked}\n"));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", db, "--reason", "existing reference"]));
    let path = plan(&d, db);
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    assert_eq!(saved.type_pins.len(), 1);
    assert_eq!(saved.type_routines.len(), 1);
    let out = succeeds(apply_plan(&d, db, &path, false));
    assert!(stdout(&out).contains("nothing to apply"));
    assert_eq!(
        scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
        1
    );

    let mut unpinned = saved.clone();
    unpinned.type_pins.clear();
    let edited = d.dir.join("unpinned.json");
    std::fs::write(&edited, serde_json::to_vec_pretty(&unpinned).unwrap()).unwrap();
    let out = apply_plan(&d, db, &edited, false);
    assert_ne!(code(&out), 0, "an empty apply accepted missing type pins");
    assert!(
        stderr(&out).contains("no type pin for `app.checked`"),
        "{}",
        stderr(&out)
    );

    on_server(db, &root.replace("RETURN true", "RETURN length($1) > 0"));
    let out = apply_plan(&d, db, &path, false);
    assert_ne!(
        code(&out),
        0,
        "an empty apply accepted a replaced reviewed root"
    );
    assert!(
        stderr(&out).contains("reviewed type routines changed"),
        "{}",
        stderr(&out)
    );
    on_server(db, root);
    on_server(
        db,
        "ALTER DOMAIN app.checked ADD CONSTRAINT nonempty CHECK (length(VALUE) > 0) NOT VALID",
    );
    let out = apply_plan(&d, db, &path, false);
    assert_ne!(code(&out), 0, "an empty apply accepted a changed type");
    assert!(
        stderr(&out).contains("changed since planning"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        scalar(db, "SELECT is_called::int::bigint FROM app.calls"),
        0
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*)::bigint FROM public.__pbps_state WHERE kind <> 'failed'"
        ),
        1
    );

    // An unrelated routine's replacement did not invalidate reference-free
    // no-ops before this gate; checking every empty plan would change that.
    let role = UntrustedRole::new(server, "empty_apply_plain");
    let plain = OwnDatabase::new(server, "refs_empty_plain");
    let db = plain.connection();
    on_server(
        db,
        &format!(
            "CREATE SCHEMA app; CREATE TABLE app.t(id integer PRIMARY KEY); CREATE FUNCTION app.unused() RETURNS integer LANGUAGE sql AS 'SELECT 1'; ALTER FUNCTION app.unused() OWNER TO {}",
            role.name
        ),
    );
    let d = Demo::new("refs-empty-plain");
    d.keyed(db);
    d.table(&declarations(""));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", db, "--reason", "reference-free table"]));
    let path = plan(&d, db);
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.changes.is_empty() && saved.type_pins.is_empty());
    assert!(saved.routine_pins.is_some());
    on_server(
        db,
        "CREATE OR REPLACE FUNCTION app.unused() RETURNS integer LANGUAGE sql AS 'SELECT 2'",
    );
    let out = succeeds(apply_plan(&d, db, &path, false));
    assert!(stdout(&out).contains("nothing to apply"));
    assert_eq!(
        scalar(db, "SELECT count(*)::bigint FROM public.__pbps_state"),
        1
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16"]
fn empty_apply_checks_referenced_type_prerequisites_on_postgres_16() {
    empty_apply_checks_referenced_type_prerequisites(
        &std::env::var("PBPS_TEST_PG_OLD_DB").expect("PG 16 fixture"),
    );
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn empty_apply_checks_referenced_type_prerequisites_on_postgres_18() {
    empty_apply_checks_referenced_type_prerequisites(&server());
}
