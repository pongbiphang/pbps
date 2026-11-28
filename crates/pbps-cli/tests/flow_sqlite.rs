//! End-to-end CLI flow tests for `dialect: sqlite` (#2950).
//!
//! Runs the real `pbps` binary against real temporary SQLite files with no
//! containers and no `#[ignore]`. Every test creates its own isolated temp
//! directory via `std::env::temp_dir()`, never removes an open database file,
//! uses no Unix-only permissions calls, and makes no assumption about platform
//! path separators.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use pbps_model::{IdsFile, StateKind, StateSnapshot};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pbps-flow-sqlite-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct SqliteProject {
    dir: TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl SqliteProject {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let root = dir.join("proj");
        let db = dir.join("app.db");
        let init = run_pbps(
            &dir.path,
            &[
                "--project",
                root.to_str().unwrap(),
                "init",
                "--dialect",
                "sqlite",
                "--env",
                "local",
            ],
            &[],
        );
        assert_eq!(code(&init), 0, "init failed: {}", stderr(&init));
        Self { dir, root, db }
    }

    fn write_schema(&self, name: &str, yaml: &str) {
        std::fs::write(self.root.join("schema").join(name), yaml).unwrap();
    }

    fn remove_schema(&self, name: &str) {
        std::fs::remove_file(self.root.join("schema").join(name)).unwrap();
    }

    fn pbps(&self, args: &[&str]) -> Output {
        self.pbps_with_env(args, &[])
    }

    fn pbps_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut full = vec!["--project", self.root.to_str().unwrap()];
        full.extend_from_slice(args);
        run_pbps(&self.dir.path, &full, envs)
    }

    fn db_str(&self) -> &str {
        self.db.to_str().unwrap()
    }

    fn bootstrap_starter(&self) {
        self.write_schema("main.items.yml", ITEMS_TABLE);
        let plan = self.pbps(&["plan"]);
        assert_eq!(code(&plan), 0, "offline plan failed: {}", stderr(&plan));
        let boot = self.pbps(&["bootstrap", "--db", self.db_str(), "--create"]);
        assert_eq!(code(&boot), 0, "bootstrap failed: {}", stderr(&boot));
    }
}

const ITEMS_TABLE: &str = "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: text
    nullable: false
primary_key: [id]
";

const ITEMS_VIEW: &str = "\
view: main.v_items
definition: |-
  SELECT id, name FROM main.items
";

fn run_pbps(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(cwd)
        .arg("--no-input")
        .args(args)
        .env_remove("LOCAL_CONN")
        .env_remove("PBPS_ASSERTION_CONN");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn plan_checksum(plan_path: &Path) -> String {
    let raw = std::fs::read_to_string(plan_path).unwrap();
    let plan: pbps_model::SavedPlan = serde_json::from_str(&raw).unwrap();
    plan.wire_fingerprint()
}

fn exec_sqlite(db: &Path, sql: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Sqlite, db.to_str().unwrap())
            .await
            .unwrap();
        conn.execute(sql).await.unwrap();
    });
}

#[test]
fn init_validates_plans_bootstraps_and_adopts_from_existing_sqlite_db() {
    let ws = TempDir::new("init");
    let proj = ws.join("fresh");

    // 1. `init --dialect sqlite --env local` creates project files and never a database.
    let init = run_pbps(
        &ws.path,
        &[
            "--project",
            proj.to_str().unwrap(),
            "init",
            "--dialect",
            "sqlite",
            "--env",
            "local",
        ],
        &[],
    );
    assert_eq!(code(&init), 0, "{}", stderr(&init));
    let config = std::fs::read_to_string(proj.join("pbps.yml")).unwrap();
    assert!(config.contains("dialect: sqlite"), "{config}");
    assert!(config.contains("LOCAL_CONN"), "{config}");
    assert!(proj.join("schema/.gitkeep").is_file());
    assert!(proj.join(".pbps/ids").is_dir());

    // Validates, plans, and bootstraps via relative path in CWD using env var.
    let validate = run_pbps(
        &ws.path,
        &["--project", proj.to_str().unwrap(), "validate"],
        &[],
    );
    assert_eq!(code(&validate), 0, "{}", stderr(&validate));
    let plan = run_pbps(
        &ws.path,
        &["--project", proj.to_str().unwrap(), "plan"],
        &[],
    );
    assert_eq!(code(&plan), 0, "{}", stderr(&plan));

    let rel_db = "relative.db";
    let abs_db = ws.join(rel_db);
    assert!(!abs_db.exists(), "init must never create a database");

    // Bootstrap without `--create` refuses and creates nothing.
    let missing = run_pbps(
        &ws.path,
        &[
            "--project",
            proj.to_str().unwrap(),
            "bootstrap",
            "--env",
            "local",
        ],
        &[("LOCAL_CONN", rel_db)],
    );
    assert_ne!(code(&missing), 0);
    assert!(
        stderr(&missing).contains("--create"),
        "{}",
        stderr(&missing)
    );
    assert!(
        !abs_db.exists(),
        "bootstrap without --create must not create a file"
    );

    // Bootstrap with `--create` creates the file relative to CWD and succeeds.
    let boot = run_pbps(
        &ws.path,
        &[
            "--project",
            proj.to_str().unwrap(),
            "bootstrap",
            "--env",
            "local",
            "--create",
        ],
        &[("LOCAL_CONN", rel_db)],
    );
    assert_eq!(code(&boot), 0, "{}", stderr(&boot));
    assert!(abs_db.is_file(), "relative path resolves against CWD");

    // Populate a table + view in the database, then test `init --dialect sqlite --from local`.
    exec_sqlite(
        &abs_db,
        "CREATE TABLE main.accounts (id INTEGER PRIMARY KEY NOT NULL, email TEXT NOT NULL);
         CREATE VIEW main.v_accounts AS SELECT id, email FROM main.accounts;",
    );
    let adopted = ws.join("adopted");
    let from = run_pbps(
        &ws.path,
        &[
            "--project",
            adopted.to_str().unwrap(),
            "init",
            "--dialect",
            "sqlite",
            "--from",
            "local",
        ],
        &[("LOCAL_CONN", rel_db)],
    );
    assert_eq!(code(&from), 0, "{}", stderr(&from));
    let adopted_validate = run_pbps(
        &ws.path,
        &["--project", adopted.to_str().unwrap(), "validate"],
        &[],
    );
    assert_eq!(code(&adopted_validate), 0, "{}", stderr(&adopted_validate));
    assert!(
        stdout(&adopted_validate).contains("1 table(s)")
            && stdout(&adopted_validate).contains("1 module(s)"),
        "{}",
        stdout(&adopted_validate)
    );

    // Negative: unknown `--dialect` is refused and lists all supported dialects including sqlite.
    let bad = run_pbps(
        &ws.path,
        &[
            "--project",
            ws.join("bad").to_str().unwrap(),
            "init",
            "--dialect",
            "oracle",
        ],
        &[],
    );
    assert_eq!(code(&bad), 2);
    let err = stderr(&bad);
    for d in pbps_config::DialectName::ALL.iter().map(|d| d.as_str()) {
        assert!(err.contains(d), "expected `{d}` in clap error: {err}");
    }
}

