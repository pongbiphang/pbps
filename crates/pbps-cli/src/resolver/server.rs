//! A dedicated scratch server pbps did not provision.
//!
//! The Docker profile owns everything it uses, so it can require the exact
//! controls it asked for. A supplied server is the operator's, but it is not
//! an arbitrary one: the operator names the externally enforced profile their
//! container meets, pbps reads the daemon's record of that container and then
//! measures the kernel against the profile's **known layout** — a row the
//! layout does not name is refused as that row. It proves the engine is not
//! the target's, reaches it through a run-owned forwarder the way the Docker
//! profile does, proves nothing else can reach it, and only then creates its
//! own uniquely named resources. None of this is binding qualification:
//! engine build compatibility and deployment context are `qualify`'s, the
//! managed declarations cross only in `resolve` (#613), and source handling
//! remains a later #595 step.

use crate::resolver::docker::{CandidateImage, LocalApi, forwarder::Forwarder};
use crate::resolver::native::{
    FORWARDER_PRIVILEGES, MqueueLease, NativeTarget, ProcessLease, TargetWitness, guarded_tasks,
};
use crate::resolver::scope::{self, PlannedGrant};
use pbps_db::Driver;
use pbps_db::resolver::environment::{
    AuthorizationFingerprint, EnvironmentFacts, FactStatus, RuleVersion, ScopeReport, Verdict,
};
use pbps_db::resolver::{InstanceObservation, ScratchNames};
use pbps_db::transport::{StreamConn, StreamLogin};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

mod engine;
mod exclusivity;
pub(crate) mod profile;
mod runtime;

use exclusivity::TcpPair;
use profile::ServerProfile;
use runtime::{ServerProcesses, ServerRuntime};

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error(
        "resolver server profile {name} is not implemented for this engine; implemented: {implemented}"
    )]
    UnsupportedProfile { name: String, implemented: String },
    #[error(
        "the dedicated resolver server endpoint must name a profile, a container, a user and a password"
    )]
    Endpoint,
    #[error(
        "the container runtime managing the dedicated resolver server could not be reached: {0}"
    )]
    Daemon(String),
    #[error("the supplied container is not one this run can use: {0}")]
    Container(&'static str),
    #[error("the supplied container's configuration does not meet the named profile: {0}")]
    Configuration(&'static str),
    #[error("the supplied container does not run the profile's engine executable exactly once")]
    EngineExecutable,
    #[error(
        "the supplied server's externally enforced containment could not be measured or does not meet the named profile: {0}"
    )]
    Containment(Premise),
    #[error("the supplied container's mount table has a row the named profile does not admit: {0}")]
    Mount(String),
    #[error("the supplied resolver server is inside the target's instance")]
    TargetInstance,
    #[error("the run's channel into the supplied server could not be established: {0}")]
    Channel(String),
    #[error("the supplied server is not exclusively this run's; discard the run: {0}")]
    Exclusivity(Signal),
    #[error("the supplied server's required runtime premise changed or became unreadable: {0}")]
    Unqualified(&'static str),
    #[error("the supplied server's read-only engine identity could not be established: {0}")]
    Identity(String),
    #[error("this resolver run exceeded its bounded lifetime on the supplied server")]
    Deadline,
    #[error(
        "a cancelled operation left this resolver run unusable; close it and start a fresh run"
    )]
    Cancelled,
    #[error("the run-owned scratch resources could not be created on the supplied server")]
    Scratch,
    #[error("resolver cleanup could not be confirmed; inspect the reported run-owned resources")]
    Cleanup,
    #[error("this resolver run has already been consumed or closed")]
    Consumed,
    #[error("the analysis scope could not be qualified or changed under the run: {0}")]
    Scope(String),
    #[error("the desired bindings could not be resolved on this run: {0}")]
    Binding(String),
}

/// Which premise of the named profile a refusal is about.
///
/// One sentence for every measurement is diagnosable by nobody. An operator
/// reading a refusal needs to know which control their container does not
/// have, and a fixture whose containers are removed as it exits needs to say
/// so before they go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Premise {
    Resources,
    Lease,
    Network,
    Anchors,
    Mounts,
    Device,
    Occupants,
    Accounting,
}

impl Premise {
    fn named<E>(self) -> impl FnOnce(E) -> Error {
        move |_| Error::Containment(self)
    }
}

impl std::fmt::Display for Premise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Resources => "the container's cgroup bounds are missing, unreadable or above the profile's ceilings",
            Self::Lease => "the engine service process changed or became unreadable",
            Self::Network => "the container's network namespace is not a lone loopback device with no route out",
            Self::Anchors => "the container's /proc, /sys, cgroup or mqueue does not match its private runtime",
            Self::Mounts => "the container's mount table is unreadable",
            Self::Device => "the container's /dev is not a root-owned tmpfs an unprivileged task cannot write to",
            Self::Occupants => "a task in the container's PID namespace is not at the profile's uid, group, privileges or cgroup",
            Self::Accounting => "a process outside the container's PID namespace shares its network, mount or IPC namespace",
        })
    }
}

/// Which signal says the supplied server is not this run's alone.
///
/// Three independent ones answer that question — the kernel's own table of
/// the engine's namespace, the engine's list of who is connected, and its
/// cumulative counter — and a refusal that does not say which is diagnosable
/// by nobody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A socket in the engine's namespace that this run did not open.
    ForeignSocket,
    /// A session this run opened is no longer in the engine's table. Absent
    /// is not accounted for: the run would be holding a dead connection.
    MissingChannel,
    /// The engine reports a session this run did not open, or no longer
    /// reports one it did.
    SessionList,
    /// The cumulative counter has moved by more than this run's own sessions.
    SessionCounter,
    /// A row the counter was summed over has gone, which is how a total is
    /// made to come back down.
    CounterRows,
    /// The engine could not answer at all. Unreadable is not idle.
    Unreadable,
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ForeignSocket => "a socket in the engine's namespace is not this run's",
            Self::MissingChannel => "a session this run opened is no longer there",
            Self::SessionList => "the engine's session list is not this run's alone",
            Self::SessionCounter => "the engine's cumulative session counter moved",
            Self::CounterRows => "a row that counter was summed over has gone",
            Self::Unreadable => "the engine could not report its sessions",
        })
    }
}

/// Failed operations name resources whose removal could not be confirmed.
/// They are generated names, never supplied credentials or existing objects.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{cause}")]
pub struct ServerFailure {
    pub cause: Error,
    pub recovery_names: Vec<String>,
}

impl From<Error> for ServerFailure {
    fn from(cause: Error) -> Self {
        failure(cause)
    }
}

fn failure(cause: Error) -> ServerFailure {
    ServerFailure {
        cause,
        recovery_names: Vec::new(),
    }
}

/// Everything an actual run needs about one supplied server, as one
/// validated value read from the configured credential variable.
///
/// The externally enforced profile and the container are facts about a
/// running deployment, not checked-in intentions, so they live with the
/// credentials rather than in `pbps.yml`. Nothing in git therefore claims a
/// server meets a profile pbps has not measured. There is no host, port or
/// socket: the engine is reached through the container runtime alone.
pub struct ScratchEndpoint {
    profile: String,
    container: String,
    daemon: PathBuf,
    user: String,
    password: String,
}

impl std::fmt::Debug for ScratchEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScratchEndpoint")
            .field("profile", &self.profile)
            .field("container", &self.container)
            .field("daemon", &self.daemon)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

const DEFAULT_DAEMON: &str = "/var/run/docker.sock";

impl ScratchEndpoint {
    /// Parses the configured environment variable's value. A failure never
    /// echoes the value: it carries the scratch server's password.
    pub fn parse(value: &str) -> Result<Self, Error> {
        let mut fields: [(&str, Option<String>); 5] = [
            ("profile", None),
            ("container", None),
            ("daemon", None),
            ("user", None),
            ("password", None),
        ];
        for field in value.split_whitespace() {
            let (key, value) = field.split_once('=').ok_or(Error::Endpoint)?;
            let slot = fields
                .iter_mut()
                .find(|(name, _)| *name == key)
                .ok_or(Error::Endpoint)?;
            if value.is_empty() || slot.1.replace(value.to_owned()).is_some() {
                return Err(Error::Endpoint);
            }
        }
        let [profile, container, daemon, user, password] = fields.map(|(_, value)| value);
        let profile = profile.ok_or(Error::Endpoint)?;
        if named(&profile) != profile {
            return Err(Error::Endpoint);
        }
        let container = container.ok_or(Error::Endpoint)?;
        if container.len() > 256
            || container.starts_with('-')
            || !container
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(Error::Endpoint);
        }
        let daemon = PathBuf::from(daemon.as_deref().unwrap_or(DEFAULT_DAEMON));
        if !daemon.is_absolute() {
            return Err(Error::Endpoint);
        }
        Ok(Self {
            profile,
            container,
            daemon,
            user: user.ok_or(Error::Endpoint)?,
            password: password.ok_or(Error::Endpoint)?,
        })
    }

    /// Reads the configured credential variable. Selection (#606) never does
    /// this; only a run that actually needs a resolver reads the value, and a
    /// failure never echoes it.
    pub fn from_profile(profile: &pbps_config::resolver::ResolverProfile) -> Result<Self, Error> {
        let pbps_config::resolver::ResolverProfile::Server { url_env } = profile else {
            return Err(Error::Endpoint);
        };
        Self::parse(&std::env::var(url_env).map_err(|_| Error::Endpoint)?)
    }

    fn login(&self, database: &str) -> StreamLogin {
        StreamLogin {
            user: self.user.clone(),
            password: self.password.clone(),
            database: database.to_owned(),
        }
    }
}

