//! `pbps ids migrate | fmt | resolve` against real projects and real git (#2600).

use pbps_model::ids_tree;
use pbps_model::{IdsFile, Tombstone};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The project directory, and the root of the git repository around it (the
/// same directory unless the project lives in a subdirectory).
struct Project(PathBuf, PathBuf);

impl Project {
    fn new() -> Self {
        Self::in_subdirectory("")
    }
    fn in_subdirectory(sub: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "pbps-ids-cmd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir, root)
    }
    fn write(&self, rel: &str, text: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.0.join(rel)).unwrap()
    }
    fn has(&self, rel: &str) -> bool {
        self.0.join(rel).exists()
    }
    fn pbps(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbps"))
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&self.1)
            .output()
            .unwrap()
    }
    fn git_ok(&self, args: &[&str]) {
        let out = self.git(args);
        assert!(out.status.success(), "git {args:?}: {}", text(&out));
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const CUSTOMER: &str = "table: dbo.customer\ncolumns:\n  customer_id: {type: int}\n  email: {type: text}\n  full_name: {type: text}\n";

/// The old identity file for the project below: a table with a tombstoned
/// column, a dropped table, a role, a type and an extension.
fn legacy() -> IdsFile {
    let uid = |s: &str| s.parse().unwrap();
    let tomb = |was: &str| Tombstone {
        was: was.into(),
        dropped_at: "2026-08-30".into(),
        reason: "REG-1".into(),
        operator: "leon".into(),
        require: None,
    };
    let mut ids = IdsFile::default();
    ids.tables
        .insert(uid("t_aaaaaa"), "dbo.customer".parse().unwrap());
    for (u, n) in [
        ("c_aaaaa1", "customer_id"),
        ("c_aaaaa2", "full_name"),
        ("c_aaaaa4", "email"),
    ] {
        ids.columns
            .insert(uid(u), format!("dbo.customer.{n}").parse().unwrap());
    }
    ids.tombstones
        .insert(uid("c_aaaaa3"), tomb("dbo.customer.national_id"));
    ids.tombstones.insert(uid("t_dddddd"), tomb("dbo.legacy"));
    ids.roles.insert(uid("r_aaaaaa"), "app_reader".into());
    ids.types
        .insert(uid("y_aaaaaa"), "dbo.status".parse().unwrap());
    ids.extensions.insert(uid("e_aaaaaa"), "pgcrypto".into());
    ids
}

fn legacy_project() -> Project {
    legacy_project_in("")
}

fn legacy_project_in(sub: &str) -> Project {
    let p = Project::in_subdirectory(sub);
    p.write(
        "pbps.yml",
        "# the project\ndialect: postgres\nids_file: schema.ids.json\n",
    );
    p.write("schema/dbo/customer.yml", CUSTOMER);
    p.write("schema/roles/app_reader.yaml", "role: app_reader\n");
    p.write(
        "schema/types/status.yml",
        "enum: dbo.status\nvalues: [new, done]\n",
    );
    p.write(
        "schema/extensions/pgcrypto.yml",
        "extension: pgcrypto\nschema: public\nversion: \"1.3\"\n",
    );
    p.write(
        "schema.ids.json",
        &(serde_json::to_string_pretty(&legacy()).unwrap() + "\n"),
    );
    p
}

fn tree_of(p: &Project) -> std::collections::BTreeMap<String, String> {
    pbps_load::read_ids_tree(&p.0.join(".pbps/ids")).unwrap()
}

fn assert_ok(out: &Output) {
    assert!(out.status.success(), "{}", text(out));
}

fn assert_refused(out: &Output, needle: &str) {
    assert!(!out.status.success(), "unexpected success: {}", text(out));
    assert!(
        text(out).contains(needle),
        "{needle:?} not in: {}",
        text(out)
    );
}

// ---------------------------------------------------------------- migrate ---

#[test]
fn migrate_writes_one_file_per_declaration_and_reads_back_to_the_same_mapping() {
    let p = legacy_project();
    assert_ok(&p.pbps(&["ids", "migrate"]));
    for rel in [
        ".pbps/ids/dbo/customer.json",
        ".pbps/ids/roles/app_reader.json",
        ".pbps/ids/types/status.json",
        ".pbps/ids/extensions/pgcrypto.json",
        ".pbps/ids/dbo.legacy.t_dddddd.json",
    ] {
        assert!(p.has(rel), "{rel}");
    }
    // The tombstoned column stays in its table's file, and the columns follow the YAML.
    let customer = p.read(".pbps/ids/dbo/customer.json");
    assert!(customer.contains("national_id"));
    let order: Vec<_> = ["customer_id", "email", "full_name"]
        .iter()
        .map(|n| customer.find(n).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{customer}");
    assert_eq!(ids_tree::assemble(&tree_of(&p)).unwrap(), legacy());
    // The config moved to `ids_dir`, comments kept; the old file is gone.
    let config = p.read("pbps.yml");
    assert!(
        config.contains("# the project") && config.contains("ids_dir: .pbps/ids"),
        "{config}"
    );
    assert!(!config.contains("ids_file"));
    assert!(!p.has("schema.ids.json"));
}

#[test]
fn migrate_is_idempotent_and_finishes_an_interrupted_run() {
    let p = legacy_project();
    assert_ok(&p.pbps(&["ids", "migrate"]));
    let again = p.pbps(&["ids", "migrate"]);
    assert_ok(&again);
    assert!(
        text(&again).contains("already migrated"),
        "{}",
        text(&again)
    );
    // A crash after the tree and the delete, before the config: only the config is left.
    p.write("pbps.yml", "dialect: postgres\nids_file: schema.ids.json\n");
    let finished = p.pbps(&["ids", "migrate"]);
    assert_ok(&finished);
    assert!(p.read("pbps.yml").contains("ids_dir: .pbps/ids"));
    assert!(!p.read("pbps.yml").contains("ids_file"));
    // A crash after the tree, before the delete: the same tree is accepted beside the file.
    let q = legacy_project();
    assert_ok(&q.pbps(&["ids", "migrate"]));
    q.write(
        "schema.ids.json",
        &(serde_json::to_string_pretty(&legacy()).unwrap() + "\n"),
    );
    q.write("pbps.yml", "dialect: postgres\nids_file: schema.ids.json\n");
    assert_ok(&q.pbps(&["ids", "migrate"]));
    assert!(!q.has("schema.ids.json"));
}

#[test]
fn migrate_refuses_a_tree_that_differs_and_leaves_everything_as_it_was() {
    let p = legacy_project();
    let other = {
        let q = legacy_project();
        assert_ok(&q.pbps(&["ids", "migrate"]));
        q.read(".pbps/ids/roles/app_reader.json")
            .replace("r_aaaaaa", "r_bbbbbb")
    };
    p.write(".pbps/ids/roles/app_reader.json", &other);
    let config = p.read("pbps.yml");
    assert_refused(&p.pbps(&["ids", "migrate"]), "differs");
    assert!(p.has("schema.ids.json"));
    assert_eq!(p.read("pbps.yml"), config);
    assert!(!p.has(".pbps/ids/dbo/customer.json"));
}

#[test]
fn migrate_removes_a_custom_ids_file_and_names_ids_dir() {
    let p = Project::new();
    p.write(
        "pbps.yml",
        "dialect: postgres\nids_file: db/identities.json\n",
    );
    p.write("schema/dbo/customer.yml", CUSTOMER);
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_aaaaaa".parse().unwrap(), "dbo.customer".parse().unwrap());
    for (u, n) in [
        ("c_aaaaa1", "customer_id"),
        ("c_aaaaa2", "full_name"),
        ("c_aaaaa4", "email"),
    ] {
        ids.columns.insert(
            u.parse().unwrap(),
            format!("dbo.customer.{n}").parse().unwrap(),
        );
    }
    p.write(
        "db/identities.json",
        &(serde_json::to_string_pretty(&ids).unwrap() + "\n"),
    );
    assert_ok(&p.pbps(&["ids", "migrate"]));
    assert!(!p.has("db/identities.json"));
    assert!(p.read("pbps.yml").contains("ids_dir: .pbps/ids"));
    assert_eq!(ids_tree::assemble(&tree_of(&p)).unwrap(), ids);
}

#[test]
fn migrate_refuses_an_identity_with_no_declaration_and_no_tombstone() {
    let p = legacy_project();
    std::fs::remove_file(p.0.join("schema/types/status.yml")).unwrap();
    assert_refused(&p.pbps(&["ids", "migrate"]), "dbo.status");
    assert!(p.has("schema.ids.json"));
    assert!(!p.has(".pbps/ids"));
}

#[test]
fn migrate_without_any_identity_file_says_there_is_nothing_to_migrate() {
    let p = Project::new();
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    assert_refused(&p.pbps(&["ids", "migrate"]), "nothing to migrate");
}

// -------------------------------------------------------------------- fmt ---

fn migrated() -> Project {
    migrated_in("")
}

fn migrated_in(sub: &str) -> Project {
    let p = legacy_project_in(sub);
    assert_ok(&p.pbps(&["ids", "migrate"]));
    p
}

#[test]
fn fmt_check_lists_a_hand_edited_file_and_fmt_restores_the_canonical_bytes() {
    let p = migrated();
    let canonical = p.read(".pbps/ids/dbo/customer.json");
    assert_ok(&p.pbps(&["ids", "fmt", "--check"]));
    let edited = canonical.replace("\n\n    \"c_aaaaa2\"", "\n    \"c_aaaaa2\"");
    assert_ne!(edited, canonical);
    p.write(".pbps/ids/dbo/customer.json", &edited);
    let check = p.pbps(&["ids", "fmt", "--check"]);
    assert_refused(&check, "customer.json");
    assert_eq!(
        p.read(".pbps/ids/dbo/customer.json"),
        edited,
        "--check must not write"
    );
    assert_ok(&p.pbps(&["ids", "fmt"]));
    assert_eq!(p.read(".pbps/ids/dbo/customer.json"), canonical);
    assert_ok(&p.pbps(&["ids", "fmt", "--check"]));
}

#[test]
fn fmt_refuses_a_malformed_file_and_rewrites_none() {
    let p = migrated();
    let canonical = p.read(".pbps/ids/dbo/customer.json");
    let edited = canonical.replace("\n\n    \"c_aaaaa2\"", "\n    \"c_aaaaa2\"");
    p.write(".pbps/ids/dbo/customer.json", &edited);
    p.write(".pbps/ids/types/status.json", "{ not json");
    assert_refused(&p.pbps(&["ids", "fmt"]), "status.json");
    assert_eq!(p.read(".pbps/ids/dbo/customer.json"), edited);
    assert_refused(&p.pbps(&["ids", "fmt", "--check"]), "status.json");
}

// ---------------------------------------------------------------- resolve ---

/// A project in git where two branches renamed the same column differently.
/// The merge is left in progress: the YAML and the ids file both conflict.
fn conflicted() -> Project {
    conflicted_in("")
}

fn conflicted_in(sub: &str) -> Project {
    let p = migrated_in(sub);
    if !sub.is_empty() {
        // Another project at the repository root keeps a file at the very same
        // relative path: a bare path in an index lookup would read it instead.
        let other = p
            .read(".pbps/ids/dbo/customer.json")
            .replace("t_aaaaaa", "t_bbbbbb");
        std::fs::create_dir_all(p.1.join(".pbps/ids/dbo")).unwrap();
        std::fs::write(p.1.join(".pbps/ids/dbo/customer.json"), other).unwrap();
    }
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "base"]);
    let rename = |branch: &str, to: &str| {
        p.git_ok(&["checkout", "-q", "-b", branch, "main"]);
        p.write(
            "schema/dbo/customer.yml",
            &CUSTOMER.replace("full_name", to),
        );
        let ids = p
            .read(".pbps/ids/dbo/customer.json")
            .replace("\"full_name\"", &format!("\"{to}\""));
        p.write(".pbps/ids/dbo/customer.json", &ids);
        p.git_ok(&["commit", "-q", "-am", branch]);
    };
    rename("ours", "name");
    rename("theirs", "display_name");
    p.git_ok(&["checkout", "-q", "ours"]);
    let merge = p.git(&["merge", "theirs"]);
    assert!(!merge.status.success(), "{}", text(&merge));
    let unmerged = text(&p.git(&["ls-files", "-u"]));
    assert!(
        unmerged.contains(".pbps/ids/dbo/customer.json"),
        "{unmerged}"
    );
    p
}

