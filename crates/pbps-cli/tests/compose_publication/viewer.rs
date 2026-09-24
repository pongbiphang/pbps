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
    // The reviewed base travels with the result, so a merge request can
    // target it; a restarted viewer rediscovers it from the receipt.
    assert_eq!(delivered["details"]["remote_base_ref"], "refs/heads/master");
    for viewer in [&served, &serve(&f)] {
        let listed = viewer.action("list", serde_json::json!({})).json();
        let saved = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|outcome| outcome["operation_id"] == operation)
            .expect("the delivered result is listed");
        assert_eq!(saved["details"]["remote_base_ref"], "refs/heads/master");
    }
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
    // Once confirmed, a busy publisher can no longer mean "not attempted":
    // the receipt exists, so the answer stays unavailable, never refused.
    let other = f.publisher();
    let busy = confirm();
    assert_eq!(busy.status, 409, "{}", busy.body);
    assert!(!busy.body.contains("not_attempted"), "{}", busy.body);
    drop(other);
    assert_eq!(confirm().json()["status"], "delivered");
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
    // Released and retired in the same request: nothing is left pending.
    assert_eq!(refused["cleanup_pending"], false, "{refused}");
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

#[test]
fn a_failed_stale_release_is_retried_by_the_next_preview() {
    let f = Fixture::new("viewer-compose-stale-retry");
    let served = serve(&f);
    let first = served.action("preview", intent()).json();
    let operation = first["operation_id"].as_str().unwrap();
    // An entry nobody owns blocks the snapshot's retirement (#747).
    let snapshot = f
        .repo
        .root
        .join(format!(".git/pbps-compose-v2/snapshots/{operation}"));
    let foreign = snapshot.join("foreign.lock");
    fs::write(&foreign, "another owner").unwrap();
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
    assert_eq!(refused["problem"], "remote_base_changed", "{refused}");
    assert_eq!(refused["cleanup_pending"], true);
    // The page reopens the form; the preview itself retries the release and
    // says why it cannot proceed while the foreign entry remains.
    let blocked = served.action("preview", intent());
    assert_eq!(blocked.status, 409, "{}", blocked.body);
    assert!(blocked.body.contains("still retiring"), "{}", blocked.body);
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "another owner");
    fs::remove_file(&foreign).unwrap();
    git(&f.remote, &["update-ref", "refs/heads/master", old]);
    let second = served.action("preview", intent()).json();
    assert_ne!(second["operation_id"], first["operation_id"]);
    let released = f.publisher().resource_reports().unwrap();
    assert_eq!(
        released
            .iter()
            .find(|report| report.operation_id == operation)
            .unwrap()
            .state,
        ResourceState::Spent
    );
    let delivered = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": second["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
}

#[test]
fn a_receiptless_confirmed_handle_stays_definite_while_the_publisher_is_busy() {
    let f = Fixture::new("viewer-compose-receiptless");
    let served = serve(&f);
    let preview = served.action("preview", intent()).json();
    let confirm = || {
        served.action(
            "confirm",
            serde_json::json!({"candidate_id": preview["candidate_id"]}),
        )
    };
    // A transient prerequisite fails after review: the handle becomes
    // confirmed, yet nothing was published and no receipt exists.
    git(&f.repo.root, &["config", "commit.gpgSign", "not-a-boolean"]);
    let first = confirm().json();
    assert_eq!(first["status"], "refused", "{first}");
    assert_eq!(first["problem"], "signing_unavailable");
    // Retrying while another compose holds the lock is still a definite
    // refusal the page may retry, not an unknown outcome.
    let other = f.publisher();
    let busy = confirm().json();
    assert_eq!(busy["status"], "refused", "{busy}");
    assert_eq!(busy["remote"], "not_attempted");
    drop(other);
    git(&f.repo.root, &["config", "--unset", "commit.gpgSign"]);
    assert_eq!(confirm().json()["status"], "delivered");
}