/// The daemon's record of the supplied container, pinned at admission and
/// compared on every check. A restarted or replaced container is a different
/// runtime, whatever its name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pinned {
    id: String,
    pid: u32,
    started: String,
    image: String,
}

fn pin(state: &Value) -> Result<Pinned, Error> {
    let id = state["Id"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(Error::Container("the runtime reports no container id"))?;
    let running = &state["State"];
    if running["Running"] != true || running["Restarting"] != false || running["Paused"] != false {
        return Err(Error::Container("the container is not running"));
    }
    let pid = running["Pid"]
        .as_u64()
        .filter(|pid| *pid > 0)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or(Error::Container("the runtime reports no init process"))?;
    let started = running["StartedAt"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(Error::Container("the runtime reports no start time"))?;
    let image = state["Image"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(Error::Container("the runtime reports no image"))?;
    Ok(Pinned {
        id: id.to_owned(),
        pid,
        started: started.to_owned(),
        image: image.to_owned(),
    })
}

/// An admitted supplied server, bound to the connection that qualified it.
///
/// Every capability here is a live handle. Losing the channel, the runtime
/// premises, exclusivity or the target binding discards the analysis; a
/// later matching observation cannot revive it. What survives all of that is
/// the cleanup capability, which is not a handle at all: the credentials, the
/// daemon path and the container's pinned identity, from which a fresh
/// session can be opened to remove exactly the two generated names.
pub struct DedicatedServer {
    inner: Option<Inner>,
}

struct Inner {
    control: Control,
    /// `None` once a check has refused: the analysis is over, the cleanup
    /// capability is not.
    analysis: Option<Analysis>,
    refusal: Option<Error>,
}

/// What removing the run's resources needs, and nothing that measuring the
/// run needs. Kept apart from `Analysis` so that no failure of the analysis
/// can take it away (finding on #640: eight review findings were one shape,
/// a failure path that had moved the state out of `self` and dropped the
/// control session with it).
struct Control {
    endpoint: ScratchEndpoint,
    profile: &'static ServerProfile,
    driver: Driver,
    api: LocalApi,
    image: CandidateImage,
    pinned: Pinned,
    identity: InstanceObservation,
    /// The control session, opened through a run-owned forwarder. `None`
    /// after a cancelled await left its protocol stream in no known state;
    /// cleanup then opens a fresh one.
    session: Option<Session>,
    /// Recorded before the first creation statement runs, so a future
    /// cancelled anywhere during creation still leaves the names where
    /// `discard` can act on them.
    pending: Option<ScratchNames>,
    /// Set across every await on the control session. Found set on entry, it
    /// says the previous await was cancelled mid-exchange, which is the one
    /// way a protocol stream is left in a state nobody can reason about.
    in_flight: bool,
    /// Forwarders whose session is over but whose removal has not been
    /// confirmed: a poisoned control session's, a refused scratch session's.
    /// Dropping one requests its removal; only `close` confirms it, and a
    /// run-owned container nobody can confirm gone is a recovery name like
    /// the database and the login (finding on #640).
    stale: Vec<Forwarder>,
    /// Forwarder containers already reported unconfirmed.
    unconfirmed: Vec<String>,
    /// Run-local authorization roles this run created on the shared server,
    /// recorded before they are created so cleanup drops them and names any it
    /// cannot (finding-family #652). Global to the cluster, so the scratch
    /// database being dropped does not remove them.
    roles: Vec<String>,
    /// An administrative session on the scratch database, from the moment it
    /// opens until the step that needed it retires it. Held here rather than
    /// in that step's locals so that a cancelled step leaves it where cleanup
    /// can confirm its forwarder's removal or name it (#1031); found here on
    /// the next check, it is the mark of that cancellation.
    admin: Option<Session>,
}

impl Control {
    /// Qualification returns its refusal immediately, but cleanup still owns
    /// every forwarder whose failed opening could not confirm removal.
    fn retain_failure(&mut self, failure: ServerFailure) -> Error {
        self.unconfirmed.extend(failure.recovery_names);
        failure.cause
    }

    /// Ends a session without confirming its forwarder's removal yet.
    fn retire(&mut self, session: Session) {
        let Session {
            connection,
            forwarder,
            ..
        } = session;
        drop(connection);
        self.stale.push(forwarder);
    }
}

struct Analysis {
    runtime: ServerRuntime,
    target: TargetWitness,
    deadline: Instant,
    /// The engine's cumulative session count when this run was admitted, and
    /// how many sessions the run has opened since. Their sum is the only
    /// total the engine may report: one more means a session this run did
    /// not open, even if it had already closed by the time of the read.
    accepted: u64,
    opened: u64,
    /// The counter's origin. If it changes the engine restarted or its
    /// statistics were reset, and nothing may be compared across that.
    epoch: String,
    /// What the total was summed over when it was taken. A total alone can be
    /// brought back down — dropping a PostgreSQL database takes its share of
    /// the sum with it — so a row that has since disappeared is a refusal
    /// even when the arithmetic works out.
    continuity: BTreeSet<String>,
}

/// What opening a session through the daemon needs, borrowed from wherever
/// it currently lives.
#[derive(Clone, Copy)]
struct Channel<'a> {
    api: &'a LocalApi,
    image: &'a CandidateImage,
    driver: Driver,
    pinned: &'a Pinned,
    profile: &'static ServerProfile,
}

impl Control {
    fn channel(&self) -> Channel<'_> {
        Channel {
            api: &self.api,
            image: &self.image,
            driver: self.driver,
            pinned: &self.pinned,
            profile: self.profile,
        }
    }
}

/// One session into the engine, bound to its actual kernel endpoints.
struct Session {
    connection: StreamConn,
    forwarder: Forwarder,
    /// The forwarder container's init: the root deadline guard, whose PID
    /// namespace is the one place outside the engine's allowed to share its
    /// network namespace.
    guard: ProcessLease,
    /// The fixed process policy alone cannot identify an exposed IPC filesystem.
    mqueue: MqueueLease,
    pair: TcpPair,
    /// The engine process holding the server end of this session.
    backend: ProcessLease,
    /// The engine's own key for this session, read without privilege because
    /// a run-owned login has none.
    session_key: String,
}

impl Session {
    /// Opens one qualified session: a run-owned forwarder into the container,
    /// one login through it, and the kernel's word that the session it holds
    /// is the one the engine sees.
    async fn open(
        channel: Channel<'_>,
        runtime: &ServerRuntime,
        login: StreamLogin,
        known: &[&TcpPair],
    ) -> Result<Self, ServerFailure> {
        let (forwarder, mut connection) = Forwarder::open(
            channel.api,
            channel.image,
            channel.driver,
            &channel.pinned.id,
            login,
            channel.profile.lifetime.as_secs(),
        )
        .await
        .map_err(|failure| ServerFailure {
            cause: Error::Channel(failure.cause.to_string()),
            recovery_names: failure.recovery_names,
        })?;
        #[cfg(test)]
        live_tests::admission_recovery::after_open(&forwarder);
        // The forwarder is a run-owned container from here on. Every step
        // below can fail, and dropping the forwarder would only request its
        // removal, not confirm it, leaving a container neither cleanup nor a
        // recovery name can identify (finding on #640). So it stays a local
        // through the fallible steps and is closed on failure; only complete
        // success moves it into the returned session. An unconfirmed close
        // carries its generated name separately from the original refusal.
        #[cfg(test)]
        live_tests::forwarder_mqueue::after_open(&forwarder);
        let prepared = async {
            let guard = forwarder
                .pid()
                .map_err(|error| Error::Channel(error.to_string()))
                .and_then(|pid| {
                    ProcessLease::capture(pid).map_err(|_| {
                        Error::Channel("the forwarder's init process is unreadable".into())
                    })
                })?;
            let mqueue = MqueueLease::capture(&guard).map_err(|_| {
                Error::Channel(
                    "the forwarder's mqueue does not belong to its held IPC namespace".into(),
                )
            })?;
            let (pair, backend) =
                exclusivity::bind(runtime.init(), &guard, channel.profile.port, known)
                    .map_err(|reason| Error::Channel(reason.to_owned()))?;
            check_kernel_parts(runtime.init(), &guard, &mqueue, &pair, &backend)?;
            let own = engine::own_session(&mut connection)
                .await
                .map_err(|error| Error::Identity(error.to_string()))?;
            // PostgreSQL's backend reports its own pid; the process holding the
            // server end of our session must be that backend. SQL Server has
            // no such mapping and its engine process is the holder.
            correlate(&own.process, backend.namespace_pid(), runtime.engine())?;
            check_kernel_parts(runtime.init(), &guard, &mqueue, &pair, &backend)?;
            Ok::<_, Error>((guard, mqueue, pair, backend, own.key))
        }
        .await;
        match prepared {
            Ok((guard, mqueue, pair, backend, session_key)) => Ok(Self {
                connection,
                forwarder,
                guard,
                mqueue,
                pair,
                backend,
                session_key,
            }),
            Err(error) => Err(close_failed_session(connection, forwarder, error).await),
        }
    }

    /// The forwarder is still the fixed program at its fixed privileges, and
    /// the session's two ends are still where they were, held by whom they
    /// were held by.
    fn check_kernel(&self, init: &ProcessLease) -> Result<(), Error> {
        check_kernel_parts(init, &self.guard, &self.mqueue, &self.pair, &self.backend)
    }

    async fn check(&self, init: &ProcessLease) -> Result<(), Error> {
        self.check_kernel(init)?;
        self.forwarder
            .check()
            .await
            .map_err(|error| Error::Channel(error.to_string()))?;
        self.check_kernel(init)
    }
}

/// The kernel half of a session check, on the parts rather than a built
/// `Session`, so `Session::open` can run it before it owns one.
fn check_kernel_parts(
    init: &ProcessLease,
    forwarder_guard: &ProcessLease,
    mqueue: &MqueueLease,
    pair: &TcpPair,
    backend: &ProcessLease,
) -> Result<(), Error> {
    let channel = |reason: &str| Error::Channel(reason.to_owned());
    guarded_tasks(forwarder_guard, FORWARDER_PRIVILEGES).map_err(|_| {
        channel("a forwarder task does not meet the fixed privilege and descriptor limits")
    })?;
    mqueue
        .check(forwarder_guard)
        .map_err(|_| channel("the forwarder's mqueue does not belong to its held IPC namespace"))?;
    exclusivity::still_bound(init, pair, backend)
        .map_err(|_| channel("the session's kernel endpoints changed"))
}

impl Analysis {
    async fn check(&mut self, control: &mut Control, extra: Option<&Session>) -> Result<(), Error> {
        if Instant::now() >= self.deadline {
            return Err(Error::Deadline);
        }
        let target_changed =
            |_| Error::Unqualified("the target binding this run was aimed at changed");
        self.target.check().map_err(target_changed)?;
        let state = control
            .api
            .inspect_container(&control.pinned.id)
            .await
            .map_err(|error| Error::Daemon(error.to_string()))?
            .ok_or(Error::Container("the container is gone"))?;
        if pin(&state)? != control.pinned {
            return Err(Error::Container("the container was restarted or replaced"));
        }
        let expected = self.kernel_side(control, extra).await?;
        // Read the engine's own answer between two kernel censuses: neither
        // signal may be the only thing standing between two compilations.
        let session = control.session.as_mut().ok_or(Error::Cancelled)?;
        let reported = engine::client_sessions(&mut session.connection)
            .await
            .map_err(|error| Error::Exclusivity(signal(&error)))?;
        if reported.own != session.session_key {
            return Err(Error::Unqualified(
                "the engine no longer reports this run's own session",
            ));
        }
        self.counted(&reported.counter)?;
        exclusivity::only_our_sessions(&reported.clients, &expected)?;
        let identity = engine::identity(&mut session.connection)
            .await
            .map_err(|error| Error::Identity(error.to_string()))?;
        if identity != control.identity {
            return Err(Error::Unqualified("the engine's instance identity changed"));
        }
        self.kernel_side(control, extra).await?;
        self.target.check().map_err(target_changed)?;
        // The inventory above is only as fresh as the moment it was read, and
        // the checks after it look at nothing a session would move. Closing
        // the window with one more counter read is what makes the whole of
        // this check, rather than an instant inside it, the thing that holds.
        let session = control.session.as_mut().ok_or(Error::Cancelled)?;
        let closing = engine::session_counter(&mut session.connection)
            .await
            .map_err(|error| Error::Exclusivity(signal(&error)))?;
        self.counted(&closing)
    }

    /// Everything the kernel says: the runtime premises, each session's
    /// endpoints and forwarder, and the census of the engine's namespace.
    /// Answers with the session keys the engine must then report.
    async fn kernel_side(
        &self,
        control: &Control,
        extra: Option<&Session>,
    ) -> Result<BTreeSet<String>, Error> {
        let session = control.session.as_ref().ok_or(Error::Cancelled)?;
        let mut forwarders = vec![&session.guard];
        forwarders.extend(extra.map(|extra| &extra.guard));
        self.runtime.check(&forwarders)?;
        let init = self.runtime.init();
        session.check(init).await?;
        if let Some(extra) = extra {
            extra.check(init).await?;
        }
        let mut pairs = vec![&session.pair];
        pairs.extend(extra.map(|extra| &extra.pair));
        exclusivity::census(init, &pairs)?;
        let mut expected = BTreeSet::from([session.session_key.clone()]);
        expected.extend(extra.map(|extra| extra.session_key.clone()));
        Ok(expected)
    }

    /// The only total this run may see, and the rows it must still be over.
    fn counted(&self, counter: &pbps_db::resolver::SessionCounter) -> Result<(), Error> {
        if counter.epoch != self.epoch {
            return Err(Error::Unqualified(
                "the engine's statistics epoch changed; it restarted or was reset",
            ));
        }
        if counter.total != self.accepted + self.opened {
            return Err(Error::Exclusivity(Signal::SessionCounter));
        }
        // Superset: this run's own scratch database adds a row, and nothing
        // may take one away — that is how a total is made to come back down.
        //
        // This catches a row *present at admission* being dropped to pay for
        // an intruding session. It does not catch add-use-remove: a database
        // created, used and dropped entirely between two of this run's checks
        // was never in the baseline set, so its increment and its removal both
        // fall outside every snapshot — a limitation of PostgreSQL's
        // per-database counter, of the same family as the walsender case
        // (#651), recorded as #682. SQL Server's single server-level counter
        // has no row to drop and catches it.
        if !self
            .continuity
            .iter()
            .all(|row| counter.continuity.contains(row))
        {
            return Err(Error::Exclusivity(Signal::CounterRows));
        }
        Ok(())
    }
}

impl Inner {
    /// Every check goes through here: a refusal ends the analysis for good,
    /// and a cancelled one is found on the next call by the flag it left set.
    async fn check(&mut self, extra: Option<&Session>) -> Result<(), Error> {
        if let Some(admin) = self.control.admin.take() {
            self.control.retire(admin);
            self.refuse(Error::Cancelled);
        }
        if self.control.in_flight {
            if let Some(session) = self.control.session.take() {
                self.control.retire(session);
            }
            self.control.in_flight = false;
            self.refuse(Error::Cancelled);
        }
        let Some(analysis) = self.analysis.as_mut() else {
            return Err(self.refusal.clone().unwrap_or(Error::Cancelled));
        };
        self.control.in_flight = true;
        let outcome = analysis.check(&mut self.control, extra).await;
        self.control.in_flight = false;
        if let Err(cause) = &outcome {
            self.refuse(cause.clone());
        }
        outcome
    }

    fn refuse(&mut self, cause: Error) {
        self.analysis = None;
        self.refusal.get_or_insert(cause);
    }

    fn live(&self) -> Result<&Analysis, Error> {
        self.analysis
            .as_ref()
            .ok_or_else(|| self.refusal.clone().unwrap_or(Error::Cancelled))
    }
}

/// A failed admission has no owner left to retain its forwarder. Close the
/// protocol stream first, then confirm removal or return its generated name.
async fn close_failed_session(
    connection: StreamConn,
    forwarder: Forwarder,
    cause: Error,
) -> ServerFailure {
    drop(connection);
    let name = forwarder.resource_name().to_owned();
    let mut outcome = failure(cause);
    if forwarder.close().await.is_err() {
        outcome.recovery_names.push(name);
    }
    outcome
}

impl DedicatedServer {
    /// No database, DDL or declaration crosses this call: it ends with a
    /// qualified control session on the supplied maintenance database. A
    /// refusal names any run-owned forwarder whose removal is unconfirmed.
    pub async fn admit(
        endpoint: ScratchEndpoint,
        target: &mut NativeTarget,
    ) -> Result<Self, ServerFailure> {
        // Captured before any forwarder is opened, so the analysis deadline is
        // no later than every forwarder's own root-guard deadline: a forwarder
        // is created after this instant and its guard lives `profile.lifetime`
        // from then, so `admitted + lifetime` expires first. Setting the
        // deadline after admission instead let the supervisor remove a
        // forwarder while the analysis still thought the run live, refusing an
        // otherwise valid check near the bound (finding on #640).
        let admitted = Instant::now();
        let driver = target
            .driver()
            .map_err(|_| Error::Unqualified("the target's driver is unreadable"))?;
        let profile = profile::supported(&endpoint.profile, driver).ok_or_else(|| {
            Error::UnsupportedProfile {
                name: endpoint.profile.clone(),
                implemented: profile::implemented(driver).join(", "),
            }
        })?;
        let daemon = |error: crate::resolver::docker::Error| Error::Daemon(error.to_string());
        let mut api = LocalApi::connect_native(&endpoint.daemon)
            .await
            .map_err(daemon)?;
        let state = api
            .inspect_container(&endpoint.container)
            .await
            .map_err(daemon)?
            .ok_or(Error::Container("no such container"))?;
        let pinned = pin(&state)?;
        // Separation is established from the actual processes before any
        // database is created, any DDL runs or any declaration is sent — and
        // before the configuration and containment measurements, so a
        // resolver aimed at the target is refused as the target rather than
        // as a container that happens to be configured differently.
        let processes = ServerProcesses::identify(pinned.pid, profile)?;
        target
            .check()
            .await
            .map_err(|_| Error::Unqualified("the target this run was aimed at changed"))?;
        processes.separate_from(
            target
                .service()
                .map_err(|_| Error::Unqualified("the target's service process is unreadable"))?,
        )?;
        profile::configuration(&state).map_err(Error::Configuration)?;
        let runtime = ServerRuntime::bind(processes, profile)?;
        runtime.check(&[])?;
        let image = api
            .inspect_image(&pinned.image)
            .await
            .map_err(daemon)?
            .ok_or(Error::Container("the container's image is gone"))?;
        let channel = Channel {
            api: &api,
            image: &image,
            driver,
            pinned: &pinned,
            profile,
        };
        let login = endpoint.login(profile.maintenance_database);
        let mut session = Session::open(channel, &runtime, login, &[]).await?;
        let prepared = async {
            // The supplied credentials are the administrative ones, so this is
            // where the privileged reads belong: the identity, and the inventory
            // and counter the exclusion strategy compares against.
            let identity = engine::identity(&mut session.connection)
                .await
                .map_err(|error| Error::Identity(error.to_string()))?;
            let inventory = engine::client_sessions(&mut session.connection)
                .await
                .map_err(|error| Error::Exclusivity(signal(&error)))?;
            if inventory.own != session.session_key {
                return Err(Error::Unqualified(
                    "the engine does not report this run's own session",
                ));
            }
            // The first inventory is the baseline every later total is measured
            // against, so a session this run did not open must be refused here
            // rather than absorbed into it: one that disconnects before the next
            // read would leave a clean list and an inflated baseline behind.
            exclusivity::only_our_sessions(
                &inventory.clients,
                &BTreeSet::from([session.session_key.clone()]),
            )?;
            // A cloned cluster can report the target's identifier. It is compared
            // in addition to the runtime separation above, never instead of it.
            if identity.instance_key
                == target
                    .identity()
                    .map_err(|_| Error::Unqualified("the target's identity is unreadable"))?
                    .instance_key
            {
                return Err(Error::TargetInstance);
            }
            let witness = target
                .witness()
                .map_err(|_| Error::Unqualified("the target's witness is unreadable"))?;
            Ok::<_, Error>((identity, inventory, witness))
        }
        .await;
        let (identity, inventory, witness) = match prepared {
            Ok(prepared) => prepared,
            Err(cause) => {
                return Err(
                    close_failed_session(session.connection, session.forwarder, cause).await,
                );
            }
        };
        let mut inner = Inner {
            control: Control {
                endpoint,
                profile,
                driver,
                api,
                image,
                pinned,
                identity,
                session: Some(session),
                pending: None,
                in_flight: false,
                stale: Vec::new(),
                unconfirmed: Vec::new(),
                roles: Vec::new(),
                admin: None,
            },
            analysis: Some(Analysis {
                runtime,
                target: witness,
                deadline: admitted + profile.lifetime,
                accepted: inventory.counter.total,
                opened: 0,
                epoch: inventory.counter.epoch,
                continuity: inventory.counter.continuity.into_iter().collect(),
            }),
            refusal: None,
        };
        #[cfg(test)]
        live_tests::admission_recovery::after_admission(
            &inner.control.session.as_ref().unwrap().forwarder,
        );
        if let Err(cause) = inner.check(None).await {
            let names = close_control(&mut inner.control).await;
            return Err(report(failure(cause), names));
        }
        Ok(Self { inner: Some(inner) })
    }

    /// Read-only: the engine identity the admission established. Refused
    /// once anything has invalidated the analysis, and an observed failure
    /// here discards it for good. Leaving it in place would let a restored
    /// limit, a removed mount or an exited extra process revive an admission
    /// that had already been refused — the change-and-restore case, one
    /// level up.
    pub fn identity(&mut self) -> Result<&InstanceObservation, Error> {
        let inner = self.inner.as_mut().ok_or(Error::Consumed)?;
        let analysis = inner.live()?;
        let outcome = analysis
            .target
            .check()
            .map_err(|_| Error::Unqualified("the target binding this run was aimed at changed"))
            .and_then(|()| {
                let session = inner.control.session.as_ref().ok_or(Error::Cancelled)?;
                analysis.runtime.check(&[&session.guard])?;
                session.check_kernel(analysis.runtime.init())
            });
        if let Err(cause) = &outcome {
            inner.refuse(cause.clone());
        }
        outcome?;
        Ok(&inner.control.identity)
    }

    pub async fn check(&mut self) -> Result<(), Error> {
        self.inner
            .as_mut()
            .ok_or(Error::Consumed)?
            .check(None)
            .await
    }

    /// Creates this run's own uniquely named database and login, then opens a
    /// second qualified session as that login. Anything already on the server
    /// is left exactly as it was, including its logging, audit and grants.
    ///
    /// Takes `&mut self` rather than consuming the server so that a future
    /// cancelled part-way through creation leaves the caller holding
    /// something: the names are recorded before the first statement runs, and
    /// `discard` removes whatever landed. The server moves into the run only
    /// once creation has completed.
    pub async fn open_scratch(
        &mut self,
        recipe: &pbps_db::resolver::environment::DatabaseRecipe,
    ) -> Result<ScratchRun, ServerFailure> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| failure(Error::Consumed))?;
        let (scratch, names) = Self::create(inner, recipe).await?;
        // Nothing awaits between here and the move, so a cancellation cannot
        // strike a run that exists in neither place.
        let inner = self.inner.take().ok_or_else(|| failure(Error::Consumed))?;
        Ok(ScratchRun {
            inner,
            scratch: Some(scratch),
            names,
            in_flight: false,
            removed: false,
            scope: None,
            compiled: false,
        })
    }

    async fn create(
        inner: &mut Inner,
        recipe: &pbps_db::resolver::environment::DatabaseRecipe,
    ) -> Result<(Session, ScratchNames), ServerFailure> {
        inner.check(None).await.map_err(failure)?;
        let names = generated_names().map_err(failure)?;
        inner.control.pending = Some(names.clone());
        let created = {
            let session = inner
                .control
                .session
                .as_mut()
                .ok_or_else(|| failure(Error::Cancelled))?;
            inner.control.in_flight = true;
            let created = engine::create_scratch(&mut session.connection, &names, recipe).await;
            inner.control.in_flight = false;
            created
        };
        if created.is_err() {
            // Creation is several statements; remove whatever landed. The
            // analysis is over either way: a server this run could not
            // create on is not one it can compile on.
            inner.refuse(Error::Scratch);
            return Err(cleanup(&mut inner.control, &names, Error::Scratch).await);
        }
        let opened = async {
            let analysis = inner.live()?;
            let control_pair = &inner.control.session.as_ref().ok_or(Error::Cancelled)?.pair;
            Session::open(
                inner.control.channel(),
                &analysis.runtime,
                StreamLogin {
                    user: names.login().to_owned(),
                    password: names.password().to_owned(),
                    database: names.database().to_owned(),
                },
                &[control_pair],
            )
            .await
        }
        .await;
        let scratch = match opened {
            Ok(scratch) => scratch,
            Err(failure) => {
                let cause = inner.control.retain_failure(failure);
                inner.refuse(cause.clone());
                return Err(cleanup(&mut inner.control, &names, cause).await);
            }
        };
        // Opening the scratch session moved the engine's counter by one, and
        // the control session is what reads it back.
        if let Some(analysis) = inner.analysis.as_mut() {
            analysis.opened = 1;
        }
        if let Err(cause) = inner.check(Some(&scratch)).await {
            // Retire rather than drop: dropping only requests the forwarder's
            // background removal, so a later cleanup could report success
            // without confirming this container is gone (finding on #640).
            inner.control.retire(scratch);
            return Err(cleanup(&mut inner.control, &names, cause).await);
        }
        Ok((scratch, names))
    }

    /// Removes anything a cancelled `open_scratch` created, and discards the
    /// server. Safe to call at any time and safe to call again: a cancelled
    /// `discard` leaves the server holding the same names to retry with.
    pub async fn discard(&mut self) -> Result<(), ServerFailure> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        let Some(names) = inner.control.pending.clone() else {
            let unconfirmed = close_control(&mut inner.control).await;
            return if unconfirmed.is_empty() {
                // Only now is there nothing left to confirm; keep the run
                // otherwise so a retried `discard` re-reports the container
                // `close_control` wrote back into run-owned state (finding on
                // #640), rather than entering the empty branch and reporting
                // success.
                self.inner = None;
                Ok(())
            } else {
                Err(ServerFailure {
                    cause: Error::Cleanup,
                    recovery_names: unconfirmed,
                })
            };
        };
        let removal = cleanup(&mut inner.control, &names, Error::Cleanup).await;
        // Unconditionally, as `close` does: when `remove` could not confirm
        // the scratch objects gone, the control session and any retired
        // forwarder are still this run's and must be closed and named, not
        // left to a `Drop` that only requests removal (finding on #640).
        let unconfirmed = close_control(&mut inner.control).await;
        let removal = report(removal, unconfirmed);
        if removal.recovery_names.is_empty() {
            self.inner = None;
            return Ok(());
        }
        Err(removal)
    }
}

