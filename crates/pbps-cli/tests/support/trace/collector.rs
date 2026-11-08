//! The one sink scanner and process adapters shared by both engines (#2488).
use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

pub(super) const SOURCES: [&str; 6] = [
    "TRACE2488_PRIVATE_VIEW_s5C8",
    "TRACE2488_PRIVATE_ROUTINE_d9E2",
    "TRACE2488_PRIVATE_TRIGGER_k7H4",
    "TRACE2488_PRIVATE_CHECK_m3J6",
    "TRACE2488_PRIVATE_DEFAULT_p8L1",
    "TRACE2488_PRIVATE_COMPUTED_q6R2",
];
pub(super) const DICTIONARY: [&str; 3] = [
    "TRACE2488_PRIVATE_CHOICE_red",
    "TRACE2488_PRIVATE_CHOICE_amber",
    "TRACE2488_PRIVATE_CHOICE_green",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Sink {
    Stdout,
    Stderr,
    Artifact,
    Plan,
    Sql,
    Ledger,
    Snapshot,
    HookArgv,
    HookEnvironment,
    HookStdin,
    Ui,
}

pub(super) const SINKS: [Sink; 11] = [
    Sink::Stdout,
    Sink::Stderr,
    Sink::Artifact,
    Sink::Plan,
    Sink::Sql,
    Sink::Ledger,
    Sink::Snapshot,
    Sink::HookArgv,
    Sink::HookEnvironment,
    Sink::HookStdin,
    Sink::Ui,
];

#[derive(Default)]
pub(super) struct Trace {
    forbidden: BTreeSet<Vec<u8>>,
    seen: BTreeSet<Sink>,
}

impl Trace {
    pub(super) fn new() -> Self {
        let mut trace = Self::default();
        for value in SOURCES.into_iter().chain(DICTIONARY) {
            trace.candidate(value.as_bytes());
        }
        trace
    }

    pub(super) fn candidate(&mut self, value: &[u8]) {
        assert!(!value.is_empty());
        let digest = Sha256::digest(value);
        self.forbidden.insert(value.to_vec());
        self.forbidden.insert(format!("{digest:x}").into_bytes());
        self.forbidden.insert(format!("{digest:X}").into_bytes());
        self.forbidden.insert(
            base64::engine::general_purpose::STANDARD
                .encode(digest)
                .into_bytes(),
        );
    }

    pub(super) fn check(&mut self, sink: Sink, bytes: &[u8]) -> Result<(), String> {
        self.seen.insert(sink);
        // Patterns are UTF-8 fixture definitions or ASCII verifiers. Lossy
        // decoding preserves each pattern even in an otherwise binary artifact.
        let text = String::from_utf8_lossy(bytes);
        if self
            .forbidden
            .iter()
            .any(|value| text.contains(std::str::from_utf8(value).unwrap()))
        {
            // Even an assertion failure must not echo a captured definition.
            Err(format!(
                "private definition or unkeyed verifier reached {sink:?}"
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn collect(&mut self, sink: Sink, bytes: &[u8]) {
        self.check(sink, bytes).unwrap();
    }

    pub(super) fn output(&mut self, output: &Output) {
        self.collect(Sink::Stdout, &output.stdout);
        self.collect(Sink::Stderr, &output.stderr);
    }

    pub(super) fn artifact(&mut self, sink: Sink, path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).unwrap();
        self.collect(Sink::Artifact, &bytes);
        if sink != Sink::Artifact {
            self.collect(sink, &bytes);
        }
        bytes
    }

    pub(super) fn rows(&mut self, raw: &str) {
        self.collect(Sink::Ledger, raw.as_bytes());
        let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            self.collect(
                Sink::Snapshot,
                row["state_json"].as_str().unwrap().as_bytes(),
            );
        }
    }

    pub(super) fn ui(&mut self, bytes: &[u8]) {
        self.collect(Sink::Ui, bytes);
    }

    pub(super) fn files(&mut self, root: &Path) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                self.files(&path);
            } else if kind.is_file() {
                let bytes = self.artifact(Sink::Artifact, &path);
                match path.extension().and_then(|s| s.to_str()) {
                    Some("stdout") => self.collect(Sink::Stdout, &bytes),
                    Some("stderr") => self.collect(Sink::Stderr, &bytes),
                    Some("argv") => self.collect(Sink::HookArgv, &bytes),
                    Some("environment") => self.collect(Sink::HookEnvironment, &bytes),
                    Some("stdin") => self.collect(Sink::HookStdin, &bytes),
                    _ => {}
                }
            } else {
                panic!(
                    "trace cannot inspect a non-regular artifact at {}",
                    path.display()
                );
            }
        }
    }

    pub(super) fn complete(&self) {
        assert_eq!(
            self.seen,
            SINKS.into_iter().collect(),
            "unobserved output sink"
        );
    }
}

