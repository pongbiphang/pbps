//! LocalDB discovery: from the instance an operator named to the pipe its
//! running process listens on (#2271, DEC-2271.2).
//!
//! LocalDB accepts a connection only on a named pipe whose name changes every
//! time the instance starts, so a connection string can name the instance but
//! not the endpoint. This module finds the endpoint and does nothing else. It
//! runs `SqlLocalDB.exe info <instance>` once, reads one pipe out of what it
//! prints, and hands the rewritten connection string back to the ordinary
//! driver, which admits it like any other and does all the SQL. It starts,
//! stops and creates no instance: the operator provides a running one, as a
//! supplied server is provided.
//!
//! The helper is an inspection tool, not an execution engine, so the way it
//! runs is narrow on purpose:
//!
//! - no shell: the program is one fixed path and the instance name is one
//!   argument checked against a short alphabet by [`LocalDbSelector`];
//! - no SQL, no credential and no target string reaches it;
//! - its output is bounded and so is its time, and a helper that overruns
//!   either is terminated rather than awaited.
//!
//! The output is read for the one thing whose spelling does not depend on the
//! Windows display language: a pipe path. The labels in front of it are
//! translated, so none is matched. A result with no pipe, with two different
//! pipes, or with a pipe of another shape is a failure, never an empty or a
//! guessed answer, and no failure is retried: a retry would be a guess about
//! an instance state that nothing here has measured.

use pbps_db::{DbError, LocalDbSelector};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;

/// What the helper may print before it is treated as something other than an
/// instance description. Its real output is a few hundred bytes.
const OUTPUT_LIMIT: usize = 16 * 1024;

/// The major version this tool qualifies LocalDB at; its tools live under
/// the matching `<major>0` directory. Another major is a separate
/// qualification, not a different path (DEC-2271.2).
const QUALIFIED_DIRECTORY: &str = "170";

/// Why discovery produced no endpoint. Each is a distinct fact the operator can
/// act on; none is an empty result.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    #[error("{0}")]
    Selector(String),
    #[error("SqlLocalDB.exe was not found at {0}; install the SQL Server 2025 (17.x) LocalDB")]
    NotInstalled(String),
    #[error("SqlLocalDB.exe could not be started: {0}")]
    Spawn(String),
    #[error("SqlLocalDB.exe did not answer within {0:?} and was terminated")]
    TimedOut(Duration),
    #[error("SqlLocalDB.exe printed more than {OUTPUT_LIMIT} bytes and was terminated")]
    TooMuchOutput,
    #[error("SqlLocalDB.exe reported an error for the instance ({0}); check that it exists")]
    Exit(String),
    #[error(
        "the instance reports no named pipe; start it, as `SqlLocalDB.exe start <instance>`, \
         before the resolver needs it"
    )]
    NoUsablePipe,
    #[error("the instance reported {0} different named pipes; none was chosen")]
    AmbiguousPipe(usize),
    #[error("the instance's pipe is not a LocalDB pipe: {0}")]
    UnexpectedPipe(String),
}

/// One instance, selected and then resolved to the connection string that
/// reaches it now. The three states of discovery stay three types: a selector
/// (what the operator named), this (where it is), and [`Failure`].
#[derive(Debug)]
pub struct Resolved {
    instance: String,
    pipe: String,
    connection: String,
}

impl Resolved {
    #[must_use]
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The pipe this run found, kept so evidence can say which one it used.
    #[must_use]
    pub fn pipe(&self) -> &str {
        &self.pipe
    }

    /// The caller's connection string with the pipe for its server, ready for
    /// [`pbps_db::Conn::connect`].
    #[must_use]
    pub fn connection(&self) -> &str {
        &self.connection
    }
}

/// Where the qualified major's `SqlLocalDB.exe` is installed, from the
/// environment's `ProgramFiles`. A fixed location, not a `PATH` search: a
/// lookup through `PATH` would run whatever the caller's directory order puts
/// first, and an older major's tool would report instances it cannot run.
#[must_use]
pub fn helper() -> Option<PathBuf> {
    let root = std::env::var_os("ProgramFiles")?;
    Some(
        Path::new(&root)
            .join("Microsoft SQL Server")
            .join(QUALIFIED_DIRECTORY)
            .join("Tools")
            .join("Binn")
            .join("SqlLocalDB.exe"),
    )
}

