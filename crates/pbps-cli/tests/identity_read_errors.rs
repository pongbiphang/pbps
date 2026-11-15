//! An ordinary `plan` whose Git baseline holds identity history Git cannot
//! read refuses, with the repair; it never reads that history as empty
//! (#1814). Absent history stays a first run. Everything here is offline: the
//! damage is made in a disposable repository.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Project(PathBuf);

impl Project {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pbps-idread-{name}-{}-{}",
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
    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.0.join(rel)).unwrap()
    }
    fn pbps(&self, args: &[&str]) -> Output {
        self.pbps_with_path(args, None)
    }
    /// With `bin` first on `PATH`, so a stand-in `git` there is the one pbps runs.
    fn pbps_with_path(&self, args: &[&str], bin: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pbps"));
        command.args(args).current_dir(&self.0);
        if let Some(bin) = bin {
            let old = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin.to_path_buf()];
            paths.extend(std::env::split_paths(&old));
            command.env("PATH", std::env::join_paths(paths).unwrap());
        }
        command.output().unwrap()
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
    fn commit_everything(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "all"]);
    }
    /// Deletes the loose object `HEAD:<path>` names: the repository the issue
    /// describes, where `git show HEAD:<path>` exits 128 with `bad object`.
    fn lose_the_blob_of(&self, path: &str) {
        let oid = self.git(&["rev-parse", &format!("HEAD:{path}")]);
        let object = self.0.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        assert!(object.is_file(), "{path} is not a loose object");
        std::fs::remove_file(object).unwrap();
    }
    /// A directory holding a `git` that runs the real one except where
    /// `refuse` (a shell `case` pattern over the whole argument line) matches,
    /// where it exits 128 as a git that cannot do its job would.
    fn git_that_fails_on(&self, refuse: &str) -> PathBuf {
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("git"))
            .find(|git| git.is_file())
            .expect("git is installed");
        let bin = self.0.join(".stand-in-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \" $* \" in\n  {refuse}) echo 'fatal: injected failure' >&2; exit 128;;\nesac\nexec '{}' \"$@\"\n",
                real.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Parallel tests fork while the script is being written; until every
        // such child has exec'd, running it fails with ETXTBSY. Wait it out
        // here, where a failure is visible, rather than inside pbps.
        for _ in 0..200 {
            match Command::new(&script).arg("--version").output() {
                Ok(out) if out.status.success() => return bin,
                _ => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        panic!("the stand-in git never became runnable");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const CUSTOMER: &str = "table: dbo.customer\ncolumns:\n  customer_id: {type: int, nullable: false}\n  email: {type: text}\n";
const IDS_FILE: &str = ".pbps/ids/dbo/customer.json";

/// A committed project whose identities are in the tree, as a first `plan`
/// writes them.
fn committed_with_the_identity_tree() -> Project {
    let p = Project::new("tree");
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    assert!(p.pbps(&["plan", "--no-dev"]).status.success());
    p.commit_everything();
    p
}

/// A committed project whose identities are still the legacy one file, with
/// the working tree migrated so only history holds the legacy blob.
fn committed_with_the_legacy_identity_file() -> Project {
    let p = Project::new("legacy");
    p.write("pbps.yml", "dialect: postgres\nids_file: schema.ids.json\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    p.write(
        "schema.ids.json",
        r#"{"version": 1, "tables": {"t_aaaaaa": "dbo.customer"}, "columns": {"c_aaaaa1": "dbo.customer.customer_id", "c_aaaaa2": "dbo.customer.email"}}
"#,
    );
    p.commit_everything();
    let migrated = p.pbps(&["ids", "migrate"]);
    assert!(migrated.status.success(), "{}", text(&migrated));
    p
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn report(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)))
}

/// `plan` against `HEAD` with an existing output file, which must survive a
/// refusal.
fn plan_with_output(p: &Project, bin: Option<&Path>, since: &str) -> (Output, Vec<u8>) {
    p.write("plan.json", "existing reviewed plan\n");
    let out = p.pbps_with_path(
        &[
            "plan",
            "--no-dev",
            "--since",
            since,
            "--out",
            "plan.json",
            "--format",
            "json",
        ],
        bin,
    );
    (out, p.read("plan.json"))
}

/// The refusal: exit 1, the unreadable-baseline finding, the revision, the
/// repair, and no output written.
fn assert_refused_with_the_repair(p: &Project, out: &Output, written: &[u8], what: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", text(out));
    let report = report(out);
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "baseline.unreadable")
        .unwrap_or_else(|| panic!("no baseline.unreadable finding: {report}"));
    let message = finding["message"].as_str().unwrap();
    for needle in [
        what,
        "at `HEAD`",
        "git fsck",
        "git fetch",
        "Do not regenerate",
    ] {
        assert!(message.contains(needle), "{needle:?} not in: {message}");
    }
    assert_eq!(
        written, b"existing reviewed plan\n",
        "the output was written"
    );
    assert!(p.0.join(".git").is_dir(), "the fixture lost its repository");
}