#[test]
fn bootstrap_plan_rehearsal_apply_verify_explain_and_status_end_to_end() {
    let p = SqliteProject::new("lifecycle");
    p.write_schema("main.items.yml", ITEMS_TABLE);
    assert_eq!(code(&p.pbps(&["validate"])), 0);
    assert_eq!(code(&p.pbps(&["plan"])), 0);

    // Negative: bootstrap without `--create` on a missing file refuses and creates nothing.
    let no_create = p.pbps(&["bootstrap", "--db", p.db_str()]);
    assert_eq!(code(&no_create), 1);
    assert!(
        stderr(&no_create).contains("--create"),
        "{}",
        stderr(&no_create)
    );
    assert!(!p.db.exists());

    // Bootstrap with `--create` creates the file and records entry #1.
    let boot = p.pbps(&["bootstrap", "--db", p.db_str(), "--create"]);
    assert_eq!(code(&boot), 0, "{}", stderr(&boot));
    assert!(p.db.is_file());
    assert!(stdout(&boot).contains("entry #1"), "{}", stdout(&boot));

    // Verify clean.
    let verify = p.pbps(&["verify", "--db", p.db_str()]);
    assert_eq!(code(&verify), 0, "{}", stderr(&verify));

    // Add a column and a view, plan with default in-process rehearsal.
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: text
    nullable: false
  notes:
    type: text
    nullable: true
primary_key: [id]
",
    );
    p.write_schema("main.v_items.yml", ITEMS_VIEW);
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let plan_file = p.dir.join("add.plan.json");
    let planned = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--out",
        plan_file.to_str().unwrap(),
    ]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    assert!(
        stdout(&planned).contains("converged (in-process scratch)"),
        "{}",
        stdout(&planned)
    );

    // `explain` in human and JSON modes shows in-process rehearsal and library version.
    let exp_text = p.pbps(&["explain", "--plan", plan_file.to_str().unwrap()]);
    assert_eq!(code(&exp_text), 0, "{}", stderr(&exp_text));
    assert!(
        stdout(&exp_text).contains("converged (in-process scratch)"),
        "{}",
        stdout(&exp_text)
    );
    let exp_json = p.pbps(&[
        "explain",
        "--plan",
        plan_file.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&exp_json), 0, "{}", stderr(&exp_json));
    let explained: serde_json::Value = serde_json::from_slice(&exp_json.stdout).unwrap();
    assert_eq!(explained["data"]["dialect"], "sqlite");
    assert_eq!(explained["data"]["rehearsal"]["outcome"], "converged");
    assert_eq!(explained["data"]["rehearsal"]["scratch"], "in-process");
    assert!(
        explained["data"]["rehearsal"]["library"]
            .as_str()
            .is_some_and(|lib| lib.starts_with("SQLite ")),
        "{explained}"
    );

    // `--no-rehearse` seals rehearsal off.
    let noreh_file = p.dir.join("noreh.plan.json");
    let noreh = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--no-rehearse",
        "--out",
        noreh_file.to_str().unwrap(),
    ]);
    assert_eq!(code(&noreh), 0, "{}", stderr(&noreh));
    let noreh_json = p.pbps(&[
        "explain",
        "--plan",
        noreh_file.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&noreh_json), 0, "{}", stderr(&noreh_json));
    let noreh_val: serde_json::Value = serde_json::from_slice(&noreh_json.stdout).unwrap();
    assert_eq!(noreh_val["data"]["rehearsal"]["outcome"], "off");
    assert!(noreh_val["data"]["rehearsal"].get("library").is_none());

    // Negative: wrong checksum is refused by `apply`.
    let bad_chk = "0".repeat(64);
    let bad_apply = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        plan_file.to_str().unwrap(),
        "--checksum",
        &bad_chk,
    ]);
    assert_ne!(code(&bad_apply), 0);

    // Apply with real checksum succeeds and prints preflight floor warning + entry #2.
    let chk = plan_checksum(&plan_file);
    let applied = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        plan_file.to_str().unwrap(),
        "--checksum",
        &chk,
    ]);
    assert_eq!(code(&applied), 0, "{}", stderr(&applied));
    assert!(
        stdout(&applied).contains("entry #2"),
        "{}",
        stdout(&applied)
    );

    // Verify clean after apply, and `status` reports environment ok.
    assert_eq!(code(&p.pbps(&["verify", "--db", p.db_str()])), 0);
    let status = p.pbps_with_env(
        &["status", "--format", "json"],
        &[("LOCAL_CONN", p.db_str())],
    );
    assert_eq!(code(&status), 0, "{}", stderr(&status));
    let status_val: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let row = &status_val["data"][0];
    assert_eq!(row["environment"], "local", "{status_val}");
    assert_eq!(row["state"], "ok", "{status_val}");

    // Negative: live drift makes `verify` exit 2.
    exec_sqlite(&p.db, "ALTER TABLE main.items ADD COLUMN rogue TEXT;");
    let drifted = p.pbps(&["verify", "--db", p.db_str()]);
    assert_eq!(
        code(&drifted),
        2,
        "expected drift exit 2: {}",
        stderr(&drifted)
    );
}

