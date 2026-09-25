//! The deployment trigger of #64 step 5: `plan --out` and `apply --plan`, run
//! as the same executable with typed arguments (ADR-0006 "Trigger").
//!
//! The browser supplies an environment *name*, file paths, the checksum the
//! deployment gate approved and the risk classes it allowed, never a command
//! line: each value becomes one joined `--flag=value` argument, so none can
//! turn into another option, and nothing reaches a shell. The UI holds no
//! approval: the checksum is whatever the request carries, and the CLI decides
//! whether it matches (DEC-1025.1).
//!
//! A run outlives the request that started it (DEC-1025.3). The child is not
//! tied to the connection, so a closed tab cannot interrupt an apply; the page
//! asks for the outcome with `runs`. Only `std::process` is used, so the
//! trigger needs no platform-specific code.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Requests carry a few names and paths; anything larger is not one of them.
pub const BODY_LIMIT: u64 = 16 * 1024;
/// What is kept of each output stream. The rest is still read, so a verbose
/// child never blocks on a full pipe.
const OUTPUT_LIMIT: usize = 64 * 1024;

pub const ACTIONS: [&str; 3] = ["plan", "apply", "runs"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRequest {
    environment: String,
    out: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyRequest {
    environment: String,
    plan: String,
    checksum: String,
    allow: Vec<String>,
    staged: bool,
    resume: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsRequest {}

/// One validated trigger, before it runs.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Plan {
        environment: String,
        out: String,
    },
    Apply {
        environment: String,
        plan: String,
        checksum: String,
        allow: Vec<String>,
        staged: bool,
        resume: bool,
    },
}

impl Invocation {
    fn parse(action: &str, body: &[u8]) -> Result<Self, String> {
        let malformed = |e: serde_json::Error| format!("Malformed {action} request: {e}");
        match action {
            "plan" => {
                let r: PlanRequest = serde_json::from_slice(body).map_err(malformed)?;
                Ok(Self::Plan {
                    environment: value("environment", r.environment)?,
                    out: value("plan file", r.out)?,
                })
            }
            "apply" => {
                let r: ApplyRequest = serde_json::from_slice(body).map_err(malformed)?;
                let allow = r
                    .allow
                    .into_iter()
                    .map(|class| {
                        // A risk class is a lowercase word with hyphens. Which
                        // words exist is the CLI's to say; this only keeps a
                        // comma or an option out of the joined list.
                        if !class.is_empty()
                            && class.starts_with(|c: char| c.is_ascii_lowercase())
                            && class.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                        {
                            Ok(class)
                        } else {
                            Err(format!("`{class}` is not a risk class name"))
                        }
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Self::Apply {
                    environment: value("environment", r.environment)?,
                    plan: value("plan file", r.plan)?,
                    checksum: value("checksum", r.checksum)?,
                    allow,
                    staged: r.staged,
                    resume: r.resume,
                })
            }
            _ => Err("Unknown trigger action".into()),
        }
    }

    fn environment(&self) -> &str {
        match self {
            Self::Plan { environment, .. } | Self::Apply { environment, .. } => environment,
        }
    }

    fn action(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::Apply { .. } => "apply",
        }
    }

    /// `--env`, never `--db`: a name from the browser cannot become a
    /// connection string, and the child reads the connection from its own
    /// environment (ADR-0015 decision 4).
    fn arguments(&self) -> Vec<String> {
        match self {
            Self::Plan { environment, out } => vec![
                "plan".into(),
                format!("--env={environment}"),
                format!("--out={out}"),
            ],
            Self::Apply {
                environment,
                plan,
                checksum,
                allow,
                staged,
                resume,
            } => {
                let mut arguments = vec![
                    "apply".into(),
                    format!("--env={environment}"),
                    format!("--plan={plan}"),
                    format!("--checksum={checksum}"),
                ];
                if !allow.is_empty() {
                    arguments.push(format!("--allow={}", allow.join(",")));
                }
                if *staged {
                    arguments.push("--staged".into());
                }
                if *resume {
                    arguments.push("--resume".into());
                }
                arguments
            }
        }
    }
}

fn value(name: &str, value: String) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("A {name} is required"))
    } else if value.chars().any(char::is_control) {
        Err(format!("The {name} contains a control character"))
    } else {
        Ok(value)
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

struct Run {
    action: &'static str,
    arguments: Vec<String>,
    child: Child,
    ended: Option<Option<i32>>,
    stdout: Arc<Mutex<Captured>>,
    stderr: Arc<Mutex<Captured>>,
}

impl Run {
    fn poll(&mut self) -> Result<(), String> {
        if self.ended.is_none() {
            match self.child.try_wait() {
                Ok(Some(status)) => self.ended = Some(status.code()),
                Ok(None) => {}
                // Unknown is not ended: the run stays reported as running.
                Err(_) => return Err("The run's state could not be read".into()),
            }
        }
        Ok(())
    }
}

/// What the page is told about one run. `code` is absent while it runs, and
/// also when it ended without one (a signal); `ended` tells the two apart.
#[derive(Debug, Serialize)]
struct RunView {
    environment: String,
    action: &'static str,
    arguments: Vec<String>,
    ended: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

pub struct Trigger {
    executable: PathBuf,
    project: PathBuf,
    runs: BTreeMap<String, Run>,
}

impl Trigger {
    pub fn new(executable: PathBuf, project: PathBuf) -> Self {
        Self {
            executable,
            project,
            runs: BTreeMap::new(),
        }
    }

    pub fn answer(&mut self, action: &str, body: &[u8]) -> Result<Vec<u8>, (u16, String)> {
        if action == "runs" {
            serde_json::from_slice::<RunsRequest>(body)
                .map_err(|e| (400, format!("Malformed runs request: {e}")))?;
            return self.views().map_err(|message| (500, message));
        }
        let invocation = Invocation::parse(action, body).map_err(|message| (400, message))?;
        self.start(invocation)?;
        self.views().map_err(|message| (500, message))
    }

    fn start(&mut self, invocation: Invocation) -> Result<(), (u16, String)> {
        let environment = invocation.environment().to_owned();
        if let Some(run) = self.runs.get_mut(&environment) {
            run.poll().map_err(|message| (500, message))?;
            if run.ended.is_none() {
                return Err((
                    409,
                    format!(
                        "A {} is still running against `{environment}`; wait for its outcome",
                        run.action
                    ),
                ));
            }
        }
        if let Invocation::Plan { out, .. } = &invocation {
            unused_path(&self.project.join(out)).map_err(|message| (409, message))?;
        }
        let arguments = invocation.arguments();
        let mut child = Command::new(&self.executable)
            .current_dir(&self.project)
            .arg("--project")
            // The child is already inside the selected project; see client.rs.
            .arg(".")
            .arg("--no-input")
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| (502, "Could not start the pbps command".to_owned()))?;
        let stdout = capture(child.stdout.take().expect("piped"));
        let stderr = capture(child.stderr.take().expect("piped"));
        self.runs.insert(
            environment,
            Run {
                action: invocation.action(),
                arguments,
                child,
                ended: None,
                stdout,
                stderr,
            },
        );
        Ok(())
    }

    fn views(&mut self) -> Result<Vec<u8>, String> {
        let mut views = Vec::new();
        for (environment, run) in &mut self.runs {
            run.poll()?;
            let text = |captured: &Arc<Mutex<Captured>>| {
                let captured = captured.lock().unwrap_or_else(|e| e.into_inner());
                (
                    String::from_utf8_lossy(&captured.bytes).into_owned(),
                    captured.truncated,
                )
            };
            let (stdout, out_cut) = text(&run.stdout);
            let (stderr, err_cut) = text(&run.stderr);
            views.push(RunView {
                environment: environment.clone(),
                action: run.action,
                arguments: run.arguments.clone(),
                ended: run.ended.is_some(),
                code: run.ended.flatten(),
                stdout,
                stderr,
                truncated: out_cut || err_cut,
            });
        }
        Ok(serde_json::to_vec(&views).expect("run views serialize"))
    }
}

/// `plan --out` replaces whatever is at its path. A saved plan is the
/// artifact a checksum was approved for, so the viewer refuses a path that
/// already names anything, a dangling link included, and one it cannot
/// inspect (DEC-1025.2).
fn unused_path(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!(
            "`{}` already exists; choose a new plan file",
            path.display()
        )),
        Err(_) => Err(format!("`{}` could not be inspected", path.display())),
    }
}

