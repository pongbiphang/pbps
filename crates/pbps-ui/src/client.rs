//! A fixed command vocabulary; no shell, connection string or arbitrary argv.

use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Status,
    Drift(String),
    Plan(String),
    Timeline(String),
    Docs,
}

impl View {
    fn arguments(&self) -> Vec<String> {
        match self {
            Self::Status => vec!["status".into(), "--format=json".into()],
            // `--env`, never `--db`: a name from the browser cannot become a
            // connection string. Joining flag and value also keeps a leading
            // hyphen in a name from becoming another CLI option.
            Self::Drift(env) => vec![
                "verify".into(),
                format!("--env={env}"),
                "--format=json".into(),
            ],
            Self::Plan(path) => vec![
                "explain".into(),
                format!("--plan={path}"),
                "--format=json".into(),
            ],
            Self::Timeline(env) => vec![
                "state".into(),
                "list".into(),
                format!("--env={env}"),
                "--format=json".into(),
            ],
            Self::Docs => vec!["docs".into(), "--format=html".into()],
        }
    }

    fn command(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Drift(_) => "verify",
            Self::Plan(_) => "explain",
            Self::Timeline(_) => "state list",
            Self::Docs => "docs",
        }
    }
}

pub struct Client {
    pub executable: PathBuf,
    pub project: PathBuf,
}

impl Client {
    pub fn read(&self, view: &View) -> Result<Vec<u8>, String> {
        let output = Command::new(&self.executable)
            .current_dir(&self.project)
            .arg("--project")
            // The child is already inside the selected project. Repeating a
            // relative root can select a same-named nested project instead.
            .arg(".")
            .arg("--no-input")
            .args(view.arguments())
            .stdin(Stdio::null())
            // Credentials belong to the child environment (ADR-0015 §4).
            // Only its typed stdout is a browser contract; stderr is terminal
            // diagnostics and must not become a second, untyped data channel.
            .stderr(Stdio::null())
            .output()
            .map_err(|_| "Could not start the pbps read command".to_owned())?;
        if matches!(view, View::Docs) {
            if !output.status.success() || std::str::from_utf8(&output.stdout).is_err() {
                return Err(
                    "Could not render the schema. Run pbps docs --format html for diagnostics"
                        .into(),
                );
            }
            Ok(output.stdout)
        } else {
            crate::contract::parse(
                view.command(),
                &output.stdout,
                output.status.code().unwrap_or(-1),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_values_remain_one_named_option_and_never_select_a_command() {
        let hostile = "--db=secret; apply $(anything)";
        assert_eq!(
            View::Drift(hostile.into()).arguments(),
            ["verify", &format!("--env={hostile}"), "--format=json"]
        );
        assert_eq!(
            View::Plan(hostile.into()).arguments(),
            ["explain", &format!("--plan={hostile}"), "--format=json"]
        );
        assert_eq!(
            View::Timeline("dev".into()).arguments(),
            ["state", "list", "--env=dev", "--format=json"]
        );
        assert_eq!(View::Docs.arguments(), ["docs", "--format=html"]);
    }

    #[test]
    fn a_missing_executable_is_not_an_empty_report() {
        let client = Client {
            executable: PathBuf::from("/no/such/pbps-executable"),
            project: std::env::temp_dir(),
        };
        assert!(client.read(&View::Status).is_err());
        assert!(client.read(&View::Docs).is_err());
    }
}
