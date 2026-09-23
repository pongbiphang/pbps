//! Inspect the actual dropped waiter before any engine initialization.

use super::*;
use crate::resolver::docker::{CandidateRun, LocalApi};
use crate::resolver::native::awaiting_engine;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[path = "launch_tests/masks.rs"]
mod masks;

#[path = "launch_tests/seccomp.rs"]
mod seccomp;

async fn accepts_waiter(change: impl FnOnce(&mut Launch)) -> bool {
    inspect_waiter(change, |run, driver| {
        awaiting_engine(
            run.native_pid().unwrap(),
            &engine::private_channel_profile(driver),
        )
        .is_ok()
    })
    .await
}

async fn inspect_waiter(
    change: impl FnOnce(&mut Launch),
    inspect: impl FnOnce(&CandidateRun, Driver) -> bool,
) -> bool {
    inspect_waiter_with_file(change, inspect, None).await
}

async fn inspect_waiter_with_file(
    change: impl FnOnce(&mut Launch),
    inspect: impl FnOnce(&CandidateRun, Driver) -> bool,
    mutation: Option<(&str, &str)>,
) -> bool {
    let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
    let reference = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit supported engine required"),
    };
    let mut api = LocalApi::connect(std::path::Path::new(&socket))
        .await
        .unwrap();
    let image = api.inspect_image(&reference).await.unwrap().unwrap();
    let attach_api = LocalApi::connect(std::path::Path::new(&socket))
        .await
        .unwrap();
    let owner = format!("{:032x}", rand::random::<u128>());
    let mut launch = Launch::reserved(&image, driver, &owner, "Pbps!LaunchFixture742").unwrap();
    change(&mut launch);
    let run = CandidateRun::start_launch(api, image, owner, launch, LIFETIME_SECS)
        .await
        .unwrap();
    let observed = async {
        let mut stream = attach_api.attach_inner(run.container_id()).await?;
        stream
            .write_all(b"pbps-bootstrap-probe-v1\n")
            .await
            .map_err(|_| Error::Start)?;
        stream.flush().await.map_err(|_| Error::Start)?;
        let mut greeting = [0; b"pbps-bootstrap-ready-v1\n".len()];
        stream
            .read_exact(&mut greeting)
            .await
            .map_err(|_| Error::Start)?;
        if &greeting != b"pbps-bootstrap-ready-v1\n" {
            return Err(Error::Start);
        }
        // Never send the start line: refusal must precede initialization,
        // independently of any later database handshake or declaration.
        let changed = if let Some((path, mode)) = mutation {
            use crate::resolver::docker::file_fixture::{ChangedFile, mutations};
            let mut observer = LocalApi::connect(std::path::Path::new(&socket)).await?;
            let changed = ChangedFile::capture(&mut observer, run.container_id(), path).await;
            let (_, text) = mutations(path, changed.original())
                .into_iter()
                .find(|(name, _)| *name == mode)
                .expect("known fixture mutation");
            changed.replace(&text);
            Some(changed)
        } else {
            None
        };
        let accepted = inspect(&run, driver);
        drop(changed);
        // A host-side limit change must not alter Docker's reported recipe.
        run.check().await?;
        Ok(accepted)
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), observed).await;
    run.close().await.unwrap();
    result.unwrap().unwrap()
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn a_wrong_workload_identity_is_refused_before_initialization() {
    assert!(
        accepts_waiter(|_| {}).await,
        "ordinary dropped waiter qualifies"
    );
    for (flag, replacement) in [
        ("--reuid=", "--reuid=4242"),
        ("--regid=", "--regid=4242"),
        ("--clear-groups", "--groups=4242"),
        ("--bounding-set=", "add-setuid"),
    ] {
        let accepted = accepts_waiter(|launch| {
            let argument = launch.body["Cmd"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|arg| arg.as_str().is_some_and(|arg| arg.starts_with(flag)))
                .unwrap();
            *argument = if replacement == "add-setuid" {
                json!(format!("{},+setuid", argument.as_str().unwrap()))
            } else {
                json!(replacement)
            };
        })
        .await;
        assert!(
            !accepted,
            "the waiter exceeded its final identity or capability ceiling: {replacement}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn a_guard_without_termination_authority_is_refused_before_initialization() {
    let accepted = accepts_waiter(|launch| {
        launch.body["HostConfig"]["CapAdd"]
            .as_array_mut()
            .unwrap()
            .retain(|cap| cap != "KILL");
    })
    .await;
    assert!(
        !accepted,
        "a differently owned workload needs the root guard's effective CAP_KILL"
    );
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn the_launch_cannot_omit_inherited_no_new_privileges() {
    let accepted = accepts_waiter(|launch| {
        launch.body["HostConfig"]["SecurityOpt"]
            .as_array_mut()
            .unwrap()
            .retain(|option| option != "no-new-privileges");
    })
    .await;
    assert!(
        !accepted,
        "exec must not regain privileges after the checked drop"
    );
}

/// Changes only this fixture's held guard/waiter; the daemon still reports
/// the requested 1024:1024 profile throughout each observation.
#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture on its native host"]
async fn effective_descriptor_limits_are_required_before_initialization() {
    use crate::resolver::native::{ExecutionLease, ProcessLease};
    use rustix::process::{Pid, Resource, Rlimit, prlimit};

    let mut accepted_unbounded = Vec::new();
    for subject in ["guard", "waiter"] {
        let accepted = inspect_waiter(
            |_| {},
            |run, driver| {
                let pid = run.native_pid().unwrap();
                let root = ProcessLease::capture(pid).unwrap();
                let execution =
                    ExecutionLease::capture(pid, engine::workload_limits(driver)).unwrap();
                let limits = |process: &ProcessLease| {
                    process.check().unwrap();
                    let text = process.read_proc("limits", 16384).unwrap();
                    let row = text
                        .lines()
                        .find(|line| line.starts_with("Max open files"))
                        .unwrap()
                        .to_owned();
                    process.check().unwrap();
                    row
                };
                let children = root
                    .read_proc(&format!("task/{pid}/children"), 4096)
                    .unwrap();
                let children: Vec<u32> = children
                    .split_whitespace()
                    .map(|id| id.parse().unwrap())
                    .collect();
                assert_eq!(
                    children.len(),
                    1,
                    "the fixed waiter has not begun initialization"
                );
                let waiter = ProcessLease::capture(children[0]).unwrap();
                eprintln!(
                    "nofile before {subject}: guard={}, waiter={}",
                    limits(&root),
                    limits(&waiter)
                );
                assert!(awaiting_engine(pid, &engine::private_channel_profile(driver)).is_ok());
                let selected = if subject == "guard" { &root } else { &waiter };
                let selected_pid =
                    Pid::from_raw(selected.observer_pid().unwrap().try_into().unwrap()).unwrap();
                prlimit(
                    Some(selected_pid),
                    Resource::Nofile,
                    Rlimit {
                        current: Some(1024),
                        maximum: Some(2048),
                    },
                )
                .unwrap();
                selected.check().unwrap();
                eprintln!(
                    "nofile raised {subject}: guard={}, waiter={}",
                    limits(&root),
                    limits(&waiter)
                );
                if subject == "waiter" {
                    // Lowering a parent's limit does not change an already-forked
                    // child. A guard-only reading cannot certify the waiter.
                    prlimit(
                        Some(Pid::from_raw(pid.try_into().unwrap()).unwrap()),
                        Resource::Nofile,
                        Rlimit {
                            current: Some(512),
                            maximum: Some(512),
                        },
                    )
                    .unwrap();
                    eprintln!(
                        "nofile lowered parent: guard={}, waiter={}",
                        limits(&root),
                        limits(&waiter)
                    );
                    assert!(
                        execution.check().is_ok(),
                        "a tighter root ceiling is bounded"
                    );
                    awaiting_engine(pid, &engine::private_channel_profile(driver)).is_ok()
                } else {
                    execution.check().is_ok()
                }
            },
        )
        .await;
        if accepted {
            accepted_unbounded.push(subject);
        }
    }
    assert!(
        accepted_unbounded.is_empty(),
        "effective descriptor limits accepted: {accepted_unbounded:?}"
    );
}

#[tokio::test]
#[ignore = "requires the owned Docker fixture and a private observer mount namespace"]
async fn unreadable_effective_limits_are_not_a_bounded_answer() {
    use crate::resolver::native::{ExecutionLease, ProcessLease};
    use std::process::Command;
    assert_eq!(
        std::env::var("PBPS_LIMITS_PRIVATE_PROC_FIXTURE").as_deref(),
        Ok("1")
    );
    assert_ne!(
        std::fs::read_link("/proc/self/ns/mnt").unwrap(),
        std::fs::read_link("/proc/1/ns/mnt").unwrap()
    );
    let accepted = inspect_waiter(
        |_| {},
        |run, driver| {
            let pid = run.native_pid().unwrap();
            let process = ProcessLease::capture(pid).unwrap();
            let execution = ExecutionLease::capture(pid, engine::workload_limits(driver)).unwrap();
            let target = format!("/proc/{pid}/limits");
            // Only this observer's private mount namespace changes. clear_refs
            // has no read operation, while stat/exe/namespace identity stay live.
            assert!(
                Command::new("mount")
                    .args(["--bind", &format!("/proc/{pid}/clear_refs"), &target])
                    .status()
                    .unwrap()
                    .success()
            );
            let unreadable = process.read_proc("limits", 16384).is_err();
            let unchanged = process.check().is_ok();
            let accepted = execution.check().is_ok();
            assert!(
                Command::new("umount")
                    .arg(&target)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                unreadable && unchanged,
                "refusal must concern limits, not lost identity"
            );
            assert!(
                execution.check().is_ok(),
                "restoring the observer view restores the kernel evidence"
            );
            accepted
        },
    )
    .await;
    assert!(!accepted, "unreadable effective limits were admitted");
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn host_information_is_refused_before_engine_initialization() {
    let mut wrong = Vec::new();
    let mut cases = vec![None];
    for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/hostname"] {
        for (mode, _) in crate::resolver::docker::file_fixture::mutations(path, "") {
            cases.push(Some((path, mode)));
        }
    }
    for mutation in cases {
        let admits = inspect_waiter_with_file(
            |_| (),
            |run, driver| {
                let pid = run.native_pid().unwrap();
                crate::resolver::native::ExecutionLease::capture(
                    pid,
                    engine::workload_limits(driver),
                )
                .is_ok()
                    && awaiting_engine(pid, &engine::private_channel_profile(driver)).is_ok()
            },
            mutation,
        )
        .await;
        eprintln!("pre-initialization host files mutation={mutation:?}, admitted={admits}");
        if admits != mutation.is_none() {
            wrong.push(mutation);
        }
    }
    assert!(
        wrong.is_empty(),
        "wrong pre-initialization admission: {wrong:?}"
    );
}
