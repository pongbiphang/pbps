use super::*;
use pbps_config::resolver::{PullPolicy, ResolverProfile};
use pbps_db::transport::PeerVerifiedConn;

#[tokio::test]
#[ignore = "requires a disposable native Linux host, direct root Docker channel and an explicitly owned loopback TLS target"]
async fn native_factory_qualifies_before_bootstrap_and_rejects_rebound_target_connections() {
    assert_eq!(
        std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref(),
        Ok("1")
    );
    let driver = match std::env::var("PBPS_NATIVE_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit fixture engine required"),
    };
    let socket = std::path::PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let primary = std::env::var("PBPS_NATIVE_CONNECTION").unwrap();
    let service = std::env::var("PBPS_NATIVE_SERVICE_PID")
        .unwrap()
        .parse()
        .unwrap();
    let profile = ResolverProfile::Docker {
        image: std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap(),
        pull: PullPolicy::Never,
    };
    let mut stale_identities = Vec::new();
    for action in [
        "close",
        "rebound",
        "drop-target",
        "kill-control",
        "kill-workload",
        "control-limit",
        "workload-limit",
        "control-nofile",
        "workload-nofile",
        "control-child-nofile",
        "workload-child-nofile",
    ] {
        let peer = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
        let mut target = Some(NativeTarget::establish(peer, service).await.unwrap());
        let mut api = LocalApi::connect_native(&socket).await.unwrap();
        let image = api.acquire(&profile).await.unwrap();
        let different = LocalApi::connect_native(&socket).await.unwrap();
        assert!(
            matches!(
                super::super::ReservedSession::reserve(different, image.clone(), driver).await,
                Err(StartFailure {
                    cause: Error::Profile,
                    ..
                })
            ),
            "image provenance cannot move to another API connection"
        );
        let mut session = CandidateSession::start(api, image, target.as_mut().unwrap())
            .await
            .expect("the complete native factory must pass its real startup and backend gates");
        session.check().await.unwrap();
        assert!(session.identity().is_ok(), "unchanged live candidate");
        assert!(target.as_mut().unwrap().identity().is_ok());
        session
            .verify_target_separation(target.as_mut().unwrap())
            .await
            .unwrap();
        let state = session.state.as_mut().unwrap();
        assert!(state.native.is_some());
        assert!(state.target.is_some());
        let resources = [
            state.control.container_id().to_owned(),
            state.workload.container_id().to_owned(),
        ];
        state
            .connection
            .execute("CREATE TABLE pbps_native_factory_table (id integer)")
            .await
            .unwrap();
        state
            .connection
            .execute(
                "CREATE VIEW pbps_native_factory_view AS SELECT id FROM pbps_native_factory_table",
            )
            .await
            .unwrap();
        session.check().await.unwrap();
        if action.starts_with("kill-") || action.ends_with("-limit") || action.ends_with("-nofile")
        {
            let state = session.state.as_ref().unwrap();
            let run = if action.contains("control") {
                &state.control
            } else {
                &state.workload
            };
            if action.starts_with("kill-") {
                let mut observer = LocalApi::connect_native(&socket).await.unwrap();
                let (status, _) = observer
                    .request(
                        hyper::Method::POST,
                        &format!("/v1.47/containers/{}/kill?signal=KILL", run.container_id()),
                    )
                    .await
                    .unwrap();
                assert_eq!(status, hyper::StatusCode::NO_CONTENT);
            } else if action.ends_with("-nofile") {
                use crate::resolver::native::ProcessLease;
                use rustix::process::{Pid, Resource, Rlimit, prlimit};
                let root = run.native_pid().unwrap();
                // This fixture's fixed, source-free tree is quiescent here.
                // These IDs select owned mutation subjects, not a production
                // completeness claim or a substitute for namespace admission.
                let pids = if action.contains("-child-") {
                    let mut pending = vec![root];
                    let mut descendants = Vec::new();
                    while let Some(pid) = pending.pop() {
                        let process = ProcessLease::capture(pid).unwrap();
                        let children = process
                            .read_proc(&format!("task/{pid}/children"), 4096)
                            .unwrap();
                        for child in children
                            .split_whitespace()
                            .map(|pid| pid.parse::<u32>().unwrap())
                        {
                            assert!(!descendants.contains(&child));
                            descendants.push(child);
                            assert!(descendants.len() <= 512);
                            pending.push(child);
                        }
                    }
                    descendants
                } else {
                    vec![root]
                };
                assert!(!pids.is_empty());
                for pid in pids {
                    let process = ProcessLease::capture(pid).unwrap();
                    let before = process.read_proc("limits", 16384).unwrap();
                    let before = before
                        .lines()
                        .find(|line| line.starts_with("Max open files"))
                        .unwrap();
                    prlimit(
                        Some(Pid::from_raw(pid.try_into().unwrap()).unwrap()),
                        Resource::Nofile,
                        Rlimit {
                            current: Some(1024),
                            maximum: Some(2048),
                        },
                    )
                    .unwrap();
                    process.check().unwrap();
                    let after = process.read_proc("limits", 16384).unwrap();
                    let after = after
                        .lines()
                        .find(|line| line.starts_with("Max open files"))
                        .unwrap();
                    eprintln!(
                        "{action} {}: {before} -> {after}",
                        process.executable_path().display()
                    );
                }
                run.check()
                    .await
                    .expect("Docker's expected configuration still matches");
            } else {
                // Change the actual owned cgroup without changing Docker's
                // reported recipe, so only the native lease can notice it.
                let cgroup =
                    std::fs::read_to_string(format!("/proc/{}/cgroup", run.native_pid().unwrap()))
                        .unwrap();
                let relative = cgroup.trim().strip_prefix("0::/").unwrap();
                let path = std::path::Path::new("/sys/fs/cgroup")
                    .join(relative)
                    .join("pids.max");
                let previous: u64 = std::fs::read_to_string(&path)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                std::fs::write(&path, (previous + 1).to_string()).unwrap();
            }
            target.as_mut().unwrap().check().await.unwrap();
            // No CandidateSession::check may precede this access: that would
            // erase the stale cached capability this regression exercises.
            if session.identity().is_ok() {
                stale_identities.push(action);
            } else {
                assert!(session.identity().is_err(), "failure is terminal");
                assert!(session.check().await.is_err());
            }
            // Also clean up on the deliberately broken baseline, before the
            // assertion below, so a regression cannot strand its fixtures.
            if session.state.is_some() {
                let _ = session.close().await;
            }
        } else if action == "rebound" {
            let peer = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
            let mut replacement = NativeTarget::establish(peer, service).await.unwrap();
            assert!(
                target
                    .as_mut()
                    .unwrap()
                    .same_instance(&mut replacement)
                    .await
                    .unwrap()
            );
            assert!(
                session
                    .verify_target_separation(&mut replacement)
                    .await
                    .is_err()
            );
            assert!(session.identity().is_err());
            assert!(
                session.check().await.is_err(),
                "target replacement cannot revive partial work"
            );
        } else if action == "drop-target" {
            drop(target.take());
            assert!(session.identity().is_err());
            assert!(
                session.check().await.is_err(),
                "target loss must discard scratch without a separate target check"
            );
            assert!(
                session.check().await.is_err(),
                "the discarded run cannot resume"
            );
        } else {
            session.close().await.unwrap();
        }
        let mut observer = LocalApi::connect_native(&socket).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let mut absent = true;
                for id in &resources {
                    absent &= observer
                        .request(hyper::Method::GET, &format!("/v1.47/containers/{id}/json"))
                        .await
                        .unwrap()
                        .0
                        == hyper::StatusCode::NOT_FOUND;
                }
                if absent {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("only the complete run's two owned resources must be removed");
        if let Some(mut target) = target {
            target.check().await.unwrap();
        }
    }
    assert!(
        stale_identities.is_empty(),
        "direct identity access accepted expired runtime leases: {stale_identities:?}"
    );
}
