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
        let directory = std::env::temp_dir().join(format!("pbps-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("schema")).unwrap();
        std::fs::write(
            directory.join("pbps.yml"),
            "dialect: mssql\nenvironments:\n  unconfigured:\n    url_env: PBPS_UI_TEST_UNSET\n",
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
fn the_launch_banner_discloses_whether_the_viewer_can_write() {
    let viewer = Viewer::start("banner");
    // Compose commits and pushes on Linux (#494); the banner must not call
    // that viewer read-only, and must say so where compose is refused.
    if cfg!(target_os = "linux") {
        assert!(!viewer.banner.contains("Read-only"), "{}", viewer.banner);
        assert!(
            viewer.banner.contains("commit a reviewed change"),
            "{}",
            viewer.banner
        );
    } else {
        assert!(viewer.banner.contains("Read-only"), "{}", viewer.banner);
    }
}