/// An admitted server plus this run's own scratch resources.
pub struct ScratchRun {
    inner: Inner,
    /// The compilation capability. Dropped on the first refusal, so no later
    /// matching observation can revive the run, while everything cleanup
    /// needs stays.
    scratch: Option<Session>,
    names: ScratchNames,
    in_flight: bool,
    removed: bool,
    /// The qualified analysis scope, once `qualify` has established it. Bound
    /// to the scratch session, so a reopened session cannot inherit it.
    scope: Option<QualifiedScope>,
    /// Set once `resolve` has started compiling: the scratch database then
    /// holds a namespace, and a second compilation into it would build on
    /// what the first left rather than on the scope alone.
    compiled: bool,
}

/// What a run is asked to qualify: the schemas its plan writes to, the extras
/// the dialect appends after each on the write path, and the grants the plan
/// performs before its DDL. Pure data supplied by the caller.
#[derive(Debug, Clone, Default)]
pub struct ScopeRequest {
    pub schemas: Vec<String>,
    pub write_path_extras: Vec<String>,
    pub planned: Vec<PlannedGrant>,
}

/// What a run compiles and compares. Pure data supplied by the planner.
#[derive(Debug, Clone, Copy)]
pub struct BindingRequest<'a> {
    /// The plan from an empty schema to `desired`, as the differ produces it.
    pub bootstrap: &'a [pbps_model::Change],
    /// The declarations. Scratch's user schemas hold these and nothing else.
    pub desired: &'a pbps_model::Schema,
    /// The target's managed side, by the names pbps's own model gives it.
    pub base: &'a pbps_model::Schema,
}

