//! Change only owned fixtures' effective proc masks, retaining Docker's report.

use super::*;
use crate::resolver::native::{ExecutionLease, ProcessLease};
use std::path::Path;
use std::process::Command;

type FixtureResult<T> = Result<T, String>;

fn observation(
    run: &CandidateRun,
    driver: Driver,
    control: bool,
    mode: &str,
) -> FixtureResult<bool> {
    let pid = run
        .native_pid()
        .map_err(|error| format!("pid: {error:?}"))?;
    let process = ProcessLease::capture(pid).map_err(|_| "process capture")?;
    let limits = if control {
        engine::control_limits(driver)
    } else {
        engine::workload_limits(driver)
    };
    let execution =
        ExecutionLease::capture(pid, limits).map_err(|_| "initial execution capture")?;
    let stat = process.read_proc("stat", 65536).map_err(|_| "held stat")?;
    let start = stat
        .rsplit_once(')')
        .ok_or("stat delimiter")?
        .1
        .split_whitespace()
        .nth(19)
        .ok_or("start ticks")?;
    let result = Command::new("/usr/bin/python3")
        .arg("-c")
        .arg(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/resolver-mask-fixture.py"
        )))
        .arg(std::env::var("PBPS_RESOLVER_TEST_SOCKET").map_err(|_| "fixture socket")?)
        .arg(run.container_id())
        .arg(pid.to_string())
        .arg(start)
        .arg(mode)
        .status()
        .map_err(|error| format!("helper start: {error}"))?;
    if !result.success() {
        return Err(format!("owned mask helper failed: {result}"));
    }
    process.check().map_err(|_| "process continuity")?;
    let admitted = execution.check().is_ok();
    eprintln!(
        "proc mask mode={mode}, control={control}, admitted={admitted}, process identity retained"
    );
    Ok(admitted)
}

async fn inspect_mask(control: bool, mode: &str) -> FixtureResult<bool> {
    let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").map_err(|_| "fixture socket")?;
    let reference = std::env::var("PBPS_RESOLVER_TEST_IMAGE").map_err(|_| "fixture image")?;
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").as_deref() {
        Ok("pg") => Driver::Postgres,
        Ok("mssql") => Driver::Mssql,
        _ => return Err("explicit supported engine required".into()),
    };
    let mut api = LocalApi::connect(Path::new(&socket))
        .await
        .map_err(|error| format!("api: {error:?}"))?;
    let image = api
        .inspect_image(&reference)
        .await
        .map_err(|error| format!("image: {error:?}"))?
        .ok_or("missing fixture image")?;
    let attach = LocalApi::connect(Path::new(&socket))
        .await
        .map_err(|error| format!("attach api: {error:?}"))?;
    let owner = format!("{:032x}", rand::random::<u128>());
    let mut launch = Launch::reserved(&image, driver, &owner, "Pbps!MaskFixture629")
        .map_err(|error| format!("launch: {error:?}"))?;
    launch.body["Labels"]["io.pbps.resolver.mask-fixture"] = json!("v1");
    let workload = CandidateRun::start_launch(api, image.clone(), owner, launch, LIFETIME_SECS)
        .await
        .map_err(|error| format!("workload start: {error:?}"))?;
    let mut forwarder = None;
    let observe = async {
        let mut stream = attach
            .attach_inner(workload.container_id())
            .await
            .map_err(|error| format!("attach: {error:?}"))?;
        stream
            .write_all(b"pbps-bootstrap-probe-v1\n")
            .await
            .map_err(|error| error.to_string())?;
        stream.flush().await.map_err(|error| error.to_string())?;
        let mut ready = [0; b"pbps-bootstrap-ready-v1\n".len()];
        stream
            .read_exact(&mut ready)
            .await
            .map_err(|error| error.to_string())?;
        if &ready != b"pbps-bootstrap-ready-v1\n" {
            return Err("wrong bootstrap greeting".into());
        }
        // Do not initialize an engine: each mutation is isolated to a fresh
        // source-free pair. The ordinary compilation suite tests the positives.
        if control {
            let api = LocalApi::connect(Path::new(&socket))
                .await
                .map_err(|error| format!("control api: {error:?}"))?;
            let owner = format!("{:032x}", rand::random::<u128>());
            let mut launch = Launch::control(
                &image,
                driver,
                &owner,
                workload.container_id(),
                LIFETIME_SECS,
            )
            .map_err(|error| format!("control launch: {error:?}"))?;
            launch.body["Labels"]["io.pbps.resolver.mask-fixture"] = json!("v1");
            forwarder = Some(
                CandidateRun::start_launch(api, image, owner, launch, LIFETIME_SECS)
                    .await
                    .map_err(|error| format!("control start: {error:?}"))?,
            );
        }
        let run = forwarder.as_ref().unwrap_or(&workload);
        let result = observation(run, driver, control, mode);
        // Measure the actual mutation independently of the unchanged report.
        run.check()
            .await
            .map_err(|error| format!("unchanged recipe: {error:?}"))?;
        result
    };
    let observed = tokio::time::timeout(std::time::Duration::from_secs(30), observe).await;
    // Helper failure, rejection and timeout all follow ordinary owned cleanup
    // before any assertion can unwind this test.
    let control_cleanup = match forwarder {
        Some(run) => run
            .close()
            .await
            .map_err(|error| format!("control cleanup: {error:?}")),
        None => Ok(()),
    };
    let workload_cleanup = workload
        .close()
        .await
        .map_err(|error| format!("workload cleanup: {error:?}"));
    control_cleanup?;
    workload_cleanup?;
    observed.map_err(|_| "fixture timeout")?
}

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture on its native host"]
async fn every_existing_proc_interface_requires_its_effective_mask() {
    let mut failures = Vec::new();
    for control in [false, true] {
        for mode in [
            "observe",
            "missing-directory",
            "missing-file",
            "wrong-device",
            "nonempty-directory",
            "writable-directory",
            "unreadable-directory",
        ] {
            match inspect_mask(control, mode).await {
                Ok(admitted) if admitted == (mode == "observe") => {}
                result => failures.push(format!("control={control}, mode={mode}: {result:?}")),
            }
        }
    }
    assert!(failures.is_empty(), "effective proc masks: {failures:#?}");
}
