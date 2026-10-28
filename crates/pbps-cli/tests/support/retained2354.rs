//! CHECK-only retention; defaults deliberately keep their ordinary ALTER.
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
        d.table(&table(before, default, false).replace("type: app.mood", &format!("type: {ty:?}")));
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
    pbps_model::retained::expressions(&plan.changes).count()
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
        assert!(
            plan.changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::AlterColumnDefault { .. }))
        );
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