/// A scope the run has qualified: the compatibility report, the deployment
/// authorization fingerprint, and everything requalification re-compares
/// against. Only `qualify` builds it; it is bound to the target and scratch
/// connections so no other run or reopened session can present it as its own.
struct QualifiedScope {
    report: ScopeReport,
    authorization: AuthorizationFingerprint,
    /// The target's facts as sealed at `qualify`, visibility expected after
    /// the plan's grants: what every later check re-reads the target against.
    target: EnvironmentFacts,
    /// The digest of the target's authorization as read, before the plan's
    /// grants are projected onto it. The projected digest above cannot see a
    /// target that performed one of those very grants in the meantime — the
    /// projection is idempotent — so the raw read is sealed and compared too
    /// (finding on #688).
    target_authorization: String,
    /// The digest of the scratch side's own context as the reproduced
    /// deployer read it once the scope settled: what a later check compares
    /// the scratch side with on an engine whose scratch, having run the
    /// plan's grants, no longer equals the target as read.
    scratch_authorization: String,
    map: scope::Principals,
    schemas: Vec<String>,
    write_path_extras: Vec<String>,
    planned: Vec<PlannedGrant>,
    target_connection: pbps_db::transport::ConnectionId,
    scratch_connection: pbps_db::transport::ConnectionId,
}

