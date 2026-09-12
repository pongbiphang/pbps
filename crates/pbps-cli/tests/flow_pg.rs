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

/// #248, end to end: a rebuild forced by an ordinary view edit still lets a
/// declared, least-privilege role read the view afterwards.
///
/// Read as the granted role itself, never as `postgres` — a superuser passes
/// every privilege check without consulting an ACL at all, so a superuser
/// read would pass whether or not the rebuild restored anything (DECISIONS
/// 375, the live suite's own correction of exactly that mistake). Only a
/// query that can fail on a missing grant is evidence the grant came back.
///
/// The refusal itself reads the plan: `read_acl` calls a grant restorable only
/// when the plan's own `ChangeSet` carries a matching `Grant`, so a plan that
/// does not restate this grant is exactly the plan that still refuses to
/// rebuild — there is no reachable path where the apply proceeds and the
/// grant is silently lost. That coupling is why reverting the differ's
/// restatement fails this test at the `plan --db` step, not at the final
/// read below — the refusal proves the plan *contains* the `Grant`; only the
/// role's own read after the apply proves the emitted statement lands on the
/// rebuilt object, with the declared permission, *after* the `CREATE` rather
/// than before it. Two different facts, and this test is both of them.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_view_rebuild_restates_a_declared_roles_grant_and_the_role_still_reads_it() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_view_reader_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "view-rebuild-grant");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION \
             PASSWORD 'view-rebuild-grant'; CREATE SCHEMA app",
            role.1
        ),
    );
    // The same connection, reissued under the least-privilege role's own
    // credentials rather than the admin's (`doctor_reports_data_and_role_grant_gaps_from_the_declarations`'s
    // idiom for the same reason).
    let login = format!(
        "{} user={} password=view-rebuild-grant",
        connection
            .split_whitespace()
            .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );

    let d = Demo::new("view-rebuild-grant");
    d.table(ONE_COLUMN);
    let view = d.dir.join("schema/v.yml");
    std::fs::write(&view, "view: app.v\ndefinition: SELECT id FROM app.t\n").unwrap();
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!(
            "role: {}\ngrants:\n  schema::app: [usage]\n  app.v: [select]\n",
            role.1
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    // Granted from the start: the ordinary path, unaffected by #248.
    on_server(&login, "SELECT id FROM app.v");

    // The view's return shape changes — every edit is a rebuild on this
    // engine, with no `CREATE OR REPLACE` fallback (ADR-0009 §3) — and the
    // role's declared grant is unchanged by this revision, so before #248 the
    // connected plan would have refused rather than silently drop it.
    std::fs::write(
        &view,
        "view: app.v\ndefinition: SELECT id, id * 2 AS doubled FROM app.t\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(apply_plan(&d, connection, &plan, false));

    // The rebuilt object, read by the role the plan declared — not by the
    // admin connection that ran the apply.
    on_server(&login, "SELECT doubled FROM app.v");
    succeeds(d.run(&["verify", "--db", connection]));
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
    let refused = |o: Output, remedy: &str, has_sql: bool| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(stderr(&o).contains(remedy), "{}", stderr(&o));
        assert_eq!(stderr(&o).contains("ALTER ROLE"), has_sql, "{}", stderr(&o));
    };
    refused(d.run(&planning), "has not been run", true); // NotRunYet
    refuses_connected_check_json(&d, connection, "rename_evidence (PostgreSQL)", "ALTER ROLE");
    on_server(&server, &format!("CREATE ROLE {new}"));
    refused(d.run(&planning), "Resolve the name collision", false); // BothPresent
    on_server(&server, &format!("DROP ROLE {new}"));
    on_server(&server, &format!("ALTER ROLE {old} RENAME TO {parked}"));
    refused(d.run(&planning), "Restore the intended principal", false); // NeitherPresent
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
    refused(
        apply_plan(&d, connection, &plan, false),
        "Resolve the name collision",
        false,
    );
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
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let assert_omitted_modules = |result: &Output, present: bool| {
        let message = stderr(result);
        for object in ["aggregate app.t(bigint)", "view app.bad(int)"] {
            assert_eq!(message.contains(object), present, "{message}");
        }
        assert!(!message.contains("function app.t(integer)"), "{message}");
        // An unrelated cluster role may independently refuse the plan, but
        // connection, snapshot, or planning errors must never count as clean.
        if message.contains("`unmanaged: error`") {
            assert_ne!(code(result), 0, "{message}");
        } else {
            assert_eq!(code(result), 0, "{message}");
        }
    };
    let own = OwnDatabase::new(&server(), "unmanaged-modules");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("unmanaged-modules");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.t(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    // Roles are cluster-wide, so compare only this test's objects rather than
    // assuming an own database means a stable unmanaged inventory.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nunmanaged: error\n",
    )
    .unwrap();
    d.commit();
    let control = d.run(&["plan", "--db", connection]);
    assert_omitted_modules(&control, false);
    let role = Role(
        connection.to_owned(),
        format!("pbps_unmanaged_churn_{}", std::process::id()),
    );
    on_server(connection, &format!("CREATE ROLE {}", role.1));

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
            assert_omitted_modules(&result, true);
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
    assert!(stderr(&restored).contains(&role.1), "{}", stderr(&restored));
    assert_omitted_modules(&restored, false);
    let role_name = role.1.clone();
    drop(role);
    let after_role_drop = d.run(&["plan", "--db", connection]);
    assert!(
        !stderr(&after_role_drop).contains(&role_name),
        "{}",
        stderr(&after_role_drop)
    );
    assert_omitted_modules(&after_role_drop, false);
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

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_rename_refuses_a_recreated_source_even_when_resuming_only_the_closing_read() {
    let own = OwnDatabase::new(&server(), "invariant_recreated_source");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-recreated-source", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.recreate_source() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.t') IS NOT NULL AND to_regclass('app.t') IS NULL THEN
            CREATE TABLE app.t (impostor text);
          END IF;
        END $$;
        CREATE EVENT TRIGGER recreate_source ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.recreate_source();
    "#,
    );
    for resume in [false, true] {
        let mut flags = vec!["--allow", "rename", "--staged"];
        if resume {
            flags.push("--resume");
        }
        let refused = approved_apply(&d, connection, &plan, &flags);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("app.t"), "{}", stderr(&refused));
        let checkpoint = latest_snapshot(connection);
        assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 2);
        assert!(
            checkpoint
                .schema
                .tables
                .contains_key(&"moved.u".parse().unwrap())
        );
        assert!(
            !checkpoint
                .schema
                .tables
                .contains_key(&"app.t".parse().unwrap())
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            0
        );
    }
    // An unrelated unmanaged table does not become drift when the source is repaired.
    on_server(
        connection,
        "DROP EVENT TRIGGER recreate_source; DROP TABLE app.t; CREATE TABLE witness.unrelated (id bigint)",
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    ));
    assert_eq!(
        latest_snapshot(connection).kind,
        pbps_model::StateKind::Apply
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_transactional_rename_rolls_back_when_a_trigger_recreates_its_source() {
    let own = OwnDatabase::new(&server(), "invariant_recreated_transaction");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-recreated-transaction", ONE_COLUMN);
    on_server(
        connection,
        "CREATE SCHEMA moved; CREATE SCHEMA witness; INSERT INTO app.t VALUES (42)",
    );
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, false);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.recreate_source() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.t') IS NOT NULL AND to_regclass('app.t') IS NULL THEN
            CREATE TABLE app.t (impostor text);
          END IF;
        END $$;
        CREATE EVENT TRIGGER recreate_source ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.recreate_source();
    "#,
    );
    let refused = approved_apply(&d, connection, &plan, &["--allow", "rename"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("app.t"), "{}", stderr(&refused));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t WHERE id = 42"),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid IN (to_regclass('moved.t'), to_regclass('moved.u'))"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
        ),
        0
    );
    assert_eq!(
        latest_snapshot(connection).kind,
        pbps_model::StateKind::Failed
    );
    on_server(connection, "DROP EVENT TRIGGER recreate_source");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_rename_refuses_a_recreated_intermediate_name_at_closing_and_resume() {
    let own = OwnDatabase::new(&server(), "invariant_recreated_intermediate");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-recreated-intermediate", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.recreate_intermediate() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.u') IS NOT NULL AND to_regclass('moved.t') IS NULL THEN
            CREATE TABLE moved.t (impostor text);
          END IF;
        END $$;
        CREATE EVENT TRIGGER recreate_intermediate ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.recreate_intermediate();
    "#,
    );
    for resume in [false, true] {
        let mut flags = vec!["--allow", "rename", "--staged"];
        if resume {
            flags.push("--resume");
        }
        let refused = approved_apply(&d, connection, &plan, &flags);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("moved.t"), "{}", stderr(&refused));
        let checkpoint = latest_snapshot(connection);
        assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 2);
        assert!(
            checkpoint
                .schema
                .tables
                .contains_key(&"moved.u".parse().unwrap())
        );
        assert!(
            !checkpoint
                .schema
                .tables
                .contains_key(&"moved.t".parse().unwrap())
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            0
        );
    }
    on_server(
        connection,
        "DROP EVENT TRIGGER recreate_intermediate; DROP TABLE moved.t",
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn unapproved_data_triggers_cannot_use_the_deployers_privileges() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let admin = server();
    let deployer = format!("pbps_trigger_deployer_{}", std::process::id());
    let attacker = format!("pbps_trigger_attacker_{}", std::process::id());
    let _roles = Roles(admin.clone(), vec![deployer.clone(), attacker.clone()]);
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
         CREATE ROLE {attacker} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'"
        ),
    );
    for (operation, before, after, allow) in [
        (
            "INSERT",
            "",
            "data:\n  mode: exact\n  rows:\n    new: {label: New}\n",
            "",
        ),
        (
            "UPDATE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows:\n    old: {label: New}\n",
            "data-update",
        ),
        (
            "DELETE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows: {}\n",
            "data-delete",
        ),
    ] {
        for staged in [false, true] {
            let slug = format!("trigger_{}_{}", operation.to_lowercase(), staged);
            let own = OwnDatabase::new(&admin, &slug);
            let connection = own.connection();
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA app AUTHORIZATION {deployer}; CREATE SCHEMA attacker AUTHORIZATION {attacker}; \
                 GRANT USAGE ON SCHEMA app TO {attacker}; GRANT USAGE ON SCHEMA attacker TO {deployer}; \
                 GRANT CREATE ON SCHEMA public TO {deployer}; \
                 CREATE TABLE public.secret(value text); INSERT INTO public.secret VALUES ('test-only-secret'); \
                 GRANT SELECT ON public.secret TO {deployer}; \
                 CREATE TABLE attacker.leaked(value text); ALTER TABLE attacker.leaked OWNER TO {attacker}; \
                 GRANT INSERT ON attacker.leaked TO {deployer}"
                ),
            );
            let login = |role: &str| {
                format!(
                    "{} user={role} password=trigger-test",
                    connection
                        .split_whitespace()
                        .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            let deployment = login(&deployer);
            let attack = login(&attacker);
            assert_eq!(
                scalar(
                    &deployment,
                    "SELECT count(*) FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication)"
                ),
                0
            );
            assert!(try_on_server(&attack, "SELECT * FROM public.secret").is_err());
            let d = Demo::new(&slug);
            std::fs::write(
                d.dir.join("pbps.yml"),
                format!(
                    "dialect: postgres\nunmanaged: {}\n",
                    if staged { "warn" } else { "ignore" }
                ),
            )
            .unwrap();
            let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
            d.table(&format!("{declared}{before}"));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", &deployment]));
            d.table(&format!("{declared}{after}"));
            let plan = connected_artifact(&d, &deployment, staged);
            // Installed after approval by a role that cannot read the secret.
            on_server(connection, &format!("GRANT TRIGGER ON app.t TO {attacker}"));
            let level = if staged { "STATEMENT" } else { "ROW" };
            on_server(
                &attack,
                &format!(
                    "CREATE FUNCTION attacker.steal() RETURNS trigger LANGUAGE plpgsql SECURITY INVOKER AS \
                 $$BEGIN INSERT INTO attacker.leaked SELECT value FROM public.secret; \
                 IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$; \
                 CREATE TRIGGER steal BEFORE {operation} ON app.t FOR EACH {level} EXECUTE FUNCTION attacker.steal()"
                ),
            );
            if operation == "INSERT" && !staged {
                use pbps_dialect::Dialect;
                // Merely recording this trigger cannot make its independently
                // replaceable invoker function safe to run as the deployer.
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &deployment).await.unwrap();
                    let recorded = pbps_pg::catalog::introspect(&mut conn).await.unwrap().schema;
                    let dialect = pbps_pg::Postgres::new();
                    conn.begin(dialect.transaction_framing()).await.unwrap();
                    let refused = pbps_pg::data_triggers::prepare(&mut conn, &[pbps_dialect::RowWrite {
                        table: pbps_model::TableName::new("app", "t"), operation: pbps_dialect::RowOperation::Insert,
                    }], &recorded, &Default::default()).await;
                    assert!(refused.is_err(), "recording a lower-privileged owner's invoker trigger must not authorize it");
                    conn.rollback(dialect.transaction_framing()).await.unwrap();
                });
            }
            let blocked_plan = d.run(&["plan", "--db", &deployment]);
            assert_eq!(code(&blocked_plan), 1);
            assert!(
                stderr(&blocked_plan).contains("unsafe data trigger"),
                "{}",
                stderr(&blocked_plan)
            );
            let mut extra = Vec::new();
            if staged {
                extra.push("--staged");
            }
            if !allow.is_empty() {
                extra.extend(["--allow", allow]);
            }
            let refused = approved_apply(&d, &deployment, &plan, &extra);
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM attacker.leaked"),
                0,
                "{operation} staged={staged} leaked despite approval boundary: {}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert_eq!(
                code(&refused),
                1,
                "{}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert!(
                stderr(&refused).contains("trigger") && stderr(&refused).contains("steal"),
                "{}",
                stderr(&refused)
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
                ),
                0
            );
            on_server(&deployment, "DROP TRIGGER steal ON app.t");
            let other_event = if operation == "INSERT" {
                "UPDATE"
            } else {
                "INSERT"
            };
            on_server(
                &deployment,
                &format!(
                    "CREATE TRIGGER disabled BEFORE {operation} ON app.t FOR EACH ROW EXECUTE FUNCTION attacker.steal();                  ALTER TABLE app.t DISABLE TRIGGER disabled;                  CREATE TRIGGER another_event BEFORE {other_event} ON app.t FOR EACH ROW EXECUTE FUNCTION attacker.steal();                  CREATE TABLE app.unrelated (id integer);                  CREATE TRIGGER unrelated BEFORE {operation} ON app.unrelated FOR EACH ROW EXECUTE FUNCTION attacker.steal()"
                ),
            );
            succeeds(approved_apply(&d, &deployment, &plan, &extra));
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM attacker.leaked"),
                0
            );
            succeeds(d.run(&["verify", "--db", &deployment]));
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn recorded_triggers_run_but_their_names_cannot_authorize_replacements() {
    let own = OwnDatabase::new(&server(), "recorded_triggers");
    let connection = own.connection();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    let d = bootstrapped_demo(connection, "recorded-triggers", declared);
    on_server(
        connection,
        "CREATE TABLE app.calls (value integer); \
        CREATE FUNCTION app.record_call() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN INSERT INTO app.calls VALUES (1); RETURN NEW; END$$; \
        CREATE TRIGGER record_call BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.record_call()",
    );
    succeeds(d.run(&["pull", "--db", connection, "--force"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt the intended trigger",
    ]));
    d.table(&format!(
        "{declared}data:\n  mode: exact\n  rows:\n    first: {{label: First}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.calls"), 1);
    succeeds(d.run(&["verify", "--db", connection]));
    d.table(&format!("{declared}data:\n  mode: exact\n  rows:\n    first: {{label: First}}\n    second: {{label: Second}}\n"));
    let plan = connected_artifact(&d, connection, false);
    on_server(
        connection,
        "CREATE FUNCTION app.replacement() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE OR REPLACE TRIGGER record_call BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.replacement()",
    );
    let refused = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE code = 'second'"
        ),
        0
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.calls"), 1);
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_trigger_guard_holds_off_replacement_until_the_write_finishes() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "trigger_lock");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (id integer); \
        CREATE FUNCTION app.trusted() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE FUNCTION app.replacement() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE TRIGGER guarded BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.trusted()",
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut writer = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let mut other = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
        let dialect = pbps_pg::Postgres::new();
        let mut write = RowWrite { table: pbps_model::TableName::new("app", "t"), operation: RowOperation::Insert };
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let guard = pbps_pg::data_triggers::prepare(&mut writer, std::slice::from_ref(&write), &baseline, &Default::default()).await.unwrap();
        other.execute("SET lock_timeout = '100ms'").await.unwrap();
        let replace = "CREATE OR REPLACE TRIGGER guarded BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.replacement()";
        let blocked = other.execute(replace).await.unwrap_err();
        assert!(matches!(blocked, pbps_db::DbError::Driver { ref code, .. } if code.as_deref() == Some("55P03")), "{blocked}");
            writer.execute("ALTER TABLE app.t RENAME TO renamed").await.unwrap();
            write.table = pbps_model::TableName::new("app", "renamed");
        pbps_pg::data_triggers::check(&mut writer, &write, &guard).await.unwrap();
        writer.execute("INSERT INTO app.renamed VALUES (1)").await.unwrap();
        writer.commit(dialect.transaction_framing()).await.unwrap();
        // After release the same replacement succeeds, proving the first
        // failure came from the guard's lock, not invalid DDL or permissions.
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
            other.execute(&replace.replace("ON app.t", "ON app.renamed")).await.unwrap();
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let refused = pbps_pg::data_triggers::prepare(&mut writer, &[write], &baseline, &Default::default()).await;
        assert!(refused.is_err());
        writer.rollback(dialect.transaction_framing()).await.unwrap();
    });
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn bootstrap_checks_triggers_that_arrive_with_a_created_table() {
    let own = OwnDatabase::new(&server(), "bootstrap_trigger");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE public.marker (value integer); \
        CREATE FUNCTION public.row_hook() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN INSERT INTO public.marker VALUES (1); RETURN NEW; END$$; \
        CREATE FUNCTION public.attach_hook() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
        BEGIN \
          IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() d JOIN pg_class c ON c.oid = d.objid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'app' AND c.relname = 't') THEN \
            EXECUTE 'CREATE TRIGGER unexpected BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION public.row_hook()'; \
          END IF; \
        END$$; \
        CREATE EVENT TRIGGER attach_hook ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.attach_hook()",
    );
    let d = Demo::new("bootstrap-trigger");
    d.table("table: app.t\ncolumns:\n  code: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\ndata:\n  mode: exact\n  rows:\n    first: {}\n");
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("unexpected"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.marker"), 0);
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('app.t')"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'bootstrap'"
        ),
        0
    );
    on_server(connection, "DROP EVENT TRIGGER attach_hook");
    succeeds(d.run(&["bootstrap", "--db", connection]));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn update_of_triggers_only_guard_columns_the_statement_can_fire() {
    for staged in [false, true] {
        for level in ["ROW", "STATEMENT"] {
            let own = OwnDatabase::new(&server(), &format!("update_of_{staged}_{level}"));
            let connection = own.connection();
            let declared = "table: app.t\ncolumns:\n  code: {type: text, nullable: false}\n  label: {type: text, nullable: false}\n  audit: {type: text, nullable: true}\nprimary_key: {name: pk_t, columns: [code]}\ndata:\n  mode: exact\n  rows:\n";
            let d = bootstrapped_demo(
                connection,
                "update-of",
                &format!("{declared}    first: {{label: Old}}\n"),
            );
            on_server(
                connection,
                &format!(
                    "CREATE TABLE public.fired (value integer); \
                 CREATE FUNCTION public.mark() RETURNS trigger LANGUAGE plpgsql AS \
                 $$BEGIN INSERT INTO public.fired VALUES (1); RETURN NEW; END$$; \
                 CREATE TRIGGER untouched BEFORE UPDATE OF audit ON app.t FOR EACH {level} EXECUTE FUNCTION public.mark(); \
                 BEGIN; UPDATE app.t SET label = 'Measured'; SELECT 1; COMMIT;"
                ),
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
            on_server(
                connection,
                "UPDATE app.t SET label = 'Old'; UPDATE app.t SET audit = NULL",
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 1);
            on_server(connection, "TRUNCATE public.fired");
            d.table(&format!("{declared}    first: {{label: New}}\n"));
            let plan = connected_artifact(&d, connection, staged);
            let extra = if staged {
                vec!["--staged", "--allow", "data-update"]
            } else {
                vec!["--allow", "data-update"]
            };
            succeeds(approved_apply(&d, connection, &plan, &extra));
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
            succeeds(d.run(&["verify", "--db", connection]));
            d.table(&format!("{declared}    first: {{label: Next}}\n"));
            let plan = connected_artifact(&d, connection, staged);
            on_server(
                connection,
                &format!(
                    "CREATE OR REPLACE TRIGGER untouched BEFORE UPDATE OF audit, label ON app.t FOR EACH {level} EXECUTE FUNCTION public.mark()"
                ),
            );
            let refused = approved_apply(&d, connection, &plan, &extra);
            assert_eq!(
                code(&refused),
                1,
                "{}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert!(
                stderr(&refused).contains("unsafe data trigger"),
                "{}",
                stderr(&refused)
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM app.t WHERE label = 'New'"),
                1
            );
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn update_of_generated_columns_follows_their_source_columns() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "update_of_generated");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (label text, audit text, size integer GENERATED ALWAYS AS (length(label)) STORED); \
         INSERT INTO app.t (label) VALUES ('Old'); CREATE TABLE public.fired (value integer); \
         CREATE FUNCTION public.mark() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.fired VALUES (1); RETURN NEW; END$$; \
         CREATE TRIGGER derived AFTER UPDATE OF size ON app.t FOR EACH ROW EXECUTE FUNCTION public.mark(); \
         UPDATE app.t SET audit = 'untouched'",
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
    on_server(connection, "UPDATE app.t SET label = 'Measured'");
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 1);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let dialect = pbps_pg::Postgres::new();
        for (column, allowed) in [("audit", true), ("label", false)] {
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "t"),
                operation: RowOperation::Update {
                    columns: [column.to_owned()].into(),
                },
            };
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await;
            assert_eq!(
                checked.is_ok(),
                allowed,
                "column {column}: {:?}",
                checked.err()
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        }
        conn.execute("CREATE FUNCTION public.noop() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; CREATE TRIGGER before_update BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION public.noop(); ALTER TABLE app.t DISABLE TRIGGER before_update; TRUNCATE public.fired; UPDATE app.t SET audit = 'again'").await.unwrap();
        let rows = conn.query("SELECT count(*)::int8 AS n FROM public.fired").await.unwrap();
        assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(1));
        let write = RowWrite {
            table: pbps_model::TableName::new("app", "t"),
            operation: RowOperation::Update { columns: ["audit".to_owned()].into() },
        };
        conn.begin(dialect.transaction_framing()).await.unwrap();
        let checked = pbps_pg::data_triggers::prepare(&mut conn, &[write], &Default::default(), &Default::default()).await;
        assert!(checked.is_err(), "a disabled BEFORE ROW trigger still widens generated-column updates");
        conn.rollback(dialect.transaction_framing()).await.unwrap();
    });
}

