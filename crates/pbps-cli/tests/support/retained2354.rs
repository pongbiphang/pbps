//! Shared CHECK/DEFAULT retention and conservative fallback across label renames.
use super::*;

struct Scratch {
    database: OwnDatabase,
    role: String,
    connection: String,
}
impl Scratch {
    fn new(server: &str) -> Self {
        let database = OwnDatabase::new(server, "retained2354scratch");
        let role = format!("retained2354_{}", std::process::id());
        on_server(
            server,
            &format!(
                "CREATE ROLE {role} LOGIN PASSWORD 'retained2354' NOSUPERUSER NOCREATEDB NOCREATEROLE; GRANT pg_read_all_settings TO {role}; ALTER DATABASE {} OWNER TO {role}",
                database.name
            ),
        );
        let connection = format!(
            "{} user={role} password=retained2354",
            database.connection()
        );
        Self {
            database,
            role,
            connection,
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.database.server,
            &format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.database.name
            ),
        );
        let _ = try_on_server(
            &self.database.server,
            &format!("DROP ROLE IF EXISTS {}", self.role),
        );
    }
}
struct Fixture {
    own: OwnDatabase,
    scratch: Scratch,
    d: Demo,
    path: PathBuf,
    ty: String,
}
impl Fixture {
    fn new(server: &str, tag: &str, before: &str, default: bool) -> Self {
        Self::typed(server, tag, before, default, "app.mood", "[sad, ok]")
    }
    fn typed(server: &str, tag: &str, before: &str, default: bool, ty: &str, values: &str) -> Self {
        Self::declared(server, tag, &table(before, default, false), ty, values)
    }
    fn declared(server: &str, tag: &str, declaration: &str, ty: &str, values: &str) -> Self {
        let own = OwnDatabase::new(server, "retained2354target");
        on_server(own.connection(), "CREATE SCHEMA app");
        let d = Demo::new(tag);
        std::fs::write(
            d.dir.join("schema/app.mood.type.yml"),
            format!("enum: app.mood\nvalues: {values}\n"),
        )
        .unwrap();
        if ty == "app.dm" {
            std::fs::write(
                d.dir.join("schema/app.dm.type.yml"),
                "domain: app.dm\ntype: app.mood\n",
            )
            .unwrap();
        }
        d.table(&declaration.replace("type: app.mood", &format!("type: {ty:?}")));
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", own.connection()]));
        let scratch = Scratch::new(server);
        std::fs::write(d.dir.join("pbps.yml"),"dialect: postgres\nresolvers:\n  scratch:\n    kind: server\n    url_env: PBPS_RETAINED2354_SCRATCH\n").unwrap();
        let path = d.dir.join("retained.plan.json");
        Self {
            own,
            scratch,
            d,
            path,
            ty: ty.into(),
        }
    }
    fn rename(&self, expression: &str, default: bool) {
        std::fs::write(
            self.d.dir.join("schema/app.mood.type.yml"),
            "enum: app.mood\nvalues: [{name: blue, renamed_from: sad}, ok]\n",
        )
        .unwrap();
        self.d.table(
            &table(expression, default, true)
                .replace("type: app.mood", &format!("type: {:?}", self.ty)),
        );
        self.d.commit();
    }
    fn plan(&self, resolver: bool, staged: bool) -> pbps_model::SavedPlan {
        let mut args = vec![
            "plan",
            "--db",
            self.own.connection(),
            "--out",
            self.path.to_str().unwrap(),
            "--format",
            "json",
        ];
        if resolver {
            args.extend(["--resolve-with", "scratch", "--no-rehearse"]);
        }
        if staged {
            args.push("--staged");
        }
        succeeds(self.d.run_with_env(
            &args,
            &[("PBPS_RETAINED2354_SCRATCH", &self.scratch.connection)],
        ));
        serde_json::from_str(&std::fs::read_to_string(&self.path).unwrap()).unwrap()
    }
    fn apply(&self, staged: bool, resume: bool) -> Output {
        let checksum = plan_checksum(&self.path);
        let mut args = vec![
            "apply",
            "--db",
            self.own.connection(),
            "--plan",
            self.path.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--allow",
            "rename,constraint",
        ];
        if staged {
            args.push("--staged");
        }
        if resume {
            args.push("--resume");
        }
        self.d.run(&args)
    }
    fn identity(&self) -> String {
        text_of(
            self.own.connection(),
            "SELECT oid::text || '/' || conbin::text || '/' || convalidated::text FROM pg_constraint WHERE conrelid='app.t'::regclass AND conname='allowed'",
        )
    }
}
fn table(expression: &str, default: bool, renamed: bool) -> String {
    let default = if default {
        format!(", default: \"'{}'\"", if renamed { "blue" } else { "sad" })
    } else {
        String::new()
    };
    format!(
        "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: app.mood{default}}}\nprimary_key: [id]\nchecks:\n  allowed: {expression:?}\n"
    )
}
fn retained(plan: &pbps_model::SavedPlan) -> usize {
    pbps_model::retained::expressions(&plan.changes)
        .filter(|e| matches!(e.target, pbps_model::ExpressionTarget::Check { .. }))
        .count()
}
fn rebuilt(plan: &pbps_model::SavedPlan) -> bool {
    plan.changes.changes.iter().any(|p| {
        matches!(
            p.change,
            pbps_model::Change::DropCheck { .. } | pbps_model::Change::AddCheck { .. }
        )
    })
}
fn servers() -> [String; 2] {
    [
        std::env::var("PBPS_TEST_PG_DB").unwrap(),
        std::env::var("PBPS_TEST_PG_OLD_DB").unwrap(),
    ]
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_million_rows_keep_the_check_and_advance_only_the_approved_record() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354million", "m <> 'sad'", true);
        on_server(
            f.own.connection(),
            "INSERT INTO app.t SELECT g,'ok'::app.mood FROM generate_series(1,1000000) g",
        );
        let before = f.identity();
        f.rename("m <> 'blue'", true);
        let plan = f.plan(true, false);
        assert_eq!(retained(&plan), 1, "{:?}", plan.changes);
        assert!(!rebuilt(&plan));
        assert_eq!(retained_defaults(&plan), 1);
        assert!(!altered_default(&plan));
        // The default connected rehearsal must preserve the optimized plan too.
        succeeds(f.d.run_with_env(
            &[
                "plan",
                "--db",
                f.own.connection(),
                "--resolve-with",
                "scratch",
                "--out",
                f.path.to_str().unwrap(),
            ],
            &[("PBPS_RETAINED2354_SCRATCH", &f.scratch.connection)],
        ));
        let rehearsed: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        assert_eq!(retained(&rehearsed), 1);
        assert!(rehearsed.rehearsal.is_some());
        succeeds(f.apply(false, false));
        assert_eq!(
            f.identity(),
            before,
            "the oid, tree and validation must survive"
        );
        let state = latest_snapshot(f.own.connection());
        assert_eq!(
            state.declared.expressions.checks[&"app.t".parse().unwrap()]["allowed"],
            "m <> 'blue'"
        );
        assert_eq!(
            scalar(f.own.connection(), "SELECT count(*) FROM app.t"),
            1000000
        );
        assert!(f.plan(true, false).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_text_semantics_and_missing_resolver_keep_the_rebuild() {
    for server in servers() {
        for (i, before, after) in [
            (
                0,
                "m IS NOT NULL AND 'sad' <> 'other'",
                "m IS NOT NULL AND 'blue' <> 'other'",
            ),
            (1, "m <> 'sad'", "m = 'blue'"),
            (2, "m::text <> 'sad'", "m::text <> 'blue'"),
            (
                3,
                "m <> ('sad'::text)::app.mood",
                "m <> ('blue'::text)::app.mood",
            ),
            (4, "m <> 'sad' AND id > 0", "m <> 'blue' OR id > 0"),
        ] {
            let f = Fixture::new(&server, &format!("retained2354negative{i}"), before, false);
            f.rename(after, false);
            let p = f.plan(true, false);
            assert_eq!(retained(&p), 0);
            assert!(rebuilt(&p), "{:?}", p.changes);
        }
        let f = Fixture::new(&server, "retained2354offline", "m <> 'sad'", false);
        f.rename("m <> 'blue'", false);
        assert!(rebuilt(&f.plan(false, false)));
        succeeds(f.d.run(&[
            "plan",
            "--since",
            "HEAD^",
            "--out",
            f.path.to_str().unwrap(),
        ]));
        let offline: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        assert!(rebuilt(&offline));
        assert_eq!(retained(&offline), 0);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_tampered_payloads_refuse_and_fresh_binding_cannot_be_skipped() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354tamper", "m <> 'sad'", false);
        f.rename("m <> 'blue'", false);
        let original = f.plan(true, false);
        assert_eq!(retained(&original), 1);
        for structural in [true, false] {
            let mut plan = original.clone();
            for p in &mut plan.changes.changes {
                if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut p.change {
                    if structural {
                        retained[0].anchors.clear();
                    } else {
                        retained[0].after = "m IS NOT NULL".into();
                    }
                }
            }
            std::fs::write(&f.path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
            let out = f.apply(false, false);
            assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            assert!(stderr(&out).contains("retained CHECK"), "{}", stderr(&out));
            assert_eq!(
                text_of(
                    f.own.connection(),
                    "SELECT enumlabel FROM pg_enum WHERE enumtypid='app.mood'::regtype ORDER BY enumsortorder LIMIT 1"
                ),
                "sad"
            );
            assert_eq!(
                latest_snapshot(f.own.connection())
                    .declared
                    .expressions
                    .checks[&"app.t".parse().unwrap()]["allowed"],
                "m <> 'sad'"
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_arrays_domains_chains_and_type_moves_preserve_bound_values() {
    for server in servers() {
        for (tag, ty, before, after) in [
            (
                "array",
                "app.mood[]",
                "m <> '{sad}'::app.mood[]",
                "m <> '{blue}'::app.mood[]",
            ),
            (
                "domain",
                "app.dm",
                "m::app.mood <> 'sad'",
                "m::app.mood <> 'blue'",
            ),
        ] {
            let f = Fixture::typed(
                &server,
                &format!("retained2354{tag}"),
                before,
                false,
                ty,
                "[sad, ok]",
            );
            let identity = f.identity();
            f.rename(after, false);
            let p = f.plan(true, false);
            assert_eq!(retained(&p), 1, "{p:?}");
            succeeds(f.apply(false, false));
            assert_eq!(f.identity(), identity);
            assert!(f.plan(true, false).changes.is_empty());
        }
        let f = Fixture::typed(
            &server,
            "retained2354chain",
            "m <> 'sad' AND m <> 'blue'",
            false,
            "app.mood",
            "[sad, blue, ok]",
        );
        let identity = f.identity();
        f.rename("m <> 'blue' AND m <> 'green'", false);
        std::fs::write(f.d.dir.join("schema/app.mood.type.yml"),"enum: app.mood\nvalues: [{name: blue, renamed_from: sad}, {name: green, renamed_from: blue}, ok]\n").unwrap();
        f.d.commit();
        let p = f.plan(true, false);
        assert_eq!(retained(&p), 1);
        assert_eq!(
            pbps_model::retained::expressions(&p.changes)
                .next()
                .unwrap()
                .anchors
                .len(),
            2
        );
        succeeds(f.apply(false, false));
        assert_eq!(f.identity(), identity);
        assert!(f.plan(true, false).changes.is_empty());
        drop(f);
        let f = Fixture::new(&server, "retained2354move", "m <> 'sad'", false);
        let identity = f.identity();
        on_server(f.own.connection(), "CREATE SCHEMA moved");
        f.rename("m <> 'blue'", false);
        std::fs::write(f.d.dir.join("schema/app.mood.type.yml"),"enum: moved.feeling\nrenamed_from: app.mood\nvalues: [{name: blue, renamed_from: sad}, ok]\n").unwrap();
        f.d.table(
            &table("m <> 'blue'", false, true).replace("type: app.mood", "type: moved.feeling"),
        );
        succeeds(f.d.run(&["plan", "--no-dev"]));
        f.d.commit();
        assert_eq!(retained(&f.plan(true, false)), 1);
        succeeds(f.apply(false, false));
        assert_eq!(f.identity(), identity);
        assert!(f.plan(true, false).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_collations_floats_arguments_and_unavailable_scratch_rebuild() {
    for server in servers() {
        for (i, before, after) in [
            (
                0,
                "m <> 'sad' AND ('a' COLLATE \"C\") < ('z' COLLATE \"C\")",
                "m <> 'blue' AND ('a' COLLATE \"POSIX\") < ('z' COLLATE \"POSIX\")",
            ),
            (
                1,
                "m <> 'sad' AND '1.000000000000001'::float8 <> 0",
                "m <> 'blue' AND '1.000000000000002'::float8 <> 0",
            ),
            (
                2,
                "m <> 'sad' AND length(m::text) > 0",
                "m <> 'blue' AND length(m::text) > 1",
            ),
        ] {
            let f = Fixture::new(&server, &format!("retained2354semantic{i}"), before, false);
            f.rename(after, false);
            let p = f.plan(true, false);
            assert!(rebuilt(&p));
            assert_eq!(retained(&p), 0);
        }
        let mut f = Fixture::new(&server, "retained2354unavailable", "m <> 'sad'", false);
        f.rename("m <> 'blue'", false);
        f.scratch.connection =
            "host=127.0.0.1 port=1 user=missing dbname=missing connect_timeout=1 sslmode=disable"
                .into();
        assert!(rebuilt(&f.plan(true, false)));
        drop(f);
        let f = Fixture::new(&server, "retained2354stale", "m <> 'sad'", false);
        f.rename("m <> 'sad'", false);
        f.plan(true, false);
        let out = f.apply(false, false);
        assert_ne!(code(&out), 0);
        assert!(stderr(&out).contains("app.t"), "{}", stderr(&out));
        assert_eq!(
            text_of(
                f.own.connection(),
                "SELECT enumlabel FROM pg_enum WHERE enumtypid='app.mood'::regtype ORDER BY enumsortorder LIMIT 1"
            ),
            "sad"
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_staged_failures_keep_the_old_record_until_verified_closure() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354staged", "m <> 'sad'", false);
        f.rename("m <> 'blue'", false);
        let original = f.plan(true, true);
        assert_eq!(retained(&original), 1);
        let mut bad = original.clone();
        for p in &mut bad.changes.changes {
            if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut p.change {
                retained[0].after = "m IS NOT NULL".into();
            }
        }
        std::fs::write(&f.path, serde_json::to_string_pretty(&bad).unwrap()).unwrap();
        let out = f.apply(true, false);
        assert_ne!(code(&out), 0);
        assert_eq!(
            latest_snapshot(f.own.connection())
                .declared
                .expressions
                .checks[&"app.t".parse().unwrap()]["allowed"],
            "m <> 'sad'"
        );
        std::fs::write(&f.path, serde_json::to_string_pretty(&original).unwrap()).unwrap();
        // Sequence increments survive the carrier savepoint: stop only the closing proof.
        on_server(
            f.own.connection(),
            r#"
CREATE SCHEMA witness;
CREATE SEQUENCE witness.proofs;
CREATE FUNCTION witness.stop_closing() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_query() LIKE '%CREATE TEMP TABLE pg_temp.pbps_retained_%'
     AND nextval('witness.proofs') > 1 THEN
    RAISE EXCEPTION 'stop the closing proof';
  END IF;
END $$;
CREATE EVENT TRIGGER stop_closing ON ddl_command_start WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION witness.stop_closing();
"#,
        );
        let stopped = f.apply(true, false);
        assert_ne!(
            code(&stopped),
            0,
            "{}{}",
            stdout(&stopped),
            stderr(&stopped)
        );
        let checkpoint = latest_snapshot(f.own.connection());
        assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 1);
        assert_eq!(
            checkpoint.declared.expressions.checks[&"app.t".parse().unwrap()]["allowed"],
            "m <> 'sad'"
        );
        on_server(f.own.connection(), "DROP EVENT TRIGGER stop_closing");
        succeeds(f.apply(true, true));
        let state = latest_snapshot(f.own.connection());
        assert!(state.staged.is_none());
        assert_eq!(
            state.declared.expressions.checks[&"app.t".parse().unwrap()]["allowed"],
            "m <> 'blue'"
        );
        assert!(f.plan(true, false).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_relation_lock_allows_readers_and_writers_but_blocks_check_replacement() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354locks", "m <> 'sad'", false);
        on_server(f.own.connection(), "INSERT INTO app.t VALUES (1,'ok')");
        f.rename("m <> 'blue'", false);
        let plan = f.plan(true, false);
        assert_eq!(retained(&plan), 1);
        let state = latest_snapshot(f.own.connection());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            use pbps_dialect::Dialect;
            let mut conn=pbps_db::Conn::connect(pbps_db::Driver::Postgres,f.own.connection()).await.unwrap();
            let pg=pbps_pg::Postgres::new();
            let framing=pg.transaction_framing(conn.session_zone().unwrap());
            conn.begin(&framing).await.unwrap();
            let held=pbps_pg::retained::hold(&mut conn,&plan.changes,Some(&state.declared.overlay(&state.schema))).await.unwrap();
            conn.execute("ALTER TYPE app.mood RENAME VALUE 'sad' TO 'blue'").await.unwrap();
            held.verify(&mut conn,&pg,&state.schema).await.unwrap();
            let rows=conn.query("SELECT mode FROM pg_locks WHERE pid=pg_backend_pid() AND relation='app.t'::regclass").await.unwrap();
            assert!(rows.iter().any(|r|r.try_get::<&str>("mode").unwrap()==Some("AccessShareLock")));
            let mut other=pbps_db::Conn::connect(pbps_db::Driver::Postgres,f.own.connection()).await.unwrap();
            other.execute("SET lock_timeout='100ms'; SELECT 1 FROM app.t; UPDATE app.t SET m='ok' WHERE id=1").await.unwrap();
            let error=other.execute("ALTER TABLE app.t DROP CONSTRAINT allowed").await.unwrap_err();
            assert_eq!(error.server_error_code().as_deref(),Some("55P03"));
            // A ledger write in this transaction must not release the relation lock.
            pbps_pg::state::record(&mut conn,&state).await.unwrap();
            assert!(other.execute("ALTER TABLE app.t DROP CONSTRAINT allowed").await.is_err());
            conn.rollback(&framing).await.unwrap();
        });
    }
}

struct Role {
    server: String,
    name: String,
    database: String,
}
impl Drop for Role {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.server,
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.database),
        );
        let _ = try_on_server(&self.server, &format!("DROP ROLE IF EXISTS {}", self.name));
    }
}
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_missing_temp_and_denied_retention_queries_keep_the_ordinary_plan() {
    for server in servers() {
        for denied in [false, true] {
            let f = Fixture::new(&server, "retained2354capabilities", "m <> 'sad'", false);
            let role = Role {
                server: server.clone(),
                name: format!("retained2354actor_{}", std::process::id()),
                database: f.own.name.clone(),
            };
            on_server(
                &server,
                &format!(
                    "CREATE ROLE {} LOGIN PASSWORD 'retained2354actor' NOSUPERUSER; GRANT pg_read_all_settings TO {}; ALTER DATABASE {} OWNER TO {}",
                    role.name, role.name, f.own.name, role.name
                ),
            );
            on_server(
                f.own.connection(),
                &format!(
                    "ALTER SCHEMA app OWNER TO {r}; ALTER TYPE app.mood OWNER TO {r}; ALTER TABLE app.t OWNER TO {r}; ALTER TABLE public.__pbps_state OWNER TO {r}; ALTER TABLE public.__pbps_lock OWNER TO {r}",
                    r = role.name
                ),
            );
            if denied {
                // Ordinary introspection still works; the new capability read cannot answer.
                on_server(
                    f.own.connection(),
                    "REVOKE EXECUTE ON FUNCTION pg_catalog.has_column_privilege(text,text,text) FROM PUBLIC",
                );
            } else {
                on_server(
                    &server,
                    &format!(
                        "REVOKE TEMP ON DATABASE {} FROM PUBLIC, {}",
                        f.own.name, role.name
                    ),
                );
            }
            f.rename("m <> 'blue'", false);
            let connection = format!(
                "{} user={} password=retained2354actor",
                f.own.connection(),
                role.name
            );
            succeeds(f.d.run_with_env(
                &[
                    "plan",
                    "--db",
                    &connection,
                    "--resolve-with",
                    "scratch",
                    "--no-rehearse",
                    "--out",
                    f.path.to_str().unwrap(),
                ],
                &[("PBPS_RETAINED2354_SCRATCH", &f.scratch.connection)],
            ));
            let p: pbps_model::SavedPlan =
                serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
            assert!(rebuilt(&p), "denied={denied}: {p:?}");
            assert_eq!(retained(&p), 0);
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_replaced_function_bodies_still_refuse_through_existing_pins() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354functionpins", "m <> 'sad'", false);
        let role = Role {
            server: server.clone(),
            name: format!("retained2354routine_{}", std::process::id()),
            database: f.own.name.clone(),
        };
        on_server(&server, &format!("CREATE ROLE {}", role.name));
        on_server(
            f.own.connection(),
            &format!(
                "CREATE FUNCTION app.f(integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 > 0; ALTER FUNCTION app.f(integer) OWNER TO {}; ALTER TABLE app.t DROP CONSTRAINT allowed; ALTER TABLE app.t ADD CONSTRAINT allowed CHECK (m <> 'sad' AND app.f(id))",
                role.name
            ),
        );
        f.d.keyed(f.own.connection());
        f.d.table(&table("m <> 'sad' AND app.f(id)", false, false));
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.own.connection(),
            "--reason",
            "adopt the fixture's function CHECK",
        ]));
        std::fs::write(
            f.d.dir.join("externals.sql"),
            "CREATE FUNCTION app.f(integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1 > 0;",
        )
        .unwrap();
        let config = f.d.dir.join("pbps.yml");
        std::fs::write(
            &config,
            std::fs::read_to_string(&config).unwrap().replace(
                "    url_env: PBPS_RETAINED2354_SCRATCH",
                "    url_env: PBPS_RETAINED2354_SCRATCH\n    externals: externals.sql",
            ),
        )
        .unwrap();
        f.rename("m <> 'blue' AND app.f(id)", false);
        let complete = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            complete.replace("    externals: externals.sql\n", ""),
        )
        .unwrap();
        let missing = f.plan(true, false);
        assert!(rebuilt(&missing));
        assert_eq!(retained(&missing), 0);
        std::fs::write(&config, complete).unwrap();
        let p = f.plan(true, false);
        assert!(
            p.routine_pins.is_some(),
            "the replaceable function must be pinned"
        );
        assert_eq!(retained(&p), 1, "{p:?}");
        on_server(
            f.own.connection(),
            "CREATE OR REPLACE FUNCTION app.f(integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN true",
        );
        let out = f.apply(false, false);
        assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        assert_eq!(
            text_of(
                f.own.connection(),
                "SELECT enumlabel FROM pg_enum WHERE enumtypid='app.mood'::regtype ORDER BY enumsortorder LIMIT 1"
            ),
            "sad"
        );
    }
}

fn layout_schema(expression: &str, renamed: bool) -> String {
    table(expression, false, renamed).replace("primary_key:", "  n: {type: app.mood}\nprimary_key:")
}
fn layout_retention(server: &str, hole: bool) {
    let f = Fixture::declared(
        server,
        "retained2354layout",
        &layout_schema("m <> 'sad'", false),
        "app.mood",
        "[sad, ok]",
    );
    // Rebuild the physical fixture without changing its logical declaration.
    // A second enum column makes an incorrect identity mapping observable.
    on_server(
        f.own.connection(),
        &format!(
            "DROP TABLE app.t; CREATE TABLE app.t ({}m app.mood); \
             ALTER TABLE app.t ADD COLUMN id integer NOT NULL; \
             ALTER TABLE app.t ADD COLUMN n app.mood; {} \
             ALTER TABLE app.t ADD PRIMARY KEY (id); \
             ALTER TABLE app.t ADD CONSTRAINT allowed CHECK (m <> 'sad'); \
             INSERT INTO app.t (id,m,n) VALUES (1,'ok','ok')",
            if hole { "removed integer, " } else { "" },
            if hole {
                "ALTER TABLE app.t DROP COLUMN removed;"
            } else {
                ""
            },
        ),
    );
    let layout = text_of(
        f.own.connection(),
        "SELECT string_agg(attnum::text || ':' || attname, ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='app.t'::regclass AND attnum>0 AND NOT attisdropped",
    );
    assert_eq!(layout, if hole { "2:m,3:id,4:n" } else { "1:m,2:id,3:n" });
    f.rename("m <> 'blue'", false);
    f.d.table(&layout_schema("m <> 'blue'", true));
    f.d.commit();
    let before = f.identity();
    let plan = f.plan(true, false);
    assert_eq!(
        retained(&plan),
        1,
        "physical layout {layout} must retain the CHECK"
    );
    assert!(!rebuilt(&plan));
    let mut tampered = plan.clone();
    for step in &mut tampered.changes.changes {
        if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut step.change {
            retained[0].after = "n <> 'blue'".into();
        }
    }
    std::fs::write(&f.path, serde_json::to_string_pretty(&tampered).unwrap()).unwrap();
    let out = f.apply(false, false);
    assert_ne!(code(&out), 0, "a different column cannot prove retention");
    assert!(stderr(&out).contains("retained CHECK"), "{}", stderr(&out));
    assert_eq!(f.identity(), before);
    // Scratch must preserve the same distinction, even though it binds both
    // expressions on one carrier rather than reading the real table's tree.
    f.d.table(&layout_schema("n <> 'blue'", true));
    f.d.commit();
    let different = f.plan(true, false);
    assert_eq!(retained(&different), 0);
    assert!(rebuilt(&different));
    f.d.table(&layout_schema("m <> 'blue'", true));
    f.d.commit();
    f.plan(true, false);
    succeeds(f.apply(false, false));
    assert_eq!(f.identity(), before);
    assert_eq!(
        latest_snapshot(f.own.connection())
            .declared
            .expressions
            .checks[&"app.t".parse().unwrap()]["allowed"],
        "m <> 'blue'"
    );
    assert!(f.plan(true, false).changes.is_empty());
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_physical_column_order_keeps_logical_bindings() {
    for server in servers() {
        layout_retention(&server, false);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_dropped_attribute_holes_keep_logical_bindings() {
    for server in servers() {
        layout_retention(&server, true);
    }
}

fn retained_defaults(plan: &pbps_model::SavedPlan) -> usize {
    pbps_model::retained::expressions(&plan.changes)
        .filter(|e| matches!(e.target, pbps_model::ExpressionTarget::ColumnDefault { .. }))
        .count()
}
fn altered_default(plan: &pbps_model::SavedPlan) -> bool {
    plan.changes
        .changes
        .iter()
        .any(|p| matches!(p.change, pbps_model::Change::AlterColumnDefault { .. }))
}
fn default_identity(f: &Fixture) -> String {
    text_of(
        f.own.connection(),
        "SELECT d.oid::text || '/' || d.adbin::text FROM pg_attrdef d JOIN pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.t'::regclass AND a.attname='m'",
    )
}
fn authored_default(f: &Fixture) -> String {
    latest_snapshot(f.own.connection())
        .declared
        .expressions
        .defaults[&"app.t".parse().unwrap()]["m"]
        .clone()
}
fn default_table(expression: &str) -> String {
    table("true", false, false).replace(
        "m: {type: app.mood}",
        &format!("m: {{type: app.mood, default: {expression:?}}}"),
    )
}
fn default_fixture(server: &str, tag: &str, before: &str, after: &str, ty: &str) -> Fixture {
    let f = Fixture::declared(server, tag, &default_table(before), ty, "[sad, ok]");
    f.rename("true", true);
    let desired = default_table(after).replace("type: app.mood", &format!("type: {ty:?}"));
    if std::fs::read_to_string(f.d.dir.join("schema/app.t.yml")).unwrap() != desired {
        f.d.table(&desired);
        f.d.commit();
    }
    f
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_defaults_keep_identity_and_advance_without_real_table_ddl() {
    for server in servers() {
        for (ty, before, after) in [
            ("app.mood", "'sad'", "'blue'"),
            ("app.mood[]", "'{sad}'", "'{blue}'"),
            ("app.dm", "'sad'", "'blue'"),
        ] {
            let f = default_fixture(&server, "retained2354default", before, after, ty);
            let identity = default_identity(&f);
            let p = f.plan(true, false);
            assert_eq!(p.version, pbps_model::plan::CURRENT_VERSION);
            assert_eq!(retained_defaults(&p), 1, "{:?}", p.changes);
            assert!(!altered_default(&p));
            assert!(
                p.changes
                    .changes
                    .iter()
                    .all(|p| matches!(p.change, pbps_model::Change::RenameEnumValue { .. }))
            );
            // Any real-table DDL, including a redundant SET DEFAULT, fails apply.
            on_server(
                f.own.connection(),
                r#"
CREATE FUNCTION public.refuse_real_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() WHERE objid='app.t'::regclass AND command_tag='ALTER TABLE') THEN
    RAISE EXCEPTION 'retention must not alter the real table';
  END IF;
END $$;
CREATE EVENT TRIGGER refuse_real_ddl ON ddl_command_end EXECUTE FUNCTION public.refuse_real_ddl();
"#,
            );
            succeeds(f.apply(false, false));
            assert_eq!(default_identity(&f), identity);
            assert_eq!(authored_default(&f), after);
            on_server(f.own.connection(), "INSERT INTO app.t (id) VALUES (1)");
            assert_eq!(
                text_of(f.own.connection(), "SELECT m::text FROM app.t WHERE id=1"),
                if ty.ends_with("[]") { "{blue}" } else { "blue" }
            );
            assert!(f.plan(true, false).changes.is_empty());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_default_values_and_text_casts_keep_ordinary_alteration() {
    for server in servers() {
        for (before, after) in [
            ("'sad'", "'ok'"),
            ("('sad'::text)::app.mood", "('blue'::text)::app.mood"),
        ] {
            let f = default_fixture(
                &server,
                "retained2354differentdefault",
                before,
                after,
                "app.mood",
            );
            let p = f.plan(true, false);
            assert_eq!(retained_defaults(&p), 0);
            assert!(altered_default(&p));
            succeeds(f.apply(false, false));
            assert_eq!(authored_default(&f), after);
            on_server(f.own.connection(), "INSERT INTO app.t (id) VALUES (1)");
            assert_eq!(
                text_of(f.own.connection(), "SELECT m::text FROM app.t"),
                if after == "'ok'" { "ok" } else { "blue" }
            );
            assert!(f.plan(true, false).changes.is_empty());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_defaults_offline_unavailable_and_unprovable_keep_alteration() {
    for server in servers() {
        let mut f = Fixture::new(&server, "retained2354defaultoffline", "true", true);
        f.rename("true", true);
        let p = f.plan(false, false);
        assert_eq!(retained_defaults(&p), 0);
        assert!(altered_default(&p));
        succeeds(f.d.run(&[
            "plan",
            "--since",
            "HEAD^",
            "--out",
            f.path.to_str().unwrap(),
        ]));
        let offline: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        assert_eq!(retained_defaults(&offline), 0);
        assert!(altered_default(&offline));
        f.scratch.connection =
            "host=127.0.0.1 port=1 user=missing dbname=missing connect_timeout=1 sslmode=disable"
                .into();
        let p = f.plan(true, false);
        assert_eq!(retained_defaults(&p), 0);
        assert!(altered_default(&p));
        drop(f);
        // An unsupported carrier shape is advisory failure, never a proof.
        let extra = |expr| {
            default_table(expr).replace(
                "primary_key:",
                "  g: {type: integer, generated: {expression: 'id + 1', stored: true}}\nprimary_key:",
            )
        };
        let f = Fixture::declared(
            &server,
            "retained2354unprovabledefault",
            &extra("'sad'"),
            "app.mood",
            "[sad, ok]",
        );
        f.rename("true", true);
        f.d.table(&extra("'blue'"));
        f.d.commit();
        let p = f.plan(true, false);
        assert_eq!(retained_defaults(&p), 0);
        assert!(altered_default(&p));
        succeeds(f.apply(false, false));
        assert_eq!(authored_default(&f), "'blue'");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_defaults_ignore_physical_order_and_dropped_attribute_holes() {
    for server in servers() {
        for hole in [false, true] {
            let f = Fixture::new(&server, "retained2354defaultlayout", "true", true);
            on_server(
                f.own.connection(),
                &format!(
                    "DROP TABLE app.t; CREATE TABLE app.t ({}m app.mood DEFAULT 'sad', id integer NOT NULL PRIMARY KEY, CONSTRAINT allowed CHECK (true)); {}",
                    if hole { "removed integer," } else { "" },
                    if hole {
                        "ALTER TABLE app.t DROP COLUMN removed;"
                    } else {
                        ""
                    }
                ),
            );
            let layout = text_of(
                f.own.connection(),
                "SELECT string_agg(attnum::text || ':' || attname, ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='app.t'::regclass AND attnum>0 AND NOT attisdropped",
            );
            assert_eq!(layout, if hole { "2:m,3:id" } else { "1:m,2:id" });
            let identity = default_identity(&f);
            f.rename("true", true);
            let p = f.plan(true, false);
            assert_eq!(retained_defaults(&p), 1);
            assert!(!altered_default(&p));
            succeeds(f.apply(false, false));
            assert_eq!(default_identity(&f), identity);
            assert_eq!(authored_default(&f), "'blue'");
            assert!(f.plan(true, false).changes.is_empty());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_default_drift_and_forged_proofs_never_record_falsely() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354defaultdrift", "true", true);
        f.rename("true", true);
        let original = f.plan(true, false);
        assert_eq!(retained_defaults(&original), 1);
        for identity in [true, false] {
            let mut bad = original.clone();
            for p in &mut bad.changes.changes {
                if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut p.change {
                    if identity {
                        if let pbps_model::ExpressionTarget::ColumnDefault { column, .. } =
                            &mut retained[0].target
                        {
                            column.name = "id".into();
                        }
                    } else {
                        retained[0].after = "'ok'".into();
                    }
                }
            }
            assert_eq!(pbps_model::retained::validate(&bad).is_err(), identity);
            std::fs::write(&f.path, serde_json::to_string_pretty(&bad).unwrap()).unwrap();
            let out = f.apply(false, false);
            assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out).contains("retained DEFAULT"),
                "{}",
                stderr(&out)
            );
            assert_eq!(authored_default(&f), "'sad'");
        }
        std::fs::write(&f.path, serde_json::to_string_pretty(&original).unwrap()).unwrap();
        on_server(
            f.own.connection(),
            "ALTER TABLE app.t ALTER COLUMN m SET DEFAULT 'ok'",
        );
        let out = f.apply(false, false);
        assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        assert_eq!(authored_default(&f), "'sad'");
        assert_eq!(
            text_of(
                f.own.connection(),
                "SELECT enumlabel FROM pg_enum WHERE enumtypid='app.mood'::regtype ORDER BY enumsortorder LIMIT 1"
            ),
            "sad"
        );
        on_server(
            f.own.connection(),
            "ALTER TABLE app.t ALTER COLUMN m SET DEFAULT 'sad'",
        );
        // Replaced catalog identity is observed freshly; no saved OID is authority.
        f.plan(true, false);
        succeeds(f.apply(false, false));
        assert_eq!(authored_default(&f), "'blue'");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_default_relation_lock_blocks_replacement_through_recording() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354defaultlock", "true", true);
        f.rename("true", true);
        let plan = f.plan(true, false);
        assert_eq!(retained_defaults(&plan), 1);
        let state = latest_snapshot(f.own.connection());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            use pbps_dialect::Dialect;
            let mut conn=pbps_db::Conn::connect(pbps_db::Driver::Postgres,f.own.connection()).await.unwrap();
            let pg=pbps_pg::Postgres::new();
            let framing=pg.transaction_framing(conn.session_zone().unwrap());
            conn.begin(&framing).await.unwrap();
            let held=pbps_pg::retained::hold(&mut conn,&plan.changes,Some(&state.declared.overlay(&state.schema))).await.unwrap();
            let mut forged = plan.changes.clone();
            for p in &mut forged.changes {
                if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut p.change {
                    retained[0].after = "'ok'".into();
                }
            }
            let bad = pbps_pg::retained::hold(&mut conn,&forged,Some(&state.declared.overlay(&state.schema))).await.unwrap();
            conn.execute("ALTER TYPE app.mood RENAME VALUE 'sad' TO 'blue'").await.unwrap();
            assert!(bad.verify(&mut conn,&pg,&state.schema).await.is_err(), "fresh DEFAULT equality cannot be bypassed");
            held.verify(&mut conn,&pg,&state.schema).await.unwrap();
            let mut other=pbps_db::Conn::connect(pbps_db::Driver::Postgres,f.own.connection()).await.unwrap();
            other.execute("SET lock_timeout='100ms'; SELECT * FROM app.t; INSERT INTO app.t(id) VALUES(1)").await.unwrap();
            let error=other.execute("ALTER TABLE app.t ALTER COLUMN m SET DEFAULT 'ok'").await.unwrap_err();
            assert_eq!(error.server_error_code().as_deref(),Some("55P03"));
            pbps_pg::state::record(&mut conn,&state).await.unwrap();
            assert!(other.execute("ALTER TABLE app.t ALTER COLUMN m SET DEFAULT 'ok'").await.is_err());
            conn.rollback(&framing).await.unwrap();
        });
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_default_staged_closure_and_resume_verify_before_recording() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354defaultstaged", "true", true);
        f.rename("true", true);
        let original = f.plan(true, true);
        assert_eq!(retained_defaults(&original), 1);
        let mut bad = original.clone();
        for p in &mut bad.changes.changes {
            if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut p.change {
                retained[0].after = "'ok'".into();
            }
        }
        std::fs::write(&f.path, serde_json::to_string_pretty(&bad).unwrap()).unwrap();
        let out = f.apply(true, false);
        assert_ne!(code(&out), 0);
        assert_eq!(
            latest_snapshot(f.own.connection())
                .declared
                .expressions
                .defaults[&"app.t".parse().unwrap()]["m"],
            "'sad'"
        );
        std::fs::write(&f.path, serde_json::to_string_pretty(&original).unwrap()).unwrap();
        // Sequence increments survive the carrier savepoint: stop only the closing proof.
        on_server(
            f.own.connection(),
            r#"
CREATE SCHEMA witness;
CREATE SEQUENCE witness.proofs;
CREATE FUNCTION witness.stop_closing() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_query() LIKE '%CREATE TEMP TABLE pg_temp.pbps_retained_%'
     AND nextval('witness.proofs') > 1 THEN
    RAISE EXCEPTION 'stop the closing proof';
  END IF;
END $$;
CREATE EVENT TRIGGER stop_closing ON ddl_command_start WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION witness.stop_closing();
"#,
        );
        let stopped = f.apply(true, false);
        assert_ne!(
            code(&stopped),
            0,
            "{}{}",
            stdout(&stopped),
            stderr(&stopped)
        );
        let checkpoint = latest_snapshot(f.own.connection());
        assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 1);
        assert_eq!(
            checkpoint.declared.expressions.defaults[&"app.t".parse().unwrap()]["m"],
            "'sad'"
        );
        on_server(f.own.connection(), "DROP EVENT TRIGGER stop_closing");
        succeeds(f.apply(true, true));
        let state = latest_snapshot(f.own.connection());
        assert!(state.staged.is_none());
        assert_eq!(
            state.declared.expressions.defaults[&"app.t".parse().unwrap()]["m"],
            "'blue'"
        );
        assert!(f.plan(true, false).changes.is_empty());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_default_binding_rejects_trailing_statements_without_execution() {
    for server in servers() {
        let f = Fixture::new(&server, "retained2354defaultsingle", "true", true);
        f.rename("true", true);
        let plan = f.plan(true, false);
        let e = pbps_model::retained::expressions(&plan.changes)
            .find(|e| matches!(e.target, pbps_model::ExpressionTarget::ColumnDefault { .. }))
            .unwrap();
        let state = latest_snapshot(f.own.connection());
        on_server(
            f.own.connection(),
            "CREATE SEQUENCE public.default_bind_attempts",
        );
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            use pbps_dialect::Dialect;
            let mut conn=pbps_db::Conn::connect(pbps_db::Driver::Postgres,f.own.connection()).await.unwrap();
            let pg=pbps_pg::Postgres::new();
            let framing=pg.transaction_framing(conn.session_zone().unwrap());
            conn.begin(&framing).await.unwrap();
            let table=e.target.table();
            let carrier=pbps_pg::retained::create_carrier(&mut conn,&pg,table,&state.schema.tables[table],0,true).await.unwrap();
            pbps_pg::retained::bind(&mut conn,&pg,&e.target,"'sad'::app.mood /* ; stays one statement */",&carrier).await.unwrap();
            conn.execute("SAVEPOINT malicious_default").await.unwrap();
            let result=pbps_pg::retained::bind(&mut conn,&pg,&e.target,"'sad'); SELECT pg_catalog.nextval('public.default_bind_attempts'::pg_catalog.regclass); --",&carrier).await;
            assert!(result.is_err(), "DEFAULT binding must parse exactly one statement");
            conn.execute("ROLLBACK TO malicious_default; RELEASE malicious_default").await.unwrap();
            // Sequence calls survive rollback, so partial execution cannot hide.
            let rows=conn.query("SELECT NOT is_called AS absent FROM public.default_bind_attempts").await.unwrap();
            assert_eq!(rows[0].try_get::<bool>("absent").unwrap(),Some(true));
            conn.rollback(&framing).await.unwrap();
        });
        assert_eq!(authored_default(&f), "'sad'");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_statement_tails_never_execute_at_scratch() {
    for server in servers() {
        for transaction_tail in [false, true] {
            let f = Fixture::new(&server, "retained2354statementtail", "m <> 'sad'", false);
            let tail = format!(
                "m <> 'blue'); {} CREATE TABLE public.retained2354_marker(id integer); --",
                if transaction_tail { "COMMIT;" } else { "" }
            );
            f.rename(&tail, false);
            let out = f.d.run_with_env(
                &[
                    "plan",
                    "--db",
                    f.own.connection(),
                    "--resolve-with",
                    "scratch",
                    "--no-rehearse",
                    "--out",
                    f.path.to_str().unwrap(),
                ],
                &[("PBPS_RETAINED2354_SCRATCH", &f.scratch.connection)],
            );
            if code(&out) == 0 {
                let p: pbps_model::SavedPlan =
                    serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
                assert_eq!(retained(&p), 0, "a statement tail is never retention proof");
                assert!(rebuilt(&p));
            } else {
                assert!(
                    format!("{}{}", stdout(&out), stderr(&out)).contains("app.t"),
                    "{}{}",
                    stdout(&out),
                    stderr(&out)
                );
            }
            for db in [f.own.connection(), f.scratch.database.connection()] {
                assert_eq!(
                    scalar(
                        db,
                        "SELECT (pg_catalog.to_regclass('public.retained2354_marker') IS NOT NULL)::integer::bigint"
                    ),
                    0
                );
            }
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_statement_tails_never_execute_at_apply() {
    for server in servers() {
        for transaction_tail in [false, true] {
            let f = Fixture::new(&server, "retained2354applytail", "m <> 'sad'", false);
            f.rename("m <> 'blue'", false);
            let tail = format!(
                "m <> 'blue'); {} CREATE TABLE public.retained2354_marker(id integer); --",
                if transaction_tail { "COMMIT;" } else { "" }
            );
            // Saved proof is not authority to execute a tail during fresh binding.
            let mut p = f.plan(true, false);
            assert_eq!(retained(&p), 1);
            for step in &mut p.changes.changes {
                if let pbps_model::Change::RenameEnumValue { retained, .. } = &mut step.change {
                    retained[0].after = tail.clone();
                }
            }
            let identity = f.identity();
            std::fs::write(&f.path, serde_json::to_string_pretty(&p).unwrap()).unwrap();
            let out = f.apply(false, false);
            assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out).contains("retained CHECK app.t.allowed"),
                "{}",
                stderr(&out)
            );
            assert_eq!(
                scalar(
                    f.own.connection(),
                    "SELECT (pg_catalog.to_regclass('public.retained2354_marker') IS NOT NULL)::integer::bigint"
                ),
                0
            );
            assert_eq!(f.identity(), identity);
            assert_eq!(
                text_of(
                    f.own.connection(),
                    "SELECT enumlabel FROM pg_enum WHERE enumtypid='app.mood'::regtype ORDER BY enumsortorder LIMIT 1"
                ),
                "sad"
            );
            assert_eq!(
                latest_snapshot(f.own.connection())
                    .declared
                    .expressions
                    .checks[&"app.t".parse().unwrap()]["allowed"],
                "m <> 'sad'"
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn retained2354_literals_and_complete_comment_tails_keep_one_statement() {
    for server in servers() {
        for (before, after) in [
            ("m <> 'sad'", "m <> 'blue'"),
            (
                "m <> 'sad' AND '; COMMIT; --' = '; COMMIT; --'",
                "m <> 'blue' AND '; COMMIT; --' = '; COMMIT; --'",
            ),
            (
                "m <> 'sad' /* harmless ; COMMIT; */",
                "m <> 'blue' /* harmless ; COMMIT; */",
            ),
            ("m <> 'sad' -- tail\n", "m <> 'blue' -- tail\n"),
        ] {
            let f = Fixture::new(&server, "retained2354statementtext", before, false);
            f.rename(after, false);
            let identity = f.identity();
            let p = f.plan(true, false);
            assert_eq!(retained(&p), 1, "{after}: {:?}", p.changes);
            assert!(!rebuilt(&p));
            succeeds(f.apply(false, false));
            assert_eq!(f.identity(), identity);
            assert_eq!(
                latest_snapshot(f.own.connection())
                    .declared
                    .expressions
                    .checks[&"app.t".parse().unwrap()]["allowed"],
                after
            );
            assert!(f.plan(true, false).changes.is_empty());
        }
        // No newline: this comment consumes the wrapper's closing syntax.
        let f = Fixture::new(
            &server,
            "retained2354incompletecomment",
            "m <> 'sad'",
            false,
        );
        f.rename("m <> 'blue' -- tail", false);
        let p = f.plan(true, false);
        assert_eq!(retained(&p), 0);
        assert!(rebuilt(&p));
    }
}