#[test]
fn resolve_refuses_while_a_declaration_still_has_conflict_markers() {
    let p = conflicted();
    assert!(p.read("schema/dbo/customer.yml").contains("<<<<<<<"));
    let before = p.read(".pbps/ids/dbo/customer.json");
    assert_refused(&p.pbps(&["ids", "resolve"]), "conflict markers");
    assert_eq!(p.read(".pbps/ids/dbo/customer.json"), before);
}

#[test]
fn resolve_takes_the_name_the_resolved_yaml_declares_and_stages_nothing() {
    let p = conflicted();
    p.write(
        "schema/dbo/customer.yml",
        &CUSTOMER.replace("full_name", "display_name"),
    );
    let out = p.pbps(&["ids", "resolve"]);
    assert_ok(&out);
    let ids = p.read(".pbps/ids/dbo/customer.json");
    assert!(
        !ids.contains("<<<<<<<") && ids.contains("\"c_aaaaa2\": \"display_name\""),
        "{ids}"
    );
    // Canonical bytes, and the conflict is still the user's to stage.
    assert_ok(&p.pbps(&["ids", "fmt", "--check"]));
    let unmerged = text(&p.git(&["ls-files", "-u"]));
    assert!(
        unmerged.contains(".pbps/ids/dbo/customer.json"),
        "still unstaged: {unmerged}"
    );
    p.git_ok(&["add", "-A"]);
    assert_ok(&p.pbps(&["ids", "resolve"]));
}