/// Cluster-wide roles outlive the database that uses them, so a test holding
/// both drops the database first: `DROP ROLE` fails while the role still owns
/// an object anywhere in the cluster.
struct ClusterRoles {
    server: String,
    names: Vec<String>,
}

impl Drop for ClusterRoles {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = try_on_server(&self.server, &format!("DROP ROLE IF EXISTS {name}"));
        }
    }
}

/// The membership topology DECISIONS 450 is about: `owner` owns the trigger
/// function and can act as the deployer, and `inheritor` and `distant` are the
/// roles each case hands the owner's rights to. Fields drop in declaration
/// order, so the database goes before the roles that own objects inside it.
struct Topology {
    own: OwnDatabase,
    _roles: ClusterRoles,
    deployer: String,
    owner: String,
    inheritor: String,
    distant: String,
    deployment: String,
}

impl Topology {
    fn connection(&self) -> &str {
        self.own.connection()
    }
}

/// A non-superuser deployer, a separate function owner that can `SET ROLE` to
/// it, and two roles that hold no membership yet. The trigger function is
/// `hook.audit`, owned by `owner` and invoker-rights like any plpgsql trigger
/// function, so its body runs with whatever privileges the deployer brings.
fn inherited_owner_topology(admin: &str, slug: &str) -> Topology {
    let pid = std::process::id();
    let deployer = format!("pbps_{slug}_deployer_{pid}");
    let owner = format!("pbps_{slug}_owner_{pid}");
    let inheritor = format!("pbps_{slug}_inheritor_{pid}");
    let distant = format!("pbps_{slug}_distant_{pid}");
    let roles = ClusterRoles {
        server: admin.to_owned(),
        names: vec![
            distant.clone(),
            inheritor.clone(),
            owner.clone(),
            deployer.clone(),
        ],
    };
    on_server(
        admin,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
             CREATE ROLE {owner} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
             CREATE ROLE {inheritor} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
             CREATE ROLE {distant} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
             GRANT {deployer} TO {owner} WITH INHERIT FALSE, SET TRUE"
        ),
    );
    let own = OwnDatabase::new(admin, slug);
    on_server(
        own.connection(),
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; CREATE SCHEMA hook AUTHORIZATION {owner}; \
             GRANT USAGE ON SCHEMA hook TO {deployer}; GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE hook.calls (value integer); ALTER TABLE hook.calls OWNER TO {owner}; \
             GRANT INSERT, SELECT ON hook.calls TO {deployer}; \
             CREATE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO hook.calls VALUES (1); \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$; \
             ALTER FUNCTION hook.audit() OWNER TO {owner}"
        ),
    );
    let deployment = as_role(own.connection(), &deployer);
    assert_eq!(
        scalar(
            &deployment,
            "SELECT count(*) FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication)"
        ),
        0
    );
    Topology {
        own,
        _roles: roles,
        deployer,
        owner,
        inheritor,
        distant,
        deployment,
    }
}

