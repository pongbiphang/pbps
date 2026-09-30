//! Analysis ownership for an admitted, run-owned container. The candidate
//! retains cleanup state until the shared ScratchRun takes the whole owner.

use super::{CandidateSession, State};
use crate::resolver::docker::profile::{Launch, engine};
use crate::resolver::docker::{CandidateRun, Error as DockerError};
use crate::resolver::native::{
    ExecutionLease, MqueueLease, ProcessLease, WorkloadPrivileges, guarded_tasks,
    observed_socket_holders, private_network,
};
#[cfg(test)]
use crate::resolver::server::container_tests::relay_recovery::{self, Site};
use crate::resolver::server::{self, Error, ScratchRun, ServerFailure, exclusivity};
use pbps_db::Driver;
use pbps_db::resolver::environment::DatabaseRecipe;
use pbps_db::resolver::{ScratchNames, SessionCounter};
use pbps_db::transport::{StreamConn, StreamLogin};
use std::collections::BTreeSet;
use tokio::io::AsyncWriteExt as _;
use tokio::time::Instant;

fn invalid() -> Error {
    Error::Unqualified("the owned container run changed or became unreadable")
}

fn converted(error: DockerError) -> Error {
    Error::Channel(error.to_string())
}

fn failed(cause: Error, names: Vec<String>) -> ServerFailure {
    ServerFailure {
        cause,
        recovery_names: names,
    }
}

fn generated_owner() -> String {
    format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    )
}

/// Retained in CandidateSession before its first scratch DDL. A cancelled
/// creation leaves the candidate holding every name and any launched relay.
pub(super) struct Pending {
    names: ScratchNames,
    relay_name: Option<String>,
    relay: Option<CandidateRun>,
    unconfirmed: Vec<String>,
    removed: bool,
}

impl Pending {
    fn new(names: ScratchNames) -> Self {
        Self {
            names,
            relay_name: None,
            relay: None,
            unconfirmed: Vec::new(),
            removed: false,
        }
    }

    fn names(&self) -> Vec<String> {
        let mut names = if self.removed {
            Vec::new()
        } else {
            vec![
                self.names.database().to_owned(),
                self.names.login().to_owned(),
            ]
        };
        names.extend(self.unconfirmed.iter().cloned());
        if let Some(name) = &self.relay_name {
            names.push(name.clone());
        }
        names.sort_unstable();
        names.dedup();
        names
    }
}

struct KernelChannel {
    guard: ProcessLease,
    mqueue: MqueueLease,
    pair: exclusivity::TcpPair,
    backend: ProcessLease,
    key: String,
    execution: ExecutionLease,
    holders: Vec<ProcessLease>,
    privileges: WorkloadPrivileges,
    executable: &'static str,
    connection: pbps_db::transport::ConnectionId,
}

impl KernelChannel {
    async fn capture(
        workload_pid: u32,
        layout: engine::Layout,
        relay: &CandidateRun,
        connection: &mut StreamConn,
        known: &[&exclusivity::TcpPair],
    ) -> Result<Self, Error> {
        let root = ProcessLease::capture(workload_pid).map_err(|_| invalid())?;
        let guard =
            ProcessLease::capture(relay.native_pid().map_err(converted)?).map_err(|_| invalid())?;
        let mqueue = MqueueLease::capture(&guard).map_err(|_| invalid())?;
        let execution = ExecutionLease::capture(
            relay.native_pid().map_err(converted)?,
            engine::control_limits_for(layout),
        )
        .map_err(|_| invalid())?;
        let profile = engine::private_channel_profile(connection.driver());
        let (pair, backend) = exclusivity::bind(&root, &guard, profile.port, known)
            .map_err(|reason| Error::Channel(reason.into()))?;
        server::check_kernel_parts(&root, &guard, &mqueue, &pair, &backend)?;
        let own = server::engine::own_session(connection)
            .await
            .map_err(|error| Error::Identity(error.to_string()))?;
        server::correlate(&own.process, backend.namespace_pid(), &root)?;
        let holders =
            observed_socket_holders(&guard, pair.client_inode()).map_err(|_| invalid())?;
        let result = Self {
            guard,
            mqueue,
            pair,
            backend,
            key: own.key,
            execution,
            holders,
            privileges: profile.privileges,
            executable: profile.executable,
            connection: connection.id(),
        };
        result.check_kernel(&root, connection.id())?;
        Ok(result)
    }

