//! `pbps ids migrate | fmt | resolve` against real projects and real git (#2600).

use pbps_model::ids_tree;
use pbps_model::{IdsFile, Tombstone};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pbps-ids-cmd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
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
            .current_dir(&self.0)
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
        let _ = std::fs::remove_dir_all(&self.0);
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
    let p = Project::new();
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
    let p = legacy_project();
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
    let p = migrated();
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

// -------------------------------------------- fail closed until the switch ---

const NOT_READ: &str = "does not read it yet";

#[test]
fn plan_and_validate_refuse_a_migrated_project_instead_of_reading_no_identities() {
    let p = migrated();
    for args in [&["validate"][..], &["plan", "--check"][..], &["doctor"][..]] {
        let out = p.pbps(args);
        assert!(text(&out).contains(NOT_READ), "{args:?}: {}", text(&out));
    }
    for args in [&["validate"][..], &["plan", "--check"][..]] {
        assert_refused(&p.pbps(args), "identity tree `.pbps/ids`");
    }
    // `doctor` must not call a migrated project a fresh one.
    assert!(!text(&p.pbps(&["doctor"])).contains("mints one"));
    // Nothing was minted in the meantime.
    assert!(!p.has("schema.ids.json"));
}

#[test]
fn a_project_with_neither_file_still_validates_as_a_fresh_project() {
    let p = Project::new();
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    let out = p.pbps(&["validate"]);
    assert!(!text(&out).contains(NOT_READ), "{}", text(&out));
    // An empty identity directory is no tree either.
    std::fs::create_dir_all(p.0.join(".pbps/ids")).unwrap();
    let out = p.pbps(&["validate"]);
    assert!(!text(&out).contains(NOT_READ), "{}", text(&out));
}

#[test]
fn a_revision_holding_the_tree_and_no_legacy_blob_is_refused_by_since() {
    let p = migrated();
    p.git_ok(&["init", "-q", "-b", "main"]);
    p.git_ok(&["add", "-A"]);
    p.git_ok(&["commit", "-q", "-m", "migrated"]);
    // The working tree goes back to the legacy layout; only the revision is migrated.
    std::fs::remove_dir_all(p.0.join(".pbps")).unwrap();
    p.write("pbps.yml", "dialect: postgres\n");
    p.write(
        "schema.ids.json",
        &(serde_json::to_string_pretty(&legacy()).unwrap() + "\n"),
    );
    let out = p.pbps(&["validate", "--since", "HEAD"]);
    assert!(text(&out).contains(NOT_READ), "{}", text(&out));
    assert!(
        text(&out).contains("at `HEAD`") || text(&out).contains("HEAD"),
        "{}",
        text(&out)
    );
}
