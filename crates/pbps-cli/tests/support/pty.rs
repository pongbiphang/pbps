//! The Linux terminal fixture must run, or its caller has not passed.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const REQUIRED: &str =
    "PTY tests require util-linux script and GNU timeout; install util-linux and coreutils";

pub(super) fn run(command: &str, input: &[u8]) -> Output {
    run_with_limit(command, input, Duration::from_secs(60))
}

fn run_with_limit(command: &str, input: &[u8], limit: Duration) -> Output {
    let capture = TempDir::new();
    let stdout = capture.0.join("stdout");
    let stderr = capture.0.join("stderr");
    // Files cannot leave wait_with_output blocked on a descendant's inherited
    // pipe after the deadline. util-linux script forwards TERM to its shell;
    // allow its cleanup to finish before timeout's final KILL.
    let mut child = Command::new("timeout")
        .args([
            "--kill-after=5s",
            &format!("{}s", limit.as_secs_f64()),
            "script",
            "-qec",
            command,
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create_new(&stdout).expect("create PTY stdout capture"))
        .stderr(std::fs::File::create_new(&stderr).expect("create PTY stderr capture"))
        .spawn()
        .unwrap_or_else(|error| panic!("{REQUIRED}: {error}"));
    let mut stdin = child.stdin.take().expect("PTY stdin was piped");
    let written = stdin.write_all(input);
    drop(stdin);
    // Even a failed input write waits for the bounded fixture to reap its
    // child before reporting failure, rather than abandoning a terminal.
    let status = child.wait().expect("wait for the bounded PTY fixture");
    let out = Output {
        status,
        stdout: std::fs::read(stdout).expect("read PTY stdout capture"),
        stderr: std::fs::read(stderr).expect("read PTY stderr capture"),
    };
    let details = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !matches!(out.status.code(), Some(125..=127)),
        "{REQUIRED}: {}\n{details}",
        out.status
    );
    assert!(
        !matches!(out.status.code(), Some(124 | 137)),
        "PTY fixture timed out or was killed (limit {limit:?}): {details}"
    );
    assert!(written.is_ok(), "write PTY input: {written:?}\n{details}");
    assert!(
        matches!(out.status.code(), Some(0 | 2)),
        "PTY command failed: {}\n{details}",
        out.status
    );
    out
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pbps-pty-{}-{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir(&path).expect("create PTY fixture directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn executable(name: &str) -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|path| path.join(name))
            .find(|path| {
                path.metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
            .unwrap_or_else(|| panic!("{REQUIRED}: cannot find {name}"))
            .canonicalize()
            .expect("resolve required PTY fixture executable")
    }

    #[test]
    fn every_terminal_regression_refuses_a_missing_fixture() {
        let timeout = executable("timeout");
        let git = executable("git");
        let script = executable("script");
        for missing in ["script", "timeout"] {
            let path = TempDir::new();
            std::os::unix::fs::symlink(&git, path.0.join("git")).unwrap();
            let (present, program) = if missing == "script" {
                ("timeout", &timeout)
            } else {
                ("script", &script)
            };
            std::os::unix::fs::symlink(program, path.0.join(present)).unwrap();
            // Run the real callers, not only this helper: restoring an early
            // return in any terminal test must turn this regression red.
            for name in [
                "a_terminal_turns_the_prompt_on_and_the_answer_is_recorded",
                "the_prompt_keeps_asking_until_every_ambiguity_is_answered",
                "stopping_part_way_through_the_loop_still_records_nothing",
            ] {
                let out = Command::new(&timeout)
                    .args(["--kill-after=5s", "15s"])
                    .arg(std::env::current_exe().unwrap())
                    .args(["--exact", name, "--nocapture"])
                    .env("PATH", &path.0)
                    .output()
                    .unwrap();
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                assert_eq!(
                    out.status.code(),
                    Some(101),
                    "{name} must fail as a Rust test without {missing}: {text}"
                );
                assert!(text.contains(REQUIRED), "{name}, missing {missing}: {text}");
            }
        }
    }

    #[test]
    fn a_stalled_terminal_fails_with_a_bounded_wait() {
        let started = std::time::Instant::now();
        let failure = std::panic::catch_unwind(|| {
            run_with_limit("exec /bin/sleep 1", b"", Duration::from_millis(200))
        })
        .expect_err("a terminal that missed its deadline must fail");
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(message.contains("PTY fixture timed out"), "{message}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