impl ScratchRun {
    pub fn database(&self) -> &str {
        self.names.database()
    }

    /// The verdict of the qualified scope, if `qualify` has run. `Verified`
    /// is the only value a later delivery step may build evidence on.
    pub fn verdict(&self) -> Option<Verdict> {
        self.scope.as_ref().map(|scope| scope.report.verdict())
    }

    /// The deployment authorization fingerprint of the qualified scope, which
    /// later steps seal (#614) and apply rechecks against its own session
    /// (#616). Present once `qualify` has run.
    pub fn authorization_fingerprint(&self) -> Option<&AuthorizationFingerprint> {
        self.scope.as_ref().map(|scope| &scope.authorization)
    }

    /// The target and scratch connections the scope is bound to, so a caller
    /// can confirm a reused scope belongs to the connections in hand.
    pub fn scope_connections(
        &self,
    ) -> Option<(
        pbps_db::transport::ConnectionId,
        pbps_db::transport::ConnectionId,
    )> {
        self.scope
            .as_ref()
            .map(|scope| (scope.target_connection, scope.scratch_connection))
    }

    /// Qualifies the analysis scope: reads the target's environment and
    /// deployment authorization, reproduces that authorization on the scratch
    /// database, reads the scratch environment as the reproduced deployer, and
    /// compares the two under the versioned rule (ADR-0016 cases 5, 16, 23;
    /// SPEC §9.3.3). The result is sealed and bound to both connections. No
    /// declaration is transferred and no SQL surface is exposed here.
    ///
    /// Every engine-specific step routes through `resolver::scope`; this is
    /// the lifecycle, which is the same for both engines. Qualification is
    /// not a binding adapter: SQL Server's stays unimplemented (#619, #620).
    pub async fn qualify(
        &mut self,
        target: &mut NativeTarget,
        request: &ScopeRequest,
    ) -> Result<Verdict, Error> {
        // A cancelled step's session, or a refusal already recorded, ends
        // the run before anything below records new cleanup state.
        self.refuse_held_admin()?;
        self.inner.live()?;
        let driver = self.inner.control.driver;
        let read =
            |error: crate::resolver::native::EnvironmentError| Error::Scope(error.to_string());
        let db = |error: pbps_db::DbError| Error::Scope(error.to_string());
        // The target as its own deployer sees it. Authorization is read and
        // reconstructed for the write-path extras too, not only the in-scope
        // schemas, or a schema on the path but outside `schemas` would be
        // missing from scratch and drop out of its visibility (finding on
        // #688); which schemas that is, is the engine's to say.
        let scope_schemas =
            scope::scope_schemas(driver, &request.schemas, &request.write_path_extras)
                .map_err(Error::Scope)?;
        let (mut target_facts, target_auth) = target
            .scope_facts(
                &request.schemas,
                &request.write_path_extras,
                &scope_schemas,
                &request.planned,
            )
            .await
            .map_err(read)?;
        // A planned grant whose grantor the engine would pick among several
        // inherited option holders cannot be predicted here (the pick follows
        // catalog order, which the reproduction's fresh roles do not share),
        // so the scope is refused before anything is built rather than
        // guessed at (finding on #688).
        let ambiguous = target_auth.unpredictable(&request.planned);
        if !ambiguous.is_empty() {
            return Err(Error::Scope(format!(
                "the grantor of a planned grant cannot be predicted on the target: {}",
                ambiguous.join("; ")
            )));
        }
        let target_connection = target
            .connection_id()
            .map_err(|_| Error::Scope("the target binding is unreadable".into()))?;
        // Reproduce that authorization on the scratch database through the
        // control session; the role names are recorded before creation so
        // cleanup drops them even if reconstruction fails part-way.
        let login = self.names.login().to_owned();
        let token = login[login.len().saturating_sub(16)..].to_owned();
        let map = scope::Principals::generate(&target_auth, &request.planned, &login, &token);
        // Added to, never replaced: a role an earlier attempt may have
        // created must stay among the names cleanup drops.
        for role in map.server_wide_names() {
            if !self.inner.control.roles.contains(&role) {
                self.inner.control.roles.push(role);
            }
        }
        // The mapped deployer runs the plan's grants on scratch and is the
        // principal every scratch read below runs as; its name follows from
        // the principal alone, which the planned grants do not change. `None`
        // is a deployer the scratch session already is.
        let deployer = map
            .deployer(&target_auth)
            .map_err(|reason| Error::Scope(reason.into()))?;
        // Reconstruction's schema and grant DDL is database-local, so it must
        // run *in* the scratch database — the control session is on the
        // maintenance database and its `CREATE SCHEMA`/`GRANT ON SCHEMA` would
        // land there instead (finding on #688). Open an admin session to the
        // scratch database for it; the run login cannot, being unprivileged.
        self.admin_session().await?;
        let reconstruction = self.inner.control.admin.as_mut().ok_or(Error::Cancelled)?;
        let outcome = scope::prepare(
            &mut reconstruction.connection,
            &map,
            &target_auth,
            &request.planned,
            self.names.database(),
            self.names.login(),
        )
        .await
        .map_err(db);
        // The admin session is done either way; retire it so cleanup closes
        // its forwarder and reports it if that cannot be confirmed.
        self.retire_admin();
        outcome?;
        // The scratch session opened before reconstruction, so it did not load
        // the login defaults reconstruction set — PostgreSQL's `ALTER ROLE ...
        // SET` and `session_preload_libraries`, which a live session cannot
        // load at all; SQL Server's default language, which decides the date
        // format a statement reads a literal under. Reopen it so the
        // environment read observes the reproduced settings and any preloaded
        // code (finding on #688).
        {
            let stale = self.scratch.take().ok_or(Error::Cancelled)?;
            self.inner.control.retire(stale);
            let reopened = {
                let analysis = self.inner.live()?;
                let control_pair = &self
                    .inner
                    .control
                    .session
                    .as_ref()
                    .ok_or(Error::Cancelled)?
                    .pair;
                Session::open(
                    self.inner.control.channel(),
                    &analysis.runtime,
                    StreamLogin {
                        user: self.names.login().to_owned(),
                        password: self.names.password().to_owned(),
                        database: self.names.database().to_owned(),
                    },
                    &[control_pair],
                )
                .await
                .map_err(|failure| {
                    let cause = self.inner.control.retain_failure(failure);
                    self.inner.refuse(cause.clone());
                    cause
                })?
            };
            if let Some(analysis) = self.inner.analysis.as_mut() {
                analysis.opened += 1;
            }
            self.scratch = Some(reopened);
        }
        let target_authorization = target_auth.digest();
        // The expected visibility is what the deployer would see on each path
        // *after* the plan's preceding grants — so a planned USAGE grant that
        // intentionally reveals or hides a schema is compared against its
        // intended effect, not the pre-plan reading (finding on #688).
        target_auth.project_visibility(
            &request.planned,
            &request.schemas,
            &request.write_path_extras,
            &mut target_facts.catalog,
        );
        // The libraries to look for on scratch are the target's, not the fresh
        // scratch catalog's, which has no extensions installed yet.
        let required = scope::required_libraries(driver, &target_facts.catalog);
        // Read the scratch side as the reproduced deployer: the reproduced
        // authorization and the catalog over the connection, then the
        // backend's executables, which include any session-preloaded code.
        let (scratch_facts, auth_differences, scratch_connection, scratch_authorization) = {
            let scratch = self.scratch.as_mut().ok_or(Error::Cancelled)?;
            let connection = scratch.connection.id();
            scope::enter(&mut scratch.connection, driver, deployer.as_deref())
                .await
                .map_err(db)?;
            // Over the full scope, extras included: the expected context
            // covers every schema reconstruction created, and a read of the
            // in-scope schemas alone would report the extras absent and
            // refuse a faithful reproduction (finding on #688).
            let differences = scope::settle(
                &mut scratch.connection,
                &map,
                &target_auth,
                &request.planned,
                &scope_schemas,
            )
            .await
            .map_err(db)?;
            let sealed = scope::seal_scratch(&mut scratch.connection, driver, &scope_schemas)
                .await
                .map_err(db)?;
            let catalog = scope::read_catalog(
                &mut scratch.connection,
                driver,
                &request.schemas,
                &request.write_path_extras,
            )
            .await
            .map_err(db)?;
            let executables = crate::resolver::native::executables::executables(
                &scratch.backend,
                &required,
                &scope::library_path(driver, &catalog),
                scope::engine_packages(driver),
            )
            .await
            .map_err(|_| Error::Scope("the scratch backend's executables are unreadable".into()))?;
            (
                EnvironmentFacts {
                    catalog,
                    executables,
                },
                differences,
                connection,
                sealed,
            )
        };
        // Compare, folding the authorization reproduction into the report so a
        // deployer the scratch could not reproduce is a mismatch, not a pass.
        let mut report = scope::compare(driver, &target_facts, &scratch_facts);
        for key in &auth_differences {
            report.facts.insert(
                format!("authorization:{key}"),
                FactStatus::Mismatch {
                    target: "the target deployer's authorization".into(),
                    resolver: "not reproduced on scratch".into(),
                },
            );
        }
        let verdict = report.verdict();
        let authorization = AuthorizationFingerprint {
            rule: RuleVersion::new(target_auth.rule()),
            digest: target_auth.sealed_digest(&request.planned),
        };
        self.scope = Some(QualifiedScope {
            report,
            authorization,
            target: target_facts,
            target_authorization,
            scratch_authorization,
            map,
            schemas: request.schemas.clone(),
            write_path_extras: request.write_path_extras.clone(),
            planned: request.planned.clone(),
            target_connection,
            scratch_connection,
        });
        Ok(verdict)
    }

