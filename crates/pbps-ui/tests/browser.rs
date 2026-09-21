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