    fn check_kernel(
        &self,
        root: &ProcessLease,
        connection: pbps_db::transport::ConnectionId,
    ) -> Result<(), Error> {
        if self.connection != connection {
            return Err(invalid());
        }
        self.execution.check().map_err(|_| invalid())?;
        self.backend.check().map_err(|_| invalid())?;
        if self
            .backend
            .executable_path()
            .file_name()
            .is_none_or(|name| name != self.executable)
        {
            return Err(invalid());
        }
        if root
            .same_namespace(&self.guard, "pid")
            .map_err(|_| invalid())?
            || root
                .same_namespace(&self.guard, "mnt")
                .map_err(|_| invalid())?
            || !root
                .same_namespace(&self.guard, "net")
                .map_err(|_| invalid())?
            || !root
                .same_namespace(&self.guard, "user")
                .map_err(|_| invalid())?
        {
            return Err(invalid());
        }
        private_network(root).map_err(|_| invalid())?;
        guarded_tasks(root, self.privileges).map_err(|_| invalid())?;
        server::check_kernel_parts(root, &self.guard, &self.mqueue, &self.pair, &self.backend)?;
        let holders = observed_socket_holders(&self.guard, self.pair.client_inode())
            .map_err(|_| invalid())?;
        if holders.len() != 3 || self.holders.len() != 3 {
            return Err(invalid());
        }
        let mut shells = 0;
        let mut pipes = 0;
        for (held, current) in self.holders.iter().zip(&holders) {
            if !held.same_process(current).map_err(|_| invalid())? {
                return Err(invalid());
            }
            match current
                .executable_path()
                .file_name()
                .and_then(|name| name.to_str())
            {
                Some("bash") => shells += 1,
                Some("cat") => pipes += 1,
                _ => return Err(invalid()),
            }
        }
        if shells != 1 || pipes != 2 {
            return Err(invalid());
        }
        Ok(())
    }

    async fn check(
        &self,
        root: &ProcessLease,
        relay: &CandidateRun,
        connection: pbps_db::transport::ConnectionId,
    ) -> Result<(), Error> {
        self.check_kernel(root, connection)?;
        relay.check().await.map_err(converted)?;
        self.check_kernel(root, connection)
    }
}

/// An extra channel consists of its protocol stream, supervised relay and
/// kernel binding; none is stored separately from its cleanup owner.
pub(crate) struct ContainerSession {
    pub(crate) connection: StreamConn,
    run: CandidateRun,
    kernel: KernelChannel,
}

impl ContainerSession {
    pub(crate) fn backend(&self) -> &ProcessLease {
        &self.kernel.backend
    }
    fn pair(&self) -> &exclusivity::TcpPair {
        &self.kernel.pair
    }
    fn key(&self) -> &str {
        &self.kernel.key
    }
}

struct Census {
    accepted: u64,
    opened: u64,
    epoch: String,
    continuity: BTreeSet<String>,
    deadline: Instant,
}

impl Census {
    fn counted(&self, counter: &SessionCounter) -> Result<(), Error> {
        if self.epoch != counter.epoch {
            return Err(invalid());
        }
        if counter.total != self.accepted + self.opened {
            return Err(Error::Exclusivity(server::Signal::SessionCounter));
        }
        if !self
            .continuity
            .iter()
            .all(|row| counter.continuity.contains(row))
        {
            return Err(Error::Exclusivity(server::Signal::CounterRows));
        }
        Ok(())
    }
}

/// The container arm of the closed runtime owner. The existing CandidateRun
/// supervisor remains authoritative for workload and every relay.
pub(crate) struct ContainerControl {
    state: Option<State>,
    control_kernel: KernelChannel,
    census: Census,
    pub(crate) admin: Option<ContainerSession>,
    retired: Vec<CandidateRun>,
    unconfirmed: Vec<String>,
    pending_relay: Option<CandidateRun>,
    pending_relay_name: Option<String>,
    pub(crate) roles: Vec<String>,
    refusal: Option<Error>,
    in_flight: bool,
    removed: bool,
}

