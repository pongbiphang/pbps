//! A disposable repository for the tests of this module.
//!
//! The compose is a protocol over `git`, so its tests are run against `git`
//! (AGENTS.md — measure against a real engine before believing yourself). A
//! fixture that stood in for one would be a second implementation of the very
//! thing under test.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::git::Git;
use super::record::random_name;

pub struct Scratch {
    pub root: PathBuf,
    pub private: PathBuf,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "pbps-compose-{name}-{}-{}",
            std::process::id(),
            random_name().expect("randomness")
        ));
        let root = base.join("checkout");
        let private = base.join("private");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let scratch = Self { root, private };
        scratch.git(&["init", "-q", "-b", "main", "."]);
        scratch.git(&["config", "user.email", "compose@example.invalid"]);
        scratch.git(&["config", "user.name", "Compose Test"]);
        scratch.git(&["config", "commit.gpgSign", "false"]);
        scratch
    }

    /// Plain `git`, outside the UI's own guarded runner: the fixtures a test
    /// sets up are the user's side of the story, and dressing them the way the
    /// UI dresses its own commands would hide a difference rather than show
    /// one.
    pub fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(arguments)
            .output()
            .expect("git runs in the tests");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    /// The same, for a command the test *expects* to fail.
    pub fn git_try(&self, arguments: &[&str]) -> (bool, String) {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(arguments)
            .output()
            .expect("git runs in the tests");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_owned(),
        )
    }

    pub fn write(&self, relative: &str, contents: &[u8]) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn runner(&self) -> Git {
        Git::new(self.root.clone(), &self.private, Duration::from_secs(30))
            .expect("the guarded runner")
    }

    pub fn git_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

pub fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
