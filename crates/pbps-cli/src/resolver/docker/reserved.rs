//! Reserve the private runtime before starting database initialization. Only
//! source-free fixed bytes cross this gate after target separation and actual
//! kernel controls have been checked; declarations never enter this module.

use super::profile::{Launch, engine};
use super::{
    AttachStream, CandidateImage, CandidateRun, CandidateSession, Error, LocalApi, StartFailure,
};
use crate::resolver::native::{ExecutionLease, NativeTarget, ProcessLease, awaiting_engine};
use pbps_db::Driver;
use pbps_db::transport::ConnectionId;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub struct ReservedSession {
    workload: CandidateRun,
    bootstrap: AttachStream,
    control_api: LocalApi,
    attach_api: LocalApi,
    /// Held back, unspent, so a login refused by a still-starting engine can
    /// be retried on a control container of its own (issue #638).
    retry_api: LocalApi,
    image: CandidateImage,
    driver: Driver,
    password: String,
}

struct GateLease {
    root: ProcessLease,
    execution: ExecutionLease,
    target: ConnectionId,
}

impl GateLease {
    fn check(&self, target: &NativeTarget) -> Result<(), Error> {
        self.root.check().map_err(|_| Error::RuntimeChanged)?;
        self.execution.check().map_err(|_| Error::RuntimeChanged)?;
        if target.connection_id().map_err(|_| Error::RuntimeChanged)? != self.target {
            return Err(Error::RuntimeChanged);
        }
        Ok(())
    }
}

fn failure(cause: Error) -> StartFailure {
    StartFailure {
        cause,
        recovery_names: Vec::new(),
    }
}

impl ReservedSession {
    /// Starts only a bounded root timer and low-privilege fixed waiter. The
    /// engine installation is not initialized until `start` qualifies target
    /// separation and the effective kernel boundary.
    pub async fn reserve(
        api: LocalApi,
        image: CandidateImage,
        driver: Driver,
    ) -> Result<Self, StartFailure> {
        let bootstrap_api = api.additional().await.map_err(failure)?;
        // Only observations made on this still-owned native API connection
        // can choose bootstrap inputs. Acquisition through a different peer
        // or a reconnected API must be repeated before provisioning.
        if image.acquisition != Some(api.id) {
            return Err(failure(Error::Profile));
        }
        let control_api = api.additional().await.map_err(failure)?;
        let attach_api = api.additional().await.map_err(failure)?;
        let retry_api = api.additional().await.map_err(failure)?;
        Self::reserve_channels(
            api,
            bootstrap_api,
            control_api,
            attach_api,
            retry_api,
            image,
            driver,
        )
        .await
    }

