//! SQL Server readiness diagnostics shared by preview and resolver acquisition.

use super::ProduceError;
use pbps_db::Conn;
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Why the server never answered. The one case that earns another start is a
/// SQL Server that crashed while starting (DEC-2762.1); everything else is
/// reported as it always was.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum NotReady {
    /// The container exited and its log names a `core.sqlservr` dump. Carries
    /// the report, so the crash stays on record when the container is gone.
    Crashed(String),
    Failed(String),
}

impl NotReady {
    pub(super) fn into_report(self) -> String {
        match self {
            Self::Crashed(report) | Self::Failed(report) => report,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RuntimeState {
    running: bool,
    exit_code: i64,
    #[serde(rename = "OOMKilled")]
    oom_killed: bool,
    error: String,
}

enum Observed {
    Running,
    Stopped(RuntimeState),
    Missing,
    Unknown(String),
}

impl Observed {
    fn describe(&self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::Stopped(state) => format!(
                "exit status {}, OOM killed: {}; {}",
                state.exit_code, state.oom_killed, state.error
            ),
            Self::Missing => "the owned container no longer exists".into(),
            Self::Unknown(reason) => format!("unknown ({reason})"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Poll {
    Wait,
    Exited,
    TimedOut,
}

fn readiness(state: &Observed, elapsed: std::time::Duration) -> Poll {
    match state {
        Observed::Stopped(_) | Observed::Missing => Poll::Exited,
        Observed::Running | Observed::Unknown(_) if elapsed >= READY_TIMEOUT => Poll::TimedOut,
        Observed::Running | Observed::Unknown(_) => Poll::Wait,
    }
}

fn observe_container(out: &std::process::Output, id: &str) -> Observed {
    if out.status.success() {
        return match serde_json::from_slice::<RuntimeState>(&out.stdout) {
            Ok(state) if state.running => Observed::Running,
            Ok(state) => Observed::Stopped(state),
            Err(error) => Observed::Unknown(format!("invalid container state: {error}")),
        };
    }
    let error = String::from_utf8_lossy(&out.stderr);
    // Match the whole diagnostic for this owned id. Daemon/permission errors,
    // and errors merely mentioning another absent object, prove no termination.
    if out.stdout.iter().all(u8::is_ascii_whitespace)
        && ["object", "container"].iter().any(|kind| {
            [id.to_owned(), format!("\"{id}\"")].iter().any(|name| {
                error
                    .trim()
                    .eq_ignore_ascii_case(&format!("Error: no such {kind}: {name}"))
            })
        })
    {
        Observed::Missing
    } else {
        Observed::Unknown(format!(
            "docker inspect failed ({}): {}",
            out.status,
            error.trim()
        ))
    }
}

async fn readiness_probe(
    started: std::time::Instant,
    attempt: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    // Restarting a slow but valid handshake every few seconds prevents it
    // from ever becoming ready. SQL gets the remaining overall budget.
    tokio::time::timeout(READY_TIMEOUT.saturating_sub(started.elapsed()), attempt)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("SQL readiness poll timed out")))
}

fn runtime_budget(started: std::time::Instant) -> std::time::Duration {
    // An unresponsive runtime must not extend the overall readiness wait.
    std::time::Duration::from_secs(5).min(READY_TIMEOUT.saturating_sub(started.elapsed()))
}

async fn runtime_output(
    args: &[&str],
    within: std::time::Duration,
) -> Result<std::process::Output, String> {
    tokio::time::timeout(
        within,
        tokio::process::Command::new("docker")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("docker {} timed out", args[0]))?
    .map_err(|error| format!("cannot run docker {}: {error}", args[0]))
}

fn startup_diagnostic(text: &str, password: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .replace(password, "[redacted]")
}

/// How much of a container's log a startup failure reports: the first lines,
/// where SQL Server prints its fatal error, reason and stack, and the last
/// lines, where the dump collector ends. The old twenty-line tail reached only
/// the collector's `find` noise (#2762), so the crash itself was never read.
const LOG_HEAD: usize = 40;
const LOG_TAIL: usize = 20;

fn log_excerpt(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    if lines.len() <= LOG_HEAD + LOG_TAIL {
        return lines.join("\n");
    }
    let skipped = lines.len() - LOG_HEAD - LOG_TAIL;
    format!(
        "{}\n[{skipped} lines omitted]\n{}",
        lines[..LOG_HEAD].join("\n"),
        lines[lines.len() - LOG_TAIL..].join("\n")
    )
}

/// A container that exited before its server answered. A start is called a
/// crash only when the engine's own dump collector says so: an exit with no
/// `core.sqlservr` in its log, or a container the runtime no longer knows,
/// proves nothing about the engine (DEC-958.1 states the same condition for
/// the dedicated-server fixture).
fn after_exit(observed: &Observed, log: Result<&str, &str>, password: &str) -> NotReady {
    let (shown, names_a_dump) = match log {
        Ok(text) => (log_excerpt(text), text.contains("core.sqlservr")),
        Err(error) => (format!("container logs unavailable: {error}"), false),
    };
    let report = startup_diagnostic(
        &format!(
            "the scratch container stopped before SQL was ready: {}; container log (first and last lines):\n{shown}",
            observed.describe()
        ),
        password,
    );
    if matches!(observed, Observed::Stopped(_)) && names_a_dump {
        NotReady::Crashed(report)
    } else {
        NotReady::Failed(report)
    }
}

pub(super) async fn ready(connection: &str, id: &str, password: &str) -> Result<(), NotReady> {
    let started = std::time::Instant::now();
    loop {
        let answer = readiness_probe(started, async {
            let mut c = Conn::connect(pbps_db::Driver::Mssql, connection).await?;
            c.query("SELECT 1 AS ok;").await?;
            Ok(())
        })
        .await;
        if answer.is_ok() {
            return Ok(());
        }
        let observed = async {
            match runtime_output(
                &["inspect", "--format", "{{json .State}}", id],
                runtime_budget(started),
            )
            .await
            {
                Ok(out) => observe_container(&out, id),
                Err(error) => Observed::Unknown(error),
            }
        }
        .await;
        match readiness(&observed, started.elapsed()) {
            Poll::Exited => {
                let logs = runtime_output(&["logs", id], runtime_budget(started)).await;
                let text = logs.map(|out| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    )
                });
                return Err(after_exit(
                    &observed,
                    text.as_deref().map_err(String::as_str),
                    password,
                ));
            }
            Poll::TimedOut => {
                return Err(NotReady::Failed(startup_diagnostic(
                    &format!(
                        "the scratch container did not accept connections within {}s; last SQL error: {}; last runtime state: {}",
                        READY_TIMEOUT.as_secs(),
                        answer.unwrap_err(),
                        observed.describe()
                    ),
                    password,
                )));
            }
            Poll::Wait => {}
        }
        tokio::time::sleep(
            std::time::Duration::from_secs(2).min(READY_TIMEOUT.saturating_sub(started.elapsed())),
        )
        .await;
    }
}

/// A container started and its server answering, with one more start when the
/// first server crashed while starting (DEC-2762.1). SQL Server 2025 aborts
/// inside its own start-up threads, 1 to 7 s after `docker start`, when the
/// host's CPUs are saturated (measured on #2762: 0 of 30 idle starts, 2 of 30
/// under 32 busy loops); no connection has been made and nothing of the run is
/// in the container yet, so a fresh container is the same start.
///
/// [`Launch::start`] is told which container it makes, because a journaled
/// owner resets its record before the second.
/// The crashed container is confirmed removed by `discard` before the second is
/// named, so the two never coexist. The second crash is not retried. The
/// container that comes back is always the one the caller must still remove.
pub(super) struct Started<C> {
    pub container: C,
    pub ready: Result<(), ProduceError>,
    /// The first container's report when a second was started, whatever its
    /// outcome: the crash stays on record even when the retry succeeds.
    pub first_crash: Option<String>,
}

/// What [`started_ready`] drives; a trait rather than closures because an
/// async closure over borrowed arguments cannot be sent to a spawned task.
#[allow(
    async_fn_in_trait,
    reason = "used by value in one function; no Send bound is needed"
)]
pub(super) trait Launch {
    type Container;
    /// `again` is true for the second container, after `discard` confirmed
    /// the first one gone.
    async fn start(&mut self, again: bool) -> Result<Self::Container, ProduceError>;
    /// Waits for the server and hands the container back.
    async fn wait(&mut self, container: Self::Container)
    -> (Self::Container, Result<(), NotReady>);
    /// Removes the container and confirms it gone.
    async fn discard(&mut self, container: Self::Container) -> Result<(), Vec<String>>;
}