impl ContainerControl {
    pub(crate) fn driver(&self) -> Driver {
        self.state.as_ref().expect("live state").connection.driver()
    }
    pub(crate) fn refusal(&self) -> Option<Error> {
        self.refusal.clone()
    }
    pub(crate) fn live(&self) -> Result<(), Error> {
        self.refusal.clone().map_or(Ok(()), Err)
    }
    pub(crate) fn refuse(&mut self, cause: Error) {
        if self.refusal.is_none() {
            self.refusal = Some(cause);
        }
    }
    pub(crate) fn admin_login(&self, database: &str) -> StreamLogin {
        {
            let mut login = engine::login(
                self.driver(),
                self.state.as_ref().expect("live state").password.clone(),
            );
            login.database = database.to_owned();
            login
        }
    }
    pub(crate) fn retire(&mut self, session: ContainerSession) {
        drop(session.connection);
        self.retired.push(session.run);
    }
    pub(crate) fn retire_admin(&mut self) {
        if let Some(admin) = self.admin.take() {
            self.retire(admin);
        }
    }

    /// Every current channel is bound twice to the same workload. The exact
    /// socket census rejects a foreign pair; engine inventory/counter provide
    /// independent evidence, including a connection that already closed.
    pub(crate) async fn check(&mut self, scratch: Option<&ContainerSession>) -> Result<(), Error> {
        if self.in_flight || self.admin.is_some() {
            self.refuse(Error::Cancelled);
        }
        self.live()?;
        self.in_flight = true;
        let outcome = self.check_inner(scratch).await;
        self.in_flight = false;
        if let Err(cause) = &outcome {
            self.refuse(cause.clone());
        }
        outcome
    }

    async fn check_inner(&mut self, scratch: Option<&ContainerSession>) -> Result<(), Error> {
        #[cfg(test)]
        let now = server::container_tests::deadline_now();
        #[cfg(not(test))]
        let now = Instant::now();
        if now >= self.census.deadline {
            return Err(Error::Deadline);
        }
        let state = self.state.as_mut().ok_or(Error::Consumed)?;
        state
            .target
            .as_ref()
            .ok_or_else(invalid)?
            .check()
            .map_err(|_| invalid())?;
        state.workload.check().await.map_err(converted)?;
        state.control.check().await.map_err(converted)?;
        let native = state.native.as_ref().ok_or_else(invalid)?;
        native.workload.check().map_err(|_| invalid())?;
        native.control.check().map_err(|_| invalid())?;
        let root = ProcessLease::capture(state.workload.native_pid().map_err(converted)?)
            .map_err(|_| invalid())?;
        self.control_kernel
            .check(&root, &state.control, state.connection.id())
            .await?;
        if let Some(scratch) = scratch {
            scratch
                .kernel
                .check(&root, &scratch.run, scratch.connection.id())
                .await?;
        }
        let mut pairs = vec![&self.control_kernel.pair];
        pairs.extend(scratch.map(ContainerSession::pair));
        exclusivity::census(&root, &pairs)?;
        let expected: BTreeSet<String> = std::iter::once(self.control_kernel.key.clone())
            .chain(scratch.map(|session| session.key().to_owned()))
            .collect();
        let reported = server::engine::client_sessions(&mut state.connection)
            .await
            .map_err(|error| Error::Exclusivity(server::signal(&error)))?;
        if reported.own != self.control_kernel.key {
            return Err(invalid());
        }
        self.census.counted(&reported.counter)?;
        exclusivity::only_our_sessions(&reported.clients, &expected)?;
        let identity = server::engine::identity(&mut state.connection)
            .await
            .map_err(|error| Error::Identity(error.to_string()))?;
        if identity != state.identity {
            return Err(invalid());
        }
        self.control_kernel
            .check(&root, &state.control, state.connection.id())
            .await?;
        if let Some(scratch) = scratch {
            scratch
                .kernel
                .check(&root, &scratch.run, scratch.connection.id())
                .await?;
        }
        exclusivity::census(&root, &pairs)?;
        state
            .target
            .as_ref()
            .ok_or_else(invalid)?
            .check()
            .map_err(|_| invalid())?;
        let closing = server::engine::session_counter(&mut state.connection)
            .await
            .map_err(|error| Error::Exclusivity(server::signal(&error)))?;
        self.census.counted(&closing)
    }

