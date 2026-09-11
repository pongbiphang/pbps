//! The end-to-end flow on PostgreSQL, through the binary.
//!
//! `tests/flow.rs` is the SQL Server suite; this file is the same shape on the
//! other engine, for the path a user actually types: `bootstrap`, `verify`,
//! `plan --db`, `apply`, `state list`, `doctor`. Each connected command reaches
//! the engine through `engine.rs`, and only a run against a real server can
//! say that every arm it routes to answers — the dialect's own live suite
//! (`pbps-pg/tests/live.rs`) proves each function, not that the CLI reaches
//! it (issue #85, DECISIONS 417).
//!
//! Run with `scripts/live-tests-pg.sh`, or by hand:
//! `PBPS_TEST_PG_DB=... cargo test -p pbps-cli --test flow_pg -- --ignored`.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

fn server() -> String {
    std::env::var("PBPS_TEST_PG_DB")
        .expect("PBPS_TEST_PG_DB is not set; see scripts/live-tests-pg.sh")
}

/// Runs one statement against the server under test, through the same
/// connection type the tool uses. Panics on failure: a test that cannot
/// arrange its state has not passed (DECISIONS 198).
fn on_server(connection: &str, sql: &str) {
    if let Err(e) = try_on_server(connection, sql) {
        panic!("{e}");
    }
}

fn try_on_server(connection: &str, sql: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("no runtime: {e}"))?;
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .map_err(|e| format!("cannot reach the server under test: {e}"))?;
        conn.execute(sql)
            .await
            .map_err(|e| format!("`{sql}` failed: {e}"))?;
        Ok(())
    })
}

/// A database of this test's own, dropped when the guard goes out of scope,
/// for the reason `tests/flow.rs` gives: the ledger is per database, and two
/// tests writing one ledger fail each other in an order that depends on the
/// scheduler.
struct OwnDatabase {
    server: String,
    name: String,
    connection: String,
}

impl OwnDatabase {
    fn new(server: &str, slug: &str) -> Self {
        let name = format!("pbps_cli_{slug}_{}", std::process::id());
        on_server(
            server,
            &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
        );
        on_server(server, &format!("CREATE DATABASE \"{name}\""));
        Self {
            server: server.to_owned(),
            connection: with_dbname(server, &name),
            name,
        }
    }

    fn connection(&self) -> &str {
        &self.connection
    }
}