/// The same server, reached as another role: the libpq keyword form the suite
/// is configured with, with its `user` and `password` words replaced.
fn as_role(connection: &str, role: &str) -> String {
    let kept: Vec<&str> = connection
        .split_whitespace()
        .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
        .collect();
    format!("{} user={role} password=trigger-test", kept.join(" "))
}

/// A trigger on `app.t` whose function the deployer only executes, recorded as
/// the tool would record it, so nothing but ownership rights can refuse it.
fn recorded_audit_trigger(topology: &Topology, event: &str) -> pbps_model::Schema {
    on_server(
        &topology.deployment,
        &format!(
            "CREATE TABLE app.t (id integer); \
             CREATE TRIGGER audit BEFORE {event} ON app.t FOR EACH ROW EXECUTE FUNCTION hook.audit()"
        ),
    );
    let recorded = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &topology.deployment)
                .await
                .unwrap();
            pbps_pg::catalog::introspect(&mut conn)
                .await
                .unwrap()
                .schema
        });
    let id = pbps_model::ModuleId::Trigger {
        on: pbps_model::TableName::new("app", "t"),
        name: "audit".to_owned(),
    };
    assert!(
        recorded.modules.contains_key(&id),
        "the trigger has to be recorded, or the recording check is what refuses"
    );
    recorded
}

