//! `pbps partitions extend` without a database (#1789, DEC-1789.1).
//!
//! The command reads and writes declaration files and nothing else, so what it
//! writes is checked here against the commands that read those files next:
//! `fmt --check` and `validate`. That the engine reads the spellings it
//! generates back unchanged, and that the partitions plan, apply, verify and
//! replan empty, is `flow_pg.rs`'s
//! `a_generated_series_is_planned_applied_and_verified_through_the_cli`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str, dialect: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-extend-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(dir.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        Self { dir }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join("schema").join(file)
    }

    /// Writes a declaration and has `fmt` canonicalize the whole project, so
    /// a test states its tree in whatever shape reads best and starts from
    /// the file `fmt` would write.
    fn declare(&self, file: &str, text: &str) {
        std::fs::write(self.path(file), text).unwrap();
        let o = self.run(&["fmt"]);
        assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    }

    /// Sets the project's `naming.table` rule, in `pbps.yml`.
    fn naming_table(&self, severity: &str, pattern: &str) {
        std::fs::write(
            self.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\npolicies:\n  rules:\n    naming.table: \
                 {{severity: {severity}, pattern: \"{pattern}\"}}\n"
            ),
        )
        .unwrap();
    }

    /// Replaces `pbps.yml` with `text`, as the project's own file.
    fn pbps_yml(&self, text: &str) {
        std::fs::write(self.dir.join("pbps.yml"), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--project")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn extend(&self, table: &str, every: &str, until: &str) -> Output {
        // `--every=` so that a step beginning with `-` reaches the command as
        // a value and is refused there, by the grammar that owns it.
        self.run(&[
            "partitions",
            "extend",
            table,
            &format!("--every={every}"),
            &format!("--until={until}"),
        ])
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        collect(&self.dir, &mut out);
        out.sort();
        out
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn collect(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            out.push((path, bytes));
        }
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

const EVENTS: &str = "table: app.ev\n\
    columns:\n  id: {type: integer, nullable: false}\n  ts: {type: date, nullable: false}\n\
    primary_key: [id, ts]\n\
    partition_by: [ts]\n\
    partitions:\n  ev_2026_11: {from: ['2026-11-01'], to: ['2026-12-01']}\n  ev_rest: default\n";

/// What it writes passes `fmt --check` and `validate`, and the diff is the new
/// entries alone, in the places the parent's file keeps its partitions.
#[test]
fn the_extended_file_is_canonical_valid_and_differs_by_the_new_entries_alone() {
    let p = Project::new("canonical", "postgres");
    p.declare("app.ev.yml", EVENTS);
    let before = p.read("app.ev.yml");

    let o = p.extend("app.ev", "1month", "2027-03-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let said = stdout(&o);
    assert!(
        said.contains("Added 3 partition(s) to app.ev")
            && said.contains("app.ev_2026_12  [2026-12-01, 2027-01-01)")
            && said.contains("app.ev_2027_02  [2027-02-01, 2027-03-01)")
            && said.contains("DEFAULT partition")
            && said.contains("pbps plan"),
        "{said}"
    );

    let after = p.read("app.ev.yml");
    let added: Vec<&str> = after.lines().filter(|l| !before.contains(l)).collect();
    assert_eq!(
        added,
        [
            "  ev_2026_12: {from: [\"2026-12-01\"], to: [\"2027-01-01\"]}",
            "  ev_2027_01: {from: [\"2027-01-01\"], to: [\"2027-02-01\"]}",
            "  ev_2027_02: {from: [\"2027-02-01\"], to: [\"2027-03-01\"]}",
        ]
    );
    // Nothing was removed or reordered: the old lines are all still there, in
    // order.
    let kept: Vec<&str> = after.lines().filter(|l| before.contains(l)).collect();
    assert_eq!(kept, before.lines().collect::<Vec<_>>());

    for args in [&["fmt", "--check"][..], &["validate"][..]] {
        let o = p.run(args);
        assert_eq!(code(&o), 0, "{args:?}: {}{}", stdout(&o), stderr(&o));
    }
    // No identity was written: `plan` assigns them.
    assert!(
        !p.files()
            .iter()
            .any(|(path, _)| path.to_string_lossy().contains("ids")),
        "{:?}",
        p.files().iter().map(|f| &f.0).collect::<Vec<_>>()
    );
}

/// A second run finds the series where the first left it and writes nothing,
/// byte for byte, and says so.
#[test]
fn a_second_run_writes_nothing() {
    let p = Project::new("idempotent", "postgres");
    p.declare("app.ev.yml", EVENTS);
    assert_eq!(code(&p.extend("app.ev", "1month", "2027-03-01")), 0);
    let first = p.files();

    for until in ["2027-03-01", "2027-01-15", "2026-01-01"] {
        let o = p.extend("app.ev", "1month", until);
        assert_eq!(code(&o), 0, "{until}: {}{}", stdout(&o), stderr(&o));
        assert!(
            stdout(&o).contains("already reaches `2027-03-01`") && stdout(&o).contains("nothing"),
            "{until}: {}",
            stdout(&o)
        );
        assert_eq!(p.files(), first, "{until} changed a file");
    }
}

/// Only the parent's file changes; every other file, the declarations of other
/// tables and a role among them, is the same bytes.
#[test]
fn other_files_are_left_untouched() {
    let p = Project::new("others", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.declare(
        "app.other.yml",
        "table: app.other\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\n",
    );
    std::fs::create_dir_all(p.path("roles")).unwrap();
    p.declare(
        "roles/reader.yml",
        "role: reader\n\ngrants:\n  \"schema::app\": [usage]\n  app.other: [select]\n",
    );
    let before = p.files();

    let o = p.extend("app.ev", "1month", "2027-01-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let after = p.files();
    assert_eq!(before.len(), after.len());
    let changed: Vec<&PathBuf> = before
        .iter()
        .zip(&after)
        .filter(|(a, b)| a != b)
        .map(|(a, _)| &a.0)
        .collect();
    assert_eq!(changed, [&p.path("app.ev.yml")]);
}

/// A file `fmt` would rewrite is refused with `fmt` as the remedy, and left
/// alone: rewriting it would put a reformatting into the same diff.
#[test]
fn a_file_that_is_not_canonical_is_refused_and_left_alone() {
    let p = Project::new("noncanonical", "postgres");
    p.declare("app.ev.yml", EVENTS);
    let canonical = p.read("app.ev.yml");
    // A harmless difference `fmt` would erase.
    std::fs::write(p.path("app.ev.yml"), format!("# kept by hand\n{canonical}")).unwrap();
    let before = p.files();
    let o = p.extend("app.ev", "1month", "2027-01-01");
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("not in canonical form") && stderr(&o).contains("pbps fmt"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);
    // And once formatted, it goes through.
    assert_eq!(code(&p.run(&["fmt"])), 0);
    assert_eq!(code(&p.extend("app.ev", "1month", "2027-01-01")), 0);
}

/// A pending `renamed_from` is a one-shot annotation `fmt` keeps until the ids
/// file absorbs it; the generator must keep it too.
#[test]
fn a_pending_rename_in_the_parents_file_is_kept() {
    let p = Project::new("renamed", "postgres");
    p.declare("app.ev.yml", &format!("renamed_from: app.old_ev\n{EVENTS}"));
    let before = p.read("app.ev.yml");
    assert!(before.contains("renamed_from: app.old_ev"), "{before}");
    let o = p.extend("app.ev", "1month", "2027-01-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let after = p.read("app.ev.yml");
    assert!(after.contains("renamed_from: app.old_ev"), "{after}");
    assert_eq!(code(&p.run(&["fmt", "--check"])), 0);
}

/// Every refusal leaves every file as it was, and says what to do.
#[test]
fn what_cannot_be_extended_is_refused_and_nothing_is_written() {
    // (the table's file, the table, every, until, what the refusal says)
    let cases: &[(&str, &str, &str, &str, &str)] = &[
        // An end that is not a whole number of steps after the frontier, with
        // the nearest that are.
        (
            EVENTS,
            "app.ev",
            "1month",
            "2027-03-15",
            "`2027-03-01` and `2027-04-01`",
        ),
        // A step with no unit, and one that is not positive.
        (EVENTS, "app.ev", "1", "2027-03-01", "has no unit"),
        (
            EVENTS,
            "app.ev",
            "0month",
            "2027-03-01",
            "no positive whole number",
        ),
        (
            EVENTS,
            "app.ev",
            "-1month",
            "2027-03-01",
            "no positive whole number",
        ),
        // An end spelled as another type's value.
        (
            EVENTS,
            "app.ev",
            "1month",
            "2027-03",
            "not how the engine prints a date bound",
        ),
        // A table that is not declared, and a partition.
        (
            EVENTS,
            "app.nope",
            "1month",
            "2027-03-01",
            "is not declared",
        ),
        (
            EVENTS,
            "app.ev_rest",
            "1month",
            "2027-03-01",
            "is a partition of `app.ev`",
        ),
        // A table that is not partitioned.
        (
            "table: app.ev\ncolumns:\n  id: {type: integer, nullable: false}\n",
            "app.ev",
            "1month",
            "2027-03-01",
            "declares no `partition_by:`",
        ),
        // Another tool's partitions.
        (
            "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\npartition_by: [ts]\npartitions: external\n",
            "app.ev",
            "1month",
            "2027-03-01",
            "partitions: external",
        ),
        // A key of several columns.
        (
            "table: app.ev\ncolumns:\n  tenant: {type: integer, nullable: false}\n  ts: {type: date, nullable: false}\npartition_by: [tenant, ts]\npartitions:\n  p: {from: [1, '2026-01-01'], to: [1, '2026-02-01']}\n",
            "app.ev",
            "1month",
            "2027-03-01",
            "2 columns",
        ),
        // A key that has no step.
        (
            "table: app.ev\ncolumns:\n  k: {type: text, nullable: false}\npartition_by: [k]\npartitions:\n  p: {from: ['a'], to: ['b']}\n",
            "app.ev",
            "1month",
            "2027-03-01",
            "a series is generated for a `date`",
        ),
        // A range that already reaches MAXVALUE.
        (
            "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\npartition_by: [ts]\npartitions:\n  p: {from: ['2026-01-01'], to: [MAXVALUE]}\n",
            "app.ev",
            "1month",
            "2027-03-01",
            "reaches MAXVALUE",
        ),
        // No range declared and no --from.
        (
            "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\npartition_by: [ts]\n",
            "app.ev",
            "1month",
            "2027-03-01",
            "--from",
        ),
    ];
    for (i, (file, table, every, until, says)) in cases.iter().enumerate() {
        let p = Project::new(&format!("refused-{i}"), "postgres");
        std::fs::write(p.path("app.ev.yml"), file).unwrap();
        // Canonical where the file loads, so the refusal is the case's own and
        // not the canonical-form check.
        let _ = p.run(&["fmt"]);
        let before = p.files();
        let o = p.extend(table, every, until);
        assert_ne!(code(&o), 0, "case {i}: {}{}", stdout(&o), stderr(&o));
        assert!(
            stderr(&o).contains(says),
            "case {i}: want `{says}` in\n{}",
            stderr(&o)
        );
        assert_eq!(p.files(), before, "case {i} wrote a file");
    }
}

/// A name the series would give a partition that something else in the schema
/// holds is refused before a byte is written: another table, or an object of
/// another kind that shares the schema's relation namespace.
#[test]
fn a_generated_name_that_is_taken_is_refused_and_nothing_is_written() {
    let p = Project::new("taken", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.declare(
        "app.ev_2027_01.yml",
        "table: app.ev_2027_01\ncolumns:\n  id: {type: integer, nullable: false}\n",
    );
    let before = p.files();
    let o = p.extend("app.ev", "1month", "2027-03-01");
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("`app.ev_2027_01` is already declared"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);

    // A view of that name is not a table, and the declaration checks that
    // `validate` runs are what refuse it, after the file is generated and
    // before it is written.
    let q = Project::new("taken-view", "postgres");
    q.declare("app.ev.yml", EVENTS);
    q.declare(
        "app.ev_2027_01.view.yml",
        "view: app.ev_2027_01\ndefinition: |-\n  SELECT 1 AS one\n",
    );
    assert_eq!(code(&q.run(&["validate"])), 0, "the view alone is valid");
    let before = q.files();
    let o = q.extend("app.ev", "1month", "2027-03-01");
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("would make the declarations invalid")
            && stderr(&o).contains("ev_2027_01"),
        "{}",
        stderr(&o)
    );
    assert_eq!(q.files(), before);
}

/// A tree that already fails `validate` is not extended: what the checks say
/// afterwards must be about the generated partitions alone.
#[test]
fn a_tree_that_already_has_problems_is_not_extended() {
    let p = Project::new("invalid", "postgres");
    // Two default partitions: `check_partitions` refuses the tree.
    p.declare(
        "app.ev.yml",
        &EVENTS.replace(
            "  ev_rest: default\n",
            "  ev_rest: default\n  ev_other: default\n",
        ),
    );
    assert_ne!(code(&p.run(&["validate"])), 0);
    let before = p.files();
    let o = p.extend("app.ev", "1month", "2027-03-01");
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("already have") && stderr(&o).contains("pbps validate"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);
}

/// The names a table-naming rule accepts: the existing partitions (`ev_2026_11`
/// and `ev_rest`) and the monthly ones the generator writes, but not a daily
/// one.
const MONTHLY: &str = "^ev(_[0-9]{4}_[0-9]{2}|_rest)?$";

/// `validate` runs the project's `policies:`, and so must the command that
/// writes declarations: a rule set to `error` that the generated names break
/// is named before a byte is written, not by the `validate` after it
/// (#1904).
#[test]
fn a_generated_name_an_error_rule_rejects_is_refused_and_nothing_is_written() {
    let p = Project::new("policy-refused", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.naming_table("error", MONTHLY);
    assert_eq!(code(&p.run(&["validate"])), 0, "the tree is clean to start");
    let before = p.files();

    // Daily names (`ev_2026_12_01`) are not the monthly scheme the rule asks.
    let o = p.extend("app.ev", "1day", "2026-12-03");
    assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let said = stderr(&o);
    assert!(
        said.contains("naming.table")
            && said.contains("ev_2026_12_01")
            && said.contains("ev_2026_12_02")
            && said.contains("nothing was written"),
        "{said}"
    );
    assert_eq!(p.files(), before, "a file changed");
}

/// The positive twin: the same project and rule, a series whose names the rule
/// accepts, is written, and the `validate` after it is clean.
#[test]
fn a_generated_name_an_error_rule_accepts_is_written() {
    let p = Project::new("policy-accepted", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.naming_table("error", MONTHLY);
    let before = p.read("app.ev.yml");

    let o = p.extend("app.ev", "1month", "2027-02-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_ne!(p.read("app.ev.yml"), before);
    let v = p.run(&["validate"]);
    assert_eq!(code(&v), 0, "{}{}", stdout(&v), stderr(&v));
}

/// A finding the tree already had is not this change's: it does not block the
/// command, and it is not named as if the series had caused it. A finding the
/// series does add still refuses, beside it.
#[test]
fn an_error_finding_the_tree_already_had_does_not_block_the_command() {
    let p = Project::new("policy-existing", "postgres");
    p.declare("app.ev.yml", EVENTS);
    // `ev_rest` already breaks this one, and the monthly names do not.
    p.naming_table("error", "^ev(_[0-9]{4}_[0-9]{2})?$");
    let v = p.run(&["validate"]);
    assert_ne!(code(&v), 0, "the tree already has the finding");
    assert!(stderr(&v).contains("ev_rest"), "{}", stderr(&v));

    // Daily names add findings of their own: refused, and `ev_rest` is not
    // among those named.
    let before = p.files();
    let o = p.extend("app.ev", "1day", "2026-12-03");
    assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("ev_2026_12_01") && !stderr(&o).contains("ev_rest"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);

    // Monthly names add none: written, and `validate` still names only the
    // finding it had.
    let o = p.extend("app.ev", "1month", "2027-02-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_ne!(p.files(), before);
    let v = p.run(&["validate"]);
    assert_ne!(code(&v), 0);
    assert!(
        stderr(&v).contains("ev_rest") && !stderr(&v).contains("ev_2026_12"),
        "{}",
        stderr(&v)
    );
}

/// A rule below `error` is advice, as it is for `validate`, which prints it
/// beside a valid project.
#[test]
fn a_rule_below_error_does_not_block_the_command() {
    let p = Project::new("policy-warning", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.naming_table("warning", MONTHLY);

    let o = p.extend("app.ev", "1day", "2026-12-03");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let v = p.run(&["validate"]);
    assert_eq!(code(&v), 0, "{}{}", stdout(&v), stderr(&v));
    assert!(stderr(&v).contains("ev_2026_12_01"), "{}", stderr(&v));
}

/// `naming.table` at `error` with no pattern: the rule cannot run, and
/// `validate` names the block as `policy.invalid`.
const NO_PATTERN: &str =
    "dialect: postgres\npolicies:\n  rules:\n    naming.table: {severity: error}\n";

/// A `policies:` block that is itself invalid is not "no findings": the
/// evaluator skips the rule it cannot run, so a delta of findings before and
/// after would be empty and the command would write while `validate` refuses
/// the project. Refused first, by the finding `validate` names it with, and
/// nothing is written (#1942).
#[test]
fn a_policies_block_with_a_problem_is_refused_and_nothing_is_written() {
    let p = Project::new("policy-invalid", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.pbps_yml(NO_PATTERN);
    let v = p.run(&["validate", "--format", "json"]);
    assert_ne!(code(&v), 0, "the premise: validate refuses this project");
    assert!(stdout(&v).contains("\"policy.invalid\""), "{}", stdout(&v));
    let before = p.files();

    let o = p.extend("app.ev", "1month", "2027-02-01");
    assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let said = stderr(&o);
    assert!(
        said.contains("policy.invalid")
            && said.contains("naming.table")
            && said.contains("1 problem(s)")
            && said.contains("nothing was written")
            && said.contains("pbps validate"),
        "{said}"
    );
    assert_eq!(p.files(), before, "a file changed");
}

/// The positive twin: the same command on the same tree, once the block is
/// valid, writes, and the `validate` after it is clean. The only difference
/// from the refusal above is the pattern the rule was missing.
#[test]
fn the_same_series_is_written_once_the_policies_block_is_valid() {
    let p = Project::new("policy-repaired", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.pbps_yml(NO_PATTERN);
    let before = p.files();
    let o = p.extend("app.ev", "1month", "2027-02-01");
    assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(p.files(), before);

    p.naming_table("error", MONTHLY);
    let o = p.extend("app.ev", "1month", "2027-02-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(p.read("app.ev.yml").contains("ev_2027_01"));
    let v = p.run(&["validate"]);
    assert_eq!(code(&v), 0, "{}{}", stdout(&v), stderr(&v));
}

/// Every problem of the block is named, whatever its shape, and the count is
/// the block's: a misspelled rule id, a rule switched on by a YAML boolean and
/// a rule missing its pattern are each `policy.invalid` to `validate`.
#[test]
fn every_problem_in_the_policies_block_is_named() {
    let p = Project::new("policy-invalid-all", "postgres");
    p.declare("app.ev.yml", EVENTS);
    p.pbps_yml(
        "dialect: postgres\npolicies:\n  rules:\n    naming.tabel: {severity: error, \
         pattern: \"^x$\"}\n    naming.column: on\n    naming.table: {severity: error}\n",
    );
    let v = p.run(&["validate"]);
    assert_ne!(code(&v), 0);
    let before = p.files();

    let o = p.extend("app.ev", "1month", "2027-02-01");
    assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let said = stderr(&o);
    assert!(
        said.contains("3 problem(s)")
            && said.contains("naming.tabel")
            && said.contains("naming.column")
            && said.contains("naming.table"),
        "{said}"
    );
    assert_eq!(p.files(), before);
}

/// The refusal is about the block, not about the series: it also answers a
/// request that would add nothing, which would otherwise say "nothing to add"
/// to a project `validate` refuses. A block whose rules are only advice, or
/// off, is not a problem and does not refuse.
#[test]
fn an_invalid_policies_block_refuses_even_when_there_is_nothing_to_add() {
    let p = Project::new("policy-invalid-noop", "postgres");
    p.declare("app.ev.yml", EVENTS);
    // The series already reaches 2026-12-01.
    p.pbps_yml(NO_PATTERN);
    let o = p.extend("app.ev", "1month", "2026-12-01");
    assert_ne!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("policy.invalid"), "{}", stderr(&o));
    assert!(!stdout(&o).contains("nothing to add"), "{}", stdout(&o));

    // Off, or a warning with its pattern: a valid block, and the same request
    // is answered as before.
    p.pbps_yml("dialect: postgres\npolicies:\n  rules:\n    naming.table: off\n");
    let o = p.extend("app.ev", "1month", "2026-12-01");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("nothing to add"), "{}", stdout(&o));
}

/// What the engine cannot extend is still said first, by name: an invalid
/// block does not hide that SQL Server has no RANGE partitions behind a
/// problem the user could fix and still not get a series.
#[test]
fn the_engine_refusal_comes_before_the_policies_block() {
    let p = Project::new("policy-invalid-mssql", "mssql");
    std::fs::write(
        p.path("dbo.ev.yml"),
        "table: dbo.ev\ncolumns:\n  ts: {type: date, nullable: false}\npartition_by: [ts]\n",
    )
    .unwrap();
    p.pbps_yml("dialect: mssql\npolicies:\n  rules:\n    naming.table: {severity: error}\n");
    let before = p.files();
    let o = p.extend("dbo.ev", "1month", "2027-03-01");
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("mssql does not support generating a series of range partitions")
            && !stderr(&o).contains("policy.invalid"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);
}

/// SQL Server has no RANGE partitions: refused by name, nothing written.
#[test]
fn sql_server_is_refused_by_name() {
    let p = Project::new("mssql", "mssql");
    std::fs::write(
        p.path("dbo.ev.yml"),
        "table: dbo.ev\ncolumns:\n  ts: {type: date, nullable: false}\npartition_by: [ts]\n",
    )
    .unwrap();
    let before = p.files();
    let o = p.extend("dbo.ev", "1month", "2027-03-01");
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("mssql does not support generating a series of range partitions"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);
}

/// With no range declared the series starts at --from, and only then is --from
/// accepted; a start off the grid is refused.
#[test]
fn from_starts_a_series_only_where_none_is_declared() {
    let p = Project::new("from", "postgres");
    p.declare(
        "app.ev.yml",
        "table: app.ev\ncolumns:\n  ts: {type: timestamptz, nullable: false}\npartition_by: [ts]\n",
    );
    let before = p.files();
    // Another zone's offset is not generated.
    let o = p.run(&[
        "partitions",
        "extend",
        "app.ev",
        "--every",
        "1week",
        "--from",
        "2026-12-28 00:00:00+08",
        "--until",
        "2027-01-11 00:00:00+00",
    ]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("not in UTC"), "{}", stderr(&o));
    assert_eq!(p.files(), before);

    let o = p.run(&[
        "partitions",
        "extend",
        "app.ev",
        "--every",
        "1week",
        "--from",
        "2026-12-28 00:00:00+00",
        "--until",
        "2027-01-11 00:00:00+00",
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let text = p.read("app.ev.yml");
    assert!(
        text.contains(
            "  ev_2026_12_28: {from: [\"2026-12-28 00:00:00+00\"], to: [\"2027-01-04 00:00:00+00\"]}\n  ev_2027_01_04: {from: [\"2027-01-04 00:00:00+00\"], to: [\"2027-01-11 00:00:00+00\"]}\n"
        ),
        "{text}"
    );
    assert_eq!(code(&p.run(&["fmt", "--check"])), 0);
    assert_eq!(code(&p.run(&["validate"])), 0);

    // Now a range is declared, and --from is no longer the start.
    let o = p.run(&[
        "partitions",
        "extend",
        "app.ev",
        "--every",
        "1week",
        "--from",
        "2026-12-28 00:00:00+00",
        "--until",
        "2027-02-01 00:00:00+00",
    ]);
    assert_ne!(code(&o), 0);
    assert!(
        stderr(&o).contains("accepted only for a parent that declares no range"),
        "{}",
        stderr(&o)
    );
}

/// An integer key steps by a count and is named by its lower bound; the bound
/// is its decimal text.
#[test]
fn an_integer_key_is_extended_by_a_count() {
    let p = Project::new("integer", "postgres");
    p.declare(
        "app.ev.yml",
        "table: app.ev\ncolumns:\n  id: {type: bigint, nullable: false}\npartition_by: [id]\npartitions:\n  ev_0: {from: [0], to: [1000]}\n",
    );
    let o = p.extend("app.ev", "1000", "3000");
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let text = p.read("app.ev.yml");
    assert!(
        text.contains("  ev_1000: {from: [\"1000\"], to: [\"2000\"]}\n  ev_2000: {from: [\"2000\"], to: [\"3000\"]}\n"),
        "{text}"
    );
    assert_eq!(code(&p.run(&["fmt", "--check"])), 0);
    // A unit means nothing to an integer key.
    let before = p.files();
    let o = p.extend("app.ev", "1month", "5000");
    assert_ne!(code(&o), 0);
    assert!(
        stderr(&o).contains("plain positive whole number"),
        "{}",
        stderr(&o)
    );
    assert_eq!(p.files(), before);
}