impl Drop for OwnDatabase {
    fn drop(&mut self) {
        // Tolerant: a panic while unwinding aborts the process and takes the
        // rest of the suite with it. `FORCE` because a CLI process that
        // panicked mid-command may still hold its session for a moment.
        let _ = try_on_server(
            &self.server,
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// The libpq keyword form `PBPS_TEST_PG_DB` is written in, with its `dbname`
/// replaced: `key=value` words separated by spaces, and a `dbname` that is
/// already there has to go, or the driver takes whichever it reads last.
fn with_dbname(connection: &str, name: &str) -> String {
    let kept: Vec<&str> = connection
        .split_whitespace()
        .filter(|word| !word.starts_with("dbname="))
        .collect();
    format!("{} dbname={name}", kept.join(" "))
}

struct Demo {
    dir: PathBuf,
}

impl Demo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-flow-pg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(dir.join("pbps.yml"), "dialect: postgres\n").unwrap();
        let d = Self { dir };
        d.git(&["init", "-q"]);
        d.git(&["config", "user.email", "d@e.f"]);
        d.git(&["config", "user.name", "demo"]);
        d
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "wip"]);
    }

    fn table(&self, body: &str) {
        std::fs::write(self.dir.join("schema/app.t.yml"), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(BIN)
            .arg("--project")
            .arg(&self.dir)
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn plan_checksum(path: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap();
    serde_json::from_str::<pbps_model::SavedPlan>(&raw)
        .unwrap()
        .checksum()
}

const ONE_COLUMN: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n\
                          primary_key: {name: pk_t, columns: [id]}\n";
const TWO_COLUMNS: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  \
                           label: {type: varchar(50)}\n\
                           primary_key: {name: pk_t, columns: [id]}\n";

fn succeeds(o: Output) -> Output {
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    o
}

fn apply_plan(d: &Demo, connection: &str, plan: &std::path::Path, staged: bool) -> Output {
    let checksum = plan_checksum(plan);
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    if staged {
        args.push("--staged");
    }
    d.run(&args)
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn module_rebuilds_refuse_carried_state_before_planning_and_before_recording() {
    let server = server();
    let own = OwnDatabase::new(&server, "module-rebuild");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("module-rebuild");
    d.table(ONE_COLUMN);
    let view = d.dir.join("schema/v.yml");
    std::fs::write(&view, "view: app.v\ndefinition: SELECT id FROM app.t\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    std::fs::write(
        &view,
        "view: app.v\ndefinition: SELECT id FROM app.t WHERE id > 0\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];

    on_server(connection, "ALTER VIEW app.v SET (security_invoker = true)");
    let o = d.run(&planning);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("security_invoker"), "{}", stderr(&o));
    assert!(!plan.exists());
    on_server(connection, "ALTER VIEW app.v RESET (security_invoker)");
    succeeds(d.run(&planning));

    let staged = d.run(&["plan", "--db", connection, "--staged"]);
    assert_eq!(code(&staged), 1, "{}{}", stdout(&staged), stderr(&staged));
    assert!(stderr(&staged).contains("module rebuilds require a transaction"));

    // This attribute is outside the baseline checksum. Apply must ask again.
    on_server(
        connection,
        "COMMENT ON VIEW app.v IS 'keep this operator note'",
    );
    let o = apply_plan(&d, connection, &plan, false);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("keep this operator note"),
        "{}",
        stderr(&o)
    );
    on_server(
        connection,
        "DO $$ BEGIN IF obj_description('app.v'::regclass) IS DISTINCT FROM 'keep this operator note' THEN RAISE EXCEPTION 'note was lost'; END IF; END $$",
    );
    on_server(connection, "COMMENT ON VIEW app.v IS NULL");

    // The replacement itself can acquire state that the precheck never saw.
    on_server(
        connection,
        "CREATE FUNCTION public.note_rebuilt_view() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN COMMENT ON VIEW app.v IS 'injected after create'; END $$",
    );
    on_server(
        connection,
        "CREATE EVENT TRIGGER note_rebuilt_view ON ddl_command_end WHEN TAG IN ('CREATE VIEW') EXECUTE FUNCTION public.note_rebuilt_view()",
    );
    let o = apply_plan(&d, connection, &plan, false);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("injected after create"),
        "{}",
        stderr(&o)
    );
    on_server(
        connection,
        "DO $$ BEGIN IF obj_description('app.v'::regclass) IS NOT NULL OR pg_get_viewdef('app.v'::regclass) LIKE '%WHERE%' THEN RAISE EXCEPTION 'failed rebuild committed'; END IF; END $$",
    );
    on_server(connection, "DROP EVENT TRIGGER note_rebuilt_view");
    on_server(connection, "DROP FUNCTION public.note_rebuilt_view()");
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn routine_rebuilds_do_not_restore_revoked_public_execute() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-rebuild");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("routine-rebuild");
    let file = d.dir.join("schema/secret.yml");
    let declaration = |value| {
        format!(
            "function: app.secret()\ndefinition: () RETURNS integer LANGUAGE sql SECURITY DEFINER AS $$ SELECT {value} $$\n"
        )
    };
    std::fs::write(&file, declaration(1)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    std::fs::write(&file, declaration(2)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    succeeds(d.run(&planning));
    on_server(
        connection,
        "REVOKE EXECUTE ON FUNCTION app.secret() FROM PUBLIC",
    );
    for o in [d.run(&planning), apply_plan(&d, connection, &plan, false)] {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(stderr(&o).contains("PUBLIC"), "{}", stderr(&o));
    }
    on_server(
        connection,
        "DO $$ BEGIN IF app.secret() <> 1 OR EXISTS (SELECT 1 FROM pg_proc p, LATERAL aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a WHERE p.oid = 'app.secret()'::regprocedure AND a.grantee = 0 AND a.privilege_type = 'EXECUTE') THEN RAISE EXCEPTION 'routine rebuild restored public access'; END IF; END $$",
    );
    // An explicit ACL remains carried state even if its entries resemble the
    // default. Recreate the original default-only fixture for the safe case.
    on_server(connection, "DROP FUNCTION app.secret()");
    on_server(
        connection,
        "CREATE FUNCTION app.secret() RETURNS integer LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
    );
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn grantless_cluster_role_additions_and_removals_update_the_recorded_scope() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_grantless_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "grantless-role");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("grantless-role");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let file = d.dir.join("schema/reader.yml");
    std::fs::write(&file, format!("role: {}\n", role.1)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    let missing = d.run(&planning);
    assert_eq!(
        code(&missing),
        1,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
    assert!(
        stderr(&missing).contains("is missing"),
        "{}",
        stderr(&missing)
    );
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    succeeds(d.run(&planning));
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    // The incoming role's grants are pinned even before its UID is recorded.
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA app TO {}", role.1),
    );
    let changed = apply_plan(&d, connection, &plan, false);
    assert_eq!(
        code(&changed),
        1,
        "{}{}",
        stdout(&changed),
        stderr(&changed)
    );
    assert!(
        stderr(&changed).contains("no longer the database"),
        "{}",
        stderr(&changed)
    );
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.1),
    );
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA app TO {}", role.1),
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2, "{}{}", stdout(&drift), stderr(&drift));
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.1),
    );
    on_server(&server, &format!("DROP ROLE {}", role.1));
    let missing = d.run(&["verify", "--db", connection]);
    assert_eq!(
        code(&missing),
        2,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    succeeds(d.run(&["verify", "--db", connection]));

    std::fs::remove_file(file).unwrap();
    succeeds(d.run(&[
        "drop-role",
        &role.1,
        "--reason",
        "stop managing this cluster role",
    ]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&planning));
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    assert!(saved.ids.roles.is_empty());
    succeeds(apply_plan(&d, connection, &plan, false));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let last = pbps_pg::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(last.snapshot.kind, pbps_model::StateKind::Apply);
        assert!(last.snapshot.ids.roles.is_empty());
        assert!(last.snapshot.schema.roles.is_empty());
    });
    on_server(&server, &format!("DROP ROLE {}", role.1));
    succeeds(d.run(&["verify", "--db", connection]));

    // A nonempty staged plan must adopt an incoming grantless role in its
    // first checkpoint too, not only at the final identity recording.
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {}\n", role.1),
    )
    .unwrap();
    d.table(TWO_COLUMNS);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]));
    succeeds(apply_plan(&d, connection, &plan, true));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(&server, &format!("DROP ROLE {}", role.1));
    let missing = d.run(&["verify", "--db", connection]);
    assert_eq!(
        code(&missing),
        2,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn completed_cluster_role_renames_are_pinned_and_recorded_without_role_sql() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for name in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {name}"));
            }
        }
    }
    let server = server();
    let old = format!("pbps_old_{}", std::process::id());
    let new = format!("pbps_new_{}", std::process::id());
    let parked = format!("pbps_parked_{}", std::process::id());
    let _roles = Roles(
        server.clone(),
        vec![old.clone(), new.clone(), parked.clone()],
    );
    on_server(&server, &format!("CREATE ROLE {old}"));
    let own = OwnDatabase::new(&server, "role-rename");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("role-rename");
    d.table(ONE_COLUMN);
    let role_file = d.dir.join("schema/reader.yml");
    let declaration =
        |name: &str| format!("role: {name}\ngrants:\n  app.t: [select]\n  schema::app: [usage]\n");
    std::fs::write(&role_file, declaration(&old)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    std::fs::write(&role_file, declaration(&new)).unwrap();
    succeeds(d.run(&["rename-role", &old, &new]));
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    let refused = |o: Output| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(stderr(&o).contains("ALTER ROLE"), "{}", stderr(&o));
    };
    refused(d.run(&planning)); // NotRunYet
    on_server(&server, &format!("CREATE ROLE {new}"));
    refused(d.run(&planning)); // BothPresent
    on_server(&server, &format!("DROP ROLE {new}"));
    on_server(&server, &format!("ALTER ROLE {old} RENAME TO {parked}"));
    refused(d.run(&planning)); // NeitherPresent
    on_server(&server, &format!("ALTER ROLE {parked} RENAME TO {new}"));
    succeeds(d.run(&planning)); // Done; no SQL is needed to move the grants.
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    on_server(&server, &format!("CREATE ROLE {old}"));
    refused(apply_plan(&d, connection, &plan, false));
    on_server(&server, &format!("DROP ROLE {old}"));
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
    let listing = succeeds(d.run(&["state", "list", "--db", connection, "--format", "json"]));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout(&listing)).unwrap()["data"]["entries"][0]
            ["kind"],
        "apply"
    );

    // Done also covers drop-and-create: plan its missing grants (377), while
    // unrelated drift still refuses. Exercise the staged baseline as well.
    std::fs::write(&role_file, declaration(&old)).unwrap();
    succeeds(d.run(&["rename-role", &new, &old]));
    succeeds(d.run(&["plan"]));
    d.commit();
    on_server(connection, &format!("REVOKE SELECT ON app.t FROM {new}"));
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {new}"),
    );
    on_server(&server, &format!("DROP ROLE {new}"));
    on_server(&server, &format!("CREATE ROLE {old}"));
    on_server(connection, "ALTER TABLE app.t ADD COLUMN stray integer");
    let o = d.run(&planning);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("drifted"), "{}", stderr(&o));
    on_server(connection, "ALTER TABLE app.t DROP COLUMN stray");
    on_server(connection, &format!("GRANT USAGE ON SCHEMA app TO {old}"));
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]));
    succeeds(apply_plan(&d, connection, &plan, true));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        &format!(
            "DO $$ BEGIN IF NOT has_table_privilege('{old}', 'app.t', 'SELECT') THEN RAISE EXCEPTION 'grant was not restored'; END IF; END $$"
        ),
    );
}

