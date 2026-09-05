//! End-to-end flow tests.
//!
//! Unit tests cover the logic of each layer, but what a user actually meets is
//! what happens when a command runs: the exit code, the message, and whether a
//! file was written. CI depends entirely on exit codes, so they are pinned here.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Demo {
    /// The project directory: where `pbps.yml` is.
    dir: PathBuf,
    /// The git working tree, which is the project directory unless the demo was
    /// built nested. Removed on drop, so it has to be tracked separately.
    root: PathBuf,
}

impl Demo {
    fn new(name: &str) -> Self {
        Self::nested(name, "")
    }

    /// A project `sub` levels below the repo root. `sub` empty puts them at the
    /// same place, which is the ordinary case.
    fn nested(name: &str, sub: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pbps-flow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = if sub.is_empty() {
            root.clone()
        } else {
            root.join(sub)
        };
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(dir.join("pbps.yml"), "dialect: mssql\n").unwrap();

        let d = Self { dir, root };
        d.git(&["init", "-q"]);
        d.git(&["config", "user.email", "d@e.f"]);
        d.git(&["config", "user.name", "demo"]);
        d
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
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
        std::fs::write(self.dir.join("schema/dbo.t.yml"), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--project")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn ids_path(&self) -> PathBuf {
        self.dir.join("schema.ids.json")
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
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

/// The command ran correctly and found something the user must act on.
///
/// Distinct from 1, which means the tool could not answer at all (SPEC §14.1).
/// Asserting the exact code rather than "non-zero" is the point: a pipeline
/// that treats an unreachable database the same as an invalid declaration wakes
/// the wrong person, and only a test can hold the two apart.
const FINDING: i32 = 2;

const ONE_COLUMN: &str = "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n";

#[test]
fn first_run_creates_the_ids_file_and_warns_about_the_empty_baseline() {
    let d = Demo::new("first");
    d.table(ONE_COLUMN);

    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(d.ids_path().exists(), "the identity file should be created");
    assert!(
        stderr(&o).contains("the baseline is empty"),
        "an empty baseline must warn loudly, or it reads as a real plan: {}",
        stderr(&o)
    );
    assert!(stdout(&o).contains("create table dbo.t"), "{}", stdout(&o));
}

#[test]
fn an_ambiguous_rename_is_blocked_and_the_exact_command_is_printed() {
    let d = Demo::new("ambig");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  ident: {type: bigint, nullable: false}\n");
    let o = d.run(&["plan"]);

    assert_eq!(
        code(&o),
        FINDING,
        "an ambiguity must exit as a finding, or CI cannot block on it"
    );
    let msg = stderr(&o);
    assert!(
        msg.contains("pbps rename dbo.t.id ident"),
        "the message must contain a copy-pastable command: {msg}"
    );
    assert!(
        msg.contains("pbps drop dbo.t.id"),
        "the drop option must be offered too: {msg}"
    );
}

#[test]
fn recording_the_intent_resolves_the_rename() {
    let d = Demo::new("rename");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();
    let before = std::fs::read_to_string(d.ids_path()).unwrap();

    d.table("table: dbo.t\ncolumns:\n  ident: {type: bigint, nullable: false}\n");
    assert_eq!(code(&d.run(&["rename", "dbo.t.id", "ident"])), 0);

    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("rename column id -> ident"),
        "{}",
        stdout(&o)
    );

    // A rename should touch exactly one line of the identity file; that is what
    // makes it reviewable.
    let after = std::fs::read_to_string(d.ids_path()).unwrap();
    let changed = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        changed, 1,
        "a rename should change exactly one line of the identity file\nbefore:\n{before}\nafter:\n{after}"
    );
}

#[test]
fn deleting_a_column_requires_a_reason_and_leaves_a_tombstone() {
    let d = Demo::new("drop");
    d.table("table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  pii: {type: nvarchar(20)}\n");
    d.run(&["plan"]);
    d.commit();

    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), FINDING, "a drop without a reason must be blocked");
    assert!(stderr(&o).contains("--reason"), "{}", stderr(&o));

    assert_eq!(
        code(&d.run(&["drop", "dbo.t.pii", "--reason", "REG-2026-042"])),
        0
    );
    let ids = std::fs::read_to_string(d.ids_path()).unwrap();
    assert!(
        ids.contains("tombstones"),
        "a tombstone should be left: {ids}"
    );
    assert!(
        ids.contains("REG-2026-042"),
        "the tombstone must record the reason: {ids}"
    );
    assert!(
        ids.contains(r#""operator": "demo""#),
        "audit identity must come from the project repository, not the caller's cwd: {ids}"
    );
    assert!(
        !std::fs::read_to_string(d.dir.join("schema/dbo.t.yml"))
            .unwrap()
            .contains("pii"),
        "the declarations must not keep a zombie column"
    );

    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("destructive"), "{}", stdout(&o));
}

#[test]
fn check_mode_fails_when_the_ids_file_is_stale_and_never_writes() {
    let d = Demo::new("check");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    // A developer added a column without running plan locally.
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  extra: {type: int}\n",
    );
    let before = std::fs::read_to_string(d.ids_path()).unwrap();

    let o = d.run(&["plan", "--check"]);
    assert_eq!(code(&o), FINDING);
    assert!(
        stderr(&o).contains("the identity file is out of date"),
        "{}",
        stderr(&o)
    );
    assert_eq!(
        std::fs::read_to_string(d.ids_path()).unwrap(),
        before,
        "--check must never write a file"
    );

    // It passes once plan has been run locally.
    assert_eq!(code(&d.run(&["plan"])), 0);
    assert_eq!(code(&d.run(&["plan", "--check"])), 0);
}

#[test]
fn narrowing_a_type_is_reported_as_a_risk() {
    let d = Demo::new("narrow");
    d.table("table: dbo.t\ncolumns:\n  a: {type: nvarchar(200)}\n");
    d.run(&["plan"]);
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  a: {type: nvarchar(50)}\n");
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("narrowing"), "{s}");
    assert!(
        s.contains("--allow narrowing"),
        "the user must be told how to approve it: {s}"
    );
}

#[test]
fn adding_a_required_column_to_an_existing_table_is_gated() {
    let d = Demo::new("add-required");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  required: {type: int, nullable: false}\n",
    );
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("not-null"), "{text}");
    assert!(text.contains("--allow not-null"), "{text}");
}

#[test]
fn a_null_default_does_not_bypass_the_required_column_gate() {
    let d = Demo::new("add-required-null-default");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  required: {type: int, nullable: false, default: \"CAST(NULL AS int)\"}\n",
    );
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("not-null"), "{text}");
    assert!(text.contains("--allow not-null"), "{text}");
}

/// A YAML double-quoted scalar can carry a bare `\r`, and the engine ends a
/// `--` comment there. Read as one long comment, the `NULL` after it vanished
/// from the value-source scan, the gate was skipped, and the apply failed on
/// the first existing row.
#[test]
fn a_null_default_behind_a_carriage_return_does_not_bypass_the_required_column_gate() {
    let d = Demo::new("add-required-cr-null-default");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  required: {type: int, nullable: false, default: \"-- carried over\\rNULL\"}\n",
    );
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("not-null"), "{text}");
    assert!(text.contains("--allow not-null"), "{text}");
}

#[test]
fn an_invalid_declaration_is_rejected() {
    let d = Demo::new("invalid");
    d.table("table: no_schema_prefix\ncolumns:\n  a: {type: int}\n");
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), FINDING);
}