pub(super) fn ui_child_wait(
    slowest: Duration,
    elapsed: Duration,
    action: &str,
    last: &serde_json::Value,
) -> Duration {
    // Resolver commands slow down with the host. Allow several times an earlier
    // command's cost, but keep a hung child bounded even after a very slow step.
    let budget = slowest
        .saturating_mul(3)
        .clamp(Duration::from_secs(180), Duration::from_secs(600));
    assert!(
        elapsed < budget,
        "{action}: UI child never completed after {elapsed:?} (budget {budget:?}, slowest earlier command {slowest:?}); last state: ended={}, code={}, truncated={}, stdout_bytes={}, stderr_bytes={}",
        last["ended"],
        last["code"],
        last["truncated"],
        last["stdout"].as_str().map(str::len).unwrap_or(0),
        last["stderr"].as_str().map(str::len).unwrap_or(0),
    );
    budget
}

pub(super) struct OfflineExport(pub(super) PathBuf);

impl Drop for OfflineExport {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Use the public HTTP boundary: Trigger is intentionally private to the UI.
// The idle listener lives only until this integration-test process exits.
pub(super) struct Ui {
    address: String,
    token: String,
}

impl Ui {
    pub(super) fn new(executable: PathBuf, project: PathBuf) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let viewer = pbps_ui::Viewer::bind(pbps_ui::Config {
                executable,
                project,
                token: [0x48; 32],
                docs_style_hash: "A".repeat(44),
            })
            .unwrap();
            sender.send(viewer.url()).unwrap();
            viewer.serve().unwrap();
        });
        let url = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let (address, token) = url
            .strip_prefix("http://")
            .unwrap()
            .split_once("/#")
            .unwrap();
        Self {
            address: address.into(),
            token: token.into(),
        }
    }

    pub(super) fn answer(&mut self, action: &str, body: &[u8]) -> Vec<u8> {
        use std::io::{Read as _, Write as _};
        let mut stream = std::net::TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(90)))
            .unwrap();
        write!(stream, "POST /api/trigger/{action} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nX-Pbps-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", self.address, self.address, self.token, body.len()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 200 "),
            "UI refused the test request"
        );
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
        response[split..].to_vec()
    }
}

pub(super) const DISPOSAL_SHIM: &str = r#"#!/bin/sh
set -eu
if [ "$1" = run ]; then
  set +e
  "$PBPS_TRACE_DOCKER_REAL" "$@" > "$PBPS_TRACE_CREATED"
  pbps_trace_status=$?
  set -e
  cat "$PBPS_TRACE_CREATED"
  exit "$pbps_trace_status"
fi
if [ "$1" = rm ]; then
  # Recovery precedes this run opening its capture (DEC-2410.3). Once opened,
  # even an empty capture keeps every target subject to the disposal assertion.
  # Recorder-only controls omit CREATED and always exercise recording.
  if [ -n "${PBPS_TRACE_CREATED:-}" ] && [ ! -e "$PBPS_TRACE_CREATED" ]; then
    exec "$PBPS_TRACE_DOCKER_REAL" "$@"
  fi
  # Drop may record another attempt while the parent reads this append-only log.
  printf '%s\n' "$@" "" >> "$PBPS_TRACE_ATTEMPTED"
  echo 'forced trace Docker disposal failure' >&2
  exit 1
fi
exec "$PBPS_TRACE_DOCKER_REAL" "$@"
"#;

pub(super) fn assert_disposal_attempts(recorded: &str, id: &str) {
    // A blank line commits one argv record; a late writer may still be appending.
    let (complete, _) = recorded
        .rsplit_once("\n\n")
        .expect("at least one disposal attempt must be recorded");
    assert!(
        !complete.is_empty(),
        "at least one disposal attempt must be recorded"
    );
    for attempt in complete.split("\n\n") {
        let mut args = attempt.split('\n');
        assert_eq!(args.next(), Some("rm"));
        let removed = args.filter(|v| !v.starts_with('-')).collect::<Vec<_>>();
        assert_eq!(
            removed,
            [id],
            "cleanup must target exactly this run's new container"
        );
    }
}

pub(super) fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

