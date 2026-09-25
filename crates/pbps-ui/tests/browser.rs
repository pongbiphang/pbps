//! Execute the shipped JavaScript, including asynchronous DOM event handling.
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn compose_form_keeps_confirmation_bound_to_the_latest_reviewed_generation() {
    let node = std::env::var_os("PBPS_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(
            "Node.js is required for the shipped browser behavior tests (or set PBPS_TEST_NODE)",
        );
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(include_bytes!("../assets/compose.js"))
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin
        .write_all(include_bytes!("compose-browser.cjs"))
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("compose browser behavior passed"));
}

/// Ledger identifiers reach the page as the CLI wrote them, every digit (#492).
///
/// The bytes are the viewer's own: CLI envelopes run through
/// `contract::parse`, as the server does, then served to the shipped app.js,
/// which reads them with `response.json()`. `9007199254740993` is the first
/// integer a JavaScript number cannot hold; sent as a JSON number it renders as
/// `9007199254740992`. Controls: the last safe integer, a small id, a negative
/// one where the type permits it, and an absent `last_entry`.
#[test]
fn ledger_identifiers_render_with_every_digit() {
    use serde_json::json;
    const IDS: [i64; 6] = [
        9_007_199_254_740_991,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        i64::MAX,
        7,
        -3,
    ];
    let envelope = |command: &str, data: serde_json::Value| {
        let cli = json!({"schema_version": 1, "tool_version": "0.0.0", "command": command,
            "result": "ok", "findings": [], "data": data});
        let served = pbps_ui::contract::parse(command, &serde_json::to_vec(&cli).unwrap(), 0)
            .expect("a valid envelope");
        String::from_utf8(served).unwrap()
    };
    let mut environments: Vec<_> = IDS
        .iter()
        .enumerate()
        .map(|(n, id)| {
            json!({"environment": format!("env{n}"), "state": "ok", "checked_at": "now",
                "last_entry": id})
        })
        .collect();
    environments.push(json!({"environment": "fresh", "state": "ok", "checked_at": "now"}));
    let entries: Vec<_> = IDS
        .iter()
        .map(|id| json!({"id": id, "applied_at": "now", "kind": "apply", "operator": "ci"}))
        .collect();
    let responses = json!({
        "status": envelope("status", json!(environments)),
        "timeline": envelope("state list", json!({"environment": "env0",
            "initialized": true, "limit": 20, "entries": entries})),
        "drift": envelope("verify", json!({"version": 1, "environment": "env0",
            "checked_at": "now", "baseline": {"entry_id": IDS[2], "applied_at": "now",
            "checksum": "c"}, "live_checksum": "c", "changes": []})),
    });
    let ids: Vec<String> = IDS.iter().map(i64::to_string).collect();
    let expected = json!({"status": ids, "timeline": ids, "drift": IDS[2].to_string()});

    let node = std::env::var_os("PBPS_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(
            "Node.js is required for the shipped browser behavior tests (or set PBPS_TEST_NODE)",
        );
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "const RESPONSES = {responses};\nconst EXPECTED = {expected};"
    )
    .unwrap();
    stdin.write_all(include_bytes!("app-browser.cjs")).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.write_all(include_bytes!("../assets/app.js")).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin
        .write_all(include_bytes!("app-browser-check.cjs"))
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("app browser behavior passed"));
}
