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

fn json_output(o: Output) -> serde_json::Value {
    let text = stdout(&o);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}\n{}", stderr(&o)))
}

fn scalar(connection: &str, sql: &str) -> i64 {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            c.query(sql).await.unwrap()[0]
                .try_get_at::<i64>(0)
                .unwrap()
                .unwrap()
        })
}

fn latest_snapshot(connection: &str) -> pbps_model::StateSnapshot {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            pbps_pg::state::latest(&mut c)
                .await
                .unwrap()
                .unwrap()
                .snapshot
        })
}

fn connected_artifact(d: &Demo, connection: &str, staged: bool) -> PathBuf {
    succeeds(d.run(&["plan"]));
    d.commit();
    let path = d.dir.join("invariant-plan.json");
    let mut args = vec!["plan", "--db", connection, "--out", path.to_str().unwrap()];
    if staged {
        args.push("--staged");
    }
    succeeds(d.run(&args));
    path
}

fn approved_apply(d: &Demo, connection: &str, plan: &std::path::Path, extra: &[&str]) -> Output {
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
    args.extend_from_slice(extra);
    d.run(&args)
}

fn bootstrapped_demo(connection: &str, slug: &str, table: &str) -> Demo {
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    d.table(table);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn connected_cost_distinguishes_rewrites_scans_unknowns_and_risk() {
    let own = OwnDatabase::new(&server(), "cost");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("cost");
    let before = "table: app.t\ncolumns:\n  i: {type: integer}\n  w: {type: varchar(10)}\n  j: {type: integer}\n  n: {type: integer}\nindexes:\n  ix_j: {columns: [j]}\nchecks:\n  ck_n: n > 0\n";
    d.table(before);
    let other = d.dir.join("schema/u.yml");
    std::fs::write(&other, "table: app.u\ncolumns:\n  v: {type: integer}\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "INSERT INTO app.t SELECT v, 'ok', v, v FROM generate_series(1,3) AS v; ANALYZE app.t",
    );
    d.table(
        &before
            .replace("i: {type: integer}", "i: {type: bigint}")
            .replace("varchar(10)", "varchar(20)")
            .replace("j: {type: integer}", "j: {type: bigint}")
            .replace("n: {type: integer}", "n: {type: integer, nullable: false}"),
    );
    std::fs::write(&other, "table: app.u\ncolumns:\n  v: {type: bigint}\n").unwrap();
    std::fs::write(
        d.dir.join("schema/new.yml"),
        "table: app.new_table\ncolumns:\n  id: {type: integer}\n",
    )
    .unwrap();
    let offline = succeeds(d.run(&["plan", "--format", "json"]));
    let offline: serde_json::Value = serde_json::from_str(&stdout(&offline)).unwrap();
    assert!(offline["data"].get("cost").is_none());
    d.commit();
    let artifact = d.dir.join("deployment.json");
    let out = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        artifact.to_str().unwrap(),
    ]));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/envelope.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    jsonschema::validator_for(&schema)
        .unwrap()
        .validate(&report)
        .unwrap();
    let cost = &report["data"]["cost"];
    assert_eq!(cost["engine"], "postgres");
    assert_eq!(cost["status"], "available");
    let changes = cost["changes"].as_array().unwrap();
    let saved_text = std::fs::read_to_string(&artifact).unwrap();
    let saved: pbps_model::SavedPlan = serde_json::from_str(&saved_text).unwrap();
    let saved_json: serde_json::Value = serde_json::from_str(&saved_text).unwrap();
    assert!(saved_json.get("cost").is_none());
    assert_eq!(changes.len(), saved.changes.changes.len());
    let mut checked = 0;
    for (index, p) in saved.changes.changes.iter().enumerate() {
        let c = &changes[index];
        assert_eq!(c["change_index"], index);
        match &p.change {
            pbps_model::Change::CreateTable { .. } => {
                assert_eq!(c["status"], "unavailable");
                assert!(c["reason"].as_str().unwrap().contains("no measurement"));
                checked += 1;
            }
            pbps_model::Change::AlterColumnType { column, .. } => {
                assert_eq!(c["lock"], "AccessExclusiveLock");
                assert_eq!(c["blocks"], "reads and writes");
                match column.name.as_str() {
                    "i" => {
                        assert_eq!(c["rewrite"]["value"], "yes");
                        assert_eq!(c["reads"]["value"], "every_row");
                    }
                    "w" => {
                        assert_eq!(c["rewrite"]["value"], "no");
                        assert_eq!(c["reads"]["value"], "nothing");
                    }
                    "j" => {
                        assert_eq!(c["rewrite"]["value"], "unknown");
                        assert!(c["rewrite"]["reason"].as_str().unwrap().contains("index"));
                    }
                    "v" => assert_eq!(c["rows"]["status"], "never_analyzed"),
                    name => panic!("unexpected column {name}"),
                }
                if column.name != "v" {
                    assert_eq!(c["rows"]["status"], "estimated");
                    assert_eq!(c["rows"]["count"], 3);
                }
                checked += 1;
            }
            pbps_model::Change::AlterColumnNullability { .. } => {
                assert_eq!(c["rewrite"]["value"], "no");
                assert_eq!(c["reads"]["value"], "unknown");
                assert!(c["reads"]["reason"].as_str().unwrap().contains("ck_n"));
                checked += 1;
            }
            change @ pbps_model::Change::DropTable { .. }
            | change @ pbps_model::Change::RenameTable { .. }
            | change @ pbps_model::Change::AddColumn { .. }
            | change @ pbps_model::Change::DropColumn { .. }
            | change @ pbps_model::Change::RenameColumn { .. }
            | change @ pbps_model::Change::AlterColumnDefault { .. }
            | change @ pbps_model::Change::SetColumnDeprecated { .. }
            | change @ pbps_model::Change::SetPrimaryKey { .. }
            | change @ pbps_model::Change::AddUnique { .. }
            | change @ pbps_model::Change::DropUnique { .. }
            | change @ pbps_model::Change::AddForeignKey { .. }
            | change @ pbps_model::Change::DropForeignKey { .. }
            | change @ pbps_model::Change::AddCheck { .. }
            | change @ pbps_model::Change::DropCheck { .. }
            | change @ pbps_model::Change::AddIndex { .. }
            | change @ pbps_model::Change::DropIndex { .. }
            | change @ pbps_model::Change::InsertRow { .. }
            | change @ pbps_model::Change::UpdateRow { .. }
            | change @ pbps_model::Change::DeleteRow { .. }
            | change @ pbps_model::Change::SetDataMode { .. }
            | change @ pbps_model::Change::CreateModule { .. }
            | change @ pbps_model::Change::AlterModule { .. }
            | change @ pbps_model::Change::DropModule { .. }
            | change @ pbps_model::Change::CreateRole { .. }
            | change @ pbps_model::Change::DropRole { .. }
            | change @ pbps_model::Change::RenameRole { .. }
            | change @ pbps_model::Change::Grant { .. }
            | change @ pbps_model::Change::Revoke { .. } => panic!("unexpected change {change:?}"),
        }
    }
    assert_eq!(checked, 6);
    let human = stdout(&succeeds(d.run(&["plan", "--db", connection])));
    for text in [
        "Operational cost estimate",
        "Rewrite: yes",
        "Rewrite: no",
        "unknown",
        "approximately 3",
        "never analyzed",
        "ck_n",
        "AccessExclusiveLock",
        "no measurement",
    ] {
        assert!(human.contains(text), "missing {text}: {human}");
    }
    let risks: Vec<_> = saved.changes.risks().iter().map(|r| r.as_str()).collect();
    assert_eq!(report["data"]["risks"], serde_json::json!(risks));
    // Tightening nullability still requires its ordinary approval; cost never
    // grants it. The widening-only changes keep their existing risk classes.
    let applied = apply_plan(&d, connection, &artifact, false);
    assert_ne!(
        code(&applied),
        0,
        "{}{}",
        stdout(&applied),
        stderr(&applied)
    );
    assert!(stderr(&applied).contains("--allow"), "{}", stderr(&applied));
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

