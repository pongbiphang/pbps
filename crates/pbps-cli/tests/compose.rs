//! Composing reviewed intent end to end, against a real `git` and the real
//! `pbps` binary (ADR-0015 decision 5, #494).
//!
//! These are not unit tests of a protocol: they run the protocol. A fixture
//! standing in for `git` or for the intent command would be a second
//! implementation of the very thing under test, which is what AGENTS.md's
//! "measure against a real engine" rule exists to prevent.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use pbps_ui::compose::cli::{Cli, Intent};
use pbps_ui::compose::git::Git;
use pbps_ui::compose::repo_path::RepoPath;
use pbps_ui::compose::run::{Compose, Refusal, Request};

struct Checkout {
    root: PathBuf,
    private: PathBuf,
}

impl Checkout {
    /// A project at the worktree root — the ordinary layout — with its
    /// identities already recorded and committed.
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "pbps-compose-e2e-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let root = base.join("checkout");
        let private = base.join("private");
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let checkout = Self { root, private };
        checkout.write(
            "pbps.yml",
            "dialect: mssql\nenvironments:\n  unconfigured:\n    url_env: PBPS_COMPOSE_UNSET\n",
        );
        checkout.write(
            "schema/customer.yml",
            "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\n  \
             customer_name: {type: \"nvarchar(100)\", nullable: true}\nprimary_key: [id]\n",
        );
        // `plan` is what first records the identities — the same way a user
        // has them before ever asking for a rename, since a rename resolves
        // the edited declarations *against* the ids file and rewrites that
        // file alone.
        checkout.pbps(&["plan", "--no-input"]);
        checkout.git(&["init", "-q", "-b", "main", "."]);
        checkout.git(&["config", "user.email", "compose@example.invalid"]);
        checkout.git(&["config", "user.name", "Compose Test"]);
        checkout.git(&["config", "commit.gpgSign", "false"]);
        checkout.git(&["add", "-A"]);
        checkout.git(&["commit", "-q", "-m", "the schema as it stands"]);
        checkout
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.root.join(relative)).unwrap()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    fn pbps(&self, arguments: &[&str]) {
        let output = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&self.root)
            .args(["--project", "."])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "pbps {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn runner(&self) -> Git {
        Git::new(self.root.clone(), &self.private, Duration::from_secs(60)).unwrap()
    }

    fn cli(&self) -> Cli {
        Cli {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_pbps")),
            deadline: Duration::from_secs(60),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

fn project_file() -> RepoPath {
    RepoPath::new(b"pbps.yml").unwrap()
}

fn compose<'a>(checkout: &Checkout, git: &'a Git, cli: &'a Cli, file: &'a RepoPath) -> Compose<'a> {
    Compose {
        git,
        cli,
        project: None,
        project_file: file,
        git_dir: checkout.path(".git"),
        remote_name: "pbps-ui-test".to_owned(),
    }
}

fn rename_request() -> Request {
    Request {
        intent: Intent::Rename {
            from: "dbo.customer.customer_name".to_owned(),
            to: "full_name".to_owned(),
        },
        message: "rename dbo.customer.customer_name full_name".to_owned(),
        remote: String::new(),
        shown: Default::default(),
    }
}

#[test]
fn a_rename_is_recorded_as_one_commit_holding_the_declaration_and_the_ids_file() {
    let checkout = Checkout::new("rename");
    let tip = checkout.git(&["rev-parse", "HEAD"]);
    // The user has already edited the declaration, in an editor, in the
    // working tree. An intent command does not edit a declaration; it resolves
    // the edited ones against the ids file and rewrites the ids file alone.
    checkout.write(
        "schema/customer.yml",
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\n  \
         full_name: {type: \"nvarchar(100)\", nullable: true}\nprimary_key: [id]\n",
    );
    let git = checkout.runner();
    let cli = checkout.cli();
    let file = project_file();

    let composed = compose(&checkout, &git, &cli, &file)
        .run(&rename_request())
        .expect("the compose finishes");

    assert_eq!(composed.branch, "refs/heads/main");
    assert_eq!(checkout.git(&["rev-parse", "HEAD"]), composed.commit);
    assert_eq!(checkout.git(&["rev-parse", "HEAD^"]), tip);
    assert_eq!(
        checkout.git(&["log", "-1", "--format=%s"]),
        "rename dbo.customer.customer_name full_name"
    );

    // The commit holds both halves of the change, and nothing else.
    let mut committed = checkout.git(&["show", "--name-only", "--format=", "HEAD"]);
    committed = committed.trim().to_owned();
    let mut names: Vec<&str> = committed.lines().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["schema.ids.json", "schema/customer.yml"]);

    // And the working tree matches it: the whole point of preparing the user's
    // index under the lock rather than leaving them to reconcile it.
    assert_eq!(
        checkout.git(&["status", "--porcelain"]),
        "",
        "the checkout is clean after a compose"
    );
    assert!(checkout.read("schema.ids.json").contains("full_name"));
    assert!(composed.hooks_did_not_run);
}