    /// Preparation cannot create a relay. Save its name at the launch boundary
    /// and its handle before attach/login, so cancellation loses neither.
    pub(crate) async fn open_channel(
        &mut self,
        login: StreamLogin,
        scratch: Option<&ContainerSession>,
    ) -> Result<ContainerSession, ServerFailure> {
        self.check(scratch)
            .await
            .map_err(|cause| failed(cause, self.recovery_names()))?;
        let owner = generated_owner();
        let name = format!("pbps-resolver-{owner}");
        self.in_flight = true;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| failed(Error::Consumed, self.recovery_names()))?;
        let launch = Launch::control(
            &state.image,
            state.connection.driver(),
            &owner,
            state.workload.container_id(),
            super::super::profile::LIFETIME_SECS,
        )
        .map_err(|error| failed(converted(error), self.recovery_names()))?;
        #[cfg(test)]
        relay_recovery::before_prepare(Site::Admin, &name, &mut self.unconfirmed)
            .map_err(|error| failed(converted(error), self.recovery_names()))?;
        let api = state
            .analysis_api
            .additional()
            .await
            .map_err(|error| failed(converted(error), self.recovery_names()))?;
        self.pending_relay_name = Some(name.clone());
        let start = CandidateRun::start_launch(
            api,
            state.image.clone(),
            owner,
            launch,
            super::super::profile::LIFETIME_SECS,
        );
        #[cfg(test)]
        let start = relay_recovery::start(Site::Admin, &name, start);
        let relay = match start.await {
            Ok(relay) => relay,
            Err(error) => {
                if error.recovery_names.is_empty() {
                    self.pending_relay_name = None;
                } else {
                    self.unconfirmed.extend(error.recovery_names);
                }
                return Err(failed(converted(error.cause), self.recovery_names()));
            }
        };
        self.pending_relay = Some(relay);
        let mut connection = connect_relay(
            state,
            self.pending_relay.as_ref().expect("owned relay"),
            login,
        )
        .await
        .map_err(|cause| failed(cause, self.recovery_names()))?;
        let mut known = vec![&self.control_kernel.pair];
        known.extend(scratch.map(ContainerSession::pair));
        let kernel = KernelChannel::capture(
            state
                .workload
                .native_pid()
                .map_err(|error| failed(converted(error), self.recovery_names()))?,
            state.layout,
            self.pending_relay.as_ref().expect("owned relay"),
            &mut connection,
            &known,
        )
        .await
        .map_err(|cause| failed(cause, self.recovery_names()))?;
        // The new channel is admitted only if it is one of the exact owned
        // kernel pairs and one of the engine's exact reported sessions.
        let root = ProcessLease::capture(
            state
                .workload
                .native_pid()
                .map_err(|error| failed(converted(error), self.recovery_names()))?,
        )
        .map_err(|_| failed(invalid(), self.recovery_names()))?;
        let mut pairs = vec![&self.control_kernel.pair, &kernel.pair];
        pairs.extend(scratch.map(ContainerSession::pair));
        exclusivity::census(&root, &pairs).map_err(|cause| failed(cause, self.recovery_names()))?;
        let mut expected = BTreeSet::from([self.control_kernel.key.clone(), kernel.key.clone()]);
        if let Some(scratch) = scratch {
            expected.insert(scratch.key().to_owned());
        }
        let recovery = self.recovery_names();
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| failed(Error::Consumed, recovery.clone()))?;
        let reported = server::engine::client_sessions(&mut state.connection)
            .await
            .map_err(|error| {
                failed(Error::Exclusivity(server::signal(&error)), recovery.clone())
            })?;
        exclusivity::only_our_sessions(&reported.clients, &expected)
            .map_err(|cause| failed(cause, recovery.clone()))?;
        if reported.own != self.control_kernel.key
            || reported.counter.epoch != self.census.epoch
            || reported.counter.total != self.census.accepted + self.census.opened + 1
            || !self
                .census
                .continuity
                .iter()
                .all(|row| reported.counter.continuity.contains(row))
        {
            return Err(failed(
                Error::Exclusivity(server::Signal::SessionCounter),
                recovery.clone(),
            ));
        }
        let identity = server::engine::identity(&mut state.connection)
            .await
            .map_err(|error| failed(Error::Identity(error.to_string()), recovery.clone()))?;
        if identity != state.identity {
            return Err(failed(invalid(), recovery.clone()));
        }
        state
            .target
            .as_ref()
            .ok_or_else(|| failed(invalid(), recovery.clone()))?
            .check()
            .map_err(|_| failed(invalid(), recovery.clone()))?;
        kernel
            .check(
                &root,
                self.pending_relay.as_ref().expect("owned relay"),
                connection.id(),
            )
            .await
            .map_err(|cause| failed(cause, recovery.clone()))?;
        exclusivity::census(&root, &pairs).map_err(|cause| failed(cause, recovery.clone()))?;
        let relay = self.pending_relay.take().expect("owned relay");
        self.pending_relay_name = None;
        self.census.opened += 1;
        self.in_flight = false;
        Ok(ContainerSession {
            connection,
            run: relay,
            kernel,
        })
    }

    fn recovery_names(&self) -> Vec<String> {
        let mut names = self.unconfirmed.clone();
        if let Some(name) = &self.pending_relay_name {
            names.push(name.clone());
        }
        names.extend(self.roles.iter().cloned());
        names.sort_unstable();
        names.dedup();
        names
    }

    pub(crate) async fn close(
        &mut self,
        scratch: Option<ContainerSession>,
        names: &ScratchNames,
        cause: Error,
    ) -> ServerFailure {
        if let Some(scratch) = scratch {
            self.retire(scratch);
        }
        self.retire_admin();
        #[cfg(test)]
        crate::resolver::server::container_tests::pause("container-close-owned").await;
        // End every scratch/admin transport before DROP DATABASE. Record the
        // entire batch first, so cancellation while confirming one removal
        // cannot lose the names of the others.
        if let Some(relay) = self.pending_relay.take()
            && close_named(relay, &mut self.unconfirmed).await
        {
            self.pending_relay_name = None;
        }
        for relay in &self.retired {
            self.unconfirmed.push(relay.resource_name().to_owned());
        }
        for relay in std::mem::take(&mut self.retired) {
            close_named(relay, &mut self.unconfirmed).await;
        }
        let mut removed = self.removed;
        if !removed && let Some(state) = self.state.as_mut().filter(|_| !self.in_flight) {
            self.in_flight = true;
            let dropped = server::engine::drop_scratch(&mut state.connection, names).await;
            if dropped.is_ok() {
                self.roles = server::engine::drop_roles(&mut state.connection, &self.roles).await;
                removed = true;
            }
            self.in_flight = false;
        }
        if !removed && let Some(state) = &self.state {
            self.in_flight = true;
            removed =
                remove_with_janitor(state, names, &mut self.roles, &mut self.unconfirmed).await;
            self.in_flight = false;
        }
        self.removed = removed;
        if let Some(state) = self.state.take() {
            let control_name = state.control.resource_name().to_owned();
            let workload_name = state.workload.resource_name().to_owned();
            self.unconfirmed
                .extend([control_name.clone(), workload_name.clone()]);
            let control_ok = state.control.close().await.is_ok();
            if control_ok {
                self.unconfirmed.retain(|name| name != &control_name);
            }
            let workload_ok = state.workload.close().await.is_ok();
            if workload_ok {
                self.unconfirmed.retain(|name| name != &workload_name);
                // Confirmed removal destroys this workload's private SQL
                // storage (DECISIONS 497), even when SQL cleanup failed.
                // Independent relay obligations remain in recovery_names.
                self.removed = true;
                self.roles.clear();
            }
        }
        let mut recovery = self.recovery_names();
        if !self.removed {
            recovery.extend([names.database().to_owned(), names.login().to_owned()]);
        }
        recovery.sort_unstable();
        recovery.dedup();
        failed(cause, recovery)
    }
}

