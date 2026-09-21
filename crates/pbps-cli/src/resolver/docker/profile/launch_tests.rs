//! Inspect the actual dropped waiter before any engine initialization.

use super::*;
use crate::resolver::docker::{CandidateRun, LocalApi};
use crate::resolver::native::awaiting_engine;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

async fn accepts_waiter(change: impl FnOnce(&mut Launch)) -> bool {
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
        Ok(awaiting_engine(run.native_pid()?, &engine::private_channel_profile(driver)).is_ok())
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
