//! Everything the compose needs that the UI worked out at launch, and the two
//! routes that use it.
//!
//! Kept beside the server rather than inside `compose`, because the compose is
//! a protocol over `git` and this is a web page's view of it: what the page
//! may ask for, what it is told, and what it is never told.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compose::cli::Cli;
use crate::compose::git::Git;
use crate::compose::locate::{self, Checkout};
use crate::compose::recover;
use crate::compose::repo_path::RepoPath;
use crate::compose::run::{Compose, Request};

/// A subprocess still running when this expires is killed with its process
/// group, and the page is shown the command to run by hand — SPEC §6.4's rule
/// for the CLI, applied unchanged. It bounds the helpers the environment
/// cannot reach, such as a pinentry the user's `gpg-agent` chooses.
const DEADLINE: Duration = Duration::from_secs(120);

pub struct Site {
    checkout: Checkout,
    executable: PathBuf,
    project_within: Option<RepoPath>,
    project_file: RepoPath,
    private: PathBuf,
    /// `pbps-ui-<random>`, drawn once per launch: the remote name that exists
    /// only in the environment of the two processes that use it.
    remote_name: String,
    /// What recovery did with any record a crash left behind, read once at
    /// launch, before the UI offers to compose.
    recovered: Vec<recover::Recovered>,
}

impl Site {
    pub fn at(executable: &Path, project: &Path, token: [u8; 32]) -> Option<Self> {
        let checkout = locate::discover(project)?;
        let project_within = match &checkout.project_within {
            None => None,
            Some(within) => Some(RepoPath::new(within.as_bytes()).ok()?),
        };
        let project_file = RepoPath::new(checkout.project_file.as_bytes()).ok()?;
        let private = checkout.git_dir.join("pbps-ui").join("private");
        std::fs::create_dir_all(&private).ok()?;
        // Drawn from the launch token's own randomness rather than asking the
        // kernel again: the UI already holds 32 bytes nobody else has, and the
        // remote name only has to be one no configuration file already holds.
        let remote_name = format!(
            "pbps-ui-{}",
            token[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let mut site = Self {
            checkout,
            executable: executable.to_path_buf(),
            project_within,
            project_file,
            private,
            remote_name,
            recovered: Vec::new(),
        };
        // At launch, and before it offers to compose.
        if let Ok(git) = site.git() {
            site.recovered = recover::at_launch(&git, &site.checkout.git_dir);
        }
        Some(site)
    }

    fn git(&self) -> std::io::Result<Git> {
        Git::new(self.checkout.root.clone(), &self.private, DEADLINE)
    }

    fn cli(&self) -> Cli {
        Cli {
            executable: self.executable.clone(),
            deadline: DEADLINE,
        }
    }

    /// What the page needs to draw the form, and nothing a browser has no use
    /// for: no URL, no path outside the repository, no environment.
    pub fn context(&self) -> serde_json::Value {
        let Ok(git) = self.git() else {
            return Self::unavailable();
        };
        let lease = crate::compose::refs::lease(&git);
        serde_json::json!({
            "available": lease.is_ok(),
            "branch": lease.as_ref().ok().map(|l| l.reference.clone()),
            "tip": lease.as_ref().ok().map(|l| l.tip.clone()),
            "why": lease.as_ref().err().map(std::string::ToString::to_string),
            "remotes": remotes(&git),
            "recovered": self.recovered,
            "hooks_will_not_run": true,
        })
    }

    pub fn unavailable() -> serde_json::Value {
        serde_json::json!({
            "available": false,
            "why": "This project is not inside a git checkout, so there is nothing to record \
                    the intent in.",
            "remotes": Vec::<String>::new(),
            "recovered": Vec::<u8>::new(),
        })
    }

    pub fn answer(&self, url: &str, request: &Request) -> (u16, &'static str, Vec<u8>) {
        let Ok(git) = self.git() else {
            return refused(409, "git could not be prepared for this checkout");
        };
        let cli = self.cli();
        let compose = Compose {
            git: &git,
            cli: &cli,
            project: self.project_within.as_ref(),
            project_file: &self.project_file,
            git_dir: self.checkout.git_dir.clone(),
            remote_name: self.remote_name.clone(),
            watching: None,
        };
        match url {
            "/api/compose/preview" => match compose.preview(request) {
                Ok(preview) => ok(&preview),
                Err(refusal) => refused(409, &refusal.to_string()),
            },
            "/api/compose/record" => match compose.run(request) {
                Ok(composed) => ok(&composed),
                Err(refusal) => refused(409, &refusal.to_string()),
            },
            _ => refused(404, "No such compose route"),
        }
    }
}

/// The remotes by name alone. A URL may carry a credential, and the page is
/// shown one only after the compose has rebuilt it from the parts that name a
/// destination.
fn remotes(git: &Git) -> Vec<String> {
    git.run(&["remote"])
        .ok()
        .filter(crate::compose::git::Run::ok)
        .map(|answer| {
            String::from_utf8_lossy(&answer.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn ok(value: &impl serde::Serialize) -> (u16, &'static str, Vec<u8>) {
    match serde_json::to_vec(&serde_json::json!({ "ok": true, "data": value })) {
        Ok(bytes) => (200, "application/json", bytes),
        Err(_) => refused(500, "The answer could not be rendered"),
    }
}

pub fn refused(code: u16, why: &str) -> (u16, &'static str, Vec<u8>) {
    let body = serde_json::to_vec(&serde_json::json!({ "ok": false, "refusal": why }))
        .unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    (code, "application/json", body)
}
