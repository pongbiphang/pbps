//! Exec hooks (SPEC §9.4): pbps runs a command and the command does the
//! talking.
//!
//! There is no Slack integration here, and there will not be one. An exec hook
//! holds no credentials of its own, outlives any chat API, and delivers through
//! whatever a team already runs. What pbps owes the hook is a stable typed
//! payload on stdin; where it goes from there is not this tool's business.
//!
//! # Why a failing hook does not fail the command
//!
//! `on_apply` runs after a transaction has committed, `on_apply_attempt` after
//! either apply outcome, and `on_drift` after the verdict is already decided —
//! in every case the fact being reported has happened, and a non-zero exit from
//! the notifier cannot un-happen it.
//! Returning failure here would report "the apply failed" about a database that
//! was changed successfully, which is the more dangerous lie. So the failure is
//! printed loudly and the command's own verdict stands.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Stable input for `on_apply_attempt`. Unlike the legacy `on_apply` plan JSON,
/// this reports the event, so one hook sees both completed and failed attempts
/// and can fan the ledger out to append-only storage without guessing from
/// process state. The explicit version lets consumers reject a future contract
/// they do not understand rather than silently misreading it.
#[derive(serde::Serialize)]
struct ApplyAttemptPayload<'a> {
    version: u32,
    event: &'static str,
    plan_path: String,
    checksum: &'a str,
    outcome: &'static str,
    environment: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ledger_entry: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// Runs the typed `on_apply_attempt` hook for either apply outcome.
pub fn run_apply_attempt(
    command: &str,
    plan_path: &Path,
    checksum: &str,
    environment: &str,
    ledger_entry: Option<i64>,
    error: Option<&str>,
) {
    let payload = ApplyAttemptPayload {
        version: 1,
        event: "apply",
        plan_path: plan_path.display().to_string(),
        checksum,
        outcome: if error.is_some() {
            "failure"
        } else {
            "success"
        },
        environment,
        ledger_entry,
        error,
    };
    // This type has no fallible values or non-string map keys. Keep hook
    // failure semantics even if that ever changes: notification must not
    // replace the deployment result.
    match serde_json::to_string(&payload) {
        Ok(json) => run(command, &json, "on_apply_attempt"),
        Err(e) => eprintln!("warning: the on_apply_attempt payload could not be serialized: {e}"),
    }
}

/// Runs `command` through the platform shell with `payload` on stdin.
///
/// The shell is what makes the hook a one-liner in `pbps.yml` — pipes,
/// redirection and `$VAR` all work, and the alternative would be a config
/// format that reinvents argv.
pub fn run(command: &str, payload: &str, what: &str) {
    let mut shell = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };

    let child = shell.stdin(Stdio::piped()).env("PBPS_HOOK", what).spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: the {what} hook could not be started (`{command}`): {e}");
            return;
        }
    };

    // A hook that ignores stdin closes it and the write fails with EPIPE. That
    // is the hook's prerogative — `on_drift: notify-send "drift"` never reads
    // the payload — so a broken pipe is not worth a warning of its own; the
    // exit status below is the verdict that matters.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }

    match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!(
            "warning: the {what} hook exited with {status} (`{command}`); \
             the {what} itself was not affected"
        ),
        Err(e) => eprintln!("warning: the {what} hook could not be waited for: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_payload_names_the_artifact_pin_and_outcome() {
        let payload = ApplyAttemptPayload {
            version: 1,
            event: "apply",
            plan_path: "release/plan.json".into(),
            checksum: "abc123",
            outcome: "failure",
            environment: "prod",
            ledger_entry: None,
            error: Some("permission denied"),
        };
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["plan_path"], "release/plan.json");
        assert_eq!(json["checksum"], "abc123");
        assert_eq!(json["outcome"], "failure");
        assert_eq!(json["error"], "permission denied");
        assert!(json.get("ledger_entry").is_none());
    }
}