/// An offline plan is a preview and the file has to say so in its own terms
/// (SPEC §7.3): `apply` decides by reading `origin`, not by noticing that
/// something is missing.
#[test]
fn plan_writes_a_preview_plan_as_json() {
    let d = Demo::new("out");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  b: {type: int}\n");
    let out = d.dir.join("plan.json");
    let o = d.run(&["plan", "--out", out.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(json["changes"]["changes"][0]["op"], "add_column");
    assert_eq!(json["origin"], "preview");
    assert_eq!(json["dialect"], "mssql");
    // The identity mapping travels with the plan, so an apply needs nothing
    // else; and the baseline carries the fingerprint apply compares against.
    assert!(json["ids"]["tables"].is_object(), "{json}");
    assert_eq!(
        json["baseline"]["checksum"].as_str().unwrap().len(),
        64,
        "{json}"
    );
}

#[test]
fn a_directory_without_a_project_file_is_reported_clearly() {
    let dir = std::env::temp_dir().join(format!("pbps-noproj-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let o = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("pbps.yml"), "{}", stderr(&o));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- init (SPEC 14.2) ----

#[test]
fn init_creates_a_complete_valid_project_without_an_existing_config() {
    let dir = std::env::temp_dir().join(format!("pbps-init-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .args(["init", "--env", "qa-east"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(dir.join("pbps.yml").is_file());
    assert!(dir.join("schema").is_dir());
    assert!(dir.join("schema.ids.json").is_file());
    let config = std::fs::read_to_string(dir.join("pbps.yml")).unwrap();
    assert!(config.contains("QA_EAST_CONN"), "{config}");
    assert!(config.contains("config schema 1"), "{config}");

    let validate = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(code(&validate), 0, "{}", stderr(&validate));
    assert!(stdout(&o).contains("Next: `pbps validate`"));
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pbps-init-")),
        "the staging directory must not survive a successful init"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// git tracks files, not directories. A project initialized with no
/// declarations used to lose `schema/` on the first clone, and every command
/// then failed on a directory the user had never deleted.
#[test]
fn an_initialized_project_survives_a_clone() {
    let root = std::env::temp_dir().join(format!("pbps-init-clone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let origin = root.join("origin");

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&origin)
        .args(["init", "--env", "prod"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(origin.join("schema/.gitkeep").is_file());
    // The listing must name every file init creates, or the preview is not one.
    assert!(stdout(&o).contains(".gitkeep"), "{}", stdout(&o));

    let git = |args: &[&str], cwd: &std::path::Path| {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
    };
    git(&["init", "-q"], &origin);
    git(&["config", "user.email", "d@e.f"], &origin);
    git(&["config", "user.name", "demo"], &origin);
    git(&["add", "-A"], &origin);
    git(&["commit", "-qm", "init"], &origin);

    let clone = root.join("clone");
    git(
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
        &root,
    );
    assert!(
        clone.join("schema").is_dir(),
        "the declaration directory did not survive the clone"
    );

    for command in ["validate", "plan"] {
        let o = Command::new(BIN)
            .arg("--project")
            .arg(&clone)
            .arg(command)
            .output()
            .unwrap();
        assert_eq!(code(&o), 0, "{command}: {}", stderr(&o));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The file init writes itself cannot be the thing that blocks init.
#[test]
fn init_accepts_a_declaration_directory_holding_only_an_empty_gitkeep() {
    let dir = std::env::temp_dir().join(format!("pbps-init-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("schema")).unwrap();
    std::fs::write(dir.join("schema/.gitkeep"), "").unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .args(["init", "--env", "prod"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(dir.join("pbps.yml").is_file());
    assert!(dir.join("schema/.gitkeep").is_file());
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pbps-init-")),
        "the staging directory must not survive a successful init"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Project::discover` walks the target's ancestors, so a lexical `..` used to
/// put the project being escaped from on that walk and refuse a sibling.
#[test]
fn init_into_a_sibling_directory_is_not_refused_by_the_project_it_escapes() {
    let root = std::env::temp_dir().join(format!("pbps-init-sibling-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let inside = root.join("a/b");
    std::fs::create_dir_all(&inside).unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&inside)
        .args(["init", "--env", "prod"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let o = Command::new(BIN)
        .args(["--project", "../newproj", "init", "--env", "dev"])
        .current_dir(&inside)
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(root.join("a/newproj/pbps.yml").is_file());

    // The guard itself still holds: a directory genuinely under the project is
    // refused.
    let o = Command::new(BIN)
        .args(["--project", "nested", "init", "--env", "dev"])
        .current_dir(&inside)
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    assert!(stderr(&o).contains("already inside"), "{}", stderr(&o));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn init_refuses_existing_files_without_changing_them() {
    let dir = std::env::temp_dir().join(format!("pbps-init-existing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("schema")).unwrap();
    // Non-YAML files matter too. A common `.gitkeep` used to pass the initial
    // check, then make the commit fail after staging and leave the staging tree
    // behind.
    let declaration = dir.join("schema/.gitkeep");
    std::fs::write(&declaration, "do not touch me\n").unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .arg("init")
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("will not overwrite"), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(&declaration).unwrap(),
        "do not touch me\n"
    );
    assert!(!dir.join("pbps.yml").exists());
    assert!(!dir.join("schema.ids.json").exists());
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pbps-init-")),
        "a refusal must not leave a staging directory"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_from_a_missing_connection_variable_leaves_no_project() {
    let dir = std::env::temp_dir().join(format!("pbps-init-no-conn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let var = format!("PBPS_INIT_MISSING_{}", std::process::id());

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .args(["init", "--from", "prod", "--url-env", &var])
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    let err = stderr(&o);
    assert!(err.contains(&var), "{err}");
    assert!(err.contains("Export it"), "{err}");
    assert!(!dir.join("pbps.yml").exists());
    assert!(!dir.join("schema.ids.json").exists());
    assert!(!dir.join("schema").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pull_accepts_the_pristine_identity_file_created_by_init() {
    let dir = std::env::temp_dir().join(format!("pbps-init-pull-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let init = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .args(["init", "--env", "source"])
        .output()
        .unwrap();
    assert_eq!(code(&init), 0, "{}", stderr(&init));

    let pull = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .args([
            "pull",
            "--db",
            "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&pull), 1);
    let err = stderr(&pull);
    assert!(!err.contains("already has declarations"), "{err}");
    assert!(err.contains("cannot reach"), "pull should connect: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A jump-version deploy: with an environment several versions behind, a rename
/// must still read as a rename. This is the property the uid matching design
/// exists to protect, checked once more against real files.
#[test]
fn a_rename_survives_across_versions() {
    let d = Demo::new("jump");
    d.table("table: dbo.t\ncolumns:\n  old_name: {type: nvarchar(50)}\n");
    d.run(&["plan"]);
    d.commit();
    let v1 = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&d.dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    // v2: the rename.
    d.table("table: dbo.t\ncolumns:\n  new_name: {type: nvarchar(50)}\n");
    d.run(&["rename", "dbo.t.old_name", "new_name"]);
    d.run(&["plan"]);
    d.commit();

    // v3: just a longer column, with no intent at all.
    d.table("table: dbo.t\ncolumns:\n  new_name: {type: nvarchar(200)}\n");
    d.run(&["plan"]);
    d.commit();

    // Compute a plan for an environment stuck at v1.
    let o = d.run(&["plan", "--since", &v1]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let s = stdout(&o);
    assert!(
        s.contains("rename column old_name -> new_name"),
        "across versions a rename must still read as a rename: {s}"
    );
    assert!(
        !s.contains("drop column"),
        "it must never degrade into a data-losing plan: {s}"
    );
}

#[test]
fn baseline_can_come_from_a_snapshot_file_without_git() {
    let d = Demo::new("snapshot");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);

    // Assemble a state snapshot by hand; Phase 3's pbps snapshot will produce it.
    let ids: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.ids_path()).unwrap()).unwrap();
    let snap = serde_json::json!({
        "version": 4,
        "kind": "baseline",
        "schema": { "tables": { "dbo.t": { "columns": {
            "id": { "type": "bigint", "nullable": false }
        }}}},
        "ids": ids,
        "operator": "demo"
    });
    let path = d.dir.join("base.json");
    std::fs::write(&path, snap.to_string()).unwrap();

    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  extra: {type: int}\n",
    );
    let o = d.run(&["plan", "--base", path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("add column extra"), "{}", stdout(&o));
}

#[test]
fn fmt_normalises_and_check_mode_never_writes() {
    let d = Demo::new("fmt");
    let messy = "table:    dbo.t\ncolumns:\n  id: {type: BIGINT, nullable: false}\n";
    d.table(messy);

    let o = d.run(&["fmt", "--check"]);
    assert_eq!(
        code(&o),
        FINDING,
        "a file that is not canonical should exit as a finding"
    );
    assert_eq!(
        std::fs::read_to_string(d.dir.join("schema/dbo.t.yml")).unwrap(),
        messy,
        "--check must never write a file"
    );

    assert_eq!(code(&d.run(&["fmt"])), 0);
    let after = std::fs::read_to_string(d.dir.join("schema/dbo.t.yml")).unwrap();
    assert!(
        after.contains("type: bigint"),
        "the type should be normalized: {after}"
    );
    assert_eq!(
        code(&d.run(&["fmt", "--check"])),
        0,
        "it should pass once rewritten"
    );
}

/// The split of side effects in SPEC §6.2: plan absorbs the annotation into the
/// ids file but never touches the YAML; stripping the now-redundant line is
/// fmt's job, and fmt must not strip one whose fact is not absorbed yet.
#[test]
fn fmt_strips_a_renamed_from_only_after_plan_absorbs_it() {
    let d = Demo::new("strip");
    d.table("table: dbo.t\ncolumns:\n  old_name: {type: nvarchar(50)}\n");
    d.run(&["plan"]);
    d.commit();

    let annotated = "table: dbo.t\n\ncolumns:\n  new_name:\n    type: nvarchar(50)\n    renamed_from: old_name\n";
    d.table(annotated);

    // The intent is still pending: fmt must keep the annotation, or the rename
    // would silently degrade into an ambiguity at the next plan.
    assert_eq!(code(&d.run(&["fmt"])), 0);
    let kept = std::fs::read_to_string(d.dir.join("schema/dbo.t.yml")).unwrap();
    assert!(
        kept.contains("renamed_from: old_name"),
        "a pending annotation must survive fmt: {kept}"
    );

    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        std::fs::read_to_string(d.dir.join("schema/dbo.t.yml"))
            .unwrap()
            .contains("renamed_from"),
        "plan must never rewrite the user's YAML"
    );

    // Absorbed now: fmt reports the file as non-canonical, then strips it.
    assert_eq!(code(&d.run(&["fmt", "--check"])), FINDING);
    assert_eq!(code(&d.run(&["fmt"])), 0);
    let stripped = std::fs::read_to_string(d.dir.join("schema/dbo.t.yml")).unwrap();
    assert!(
        !stripped.contains("renamed_from"),
        "an absorbed annotation is redundant and must be stripped: {stripped}"
    );
    assert!(stripped.contains("new_name"), "{stripped}");

    // The stripped file still plans cleanly — the fact lives in the ids file.
    assert_eq!(code(&d.run(&["plan", "--check"])), 0);
}

/// SPEC §5.3: two branches each add a same-named column, each hands out its own
/// uid, and git auto-merges the two lines cleanly. validate is the only thing
/// that can catch the result.
#[test]
fn validate_rejects_one_name_mapped_to_two_uids() {
    let d = Demo::new("dupuid");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    assert_eq!(code(&d.run(&["validate"])), 0);

    // Simulate the auto-merge: a second uid pointing at the same column name.
    let mut ids: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.ids_path()).unwrap()).unwrap();
    ids["columns"]["c_zzzzzz"] = serde_json::json!("dbo.t.id");
    std::fs::write(d.ids_path(), ids.to_string()).unwrap();

    let o = d.run(&["validate"]);
    assert_eq!(
        code(&o),
        FINDING,
        "a scrambled identity file must fail validate"
    );
    let msg = stderr(&o);
    assert!(
        msg.contains("both point at"),
        "the duplicate must be named: {msg}"
    );
    assert!(
        msg.contains("decide which uid survives"),
        "the user must be told the remedy, since no algorithm can pick: {msg}"
    );
}

/// A file the tool writes must read back: a bare `no` is a boolean to YAML.
#[test]
fn fmt_quotes_scalars_that_yaml_would_misread() {
    let d = Demo::new("quote");
    d.table("table: dbo.t\ncolumns:\n  id: {type: int, default: no, description: 0123}\n");
    assert_eq!(code(&d.run(&["fmt"])), 0);

    let after = std::fs::read_to_string(d.dir.join("schema/dbo.t.yml")).unwrap();
    assert!(after.contains(r#"default: "no""#), "{after}");
    assert!(after.contains(r#"description: "0123""#), "{after}");

    // It reads back, and the values are unchanged.
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}

// ---- Phase 2: the mssql dialect wired into plan and validate ----

/// `plan --sql` writes a runnable T-SQL preview, with the warning header.
#[test]
fn plan_sql_writes_a_tsql_preview() {
    let d = Demo::new("plan-sql");
    d.table(ONE_COLUMN);
    let sql_path = d.dir.join("preview.sql");
    let o = d.run(&["plan", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let script = std::fs::read_to_string(&sql_path).unwrap();
    assert!(
        script.contains("-- A preview, not an applyable plan"),
        "{script}"
    );
    assert!(
        script.contains("CREATE TABLE [dbo].[t] (\n    [id] bigint NOT NULL\n);"),
        "{script}"
    );
}

/// `strategy: online` has to survive the whole path — load, ids, diff, plan
/// file, emitter — or a user who declared it would get an exclusive-lock
/// rebuild on the large table they annotated to avoid exactly that (ADR-0003).
#[test]
fn an_online_strategy_reaches_the_emitted_sql_and_says_it_is_unverified() {
    let d = Demo::new("plan-sql-online");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.table(
        "table: dbo.t\nstrategy:\n  online: true\ncolumns:\n  id: {type: bigint, nullable: false}\nindexes:\n  ix_t_id:\n    columns: [id]\n",
    );
    let sql_path = d.dir.join("preview.sql");
    let o = d.run(&["plan", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let script = std::fs::read_to_string(&sql_path).unwrap();
    assert!(script.contains("WITH (ONLINE = ON);"), "{script}");
    // An offline plan cannot read the target's edition, and a preview that
    // reads as verified is the one thing worse than no preview.
    assert!(stdout(&o).contains("unverified"), "{}", stdout(&o));
}

/// A rename plans as sp_rename — proof the dialect, not a drop+add, is in charge.
#[test]
fn a_planned_rename_emits_sp_rename() {
    let d = Demo::new("plan-sql-rename");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  ident: {type: bigint, nullable: false, renamed_from: id}\n");
    let sql_path = d.dir.join("preview.sql");
    let o = d.run(&["plan", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let script = std::fs::read_to_string(&sql_path).unwrap();
    assert!(
        script.contains("EXEC sp_rename N'[dbo].[t].[id]', N'ident', 'COLUMN';"),
        "{script}"
    );
    assert!(
        !script.contains("DROP"),
        "a rename must not plan as drop+add: {script}"
    );
}

/// The dialect's type knowledge reaches diff: two spellings of one type are not
/// a change, and validate rejects a type the engine does not have.
#[test]
fn respelling_a_type_produces_no_plan() {
    let d = Demo::new("plan-respell");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  id: {type: BIGINT, nullable: false}\n");
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("No changes."), "{}", stdout(&o));
}

#[test]
fn validate_rejects_what_the_engine_would_refuse() {
    let d = Demo::new("validate-dialect");
    // A postgres type, and a primary key over a nullable column.
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: jsonb}\n  code: {type: int}\nprimary_key: [code]\n",
    );
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), FINDING);
    let err = stderr(&o);
    assert!(err.contains("has no type `jsonb`"), "{err}");
    assert!(err.contains("must be NOT NULL"), "{err}");
}

/// A narrowing type change carries the narrowing risk into the printed plan.
#[test]
fn a_narrowing_change_is_flagged_in_the_plan() {
    let d = Demo::new("plan-narrowing");
    d.table("table: dbo.t\ncolumns:\n  name: {type: 'nvarchar(100)'}\n");
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  name: {type: 'nvarchar(50)'}\n");
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("[narrowing]"), "{out}");
    assert!(out.contains("--allow narrowing"), "{out}");
}

// ---- pull (the paths that need no database) ----

/// pull must never clobber an existing project by accident: the connection is
/// not even attempted when declarations are already there.
#[test]
fn pull_refuses_to_overwrite_existing_declarations() {
    let d = Demo::new("pull-refuse");
    d.table(ONE_COLUMN);
    let o = d.run(&[
        "pull",
        "--db",
        "Server=nowhere.invalid,1433;Database=x;User Id=u;Password=p",
    ]);
    assert_eq!(code(&o), 1);
    let err = stderr(&o);
    assert!(err.contains("already has declarations"), "{err}");
    assert!(err.contains("--force"), "{err}");
    // Refusal happened before any connection attempt.
    assert!(!err.contains("nowhere.invalid"), "{err}");
}

/// An ids file that names only roles is identity state too: a role-only
/// project whose declaration directory is empty (a role file deleted by
/// mistake, say) must not have its `r_` mapping replaced by an unforced pull.
#[test]
fn pull_refuses_to_overwrite_role_identities() {
    let d = Demo::new("pull-refuse-roles");
    std::fs::write(
        d.ids_path(),
        "{\"version\":1,\"tables\":{},\"columns\":{},\"roles\":{\"r_aaaaaa\":\"app_reader\"}}\n",
    )
    .unwrap();
    let o = d.run(&[
        "pull",
        "--db",
        "Server=nowhere.invalid,1433;Database=x;User Id=u;Password=p",
    ]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("already has declarations"), "{err}");
    assert!(!err.contains("nowhere.invalid"), "{err}");
}

#[test]
fn pull_names_the_dialect_it_needs() {
    let d = Demo::new("pull-dialect");
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let o = d.run(&["pull", "--db", "Server=x;Database=y"]);
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains("only implemented for mssql"),
        "{}",
        stderr(&o)
    );
}

// ---- docs (SPEC 9.4) and the strategy block (ADR-0003) ----

/// Written with explicit `\n` rather than a `\`-continued literal: Rust strips
/// the leading whitespace of a continued line, which silently unindents YAML.
const DOCUMENTED: &str = concat!(
    "table: dbo.t\n",
    "description: A documented table.\n",
    "strategy:\n",
    "  online: true\n",
    "columns:\n",
    "  id: {type: bigint, nullable: false, description: Surrogate key.}\n",
    "  old: {type: int, deprecated: use id instead}\n",
    "primary_key: [id]\n",
);

#[test]
fn docs_renders_markdown_to_stdout_by_default() {
    let d = Demo::new("docs-md");
    d.table(DOCUMENTED);
    let o = d.run(&["docs"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("A documented table."), "{out}");
    assert!(out.contains("Surrogate key."), "{out}");
    assert!(out.contains("## Do not use"), "{out}");
    assert!(out.contains("use id instead"), "{out}");
    assert!(out.contains("```mermaid"), "{out}");
}

/// The air-gap rule applies to artifacts too: a page that needs the network to
/// look right is not self-contained.
#[test]
fn docs_html_is_one_self_contained_file() {
    let d = Demo::new("docs-html");
    d.table(DOCUMENTED);
    let path = d.dir.join("schema.html");
    let o = d.run(&["docs", "--format", "html", "--out", path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let html = std::fs::read_to_string(&path).unwrap();
    assert!(html.starts_with("<!doctype html>"), "{html}");
    for forbidden in ["http://", "https://", "<script", "<link", "src="] {
        assert!(!html.contains(forbidden), "found `{forbidden}` in the page");
    }
    // Progress goes to stderr so a piped document is never corrupted.
    assert!(stdout(&o).is_empty(), "stdout: {}", stdout(&o));
}

/// Identical declarations must produce byte-identical files, or every docs run
/// would show up as a diff.
#[test]
fn docs_output_is_deterministic() {
    let d = Demo::new("docs-determinism");
    d.table(DOCUMENTED);
    let first = stdout(&d.run(&["docs"]));
    for _ in 0..3 {
        assert_eq!(stdout(&d.run(&["docs"])), first);
    }
    assert!(!first.is_empty());
}

#[test]
fn docs_names_the_formats_it_has() {
    let d = Demo::new("docs-bad-format");
    d.table(ONE_COLUMN);
    let o = d.run(&["docs", "--format", "pdf"]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("markdown"), "{}", stderr(&o));
}

/// A project that has never run `plan` has no ids file; documentation must not
/// be the one command that fails on it.
#[test]
fn docs_works_before_the_ids_file_exists() {
    let d = Demo::new("docs-no-ids");
    d.table(ONE_COLUMN);
    assert!(!d.ids_path().exists());
    let o = d.run(&["docs", "--format", "erd"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).starts_with("erDiagram"), "{}", stdout(&o));
}

/// Unlike `renamed_from`, a strategy is persistent: fmt must not quietly turn an
/// online alter into a blocking one.
#[test]
fn fmt_preserves_the_strategy_block() {
    let d = Demo::new("fmt-strategy");
    d.table(DOCUMENTED);
    let o = d.run(&["fmt"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = std::fs::read_to_string(d.dir.join("schema/dbo.t.yml")).unwrap();
    assert!(text.contains("strategy:\n  online: true"), "{text}");

    // And it is a fixpoint: a second fmt changes nothing.
    let o = d.run(&["fmt", "--check"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}

/// ADR-0003: a typo must not silently become a no-op.
#[test]
fn an_unknown_strategy_key_is_rejected() {
    let d = Demo::new("strategy-typo");
    d.table("table: dbo.t\nstrategy:\n  onlnie: true\ncolumns:\n  id: {type: int}\n");
    let o = d.run(&["validate"]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("onlnie"), "{}", stderr(&o));
}

/// SPEC §7.3: "a plan computed offline is always a preview and is never
/// accepted by `apply`". The refusal happens before anything connects, which is
/// what makes it testable here — and what makes it a property of the artifact
/// rather than of the deployment.
#[test]
fn apply_refuses_an_offline_preview_without_ever_connecting() {
    let d = Demo::new("preview");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    d.table("table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  b: {type: int}\n");
    let plan = d.dir.join("preview.json");
    d.run(&["plan", "--out", plan.to_str().unwrap()]);
    let checksum = plan_checksum(&plan);

    let o = d.run(&[
        "apply",
        // A port nothing listens on, so a connection that should never be
        // attempted is refused instantly rather than waiting out a timeout.
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "destructive",
    ]);
    assert_eq!(code(&o), 1);
    let err = stderr(&o);
    assert!(err.contains("preview"), "{err}");
    assert!(err.contains("plan --db"), "the remedy must be named: {err}");
    assert!(!err.contains("connect"), "it must not have tried: {err}");
}

/// The checksum shown to the reviewer is an input to apply, not merely an
/// audit value written afterwards. A different artifact must be rejected
/// before the target is contacted even when both files deserialize cleanly.
#[test]
fn apply_refuses_an_artifact_that_does_not_match_the_approved_checksum() {
    let d = Demo::new("apply-pin");
    d.table(ONE_COLUMN);
    let plan = write_plan(&d, "plan.json", "transactional");
    let actual = plan_checksum(&plan);
    let approved = if actual.starts_with('0') {
        "1".repeat(64)
    } else {
        "0".repeat(64)
    };

    let o = d.run(&[
        "apply",
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &approved,
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("approved checksum"), "{err}");
    assert!(err.contains("plan checksum now"), "{err}");
    assert!(
        !err.contains("connect"),
        "the target must not be contacted: {err}"
    );
}

/// `risks` is reviewer-facing redundant data. Recompute it from the typed
/// change so changing both the artifact and the command-line checksum cannot
/// turn a DROP into an ungated operation.
#[test]
fn apply_refuses_a_plan_whose_risks_were_removed() {
    let d = Demo::new("apply-risks");
    d.table(ONE_COLUMN);
    let plan_path = d.dir.join("riskless-drop.json");
    let plan = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "mssql",
        "2026-09-04T00:00:00Z",
        pbps_model::PlanBaseline {
            description: "test as queried".into(),
            checksum: "0".repeat(64),
        },
        pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange {
                change: pbps_model::Change::DropTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    name: "dbo.t".parse().unwrap(),
                },
                risks: Default::default(),
                strategy: Default::default(),
                findings: Default::default(),
            }],
        },
        pbps_model::IdsFile::default(),
    );
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    let checksum = plan.checksum();

    let o = d.run(&[
        "apply",
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--plan",
        plan_path.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("inconsistent risks"), "{err}");
    assert!(err.contains("destructive"), "{err}");
    assert!(
        !err.contains("connect"),
        "the target must not be contacted: {err}"
    );
}

/// `on_apply_attempt` is the append-only audit fan-out of SPEC 8.1, so it has
/// to see the attempts that were *refused* as well as the ones that ran: a
/// stale `--checksum` is exactly the event an audit sink exists to record.
/// Once the file has been read as a plan there is an artifact to report on,
/// and every refusal after that point is an attempt against it; before that
/// point there is no checksum, so nothing is reported.
#[test]
fn a_refused_artifact_is_still_an_attempt_for_the_hook() {
    let d = Demo::new("refused-attempt-hook");
    // Keep a space in the path: removing the quotes would otherwise make this
    // test pass while restoring the `cmd.exe /C` bug it exists to catch.
    let hook_out = d.dir.join("apply hook.json");
    // This is the one hook test that reaches the Windows CI runner (the
    // others need a live server), and it has been wrong about that runner
    // twice: a double-quoted YAML scalar read the `C:\Users\...` backslashes
    // as escapes and refused the whole config, and `cat` is not on `cmd`'s
    // PATH there. So: a single-quoted scalar, and `more`, which is built in
    // and copies stdin to stdout when neither is a console.
    let capture = if cfg!(windows) {
        format!("more > \"{}\"", hook_out.display())
    } else {
        format!("cat > \"{}\"", hook_out.display())
    };
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nhooks:\n  on_apply_attempt: '{capture}'\n"),
    )
    .unwrap();
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.table("table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  b: {type: int}\n");
    let plan = d.dir.join("preview.json");
    let previewed = d.run(&["plan", "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&previewed), 0, "{}", stderr(&previewed));
    let checksum = plan_checksum(&plan);
    // A port nothing listens on: none of these may reach a connection, and one
    // that tried would fail instantly rather than waiting out a timeout.
    const NOWHERE: &str = "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p";

    let hook = |what: &str, out: &Output| -> serde_json::Value {
        let raw = std::fs::read_to_string(&hook_out).unwrap_or_else(|e| {
            panic!(
                "{what}: the attempt hook did not run: {e}\nstderr was:\n{}",
                stderr(out)
            )
        });
        std::fs::remove_file(&hook_out).unwrap();
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {raw:?}"))
    };

    // The finding's case: the artifact is fine, the approval is not.
    let stale = d.run(&[
        "apply",
        "--db",
        NOWHERE,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        "0000000000000000000000000000000000000000000000000000000000000000",
    ]);
    assert_eq!(code(&stale), 1);
    assert!(
        stderr(&stale).contains("no longer matches the artifact approved"),
        "{}",
        stderr(&stale)
    );
    let event = hook("stale checksum", &stale);
    assert_eq!(event["outcome"], "failure", "{event}");
    assert_eq!(event["event"], "apply", "{event}");
    assert_eq!(
        event["checksum"], checksum,
        "the event carries the artifact's checksum, not the one typed: {event}"
    );
    assert_eq!(event["plan_path"], plan.to_str().unwrap(), "{event}");
    assert!(
        event["error"]
            .as_str()
            .is_some_and(|e| e.contains("no longer matches the artifact approved")),
        "{event}"
    );
    assert!(event.get("ledger_entry").is_none(), "{event}");

    // A refusal further down the same path — the preview check — reports too,
    // which is what makes this a property of the path and not of one check.
    let preview = d.run(&[
        "apply",
        "--db",
        NOWHERE,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_eq!(code(&preview), 1);
    assert!(stderr(&preview).contains("preview"), "{}", stderr(&preview));
    let event = hook("preview refusal", &preview);
    assert_eq!(event["outcome"], "failure", "{event}");
    assert!(
        event["error"]
            .as_str()
            .is_some_and(|e| e.contains("preview")),
        "{event}"
    );

    // Before the file is a plan there is no artifact and no checksum, so there
    // is nothing for the event to be about.
    let not_a_plan = d.dir.join("not-a-plan.json");
    std::fs::write(&not_a_plan, "{\"version\": \"a string\"}\n").unwrap();
    let unreadable = d.run(&[
        "apply",
        "--db",
        NOWHERE,
        "--plan",
        not_a_plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_eq!(code(&unreadable), 1);
    assert!(
        stderr(&unreadable).contains("is not a pbps plan"),
        "{}",
        stderr(&unreadable)
    );
    assert!(
        !hook_out.exists(),
        "a file that is not a plan has no checksum to report an attempt on"
    );
}

// ---- Phase 3.5: the module model (ADR-0002) ----

impl Demo {
    fn module(&self, file: &str, body: &str) {
        std::fs::write(self.dir.join("schema").join(file), body).unwrap();
    }
}

const A_VIEW: &str = "view: dbo.active_t\ndefinition: |-\n  SELECT id FROM dbo.t WHERE id > 0\n";

/// A module is part of the desired state, so it must reach the plan and the
/// SQL — and it must do so as `CREATE OR ALTER`, which preserves the
/// permissions granted on the object (ADR-0002).
#[test]
fn a_view_plans_as_create_or_alter() {
    let d = Demo::new("module-plan");
    d.table(ONE_COLUMN);
    d.module("v.yml", A_VIEW);

    let sql_path = d.dir.join("preview.sql");
    let o = d.run(&["plan", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("+ create view"), "{}", stdout(&o));

    let script = std::fs::read_to_string(&sql_path).unwrap();
    assert!(
        // No terminator is added: the stored text is what pbps sent, and a
        // semicolon the declaration did not have would come back as part of the
        // body and read as a change on every plan thereafter.
        script.contains(
            "CREATE OR ALTER VIEW [dbo].[active_t]\nAS\nSELECT id FROM dbo.t WHERE id > 0"
        ),
        "{script}"
    );
    // The view selects a column, so the table has to exist first.
    assert!(
        script.find("CREATE TABLE").unwrap() < script.find("CREATE OR ALTER").unwrap(),
        "{script}"
    );
}

/// Modules carry no data, so they carry no identity: a removed one is a drop
/// that needs no tombstone and no reason — but it still faces the gate, because
/// what it destroys is the validity of whatever depends on it.
#[test]
fn a_removed_module_drops_without_intent_but_needs_approval() {
    let d = Demo::new("module-drop");
    d.table(ONE_COLUMN);
    d.module("v.yml", A_VIEW);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    std::fs::remove_file(d.dir.join("schema/v.yml")).unwrap();
    let o = d.run(&["plan"]);
    assert_eq!(
        code(&o),
        0,
        "a module drop must not need intent recorded first: {}",
        stderr(&o)
    );
    assert!(stdout(&o).contains("- drop view"), "{}", stdout(&o));
    assert!(stdout(&o).contains("--allow destructive"), "{}", stdout(&o));
    assert!(
        !std::fs::read_to_string(d.ids_path())
            .unwrap()
            .contains("active_t"),
        "a module must never enter the identity file"
    );
}

/// Reindenting a definition is not a change. A tool that re-stated every view
/// on every deploy would teach reviewers to skim the plan.
#[test]
fn reformatting_a_definition_is_not_a_change() {
    let d = Demo::new("module-noop");
    d.table(ONE_COLUMN);
    d.module("v.yml", A_VIEW);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.module(
        "v.yml",
        "view: dbo.active_t\ndefinition: |-\n  SELECT id\n  FROM dbo.t\n  WHERE id > 0\n",
    );
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("No changes."), "{}", stdout(&o));
}

/// SQL Server keeps tables and modules in one namespace per schema, so a view
/// named after a table is a collision the engine would only report at apply
/// time — on a database that is already half-changed.
#[test]
fn a_module_named_after_a_table_is_refused() {
    let d = Demo::new("module-clash");
    d.table(ONE_COLUMN);
    d.module("v.yml", "view: dbo.t\ndefinition: SELECT 1\n");
    let o = d.run(&["validate"]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("already declared"), "{}", stderr(&o));
}

/// A trigger has to name a table that is actually managed here, or pbps would
/// be maintaining a trigger on an object it knows nothing about.
#[test]
fn a_trigger_on_an_undeclared_table_is_refused() {
    let d = Demo::new("module-trigger");
    d.table(ONE_COLUMN);
    d.module(
        "trg.yml",
        "trigger: dbo.trg_audit\non: dbo.absent\ndefinition: |-\n  AFTER INSERT AS SELECT 1;\n",
    );
    let o = d.run(&["validate"]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("not declared here"), "{}", stderr(&o));
}

/// `fmt` owns the file format for modules too, and a definition is exactly the
/// kind of text YAML quoting mangles.
#[test]
fn fmt_canonicalises_a_module_file() {
    let d = Demo::new("module-fmt");
    d.table(ONE_COLUMN);
    d.module(
        "v.yml",
        "view: dbo.active_t\ndefinition: \"SELECT id FROM dbo.t\"\n",
    );
    let o = d.run(&["fmt"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let written = std::fs::read_to_string(d.dir.join("schema/v.yml")).unwrap();
    assert!(written.contains("definition: |-"), "{written}");
    assert_eq!(
        code(&d.run(&["fmt", "--check"])),
        0,
        "fmt is not idempotent"
    );
}

// ---- Phase 3.5: staged apply (ADR-0003 decision 2) ----

/// Staged execution is a property of a plan that is going to be applied, and an
/// offline plan never is. Accepting the flag there would write "staged" into a
/// preview nobody can apply.
#[test]
fn staged_needs_a_target() {
    let d = Demo::new("staged-offline");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan", "--staged"]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("--db or --env"), "{}", stderr(&o));
}

/// Writes a plan file by hand: the mode check is a property of the artifact,
/// and it is made before the plan's contents matter at all — so an empty change
/// list is enough, and nothing here has to connect.
fn write_plan(d: &Demo, name: &str, mode: &str) -> PathBuf {
    let path = d.dir.join(name);
    let version = pbps_model::plan::CURRENT_VERSION;
    let plan = format!(
        r#"{{
  "version": {version},
  "origin": "database",
  "mode": "{mode}",
  "dialect": "mssql",
  "created_at": "2026-08-31T09:00:00Z",
  "baseline": {{ "description": "prod as queried (entry #1)", "checksum": "deadbeef" }},
  "changes": {{ "changes": [] }},
  "ids": {{ "version": 1, "tables": {{}}, "columns": {{}} }}
}}
"#
    );
    std::fs::write(&path, plan).unwrap();
    path
}

/// The mode lives in the file because that is what the deployment gate
/// approved; running whichever the operator typed would be the tool choosing
/// the loser of a disagreement.
#[test]
fn apply_refuses_a_mode_the_plan_does_not_declare() {
    let d = Demo::new("staged-mode");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);

    let unreachable = "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p";

    // A transactional plan applied with --staged.
    let plain = write_plan(&d, "plain.json", "transactional");
    let plain_checksum = plan_checksum(&plain);
    let o = d.run(&[
        "apply",
        "--db",
        unreachable,
        "--plan",
        plain.to_str().unwrap(),
        "--checksum",
        &plain_checksum,
        "--staged",
    ]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("transactional plan"), "{}", stderr(&o));

    // ...and a staged plan applied without it.
    let staged = write_plan(&d, "staged.json", "staged");
    let staged_checksum = plan_checksum(&staged);
    let o = d.run(&[
        "apply",
        "--db",
        unreachable,
        "--plan",
        staged.to_str().unwrap(),
        "--checksum",
        &staged_checksum,
    ]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("staged plan"), "{}", stderr(&o));
    assert!(
        !stderr(&o).contains("connect"),
        "the refusal must happen before connecting: {}",
        stderr(&o)
    );
}

/// `--resume` continues a staged apply, so asking for it on a transactional one
/// is a mistake worth naming rather than quietly ignoring.
#[test]
fn resume_without_staged_is_refused() {
    let d = Demo::new("staged-resume");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    let plain = write_plan(&d, "plain.json", "transactional");
    let o = d.run(&[
        "apply",
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--plan",
        plain.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plain),
        "--resume",
    ]);
    assert_ne!(code(&o), 0);
}

// ---- the optional dev database (SPEC §9.3) ----

/// A rehearsal answers a preview's question; `plan --db` produces the artifact
/// the deployment gate approves. Combining them would invite a dev-verified
/// plan to be read as a target-verified one.
#[test]
fn dev_and_db_are_refused_together() {
    let d = Demo::new("dev-and-db");
    d.table(ONE_COLUMN);
    let o = d.run(&[
        "plan",
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--dev",
        "docker://mcr.microsoft.com/mssql/server:2022-latest",
    ]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("separately"), "{}", stderr(&o));
}

/// The air-gap promise of §9.1: with no dev database configured, `plan` does
/// exactly what it always did and says nothing about one.
#[test]
fn a_plan_without_a_dev_database_never_mentions_one() {
    let d = Demo::new("dev-absent");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!stdout(&o).contains("rehearsal"), "{}", stdout(&o));
}

/// `dev.url_env` names the variable, never the string. An unset one has to say
/// which variable and offer the flag, or a developer is left guessing.
#[test]
fn an_unset_dev_variable_names_itself() {
    let d = Demo::new("dev-env");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\ndev:\n  url_env: PBPS_DEV_CONN_ABSENT\n",
    )
    .unwrap();
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains("PBPS_DEV_CONN_ABSENT"),
        "{}",
        stderr(&o)
    );
}

/// The rehearsal itself, against a real engine: the declarations have to
/// compile and the plan has to converge on them (SPEC §9.3, §11.5 invariant 3).
///
/// `#[ignore]`d like the other live tests — run it with
/// `PBPS_TEST_DB=... cargo test -p pbps-cli --test flow -- --ignored`, or
/// through `scripts/live-tests.sh`, which sets the variable.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_rehearsal_against_a_real_engine_reports_convergence() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let d = Demo::new("dev-live");
    d.table(ONE_COLUMN);
    d.module("v.yml", A_VIEW);

    let o = d.run(&["plan", "--dev", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("Dev rehearsal:"), "{out}");
    assert!(out.contains("converges"), "{out}");
    // Edition honesty: a green rehearsal must not read as a promise about the
    // target, and the scratch database must be gone either way.
    assert!(out.contains("still a preview"), "{out}");
}

/// The first command in the Phase 3.1 journey has to work against a real
/// catalog, not merely construct an empty project offline. The configured test
/// database may contain any supported objects; init must render all of them and
/// leave a project that the ordinary loader accepts.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn init_from_a_real_database_produces_a_valid_project() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let dir = std::env::temp_dir().join(format!("pbps-init-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let var = format!("PBPS_INIT_LIVE_{}", std::process::id());

    let o = Command::new(BIN)
        .env(&var, &connection)
        .arg("--project")
        .arg(&dir)
        .args(["init", "--from", "source", "--url-env", &var])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("pbps baseline --env source --reason initial-adoption"),
        "{}",
        stdout(&o)
    );
    let all = format!("{}{}", stdout(&o), stderr(&o));
    assert!(
        !all.contains(&connection),
        "connection string leaked: {all}"
    );

    let validate = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(code(&validate), 0, "{}", stderr(&validate));
    assert!(dir.join("schema.ids.json").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A password must not reach a log even when the command fails, and the failure
/// message is the easiest place to leak one.
#[test]
fn a_connection_string_never_appears_in_output() {
    let d = Demo::new("secret");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);

    let o = d.run(&[
        "verify",
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=hunter2",
    ]);
    assert_eq!(code(&o), 1);
    let all = format!("{}{}", stderr(&o), String::from_utf8_lossy(&o.stdout));
    assert!(!all.contains("hunter2"), "{all}");
}

/// A project with no environments must explain how to add one rather than
/// print an empty table.
#[test]
fn status_without_environments_says_how_to_configure_them() {
    let d = Demo::new("noenv");
    d.table(ONE_COLUMN);
    let o = d.run(&["status"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("url_env"), "{out}");
}

/// `--check` never connects, so pairing it with a target is a mistake worth
/// naming rather than quietly resolving one way or the other.
#[test]
fn plan_check_and_db_are_refused_together() {
    let d = Demo::new("checkdb");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan", "--check", "--db", "Server=x;Database=y"]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("--check"), "{}", stderr(&o));
}

/// The project does not have to be the repo root, and the baseline's git paths
/// have to be right when it is not.
///
/// The regression this pins is platform-shaped: computing the path by
/// canonicalizing and stripping the toplevel prefix works on Linux and fails on
/// Windows, where `canonicalize` returns a `\\?\` verbatim path and the temp
/// directory arrives in 8.3 short form. What reached `git show` was then an
/// absolute path, which resolves to the root tree — and a tree listing parsed
/// as JSON reports "the identity file is malformed", naming nothing that would
/// lead anyone to the path. A nested project exercises the same conversion on
/// every platform.
#[test]
fn a_project_below_the_repo_root_reads_its_own_baseline() {
    let d = Demo::nested("nested", "db/app");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // The second plan is the one that has to read the first one's ids file back
    // out of git: an empty baseline would report the table as newly created.
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  extra: {type: int}\n",
    );
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("extra"), "{s}");
    assert!(
        !s.to_lowercase().contains("create table"),
        "the baseline was not found; the plan re-creates the table:\n{s}"
    );
}

/// `--check` is the file check CI runs: it changes nothing and connects to
/// nothing. Refusing an explicit `--dev` is not enough — a `dev:` block in
/// pbps.yml would otherwise start a container behind the same command.
#[test]
fn check_mode_ignores_a_configured_dev_database() {
    let d = Demo::new("checkdev");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\ndev:\n  url_env: PBPS_NO_SUCH_VARIABLE\n",
    )
    .unwrap();
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    // The variable is deliberately unset: reaching `dev::spec` at all fails on
    // it, so a pass proves the dev database was never resolved.
    let o = d.run(&["plan", "--check"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        !stderr(&o).contains("PBPS_NO_SUCH_VARIABLE"),
        "{}",
        stderr(&o)
    );

    // The flag is still refused outright, because asking for one explicitly and
    // being silently ignored is worse than being told.
    let o = d.run(&["plan", "--check", "--dev", "docker://mssql"]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("--check"), "{}", stderr(&o));
}

/// `depends_on:` exists for the edges the identifier scan cannot see. Bootstrap
/// builds from nothing and so drops the online strategies, but dropping the
/// dependencies with them would make it emit a dependent module first and fail
/// on declarations that plan perfectly well.
#[test]
fn bootstrap_honours_declared_module_dependencies() {
    let d = Demo::new("bootdeps");
    d.table(ONE_COLUMN);
    // Neither definition names the other, so only `depends_on:` can order them.
    d.module(
        "dbo.first.yml",
        "view: dbo.first\ndefinition: |-\n  SELECT id FROM dbo.t\n",
    );
    d.module(
        "dbo.second.yml",
        "view: dbo.second\ndepends_on: [dbo.first]\ndefinition: |-\n  SELECT id FROM dbo.t\n",
    );
    d.run(&["plan"]);

    let sql_path = d.dir.join("boot.sql");
    let o = d.run(&["bootstrap", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let sql = std::fs::read_to_string(&sql_path).unwrap();
    let first = sql.find("[dbo].[first]").expect(&sql);
    let second = sql.find("[dbo].[second]").expect(&sql);
    assert!(first < second, "dependency order was discarded:\n{sql}");
}

/// Bootstrap builds what the identity file knows; a declared role (or
/// table) it does not know would be skipped silently, and the empty state
/// recorded as the whole one. Refused by name, with `pbps plan` as the
/// remedy (DECISIONS 109).
#[test]
fn bootstrap_refuses_a_declared_object_the_identity_file_does_not_know() {
    let d = Demo::new("bootids");
    std::fs::create_dir_all(d.dir.join("schema").join("roles")).unwrap();
    std::fs::write(
        d.dir.join("schema").join("roles").join("reporting.yml"),
        "role: reporting\ngrants:\n  schema::dbo: [select]\n",
    )
    .unwrap();
    // A role-only project, never planned: the identity file is empty.
    let sql_path = d.dir.join("boot.sql");
    let o = d.run(&["bootstrap", "--sql", sql_path.to_str().unwrap()]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("role reporting") && stderr(&o).contains("pbps plan"),
        "{}",
        stderr(&o)
    );
    assert!(!sql_path.exists(), "nothing was written");

    // Planned, it builds; a table added after that plan is refused the same
    // way, by name.
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["bootstrap", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        std::fs::read_to_string(&sql_path)
            .unwrap()
            .contains("CREATE ROLE [reporting]")
    );
    d.table(ONE_COLUMN);
    let o = d.run(&["bootstrap", "--sql", sql_path.to_str().unwrap()]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(stderr(&o).contains("dbo.t"), "{}", stderr(&o));
}

// ---- Phase 3.1: one machine-readable shape, and three exit codes ----

/// The whole point of the JSON view is that a frontend can point at the line
/// without re-parsing a rendered diagnostic.
#[test]
fn validate_json_carries_the_id_the_file_and_the_line() {
    let d = Demo::new("vjson");
    // A semantic error rather than a dialect one: only the loader has a span,
    // because the dialect is span-free by design (constraint 1 in CLAUDE.md).
    d.table("table: dbo.t\ncolumns:\n  id: {type: bigint}\nindexes:\n  ix:\n    columns: [id sideways]\n");

    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();

    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "validate");
    assert_eq!(v["result"], "findings");
    let f = &v["findings"][0];
    assert_eq!(f["id"], "load.semantic", "{v}");
    assert_eq!(f["severity"], "error");
    assert!(
        f["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("dbo.t.yml"),
        "{v}"
    );
    assert_eq!(f["location"]["line"], 6, "{v}");
}

/// A successful run must still produce the envelope: a consumer that only ever
/// sees JSON when something is wrong cannot tell "clean" from "did not run".
#[test]
fn validate_json_of_a_clean_project_is_ok_with_no_findings() {
    let d = Demo::new("vjsonok");
    d.table(ONE_COLUMN);

    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "ok");
    assert_eq!(v["findings"].as_array().unwrap().len(), 0);
    assert_eq!(v["data"]["tables"], 1);
    assert_eq!(v["data"]["dialect"], "mssql");
}

/// `fmt --check` and `fmt` answer the same question and must not need two
/// parsers; `mode` is what separates "must be fixed" from "was fixed".
#[test]
fn fmt_json_names_each_file_and_which_mode_ran() {
    let d = Demo::new("fjson");
    d.table("table:  dbo.t\ncolumns:\n  id:  {type: BIGINT}\n");

    let o = d.run(&["fmt", "--check", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["data"]["mode"], "check");
    assert_eq!(v["findings"][0]["id"], "fmt.not-canonical");
    assert_eq!(v["findings"][0]["remedy"], "pbps fmt");

    let o = d.run(&["fmt", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["data"]["mode"], "write");
    assert_eq!(v["result"], "ok", "a file that was fixed is not a finding");
    assert_eq!(v["findings"][0]["id"], "fmt.rewritten");
    assert_eq!(v["findings"][0]["severity"], "note");
}

/// The negative case the split exists for: a project that cannot be found is a
/// tool failure, not a finding, and the two must not share an exit code — a
/// drift-watch pipeline routes them to different people (SPEC §14.1).
#[test]
fn a_broken_declaration_and_a_missing_project_have_different_exit_codes() {
    let d = Demo::new("codes");
    d.table("table: no_schema_prefix\ncolumns:\n  a: {type: int}\n");
    assert_eq!(code(&d.run(&["validate"])), FINDING);

    let nowhere = std::env::temp_dir().join(format!("pbps-none-{}", std::process::id()));
    std::fs::create_dir_all(&nowhere).unwrap();
    let o = Command::new(BIN)
        .arg("--project")
        .arg(&nowhere)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let _ = std::fs::remove_dir_all(&nowhere);
}

/// `--format text` was the spelling the connected commands took before `human`
/// became the one word across all of them; a pipeline already passing it must
/// not break to gain a synonym.
#[test]
fn text_is_still_accepted_as_a_name_for_human() {
    let d = Demo::new("alias");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["validate", "--format", "text"])), 0);
    assert_eq!(code(&d.run(&["validate", "--format", "human"])), 0);
}

// ---- Phase 3.1: the plan summary and `explain` ----

/// A plan builder for the explain tests: two changes, three risk classes, one
/// table — enough that a summary and a grouped risk list have something to say.
fn risky_plan(d: &Demo) -> PathBuf {
    d.table(concat!(
        "table: dbo.customer\n",
        "columns:\n",
        "  id: {type: bigint, nullable: false}\n",
        "  pii: {type: nvarchar(50)}\n",
        "  code: {type: nvarchar(20)}\n",
        "primary_key: [id]\n"
    ));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.table(concat!(
        "table: dbo.customer\n",
        "columns:\n",
        "  id: {type: bigint, nullable: false}\n",
        "  code: {type: nvarchar(10), nullable: false}\n",
        "primary_key: [id]\n"
    ));
    assert_eq!(
        code(&d.run(&["drop", "dbo.customer.pii", "--reason", "REG-1"])),
        0
    );
    let path = d.dir.join("plan.json");
    let o = d.run(&["plan", "--out", path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    path
}

/// The first screenful has to say how big the plan is and what kind of trouble
/// it carries; a sixty-table plan and a one-table drop must not open the same
/// way.
#[test]
fn every_plan_opens_with_a_summary_of_its_size_and_risks() {
    let d = Demo::new("summary");
    risky_plan(&d);
    let out = stdout(&d.run(&["plan"]));

    assert!(out.contains("2 change(s) across 1 table(s)"), "{out}");
    assert!(out.contains("destructive"), "{out}");
    assert!(
        out.contains("data is lost, and no plan brings it back"),
        "the class must be explained, not just named: {out}"
    );
    // The summary comes before the change list, or it is not a summary.
    let summary_at = out.find("2 change(s) across").unwrap();
    let list_at = out.find("drop column pii").unwrap();
    assert!(summary_at < list_at, "{out}");
}

/// A plan with nothing risky in it must say so, rather than leaving the reader
/// to notice an absence.
#[test]
fn a_plan_with_no_risk_says_it_needs_no_allow() {
    let d = Demo::new("norisk");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  extra: {type: int}\n",
    );

    let out = stdout(&d.run(&["plan"]));
    assert!(out.contains("needs no --allow"), "{out}");
    assert!(!out.contains("needs approval to apply"), "{out}");
}

/// `explain` is for the reviewer at the deployment gate: no checkout, no
/// credentials, and every question they have to answer in one place.
#[test]
fn explain_answers_the_reviewers_questions_without_a_connection() {
    let d = Demo::new("explain");
    let plan = risky_plan(&d);

    let o = d.run(&["explain", "--plan", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);

    // What, why, how, and the exact command that approves it.
    assert!(out.contains("drop column pii"), "{out}");
    assert!(out.contains("data is lost"), "{out}");
    assert!(
        out.contains("one transaction, all or nothing"),
        "the execution mode must be stated: {out}"
    );
    // The risks are named with what each one means; the *approval command* is
    // asserted separately, on an applyable plan — this fixture is a preview,
    // which has nothing to approve.
    assert!(out.contains("destructive"), "{out}");
    assert!(out.contains("narrowing"), "{out}");
    assert!(out.contains("not-null"), "{out}");
    // The probes are what a reviewer most wants and cannot get from plan.sql.
    assert!(
        out.contains("Checks that run before the first statement"),
        "{out}"
    );
    assert!(out.contains("NOT NULL would reject"), "{out}");
    // And the one thing the file cannot answer must be named as unanswered
    // rather than quietly omitted.
    assert!(out.contains("Not checked: no --db or --env"), "{out}");
}

/// An offline plan is a preview, and a reviewer must not be able to read this
/// output and believe they are approving something applyable (SPEC §7.3).
#[test]
fn explain_says_a_preview_is_a_preview() {
    let d = Demo::new("explainprev");
    let plan = risky_plan(&d);
    let out = stdout(&d.run(&["explain", "--plan", plan.to_str().unwrap()]));
    assert!(out.contains("`apply` will refuse it"), "{out}");

    let v: serde_json::Value = serde_json::from_str(&stdout(&d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ])))
    .unwrap();
    assert_eq!(v["data"]["applyable"], false);
    assert_eq!(v["findings"][0]["id"], "plan.preview");
}

/// The checksum is what makes the approval mean something: it is what `apply`
/// recomputes, so the reviewer has to be shown the one they are approving.
#[test]
fn explain_json_carries_the_checksum_and_the_risk_detail() {
    let d = Demo::new("explainjson");
    let plan = risky_plan(&d);
    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();

    assert_eq!(v["command"], "explain");
    assert_eq!(v["data"]["mode"], "transactional");
    assert_eq!(v["data"]["change_count"], 2);
    assert_eq!(v["data"]["table_count"], 1);
    assert_eq!(v["data"]["checksum"].as_str().unwrap().len(), 64);
    assert_eq!(v["data"]["risks"][0]["class"], "destructive");
    assert!(!v["data"]["risks"][0]["why"].as_str().unwrap().is_empty());
    assert_eq!(v["data"]["probes"].as_array().unwrap().len(), 2);
    // No target was given, so the field must be absent rather than a guess.
    assert!(v["data"].get("target").is_none(), "{v}");
}

/// Explaining is not gating. A plan full of destructive changes is exactly what
/// this command exists to describe, and failing on one would make the
/// reviewer's own tool look broken in their terminal.
#[test]
fn explain_never_fails_on_a_risky_plan() {
    let d = Demo::new("explainexit");
    let plan = risky_plan(&d);
    assert_eq!(
        code(&d.run(&["explain", "--plan", plan.to_str().unwrap()])),
        0
    );

    // But a plan that is not there, or not a plan, is a tool failure.
    let o = d.run(&["explain", "--plan", "nowhere.json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    std::fs::write(d.dir.join("junk.json"), "{\"nope\": 1}").unwrap();
    let o = d.run(&[
        "explain",
        "--plan",
        d.dir.join("junk.json").to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
}

// ---- Phase 3.1: `doctor` ----

/// The point of the command: everything wrong with the project in one run,
/// rather than one thing per command over two days.
#[test]
fn doctor_reports_the_project_and_never_writes() {
    let d = Demo::new("doctor");
    d.table(ONE_COLUMN);
    d.commit();

    let o = d.run(&["doctor"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("pbps.yml"), "{out}");
    assert!(out.contains("1 table(s)"), "{out}");
    assert!(
        out.contains("no environments are configured"),
        "an unconfigured estate must be said out loud: {out}"
    );
    // Nothing this command does may create the identity file: it is the command
    // someone runs when they are not yet sure what they are pointed at.
    assert!(!d.ids_path().exists(), "doctor must not write");
}

/// `doctor` runs `validate` rather than a second copy of it, so a broken
/// declaration has to reach the report with the same id `validate` gives it.
#[test]
fn doctor_reports_the_same_finding_validate_would() {
    let d = Demo::new("doctorvalidate");
    d.table("table: dbo.t\ncolumns:\n  id: {type: jsonb}\n");

    let doctored: serde_json::Value =
        serde_json::from_str(&stdout(&d.run(&["doctor", "--format", "json"]))).unwrap();
    let validated: serde_json::Value =
        serde_json::from_str(&stdout(&d.run(&["validate", "--format", "json"]))).unwrap();

    assert_eq!(doctored["findings"][0]["id"], "dialect.rejected");
    assert_eq!(
        doctored["findings"][0]["message"], validated["findings"][0]["message"],
        "the two must not be able to disagree"
    );
    assert_eq!(code(&d.run(&["doctor"])), FINDING);
}

/// An environment naming a variable nobody exported is the single most common
/// first-run failure, and it must not take the rest of the report down with it.
#[test]
fn doctor_names_an_unset_connection_variable_without_echoing_anything() {
    let d = Demo::new("doctorenv");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_DOCTOR_UNSET\n",
    )
    .unwrap();

    let o = d.run(&["doctor"]);
    // Exit 1, not 2: `doctor` could not look at this environment, so it did not
    // answer its own question about it (SPEC 9.8, and see `doctor::outcome`).
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("PBPS_DOCTOR_UNSET"), "{out}");
    assert!(out.contains("unconfigured"), "{out}");
    // The project half of the report still ran.
    assert!(out.contains("1 table(s)"), "{out}");
}

/// The negative case for the split: a checkout with no commits and no checkout
/// at all have different remedies, and one message for both sends the user to
/// the wrong one.
#[test]
fn doctor_tells_an_empty_checkout_from_no_checkout() {
    let d = Demo::new("doctorgit");
    d.table(ONE_COLUMN);

    let out = stdout(&d.run(&["doctor"]));
    assert!(out.contains("no commits yet"), "{out}");
    assert!(out.contains("git add -A && git commit"), "{out}");
    assert!(!out.contains("not inside a git checkout"), "{out}");

    d.commit();
    let out = stdout(&d.run(&["doctor"]));
    assert!(!out.contains("no commits yet"), "{out}");
}

/// `doctor`'s permissions query has never met a real `sys.fn_my_permissions`
/// until this runs, and a catalog query that is wrong offline is wrong
/// silently: it returns an empty set, which `missing` reads as "this account
/// holds nothing" and reports as one error per required permission. Only a live
/// server can tell the two apart.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn doctor_against_a_real_server_reads_its_edition_and_permissions() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let d = Demo::new("doctor-live");
    d.table(ONE_COLUMN);
    d.commit();
    let var = format!("PBPS_DOCTOR_LIVE_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nenvironments:\n  test:\n    url_env: {var}\n"),
    )
    .unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["doctor", "--format", "json"])
        .env(&var, &connection)
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let env = &v["data"]["environments"][0];

    assert_eq!(env["environment"], "test", "{v}");
    assert!(
        env["server_version"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "{v}"
    );
    assert!(
        env["edition"].as_str().is_some_and(|s| !s.is_empty()),
        "{v}"
    );
    // The container runs Developer edition, which does support ONLINE. The
    // assertion is on the field being answered at all: `None` would mean the
    // SERVERPROPERTY read failed and the report silently said nothing.
    assert!(env["supports_online"].is_boolean(), "{v}");
    // The account the test container gives us is `sa`, which holds CONTROL.
    // Anything in this list means the permissions query came back empty or the
    // names it returns are not the ones `REQUIRED` spells.
    assert_eq!(
        env["missing_permissions"].as_array().unwrap().len(),
        0,
        "the deployment account should hold everything: {v}"
    );
    // A database with a ledger is ready; one without has never been touched.
    // Both are legitimate here, and neither is unreachable.
    assert!(
        matches!(env["state"].as_str(), Some("ready" | "uninitialized")),
        "{v}"
    );
    assert!(
        !stdout(&o).contains("Password"),
        "no part of a connection string may reach the report"
    );
}

// ---- Phase 3.1: the interactive prompt (SPEC 6.3) ----

/// The conversation itself is unit-tested in `prompt`; what only the real binary
/// can show is the wiring — that a terminal is what turns the prompt on, and
/// that the answer reaches the identity file through the ordinary resolve.
///
/// Linux only: this drives a pseudo-terminal through util-linux `script`, whose
/// flags differ on BSD and which does not exist on Windows. Asserting it on the
/// one platform where the harness is stable beats asserting it nowhere.
#[cfg(target_os = "linux")]
#[test]
fn a_terminal_turns_the_prompt_on_and_the_answer_is_recorded() {
    let d = Demo::new("prompt");
    d.table("table: dbo.t\ncolumns:\n  customer_name: {type: nvarchar(50)}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.table("table: dbo.t\ncolumns:\n  full_name: {type: nvarchar(50)}\n");

    let out = Command::new("script")
        .args([
            "-qec",
            &format!("{BIN} --project {} plan", d.dir.display()),
            "/dev/null",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("stdin was piped")
                .write_all(b"1\n")?;
            child.wait_with_output()
        });
    let Ok(out) = out else {
        // `script` is not installed. Skipping beats failing a suite over a
        // missing test fixture, and the conversation is covered by unit tests.
        return;
    };
    let text = stdout(&out);
    assert!(
        text.contains("customer_name was renamed to full_name"),
        "the likely rename must be offered first: {text}"
    );
    assert_eq!(code(&out), 0, "{text}");

    let ids = std::fs::read_to_string(d.ids_path()).unwrap();
    assert!(ids.contains("full_name"), "{ids}");
    // And the recorded answer must be a rename, not a drop-and-add: a tombstone
    // here would mean the prompt wrote something other than what was chosen.
    assert!(!ids.contains("tombstones"), "{ids}");
}

/// With no terminal — every CI run — the behaviour of SPEC 6.4 is unchanged, and
/// `--no-input` must not change it either. The flag declines a prompt; it can
/// never answer one (SPEC 14.3).
#[test]
fn without_a_terminal_nothing_is_asked_and_no_input_changes_nothing() {
    let d = Demo::new("noinput");
    d.table("table: dbo.t\ncolumns:\n  customer_name: {type: nvarchar(50)}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let before = std::fs::read_to_string(d.ids_path()).unwrap();
    d.table("table: dbo.t\ncolumns:\n  full_name: {type: nvarchar(50)}\n");

    for args in [&["plan"][..], &["--no-input", "plan"][..]] {
        let o = d.run(args);
        assert_eq!(code(&o), FINDING, "{args:?}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("pbps rename dbo.t.customer_name full_name"),
            "{args:?}: {}",
            stderr(&o)
        );
        assert_eq!(
            std::fs::read_to_string(d.ids_path()).unwrap(),
            before,
            "{args:?}: an unanswered question must record nothing"
        );
    }
}

// ---- Phase 3.1: editor schemas, completions, man pages ----

/// All three answer without a project. Requiring one would mean a user could
/// not install completions until after they had succeeded at the thing
/// completions are meant to help them do.
#[test]
fn the_integration_commands_need_no_project() {
    let nowhere = std::env::temp_dir().join(format!("pbps-noproj-{}", std::process::id()));
    std::fs::create_dir_all(&nowhere).unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .arg("--project")
            .arg(&nowhere)
            .args(args)
            .output()
            .unwrap()
    };

    let o = run(&["schema"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["title"], "pbps declaration");

    let o = run(&["schema", "--kind", "config"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("dialect"), "{}", stdout(&o));

    for shell in ["bash", "zsh", "fish", "powershell"] {
        let o = run(&["completions", shell]);
        assert_eq!(code(&o), 0, "{shell}: {}", stderr(&o));
        assert!(stdout(&o).contains("pbps"), "{shell}");
    }

    let man = nowhere.join("man");
    let o = run(&["man", "--out", man.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // One page per command: a single page documenting all of them is the page
    // nobody reads, and `man pbps-apply` is what an operator types.
    assert!(man.join("pbps.1").exists());
    assert!(man.join("pbps-apply.1").exists());
    assert!(man.join("pbps-doctor.1").exists());

    let _ = std::fs::remove_dir_all(&nowhere);
}

/// The schema exists to catch a typo before `validate` does, and that only
/// works if it refuses what the loader refuses.
#[test]
fn the_declaration_schema_describes_what_the_loader_accepts() {
    let d = Demo::new("schema");
    let o = d.run(&["schema"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();

    let table = &v["$defs"]["TableDto"];
    assert_eq!(table["additionalProperties"], serde_json::json!(false));
    // `columns` has no default in the loader, so the schema must require it —
    // an editor that accepted a table with no columns would bless a file every
    // later command fails on.
    let required: Vec<&str> = table["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert!(required.contains(&"table"), "{table}");
    assert!(required.contains(&"columns"), "{table}");
    // And the one-shot annotation must be describable, or an editor flags what
    // the format documents.
    assert!(table["properties"].get("renamed_from").is_some(), "{table}");
    assert!(table["properties"].get("strategy").is_some(), "{table}");
}

// ---- Review follow-ups on the Phase 3.1 PR ----

/// The command's one promise: a reviewer holding nothing but plan.json, in a
/// directory with no project, still gets the whole of the file's answer. The
/// plan names its own dialect, so there is nothing left to discover.
#[test]
fn explain_works_in_a_directory_with_no_project() {
    let d = Demo::new("explainbare");
    let plan = risky_plan(&d);

    let elsewhere = std::env::temp_dir().join(format!("pbps-reviewer-{}", std::process::id()));
    std::fs::create_dir_all(&elsewhere).unwrap();
    let o = Command::new(BIN)
        .arg("--project")
        .arg(&elsewhere)
        .args(["explain", "--plan", plan.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("drop column pii"), "{}", stdout(&o));
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// The approval command has to survive being pasted. `apply` requires exactly
/// one of --db / --env, so one printed without a target fails immediately —
/// which would make the single most important line of the report the one that
/// does not work.
#[test]
fn the_approval_command_explain_prints_carries_a_target() {
    let d = Demo::new("approvecmd");
    d.table(ONE_COLUMN);
    // An applyable plan, since a preview has no approval command at all.
    let plan = write_plan(&d, "target.json", "transactional");

    let out = stdout(&d.run(&["explain", "--plan", plan.to_str().unwrap()]));
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("pbps apply"))
        .unwrap_or_else(|| panic!("no approval command in:\n{out}"));
    assert!(line.contains("--plan"), "{line}");
    // A redacted --db label is not a connection string and must never be
    // printed as though it were; the placeholder is the honest form.
    assert!(line.contains("--env <environment>"), "{line}");
}

/// A project with no environments is the shape a consumer meets first, and it
/// must not be the one that arrives as a parse error (SPEC 9.8).
#[test]
fn status_json_stays_json_when_there_are_no_environments() {
    let d = Demo::new("statusempty");
    d.table(ONE_COLUMN);

    let o = d.run(&["status", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["command"], "status");
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
    assert_eq!(v["findings"][0]["id"], "project.no-environments");
    // Still `ok`: status is a report, not a gate, and it always exits 0.
    assert_eq!(v["result"], "ok");
}

/// The schema's whole justification is that it accepts exactly what the loader
/// accepts. A module file names one kind; a file with none, or with two, is
/// refused by `convert_module`, so the schema has to refuse it too or an editor
/// blesses a declaration `validate` cannot load.
#[test]
fn the_module_schema_demands_exactly_one_kind() {
    let d = Demo::new("moduleschema");
    let v: serde_json::Value = serde_json::from_str(&stdout(&d.run(&["schema"]))).unwrap();
    let branches = v["$defs"]["ModuleDto"]["oneOf"].as_array().unwrap();

    assert_eq!(branches.len(), 4, "{v}");
    for kind in ["view", "procedure", "function"] {
        let branch = branches
            .iter()
            .find(|b| b["required"] == serde_json::json!([kind]))
            .unwrap_or_else(|| panic!("no branch for {kind}: {v}"));
        // The type is pinned as well as the presence: `required` is satisfied
        // by an explicit null, which YAML writes as often as not (`view:` with
        // nothing after it), and the loader reads that as absent.
        assert_eq!(branch["properties"][kind]["type"], "string", "{branch}");
        // `on:` names the table a trigger fires on, and the loader rejects it
        // on everything else. Left optional here, the published schema blessed
        // a view with one — a document `pbps validate` refuses.
        assert_eq!(branch["properties"]["on"]["type"], "null", "{branch}");
        // And the other three kind keys, or `oneOf` stops doing the work it was
        // chosen for: with only `on:` constrained, `trigger` + `on` + `view`
        // was disqualified from this branch and unconstrained in the trigger
        // one, so it matched exactly one branch and was blessed.
        for other in ["view", "procedure", "function", "trigger"] {
            if other == kind {
                continue;
            }
            assert_eq!(branch["properties"][other]["type"], "null", "{branch}");
        }
    }
    // And the trigger branch requires it, which the loader also does: a trigger
    // that does not say which table it is on is refused.
    let trigger = branches
        .iter()
        .find(|b| b["required"] == serde_json::json!(["trigger", "on"]))
        .unwrap_or_else(|| panic!("no trigger branch requiring `on`: {v}"));
    assert_eq!(
        trigger["properties"]["trigger"]["type"], "string",
        "{trigger}"
    );
    assert_eq!(trigger["properties"]["on"]["type"], "string", "{trigger}");
    for other in ["view", "procedure", "function"] {
        assert_eq!(trigger["properties"][other]["type"], "null", "{trigger}");
    }
}

/// The hole the `on:` constraint opened, and the reason every branch has to
/// name the other kinds: `trigger` + `on` + `view` is disqualified from the
/// view branch by `on`, so with the trigger branch saying nothing about `view`
/// it matched exactly one branch — which is what `oneOf` calls valid.
/// `convert_module` refuses it, so an editor was blessing a file `pbps
/// validate` rejects.
#[test]
fn the_module_schema_refuses_a_second_kind_beside_a_trigger() {
    let d = Demo::new("modulekinds");
    d.table(ONE_COLUMN);

    for body in [
        "trigger: dbo.tr\non: dbo.t\nview: dbo.v\ndefinition: |-\n  AFTER INSERT AS SELECT 1\n",
        "view: dbo.v\ntrigger: dbo.tr\ndefinition: |-\n  SELECT id FROM dbo.t\n",
    ] {
        d.module("dbo.two.yml", body);
        let o = d.run(&["validate"]);
        assert_eq!(code(&o), 2, "{body}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("2 objects at once"),
            "{body}: {}",
            stderr(&o)
        );
    }
}

/// The other direction, and why the exclusions are `{"type": "null"}` rather
/// than `false`: YAML writes `procedure:` with nothing after it, serde reads
/// that as absent, and the loader accepts the file. `false` would have made the
/// schema stricter than the tool — a smaller failure than blessing what the
/// tool refuses, but the same disagreement.
#[test]
fn an_empty_kind_key_is_absent_to_both_the_loader_and_the_schema() {
    let d = Demo::new("modulenullkey");
    d.table(ONE_COLUMN);

    for body in [
        "view: dbo.v\nprocedure:\ndefinition: |-\n  SELECT id FROM dbo.t\n",
        "view: dbo.v\non:\ndefinition: |-\n  SELECT id FROM dbo.t\n",
        "trigger: dbo.tr\non: dbo.t\nview:\ndefinition: |-\n  AFTER INSERT AS SELECT 1\n",
    ] {
        d.module("dbo.one.yml", body);
        let o = d.run(&["validate"]);
        assert_eq!(code(&o), 0, "{body}: {}", stderr(&o));
    }
}

// ---- Second review round ----

/// One blocker is not one question. A table that lost two columns and gained
/// two arrives as a single `AmbiguousColumns` holding all four names, so
/// answering it once leaves the other pair ambiguous — and before the prompt
/// looped, the answer already given was discarded as well, which made a
/// two-column rename impossible to complete interactively.
#[cfg(target_os = "linux")]
#[test]
fn the_prompt_keeps_asking_until_every_ambiguity_is_answered() {
    let d = Demo::new("multiprompt");
    d.table(concat!(
        "table: dbo.t\n",
        "columns:\n",
        "  customer_name: {type: nvarchar(50)}\n",
        "  customer_zip: {type: nvarchar(10)}\n"
    ));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.table(concat!(
        "table: dbo.t\n",
        "columns:\n",
        "  full_name: {type: nvarchar(50)}\n",
        "  postcode: {type: nvarchar(10)}\n"
    ));

    let out = Command::new("script")
        .args([
            "-qec",
            &format!("{BIN} --project {} plan", d.dir.display()),
            "/dev/null",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            // The likeliest pairing is offered first each round, so "1" twice
            // is the ordinary answer to "yes, both of these".
            child
                .stdin
                .as_mut()
                .expect("stdin was piped")
                .write_all(b"1\n1\n")?;
            child.wait_with_output()
        });
    let Ok(out) = out else {
        return; // `script` is not installed; the conversation is unit-tested.
    };
    let text = stdout(&out);
    assert_eq!(code(&out), 0, "{text}");
    assert!(
        text.contains("rename column customer_name -> full_name"),
        "{text}"
    );
    assert!(
        text.contains("rename column customer_zip -> postcode"),
        "{text}"
    );

    let ids = std::fs::read_to_string(d.ids_path()).unwrap();
    assert!(
        ids.contains("full_name") && ids.contains("postcode"),
        "{ids}"
    );
    assert!(!ids.contains("tombstones"), "neither was a drop: {ids}");
}

/// Stopping part-way through must still record nothing, now that the loop
/// carries answers between rounds. A half-answered ambiguity written to the ids
/// file would be a decision the user never finished making.
#[cfg(target_os = "linux")]
#[test]
fn stopping_part_way_through_the_loop_still_records_nothing() {
    let d = Demo::new("multistop");
    d.table(concat!(
        "table: dbo.t\n",
        "columns:\n",
        "  customer_name: {type: nvarchar(50)}\n",
        "  customer_zip: {type: nvarchar(10)}\n"
    ));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let before = std::fs::read_to_string(d.ids_path()).unwrap();
    d.table(concat!(
        "table: dbo.t\n",
        "columns:\n",
        "  full_name: {type: nvarchar(50)}\n",
        "  postcode: {type: nvarchar(10)}\n"
    ));

    let out = Command::new("script")
        .args([
            "-qec",
            &format!("{BIN} --project {} plan", d.dir.display()),
            "/dev/null",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            // Answer the first, then decline the second.
            child
                .stdin
                .as_mut()
                .expect("stdin was piped")
                .write_all(b"1\n\n")?;
            child.wait_with_output()
        });
    let Ok(out) = out else {
        return;
    };
    assert_eq!(code(&out), FINDING, "{}", stdout(&out));
    assert_eq!(
        std::fs::read_to_string(d.ids_path()).unwrap(),
        before,
        "an unfinished answer must record nothing"
    );
    // And the commands cover the *whole* original problem, not just the part
    // the user had not reached. Nothing was recorded, so they are back where
    // they started; a list omitting the question they did answer would send
    // them to fix half of it.
    let text = stdout(&out);
    assert!(text.contains("pbps rename dbo.t.customer_name"), "{text}");
    assert!(text.contains("pbps rename dbo.t.customer_zip"), "{text}");
}

// ---- Third review round ----

/// An offline plan cannot be applied at all — `apply` refuses a `Preview`
/// structurally, whatever target and whatever `--allow` (SPEC 7.3). Printing an
/// apply command anyway had the report contradict its own first line and hand
/// the reviewer something that cannot work.
#[test]
fn explain_offers_no_apply_command_for_a_preview() {
    let d = Demo::new("previewcmd");
    let plan = risky_plan(&d);
    let out = stdout(&d.run(&["explain", "--plan", plan.to_str().unwrap()]));

    assert!(!out.contains("pbps apply"), "{out}");
    assert!(out.contains("This plan cannot be applied"), "{out}");
    // And it says what to do instead, or the reviewer is left with a refusal
    // and no next step.
    assert!(out.contains("pbps plan --env"), "{out}");

    let v: serde_json::Value = serde_json::from_str(&stdout(&d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ])))
    .unwrap();
    assert_eq!(v["data"]["applyable"], false);
    assert!(
        !v["data"]["approve_with"]
            .as_str()
            .unwrap()
            .contains("apply"),
        "{v}"
    );
}

/// The command is advertised as copy-pastable, so a plan under a path with a
/// space in it has to survive the paste rather than arrive at `apply` as two
/// arguments.
///
/// Only the space case is asserted end to end. Which *other* characters are
/// quoted is pinned by `explain`'s own unit tests, on values chosen rather than
/// inherited: an assertion that a plain path stays unquoted would depend on the
/// shape of the ambient temp directory, and on Windows that is
/// `C:\Users\RUNNER~1\...` — which is how the tilde rule was found in the first
/// place, one CI cycle too late.
#[test]
fn the_approval_command_quotes_a_path_a_shell_would_split() {
    let d = Demo::new("quotedpath");
    risky_plan(&d);
    let dir = d.dir.join("release plans");
    std::fs::create_dir_all(&dir).unwrap();
    let plan = dir.join("plan.json");
    std::fs::copy(d.dir.join("plan.json"), &plan).unwrap();

    let out = stdout(&d.run(&["explain", "--plan", plan.to_str().unwrap()]));
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("pbps "))
        .unwrap_or_else(|| panic!("no command in:\n{out}"));
    assert!(line.contains('"'), "the path must be quoted: {line}");
    assert!(line.contains("release plans"), "{line}");
}

/// `doctor` asks "is this environment ready", and for an unreachable target it
/// did not answer that question — it could not look. That is exit 1, not 2, or
/// a pipeline routes a firewall to the author of the schema change (SPEC 9.8).
#[test]
fn doctor_exits_one_when_it_could_not_look_and_two_when_it_looked() {
    let d = Demo::new("doctorexit");
    // Could not look: the variable naming the connection string is not set.
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_DOCTOR_EXIT_UNSET\n",
    )
    .unwrap();
    let o = d.run(&["doctor"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    // The finding is still in the report; only the exit code differs.
    assert!(stdout(&o).contains("unconfigured"), "{}", stdout(&o));

    // Looked, and found something: a declaration the dialect rejects, with no
    // environments to be unreachable.
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\n").unwrap();
    d.table("table: dbo.t\ncolumns:\n  id: {type: jsonb}\n");
    assert_eq!(code(&d.run(&["doctor"])), FINDING);
}

// ---- Fourth review round ----

/// `plan --check` is the command CI runs, so its findings have to reach the
/// annotator like every other read-only command's (SPEC 9.8). Without
/// `--format json` here, missing intent and a stale identity file were the two
/// findings a pipeline could not consume.
#[test]
fn plan_check_speaks_json_like_every_other_read_only_command() {
    let d = Demo::new("planjson");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // A stale identity file.
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  extra: {type: int}\n",
    );
    let o = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["command"], "plan");
    assert_eq!(v["findings"][0]["id"], "identity.stale");
    assert_eq!(v["findings"][0]["remedy"], "pbps plan");

    // And missing intent, which is the other thing --check exists to catch.
    d.table("table: dbo.t\ncolumns:\n  ident: {type: bigint, nullable: false}\n");
    let o = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["findings"][0]["id"], "identity.ambiguous-columns");
    assert!(
        v["findings"][0]["remedy"]
            .as_str()
            .unwrap()
            .contains("pbps rename dbo.t.id ident"),
        "the copy-pastable command must survive into JSON: {v}"
    );
}

/// A clean plan still produces the envelope, with the shape of the change set
/// as data — a consumer that only ever sees JSON when something is wrong cannot
/// tell "clean" from "did not run".
#[test]
fn plan_json_of_a_clean_run_carries_the_change_summary() {
    let d = Demo::new("planjsonok");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "ok");
    assert_eq!(v["data"]["changes"], 1);
    assert_eq!(v["data"]["tables"], 1);
    // The empty baseline is a warning, not silence: everything reading as newly
    // created is the one thing most easily mistaken for a real plan.
    assert_eq!(v["findings"][0]["id"], "baseline.empty");
}

/// `baseline`, `apply` and `unlock` each require exactly one of --db / --env, so
/// a per-environment remedy without one is a command that fails when pasted —
/// and these are aimed at whoever is meeting the environment for the first time.
#[test]
fn every_per_environment_remedy_names_its_environment() {
    let d = Demo::new("remedyenv");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_REMEDY_UNSET\n",
    )
    .unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&stdout(&d.run(&["doctor", "--format", "json"]))).unwrap();
    for f in v["findings"].as_array().unwrap() {
        let Some(remedy) = f["remedy"].as_str() else {
            continue;
        };
        if !remedy.starts_with("pbps ") && !remedy.contains(": pbps ") {
            continue;
        }
        // Every pbps command named in a per-environment remedy needs a target.
        if ["baseline", "apply", "unlock"]
            .iter()
            .any(|c| remedy.contains(&format!("pbps {c}")))
        {
            assert!(remedy.contains("--env "), "{remedy}");
        }
    }
}

// ---- Fifth review round ----

/// The envelope's `result` is what `scripts/findings-to-github.py` maps to its
/// own exit code — a pipe loses the producer's status. Two values could not
/// express "could not answer", so the converter turned `doctor`'s exit 1 into a
/// 2 and routed an unreachable database to the author of the schema change.
#[test]
fn the_envelope_result_matches_the_exit_code_the_command_used() {
    let d = Demo::new("resultcode");
    d.table(ONE_COLUMN);

    // Could not answer.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_RESULT_UNSET\n",
    )
    .unwrap();
    let o = d.run(&["doctor", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "unanswerable");

    // Answered, and found something.
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\n").unwrap();
    d.table("table: dbo.t\ncolumns:\n  id: {type: jsonb}\n");
    let o = d.run(&["doctor", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "findings");

    // Answered, nothing to act on.
    d.table(ONE_COLUMN);
    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "ok");
}

/// `fmt` on a file it cannot parse has not decided whether that file is
/// canonical — it never got to look. The findings are the parse errors; the
/// routing is "the tool could not run".
#[test]
fn fmt_on_an_unparseable_file_is_unanswerable_not_a_finding() {
    let d = Demo::new("fmtunparseable");
    d.table("table: dbo.t\ncolumns: [this is not a mapping\n");

    let o = d.run(&["fmt", "--check", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "unanswerable");
    assert!(
        v["findings"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("load."),
        "{v}"
    );
}

// ---- Sixth review round ----

/// Everything in `plan` can fail before it reaches the point of serializing,
/// and a failure that escaped early printed prose to stderr and nothing to
/// stdout — so a consumer asking for JSON got "pbps produced no output" instead
/// of the typed findings it was owed.
#[test]
fn plan_json_emits_an_envelope_even_when_the_declarations_do_not_load() {
    let d = Demo::new("planearly");
    d.table("table: dbo.t\ncolumns: [unclosed\n");

    let o = d.run(&["plan", "--check", "--format", "json"]);
    // Exit 1 in both formats: `plan`'s question is "what changes", and with
    // declarations it cannot read it did not answer that. `validate` is the
    // command whose question *is* validity, and there the same errors are a
    // finding.
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["command"], "plan");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "load.yaml");
    assert!(
        v["findings"][0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("dbo.t.yml"),
        "{v}"
    );
    // The human format still says the same thing, the same way it always did.
    assert_eq!(code(&d.run(&["plan", "--check"])), 1);
}

/// Choosing an output format must not disable a check the project asked for.
/// With `dev:` configured, JSON mode skipped `dev::rehearse` entirely, so a
/// plan that does not converge came back `result: "ok"`.
///
/// Live, because a rehearsal without an engine is not a rehearsal.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn the_rehearsal_still_runs_when_the_output_is_json() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let d = Demo::new("rehearsejson");
    // A check constraint the engine stores in its own spelling: the difference
    // only exists once a real engine has written it back.
    d.table(concat!(
        "table: dbo.t\n",
        "columns:\n",
        "  id: {type: bigint, nullable: false}\n",
        "checks:\n",
        "  ck_pos: \"id > 0\"\n"
    ));

    let o = d.run(&["plan", "--dev", &connection, "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();

    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"rehearsal.spelling"),
        "the rehearsal must reach the envelope, not be skipped: {v}"
    );
    // stdout stays one JSON document: the rehearsal's own multi-line report
    // would corrupt it.
    assert!(!stdout(&o).contains("Dev rehearsal:"), "{}", stdout(&o));
}

// ---- Seventh review round ----

/// `apply` refuses a plan whose version this build does not understand, and
/// `explain` must refuse it too. A newer plan may carry semantics this binary
/// has no idea about — explaining it would show a reviewer an incomplete
/// account of what they are approving, and an approval command for an artifact
/// `apply` will reject anyway.
#[test]
fn explain_refuses_a_plan_version_it_does_not_understand() {
    let d = Demo::new("planversion");
    d.table(ONE_COLUMN);
    let plan = write_plan(&d, "future.json", "transactional");

    // The same plan, one version ahead.
    let raw = std::fs::read_to_string(&plan).unwrap();
    let current = pbps_model::plan::CURRENT_VERSION;
    let bumped = raw.replace(
        &format!("\"version\": {current}"),
        &format!("\"version\": {}", current + 1),
    );
    assert_ne!(raw, bumped, "the fixture must carry a version to bump");
    std::fs::write(&plan, bumped).unwrap();

    let o = d.run(&["explain", "--plan", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(
        stderr(&o).contains(&format!("version {}", current + 1)),
        "{}",
        stderr(&o)
    );
    assert!(
        !stdout(&o).contains("pbps apply"),
        "a plan this build cannot read must not come with an approval command: {}",
        stdout(&o)
    );
}

/// The one-envelope contract holds for the failures too. A missing or malformed
/// plan left stdout empty, so the converter reported its own generic "produced
/// no output" instead of a report naming the bad file.
#[test]
fn explain_json_emits_an_envelope_when_the_plan_cannot_be_read() {
    let d = Demo::new("explainbadplan");
    d.table(ONE_COLUMN);

    for plan in ["nowhere.json", "junk.json"] {
        if plan == "junk.json" {
            std::fs::write(d.dir.join(plan), "{\"nope\": 1}").unwrap();
        }
        let path = d.dir.join(plan);
        let o = d.run(&[
            "explain",
            "--plan",
            path.to_str().unwrap(),
            "--format",
            "json",
        ]);
        assert_eq!(code(&o), 1, "{plan}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{plan}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["command"], "explain");
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert_eq!(v["findings"][0]["id"], "plan.unreadable");
    }
}

// ---- Eighth review round ----

/// `Change::table()` returns a module's own name for a module change, so
/// counting its distinct values called a one-view plan "1 table". Modules are a
/// different kind of object (ADR-0002) and are counted apart.
#[test]
fn a_plan_of_modules_is_not_reported_as_a_plan_of_tables() {
    let d = Demo::new("modulecount");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.module("v.yml", A_VIEW);

    let out = stdout(&d.run(&["plan"]));
    assert!(out.contains("1 module(s)"), "{out}");
    assert!(!out.contains("1 table(s)"), "a view is not a table: {out}");

    let v: serde_json::Value =
        serde_json::from_str(&stdout(&d.run(&["plan", "--format", "json"]))).unwrap();
    assert_eq!(v["data"]["modules"], 1, "{v}");
    assert_eq!(v["data"]["tables"], 0, "{v}");
}

/// And a plan touching both says both, rather than folding one into the other's
/// count.
#[test]
fn a_plan_touching_tables_and_modules_counts_them_separately() {
    let d = Demo::new("bothcount");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  extra: {type: int}\n",
    );
    d.module("v.yml", A_VIEW);

    let out = stdout(&d.run(&["plan"]));
    assert!(out.contains("1 table(s) and 1 module(s)"), "{out}");
}

/// `verify` could not answer, so it must say so through the envelope like every
/// other read-only command — not leave stdout empty for the converter to
/// report as its own generic failure.
#[test]
fn verify_json_emits_an_envelope_when_it_cannot_connect() {
    let d = Demo::new("verifyjson");
    d.table(ONE_COLUMN);

    let o = d.run(&[
        "verify",
        "--db",
        "Server=127.0.0.1,1;Database=nope;User Id=u;Password=p;TrustServerCertificate=true",
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "verify");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "environment.unreachable");
    // And still no connection string anywhere in it.
    assert!(!stdout(&o).contains("Password"), "{}", stdout(&o));
}

/// `explain --db` must not call a target ready while an apply holds the lock.
/// During an ordinary transactional apply the newest ledger entry is a
/// completed snapshot, so reading `latest` alone reports `ready` — and hands
/// the reviewer an approval command for an environment that is changing
/// underneath them.
///
/// Live, because only a real ledger can hold a real lock.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn explain_reports_a_locked_target_rather_than_a_ready_one() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let d = Demo::new("explainlock");
    d.table(ONE_COLUMN);
    d.commit();
    let plan = write_plan(&d, "target.json", "transactional");

    // A ledger with an ordinary completed entry, and a lock held on top of it.
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );
    let held = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--db",
        &connection,
        "--format",
        "json",
    ]);
    assert_eq!(code(&held), 0, "{}", stderr(&held));
    let v: serde_json::Value = serde_json::from_str(&stdout(&held)).unwrap();
    // With no apply running the target is ready; that is the control.
    assert_eq!(v["data"]["target"]["state"], "ready", "{v}");

    // Now take the lock the way an apply does, and ask again.
    d.git(&["init", "-q"]);
    let locked = std::process::Command::new("docker")
        .args([
            "exec",
            "pbps-test-mssql",
            "/opt/mssql-tools18/bin/sqlcmd",
            "-C",
            "-S",
            "localhost",
            "-U",
            "sa",
            "-P",
            "Pbps!Test12345",
            "-Q",
            "INSERT INTO dbo.__pbps_lock (id, locked_by) VALUES (1, 'someone-else');",
        ])
        .output();
    if locked.map(|o| !o.status.success()).unwrap_or(true) {
        return; // Not the scripted container; the control above already ran.
    }

    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--db",
        &connection,
        "--format",
        "json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let _ = d.run(&["unlock", "--db", &connection]);

    assert_eq!(v["data"]["target"]["state"], "locked", "{v}");
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "target.not-ready"),
        "{v}"
    );
}

// ---- Ninth review round ----

/// The identity file escaped serialization the same way the declarations did,
/// one line below the fix for them.
#[test]
fn plan_json_emits_an_envelope_when_the_identity_file_is_unreadable() {
    let d = Demo::new("planids");
    d.table(ONE_COLUMN);
    std::fs::write(d.ids_path(), "{ this is not json").unwrap();

    let o = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "plan");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "identity.unreadable");
    assert!(
        v["findings"][0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("schema.ids.json"),
        "{v}"
    );
}

/// `doctor` answers "can I deploy from here", and while the lock is held an
/// apply is refused — so a readiness check that passed would be answering a
/// different question than the one it was asked.
///
/// Live, because only a real ledger can hold a real lock.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn doctor_does_not_report_ready_while_the_lock_is_held() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let d = Demo::new("doctorlock");
    d.table(ONE_COLUMN);
    d.commit();
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );

    let taken = std::process::Command::new("docker")
        .args([
            "exec",
            "pbps-test-mssql",
            "/opt/mssql-tools18/bin/sqlcmd",
            "-C",
            "-S",
            "localhost",
            "-U",
            "sa",
            "-P",
            "Pbps!Test12345",
            "-Q",
            "INSERT INTO dbo.__pbps_lock (id, locked_by) VALUES (1, 'someone-else');",
        ])
        .output();
    if taken.map(|o| !o.status.success()).unwrap_or(true) {
        return; // Not the scripted container.
    }

    let o = d.run(&["doctor", "--db", &connection, "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let _ = d.run(&["unlock", "--db", &connection]);

    assert_eq!(
        v["result"], "findings",
        "a held lock is not a clean report: {v}"
    );
    assert_eq!(code(&o), FINDING, "{}", stderr(&o));
    let locked = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "state.locked")
        .unwrap_or_else(|| panic!("no state.locked finding: {v}"));
    assert_eq!(locked["severity"], "error", "{v}");
    // It found this by looking, so it is a finding (exit 2), not unanswerable.
    assert!(
        locked["remedy"].as_str().unwrap().contains("pbps unlock"),
        "{v}"
    );
}

// ---- Tenth review round ----

/// A path no shell-quoting can carry across POSIX shells, PowerShell and `cmd`
/// alike is printed on a line of its own instead of being guessed at. `cmd`
/// does not treat `'` as quoting, so `&` in a single-quoted path splits the
/// command there — the opposite of the "fails safe" it was claimed to be.
#[test]
fn an_unquotable_plan_path_is_shown_rather_than_inlined() {
    let d = Demo::new("unquotable");
    risky_plan(&d);
    let dir = d.dir.join("a&b");
    std::fs::create_dir_all(&dir).unwrap();
    let plan = dir.join("plan.json");
    std::fs::copy(d.dir.join("plan.json"), &plan).unwrap();

    let out = stdout(&d.run(&["explain", "--plan", plan.to_str().unwrap()]));
    let command = out
        .lines()
        .find(|l| l.trim_start().starts_with("pbps "))
        .unwrap_or_else(|| panic!("no command in:\n{out}"));
    assert!(command.contains("<plan path>"), "{command}");
    assert!(
        !command.contains("a&b"),
        "the path must not be inlined at all: {command}"
    );
    // And it is shown, so the reviewer can still act on it.
    assert!(out.contains("<plan path> is:"), "{out}");
    assert!(out.contains("a&b"), "{out}");

    let v: serde_json::Value = serde_json::from_str(&stdout(&d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ])))
    .unwrap();
    assert!(
        v["data"]["plan_path"].as_str().unwrap().contains("a&b"),
        "{v}"
    );
}

/// A plan naming an engine this build has no dialect for is one it cannot
/// explain, and the one-envelope contract covers that failure like the others.
#[test]
fn explain_json_emits_an_envelope_for_a_dialect_it_cannot_explain() {
    let d = Demo::new("explaindialect");
    d.table(ONE_COLUMN);
    let plan = write_plan(&d, "pg.json", "transactional");
    let raw = std::fs::read_to_string(&plan).unwrap();
    std::fs::write(&plan, raw.replace("\"mssql\"", "\"postgres\"")).unwrap();

    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "plan.unsupported-dialect");
    assert!(
        v["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("postgres"),
        "the message must name the engine: {v}"
    );
}

/// An unset `url_env` variable is the commonest first-run problem, and `doctor`
/// has a diagnosis for it. Failing at `target.resolve` instead made the
/// single-environment path — the one a person onboarding actually types — the
/// one that answered worst.
#[test]
fn doctor_env_diagnoses_an_unset_variable_rather_than_failing_at_resolution() {
    let d = Demo::new("doctorenvunset");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_DOCTOR_ENV_UNSET\n",
    )
    .unwrap();

    let o = d.run(&["doctor", "--env", "prod", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["data"]["environments"][0]["environment"], "prod");
    assert_eq!(v["data"]["environments"][0]["state"], "unconfigured");
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "environment.unconfigured"
                && f["message"]
                    .as_str()
                    .unwrap()
                    .contains("PBPS_DOCTOR_ENV_UNSET")),
        "{v}"
    );
    // The project half of the report still ran.
    assert_eq!(v["data"]["tables"], 1, "{v}");
}

/// A database pbps has never touched must come back `uninitialized`, not
/// `unreachable`. This regressed once — the lock check was added ahead of the
/// initialization check, and `lock_holder` selects from a `__pbps_lock` that a
/// virgin database does not have — so it is pinned against a real server.
///
/// `tempdb` is the target: it always exists, holds none of pbps's tables, and
/// nothing here writes to it.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn explain_calls_a_never_initialized_database_uninitialized() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    // Point the same server at a database pbps has never initialized.
    let virgin = if connection.to_lowercase().contains("database=") {
        connection
            .split(';')
            .map(|p| {
                if p.to_lowercase().starts_with("database=") {
                    "Database=tempdb".to_owned()
                } else {
                    p.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(";")
    } else {
        format!("{connection};Database=tempdb")
    };

    let d = Demo::new("explainvirgin");
    d.table(ONE_COLUMN);
    let plan = write_plan(&d, "target.json", "transactional");

    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--db",
        &virgin,
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(
        v["data"]["target"]["state"], "uninitialized",
        "a reachable database pbps has never touched is not unreachable: {v}"
    );
}

// ---- Self-review: the same pattern, a third time ----

/// Found by sweeping this branch for the shape the review kept catching — an
/// error read as good news. `server_version` and `edition` were `.ok()` and
/// `if let Ok`, so a server whose version could not be read never had the
/// 2016-SP1 `CREATE OR ALTER` gate applied, and `doctor` could still say
/// `ready` for a server that would reject every module statement in the plan.
///
/// The live counterpart is the control: against a real server the capabilities
/// *are* read, so no such finding appears and the fields are populated.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn doctor_reports_server_capabilities_or_says_it_could_not_read_them() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let d = Demo::new("doctorcaps");
    d.table(ONE_COLUMN);
    d.commit();

    let o = d.run(&["doctor", "--db", &connection, "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let env = &v["data"]["environments"][0];

    // Read successfully, so both capability answers are present and the
    // "undetermined" finding is absent. An empty `supports_create_or_alter`
    // must never be able to pass for "fine".
    assert!(env["supports_create_or_alter"].is_boolean(), "{v}");
    assert!(env["supports_online"].is_boolean(), "{v}");
    assert!(env.get("server_capabilities_unknown").is_none(), "{v}");
    assert!(
        !v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "server.capabilities-unknown"),
        "{v}"
    );
}

// ---- Eleventh review round ----

/// `postgres` is an accepted `DialectName` with no implementation yet, so this
/// is a reachable failure on a perfectly valid project — and it escaped before
/// the JSON branch, leaving stdout empty.
#[test]
fn validate_json_emits_an_envelope_for_a_dialect_with_no_implementation() {
    let d = Demo::new("validatedialect");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();

    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "project.unsupported-dialect");
    assert!(
        v["findings"][0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("pbps.yml"),
        "{v}"
    );
}

/// The file half of an explanation is the whole point of the command, and
/// `target_state` already degrades an unreachable database to one line in the
/// report. An unset `url_env` variable must not do worse than an unplugged
/// network cable — it did, suppressing the entire explanation.
#[test]
fn explain_still_explains_when_the_environment_variable_is_unset() {
    let d = Demo::new("explainunset");
    let plan = risky_plan(&d);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_EXPLAIN_UNSET\n",
    )
    .unwrap();

    let o = d.run(&["explain", "--plan", plan.to_str().unwrap(), "--env", "prod"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    // The explanation is all there.
    assert!(out.contains("drop column pii"), "{out}");
    assert!(out.contains("data is lost"), "{out}");
    // And the target is reported as what it is, not silently omitted.
    assert!(out.contains("unconfigured"), "{out}");
    assert!(out.contains("PBPS_EXPLAIN_UNSET"), "{out}");

    let v: serde_json::Value = serde_json::from_str(&stdout(&d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--env",
        "prod",
        "--format",
        "json",
    ])))
    .unwrap();
    assert_eq!(v["data"]["target"]["state"], "unconfigured", "{v}");
    assert_eq!(v["data"]["change_count"], 2, "{v}");
}

/// Environment names are YAML map keys, so `US West` is a valid one — and
/// interpolated verbatim into a copy-pastable remedy it becomes two arguments.
#[test]
fn a_remedy_quotes_an_environment_name_a_shell_would_split() {
    let d = Demo::new("remedyquote");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  \"US West\":\n    url_env: PBPS_REMEDY_QUOTE_UNSET\n",
    )
    .unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&stdout(&d.run(&["doctor", "--format", "json"]))).unwrap();
    for f in v["findings"].as_array().unwrap() {
        let Some(remedy) = f["remedy"].as_str() else {
            continue;
        };
        if !remedy.contains("--env ") {
            continue;
        }
        assert!(
            remedy.contains("--env \"US West\""),
            "an environment name a shell would split must be quoted: {remedy}"
        );
    }
}

// ---- The sweep: every fallible step before a JSON branch ----
//
// The findings above arrived one command at a time, each one an error escaping
// through `?` before its command reached the envelope. Rather than wait for the
// rest to be reported, `output::or_unanswerable` was introduced and every such
// step in every read-only command routed through it. These are the sites that
// sweep found; they are grouped because they are one bug, not four.

/// `doctor`'s first act is selecting the dialect, so a project pbps.yml the
/// tool cannot serve left stdout empty for the one command whose entire job is
/// to say what is wrong with the project.
#[test]
fn doctor_json_emits_an_envelope_for_a_dialect_with_no_implementation() {
    let d = Demo::new("doctordialect");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();

    let o = d.run(&["doctor", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "doctor");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "project.unsupported-dialect");
}

/// `verify` exits 2 on drift and 1 when it could not look (decision 25), and a
/// scheduled drift-watch tells them apart from the envelope. Refusing the
/// dialect without one made that scheduled job read "no output" instead.
#[test]
fn verify_json_emits_an_envelope_for_a_dialect_with_no_implementation() {
    let d = Demo::new("verifydialect");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();

    let o = d.run(&["verify", "--db", "Server=x;Database=y", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "verify");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "project.unsupported-dialect");
}

/// Resolving the target happens in the dispatcher, before the command body, so
/// an unset `url_env` — the commonest first-run failure — escaped even though
/// the body itself was careful.
#[test]
fn verify_json_emits_an_envelope_when_the_environment_variable_is_unset() {
    let d = Demo::new("verifyunset");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_VERIFY_UNSET\n",
    )
    .unwrap();

    let o = d.run(&["verify", "--env", "prod", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "verify");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "environment.unconfigured");
}

/// `status` always exits 0 when it can report (decision 25), which is exactly
/// why the case where it cannot has to be visible in the envelope rather than
/// inferred from an empty stdout.
#[test]
fn status_json_emits_an_envelope_for_a_dialect_with_no_implementation() {
    let d = Demo::new("statusdialect");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nenvironments:\n  prod:\n    url_env: PBPS_STATUS_UNSET\n",
    )
    .unwrap();

    let o = d.run(&["status", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "status");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "project.unsupported-dialect");
}

/// Listing the declarations is the step before `fmt` has anything at all to
/// say. A declarations path that is not a directory is a misconfigured
/// `pbps.yml`, not a formatting finding — but it still has to arrive as one.
#[test]
fn fmt_json_emits_an_envelope_when_the_declarations_cannot_be_listed() {
    let d = Demo::new("fmtunlistable");
    // A file where the schema directory should be: the listing fails for a
    // reason the user can act on, which is the case worth reporting. Permission
    // bits would not do — the test suite may run as root.
    std::fs::remove_dir_all(d.dir.join("schema")).unwrap();
    std::fs::write(d.dir.join("schema"), "not a directory\n").unwrap();

    let o = d.run(&["fmt", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "fmt");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "load.io");
}

// ---- Twelfth review round ----

/// The site the sweep above missed. `plan`'s dialect step is a bare `dialect(`
/// call in the same module, not `crate::dialect`, so the grep that found the
/// other seven walked past it — which is the argument for the wrapper being at
/// the call site rather than for being better at grepping.
#[test]
fn plan_json_emits_an_envelope_for_a_dialect_with_no_implementation() {
    let d = Demo::new("plandialect");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();

    let o = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "plan");
    assert_eq!(v["result"], "unanswerable");
    assert_eq!(v["findings"][0]["id"], "project.unsupported-dialect");
}

// ---- Fourteenth review round ----

/// The last upstream site in the escaping-`?` pattern, and the only one no
/// command body could have caught: project discovery runs before dispatch. Its
/// failure is also the very first one a new user meets.
#[test]
fn every_json_command_emits_an_envelope_when_the_project_cannot_be_discovered() {
    let d = Demo::new("nodiscovery");
    // A directory that is not a project and has no project above it: `pbps.yml`
    // is removed and the search must not escape into the repository this test
    // suite itself lives in.
    std::fs::remove_file(d.dir.join("pbps.yml")).unwrap();

    for command in [
        vec!["validate"],
        vec!["plan", "--check"],
        vec!["fmt", "--check"],
        vec!["doctor"],
        vec!["status"],
    ] {
        let mut args = command.clone();
        args.extend(["--format", "json"]);
        let o = d.run(&args);
        assert_eq!(code(&o), 1, "{command:?}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{command:?}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["command"], command[0], "{v}");
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert_eq!(v["findings"][0]["id"], "project.undiscoverable", "{v}");
    }
}

/// The negative half: a command that speaks no envelope must not grow one, and
/// the human format must stay human. A JSON envelope on stdout where a script
/// expects a rendered document would be a new bug, not a fix.
#[test]
fn a_command_without_an_envelope_does_not_gain_one_from_the_discovery_wrapper() {
    let d = Demo::new("nodiscoveryhuman");
    std::fs::remove_file(d.dir.join("pbps.yml")).unwrap();

    for args in [vec!["validate"], vec!["docs"], vec!["unlock", "--env", "x"]] {
        let o = d.run(&args);
        assert_ne!(code(&o), 0, "{args:?}");
        assert!(
            !stdout(&o).trim_start().starts_with('{'),
            "{args:?} printed an envelope on stdout: {}",
            stdout(&o)
        );
    }
}

/// A change the differ cannot express is a *reachable* planning failure — it
/// needs only a declaration that adds IDENTITY to an existing column (decision
/// 5: it cannot be done with ALTER) — and it left `plan --format json` printing
/// prose on stderr and nothing on stdout.
///
/// Each unexpressible change becomes its own finding rather than one collapsed
/// message: the differ hands back one error per change, and the column name in
/// it is the entire remedy.
#[test]
fn plan_json_emits_an_envelope_when_a_change_cannot_be_expressed() {
    let d = Demo::new("unexpressible");
    d.table(ONE_COLUMN);
    d.commit();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // The same column, now IDENTITY: a change ALTER cannot make.
    d.table("table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false, identity: [1, 1]}\n");

    let o = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "plan");
    assert_eq!(v["result"], "unanswerable", "{v}");
    assert_eq!(v["findings"][0]["id"], "change.unexpressible", "{v}");
    assert!(
        v["findings"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("IDENTITY")),
        "the message must name what cannot be done: {v}"
    );
}

// ---- Sixteenth review round ----

/// A plan that reads and deserializes perfectly can still carry a typed change
/// the emitter refuses — a `create_table` whose table has no columns. That is a
/// third, later failure than the two `cmd_explain` already handled, and it left
/// the reviewer's own command printing nothing at all.
#[test]
fn explain_json_emits_an_envelope_when_a_plan_cannot_be_rendered() {
    let d = Demo::new("explainunrenderable");
    d.table(ONE_COLUMN);
    d.commit();
    let plan = d.dir.join("plan.json");
    assert_eq!(code(&d.run(&["plan", "--out", plan.to_str().unwrap()])), 0);
    let raw = std::fs::read_to_string(&plan).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();

    // Empty the created table's columns. The plan stays a structurally valid,
    // current-version plan that deserializes cleanly — which is the point: the
    // two earlier guards both pass, and only the emitter can refuse it.
    let changes = v["changes"]["changes"].as_array_mut().unwrap();
    let target = changes
        .iter_mut()
        .find(|c| c["op"] == "create_table")
        .expect("the plan should create a table");
    target["table"]["columns"] = serde_json::json!({});
    let broken = d.dir.join("broken.json");
    std::fs::write(&broken, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let o = d.run(&[
        "explain",
        "--plan",
        broken.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_ne!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "explain");
    assert_eq!(v["result"], "unanswerable", "{v}");
    // Specifically the *third* guard, not one of the two that already existed:
    // a plan.unreadable here would mean the fixture is broken in a way that
    // never reaches the emitter, and the test would pass without testing this.
    assert_eq!(v["findings"][0]["id"], "plan.unexplainable", "{v}");
}

// ---- Seventeenth review round ----

/// A `--base` that is missing, malformed or of an unsupported version is a
/// failure of the *input*: the command never got as far as comparing anything.
/// It escaped before the JSON branch, so an offline `plan` in CI printed
/// nothing at all for the commonest way of pointing it at the wrong file.
#[test]
fn plan_json_emits_an_envelope_when_the_baseline_cannot_be_read() {
    let d = Demo::new("planbadbase");
    d.table(ONE_COLUMN);
    // The identity check runs first, so the project has to be settled or the
    // test would pass on a `identity.stale` finding instead — a different
    // envelope, from a different guard.
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    for base in ["nowhere.json", "junk.json"] {
        if base == "junk.json" {
            std::fs::write(d.dir.join(base), "not a state snapshot").unwrap();
        }
        let path = d.dir.join(base);
        let o = d.run(&[
            "plan",
            "--check",
            "--base",
            path.to_str().unwrap(),
            "--format",
            "json",
        ]);
        assert_eq!(code(&o), 1, "{base}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{base}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["command"], "plan");
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert_eq!(v["findings"][0]["id"], "baseline.unreadable", "{v}");
    }
}

// ---- Eighteenth review round ----

/// `explain`'s file half is the whole point of the command: the reviewer may
/// have been handed nothing but the plan. `--env` needs a project to resolve
/// the *name*, but that is a fact about the environment, not about the
/// explanation — and discovery failing suppressed the entire report.
#[test]
fn explain_still_explains_with_an_env_outside_a_project() {
    let d = Demo::new("explainnoproject");
    let plan = risky_plan(&d);
    let elsewhere = d.root.join("no-project");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&elsewhere)
        .args(["explain", "--plan", plan.to_str().unwrap(), "--env", "prod"])
        .output()
        .unwrap();
    // Still exit 0: explaining a plan is not a gate, whatever the target did.
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("dbo.customer"),
        "the plan must still be explained: {out}"
    );
    assert!(
        out.contains("prod"),
        "the environment must still be named, as unresolved: {out}"
    );
}

/// The artifact is the deliverable, so a `--out` that cannot be written is a
/// failure of the whole command — and it escaped before the JSON branch. A
/// missing parent directory is the ordinary way this happens in CI.
#[test]
fn plan_json_emits_an_envelope_when_the_artifact_cannot_be_written() {
    let d = Demo::new("planunwritable");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let nowhere = d.dir.join("no/such/dir/plan.json");
    for flag in ["--out", "--sql"] {
        let o = d.run(&["plan", flag, nowhere.to_str().unwrap(), "--format", "json"]);
        assert_eq!(code(&o), 1, "{flag}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{flag}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["command"], "plan");
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert_eq!(v["findings"][0]["id"], "plan.unwritable", "{v}");
    }
}

/// The three-way exit-code split is the feature (SPEC §9.8), and this is the
/// case that most needs it to be right: the database *was* reached and a
/// difference *was* established — the differ simply has no `Change` for it —
/// so it is drift, not "could not look". Folding it into the connection
/// catch-all reported `environment.unreachable` and exited 1, waking whoever
/// owns CI instead of whoever owns the schema.
///
/// Only a real engine can produce this: `IDENTITY` is a property no `ALTER`
/// can change, so it needs a table that really has one and really loses it.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn verify_calls_an_unexpressible_live_difference_drift_not_unreachable() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let sqlcmd = |query: &str| {
        std::process::Command::new("docker")
            .args([
                "exec",
                "pbps-test-mssql",
                "/opt/mssql-tools18/bin/sqlcmd",
                "-C",
                "-S",
                "localhost",
                "-U",
                "sa",
                "-P",
                "Pbps!Test12345",
                "-Q",
                query,
            ])
            .output()
    };

    let d = Demo::new("verifyidentity");
    d.table("table: dbo.ident_drift\ncolumns:\n  id: {type: bigint, nullable: false, identity: [1, 1]}\n");
    d.commit();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let made = sqlcmd(
        "IF OBJECT_ID(N'dbo.ident_drift', N'U') IS NOT NULL DROP TABLE dbo.ident_drift; \
         CREATE TABLE dbo.ident_drift (id bigint IDENTITY(1,1) NOT NULL);",
    );
    if made.map(|o| !o.status.success()).unwrap_or(true) {
        return; // Not the scripted container.
    }
    // Records the live state, which has the IDENTITY, as the baseline.
    assert_eq!(
        code(&d.run(&["snapshot", "--db", &connection, "--force"])),
        0
    );
    // The same table without it. No ALTER can do this, which is the point.
    let _ =
        sqlcmd("DROP TABLE dbo.ident_drift; CREATE TABLE dbo.ident_drift (id bigint NOT NULL);");

    let o = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let _ = sqlcmd("DROP TABLE dbo.ident_drift;");

    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(
        code(&o),
        FINDING,
        "reached and differing is exit 2, not 1: {v}"
    );
    assert_eq!(v["result"], "findings", "{v}");
    // By id, not by position: the report now also carries the ordinary drift
    // summary, and asserting `findings[0]` would break on an unrelated change
    // to their order.
    let unexpressible = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "state.drift-unexpressible")
        .unwrap_or_else(|| panic!("no unexpressible finding: {v}"));
    assert!(
        unexpressible["message"]
            .as_str()
            .is_some_and(|m| m.contains("IDENTITY")),
        "the message must name what differs: {v}"
    );
    // It reaches the *report*, which is what the `on_drift` hook receives —
    // the whole point of carrying it there rather than on a parallel path.
    assert_eq!(
        v["data"]["unexpressible"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    // And the summary counts it. "0 difference(s)" beside a drift verdict
    // reads as a bug in the tool rather than a fact about the database.
    let summary = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "state.drift")
        .unwrap_or_else(|| panic!("no drift summary: {v}"));
    assert!(
        summary["message"]
            .as_str()
            .is_some_and(|m| m.contains("1 difference")),
        "{summary}"
    );
}

/// The last two pre-existing instances of "an absence read as good news",
/// closed after the review stopped rather than left as a follow-up.
///
/// `pull`'s guard is the only thing between an unforced pull and the user's
/// declarations, and it read three different "I do not know" answers as
/// "there is nothing here": a failed listing, and a declarations path that
/// exists but is not a directory.
///
/// Only the second is reachable from a test — a listing that fails on a real
/// directory needs permissions this suite cannot rely on, since it may run as
/// root. That one is fixed by inspection and carries no test; saying so is
/// better than a test that passes down the path that already worked.
#[test]
fn pull_refuses_when_it_cannot_tell_whether_declarations_exist() {
    let d = Demo::new("pullunlistable");
    // A file where the declarations directory should be.
    std::fs::remove_dir_all(d.dir.join("schema")).unwrap();
    std::fs::write(d.dir.join("schema"), "not a directory\n").unwrap();

    // The guard runs before anything connects, so the unreachable server in the
    // connection string is never contacted — and must not be what fails.
    let o = d.run(&["pull", "--db", "Server=x;Database=y"]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("is not a directory"),
        "the refusal must name the real problem, not a connection failure: {}",
        stderr(&o)
    );
    // And it must not have begun overwriting anything.
    assert!(
        d.dir.join("schema").is_file(),
        "pull must not have touched the declarations path"
    );
}

// ---- Nineteenth review round ----
//
// Two writes escaped the envelope: `fmt` rewriting a declaration, and `plan`
// writing the identity file. Both are fixed in `crates/pbps-cli/src/main.rs`
// and **neither carries a test**, which is worth stating rather than papering
// over with one that passes elsewhere.
//
// Making a write fail while the matching read succeeds needs either permission
// bits or an immutable flag. This suite may run as root, which defeats the
// first, and it runs on Windows too, which defeats the second. Every cheaper
// fixture — a directory where the file goes, a file where the directory goes —
// trips the *read* guard one line earlier and produces that guard's envelope,
// so a test built on one would assert a passing behaviour that already worked.
// That mistake has been made three times on this branch already.

// ---- Twenty-first review round ----

/// `db::target` refuses both flags for every other command; `explain`'s
/// project-free path bypassed that resolver and silently preferred `--db`
/// while still printing `--env` in the approval command. The reviewer would
/// validate one database and paste a command that applies to another.
#[test]
fn explain_refuses_a_db_and_an_env_together() {
    let d = Demo::new("explainbothtargets");
    let plan = risky_plan(&d);

    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--db",
        "Server=x;Database=y",
        "--env",
        "prod",
    ]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("pass one of them"),
        "the refusal must be the shared one, not a connection failure: {}",
        stderr(&o)
    );
    // And nothing was explained: an approval command naming the wrong target
    // is the specific harm here.
    assert!(
        !stdout(&o).contains("pbps apply"),
        "no approval command may be printed: {}",
        stdout(&o)
    );
}

// ---- Twenty-second review round ----

/// The refusal added one round earlier was a bare `return`, which put back the
/// escape the rest of `explain` had just been cured of. A new refusal is still
/// an answer, and a consumer has to be able to read it.
#[test]
fn explain_json_emits_an_envelope_when_the_target_flags_conflict() {
    let d = Demo::new("explainbothjson");
    let plan = risky_plan(&d);

    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--db",
        "Server=x;Database=y",
        "--env",
        "prod",
        "--format",
        "json",
    ]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "explain");
    assert_eq!(v["result"], "unanswerable", "{v}");
    assert_eq!(v["findings"][0]["id"], "target.conflicting", "{v}");
}

/// Flag validations run before `cmd_plan` and its JSON handling, which is
/// exactly why they escaped the envelope. A consumer told "produced no output"
/// cannot say which flags contradicted each other.
#[test]
fn plan_json_emits_an_envelope_when_the_flags_contradict() {
    let d = Demo::new("planflags");
    d.table(ONE_COLUMN);
    d.commit();

    for args in [
        vec!["plan", "--check", "--db", "Server=x;Database=y"],
        vec!["plan", "--staged"],
    ] {
        let mut argv = args.clone();
        argv.extend(["--format", "json"]);
        let o = d.run(&argv);
        assert_eq!(code(&o), 1, "{args:?}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{args:?}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["command"], "plan");
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert_eq!(v["findings"][0]["id"], "flags.conflicting", "{v}");
    }
}

// ---- Twenty-sixth review round ----

/// `--check` is the read-only file check CI runs, and "read-only" has to hold
/// for the artifact flags too. With a current identity file the command fell
/// through to the unconditional `--out` / `--sql` writes, so a job that meant
/// to check wrote a plan and a script — and a later reviewer had an artifact no
/// deployment ever produced.
///
/// Refused rather than skipped: silently not writing leaves the previous run's
/// file on disk, and the job then reviews a stale one instead of none.
#[test]
fn check_mode_refuses_the_flags_that_would_write_a_file() {
    let d = Demo::new("checkwrites");
    d.table(ONE_COLUMN);
    // Planned and committed first, so the identity file is current: that is the
    // state in which the writes used to be reached.
    d.run(&["plan"]);
    d.commit();

    for flag in ["--out", "--sql"] {
        let path = d
            .dir
            .join(format!("artifact{}", flag.trim_start_matches('-')));
        let o = d.run(&["plan", "--check", flag, path.to_str().unwrap()]);
        assert_eq!(code(&o), 1, "{flag}: {}", stderr(&o));
        assert!(stderr(&o).contains("--check"), "{flag}: {}", stderr(&o));
        assert!(!path.exists(), "{flag} wrote {} anyway", path.display());
    }
}

/// The same three refusals through `--format json`. The `--dev` half used to be
/// a `bail!` deep inside the command, after the artifact writes, so a consumer
/// asking for JSON got an empty stdout and the converter's generic "no output"
/// — which names no flag and so cannot be acted on.
#[test]
fn check_mode_refusals_reach_the_json_envelope() {
    let d = Demo::new("checkwritesjson");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    let artifact = d.dir.join("artifact.json");
    for args in [
        vec!["plan", "--check", "--dev", "docker://mssql"],
        vec!["plan", "--check", "--out", artifact.to_str().unwrap()],
        vec!["plan", "--check", "--sql", artifact.to_str().unwrap()],
    ] {
        let mut argv = args.clone();
        argv.extend(["--format", "json"]);
        let o = d.run(&argv);
        assert_eq!(code(&o), 1, "{args:?}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{args:?}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["command"], "plan");
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert_eq!(v["findings"][0]["id"], "flags.conflicting", "{v}");
        assert!(!artifact.exists(), "{args:?} wrote the artifact anyway");
    }
}

/// The negative case the refusals must not have swallowed: without `--check`,
/// both artifact flags still produce their files.
#[test]
fn the_artifact_flags_still_write_without_check() {
    let d = Demo::new("artifactswrite");
    d.table(ONE_COLUMN);

    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let o = d.run(&[
        "plan",
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(plan.is_file(), "no {}", plan.display());
    assert!(sql.is_file(), "no {}", sql.display());
}

/// The config schema's justification is the declaration schema's: it accepts
/// exactly what the tool accepts. `dev::spec` requires exactly one backend and
/// refuses both `dev: {}` and a block naming two, so a schema that left the
/// derive's shape alone blessed a pbps.yml no `plan` can run.
#[test]
fn the_config_schema_demands_exactly_one_dev_backend() {
    let d = Demo::new("devschema");
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&d.run(&["schema", "--kind", "config"]))).unwrap();
    let branches = v["$defs"]["Dev"]["oneOf"].as_array().unwrap();

    assert_eq!(branches.len(), 2, "{v}");
    for (key, other) in [("docker", "url_env"), ("url_env", "docker")] {
        let branch = branches
            .iter()
            .find(|b| b["required"] == serde_json::json!([key]))
            .unwrap_or_else(|| panic!("no branch for {key}: {v}"));
        // The type is pinned as well as the presence: `required` is satisfied
        // by an explicit null, which YAML writes as `docker:` with nothing
        // after it, and serde reads that as absent.
        assert_eq!(branch["properties"][key]["type"], "string", "{branch}");
        // And the other key is constrained, not merely unrequired: `required`
        // alone accepts a block naming both, which `dev::spec` refuses. Pinned
        // to null rather than `false` for the reason the module branches are:
        // `url_env:` written empty is absent to serde, so the block resolves.
        assert_eq!(branch["properties"][other]["type"], "null", "{branch}");
    }
}

/// The other half of that property, from the tool's side: the two shapes the
/// schema now refuses are the two the CLI refuses. Asserted against a real
/// `plan`, because a schema pinned only to itself would keep agreeing with a
/// loader that had changed underneath it.
#[test]
fn a_dev_block_naming_neither_or_both_backends_is_refused() {
    for (name, block) in [
        ("devneither", "dev: {}\n"),
        ("devboth", "dev:\n  docker: img\n  url_env: PBPS_DEV_URL\n"),
    ] {
        let d = Demo::new(name);
        std::fs::write(d.dir.join("pbps.yml"), format!("dialect: mssql\n{block}")).unwrap();
        d.table(ONE_COLUMN);

        let o = d.run(&["plan", "--format", "json"]);
        assert_eq!(code(&o), 1, "{name}: {}", stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o))
            .unwrap_or_else(|e| panic!("{name}: stdout was not JSON ({e}): {}", stdout(&o)));
        assert_eq!(v["findings"][0]["id"], "rehearsal.unavailable", "{v}");
    }
}

// ---- Twenty-seventh review round ----

/// A lock held over an empty ledger is exactly what a *first* `bootstrap` looks
/// like while it runs: `state::lock` calls `ensure_tables`, so both tables
/// exist before anything records a snapshot. `status` returned at
/// "uninitialized" without reading the lock, so an apply in flight — and the
/// stale lock an interrupted first bootstrap leaves behind — were invisible in
/// the human view and the JSON alike.
///
/// Set up with the tool's own functions rather than a copy of the DDL: no flag
/// produces this state, and a hand-written `INSERT` would pin the test to a
/// ledger shape the tool is free to change.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn status_reports_a_lock_held_over_an_empty_ledger() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let d = Demo::new("statuslock");
    d.table(ONE_COLUMN);
    d.commit();
    let var = format!("PBPS_STATUS_LOCK_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nenvironments:\n  test:\n    url_env: {var}\n"),
    )
    .unwrap();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        // Whatever an earlier test left: this asserts about an *empty* ledger.
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        pbps_mssql::state::lock(&mut conn, "the-interrupted-bootstrap")
            .await
            .expect("take the lock");
        // The premise, stated rather than assumed: the ledger really is empty
        // while the lock is really held. If `lock` ever stopped creating the
        // tables this test would be exercising nothing.
        assert!(
            matches!(pbps_mssql::state::latest(&mut conn).await, Ok(None)),
            "the ledger should exist and be empty"
        );
        assert!(
            pbps_mssql::state::lock_holder(&mut conn)
                .await
                .unwrap()
                .is_some()
        );
    });

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&var, &connection)
        .output()
        .unwrap();

    let human = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status"])
        .env(&var, &connection)
        .output()
        .unwrap();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = pbps_mssql::state::unlock(&mut conn).await;
        let _ = conn
            .execute("DROP TABLE dbo.__pbps_state; DROP TABLE dbo.__pbps_lock;")
            .await;
    });

    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    // Still uninitialized — that half was never wrong.
    assert_eq!(v["data"][0]["state"], "uninitialized", "{v}");
    assert!(
        v["data"][0]["locked_by"]
            .as_str()
            .is_some_and(|s| s.contains("the-interrupted-bootstrap")),
        "the lock is missing from the row: {v}"
    );
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "state.locked"),
        "a consumer keying off findings alone is told nothing: {v}"
    );
    // And the human view, which is where an operator actually reads it.
    assert!(
        stdout(&human).contains("locked by the-interrupted-bootstrap"),
        "{}",
        stdout(&human)
    );
}