/// An unavailable rehearsal is not evidence that the PostgreSQL plan is
/// invalid, and must be refused before SQL Server setup sees either backend.
#[test]
fn postgres_rehearsals_refuse_before_connecting_or_starting_a_container() {
    let d = Demo::new("unsupported-dev");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    d.commit();

    for backend in ["not-a-connection", "docker://not-an-image"] {
        let o = d.run(&["plan", "--dev", backend, "--format", "json"]);
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
        assert_eq!(v["findings"][0]["id"], "rehearsal.unavailable", "{v}");
        assert!(
            stdout(&o).contains("dev rehearsal is not supported for dialect `postgres`"),
            "{v}"
        );
    }

    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\ndev: {url_env: PBPS_UNSUPPORTED_DEV_TEST}\n",
    )
    .unwrap();
    let o = d.run_with_env(
        &["plan"],
        &[("PBPS_UNSUPPORTED_DEV_TEST", "not-a-connection")],
    );
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("dev rehearsal is not supported for dialect `postgres`"),
        "{}",
        stderr(&o)
    );

    // The configured backend must not prevent the existing read-only check.
    let o = d.run(&["plan", "--check"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// The principal belongs to the cluster; bootstrap creates its managed grants
/// in this database, not the role itself (DECISIONS 211).
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn bootstrap_grants_to_an_existing_cluster_role_without_adopting_existing_grants() {
    struct Role {
        server: String,
        name: String,
    }
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.server, &format!("DROP ROLE {}", self.name));
        }
    }

    let server = server();
    let role = Role {
        server: server.clone(),
        name: format!("pbps_cli_bootstrap_reader_{}", std::process::id()),
    };
    on_server(&server, &format!("CREATE ROLE {}", role.name));
    // Declared after the role so the database (and its grants) drops first.
    let own = OwnDatabase::new(&server, "role-bootstrap");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");

    let d = Demo::new("role-bootstrap");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!(
            "role: {}\ngrants:\n  app.t: [select]\n  schema::app: [usage]\n",
            role.name
        ),
    )
    .unwrap();
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    d.commit();

    // WITH GRANT OPTION is outside role.grants, but must still refuse before
    // the build and must not produce any ledger snapshot.
    on_server(
        connection,
        &format!(
            "GRANT USAGE ON SCHEMA app TO {} WITH GRANT OPTION",
            role.name
        ),
    );
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("WITH GRANT OPTION"), "{}", stderr(&o));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR EXISTS (SELECT 1 FROM public.__pbps_state) THEN RAISE EXCEPTION 'refused bootstrap wrote state'; END IF; END $$",
    );
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.name),
    );

    // Grants already in the managed set still make bootstrap refuse.
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA app TO {}", role.name),
    );
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("already has"), "{}", stderr(&o));
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.name),
    );

    // A single deployer's CREATE TABLE can trigger an unsupported grant.
    // The read-back must refuse and roll the whole build back, too.
    on_server(
        connection,
        &format!(
            "CREATE FUNCTION public.inject_grant() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF to_regclass('app.t') IS NOT NULL THEN \
         GRANT SELECT ON app.t TO {} WITH GRANT OPTION; END IF; END $$; \
         CREATE EVENT TRIGGER inject_grant ON ddl_command_end WHEN TAG IN ('CREATE TABLE') \
         EXECUTE FUNCTION public.inject_grant()",
            role.name
        ),
    );
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("WITH GRANT OPTION"), "{}", stderr(&o));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL THEN RAISE EXCEPTION 'refused bootstrap committed its table'; END IF; END $$",
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let latest = pbps_pg::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Failed);
        assert!(latest.snapshot.schema.tables.is_empty());
        assert!(latest.snapshot.schema.roles.is_empty());
    });
    on_server(
        connection,
        "DROP EVENT TRIGGER inject_grant; DROP FUNCTION public.inject_grant()",
    );

    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    on_server(
        connection,
        &format!(
            "DO $$ BEGIN IF NOT has_table_privilege('{}', 'app.t', 'SELECT') OR \
         NOT has_schema_privilege('{}', 'app', 'USAGE') THEN \
         RAISE EXCEPTION 'bootstrap did not apply the declared grants'; END IF; END $$",
            role.name, role.name
        ),
    );
    let o = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // The existing table must remain protected by the ordinary empty check.
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("already has"), "{}", stderr(&o));
}