pub(super) fn has_namespace(object: &serde_json::Value, namespace: &str) -> bool {
    object["name"]
        .as_array()
        .is_some_and(|names| names.first().and_then(|v| v.as_str()) == Some(namespace))
        || object["signature"]
            .as_array()
            .is_some_and(|s| s.iter().any(|o| has_namespace(o, namespace)))
}

pub(super) fn keyed_records_differ(first: &serde_json::Value, second: &serde_json::Value) {
    let before = |plan: &serde_json::Value| plan["analysis"]["evidence"]["before"].clone();
    let a = before(first);
    let b = before(second);
    assert_ne!(a["key_id"], b["key_id"]);
    let records = b["prerequisites"].as_array().unwrap();
    for record in a["prerequisites"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| has_namespace(&r["object"], "ext"))
    {
        let other = records
            .iter()
            .find(|r| r["object"] == record["object"])
            .unwrap();
        assert_ne!(
            record["properties"], other["properties"],
            "a retained property became a key-independent guessing oracle"
        );
    }
}

/// Preserve both streams before the UI drops stderr or truncates a run payload.
pub(super) fn capture_launcher(
    root: &Path,
    binary: &Path,
    environment: &[(String, String)],
) -> PathBuf {
    let path = root.join("ui-launcher.sh");
    let mut script = String::from("#!/bin/sh\nset -eu\n");
    for (key, value) in environment {
        script.push_str(&format!("export {key}={}\n", quoted(value)));
    }
    script.push_str(&format!("cd {}\nset +e\nprintf %+s \"$$\" > ui-last.pid\n{} \"$@\" > ui-$$.stdout 2> ui-$$.stderr\nresult=$?\ncat ui-$$.stdout\ncat ui-$$.stderr >&2\nexit \"$result\"\n", quoted(&root.display().to_string()), quoted(&binary.display().to_string())));
    executable(&path, &script);
    path
}

pub(super) fn read_ui(
    trace: &mut Trace,
    client: &pbps_ui::client::Client,
    root: &Path,
    view: &pbps_ui::client::View,
) {
    let result = client.read(view);
    // Read the raw streams even if the UI rejected its typed stdout contract.
    trace.files(root);
    let bytes = result.unwrap_or_else(|why| {
        let pid = std::fs::read_to_string(root.join("ui-last.pid")).unwrap();
        let stdout = std::fs::read(root.join(format!("ui-{pid}.stdout"))).unwrap();
        let stderr = std::fs::read(root.join(format!("ui-{pid}.stderr"))).unwrap();
        panic!(
            "{view:?}: {why}; stdout={} stderr={}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    });
    trace.ui(&bytes);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn ui_reader_cannot_discard_private_terminal_stderr() {
        let project = crate::Demo::new("trace2488-ui-stderr");
        // The source-bearing stand-in is outside the scanned project: finding
        // its script would not prove that a UI reader preserved stderr.
        let external = OfflineExport(project.dir.with_extension("terminal-source"));
        std::fs::create_dir(&external.0).unwrap();
        let binary = external.0.join("fake-pbps.sh");
        executable(
            &binary,
            &format!(
                "#!/bin/sh\nprintf '<p>public schema</p>'\nprintf '%s' '{}' >&2\n",
                SOURCES[0]
            ),
        );
        let source =
            crate::stand_in::retry_exec(|| std::process::Command::new(&binary).output()).unwrap();
        assert!(source.status.success());
        assert_eq!(source.stderr, SOURCES[0].as_bytes());
        let launcher = capture_launcher(&project.dir, &binary, &[]);
        let probe = crate::stand_in::retry_exec(|| {
            std::process::Command::new(&launcher)
                .current_dir(&project.dir)
                .output()
        })
        .unwrap();
        assert!(probe.status.success());
        // Successful probes settle inherited write descriptors. Their output
        // must not supply the leak that the actual UI-read control observes.
        for entry in std::fs::read_dir(&project.dir).unwrap() {
            let path = entry.unwrap().path();
            if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("stdout" | "stderr")
            ) {
                std::fs::remove_file(path).unwrap();
            }
        }
        Trace::new().files(&project.dir);
        let client = pbps_ui::client::Client {
            executable: launcher,
            project: project.dir.clone(),
        };
        let rejected = std::panic::catch_unwind(|| {
            read_ui(
                &mut Trace::new(),
                &client,
                &project.dir,
                &pbps_ui::client::View::Docs,
            );
        })
        .expect_err("the UI discarded private stderr without scanning it");
        let message = rejected
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| rejected.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(
            message.contains("private definition or unkeyed verifier reached"),
            "the control failed before scanning private stderr"
        );
    }
}
