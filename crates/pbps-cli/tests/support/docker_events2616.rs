//! Live Docker evidence for the rehearsal fixtures. History alone can lose a
//! creation after only 256 unrelated daemon events (#2616).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Event {
    action: String,
    actor: Actor,
}

#[derive(serde::Deserialize)]
struct Actor {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Attributes")]
    attributes: BTreeMap<String, String>,
}

fn creations(events: &[Event], pid: u32) -> Vec<String> {
    let process = format!("{pid}-");
    events
        .iter()
        .filter(|event| event.action == "create")
        .filter_map(|event| {
            let name = event.actor.attributes.get("name")?;
            let owner = event.actor.attributes.get("pbps.resolver.process")?;
            (name.starts_with("pbps-scratch-") && owner.starts_with(&process)).then(|| name.clone())
        })
        .collect()
}

struct Files(PathBuf);

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Collector {
    child: Child,
    files: Files,
    stopped: bool,
}

impl Collector {
    fn start(mut command: Command) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!(
            "pbps-events2616-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&dir).map_err(|error| {
            format!("Docker event collector: {error}; status=not started; stderr=unavailable")
        })?;
        let files = Files(dir);
        let prepare = || -> std::io::Result<_> {
            Ok((
                std::fs::File::create(files.0.join("stdout"))?,
                std::fs::File::create(files.0.join("stderr"))?,
            ))
        };
        let (stdout, stderr) = prepare().map_err(|error| {
            format!("Docker event collector: {error}; status=not started; stderr=unavailable")
        })?;
        let child = command
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|error| {
                format!("Docker event collector: {error}; status=not started; stderr=unavailable")
            })?;
        Ok(Self {
            child,
            files,
            stopped: false,
        })
    }

    fn error(&mut self, reason: impl std::fmt::Display) -> String {
        let status = match self.child.try_wait() {
            Ok(Some(status)) => status.to_string(),
            Ok(None) => "running".into(),
            Err(error) => format!("unreadable ({error})"),
        };
        let stderr = std::fs::read_to_string(self.files.0.join("stderr"))
            .unwrap_or_else(|error| format!("unreadable ({error})"));
        format!("Docker event collector: {reason}; status={status}; stderr={stderr:?}")
    }

    fn read_events(&mut self, complete: bool) -> Result<Vec<Event>, String> {
        let bytes =
            std::fs::read(self.files.0.join("stdout")).map_err(|error| self.error(error))?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1);
        // A live writer may be between writes of one JSON line. Once stopped,
        // a partial final line is damaged evidence, never an observed absence.
        if complete && end != bytes.len() {
            return Err(self.error("incomplete final event"));
        }
        let text = std::str::from_utf8(&bytes[..end]).map_err(|error| self.error(error))?;
        text.lines()
            .map(|line| serde_json::from_str(line).map_err(|error| self.error(error)))
            .collect()
    }

    fn events(&mut self) -> Result<Vec<Event>, String> {
        match self.child.try_wait() {
            Ok(None) => self.read_events(false),
            Ok(Some(_)) => Err(self.error("unexpected termination")),
            Err(error) => Err(self.error(error)),
        }
    }

    fn stop(&mut self) -> Result<Vec<Event>, String> {
        self.events()?;
        self.child.kill().map_err(|error| self.error(error))?;
        let status = self.child.wait().map_err(|error| self.error(error))?;
        self.stopped = true;
        // Only our deliberate termination is expected. An exit racing kill
        // must still report its actual failure instead of yielding no events.
        #[cfg(unix)]
        let killed = {
            use std::os::unix::process::ExitStatusExt;
            status.signal() == Some(9)
        };
        #[cfg(not(unix))]
        let killed = status.code() == Some(1);
        if !killed {
            return Err(self.error("collector exited before it was stopped"));
        }
        self.read_events(true)
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct OwnedContainer {
    id: String,
    stem: String,
    sequence: u32,
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| {
            format!("docker {args:?}: {error}; status=not started; stderr=unavailable")
        })?;
    if !out.status.success() {
        return Err(format!(
            "docker {args:?}: status={}; stderr={:?}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|error| {
        format!(
            "docker {args:?}: {error}; status={}; stderr={:?}",
            out.status, out.stderr
        )
    })
}

impl OwnedContainer {
    fn new(image: &str) -> Result<Self, String> {
        let stem = format!(
            "c2616-events-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        );
        let id = docker(&[
            "create",
            "--name",
            &stem,
            "--network",
            "none",
            "--entrypoint",
            "/bin/true",
            image,
        ])?;
        Ok(Self {
            id: id.trim().to_owned(),
            stem,
            sequence: 0,
        })
    }

    fn rename(&mut self) -> Result<String, String> {
        self.sequence += 1;
        let name = format!("{}-{}", self.stem, self.sequence);
        docker(&["rename", &self.id, &name])?;
        Ok(name)
    }
}

impl Drop for OwnedContainer {
    fn drop(&mut self) {
        // PostgreSQL images declare an anonymous volume even without start.
        let _ = docker(&["rm", "-f", "-v", &self.id]);
    }
}

pub(super) struct DockerEvents {
    collector: Collector,
    marker: OwnedContainer,
}

impl DockerEvents {
    pub(super) fn start(image: &str) -> Result<Self, String> {
        let marker = OwnedContainer::new(image)?;
        let mut command = Command::new("docker");
        command.args([
            "events",
            "--filter",
            "type=container",
            "--filter",
            "event=create",
            "--filter",
            "event=rename",
            "--format",
            "{{json .}}",
        ]);
        let mut events = Self {
            collector: Collector::start(command)?,
            marker,
        };
        events.barrier(true)?;
        Ok(events)
    }

    fn barrier(&mut self, starting: bool) -> Result<(), String> {
        let deadline = Instant::now() + super::LIMIT;
        loop {
            let name = self
                .marker
                .rename()
                .map_err(|error| self.collector.error(error))?;
            let retry = Instant::now() + Duration::from_millis(100);
            loop {
                if self.collector.events()?.iter().any(|event| {
                    event.action == "rename"
                        && event.actor.id == self.marker.id
                        && event.actor.attributes.get("name") == Some(&name)
                }) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(self.collector.error("event barrier timed out"));
                }
                if starting && Instant::now() >= retry {
                    // The first rename can precede subscription. A later
                    // observed rename proves readiness before pbps is spawned.
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    pub(super) fn created_by(mut self, pid: u32) -> Result<Vec<String>, String> {
        // The final marker fences earlier events before stopping the stream.
        self.barrier(false)?;
        let events = self.collector.stop()?;
        Ok(creations(&events, pid))
    }
}

fn historical_creations(pid: u32, since: u64) -> Vec<String> {
    let until = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 1)
    .to_string();
    let text = docker(&[
        "events",
        "--since",
        &since.to_string(),
        "--until",
        &until,
        "--filter",
        "type=container",
        "--filter",
        "event=create",
        "--format",
        "{{json .}}",
    ])
    .unwrap();
    let events: Vec<Event> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    creations(&events, pid)
}

#[test]
#[ignore = "needs live postgres and docker"]
fn creation_evidence_survives_unrelated_history_eviction() {
    let f = super::Fixture::new(
        &super::server(),
        "events2616",
        super::Kind::Docker,
        &super::Options::default(),
    );
    f.add_column("true");
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let evidence = DockerEvents::start(&super::image()).unwrap();
    let (out, pid) = f.plan_as_process(&[]);
    assert_eq!(super::code(&out), 0, "{}", super::said(&out));
    assert!(super::stdout(&out).contains("it answered the binding questions"));
    assert!(super::stdout(&out).contains("Rehearsal: converged on scratch"));

    let mut noise = OwnedContainer::new(&super::image()).unwrap();
    // These are real, unrelated events from our own fixture, after the tested
    // creation. Keeping the stream open also exercises its retained evidence.
    for _ in 0..300 {
        noise.rename().unwrap();
    }
    let made = evidence.created_by(pid).unwrap();
    assert_eq!(made.len(), 1, "one container for both uses: {made:?}");
    assert!(
        super::remaining_containers()
            .iter()
            .all(|name| !made.contains(name))
    );
    assert!(
        historical_creations(pid, since).is_empty(),
        "the control must evict the creation"
    );
}

fn failed_collector() -> Collector {
    let mut command = Command::new(super::BIN);
    command.arg("--collector-error-2616");
    let mut collector = Collector::start(command).unwrap();
    assert_eq!(collector.child.wait().unwrap().code(), Some(2));
    collector
}

#[test]
fn a_failed_collector_reports_status_and_stderr_instead_of_absence() {
    let mut collector = failed_collector();
    let error = collector
        .events()
        .err()
        .expect("collector failure must be explicit");
    assert!(
        error.contains("status=exit code: 2") || error.contains("status=exit status: 2"),
        "{error}"
    );
    assert!(
        error.contains("stderr=") && error.contains("--collector-error-2616"),
        "{error}"
    );
}

#[test]
fn unreadable_or_damaged_events_are_errors_with_process_diagnostics() {
    let mut collector = failed_collector();
    let path = collector.files.0.join("stdout");
    for bytes in [b"not json\n".as_slice(), b"\xff\n", b"{\"Action\":"] {
        std::fs::write(&path, bytes).unwrap();
        let error = collector.read_events(true).err().expect("damaged evidence");
        assert!(
            error.contains("status=exit") && error.contains("--collector-error-2616"),
            "{error}"
        );
    }
    std::fs::remove_file(path).unwrap();
    let error = collector
        .read_events(true)
        .err()
        .expect("unreadable evidence");
    assert!(
        error.contains("status=exit") && error.contains("--collector-error-2616"),
        "{error}"
    );
}
