//! Real kernel pairing with only explicitly owned proc directories visible.
//! No host PID namespace or Docker socket enters the inspecting container.

use super::*;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, Output};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const INNER: &str = "resolver::docker::session::kernel_tests::the_actual_private_backend_and_controls_remain_bound_during_compilation";
const READER_IMAGE: &str =
    "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";

struct Pipes {
    input: tokio::io::unix::AsyncFd<std::fs::File>,
    output: tokio::io::unix::AsyncFd<std::fs::File>,
}

impl AsyncRead for Pipes {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            let mut ready = std::task::ready!(self.input.poll_read_ready(cx))?;
            match ready
                .try_io(|fd| std::io::Read::read(&mut fd.get_ref(), buf.initialize_unfilled()))
            {
                Ok(Ok(count)) => {
                    buf.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => (),
            }
        }
    }
}

impl AsyncWrite for Pipes {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        loop {
            let mut ready = std::task::ready!(self.output.poll_write_ready(cx))?;
            if let Ok(result) = ready.try_io(|fd| std::io::Write::write(&mut fd.get_ref(), data)) {
                return Poll::Ready(result);
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn driver() -> Driver {
    match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit supported fixture engine required"),
    }
}

#[tokio::test]
#[ignore = "inner inspector; requires this fixture's bounded proc mounts and protocol pipes"]
async fn the_actual_private_backend_and_controls_remain_bound_during_compilation() {
    let driver = driver();
    let password = std::env::var("PBPS_KERNEL_PASSWORD").unwrap();
    let workload = std::env::var("PBPS_KERNEL_WORKLOAD_PID")
        .unwrap()
        .parse()
        .unwrap();
    let control = std::env::var("PBPS_KERNEL_CONTROL_PID")
        .unwrap()
        .parse()
        .unwrap();
    use std::os::unix::fs::OpenOptionsExt as _;
    // Nonblocking pipes make driver cancellation close immediately; a blocking
    // filesystem read would keep Tokio's worker alive after the test returns.
    let pipes = Pipes {
        input: tokio::io::unix::AsyncFd::new(
            std::fs::File::options()
                .read(true)
                .custom_flags(0x800)
                .open("/proc/self/fd/3")
                .unwrap(),
        )
        .unwrap(),
        output: tokio::io::unix::AsyncFd::new(
            std::fs::File::options()
                .write(true)
                .custom_flags(0x800)
                .open("/proc/self/fd/4")
                .unwrap(),
        )
        .unwrap(),
    };
    let mut connection = StreamConn::connect(driver, pipes, engine::login(driver, password))
        .await
        .unwrap();
    let identity = engine::identity(&mut connection).await.unwrap();
    let workload_limits = ExecutionLease::capture(workload, engine::workload_limits(driver))
        .expect("workload kernel limits");
    let control_limits = ExecutionLease::capture(control, engine::control_limits(driver))
        .expect("control kernel limits");
    let channel = PrivateChannelLease::capture(
        workload,
        control,
        connection.id(),
        &identity,
        engine::private_channel_profile(driver),
    )
    .expect("actual backend and private forwarder socket owners");
    assert!(
        PrivateChannelLease::capture(
            control,
            workload,
            connection.id(),
            &identity,
            engine::private_channel_profile(driver)
        )
        .is_err(),
        "another process tree cannot substitute for the backend"
    );
    assert!(
        channel
            .separate_from(&crate::resolver::native::ProcessLease::capture(workload).unwrap())
            .is_err(),
        "another database on this runtime cannot be a separate target"
    );
    connection
        .execute("CREATE TABLE pbps_kernel_table (id integer NOT NULL)")
        .await
        .unwrap();
    connection
        .execute("CREATE VIEW pbps_kernel_view AS SELECT id FROM pbps_kernel_table")
        .await
        .unwrap();
    let rows = connection
        .query("SELECT COUNT(*) AS rows FROM pbps_kernel_view")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let after = engine::identity(&mut connection).await.unwrap();
    assert_eq!(identity, after);
    channel.check(connection.id(), &after).unwrap();
    workload_limits.check().unwrap();
    control_limits.check().unwrap();
}

fn command(host: &str, args: &[&str]) -> Result<Output, String> {
    let output = Command::new("docker")
        .args(["--host", host])
        .args(args)
        .output()
        .map_err(|_| "fixture Docker command failed")?;
    if !output.status.success() {
        return Err(format!(
            "fixture Docker {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output)
}

async fn inspect_pair(
    host: &str,
    path: &std::path::Path,
    driver: Driver,
    workload: &CandidateRun,
    control: &CandidateRun,
    password: &str,
) -> Result<String, String> {
    let mut backend = LocalApi::connect(path)
        .await
        .map_err(|e| e.to_string())?
        .attach_inner(control.container_id())
        .await
        .map_err(|e| e.to_string())?;
    backend
        .write_all(b"pbps-control-v1\n")
        .await
        .map_err(|_| "control probe failed")?;
    backend.flush().await.map_err(|_| "control flush failed")?;
    // The fixed forwarder has exactly a shell and two cat children after its
    // one connection opens. No SQL declaration is sent while waiting for it.
    let mut ready = false;
    for _ in 0..65 {
        let top = command(host, &["top", control.container_id(), "-eo", "pid,comm"])?;
        if String::from_utf8_lossy(&top.stdout)
            .lines()
            .filter(|line| line.split_whitespace().nth(1) == Some("cat"))
            .count()
            == 2
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !ready {
        return Err("the fixed forwarder did not connect".into());
    }
    let mut pids = std::collections::BTreeSet::new();
    for run in [workload, control] {
        let top = command(host, &["top", run.container_id(), "-eo", "pid"])?;
        for line in String::from_utf8_lossy(&top.stdout).lines().skip(1) {
            pids.insert(
                line.trim()
                    .parse::<u32>()
                    .map_err(|_| "invalid owned fixture PID")?,
            );
        }
    }
    if pids.len() > 1024
        || !pids.contains(&workload.native_pid().map_err(|e| e.to_string())?)
        || !pids.contains(&control.native_pid().map_err(|e| e.to_string())?)
    {
        return Err("invalid owned fixture process scope".into());
    }
    let name = format!("pbps-pair-reader-{}", token());
    let mut create = Command::new("docker");
    create.args([
        "--host",
        host,
        "create",
        "--name",
        &name,
        "--pull",
        "never",
        "--interactive",
        "--network",
        "none",
        "--cgroupns",
        "host",
        "--cap-drop",
        "ALL",
        "--cap-add",
        "SYS_PTRACE",
        "--cap-add",
        "DAC_READ_SEARCH",
        "--security-opt",
        "no-new-privileges",
        "--memory",
        "768m",
        "--cpus",
        "1",
        "--pids-limit",
        "256",
    ]);
    // Only proc directories belonging to these two freshly created resources
    // are mounted. The inspector cannot enumerate host processes, see a host
    // root, or access a runtime socket. It never writes these proc mounts.
    for pid in pids {
        create.args([
            "--mount",
            &format!("type=bind,src=/proc/{pid},dst=/pbps-owned-proc/{pid},readonly"),
        ]);
    }
    for (key, value) in [
        ("PBPS_NATIVE_OWNED_PROC_FIXTURE", "1".into()),
        (
            "PBPS_KERNEL_WORKLOAD_PID",
            workload.native_pid().unwrap().to_string(),
        ),
        (
            "PBPS_KERNEL_CONTROL_PID",
            control.native_pid().unwrap().to_string(),
        ),
        ("PBPS_KERNEL_PASSWORD", password.to_owned()),
        (
            "PBPS_RESOLVER_TEST_DRIVER",
            if driver == Driver::Postgres {
                "pg"
            } else {
                "mssql"
            }
            .into(),
        ),
    ] {
        create.args(["-e", &format!("{key}={value}")]);
    }
    create.args(["--entrypoint", "/usr/bin/timeout", READER_IMAGE, "--signal=KILL", "60s", "/bin/bash", "-ec", &format!("IFS= read -r ready; test \"$ready\" = pbps-kernel-reader-v1; exec 3<&0 4>&1; status=0; /usr/bin/timeout --signal=KILL 45s /pbps-kernel-tests --ignored --exact {INNER} --nocapture >/pbps-result 2>&1 || status=$?; printf '%s' \"$status\" >/pbps-exit; sleep 10")]);
    let created = create.output().map_err(|_| "cannot create inspector")?;
    if !created.status.success() {
        return Err(format!(
            "cannot create inspector: {}",
            String::from_utf8_lossy(&created.stderr)
        ));
    }
    let reader = String::from_utf8(created.stdout)
        .map_err(|_| "invalid inspector ID")?
        .trim()
        .to_owned();
    let result = async {
        let binary = std::env::current_exe().map_err(|_| "cannot locate test binary")?;
        command(
            host,
            &[
                "cp",
                binary.to_str().ok_or("invalid binary path")?,
                &format!("{reader}:/pbps-kernel-tests"),
            ],
        )?;
        command(host, &["start", &reader])?;
        let mut peer = LocalApi::connect(path)
            .await
            .map_err(|e| e.to_string())?
            .attach_inner(&reader)
            .await
            .map_err(|e| e.to_string())?;
        peer.write_all(b"pbps-kernel-reader-v1\n")
            .await
            .map_err(|_| "inspector probe failed")?;
        peer.flush()
            .await
            .map_err(|_| "inspector probe flush failed")?;
        let relay = tokio::io::copy_bidirectional(&mut peer, &mut backend);
        let completion = async {
            loop {
                if let Ok(status) = command(host, &["exec", &reader, "/bin/cat", "/pbps-exit"]) {
                    let result = command(host, &["exec", &reader, "/bin/cat", "/pbps-result"])?;
                    let output =
                        String::from_utf8(result.stdout).map_err(|_| "invalid test result")?;
                    if status.stdout != b"0" || !output.contains("test result: ok. 1 passed") {
                        return Err(output);
                    }
                    return Ok(output);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(50), async {
            tokio::select! {
                output = completion => output,
                _ = relay => Err("inspector protocol ended before the test completed".into()),
            }
        })
        .await
        .map_err(|_| "inspector did not complete".to_owned())?
    }
    .await;
    let removed = command(host, &["rm", "--force", "--volumes", &reader]);
    removed?;
    result
}

#[tokio::test]
#[ignore = "requires explicit owned Docker fixtures; inspector receives only those resources' proc directories"]
async fn private_channel_kernel_pairing_uses_only_owned_process_mounts() {
    let path = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let mut api = LocalApi::connect(&path).await.unwrap();
    let image = api.inspect_image(&image).await.unwrap().unwrap();
    let driver = driver();
    let owner = token();
    let password = format!("Pbps!{:032x}", rand::random::<u128>());
    let launch = Launch::with_password(&image, driver, &owner, &password).unwrap();
    let workload = CandidateRun::start_launch(
        api,
        image.clone(),
        owner,
        launch,
        super::super::profile::LIFETIME_SECS,
    )
    .await
    .unwrap();
    let owner = token();
    let launch = Launch::control(
        &image,
        driver,
        &owner,
        workload.container_id(),
        super::super::profile::LIFETIME_SECS,
    )
    .unwrap();
    let control = CandidateRun::start_launch(
        LocalApi::connect(&path).await.unwrap(),
        image.clone(),
        owner,
        launch,
        super::super::profile::LIFETIME_SECS,
    )
    .await;
    let control = match control {
        Ok(control) => control,
        Err(error) => {
            workload.close().await.unwrap();
            // `{error:?}` and not `{error}`: `StartFailure`'s `Display` is its
            // cause alone, and the fixture reads `recovery_names` out of this
            // panic to report and remove a container whose cleanup could not
            // be confirmed. Printed by name, that container is neither
            // reported nor removed (#724).
            panic!("{error:?}");
        }
    };
    let result = inspect_pair(
        &format!("unix://{}", path.display()),
        &path,
        driver,
        &workload,
        &control,
        &password,
    )
    .await;
    let closed_control = control.close().await;
    let closed_workload = workload.close().await;
    closed_control.unwrap();
    closed_workload.unwrap();
    assert!(result.is_ok(), "{}", result.unwrap_err());
}