#[test]
fn rehearsal_refuses_broken_view_and_writes_no_plan() {
    let p = SqliteProject::new("rehearsal-fail");
    p.bootstrap_starter();
    p.write_schema(
        "main.v_broken.yml",
        "view: main.v_broken\ndefinition: |-\n  SELECT id FROM main.does_not_exist\n",
    );
    let plan_file = p.dir.join("broken.plan.json");
    let out = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--out",
        plan_file.to_str().unwrap(),
    ]);
    assert_ne!(
        code(&out),
        0,
        "rehearsal must refuse view over missing table"
    );
    assert!(
        !plan_file.exists(),
        "failed rehearsal must not write a plan"
    );
    let combined = format!("{}\n{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("does_not_exist") || combined.contains("rehearsal"),
        "{combined}"
    );
}

#[test]
fn doctor_reports_ready_sqlite_library_and_path_remedy_when_missing() {
    let p = SqliteProject::new("doctor");
    p.bootstrap_starter();

    let ok = p.pbps_with_env(&["doctor", "--env", "local"], &[("LOCAL_CONN", p.db_str())]);
    assert_eq!(
        code(&ok),
        0,
        "stdout={}\nstderr={}",
        stdout(&ok),
        stderr(&ok)
    );
    let out = stdout(&ok);
    assert!(out.contains("library"), "expected library label:\n{out}");
    assert!(out.contains("SQLite "), "{out}");
    assert!(
        !out.contains("fingerprint key"),
        "no PG fingerprint note:\n{out}"
    );
    assert!(
        !out.contains("resolver compatibility"),
        "no resolver noise:\n{out}"
    );

    // Negative: missing file is reported as unreachable with a path remedy, never host/firewall.
    let missing = p.dir.join("missing.db");
    let bad = p.pbps_with_env(
        &["doctor", "--env", "local"],
        &[("LOCAL_CONN", missing.to_str().unwrap())],
    );
    assert_ne!(code(&bad), 0);
    let combined = format!("{}\n{}", stdout(&bad), stderr(&bad));
    assert!(
        !combined.contains("firewall"),
        "server wording leaked:\n{combined}"
    );
    assert!(!missing.exists(), "doctor must never create a missing file");
}

