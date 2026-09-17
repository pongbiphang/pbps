//! A pbps-launched forwarder into a container pbps did not launch.
//!
//! The Docker profile's control container is the one program allowed to open
//! a connection to an engine in a route-less network namespace: it shares
//! that namespace and nothing else, and pipes exactly one TCP session through
//! an authenticated attach stream. A supplied scratch server (#609) is reached
//! the same way, so the operator supplies no relay and no socket directory —
//! the forwarder is run-owned, launched from the supplied container's own
//! image, and removed with the run.

use super::profile::Launch;
use super::{CandidateImage, CandidateRun, Error, LocalApi, StartFailure};
use pbps_db::Driver;
use pbps_db::transport::{StreamConn, StreamLogin};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

pub(crate) struct Forwarder {
    run: CandidateRun,
}

fn failure(cause: Error) -> StartFailure {
    StartFailure {
        cause,
        recovery_names: Vec::new(),
    }
}

impl Forwarder {
    /// One forwarder, one attach, one login. The forwarder's root guard
    /// lives `lifetime_secs`; past that the connection ends and the run with
    /// it, which is why callers pass the run's own bound.
    pub(crate) async fn open(
        api: &LocalApi,
        image: &CandidateImage,
        driver: Driver,
        workload: &str,
        login: StreamLogin,
        lifetime_secs: u64,
    ) -> Result<(Self, StreamConn), StartFailure> {
        let control_api = api.additional().await.map_err(failure)?;
        let attach_api = api.additional().await.map_err(failure)?;
        let owner = format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        );
        let launch =
            Launch::control(image, driver, &owner, workload, lifetime_secs).map_err(failure)?;
        let run =
            CandidateRun::start_launch(control_api, image.clone(), owner, launch, lifetime_secs)
                .await?;
        let connect = async {
            let mut stream = attach_api.attach_inner(run.container_id()).await?;
            stream
                .write_all(b"pbps-control-v1\n")
                .await
                .map_err(|_| Error::ControlLost)?;
            let connection = StreamConn::connect(driver, stream, login)
                .await
                .map_err(|_| Error::Start)?;
            run.check().await?;
            Ok(connection)
        };
        // The engine is already serving — a supplied server is admitted warm
        // — so there is no startup to wait behind, only the forwarder's own
        // connect loop and one login.
        let result = tokio::time::timeout(Duration::from_secs(90), connect)
            .await
            .unwrap_or(Err(Error::Start));
        match result {
            Ok(connection) => Ok((Self { run }, connection)),
            Err(cause) => {
                let name = run.resource_name().to_owned();
                let recovered = run.close().await.is_ok();
                Err(StartFailure {
                    cause,
                    recovery_names: (!recovered).then_some(name).into_iter().collect(),
                })
            }
        }
    }

    pub(crate) fn pid(&self) -> Result<u32, Error> {
        self.run.native_pid()
    }

    pub(crate) fn resource_name(&self) -> &str {
        self.run.resource_name()
    }

    /// Continuity of the forwarder container as the daemon reports it.
    pub(crate) async fn check(&self) -> Result<(), Error> {
        self.run.check().await
    }

    /// Removal, confirmed. Dropping a forwarder requests the same removal
    /// without waiting for the confirmation.
    pub(crate) async fn close(self) -> Result<(), Error> {
        self.run.close().await
    }
}