/// DECISIONS 450. A role that inherits the function owner's rights can replace
/// the body — the OID and the recorded trigger definition do not move — so the
/// direct owner's `SET` path is not the question the guard has to answer.
///
/// The cases are ordered so that each one differs from the last in one grant,
/// and the trusted ones are the control: a predicate that simply refused every
/// membership would pass the refusals and fail here.
fn effective_ownership_decides_trust(admin: &str, slug: &str) {
    let topology = inherited_owner_topology(admin, slug);
    let recorded = recorded_audit_trigger(&topology, "INSERT");
    let Topology {
        deployer,
        owner,
        inheritor,
        distant,
        ..
    } = &topology;
    let cases: [(&str, String, bool); 6] = [
        (
            "a direct owner that can act as the deployer is trusted",
            String::new(),
            true,
        ),
        (
            "a role inheriting the owner's rights without a SET path is not",
            format!("GRANT {owner} TO {inheritor} WITH INHERIT TRUE, SET FALSE"),
            false,
        ),
        (
            "an inheritor that can itself act as the deployer is trusted",
            format!("GRANT {deployer} TO {inheritor} WITH INHERIT FALSE, SET TRUE"),
            true,
        ),
        (
            "disabled inheritance confers no ownership rights",
            format!(
                "REVOKE {deployer} FROM {inheritor}; REVOKE {owner} FROM {inheritor}; \
                 GRANT {owner} TO {inheritor} WITH INHERIT FALSE, SET FALSE"
            ),
            true,
        ),
        (
            "inheritance reaches through a trusted inheritor to an untrusted one",
            format!(
                "REVOKE {owner} FROM {inheritor}; \
                 GRANT {owner} TO {inheritor} WITH INHERIT TRUE, SET FALSE; \
                 GRANT {deployer} TO {inheritor} WITH INHERIT FALSE, SET TRUE; \
                 GRANT {inheritor} TO {distant} WITH INHERIT TRUE, SET FALSE"
            ),
            false,
        ),
        (
            "the direct owner still needs a SET path of its own",
            format!(
                "REVOKE {inheritor} FROM {distant}; REVOKE {owner} FROM {inheritor}; \
                 REVOKE {deployer} FROM {inheritor}; REVOKE {deployer} FROM {owner}"
            ),
            false,
        ),
    ];
    let connection = topology.connection().to_owned();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use pbps_dialect::Dialect;
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &topology.deployment)
                .await
                .unwrap();
            // Inside this runtime rather than through `on_server`: a nested
            // `block_on` panics, and the grants have to land between the checks.
            let mut admin = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &connection)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            for (property, grant, trusted) in cases {
                if !grant.is_empty() {
                    admin.execute(&grant).await.unwrap();
                }
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    &[pbps_dialect::RowWrite {
                        table: pbps_model::TableName::new("app", "t"),
                        operation: pbps_dialect::RowOperation::Insert,
                    }],
                    &recorded,
                    &Default::default(),
                )
                .await;
                let refusal = checked.as_ref().err().map(|e| e.to_string());
                conn.rollback(dialect.transaction_framing()).await.unwrap();
                assert_eq!(checked.is_ok(), trusted, "{property}: {refusal:?}");
                if let Some(message) = refusal {
                    assert!(message.contains("unsafe data trigger"), "{message}");
                }
            }
        });
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn effective_function_ownership_not_the_direct_owner_decides_trigger_trust() {
    effective_ownership_decides_trust(&server(), "effective_owner");
}

