//! The real binary serves the shell and invokes its own read commands.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

struct Viewer {
    child: Child,
    directory: PathBuf,
    address: String,
    token: String,
    banner: String,
    // Kept open: a closed pipe would make the viewer's next eprintln panic.
    _stderr: BufReader<std::process::ChildStderr>,
}

impl Viewer {
    fn start(name: &str) -> Self {
        Self::start_with(name, "", &[])
    }

    /// `environments` is appended to the project's environment list, and
    /// `variables` is set in the viewer's (and so its children's) environment.
    fn start_with(name: &str, environments: &str, variables: &[(&str, &str)]) -> Self {
        let directory = std::env::temp_dir().join(format!("pbps-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("schema")).unwrap();
        std::fs::write(
            directory.join("pbps.yml"),
            format!(
                "dialect: mssql\nenvironments:\n  unconfigured:\n    url_env: PBPS_UI_TEST_UNSET\n{environments}"
            ),
        )
        .unwrap();
        std::fs::write(
            directory.join("schema/t.yml"),
            "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(directory.parent().unwrap())
            .arg("--project")
            .arg(directory.file_name().unwrap())
            .arg("ui")
            .env_remove("PBPS_UI_TEST_UNSET")
            .envs(variables.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let line = line
            .trim()
            .strip_prefix("http://")
            .expect("the UI prints its launch URL");
        let (address, token) = line.split_once("/#").unwrap();
        let mut banner = String::new();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        stderr.read_line(&mut banner).unwrap();
        assert!(address.starts_with("127.0.0.1:"));
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        Self {
            child,
            directory,
            address: address.into(),
            token: token.into(),
            banner,
            _stderr: stderr,
        }
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, String) {
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.address
        )
        .unwrap();
        for (name, value) in headers {
            write!(socket, "{name}: {value}\r\n").unwrap();
        }
        write!(socket, "\r\n").unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, head.into(), body.into())
    }

    fn read(&self, path: &str) -> (u16, String, String) {
        self.request("GET", path, &[("X-Pbps-Token", &self.token)])
    }

    /// A write as the page sends it: token, matching origin, JSON body.
    fn post(&self, path: &str, body: &str) -> (u16, String, String) {
        let origin = format!("http://{}", self.address);
        let length = body.len().to_string();
        self.request_with_body(
            "POST",
            path,
            &[
                ("X-Pbps-Token", &self.token),
                ("Origin", &origin),
                ("Content-Type", "application/json"),
                ("Content-Length", &length),
            ],
            body,
        )
    }

    fn request_with_body(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, String) {
        let mut socket = TcpStream::connect(&self.address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.address
        )
        .unwrap();
        for (name, value) in headers {
            write!(socket, "{name}: {value}\r\n").unwrap();
        }
        write!(socket, "\r\n{body}").unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, head.into(), body.into())
    }

    /// Asks for the runs until every one has ended, and returns every body
    /// the viewer sent on the way.
    fn wait_for_runs(&self) -> (Vec<serde_json::Value>, Vec<String>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut bodies = Vec::new();
        loop {
            let (status, _, body) = self.post("/api/trigger/runs", "{}");
            assert_eq!(status, 200, "{body}");
            let runs: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
            bodies.push(body);
            if runs.iter().all(|run| run["ended"] == true) {
                return (runs, bodies);
            }
            assert!(std::time::Instant::now() < deadline, "a run never ended");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn shell_reads_are_public_but_project_reads_require_the_launch_token() {
    let viewer = Viewer::start("shell");
    for path in ["/", "/app.js", "/compose.js", "/style.css"] {
        let (status, headers, body) = viewer.request("GET", path, &[]);
        assert_eq!(status, 200, "{path}: {body}");
        assert!(headers.to_lowercase().contains("cache-control: no-store"));
        assert!(headers.to_lowercase().contains("content-security-policy:"));
        assert!(!body.contains(&viewer.token));
        assert!(!body.contains("PBPS_UI_TEST_UNSET"));
    }
    for path in [
        "/api/status",
        "/api/drift?env=unconfigured",
        "/api/plan?path=absent.json",
        "/api/timeline?env=unconfigured",
        "/api/docs",
        "/api/apply",
    ] {
        assert_eq!(viewer.request("GET", path, &[]).0, 403, "{path}");
        assert_eq!(
            viewer.request("GET", path, &[("X-Pbps-Token", "wrong")]).0,
            403
        );
    }
    let (status, _, body) = viewer.read("/api/status");
    assert_eq!(status, 200, "{body}");
    let report: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["command"], "status");
    assert_eq!(report["data"][0]["environment"], "unconfigured");
    assert_eq!(report["data"][0]["state"], "unconfigured");
    assert!(!viewer.directory.join("schema.ids.json").exists());
    let (status, _, body) =
        viewer.request("HEAD", "/api/status", &[("X-Pbps-Token", &viewer.token)]);
    assert_eq!(status, 200);
    assert!(body.is_empty());
}

#[test]
fn request_guards_precede_routing_and_only_compose_actions_write() {
    let viewer = Viewer::start("guards");
    for path in ["/api/status", "/api/apply", "/"] {
        assert_eq!(
            viewer
                .request(
                    "POST",
                    path,
                    &[
                        ("Origin", "https://foreign.invalid"),
                        ("X-Pbps-Token", &viewer.token)
                    ]
                )
                .0,
            403
        );
        assert_eq!(
            viewer
                .request(
                    "GET",
                    path,
                    &[("Origin", "null"), ("X-Pbps-Token", &viewer.token)]
                )
                .0,
            403
        );
    }
    assert_eq!(
        viewer
            .request("POST", "/api/status", &[("X-Pbps-Token", &viewer.token)])
            .0,
        403
    );
    let origin = format!("http://{}", viewer.address);
    assert_eq!(
        viewer
            .request(
                "POST",
                "/api/apply",
                &[("Origin", &origin), ("X-Pbps-Token", &viewer.token)]
            )
            .0,
        405
    );
    assert_eq!(viewer.read("/api/apply").0, 400);
    assert_eq!(viewer.read("/api/status?db=connection").0, 400);
    assert_eq!(viewer.read("/api/docs?out=changed.html").0, 400);
    assert_eq!(
        viewer
            .request(
                "GET",
                "/api/status",
                &[
                    ("X-Pbps-Token", &viewer.token),
                    ("x-pbps-token", &viewer.token)
                ]
            )
            .0,
        403
    );
    assert_eq!(
        viewer
            .request(
                "GET",
                "/api/status",
                &[("Host", "rebind.invalid"), ("X-Pbps-Token", &viewer.token)]
            )
            .0,
        403
    );
    assert!(!viewer.directory.join("changed.html").exists());
}

#[test]
fn child_failures_remain_typed_answers_and_docs_use_the_exact_csp_stylesheet() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let viewer = Viewer::start("responses");
    for (path, command) in [
        ("/api/drift?env=unconfigured", "verify"),
        ("/api/timeline?env=unconfigured", "state list"),
        ("/api/plan?path=missing%20plan.json", "explain"),
    ] {
        let (status, _, body) = viewer.read(path);
        assert_eq!(status, 200, "{body}");
        let report: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(report["command"], command);
        assert_eq!(report["result"], "unanswerable");
        assert!(!report["findings"].as_array().unwrap().is_empty());
        assert!(report.get("data").is_none());
    }
    let (status, _, html) = viewer.read("/api/docs");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("dbo.t"));
    assert!(!html.contains("<script"));
    let style = html
        .split_once("<style>")
        .unwrap()
        .1
        .split_once("</style>")
        .unwrap()
        .0;
    assert_eq!(style, pbps_docs::html::style_contents());
    let expected =
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(style.as_bytes()));
    let (_, headers, _) = viewer.request("GET", "/", &[]);
    assert!(headers.contains(&format!("'sha256-{expected}'")));
    assert!(!headers.contains("unsafe-inline"));
    let before = std::fs::read(viewer.directory.join("schema/t.yml")).unwrap();
    assert!(!before.is_empty());
    assert!(!viewer.directory.join("schema.ids.json").exists());
}

