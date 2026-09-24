//! The local viewer's compose actions end to end over real HTTP (#494): the
//! same loopback, Host, Origin and token gate as every read, and nothing
//! outside the fixed actions can write.

use super::*;
use pbps_ui::compose::ResourceState;
use std::io::Read;
use std::net::TcpStream;

struct Served {
    address: String,
    token: String,
}

fn serve(f: &Fixture) -> Served {
    let viewer = pbps_ui::Viewer::bind(pbps_ui::Config {
        executable: BIN.into(),
        project: f.repo.project.clone(),
        token: [7; 32],
        docs_style_hash: format!("{}=", "A".repeat(43)),
    })
    .unwrap();
    let url = viewer.url();
    let (address, token) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once("/#")
        .unwrap();
    let served = Served {
        address: address.to_owned(),
        token: token.to_owned(),
    };
    // The viewer serves until the test process exits.
    std::thread::spawn(move || viewer.serve());
    served
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        assert_eq!(self.status, 200, "{}", self.body);
        serde_json::from_str(&self.body).unwrap()
    }
}

impl Served {
    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(180)))
            .unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.address);
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ));
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let text = String::from_utf8(response).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        Reply {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            body: body.to_owned(),
        }
    }

    /// What the shipped page sends: POST, its own origin, the token, JSON.
    fn action(&self, action: &str, body: serde_json::Value) -> Reply {
        let origin = format!("http://{}", self.address);
        self.send(
            "POST",
            &format!("/api/compose/{action}"),
            &[
                ("Origin", &origin),
                ("X-Pbps-Token", &self.token),
                ("Content-Type", "application/json"),
            ],
            body.to_string().as_bytes(),
        )
    }
}

fn intent() -> serde_json::Value {
    serde_json::json!({
        "intent": {"kind": "rename", "from": "dbo.t.id", "to": "ident"},
        "message": "Rename the identifier",
        "remote": "origin",
        "remote_base_ref": "refs/heads/master",
    })
}

#[test]
fn the_viewer_previews_confirms_and_reconciles_through_its_compose_actions() {
    let f = Fixture::new("viewer-compose");
    let before = f.repo.preserved();
    let served = serve(&f);
    let preview = served.action("preview", intent()).json();
    assert!(preview["diff"].as_str().unwrap().contains("ident"));
    let candidate = preview["candidate_id"].as_str().unwrap();
    let operation = preview["operation_id"].as_str().unwrap();
    // The browser holds only the opaque handle; the server owns the rest.
    let delivered = served
        .action("confirm", serde_json::json!({"candidate_id": candidate}))
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    assert_eq!(delivered["remote"], "delivered");
    let commit = delivered["details"]["commit"].as_str().unwrap();
    let output_ref = preview["output_ref"].as_str().unwrap();
    assert_eq!(f.remote_ref(output_ref).as_deref(), Some(commit));
    let listed = served.action("list", serde_json::json!({})).json();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|outcome| outcome["operation_id"] == operation)
    );
    let recovered = served
        .action("recover", serde_json::json!({"operation_id": operation}))
        .json();
    assert_eq!(recovered["status"], "delivered");
    assert_eq!(recovered["details"]["commit"], commit);
    // A second confirmation of the same handle returns the recorded result.
    let again = served
        .action("confirm", serde_json::json!({"candidate_id": candidate}))
        .json();
    assert_eq!(again["details"]["commit"], commit);
    f.source_unchanged(&before);
}

