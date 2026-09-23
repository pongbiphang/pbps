//! A matching Docker report and filter mode must not certify wrong rules.

use super::*;
use crate::resolver::native::ProcessLease;
use hyper::{Method, StatusCode};

#[derive(Clone, Copy, Debug)]
enum Mutation {
    None,
    Connect,
    Process,
    FastOpen,
    CompatConnect,
    Default,
    MissingProbe,
}

async fn probe_accepts(mutation: Mutation) -> bool {
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
    let attach = LocalApi::connect(std::path::Path::new(&socket))
        .await
        .unwrap();
    let mut observer = LocalApi::connect(std::path::Path::new(&socket))
        .await
        .unwrap();
    let owner = format!("{:032x}", rand::random::<u128>());
    let expected = Launch::reserved(&image, driver, &owner, "Pbps!SeccompFixture633").unwrap();
    let mut launch = Launch {
        body: expected.body.clone(),
    };
    match mutation {
        Mutation::None => {}
        Mutation::Connect | Mutation::Process | Mutation::FastOpen | Mutation::CompatConnect => {
            let mut policy: Value = serde_json::from_str(SECCOMP).unwrap();
            let rule = match mutation {
                Mutation::Connect => json!({"names":["connect"],"action":"SCMP_ACT_ALLOW"}),
                Mutation::Process => {
                    json!({"names":["ptrace","process_vm_readv","process_vm_writev"],"action":"SCMP_ACT_ALLOW"})
                }
                Mutation::FastOpen => {
                    json!({"names":["sendto","sendmsg","sendmmsg"],"action":"SCMP_ACT_ALLOW"})
                }
                Mutation::CompatConnect => {
                    json!({"names":["socketcall"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":3,"op":"SCMP_CMP_EQ"}]})
                }
                Mutation::None | Mutation::Default | Mutation::MissingProbe => unreachable!(),
            };
            policy["syscalls"].as_array_mut().unwrap().push(rule);
            launch.body["HostConfig"]["SecurityOpt"] =
                json!(["no-new-privileges", format!("seccomp={policy}")]);
        }
        Mutation::Default => {
            launch.body["HostConfig"]["SecurityOpt"] = json!(["no-new-privileges"]);
        }
        Mutation::MissingProbe => {
            let program = launch.body["Cmd"]
                .as_array_mut()
                .unwrap()
                .last_mut()
                .unwrap();
            *program = json!(
                program
                    .as_str()
                    .unwrap()
                    .replace("/usr/bin/perl", "/pbps-missing-probe")
            );
        }
    }
    let run = CandidateRun::start_launch(api, image, owner, launch, LIFETIME_SECS)
        .await
        .unwrap();
    let id = run.container_id().to_owned();
    let result = async {
        let mut stream = attach.attach_inner(&id).await.unwrap();
        stream
            .write_all(b"pbps-bootstrap-probe-v1\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let mut ready = [0; b"pbps-bootstrap-ready-v1\n".len()];
        stream.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"pbps-bootstrap-ready-v1\n");
        // The old native mode/credential check admits all these wrong filters.
        // This is measured on the actual container, not a fabricated status.
        let process = awaiting_engine(
            run.native_pid().unwrap(),
            &engine::private_channel_profile(driver),
        )
        .unwrap();
        let status = process.read_proc("status", 65536).unwrap();
        assert!(
            status
                .lines()
                .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["Seccomp:", "2"])
        );
        let storage = engine::workload_limits(driver)
            .storage_path
            .trim_start_matches('/');
        assert!(process.read_root_dir(storage).unwrap().is_empty());
        run.check().await.unwrap();
        if !matches!(mutation, Mutation::MissingProbe) {
            let (status, bytes) = observer
                .request(Method::GET, &format!("/v1.47/containers/{id}/json"))
                .await
                .unwrap();
            assert_eq!(status, StatusCode::OK);
            let mut reported: Value = serde_json::from_slice(&bytes).unwrap();
            // Simulate a runtime reporting the requested derivative while the
            // measured container actually executes the altered filter.
            reported["HostConfig"]["SecurityOpt"] =
                expected.body["HostConfig"]["SecurityOpt"].clone();
            expected.check_configuration(&reported).unwrap();
        }
        stream.write_all(b"pbps-seccomp-probe-v1\n").await.unwrap();
        stream.flush().await.unwrap();
        let mut policy_ready = [0; b"pbps-seccomp-ready-v1\n".len()];
        let accepted = matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream.read_exact(&mut policy_ready)
            )
            .await,
            Ok(Ok(_))
        ) && &policy_ready == b"pbps-seccomp-ready-v1\n";
        if accepted {
            // No start message is sent: even a successful policy check leaves
            // initialization behind its own unopened gate.
            assert!(
                ProcessLease::capture(run.native_pid().unwrap())
                    .unwrap()
                    .read_root_dir(storage)
                    .unwrap()
                    .is_empty()
            );
        }
        accepted
    }
    .await;
    run.close().await.unwrap();
    assert_eq!(
        observer
            .request(Method::GET, &format!("/v1.47/containers/{id}/json"))
            .await
            .unwrap()
            .0,
        StatusCode::NOT_FOUND
    );
    result
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn reported_policy_and_filter_mode_cannot_replace_effective_bootstrap_probes() {
    assert!(
        probe_accepts(Mutation::None).await,
        "the ordinary policy must permit contained startup"
    );
    let mut accepted = Vec::new();
    for mutation in [
        Mutation::Connect,
        Mutation::Process,
        Mutation::FastOpen,
        Mutation::CompatConnect,
        Mutation::Default,
        Mutation::MissingProbe,
    ] {
        let actual = probe_accepts(mutation).await;
        eprintln!("workload policy {mutation:?}: accepted={actual}");
        if actual {
            accepted.push(mutation);
        }
    }
    assert!(
        accepted.is_empty(),
        "wrong or unanswerable policies crossed the pre-initialization gate: {accepted:?}"
    );
}

async fn control_probe_accepts(mutation: Mutation) -> bool {
    let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
    let reference = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit supported engine required"),
    };
    let path = std::path::Path::new(&socket);
    let mut api = LocalApi::connect(path).await.unwrap();
    let image = api.inspect_image(&reference).await.unwrap().unwrap();
    let owner = format!("{:032x}", rand::random::<u128>());
    let launch = Launch::reserved(&image, driver, &owner, "Pbps!SeccompFixture633").unwrap();
    let workload = CandidateRun::start_launch(api, image.clone(), owner, launch, LIFETIME_SECS)
        .await
        .unwrap();
    let owner = format!("{:032x}", rand::random::<u128>());
    let mut launch = Launch::control(
        &image,
        driver,
        &owner,
        workload.container_id(),
        LIFETIME_SECS,
    )
    .unwrap();
    let program = launch.body["Cmd"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    // Keep the actual control probe and dropped identity. Replace only the
    // database exchange: this source-free case must never initialize an engine.
    *program = json!(program.as_str().unwrap().replace(
        engine::control_program(driver),
        "IFS= read -r probe; test \"$probe\" = pbps-control-probe-v1; printf 'pbps-control-probe-ready-v1\\n'; IFS= read -r hold"
    ));
    match mutation {
        Mutation::None => {}
        Mutation::FastOpen | Mutation::Process => {
            let policy_text = launch.body["HostConfig"]["SecurityOpt"][1]
                .as_str()
                .unwrap()
                .strip_prefix("seccomp=")
                .unwrap();
            let mut policy: Value = serde_json::from_str(policy_text).unwrap();
            let names = if matches!(mutation, Mutation::FastOpen) {
                vec!["sendto", "sendmsg", "sendmmsg"]
            } else {
                vec!["ptrace", "process_vm_readv", "process_vm_writev"]
            };
            policy["syscalls"]
                .as_array_mut()
                .unwrap()
                .push(json!({"names":names,"action":"SCMP_ACT_ALLOW"}));
            launch.body["HostConfig"]["SecurityOpt"] =
                json!(["no-new-privileges", format!("seccomp={policy}")]);
        }
        Mutation::MissingProbe => {
            let program = launch.body["Cmd"]
                .as_array_mut()
                .unwrap()
                .last_mut()
                .unwrap();
            *program = json!(
                program
                    .as_str()
                    .unwrap()
                    .replace("/usr/bin/perl", "/pbps-missing-probe")
            );
        }
        Mutation::Connect | Mutation::CompatConnect | Mutation::Default => unreachable!(),
    }
    let control = CandidateRun::start_launch(
        LocalApi::connect(path).await.unwrap(),
        image,
        owner,
        launch,
        LIFETIME_SECS,
    )
    .await;
    let accepted = match control {
        Ok(control) => {
            let observed = async {
                let mut stream = LocalApi::connect(path)
                    .await?
                    .attach_inner(control.container_id())
                    .await?;
                stream
                    .write_all(b"pbps-control-probe-v1\n")
                    .await
                    .map_err(|_| Error::ControlLost)?;
                stream.flush().await.map_err(|_| Error::ControlLost)?;
                let mut ready = [0; b"pbps-control-probe-ready-v1\n".len()];
                stream
                    .read_exact(&mut ready)
                    .await
                    .map_err(|_| Error::ControlLost)?;
                Ok::<_, Error>(&ready == b"pbps-control-probe-ready-v1\n")
            };
            let accepted = matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), observed).await,
                Ok(Ok(true))
            );
            control.close().await.unwrap();
            accepted
        }
        Err(failure) => {
            assert!(failure.recovery_names.is_empty(), "{failure:?}");
            false
        }
    };
    workload.close().await.unwrap();
    accepted
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn the_owned_forwarder_requires_its_effective_process_and_fastopen_restrictions() {
    assert!(
        control_probe_accepts(Mutation::None).await,
        "the ordinary forwarder policy must pass"
    );
    let mut accepted = Vec::new();
    for mutation in [
        Mutation::Process,
        Mutation::FastOpen,
        Mutation::MissingProbe,
    ] {
        let actual = control_probe_accepts(mutation).await;
        eprintln!("forwarder policy {mutation:?}: accepted={actual}");
        if actual {
            accepted.push(mutation);
        }
    }
    assert!(
        accepted.is_empty(),
        "the forwarder accepted missing restrictions: {accepted:?}"
    );
}
