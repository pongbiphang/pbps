use super::*;

fn profile() -> ExecutionProfile {
    let (storage_path, storage_bytes) = match std::env::var("PBPS_TEST_EXECUTION_ENGINE")
        .expect("explicit owned fixture engine")
        .as_str()
    {
        "pg" => ("/var/lib/postgresql", 268435456),
        "mssql" => ("/var/opt/mssql", 1073741824),
        _ => panic!("unsupported fixture engine"),
    };
    ExecutionProfile {
        memory: 3221225472,
        nano_cpus: 2000000000,
        pids: 512,
        storage_path,
        storage_bytes,
    }
}

#[test]
#[ignore = "read-only inspector for one explicitly owned native root guard; launched by the outer fixture"]
fn actual_owned_root_limits_and_visible_mounts_match_the_profile() {
    let pid = std::env::var("PBPS_TEST_EXECUTION_PID")
        .expect("explicit owned root guard PID")
        .parse()
        .unwrap();
    let lease = ExecutionLease::capture(pid, profile())
        .expect("actual kernel execution controls must match");
    lease.check().unwrap();
    let mut wrong = profile();
    wrong.memory += 1;
    assert!(
        ExecutionLease::capture(pid, wrong).is_err(),
        "reported limits cannot replace the actual cgroup limit"
    );
    let mut wrong = profile();
    wrong.storage_bytes += 1;
    assert!(
        ExecutionLease::capture(pid, wrong).is_err(),
        "reported storage cannot replace the actual visible tmpfs mount"
    );
    let observed = read_bounded(
        &proc_base(&lease.process.directory).join("mountinfo"),
        1024 * 1024,
    )
    .unwrap();
    let missing: String = observed
        .lines()
        .filter(|line| {
            !line
                .split_whitespace()
                .nth(4)
                .is_some_and(|path| path == "/proc/sysrq-trigger")
        })
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(
        check_mounts(&missing, &profile()).is_err(),
        "an absent required control is unknown"
    );
}

#[tokio::test]
#[ignore = "requires Docker and explicit stock image; inspector reads only this fixture's root guard and cgroup"]
async fn owned_root_guard_controls_are_measured_on_the_native_kernel() {
    use crate::resolver::docker::{CandidateRun, LocalApi};
    use pbps_db::Driver;
    use std::process::Command;
    let socket = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let engine = std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap();
    let driver = match engine.as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("unsupported fixture"),
    };
    let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let mut api = LocalApi::connect(&socket).await.unwrap();
    let image = api
        .inspect_image(&image)
        .await
        .unwrap()
        .expect("explicit preloaded image");
    let run = CandidateRun::start(api, image, driver).await.unwrap();
    let host = format!("unix://{}", socket.display());
    let inspected = Command::new("docker")
        .args([
            "--host",
            &host,
            "inspect",
            "--format",
            "{{.State.Pid}}",
            run.container_id(),
        ])
        .output()
        .unwrap();
    assert!(inspected.status.success());
    let pid = String::from_utf8(inspected.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(pid.parse::<u32>().is_ok_and(|pid| pid > 0));
    let name = format!("pbps-kernel-reader-{:032x}", rand::random::<u128>());
    // No host bind, runtime socket, DAC capability, external network or
    // arbitrary process enumeration enters the inspector. SYS_PTRACE permits
    // reading this known root-owned guard's proc metadata from its ancestor
    // PID namespace. The cgroup filesystem remains Docker's read-only mount.
    let created = Command::new("docker").args(["--host", &host, "create", "--name", &name,
        "--pull", "never", "--network", "none", "--pid", "host", "--cgroupns", "host",
        "--cap-drop", "ALL", "--cap-add", "SYS_PTRACE", "--security-opt", "no-new-privileges",
        "--memory", "768m", "--cpus", "1", "--pids-limit", "256",
        "-e", &format!("PBPS_TEST_EXECUTION_PID={pid}"), "-e", &format!("PBPS_TEST_EXECUTION_ENGINE={engine}"),
        "--entrypoint", "/usr/bin/timeout", "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
        "--signal=KILL", "30s", "/pbps-kernel-tests", "--ignored", "--exact",
        "resolver::native::execution::tests::actual_owned_root_limits_and_visible_mounts_match_the_profile", "--nocapture"
    ]).output().unwrap();
    if !created.status.success() {
        run.close().await.unwrap();
        panic!("cannot create owned kernel inspector");
    }
    let reader = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    let copied = Command::new("docker")
        .args(["--host", &host, "cp"])
        .arg(std::env::current_exe().unwrap())
        .arg(format!("{reader}:/pbps-kernel-tests"))
        .output()
        .unwrap();
    let tested = if copied.status.success() {
        Some(
            Command::new("docker")
                .args(["--host", &host, "start", "--attach", &reader])
                .output()
                .unwrap(),
        )
    } else {
        None
    };
    let removed = Command::new("docker")
        .args(["--host", &host, "rm", "--force", "--volumes", &reader])
        .output()
        .unwrap();
    let closed = run.close().await;
    assert!(removed.status.success());
    closed.unwrap();
    let tested = tested.expect("the fixture binary must be copied into the owned inspector");
    assert!(
        tested.status.success(),
        "native inspector failed: {}",
        String::from_utf8_lossy(&tested.stdout)
    );
    assert!(String::from_utf8_lossy(&tested.stdout).contains("1 passed"));
}