#[test]
fn compose_actions_keep_the_viewer_gate_and_accept_nothing_else() {
    let f = Fixture::new("viewer-compose-gate");
    let before = f.repo.preserved();
    let served = serve(&f);
    let origin = format!("http://{}", served.address);
    let body = intent().to_string();
    let refused = |headers: &[(&str, &str)], method: &str, path: &str, body: &[u8]| {
        served.send(method, path, headers, body).status
    };
    let token = served.token.as_str();
    let json = ("Content-Type", "application/json");
    for (label, status, reply) in [
        (
            "no token",
            403,
            refused(
                &[("Origin", &origin), json],
                "POST",
                "/api/compose/preview",
                body.as_bytes(),
            ),
        ),
        (
            "foreign origin",
            403,
            refused(
                &[
                    ("Origin", "http://evil.test"),
                    ("X-Pbps-Token", token),
                    json,
                ],
                "POST",
                "/api/compose/preview",
                body.as_bytes(),
            ),
        ),
        (
            "no origin",
            403,
            refused(
                &[("X-Pbps-Token", token), json],
                "POST",
                "/api/compose/preview",
                body.as_bytes(),
            ),
        ),
        (
            "read method",
            405,
            refused(&[("X-Pbps-Token", token)], "GET", "/api/compose/list", b""),
        ),
        (
            "form post",
            415,
            refused(
                &[
                    ("Origin", &origin),
                    ("X-Pbps-Token", token),
                    ("Content-Type", "text/plain"),
                ],
                "POST",
                "/api/compose/preview",
                body.as_bytes(),
            ),
        ),
        (
            "unknown action",
            405,
            refused(
                &[("Origin", &origin), ("X-Pbps-Token", token), json],
                "POST",
                "/api/compose/apply",
                body.as_bytes(),
            ),
        ),
        (
            "unknown field",
            400,
            served
                .action("list", serde_json::json!({"sql": "DROP TABLE t"}))
                .status,
        ),
        (
            "oversized",
            413,
            served
                .action(
                    "preview",
                    serde_json::json!({"message": "x".repeat(70 * 1024)}),
                )
                .status,
        ),
        (
            "handle never previewed",
            409,
            served
                .action(
                    "confirm",
                    serde_json::json!({"candidate_id": "f".repeat(64)}),
                )
                .status,
        ),
    ] {
        assert_eq!(reply, status, "{label}");
    }
    // Nothing was captured, committed or pushed by any refused request.
    assert!(
        !f.repo.root.join(".git/pbps-compose-v2/resources").exists()
            || fs::read_dir(f.repo.root.join(".git/pbps-compose-v2/resources"))
                .unwrap()
                .next()
                .is_none()
    );
    assert!(
        git(&f.remote, &["for-each-ref", "refs/heads/pbps-compose/"]).is_empty(),
        "a refused request published a branch"
    );
    f.source_unchanged(&before);
}

#[test]
fn a_busy_publisher_refuses_confirmation_without_consuming_the_reviewed_handle() {
    let f = Fixture::new("viewer-compose-busy");
    let served = serve(&f);
    let preview = served.action("preview", intent()).json();
    let candidate = preview["candidate_id"].as_str().unwrap();
    let operation = preview["operation_id"].as_str().unwrap();
    let confirm = || served.action("confirm", serde_json::json!({"candidate_id": candidate}));
    // Another compose (a CLI or a second viewer) holds the owner lock.
    let other = f.publisher();
    let refused = confirm().json();
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(refused["operation_id"], operation);
    assert_eq!(refused["remote"], "not_attempted");
    assert_eq!(refused["problem"], "repository_unavailable");
    assert!(!f.record(operation).exists(), "a refusal wrote a receipt");
    drop(other);
    // The same frozen handle is still confirmable once the lock clears.
    let delivered = confirm().json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    assert_eq!(
        f.remote_ref(preview["output_ref"].as_str().unwrap())
            .as_deref(),
        delivered["details"]["commit"].as_str()
    );
}

#[test]
fn a_stale_base_refusal_releases_the_workflow_for_a_fresh_preview() {
    let f = Fixture::new("viewer-compose-stale");
    let served = serve(&f);
    let first = served.action("preview", intent()).json();
    let operation = first["operation_id"].as_str().unwrap();
    // The remote base branch advances after the preview was reviewed.
    let old = String::from_utf8(git(&f.remote, &["rev-parse", "refs/heads/master"])).unwrap();
    let old = old.trim();
    let advanced = String::from_utf8(git(
        &f.remote,
        &[
            "-c",
            "user.name=Remote",
            "-c",
            "user.email=remote@example.test",
            "commit-tree",
            &format!("{old}^{{tree}}"),
            "-p",
            old,
            "-m",
            "advance",
        ],
    ))
    .unwrap();
    git(
        &f.remote,
        &["update-ref", "refs/heads/master", advanced.trim()],
    );
    let refused = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": first["candidate_id"]}),
        )
        .json();
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(refused["problem"], "remote_base_changed");
    assert!(!f.record(operation).exists(), "a refusal wrote a receipt");
    // Reconfirming cannot cure a moved base; the workflow is released and
    // the refused operation's private resources are retired.
    let reports = f.publisher().resource_reports().unwrap();
    let released = reports
        .iter()
        .find(|report| report.operation_id == operation)
        .unwrap();
    assert_eq!(released.state, ResourceState::Spent);
    git(&f.remote, &["update-ref", "refs/heads/master", old]);
    let second = served.action("preview", intent()).json();
    assert_ne!(second["operation_id"], first["operation_id"]);
    let delivered = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": second["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
}