pub(super) async fn started_ready<L: Launch>(
    launch: &mut L,
) -> Result<Started<L::Container>, ProduceError> {
    let first = launch.start(false).await?;
    let (first, waited) = launch.wait(first).await;
    let crash = match waited {
        Ok(()) => {
            return Ok(Started {
                container: first,
                ready: Ok(()),
                first_crash: None,
            });
        }
        Err(NotReady::Failed(why)) => {
            return Ok(Started {
                container: first,
                ready: Err(ProduceError::Acquire(why)),
                first_crash: None,
            });
        }
        Err(NotReady::Crashed(report)) => report,
    };
    launch.discard(first).await.map_err(ProduceError::Cleanup)?;
    let second = match launch.start(true).await {
        Ok(second) => second,
        Err(ProduceError::Acquire(why)) => {
            return Err(ProduceError::Acquire(format!(
                "{why}; the first start had stopped before SQL was ready:\n{crash}"
            )));
        }
        Err(other) => return Err(other),
    };
    let (second, waited) = launch.wait(second).await;
    let ready = match waited {
        Ok(()) => Ok(()),
        Err(NotReady::Crashed(report) | NotReady::Failed(report)) => {
            Err(ProduceError::Acquire(format!(
                "{report}\nthe first start of this run had also stopped before SQL was ready:\n{crash}"
            )))
        }
    };
    Ok(Started {
        container: second,
        ready,
        first_crash: Some(crash),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_slow_successful_probe_keeps_the_overall_readiness_budget() {
        let answer = readiness_probe(std::time::Instant::now(), async {
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            Ok(())
        })
        .await;
        assert!(answer.is_ok(), "{answer:?}");
        let expired = std::time::Instant::now() - READY_TIMEOUT;
        let answer = readiness_probe(expired, std::future::pending()).await;
        assert_eq!(
            answer.unwrap_err().to_string(),
            "SQL readiness poll timed out"
        );
    }

    fn runtime_answer(success: bool, stdout: &str, stderr: &str) -> std::process::Output {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn only_proven_termination_shortens_the_readiness_wait() {
        let waiting = std::time::Duration::from_secs(1);
        // State fields measured through Docker's API and native Podman;
        // Podman's additional runtime fields must not invalidate the read.
        for sample in [
            r#"{"Running":true,"ExitCode":0,"OOMKilled":false,"Error":"","Status":"running","Pid":12}"#,
            r#"{"Running":true,"ExitCode":0,"OOMKilled":false,"Error":"","OciVersion":"1.2.1","ConmonPid":11}"#,
        ] {
            let state = observe_container(&runtime_answer(true, sample, ""), "owned");
            assert!(matches!(state, Observed::Running));
            assert_eq!(readiness(&state, waiting), Poll::Wait);
            assert_eq!(readiness(&state, READY_TIMEOUT), Poll::TimedOut);
        }
        let stopped = observe_container(
            &runtime_answer(
                true,
                r#"{"Running":false,"ExitCode":137,"OOMKilled":false,"Error":""}"#,
                "",
            ),
            "owned",
        );
        assert_eq!(readiness(&stopped, waiting), Poll::Exited);
        assert_eq!(readiness(&stopped, READY_TIMEOUT), Poll::Exited);
        assert!(
            stopped
                .describe()
                .contains("exit status 137, OOM killed: false")
        );
        for malformed in [
            "",
            "null",
            "{}",
            r#"{"Running":false}"#,
            r#"{"Running":"false","ExitCode":137,"OOMKilled":false,"Error":""}"#,
        ] {
            let state = observe_container(&runtime_answer(true, malformed, ""), "owned");
            assert!(matches!(state, Observed::Unknown(_)));
            assert_eq!(readiness(&state, waiting), Poll::Wait);
            assert_eq!(readiness(&state, READY_TIMEOUT), Poll::TimedOut);
        }
        for message in [
            "permission denied",
            "cannot connect to the Docker daemon",
            "Error: no such object: another",
            "Error: no such object: owned\npermission denied",
        ] {
            let state = observe_container(&runtime_answer(false, "", message), "owned");
            assert!(matches!(state, Observed::Unknown(_)));
            assert_eq!(readiness(&state, waiting), Poll::Wait);
            assert_eq!(readiness(&state, READY_TIMEOUT), Poll::TimedOut);
        }
        for message in [
            "Error: No such object: owned\n",
            "error: no such container: owned",
            "Error: no such object: \"owned\"\n",
        ] {
            let state = observe_container(&runtime_answer(false, "\n", message), "owned");
            assert!(matches!(state, Observed::Missing));
            assert_eq!(readiness(&state, waiting), Poll::Exited);
        }
    }

    #[test]
    fn startup_diagnostics_keep_causes_but_never_the_generated_password() {
        let password = "Pbps!123secret";
        let state = observe_container(
            &runtime_answer(
                true,
                r#"{"Running":false,"ExitCode":137,"OOMKilled":true,"Error":"Pbps!123secret failed"}"#,
                "",
            ),
            "owned",
        );
        let text = startup_diagnostic(
            &format!(
                "{}\nlog: {password}\nlogin: Pbps!123\rsecret; error 18456",
                state.describe()
            ),
            password,
        );
        assert!(!text.contains(password), "{text}");
        assert!(!text.contains('\r'), "{text}");
        assert_eq!(text.matches("[redacted]").count(), 3);
        assert!(text.contains("exit status 137, OOM killed: true"), "{text}");
        assert!(text.contains("error 18456"), "{text}");
        assert_eq!(
            startup_diagnostic("ordinary connection refusal", password),
            "ordinary connection refusal"
        );
    }
    // The head of a real crash (SQL Server 2025 17.0.4075.5, #2762's family),
    // then the dump collector's lines as the gates logged them.
    const CRASH_HEAD: &str = "SQL Server 2025 will run as non-root by default.
This container is running as user mssql.
This program has encountered a fatal error and cannot continue running at Tue Nov 10 18:39:54 2026
The following diagnostic information is available:

         Reason: 0x00000004
        Message: ASSERT: Expression=(IsStarted()) File=LibOS\\Windows\\Kernel\\SQLPal\\common\\dk\\sos\\src\\scheduler.cpp Line=1377
        Process: 11 - sqlservr
     Last errno: 2
Successfully captured dump: /var/opt/mssql/log/core.sqlservr.11_10_2026_18_39_54.11";

    fn crash_log(noise: usize) -> String {
        let mut log = String::from(CRASH_HEAD);
        for n in 0..noise {
            log.push_str(&format!(
                "\nfind: '/proc/11/task/{n}/fdinfo': Permission denied"
            ));
        }
        log.push_str("\nNot compressing the dump files, moving instead to: /var/opt/mssql/log/core.sqlservr.11_10_2026_18_39_57.11.d");
        log
    }

    fn stopped(code: i64) -> Observed {
        Observed::Stopped(RuntimeState {
            running: false,
            exit_code: code,
            oom_killed: false,
            error: String::new(),
        })
    }

    #[test]
    fn a_report_of_a_long_crash_log_keeps_the_fatal_error_the_old_tail_dropped() {
        let log = crash_log(300);
        let report = match after_exit(&stopped(1), Ok(&log), "pw") {
            NotReady::Crashed(report) => report,
            other => panic!("{other:?}"),
        };
        assert!(report.contains("Reason: 0x00000004"), "{report}");
        assert!(report.contains("scheduler.cpp Line=1377"), "{report}");
        assert!(report.contains("Last errno: 2"), "{report}");
        assert!(
            report.contains("core.sqlservr.11_10_2026_18_39_57.11.d"),
            "{report}"
        );
        assert!(report.contains("lines omitted]"), "{report}");
        // Bounded: the head, the tail and a marker, not the whole log.
        assert!(
            report.lines().count() < 80,
            "{} lines",
            report.lines().count()
        );
        // Negative: the old twenty-line tail of this log is collector noise only.
        let old_tail: Vec<&str> = log.lines().rev().take(20).collect();
        assert!(old_tail.iter().all(|l| !l.contains("Reason")));
    }

    #[test]
    fn a_short_log_is_reported_whole_and_the_password_never_appears() {
        let log = "fatal\nPassword was pw1 here\ncore.sqlservr.1";
        let report = match after_exit(&stopped(1), Ok(log), "pw1") {
            NotReady::Crashed(report) => report,
            other => panic!("{other:?}"),
        };
        assert!(!report.contains("pw1"), "{report}");
        assert!(
            report.contains("[redacted]") && !report.contains("lines omitted"),
            "{report}"
        );
    }

    #[test]
    fn only_an_exit_whose_log_names_a_dump_is_a_startup_crash() {
        // Exit code is not the signal: the dump collector is.
        for code in [1, 0, 137] {
            assert!(matches!(
                after_exit(&stopped(code), Ok(&crash_log(2)), "pw"),
                NotReady::Crashed(_)
            ));
        }
        // Negative: an exit with no dump in its log (a bad SA password, a
        // permissions error, a killed container).
        for log in [
            "ERROR: Unable to set system administrator password: Password validation failed.",
            "",
            "the word core without the file name: core.sql",
        ] {
            assert!(
                matches!(after_exit(&stopped(1), Ok(log), "pw"), NotReady::Failed(_)),
                "{log}"
            );
        }
        // Negative: a container the runtime no longer knows proves nothing, even
        // when a log was somehow read.
        assert!(matches!(
            after_exit(&Observed::Missing, Ok(&crash_log(2)), "pw"),
            NotReady::Failed(_)
        ));
        // Negative: logs that cannot be read cannot name a dump.
        let unread = after_exit(&stopped(1), Err("docker logs timed out"), "pw");
        assert!(
            matches!(&unread, NotReady::Failed(r) if r.contains("container logs unavailable")),
            "{unread:?}"
        );
    }

    /// A scripted start: each container's wait result in order.
    #[derive(Default)]
    struct Script {
        waits: std::collections::VecDeque<Result<(), NotReady>>,
        starts: Vec<bool>,
        discarded: Vec<u32>,
        made: u32,
        second_start_fails: bool,
        removal_fails: bool,
    }

    impl Launch for Script {
        type Container = u32;
        async fn start(&mut self, again: bool) -> Result<u32, ProduceError> {
            self.starts.push(again);
            if again && self.second_start_fails {
                return Err(ProduceError::Acquire("starting image: no port".into()));
            }
            self.made += 1;
            Ok(self.made)
        }
        async fn wait(&mut self, container: u32) -> (u32, Result<(), NotReady>) {
            (container, self.waits.pop_front().expect("scripted"))
        }
        async fn discard(&mut self, container: u32) -> Result<(), Vec<String>> {
            self.discarded.push(container);
            if self.removal_fails {
                Err(vec!["the docker container pbps-scratch-x".into()])
            } else {
                Ok(())
            }
        }
    }

    fn crashed(tag: &str) -> Result<(), NotReady> {
        Err(NotReady::Crashed(format!("crash {tag}")))
    }

    #[tokio::test]
    async fn a_startup_crash_is_followed_by_one_more_start_and_keeps_the_first_report() {
        let mut run = Script {
            waits: [crashed("one"), Ok(())].into(),
            ..Script::default()
        };
        let started = started_ready(&mut run).await.unwrap();
        assert_eq!(started.container, 2, "the caller gets the second container");
        assert!(started.ready.is_ok());
        assert_eq!(started.first_crash.as_deref(), Some("crash one"));
        assert_eq!(run.starts, [false, true]);
        assert_eq!(
            run.discarded,
            [1],
            "the crashed one is removed, not the live one"
        );
    }

    #[tokio::test]
    async fn a_second_crash_fails_with_both_reports_and_starts_no_third() {
        let mut run = Script {
            waits: [crashed("one"), crashed("two")].into(),
            ..Script::default()
        };
        let started = started_ready(&mut run).await.unwrap();
        let error = started.ready.unwrap_err().to_string();
        assert!(
            error.contains("crash two") && error.contains("crash one"),
            "{error}"
        );
        assert_eq!(started.container, 2, "left for the caller to remove");
        assert_eq!(run.starts, [false, true], "never a third start");
        assert_eq!(run.discarded, [1]);
    }

    #[tokio::test]
    async fn a_ready_first_start_and_a_failure_without_a_dump_are_never_retried() {
        let mut ready = Script {
            waits: [Ok(())].into(),
            ..Script::default()
        };
        let started = started_ready(&mut ready).await.unwrap();
        assert!(started.ready.is_ok() && started.first_crash.is_none());
        assert_eq!((ready.starts.len(), ready.discarded.len()), (1, 0));

        let mut failed = Script {
            waits: [Err(NotReady::Failed("did not accept connections".into()))].into(),
            ..Script::default()
        };
        let started = started_ready(&mut failed).await.unwrap();
        assert!(
            started
                .ready
                .unwrap_err()
                .to_string()
                .contains("did not accept connections")
        );
        assert!(started.first_crash.is_none());
        assert_eq!((failed.starts.len(), failed.discarded.len()), (1, 0));
    }

    #[tokio::test]
    async fn an_unconfirmed_removal_of_the_crashed_container_starts_no_second() {
        let mut run = Script {
            waits: [crashed("one")].into(),
            removal_fails: true,
            ..Script::default()
        };
        let error = started_ready(&mut run).await.err().expect("refused");
        assert!(
            matches!(&error, ProduceError::Cleanup(names) if names.len() == 1),
            "{error:?}"
        );
        assert_eq!(
            run.starts,
            [false],
            "a name that may still be held is not reused"
        );
    }

    #[tokio::test]
    async fn a_second_start_that_cannot_be_made_still_names_the_first_crash() {
        let mut run = Script {
            waits: [crashed("one")].into(),
            second_start_fails: true,
            ..Script::default()
        };
        let error = started_ready(&mut run)
            .await
            .err()
            .expect("refused")
            .to_string();
        assert!(
            error.contains("no port") && error.contains("crash one"),
            "{error}"
        );
    }
}