#[test]
fn descriptor_limits_require_one_finite_bounded_soft_and_hard_pair() {
    for pair in ["1024 1024", "512 1024", "512 512", "0 0"] {
        assert!(
            check_descriptor_limits(&format!(
                "Limit Soft Limit Hard Limit Units\nMax open files    {pair} files\n"
            ))
            .is_ok()
        );
    }
    for row in [
        "",
        "Max open files",
        "Max open files 1024 files",
        "Max open files 1024 1024",
        "Max open files 1024 2048 files",
        "Max open files 2048 2048 files",
        "Max open files 1024 512 files",
        "Max open files 1024 unlimited files",
        "Max open files unlimited unlimited files",
        "Max open files -1 1024 files",
        "Max open files +1 1024 files",
        "Max open files 1 18446744073709551616 files",
        "Max open files 1 1024 bytes",
        "Max open files 1 1024 files extra",
        "Max open files 1024 1024 files\nMax open files 1024 1024 files",
        "Max open files 1024 1024 files\nMax open files unlimited unlimited files",
    ] {
        assert!(check_descriptor_limits(row).is_err(), "{row:?}");
    }
}

#[test]
fn a_held_process_cannot_supply_limits_after_it_exits() {
    use rustix::process::{Pid, Resource, Rlimit, prlimit};
    // Capture only after exec, as the other native fixtures do (#638).
    // Widen the transition so removing that wait exposes the wrong program
    // instead of relying on the rare fork/exec window observed in CI.
    let mut child = super::super::spawned_and_execed(
        std::process::Command::new("/bin/bash").args(["-ec", "sleep 0.05; exec /usr/bin/sleep 30"]),
        "sleep",
    );
    let pid = child.id();
    let lowered = prlimit(
        Some(Pid::from_raw(pid.try_into().unwrap()).unwrap()),
        Resource::Nofile,
        Rlimit {
            current: Some(512),
            maximum: Some(512),
        },
    );
    let process = ProcessLease::capture(pid);
    let running_sleep = process
        .as_ref()
        .is_ok_and(|process| process.executable_path().ends_with("sleep"));
    let before = process.as_ref().ok().map(check_file_descriptors);
    child.kill().unwrap();
    child.wait().unwrap();
    lowered.unwrap();
    assert!(
        running_sleep,
        "the fixture must capture the executed program"
    );
    before.unwrap().expect("a live, tighter limit is bounded");
    let process = process.unwrap();
    assert!(process.check().is_err());
    assert!(
        check_file_descriptors(&process).is_err(),
        "held proc evidence must not survive process loss"
    );
}