fn refuses_connected_check_json(d: &Demo, connection: &str, name: &str, detail: &str) {
    let out = d.run(&["plan", "--db", connection, "--format", "json"]);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["result"], "unanswerable", "{report}");
    assert!(
        report["findings"].as_array().unwrap().iter().any(|f| {
            let message = f["message"].as_str().unwrap_or_default();
            message.contains(name) && message.contains(detail)
        }),
        "{report}"
    );
}

fn passed_connected_check_json(d: &Demo, connection: &str, name: &str) -> serde_json::Value {
    let out = succeeds(d.run(&["plan", "--db", connection, "--format", "json"]));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let check = report["data"]["connected_checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {report}"));
    assert_eq!(check["status"], "passed", "{report}");
    assert_eq!(check["engine"], "PostgreSQL", "{report}");
    check.clone()
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
    // A comment is carried state the module precondition itself checks, not
    // an introspection limitation that an earlier guard could refuse instead.
    on_server(
        connection,
        "COMMENT ON VIEW app.v IS 'preserve this review note'",
    );
    refuses_connected_check_json(
        &d,
        connection,
        "before_a_rebuild (PostgreSQL)",
        "preserve this review note",
    );
    assert!(!plan.exists());
    on_server(connection, "COMMENT ON VIEW app.v IS NULL");
    let check = passed_connected_check_json(&d, connection, "before_a_rebuild");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .starts_with("1 module rebuild"),
        "{check}"
    );
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
fn bootstrap_requires_cluster_roles_before_building_and_before_recording() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_bootstrap_empty_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "bootstrap-missing-role");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("bootstrap-missing-role");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {}\n", role.1),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = |o: Output| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(
            stderr(&o).contains(&format!("CREATE ROLE \"{}\"", role.1)),
            "{}",
            stderr(&o)
        );
    };
    refused(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR EXISTS (SELECT 1 FROM public.__pbps_state) THEN RAISE EXCEPTION 'missing-role bootstrap built or recorded state'; END IF; END $$",
    );

    on_server(&server, &format!("CREATE ROLE {}", role.1));
    on_server(
        connection,
        &format!(
            "CREATE FUNCTION public.remove_bootstrap_role() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() WHERE object_identity = 'app.t') THEN DROP ROLE {}; END IF; END $$",
            role.1
        ),
    );
    on_server(
        connection,
        "CREATE EVENT TRIGGER remove_bootstrap_role ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.remove_bootstrap_role()",
    );
    refused(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        &format!(
            "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{}') THEN RAISE EXCEPTION 'missing-role read-back did not roll back the build'; END IF; END $$",
            role.1
        ),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let last = pbps_pg::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(last.snapshot.kind, pbps_model::StateKind::Failed);
        assert!(last.snapshot.schema.roles.is_empty());
        assert!(last.snapshot.ids.roles.is_empty());
    });
    on_server(connection, "DROP EVENT TRIGGER remove_bootstrap_role");
    on_server(connection, "DROP FUNCTION public.remove_bootstrap_role()");
    succeeds(d.run(&["bootstrap", "--db", connection]));
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
    refuses_connected_check_json(&d, connection, "missing_roles (PostgreSQL)", &role.1);
    assert!(!plan.exists());
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    let check = passed_connected_check_json(&d, connection, "missing_roles");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .starts_with("1 incoming or renamed"),
        "{check}"
    );
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
    refuses_connected_check_json(&d, connection, "rename_evidence (PostgreSQL)", "ALTER ROLE");
    on_server(&server, &format!("CREATE ROLE {new}"));
    refused(d.run(&planning)); // BothPresent
    on_server(&server, &format!("DROP ROLE {new}"));
    on_server(&server, &format!("ALTER ROLE {old} RENAME TO {parked}"));
    refused(d.run(&planning)); // NeitherPresent
    on_server(&server, &format!("ALTER ROLE {parked} RENAME TO {new}"));
    let check = passed_connected_check_json(&d, connection, "rename_evidence");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .starts_with("1 external cluster-role rename"),
        "{check}"
    );
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
    deployment_loop(&server(), "loop");
}

#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn the_deployment_loop_reads_constraints_before_postgres_18() {
    let server = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    on_server(
        &server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer >= 180000 THEN RAISE EXCEPTION 'this regression needs a pre-18 server'; END IF; END $$",
    );
    deployment_loop(&server, "old-loop");
}

