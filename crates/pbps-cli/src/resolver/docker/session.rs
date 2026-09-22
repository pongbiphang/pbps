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

/// SQL Server's "login failed for user", which during startup means the engine
/// is answering TDS before it can authenticate rather than that the credential
/// is wrong (issue #638).
const MSSQL_LOGIN_FAILED: &str = "18456";

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
        let retry_api = LocalApi::connect_peer(&api.socket_path, api.peer.0)
            .await
            .map_err(failure)?;
        super::ReservedSession::reserve_channels(
            api,
            bootstrap_api,
            control_api,
            attach_api,
            retry_api,
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
        retry_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
        password: String,
        workload: CandidateRun,
    ) -> Result<Self, StartFailure> {
        let result = Self::connect_control(
            control_api,
            attach_api,
            retry_api,
            image,
            driver,
            password,
            &workload,
        )
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

    /// Opens the sole private session, retrying only while the engine is
    /// still refusing logins it is about to accept.
    ///
    /// The bootstrap's engine-owned readiness greeting is what this is
    /// supposed to wait behind, and on PostgreSQL it is exact: its greeting
    /// waits for `postmaster.pid` to say `ready`, and for that engine
    /// accepting a connection and authenticating it are the same moment. On
    /// SQL Server the greeting is **early**, measured on the pinned image:
    ///
    /// ```text
    /// in-container 127.0.0.1:1433 accepts            4170ms
    /// "SQL Server is now ready for client connections"  4170ms   <- the greeting
    /// sa can actually log in                         4766ms
    /// ```
    ///
    /// Nothing inside the engine container can close that ~600ms: the only
    /// honest readiness signal for this engine is a successful login, a login
    /// needs `connect`, and the workload's seccomp policy denies `connect` by
    /// design — only the trusted forwarder may initiate a connection
    /// (`native_and_compat_network_and_process_bypasses_are_not_allowed`
    /// pins that). Nor is there a later log line to wait for: at the instant
    /// the login first worked the errorlog tail was msdb upgrade steps, whose
    /// presence depends on whether this is a first start.
    ///
    /// So the retry lives here, where the login already is. Each attempt needs
    /// a control container of its own: the forwarder opens exactly one TCP
    /// session to the engine and then pipes it, so a refused login spends the
    /// channel. The attempt closes its own container before this loop builds
    /// the next, and the whole loop shares the one 90-second budget a single
    /// attempt used to have, so nothing waits longer than it did (DECISIONS
    /// 500, issue #638).
    ///
    /// Narrow on purpose: only SQL Server's `18456` is retried. Every other
    /// refusal is reported on the first attempt, because a credential this
    /// resolver generated for a container it started is not going to become
    /// correct by being asked again.
    async fn connect_control(
        api: LocalApi,
        attach_api: LocalApi,
        retry_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
        password: String,
        workload: &CandidateRun,
    ) -> Result<(CandidateRun, StreamConn, InstanceObservation), StartFailure> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        let (mut api, mut attach_api) = (api, attach_api);
        loop {
            let attempt = Self::one_control_attempt(
                api,
                attach_api,
                image.clone(),
                driver,
                password.clone(),
                workload,
                deadline,
            )
            .await;
            let (failure, engine_starting) = match attempt {
                Ok(session) => return Ok(session),
                Err(failure) => failure,
            };
            // A non-empty `recovery_names` means the attempt could not confirm
            // that its own control container is gone. Retrying past that would
            // carry the name into a session that tracks only the container the
            // *next* attempt made, so an operator would be told to recover one
            // container while another stood untracked in the workload's
            // namespace. An unconfirmed cleanup is terminal.
            if !engine_starting
                || !failure.recovery_names.is_empty()
                || tokio::time::Instant::now() >= deadline
            {
                return Err(failure);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // Rechecked after the sleep, not only before it: with less than
            // the sleep left on the budget the wait itself carries past the
            // deadline, and `one_control_attempt` starts a control container
            // before it ever reaches `timeout_at`. Without this the loop would
            // exceed the 90 seconds it advertises and start one more container
            // to clean up while doing it.
            if tokio::time::Instant::now() >= deadline {
                return Err(failure);
            }
            // The attempt closed its own control container, confirmed; mint
            // the next pair from the handle held back for exactly this, which
            // re-verifies the daemon peer the way every other additional
            // connection here does — and, on the unqualified channels only a
            // test is allowed to supply, mints that kind rather than refusing
            // to mint at all. After the deadline check, so a loop that is
            // about to stop does not open connections to discard.
            //
            // A failure to mint is reported as itself. Minting says
            // `ControlLost` or `NativeDaemon` — the protected Docker channel
            // is gone or its authenticated peer changed — and answering that
            // with the previous login's `Error::Start` would send an operator
            // to look at container startup for a problem in the daemon
            // connection. The recovery names the spent attempt left are
            // carried across, since nothing has recovered them.
            //
            // Bounded by the same deadline, because acquiring a channel is
            // itself a wait: `connect_profile` permits a 15-second connect and
            // a 15-second handshake, and a slow daemon near the end of the
            // budget would otherwise spend a minute past it before anyone
            // looked at the clock.
            // Short-circuiting on purpose: a first mint that reports the
            // daemon peer changed is the answer, and asking for a second
            // channel from the same handle cannot improve it. Awaiting it
            // anyway lets a stalled connect run out the deadline, and the
            // timeout arm below then reports the previous login's
            // `Error::Start` — burying the daemon failure that is the cause.
            let acquired = tokio::time::timeout_at(deadline, async {
                let next_api = retry_api.additional_of_my_kind().await?;
                let next_attach = retry_api.additional_of_my_kind().await?;
                Ok::<_, Error>((next_api, next_attach))
            })
            .await;
            let Ok(acquired) = acquired else {
                return Err(failure);
            };
            (api, attach_api) = match acquired {
                Ok(pair) => pair,
                Err(cause) => {
                    return Err(StartFailure {
                        cause,
                        recovery_names: failure.recovery_names,
                    });
                }
            };
            // And checked once more with the channels in hand. The bound above
            // stops the *wait*; this stops the *launch*, because
            // `one_control_attempt` creates its control container before it
            // ever reaches its own `timeout_at`. The two guards are two
            // different things the budget has to survive.
            if tokio::time::Instant::now() >= deadline {
                return Err(failure);
            }
        }
    }

    /// One control container, one attach, one login. The `bool` says whether
    /// the failure was the engine refusing a login it is about to accept.
    async fn one_control_attempt(
        api: LocalApi,
        attach_api: LocalApi,
        image: CandidateImage,
        driver: Driver,
        password: String,
        workload: &CandidateRun,
        deadline: tokio::time::Instant,
    ) -> Result<(CandidateRun, StreamConn, InstanceObservation), (StartFailure, bool)> {
        let owner = token();
        let launch = Launch::control(
            &image,
            driver,
            &owner,
            workload.container_id(),
            super::profile::LIFETIME_SECS,
        )
        .map_err(|cause| (failure(cause), false))?;
        let control =
            CandidateRun::start_launch(api, image, owner, launch, super::profile::LIFETIME_SECS)
                .await
                .map_err(|failure| (failure, false))?;
        // Set by the login's own error arm and read after the future has been
        // driven to completion, which is the only place that can tell a
        // still-starting engine from a refusal that will not change.
        let mut engine_starting = false;
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
                        let code = error.server_error_code();
                        engine_starting = code.as_deref() == Some(MSSQL_LOGIN_FAILED);
                        #[cfg(test)]
                        eprintln!("private startup stage=protocol, driver_code={code:?}");
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
        let result = tokio::time::timeout_at(deadline, connect)
            .await
            .unwrap_or(Err(Error::Start));
        match result {
            Ok((connection, identity)) => Ok((control, connection, identity)),
            Err(cause) => {
                let name = control.resource_name().to_owned();
                let recovered = control.close().await.is_ok();
                Err((
                    StartFailure {
                        cause,
                        recovery_names: (!recovered).then_some(name).into_iter().collect(),
                    },
                    engine_starting,
                ))
            }
        }
    }

    pub fn identity(&mut self) -> Result<&InstanceObservation, Error> {
        let state = self.state.as_ref().ok_or(Error::ControlLost)?;
        // The supervisor cannot clear this handle's cached state. Revalidate
        // its native leases here even when no async check preceded the read.
        let outcome = check_target(state).and_then(|()| match &state.native {
            Some(native) => native.check(state),
            None => Ok(()), // Only private transport fixtures lack native admission.
        });
        if let Err(error) = outcome {
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