/// The `INHERIT`/`SET` grant options arrived together in PostgreSQL 16, and the
/// predicate has to mean the same thing on the oldest engine that has them: a
/// release where it silently read as "always trusted" would leave the hole open
/// on every supported server but one.
#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn effective_function_ownership_decides_trigger_trust_before_postgres_17() {
    let server = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    on_server(
        &server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer >= 170000 THEN RAISE EXCEPTION 'this regression needs a pre-17 server'; END IF; END $$",
    );
    effective_ownership_decides_trust(&server, "effective_owner_old");
}

/// Planning is not the last word: a membership granted after the plan was
/// approved is caught by the re-read the writer does under its own lock. The
/// lock cannot help here — `GRANT` does not touch the guarded table — which is
/// why the check is repeated rather than trusted from planning time.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_membership_granted_after_planning_is_refused_before_the_row_write() {
    let topology = inherited_owner_topology(&server(), "late_membership");
    let recorded = recorded_audit_trigger(&topology, "INSERT");
    let connection = topology.connection().to_owned();
    let grant = format!(
        "GRANT {} TO {} WITH INHERIT TRUE, SET FALSE",
        topology.owner, topology.inheritor
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use pbps_dialect::Dialect;
            let mut writer =
                pbps_db::Conn::connect(pbps_db::Driver::Postgres, &topology.deployment)
                    .await
                    .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = pbps_dialect::RowWrite {
                table: pbps_model::TableName::new("app", "t"),
                operation: pbps_dialect::RowOperation::Insert,
            };
            writer.begin(dialect.transaction_framing()).await.unwrap();
            let guard = pbps_pg::data_triggers::prepare(
                &mut writer,
                std::slice::from_ref(&write),
                &recorded,
                &Default::default(),
            )
            .await
            .expect("the topology is trusted while planning");
            // From another session, so the guarded table's lock is the one thing
            // that could have stopped it, and it does not.
            pbps_db::Conn::connect(pbps_db::Driver::Postgres, &connection)
                .await
                .unwrap()
                .execute(&grant)
                .await
                .unwrap();
            let refused = pbps_pg::data_triggers::check(&mut writer, &write, &guard).await;
            writer
                .rollback(dialect.transaction_framing())
                .await
                .unwrap();
            let message = refused.expect_err("the write must not run").to_string();
            assert!(message.contains("unsafe data trigger"), "{message}");
        });
}