fn deployment_loop(server: &str, slug: &str) {
    let own = OwnDatabase::new(server, slug);
    let connection = own.connection().to_owned();
    // The tool never creates a schema (`doctor` says so); the environment has
    // to bring it, exactly as on the other engine.
    on_server(&connection, "CREATE SCHEMA app");

    let d = Demo::new(slug);
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
    // Older servers lack the new flags, not constraints themselves. The
    // fallback must still observe an independently added ordinary CHECK.
    on_server(
        &connection,
        "ALTER TABLE app.t ADD CONSTRAINT positive_id CHECK (id > 0)",
    );
    let drift = d.run(&["verify", "--db", &connection]);
    assert_ne!(code(&drift), 0, "{}{}", stdout(&drift), stderr(&drift));
    assert!(
        stdout(&drift).contains("positive_id"),
        "{}{}",
        stdout(&drift),
        stderr(&drift)
    );
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

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_reports_data_and_role_grant_gaps_from_the_declarations() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let server = server();
    let roles = Roles(
        server.clone(),
        vec![
            format!("pbps_doctor_deployer_{}", std::process::id()),
            format!("pbps_doctor_reader_{}", std::process::id()),
        ],
    );
    let deployer = &roles.1[0];
    let reader = &roles.1[1];
    let own = OwnDatabase::new(&server, "doctor-demand");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {deployer} LOGIN PASSWORD 'doctor-test'; CREATE ROLE {reader}; \
         CREATE SCHEMA app AUTHORIZATION {deployer}; \
         CREATE TABLE app.t (id bigint NOT NULL PRIMARY KEY); \
         ALTER TABLE app.t OWNER TO {deployer}; \
         REVOKE INSERT ON app.t FROM {deployer}; \
         CREATE SCHEMA shared; CREATE VIEW shared.v AS SELECT 1 AS id; \
         CREATE FUNCTION shared.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
         CREATE FUNCTION shared.f(text) RETURNS text LANGUAGE sql AS 'SELECT $1'; \
         GRANT USAGE ON SCHEMA shared TO {deployer}; \
         GRANT SELECT ON shared.v TO {deployer}; \
         GRANT EXECUTE ON FUNCTION shared.f(text) TO {deployer} WITH GRANT OPTION; \
         GRANT CREATE ON SCHEMA public TO {deployer}"
        ),
    );
    let login = format!(
        "{} user={deployer} password=doctor-test",
        connection
            .split_whitespace()
            .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let d = Demo::new("doctor-demand");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nenvironments:\n  dev:\n    url_env: PBPS_FLOW_PG_DEV\n",
    )
    .unwrap();
    d.table(&format!(
        "{ONE_COLUMN}data:\n  mode: ensure\n  rows:\n    1: {{}}\n"
    ));
    std::fs::write(d.dir.join("schema/reader.yml"), format!(
        "role: {reader}\ngrants:\n  schema::shared: [usage]\n  shared.v: [select]\n  shared.f(integer): [execute]\n"
    )).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let diagnose = || {
        let output = d.run_with_env(
            &["doctor", "--format", "json"],
            &[("PBPS_FLOW_PG_DEV", login.as_str())],
        );
        let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        (output, json)
    };
    let (output, json) = diagnose();
    assert_eq!(code(&output), 2, "{json}");
    let gaps = json["data"]["environments"][0]["missing_permissions"]
        .as_array()
        .unwrap();
    for expected in [
        "INSERT on TABLE \"app\".\"t\"",
        "USAGE WITH GRANT OPTION on SCHEMA \"shared\"",
        "SELECT WITH GRANT OPTION on TABLE \"shared\".\"v\"",
        "EXECUTE WITH GRANT OPTION on ROUTINE \"shared\".\"f\"(integer)",
    ] {
        assert!(
            gaps.iter()
                .any(|g| g.as_str().unwrap().starts_with(expected)),
            "missing {expected}: {json}"
        );
    }
    assert!(
        !gaps
            .iter()
            .any(|g| g.as_str().unwrap().starts_with("DELETE")
                || g.as_str().unwrap().starts_with("UPDATE")),
        "a key-only ensure block never updates or deletes: {json}"
    );
    on_server(
        connection,
        &format!(
            "GRANT INSERT (id) ON app.t TO {deployer}; \
         GRANT USAGE ON SCHEMA shared TO {deployer} WITH GRANT OPTION; \
         GRANT SELECT ON shared.v TO {deployer} WITH GRANT OPTION; \
         GRANT EXECUTE ON FUNCTION shared.f(integer) TO {deployer} WITH GRANT OPTION"
        ),
    );
    let (_, json) = diagnose();
    assert_eq!(
        json["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([]),
        "the named remedies remove every permission gap: {json}"
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn arriving_overloads_rebind_unchanged_callers_in_the_approved_plan() {
    let server = server();
    let own = OwnDatabase::new(&server, "overload-rebind");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("overload-rebind");
    std::fs::write(d.dir.join("schema/f-bigint.yml"),
        "function: app.f(bigint)\ndefinition: (n bigint) RETURNS text LANGUAGE sql AS $$ SELECT 'old' $$\n").unwrap();
    std::fs::write(d.dir.join("schema/caller.yml"),
        "function: app.caller()\ndefinition: () RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT f(1); END\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "DO $$ BEGIN IF app.caller() <> 'old' THEN RAISE EXCEPTION 'wrong initial binding'; END IF; END $$",
    );

    std::fs::write(d.dir.join("schema/f-integer.yml"),
        "function: app.f(integer)\ndefinition: (n integer) RETURNS text LANGUAGE sql AS $$ SELECT 'new' $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    succeeds(d.run(&planning));
    // Run the real plan before inspecting its shape: an omitted rebuild must
    // fail on the engine's observed binding, not merely a mirrored assertion.
    succeeds(apply_plan(&d, connection, &plan, false));
    on_server(
        connection,
        "DO $$ BEGIN IF app.caller() <> 'new' THEN RAISE EXCEPTION 'successful apply retained the old overload binding'; END IF; END $$",
    );
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    let rebuilt: Vec<_> = saved
        .changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::AlterModule { id, .. } = &p.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(rebuilt, ["app.caller()"]);
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&planning));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));

    // A second new signature still reaches the ordinary carried-state guard.
    // The scanner is intentionally conservative about overload applicability.
    std::fs::write(d.dir.join("schema/f-smallint.yml"),
        "function: app.f(smallint)\ndefinition: (n smallint) RETURNS text LANGUAGE sql AS $$ SELECT 'small' $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    on_server(connection, "COMMENT ON FUNCTION app.caller() IS 'keep me'");
    let refused = d.run(&planning);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("comment"), "{}", stderr(&refused));
    on_server(connection, "COMMENT ON FUNCTION app.caller() IS NULL");
    succeeds(d.run(&planning));
    let staged = d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]);
    assert_eq!(code(&staged), 1, "{}{}", stdout(&staged), stderr(&staged));
    assert!(
        stderr(&staged).contains("require a transaction"),
        "{}",
        stderr(&staged)
    );
    on_server(
        connection,
        "COMMENT ON FUNCTION app.caller() IS 'arrived after planning'",
    );
    let refused = apply_plan(&d, connection, &plan, false);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("comment"), "{}", stderr(&refused));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regprocedure('app.f(smallint)') IS NOT NULL OR app.caller() <> 'new' THEN RAISE EXCEPTION 'refused rebuild changed the database'; END IF; END $$",
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn unrelated_routine_limitations_do_not_refuse_a_managed_table_or_overload() {
    let own = OwnDatabase::new(&server(), "limitation-scope");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("limitation-scope");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.t(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "CREATE AGGREGATE app.t(bigint) (SFUNC = int8pl, STYPE = bigint, INITCOND = '0')",
    );
    succeeds(d.run(&["plan", "--db", connection]));
    succeeds(d.run(&["snapshot", "--db", connection, "--force"]));
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "accept managed state",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));

    // A real table limitation must still refuse the same managed scope.
    on_server(connection, "ALTER TABLE app.t SET UNLOGGED");
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("UNLOGGED"),
        "{}",
        stderr(&refused)
    );
    on_server(connection, "ALTER TABLE app.t SET LOGGED");

    // The exact managed signature becoming unsupported is also a limitation.
    on_server(
        connection,
        "DROP FUNCTION app.t(integer); CREATE AGGREGATE app.t(integer) (SFUNC = int4pl, STYPE = integer, INITCOND = '0')",
    );
    let refused = d.run(&["baseline", "--db", connection, "--reason", "must refuse"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("aggregate"),
        "{}",
        stderr(&refused)
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn omitted_modules_obey_unmanaged_policy_even_beside_a_managed_overload() {
    let own = OwnDatabase::new(&server(), "unmanaged-modules");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("unmanaged-modules");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.t(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    // Roles are cluster-wide: this database still sees principals left by
    // other live tests. Keep their refusal as a control rather than assuming
    // an own database means an empty unmanaged inventory.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nunmanaged: error\n",
    )
    .unwrap();
    d.commit();
    let control = d.run(&["plan", "--db", connection]);

    on_server(
        connection,
        "CREATE AGGREGATE app.t(bigint) (SFUNC = int8pl, STYPE = bigint, INITCOND = '0'); CREATE VIEW app.\"bad(int)\" AS SELECT id FROM app.t",
    );

    for policy in ["ignore", "warn", "error"] {
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: postgres\nunmanaged: {policy}\n"),
        )
        .unwrap();
        d.commit();
        let result = d.run(&["plan", "--db", connection]);
        if policy == "error" {
            assert_ne!(code(&result), 0, "{}", stdout(&result));
            let message = stderr(&result);
            assert!(message.contains("`unmanaged: error`"), "{message}");
            assert!(message.contains("aggregate app.t(bigint)"), "{message}");
            assert!(message.contains("view app.bad(int)"), "{message}");
            let verified = d.run(&["verify", "--db", connection, "--format", "json"]);
            let report: serde_json::Value = serde_json::from_str(&stdout(&verified)).unwrap();
            assert_eq!(code(&verified), 2, "{report}: {}", stderr(&verified));
            assert!(
                report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| {
                        finding["id"] == "state.unmanaged-refused"
                            && finding["message"].as_str().is_some_and(|message| {
                                message.contains("aggregate app.t(bigint)")
                                    && message.contains("view app.bad(int)")
                            })
                    }),
                "{report}"
            );
        } else {
            let message = stderr(&result);
            assert_eq!(
                message.contains("are not declared and are left alone"),
                policy == "warn",
                "{message}"
            );
            succeeds(result);
        }
    }
    // Removing only our omitted modules restores the original policy answer;
    // the same-name managed function must not become an unmanaged object.
    on_server(
        connection,
        "DROP AGGREGATE app.t(bigint); DROP VIEW app.\"bad(int)\"",
    );
    let restored = d.run(&["plan", "--db", connection]);
    assert_eq!(code(&restored), code(&control));
    assert_eq!(stderr(&restored), stderr(&control));
}