#[test]
fn resolve_refuses_a_name_the_yaml_does_not_decide_and_writes_nothing() {
    let p = conflicted();
    p.write(
        "schema/dbo/customer.yml",
        &CUSTOMER.replace("full_name", "title"),
    );
    let before = p.read(".pbps/ids/dbo/customer.json");
    let out = p.pbps(&["ids", "resolve"]);
    assert_refused(&out, "c_aaaaa2");
    let said = text(&out);
    assert!(
        said.contains("\"name\"") && said.contains("\"display_name\""),
        "{said}"
    );
    assert!(said.contains("pbps rename"), "{said}");
    assert_eq!(p.read(".pbps/ids/dbo/customer.json"), before);
}

#[test]
fn resolve_reports_what_the_validator_still_finds_after_the_merge() {
    let p = conflicted();
    // A declaration no identity file covers is `plan`'s to mint; resolve still
    // merges what it can and says so, rather than staying silent.
    p.write(
        "schema/dbo/customer.yml",
        &CUSTOMER.replace("full_name", "display_name"),
    );
    p.write(
        "schema/dbo/extra.yml",
        "table: dbo.extra\ncolumns:\n  id: {type: int}\n",
    );
    let out = p.pbps(&["ids", "resolve"]);
    assert_refused(&out, "ids.yaml-without-ids");
    assert!(
        p.read(".pbps/ids/dbo/customer.json")
            .contains("\"display_name\"")
    );
}