/// Resolves a connection string that names a `(localdb)` instance.
///
/// `Ok(None)` is a string that names none, which is not this module's to
/// open. `budget` bounds the whole discovery.
pub async fn resolve(
    connection_string: &str,
    helper: Option<&Path>,
    budget: Duration,
) -> Result<Option<Resolved>, Failure> {
    let selector = LocalDbSelector::read(connection_string).map_err(selector_failure)?;
    let Some(selector) = selector else {
        return Ok(None);
    };
    let Some(helper) = helper else {
        return Err(Failure::NotInstalled(
            "%ProgramFiles%\\Microsoft SQL Server\\170 (ProgramFiles is not set)".into(),
        ));
    };
    let pipe = discover(helper, selector.instance(), budget).await?;
    let connection = selector.with_pipe(&pipe).map_err(selector_failure)?;
    Ok(Some(Resolved {
        instance: selector.instance().to_owned(),
        pipe,
        connection,
    }))
}

fn selector_failure(error: DbError) -> Failure {
    Failure::Selector(error.to_string())
}

/// Runs `helper info <instance>` and reads its pipe.
pub async fn discover(helper: &Path, instance: &str, budget: Duration) -> Result<String, Failure> {
    let deadline = Instant::now() + budget;
    let mut child = Command::new(helper)
        .arg("info")
        .arg(instance)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // A future dropped by cancellation must not leave the helper running.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Failure::NotInstalled(helper.display().to_string())
            } else {
                Failure::Spawn(error.to_string())
            }
        })?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut output = Vec::new();
    let read = async {
        // One byte past the limit proves the output overran it.
        let mut limited = (&mut stdout).take(OUTPUT_LIMIT as u64 + 1);
        limited.read_to_end(&mut output).await
    };
    let outcome = tokio::time::timeout_at(deadline, read).await;
    let reaped = match outcome {
        Ok(Ok(_)) if output.len() <= OUTPUT_LIMIT => {
            tokio::time::timeout_at(deadline, child.wait()).await
        }
        Ok(Ok(_)) => {
            terminate(&mut child).await;
            return Err(Failure::TooMuchOutput);
        }
        Ok(Err(error)) => {
            terminate(&mut child).await;
            return Err(Failure::Spawn(error.to_string()));
        }
        Err(_) => {
            terminate(&mut child).await;
            return Err(Failure::TimedOut(budget));
        }
    };
    let status = match reaped {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => return Err(Failure::Spawn(error.to_string())),
        Err(_) => {
            terminate(&mut child).await;
            return Err(Failure::TimedOut(budget));
        }
    };
    if !status.success() {
        return Err(Failure::Exit(status.code().map_or_else(
            || "no exit code".to_owned(),
            |code| format!("exit code {code}"),
        )));
    }
    pipe_in(&output)
}

/// A timed-out or runaway helper is killed and reaped; the error that follows
/// is the failure, whatever the kill itself reports.
/// `kill_on_drop` already stops a helper whose future is dropped; this kill
/// is the one that also reaps it on the paths that return. Either alone ends
/// the process, so a test that removes only one cannot fail; removing both
/// does (`a_silent_helper_is_terminated_at_the_budget`).
async fn terminate(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
}

const PIPE_PREFIX: &str = "np:\\\\.\\pipe\\";

