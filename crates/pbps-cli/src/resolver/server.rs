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
//! engine build compatibility, deployment context and source handling remain
//! the later #595 steps, and no declaration is transferred here.

use crate::resolver::docker::{CandidateImage, LocalApi, forwarder::Forwarder};
use crate::resolver::native::{
    NativeTarget, ProcessLease, TargetWitness, guard, observe_incidental, process_scope, security,
};
use pbps_db::Driver;
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
            Self::Anchors => "the container's /proc or /sys is not its own instance",
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

/// Cleanup failures name the resources a human still has to remove. They are
/// generated names, never the supplied credentials or any existing object.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{cause}")]
pub struct ServerFailure {
    pub cause: Error,
    pub recovery_names: Vec<String>,
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
}

impl Control {
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
    ) -> Result<Self, Error> {
        let (forwarder, mut connection) = Forwarder::open(
            channel.api,
            channel.image,
            channel.driver,
            &channel.pinned.id,
            login,
            channel.profile.lifetime.as_secs(),
        )
        .await
        .map_err(|failure| {
            Error::Channel(if failure.recovery_names.is_empty() {
                failure.cause.to_string()
            } else {
                format!(
                    "{}; unconfirmed removal of {}",
                    failure.cause,
                    failure.recovery_names.join(", ")
                )
            })
        })?;
        // The forwarder is a run-owned container from here on. Every step
        // below can fail, and dropping the forwarder would only request its
        // removal, not confirm it, leaving a container neither cleanup nor a
        // recovery name can identify (finding on #640). So it stays a local
        // through the fallible steps and is closed on failure; only complete
        // success moves it into the returned session. The residual — naming
        // a forwarder whose close *also* fails through the admission error —
        // is #683.
        let prepared = async {
            let guard = forwarder
                .pid()
                .map_err(|error| Error::Channel(error.to_string()))
                .and_then(|pid| {
                    ProcessLease::capture(pid).map_err(|_| {
                        Error::Channel("the forwarder's init process is unreadable".into())
                    })
                })?;
            let (pair, backend) =
                exclusivity::bind(runtime.init(), &guard, channel.profile.port, known)
                    .map_err(|reason| Error::Channel(reason.to_owned()))?;
            check_kernel_parts(runtime.init(), &guard, &pair, &backend)?;
            let own = engine::own_session(&mut connection)
                .await
                .map_err(|error| Error::Identity(error.to_string()))?;
            // PostgreSQL's backend reports its own pid; the process holding the
            // server end of our session must be that backend. SQL Server has
            // no such mapping and its engine process is the holder.
            correlate(&own.process, backend.namespace_pid(), runtime.engine())?;
            Ok::<_, Error>((guard, pair, backend, own.key))
        }
        .await;
        match prepared {
            Ok((guard, pair, backend, session_key)) => Ok(Self {
                connection,
                forwarder,
                guard,
                pair,
                backend,
                session_key,
            }),
            Err(error) => {
                drop(connection);
                let _ = forwarder.close().await;
                Err(error)
            }
        }
    }

    /// The forwarder is still the fixed program at its fixed privileges, and
    /// the session's two ends are still where they were, held by whom they
    /// were held by.
    fn check_kernel(&self, init: &ProcessLease) -> Result<(), Error> {
        check_kernel_parts(init, &self.guard, &self.pair, &self.backend)
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
    pair: &TcpPair,
    backend: &ProcessLease,
) -> Result<(), Error> {
    let channel = |reason: &str| Error::Channel(reason.to_owned());
    guard(forwarder_guard).map_err(|_| channel("the forwarder's init is not the fixed guard"))?;
    for (pid, directory) in process_scope(forwarder_guard)
        .map_err(|_| channel("the forwarder's processes are unreadable"))?
    {
        if pid == forwarder_guard.pid() {
            continue;
        }
        observe_incidental(pid, &directory, |process| security(&process, 65534, 0))
            .map_err(|_| channel("a forwarder process is not at the fixed privileges"))?;
    }
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

impl DedicatedServer {
    /// No database, DDL or declaration crosses this call: it ends with a
    /// qualified control session on the supplied maintenance database.
    pub async fn admit(
        endpoint: ScratchEndpoint,
        target: &mut NativeTarget,
    ) -> Result<Self, Error> {
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
            },
            analysis: Some(Analysis {
                runtime,
                target: target
                    .witness()
                    .map_err(|_| Error::Unqualified("the target's witness is unreadable"))?,
                deadline: admitted + profile.lifetime,
                accepted: inventory.counter.total,
                opened: 0,
                epoch: inventory.counter.epoch,
                continuity: inventory.counter.continuity.into_iter().collect(),
            }),
            refusal: None,
        };
        inner.check(None).await?;
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
                analysis.runtime.check(&[&session.guard])
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
            Err(cause) => {
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
}

impl ScratchRun {
    pub fn database(&self) -> &str {
        self.names.database()
    }

    /// Re-qualifies the supplied server, the channels, exclusivity and the
    /// target binding. Later delivery steps compile declarations only through
    /// a session this has just re-qualified; no SQL surface is exposed here,
    /// because this step enables no declaration transfer.
    ///
    /// A failure is terminal for the analysis but not for cleanup: the run
    /// keeps answering with the same cause, and `close` still removes what it
    /// created. A cancelled check is the same, found by the flag it left set.
    pub async fn check(&mut self) -> Result<(), Error> {
        if self.in_flight {
            self.in_flight = false;
            if let Some(scratch) = self.scratch.take() {
                self.inner.control.retire(scratch);
            }
            self.inner.refuse(Error::Cancelled);
        }
        let Some(scratch) = self.scratch.as_ref() else {
            return Err(self.inner.refusal.clone().unwrap_or(Error::Cancelled));
        };
        self.in_flight = true;
        let outcome = self.inner.check(Some(scratch)).await;
        self.in_flight = false;
        if outcome.is_err()
            && let Some(scratch) = self.scratch.take()
        {
            self.inner.control.retire(scratch);
        }
        outcome
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
    report(
        removal_outcome(removed, cause, names),
        control.unconfirmed.clone(),
    )
}

async fn remove(control: &mut Control, names: &ScratchNames) -> Result<(), ()> {
    if control.in_flight {
        if let Some(session) = control.session.take() {
            control.retire(session);
        }
        control.in_flight = false;
    }
    if let Some(session) = control.session.as_mut() {
        control.in_flight = true;
        let outcome = engine::drop_scratch(&mut session.connection, names).await;
        control.in_flight = false;
        if outcome.is_ok() {
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
            .map_err(|_| ())
    }
    .await;
    drop(connection);
    let name = forwarder.resource_name().to_owned();
    if forwarder.close().await.is_err() {
        control.unconfirmed.push(name);
    }
    outcome
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
