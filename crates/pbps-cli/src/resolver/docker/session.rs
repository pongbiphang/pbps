//! A source-free candidate session through a fixed run-private forwarder.
//! This is not admission: the target and measured containment still need to
//! be qualified together before any declarations or binding work are allowed.

use super::profile::{Launch, engine};
use super::{CandidateImage, CandidateRun, Error, LocalApi, StartFailure};
use crate::resolver::native::{ExecutionLease, NativeTarget, PrivateChannelLease, TargetWitness};
use pbps_db::Driver;
use pbps_db::resolver::InstanceObservation;
use pbps_db::transport::StreamConn;
use tokio::io::AsyncWriteExt as _;

pub struct CandidateSession {
    state: Option<State>,
}

struct State {
    connection: StreamConn,
    identity: InstanceObservation,
    control: CandidateRun,
    workload: CandidateRun,
    native: Option<NativeRun>,
    target: Option<TargetWitness>,
}

struct NativeRun {
    channel: PrivateChannelLease,
    workload: ExecutionLease,
    control: ExecutionLease,
}

impl NativeRun {
    fn capture(state: &State, driver: Driver) -> Result<Self, Error> {
        let workload = ExecutionLease::capture(
            state.workload.native_pid()?,
            engine::workload_limits(driver),
        )
        .map_err(|_| Error::RuntimeChanged)?;
        let control =
            ExecutionLease::capture(state.control.native_pid()?, engine::control_limits(driver))
                .map_err(|_| Error::RuntimeChanged)?;
        let channel = PrivateChannelLease::capture(
            state.workload.native_pid()?,
            state.control.native_pid()?,
            state.connection.id(),
            &state.identity,
            engine::private_channel_profile(driver),
        )
        .map_err(|_| Error::RuntimeChanged)?;
        Ok(Self {
            channel,
            workload,
            control,
        })
    }

    fn check(&self, state: &State) -> Result<(), Error> {
        self.workload.check().map_err(|_| Error::RuntimeChanged)?;
        self.control.check().map_err(|_| Error::RuntimeChanged)?;
        self.channel
            .check(state.connection.id(), &state.identity)
            .map_err(|_| Error::RuntimeChanged)
    }
}

fn token() -> String {
    format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    )
}

fn failure(cause: Error) -> StartFailure {
    StartFailure {
        cause,
        recovery_names: Vec::new(),
    }
}

impl CandidateSession {
    /// No database connection or arbitrary SQL method is exposed. This
    /// candidate exists only to qualify source-free runtime/backend facts.
    pub async fn start(
        api: LocalApi,
        image: CandidateImage,
        target: &mut NativeTarget,
    ) -> Result<Self, StartFailure> {
        let driver = target
            .driver()
            .map_err(|_| failure(Error::RuntimeChanged))?;
        super::ReservedSession::reserve(api, image, driver)
            .await?
            .start(target)
            .await
    }

    pub(super) fn capture_native(&mut self) -> Result<(), Error> {
        let state = self.state.as_mut().ok_or(Error::ControlLost)?;
        state.native = Some(NativeRun::capture(state, state.connection.driver())?);
        Ok(())
    }

    pub(super) fn bind_target(&mut self, target: &NativeTarget) -> Result<(), Error> {
        self.state.as_mut().ok_or(Error::ControlLost)?.target =
            Some(target.witness().map_err(|_| Error::RuntimeChanged)?);
        Ok(())
    }

    #[cfg(test)]
    async fn start_channels(
        api: LocalApi,
        control_api: LocalApi,
        attach_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
    ) -> Result<Self, StartFailure> {
        let bootstrap_api = LocalApi::connect_peer(&api.socket_path, api.peer.0)
            .await
            .map_err(failure)?;
        super::ReservedSession::reserve_channels(
            api,
            bootstrap_api,
            control_api,
            attach_api,
            image,
            driver,
        )
        .await?
        .release()
        .await
    }

    pub(super) async fn connect_workload(
        control_api: LocalApi,
        attach_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
        password: String,
        workload: CandidateRun,
    ) -> Result<Self, StartFailure> {
        let result =
            Self::connect_control(control_api, attach_api, image, driver, password, &workload)
                .await;
        match result {
            Ok((control, connection, identity)) => Ok(Self {
                state: Some(State {
                    connection,
                    identity,
                    control,
                    workload,
                    native: None,
                    target: None,
                }),
            }),
            Err(mut error) => {
                let name = workload.resource_name().to_owned();
                if workload.close().await.is_err() {
                    error.recovery_names.push(name);
                }
                Err(error)
            }
        }
    }