/// The other half-present ledger: `dbo.__pbps_state` dropped by hand while
/// `dbo.__pbps_lock` survives with a live row.
///
/// The round-27 fix read the lock only when the ledger was *empty*, on the
/// reasoning that nothing can take a lock without `ensure_tables` creating the
/// state table first. True of every path the tool controls, and beside the
/// point: a hand-dropped state table leaves the lock behind, the next apply
/// recreates the table and then fails to take that lock, and `status` was
/// hiding the one line that explains the failure.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn status_reports_a_lock_that_outlived_its_state_table() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let d = Demo::new("statuslockorphan");
    d.table(ONE_COLUMN);
    d.commit();
    let var = format!("PBPS_STATUS_ORPHAN_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nenvironments:\n  test:\n    url_env: {var}\n"),
    )
    .unwrap();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        pbps_mssql::state::lock(&mut conn, "the-orphaned-apply")
            .await
            .expect("take the lock");
        // The state table goes, the lock stays. Stated rather than assumed:
        // `latest` must now answer `NotInitialized`, which is the branch that
        // used to return without looking.
        conn.execute("DROP TABLE dbo.__pbps_state;")
            .await
            .expect("drop the state table");
        assert!(
            matches!(
                pbps_mssql::state::latest(&mut conn).await,
                Err(pbps_db::LedgerError::NotInitialized)
            ),
            "the premise: the state table is gone"
        );
        assert!(
            pbps_mssql::state::lock_holder(&mut conn)
                .await
                .unwrap()
                .is_some(),
            "the premise: the lock survived"
        );
    });

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&var, &connection)
        .output()
        .unwrap();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        // Also the negative case for `unlock`, which guarded on the *state*
        // table and so could not release a lock that outlived it.
        assert!(
            pbps_mssql::state::unlock(&mut conn).await.unwrap(),
            "unlock must release a lock whose state table is gone"
        );
        let _ = conn.execute("DROP TABLE dbo.__pbps_lock;").await;
    });

    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["data"][0]["state"], "uninitialized", "{v}");
    assert!(
        v["data"][0]["locked_by"]
            .as_str()
            .is_some_and(|s| s.contains("the-orphaned-apply")),
        "the surviving lock is missing from the row: {v}"
    );
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "state.locked"),
        "{v}"
    );
}

