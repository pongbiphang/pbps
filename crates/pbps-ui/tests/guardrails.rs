//! ADR-0006's refusals over HTTP, against a running viewer (#1050). The
//! executable the viewer runs is a stand-in that logs every argument vector
//! it receives, so the test sees exactly what each route asked the CLI to do.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

const SQL: &str = "DROP TABLE dbo.t; --";
const APPROVAL: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

struct Viewer {
    address: String,
    token: String,
}

impl Viewer {
    fn start(project: &Path, executable: PathBuf) -> Self {
        let viewer = pbps_ui::Viewer::bind(pbps_ui::Config {
            executable,
            project: project.to_path_buf(),
            token: [7; 32],
            docs_style_hash: "A".repeat(43) + "=",
        })
        .unwrap();
        let url = viewer.url();
        std::thread::spawn(move || viewer.serve());
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

    fn send(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(60)))
            .unwrap();
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: {0}\r\nOrigin: http://{0}\r\nX-Pbps-Token: {1}\r\nConnection: close\r\n",
            self.address, self.token
        )
        .unwrap();
        if let Some(body) = body {
            write!(
                socket,
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        } else {
            write!(socket, "\r\n").unwrap();
        }
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            body.into(),
        )
    }
}

/// Logs each invocation to a file of its own, arguments separated by U+001F,
/// then fails without output, as a CLI that answered nothing. Triggered runs
/// overlap, so a shared log could interleave their arguments.
fn stand_in(directory: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = directory.join("invocations");
    std::fs::create_dir_all(&log).unwrap();
    let script = directory.join("pbps-stand-in");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nrecord=$(mktemp '{}/run.XXXXXX')\nfor a in \"$@\"; do printf '%s\\037' \"$a\"; done > \"$record\"\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn escaped(value: &str) -> String {
    value.bytes().map(|b| format!("%{b:02X}")).collect()
}

/// Everything under `root`, as (path, bytes), except the stand-in's own
/// files.
fn files(root: &Path, skip: &[&Path], found: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if skip.iter().any(|s| *s == path) {
            continue;
        }
        if path.is_dir() {
            files(&path, skip, found);
        } else {
            // Unreadable is not empty: it could hold what is searched for.
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{} could not be read: {e}", path.display()));
            found.push((path.clone(), bytes));
        }
    }
}

#[test]
fn every_route_keeps_sql_out_of_the_cli_and_stores_no_approval() {
    let directory =
        std::env::temp_dir().join(format!("pbps-ui-guardrails-http-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let project = directory.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let (executable, log) = stand_in(&directory);
    let viewer = Viewer::start(&project, executable);
    let sql = escaped(SQL);

    // The routes come from the router's own tables, so one added later is
    // covered here the day it exists.
    let (reads, writes) = pbps_ui::routes();
    // Reads: SQL as the route's own parameter, and as an unnamed one.
    for (path, parameter) in &reads {
        let (status, body) = viewer.send("GET", &format!("{path}?sql={sql}"), None);
        assert_eq!(status, 400, "{path}: {body}");
        if let Some(parameter) = parameter {
            viewer.send("GET", &format!("{path}?{parameter}={sql}"), None);
        }
    }
    // Writes: an unnamed field on every action is refused. Compose answers
    // 501 where it is not qualified (#471), before reading any field.
    assert!(writes.len() >= 14, "{writes:?}");
    for path in &writes {
        let (status, body) = viewer.send("POST", path, Some(r#"{"sql":"DROP TABLE t"}"#));
        let unqualified = path.starts_with("/api/compose/") && cfg!(not(target_os = "linux"));
        assert_eq!(
            status,
            if unqualified { 501 } else { 400 },
            "{path}: {body}"
        );
    }
    // Then SQL in each field a trigger names.
    let quoted = serde_json::to_string(SQL).unwrap();
    for body in [
        format!(r#"{{"environment":{quoted},"out":"a.json"}}"#),
        format!(r#"{{"environment":"one","out":{quoted}}}"#),
    ] {
        viewer.send("POST", "/api/trigger/plan", Some(&body));
    }
    for (environment, plan, checksum) in [
        ("two", SQL, APPROVAL),
        ("three", "p.json", SQL),
        ("four", "p.json", APPROVAL),
    ] {
        let body = serde_json::json!({"environment": environment, "plan": plan,
            "checksum": checksum, "allow": ["rename"], "staged": false, "resume": false});
        let (status, answer) = viewer.send("POST", "/api/trigger/apply", Some(&body.to_string()));
        assert_eq!(status, 200, "{answer}");
    }
    let (status, _) = viewer.send(
        "POST",
        "/api/trigger/apply",
        Some(r#"{"environment":"five","plan":"p.json","checksum":"c","allow":["DROP TABLE t"],"staged":false,"resume":false}"#),
    );
    assert_eq!(status, 400);

    // Wait for every triggered child to end.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let (_, body) = viewer.send("POST", "/api/trigger/runs", Some("{}"));
        let runs: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        if runs.len() == 5 && runs.iter().all(|run| run["ended"] == true) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{body}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Every child's arguments: the fixed prefix, one command of the route
    // vocabulary, and options whose values carry SQL only as themselves.
    let records: Vec<String> = std::fs::read_dir(&log)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    let invocations: Vec<Vec<&str>> = records
        .iter()
        .map(|record| record.trim_end_matches('\u{1f}').split('\u{1f}').collect())
        .collect();
    // Three parameterized reads, two plans and three applies; every refused
    // request started nothing.
    assert_eq!(invocations.len(), 8, "{invocations:?}");
    for arguments in &invocations {
        assert_eq!(
            &arguments[..3],
            ["--project", ".", "--no-input"],
            "{arguments:?}"
        );
        let command = arguments[3];
        assert!(
            matches!(
                command,
                "status" | "verify" | "explain" | "state" | "docs" | "plan" | "apply"
            ),
            "{arguments:?}"
        );
        for argument in &arguments[4..] {
            let fixed = matches!(
                *argument,
                "list" | "--staged" | "--resume" | "--format=json" | "--format=html"
            );
            let option = argument.split_once('=').is_some_and(|(flag, _)| {
                matches!(
                    flag,
                    "--env" | "--plan" | "--out" | "--checksum" | "--allow"
                )
            });
            assert!(fixed || option, "{argument:?} in {arguments:?}");
            assert!(
                !argument.contains(SQL) || argument.split_once('=').unwrap().1 == SQL,
                "{argument:?}"
            );
        }
        assert!(!arguments.contains(&"--db"), "{arguments:?}");
    }

    // Nothing the viewer wrote holds the approved checksum: the run list is
    // memory, and a failed plan's claim was given back.
    let mut written = Vec::new();
    files(&project, &[], &mut written);
    for (path, bytes) in &written {
        assert!(
            !String::from_utf8_lossy(bytes).contains(APPROVAL),
            "{} holds the approval",
            path.display()
        );
    }
    assert!(written.is_empty(), "the viewer left {written:?}");
    let _ = std::fs::remove_dir_all(&directory);
}