#[test]
fn pull_round_trips_tables_views_indexes_and_refuses_overwrite_without_force() {
    let p = SqliteProject::new("pull");
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  sku:
    type: text
    nullable: false
primary_key: [id]
indexes:
  idx_items_sku:
    columns: [sku]
    unique: true
",
    );
    p.write_schema(
        "main.v_items.yml",
        "view: main.v_items\ndefinition: |-\n  SELECT id, sku FROM main.items\n",
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    assert_eq!(
        code(&p.pbps(&["bootstrap", "--db", p.db_str(), "--create"])),
        0
    );

    // Pull into a fresh initialized SQLite project.
    let pulled_proj = p.dir.join("pulled");
    assert_eq!(
        code(&run_pbps(
            &p.dir.path,
            &[
                "--project",
                pulled_proj.to_str().unwrap(),
                "init",
                "--dialect",
                "sqlite",
            ],
            &[],
        )),
        0
    );
    let pull = run_pbps(
        &p.dir.path,
        &[
            "--project",
            pulled_proj.to_str().unwrap(),
            "pull",
            "--db",
            p.db_str(),
        ],
        &[],
    );
    assert_eq!(code(&pull), 0, "{}", stderr(&pull));
    assert_eq!(
        code(&run_pbps(
            &p.dir.path,
            &["--project", pulled_proj.to_str().unwrap(), "validate"],
            &[],
        )),
        0
    );

    // Negative: second pull without `--force` refuses because declarations already exist.
    let second = run_pbps(
        &p.dir.path,
        &[
            "--project",
            pulled_proj.to_str().unwrap(),
            "pull",
            "--db",
            p.db_str(),
        ],
        &[],
    );
    assert_ne!(code(&second), 0);
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));

    // `--force` succeeds; after `baseline`, `plan --db` produces 0 changes (round-trip convergence).
    let forced = run_pbps(
        &p.dir.path,
        &[
            "--project",
            pulled_proj.to_str().unwrap(),
            "pull",
            "--db",
            p.db_str(),
            "--force",
        ],
        &[],
    );
    assert_eq!(code(&forced), 0, "{}", stderr(&forced));
    let adopt_base = run_pbps(
        &p.dir.path,
        &[
            "--project",
            pulled_proj.to_str().unwrap(),
            "baseline",
            "--db",
            p.db_str(),
            "--reason",
            "pulled-roundtrip",
        ],
        &[],
    );
    assert_eq!(code(&adopt_base), 0, "{}", stderr(&adopt_base));
    let plan = run_pbps(
        &p.dir.path,
        &[
            "--project",
            pulled_proj.to_str().unwrap(),
            "plan",
            "--db",
            p.db_str(),
        ],
        &[],
    );
    assert_eq!(code(&plan), 0, "{}", stderr(&plan));
    assert!(stdout(&plan).contains("No changes"), "{}", stdout(&plan));
}

#[test]
fn state_list_show_diff_snapshot_baseline_and_unlock_work_end_to_end() {
    let p = SqliteProject::new("state-unlock");
    p.bootstrap_starter();

    // Evolve once so there are 2 ledger entries.
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: text
    nullable: false
  tag:
    type: text
    nullable: true
primary_key: [id]
",
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let plan_file = p.dir.join("evolve.plan.json");
    assert_eq!(
        code(&p.pbps(&[
            "plan",
            "--db",
            p.db_str(),
            "--out",
            plan_file.to_str().unwrap()
        ])),
        0
    );
    let chk = plan_checksum(&plan_file);
    assert_eq!(
        code(&p.pbps(&[
            "apply",
            "--db",
            p.db_str(),
            "--plan",
            plan_file.to_str().unwrap(),
            "--checksum",
            &chk,
        ])),
        0
    );

    // `state list`, `state show 1`, `state diff 1 2`.
    let list = p.pbps(&["state", "list", "--db", p.db_str(), "--format", "json"]);
    assert_eq!(code(&list), 0, "{}", stderr(&list));
    let list_val: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list_val["data"]["entries"].as_array().unwrap().len(), 2);

    let show = p.pbps(&["state", "show", "1", "--db", p.db_str(), "--format", "json"]);
    assert_eq!(code(&show), 0, "{}", stderr(&show));
    let show_val: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(show_val["data"]["entry"]["id"], 1);

    let diff = p.pbps(&["state", "diff", "1", "2", "--db", p.db_str()]);
    assert_eq!(code(&diff), 0, "{}", stderr(&diff));
    assert!(stdout(&diff).contains("tag"), "{}", stdout(&diff));

    // Negative: `state show 999` refuses.
    let missing_entry = p.pbps(&["state", "show", "999", "--db", p.db_str()]);
    assert_ne!(code(&missing_entry), 0);

    // `snapshot` on clean DB succeeds; drifted DB refuses without `--force` and succeeds with it.
    let snap_ok = p.pbps(&["snapshot", "--db", p.db_str()]);
    assert_eq!(code(&snap_ok), 0, "{}", stderr(&snap_ok));
    exec_sqlite(&p.db, "ALTER TABLE main.items ADD COLUMN extra TEXT;");
    let snap_drift = p.pbps(&["snapshot", "--db", p.db_str()]);
    assert_ne!(code(&snap_drift), 0);
    let snap_force = p.pbps(&["snapshot", "--db", p.db_str(), "--force"]);
    assert_eq!(code(&snap_force), 0, "{}", stderr(&snap_force));

    // `baseline --reason` adopts a separate pre-existing SQLite file.
    let adopt_db = p.dir.join("preexisting.db");
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut conn =
                pbps_db::Conn::create(pbps_db::Driver::Sqlite, adopt_db.to_str().unwrap())
                    .unwrap();
            conn.execute(
                "CREATE TABLE main.items (id INTEGER NOT NULL, name TEXT NOT NULL, tag TEXT, PRIMARY KEY (id));",
            )
            .await
            .unwrap();
        });
    }
    let baseline = p.pbps(&[
        "baseline",
        "--db",
        adopt_db.to_str().unwrap(),
        "--reason",
        "initial-adoption",
    ]);
    assert_eq!(code(&baseline), 0, "{}", stderr(&baseline));
    assert_eq!(
        code(&p.pbps(&["verify", "--db", adopt_db.to_str().unwrap()])),
        0
    );

    // `unlock`: unheld reports not locked; planted lock row is cleared by `unlock`.
    let unlock_idle = p.pbps(&["unlock", "--db", p.db_str()]);
    assert_eq!(code(&unlock_idle), 0, "{}", stderr(&unlock_idle));
    exec_sqlite(
        &p.db,
        "INSERT INTO main.__pbps_lock (id, locked_by, locked_at, proof, token) VALUES (1, 'stuck-holder', '2026-01-01T00:00:00Z', 'row', 'tok-1');",
    );
    let unlock_held = p.pbps(&["unlock", "--db", p.db_str()]);
    assert_eq!(code(&unlock_held), 0, "{}", stderr(&unlock_held));
    assert!(
        stdout(&unlock_held).contains("stuck-holder") || stdout(&unlock_held).contains("Cleared"),
        "{}",
        stdout(&unlock_held)
    );
}

