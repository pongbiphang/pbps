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
//! `on_apply` runs after a transaction has committed and `on_drift` after the
//! verdict is already decided — in both cases the fact being reported has
//! happened, and a non-zero exit from the notifier cannot un-happen it.
//! Returning failure here would report "the apply failed" about a database that
//! was changed successfully, which is the more dangerous lie. So the failure is
//! printed loudly and the command's own verdict stands.

use std::io::Write as _;
use std::process::{Command, Stdio};

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
