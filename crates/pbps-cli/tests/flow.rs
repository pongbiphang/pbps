//! End-to-end flow tests.
//!
//! Unit tests cover the logic of each layer, but what a user actually meets is
//! what happens when a command runs: the exit code, the message, and whether a
//! file was written. CI depends entirely on exit codes, so they are pinned here.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Demo {
    dir: PathBuf,
}

impl Demo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-flow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(dir.join("pbps.yml"), "dialect: mssql\n").unwrap();

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

#[test]
fn plan_writes_the_change_set_as_json() {
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
    assert_eq!(json["changes"][0]["op"], "add_column");
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
        "version": 1,
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