#[test]
fn resolve_with_nothing_unmerged_checks_the_tree_and_succeeds() {
    let p = migrated();
    p.git_ok(&["init", "-q", "-b", "main"]);
    let out = p.pbps(&["ids", "resolve"]);
    assert_ok(&out);
    assert!(text(&out).contains("no unmerged"), "{}", text(&out));
}

// ----------------------------------------------------- the tree is read ---

#[test]
fn plan_and_validate_read_a_migrated_project_instead_of_reading_no_identities() {
    let p = migrated();
    let out = p.pbps(&["validate"]);
    assert_ok(&out);
    // The uids the migration kept, not an empty mapping about to mint new ones.
    assert!(
        text(&out).contains("1 table uid(s), 3 column uid(s)"),
        "{}",
        text(&out)
    );
    assert_ok(&p.pbps(&["plan", "--check"]));
    // `doctor` must not call a migrated project a fresh one.
    let doctor = text(&p.pbps(&["doctor"]));
    assert!(!doctor.contains("mints one"), "{doctor}");
    // Nothing was minted in the meantime, and nothing came back as the old file.
    assert!(!p.has("schema.ids.json"));
}

#[test]
fn a_project_with_neither_file_still_validates_as_a_fresh_project() {
    let p = Project::new();
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    let out = p.pbps(&["validate"]);
    assert!(!text(&out).contains("ids migrate"), "{}", text(&out));
    // An empty identity directory is no tree either.
    std::fs::create_dir_all(p.0.join(".pbps/ids")).unwrap();
    let out = p.pbps(&["validate"]);
    assert!(!text(&out).contains("ids migrate"), "{}", text(&out));
}

