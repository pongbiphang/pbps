//! Fixed ordinary intent/validation commands. Schema semantics remain in CLI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{Error, Result, process};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Intent {
    Rename {
        from: String,
        to: String,
    },
    RenameTable {
        from: String,
        to: String,
    },
    Drop {
        column: String,
        reason: String,
    },
    DropTable {
        table: String,
        reason: String,
    },
    RenameRole {
        from: String,
        to: String,
    },
    DropRole {
        role: String,
        reason: String,
    },
    /// Annotations already edited in declarations; the CLI resolves identity.
    Declarations,
}

impl Intent {
    pub(crate) fn arguments(&self, base: &str) -> Vec<String> {
        match self {
            Self::Rename { from, to } => {
                vec!["rename".into(), "--".into(), from.clone(), to.clone()]
            }
            Self::RenameTable { from, to } => {
                vec!["rename-table".into(), "--".into(), from.clone(), to.clone()]
            }
            Self::RenameRole { from, to } => {
                vec!["rename-role".into(), "--".into(), from.clone(), to.clone()]
            }
            Self::Drop { column, reason } => vec![
                "drop".into(),
                format!("--reason={reason}"),
                "--".into(),
                column.clone(),
            ],
            Self::DropTable { table, reason } => vec![
                "drop-table".into(),
                format!("--reason={reason}"),
                "--".into(),
                table.clone(),
            ],
            Self::DropRole { role, reason } => vec![
                "drop-role".into(),
                format!("--reason={reason}"),
                "--".into(),
                role.clone(),
            ],
            Self::Declarations => vec![
                "plan".into(),
                "--no-dev".into(),
                format!("--since={base}"),
                "--format=json".into(),
            ],
        }
    }
}

#[derive(Deserialize)]
pub(super) struct Paths {
    pub project_file: String,
    pub declarations: String,
    pub identity_file: String,
}

pub(super) struct Cli {
    pub executable: PathBuf,
    pub deadline: Duration,
}

impl Cli {
    fn run<S: AsRef<std::ffi::OsStr>>(&self, project: &Path, arguments: &[S]) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.executable);
        // All commands here are offline. An empty environment also prevents
        // repository/config overrides or an environment URL from escaping the
        // private snapshot. Git's executable search uses its default path.
        command
            .env_clear()
            .current_dir(project)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["--project", ".", "--no-input"])
            .args(arguments);
        let result = process::run(command, &[], self.deadline)?;
        if !result.status.success() {
            // A signal, timeout or failed pipe exchange does not prove that
            // private work completed. A normal refusal still needs the
            // pre-command inventory before it can authorize retirement.
            if !matches!(result.status.code(), Some(1 | 2)) {
                return Err(Error::new(
                    "The captured-input CLI did not return an ordinary refusal",
                ));
            }
            return Err(Error::rejected(
                "The CLI refused the captured inputs or intent; inspect them with the ordinary CLI",
            ));
        }
        Ok(result.stdout)
    }

    pub fn paths(&self, project: &Path) -> Result<Paths> {
        #[derive(Deserialize)]
        struct Envelope {
            data: Option<Paths>,
        }
        let bytes = self.run(project, &["doctor", "--paths-only", "--format=json"])?;
        serde_json::from_slice::<Envelope>(&bytes)
            .ok()
            .and_then(|e| e.data)
            .ok_or_else(|| Error::new("The CLI did not return its resolved input paths"))
    }

    pub fn record(&self, project: &Path, intent: &Intent, base: &str) -> Result<()> {
        self.run(project, &intent.arguments(base))?;
        Ok(())
    }

    pub fn validate(&self, project: &Path, base: &str) -> Result<()> {
        self.run(project, &["validate", "--format=json"])?;
        // The ordinary plan check reconciles the candidate identity and its
        // recorded baseline. A second UI-side ids parser would diverge.
        self.run(
            project,
            &["plan", "--check", "--format=json", "--since", base],
        )?;
        Ok(())
    }
}