/// The single LocalDB pipe the output names.
///
/// The output is text in whatever language and code page the machine uses, so
/// only the pipe is read: every `np:\\.\pipe\` token, to the end of its
/// whitespace-delimited run. UTF-16 output, which has a NUL beside each ASCII
/// byte, is decoded first so that a pipe in it is still seen.
pub fn pipe_in(output: &[u8]) -> Result<String, Failure> {
    let text = if output.starts_with(&[0xFF, 0xFE])
        || output.iter().filter(|b| **b == 0).count() * 4 > output.len()
    {
        let units: Vec<u16> = (0..output.len() / 2)
            .map(|at| u16::from_le_bytes([output[2 * at], output[2 * at + 1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(output).into_owned()
    };
    let mut found: Vec<&str> = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&PIPE_PREFIX.to_ascii_lowercase()) {
        let start = from + at;
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |len| start + len);
        let token = &text[start..end];
        if !found.contains(&token) {
            found.push(token);
        }
        from = end;
    }
    match found[..] {
        [] => Err(Failure::NoUsablePipe),
        [pipe] => localdb_shaped(pipe).map(str::to_owned),
        _ => Err(Failure::AmbiguousPipe(found.len())),
    }
}

/// `np:\\.\pipe\LOCALDB#<hex>\tsql\query`, the shape LocalDB documents for an
/// instance's pipe. A different shape is refused rather than connected to: it
/// is not what this was qualified against.
fn localdb_shaped(pipe: &str) -> Result<&str, Failure> {
    let rest = &pipe[PIPE_PREFIX.len()..];
    let shaped = rest
        .strip_prefix("LOCALDB#")
        .and_then(|rest| rest.strip_suffix("\\tsql\\query"))
        .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()));
    if shaped {
        Ok(pipe)
    } else {
        Err(Failure::UnexpectedPipe(pipe.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIPE: &str = "np:\\\\.\\pipe\\LOCALDB#8A7B6C5D\\tsql\\query";

    fn english(pipe: &str) -> String {
        format!(
            "Name:               pbps_scratch\r\nVersion:            17.0.1000.7\r\nShared name:\r\n\
             Owner:              HOST\\runner\r\nAuto-create:        No\r\nState:              Running\r\n\
             Last start time:    10/24/2026 1:00:00 PM\r\nInstance pipe name: {pipe}\r\n"
        )
    }

    #[test]
    fn the_pipe_is_read_without_trusting_any_label() {
        assert_eq!(pipe_in(english(PIPE).as_bytes()).unwrap(), PIPE);
        // Translated labels, non-ASCII text and a code page that is not UTF-8.
        let german = format!("Name: x\r\nStatus: L\u{e4}uft\r\nInstanzpipename: {PIPE}\r\n");
        assert_eq!(pipe_in(german.as_bytes()).unwrap(), PIPE);
        let mut legacy = b"Name: x\r\nOwner: H\xc4NS\r\n".to_vec();
        legacy.extend_from_slice(format!("Pipe: {PIPE}\r\n").as_bytes());
        assert_eq!(pipe_in(&legacy).unwrap(), PIPE);
        let chinese = format!("\u{540d}\u{79f0}: x\n\u{7ba1}\u{9053}: {PIPE}\n");
        assert_eq!(pipe_in(chinese.as_bytes()).unwrap(), PIPE);
        // The same pipe twice is one pipe.
        let twice = format!("{}{}", english(PIPE), format_args!("again {PIPE}\n"));
        assert_eq!(pipe_in(twice.as_bytes()).unwrap(), PIPE);
    }

    #[test]
    fn utf16_output_is_decoded_before_it_is_read() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(english(PIPE).encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(pipe_in(&bytes).unwrap(), PIPE);
    }

    #[test]
    fn no_pipe_is_a_failure_and_never_an_empty_answer() {
        let stopped = english("").replace("Running", "Stopped");
        assert_eq!(pipe_in(stopped.as_bytes()), Err(Failure::NoUsablePipe));
        for nothing in ["", "\r\n", "Name: x\r\n"] {
            assert_eq!(pipe_in(nothing.as_bytes()), Err(Failure::NoUsablePipe));
        }
    }

    #[test]
    fn two_different_pipes_choose_neither() {
        let other = "np:\\\\.\\pipe\\LOCALDB#00FF00FF\\tsql\\query";
        let two = format!("{}\r\nShared: {other}\r\n", english(PIPE));
        assert_eq!(pipe_in(two.as_bytes()), Err(Failure::AmbiguousPipe(2)));
    }

    #[test]
    fn a_pipe_of_another_shape_is_refused_rather_than_connected_to() {
        for pipe in [
            "np:\\\\.\\pipe\\sql\\query",
            "np:\\\\.\\pipe\\LOCALDB#XYZ\\tsql\\query",
            "np:\\\\.\\pipe\\LOCALDB#\\tsql\\query",
            "np:\\\\.\\pipe\\LOCALDB#8A7B6C5D\\tsql\\query\\extra",
            "np:\\\\.\\pipe\\LOCALDB#8A7B6C5D",
            "np:\\\\.\\pipe\\MSSQL$X\\sql\\query",
        ] {
            assert!(
                matches!(
                    pipe_in(english(pipe).as_bytes()),
                    Err(Failure::UnexpectedPipe(_))
                ),
                "{pipe}"
            );
        }
        // A remote pipe in the output is not a local-pipe prefix at all.
        let remote = "Instance pipe name: np:\\\\host\\pipe\\LOCALDB#8A7B6C5D\\tsql\\query\r\n";
        assert_eq!(pipe_in(remote.as_bytes()), Err(Failure::NoUsablePipe));
    }

    #[tokio::test]
    async fn a_connection_string_that_names_no_instance_runs_nothing() {
        let outcome = resolve(
            "Server=db,1433;User Id=sa;Password=x",
            Some(Path::new("/definitely/not/a/helper")),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(outcome, Ok(None)));
    }

    #[tokio::test]
    async fn an_unsupported_form_is_refused_before_the_helper_is_looked_for() {
        // No helper at all: the form is refused first, by name. Where pipes
        // are unavailable the refusal is the platform's, equally before any
        // helper.
        let outcome = resolve(
            r"Server=(localdb)\.\SHARED;Integrated Security=SSPI",
            None,
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(outcome, Err(Failure::Selector(_))), "{outcome:?}");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_selector_off_windows_never_looks_for_a_helper() {
        let outcome = resolve(
            r"Server=(localdb)\x;Integrated Security=SSPI",
            None,
            Duration::from_secs(5),
        )
        .await;
        let Err(Failure::Selector(why)) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(why.contains("only available on Windows"), "{why}");
    }

    #[tokio::test]
    async fn a_missing_helper_is_not_installed_and_never_a_guess() {
        let outcome = discover(
            Path::new("/definitely/not/a/helper"),
            "x",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(outcome, Err(Failure::NotInstalled(_))),
            "{outcome:?}"
        );
    }

    #[cfg(unix)]
    mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// A helper script. `ETXTBSY` means another test's fork inherited the
        /// still-open file; the file is complete, so the spawn is retried.
        async fn helper(tag: &str, body: &str) -> PathBuf {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "pbps-localdb-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        async fn run(
            tag: &str,
            body: &str,
            budget: Duration,
        ) -> (Result<String, Failure>, Duration) {
            let path = helper(tag, body).await;
            let started = Instant::now();
            let mut outcome = discover(&path, "x", budget).await;
            for _ in 0..20 {
                if !matches!(&outcome, Err(Failure::Spawn(why)) if why.contains("busy")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                outcome = discover(&path, "x", budget).await;
            }
            let _ = std::fs::remove_file(&path);
            (outcome, started.elapsed())
        }

        #[tokio::test]
        async fn the_helper_is_called_with_the_verb_and_the_instance_and_nothing_else() {
            let (outcome, _) = run(
                "args",
                &format!(
                    "[ \"$#\" = 2 ] && [ \"$1\" = info ] && [ \"$2\" = x ] || exit 3\n\
                     printf 'Instance pipe name: %s\\n' '{PIPE}'"
                ),
                Duration::from_secs(10),
            )
            .await;
            assert_eq!(outcome.unwrap(), PIPE);
        }

        #[tokio::test]
        async fn a_helper_error_is_the_instances_failure_not_an_empty_answer() {
            let (outcome, _) = run(
                "exit",
                &format!("printf 'Instance pipe name: %s\\n' '{PIPE}'\nexit 1"),
                Duration::from_secs(10),
            )
            .await;
            // A pipe printed beside a failure status is still a failure.
            assert!(matches!(outcome, Err(Failure::Exit(_))), "{outcome:?}");
        }

        #[tokio::test]
        async fn a_silent_helper_is_terminated_at_the_budget() {
            let marker =
                std::env::temp_dir().join(format!("pbps-localdb-alive-{}", std::process::id()));
            let (outcome, elapsed) = run(
                "hang",
                &format!("sleep 1; touch '{}'; sleep 30", marker.display()),
                Duration::from_millis(300),
            )
            .await;
            assert!(matches!(outcome, Err(Failure::TimedOut(_))), "{outcome:?}");
            assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
            // Terminated, not abandoned: it never reaches the second second.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert!(!marker.exists(), "the helper outlived its budget");
        }

        #[tokio::test]
        async fn a_helper_that_prints_without_end_is_terminated_at_the_limit() {
            let (outcome, elapsed) = run("flood", "yes 'x'", Duration::from_secs(20)).await;
            assert_eq!(outcome, Err(Failure::TooMuchOutput));
            assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        }

        #[tokio::test]
        async fn a_helper_that_prints_then_hangs_still_ends_at_the_budget() {
            let (outcome, elapsed) = run(
                "stall",
                &format!("printf 'Instance pipe name: %s\\n' '{PIPE}'\nexec 1>&-\nsleep 30"),
                Duration::from_millis(400),
            )
            .await;
            assert!(matches!(outcome, Err(Failure::TimedOut(_))), "{outcome:?}");
            assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        }
    }
}