fn capture(mut stream: impl Read + Send + 'static) -> Arc<Mutex<Captured>> {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let sink = Arc::clone(&captured);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let mut captured = sink.lock().unwrap_or_else(|e| e.into_inner());
            let room = OUTPUT_LIMIT.saturating_sub(captured.bytes.len());
            captured.bytes.extend_from_slice(&buffer[..read.min(room)]);
            if read > room {
                captured.truncated = true;
            }
        }
    });
    captured
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(body: serde_json::Value) -> Result<Invocation, String> {
        Invocation::parse("apply", &serde_json::to_vec(&body).unwrap())
    }

    #[test]
    fn browser_values_stay_one_named_option_each_and_select_no_command() {
        let hostile = "--db=Server=x;Password=p; rm -rf / $(apply)";
        let plan = Invocation::parse(
            "plan",
            &serde_json::to_vec(&serde_json::json!({"environment": hostile, "out": hostile}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            plan.arguments(),
            [
                "plan",
                &format!("--env={hostile}"),
                &format!("--out={hostile}")
            ]
        );
        let run = apply(serde_json::json!({"environment": "prod", "plan": hostile,
            "checksum": hostile, "allow": ["rename", "not-null"], "staged": true, "resume": true}))
        .unwrap();
        assert_eq!(
            run.arguments(),
            [
                "apply",
                "--env=prod",
                &format!("--plan={hostile}"),
                &format!("--checksum={hostile}"),
                "--allow=rename,not-null",
                "--staged",
                "--resume",
            ]
        );
        let plain = apply(serde_json::json!({"environment": "prod", "plan": "p.json",
            "checksum": "c", "allow": [], "staged": false, "resume": false}))
        .unwrap();
        assert_eq!(
            plain.arguments(),
            ["apply", "--env=prod", "--plan=p.json", "--checksum=c"]
        );
    }

    #[test]
    fn a_request_that_is_not_exactly_one_typed_trigger_is_refused() {
        let good = serde_json::json!({"environment": "prod", "plan": "p.json",
            "checksum": "c", "allow": [], "staged": false, "resume": false});
        assert!(apply(good.clone()).is_ok());
        for (key, bad) in [
            ("sql", serde_json::json!("DROP TABLE t")),
            ("db", serde_json::json!("Server=x")),
            ("arguments", serde_json::json!(["--db=x"])),
        ] {
            let mut body = good.clone();
            body[key] = bad;
            assert!(apply(body).is_err(), "{key}");
        }
        for (key, bad) in [
            ("environment", ""),
            ("plan", ""),
            ("checksum", ""),
            ("environment", "prod\n--db=x"),
            ("checksum", "c\u{0}"),
        ] {
            let mut body = good.clone();
            body[key] = bad.into();
            assert!(apply(body).is_err(), "{key}={bad:?}");
        }
        for class in [
            "",
            "Rename",
            "rename,destructive",
            "--db=x",
            "-rename",
            "not null",
        ] {
            let mut body = good.clone();
            body["allow"] = serde_json::json!([class]);
            assert!(apply(body).is_err(), "{class:?}");
        }
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("checksum");
        assert!(apply(missing).is_err());
        assert!(Invocation::parse("push", b"{}").is_err());
        assert!(Invocation::parse("plan", b"{\"environment\":\"prod\"}").is_err());
    }

    #[test]
    fn a_plan_file_path_that_names_anything_is_refused() {
        let directory =
            std::env::temp_dir().join(format!("pbps-ui-trigger-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        assert!(unused_path(&directory.join("fresh.json")).is_ok());
        std::fs::write(directory.join("plan.json"), "{}").unwrap();
        assert!(unused_path(&directory.join("plan.json")).is_err());
        assert!(unused_path(&directory).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(directory.join("nowhere"), directory.join("dangling"))
                .unwrap();
            assert!(unused_path(&directory.join("dangling")).is_err());
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A stand-in for `pbps`: it prints its arguments, waits on a file the
    /// test controls, and exits with a chosen code, so a run can be observed
    /// while it is still going.
    #[cfg(unix)]
    fn stand_in(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join("pbps-stand-in");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\"\necho diagnostics >&2\n\
             while [ ! -e release ]; do sleep 0.05; done\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    fn runs(trigger: &mut Trigger) -> Vec<serde_json::Value> {
        serde_json::from_slice(&trigger.answer("runs", b"{}").unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_run_outlives_its_request_and_blocks_a_second_run_on_that_environment_only() {
        let directory =
            std::env::temp_dir().join(format!("pbps-ui-trigger-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let mut trigger = Trigger::new(stand_in(&directory), directory.clone());
        let body = |environment: &str| {
            serde_json::to_vec(&serde_json::json!({"environment": environment,
                "plan": "p.json", "checksum": "abc", "allow": ["rename"],
                "staged": false, "resume": false}))
            .unwrap()
        };

        // The request returns while the child is still running.
        let started: Vec<serde_json::Value> =
            serde_json::from_slice(&trigger.answer("apply", &body("prod")).unwrap()).unwrap();
        assert_eq!(started[0]["ended"], false);
        assert_eq!(started[0]["code"], serde_json::Value::Null);

        let refused = trigger.answer("apply", &body("prod")).unwrap_err();
        assert_eq!(refused.0, 409, "{}", refused.1);
        assert!(trigger.answer("apply", &body("test")).is_ok());

        std::fs::write(directory.join("release"), "").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let ended = loop {
            let all = runs(&mut trigger);
            if all.iter().all(|run| run["ended"] == true) {
                break all;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the stand-in never ended"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let prod = ended
            .iter()
            .find(|run| run["environment"] == "prod")
            .unwrap();
        assert_eq!(prod["code"], 3);
        assert_eq!(prod["action"], "apply");
        // Exactly the typed arguments reached the child, one per line.
        assert_eq!(
            prod["stdout"],
            "--project\n.\n--no-input\napply\n--env=prod\n--plan=p.json\n--checksum=abc\n--allow=rename\n"
        );
        assert_eq!(prod["stderr"], "diagnostics\n");
        // Once the run has ended, the environment takes another.
        assert!(trigger.answer("apply", &body("prod")).is_ok());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_plan_path_starts_no_child() {
        let directory =
            std::env::temp_dir().join(format!("pbps-ui-trigger-refused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("release"), "").unwrap();
        std::fs::write(directory.join("taken.json"), "approved bytes").unwrap();
        let mut trigger = Trigger::new(stand_in(&directory), directory.clone());
        let refused = trigger
            .answer("plan", br#"{"environment":"prod","out":"taken.json"}"#)
            .unwrap_err();
        assert_eq!(refused.0, 409);
        assert!(runs(&mut trigger).is_empty());
        assert_eq!(
            std::fs::read_to_string(directory.join("taken.json")).unwrap(),
            "approved bytes"
        );
        assert!(
            trigger
                .answer("plan", br#"{"environment":"prod","out":"new.json"}"#)
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}