/// The path a user types, on the topology of DECISIONS 450. The trigger is
/// installed and adopted while nothing can replace its function, so the
/// recording check and the direct owner's `SET` path both pass; only then does a
/// role that inherits the owner's rights take the body over. Every row DML kind
/// has to refuse, and refuse before the body copies a secret that only the
/// deployment role can read.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn an_inherited_trigger_function_owner_cannot_use_the_deployers_privileges() {
    let admin = server();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    for (operation, before, after, allow) in [
        (
            "INSERT",
            "",
            "data:\n  mode: exact\n  rows:\n    new: {label: New}\n",
            "",
        ),
        (
            "UPDATE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows:\n    old: {label: New}\n",
            "data-update",
        ),
        (
            "DELETE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows: {}\n",
            "data-delete",
        ),
    ] {
        let slug = format!("inherited_{}", operation.to_lowercase());
        let topology = inherited_owner_topology(&admin, &slug);
        let connection = topology.connection().to_owned();
        let deployment = topology.deployment.clone();
        let d = Demo::new(&slug);
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\nunmanaged: ignore\n",
        )
        .unwrap();
        d.table(declared);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &deployment]));
        // The trigger arrives the way an operator installs one, and is adopted
        // from the engine rather than written by hand: the guard compares the
        // deparsed definition, and only a pulled one is that text exactly.
        on_server(
            &deployment,
            &format!(
                "CREATE TRIGGER audit BEFORE {operation} ON app.t FOR EACH ROW EXECUTE FUNCTION hook.audit()"
            ),
        );
        // Pulled into a project of its own, and only the trigger is carried
        // across. The function stays unmanaged: a managed body would make the
        // replacement read as ordinary drift, and the refusal under test would
        // never be the thing that stopped the write. Deleting the pulled files
        // instead would ask for drop intent, which is a different test.
        let pulled = Demo::new(&format!("{slug}-pull"));
        succeeds(pulled.run(&["pull", "--db", &deployment]));
        let module = std::fs::read_dir(pulled.dir.join("schema"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let body = std::fs::read_to_string(&path).ok()?;
                body.lines()
                    .any(|line| line.starts_with("trigger:"))
                    .then_some(body)
            })
            .next()
            .expect("the engine's own spelling of the trigger");
        std::fs::write(d.dir.join("schema/audit.yml"), module).unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            &deployment,
            "--reason",
            "adopt the audit trigger",
        ]));
        succeeds(d.run(&["verify", "--db", &deployment]));
        // Built after the adoption, so none of it is managed or recorded: a
        // secret the deployer alone can read, and a table only the inheritor
        // owns to read it back out of.
        let Topology {
            deployer,
            owner,
            inheritor,
            ..
        } = &topology;
        on_server(
            &connection,
            &format!(
                "CREATE SCHEMA secrets; CREATE TABLE secrets.vault (value text); \
                 INSERT INTO secrets.vault VALUES ('test-only-secret'); \
                 GRANT USAGE ON SCHEMA secrets TO {deployer}; GRANT SELECT ON secrets.vault TO {deployer}; \
                 CREATE SCHEMA leak AUTHORIZATION {inheritor}; CREATE TABLE leak.copied (value text); \
                 ALTER TABLE leak.copied OWNER TO {inheritor}; \
                 GRANT USAGE ON SCHEMA leak TO {deployer}; GRANT INSERT ON leak.copied TO {deployer}"
            ),
        );
        let attack = as_role(&connection, inheritor);
        assert!(try_on_server(&attack, "SELECT * FROM secrets.vault").is_err());
        if !before.is_empty() {
            d.table(&format!("{declared}{before}"));
            let seed = connected_artifact(&d, &deployment, false);
            succeeds(approved_apply(&d, &deployment, &seed, &[]));
        }
        d.table(&format!("{declared}{after}"));
        let plan = connected_artifact(&d, &deployment, false);
        let applies = scalar(
            &connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'",
        );
        // Approved, recorded, unchanged — and now owned in effect by a role
        // that cannot act as the deployer.
        on_server(
            &connection,
            &format!("GRANT {owner} TO {inheritor} WITH INHERIT TRUE, SET FALSE"),
        );
        on_server(
            &attack,
            "CREATE OR REPLACE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO leak.copied SELECT value FROM secrets.vault; \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$",
        );
        let blocked = d.run(&["plan", "--db", &deployment]);
        assert_eq!(
            code(&blocked),
            1,
            "{}{}",
            stdout(&blocked),
            stderr(&blocked)
        );
        assert!(
            stderr(&blocked).contains("unsafe data trigger"),
            "{}",
            stderr(&blocked)
        );
        let mut extra = Vec::new();
        if !allow.is_empty() {
            extra.extend(["--allow", allow]);
        }
        let refused = approved_apply(&d, &deployment, &plan, &extra);
        assert_eq!(
            scalar(&connection, "SELECT count(*) FROM leak.copied"),
            0,
            "{operation} copied the secret: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("unsafe data trigger") && stderr(&refused).contains("audit"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                &connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            applies,
            "a refused write must not record an apply"
        );
        // The control: the same plan, the same recorded trigger, once the
        // inherited rights are gone and the legitimate body is back. Replacing
        // a function does not move its ownership, so `owner` still owns it.
        on_server(&connection, &format!("REVOKE {owner} FROM {inheritor}"));
        on_server(
            &connection,
            "CREATE OR REPLACE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO hook.calls VALUES (1); \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$",
        );
        succeeds(approved_apply(&d, &deployment, &plan, &extra));
        assert_eq!(scalar(&connection, "SELECT count(*) FROM leak.copied"), 0);
        assert_eq!(
            scalar(&connection, "SELECT count(*) FROM hook.calls"),
            1,
            "the trusted trigger has to have run, or nothing was proved"
        );
        succeeds(d.run(&["verify", "--db", &deployment]));
    }
}

