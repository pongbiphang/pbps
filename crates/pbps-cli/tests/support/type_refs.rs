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
            d.table(&declarations("  value: {type: app.value}\n"));
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
        assert!(
            !stderr(&out)
                .split("error:")
                .last()
                .unwrap()
                .contains("app.element_seen"),
            "{}",
            stderr(&out)
        );
        let config = d.dir.join("pbps.yml");
        let mut trusted = std::fs::read_to_string(&config).unwrap();
        trusted.push_str(&format!("trusted_roles: [{}]\n", role.name));
        std::fs::write(config, trusted).unwrap();
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