#[test]
fn rename_and_guarded_drop_work_end_to_end_and_refuse_violating_rows() {
    let p = SqliteProject::new("rename-drop");
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  old_name:
    type: text
    nullable: false
  obsolete:
    type: text
    nullable: true
primary_key: [id]
",
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    assert_eq!(
        code(&p.pbps(&["bootstrap", "--db", p.db_str(), "--create"])),
        0
    );

    // Insert a row that violates the drop requirement (`obsolete IS NULL`).
    exec_sqlite(
        &p.db,
        "INSERT INTO main.items (id, old_name, obsolete) VALUES (1, 'widget', 'still-used');",
    );

    // Step 1: rename `old_name` -> `title` in YAML, then record `rename`.
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  title:
    type: text
    nullable: false
  obsolete:
    type: text
    nullable: true
primary_key: [id]
",
    );
    let ren = p.pbps(&["rename", "main.items.old_name", "title"]);
    assert_eq!(code(&ren), 0, "rename failed: {}", stderr(&ren));

    // Step 2: remove `obsolete` in YAML, then record `drop --require`.
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  title:
    type: text
    nullable: false
primary_key: [id]
",
    );
    let drp = p.pbps(&[
        "drop",
        "main.items.obsolete",
        "--reason",
        "column retired",
        "--require",
        "obsolete IS NULL",
        "--db",
        p.db_str(),
    ]);
    assert_eq!(code(&drp), 0, "drop --require failed: {}", stderr(&drp));

    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let plan_file = p.dir.join("ren-drop.plan.json");

    // Negative 1: default in-process rehearsal runs on a page copy holding live rows,
    // so the violating row blocks `plan --db` before any plan file is written.
    let reh_blocked = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--out",
        plan_file.to_str().unwrap(),
    ]);
    assert_ne!(code(&reh_blocked), 0, "rehearsal must catch violating row");
    assert!(
        stderr(&reh_blocked).contains("obsolete IS NULL")
            && stderr(&reh_blocked).contains("1 row(s)"),
        "{}",
        stderr(&reh_blocked)
    );
    assert!(!plan_file.exists(), "failed rehearsal must not write plan");

    // Clear the violating row so `plan --db` (rehearsed) succeeds.
    exec_sqlite(&p.db, "UPDATE main.items SET obsolete = NULL;");
    let planned = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--out",
        plan_file.to_str().unwrap(),
    ]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    let chk = plan_checksum(&plan_file);

    // Negative 2: without `--allow destructive`, apply refuses the drop.
    let denied = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        plan_file.to_str().unwrap(),
        "--checksum",
        &chk,
    ]);
    assert_ne!(code(&denied), 0);

    // Negative 3: a violating row arriving before `apply` still blocks inside apply's tx.
    exec_sqlite(
        &p.db,
        "UPDATE main.items SET obsolete = 'arrived-before-apply';",
    );
    let blocked = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        plan_file.to_str().unwrap(),
        "--checksum",
        &chk,
        "--allow",
        "destructive,rename",
    ]);
    assert_ne!(code(&blocked), 0, "violating row must block guarded drop");
    assert!(
        stderr(&blocked).contains("obsolete IS NULL"),
        "{}",
        stderr(&blocked)
    );

    // Clear the violating value and apply succeeds!
    exec_sqlite(&p.db, "UPDATE main.items SET obsolete = NULL;");
    let ok = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        plan_file.to_str().unwrap(),
        "--checksum",
        &chk,
        "--allow",
        "destructive,rename",
    ]);
    assert_eq!(code(&ok), 0, "{}", stderr(&ok));
    assert_eq!(code(&p.pbps(&["verify", "--db", p.db_str()])), 0);

    // Table rename end-to-end: update YAML first, then record `rename-table`.
    p.remove_schema("main.items.yml");
    p.write_schema(
        "main.products.yml",
        "\
table: main.products
columns:
  id:
    type: integer
    nullable: false
  title:
    type: text
    nullable: false
primary_key: [id]
",
    );
    let ren_tbl = p.pbps(&["rename-table", "main.items", "main.products"]);
    assert_eq!(
        code(&ren_tbl),
        0,
        "rename-table failed: {}",
        stderr(&ren_tbl)
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let ren_plan = p.dir.join("ren-table.plan.json");
    assert_eq!(
        code(&p.pbps(&[
            "plan",
            "--db",
            p.db_str(),
            "--out",
            ren_plan.to_str().unwrap(),
        ])),
        0
    );
    let ren_chk = plan_checksum(&ren_plan);
    let ren_apply = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        ren_plan.to_str().unwrap(),
        "--checksum",
        &ren_chk,
        "--allow",
        "rename",
    ]);
    assert_eq!(code(&ren_apply), 0, "{}", stderr(&ren_apply));
    assert_eq!(code(&p.pbps(&["verify", "--db", p.db_str()])), 0);
}