/// A row operation writes more tables than the one it names: PostgreSQL runs a
/// referential action on the referencing side, and a trigger there runs under
/// the deployment role just as one on the named table does (DECISIONS 451).
///
/// The delete cases carry a statement-level trigger and no referencing row on
/// purpose. A row that still points at the declared row makes the existing
/// delete preflight refuse the plan long before the guard, but the action's own
/// statement runs whether or not it matches anything — measured on 18.6 — so a
/// statement trigger on the referencing table is the delete's live reach.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn referential_actions_cannot_reach_an_unapproved_trigger() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let admin = server();
    let deployer = format!("pbps_fk_deployer_{}", std::process::id());
    let attacker = format!("pbps_fk_attacker_{}", std::process::id());
    let _roles = Roles(admin.clone(), vec![deployer.clone(), attacker.clone()]);
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
         CREATE ROLE {attacker} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'"
        ),
    );
    // `chain` puts the trigger two foreign keys away: the first action's own
    // write is what produces the second, which only a recursive closure finds.
    for (operation, action, level, staged, chain) in [
        ("UPDATE", "CASCADE", "ROW", false, false),
        ("UPDATE", "SET NULL", "ROW", false, false),
        ("UPDATE", "SET DEFAULT", "ROW", false, false),
        ("UPDATE", "CASCADE", "ROW", false, true),
        ("DELETE", "CASCADE", "STATEMENT", false, false),
        ("DELETE", "SET NULL", "STATEMENT", false, false),
        ("UPDATE", "CASCADE", "STATEMENT", true, false),
        ("DELETE", "CASCADE", "STATEMENT", true, true),
    ] {
        let slug = format!(
            "fk_{}_{}_{level}_{staged}_{chain}",
            operation.to_lowercase(),
            action.to_lowercase().replace(' ', "_")
        )
        .to_lowercase();
        let own = OwnDatabase::new(&admin, &slug);
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA app AUTHORIZATION {deployer}; CREATE SCHEMA attacker AUTHORIZATION {attacker}; \
             GRANT USAGE ON SCHEMA app TO {attacker}; GRANT USAGE ON SCHEMA attacker TO {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE public.secret(value text); INSERT INTO public.secret VALUES ('test-only-secret'); \
             GRANT SELECT ON public.secret TO {deployer}; \
             CREATE TABLE attacker.leaked(value text); ALTER TABLE attacker.leaked OWNER TO {attacker}; \
             GRANT INSERT ON attacker.leaked TO {deployer}"
            ),
        );
        let login = |role: &str| {
            format!(
                "{} user={role} password=trigger-test",
                connection
                    .split_whitespace()
                    .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        let deployment = login(&deployer);
        let attack = login(&attacker);
        assert!(try_on_server(&attack, "SELECT * FROM public.secret").is_err());
        let d = Demo::new(&slug);
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nunmanaged: {}\n",
                if staged { "warn" } else { "ignore" }
            ),
        )
        .unwrap();
        // The referenced key is a UNIQUE column and not the primary key: a
        // declared row is identified by its key, so only another column of it
        // can change under an UPDATE at all.
        let declared = "table: app.t\ncolumns:\n  code: {type: text, nullable: false}\n  ukey: {type: text, nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\nunique:\n  uq_t_ukey: [ukey]\ndata:\n  mode: exact\n  rows:\n    fallback: {ukey: fallback, label: Fallback}\n";
        d.table(&format!(
            "{declared}    first: {{ukey: k1, label: First}}\n"
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &deployment]));
        // Unmanaged and owned by the deployer, which is what makes an invoker
        // trigger on it run with the deployer's privileges.
        let (on_update, on_delete) = if operation == "UPDATE" {
            (action, "NO ACTION")
        } else {
            ("NO ACTION", action)
        };
        // A delete case leaves the declared row unreferenced: see the note on
        // this test. Every other row here belongs to the control below.
        let held = if operation == "UPDATE" {
            "k1"
        } else {
            "fallback"
        };
        on_server(
            &deployment,
            &format!(
                "CREATE TABLE app.child(id integer PRIMARY KEY, ukey text DEFAULT 'fallback' UNIQUE, note text, \
             CONSTRAINT fk_child FOREIGN KEY (ukey) REFERENCES app.t(ukey) ON UPDATE {on_update} ON DELETE {on_delete}); \
             CREATE TABLE app.grandchild(id integer PRIMARY KEY, ukey text, \
             CONSTRAINT fk_grandchild FOREIGN KEY (ukey) REFERENCES app.child(ukey) ON UPDATE CASCADE ON DELETE CASCADE); \
             INSERT INTO app.child VALUES (1, '{held}', 'note'); \
             INSERT INTO app.grandchild VALUES (1, '{held}')"
            ),
        );
        let reached = if chain { "app.grandchild" } else { "app.child" };
        let fires = if operation == "DELETE" && action == "CASCADE" {
            "DELETE"
        } else {
            "UPDATE"
        };
        // The control: the same action, on rows of its own, with a trigger the
        // deployer owns. It proves the engine really writes `reached` here, so
        // a later "no leak" cannot pass because nothing fired at all.
        on_server(
            &deployment,
            &format!(
                "CREATE TABLE public.fired(value integer); \
             CREATE FUNCTION public.mark() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO public.fired VALUES (1); RETURN NULL; END$$; \
             CREATE TRIGGER control AFTER {fires} ON {reached} FOR EACH {level} EXECUTE FUNCTION public.mark(); \
             INSERT INTO app.t VALUES ('control', 'c1', 'Control'); \
             INSERT INTO app.child VALUES (99, 'c1', 'control'); \
             INSERT INTO app.grandchild VALUES (99, 'c1')"
            ),
        );
        on_server(
            &deployment,
            &if operation == "UPDATE" {
                "UPDATE app.t SET ukey = 'c2' WHERE code = 'control'".to_owned()
            } else {
                "DELETE FROM app.t WHERE code = 'control'".to_owned()
            },
        );
        assert!(
            scalar(connection, "SELECT count(*) FROM public.fired") > 0,
            "{slug}: the referential action never reached {reached}"
        );
        on_server(
            &deployment,
            &format!(
                "DROP TRIGGER control ON {reached}; TRUNCATE public.fired; \
             DELETE FROM app.grandchild WHERE id = 99; DELETE FROM app.child WHERE id = 99; \
             DELETE FROM app.t WHERE code = 'control'"
            ),
        );
        let after = if operation == "UPDATE" {
            format!("{declared}    first: {{ukey: k2, label: First}}\n")
        } else {
            declared.to_owned()
        };
        d.table(&after);
        let plan = connected_artifact(&d, &deployment, staged);
        // Installed after approval by a role that cannot read the secret and
        // owns neither the table it writes through nor the one it lands on.
        on_server(
            connection,
            &format!("GRANT TRIGGER ON {reached} TO {attacker}"),
        );
        on_server(
            &attack,
            &format!(
                "CREATE FUNCTION attacker.steal() RETURNS trigger LANGUAGE plpgsql SECURITY INVOKER AS \
             $$BEGIN INSERT INTO attacker.leaked SELECT value FROM public.secret; \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$; \
             CREATE TRIGGER steal BEFORE {fires} ON {reached} FOR EACH {level} EXECUTE FUNCTION attacker.steal()"
            ),
        );
        let blocked = d.run(&["plan", "--db", &deployment]);
        assert_eq!(
            code(&blocked),
            1,
            "{}{}",
            stdout(&blocked),
            stderr(&blocked)
        );
        assert!(
            stderr(&blocked).contains("unsafe data trigger") && stderr(&blocked).contains(reached),
            "{slug}: {}",
            stderr(&blocked)
        );
        let mut extra = Vec::new();
        if staged {
            extra.push("--staged");
        }
        extra.extend([
            "--allow",
            if operation == "UPDATE" {
                "data-update"
            } else {
                "data-delete"
            },
        ]);
        let refused = approved_apply(&d, &deployment, &plan, &extra);
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM attacker.leaked"),
            0,
            "{slug} leaked despite the approval boundary: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("unsafe data trigger") && stderr(&refused).contains(reached),
            "{slug}: {}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            0,
            "{slug}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.t WHERE code = 'first' AND ukey = 'k1'"
            ),
            1,
            "{slug}: the refused write must leave the named table alone"
        );
        // The same plan with the trigger gone: the row operation and every
        // action it produces are ordinary work the deployer is allowed to do.
        on_server(&deployment, &format!("DROP TRIGGER steal ON {reached}"));
        succeeds(approved_apply(&d, &deployment, &plan, &extra));
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM attacker.leaked"),
            0,
            "{slug}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.t WHERE code = 'first'"
            ),
            i64::from(operation == "UPDATE"),
            "{slug}: the row operation itself went through"
        );
        if operation == "UPDATE" {
            let landed = match action {
                "CASCADE" => "SELECT count(*) FROM app.child WHERE ukey = 'k2'",
                "SET NULL" => "SELECT count(*) FROM app.child WHERE ukey IS NULL",
                _ => "SELECT count(*) FROM app.child WHERE ukey = 'fallback'",
            };
            assert_eq!(scalar(connection, landed), 1, "{slug}: the action ran");
        }
        succeeds(d.run(&["verify", "--db", &deployment]));
    }
}

