//! SQL Server readiness diagnostics shared by preview and resolver acquisition.

use pbps_db::Conn;
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

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

pub(super) async fn ready(connection: &str, id: &str, password: &str) -> anyhow::Result<()> {
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
                let logs =
                    runtime_output(&["logs", "--tail", "20", id], runtime_budget(started)).await;
                let tail = match logs {
                    Ok(out) => format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    ),
                    Err(error) => format!("container logs unavailable: {error}"),
                };
                anyhow::bail!(
                    "{}",
                    startup_diagnostic(
                        &format!(
                            "the scratch container stopped before SQL was ready: {}; container log tail:\n{tail}",
                            observed.describe()
                        ),
                        password
                    )
                );
            }
            Poll::TimedOut => anyhow::bail!(
                "{}",
                startup_diagnostic(
                    &format!(
                        "the scratch container did not accept connections within {}s; last SQL error: {}; last runtime state: {}",
                        READY_TIMEOUT.as_secs(),
                        answer.unwrap_err(),
                        observed.describe()
                    ),
                    password
                )
            ),
            Poll::Wait => {}
        }
        tokio::time::sleep(
            std::time::Duration::from_secs(2).min(READY_TIMEOUT.saturating_sub(started.elapsed())),
        )
        .await;
    }
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
}