/// And the ordinary first run, which must stay quiet: a database pbps has
/// never touched has no lock table, and reading it has to answer "no lock"
/// rather than "I could not look". `lock_unknown` on every fresh environment
/// would be a warning about the commonest case there is.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn status_says_nothing_about_a_lock_on_a_database_with_no_ledger() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let d = Demo::new("statusnoledger");
    d.table(ONE_COLUMN);
    d.commit();
    let var = format!("PBPS_STATUS_FRESH_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nenvironments:\n  test:\n    url_env: {var}\n"),
    )
    .unwrap();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
    });

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&var, &connection)
        .output()
        .unwrap();

    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["data"][0]["state"], "uninitialized", "{v}");
    assert!(v["data"][0]["locked_by"].is_null(), "{v}");
    assert!(
        v["data"][0]["lock_unknown"].is_null(),
        "a missing lock table is not an unreadable one: {v}"
    );
}

/// One difference the differ cannot phrase must not delete the ones it can.
/// `pbps_diff::diff` accumulates every change it can express and only then
/// returns `Err(errs)`, so `verify` — which reports rather than approves — was
/// throwing that work away: the count, the human report and the `on_drift`
/// hook's payload all lost the expressible drift beside an altered `IDENTITY`.
///
/// Live, because only a real engine produces an `IDENTITY` no `ALTER` can
/// change.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn verify_keeps_the_expressible_drift_beside_an_unexpressible_one() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let sqlcmd = |query: &str| {
        std::process::Command::new("docker")
            .args([
                "exec",
                "pbps-test-mssql",
                "/opt/mssql-tools18/bin/sqlcmd",
                "-C",
                "-S",
                "localhost",
                "-U",
                "sa",
                "-P",
                "Pbps!Test12345",
                "-Q",
                query,
            ])
            .output()
    };

    let d = Demo::new("verifyboth");
    d.table("table: dbo.both_drift\ncolumns:\n  id: {type: bigint, nullable: false, identity: [1, 1]}\n");
    d.commit();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let made = sqlcmd(
        "IF OBJECT_ID(N'dbo.both_drift', N'U') IS NOT NULL DROP TABLE dbo.both_drift; \
         CREATE TABLE dbo.both_drift (id bigint IDENTITY(1,1) NOT NULL);",
    );
    if made.map(|o| !o.status.success()).unwrap_or(true) {
        return; // Not the scripted container.
    }
    assert_eq!(
        code(&d.run(&["snapshot", "--db", &connection, "--force"])),
        0
    );
    // Two differences at once: the IDENTITY is gone (no `Change` exists for
    // that) and a column has been added by hand (one does).
    let _ = sqlcmd(
        "DROP TABLE dbo.both_drift; \
         CREATE TABLE dbo.both_drift (id bigint NOT NULL, note nvarchar(50) NULL);",
    );

    let o = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let _ = sqlcmd("DROP TABLE dbo.both_drift;");

    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(code(&o), FINDING, "{v}");
    assert_eq!(
        v["data"]["unexpressible"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    // The half that used to vanish. It is what the `on_drift` hook receives,
    // so a missing entry here is an alert that understates the damage.
    assert_eq!(
        v["data"]["changes"]["changes"].as_array().map(Vec::len),
        Some(1),
        "the expressible drift was dropped: {v}"
    );
    let summary = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "state.drift")
        .unwrap_or_else(|| panic!("no drift summary: {v}"));
    assert!(
        summary["message"]
            .as_str()
            .is_some_and(|m| m.contains("2 difference")),
        "both halves have to be counted: {summary}"
    );
}

