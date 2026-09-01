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
        1,
        "an ambiguity must exit non-zero, or CI cannot block on it"
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
    assert_eq!(code(&o), 1, "a drop without a reason must be blocked");
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
    assert_eq!(code(&o), 1);
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
fn an_invalid_declaration_is_rejected() {
    let d = Demo::new("invalid");
    d.table("table: no_schema_prefix\ncolumns:\n  a: {type: int}\n");
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 1);
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
        "version": 3,
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
        1,
        "a file that is not canonical should exit non-zero"
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
    assert_eq!(code(&d.run(&["fmt", "--check"])), 1);
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
    assert_eq!(code(&o), 1, "a scrambled identity file must fail validate");
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
    assert_eq!(code(&o), 1);
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

    let o = d.run(&[
        "apply",
        // A port nothing listens on, so a connection that should never be
        // attempted is refused instantly rather than waiting out a timeout.
        "--db",
        "Server=127.0.0.1,1;Database=nowhere;User Id=u;Password=p",
        "--plan",
        plan.to_str().unwrap(),
        "--allow",
        "destructive",
    ]);
    assert_eq!(code(&o), 1);
    let err = stderr(&o);
    assert!(err.contains("preview"), "{err}");
    assert!(err.contains("plan --db"), "the remedy must be named: {err}");
    assert!(!err.contains("connect"), "it must not have tried: {err}");
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
    let plan = format!(
        r#"{{
  "version": 2,
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
    let o = d.run(&[
        "apply",
        "--db",
        unreachable,
        "--plan",
        plain.to_str().unwrap(),
        "--staged",
    ]);
    assert_eq!(code(&o), 1);
    assert!(stderr(&o).contains("transactional plan"), "{}", stderr(&o));

    // ...and a staged plan applied without it.
    let staged = write_plan(&d, "staged.json", "staged");
    let o = d.run(&[
        "apply",
        "--db",
        unreachable,
        "--plan",
        staged.to_str().unwrap(),
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