#[test]
fn retirement_actions_clean_up_and_forget_only_what_their_state_allows() {
    let f = Fixture::new("viewer-compose-retire");
    let served = serve(&f);
    let preview = served.action("preview", intent()).json();
    let operation = preview["operation_id"].as_str().unwrap().to_owned();
    let delivered = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": preview["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    assert_eq!(delivered["cleanup_pending"], true);
    let store = f.repo.root.join(".git/pbps-compose-v2");
    let snapshot = store.join(format!("snapshots/{operation}"));
    let commit_root = format!("refs/pbps-compose/{operation}/commit");
    let state = |id: &str| {
        let reports = served.action("resources", serde_json::json!({})).json();
        reports
            .as_array()
            .unwrap()
            .iter()
            .find(|report| report["operation_id"] == id)
            .map(|report| report["state"].clone())
    };
    assert!(snapshot.exists());
    assert_eq!(state(&operation), Some("confirmed".into()));
    // Cleanup retires the snapshot and keeps the root the receipt needs.
    let cleaned = served
        .action("cleanup", serde_json::json!({"operation_id": operation}))
        .json();
    assert_eq!(cleaned["status"], "delivered", "{cleaned}");
    assert_eq!(cleaned["cleanup_pending"], false);
    assert!(!snapshot.exists());
    git(&f.repo.root, &["rev-parse", "--verify", &commit_root]);
    assert_eq!(state(&operation), Some("retained".into()));
    // Forgetting needs its explicit acknowledgement.
    for body in [
        serde_json::json!({"operation_id": operation, "acknowledged": false}),
        serde_json::json!({"operation_id": operation}),
    ] {
        assert_eq!(served.action("forget", body).status, 400);
    }
    assert!(f.record(&operation).exists());
    let forgotten = served
        .action(
            "forget",
            serde_json::json!({"operation_id": operation, "acknowledged": true}),
        )
        .json();
    assert_eq!(forgotten["state"], "spent", "{forgotten}");
    assert!(!f.record(&operation).exists());
    assert!(
        git(&f.repo.root, &["for-each-ref", &commit_root]).is_empty(),
        "the commit root survived forget"
    );
    // The pushed branch is never a cleanup target.
    assert_eq!(
        f.remote_ref(preview["output_ref"].as_str().unwrap())
            .as_deref(),
        delivered["details"]["commit"].as_str()
    );
    // An unconfirmed preview has no receipt: cleanup and forget refuse and
    // keep it, and resource recovery before expiry leaves it sealed.
    let sealed = served.action("preview", intent()).json();
    let pending = sealed["operation_id"].as_str().unwrap().to_owned();
    let kept = store.join(format!("snapshots/{pending}"));
    let refused = served
        .action("cleanup", serde_json::json!({"operation_id": pending}))
        .json();
    assert_eq!(refused["problem"], "receipt_unavailable", "{refused}");
    let not_forgotten = served.action(
        "forget",
        serde_json::json!({"operation_id": pending, "acknowledged": true}),
    );
    assert_eq!(not_forgotten.status, 409, "{}", not_forgotten.body);
    let recovered = served
        .action(
            "recover-resources",
            serde_json::json!({"operation_id": pending}),
        )
        .json();
    assert_eq!(recovered["state"], "sealed");
    assert!(kept.exists());
    assert_eq!(state(&pending), Some("sealed".into()));
}

