//! The three `pbps` commands a compose runs, and the vocabulary it will
//! accept from the page.
//!
//! The page sends the *intent*, never the files: the kind of change and its
//! arguments, exactly as SPEC §6.4 prints them for the shell. The browser
//! never composes a declaration or an ids-file line, because the identity
//! mapping a rename records comes from `pbps_diff::resolve`, which the intent
//! command runs and the UI cannot — and a UI that chose the uid itself could
//! write an ids file that is internally consistent and records a different
//! identity transition than the one asked for, which `validate` accepts and a
//! later `plan` reads as a drop and a create.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

/// The six intent commands, and nothing else.
///
/// A closed vocabulary rather than a command name and arguments from the
/// browser: SPEC §14.3's rule is that the arguments are the user's decision
/// and the tool's shape is not, and a page that could name the command would
/// be a shell.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Intent {
    Rename { from: String, to: String },
    RenameTable { from: String, to: String },
    Drop { column: String, reason: String },
    DropTable { table: String, reason: String },
    RenameRole { from: String, to: String },
    DropRole { role: String, reason: String },
}

impl Intent {
    /// No `--format`, which the intent commands do not take (**measured**:
    /// `rename --format json` exited 2 with `unexpected argument '--format'`).
    /// Decision 1's list of what speaks the envelope is exact, and for these
    /// commands the page gets the exit status and the blockers report of
    /// `stderr`.
    ///
    /// `--no-input` declines any question the command would have asked, so a
    /// case it would ask about is refused and shown, never answered by the UI.
    pub fn arguments(&self) -> Vec<String> {
        match self {
            Self::Rename { from, to } => {
                vec!["rename".into(), from.clone(), to.clone()]
            }
            Self::RenameTable { from, to } => {
                vec!["rename-table".into(), from.clone(), to.clone()]
            }
            Self::Drop { column, reason } => {
                vec!["drop".into(), column.clone(), format!("--reason={reason}")]
            }
            Self::DropTable { table, reason } => vec![
                "drop-table".into(),
                table.clone(),
                format!("--reason={reason}"),
            ],
            Self::RenameRole { from, to } => {
                vec!["rename-role".into(), from.clone(), to.clone()]
            }
            Self::DropRole { role, reason } => vec![
                "drop-role".into(),
                role.clone(),
                format!("--reason={reason}"),
            ],
        }
    }

    /// The message the page is prefilled with, spelled the way the CLI's own
    /// error output spells the intent.
    pub fn message(&self) -> String {
        match self {
            Self::Rename { from, to } => format!("rename {from} {to}"),
            Self::RenameTable { from, to } => format!("rename table {from} {to}"),
            Self::Drop { column, .. } => format!("drop {column}"),
            Self::DropTable { table, .. } => format!("drop table {table}"),
            Self::RenameRole { from, to } => format!("rename role {from} {to}"),
            Self::DropRole { role, .. } => format!("drop role {role}"),
        }
    }
}

/// Where the CLI resolved the project's inputs.
///
/// Only the two fields the compose needs, and deliberately not the whole
/// `doctor` payload: a field added there must not stop a compose that reads
/// two paths. What *would* break it is one of these two being renamed or
/// removed, and that is pinned by an integration test running the real
/// binary, which is the only place the two can be compared.
#[derive(Debug, Deserialize)]
pub struct Where {
    pub declarations: String,
    pub identity_file: String,
}

#[derive(Debug, Deserialize)]
struct DoctorEnvelope {
    data: Option<Where>,
}

#[derive(Debug)]
pub enum CliRefusal {
    /// The command could not be run at all.
    Unstartable(String),
    /// It answered, and said no. `stderr` is the blockers report.
    Refused { command: String, said: String },
    /// It answered in a shape this protocol cannot read.
    Unreadable { command: String, detail: String },
}

impl std::fmt::Display for CliRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unstartable(detail) => write!(f, "could not run pbps: {detail}"),
            Self::Refused { command, said } => write!(f, "`pbps {command}` refused: {said}"),
            Self::Unreadable { command, detail } => {
                write!(f, "`pbps {command}` answered unusably: {detail}")
            }
        }
    }
}

/// The compose's own view of the CLI: the running executable, run in a
/// directory the compose chose.
pub struct Cli {
    pub executable: PathBuf,
    pub deadline: Duration,
}