#[test]
fn a_declaration_the_intent_does_not_match_refuses_and_changes_nothing() {
    // `--no-input` declines any question the command would have asked, so a
    // case it would ask about is refused and shown, never answered by the UI.
    let checkout = Checkout::new("unmatched");
    let tip = checkout.git(&["rev-parse", "HEAD"]);
    let before = checkout.read("schema/customer.yml");
    let git = checkout.runner();
    let cli = checkout.cli();
    let file = project_file();

    // The declaration still holds the old name, so the intent matches nothing.
    let refusal = compose(&checkout, &git, &cli, &file)
        .run(&rename_request())
        .expect_err("the intent command refuses");

    assert!(
        matches!(refusal, Refusal::Cli(_)),
        "expected the CLI's own refusal, got {refusal}"
    );
    assert_eq!(checkout.git(&["rev-parse", "HEAD"]), tip);
    assert_eq!(checkout.read("schema/customer.yml"), before);
    assert_eq!(checkout.git(&["status", "--porcelain"]), "");
}

#[test]
fn an_uncommitted_project_file_is_refused_before_anything_is_read() {
    // The working tree's configuration would answer the next question
    // differently, and the user commits the configuration first, as they would
    // before `pbps rename`.
    let checkout = Checkout::new("config");
    checkout.write(
        "pbps.yml",
        "dialect: mssql\nschema_dir: elsewhere\nenvironments:\n  unconfigured:\n    \
         url_env: PBPS_COMPOSE_UNSET\n",
    );
    let git = checkout.runner();
    let cli = checkout.cli();
    let file = project_file();

    let refusal = compose(&checkout, &git, &cli, &file)
        .run(&rename_request())
        .expect_err("an uncommitted pbps.yml is refused");

    assert!(
        matches!(refusal, Refusal::ConfigurationUncommitted { .. }),
        "got {refusal}"
    );
}

#[test]
fn a_chain_from_head_is_refused_before_a_file_is_placed() {
    // The case ADR-0015's Limits section names for step 4: start with
    // `HEAD -> a -> b` and assert refusal before placement.
    let checkout = Checkout::new("chain");
    checkout.write(
        "schema/customer.yml",
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\n  \
         full_name: {type: \"nvarchar(100)\", nullable: true}\nprimary_key: [id]\n",
    );
    let before = checkout.read("schema/customer.yml");
    checkout.git(&["symbolic-ref", "refs/heads/hop", "refs/heads/main"]);
    checkout.git(&["symbolic-ref", "HEAD", "refs/heads/hop"]);
    let git = checkout.runner();
    let cli = checkout.cli();
    let file = project_file();

    let refusal = compose(&checkout, &git, &cli, &file)
        .run(&rename_request())
        .expect_err("a chain is refused");

    assert!(matches!(refusal, Refusal::Ref(_)), "got {refusal}");
    assert_eq!(checkout.read("schema/customer.yml"), before);
    assert!(
        !checkout.path(".git/index.lock").exists(),
        "and no lock is left behind"
    );
    assert!(
        records_of(&checkout).is_empty(),
        "nor a record, the compose having undone itself"
    );
}

#[test]
fn a_hook_that_would_widen_or_rewrite_the_commit_never_runs() {
    let checkout = Checkout::new("hooks");
    checkout.write(
        "schema/customer.yml",
        "table: dbo.customer\ncolumns:\n  id: {type: int, nullable: false}\n  \
         full_name: {type: \"nvarchar(100)\", nullable: true}\nprimary_key: [id]\n",
    );
    let hooks = checkout.path(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for (name, body) in [
        (
            "pre-commit",
            "#!/bin/sh\necho widened > widened.txt\ngit add widened.txt\n",
        ),
        ("commit-msg", "#!/bin/sh\necho ' (hooked)' >> \"$1\"\n"),
        ("post-commit", "#!/bin/sh\necho ran > post-commit-ran.txt\n"),
        (
            "reference-transaction",
            "#!/bin/sh\necho ran > reference-transaction-ran.txt\n",
        ),
        (
            "post-index-change",
            "#!/bin/sh\necho ran > post-index-change-ran.txt\n",
        ),
    ] {
        let script = hooks.join(name);
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let git = checkout.runner();
    let cli = checkout.cli();
    let file = project_file();

    let composed = compose(&checkout, &git, &cli, &file)
        .run(&rename_request())
        .expect("the compose finishes");

    assert_eq!(
        checkout.git(&["log", "-1", "--format=%s", &composed.commit]),
        "rename dbo.customer.customer_name full_name",
        "the message is the one asked for"
    );
    for left in [
        "widened.txt",
        "post-commit-ran.txt",
        "reference-transaction-ran.txt",
        "post-index-change-ran.txt",
    ] {
        assert!(!checkout.path(left).exists(), "{left} says a hook ran");
    }
}

fn records_of(checkout: &Checkout) -> Vec<String> {
    let directory = checkout.path(".git/pbps-ui/composing");
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default()
}

/// Unused today, kept because every test above reaches for the same shape.
#[allow(dead_code)]
fn _assert_path(_: &Path) {}