// ---- Twenty-ninth review round ----

/// The same half-present ledger, at the two commands the round-28 fix did not
/// sweep. `dbo.__pbps_state` dropped by hand while `dbo.__pbps_lock` and its row
/// survive: `doctor` reported only "uninitialized" and could exit 0, and
/// `explain` labelled the target uninitialized and went on printing the
/// approval command — with an apply in fact running.
///
/// Both used to ask about initialization before the lock, deliberately, because
/// `lock_holder` selected from a table a never-initialized database does not
/// have. Round 28 removed that reason and updated only `status`.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn doctor_and_explain_see_a_lock_that_outlived_its_state_table() {
    let Ok(connection) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let d = Demo::new("orphanlock");
    d.table(ONE_COLUMN);
    d.commit();
    let var = format!("PBPS_ORPHAN_LOCK_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nenvironments:\n  test:\n    url_env: {var}\n"),
    )
    .unwrap();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        pbps_mssql::state::lock(&mut conn, "the-orphaned-apply")
            .await
            .expect("take the lock");
        conn.execute("DROP TABLE dbo.__pbps_state;")
            .await
            .expect("drop the state table");
    });

    let run = |args: &[&str]| {
        Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(args)
            .env(&var, &connection)
            .output()
            .unwrap()
    };
    let doctor = run(&["doctor", "--format", "json"]);
    let plan = write_plan(&d, "orphan.json", "transactional");
    let explain_human = run(&["explain", "--plan", plan.to_str().unwrap(), "--env", "test"]);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = pbps_mssql::state::unlock(&mut conn).await;
        let _ = conn.execute("DROP TABLE dbo.__pbps_lock;").await;
    });

    let v: serde_json::Value = serde_json::from_str(&stdout(&doctor))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&doctor)));
    assert_eq!(v["data"]["environments"][0]["state"], "locked", "{v}");
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "state.locked"),
        "{v}"
    );
    // An error, not a warning: while the lock is held an apply is refused, so a
    // readiness check that passed would answer a different question.
    assert_eq!(code(&doctor), FINDING, "{v}");

    // `explain` always exits 0 — it is the reviewer's report, not a gate — but
    // it must say the environment is changing rather than print an approval
    // command as though it were idle.
    assert_eq!(code(&explain_human), 0, "{}", stderr(&explain_human));
    let out = stdout(&explain_human);
    assert!(
        out.contains("the-orphaned-apply"),
        "the surviving lock is missing from the report:\n{out}"
    );
}

// ---- Thirtieth review round ----

/// `plan --db --format json` accepted the flag and dropped it: `cmd_plan_db`
/// prints human text, so a consumer that asked for JSON got prose on success
/// and an **empty stdout** on every failure — while the flag validations a few
/// lines above, in the same invocation, answered it properly.
///
/// Refused rather than implemented. `plan --db` is not a findings command: its
/// output is an artifact, and the typed form of that artifact already exists
/// and is better than an envelope — `--out plan.json`, read back with `explain
/// --plan --format json`. A second typed rendering would give a reviewer two
/// documents to disagree about.
#[test]
fn plan_against_a_target_refuses_json_rather_than_ignoring_it() {
    let d = Demo::new("plandbjson");
    d.table(ONE_COLUMN);
    d.run(&["plan"]);
    d.commit();

    // Deliberately unreachable: the refusal must come from the flags, before
    // anything tries to connect, so the test says nothing about the network.
    let o = d.run(&[
        "plan",
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["command"], "plan");
    assert_eq!(v["result"], "unanswerable", "{v}");
    assert_eq!(v["findings"][0]["id"], "flags.conflicting", "{v}");
    // The message has to name the path that does work, or the refusal just
    // moves the consumer's problem one step along.
    assert!(
        v["findings"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("--out") && m.contains("explain")),
        "{v}"
    );
}

/// The negative case: without a target, `--format json` still works. The
/// refusal is about the connected form only.
#[test]
fn an_offline_plan_still_speaks_json() {
    let d = Demo::new("planofflinejson");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan", "--format", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["command"], "plan");
    assert_eq!(v["result"], "ok", "{v}");
}

/// A file whose name holds a byte that is not valid UTF-8.
///
/// `#[cfg(unix)]` says the *API* can spell the name; it does not say the
/// filesystem will store one. Linux passes the bytes through, so the two tests
/// below are real coverage there and in CI; macOS's APFS validates them and
/// fails the create with `EILSEQ`. Those tests are therefore **`ignore`d on
/// macOS**, where the harness counts and prints them as such, rather than
/// skipped at runtime: a runtime skip that returns `Ok` is a test the harness
/// reports as passing with its assertions never run, and an `eprintln!` from
/// a passing test is captured, so nobody would ever see it.
///
/// Everywhere else the create must succeed. Any error — `ENOSPC`, `EMFILE`, a
/// filesystem nobody expected — fails the test, because a filesystem that
/// cannot produce the input is exactly the thing the `ignore` above is for,
/// and anything else is a broken test environment that must not read as
/// coverage.
#[cfg(unix)]
fn non_utf8_path(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt as _;

    let path = dir.join(std::ffi::OsStr::from_bytes(bytes));
    std::fs::write(&path, b"").unwrap_or_else(|e| {
        panic!(
            "cannot create a non-UTF-8 filename at {} ({e}); if this filesystem cannot hold \
             one, the test belongs behind a cfg `ignore`, not a runtime skip",
            dir.display()
        )
    });
    path
}

/// On Unix a filename is bytes, and `Path::display()` substitutes U+FFFD for
/// the ones that are not UTF-8. That character is not in `shell_arg`'s bare set
/// and is not one of its refusals either, so a lossy path came back neatly
/// double-quoted — naming a *different* file, usually one that does not exist.
/// `explain` had read the real plan and would then advertise a command that
/// cannot open it.
///
/// Unix-only because no other platform can produce the input.
#[cfg(unix)]
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses a filename that is not valid UTF-8 (EILSEQ); the input cannot exist here"
)]
#[test]
fn a_plan_path_that_is_not_utf8_becomes_the_placeholder() {
    let d = Demo::new("lossypath");
    d.table(ONE_COLUMN);
    // An *applyable* plan: `explain` prints no approval command for a preview,
    // by design, so a preview would pass this test without exercising anything.
    let src = write_plan(&d, "src.json", "transactional");
    // 0xFF is not valid UTF-8 in any position.
    let lossy = non_utf8_path(&d.dir, b"plan-\xff-.json");
    std::fs::copy(&src, &lossy).unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .arg("explain")
        .arg("--plan")
        .arg(&lossy)
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);

    // It read the real file, which is the premise: the report is about this
    // plan, and only the *command* it advertises was wrong.
    assert!(
        out.contains("prod as queried (entry #1)"),
        "the plan itself was not read:\n{out}"
    );
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("pbps apply"))
        .unwrap_or_else(|| panic!("no approval command in:\n{out}"));
    assert!(
        line.contains("<plan path>"),
        "a path that cannot be spelled must not be advertised: {line}"
    );
    // And the literal is still shown, on a line of its own, so the reader can
    // see what was read even though it cannot be pasted.
    assert!(out.contains("<plan path> is:"), "{out}");
}

// ---- Thirty-first review round ----

/// The other place round 30's lossy path went, and the worse one: serde's
/// `Path` impl **fails** on a path that is not UTF-8, so a `PathBuf` in
/// `Location.file` made that failure the whole envelope's. `explain --plan
/// <non-UTF-8> --format json` on an unreadable plan printed nothing at all and
/// exited 1 with `error: path contains invalid UTF-8 characters` — the
/// one-envelope contract broken by the envelope itself.
///
/// `Location.file` is a `String` now, built with `to_str`, and the location is
/// dropped when the path cannot be spelled. Dropping the pointer keeps the
/// finding; a lossy pointer would name a different file.
#[cfg(unix)]
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses a filename that is not valid UTF-8 (EILSEQ); the input cannot exist here"
)]
#[test]
fn an_unreadable_plan_at_a_non_utf8_path_still_produces_an_envelope() {
    let d = Demo::new("lossyenvelope");
    d.table(ONE_COLUMN);
    let bad = non_utf8_path(&d.dir, b"bad-\xff-.json");
    std::fs::write(&bad, "{ not json").unwrap();

    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .arg("explain")
        .arg("--plan")
        .arg(&bad)
        .args(["--format", "json"])
        .output()
        .unwrap();

    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {:?}", stdout(&o)));
    assert_eq!(v["command"], "explain");
    assert_eq!(v["result"], "unanswerable", "{v}");
    assert_eq!(v["findings"][0]["id"], "plan.unreadable", "{v}");
    // No location rather than a lossy one: a consumer keying off
    // `location.file` would open the wrong file, or none.
    assert!(v["findings"][0]["location"].is_null(), "{v}");

    // The negative case in the same shape: an ordinary path still carries its
    // location, so the fix did not simply stop reporting them.
    let ok = d.dir.join("also-bad.json");
    std::fs::write(&ok, "{ not json").unwrap();
    let o = d.run(&[
        "explain",
        "--plan",
        ok.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["findings"][0]["id"], "plan.unreadable", "{v}");
    assert!(
        v["findings"][0]["location"]["file"]
            .as_str()
            .is_some_and(|f| f.ends_with("also-bad.json")),
        "{v}"
    );
}

// ---- Thirty-third review round ----

/// `plan --dev docker://<image>` end to end, which is the one path through
/// `dev.rs` that had **no automated coverage at all** — every other `--dev`
/// test passes a connection string, so `Container::start` was never run.
///
/// That is how it shipped removing the container it had just returned. It built
/// the cleanup guard twice, the second binding shadowing the first, and a
/// shadowed binding is not dropped early: it lives to the end of the function,
/// so the first guard's `Drop` ran `docker rm -f` on the container handed to the
/// caller. Every `plan --dev docker://...` run then failed with "cannot reach
/// the dev database: connection refused", from the day the feature was written.
///
/// Opt-in through `PBPS_TEST_DEV_IMAGE` because it starts a second SQL Server:
/// `scripts/live-tests.sh` sets it, CI's live job deliberately does not.
#[test]
#[ignore = "needs docker and a SQL Server image; set PBPS_TEST_DEV_IMAGE (see scripts/live-tests.sh)"]
fn a_dev_container_outlives_the_call_that_started_it() {
    // Skipped, not panicked, when the variable is absent — unlike every other
    // test in this file, which panics on a missing `PBPS_TEST_DB`. The
    // difference is that CI sets `PBPS_TEST_DB` always, so its absence is a
    // broken setup worth shouting about, while `PBPS_TEST_DEV_IMAGE` is opt-in
    // and CI deliberately leaves it unset. Copying the panic here turned "this
    // test is not enabled" into a red `live` job.
    let Ok(image) = std::env::var("PBPS_TEST_DEV_IMAGE") else {
        eprintln!("skipped: PBPS_TEST_DEV_IMAGE is not set (see scripts/live-tests.sh)");
        return;
    };

    let d = Demo::new("devdocker");
    // A check constraint the engine stores in its own spelling, so a rehearsal
    // that really talked to a server has something to report. Reaching that
    // finding at all proves the container was still there to be talked to.
    d.table(concat!(
        "table: dbo.t\n",
        "columns:\n",
        "  id: {type: bigint, nullable: false}\n",
        "checks:\n",
        "  ck_pos: \"id > 0\"\n"
    ));

    let o = d.run(&[
        "plan",
        "--dev",
        &format!("docker://{image}"),
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    // Not `rehearsal.unavailable`, which is what a removed container produces.
    assert!(
        ids.contains(&"rehearsal.spelling"),
        "the rehearsal did not reach a live engine: {v}"
    );
}

// ---- Reference data, ADR-0004 ----

const LOOKUP: &str = "table: dbo.t
columns:
  code: {type: varchar(20), nullable: false}
  label: {type: nvarchar(50), nullable: false}
primary_key: [code]
data:
  mode: exact
  rows:
    new: {label: New}
";

/// The whole offline half in one run: a declaration with a `data:` block
/// reaches plan.sql as DML, in the right place relative to the CREATE.
#[test]
fn declared_rows_reach_the_plan_as_dml() {
    let d = Demo::new("dataplan");
    d.table(LOOKUP);

    let sql_path = d.dir.join("plan.sql");
    let o = d.run(&["plan", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    let sql = std::fs::read_to_string(&sql_path).unwrap();
    let create = sql.find("CREATE TABLE").expect(&sql);
    let insert = sql.find("INSERT INTO").expect(&sql);
    assert!(
        create < insert,
        "rows must go in after the table exists:\n{sql}"
    );
    assert!(
        sql.contains("([code], [label]) VALUES (N'new', N'New')"),
        "{sql}"
    );

    // And the summary names the row, so a reviewer with no connection can see
    // what is being written.
    let out = stdout(&o);
    assert!(out.contains("row new"), "{out}");
}

/// Removing a row from an `exact` declaration deletes it — and the plan says
/// so behind the `data-delete` gate rather than in the SQL alone.
#[test]
fn removing_a_row_from_an_exact_table_is_gated() {
    let d = Demo::new("datadelete");
    d.table(LOOKUP);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.table(&LOOKUP.replace("    new: {label: New}\n", ""));
    let o = d.run(&["plan"]);
    let out = stdout(&o);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(out.contains("row new"), "{out}");
    assert!(out.contains("data-delete"), "the gate must be named: {out}");
}

/// The negative case, and the promise `ensure` makes: pbps never removes what
/// it did not declare in a table the application also writes to.
#[test]
fn removing_a_row_from_an_ensure_table_deletes_nothing() {
    let d = Demo::new("dataensure");
    let ensure = LOOKUP.replace("mode: exact", "mode: ensure");
    d.table(&ensure);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.table(&ensure.replace("    new: {label: New}\n", ""));
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!stdout(&o).contains("row new"), "{}", stdout(&o));

    // The positive control, without which this test would also pass if
    // `ensure` mode planned nothing at all: adding a row still works.
    d.table(&ensure.replace(
        "    new: {label: New}\n",
        "    new: {label: New}\n    later: {label: Later}\n",
    ));
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("row later"), "{}", stdout(&o));
}

/// `validate` reports the model's own rules against the file, so the user sees
/// the declaration rather than a constraint violation halfway through an apply.
#[test]
fn a_data_block_without_a_primary_key_fails_validate() {
    let d = Demo::new("datanokey");
    d.table(&LOOKUP.replace("primary_key: [code]\n", ""));
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), FINDING, "{}", stdout(&o));
    let text = format!("{}{}", stdout(&o), stderr(&o));
    assert!(text.contains("primary key"), "{text}");
}

/// A warning, never a refusal: the tool says a thousand rows does not look like
/// reference data, and leaves the judgement with the project.
#[test]
fn an_oversized_data_block_warns_but_still_plans() {
    let d = Demo::new("datalarge");
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\nmax_data_rows: 2\n").unwrap();
    let rows: String = (0..3)
        .map(|i| format!("    r{i}: {{label: R{i}}}\n"))
        .collect();
    d.table(&format!(
        "table: dbo.t\ncolumns:\n  code: {{type: varchar(20), nullable: false}}\n  label: {{type: nvarchar(50), nullable: false}}\nprimary_key: [code]\ndata:\n  mode: exact\n  rows:\n{rows}"
    ));

    let o = d.run(&["validate", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    // The `data.max-rows` policy rule, with `max_data_rows` as its default
    // parameter (ADR-0008).
    assert_eq!(v["findings"][0]["id"], "data.max-rows", "{v}");
    assert_eq!(v["findings"][0]["severity"], "warning", "{v}");
    // A warning is not a finding the pipeline must act on.
    assert_eq!(code(&o), 0, "{}", stdout(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
}

/// A table this revision renames is read, scoped and measured under the name
/// the database still has. With the declared scope keyed by the new name the
/// read found no table, and an `ensure` -> `exact` switch in the same
/// revision planned none of its deletes: the rogue row survived an apply
/// that reported success.
#[test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
fn a_renamed_data_table_is_planned_against_the_rows_it_still_holds() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_renamedata_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("renamedata-live");
    d.table(
        "table: dbo.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\ndata:\n  mode: ensure\n  rows:\n    new: {}\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // Under `ensure` the application's own row is invisible, by design.
    sql("INSERT INTO dbo.t (code) VALUES ('rogue');");
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // One revision: the table is renamed and its block becomes `exact`.
    std::fs::remove_file(d.dir.join("schema/dbo.t.yml")).unwrap();
    std::fs::write(
        d.dir.join("schema/dbo.t2.yml"),
        "table: dbo.t2\ncolumns:\n  code: {type: varchar(20), nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\ndata:\n  mode: exact\n  rows:\n    new: {}\n",
    )
    .unwrap();
    let o = d.run(&["rename-table", "dbo.t", "dbo.t2"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("row rogue"),
        "the rogue row is read under the old name: {out}"
    );
    assert!(out.contains("data-delete"), "{out}");
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "rename,data-delete",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", &connection]);
    assert!(stdout(&o).contains("No changes"), "{}", stdout(&o));

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        let _ = c
            .execute(&format!(
                "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
            ))
            .await;
    });
}

/// A declared text the engine reads back differently — `"1.5"` in a
/// `decimal(5,2)` comes back `1.50` — would be recorded as the engine spells
/// it and drift from the declaration on every later plan. Every connected
/// command asks the engine first and refuses with the spelling to write
/// (DECISIONS 101); only a real engine can say what that spelling is.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_declared_spelling_the_engine_reads_back_differently_is_refused_before_it_is_written() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_spelling_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("spelling-live");
    let declared = |pct: &str, since: &str, extra_rows: &str| {
        format!(
            "table: dbo.rate\ncolumns:\n  code: {{type: varchar(10), nullable: false}}\n  \
             pct: {{type: \"decimal(5,2)\"}}\n  since: {{type: date}}\n\
             primary_key: [code]\ndata:\n  mode: exact\n  rows:\n    std: {{pct: \"{pct}\", \
             since: \"{since}\"}}\n{extra_rows}"
        )
    };
    // `STD` beside `std`: one row to the engine under its case-insensitive
    // collation, and no row to alias against on a table that does not exist
    // yet (DECISIONS 106).
    d.table(&declared("1.5", "2026-9-3", "    STD: {}\n"));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // Refused before the table exists, naming both spellings and the two
    // keys that are one, and nothing was built.
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(
        err.contains("rows `STD` and `std` are the same row to the database"),
        "{err}"
    );
    assert!(
        err.contains("`pct` is written \"1.5\"") && err.contains("\"1.50\""),
        "{err}"
    );
    assert!(
        err.contains("`since` is written \"2026-9-3\"") && err.contains("\"2026-09-03\""),
        "{err}"
    );

    // Written the engine's way, it goes in and comes back as declared: the
    // next connected plan has nothing to say.
    d.table(&declared("1.50", "2026-09-03", ""));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("No changes"), "{}", stdout(&o));

    // And a connected plan against a database that has the row is refused
    // the same way, before a plan that could never converge is written.
    d.table(&declared("1.5", "2026-09-03", ""));
    let o = d.run(&["plan", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`pct` is written \"1.5\"") && stderr(&o).contains("\"1.50\""),
        "{}",
        stderr(&o)
    );

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .expect("drop database");
    });
}

/// `plan --db` and `bootstrap` hand statements to a database that is not a
/// rehearsal, and used to ask none of the questions `validate` asks: a role
/// granting `execute` on a table reached an applyable plan, and its `GRANT` —
/// ordered after every table, row and module statement — would fail on a
/// database those statements had already changed (DECISIONS 141).
///
/// Offline on purpose: the refusal happens before the connection is opened,
/// so the unreachable address in the environment is the assertion. Without
/// the check the command gets as far as failing to connect.
#[test]
fn a_connected_plan_refuses_a_declaration_validate_would_reject() {
    let d = Demo::new("plan-db-validates");
    d.table(
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    std::fs::create_dir_all(d.dir.join("schema").join("roles")).unwrap();
    std::fs::write(
        d.dir.join("schema").join("roles").join("app_reader.yml"),
        "role: app_reader\ngrants:\n  dbo.customer: [execute]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // 127.0.0.1:1 answers nothing; reaching it at all is the failure.
    let unreachable = "Server=127.0.0.1,1;User Id=sa;Password=no;TrustServerCertificate=true";
    for command in [
        vec!["plan", "--db", unreachable],
        vec!["bootstrap", "--db", unreachable],
    ] {
        let o = d.run(&command);
        assert_ne!(code(&o), 0, "{}", stdout(&o));
        assert!(
            stderr(&o).contains("`execute` does not apply to `dbo.customer`"),
            "{command:?}: {}",
            stderr(&o)
        );
        assert!(
            !stderr(&o).contains("127.0.0.1"),
            "the declarations are refused before anything is connected to: {}",
            stderr(&o)
        );
    }
}

/// One revision that retypes a column and deletes an undeclared row from the
/// same `exact` table. `AlterColumnType` sorts before the row changes, so by
/// the time the `DELETE` runs the engine has converted the recorded value out
/// of the spelling the plan wrote down: `decimal(5,2)` `2.50` reads back as
/// `2` under `int`, the predicate matched nothing, and the apply aborted —
/// in staged mode after the conversion had committed (DECISIONS 146).
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_delete_beside_a_type_change_in_the_same_revision_applies() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_retype_delete_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let declared = |ty: &str| {
        format!(
            "table: dbo.t
columns:
  code: {{type: varchar(20), nullable: false}}
  pct: {{type: '{ty}'}}
primary_key: {{name: pk_t, columns: [code]}}
data:
  mode: exact
  rows:
    keep: {{}}
"
        )
    };
    let d = Demo::new("retype-delete-live");
    d.table(&declared("decimal(5,2)"));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // A row the declaration does not have, adopted into the baseline so the
    // next plan is the one that removes it.
    sql("INSERT INTO dbo.t (code, pct) VALUES ('rogue', 2.50);");
    assert_eq!(
        code(&d.run(&[
            "baseline",
            "--db",
            &connection,
            "--reason",
            "adopt the rogue row"
        ])),
        0
    );

    // The same revision retypes the column and deletes that row.
    d.table(&declared("int"));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("row rogue"), "{out}");

    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "data-delete,narrowing,destructive,not-null",
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .expect("drop database");
    });
}

/// A schema name is the one name in a declaration with no identity behind
/// it: text on both sides, whether it is a `schema::` grant target or the
/// schema half of a qualified table name. Declared `schema::DBO` on a
/// case-insensitive database grants successfully, reads back as `dbo`, and is
/// revoked and granted again by every plan after — so it is refused before a
/// plan is written (DECISIONS 142). Only the engine knows how it spells a
/// schema, and whether it considers the two one name at all.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_schema_name_the_database_spells_differently_is_refused() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_schemacase_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("schemacase-live");
    d.table(
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    std::fs::create_dir_all(d.dir.join("schema").join("roles")).unwrap();
    let role_file = d.dir.join("schema").join("roles").join("reporting.yml");
    let role = |target: &str| format!("role: reporting\ngrants:\n  schema::{target}: [select]\n");
    std::fs::write(&role_file, role("DBO")).unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`DBO` (granted to role `reporting`) is written `dbo` by the database"),
        "{}",
        stderr(&o)
    );

    // The schema half of a qualified table name is the same text on both
    // sides, and worse when it disagrees: `DBO.customer` was created as
    // `dbo.customer`, recorded as a state with no tables in it at all, and
    // reported as drift by the `verify` that followed the successful
    // bootstrap. A project of its own, because swapping one declared table
    // for another inside one is a rename nobody expressed.
    {
        let d = Demo::new("schemacase-table-live");
        d.table(
            "table: DBO.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
        );
        assert_eq!(code(&d.run(&["plan"])), 0);
        d.commit();
        let o = d.run(&["bootstrap", "--db", &connection]);
        assert_ne!(code(&o), 0, "{}", stdout(&o));
        assert!(
            stderr(&o).contains("`DBO` (the schema of `DBO.customer`) is written `dbo`"),
            "{}",
            stderr(&o)
        );
    }

    // The database's own spelling is accepted, and — the point of the
    // refusal — plans again as no change at all.
    std::fs::write(&role_file, role("dbo")).unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains("No changes."),
        "the accepted spelling has to converge: {}",
        stdout(&o)
    );

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .expect("drop database");
    });
}