impl Cli {
    /// Where the inputs are, asked *in the snapshot*, of an environment
    /// holding nothing a connection string could be read from.
    ///
    /// `doctor` otherwise examines every configured environment before it
    /// answers, and a slow or unreachable server would hold up — or under the
    /// UI's deadline kill — a compose that needs nothing but `git`
    /// (**measured**: with the environment's variable set to an unreachable
    /// server `doctor` tried the connection and reported `Connection
    /// refused`; with it unset it reported the variable unset, connected to
    /// nothing, and carried both paths, exit 0).
    pub fn where_are_the_inputs(&self, project: &Path) -> Result<Where, CliRefusal> {
        let mut stripped = BTreeMap::new();
        for name in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(name) {
                stripped.insert(name.to_owned(), value);
            }
        }
        let output = self.run(
            project,
            &[
                "doctor".to_owned(),
                "--format=json".to_owned(),
                "--no-input".to_owned(),
            ],
            Some(stripped),
        )?;
        let envelope: DoctorEnvelope =
            serde_json::from_slice(&output.stdout).map_err(|e| CliRefusal::Unreadable {
                command: "doctor".to_owned(),
                detail: e.to_string(),
            })?;
        envelope.data.ok_or_else(|| CliRefusal::Unreadable {
            command: "doctor".to_owned(),
            detail: "the envelope carried no data".to_owned(),
        })
    }

    /// The intent command, run in the snapshot the overlay was laid into.
    pub fn record(&self, project: &Path, intent: &Intent) -> Result<(), CliRefusal> {
        let mut arguments = intent.arguments();
        arguments.push("--no-input".to_owned());
        let output = self.run(project, &arguments, None)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CliRefusal::Refused {
                command: arguments.join(" "),
                said: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    /// `validate`, run on the *tree* the commit will hold rather than on the
    /// checkout: an editor can rewrite a placed file while `validate` reads
    /// the checkout, and a check that read the editor's bytes would have
    /// passed the UI's.
    pub fn validate(&self, project: &Path) -> Result<(), CliRefusal> {
        let output = self.run(
            project,
            &[
                "validate".to_owned(),
                "--format=json".to_owned(),
                "--no-input".to_owned(),
            ],
            None,
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CliRefusal::Refused {
                command: "validate".to_owned(),
                said: findings_of(&output.stdout)
                    .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_owned()),
            })
        }
    }

    fn run(
        &self,
        project: &Path,
        arguments: &[String],
        environment: Option<BTreeMap<String, OsString>>,
    ) -> Result<std::process::Output, CliRefusal> {
        let mut command = Command::new(&self.executable);
        if let Some(only) = environment {
            command.env_clear();
            for (name, value) in only {
                command.env(name, value);
            }
        }
        command
            .current_dir(project)
            // The child is already inside the selected project; repeating a
            // relative root can select a same-named nested project instead.
            .arg("--project")
            .arg(".")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| CliRefusal::Unstartable(e.to_string()))?;
        // Drained on threads, like the `git` runner's. A `doctor` or
        // `validate` envelope for a real schema is far larger than a pipe,
        // and a child blocked in a write while this loop waits for its exit
        // is a deadlock that ends at the deadline and is then reported as if
        // the command had never started.
        let mut out = child.stdout.take().expect("stdout was piped");
        let mut err = child.stderr.take().expect("stderr was piped");
        let reading = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut bytes);
            bytes
        });
        let erring = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut bytes);
            bytes
        });
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(CliRefusal::Unstartable(e.to_string())),
            }
            if started.elapsed() >= self.deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = reading.join().unwrap_or_default();
        let stderr = erring.join().unwrap_or_default();
        match status {
            Some(status) => Ok(std::process::Output {
                status,
                stdout,
                stderr,
            }),
            None => Err(CliRefusal::Unstartable(format!(
                "`pbps {}` did not finish within {}s",
                arguments.join(" "),
                self.deadline.as_secs()
            ))),
        }
    }
}