/// The deployment loop, end to end, on PostgreSQL: an empty database is
/// bootstrapped, verified clean, planned against, applied to, verified clean
/// again, and its ledger listed. Every connected command the loop touches
/// routes through the seam, so a PostgreSQL arm that refused, or answered a
/// SQL Server shape, fails here by exit code.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn the_deployment_loop_runs_end_to_end_on_postgres() {
    let server = server();
    let own = OwnDatabase::new(&server, "loop");
    let connection = own.connection().to_owned();
    // The tool never creates a schema (`doctor` says so); the environment has
    // to bring it, exactly as on the other engine.
    on_server(&connection, "CREATE SCHEMA app");

    let d = Demo::new("loop");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    d.commit();

    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("No drift"), "{}", stdout(&o));

    // One revision, planned against the database and applied through the
    // gate: a column added, and the plan's `--out` artifact is what `apply`
    // takes, checksum-pinned.
    d.table(TWO_COLUMNS);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("label"), "{}", stdout(&o));
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("No changes"), "{}", stdout(&o));

    // The ledger, read back: the bootstrap and the apply, newest first.
    let o = d.run(&["state", "list", "--db", &connection, "--format", "json"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    let entries = v["data"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("{}", stdout(&o)));
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(entries[0]["kind"], "apply", "{v}");
    assert_eq!(entries[1]["kind"], "bootstrap", "{v}");
}

/// Drift is drift on this engine too: a column added by hand is reported,
/// with the exit code a scheduled drift-watch keys on (DECISIONS 25), and a
/// `pull` reads the same database back into declarations that validate.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn drift_and_pull_are_read_from_the_postgres_catalog() {
    let server = server();
    let own = OwnDatabase::new(&server, "drift");
    let connection = own.connection().to_owned();
    on_server(&connection, "CREATE SCHEMA app");

    let d = Demo::new("drift");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    on_server(&connection, "ALTER TABLE app.t ADD COLUMN stray integer");
    let o = d.run(&["verify", "--db", &connection, "--format", "json"]);
    assert_eq!(code(&o), 2, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["result"], "findings", "{v}");
    assert!(stdout(&o).contains("stray"), "{}", stdout(&o));

    // `pull --force` overwrites the declarations with what the catalog holds,
    // and what it writes must validate under the same dialect.
    let o = d.run(&["pull", "--db", &connection, "--force"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let pulled = std::fs::read_to_string(d.dir.join("schema/app.t.yml")).unwrap();
    assert!(pulled.contains("stray"), "{pulled}");
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// `doctor` examines a PostgreSQL environment through the seam: the version
/// is read, the engine has one edition so none is reported, the ledger is
/// found, and a schema the declarations use but the database lacks is the
/// error it is on the other engine — with the remedy quoted this engine's
/// way.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn doctor_examines_a_postgres_environment() {
    let server = server();
    let own = OwnDatabase::new(&server, "doctor");
    let connection = own.connection().to_owned();

    let d = Demo::new("doctor");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nenvironments:\n  dev:\n    url_env: PBPS_FLOW_PG_DEV\n",
    )
    .unwrap();
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let o = d.run_with_env(
        &["doctor", "--format", "json"],
        &[("PBPS_FLOW_PG_DEV", connection.as_str())],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    let env = &v["data"]["environments"][0];
    assert_eq!(env["state"], "uninitialized", "{v}");
    assert!(
        env["server_version"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "{v}"
    );
    assert!(env["edition"].is_null(), "one edition, no line: {v}");
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(ids.contains(&"schema.absent"), "{v}");
    let absent = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "schema.absent")
        .unwrap();
    assert_eq!(absent["remedy"], "CREATE SCHEMA \"app\";", "{v}");
    assert_eq!(code(&o), 2, "{}{}", stdout(&o), stderr(&o));
}