#[test]
fn a_plan_whose_baseline_identity_blob_is_unreadable_refuses_with_the_repair() {
    let p = committed_with_the_identity_tree();
    // Before the damage the same plan is a clean no-op: the refusal below is
    // the damage's, not the project's.
    let (clean, _) = plan_with_output(&p, None, "HEAD");
    assert_eq!(clean.status.code(), Some(0), "{}", text(&clean));
    assert_eq!(report(&clean)["data"]["changes"], 0, "{}", text(&clean));
    let ids_before = p.read(IDS_FILE);

    p.lose_the_blob_of(IDS_FILE);
    let (out, written) = plan_with_output(&p, None, "HEAD");
    assert_refused_with_the_repair(&p, &out, &written, "cannot read the identity tree");
    assert_eq!(
        p.read(IDS_FILE),
        ids_before,
        "the identity file was written"
    );
}

#[test]
fn a_plan_whose_legacy_identity_blob_is_unreadable_refuses_with_the_repair() {
    let p = committed_with_the_legacy_identity_file();
    p.lose_the_blob_of("schema.ids.json");
    let (out, written) = plan_with_output(&p, None, "HEAD");
    assert_refused_with_the_repair(&p, &out, &written, "cannot read the identity file");
}

#[test]
fn a_git_process_that_fails_while_reading_the_identity_tree_refuses_with_the_repair() {
    let p = committed_with_the_identity_tree();
    // Nothing is damaged: only the listing of the identity directory fails.
    let bin = p.git_that_fails_on("*\" ls-tree \"*\" -- .pbps/ids \"*");
    let (out, written) = plan_with_output(&p, Some(&bin), "HEAD");
    assert_refused_with_the_repair(&p, &out, &written, "cannot read the identity tree");
    assert!(text(&out).contains("injected failure"), "{}", text(&out));
    // The same repository without the failing git plans normally.
    let (ok, _) = plan_with_output(&p, None, "HEAD");
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
}

#[test]
fn a_git_process_that_fails_while_reading_the_legacy_identity_file_refuses_with_the_repair() {
    let p = committed_with_the_legacy_identity_file();
    let bin = p.git_that_fails_on("*\" show \"*:schema.ids.json\" \"*");
    let (out, written) = plan_with_output(&p, Some(&bin), "HEAD");
    assert_refused_with_the_repair(&p, &out, &written, "cannot read the identity file");
    assert!(text(&out).contains("injected failure"), "{}", text(&out));
}

#[test]
fn a_revision_the_caller_typed_is_shown_escaped_in_the_refusal() {
    let p = committed_with_the_identity_tree();
    let odd = "side\u{202e}x";
    p.git(&["branch", odd]);
    p.lose_the_blob_of(IDS_FILE);
    let (out, _) = plan_with_output(&p, None, odd);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let message = report(&out)["findings"][0]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("side\\u{202e}x"), "{message}");
    assert!(!message.contains('\u{202e}'), "{message}");
}

#[test]
fn history_with_no_identities_at_all_is_still_a_first_run_and_plans_normally() {
    // Negative: absent is not unreadable. The declarations were committed
    // before any `plan` wrote identities, so the baseline has none.
    let p = Project::new("absent");
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    p.commit_everything();
    let (out, written) = plan_with_output(&p, None, "HEAD");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let report = report(&out);
    assert!(
        report["findings"]
            .as_array()
            .is_none_or(|f| f.iter().all(|f| f["id"] != "baseline.unreadable")),
        "{report}"
    );
    assert_ne!(
        written, b"existing reviewed plan\n",
        "the plan was not written"
    );
}

#[test]
fn an_unborn_head_is_still_the_empty_baseline_and_plans_normally() {
    // Negative: a repository with no commit has no history to be unreadable.
    let p = Project::new("unborn");
    p.write("pbps.yml", "dialect: postgres\n");
    p.write("schema/dbo/customer.yml", CUSTOMER);
    p.git(&["init", "-q", "-b", "main"]);
    let (out, _) = plan_with_output(&p, None, "HEAD");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[test]
fn a_failure_outside_the_identity_history_is_not_given_the_identity_repair() {
    // Negative: the remedy belongs to identity reads only. A damaged
    // declaration blob is refused in its own words.
    let p = committed_with_the_identity_tree();
    p.lose_the_blob_of("schema/dbo/customer.yml");
    let (out, _) = plan_with_output(&p, None, "HEAD");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(!text(&out).contains("Do not regenerate"), "{}", text(&out));
}