async fn connect_relay(
    state: &State,
    relay: &CandidateRun,
    login: StreamLogin,
) -> Result<StreamConn, Error> {
    let api = state.analysis_api.additional().await.map_err(converted)?;
    let mut stream = api
        .attach_inner(relay.container_id())
        .await
        .map_err(converted)?;
    stream
        .write_all(b"pbps-control-v1\n")
        .await
        .map_err(|_| invalid())?;
    StreamConn::connect(state.connection.driver(), stream, login)
        .await
        .map_err(|_| Error::Channel("the run-owned channel could not authenticate".into()))
}

async fn close_named(relay: CandidateRun, names: &mut Vec<String>) -> bool {
    let name = relay.resource_name().to_owned();
    let removed = relay.close().await.is_ok();
    if removed {
        names.retain(|held| held != &name);
    } else {
        names.push(name);
    }
    removed
}

async fn remove_with_janitor(
    state: &State,
    names: &ScratchNames,
    roles: &mut Vec<String>,
    unconfirmed: &mut Vec<String>,
) -> bool {
    let owner = generated_owner();
    let name = format!("pbps-resolver-{owner}");
    let result = async {
        state.workload.check().await.map_err(|_| ())?;
        let launch = Launch::control(
            &state.image.clone(),
            state.connection.driver(),
            &owner,
            state.workload.container_id(),
            super::super::profile::LIFETIME_SECS,
        )
        .map_err(|_| ())?;
        #[cfg(test)]
        relay_recovery::before_prepare(Site::Janitor, &name, unconfirmed).map_err(|_| ())?;
        let api = state.analysis_api.additional().await.map_err(|_| ())?;
        let registration = unconfirmed.len();
        unconfirmed.push(name.clone());
        let start = CandidateRun::start_launch(
            api,
            state.image.clone(),
            owner,
            launch,
            super::super::profile::LIFETIME_SECS,
        );
        #[cfg(test)]
        let start = relay_recovery::start(Site::Janitor, &name, start);
        let relay = match start.await {
            Ok(relay) => relay,
            Err(error) => {
                if error.recovery_names.is_empty() {
                    // The vector is exclusively borrowed across launch. Remove
                    // this registration without erasing an earlier obligation.
                    unconfirmed.remove(registration);
                } else {
                    unconfirmed.extend(error.recovery_names);
                }
                return Err(());
            }
        };
        let mut connection = connect_relay(
            state,
            &relay,
            engine::login(state.connection.driver(), state.password.clone()),
        )
        .await
        .map_err(|_| ())?;
        let identity = server::engine::identity(&mut connection)
            .await
            .map_err(|_| ())?;
        let mut removed = false;
        if identity == state.identity
            && server::engine::drop_scratch(&mut connection, names)
                .await
                .is_ok()
        {
            *roles = server::engine::drop_roles(&mut connection, roles).await;
            removed = true;
        }
        drop(connection);
        let closed = relay.close().await.is_ok();
        Ok::<_, ()>((removed, closed))
    }
    .await;
    if let Ok((removed, closed)) = result {
        if closed {
            unconfirmed.retain(|item| item != &name);
        }
        removed
    } else {
        false
    }
}