#[test]
fn a_project_that_still_has_only_the_legacy_file_is_told_to_migrate() {
    let p = legacy_project();
    for args in [&["validate"][..], &["plan", "--check"][..], &["doctor"][..]] {
        let out = p.pbps(args);
        assert!(
            text(&out).contains("pbps ids migrate"),
            "{args:?}: {}",
            text(&out)
        );
    }
    // Nothing was written in either place.
    assert!(p.has("schema.ids.json"));
    assert!(!p.0.join(".pbps/ids").exists());
}

/// The legacy file is read in one place only: history. A revision made before
/// `pbps ids migrate` has `schema.ids.json`; the working tree has the tree.
#[test]
fn a_pre_migration_revision_is_read_from_its_legacy_file() {
    let p = legacy_project();
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "before the migration"]);
    assert_ok(&p.pbps(&["ids", "migrate"]));
    let out = p.pbps(&["validate", "--since", "HEAD"]);
    assert_ok(&out);
    assert!(!text(&out).contains("changed"), "{}", text(&out));
}

#[test]
fn a_migrated_revision_is_read_from_its_tree() {
    let p = migrated();
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "migrated"]);
    assert_ok(&p.pbps(&["validate", "--since", "HEAD"]));
    // The baseline's identities are the tree's, not none: against itself, a
    // plan changes nothing. Against an empty mapping every table would be new.
    let out = p.pbps(&[
        "plan", "--no-dev", "--since", "HEAD", "--check", "--format", "json",
    ]);
    assert_ok(&out);
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["data"]["changes"], 0, "{report}");
}

#[test]
fn a_revision_holding_both_layouts_is_refused_by_since() {
    let p = migrated();
    p.write(
        "schema.ids.json",
        &(serde_json::to_string_pretty(&legacy()).unwrap() + "\n"),
    );
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "both"]);
    std::fs::remove_file(p.0.join("schema.ids.json")).unwrap();
    let out = p.pbps(&["validate", "--since", "HEAD"]);
    assert!(text(&out).contains("both"), "{}", text(&out));
    assert!(text(&out).contains("HEAD"), "{}", text(&out));
}

// ------------------------------------------------- the write side of plan ---

#[test]
fn a_first_plan_writes_the_tree_names_ids_dir_and_a_second_plan_leaves_it_alone() {
    let p = Project::new();
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    assert_ok(&p.pbps(&["plan", "--no-dev"]));
    let file = p.read(".pbps/ids/dbo/customer.json");
    assert!(file.contains("\"table\": \"dbo.customer\""), "{file}");
    // An older pbps now stops at the config instead of minting fresh uids.
    assert!(p.read("pbps.yml").contains("ids_dir: .pbps/ids"));
    assert!(!p.has("schema.ids.json"));
    let before = tree_of(&p);
    assert_ok(&p.pbps(&["plan", "--no-dev"]));
    assert_eq!(tree_of(&p), before);
    assert_ok(&p.pbps(&["validate"]));
}