/// The closure is the set of tables a row operation makes PostgreSQL write, and
/// no more: an action that writes nothing, an event the action does not raise
/// and a column it does not set all leave their triggers alone. A guard that
/// refused those would refuse valid plans, which is the other way to be wrong.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_write_closure_follows_writing_actions_and_stops_at_the_others() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "fk_closure");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE FUNCTION public.hook() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NULL; END$$",
    );
    let update = |columns: &[&str]| RowOperation::Update {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
    };
    // Every case declares `{s}.p`, writes it, and puts one trigger somewhere a
    // referential action may or may not reach. `p` always keeps the same shape
    // so the write is the only thing that changes.
    let head = "CREATE TABLE {s}.p(code text PRIMARY KEY, ukey text UNIQUE, other text UNIQUE, label text, \
                UNIQUE (ukey, other)); ";
    let cases: Vec<(&str, String, RowOperation, bool)> = vec![
        (
            "no_action",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE NO ACTION); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "restrict",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE RESTRICT); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "cascade",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "set_default",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE SET DEFAULT); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "delete_action_under_an_update",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE NO ACTION ON DELETE CASCADE); \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "update_action_under_a_delete",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE ON DELETE NO ACTION); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            false,
        ),
        (
            "insert_raises_no_action",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE ON DELETE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Insert,
            false,
        ),
        (
            "untouched_referenced_column",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["label"]),
            false,
        ),
        (
            "another_key_of_the_same_table",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, other text REFERENCES {{s}}.p(other) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "an_event_the_action_does_not_raise",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_child_column_the_action_does_not_set",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE, note text); \
                    CREATE TRIGGER hook AFTER UPDATE OF note ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "the_child_column_the_action_sets",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE, note text); \
                    CREATE TRIGGER hook AFTER UPDATE OF ukey ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "outside_an_on_delete_set_null_column_list",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text, other text, note text, \
                    FOREIGN KEY (ukey, other) REFERENCES {{s}}.p(ukey, other) ON DELETE SET NULL (other)); \
                    CREATE TRIGGER hook AFTER UPDATE OF ukey ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            false,
        ),
        (
            "inside_an_on_delete_set_null_column_list",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text, other text, note text, \
                    FOREIGN KEY (ukey, other) REFERENCES {{s}}.p(ukey, other) ON DELETE SET NULL (other)); \
                    CREATE TRIGGER hook AFTER UPDATE OF other ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            true,
        ),
        (
            "an_inheritance_descendant_of_the_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TABLE {{s}}.sub () INHERITS ({{s}}.c); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.sub FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_partition_of_the_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.part PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.part FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "a_statement_trigger_on_a_partition_of_the_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.part PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.part FOR EACH STATEMENT EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_statement_trigger_on_the_child_itself",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.part PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH STATEMENT EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "two_foreign_keys_away",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text UNIQUE REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TABLE {{s}}.g(id int PRIMARY KEY, ukey text REFERENCES {{s}}.c(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.g FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "a_cycle_the_walk_has_to_leave",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text UNIQUE REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE, \
                    loop_ukey text REFERENCES {{s}}.c(ukey) ON UPDATE CASCADE); \
                    CREATE TABLE {{s}}.elsewhere(id int PRIMARY KEY); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.elsewhere FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_generated_referenced_column_of_a_set_column",
            "CREATE TABLE {s}.p(code text PRIMARY KEY, label text, ukey text GENERATED ALWAYS AS (upper(label)) STORED UNIQUE); \
                    CREATE TABLE {s}.c(id int PRIMARY KEY, ukey text REFERENCES {s}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {s}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
                .to_owned(),
            update(&["label"]),
            true,
        ),
    ];
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let dialect = pbps_pg::Postgres::new();
        for (label, ddl, operation, refused) in cases {
            conn.execute(&format!("CREATE SCHEMA {label}"))
                .await
                .unwrap();
            conn.execute(&ddl.replace("{s}", label))
                .await
                .unwrap_or_else(|e| panic!("{label} setup: {e}"));
            let write = RowWrite {
                table: pbps_model::TableName::new(label, "p"),
                operation,
            };
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await;
            assert_eq!(
                checked.is_err(),
                refused,
                "{label}: {:?}",
                checked.err().map(|e| e.to_string())
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        }
    });
}

/// The lock the guard takes on a table it reaches through an action is the same
/// guarantee it takes on the named one: authentication that a concurrent
/// CREATE OR REPLACE TRIGGER could undo before the write is no guarantee.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_cascade_reached_table_is_held_until_the_write_finishes() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "fk_lock");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
        CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE); \
        CREATE TABLE app.c(id integer PRIMARY KEY, ukey text REFERENCES app.p(ukey) ON UPDATE CASCADE); \
        CREATE FUNCTION app.trusted() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE FUNCTION app.replacement() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE TRIGGER guarded BEFORE UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION app.trusted(); \
        INSERT INTO app.p VALUES ('first', 'k1'); INSERT INTO app.c VALUES (1, 'k1')",
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut writer = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let mut other = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
        let dialect = pbps_pg::Postgres::new();
        let write = RowWrite {
            table: pbps_model::TableName::new("app", "p"),
            operation: RowOperation::Update { columns: ["ukey".to_owned()].into() },
        };
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let guard = pbps_pg::data_triggers::prepare(&mut writer, std::slice::from_ref(&write), &baseline, &Default::default()).await.unwrap();
        other.execute("SET lock_timeout = '100ms'").await.unwrap();
        // The named table is not the one under contention here: `app.c` is
        // only ever written by the cascade the guard followed.
        let replace = "CREATE OR REPLACE TRIGGER guarded BEFORE UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION app.replacement()";
        let blocked = other.execute(replace).await.unwrap_err();
        assert!(matches!(blocked, pbps_db::DbError::Driver { ref code, .. } if code.as_deref() == Some("55P03")), "{blocked}");
        pbps_pg::data_triggers::check(&mut writer, &write, &guard).await.unwrap();
        writer.execute("UPDATE app.p SET ukey = 'k2' WHERE code = 'first'").await.unwrap();
        writer.commit(dialect.transaction_framing()).await.unwrap();
        // After release the same replacement succeeds, proving the first
        // failure came from the guard's lock, not invalid DDL or permissions.
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
        other.execute(replace).await.unwrap();
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let refused = pbps_pg::data_triggers::prepare(&mut writer, &[write], &baseline, &Default::default()).await;
        assert!(refused.is_err(), "a replaced trigger on the cascade's table must refuse the next write");
        writer.rollback(dialect.transaction_framing()).await.unwrap();
    });
}

/// The guard holds every table it authenticates, and a table it cannot hold is
/// a refusal with a name in it rather than a bare `permission denied`.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_reached_table_the_deployment_role_cannot_lock_is_named_in_the_refusal() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    struct Roles(String, String);
    impl Drop for Roles {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let admin = server();
    let own = OwnDatabase::new(&admin, "fk_lock_denied");
    let connection = own.connection();
    let deployer = format!("pbps_fk_locker_{}", std::process::id());
    let _roles = Roles(admin.clone(), deployer.clone());
    on_server(
        &admin,
        &format!("CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'"),
    );
    // The child belongs to someone else, and the deployment role may only read
    // it. PostgreSQL runs the action as the child's owner and needs nothing
    // from the deployer, so only the guard's own lock is missing.
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; GRANT CREATE ON SCHEMA public TO {deployer}; \
            CREATE TABLE public.c(id integer PRIMARY KEY, ukey text); \
            GRANT SELECT ON public.c TO {deployer}"
        ),
    );
    let deployment = format!(
        "{} user={deployer} password=trigger-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    on_server(
        &deployment,
        "CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE)",
    );
    on_server(
        connection,
        "ALTER TABLE public.c ADD FOREIGN KEY (ukey) REFERENCES app.p(ukey) ON UPDATE CASCADE",
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &deployment)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "p"),
                operation: RowOperation::Update {
                    columns: ["ukey".to_owned()].into(),
                },
            };
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let refused = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
            assert!(
                refused.contains("cannot lock") && refused.contains("public.c"),
                "{refused}"
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        });
}