#[test]
#[ignore = "needs PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn permission_versions_are_checked_before_planning_bootstrap_apply_and_resume() {
    struct Roles(Vec<(String, String)>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for (server, name) in &self.0 {
                let _ = try_on_server(server, &format!("DROP ROLE IF EXISTS {name}"));
            }
        }
    }
    let old_server = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    let new_server = server();
    let role = format!("pbps_version_{}", std::process::id());
    let _roles = Roles(vec![
        (old_server.clone(), role.clone()),
        (new_server.clone(), role.clone()),
    ]);
    for server in [&old_server, &new_server] {
        on_server(server, &format!("CREATE ROLE {role}"));
    }
    on_server(
        &old_server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer >= 170000 THEN RAISE EXCEPTION 'needs pre-17'; END IF; END $$",
    );
    on_server(
        &new_server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer < 170000 THEN RAISE EXCEPTION 'needs 17+'; END IF; END $$",
    );
    let old = OwnDatabase::new(&old_server, "permission_version");
    let new = OwnDatabase::new(&new_server, "permission_version");
    let bootstrap_target = OwnDatabase::new(&old_server, "permission_bootstrap");
    for db in [&old, &new, &bootstrap_target] {
        on_server(db.connection(), "CREATE SCHEMA app");
    }
    let d = Demo::new("permission-version");
    d.table(ONE_COLUMN);
    let role_file = d.dir.join("schema/reader.yml");
    let declare = |permissions: &str| {
        std::fs::write(
            &role_file,
            format!("role: {role}\ngrants:\n  app.t: [{permissions}]\n  schema::app: [usage]\n"),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
    };
    declare("select");
    for db in [&old, &new] {
        succeeds(d.run(&["bootstrap", "--db", db.connection()]));
    }
    declare("select, maintain");
    let refuses_version = |o: Output| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(
            stderr(&o).contains("permission_support (PostgreSQL)"),
            "{}",
            stderr(&o)
        );
        assert!(
            stderr(&o).contains("PostgreSQL 17 or later"),
            "{}",
            stderr(&o)
        );
    };
    refuses_version(d.run(&["bootstrap", "--db", bootstrap_target.connection()]));
    on_server(
        bootstrap_target.connection(),
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR to_regclass('public.__pbps_state') IS NOT NULL THEN RAISE EXCEPTION 'bootstrap wrote before refusing'; END IF; END $$",
    );

    let plan = d.dir.join("version-plan.json");
    let o = d.run(&[
        "plan",
        "--db",
        old.connection(),
        "--out",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let report: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(report["result"], "unanswerable", "{report}");
    assert!(
        report["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("permission_support (PostgreSQL)"),
        "{report}"
    );
    assert!(
        !plan.exists(),
        "refused planning must not write an artifact"
    );
    let o = succeeds(d.run(&[
        "plan",
        "--db",
        new.connection(),
        "--out",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let report: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(
        report["data"]["connected_checks"][0]["name"],
        "permission_support"
    );
    assert_eq!(report["data"]["connected_checks"][0]["status"], "passed");
    assert_eq!(
        report["data"]["connected_checks"][0]["engine"],
        "PostgreSQL"
    );
    refuses_version(apply_plan(&d, old.connection(), &plan, false));
    let staged = d.dir.join("version-staged.json");
    succeeds(d.run(&[
        "plan",
        "--db",
        new.connection(),
        "--out",
        staged.to_str().unwrap(),
        "--staged",
        "--format",
        "json",
    ]));
    refuses_version(apply_plan(&d, old.connection(), &staged, true));
    refuses_version(d.run(&[
        "apply",
        "--db",
        old.connection(),
        "--plan",
        staged.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&staged),
        "--staged",
        "--resume",
    ]));
    let listing = succeeds(d.run(&[
        "state",
        "list",
        "--db",
        old.connection(),
        "--format",
        "json",
    ]));
    let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
    assert_eq!(
        listing["data"]["entries"].as_array().unwrap().len(),
        1,
        "a capability refusal must not write a ledger entry: {listing}"
    );
    succeeds(apply_plan(&d, new.connection(), &plan, false));
    on_server(
        new.connection(),
        &format!(
            "DO $$ BEGIN IF NOT has_table_privilege('{role}', 'app.t', 'MAINTAIN') THEN RAISE EXCEPTION 'grant missing'; END IF; END $$"
        ),
    );

    // Reference-data adoption also remains a single JSON document, carrying
    // the notice that the human plan prints before its summary.
    d.table(&format!(
        "{ONE_COLUMN}data:\n  mode: ensure\n  rows:\n    1: {{}}\n"
    ));
    succeeds(d.run(&["plan"]));
    d.commit();
    let adoption = succeeds(d.run(&["plan", "--db", new.connection(), "--format", "json"]));
    let adoption: serde_json::Value = serde_json::from_str(&stdout(&adoption)).unwrap();
    assert!(
        adoption["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "data.adoption"),
        "{adoption}"
    );
    d.table(ONE_COLUMN);

    // A revoke-only plan still works on the old server. The guard is about
    // permissions the plan grants, not permissions mentioned in a declaration.
    std::fs::write(&role_file, format!("role: {role}\n")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "plan",
        "--db",
        old.connection(),
        "--out",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(
        saved
            .changes
            .changes
            .iter()
            .all(|p| matches!(p.change, pbps_model::Change::Revoke { .. }))
    );
    succeeds(d.run(&[
        "apply",
        "--db",
        old.connection(),
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn drop_blockers_are_named_in_planning_and_rechecked_before_apply_writes() {
    for (target, slug) in [("app.t.label", "drop_column"), ("app.t", "drop_table")] {
        let db = OwnDatabase::new(&server(), slug);
        let connection = db.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new(slug);
        d.table(TWO_COLUMNS);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        if target == "app.t" {
            std::fs::remove_file(d.dir.join("schema/app.t.yml")).unwrap();
        } else {
            d.table(ONE_COLUMN);
        }
        let command = if target == "app.t" {
            "drop-table"
        } else {
            "drop"
        };
        succeeds(d.run(&[command, target, "--reason", "no longer retained"]));
        d.commit();
        on_server(
            connection,
            "CREATE VIEW app.external_v AS SELECT label FROM app.t",
        );
        let plan = d.dir.join("drop.json");
        let out = d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            plan.to_str().unwrap(),
        ]);
        assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        assert_eq!(report["result"], "unanswerable", "{report}");
        let message = report["findings"][0]["message"].as_str().unwrap();
        assert!(
            message.contains("drop_blockers (PostgreSQL)") && message.contains("external_v"),
            "{report}"
        );
        assert!(!plan.exists(), "refused plans must not be saved");
        on_server(connection, "DROP VIEW app.external_v");
        let out = succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            plan.to_str().unwrap(),
        ]));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        let checks = report["data"]["connected_checks"].as_array().unwrap();
        assert!(
            checks
                .iter()
                .any(|c| c["name"] == "drop_blockers" && c["status"] == "passed"),
            "{report}"
        );
        let human = stdout(&succeeds(d.run(&["plan", "--db", connection])));
        assert!(
            human.contains("drop_blockers") && human.contains("catalog dependencies"),
            "{human}"
        );
        let staged = d.dir.join("drop-staged.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--staged",
            "--out",
            staged.to_str().unwrap(),
        ]));
        on_server(
            connection,
            "CREATE VIEW app.external_v AS SELECT label FROM app.t",
        );
        for (artifact, is_staged) in [(&plan, false), (&staged, true)] {
            let checksum = plan_checksum(artifact);
            let mut args = vec![
                "apply",
                "--db",
                connection,
                "--plan",
                artifact.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--allow",
                "destructive",
            ];
            if is_staged {
                args.push("--staged");
            }
            let out = d.run(&args);
            assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out).contains("drop_blockers (PostgreSQL)")
                    && stderr(&out).contains("external_v"),
                "{}",
                stderr(&out)
            );
            let listing =
                succeeds(d.run(&["state", "list", "--db", connection, "--format", "json"]));
            let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
            let entries = listing["data"]["entries"].as_array().unwrap();
            assert_eq!(
                entries.iter().filter(|e| e["kind"] != "failed").count(),
                1,
                "{listing}"
            );
            assert_eq!(
                entries[0]["kind"], "failed",
                "failed attempts retain their audit entry"
            );
            on_server(connection, "SELECT label FROM app.t");
        }
        on_server(connection, "DROP VIEW app.external_v");
        succeeds(d.run(&[
            "apply",
            "--db",
            connection,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&plan),
            "--allow",
            "destructive",
        ]));
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn drop_blockers_do_not_refuse_a_closing_resume_after_the_drop_completed() {
    use pbps_dialect::Dialect;
    let db = OwnDatabase::new(&server(), "drop_resume");
    let connection = db.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("drop-resume");
    d.table(TWO_COLUMNS);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d.table(ONE_COLUMN);
    succeeds(d.run(&["drop", "app.t.label", "--reason", "no longer retained"]));
    d.commit();
    let artifact = d.dir.join("staged.json");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        artifact.to_str().unwrap(),
    ]));
    let plan: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert_eq!(plan.changes.changes.len(), 1);
    let pg = pbps_pg::Postgres::new();
    let p = &plan.changes.changes[0];
    let statements = pg.emit(&p.change, p.strategy).unwrap();
    assert_eq!(
        statements.len(),
        1,
        "a pending DROP cannot follow a prior statement of this change"
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let mut checkpoint = pbps_pg::state::latest(&mut conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        conn.execute(&statements[0].sql).await.unwrap();
        checkpoint
            .schema
            .tables
            .get_mut(&pbps_model::TableName::new("app", "t"))
            .unwrap()
            .columns
            .shift_remove("label");
        checkpoint.ids = plan.ids.clone();
        checkpoint.kind = pbps_model::StateKind::Staged;
        checkpoint.plan_checksum = Some(plan.checksum());
        checkpoint.staged = Some(pbps_model::StagedProgress {
            completed: 1,
            total: 1,
            last_statement: statements[0].sql.clone(),
        });
        pbps_pg::state::record(&mut conn, &checkpoint)
            .await
            .unwrap();
    });
    succeeds(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        artifact.to_str().unwrap(),
        "--checksum",
        &plan.checksum(),
        "--allow",
        "destructive",
        "--staged",
        "--resume",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_cli_ledger_preserves_history_and_distinguishes_absent_empty_and_unreadable() {
    let own = OwnDatabase::new(&server(), "invariant_ledger");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("invariant-ledger");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    let history = || {
        json_output(succeeds(
            d.run(&["state", "list", "--db", connection, "--format", "json"]),
        ))
    };
    let absent = history();
    assert_eq!(absent["data"]["initialized"], false, "{absent}");
    assert_eq!(absent["data"]["entries"], serde_json::json!([]));
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopting an empty schema",
    ]));
    let recorded = latest_snapshot(connection);
    assert!(recorded.schema.tables.is_empty());
    assert_eq!(
        latest_snapshot(connection),
        recorded,
        "separate connections read the same snapshot"
    );
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let v = history();
    let entries = v["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(entries[0]["kind"], "bootstrap");
    assert_eq!(entries[0]["tables"], 1);
    assert_eq!(entries[1]["kind"], "baseline");
    assert_eq!(entries[1]["tables"], 0);
    assert_eq!(entries[1]["reason"], "adopting an empty schema");
    assert!(entries[0]["id"].as_i64().unwrap() > entries[1]["id"].as_i64().unwrap());
    let limited = json_output(succeeds(d.run(&[
        "state", "list", "--db", connection, "--format", "json", "--limit", "1",
    ])));
    assert_eq!(limited["data"]["entries"], serde_json::json!([entries[0]]));
    // Missing query columns are unreadable, never an empty successful history.
    on_server(
        connection,
        "ALTER TABLE public.__pbps_state RENAME COLUMN kind TO hidden_kind",
    );
    let broken = d.run(&["state", "list", "--db", connection, "--format", "json"]);
    assert_eq!(code(&broken), 1, "{}", stderr(&broken));
    let broken = json_output(broken);
    assert_eq!(broken["result"], "unanswerable");
    assert!(broken.get("data").is_none(), "{broken}");
    on_server(
        connection,
        "ALTER TABLE public.__pbps_state RENAME COLUMN hidden_kind TO kind; DELETE FROM public.__pbps_state",
    );
    let empty = history();
    assert_eq!(empty["data"]["initialized"], true);
    assert_eq!(empty["data"]["entries"], serde_json::json!([]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_cli_refuses_the_second_lock_holder_and_unlock_releases_only_the_gate() {
    let own = OwnDatabase::new(&server(), "invariant_lock");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-lock", ONE_COLUMN);
    on_server(
        connection,
        "INSERT INTO public.__pbps_lock (id, locked_by) VALUES (1, 'invariant-first-holder')",
    );
    let before = latest_snapshot(connection);
    for args in [
        vec!["baseline", "--db", connection, "--reason", "second holder"],
        vec!["snapshot", "--db", connection],
    ] {
        let denied = d.run(&args);
        assert_eq!(code(&denied), 1, "{}{}", stdout(&denied), stderr(&denied));
        assert!(
            stderr(&denied).contains("invariant-first-holder"),
            "{}",
            stderr(&denied)
        );
    }
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_lock WHERE locked_by = 'invariant-first-holder'"
        ),
        1
    );
    assert_eq!(latest_snapshot(connection), before);
    let diagnosis = d.run(&["doctor", "--db", connection, "--format", "json"]);
    assert_eq!(code(&diagnosis), 2);
    let diagnosis = json_output(diagnosis);
    assert!(
        diagnosis["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "state.locked")
    );
    succeeds(d.run(&["unlock", "--db", connection]));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM public.__pbps_lock"),
        0
    );
    assert_eq!(latest_snapshot(connection), before);
    succeeds(d.run(&["unlock", "--db", connection]));
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "after unlock"]));
    assert_eq!(
        latest_snapshot(connection).reason.as_deref(),
        Some("after unlock")
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_second_statement_failure_rolls_back_the_first_and_records_only_a_failed_attempt() {
    let own = OwnDatabase::new(&server(), "invariant_atomic");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-atomic", ONE_COLUMN);
    d.table(&ONE_COLUMN.replace(
        "  id:",
        "  first: {type: text}\n  second: {type: text}\n  id:",
    ));
    let plan = connected_artifact(&d, connection, false);
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert_eq!(saved.changes.changes.len(), 2);
    // An event trigger fails the second DDL after the first really executed.
    // Sequence increments survive rollback, so this cannot pass by refusing
    // in preflight before either statement ran.
    on_server(
        connection,
        r#"
        CREATE SCHEMA witness;
        CREATE SEQUENCE witness.executed;
        CREATE FUNCTION witness.fail_second() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF EXISTS (SELECT FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname IN ('first', 'second') AND NOT attisdropped) THEN
            PERFORM nextval('witness.executed');
          END IF;
          IF (SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname IN ('first', 'second') AND NOT attisdropped) = 2 THEN
            RAISE EXCEPTION 'invariant second statement failed';
          END IF;
        END $$;
        CREATE EVENT TRIGGER fail_second ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.fail_second();
    "#,
    );
    let failed = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    assert_eq!(
        scalar(
            connection,
            "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed"
        ),
        2,
        "{}{}",
        stdout(&failed),
        stderr(&failed)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname IN ('first', 'second') AND NOT attisdropped"
        ),
        0
    );
    let after = latest_snapshot(connection);
    assert_eq!(after.kind, pbps_model::StateKind::Failed);
    assert_eq!(
        after.plan_checksum.as_deref(),
        Some(plan_checksum(&plan).as_str())
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
        ),
        0
    );
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(connection, "DROP EVENT TRIGGER fail_second");
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(
        latest_snapshot(connection).kind,
        pbps_model::StateKind::Apply
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_cli_probe_counts_real_null_rows_before_any_statement_and_then_accepts_clean_data() {
    let own = OwnDatabase::new(&server(), "invariant_probe");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-probe", TWO_COLUMNS);
    on_server(
        connection,
        "INSERT INTO app.t (id, label) VALUES (1, NULL), (2, NULL), (3, 'present')",
    );
    d.table(&TWO_COLUMNS.replace(
        "label: {type: varchar(50)}",
        "label: {type: varchar(50), nullable: false}",
    ));
    let plan = connected_artifact(&d, connection, false);
    let refused = approved_apply(&d, connection, &plan, &["--allow", "not-null"]);
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("2 existing NULLs in"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("nothing has been changed"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'label' AND attnotnull"
        ),
        0
    );
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t WHERE label IS NULL"),
        2
    );
    on_server(
        connection,
        "UPDATE app.t SET label = 'filled' WHERE label IS NULL",
    );
    let applied = succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "not-null"],
    ));
    assert!(
        stdout(&applied).contains("probe(s) passed"),
        "{}",
        stdout(&applied)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'label' AND attnotnull"
        ),
        1
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn modules_apply_then_pull_round_trip_and_changed_bodies_are_drift() {
    let own = OwnDatabase::new(&server(), "invariant_modules");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-modules", ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/v.yml"),
        "view: app.v\ndefinition: SELECT id FROM app.t WHERE id > 0\n",
    )
    .unwrap();
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.f(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n + 1 $$\n").unwrap();
    let plan = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &plan, &[]));
    succeeds(d.run(&["verify", "--db", connection]));
    let fresh = Demo::new("invariant-modules-pull");
    succeeds(fresh.run(&["pull", "--db", connection]));
    let loaded = pbps_load::load_schema_dir(&fresh.dir.join("schema")).unwrap();
    assert_eq!(loaded.schema.modules.len(), 2);
    assert!(
        loaded
            .schema
            .modules
            .contains_key(&"app.v".parse().unwrap())
    );
    assert!(
        loaded
            .schema
            .modules
            .contains_key(&"app.f(integer)".parse().unwrap())
    );
    // Pull serializes the engine's canonical body, which may differ from
    // the original SQL spelling tracked in declared-state metadata.
    assert_eq!(
        loaded.schema.modules,
        latest_snapshot(connection).schema.modules
    );
    on_server(
        connection,
        "CREATE OR REPLACE VIEW app.v AS SELECT id FROM app.t WHERE id > 1",
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2, "{}{}", stdout(&drift), stderr(&drift));
    assert!(stdout(&drift).contains("app.v"), "{}", stdout(&drift));
    let overwrite = fresh.run(&["pull", "--db", connection]);
    assert_eq!(code(&overwrite), 1);
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_cli_checkpoint_survives_state_json_and_resume_checks_its_intermediate_name() {
    let own = OwnDatabase::new(&server(), "invariant_checkpoint");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-checkpoint", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.stop_rename() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.u') IS NOT NULL THEN RAISE EXCEPTION 'invariant stop after schema transfer'; END IF;
        END $$;
        CREATE EVENT TRIGGER stop_rename ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.stop_rename();
    "#,
    );
    let failed = approved_apply(&d, connection, &plan, &["--allow", "rename", "--staged"]);
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    let checkpoint = latest_snapshot(connection);
    let progress = checkpoint
        .staged
        .as_ref()
        .expect("a durable checkpoint, not a transactional rollback");
    assert_eq!((progress.completed, progress.total), (1, 2));
    assert!(
        progress.last_statement.contains("SET SCHEMA"),
        "{progress:?}"
    );
    assert!(
        checkpoint
            .schema
            .tables
            .contains_key(&"moved.t".parse().unwrap()),
        "{checkpoint:?}"
    );
    assert!(
        !checkpoint
            .schema
            .tables
            .contains_key(&"moved.u".parse().unwrap())
    );
    assert_eq!(
        latest_snapshot(connection),
        checkpoint,
        "the checkpoint round-trips on a fresh connection"
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.t')"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.u')"
        ),
        0
    );
    let listed = json_output(succeeds(
        d.run(&["state", "list", "--db", connection, "--format", "json"]),
    ));
    assert_eq!(
        listed["data"]["entries"][0]["staged"]["completed"], 1,
        "{listed}"
    );
    // Movement after the saved checkpoint is not silently adopted by resume.
    on_server(
        connection,
        "DROP EVENT TRIGGER stop_rename; ALTER TABLE moved.t ADD COLUMN rogue text",
    );
    let refused = approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    );
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("has moved since the checkpoint"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        latest_snapshot(connection)
            .staged
            .as_ref()
            .unwrap()
            .completed,
        1
    );
    on_server(connection, "ALTER TABLE moved.t DROP COLUMN rogue");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    ));
    let closed = latest_snapshot(connection);
    assert_eq!(closed.kind, pbps_model::StateKind::Apply);
    assert!(closed.staged.is_none());
    assert!(
        closed
            .schema
            .tables
            .contains_key(&"moved.u".parse().unwrap())
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.t')"
        ),
        0
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_cross_schema_cli_rename_requires_approval_and_records_only_its_declared_destination() {
    let own = OwnDatabase::new(&server(), "invariant_cross_schema");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-cross-schema", ONE_COLUMN);
    on_server(
        connection,
        "CREATE SCHEMA moved; INSERT INTO app.t VALUES (42)",
    );
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, false);
    let denied = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&denied), 1);
    assert!(stderr(&denied).contains("--allow"), "{}", stderr(&denied));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t WHERE id = 42"),
        1
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM moved.u WHERE id = 42"),
        1
    );
    for name in ["app.t", "app.u", "moved.t"] {
        assert_eq!(
            scalar(
                connection,
                &format!("SELECT count(*) FROM pg_class WHERE oid = to_regclass('{name}')")
            ),
            0,
            "no intermediate alias {name}"
        );
    }
    let recorded = latest_snapshot(connection);
    assert_eq!(
        recorded.schema.tables.keys().collect::<Vec<_>>(),
        vec![&"moved.u".parse::<pbps_model::TableName>().unwrap()]
    );
    assert_eq!(
        recorded.ids.tables.values().collect::<Vec<_>>(),
        vec![&"moved.u".parse::<pbps_model::TableName>().unwrap()]
    );
    succeeds(d.run(&["verify", "--db", connection]));
    assert!(stdout(&succeeds(d.run(&["plan", "--db", connection]))).contains("No changes"));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn postgres_rename_impact_reaches_the_cli_as_advisory_and_requires_explicit_approval() {
    let own = OwnDatabase::new(&server(), "invariant_impact");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-impact", TWO_COLUMNS);
    on_server(
        connection,
        r#"
        CREATE SCHEMA outside;
        CREATE VIEW outside.carried AS SELECT label FROM app.t;
        CREATE FUNCTION outside.text_body() RETURNS text LANGUAGE plpgsql AS $$
          BEGIN RETURN (SELECT label FROM app.t LIMIT 1); END $$;
        INSERT INTO app.t VALUES (1, 'kept');
    "#,
    );
    d.table(&TWO_COLUMNS.replace(
        "  label: {type: varchar(50)}",
        "  note: {type: varchar(50), renamed_from: label}",
    ));
    let plan = connected_artifact(&d, connection, false);
    let denied = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&denied), 1);
    assert!(stderr(&denied).contains("--allow"));
    // PostgreSQL carries catalog dependencies instead of refusing them as
    // SCHEMABINDING. A text body is advisory, printed by apply's preflight.
    let applied = succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    assert!(
        stdout(&applied).contains("outside.text_body"),
        "{}",
        stdout(&applied)
    );
    assert!(
        stdout(&applied).contains("Nothing outside the database is visible"),
        "{}",
        stdout(&applied)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM outside.carried WHERE label = 'kept'"
        ),
        1
    );
    let broken = try_on_server(connection, "SELECT outside.text_body()");
    assert!(
        broken.is_err(),
        "the advisory names a text body the rename really breaks"
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_exercises_a_real_non_superuser_and_names_the_permission_removed_from_it() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_invariant_doctor_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "invariant_doctor");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'invariant-test'; CREATE SCHEMA app AUTHORIZATION {}; GRANT CREATE ON SCHEMA public TO {}",
            role.1, role.1, role.1
        ),
    );
    let login = format!(
        "{} user={} password=invariant-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );
    let d = Demo::new("invariant-doctor");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &login]));
    assert_eq!(
        scalar(
            &login,
            "SELECT count(*) FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication)"
        ),
        0
    );
    let ready = json_output(succeeds(
        d.run(&["doctor", "--db", &login, "--format", "json"]),
    ));
    assert_eq!(
        ready["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([]),
        "{ready}"
    );
    on_server(
        connection,
        &format!("REVOKE SELECT ON app.t FROM {}", role.1),
    );
    let refused = d.run(&["doctor", "--db", &login, "--format", "json"]);
    assert_eq!(
        code(&refused),
        2,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    let refused = json_output(refused);
    let missing = refused["data"]["environments"][0]["missing_permissions"]
        .as_array()
        .unwrap();
    assert!(
        missing.iter().any(|v| v
            .as_str()
            .is_some_and(|s| s.contains("SELECT") && s.contains("app"))),
        "{refused}"
    );
    assert!(!refused.to_string().contains("invariant-test"));
    on_server(connection, &format!("GRANT SELECT ON app.t TO {}", role.1));
    succeeds(d.run(&["doctor", "--db", &login]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn reference_data_applies_pulls_defaults_and_refuses_rows_arriving_after_the_plan() {
    let own = OwnDatabase::new(&server(), "invariant_data");
    let connection = own.connection();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false, default: \"'Unlabelled'\"}\nprimary_key: {name: pk_t, columns: [code]}\n";
    let d = bootstrapped_demo(connection, "invariant-data", declared);
    d.table(&format!(
        "{declared}data:\n  mode: exact\n  rows:\n    new: {{label: New}}\n    old: {{}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    on_server(connection, "INSERT INTO app.t (code) VALUES ('rogue')");
    let refused = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("no longer the database this plan was computed against"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE code IN ('new', 'old')"
        ),
        0
    );
    on_server(connection, "DELETE FROM app.t WHERE code = 'rogue'");
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE (code = 'new' AND label = 'New') OR (code = 'old' AND label = 'Unlabelled')"
        ),
        2
    );
    succeeds(d.run(&["verify", "--db", connection]));
    assert!(stdout(&succeeds(d.run(&["plan", "--db", connection]))).contains("No changes"));
    on_server(
        connection,
        "UPDATE app.t SET label = 'changed' WHERE code = 'old'",
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2);
    assert!(stdout(&drift).contains("row old"), "{}", stdout(&drift));
    on_server(
        connection,
        "UPDATE app.t SET label = 'Unlabelled' WHERE code = 'old'",
    );
    let fresh = Demo::new("invariant-data-pull");
    succeeds(fresh.run(&["pull", "--db", connection, "--data", "app.t"]));
    let file = std::fs::read_to_string(fresh.dir.join("schema/app.t.yml")).unwrap();
    assert!(file.contains("mode: exact"), "{file}");
    assert!(file.contains("new: {label: New}"), "{file}");
    assert!(file.contains("old: {}"), "{file}");
    assert!(!file.contains("rogue"), "{file}");
    let missing = fresh.run(&["pull", "--db", connection, "--data", "app.nope", "--force"]);
    assert_eq!(code(&missing), 1);
    assert!(stderr(&missing).contains("app.nope"));
    assert_eq!(
        std::fs::read_to_string(fresh.dir.join("schema/app.t.yml")).unwrap(),
        file
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_schema_transfer_does_not_hide_an_unplanned_column_at_the_intermediate_name() {
    let own = OwnDatabase::new(&server(), "invariant_checkpoint_movement");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-checkpoint-movement", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    // This lands inside the first emitted statement. The next checkpoint
    // must catch it even though that statement also moved the table's name.
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.add_rogue() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.t') IS NOT NULL AND NOT EXISTS (
            SELECT FROM pg_attribute WHERE attrelid = 'moved.t'::regclass AND attname = 'rogue' AND NOT attisdropped
          ) THEN ALTER TABLE moved.t ADD COLUMN rogue text; END IF;
        END $$;
        CREATE EVENT TRIGGER add_rogue ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.add_rogue();
    "#,
    );
    let refused = approved_apply(&d, connection, &plan, &["--allow", "rename", "--staged"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("rogue"), "{}", stderr(&refused));
    let checkpoint = latest_snapshot(connection);
    assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 1);
    assert!(
        checkpoint.schema.tables[&"moved.t".parse().unwrap()]
            .columns
            .contains_key("rogue")
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.u')"
        ),
        0
    );
}