#[test]
fn a_relative_project_does_not_select_a_same_named_nested_project() {
    let viewer = Viewer::start("relative-project");
    let decoy = viewer.directory.join(viewer.directory.file_name().unwrap());
    std::fs::create_dir_all(decoy.join("schema")).unwrap();
    std::fs::write(
        decoy.join("pbps.yml"),
        "dialect: mssql\nenvironments:\n  decoy:\n    url_env: PBPS_UI_TEST_UNSET\n",
    )
    .unwrap();
    let (status, _, body) = viewer.read("/api/status");
    assert_eq!(status, 200, "{body}");
    let report: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["data"][0]["environment"], "unconfigured");
    assert_ne!(report["data"][0]["environment"], "decoy");
}

#[test]
fn the_ui_has_no_host_or_write_option() {
    for args in [["ui", "--host", "0.0.0.0"], ["ui", "--sql", "SELECT 1"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
}

#[test]
fn a_trigger_request_failing_a_guard_starts_nothing() {
    let viewer = Viewer::start("trigger-guards");
    let body = r#"{"environment":"unconfigured","out":"guarded.json"}"#;
    let origin = format!("http://{}", viewer.address);
    let length = body.len().to_string();
    for headers in [
        // No token.
        vec![
            ("Origin", origin.as_str()),
            ("Content-Type", "application/json"),
            ("Content-Length", length.as_str()),
        ],
        // A foreign origin.
        vec![
            ("X-Pbps-Token", viewer.token.as_str()),
            ("Origin", "https://foreign.invalid"),
            ("Content-Type", "application/json"),
            ("Content-Length", length.as_str()),
        ],
        // No origin on a write.
        vec![
            ("X-Pbps-Token", viewer.token.as_str()),
            ("Content-Type", "application/json"),
            ("Content-Length", length.as_str()),
        ],
    ] {
        let (status, _, answer) =
            viewer.request_with_body("POST", "/api/trigger/plan", &headers, body);
        assert_eq!(status, 403, "{answer}");
    }
    // Not JSON, and not a POST.
    let (status, _, _) = viewer.request_with_body(
        "POST",
        "/api/trigger/plan",
        &[
            ("X-Pbps-Token", viewer.token.as_str()),
            ("Origin", origin.as_str()),
            ("Content-Type", "text/plain"),
            ("Content-Length", length.as_str()),
        ],
        body,
    );
    assert_eq!(status, 415);
    assert_eq!(viewer.read("/api/trigger/plan").0, 405);
    let (status, _, runs) = viewer.post("/api/trigger/runs", "{}");
    assert_eq!((status, runs.as_str()), (200, "[]"));
    assert!(!viewer.directory.join("guarded.json").exists());
}

/// ADR-0015 decision 4 across the viewer (#1025, #1050): the connection
/// string is read by the child from its environment. It appears in no
/// response, served asset or file the session leaves, including when the child
/// fails to connect and reports why. The approved checksum typed into an
/// apply is not stored either (ADR-0006).
#[test]
fn a_connection_string_never_reaches_a_response_or_a_file() {
    const SECRET: &str = "Sup3rS3cretPbpsUiPassword";
    let connection = format!(
        "Server=127.0.0.1,1;Database=d;User Id=u;Password={SECRET};TrustServerCertificate=true;Connect Timeout=2"
    );
    let viewer = Viewer::start_with(
        "trigger-secret",
        "  guarded:\n    url_env: PBPS_UI_TEST_SECRET\n",
        &[("PBPS_UI_TEST_SECRET", &connection)],
    );
    // A committed project with a current identity file, so the connected plan
    // gets as far as connecting, and fails there.
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(&viewer.directory)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "declarations"]);
    let offline = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&viewer.directory)
        .args(["--no-input", "plan"])
        .output()
        .unwrap();
    assert!(offline.status.success(), "{offline:?}");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "identities"]);

    let (status, _, started) = viewer.post(
        "/api/trigger/plan",
        r#"{"environment":"guarded","out":"new.json"}"#,
    );
    assert_eq!(status, 200, "{started}");
    let (runs, bodies) = viewer.wait_for_runs();
    let plan = &runs[0];
    assert_eq!(plan["action"], "plan");
    assert_ne!(plan["code"], 0, "{plan}");
    assert!(
        plan["stderr"]
            .as_str()
            .unwrap()
            .contains("cannot connect to the database"),
        "the child reached the connection and its refusal is relayed: {plan}"
    );
    // The failed plan wrote nothing, and its claim on the file was released.
    assert!(!viewer.directory.join("new.json").exists());

    // The checksum is the CLI's to judge; its refusal reaches the page as the
    // CLI wrote it.
    std::fs::write(viewer.directory.join("saved.json"), "{}").unwrap();
    let (status, _, started) = viewer.post(
        "/api/trigger/apply",
        r#"{"environment":"guarded","plan":"saved.json","checksum":"0000","allow":[],"staged":false,"resume":false}"#,
    );
    assert_eq!(status, 200, "{started}");
    let (runs, more) = viewer.wait_for_runs();
    assert_eq!(runs[0]["action"], "apply");
    assert_eq!(runs[0]["code"], 2);
    assert!(
        runs[0]["stderr"]
            .as_str()
            .unwrap()
            .contains("a plan checksum must be exactly 64 hexadecimal characters"),
        "{}",
        runs[0]
    );
    // The reads that connect, and a compose preview, answered with the same
    // environment in place.
    let mut answers = Vec::new();
    for path in [
        "/api/status",
        "/api/drift?env=guarded",
        "/api/timeline?env=guarded",
        "/api/plan?path=saved.json",
    ] {
        let (status, _, body) = viewer.read(path);
        assert_eq!(status, 200, "{path}: {body}");
        answers.push(body);
    }
    answers.push(
        viewer
            .post(
                "/api/compose/preview",
                r#"{"intent":{"kind":"annotation"},"message":"m","remote":"origin","remote_base_ref":"refs/heads/master"}"#,
            )
            .2,
    );
    // A well-formed approval, typed into an apply the CLI refuses.
    const APPROVAL: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";
    viewer.post(
        "/api/trigger/apply",
        &format!(
            r#"{{"environment":"guarded","plan":"saved.json","checksum":"{APPROVAL}","allow":["rename"],"staged":false,"resume":false}}"#
        ),
    );
    let (_, last) = viewer.wait_for_runs();
    for body in bodies
        .iter()
        .chain(&more)
        .chain(&last)
        .chain([&started])
        .chain(&answers)
    {
        assert!(!body.contains(SECRET), "{body}");
        assert!(!body.contains("127.0.0.1,1"), "{body}");
    }
    for asset in ["/", "/app.js", "/compose.js", "/trigger.js", "/style.css"] {
        assert!(!viewer.request("GET", asset, &[]).2.contains(SECRET));
    }

    // Every file the session left, git's compressed objects included.
    fn walk(path: &std::path::Path, found: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                // A directory is state too: its name is searched below.
                found.push((path.clone(), Vec::new()));
                walk(&path, found);
            } else {
                // Unreadable is not empty: it could hold what is searched for.
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|e| panic!("{} could not be read: {e}", path.display()));
                found.push((path.clone(), bytes));
            }
        }
    }
    let mut files = Vec::new();
    walk(&viewer.directory, &mut files);
    let objects = Command::new("git")
        .current_dir(&viewer.directory)
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .output()
        .unwrap();
    assert!(objects.status.success());
    assert!(
        !objects.stdout.is_empty(),
        "the project's objects were read"
    );
    files.push(("git objects".into(), objects.stdout));
    for (path, bytes) in &files {
        // A name can hold what a file does not: search both.
        let text = format!("{}\n{}", path.display(), String::from_utf8_lossy(bytes));
        assert!(
            !text.contains(SECRET),
            "{} holds the password",
            path.display()
        );
        assert!(
            !text.contains(APPROVAL),
            "{} holds the approval",
            path.display()
        );
    }
}

#[test]
fn the_launch_banner_discloses_whether_the_viewer_can_write() {
    let viewer = Viewer::start("banner");
    // The viewer can run plan and apply everywhere (#1025), and compose
    // commits and pushes on Linux (#494). The banner says both, and never
    // calls the viewer read-only.
    assert!(!viewer.banner.contains("Read-only"), "{}", viewer.banner);
    assert!(
        viewer.banner.contains("plan and apply"),
        "{}",
        viewer.banner
    );
    assert!(
        viewer.banner.contains("interrupts a running plan or apply"),
        "{}",
        viewer.banner
    );
    if cfg!(target_os = "linux") {
        assert!(
            viewer.banner.contains("commit a reviewed change"),
            "{}",
            viewer.banner
        );
    } else {
        assert!(
            viewer.banner.contains("compose requires Linux"),
            "{}",
            viewer.banner
        );
        assert_eq!(
            viewer.banner.contains("inside WSL"),
            cfg!(windows),
            "{}",
            viewer.banner
        );
    }
}