#[test]
fn table_rebuild_change_is_refused_by_name() {
    let p = SqliteProject::new("rebuild-refusal");
    p.bootstrap_starter();

    // Altering a column's type on SQLite requires rebuilding the table (not built yet).
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: integer
    nullable: false
primary_key: [id]
",
    );
    let plan_file = p.dir.join("rebuild.plan.json");
    let out = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--out",
        plan_file.to_str().unwrap(),
    ]);
    assert_ne!(code(&out), 0);
    let combined = format!("{}\n{}", stdout(&out), stderr(&out));
    assert!(
        combined.to_ascii_lowercase().contains("sqlite")
            && combined.to_ascii_lowercase().contains("rebuild"),
        "expected SQLite rebuild refusal:\n{combined}"
    );
    assert!(!plan_file.exists(), "refused plan must not be written");
}

#[test]
fn server_only_commands_and_flags_refuse_by_name_on_sqlite() {
    let p = SqliteProject::new("server-only");
    p.bootstrap_starter();
    let db = p.db_str().to_owned();

    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            "plan --resolve-with",
            vec!["plan", "--db", &db, "--resolve-with", "scratch"],
        ),
        (
            "plan --resolve-with --no-rehearse",
            vec![
                "plan",
                "--db",
                &db,
                "--resolve-with",
                "scratch",
                "--no-rehearse",
            ],
        ),
        (
            "plan --assert",
            vec!["plan", "--db", &db, "--assert", "missing.sql"],
        ),
        ("plan --dev", vec!["plan", "--dev", "docker://postgres:18"]),
        ("rename-role", vec!["rename-role", "old_r", "new_r"]),
        (
            "drop-role",
            vec!["drop-role", "old_r", "--reason", "cleanup"],
        ),
        (
            "drop-extension",
            vec!["drop-extension", "citext", "--reason", "cleanup"],
        ),
        (
            "drop-search",
            vec!["drop-search", "config main.docs", "--reason", "cleanup"],
        ),
        (
            "drop-exclusion",
            vec!["drop-exclusion", "main.items", "ex1", "--reason", "cleanup"],
        ),
        (
            "move-rows",
            vec![
                "move-rows",
                "main.items",
                "--max-rows",
                "10",
                "--reason",
                "cleanup",
            ],
        ),
        (
            "convert",
            vec!["convert", "main.items.name", "--using", "name"],
        ),
        (
            "rebuild-table",
            vec![
                "rebuild-table",
                "main.items",
                "--max-rows",
                "10",
                "--reason",
                "cleanup",
            ],
        ),
        (
            "rebuild-table --withdraw",
            vec!["rebuild-table", "main.items", "--withdraw"],
        ),
        (
            "partitions extend",
            vec![
                "partitions",
                "extend",
                "main.items",
                "--every",
                "1month",
                "--until",
                "2028-01-01",
            ],
        ),
        ("resolver externals", vec!["resolver", "externals"]),
        ("test", vec!["test"]),
        (
            "test fixtures repin",
            vec![
                "test",
                "fixtures",
                "repin",
                "--at",
                "0000000000000000000000000000000000000000",
                "foo",
            ],
        ),
        (
            "pull --data",
            vec!["pull", "--db", &db, "--data", "main.items"],
        ),
        (
            "pull --partitions-external",
            vec!["pull", "--db", &db, "--partitions-external", "main.items"],
        ),
        (
            "pull --extension",
            vec!["pull", "--db", &db, "--extension", "citext"],
        ),
        (
            "intent --drop-role",
            vec!["intent", "--drop-role", "old_r", "cleanup"],
        ),
        (
            "intent --rename-type",
            vec!["intent", "--rename-type", "main.e1", "main.e2"],
        ),
    ];

    for (label, args) in &cases {
        let out = p.pbps(args);
        assert_ne!(code(&out), 0, "{label} unexpectedly succeeded");
        let combined = format!("{}\n{}", stdout(&out), stderr(&out));
        assert!(
            combined.to_ascii_lowercase().contains("sqlite"),
            "{label} refusal did not name SQLite:\n{combined}"
        );
    }

    // Negative (the mirror refusal): `bootstrap --create` is refused on server engines by name.
    for (dialect, engine_word) in [("postgres", "PostgreSQL"), ("mssql", "SQL Server")] {
        let server_proj = p.dir.join(format!("srv-{dialect}"));
        std::fs::create_dir_all(server_proj.join("schema")).unwrap();
        std::fs::write(
            server_proj.join("pbps.yml"),
            format!("dialect: {dialect}\n"),
        )
        .unwrap();
        let out = run_pbps(
            &p.dir.path,
            &[
                "--project",
                server_proj.to_str().unwrap(),
                "bootstrap",
                "--db",
                "Server=127.0.0.1,1;Database=x",
                "--create",
            ],
            &[],
        );
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains(engine_word) && stderr(&out).contains("--create"),
            "{dialect}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn plan_base_refuses_snapshots_from_other_engines_and_accepts_sqlite() {
    let ws = TempDir::new("snapshot-dialect");

    let make_project = |dialect: &str, file: &str, yaml: &str| {
        let root = ws.join(dialect);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        std::fs::write(root.join("schema").join(file), yaml).unwrap();
        let loaded = pbps_load::load_schema_dir(&root.join("schema")).unwrap();
        let snap = StateSnapshot::new(
            StateKind::Apply,
            dialect,
            loaded.schema,
            IdsFile::default(),
            "flow-sqlite-test",
        );
        let snap_path = ws.join(format!("{dialect}.state.json"));
        std::fs::write(&snap_path, serde_json::to_string_pretty(&snap).unwrap()).unwrap();
        (root, snap_path)
    };

    let (sqlite_root, sqlite_snap) = make_project("sqlite", "main.items.yml", ITEMS_TABLE);
    let (pg_root, pg_snap) = make_project(
        "postgres",
        "app.items.yml",
        "table: app.items\ncolumns:\n  id: { type: integer, nullable: false }\nprimary_key: [id]\n",
    );
    let (ms_root, ms_snap) = make_project(
        "mssql",
        "dbo.items.yml",
        "table: dbo.items\ncolumns:\n  id: { type: int, nullable: false }\nprimary_key: [id]\n",
    );

    // Every cross-engine pair involving SQLite is refused naming both engines.
    for (src_name, snap, dst_name, dst_root) in [
        ("sqlite", &sqlite_snap, "postgres", &pg_root),
        ("sqlite", &sqlite_snap, "mssql", &ms_root),
        ("postgres", &pg_snap, "sqlite", &sqlite_root),
        ("mssql", &ms_snap, "sqlite", &sqlite_root),
    ] {
        let out = run_pbps(
            &ws.path,
            &[
                "--project",
                dst_root.to_str().unwrap(),
                "plan",
                "--base",
                snap.to_str().unwrap(),
            ],
            &[],
        );
        assert_ne!(code(&out), 0, "{src_name} -> {dst_name} should refuse");
        let err = stderr(&out);
        assert!(
            err.contains(src_name) && err.contains(dst_name),
            "{src_name} -> {dst_name}: expected both engine names in:\n{err}"
        );
    }

    // Positive: SQLite snapshot against SQLite project succeeds.
    let ok = run_pbps(
        &ws.path,
        &[
            "--project",
            sqlite_root.to_str().unwrap(),
            "plan",
            "--base",
            sqlite_snap.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(code(&ok), 0, "{}", stderr(&ok));
}

#[test]
fn mistyped_db_path_never_creates_a_file_across_every_command() {
    let p = SqliteProject::new("no-create");
    p.bootstrap_starter();

    // Prepare a real saved plan for `apply` and `explain --db`.
    p.write_schema(
        "main.items.yml",
        "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: text
    nullable: false
  note:
    type: text
    nullable: true
primary_key: [id]
",
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let plan_file = p.dir.join("change.plan.json");
    assert_eq!(
        code(&p.pbps(&[
            "plan",
            "--db",
            p.db_str(),
            "--out",
            plan_file.to_str().unwrap()
        ])),
        0
    );
    let chk = plan_checksum(&plan_file);
    let plan_s = plan_file.to_str().unwrap().to_owned();

    let commands: Vec<(&str, Vec<String>)> = vec![
        ("bootstrap (no --create)", vec!["bootstrap".into()]),
        ("plan", vec!["plan".into()]),
        (
            "apply",
            vec![
                "apply".into(),
                "--plan".into(),
                plan_s.clone(),
                "--checksum".into(),
                chk.clone(),
            ],
        ),
        ("verify", vec!["verify".into()]),
        ("doctor", vec!["doctor".into()]),
        ("pull", vec!["pull".into()]),
        ("snapshot", vec!["snapshot".into()]),
        (
            "baseline",
            vec!["baseline".into(), "--reason".into(), "test".into()],
        ),
        ("unlock", vec!["unlock".into()]),
        (
            "explain --db",
            vec!["explain".into(), "--plan".into(), plan_s.clone()],
        ),
        ("state list", vec!["state".into(), "list".into()]),
        (
            "state show",
            vec!["state".into(), "show".into(), "1".into()],
        ),
        (
            "state diff",
            vec!["state".into(), "diff".into(), "1".into()],
        ),
        (
            "state export",
            vec!["state".into(), "export".into(), "1".into()],
        ),
        (
            "state prune",
            vec!["state".into(), "prune".into(), "--keep".into(), "1".into()],
        ),
    ];

    for (idx, (label, base_args)) in commands.iter().enumerate() {
        let missing = p.dir.join(format!("typo-{idx}.db"));
        let mut args: Vec<&str> = base_args.iter().map(String::as_str).collect();
        args.push("--db");
        args.push(missing.to_str().unwrap());
        let out = p.pbps(&args);
        // `explain` always exits 0 by design (SPEC §9.6) and reports the target
        // unreachable; every other connected command exits non-zero.
        if *label == "explain --db" {
            assert_eq!(
                code(&out),
                0,
                "explain --db should exit 0: {}",
                stderr(&out)
            );
        } else {
            assert_ne!(code(&out), 0, "{label} should fail on missing file");
        }
        assert!(
            !missing.exists(),
            "{label} created a file for a mistyped --db path!"
        );
    }

    // Negative: special SQLite strings (`""`, `":memory:"`, `"file:..."`) are also refused cleanly.
    for bad in ["", ":memory:", "file:should-not-create.db"] {
        let out = p.pbps(&["plan", "--db", bad]);
        assert_ne!(code(&out), 0, "expected refusal for `{bad}`");
        assert!(
            !stderr(&out).contains("open_in_memory"),
            "internal API name leaked for `{bad}`: {}",
            stderr(&out)
        );
    }
    assert!(!p.dir.join("should-not-create.db").exists());
}

#[test]
fn validate_matches_every_declared_object_kind_in_the_sqlite_support_table() {
    // Pins the SQLite column of `docs/STATUS.md` against real `pbps validate`:
    // `table:` and `view:` succeed; all other declaration kinds are refused.
    let ok = SqliteProject::new("support-ok");
    ok.write_schema("main.items.yml", ITEMS_TABLE);
    ok.write_schema("main.v_items.yml", ITEMS_VIEW);
    let out = ok.pbps(&["validate"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let refused_kinds: &[(&str, &str)] = &[
        ("procedure", "procedure: main.p\ndefinition: SELECT 1\n"),
        ("function", "function: main.f\ndefinition: SELECT 1\n"),
        (
            "trigger",
            "trigger: main.tr\non: main.items\ndefinition: AFTER INSERT ON main.items BEGIN SELECT 1; END\n",
        ),
        (
            "extension",
            "extension: citext\nschema: main\nversion: \"1.6\"\n",
        ),
        ("role", "role: reader\n"),
        ("enum", "enum: main.mood\nvalues: [happy, sad]\n"),
        ("domain", "domain: main.posint\ntype: integer\n"),
        (
            "text_search_config",
            "text_search_config: main.docs\nparser: default\n",
        ),
        (
            "text_search_dictionary",
            "text_search_dictionary: main.dict\ntemplate: simple\n",
        ),
        ("sequence", "sequence: main.seq\ntype: bigint\n"),
        (
            "dependency",
            "dependency: other.main.ext\nkind: table\ncolumns:\n  id: { type: integer, nullable: false }\n",
        ),
    ];

    for (kind, yaml) in refused_kinds {
        let proj = SqliteProject::new(&format!("support-{kind}"));
        proj.write_schema("main.items.yml", ITEMS_TABLE);
        proj.write_schema(&format!("{kind}.yml"), yaml);
        let out = proj.pbps(&["validate"]);
        assert_ne!(
            code(&out),
            0,
            "`{kind}:` must be refused by SQLite validate; stdout={}",
            stdout(&out)
        );
    }

    // Negative: non-`main` schema and `data:` are refused naming sqlite.
    for (tag, yaml) in [
        (
            "non-main",
            "table: app.items\ncolumns:\n  id: { type: integer, nullable: false }\nprimary_key: [id]\n",
        ),
        (
            "data",
            "table: main.items\ncolumns:\n  id: { type: integer, nullable: false }\nprimary_key: [id]\ndata:\n  mode: exact\n  rows:\n    - { id: 1 }\n",
        ),
    ] {
        let proj = SqliteProject::new(&format!("support-{tag}"));
        proj.write_schema("main.items.yml", yaml);
        let out = proj.pbps(&["validate"]);
        assert_ne!(code(&out), 0, "{tag} must be refused");
        let combined = format!("{}\n{}", stdout(&out), stderr(&out));
        assert!(
            combined.to_ascii_lowercase().contains("sqlite"),
            "{tag} refusal did not name sqlite:\n{combined}"
        );
    }
}

/// SQLite keeps a view's text without its `;` and without the line ending a
/// `definition: |` block closes with, and neither the apply's read-back nor
/// the next plan may call that a change.
#[test]
fn a_view_declared_with_a_terminator_and_a_closing_line_ending_applies_and_verifies_clean() {
    let p = SqliteProject::new("view-block");
    p.bootstrap_starter();
    p.write_schema(
        "main.v_items.yml",
        "view: main.v_items\ndefinition: |\n  SELECT id\n    FROM main.items;\n",
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let plan_file = p.dir.join("view.plan.json");
    let planned = p.pbps(&[
        "plan",
        "--db",
        p.db_str(),
        "--out",
        plan_file.to_str().unwrap(),
    ]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    let chk = plan_checksum(&plan_file);
    let applied = p.pbps(&[
        "apply",
        "--db",
        p.db_str(),
        "--plan",
        plan_file.to_str().unwrap(),
        "--checksum",
        &chk,
    ]);
    assert_eq!(
        code(&applied),
        0,
        "{}{}",
        stdout(&applied),
        stderr(&applied)
    );
    let verify = p.pbps(&["verify", "--db", p.db_str()]);
    assert_eq!(code(&verify), 0, "{}", stderr(&verify));
    let again = p.pbps(&["plan", "--db", p.db_str()]);
    assert!(stdout(&again).contains("No changes"), "{}", stdout(&again));

    // Negative: another body is a change, not layout.
    p.write_schema(
        "main.v_items.yml",
        "view: main.v_items\ndefinition: |\n  SELECT name\n    FROM main.items\n",
    );
    assert_eq!(code(&p.pbps(&["plan"])), 0);
    let changed = p.pbps(&["plan", "--db", p.db_str()]);
    assert!(
        !stdout(&changed).contains("No changes"),
        "{}",
        stdout(&changed)
    );
}