/// The findings of a refusing envelope, shown as the page shows them.
fn findings_of(stdout: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Reported {
        findings: Vec<Finding>,
    }
    #[derive(Deserialize)]
    struct Finding {
        message: String,
    }
    let reported: Reported = serde_json::from_slice(stdout).ok()?;
    if reported.findings.is_empty() {
        return None;
    }
    Some(
        reported
            .findings
            .iter()
            .map(|finding| finding.message.clone())
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_can_name_an_intent_and_never_a_command() {
        // A closed vocabulary: whatever the browser sends, it becomes one of
        // six commands with its arguments after them, and a hostile value
        // stays one argument rather than becoming another option.
        let hostile = "--db=secret; apply $(anything)";
        assert_eq!(
            Intent::Drop {
                column: hostile.to_owned(),
                reason: hostile.to_owned(),
            }
            .arguments(),
            vec![
                "drop".to_owned(),
                hostile.to_owned(),
                format!("--reason={hostile}"),
            ]
        );
        assert_eq!(
            Intent::Rename {
                from: "dbo.customer.customer_name".to_owned(),
                to: "full_name".to_owned(),
            }
            .arguments(),
            vec!["rename", "dbo.customer.customer_name", "full_name"]
        );
    }

    #[test]
    fn the_prefilled_message_is_the_intent_spelled_as_the_cli_spells_it() {
        assert_eq!(
            Intent::Rename {
                from: "dbo.customer.customer_name".to_owned(),
                to: "full_name".to_owned(),
            }
            .message(),
            "rename dbo.customer.customer_name full_name"
        );
        assert_eq!(
            Intent::DropTable {
                table: "dbo.audit_2019".to_owned(),
                reason: "retention".to_owned(),
            }
            .message(),
            "drop table dbo.audit_2019"
        );
    }

    #[test]
    fn a_request_naming_a_kind_this_version_does_not_have_is_refused() {
        // `deny_unknown_fields` and a tagged enum together: the page cannot
        // reach a seventh command by inventing a name or a field.
        assert!(serde_json::from_str::<Intent>(r#"{"kind":"apply"}"#).is_err());
        assert!(
            serde_json::from_str::<Intent>(
                r#"{"kind":"rename","from":"a.b.c","to":"d","project":"/etc"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Intent>(r#"{"kind":"rename","from":"a.b.c","to":"d"}"#).is_ok()
        );
    }

    #[test]
    fn an_answer_larger_than_a_pipe_is_read_rather_than_waited_on() {
        // The child's stdout and stderr are pipes, and a pipe holds 64 KiB.
        // A runner that waited for the exit before reading would block the
        // child in a write; the compose would then end at its deadline and be
        // reported as if the command had never started. Both streams are
        // oversized here, because draining one and not the other deadlocks
        // just as thoroughly.
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let directory = std::env::temp_dir().join(format!(
            "pbps-cli-pipe-{}-{}",
            std::process::id(),
            crate::compose::record::random_name().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let envelope = serde_json::json!({
            "schema_version": 9,
            "tool_version": "0.0.0",
            "command": "doctor",
            "result": "ok",
            "findings": [],
            "data": {
                "declarations": "./schema",
                "identity_file": "./schema.ids.json",
                // A megabyte of payload, which is what a real estate's
                // `doctor` carries and what no pipe will hold.
                "padding": "x".repeat(1024 * 1024),
            },
        });
        let script = directory.join("pbps-with-a-lot-to-say");
        let mut file = std::fs::File::create(&script).unwrap();
        write!(
            file,
            "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' 'e' >&2\ncat <<'ENVELOPE'\n{}\nENVELOPE\n",
            serde_json::to_string(&envelope).unwrap()
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let cli = Cli {
            executable: script,
            deadline: Duration::from_secs(20),
        };
        let started = std::time::Instant::now();
        let found = cli.where_are_the_inputs(&directory).expect("it answers");

        assert_eq!(found.declarations, "./schema");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it answered at the deadline, which is what a blocked pipe looks like"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_refusing_validate_is_reported_by_its_findings_not_by_an_empty_stderr() {
        let envelope = br#"{"schema_version":9,"tool_version":"0","command":"validate",
            "result":"findings","findings":[{"id":"ids.orphan","severity":"error",
            "message":"the ids file names a column no declaration has","location":null,
            "remedy":null}]}"#;
        assert_eq!(
            findings_of(envelope).unwrap(),
            "the ids file names a column no declaration has"
        );
        assert!(findings_of(b"not an envelope").is_none());
    }
}