/// Users, roles and application roles share one namespace in SQL Server; a
/// role declared under a user's name looked free to the managed set and
/// `CREATE ROLE` failed after everything ordered before it had run. Refused
/// before anything runs, by `bootstrap` and by `plan --db`, with the
/// principal's kind (DECISIONS 118). Only the engine knows who holds a name.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_role_named_like_a_user_is_refused_before_anything_runs() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_rolename_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");
    sql("CREATE USER shadow WITHOUT LOGIN;");

    let d = Demo::new("rolename-live");
    d.table(
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    std::fs::create_dir_all(d.dir.join("schema").join("roles")).unwrap();
    let role_file = d.dir.join("schema").join("roles").join("shadow.yml");
    std::fs::write(
        &role_file,
        "role: shadow\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // Two declared roles the database reads as one name pass every check
    // against the catalog — nothing holds either yet — and the second
    // `CREATE ROLE` would fail after the tables went in. The engine says
    // which names are one (DECISIONS 123).
    let pair = |d: &Demo| {
        for role in ["Reader", "reader"] {
            std::fs::write(
                d.dir
                    .join("schema")
                    .join("roles")
                    .join(format!("{role}.yml")),
                format!("role: {role}\ngrants:\n  dbo.customer: [select]\n"),
            )
            .unwrap();
        }
        assert_eq!(code(&d.run(&["plan"])), 0);
    };
    let unpair = |d: &Demo| {
        for role in ["Reader", "reader"] {
            std::fs::remove_file(
                d.dir
                    .join("schema")
                    .join("roles")
                    .join(format!("{role}.yml")),
            )
            .unwrap();
            let o = d.run(&["drop-role", role, "--reason", "never created"]);
            assert_eq!(code(&o), 0, "{}", stderr(&o));
        }
    };
    pair(&d);
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o)
            .contains("two declared roles are one name to this database: `Reader` and `reader`"),
        "{}",
        stderr(&o)
    );
    unpair(&d);

    // Bootstrap: refused before the table goes in, naming the user.
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`shadow` is a sql user"),
        "{}",
        stderr(&o)
    );

    // Without the role the project bootstraps, with another role to rename
    // later on.
    std::fs::remove_file(&role_file).unwrap();
    let o = d.run(&["drop-role", "shadow", "--reason", "never created"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let reporter_file = d.dir.join("schema").join("roles").join("reporter.yml");
    std::fs::write(
        &reporter_file,
        "role: reporter\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // Declared afterwards, the role is refused by the connected plan the
    // same way.
    std::fs::write(
        &role_file,
        "role: shadow\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["plan", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`shadow` is a sql user"),
        "{}",
        stderr(&o)
    );
    std::fs::remove_file(&role_file).unwrap();
    let o = d.run(&["drop-role", "shadow", "--reason", "never created"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // The pair, by the connected plan: the same refusal, before any plan is
    // written.
    pair(&d);
    let o = d.run(&["plan", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o)
            .contains("two declared roles are one name to this database: `Reader` and `reader`"),
        "{}",
        stderr(&o)
    );
    unpair(&d);

    // And in another case: `Shadow` is `shadow` to this database, which is
    // the engine's call under its collation, not a string comparison's
    // (DECISIONS 119).
    let shadow_file = d.dir.join("schema").join("roles").join("Shadow.yml");
    std::fs::write(
        &shadow_file,
        "role: Shadow\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["plan", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`Shadow` is `shadow` to this database, a sql user"),
        "{}",
        stderr(&o)
    );
    std::fs::remove_file(&shadow_file).unwrap();
    let o = d.run(&["drop-role", "Shadow", "--reason", "never created"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // A rename onto the name is checked like a creation: the target has to
    // be free, or `ALTER ROLE ... WITH NAME` fails after everything before it.
    std::fs::remove_file(&reporter_file).unwrap();
    std::fs::write(
        &role_file,
        "role: shadow\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    let o = d.run(&["rename-role", "reporter", "shadow"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["plan", "--db", &connection]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`shadow` is a sql user"),
        "{}",
        stderr(&o)
    );

    // Renamed to a free name instead, the plan is made — and a user created
    // under that name before the apply is met before statement one: a
    // principal is outside the managed state, so the checksum cannot see it.
    std::fs::remove_file(&role_file).unwrap();
    std::fs::write(
        d.dir.join("schema").join("roles").join("auditor.yml"),
        "role: auditor\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    let o = d.run(&["rename-role", "shadow", "auditor"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
    let plan = d.dir.join("rename.plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    sql("CREATE USER auditor WITHOUT LOGIN;");
    let apply = || {
        d.run(&[
            "apply",
            "--db",
            &connection,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&plan),
            "--allow",
            "rename",
        ])
    };
    let o = apply();
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`auditor` is a sql user"),
        "{}",
        stderr(&o)
    );
    sql("DROP USER auditor;");
    let o = apply();
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // A permission the declarations cannot hold is drift to `verify`, and
    // was "ok" to `status`, whose checksum is computed from the schema the
    // permission is carried beside, not in (DECISIONS 125). Both commands
    // now say drift, with the permission named.
    let var = format!("PBPS_STATUS_ROLE_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nenvironments:\n  test:\n    url_env: {var}\n"),
    )
    .unwrap();
    let status = || {
        Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(["status", "--format", "json"])
            .env(&var, &connection)
            .output()
            .unwrap()
    };
    let v: serde_json::Value = serde_json::from_str(&stdout(&status())).unwrap();
    assert_eq!(v["data"][0]["state"], "ok", "{v}");
    sql("GRANT SELECT ON dbo.customer TO auditor WITH GRANT OPTION;");
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 2, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&status())).unwrap();
    assert_eq!(v["data"][0]["state"], "drift", "{v}");
    assert!(
        v["data"][0]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("auditor") && d.contains("GRANT OPTION")),
        "{v}"
    );

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .expect("drop database");
    });
}

/// A dropped role's members are listed at plan time so a reviewer sees who
/// loses the role, and membership is each environment's own — so a member
/// Two roles dropped together, one a member of the other: the differ ranks
/// the drops parent-first (DECISIONS 127), but it never sees the members —
/// `plan --db` writes them in afterwards — so the order has to be applied
/// again once they are known (139). The names are chosen so that the name
/// tiebreaker alone would drop the member first, after which the holder's
/// `DROP MEMBER` names a principal already gone and the whole apply rolls
/// back. Only a real engine refuses that by name.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn roles_holding_each_other_are_dropped_parent_first_through_a_connected_plan() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_nestedroles_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    let roles = || -> i32 {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            let rows = c
                .query(&format!(
                    "USE [{name}]; SELECT COUNT(*) FROM sys.database_principals \
                     WHERE type = 'R' AND name IN ('a_analysts', 'z_reporting');"
                ))
                .await
                .expect("count roles");
            rows[0].try_get_at::<i32>(0).unwrap().unwrap()
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("nestedroles-live");
    d.table(
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    let roles_dir = d.dir.join("schema").join("roles");
    std::fs::create_dir_all(&roles_dir).unwrap();
    for role in ["a_analysts", "z_reporting"] {
        std::fs::write(
            roles_dir.join(format!("{role}.yml")),
            format!("role: {role}\ngrants:\n  dbo.customer: [select]\n"),
        )
        .unwrap();
    }
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // The environment makes one role a member of the other — the member
    // sorts first by name, which is the wrong order to drop them in.
    sql("ALTER ROLE z_reporting ADD MEMBER a_analysts;");
    for role in ["a_analysts", "z_reporting"] {
        std::fs::remove_file(roles_dir.join(format!("{role}.yml"))).unwrap();
        let o = d.run(&["drop-role", role, "--reason", "SEC-9 retired"]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
    }
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let listing = stdout(&o);
    let holder = listing
        .find("drop role z_reporting, removing 1 member(s) first: a_analysts")
        .unwrap_or_else(|| panic!("the holder's drop lists its member:\n{listing}"));
    let member = listing
        .find("drop role a_analysts")
        .unwrap_or_else(|| panic!("the member's drop:\n{listing}"));
    assert!(holder < member, "the holder is dropped first:\n{listing}");

    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(roles(), 0, "both roles are gone");

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .expect("drop database");
    });
}

/// added between `plan --db` and `apply` is invisible to the checksum. The
/// apply has to ask again before statement one (DECISIONS 92): a staged apply
/// that found out at `DROP ROLE` would have committed every `DROP MEMBER` the
/// reviewer saw. Only a real engine holds a membership to change under the
/// plan.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_member_added_after_planning_refuses_the_role_drop_before_anything_runs() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_roledrop_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    let members = || -> i32 {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            let rows = c
                .query(&format!(
                    "USE [{name}]; SELECT COUNT(*) FROM sys.database_role_members rm \
                     JOIN sys.database_principals r ON r.principal_id = rm.role_principal_id \
                     WHERE r.name = 'reporting';"
                ))
                .await
                .expect("count members");
            rows[0].try_get_at::<i32>(0).unwrap().unwrap()
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("roledrop-live");
    d.table(
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    std::fs::create_dir_all(d.dir.join("schema").join("roles")).unwrap();
    std::fs::write(
        d.dir.join("schema").join("roles").join("reporting.yml"),
        "role: reporting\ngrants:\n  dbo.customer: [select]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // The environment gives the role a member; the plan lists it.
    sql("CREATE USER analyst_a WITHOUT LOGIN; ALTER ROLE reporting ADD MEMBER analyst_a;");
    std::fs::remove_file(d.dir.join("schema").join("roles").join("reporting.yml")).unwrap();
    let o = d.run(&["drop-role", "reporting", "--reason", "SEC-9 retired"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("analyst_a"), "{}", stdout(&o));

    // Another member after the plan was made. The apply is refused before
    // anything runs, naming the member nobody reviewed, and the first member
    // still holds the role.
    sql("CREATE USER analyst_b WITHOUT LOGIN; ALTER ROLE reporting ADD MEMBER analyst_b;");
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(stderr(&o).contains("analyst_b"), "{}", stderr(&o));
    assert!(stderr(&o).contains("plan --db"), "{}", stderr(&o));
    assert_eq!(members(), 2, "nothing ran");

    // A plan made against the role as it is now lists both, and applies.
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("analyst_b"), "{}", stdout(&o));
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(members(), 0, "the role is gone");

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .expect("drop database");
    });
}

/// The connected half of ADR-0004, end to end through the real binary: the
/// rows go in with `bootstrap`, come back into the recorded state in the
/// engine's spelling, a hand-edited row is drift, a re-declared table plans
/// against what the target holds, and `pull --data` writes the block back.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn reference_data_round_trips_through_a_real_target() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    // A database of its own, so the shared server's `master` never holds the
    // lookup table, and two runs cannot meet in it.
    let name = format!("pbps_cli_refdata_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    // `seq` is the engine's column: a declaration cannot set it and an UPDATE
    // cannot change it, so it is never read back and never compared — read
    // back, its value met the omission every row has to make and the second
    // plan restated an UPDATE the engine refuses (DECISIONS 94).
    let declared = "table: dbo.t
columns:
  code: {type: varchar(20), nullable: false}
  label: {type: nvarchar(50), nullable: false, default: \"'Unlabelled'\"}
  rank: {type: int}
  seq: {type: int, nullable: false, identity: [1, 1]}
primary_key: {name: pk_t, columns: [code]}
data:
  mode: exact
  rows:
    new: {label: New, rank: 1}
    old: {}
";
    let d = Demo::new("refdata-live");
    d.table(declared);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // Bootstrap builds the table and inserts the rows; the state it records
    // has to hold them, read back.
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(
        code(&o),
        0,
        "no drift right after bootstrap: {}{}",
        stdout(&o),
        stderr(&o)
    );

    // A hand edit to a declared row is drift, named by its key.
    sql("UPDATE dbo.t SET label = N'Ancient' WHERE code = 'old';");
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), FINDING, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("row old"), "{}", stdout(&o));
    // ...and a rogue row in an `exact` table is drift too.
    sql("INSERT INTO dbo.t (code, label) VALUES ('rogue', N'Rogue');");
    let o = d.run(&["verify", "--db", &connection]);
    assert!(stdout(&o).contains("row rogue"), "{}", stdout(&o));

    // A connected plan is computed against what the target holds: the edit is
    // put back, the rogue row goes behind the gate, and the second apply of
    // the same declaration is empty rather than a primary-key violation.
    assert_eq!(
        code(&d.run(&[
            "baseline",
            "--db",
            &connection,
            "--reason",
            "adopt the hand edits"
        ])),
        0
    );
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("row old"),
        "the edited label is restated: {out}"
    );
    assert!(out.contains("row rogue"), "{out}");
    assert!(out.contains("data-delete"), "{out}");
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "data-update,data-delete",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("No changes"),
        "the same declaration must plan nothing the second time: {}",
        stdout(&o)
    );

    // A table gaining its first `data:` block: its rows are pinned by the
    // plan too, so a row that appears between plan and apply is refused —
    // under `exact`, that row would otherwise outlive the approved deletes.
    let u_path = d.dir.join("schema/dbo.u.yml");
    std::fs::write(
        &u_path,
        "table: dbo.u\ncolumns:\n  code: {type: varchar(20), nullable: false}\nprimary_key: {name: pk_u, columns: [code]}\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan2 = d.dir.join("plan2.json");
    let o = d.run(&[
        "plan",
        "--db",
        &connection,
        "--out",
        plan2.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan2.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan2),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    std::fs::write(
        &u_path,
        "table: dbo.u\ncolumns:\n  code: {type: varchar(20), nullable: false}\nprimary_key: {name: pk_u, columns: [code]}\ndata:\n  mode: exact\n  rows:\n    a: {}\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan3 = d.dir.join("plan3.json");
    let o = d.run(&[
        "plan",
        "--db",
        &connection,
        "--out",
        plan3.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    sql("INSERT INTO dbo.u (code) VALUES ('rogue');");
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan3.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan3),
    ]);
    assert_ne!(
        code(&o),
        0,
        "a row that appeared after the plan: {}",
        stdout(&o)
    );
    assert!(
        stderr(&o).contains("no longer the database this plan was computed against"),
        "{}",
        stderr(&o)
    );
    // Put back the way the plan saw it, the same plan applies.
    sql("DELETE FROM dbo.u WHERE code = 'rogue';");
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan3.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan3),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // The block comes back out in the engine's spelling: the default-valued
    // label is omitted, the explicit one is kept.
    let fresh = Demo::new("refdata-pull");
    let o = fresh.run(&["pull", "--db", &connection, "--data", "dbo.t"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let file = std::fs::read_to_string(fresh.dir.join("schema").join("dbo.t.yml")).unwrap();
    assert!(file.contains("mode: exact"), "{file}");
    assert!(file.contains("new: {label: New, rank: 1}"), "{file}");
    assert!(file.contains("old: {}"), "{file}");
    assert!(!file.contains("rogue"), "{file}");
    // And a table this database does not have is refused by name.
    let o = fresh.run(&["pull", "--db", &connection, "--data", "dbo.nope", "--force"]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("dbo.nope"), "{}", stderr(&o));
    // The row line is `validate`'s rule, suppressions included: at `error`
    // over the count nothing is written, and the same table excused by name
    // is pulled without a word (DECISIONS 114, 120).
    let strict = Demo::new("refdata-pull-strict");
    let rule =
        "dialect: mssql\npolicies:\n  rules:\n    data.max-rows: {severity: error, rows: 1}\n";
    std::fs::write(strict.dir.join("pbps.yml"), rule).unwrap();
    let o = strict.run(&["pull", "--db", &connection, "--data", "dbo.t"]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("Nothing was written"), "{}", stderr(&o));
    assert!(!strict.dir.join("schema").join("dbo.t.yml").exists());
    std::fs::write(
        strict.dir.join("pbps.yml"),
        format!(
            "{rule}  suppress:\n    - rule: data.max-rows\n      on: dbo.t\n      reason: known small\n"
        ),
    )
    .unwrap();
    let o = strict.run(&["pull", "--db", &connection, "--data", "dbo.t"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(!stderr(&o).contains("rows"), "{}", stderr(&o));
    assert!(strict.dir.join("schema").join("dbo.t.yml").is_file());

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        let _ = c
            .execute(&format!(
                "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
            ))
            .await;
    });
}

// ---- Roles and grants, ADR-0005 ----

const A_TABLE_AND_A_ROLE: [(&str, &str); 2] = [
    (
        "dbo.customer.yml",
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: {name: pk_customer, columns: [id]}\n",
    ),
    (
        "app_reader.role.yml",
        "role: app_reader\ngrants:\n  dbo.customer: [select]\n",
    ),
];

impl Demo {
    fn files(&self, files: &[(&str, &str)]) {
        for (name, body) in files {
            std::fs::write(self.dir.join("schema").join(name), body).unwrap();
        }
    }
}

/// A declared role reaches the plan as `CREATE ROLE` then `GRANT`, after the
/// object it grants on; the widening is labelled and nothing is gated.
#[test]
fn a_declared_role_is_created_then_granted_and_the_widening_is_not_gated() {
    let d = Demo::new("rolecreate");
    d.files(&A_TABLE_AND_A_ROLE);

    let sql_path = d.dir.join("plan.sql");
    let o = d.run(&["plan", "--sql", sql_path.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let sql = std::fs::read_to_string(&sql_path).unwrap();
    let create_table = sql.find("CREATE TABLE").expect(&sql);
    let create_role = sql.find("CREATE ROLE [app_reader];").expect(&sql);
    let grant = sql
        .find("GRANT SELECT ON OBJECT::[dbo].[customer] TO [app_reader];")
        .expect(&sql);
    assert!(create_table < grant && create_role < grant, "{sql}");

    let out = stdout(&o);
    assert!(out.contains("grant-widen"), "labelled: {out}");
    assert!(!out.contains("--allow"), "but not gated: {out}");
    // And the role has identity: an `r_` uid in the file.
    let ids = std::fs::read_to_string(d.ids_path()).unwrap();
    assert!(ids.contains("\"r_"), "{ids}");
    assert!(ids.contains("app_reader"), "{ids}");
}

/// Taking a permission away is the availability risk ADR-0005 gates.
#[test]
fn removing_a_permission_is_a_revoke_behind_the_gate() {
    let d = Demo::new("rolerevoke");
    d.files(&A_TABLE_AND_A_ROLE);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    d.files(&[(
        "app_reader.role.yml",
        "role: app_reader\ngrants:\n  dbo.customer: [view-definition]\n",
    )]);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("revoke select"), "{out}");
    assert!(out.contains("grant view-definition"), "{out}");
    assert!(out.contains("--allow revoke"), "{out}");
    assert!(!out.contains("--allow revoke,grant-widen"), "{out}");
}

/// A role rename is a question only its author can answer, and the answer is
/// recorded the way a table rename is — never as drop + add, which would lose
/// the role's members.
#[test]
fn renaming_a_role_needs_intent_and_rename_role_records_it() {
    let d = Demo::new("rolerename");
    d.files(&A_TABLE_AND_A_ROLE);
    assert_eq!(code(&d.run(&["plan"])), 0);
    let uid_before = {
        let ids: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.ids_path()).unwrap()).unwrap();
        ids["roles"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, v)| *v == "app_reader")
            .map(|(k, _)| k.clone())
            .unwrap()
    };
    d.commit();

    std::fs::remove_file(d.dir.join("schema").join("app_reader.role.yml")).unwrap();
    d.files(&[(
        "reader.role.yml",
        "role: reader\ngrants:\n  dbo.customer: [select]\n",
    )]);
    let o = d.run(&["plan", "--check"]);
    assert_eq!(code(&o), FINDING, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("pbps rename-role app_reader reader"),
        "{}",
        stderr(&o)
    );

    let o = d.run(&["rename-role", "app_reader", "reader"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("rename role app_reader -> reader"),
        "{}",
        stdout(&o)
    );
    // Gated like any rename: the old name is gone, and a module or an
    // application asking `IS_ROLEMEMBER('app_reader')` breaks on the spot.
    assert!(stdout(&o).contains("--allow rename"), "{}", stdout(&o));
    let ids: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.ids_path()).unwrap()).unwrap();
    assert_eq!(
        ids["roles"][&uid_before], "reader",
        "the uid survives: {ids}"
    );

    // The other answer: a drop, which needs a reason and leaves a tombstone.
    // Committed first, so the baseline knows the role by its new name.
    d.commit();
    std::fs::remove_file(d.dir.join("schema").join("reader.role.yml")).unwrap();
    let o = d.run(&["plan", "--check"]);
    assert_eq!(code(&o), FINDING);
    assert!(stderr(&o).contains("drop-role reader"), "{}", stderr(&o));
    let o = d.run(&["drop-role", "reader", "--reason", "SEC-9 retired"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("drop role reader"), "{}", stdout(&o));
    assert!(stdout(&o).contains("--allow revoke"), "{}", stdout(&o));
    let ids = std::fs::read_to_string(d.ids_path()).unwrap();
    assert!(ids.contains("SEC-9 retired"), "{ids}");
}

/// The foreign-key-target rule applied to permissions: a grant on an object
/// nobody declares is refused by `validate`, and a schema-level grant is not.
#[test]
fn a_grant_on_an_undeclared_object_fails_validate() {
    let d = Demo::new("rolevalidate");
    d.files(&A_TABLE_AND_A_ROLE);
    d.files(&[(
        "app_reader.role.yml",
        "role: app_reader\ngrants:\n  dbo.ghost: [select]\n  schema::app: [execute]\n",
    )]);
    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stdout(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["schema.grant-target"], "{v}");
    assert!(
        v["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("dbo.ghost"),
        "{v}"
    );

    // And a built-in role is the engine's, not the project's.
    d.files(&[(
        "app_reader.role.yml",
        "role: db_datareader\ngrants:\n  dbo.customer: [select]\n",
    )]);
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), FINDING);
    assert!(
        format!("{}{}", stdout(&o), stderr(&o)).contains("built-in"),
        "{}{}",
        stdout(&o),
        stderr(&o)
    );
}

/// `fmt` canonicalizes a role file, keeps a pending rename, and drops it once
/// the identity file has absorbed it — the same life cycle a table's has.
#[test]
fn fmt_keeps_a_pending_role_rename_and_strips_an_absorbed_one() {
    let d = Demo::new("rolefmt");
    d.files(&A_TABLE_AND_A_ROLE);
    assert_eq!(code(&d.run(&["plan"])), 0);
    std::fs::remove_file(d.dir.join("schema").join("app_reader.role.yml")).unwrap();
    d.files(&[(
        "reader.role.yml",
        "role: reader\nrenamed_from: app_reader\ngrants:\n  dbo.customer: [view-definition, select]\n",
    )]);
    assert_eq!(code(&d.run(&["fmt"])), 0);
    let text = std::fs::read_to_string(d.dir.join("schema").join("reader.role.yml")).unwrap();
    assert!(text.contains("renamed_from: app_reader"), "pending: {text}");
    assert!(text.contains("[select, view-definition]"), "sorted: {text}");

    assert_eq!(code(&d.run(&["plan"])), 0, "the annotation is the intent");
    assert_eq!(code(&d.run(&["fmt"])), 0);
    let text = std::fs::read_to_string(d.dir.join("schema").join("reader.role.yml")).unwrap();
    assert!(!text.contains("renamed_from"), "absorbed: {text}");
}

// ---- The policies block, ADR-0008 ----

/// A naming rule set to `error` fails `validate` with the rule's own id, and
/// a suppression with a reason takes it back out — until its expiry.
#[test]
fn a_naming_policy_fails_validate_and_a_suppression_lifts_it_until_it_expires() {
    let d = Demo::new("policynaming");
    d.table("table: dbo.t\ncolumns:\n  CustomerId: {type: int}\n");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    naming.column: {severity: error, pattern: \"[a-z_]+\"}\n",
    )
    .unwrap();
    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}", stdout(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["findings"][0]["id"], "naming.column", "{v}");
    assert_eq!(v["findings"][0]["severity"], "error", "{v}");
    assert!(
        v["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("`CustomerId`"),
        "{v}"
    );

    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    naming.column: {severity: error, pattern: \"[a-z_]+\"}\n  suppress:\n    - rule: naming.column\n      on: dbo.t\n      reason: inherited\n      until: 2999-01-01\n",
    )
    .unwrap();
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // Expired: the finding is back at its configured severity.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    naming.column: {severity: error, pattern: \"[a-z_]+\"}\n  suppress:\n    - rule: naming.column\n      on: dbo.t\n      reason: inherited\n      until: 2000-01-01\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["validate"])), FINDING);

    // And a block with a typo is refused by name rather than configuring
    // nothing: `naming.colum` silently doing nothing would be worse than the
    // rule having never been written.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    naming.colum: error\n",
    )
    .unwrap();
    let o = d.run(&["validate", "--format", "json"]);
    assert_eq!(code(&o), FINDING);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["findings"][0]["id"], "policy.invalid", "{v}");
    assert!(
        v["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("naming.colum"),
        "{v}"
    );
}

/// The plan point: adding and dropping in one table is the expand/contract
/// lint, shown under the change at the project's severity — and at `error`
/// the plan is refused before any file is written.
#[test]
fn the_expand_contract_lint_is_shown_in_the_plan_and_at_error_refuses_it() {
    let d = Demo::new("policyplan");
    d.table("table: dbo.t\ncolumns:\n  id: {type: int}\n  old: {type: int}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    // Not a rename: an add plus a drop, recorded as such.
    d.table("table: dbo.t\ncolumns:\n  id: {type: int}\n  added: {type: int}\n");
    let o = d.run(&["drop", "dbo.t.old", "--reason", "gone"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("warning: change.expand-contract"), "{out}");
    assert!(out.contains("- drop column old"), "{out}");

    // The same plan carries the finding into the saved artifact.
    let plan = d.dir.join("plan.json");
    assert_eq!(code(&d.run(&["plan", "--out", plan.to_str().unwrap()])), 0);
    let saved = std::fs::read_to_string(&plan).unwrap();
    assert!(saved.contains("change.expand-contract"), "{saved}");
    let o = d.run(&[
        "explain",
        "--plan",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "change.expand-contract"),
        "{v}"
    );

    // Raised to error: refused, with nothing written.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    change.expand-contract: error\n",
    )
    .unwrap();
    let refused = d.dir.join("refused.json");
    let o = d.run(&["plan", "--out", refused.to_str().unwrap()]);
    assert_eq!(code(&o), FINDING, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("policy"), "{}", stderr(&o));
    assert!(!refused.exists(), "a refused plan must not be written");
    let o = d.run(&["plan", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["result"], "findings", "{v}");
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "change.expand-contract" && f["severity"] == "error"),
        "{v}"
    );

    // Off: gone, and the plan is produced.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    change.expand-contract: off\n",
    )
    .unwrap();
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!stdout(&o).contains("expand-contract"), "{}", stdout(&o));
}