    async fn connect_control(
        api: LocalApi,
        attach_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
        password: String,
        workload: &CandidateRun,
    ) -> Result<(CandidateRun, StreamConn, InstanceObservation), StartFailure> {
        let owner = token();
        let launch =
            Launch::control(&image, driver, &owner, workload.container_id()).map_err(failure)?;
        let control = CandidateRun::start_launch(api, image, owner, launch).await?;
        let connect = async {
            // start_channels is private. The public factory authenticates
            // every additional daemon connection before creating resources.
            let mut stream = attach_api.attach_inner(control.container_id()).await?;
            stream
                .write_all(b"pbps-control-v1\n")
                .await
                .map_err(|_| Error::ControlLost)?;
            let mut connection =
                StreamConn::connect(driver, stream, engine::login(driver, password))
                    .await
                    .map_err(|error| {
                        #[cfg(test)]
                        eprintln!(
                            "private startup stage=protocol, driver_code={:?}",
                            error.server_error_code()
                        );
                        let _ = error;
                        Error::Start
                    })?;
            let identity = engine::identity(&mut connection).await.map_err(|error| {
                #[cfg(test)]
                eprintln!(
                    "private startup stage=identity, driver_code={:?}",
                    error.server_error_code()
                );
                let _ = error;
                Error::Start
            })?;
            control.check().await?;
            workload.check().await?;
            Ok((connection, identity))
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(90), connect)
            .await
            .unwrap_or(Err(Error::Start));
        match result {
            Ok((connection, identity)) => Ok((control, connection, identity)),
            Err(cause) => {
                let name = control.resource_name().to_owned();
                let recovered = control.close().await.is_ok();
                Err(StartFailure {
                    cause,
                    recovery_names: (!recovered).then_some(name).into_iter().collect(),
                })
            }
        }
    }

    pub fn identity(&mut self) -> Result<&InstanceObservation, Error> {
        let state = self.state.as_ref().ok_or(Error::ControlLost)?;
        if let Err(error) = check_target(state) {
            self.state = None;
            return Err(error);
        }
        Ok(&self.state.as_ref().ok_or(Error::ControlLost)?.identity)
    }

    /// Source-free separation check. This still grants no declaration or
    /// evidence capability: effective containment remains a separate gate.
    pub async fn verify_target_separation(
        &mut self,
        target: &mut NativeTarget,
    ) -> Result<(), Error> {
        let mut state = self.state.take().ok_or(Error::ControlLost)?;
        target.check().await.map_err(|_| Error::RuntimeChanged)?;
        let target_id = target.connection_id().map_err(|_| Error::RuntimeChanged)?;
        if state
            .target
            .as_ref()
            .is_some_and(|expected| expected.connection_id() != target_id)
        {
            return Err(Error::RuntimeChanged);
        }
        check_state(&mut state).await?;
        state
            .native
            .as_ref()
            .ok_or(Error::NativeDaemon)?
            .channel
            .separate_from(target.service().map_err(|_| Error::RuntimeChanged)?)
            .map_err(|_| Error::RuntimeChanged)?;
        target.check().await.map_err(|_| Error::RuntimeChanged)?;
        self.state = Some(state);
        Ok(())
    }

    pub async fn check(&mut self) -> Result<(), Error> {
        // Cancellation takes the connection and both ownership handles out of
        // the session. Their drops close traffic and request owned cleanup.
        let mut state = self.state.take().ok_or(Error::ControlLost)?;
        check_state(&mut state).await?;
        self.state = Some(state);
        Ok(())
    }

    pub async fn close(mut self) -> Result<(), StartFailure> {
        let state = self.state.take().ok_or_else(|| failure(Error::Cleanup))?;
        drop(state.connection);
        let mut recovery_names = Vec::new();
        let name = state.control.resource_name().to_owned();
        if state.control.close().await.is_err() {
            recovery_names.push(name);
        }
        let name = state.workload.resource_name().to_owned();
        if state.workload.close().await.is_err() {
            recovery_names.push(name);
        }
        if recovery_names.is_empty() {
            Ok(())
        } else {
            Err(StartFailure {
                cause: Error::Cleanup,
                recovery_names,
            })
        }
    }
}

async fn check_state(state: &mut State) -> Result<(), Error> {
    check_target(state)?;
    if let Some(native) = &state.native {
        native.check(state)?;
    }
    state.workload.check().await?;
    state.control.check().await?;
    let identity = engine::identity(&mut state.connection)
        .await
        .map_err(|_| Error::ControlLost)?;
    if identity != state.identity {
        return Err(Error::RuntimeChanged);
    }
    state.workload.check().await?;
    state.control.check().await?;
    if let Some(native) = &state.native {
        native.check(state)?;
    }
    check_target(state)?;
    Ok(())
}

fn check_target(state: &State) -> Result<(), Error> {
    if let Some(target) = &state.target {
        target.check().map_err(|_| Error::RuntimeChanged)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod kernel_tests;

#[cfg(test)]
mod native_factory_tests;