pub(super) async fn cleanup_pending(state: &State, pending: &mut Pending) -> Vec<String> {
    if let Some(relay) = pending.relay.take()
        && close_named(relay, &mut pending.unconfirmed).await
    {
        pending.relay_name = None;
    }
    if !pending.removed {
        pending.removed = remove_with_janitor(
            state,
            &pending.names,
            &mut Vec::new(),
            &mut pending.unconfirmed,
        )
        .await;
    }
    let mut recovery = pending.unconfirmed.clone();
    if !pending.removed {
        recovery.extend([
            pending.names.database().to_owned(),
            pending.names.login().to_owned(),
        ]);
    }
    if let Some(name) = &pending.relay_name {
        recovery.push(name.clone());
    }
    recovery.sort_unstable();
    recovery.dedup();
    recovery
}

impl CandidateSession {
    /// Closes an interrupted analysis without moving the candidate out of
    /// its caller. Cancellation retains scratch and relay names for a retry.
    pub async fn discard(&mut self) -> Result<(), ServerFailure> {
        if !self.analysis_in_flight {
            return Err(failed(Error::Consumed, Vec::new()));
        }
        // Retain container names before the first cleanup await. Pending
        // already owns the SQL names; merge its cleanup report before taking
        // State so cancellation keeps outstanding SQL/container names.
        if let Some(state) = self.state.as_ref() {
            self.analysis_recovery.extend([
                state.control.resource_name().to_owned(),
                state.workload.resource_name().to_owned(),
            ]);
        }
        #[cfg(test)]
        crate::resolver::server::container_tests::pause("container-discard-owned").await;
        if let Some(state) = self.state.as_ref()
            && let Some(pending) = self.pending.as_mut()
        {
            let current = cleanup_pending(state, pending).await;
            if pending.removed {
                self.analysis_recovery.retain(|name| {
                    name != pending.names.database() && name != pending.names.login()
                });
            }
            self.analysis_recovery.extend(current);
        }
        if let Some(state) = self.state.take() {
            let control_name = state.control.resource_name().to_owned();
            let workload_name = state.workload.resource_name().to_owned();
            drop(state.connection);
            if state.control.close().await.is_ok() {
                self.analysis_recovery.retain(|name| name != &control_name);
            }
            if state.workload.close().await.is_ok() {
                self.analysis_recovery.retain(|name| name != &workload_name);
                if let Some(pending) = self.pending.as_mut() {
                    // Only confirmed destruction of this private workload
                    // completes its SQL obligations (DECISIONS 497).
                    // Keep relay names and persist completion for a retry.
                    pending.removed = true;
                    self.analysis_recovery.retain(|name| {
                        name != pending.names.database() && name != pending.names.login()
                    });
                }
            }
        }
        self.analysis_recovery.sort_unstable();
        self.analysis_recovery.dedup();
        if self.analysis_recovery.is_empty() {
            self.pending = None;
            return Ok(());
        }
        Err(failed(Error::Cleanup, self.analysis_recovery.clone()))
    }