/// `--since` evaluates the declaration rules for the objects whose identity
/// changed since the revision, so an estate can adopt a rule one table at a
/// time — and a rename counts as changed under both names.
/// `--since` reads the declarations at a revision through git, whose pathspec
/// resolves against the current directory while `<rev>:<path>` resolves
/// against the repository root. A project in a subdirectory listed nothing,
/// so every table read as changed and an error-level rule failed on the
/// legacy names `--since` exists to leave alone. Its sibling `load_from_git`
/// already carried the `--full-tree` for this; the second instance is the
/// shape.
#[test]
fn validate_since_reads_the_revision_from_the_repository_root() {
    let d = Demo::nested("policysince-nested", "db/app");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    naming.table: {severity: error, pattern: \"[a-z_]+\"}\n",
    )
    .unwrap();
    d.table("table: dbo.OldTable\ncolumns:\n  id: {type: int}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // Nothing changed since HEAD, so nothing is evaluated — though the name
    // fails the rule, which the plain `validate` confirms.
    assert_eq!(code(&d.run(&["validate"])), FINDING, "the rule does fail");
    let o = d.run(&["validate", "--since", "HEAD"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // And a table that did change is still seen, from the same subdirectory.
    std::fs::write(
        d.dir.join("schema").join("dbo.NewTable.yml"),
        "table: dbo.NewTable\ncolumns:\n  id: {type: int}\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["validate", "--since", "HEAD", "--format", "json"]);
    assert_eq!(code(&o), FINDING);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let messages: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages.len(), 1, "{v}");
    assert!(messages[0].contains("NewTable"), "{v}");
}

/// A revision `--since` cannot resolve is a mistake, not an empty history.
///
/// Read as empty it is the loudest possible wrong answer: `changed_subjects`
/// marks every object changed, so a gradual-adoption rule fails declarations
/// nobody touched, and `plan` proposes creating the entire schema. The one
/// unresolvable revision that *is* the empty baseline is `HEAD` in a
/// repository with no commits.
#[test]
fn an_unknown_revision_is_refused_rather_than_read_as_empty() {
    let d = Demo::new("since-unknown");
    d.table("table: dbo.OldTable\ncolumns:\n  id: {type: int}\n");
    // Before the first commit HEAD does not resolve either, and that one is
    // the empty baseline: a repository with no previous version.
    assert_eq!(
        code(&d.run(&["plan"])),
        0,
        "an unborn HEAD is still the empty baseline"
    );
    d.commit();

    for args in [
        vec!["validate", "--since", "no-such-rev"],
        vec!["plan", "--since", "no-such-rev"],
    ] {
        let o = d.run(&args);
        let msg = format!("{}{}", stdout(&o), stderr(&o));
        assert_ne!(code(&o), 0, "{args:?} must not succeed: {msg}");
        assert!(msg.contains("no-such-rev"), "{args:?}: {msg}");
        assert!(msg.contains("not a revision"), "{args:?}: {msg}");
    }

    // The revision that does exist still works, and says nothing changed.
    let o = d.run(&["validate", "--since", "HEAD"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// `national text` *is* `ntext` to SQL Server — `types.rs` lists the alias —
/// so a default-on rule that matched the raw base name was bypassed by an
/// equivalent engine spelling (DECISIONS 187).
#[test]
fn a_deprecated_type_is_reported_under_its_alias() {
    let d = Demo::new("aliastype");
    for ty in ["ntext", "national text"] {
        d.table(&format!(
            "table: dbo.t\ncolumns:\n  id: {{type: int}}\n  body: {{type: \"{ty}\"}}\n"
        ));
        // A warning, so `validate` still succeeds: what matters is that the
        // finding is there.
        let o = d.run(&["validate", "--format", "json"]);
        assert_eq!(code(&o), 0, "{ty}: {}{}", stdout(&o), stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
        let ids: Vec<&str> = v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["id"].as_str())
            .collect();
        assert!(
            ids.contains(&"column.no-deprecated-type"),
            "{ty} is deprecated whichever way it is spelled: {v}"
        );
    }
}

/// `git ls-tree --name-only` C-quotes any path outside ASCII with
/// `core.quotePath` at its default — measured: `schéma/dbo.t.yml` comes back
/// as `"sch\303\251ma/dbo.t.yml"`, and `git show <rev>:<that>` answers
/// `fatal: path ... does not exist`. So every historical read of a
/// declaration under such a path failed, on a repository that is perfectly
/// well formed (DECISIONS 180).
#[test]
fn a_declaration_under_a_non_ascii_path_is_readable_at_a_revision() {
    let d = Demo::new("unicodepath");
    std::fs::remove_file(d.dir.join("schema/dbo.t.yml")).ok();
    let file = d.dir.join("schema").join("dbo.té.yml");
    std::fs::write(&file, "table: dbo.te\ncolumns:\n  id: {type: int}\n").unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // Something has to have changed, or neither command reads the old tree
    // at all and the bug hides behind the short circuit.
    std::fs::write(
        &file,
        "table: dbo.te\ncolumns:\n  id: {type: int}\n  note: {type: nvarchar(50)}\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);

    // The baseline is that revision's declarations. Skipped, it is *empty* —
    // and an empty baseline is not an error: the plan reports every table as
    // newly created and exits 0, which is the silence this codebase exists to
    // refuse (absent, empty and unreadable are three different things).
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let out = format!("{}{}", stdout(&o), stderr(&o));
    assert!(!out.contains("baseline is empty"), "{out}");
    assert!(!out.contains("0 objects"), "{out}");
    // The revision holds the table, so the plan against it is the one column
    // that was added — not a `CREATE TABLE`.
    assert!(out.contains("note"), "{out}");
    assert!(!out.contains("create table"), "{out}");

    // And `validate --since` copies the same listing out to a scratch
    // directory: the second reader, and it had the same bug. Skipped there,
    // the revision looks empty and every object reads as changed.
    let o = d.run(&["validate", "--since", "HEAD"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

#[test]
fn validate_since_evaluates_only_what_changed() {
    let d = Demo::new("policysince");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    naming.table: {severity: error, pattern: \"[a-z_]+\"}\n",
    )
    .unwrap();
    d.table("table: dbo.OldTable\ncolumns:\n  id: {type: int}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // Untouched since HEAD: nothing is evaluated, though the name fails the rule.
    assert_eq!(code(&d.run(&["validate"])), FINDING, "the rule does fail");
    let o = d.run(&["validate", "--since", "HEAD"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // A new table is changed.
    std::fs::write(
        d.dir.join("schema").join("dbo.NewTable.yml"),
        "table: dbo.NewTable\ncolumns:\n  id: {type: int}\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["validate", "--since", "HEAD", "--format", "json"]);
    assert_eq!(code(&o), FINDING);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let messages: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages.len(), 1, "{v}");
    assert!(messages[0].contains("NewTable"), "{v}");

    // A table changed in content only — same uid, same name, same columns,
    // one type widened — is the object the revision touched, and identity
    // alone never saw it: `--since` accepted an error-level rule on it. (A
    // new column would mint a uid and count as identity; a type does not.)
    d.commit();
    let o = d.run(&["validate", "--since", "HEAD"]);
    assert_eq!(
        code(&o),
        0,
        "committed: nothing changed since HEAD {}",
        stderr(&o)
    );
    d.table("table: dbo.OldTable\ncolumns:\n  id: {type: bigint}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    let o = d.run(&["validate", "--since", "HEAD", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let messages: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages.len(), 1, "{v}");
    assert!(messages[0].contains("OldTable"), "{v}");
}

/// A plan that **creates** a table with a foreign key, applied for real.
///
/// The gap this fills: no test here ever applied one, and the apply guard got
/// a created table's shape wrong twice in two commits because of it. The
/// differ takes the foreign keys out of the `CREATE` payload and emits them
/// as changes of their own, so the payload alone is not what the table will
/// hold — and a guard that compares against the payload refuses the plan's
/// own key (DECISIONS 181, 182).
#[test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
fn a_created_table_with_a_foreign_key_applies() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_newfk_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("newfk");
    d.table("table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: {name: pk_t, columns: [id]}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // A new table that references the one already there, and a second one
    // that references *it* — a foreign key between two tables this same plan
    // creates, which is why the differ splits them out at all.
    std::fs::write(
        d.dir.join("schema/dbo.child.yml"),
        "table: dbo.child\ncolumns:\n  id: {type: int, nullable: false}\n  t_id: {type: int}\n  code: {type: varchar(20)}\n  note: {type: nvarchar(50)}\nprimary_key: {name: pk_child, columns: [id]}\nunique:\n  uq_child_code: [code]\nindexes:\n  ix_child_t:\n    columns: [t_id]\n    include: [note]\nforeign_keys:\n  fk_child_t:\n    columns: [t_id]\n    references: dbo.t(id)\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/dbo.grand.yml"),
        "table: dbo.grand\ncolumns:\n  id:\n    type: int\n    nullable: false\n    identity: [1, 1]\n  child_id: {type: int}\n  amount: {type: \"decimal(18,2)\"}\n  code: {type: char(3)}\n  stamp: {type: datetime2(3)}\n  body: {type: nvarchar(max)}\n  blob: {type: varbinary(16)}\n  bare_dec: {type: decimal}\n  bare_char: {type: char}\n  bare_float: {type: float}\n  bare_nv: {type: nvarchar}\n  flag: {type: bit, nullable: false, default: \"0\"}\nprimary_key: {name: pk_grand, columns: [id]}\nforeign_keys:\n  fk_grand_child:\n    columns: [child_id]\n    references: dbo.child(id)\n    on_delete: cascade\n",
    )
    .unwrap();
    // Offline first, to mint the identities a deployment plan is pinned to.
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        // A new foreign key is `constraint` risk: it can refuse rows.
        "--allow",
        "constraint",
    ]);
    assert_eq!(
        code(&o),
        0,
        "the plan's own foreign key must not read as movement: {}{}",
        stdout(&o),
        stderr(&o)
    );

    // And the environment it recorded is the one it left: no drift.
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .ok();
    });
}

/// A plan that **reshapes a table already there**, applied for real: a column
/// added with a bare type and a default, one retyped, one loosened, one given
/// a default, the primary key replaced by an unnamed one, and a unique, an
/// index and a foreign key added.
///
/// The gap this fills is the mirror of the created-table test above. Every one
/// of these is a change the apply guard holds to the plan's own definition
/// once the plan has run (DECISIONS 189), and the comparison is against the
/// engine's read-back — `decimal` stored as `decimal(18,0)`, a default `0` as
/// `((0))`, an unnamed key as `PK__t__…`. Only the engine can say the guard
/// is not refusing its own plan, and before this no live plan added a column,
/// a key, a unique, an index or a foreign key to an existing table at all.
#[test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
fn a_plan_that_reshapes_an_existing_table_applies() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_reshape_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("reshape");
    std::fs::write(
        d.dir.join("schema/dbo.p.yml"),
        "table: dbo.p\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: {name: pk_p, columns: [id]}\n",
    )
    .unwrap();
    d.table(
        "table: dbo.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  region: {type: varchar(10), nullable: false}\n  flag: {type: bit, nullable: false}\n  note: {type: nvarchar(50)}\nprimary_key: {name: pk_t, columns: [code]}\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // The same table, reshaped in every way a plan can reshape one without
    // dropping anything. `code` keeps its type: it carries the old key, and
    // the engine refuses to retype a key column under its constraint.
    d.table(
        "table: dbo.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  region: {type: varchar(10), nullable: false}\n  flag: {type: bit}\n  note: {type: nvarchar(100), default: \"N''\"}\n  amount: {type: decimal, nullable: false, default: \"0\"}\n  p_id: {type: int}\nprimary_key: [code, region]\nunique:\n  uq_t_note: [note]\nindexes:\n  ix_t_p:\n    columns: [p_id, region desc]\n    include: [note]\nforeign_keys:\n  fk_t_p:\n    columns: [p_id]\n    references: dbo.p(id)\n    on_delete: cascade\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    // The plan holds every shape this test is about, or it proves nothing.
    let planned = std::fs::read_to_string(&plan).unwrap();
    for op in [
        "add_column",
        "alter_column_type",
        "alter_column_nullability",
        "alter_column_default",
        "set_primary_key",
        "add_unique",
        "add_index",
        "add_foreign_key",
    ] {
        assert!(
            planned.contains(&format!("\"op\":\"{op}\""))
                || planned.contains(&format!("\"op\": \"{op}\"")),
            "the plan must carry `{op}`: {planned}"
        );
    }
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        // The key, the unique and the foreign key are `constraint` risk.
        "--allow",
        "constraint",
    ]);
    assert_eq!(
        code(&o),
        0,
        "the plan's own definitions must not read as movement: {}{}",
        stdout(&o),
        stderr(&o)
    );

    // And the environment it recorded is the one it left: no drift.
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .ok();
    });
}

/// `apply` records the database read back, not the plan applied to the old
/// state — so a change another session makes while the plan is running would
/// be written down as this plan's own result, and every later `verify` would
/// call it clean (DECISIONS 150).
///
/// Staged with an `AFTER INSERT` trigger, because that is the one shape a test
/// can time exactly: it fires inside the apply's own transaction, between the
/// baseline read and the read-back, which is precisely the window. What it
/// does — putting a row into a *different* declared table — is something no
/// statement of the plan asks for, and nothing else in the run would notice:
/// the pinned checksum was answered before the statements, and the per-row
/// postconditions only speak for the rows the plan itself writes.
#[test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
fn a_change_that_lands_during_an_apply_is_not_recorded_as_the_plan_s_own() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_during_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("apply-during");
    d.table(
        "table: dbo.t
columns:
  code: {type: varchar(20), nullable: false}
  note: {type: nvarchar(50)}
primary_key: {name: pk_t, columns: [code]}
data:
  mode: exact
  rows:
    first: {note: kept}
",
    );
    std::fs::write(
        d.dir.join("schema/dbo.other.yml"),
        "table: dbo.other
columns:
  code: {type: varchar(20), nullable: false}
primary_key: {name: pk_other, columns: [code]}
data:
  mode: exact
  rows:
    kept: {}
",
    )
    .unwrap();
    let role = |grants: &str| format!("role: app\ngrants:\n  schema::dbo: [{grants}]\n");
    std::fs::write(d.dir.join("schema/app.yml"), role("select")).unwrap();
    // Offline first, to mint the identities `bootstrap` insists on.
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(code(&d.run(&["verify", "--db", &connection])), 0);

    // The other session, wound up to go off in the middle of the apply.
    // Through `EXEC`, because `CREATE TRIGGER` has to be first in its batch
    // and this connection has just said `USE`.
    //
    // It writes into a *different* declared table, and below the same test
    // runs again with a trigger that writes another row of the *same* table —
    // the shape a whole-table exemption would let through, since the
    // statement's own postcondition speaks only for the row the plan named
    // (DECISIONS 153).
    sql("EXEC(N'CREATE TRIGGER dbo.trg_t ON dbo.t AFTER INSERT AS \
         SET NOCOUNT ON; INSERT INTO dbo.other (code) VALUES (''rogue'');');");

    // A revision that touches `dbo.t` and says nothing at all about
    // `dbo.other`.
    d.table(
        "table: dbo.t
columns:
  code: {type: varchar(20), nullable: false}
  note: {type: nvarchar(50)}
primary_key: {name: pk_t, columns: [code]}
data:
  mode: exact
  rows:
    first: {note: kept}
    second: {note: new}
",
    );
    d.commit();
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "data-update,data-delete",
    ]);
    let err = format!("{}{}", stdout(&o), stderr(&o));
    assert_ne!(code(&o), 0, "the apply must not record it: {err}");
    assert!(
        err.contains("dbo.other"),
        "the refusal must name what moved: {err}"
    );
    assert!(err.contains("rolled back"), "{err}");

    // And nothing of the plan stayed: not its own row, and not the trigger's.
    let rows = |table: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&connection).await.expect("connect");
            let r = c
                .query(&format!("SELECT COUNT(*) FROM {table};"))
                .await
                .expect("count");
            r[0].try_get_at::<i32>(0).unwrap().unwrap()
        })
    };
    assert_eq!(rows("dbo.t"), 1, "the plan's insert must have rolled back");
    assert_eq!(rows("dbo.other"), 1, "the trigger's row must have gone too");
    // The ledger says what it said before — the apply recorded nothing, so
    // the environment still matches the state bootstrap wrote — and the
    // declarations still ask for the row a plan would put there. Both halves
    // matter: a recorded state is what `verify` measures against, and had the
    // read-back been written down, this environment would have been declared
    // clean against a table holding a row nobody declared.
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(
        code(&o),
        0,
        "the recorded state must be untouched: {}{}",
        stdout(&o),
        stderr(&o)
    );
    let o = d.run(&["plan", "--db", &connection]);
    assert!(
        stdout(&o).contains("row second"),
        "the plan is still to be applied: {}{}",
        stdout(&o),
        stderr(&o)
    );

    // The same again, with the trigger reaching a different row of the table
    // the plan *is* writing to. The plan names `dbo.t`, so exempting the
    // table would exempt this; the row it corrupts is one no statement of the
    // plan speaks for.
    sql("DROP TRIGGER dbo.trg_t;");
    sql("EXEC(N'CREATE TRIGGER dbo.trg_t ON dbo.t AFTER INSERT AS \
         SET NOCOUNT ON; UPDATE dbo.t SET note = N''corrupted'' WHERE code = ''first'';');");
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "data-update,data-delete",
    ]);
    let err = format!("{}{}", stdout(&o), stderr(&o));
    assert_ne!(code(&o), 0, "the apply must not record it: {err}");
    assert!(
        err.contains("row `first`"),
        "the refusal must name the row the trigger reached: {err}"
    );
    assert_eq!(rows("dbo.t"), 1, "the plan's insert must have rolled back");
    let untouched: i32 = rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&connection).await.expect("connect");
        let r = c
            .query("SELECT COUNT(*) FROM dbo.t WHERE code = 'first' AND note = N'kept';")
            .await
            .expect("count");
        r[0].try_get_at(0).unwrap().unwrap()
    });
    assert_eq!(
        untouched, 1,
        "the trigger's write must have rolled back too"
    );

    // And once more on the other half of the model: a trigger that revokes a
    // grant the plan leaves alone, on a role the plan *does* touch. Exempting
    // the whole role saw nothing, exactly as exempting the whole table did
    // (DECISIONS 156). `REVOKE` inside an `AFTER INSERT` trigger was measured
    // to work, which is what makes this stageable at all.
    sql("DROP TRIGGER dbo.trg_t;");
    sql("EXEC(N'CREATE TRIGGER dbo.trg_t ON dbo.t AFTER INSERT AS \
         SET NOCOUNT ON; REVOKE SELECT ON SCHEMA::dbo TO app;');");
    // The revision now also widens the role, so the plan names it.
    std::fs::write(d.dir.join("schema/app.yml"), role("select, insert")).unwrap();
    d.commit();
    let plan = d.dir.join("plan2.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "data-update,data-delete",
    ]);
    let err = format!("{}{}", stdout(&o), stderr(&o));
    assert_ne!(code(&o), 0, "the apply must not record it: {err}");
    assert!(
        err.contains("role app"),
        "the refusal must name the role whose grant moved: {err}"
    );
    // Rolled back whole: the grant the trigger took is back, and the grant the
    // plan wanted to add never landed.
    let permissions: i32 = rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&connection).await.expect("connect");
        let r = c
            .query(
                "SELECT COUNT(*) FROM sys.database_permissions p \
                 JOIN sys.database_principals r ON r.principal_id = p.grantee_principal_id \
                 WHERE r.name = 'app' AND p.permission_name = 'SELECT';",
            )
            .await
            .expect("count");
        r[0].try_get_at::<i32>(0).unwrap().unwrap()
    });
    assert_eq!(permissions, 1, "the trigger's revoke must have rolled back");

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        let _ = c
            .execute(&format!(
                "USE master; ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                 DROP DATABASE [{name}];"
            ))
            .await;
    });
}

/// A plan a policy refuses writes nothing at all — the identity file included.
///
/// ADR-0008 says an `error` refuses to produce the plan before any file is
/// written, and the identity file is a file. Minting a uid and then refusing
/// left the identity file changed for a plan that does not exist, so the next
/// run compared against identities no reviewed plan ever used (DECISIONS 154).
#[test]
fn a_policy_refusal_leaves_the_identity_file_alone() {
    let d = Demo::new("policyids");
    d.table("table: dbo.t\ncolumns:\n  note: {type: nvarchar(100)}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let before = std::fs::read_to_string(d.ids_path()).unwrap();

    // One revision that both narrows a column — which the rule below refuses —
    // and adds a table, which is what mints a new uid.
    d.table("table: dbo.t\ncolumns:\n  note: {type: nvarchar(50)}\n");
    std::fs::write(
        d.dir.join("schema/dbo.u.yml"),
        "table: dbo.u\ncolumns:\n  id: {type: int}\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\npolicies:\n  rules:\n    change.narrowing-on-data: error\n",
    )
    .unwrap();

    let o = d.run(&["plan", "--format", "json"]);
    assert_eq!(code(&o), FINDING, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "change.narrowing-on-data" && f["severity"] == "error"),
        "{v}"
    );

    let after = std::fs::read_to_string(d.ids_path()).unwrap();
    assert_eq!(
        before, after,
        "a refused plan must leave the identity file as it found it"
    );
    assert!(!after.contains("dbo.u"), "{after}");

    // And with the rule at its default the same revision goes through, so the
    // guard above is about the refusal and not about the write being broken.
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\n").unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    let after = std::fs::read_to_string(d.ids_path()).unwrap();
    assert!(after.contains("dbo.u"), "{after}");
}

/// A revision that moved `schema_dir` is read at the paths *it* used.
///
/// The historical tree is listed at today's `schema_dir` and the identity file
/// read from today's `ids_file`, so a revision that kept them elsewhere read
/// as an empty baseline: `plan` proposes creating the whole schema, and
/// `validate --since` marks every object changed — failing a gradual-adoption
/// policy on declarations nobody touched (DECISIONS 155).
#[test]
fn a_baseline_is_read_at_the_paths_its_own_revision_used() {
    let d = Demo::new("movedpaths");
    // The first revision keeps its declarations *nested*, which is the shape
    // that matters below: when the whole `legacy/` tree is gone, a path
    // conversion that asks git about the historical directory has to run it
    // inside a parent that does not exist either (DECISIONS 166).
    std::fs::create_dir_all(d.dir.join("legacy/schema")).unwrap();
    std::fs::write(
        d.dir.join("legacy/schema/dbo.t.yml"),
        "table: dbo.t\ncolumns:\n  note: {type: nvarchar(100)}\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nschema_dir: legacy/schema\nids_file: legacy/ids.json\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // The second moves them, and says so in its own `pbps.yml`.
    std::fs::create_dir_all(d.dir.join("db/tables")).unwrap();
    std::fs::rename(
        d.dir.join("legacy/schema/dbo.t.yml"),
        d.dir.join("db/tables/dbo.t.yml"),
    )
    .unwrap();
    std::fs::rename(d.dir.join("legacy/ids.json"), d.dir.join("db/ids.json")).unwrap();
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nschema_dir: db/tables\nids_file: db/ids.json\n",
    )
    .unwrap();

    // Against the previous revision, the only change is where the files live —
    // which is not a schema change at all.
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(
        !stdout(&o).contains("create table"),
        "moving the files is not creating the schema: {}",
        stdout(&o)
    );
    assert!(
        !stderr(&o).contains("the baseline is empty"),
        "the previous revision has declarations, at its own path: {}",
        stderr(&o)
    );

    // Even when the old tree is gone from the working copy entirely — parent
    // directory and all, which is what makes the git question unanswerable.
    std::fs::remove_dir_all(d.dir.join("legacy")).unwrap();
    let o = d.run(&["plan"]);
    assert_eq!(
        code(&o),
        0,
        "the old declarations directory is gone from the working tree: {}{}",
        stdout(&o),
        stderr(&o)
    );
    assert!(!stdout(&o).contains("create table"), "{}", stdout(&o));

    // And `--since` agrees: nothing about `dbo.t` changed, so a rule that
    // would fail it is not evaluated against it.
    d.commit();
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nschema_dir: db/tables\nids_file: db/ids.json\n\
         policies:\n  rules:\n    naming.table: {severity: error, pattern: \"^x_\"}\n",
    )
    .unwrap();
    let o = d.run(&["validate", "--since", "HEAD~1"]);
    assert_eq!(
        code(&o),
        0,
        "a table the revision did not touch must not be judged: {}{}",
        stdout(&o),
        stderr(&o)
    );
}

// ---- SPEC safety invariant repairs ----

/// Recording a baseline or snapshot changes the authoritative state every
/// later plan compares against, so it must respect the deployment lock just as
/// apply does. Otherwise a snapshot can bless the half-built schema between two
/// staged statements.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn snapshot_and_baseline_refuse_a_held_deployment_lock() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("record-lock");
    d.table("table: dbo.pbps_record_lock\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        pbps_mssql::state::lock(&mut conn, "another-deployment")
            .await
            .unwrap();
    });

    for args in [
        vec!["snapshot", "--db", &connection, "--force"],
        vec!["baseline", "--db", &connection, "--reason", "test"],
    ] {
        let o = d.run(&args);
        assert_eq!(code(&o), 1, "{}", stderr(&o));
        assert!(stderr(&o).contains("another-deployment"), "{}", stderr(&o));
    }

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        pbps_mssql::state::unlock(&mut conn).await.unwrap();
        conn.execute("DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;")
            .await
            .unwrap();
    });
}

/// A failed preflight is still a deployment attempt: it leaves the schema
/// unchanged, appends a failed ledger row, and sends the same typed attempt-hook
/// shape as success with `outcome: failure`. The legacy success hook must not
/// run for this outcome.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_failed_apply_is_audited_and_emitted_to_the_hook() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("failed-audit");
    let hook_out = d.dir.join("apply-hook.json");
    let legacy_hook_out = d.dir.join("legacy-apply-hook.json");
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: mssql\nhooks:\n  on_apply: \"cat > {}\"\n  on_apply_attempt: \"cat > {}\"\n",
            legacy_hook_out.display(),
            hook_out.display(),
        ),
    )
    .unwrap();
    d.table("table: dbo.pbps_failed_audit\ncolumns:\n  id: {type: bigint, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;")
            .await;
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;")
            .await;
        conn.execute(
            "IF OBJECT_ID(N'dbo.pbps_failed_audit', N'U') IS NOT NULL DROP TABLE dbo.pbps_failed_audit; \
             CREATE TABLE dbo.pbps_failed_audit (id bigint NOT NULL); \
             INSERT INTO dbo.pbps_failed_audit (id) VALUES (1);",
        )
        .await
        .unwrap();
    });
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );

    d.table(
        "table: dbo.pbps_failed_audit\ncolumns:\n  id: {type: bigint, nullable: false}\n  required: {type: int, nullable: false}\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    let plan = d.dir.join("failed.json");
    let made = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&made), 0, "{}", stderr(&made));
    let checksum = plan_checksum(&plan);
    let applied = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "not-null",
    ]);
    assert_eq!(code(&applied), 1, "{}", stdout(&applied));

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Failed);
        assert_eq!(latest.snapshot.plan_checksum.as_deref(), Some(checksum.as_str()));
        assert!(latest
            .snapshot
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("data will not accept")));
        let pulled = pbps_mssql::catalog::introspect(&mut conn).await.unwrap();
        assert!(!pulled.schema.tables[&"dbo.pbps_failed_audit".parse().unwrap()]
            .columns
            .contains_key("required"));
        conn.execute(
            "DROP TABLE dbo.pbps_failed_audit; DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });

    let hook: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hook_out).unwrap()).unwrap();
    assert_eq!(hook["version"], 1, "{hook}");
    assert_eq!(hook["outcome"], "failure", "{hook}");
    assert_eq!(hook["checksum"], checksum, "{hook}");
    assert_eq!(hook["plan_path"], plan.display().to_string(), "{hook}");
    assert!(hook.get("ledger_entry").is_none(), "{hook}");
    assert!(
        !legacy_hook_out.exists(),
        "the success-only on_apply hook ran after a failed apply"
    );
}

/// SQL Server DDL is transactional, but that guarantee is lost if COMMIT comes
/// before read-back or the success-ledger insert. Force the latter to fail and
/// prove the column addition is rolled back with it.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_ledger_failure_rolls_back_the_ddl_it_would_have_recorded() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("atomic-ledger");
    d.table("table: dbo.pbps_atomic_ledger\ncolumns:\n  id: {type: bigint, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.pbps_deny_state', N'TR') IS NOT NULL DROP TRIGGER dbo.pbps_deny_state;")
            .await;
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;")
            .await;
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;")
            .await;
        conn.execute(
            "IF OBJECT_ID(N'dbo.pbps_atomic_ledger', N'U') IS NOT NULL DROP TABLE dbo.pbps_atomic_ledger; \
             CREATE TABLE dbo.pbps_atomic_ledger (id bigint NOT NULL);",
        )
        .await
        .unwrap();
    });
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );
    d.table(
        "table: dbo.pbps_atomic_ledger\ncolumns:\n  id: {type: bigint, nullable: false}\n  note: {type: nvarchar(50)}\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    let plan = d.dir.join("atomic.json");
    assert_eq!(
        code(&d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()])),
        0
    );
    let checksum = plan_checksum(&plan);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "CREATE TRIGGER dbo.pbps_deny_state ON dbo.__pbps_state INSTEAD OF INSERT AS \
             BEGIN THROW 51000, 'ledger insert denied by test', 1; END;",
        )
        .await
        .unwrap();
    });
    let applied = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_eq!(code(&applied), 1, "{}", stdout(&applied));

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute("DROP TRIGGER dbo.pbps_deny_state;").await.unwrap();
        let pulled = pbps_mssql::catalog::introspect(&mut conn).await.unwrap();
        assert!(!pulled.schema.tables[&"dbo.pbps_atomic_ledger".parse().unwrap()]
            .columns
            .contains_key("note"));
        let latest = pbps_mssql::state::latest(&mut conn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Baseline);
        conn.execute(
            "DROP TABLE dbo.pbps_atomic_ledger; DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// Unsupported catalog facts inside a managed table are drift, even when the
/// expressible subset still has the same checksum.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_computed_column_inside_the_managed_set_is_reported_as_drift() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("computed-drift");
    let env = format!("PBPS_COMPUTED_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nunmanaged: ignore\nenvironments:\n  test:\n    url_env: {env}\n"),
    )
    .unwrap();
    d.table("table: dbo.pbps_computed_drift\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;")
            .await;
        let _ = conn
            .execute("IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;")
            .await;
        conn.execute(
            "IF OBJECT_ID(N'dbo.pbps_computed_drift', N'U') IS NOT NULL DROP TABLE dbo.pbps_computed_drift; \
             CREATE TABLE dbo.pbps_computed_drift (id int NOT NULL);",
        )
        .await
        .unwrap();
    });
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute("ALTER TABLE dbo.pbps_computed_drift ADD twice AS (id * 2);")
            .await
            .unwrap();
    });

    let plan = d.dir.join("partial-plan.json");
    let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&planned), 1, "{}", stdout(&planned));
    assert!(
        stderr(&planned).contains("computed"),
        "{}",
        stderr(&planned)
    );
    assert!(!plan.exists(), "a partial connected plan was written");

    let verify = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&verify)).unwrap();
    assert_eq!(code(&verify), FINDING, "{report}");
    assert!(
        report["data"]["unexpressible"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item.as_str().is_some_and(|s| s.contains("computed"))))
    );

    let status = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&env, &connection)
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(report["data"][0]["state"], "drift", "{report}");

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "DROP TABLE dbo.pbps_computed_drift; DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// A module's `depends_on` annotation disappears with its declaration. Carrying
/// it in the snapshot is what lets a later connected plan still drop the
/// dependent before the dependency.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn connected_planning_keeps_dependencies_for_deleted_modules() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("deleted-module-deps");
    d.module(
        "dbo.pbps_dep_base.yml",
        "view: dbo.pbps_dep_base\ndefinition: |-\n  SELECT 1 AS id\n",
    );
    d.module(
        "dbo.pbps_dep_leaf.yml",
        "view: dbo.pbps_dep_leaf\ndepends_on: [dbo.pbps_dep_base]\ndefinition: |-\n  SELECT 1 AS id\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_dep_leaf', N'V') IS NOT NULL DROP VIEW dbo.pbps_dep_leaf;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_dep_base', N'V') IS NOT NULL DROP VIEW dbo.pbps_dep_base;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
    });
    let boot = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&boot), 0, "{}", stderr(&boot));
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let state = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(state.snapshot.module_deps.len(), 1);
    });

    // A surviving module owns its current declaration hints. Reversing the old
    // edge must replace it rather than adding both directions and manufacturing
    // a cycle that falls back to name order.
    d.module(
        "dbo.pbps_dep_base.yml",
        "view: dbo.pbps_dep_base\ndepends_on: [dbo.pbps_dep_leaf]\ndefinition: |-\n  SELECT 2 AS id\n",
    );
    d.module(
        "dbo.pbps_dep_leaf.yml",
        "view: dbo.pbps_dep_leaf\ndefinition: |-\n  SELECT 2 AS id\n",
    );
    let reordered_path = d.dir.join("reordered-modules.json");
    let reordered = d.run(&[
        "plan",
        "--db",
        &connection,
        "--out",
        reordered_path.to_str().unwrap(),
    ]);
    assert_eq!(code(&reordered), 0, "{}", stderr(&reordered));
    let reordered: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&reordered_path).unwrap()).unwrap();
    let altered: Vec<String> = reordered
        .changes
        .changes
        .iter()
        .filter(|planned| matches!(planned.change, pbps_model::Change::AlterModule { .. }))
        .filter_map(|planned| planned.change.module_name().map(ToString::to_string))
        .collect();
    assert_eq!(altered, ["dbo.pbps_dep_leaf", "dbo.pbps_dep_base"]);

    std::fs::remove_file(d.dir.join("schema/dbo.pbps_dep_leaf.yml")).unwrap();
    std::fs::remove_file(d.dir.join("schema/dbo.pbps_dep_base.yml")).unwrap();
    let plan_path = d.dir.join("drop-modules.json");
    let planned = d.run(&[
        "plan",
        "--db",
        &connection,
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    let plan: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan_path).unwrap()).unwrap();
    let dropped: Vec<String> = plan
        .changes
        .changes
        .iter()
        .filter(|planned| matches!(planned.change, pbps_model::Change::DropModule { .. }))
        .filter_map(|planned| planned.change.module_name().map(ToString::to_string))
        .collect();
    assert_eq!(dropped, ["dbo.pbps_dep_leaf", "dbo.pbps_dep_base"]);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "DROP VIEW dbo.pbps_dep_leaf; DROP VIEW dbo.pbps_dep_base; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// `status` is a report rather than a gate, but it must still reflect the