    /// Re-reads the scratch side as the reproduced deployer and refuses if the
    /// scope no longer holds: an in-place change between checks that alters an
    /// extension, a setting, a collation or the deployer's reproduced
    /// authorization invalidates the run (ADR-0016 case 21). Only a scope that
    /// qualified as `Verified` is guarded; a run that never verified is already
    /// refused. The sealed scratch connection must still be the one in hand, so
    /// a reopened session cannot present the old scope as its own (case 14).
    async fn requalify(&mut self, target: &mut NativeTarget) -> Result<(), Error> {
        let Some(sealed) = self.scope.as_ref() else {
            return Ok(());
        };
        if sealed.report.verdict() != Verdict::Verified {
            return Ok(());
        }
        let driver = self.inner.control.driver;
        let db = |error: pbps_db::DbError| Error::Scope(error.to_string());
        let read =
            |error: crate::resolver::native::EnvironmentError| Error::Scope(error.to_string());
        let map = sealed.map.clone();
        let planned = sealed.planned.clone();
        let schemas = sealed.schemas.clone();
        let extras = sealed.write_path_extras.clone();
        let sealed_connection = sealed.scratch_connection;
        let sealed_target = sealed.target_connection;
        let sealed_facts = sealed.target.clone();
        let sealed_authorization = sealed.authorization.digest.clone();
        let sealed_target_authorization = sealed.target_authorization.clone();
        let sealed_scratch_authorization = sealed.scratch_authorization.clone();
        // Re-read the target: a second target session may have changed an
        // in-scope grant, extension, collation or setting since `qualify`, and
        // comparing scratch against the sealed evidence would miss it (finding
        // on #688). The sealed target connection must still be the one in hand.
        if target
            .connection_id()
            .map_err(|_| Error::Scope("the target binding is unreadable".into()))?
            != sealed_target
        {
            return Err(Error::Scope(
                "the target connection was replaced; the qualified scope cannot be reused".into(),
            ));
        }
        let scope_schemas =
            scope::scope_schemas(driver, &schemas, &extras).map_err(Error::Scope)?;
        let (mut target_facts, target_auth) = target
            .scope_facts(&schemas, &extras, &scope_schemas, &planned)
            .await
            .map_err(read)?;
        let target_authorization = target_auth.digest();
        target_auth.project_visibility(&planned, &schemas, &extras, &mut target_facts.catalog);
        // The fresh read must be the sealed one. Comparing it against scratch
        // alone would pass a target that changed into something scratch is
        // still compatible with — an extension installed after `qualify` is
        // "available on the resolver" and reads as a match — while the run
        // keeps a report and binding work made over a scope that no longer
        // exists (finding on #688; DECISIONS 520).
        let changed = changed_sections(&sealed_facts, &target_facts);
        let authorization_digest = target_auth.sealed_digest(&planned);
        // Both digests: the projected one, which a change outside the plan
        // moves, and the raw one, which a target that ran one of the plan's
        // own grants moves while the projection stays put (finding on #688).
        let authorization_moved = authorization_digest != sealed_authorization
            || target_authorization != sealed_target_authorization;
        if !changed.is_empty() || authorization_moved {
            let mut what = changed;
            if authorization_moved {
                what.push("authorization");
            }
            return Err(Error::Scope(format!(
                "the target scope changed under the run since it was qualified: {}",
                what.join(", ")
            )));
        }
        let deployer = map
            .deployer(&target_auth)
            .map_err(|reason| Error::Scope(reason.into()))?;
        let required = scope::required_libraries(driver, &target_facts.catalog);
        let (scratch_facts, auth_differences) = {
            let scratch = self.scratch.as_mut().ok_or(Error::Cancelled)?;
            if scratch.connection.id() != sealed_connection {
                return Err(Error::Scope(
                    "the scratch session was replaced; the qualified scope cannot be reused".into(),
                ));
            }
            scope::enter(&mut scratch.connection, driver, deployer.as_deref())
                .await
                .map_err(db)?;
            let differences = scope::recheck(
                &mut scratch.connection,
                &map,
                &target_auth,
                &planned,
                &scope_schemas,
                &sealed_scratch_authorization,
            )
            .await
            .map_err(db)?;
            let catalog = scope::read_catalog(&mut scratch.connection, driver, &schemas, &extras)
                .await
                .map_err(db)?;
            let executables = crate::resolver::native::executables::executables(
                &scratch.backend,
                &required,
                &scope::library_path(driver, &catalog),
                scope::engine_packages(driver),
            )
            .await
            .map_err(|_| Error::Scope("the scratch backend's executables are unreadable".into()))?;
            (
                EnvironmentFacts {
                    catalog,
                    executables,
                },
                differences,
            )
        };
        let report = scope::compare(driver, &target_facts, &scratch_facts);
        if report.verdict() != Verdict::Verified || !auth_differences.is_empty() {
            return Err(Error::Scope(
                "the analysis scope changed under the run and no longer qualifies".into(),
            ));
        }
        Ok(())
    }

    /// Opens an administrative session on the run's own scratch database.
    /// Database-local DDL and privileged catalog reads must run *in* that
    /// database, and the run login cannot make them, being unprivileged. The
    /// session moves the engine's cumulative counter, which stays moved after
    /// it closes, so the run accounts for it. It is held in the run's control
    /// state; the caller uses it there and retires it with
    /// [`Self::retire_admin`].
    async fn admin_session(&mut self) -> Result<(), Error> {
        self.refuse_held_admin()?;
        let admin_login = self.inner.control.endpoint.login(self.names.database());
        let session = {
            let analysis = self.inner.live()?;
            let control_pair = &self
                .inner
                .control
                .session
                .as_ref()
                .ok_or(Error::Cancelled)?
                .pair;
            let scratch_pair = &self.scratch.as_ref().ok_or(Error::Cancelled)?.pair;
            Session::open(
                self.inner.control.channel(),
                &analysis.runtime,
                admin_login,
                &[control_pair, scratch_pair],
            )
            .await
            .map_err(|failure| {
                let cause = self.inner.control.retain_failure(failure);
                self.inner.refuse(cause.clone());
                cause
            })?
        };
        if let Some(analysis) = self.inner.analysis.as_mut() {
            analysis.opened += 1;
        }
        let _admin = self.inner.control.admin.insert(session);
        #[cfg(test)]
        live_tests::admission_recovery::after_admin_open(&_admin.forwarder);
        #[cfg(test)]
        live_tests::admission_recovery::hold().await;
        Ok(())
    }