#[test]
fn a_new_column_in_the_middle_changes_only_its_own_file() {
    let p = migrated();
    let before = tree_of(&p);
    p.write(
        "schema/dbo/customer.yml",
        &CUSTOMER.replace(
            "  email: {type: text}\n",
            "  email: {type: text}\n  phone: {type: text}\n",
        ),
    );
    assert_ok(&p.pbps(&["plan", "--no-dev"]));
    let after = tree_of(&p);
    for (rel, text) in &before {
        if rel != "dbo/customer.json" {
            assert_eq!(&after[rel], text, "{rel} must not change");
        }
    }
    let file = &after["dbo/customer.json"];
    let at = |name: &str| file.find(&format!("\": \"{name}\"")).unwrap();
    assert!(
        at("email") < at("phone") && at("phone") < at("full_name"),
        "{file}"
    );
}

#[test]
fn moving_a_declaration_file_is_refused_by_name_and_writes_nothing() {
    let p = migrated();
    let before = tree_of(&p);
    std::fs::rename(
        p.0.join("schema/dbo/customer.yml"),
        p.0.join("schema/dbo/client.yml"),
    )
    .unwrap();
    // A new column makes the plan want to write the identities.
    let moved = p.read("schema/dbo/client.yml") + "  phone: {type: text}\n";
    p.write("schema/dbo/client.yml", &moved);
    let out = p.pbps(&["plan", "--no-dev"]);
    assert_refused(&out, "moving an identity file is not supported yet");
    assert!(text(&out).contains("#2600 PR 5b"), "{}", text(&out));
    assert_eq!(tree_of(&p), before, "a refusal writes nothing");
}

#[test]
fn validate_reports_an_identity_file_whose_declaration_is_gone_as_an_orphan() {
    let p = migrated();
    std::fs::remove_file(p.0.join("schema/dbo/customer.yml")).unwrap();
    let out = p.pbps(&["validate", "--format", "json"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("ids.orphan"), "{}", text(&out));
    // The same project, declaration restored, is clean.
    p.write("schema/dbo/customer.yml", CUSTOMER);
    assert_ok(&p.pbps(&["validate"]));
}

// ------------------------------------- history that is not a directory ---

/// A commit whose `.pbps/ids` is not a tree, made through git's index so the
/// working tree keeps its valid one.
fn revision_with_ids_object(kind: &str) -> Project {
    let p = migrated();
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "good"]);
    let sha = |out: Output| String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let head = sha(p.git(&["rev-parse", "HEAD"]));
    p.write("blob.txt", "../elsewhere\n");
    let blob = sha(p.git(&["hash-object", "-w", "blob.txt"]));
    p.git_ok(&["rm", "-r", "-q", "--cached", ".pbps/ids"]);
    let (mode, oid) = match kind {
        "symlink" => ("120000", blob),
        "file" => ("100644", blob),
        "gitlink" => ("160000", head),
        other => panic!("{other}"),
    };
    p.git_ok(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("{mode},{oid},.pbps/ids"),
    ]);
    p.git_ok(&["commit", "-q", "-m", "bad"]);
    p
}

#[test]
fn a_revision_whose_ids_dir_is_a_symlink_a_file_or_a_submodule_is_refused_never_empty() {
    for kind in ["symlink", "file", "gitlink"] {
        let p = revision_with_ids_object(kind);
        let out = p.pbps(&["validate", "--since", "HEAD"]);
        assert_refused(&out, ".pbps/ids");
        // Refused, not read as a baseline with no identities.
        assert!(
            text(&out).contains("is not a directory of identity files"),
            "{kind}: {}",
            text(&out)
        );
        // The revision before it is the same project and reads.
        assert_ok(&p.pbps(&["validate", "--since", "HEAD~1"]));
    }
}

// ------------------------------------- outputs inside the identity tree ---

#[test]
fn no_output_may_be_written_inside_the_identity_tree_existing_or_not() {
    let p = migrated();
    let before = tree_of(&p);
    #[cfg(unix)]
    std::os::unix::fs::symlink(".pbps/ids", p.0.join("alias")).unwrap();
    let mut targets = vec![".pbps/ids/x.json", ".pbps/ids/sub/y.json"];
    #[cfg(unix)]
    targets.push("alias/z.json");
    for target in targets {
        for args in [
            vec!["plan", "--no-dev", "--out", target],
            vec!["plan", "--no-dev", "--sql", target],
            vec!["docs", "--out", target],
            vec!["bootstrap", "--sql", target],
        ] {
            let out = p.pbps(&args);
            assert_refused(&out, "inside the identity tree");
            assert_eq!(tree_of(&p), before, "{args:?} wrote into the tree");
        }
    }
    // Beside the tree is fine.
    assert_ok(&p.pbps(&["plan", "--no-dev", "--out", "plan.json"]));
}