/// configured unmanaged-object policy instead of silently acting as `ignore`.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn status_reports_the_unmanaged_error_policy() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("status-unmanaged");
    let env = format!("PBPS_UNMANAGED_{}", std::process::id());
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nunmanaged: error\nenvironments:\n  test:\n    url_env: {env}\n"),
    )
    .unwrap();
    d.table("table: dbo.pbps_status_managed\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn.execute("IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;").await;
        let _ = conn.execute("IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;").await;
        conn.execute(
            "IF OBJECT_ID(N'dbo.pbps_status_managed', N'U') IS NOT NULL DROP TABLE dbo.pbps_status_managed; \
             IF OBJECT_ID(N'dbo.pbps_status_unmanaged', N'U') IS NOT NULL DROP TABLE dbo.pbps_status_unmanaged; \
             CREATE TABLE dbo.pbps_status_managed (id int NOT NULL);",
        )
        .await
        .unwrap();
    });
    // Adopt before introducing the object that violates the policy.
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nunmanaged: ignore\nenvironments:\n  test:\n    url_env: {env}\n"),
    )
    .unwrap();
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nunmanaged: error\nenvironments:\n  test:\n    url_env: {env}\n"),
    )
    .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute("CREATE TABLE dbo.pbps_status_unmanaged (id int NOT NULL);")
            .await
            .unwrap();
    });

    let status = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&env, &connection)
        .output()
        .unwrap();
    assert_eq!(code(&status), 0, "{}", stderr(&status));
    let report: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(report["data"][0]["state"], "policy", "{report}");
    assert!(report["findings"].as_array().is_some_and(|findings| {
        findings
            .iter()
            .any(|f| f["id"] == "state.unmanaged-refused")
    }));

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "DROP TABLE dbo.pbps_status_unmanaged; DROP TABLE dbo.pbps_status_managed; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// A module can be present in `sys.objects` while SQL Server withholds its
/// definition. It is still an unmanaged object, so encryption must not turn
/// `unmanaged: error` into an accidental allow-list escape in either estate
/// report. The policy is not drift and therefore must not fire `on_drift`.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn verify_and_status_reject_an_unreadable_unmanaged_module() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("verify-unreadable-unmanaged");
    let env = format!("PBPS_UNREADABLE_POLICY_{}", std::process::id());
    let drift_hook = d.dir.join("drift-hook.json");
    d.table("table: dbo.pbps_unreadable_policy\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.[pbps_unreadable.policy_secret]', N'P') IS NOT NULL \
                 DROP PROCEDURE dbo.[pbps_unreadable.policy_secret];",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_unreadable_policy', N'U') IS NOT NULL \
                 DROP TABLE dbo.pbps_unreadable_policy;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        conn.execute("CREATE TABLE dbo.pbps_unreadable_policy (id int NOT NULL);")
            .await
            .unwrap();
    });
    // Adopt the managed table before introducing the policy violation.
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: mssql\nunmanaged: error\nenvironments:\n  test:\n    url_env: {env}\n\
             hooks:\n  on_drift: \"cat > {}\"\n",
            drift_hook.display()
        ),
    )
    .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "CREATE PROCEDURE dbo.[pbps_unreadable.policy_secret] WITH ENCRYPTION AS SELECT 1;",
        )
        .await
        .unwrap();
    });

    // Warning policy still has to reach the JSON findings. The catalog cannot
    // put this encrypted procedure in `scoped.unmanaged_modules`, so this pins
    // the separate unreadable inventory all the way through verify's envelope.
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: mssql\nunmanaged: warn\nenvironments:\n  test:\n    url_env: {env}\n\
             hooks:\n  on_drift: \"cat > {}\"\n",
            drift_hook.display()
        ),
    )
    .unwrap();
    let warned = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let warned_report: serde_json::Value = serde_json::from_str(&stdout(&warned)).unwrap();
    assert_eq!(code(&warned), 0, "{warned_report}");
    assert!(warned_report["findings"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["id"] == "state.unmanaged"
                && item["message"]
                    .as_str()
                    .is_some_and(|s| s.contains("pbps_unreadable.policy_secret"))
        })
    }));

    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: mssql\nunmanaged: error\nenvironments:\n  test:\n    url_env: {env}\n\
             hooks:\n  on_drift: \"cat > {}\"\n",
            drift_hook.display()
        ),
    )
    .unwrap();

    let verify = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&verify)).unwrap();
    assert_eq!(code(&verify), FINDING, "{report}");
    assert!(
        report["findings"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["id"] == "state.unmanaged-refused"
                    && item["message"]
                        .as_str()
                        .is_some_and(|s| s.contains("pbps_unreadable.policy_secret"))
            })),
        "{report}"
    );
    assert!(
        report.get("data").is_none(),
        "policy is not drift: {report}"
    );
    assert!(!drift_hook.exists(), "a policy refusal fired on_drift");

    let status = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&env, &connection)
        .output()
        .unwrap();
    assert_eq!(code(&status), 0, "{}", stderr(&status));
    let report: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(report["data"][0]["state"], "policy", "{report}");
    assert!(report["findings"].as_array().is_some_and(|findings| {
        findings.iter().any(|finding| {
            finding["id"] == "state.unmanaged-refused"
                && finding["message"]
                    .as_str()
                    .is_some_and(|s| s.contains("pbps_unreadable.policy_secret"))
        })
    }));

    // A separate managed drift must survive beside the policy verdict. The
    // policy-only runs above intentionally have no drift data and no hook;
    // after this catalog change both the completed report and its alert are
    // required.
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute("ALTER TABLE dbo.pbps_unreadable_policy ADD drifted int NULL;")
            .await
            .unwrap();
    });
    let verify = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&verify)).unwrap();
    assert_eq!(code(&verify), FINDING, "{report}");
    let finding_ids: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|finding| finding["id"].as_str())
        .collect();
    assert!(finding_ids.contains(&"state.drift"), "{report}");
    assert!(finding_ids.contains(&"state.unmanaged-refused"), "{report}");
    assert!(
        report.get("data").is_some(),
        "drift report was discarded: {report}"
    );
    assert!(drift_hook.exists(), "managed drift did not fire on_drift");

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "DROP PROCEDURE dbo.[pbps_unreadable.policy_secret]; \
             DROP TABLE dbo.pbps_unreadable_policy; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// Rejecting plan B while plan A is interrupted must leave A's checkpoint
/// identity intact. Otherwise a second B attempt can pass the checksum check
/// and use A's progress as its own resume offset.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_failed_resume_does_not_relabel_the_interrupted_plan() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("resume-checksum-audit");
    d.table("table: dbo.pbps_resume_checksum\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_plan_b', N'V') IS NOT NULL DROP VIEW dbo.pbps_plan_b;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_resume_checksum', N'U') IS NOT NULL \
                 DROP TABLE dbo.pbps_resume_checksum;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        conn.execute("CREATE TABLE dbo.pbps_resume_checksum (id int NOT NULL);")
            .await
            .unwrap();
    });
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );

    let interrupted_checksum = "a".repeat(64);
    let checkpoint = rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        let mut checkpoint = latest.snapshot;
        checkpoint.kind = pbps_model::StateKind::Staged;
        checkpoint.git_sha = Some("plan-a-revision".into());
        checkpoint.plan_checksum = Some(interrupted_checksum.clone());
        checkpoint.staged = Some(pbps_model::StagedProgress {
            completed: 1,
            total: 2,
            last_statement: "plan A statement 1".into(),
        });
        pbps_mssql::state::record(&mut conn, &checkpoint)
            .await
            .unwrap();
        checkpoint
    });

    let plan_path = d.dir.join("plan-b.json");
    let plan_b = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "mssql",
        "2026-09-04T00:00:00Z",
        pbps_model::PlanBaseline {
            description: "interrupted checkpoint".into(),
            checksum: pbps_model::state_checksum(&checkpoint.schema, &checkpoint.ids),
        },
        pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::CreateModule {
                    name: "dbo.pbps_plan_b".parse().unwrap(),
                    module: Box::new(pbps_model::Module {
                        kind: pbps_model::ModuleKind::View,
                        description: None,
                        on: None,
                        definition: "SELECT 1 AS id".into(),
                    }),
                },
            )],
        },
        checkpoint.ids.clone(),
    )
    .staged();
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan_b).unwrap()).unwrap();
    let attempted_checksum = plan_b.checksum();
    assert_ne!(attempted_checksum, interrupted_checksum);

    for _ in 0..2 {
        let applied = d.run(&[
            "apply",
            "--db",
            &connection,
            "--plan",
            plan_path.to_str().unwrap(),
            "--checksum",
            &attempted_checksum,
            "--staged",
            "--resume",
        ]);
        assert_eq!(code(&applied), 1, "{}", stdout(&applied));
        assert!(
            stderr(&applied).contains("different plan"),
            "{}",
            stderr(&applied)
        );

        rt.block_on(async {
            let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
            let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
            assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Failed);
            assert_eq!(
                latest.snapshot.plan_checksum.as_deref(),
                Some(interrupted_checksum.as_str())
            );
            assert_eq!(latest.snapshot.git_sha.as_deref(), Some("plan-a-revision"));
            let progress = latest.snapshot.staged.as_ref().unwrap();
            assert_eq!((progress.completed, progress.total), (1, 2));
        });
    }

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let pulled = pbps_mssql::catalog::introspect(&mut conn).await.unwrap();
        assert!(
            !pulled
                .schema
                .modules
                .contains_key(&"dbo.pbps_plan_b".parse().unwrap())
        );
        conn.execute(
            "DROP TABLE dbo.pbps_resume_checksum; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// DDL and the success row commit before lock cleanup. If DELETE on the lock
/// table fails, the command must surface that cleanup problem while still
/// emitting the successful apply event with its ledger id.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn an_unlock_failure_does_not_relabel_a_successful_apply() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("unlock-after-success");
    let hook_out = d.dir.join("apply-hook.json");
    let legacy_hook_out = d.dir.join("legacy-apply-hook.json");
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: mssql\nunmanaged: ignore\nhooks:\n  on_apply: \"cat > {}\"\n  on_apply_attempt: \"cat > {}\"\n",
            legacy_hook_out.display(),
            hook_out.display(),
        ),
    )
    .unwrap();
    d.table("table: dbo.pbps_unlock_success\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_block_unlock', N'TR') IS NOT NULL \
                 DROP TRIGGER dbo.pbps_block_unlock;",
            )
            .await;
        let _ = pbps_mssql::state::unlock(&mut conn).await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.pbps_unlock_success', N'U') IS NOT NULL \
                 DROP TABLE dbo.pbps_unlock_success;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock;",
            )
            .await;
        let _ = conn
            .execute(
                "IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;",
            )
            .await;
        conn.execute("CREATE TABLE dbo.pbps_unlock_success (id int NOT NULL);")
            .await
            .unwrap();
    });
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );

    d.table(
        "table: dbo.pbps_unlock_success\ncolumns:\n  id: {type: int, nullable: false}\n  note: {type: nvarchar(20)}\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    let plan = d.dir.join("unlock-success.json");
    let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    let checksum = plan_checksum(&plan);

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "CREATE TRIGGER dbo.pbps_block_unlock ON dbo.__pbps_lock INSTEAD OF DELETE AS \
             BEGIN THROW 51000, 'unlock denied by test', 1; END;",
        )
        .await
        .unwrap();
    });

    let applied = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_eq!(code(&applied), 1, "cleanup should still fail");
    assert!(stdout(&applied).contains("Applied 1 change(s)"));
    assert!(stderr(&applied).contains("unlock denied by test"));

    let hook: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hook_out).unwrap()).unwrap();
    assert_eq!(hook["version"], 1, "{hook}");
    assert_eq!(hook["outcome"], "success", "{hook}");
    assert!(hook["ledger_entry"].is_number(), "{hook}");
    assert!(hook.get("error").is_none(), "{hook}");
    assert_eq!(
        std::fs::read_to_string(&legacy_hook_out).unwrap(),
        std::fs::read_to_string(&plan).unwrap(),
        "the legacy on_apply hook must still receive the unchanged plan JSON"
    );

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Apply);
        assert_eq!(hook["ledger_entry"], latest.id);
        let pulled = pbps_mssql::catalog::introspect(&mut conn).await.unwrap();
        assert!(
            pulled.schema.tables[&"dbo.pbps_unlock_success".parse().unwrap()]
                .columns
                .contains_key("note")
        );

        conn.execute("DROP TRIGGER dbo.pbps_block_unlock;")
            .await
            .unwrap();
        assert!(pbps_mssql::state::unlock(&mut conn).await.unwrap());
        conn.execute(
            "DROP TABLE dbo.pbps_unlock_success; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// Snapshot, baseline and bootstrap all make their ledger entry durable before
/// deleting the deployment lock. If that cleanup fails, reporting only the
/// error makes a retry look appropriate even though it would append another
/// successful entry (and, for bootstrap, repeat already-committed DDL).
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn durable_record_commands_report_success_before_an_unlock_failure() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("unlock-after-record");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nunmanaged: ignore\n",
    )
    .unwrap();
    d.table("table: dbo.pbps_unlock_record\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);

    const BLOCK_UNLOCK: &str = "CREATE TRIGGER dbo.pbps_block_unlock ON dbo.__pbps_lock INSTEAD OF DELETE AS \
         BEGIN THROW 51000, 'unlock denied by test', 1; END;";

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "IF OBJECT_ID(N'dbo.pbps_block_unlock', N'TR') IS NOT NULL \
             DROP TRIGGER dbo.pbps_block_unlock; \
             IF OBJECT_ID(N'dbo.pbps_unlock_record', N'U') IS NOT NULL \
             DROP TABLE dbo.pbps_unlock_record; \
             IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock; \
             IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state; \
             CREATE TABLE dbo.pbps_unlock_record (id int NOT NULL);",
        )
        .await
        .unwrap();
        pbps_mssql::state::lock(&mut conn, "unlock test setup")
            .await
            .unwrap();
        assert!(pbps_mssql::state::unlock(&mut conn).await.unwrap());
        conn.execute(BLOCK_UNLOCK).await.unwrap();
    });

    let baselined = d.run(&[
        "baseline",
        "--db",
        &connection,
        "--reason",
        "unlock regression",
    ]);
    assert_eq!(code(&baselined), 1, "cleanup should still fail");
    assert!(
        stdout(&baselined).contains("Baselined"),
        "{}",
        stdout(&baselined)
    );
    assert!(
        stdout(&baselined).contains("Reason recorded: unlock regression"),
        "{}",
        stdout(&baselined)
    );
    assert!(stderr(&baselined).contains("unlock denied by test"));

    let baseline_id = rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Baseline);
        conn.execute("DROP TRIGGER dbo.pbps_block_unlock;")
            .await
            .unwrap();
        assert!(pbps_mssql::state::unlock(&mut conn).await.unwrap());
        conn.execute(BLOCK_UNLOCK).await.unwrap();
        latest.id
    });

    let snapshotted = d.run(&["snapshot", "--db", &connection, "--force"]);
    assert_eq!(code(&snapshotted), 1, "cleanup should still fail");
    assert!(
        stdout(&snapshotted).contains("Recorded the state"),
        "{}",
        stdout(&snapshotted)
    );
    assert!(stderr(&snapshotted).contains("unlock denied by test"));

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        assert!(latest.id > baseline_id);
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Apply);
        conn.execute("DROP TRIGGER dbo.pbps_block_unlock;")
            .await
            .unwrap();
        assert!(pbps_mssql::state::unlock(&mut conn).await.unwrap());
        conn.execute(
            "DROP TABLE dbo.pbps_unlock_record; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();

        // Bootstrap needs an empty managed target, while the trigger needs the
        // internal lock table to exist before the command starts.
        pbps_mssql::state::lock(&mut conn, "unlock test setup")
            .await
            .unwrap();
        assert!(pbps_mssql::state::unlock(&mut conn).await.unwrap());
        conn.execute(BLOCK_UNLOCK).await.unwrap();
    });

    let bootstrapped = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&bootstrapped), 1, "cleanup should still fail");
    assert!(
        stdout(&bootstrapped).contains("Bootstrapped"),
        "{}",
        stdout(&bootstrapped)
    );
    assert!(stderr(&bootstrapped).contains("unlock denied by test"));

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Bootstrap);
        let pulled = pbps_mssql::catalog::introspect(&mut conn).await.unwrap();
        assert!(
            pulled
                .schema
                .tables
                .contains_key(&"dbo.pbps_unlock_record".parse().unwrap())
        );

        conn.execute("DROP TRIGGER dbo.pbps_block_unlock;")
            .await
            .unwrap();
        assert!(pbps_mssql::state::unlock(&mut conn).await.unwrap());
        conn.execute(
            "DROP TABLE dbo.pbps_unlock_record; \
             DROP TABLE dbo.__pbps_lock; DROP TABLE dbo.__pbps_state;",
        )
        .await
        .unwrap();
    });
}

/// A declared module the catalog cannot read back is a partial schema inside
/// the managed set, and the recorders refuse it the way they refuse a computed
/// column on a managed table. Recorded as a warning instead, the declaration
/// exempted the module from `unmanaged: error` for the `baseline` alone: the
/// snapshot's schema did not hold it, the scope every later command rebuilds
/// from that schema forgot it, and the first `verify` refused an untouched
/// database as a policy violation.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_declared_module_the_catalog_cannot_read_is_never_recorded() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("declared-unreadable");
    let env = format!("PBPS_DECLARED_UNREADABLE_{}", std::process::id());
    let config = |unmanaged: &str| {
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: mssql\nunmanaged: {unmanaged}\nenvironments:\n  test:\n    url_env: {env}\n"),
        )
        .unwrap();
    };
    config("error");
    d.table("table: dbo.pbps_declared_unreadable\ncolumns:\n  id: {type: int, nullable: false}\n");
    d.module(
        "secret.yml",
        "procedure: dbo.pbps_declared_secret\ndefinition: AS SELECT 1\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    const RESET: &str = "IF OBJECT_ID(N'dbo.pbps_declared_secret', N'P') IS NOT NULL \
         DROP PROCEDURE dbo.pbps_declared_secret; \
         IF OBJECT_ID(N'dbo.pbps_declared_later', N'P') IS NOT NULL \
         DROP PROCEDURE dbo.pbps_declared_later; \
         IF OBJECT_ID(N'dbo.pbps_declared_unreadable', N'U') IS NOT NULL \
         DROP TABLE dbo.pbps_declared_unreadable; \
         IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock; \
         IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;";
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(RESET).await.unwrap();
        conn.execute(
            "CREATE TABLE dbo.pbps_declared_unreadable (id int NOT NULL); \
             EXEC('CREATE PROCEDURE dbo.pbps_declared_secret WITH ENCRYPTION AS SELECT 1;');",
        )
        .await
        .unwrap();
    });

    // Both recorders refuse, name the module and the reason, write no ledger
    // entry, and leave no lock behind.
    for args in [
        vec!["baseline", "--db", connection.as_str(), "--reason", "adopt"],
        vec!["snapshot", "--db", connection.as_str(), "--force"],
    ] {
        let refused = d.run(&args);
        assert_eq!(code(&refused), 1, "{args:?}: {}", stderr(&refused));
        let err = stderr(&refused);
        assert!(
            err.contains("cannot be represented")
                && err.contains("procedure dbo.pbps_declared_secret is in the managed set")
                && err.contains("WITH ENCRYPTION"),
            "{args:?}: {err}"
        );
        assert!(!err.contains("not declared"), "{args:?}: {err}");
        rt.block_on(async {
            let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
            assert!(
                pbps_mssql::state::latest(&mut conn)
                    .await
                    .unwrap()
                    .is_none(),
                "{args:?} recorded a partial schema"
            );
            assert!(
                pbps_mssql::state::lock_holder(&mut conn)
                    .await
                    .unwrap()
                    .is_none(),
                "{args:?} left the lock held"
            );
        });
    }

    // Without the declaration the module is simply unmanaged, and a baseline
    // taken under `warn` stays consistent with every command after it: the
    // module is a note in `verify`, never a refusal.
    std::fs::remove_file(d.dir.join("schema/secret.yml")).unwrap();
    config("warn");
    let baselined = d.run(&["baseline", "--db", &connection, "--reason", "adopt"]);
    assert_eq!(code(&baselined), 0, "{}", stderr(&baselined));
    let verify = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&verify)).unwrap();
    assert_eq!(code(&verify), 0, "{report}");
    let ids: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["state.unmanaged"], "{report}");
    assert!(
        report["findings"][0]["message"]
            .as_str()
            .is_some_and(|s| s.contains("pbps_declared_secret")),
        "{report}"
    );

    // The one way a recorded module can become unreadable: it was readable
    // when recorded and somebody encrypted it by hand since. That is drift the
    // differ cannot phrase, and both `verify` and `status` have to say so.
    d.module(
        "later.yml",
        "procedure: dbo.pbps_declared_later\ndefinition: AS SELECT 2\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan = d.dir.join("later.json");
    let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    let checksum = plan_checksum(&plan);
    let applied = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_eq!(code(&applied), 0, "{}", stderr(&applied));
    let clean = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&clean), 0, "{}", stderr(&clean));

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(
            "EXEC('ALTER PROCEDURE dbo.pbps_declared_later WITH ENCRYPTION AS SELECT 2;');",
        )
        .await
        .unwrap();
    });
    let verify = d.run(&["verify", "--db", &connection, "--format", "json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&verify)).unwrap();
    assert_eq!(code(&verify), FINDING, "{report}");
    assert!(
        report["findings"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["id"] == "state.drift-unexpressible"
                    && item["message"].as_str().is_some_and(|s| {
                        s.contains("procedure dbo.pbps_declared_later is in the managed set")
                    })
            })
        }),
        "{report}"
    );
    let status = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["status", "--format", "json"])
        .env(&env, &connection)
        .output()
        .unwrap();
    assert_eq!(code(&status), 0, "{}", stderr(&status));
    let report: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(report["data"][0]["state"], "drift", "{report}");
    assert!(
        stdout(&status).contains("procedure dbo.pbps_declared_later is in the managed set"),
        "{report}"
    );

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(RESET).await.unwrap();
    });
}

/// A declared table whose every column is unsupported is left out of the
/// scoped schema whole (decision 14), so it is present in the database and
/// absent from the projection. Bootstrap's emptiness check read only the
/// projection, called the target empty, and ran: the CREATE failed on the
/// table that was there, and the failure audit then recorded an *empty* state
/// as the newest one — which the next `verify` under `unmanaged: ignore`
/// believed. The managed-set limitations are objects too, and count.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn bootstrap_refuses_a_declared_table_the_projection_left_out() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("bootstrap-partial");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nunmanaged: ignore\n",
    )
    .unwrap();
    d.table("table: dbo.pbps_boot_partial\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    const RESET: &str = "IF OBJECT_ID(N'dbo.pbps_boot_partial', N'U') IS NOT NULL \
         DROP TABLE dbo.pbps_boot_partial; \
         IF TYPE_ID(N'dbo.pbps_boot_udt') IS NOT NULL DROP TYPE dbo.pbps_boot_udt; \
         IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock; \
         IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;";
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(RESET).await.unwrap();
        // Its own batch: a type is not visible to the batch that creates it.
        conn.execute("CREATE TYPE dbo.pbps_boot_udt FROM int;")
            .await
            .unwrap();
        conn.execute("CREATE TABLE dbo.pbps_boot_partial (c dbo.pbps_boot_udt NULL);")
            .await
            .unwrap();
    });

    let refused = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&refused), 1, "{}", stdout(&refused));
    let err = stderr(&refused);
    assert!(
        err.contains("already has") && err.contains("dbo.pbps_boot_partial"),
        "{err}"
    );
    assert!(
        err.contains("user-defined type"),
        "the reason is named: {err}"
    );

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        // Refused before the transaction, so no failure audit — and no empty
        // "newest state" for a database that is not empty.
        assert!(
            pbps_mssql::state::latest(&mut conn)
                .await
                .unwrap()
                .is_none(),
            "bootstrap ran, failed, and recorded an empty state as the newest"
        );
        assert!(
            pbps_mssql::state::lock_holder(&mut conn)
                .await
                .unwrap()
                .is_none(),
            "the refusal left the lock held"
        );
        conn.execute(RESET).await.unwrap();
    });
}

/// A failed command releases the lock it took, and when that release fails
/// too the operator has to hear about it: the command's own error stays the
/// error, but a `__pbps_lock` row nobody mentioned makes the next attempt fail
/// as "locked" with nothing to explain why. Every command that takes the lock
/// has this failure path, so every one is exercised.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn an_unreleased_lock_after_a_failed_command_is_reported() {
    let connection = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let d = Demo::new("unlock-after-failure");
    let hook_out = d.dir.join("apply-hook.json");
    let config = |unmanaged: &str| {
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: mssql\nunmanaged: {unmanaged}\nhooks:\n  on_apply_attempt: \"cat > {}\"\n",
                hook_out.display()
            ),
        )
        .unwrap();
    };
    config("ignore");
    d.table("table: dbo.pbps_unlock_failure\ncolumns:\n  id: {type: int, nullable: false}\n");
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    const BLOCK_UNLOCK: &str = "CREATE TRIGGER dbo.pbps_block_unlock ON dbo.__pbps_lock INSTEAD OF DELETE AS \
         BEGIN THROW 51000, 'unlock denied by test', 1; END;";
    const RESET: &str = "IF OBJECT_ID(N'dbo.pbps_block_unlock', N'TR') IS NOT NULL \
         DROP TRIGGER dbo.pbps_block_unlock; \
         IF OBJECT_ID(N'dbo.pbps_unlock_failure', N'U') IS NOT NULL \
         DROP TABLE dbo.pbps_unlock_failure; \
         IF OBJECT_ID(N'dbo.pbps_unlock_stray', N'U') IS NOT NULL \
         DROP TABLE dbo.pbps_unlock_stray; \
         IF OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NOT NULL DROP TABLE dbo.__pbps_lock; \
         IF OBJECT_ID(N'dbo.__pbps_state', N'U') IS NOT NULL DROP TABLE dbo.__pbps_state;";
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(RESET).await.unwrap();
        conn.execute("CREATE TABLE dbo.pbps_unlock_failure (id int NOT NULL);")
            .await
            .unwrap();
    });
    assert_eq!(
        code(&d.run(&["baseline", "--db", &connection, "--reason", "test"])),
        0
    );

    d.table(
        "table: dbo.pbps_unlock_failure\ncolumns:\n  id: {type: int, nullable: false}\n  note: {type: nvarchar(20)}\n",
    );
    assert_eq!(code(&d.run(&["plan"])), 0);
    let plan = d.dir.join("unlock-failure.json");
    let planned = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&planned), 0, "{}", stderr(&planned));
    let checksum = plan_checksum(&plan);

    // The database moves after the plan was approved, and the lock row can no
    // longer be deleted. Each command below fails for its own reason first,
    // then fails to release.
    let block = || {
        rt.block_on(async {
            let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
            conn.execute(BLOCK_UNLOCK).await.unwrap();
        })
    };
    let clear = || {
        rt.block_on(async {
            let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
            conn.execute("DROP TRIGGER dbo.pbps_block_unlock;")
                .await
                .unwrap();
            assert!(
                pbps_mssql::state::unlock(&mut conn).await.unwrap(),
                "the failed command should have left the lock held"
            );
        })
    };
    let assert_reported = |what: &str, out: &Output, own_error: &str| {
        assert_eq!(code(out), 1, "{what}: {}", stderr(out));
        let err = stderr(out);
        assert!(err.contains(own_error), "{what} lost its own error: {err}");
        assert!(
            err.contains("was not released")
                && err.contains("unlock denied by test")
                && err.contains("pbps unlock"),
            "{what} hid the unreleased lock: {err}"
        );
    };

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute("ALTER TABLE dbo.pbps_unlock_failure ADD drifted int NULL;")
            .await
            .unwrap();
    });
    block();
    let applied = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]);
    assert_reported(
        "apply",
        &applied,
        "is no longer the database this plan was computed against",
    );
    // The hook sees the deployment's failure, not the cleanup's.
    let hook: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hook_out).unwrap()).unwrap();
    assert_eq!(hook["outcome"], "failure", "{hook}");
    let hook_error = hook["error"].as_str().unwrap();
    assert!(
        hook_error.contains("no longer the database") && !hook_error.contains("unlock denied"),
        "{hook}"
    );
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        let latest = pbps_mssql::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Failed);
    });
    clear();

    block();
    let snapshotted = d.run(&["snapshot", "--db", &connection]);
    assert_reported("snapshot", &snapshotted, "differs from the state recorded");
    clear();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute("CREATE TABLE dbo.pbps_unlock_stray (id int NOT NULL);")
            .await
            .unwrap();
    });
    config("error");
    block();
    let baselined = d.run(&["baseline", "--db", &connection, "--reason", "test"]);
    assert_reported("baseline", &baselined, "are not declared");
    clear();

    config("ignore");
    block();
    let bootstrapped = d.run(&["bootstrap", "--db", &connection]);
    assert_reported("bootstrap", &bootstrapped, "already has");
    clear();

    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(&connection).await.unwrap();
        conn.execute(RESET).await.unwrap();
    });
}

/// A plan that renames a table a role is granted on still applies.
///
/// Only a live server settles it: SQL Server carries an object-level grant
/// across `sp_rename`, so the permission is the same permission afterwards
/// under a different name, and the differ emits no grant change at all. The
/// post-apply movement guard compares the recorded state with the read-back,
/// and comparing grant targets by name alone read that one grant as two — and
/// refused every rename of a granted table (DECISIONS 157).
#[test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
fn a_rename_of_a_granted_table_is_applied_rather_than_read_as_movement() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_grantrename_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("grantrename-live");
    d.table("table: dbo.old\ncolumns:\n  code: {type: varchar(20), nullable: false}\n");
    std::fs::write(
        d.dir.join("schema/app.yml"),
        "role: app\ngrants:\n  dbo.old: [select]\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // The rename, with the grant following it in the declarations exactly as
    // the engine will follow it in the catalog.
    d.table(
        "table: dbo.new\nrenamed_from: dbo.old\ncolumns:\n  code: {type: varchar(20), nullable: false}\n",
    );
    std::fs::write(
        d.dir.join("schema/app.yml"),
        "role: app\ngrants:\n  dbo.new: [select]\n",
    )
    .unwrap();
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "OUT:{}ERR:{}", stdout(&o), stderr(&o));
    d.commit();

    let plan = d.dir.join("rename.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    // The differ has nothing to say about the grant: the engine moves it, so
    // the rename is the whole plan. Read off the artifact rather than the
    // printed summary, which mentions the target's name.
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(
        matches!(
            saved.changes.changes.as_slice(),
            [one] if matches!(one.change, pbps_model::Change::RenameTable { .. })
        ),
        "the rename alone is the plan: {:?}",
        saved.changes
    );

    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "rename",
    ]);
    assert_eq!(
        code(&o),
        0,
        "a rename of a granted table must apply: {}{}",
        stdout(&o),
        stderr(&o)
    );
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // And the grant really did travel, which is the engine fact the guard
    // now depends on.
    let held: i32 = rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&connection).await.expect("connect");
        let r = c
            .query(
                "SELECT COUNT(*) FROM sys.database_permissions p \
                 JOIN sys.database_principals r ON r.principal_id = p.grantee_principal_id \
                 WHERE r.name = 'app' AND p.class = 1 \
                   AND OBJECT_NAME(p.major_id) = 'new' AND p.permission_name = 'SELECT';",
            )
            .await
            .expect("count");
        r[0].try_get_at::<i32>(0).unwrap().unwrap()
    });
    assert_eq!(held, 1, "the grant follows the rename");

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        let _ = c
            .execute(&format!(
                "USE master; ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                 DROP DATABASE [{name}];"
            ))
            .await;
    });
}

/// A staged apply notices a change that lands between two of its reads.
///
/// It cannot roll back — that is what `--staged` is for — so the remedy is to
/// record the checkpoint and stop, rather than carry the change into every
/// later read and finally into the closing ordinary snapshot, which is what
/// `verify` measures against ever after (DECISIONS 159).
#[test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
fn a_staged_apply_stops_at_a_change_that_is_not_its_own() {
    let Ok(server) = std::env::var("PBPS_TEST_DB") else {
        panic!("PBPS_TEST_DB is not set");
    };
    let name = format!("pbps_cli_stagedmove_{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sql = |sql: &str| {
        rt.block_on(async {
            let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
            c.execute(&format!("USE [{name}]; {sql}")).await.expect(sql);
        })
    };
    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        c.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
    });
    let connection = format!("{server};Database={name}");

    let d = Demo::new("stagedmove-live");
    let rows = |rows: &str| {
        format!(
            "table: dbo.t\ncolumns:\n  code: {{type: varchar(20), nullable: false}}\n\
             primary_key: {{name: pk_t, columns: [code]}}\ndata:\n  mode: exact\n  rows:\n{rows}"
        )
    };
    d.table(&rows("    first: {}\n"));
    std::fs::write(
        d.dir.join("schema/dbo.other.yml"),
        "table: dbo.other\ncolumns:\n  code: {type: varchar(20), nullable: false}\n\
         primary_key: {name: pk_other, columns: [code]}\ndata:\n  mode: exact\n  rows:\n    kept: {}\n",
    )
    .unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // Wound up to go off inside the one statement the staged plan runs, and
    // aimed at a table the plan never mentions.
    sql("EXEC(N'CREATE TRIGGER dbo.trg_t ON dbo.t AFTER INSERT AS \
         SET NOCOUNT ON; INSERT INTO dbo.other (code) VALUES (''rogue'');');");

    // One logical change, which is all `--staged` accepts.
    d.table(&rows("    first: {}\n    second: {}\n"));
    d.commit();
    let plan = d.dir.join("staged.json");
    let o = d.run(&[
        "plan",
        "--db",
        &connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--staged",
    ]);
    let err = format!("{}{}", stdout(&o), stderr(&o));
    assert_ne!(code(&o), 0, "the run must stop: {err}");
    assert!(err.contains("dbo.other"), "it must name what moved: {err}");
    assert!(
        err.contains("nothing was rolled back") || err.contains("staged apply runs outside"),
        "and say that nothing was undone: {err}"
    );

    // The statement did commit and the checkpoint records it — that is what a
    // checkpoint is for — but the environment is left mid-deployment rather
    // than blessed as a finished apply.
    let o = d.run(&["status", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let states: Vec<&str> = v["environments"]
        .as_array()
        .map(|envs| envs.iter().filter_map(|e| e["state"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        states.contains(&"staged") || stdout(&o).contains("staged"),
        "the environment is mid-deployment: {}",
        stdout(&o)
    );

    rt.block_on(async {
        let mut c = pbps_db::Conn::connect(&server).await.expect("connect");
        let _ = c
            .execute(&format!(
                "USE master; ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                 DROP DATABASE [{name}];"
            ))
            .await;
    });
}