    /// An administrative session still held means a step was cancelled
    /// while it used it, and the caller retried without a check in between.
    /// Retire it where cleanup finds it and end the analysis, before any
    /// other guard can answer the retry and leave it held.
    fn refuse_held_admin(&mut self) -> Result<(), Error> {
        if let Some(admin) = self.inner.control.admin.take() {
            self.inner.control.retire(admin);
            self.inner.refuse(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        Ok(())
    }

    /// Ends the administrative session without confirming its forwarder's
    /// removal yet; cleanup confirms it or names it.
    fn retire_admin(&mut self) {
        if let Some(admin) = self.inner.control.admin.take() {
            self.inner.control.retire(admin);
        }
    }

    /// Compiles the desired declarations on this run's scratch database as
    /// the reproduced deployer, captures what they bound and what the target
    /// binds under one scope derived from scratch's bindings, and compares
    /// the two per surface (ADR-0016 decision 2; #613). Only a scope that
    /// qualified as `Verified` may be resolved on, every step is bracketed by
    /// a full check, and any failure ends the analysis: a namespace that
    /// changed under the run, or a compilation that stopped part-way, cannot
    /// supply a verdict. A run resolves once; another question needs a fresh
    /// run and a fresh scratch database.
    ///
    /// Only managed declarations are transferred. A retained external object
    /// the declarations could bind is not reconstructed (#617); the surfaces
    /// it could reach come back unresolved.
    pub async fn resolve(
        &mut self,
        target: &mut NativeTarget,
        request: &BindingRequest<'_>,
    ) -> Result<pbps_db::resolver::capture::Assessment, Error> {
        // A held session, or a refusal already recorded, ends the run before
        // the scope and compiled guards below can answer in its place.
        self.refuse_held_admin()?;
        self.inner.live()?;
        let extras = match self.scope.as_ref() {
            Some(scope) if scope.report.verdict() == Verdict::Verified => {
                scope.write_path_extras.clone()
            }
            _ => {
                return Err(Error::Binding(
                    "the analysis scope has not been qualified as verified".into(),
                ));
            }
        };
        if self.compiled {
            return Err(Error::Binding(
                "this run has already compiled a namespace; resolve on a fresh run".into(),
            ));
        }
        let mut reconstruction =
            engine::reconstruction(self.inner.control.driver, &extras, request.bootstrap)
                .map_err(Error::Binding)?;
        // Requalifies the scope and enters the reproduced deployer.
        self.check(target).await?;
        self.compiled = true;
        let outcome = self
            .resolve_checked(target, request, &extras, &mut reconstruction)
            .await;
        if let Err(cause) = &outcome {
            self.inner.refuse(cause.clone());
            if let Some(scratch) = self.scratch.take() {
                self.inner.control.retire(scratch);
            }
        }
        outcome
    }

    async fn resolve_checked(
        &mut self,
        target: &mut NativeTarget,
        request: &BindingRequest<'_>,
        extras: &[String],
        reconstruction: &mut engine::Reconstruction,
    ) -> Result<pbps_db::resolver::capture::Assessment, Error> {
        // In flight across the compilation: dropped part-way, the scratch
        // session is mid-transaction, and the next check must end the run.
        let scratch = self.scratch.as_mut().ok_or(Error::Cancelled)?;
        self.in_flight = true;
        let compiled = engine::compile(reconstruction, extras, &mut scratch.connection).await;
        self.in_flight = false;
        compiled.map_err(Error::Binding)?;
        self.check(target).await?;
        let base = engine::Managed::from_schema(request.base);
        let desired = engine::Managed::from_schema(request.desired);
        // Scratch is read through an administrative session: the capture
        // reads settings a least-privilege deployer need not see, and which
        // role reads a catalog row does not change what was bound.
        // The deployer's effective path per schema, as `qualify` sealed it:
        // a configured extra it cannot use holds no candidate.
        let paths = {
            let sealed = self.scope.as_ref().ok_or(Error::Cancelled)?;
            engine::paths(extras, &sealed.target.catalog.visibility)
        };
        self.admin_session().await?;
        let admin = self.inner.control.admin.as_mut().ok_or(Error::Cancelled)?;
        let captured =
            engine::capture_desired(&mut admin.connection, &base, &desired, &paths).await;
        self.retire_admin();
        let (compiled, scope) = captured.map_err(Error::Binding)?;
        self.check(target).await?;
        let current = target
            .capture_postgres(&scope)
            .await
            .map_err(|error| Error::Binding(error.to_string()))?;
        self.check(target).await?;
        Ok(engine::assess(
            current.catalog(),
            &compiled,
            &base,
            &paths,
            reconstruction,
        ))
    }

    /// Re-qualifies the supplied server, the channels, exclusivity and the
    /// target binding. Declarations are compiled only by `resolve`, between
    /// two of these; no SQL surface is exposed.
    ///
    /// A failure is terminal for the analysis but not for cleanup: the run
    /// keeps answering with the same cause, and `close` still removes what it
    /// created. A cancelled check is the same, found by the flag it left set.
    pub async fn check(&mut self, target: &mut NativeTarget) -> Result<(), Error> {
        if self.in_flight {
            self.in_flight = false;
            if let Some(scratch) = self.scratch.take() {
                self.inner.control.retire(scratch);
            }
            self.inner.refuse(Error::Cancelled);
        }
        if self.scratch.is_none() {
            return Err(self.inner.refusal.clone().unwrap_or(Error::Cancelled));
        }
        self.in_flight = true;
        let outcome = {
            let scratch = self.scratch.as_ref().expect("scratch is present");
            self.inner.check(Some(scratch)).await
        };
        if let Err(cause) = outcome {
            self.in_flight = false;
            if let Some(scratch) = self.scratch.take() {
                self.inner.control.retire(scratch);
            }
            return Err(cause);
        }
        // The runtime, channel, exclusivity and target-binding held; the
        // qualified scope must still hold too, against a *freshly re-read*
        // target, or a change on either side has invalidated it. Still in
        // flight: a check dropped between the target read's `BEGIN` and its
        // `COMMIT` leaves that transaction open on the planning connection,
        // and the next check must find the flag and end the run rather than
        // read inside the stale snapshot (finding on #688).
        if let Err(cause) = self.requalify(target).await {
            self.in_flight = false;
            self.inner.refuse(cause.clone());
            if let Some(scratch) = self.scratch.take() {
                self.inner.control.retire(scratch);
            }
            return Err(cause);
        }
        self.in_flight = false;
        Ok(())
    }

    /// Removes exactly this run's two objects, on every exit path. A failure
    /// reports their names; nothing pre-existing is touched either way.
    /// Takes `&mut self` rather than consuming the run: cancelling a removal
    /// part-way leaves the caller holding a run whose names are still there
    /// to retry with. A completed removal empties it, so calling again is a
    /// no-op.
    pub async fn close(&mut self) -> Result<(), ServerFailure> {
        if self.removed {
            return Ok(());
        }
        // Retire the scratch session into run-owned state rather than
        // closing it inline: its forwarder's unconfirmed name would otherwise
        // live only in this call and be lost if `close` is retried (finding
        // on #640). `close_control` drains it below with the control
        // forwarder, so a later `close` still reports what is not yet gone.
        if let Some(scratch) = self.scratch.take() {
            self.inner.control.retire(scratch);
        }
        let cause = self.inner.refusal.clone().unwrap_or(Error::Cleanup);
        let removal = cleanup(&mut self.inner.control, &self.names, cause).await;
        // The control session and every forwarder this run opened are its own
        // too, and a caller that keeps the run after closing it must not leave
        // them on the server: the next admission would find an old connection
        // as a session it did not open and refuse a valid run (finding on
        // #640). Success is reported only once their removal is confirmed.
        let forwarders = close_control(&mut self.inner.control).await;
        let removal = report(removal, forwarders);
        if removal.recovery_names.is_empty() {
            self.removed = true;
            return Ok(());
        }
        Err(removal)
    }
}

/// Ends the control session, confirming the forwarder's removal, and names
/// the forwarder container if that could not be confirmed. Idempotent: a
/// control without a session has nothing left to close.
async fn close_control(control: &mut Control) -> Vec<String> {
    if let Some(session) = control.session.take() {
        control.retire(session);
    }
    if let Some(admin) = control.admin.take() {
        control.retire(admin);
    }
    let mut unconfirmed = std::mem::take(&mut control.unconfirmed);
    for forwarder in std::mem::take(&mut control.stale) {
        let name = forwarder.resource_name().to_owned();
        if forwarder.close().await.is_err() {
            unconfirmed.push(name);
        }
    }
    // Write the still-unconfirmed names back into run-owned state before
    // returning them: a caller that retries `close` or `discard` must see
    // them again rather than a drained, apparently-clean run (finding on
    // #640). A forwarder whose container is gone leaves nothing here.
    control.unconfirmed = unconfirmed.clone();
    unconfirmed
}

/// Cleanup runs on success, failure and cancellation alike. It uses the
/// control session if that session is still one that can be reasoned about,
/// and opens a fresh one otherwise: a session whose last await was cancelled
/// has a protocol stream in no known state, and a removal sent down it could
/// be answered by the previous exchange. It never widens its scope: only the
/// two generated names are removed, and unremoved ones are reported.
async fn cleanup(control: &mut Control, names: &ScratchNames, cause: Error) -> ServerFailure {
    let removed = remove(control, names).await.is_ok();
    if removed {
        // Confirmed gone. An unconfirmed removal keeps the names: they are
        // the only record of what is left.
        control.pending = None;
    }
    // A janitor forwarder `remove` opened but could not confirm gone is a
    // run-owned container too: dropping the database and login through it does
    // not make it cleanup-complete. It is reported here so `create`'s failure
    // exits name it, and *left* in run-owned state — cloned, not drained — so
    // a retried `close` or `discard` names it again instead of finding an
    // apparently clean run (finding on #640). `close_control` is what writes
    // the set back after each attempt to close what it holds.
    // Both a forwarder that could not be confirmed gone and a run-local role
    // that could not be dropped are run-owned things a human must remove.
    let mut recovery = control.unconfirmed.clone();
    recovery.extend(control.roles.clone());
    report(removal_outcome(removed, cause, names), recovery)
}

async fn remove(control: &mut Control, names: &ScratchNames) -> Result<(), ()> {
    if control.in_flight {
        if let Some(session) = control.session.take() {
            control.retire(session);
        }
        control.in_flight = false;
    }
    // The run-local roles are dropped after the scratch database that they
    // own. They stay in `control.roles` until confirmed gone, so a cleanup
    // that fails before dropping them still reports them as recovery names
    // (finding-family #652); a successful drop leaves only what it could not
    // remove.
    let roles = control.roles.clone();
    if let Some(session) = control.session.as_mut() {
        control.in_flight = true;
        let dropped = engine::drop_scratch(&mut session.connection, names).await;
        let role_failures = if dropped.is_ok() {
            Some(engine::drop_roles(&mut session.connection, &roles).await)
        } else {
            None
        };
        control.in_flight = false;
        if let Some(failures) = role_failures {
            control.roles = failures;
            return Ok(());
        }
        // A session an administrator terminated is not the end of cleanup:
        // the names are still known, and a fresh session can still act.
        if let Some(session) = control.session.take() {
            control.retire(session);
        }
    }
    // A fresh session for the removal alone, on a fresh daemon connection.
    // The run's own `control.api` cannot be reused: a check cancelled in
    // flight is cancelled inside a `control.api` request, and `LocalApi`
    // invalidates permanently on cancellation, so cleanup after a cancelled
    // run would inspect the container over a dead connection (finding on
    // #640). This reconnects, the way the Docker profile's janitor does.
    //
    // It must reach the same engine the run created on: the pinned container,
    // reporting the pinned identity. A restarted container has nothing of
    // this run's on it, but that cannot be confirmed from here, so it is
    // reported rather than assumed.
    let mut api = LocalApi::connect_native(&control.endpoint.daemon)
        .await
        .map_err(|_| ())?;
    let state = api
        .inspect_container(&control.pinned.id)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if pin(&state).map_err(|_| ())? != control.pinned {
        return Err(());
    }
    let (forwarder, mut connection) = Forwarder::open(
        &api,
        &control.image,
        control.driver,
        &control.pinned.id,
        control.endpoint.login(control.profile.maintenance_database),
        control.profile.lifetime.as_secs(),
    )
    .await
    .map_err(|failure| {
        // The janitor forwarder's own name is the only record of an orphan
        // its startup could not confirm gone; keep it in run-owned state.
        control.unconfirmed.extend(failure.recovery_names);
    })?;
    let outcome = async {
        let identity = engine::identity(&mut connection).await.map_err(|_| ())?;
        if identity.instance_key != control.identity.instance_key {
            return Err(());
        }
        engine::drop_scratch(&mut connection, names)
            .await
            .map_err(|_| ())?;
        Ok(engine::drop_roles(&mut connection, &roles).await)
    }
    .await;
    let outcome = match outcome {
        Ok(role_failures) => {
            control.roles = role_failures;
            Ok(())
        }
        Err(()) => Err(()),
    };
    drop(connection);
    let name = forwarder.resource_name().to_owned();
    if forwarder.close().await.is_err() {
        control.unconfirmed.push(name);
    }
    outcome
}

/// The sections of the target's facts that differ between the sealed read
/// and a fresh one, by name, for the refusal that says what moved.
fn changed_sections(sealed: &EnvironmentFacts, fresh: &EnvironmentFacts) -> Vec<&'static str> {
    // A live fixture failure must preserve the exact compared evidence before
    // cleanup destroys it. A section name alone cannot distinguish changed
    // content, late loading and a change in read provenance.
    #[cfg(test)]
    if sealed.executables != fresh.executables {
        for (side, facts, other) in [
            ("sealed", &sealed.executables, &fresh.executables),
            ("fresh", &fresh.executables, &sealed.executables),
        ] {
            if facts.engine != other.engine {
                eprintln!("executable drift ({side} engine): {:?}", facts.engine);
            }
            for library in &facts.libraries {
                if !other.libraries.contains(library) {
                    eprintln!("executable drift ({side} library): {library:?}");
                }
            }
        }
        if sealed.executables.engine == fresh.executables.engine
            && sealed
                .executables
                .libraries
                .iter()
                .all(|library| fresh.executables.libraries.contains(library))
            && fresh
                .executables
                .libraries
                .iter()
                .all(|library| sealed.executables.libraries.contains(library))
        {
            eprintln!(
                "executable drift (library order or multiplicity): sealed={:?}; fresh={:?}",
                sealed.executables.libraries, fresh.executables.libraries
            );
        }
    }
    let mut changed = Vec::new();
    let (s, f) = (&sealed.catalog, &fresh.catalog);
    for (name, differs) in [
        ("observations", s.observations != f.observations),
        ("extensions", s.extensions != f.extensions),
        (
            "available extensions",
            s.available_extensions != f.available_extensions,
        ),
        ("collations", s.collations != f.collations),
        ("settings", s.settings != f.settings),
        ("visibility", s.visibility != f.visibility),
        ("executables", sealed.executables != fresh.executables),
    ] {
        if differs {
            changed.push(name);
        }
    }
    changed
}
/// Why the run ended and whether its resources went away are two different
/// facts. Reporting "could not create scratch resources" for a channel that
/// failed qualification would hide which of them happened.
/// Adds the forwarder names run-owned state still holds to what one exit
/// found. The reason the run ended is kept: `Cleanup` is the cause only when
/// the scratch database and login themselves could not be removed
/// (`removal_outcome`), and a forwarder that could not be confirmed gone is
/// named without replacing a refusal such as `Exclusivity` the caller must
/// still see (finding on #640). Deduplicated because `cleanup` reports the
/// retained names and `close_control` reports them again.
fn report(mut outcome: ServerFailure, names: Vec<String>) -> ServerFailure {
    outcome.recovery_names.extend(names);
    outcome.recovery_names.sort_unstable();
    outcome.recovery_names.dedup();
    outcome
}

fn removal_outcome(removed: bool, cause: Error, names: &ScratchNames) -> ServerFailure {
    if removed {
        ServerFailure {
            cause,
            recovery_names: Vec::new(),
        }
    } else {
        ServerFailure {
            cause: Error::Cleanup,
            recovery_names: vec![names.database().to_owned(), names.login().to_owned()],
        }
    }
}

/// What a failed engine read says about exclusivity.
///
/// Every failure is "the engine did not answer": unreadable is not idle. A
/// refusal the adapter raised is the same — both engines raise it for a
/// login without the privilege to see other sessions, which is a credential
/// problem and not an intrusion, and reporting it as a moved counter would
/// send an operator looking for a session that never existed (finding on
/// #640).
fn signal(error: &pbps_db::DbError) -> Signal {
    // Named rather than a wildcard: a variant added later must be read here
    // before it can be filed as "the engine could not answer".
    match error {
        pbps_db::DbError::Refused(_)
        | pbps_db::DbError::BadConnectionString(_)
        | pbps_db::DbError::Connect { .. }
        | pbps_db::DbError::ConnectTimeout { .. }
        | pbps_db::DbError::Driver { .. }
        | pbps_db::DbError::Context { .. }
        | pbps_db::DbError::WrongSession { .. }
        | pbps_db::DbError::BadRow(_) => Signal::Unreadable,
    }
}

fn correlate(
    process: &pbps_db::resolver::BackendProcess,
    backend_pid: u32,
    engine: &ProcessLease,
) -> Result<(), Error> {
    use pbps_db::resolver::BackendProcess;
    match process {
        BackendProcess::NativePid(pid) if pid.get() != backend_pid => Err(Error::Identity(
            "the engine's own backend is not the process holding this run's session".into(),
        )),
        BackendProcess::NativePid(_) | BackendProcess::RuntimeOnly => engine
            .check()
            .map_err(|_| Error::Unqualified("the engine service process changed under this run")),
    }
}

fn generated_names() -> Result<ScratchNames, Error> {
    let suffix = || format!("{:032x}", rand::random::<u128>());
    ScratchNames::new(
        format!("pbps_scratch_{}", suffix()),
        format!("pbps_run_{}", suffix()),
        format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        ),
    )
    .map_err(|_| Error::Scratch)
}

/// A refusal names the profile that was asked for, bounded and stripped of
/// anything that is not an ordinary configured name.
fn named(value: &str) -> String {
    let name: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "_.-".contains(*c))
        .take(64)
        .collect();
    if name.is_empty() {
        "<unnamed>".to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "server/live_tests.rs"]
mod live_tests;