    pub async fn open_scratch(
        &mut self,
        recipe: &DatabaseRecipe,
    ) -> Result<ScratchRun, ServerFailure> {
        if self.analysis_in_flight || self.pending.is_some() {
            return Err(failed(
                Error::Cancelled,
                self.pending.as_ref().map_or(Vec::new(), Pending::names),
            ));
        }
        self.analysis_in_flight = true;
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| failed(Error::Consumed, Vec::new()))?;
        super::check_state(state)
            .await
            .map_err(|_| failed(invalid(), Vec::new()))?;
        let control_kernel = KernelChannel::capture(
            state
                .workload
                .native_pid()
                .map_err(|error| failed(converted(error), Vec::new()))?,
            state.layout,
            &state.control,
            &mut state.connection,
            &[],
        )
        .await
        .map_err(|cause| failed(cause, Vec::new()))?;
        let inventory = server::engine::client_sessions(&mut state.connection)
            .await
            .map_err(|error| failed(Error::Exclusivity(server::signal(&error)), Vec::new()))?;
        if inventory.own != control_kernel.key {
            return Err(failed(invalid(), Vec::new()));
        }
        exclusivity::only_our_sessions(
            &inventory.clients,
            &BTreeSet::from([control_kernel.key.clone()]),
        )
        .map_err(|cause| failed(cause, Vec::new()))?;
        let names = server::generated_names().map_err(|cause| failed(cause, Vec::new()))?;
        self.pending = Some(Pending::new(names.clone()));
        let created = server::engine::create_scratch(&mut state.connection, &names, recipe).await;
        if created.is_err() {
            return Err(failed(
                Error::Scratch,
                self.pending.as_ref().expect("names").names(),
            ));
        }
        #[cfg(test)]
        crate::resolver::server::container_tests::sql_recovery::scratch_created(
            &names,
            &mut state.connection,
        )
        .await;
        #[cfg(test)]
        crate::resolver::server::container_tests::pause("container-create-owned").await;
        let pending = self.pending.as_mut().expect("names");
        let owner = generated_owner();
        let name = format!("pbps-resolver-{owner}");
        let launch = Launch::control(
            &state.image.clone(),
            state.connection.driver(),
            &owner,
            state.workload.container_id(),
            super::super::profile::LIFETIME_SECS,
        )
        .map_err(|error| failed(converted(error), pending.names()))?;
        #[cfg(test)]
        relay_recovery::before_prepare(Site::Scratch, &name, &mut pending.unconfirmed)
            .map_err(|error| failed(converted(error), pending.names()))?;
        let api = state
            .analysis_api
            .additional()
            .await
            .map_err(|error| failed(converted(error), pending.names()))?;
        pending.relay_name = Some(name.clone());
        let start = CandidateRun::start_launch(
            api,
            state.image.clone(),
            owner,
            launch,
            super::super::profile::LIFETIME_SECS,
        );
        #[cfg(test)]
        let start = relay_recovery::start(Site::Scratch, &name, start);
        let relay = match start.await {
            Ok(relay) => relay,
            Err(error) => {
                if error.recovery_names.is_empty() {
                    pending.relay_name = None;
                } else {
                    pending.unconfirmed.extend(error.recovery_names);
                }
                return Err(failed(converted(error.cause), pending.names()));
            }
        };
        pending.relay = Some(relay);
        let login = StreamLogin {
            user: names.login().to_owned(),
            password: names.password().to_owned(),
            database: names.database().to_owned(),
        };
        let mut connection = connect_relay(state, pending.relay.as_ref().expect("relay"), login)
            .await
            .map_err(|cause| failed(cause, pending.names()))?;
        let kernel = KernelChannel::capture(
            state
                .workload
                .native_pid()
                .map_err(|error| failed(converted(error), pending.names()))?,
            state.layout,
            pending.relay.as_ref().expect("relay"),
            &mut connection,
            &[&control_kernel.pair],
        )
        .await
        .map_err(|cause| failed(cause, pending.names()))?;
        // All awaits finish while the candidate still owns the relay and
        // scratch names. The final transfer below has no cancellation point.
        let root = ProcessLease::capture(
            state
                .workload
                .native_pid()
                .map_err(|error| failed(converted(error), pending.names()))?,
        )
        .map_err(|_| failed(invalid(), pending.names()))?;
        exclusivity::census(&root, &[&control_kernel.pair, &kernel.pair])
            .map_err(|cause| failed(cause, pending.names()))?;
        let current = server::engine::client_sessions(&mut state.connection)
            .await
            .map_err(|error| failed(Error::Exclusivity(server::signal(&error)), pending.names()))?;
        exclusivity::only_our_sessions(
            &current.clients,
            &BTreeSet::from([control_kernel.key.clone(), kernel.key.clone()]),
        )
        .map_err(|cause| failed(cause, pending.names()))?;
        if current.own != control_kernel.key
            || current.counter.epoch != inventory.counter.epoch
            || current.counter.total != inventory.counter.total + 1
            || !inventory
                .counter
                .continuity
                .iter()
                .all(|row| current.counter.continuity.contains(row))
        {
            return Err(failed(
                Error::Exclusivity(server::Signal::SessionCounter),
                pending.names(),
            ));
        }
        let identity = server::engine::identity(&mut state.connection)
            .await
            .map_err(|error| failed(Error::Identity(error.to_string()), pending.names()))?;
        if identity != state.identity {
            return Err(failed(invalid(), pending.names()));
        }
        exclusivity::census(&root, &[&control_kernel.pair, &kernel.pair])
            .map_err(|cause| failed(cause, pending.names()))?;
        let relay = pending.relay.take().expect("relay");
        pending.relay_name = None;
        let scratch = ContainerSession {
            connection,
            run: relay,
            kernel,
        };
        // The workload is the first supervised owner. Its deadline began
        // before reservation launched it; scratch setup cannot renew it.
        let deadline = state.workload.deadline();
        let state = self.state.take().expect("candidate owns state");
        self.pending = None;
        self.analysis_in_flight = false;
        let census = Census {
            accepted: inventory.counter.total,
            opened: 1,
            epoch: inventory.counter.epoch,
            continuity: inventory.counter.continuity.into_iter().collect(),
            deadline,
        };
        let control = ContainerControl {
            state: Some(state),
            control_kernel,
            census,
            admin: None,
            retired: Vec::new(),
            unconfirmed: Vec::new(),
            pending_relay: None,
            pending_relay_name: None,
            roles: Vec::new(),
            refusal: None,
            in_flight: false,
            removed: false,
        };
        Ok(ScratchRun::from_container(control, scratch, names))
    }
}