    pub(super) async fn reserve_channels(
        api: LocalApi,
        bootstrap_api: LocalApi,
        control_api: LocalApi,
        attach_api: LocalApi,
        retry_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
    ) -> Result<Self, StartFailure> {
        let token = format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        );
        let password = format!("Pbps!{:032x}", rand::random::<u128>());
        let launch = Launch::reserved(&image, driver, &token, &password).map_err(failure)?;
        let workload = CandidateRun::start_launch(api, image.clone(), token, launch).await?;
        let prepare = async {
            let mut bootstrap = bootstrap_api.attach_inner(workload.container_id()).await?;
            bootstrap
                .write_all(b"pbps-bootstrap-probe-v1\n")
                .await
                .map_err(|_| Error::ControlLost)?;
            bootstrap.flush().await.map_err(|_| Error::ControlLost)?;
            let mut ready = [0; b"pbps-bootstrap-ready-v1\n".len()];
            bootstrap.read_exact(&mut ready).await.map_err(|error| {
                #[cfg(test)]
                eprintln!(
                    "private startup stage=bootstrap-greeting, io_kind={:?}",
                    error.kind()
                );
                let _ = error;
                Error::Start
            })?;
            if &ready != b"pbps-bootstrap-ready-v1\n" {
                return Err(Error::Start);
            }
            workload.check().await?;
            Ok(bootstrap)
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), prepare)
            .await
            .unwrap_or(Err(Error::Start));
        match result {
            Ok(bootstrap) => Ok(Self {
                workload,
                bootstrap,
                control_api,
                attach_api,
                retry_api,
                image,
                driver,
                password,
            }),
            Err(cause) => {
                let name = workload.resource_name().to_owned();
                let recovered = workload.close().await.is_ok();
                Err(StartFailure {
                    cause,
                    recovery_names: (!recovered).then_some(name).into_iter().collect(),
                })
            }
        }
    }

    pub async fn start(self, target: &mut NativeTarget) -> Result<CandidateSession, StartFailure> {
        // Retain the actual kernel leases across the bootstrap transition;
        // releasing the gate must not turn a checked PID into a reusable name.
        let gate = match self.qualify(target).await {
            Ok(gate) => gate,
            Err(cause) => return Err(self.close_failure(cause).await),
        };
        if let Err(cause) = gate.check(target) {
            return Err(self.close_failure(cause).await);
        }
        let mut session = self.release().await?;
        let checked = async {
            gate.check(target)?;
            session.capture_native()?;
            session.verify_target_separation(target).await?;
            session.bind_target(target)?;
            Ok::<_, Error>(())
        }
        .await;
        if let Err(cause) = checked {
            let recovery_names = session
                .close()
                .await
                .err()
                .map(|error| error.recovery_names)
                .unwrap_or_default();
            return Err(StartFailure {
                cause,
                recovery_names,
            });
        }
        Ok(session)
    }

    async fn qualify(&self, target: &mut NativeTarget) -> Result<GateLease, Error> {
        target.check().await.map_err(|_| Error::RuntimeChanged)?;
        if target.driver().map_err(|_| Error::RuntimeChanged)? != self.driver {
            return Err(Error::UnsupportedLaunch);
        }
        self.workload.check().await?;
        let root = awaiting_engine(
            self.workload.native_pid()?,
            &engine::private_channel_profile(self.driver),
        )
        .map_err(|_| Error::RuntimeChanged)?;
        let execution = ExecutionLease::capture(
            self.workload.native_pid()?,
            engine::workload_limits(self.driver),
        )
        .map_err(|_| Error::RuntimeChanged)?;
        for namespace in ["pid", "mnt", "net"] {
            if root
                .same_namespace(
                    target.service().map_err(|_| Error::RuntimeChanged)?,
                    namespace,
                )
                .map_err(|_| Error::RuntimeChanged)?
            {
                return Err(Error::RuntimeChanged);
            }
        }
        target.check().await.map_err(|_| Error::RuntimeChanged)?;
        execution.check().map_err(|_| Error::RuntimeChanged)?;
        root.check().map_err(|_| Error::RuntimeChanged)?;
        Ok(GateLease {
            root,
            execution,
            target: target.connection_id().map_err(|_| Error::RuntimeChanged)?,
        })
    }

    pub(super) async fn release(mut self) -> Result<CandidateSession, StartFailure> {
        let ready = async {
            self.bootstrap
                .write_all(b"pbps-bootstrap-start-v1\n")
                .await?;
            self.bootstrap.flush().await?;
            // Listening precedes database readiness on PostgreSQL. Wait for
            // the fixed bootstrap's engine-owned readiness observation before
            // opening the sole private session; never retry its handshake.
            let mut ready = [0; b"pbps-engine-ready-v1\n".len()];
            self.bootstrap.read_exact(&mut ready).await?;
            if &ready != b"pbps-engine-ready-v1\n" {
                return Err(std::io::Error::other("invalid bootstrap readiness"));
            }
            Ok::<_, std::io::Error>(())
        };
        if !matches!(
            tokio::time::timeout(std::time::Duration::from_secs(90), ready).await,
            Ok(Ok(()))
        ) {
            return Err(self.close_failure(Error::ControlLost).await);
        }
        drop(self.bootstrap);
        CandidateSession::connect_workload(
            self.control_api,
            self.attach_api,
            self.retry_api,
            self.image,
            self.driver,
            self.password,
            self.workload,
        )
        .await
    }

    async fn close_failure(self, cause: Error) -> StartFailure {
        let name = self.workload.resource_name().to_owned();
        drop(self.bootstrap);
        let recovered = self.workload.close().await.is_ok();
        StartFailure {
            cause,
            recovery_names: (!recovered).then_some(name).into_iter().collect(),
        }
    }

    pub async fn close(self) -> Result<(), StartFailure> {
        let failure = self.close_failure(Error::Cleanup).await;
        if failure.recovery_names.is_empty() {
            Ok(())
        } else {
            Err(failure)
        }
    }
}

#[cfg(test)]
mod tests;
