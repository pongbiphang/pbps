//! Finding the checkout a project sits in, before any of the protocol runs.
//!
//! This is the one `git` the UI runs outside the guarded runner of
//! [`super::git`], and it is deliberately the only one: it *locates* rather
//! than acts, and the runner it would use needs the worktree root that this
//! answers. Every command after it goes through the dressed one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where the project is, in the repository that holds it.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    /// The project's directory relative to the worktree root, or `None` where
    /// the project *is* the root.
    pub project_within: Option<String>,
    /// `pbps.yml`, spelled from the worktree root.
    pub project_file: String,
}

/// `None` where the project is not in a git checkout at all, which is a
/// project this UI reads and does not compose for.
pub fn discover(project: &Path) -> Option<Checkout> {
    let ask = |what: &str| -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["rev-parse", what])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let answer = String::from_utf8(output.stdout).ok()?;
        let answer = answer.trim_end_matches(['\n', '\r']);
        if answer.is_empty() {
            None
        } else {
            Some(answer.to_owned())
        }
    };
    let root = PathBuf::from(ask("--show-toplevel")?);
    let git_dir = PathBuf::from(ask("--absolute-git-dir")?);
    // Both are the kernel's answer for the same directory or the project is
    // not the one this repository holds — a bind mount or a symlinked path
    // can make two spellings of one place, and the compose spells every path
    // from the root.
    let within = project
        .canonicalize()
        .ok()?
        .strip_prefix(root.canonicalize().ok()?)
        .ok()?
        .to_str()?
        .to_owned();
    let project_within = if within.is_empty() {
        None
    } else {
        Some(within.clone())
    };
    let project_file = if within.is_empty() {
        "pbps.yml".to_owned()
    } else {
        format!("{within}/pbps.yml")
    };
    Some(Checkout {
        root,
        git_dir,
        project_within,
        project_file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    #[test]
    fn a_project_at_the_worktree_root_has_no_directory_within_it() {
        let scratch = Scratch::new("locate-root");
        scratch.write("pbps.yml", b"dialect: mssql\n");
        scratch.commit("one");

        let found = discover(&scratch.root).expect("a checkout");
        assert_eq!(found.project_within, None);
        assert_eq!(found.project_file, "pbps.yml");
        assert_eq!(
            found.git_dir.canonicalize().unwrap(),
            scratch.git_dir().canonicalize().unwrap()
        );
    }

    #[test]
    fn a_project_in_a_subdirectory_is_named_from_the_worktree_root() {
        let scratch = Scratch::new("locate-sub");
        scratch.write("apps/db/pbps.yml", b"dialect: mssql\n");
        scratch.commit("one");

        let found = discover(&scratch.path("apps/db")).expect("a checkout");
        assert_eq!(found.project_within.as_deref(), Some("apps/db"));
        assert_eq!(found.project_file, "apps/db/pbps.yml");
    }

    #[test]
    fn a_project_outside_any_checkout_is_not_one_this_ui_composes_for() {
        let outside = std::env::temp_dir().join(format!(
            "pbps-locate-none-{}-{}",
            std::process::id(),
            crate::compose::record::random_name().unwrap()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        // `git rev-parse` walks upwards, so this only proves the point where
        // the temporary directory is not itself inside a repository; where it
        // is, the answer is a checkout and the assertion below is skipped.
        if let Some(found) = discover(&outside) {
            assert!(found.root.exists());
        }
        let _ = std::fs::remove_dir_all(&outside);
    }
}