// ------------------------------------------- a project below the repo root ---

#[test]
fn resolve_reads_its_own_projects_stages_when_the_project_is_in_a_subdirectory() {
    let p = conflicted_in("apps/db");
    let neighbour = std::fs::read_to_string(p.1.join(".pbps/ids/dbo/customer.json")).unwrap();
    p.write(
        "schema/dbo/customer.yml",
        &CUSTOMER.replace("full_name", "display_name"),
    );
    assert_ok(&p.pbps(&["ids", "resolve"]));
    let ids = p.read(".pbps/ids/dbo/customer.json");
    assert!(ids.contains("\"c_aaaaa2\": \"display_name\""), "{ids}");
    // The project at the repository root was neither read nor touched.
    let after = std::fs::read_to_string(p.1.join(".pbps/ids/dbo/customer.json")).unwrap();
    assert_eq!(after, neighbour);
}

#[test]
fn since_sees_the_tree_of_a_revision_when_the_project_is_in_a_subdirectory() {
    let p = migrated_in("apps/db");
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "migrated"]);
    let out = p.pbps(&["validate", "--since", "HEAD"]);
    assert_ok(&out);
    // The tree it found is this project's own, with its uids: the revision's
    // mapping equals the working tree's, so nothing is reported as changed.
    assert!(!text(&out).contains("changed"), "{}", text(&out));
}

// ------------------------------------------------ migrate resumes mid-write ---

/// The files `migrate` writes for `legacy_project()`, taken from a run on a twin.
fn expected_tree() -> std::collections::BTreeMap<String, String> {
    let twin = migrated();
    tree_of(&twin)
}

#[test]
fn migrate_resumes_from_a_tree_missing_some_of_its_files() {
    let expected = expected_tree();
    assert!(expected.len() >= 4);
    // A crash inside the write loop leaves a prefix of the files, whichever.
    for kept in 1..expected.len() {
        let p = legacy_project();
        for (rel, text) in expected.iter().take(kept) {
            p.write(&format!(".pbps/ids/{rel}"), text);
        }
        assert_ok(&p.pbps(&["ids", "migrate"]));
        assert_eq!(tree_of(&p), expected, "after keeping {kept}");
        assert!(!p.has("schema.ids.json"));
        assert!(p.read("pbps.yml").contains("ids_dir: .pbps/ids"));
    }
}

#[test]
fn migrate_refuses_a_partial_tree_with_an_extra_or_a_differing_file_and_writes_nothing() {
    let expected = expected_tree();
    let (first, text) = expected.iter().next().unwrap();
    let stray = legacy_project();
    stray.write(&format!(".pbps/ids/{first}"), text);
    stray.write(".pbps/ids/stray.json", "{}");
    assert_refused(&stray.pbps(&["ids", "migrate"]), "stray.json");
    // A file migrate would write, with its identity changed (proved to differ,
    // since a replace that matches nothing reads like one that worked).
    let customer = &expected["dbo/customer.json"];
    let changed = customer.replace("t_aaaaaa", "t_cccccc");
    assert_ne!(&changed, customer);
    let differing = legacy_project();
    differing.write(".pbps/ids/dbo/customer.json", &changed);
    assert_refused(&differing.pbps(&["ids", "migrate"]), "differs");
    for p in [&stray, &differing] {
        assert!(p.has("schema.ids.json"));
        assert!(p.read("pbps.yml").contains("ids_file"));
        // Only what was put there: nothing else was written beside it.
        let files = tree_of(p);
        assert!(files.len() <= 2, "{:?}", files.keys().collect::<Vec<_>>());
    }
}