#[test]
fn a_restarted_viewer_gates_new_previews_on_the_same_bases_receipts() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("viewer-compose-restart");
    // The destination rejects the push, so the result stays unresolved.
    let hook = f.remote.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\ncat >/dev/null\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let first = serve(&f);
    let preview = first.action("preview", intent()).json();
    let operation = preview["operation_id"].as_str().unwrap().to_owned();
    let uncertain = first
        .action(
            "confirm",
            serde_json::json!({"candidate_id": preview["candidate_id"]}),
        )
        .json();
    assert_ne!(uncertain["status"], "delivered", "{uncertain}");
    assert!(f.record(&operation).exists());
    // A restarted viewer has no memory of that workflow; the receipt does.
    let second = serve(&f);
    let refused = second.action("preview", intent());
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(
        refused.body.contains(&operation) && refused.body.contains("unresolved"),
        "{}",
        refused.body
    );
    // The refused preview is withdrawn, not left sealed.
    let sealed = f
        .publisher()
        .resource_reports()
        .unwrap()
        .into_iter()
        .filter(|report| report.state == ResourceState::Sealed)
        .count();
    assert_eq!(sealed, 0, "a refused preview kept its private snapshot");
    // An alternative beside an unresolved result is refused up front, not
    // accepted and then contradicted by the preview gate.
    let early = second.action(
        "alternative",
        serde_json::json!({"operation_id": operation}),
    );
    assert_eq!(early.status, 409, "{}", early.body);
    assert!(early.body.contains("not delivered"), "{}", early.body);
    assert_eq!(second.action("preview", intent()).status, 409);
    // Resolve it: the destination accepts, and the result is republished.
    fs::remove_file(&hook).unwrap();
    let diagnosed = second
        .action("recover", serde_json::json!({"operation_id": operation}))
        .json();
    let generation = diagnosed["details"]["delivery_generation"].clone();
    let delivered = second
        .action(
            "republish",
            serde_json::json!({"operation_id": operation, "generation": generation}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    // Delivered from this base: only an explicit alternative starts another.
    let sibling = second.action("preview", intent());
    assert_eq!(sibling.status, 409, "{}", sibling.body);
    assert!(
        sibling.body.contains("start an alternative"),
        "{}",
        sibling.body
    );
    second
        .action(
            "alternative",
            serde_json::json!({"operation_id": operation}),
        )
        .json();
    let alternative = second.action("preview", intent()).json();
    assert_ne!(alternative["operation_id"], preview["operation_id"]);
    // A different base is never blocked by another base's receipts.
    let third = serve(&f);
    fs::write(f.repo.root.join("unrelated"), "moved on").unwrap();
    git(&f.repo.root, &["add", "unrelated"]);
    git(&f.repo.root, &["commit", "-qm", "source moves on"]);
    let moved = third.action("preview", intent()).json();
    assert_ne!(moved["base"], preview["base"]);
}

#[test]
fn another_projects_receipt_never_blocks_a_preview_from_the_same_base() {
    let f = Fixture::new("viewer-compose-two-projects");
    // A second pbps project in the same checkout, committed and published
    // so both projects share one base.
    let other = f.repo.root.join("other");
    fs::create_dir_all(other.join("schema")).unwrap();
    for name in ["pbps.yml", "schema.ids.json", "schema/t.yml"] {
        fs::copy(f.repo.project.join(name), other.join(name)).unwrap();
    }
    git(&f.repo.root, &["add", "other"]);
    git(&f.repo.root, &["commit", "-qm", "second project"]);
    git(&f.repo.root, &["push", "-q", "origin", "master"]);
    let first = serve(&f);
    let preview = first.action("preview", intent()).json();
    let delivered = first
        .action(
            "confirm",
            serde_json::json!({"candidate_id": preview["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    let second = Fixture {
        repo: Repository {
            project: other.clone(),
            root: f.repo.root.clone(),
        },
        remote: f.remote.clone(),
    };
    let viewer = serve(&second);
    let admitted = viewer.action("preview", intent()).json();
    assert_eq!(admitted["base"], preview["base"]);
    // The same project is still gated.
    let sibling = serve(&f).action("preview", intent());
    assert_eq!(sibling.status, 409, "{}", sibling.body);
    // `second` shares `f`'s checkout and remote; `f` alone cleans them up.
    std::mem::forget(second);
}

#[test]
fn a_replaced_handle_is_a_definite_refusal_not_an_unknown_outcome() {
    let f = Fixture::new("viewer-compose-replaced");
    let served = serve(&f);
    let first = served.action("preview", intent()).json();
    // A second preview (another tab, or Refresh) replaces the first handle.
    let second = served.action("preview", intent()).json();
    let stale = served.action(
        "confirm",
        serde_json::json!({"candidate_id": first["candidate_id"]}),
    );
    // Refused before publication: 410, never the uncertain 409.
    assert_eq!(stale.status, 410, "{}", stale.body);
    assert!(stale.body.contains("replaced"), "{}", stale.body);
    assert!(!f.record(first["operation_id"].as_str().unwrap()).exists());
    let delivered = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": second["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
}

#[test]
fn a_replaced_handle_stays_definite_while_the_publisher_is_busy() {
    let f = Fixture::new("viewer-compose-replaced-busy");
    let served = serve(&f);
    let first = served.action("preview", intent()).json();
    // Another tab's preview replaces the first handle, unpublished.
    let second = served.action("preview", intent()).json();
    // Another compose holds the owner lock: the replaced handle is still
    // known to have no receipt, so the answer stays definite, not 409.
    let other = f.publisher();
    let stale = served.action(
        "confirm",
        serde_json::json!({"candidate_id": first["candidate_id"]}),
    );
    assert_eq!(stale.status, 410, "{}", stale.body);
    assert!(stale.body.contains("replaced"), "{}", stale.body);
    assert!(!f.record(first["operation_id"].as_str().unwrap()).exists());
    // The current handle keeps the busy-publisher answer: not attempted,
    // and still confirmable.
    let busy = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": second["candidate_id"]}),
        )
        .json();
    assert_eq!(busy["status"], "refused", "{busy}");
    assert_eq!(busy["problem"], "repository_unavailable");
    drop(other);
    let delivered = served
        .action(
            "confirm",
            serde_json::json!({"candidate_id": second["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
}

#[test]
fn a_sibling_delivered_after_the_preview_refuses_its_confirmation() {
    let f = Fixture::new("viewer-compose-confirm-recheck");
    // Two viewers preview the same base before either publishes, so both
    // pass the preview gate.
    let first = serve(&f);
    let second = serve(&f);
    let a = first.action("preview", intent()).json();
    let b = second.action("preview", intent()).json();
    assert_eq!(a["base"], b["base"]);
    let delivered = first
        .action(
            "confirm",
            serde_json::json!({"candidate_id": a["candidate_id"]}),
        )
        .json();
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    // Under the owner lock the gate is rerun: a definite refusal naming the
    // delivered sibling, and nothing published for the refused handle.
    let refused = second.action(
        "confirm",
        serde_json::json!({"candidate_id": b["candidate_id"]}),
    );
    assert_eq!(refused.status, 410, "{}", refused.body);
    let sibling = a["operation_id"].as_str().unwrap();
    assert!(
        refused.body.contains(sibling) && refused.body.contains("start an alternative"),
        "{}",
        refused.body
    );
    assert!(!f.record(b["operation_id"].as_str().unwrap()).exists());
    // The refused preview is withdrawn, not left sealed, and its handle
    // stays refused.
    let sealed = f
        .publisher()
        .resource_reports()
        .unwrap()
        .into_iter()
        .filter(|report| report.state == ResourceState::Sealed)
        .count();
    assert_eq!(
        sealed, 0,
        "a refused confirmation kept its private snapshot"
    );
    let again = second.action(
        "confirm",
        serde_json::json!({"candidate_id": b["candidate_id"]}),
    );
    assert_eq!(again.status, 410, "{}", again.body);
    assert!(!f.record(b["operation_id"].as_str().unwrap()).exists());
    // An explicit alternative beside the delivered result proceeds.
    second
        .action("alternative", serde_json::json!({"operation_id": sibling}))
        .json();
    let alternative = second.action("preview", intent()).json();
    let published = second
        .action(
            "confirm",
            serde_json::json!({"candidate_id": alternative["candidate_id"]}),
        )
        .json();
    assert_eq!(published["status"], "delivered", "{published}");
}
