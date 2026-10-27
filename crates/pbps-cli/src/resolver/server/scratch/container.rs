//! A Docker scratch: a PostgreSQL server pbps starts from the configured
//! image for one run and removes afterwards (#1674). Apart from where it
//! comes from, it is the same resolver as a supplied
//! server (DEC-1528.1): the operator vouches for the image and the runtime,
//! and pbps measures neither. pbps generates the superuser's password, so
//! the run is the run-owned layout (DEC-1672.1), and the ordinary checks of
//! separation, emptiness and compatibility still run.
//!
//! It talks to whatever `docker` the runner has: Docker, rootless Docker, or
//! Podman through its compatible command or socket (`DOCKER_HOST`).

use super::super::{Error, ProduceError, engine};

mod recovery;
#[cfg(test)]
pub(super) use recovery::{Keeper, seen, swept, swept_alone};
mod startup;
use pbps_config::resolver::PullPolicy;
use pbps_db::{Conn, Driver};
use std::time::Duration;
use tokio::process::Command;

/// How long a started server may take to accept a connection. The image's
/// first start runs initdb, then restarts the server.
pub(super) const READY: Duration = Duration::from_secs(180);

/// How long one `docker` command may take before the run gives up on it.
const COMMAND: Duration = Duration::from_secs(120);

/// Runs `run` against a container started from `image`, and removes the
/// container whatever the run's outcome. The target is bound by its
/// cluster identity first, so a target connection that reaches another
/// cluster than the planning read refuses before anything is started.
pub async fn with_container<T>(
    driver: Driver,
    target_connection: &str,
    planned_identity: &str,
    image: &str,
    pull: PullPolicy,
    run: impl AsyncFnOnce(&str) -> Result<T, ProduceError>,
) -> Result<T, ProduceError> {
    bind_target(driver, target_connection, planned_identity).await?;
    with_started(driver, image, pull, run).await
}

/// The same acquisition without a target binding, for preview rehearsal.
pub async fn with_started<T>(
    driver: Driver,
    image: &str,
    pull: PullPolicy,
    run: impl AsyncFnOnce(&str) -> Result<T, ProduceError>,
) -> Result<T, ProduceError> {
    acquire(image, pull).await?;
    // The run's token names its container and its session in the server, so a
    // later run can tell from them that the owner is gone (#2410).
    let token = format!("{:032x}", rand::random::<u128>());
    recovery::sweep(&token).await;
    let container = Container::start(driver, image, &token, &mut Anonymous(token.clone())).await?;
    let (outcome, keeper) = match container.ready(driver, READY).await {
        Ok(()) => match recovery::Keeper::open(driver, &container.connection, &token).await {
            Ok(keeper) => (run(&container.connection).await, Some(keeper)),
            Err(error) => (Err(error), None),
        },
        Err(error) => (Err(error), None),
    };
    let removed = container.remove().await;
    drop(keeper);
    match (outcome, removed) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (outcome, Ok(())) => outcome,
    }
}

/// A started container the caller keeps across several uses and removes
/// itself: one container for a connected plan's resolver and its rehearsal
/// (#2379). Removed by [`Held::remove`], which confirms it, or best effort by
/// the container's own drop guard on any other way out.
pub struct Held {
    container: Container,
    /// The run's session in the container's server, closed only once the
    /// container is gone, as `with_started` keeps it (#2410).
    keeper: recovery::Keeper,
}

impl Held {
    /// The image on the runner, the container started and accepting
    /// connections.
    pub async fn start(
        driver: Driver,
        image: &str,
        pull: PullPolicy,
    ) -> Result<Self, ProduceError> {
        Self::start_owned(driver, image, pull, &mut Anonymous::new()).await
    }

    /// [`Held::start`] for a caller that must know of the container before it
    /// exists and as soon as it does: `owner` names the run, adds its labels,
    /// is told before the create and given the container's id before anything
    /// connects to it (`pbps test`, ADR-0019 section 3).
    pub async fn start_owned(
        driver: Driver,
        image: &str,
        pull: PullPolicy,
        owner: &mut impl Owner,
    ) -> Result<Self, ProduceError> {
        acquire(image, pull).await?;
        // Named, swept and kept as `with_started`'s, so that a run killed
        // while it holds the container is recovered by the next one.
        let token = owner.token();
        recovery::sweep(&token).await;
        let container = Container::start(driver, image, &token, owner).await?;
        let kept = match container.ready(driver, READY).await {
            Ok(()) => recovery::Keeper::open(driver, &container.connection, &token).await,
            Err(error) => Err(error),
        };
        match kept {
            Ok(keeper) => Ok(Self { container, keeper }),
            Err(error) => match container.remove().await {
                Ok(()) => Err(error),
                Err(names) => Err(ProduceError::Cleanup(names)),
            },
        }
    }

    /// The superuser connection to the container's server.
    pub fn connection(&self) -> &str {
        &self.container.connection
    }

    /// Stands the drop guard down for a container whose removal by name is no
    /// longer safe (a name now held by a container that is not ours).
    pub fn disown(mut self) {
        self.container.removed = true;
    }

    /// Removes the container and confirms it gone.
    pub async fn remove(self) -> Result<(), ProduceError> {
        let Self { container, keeper } = self;
        let removed = container.remove().await;
        drop(keeper);
        removed.map_err(ProduceError::Cleanup)
    }
}

/// What a caller that owns the run it starts a container for supplies
/// ([`Held::start_owned`]). The calls happen in this order: `before_create`,
/// the `docker run`, `created`, and only then any connection to the server.
#[allow(
    async_fn_in_trait,
    reason = "the one caller uses it by value in one function; no Send bound is needed"
)]
pub trait Owner {
    /// The run's token: 32 lowercase hex digits, naming the container
    /// `pbps-scratch-<token>` and its keeper session.
    fn token(&self) -> String;
    /// Further `key=value` labels for the container.
    fn labels(&self) -> Vec<String>;
    /// Whether a later run can find the container by its labels from what
    /// `before_create` recorded. Then the drop fallback stands down until
    /// `created` has recorded its id and volumes: a fallback that removed the
    /// container first could leave a volume nothing names.
    fn recoverable(&self) -> bool {
        false
    }
    /// Called before `docker run`; an error stops the start.
    async fn before_create(&mut self) -> Result<(), String>;
    /// Called with the id `docker run` returned, before anything connects; an
    /// error removes the container and stops the start.
    async fn created(&mut self, id: &str) -> Result<(), String>;
}

/// The resolver's own run: a fresh token and nothing else to tell anyone.
pub struct Anonymous(String);

impl Anonymous {
    fn new() -> Self {
        Self(format!("{:032x}", rand::random::<u128>()))
    }
}

impl Owner for Anonymous {
    fn token(&self) -> String {
        self.0.clone()
    }
    fn labels(&self) -> Vec<String> {
        Vec::new()
    }
    async fn before_create(&mut self) -> Result<(), String> {
        Ok(())
    }
    async fn created(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

/// The planning read's cluster, read again on a fresh target connection.
pub async fn bind_target(
    driver: Driver,
    target_connection: &str,
    planned_identity: &str,
) -> Result<(), ProduceError> {
    let mut target = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Unusable(format!(
                "the target could not be connected: {error}"
            )))
        })?;
    let identity = engine::target_identity(driver, &mut target)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Read(format!(
                "the target cluster's identity: {error}"
            )))
        })?;
    if identity != planned_identity {
        return Err(ProduceError::Run(Error::Unusable(
            "the target connection reaches another database cluster than the planning read; \
             the resolver answers for the cluster the plan was read from, so nothing was \
             started"
                .into(),
        )));
    }
    Ok(())
}

/// The image is on the runner, or acquired where the policy allows it.
/// Only the runtime saying the image is not there allows a pull: an
/// inspection that timed out or was denied says nothing of it.
async fn acquire(image: &str, pull: PullPolicy) -> Result<(), ProduceError> {
    match docker(&["image", "inspect", "--format", "{{.Id}}", image]).await {
        Ok(_) => return Ok(()),
        Err(why) if no_such_image(&why) => {}
        Err(why) => {
            return Err(ProduceError::Acquire(format!(
                "whether the image {image} is on this runner could not be read: {why}"
            )));
        }
    }
    match pull {
        PullPolicy::Never => Err(ProduceError::Acquire(format!(
            "the image {image} is not on this runner, and the resolver's `pull` is `never`; \
             load it first, or set `pull: if_missing`"
        ))),
        PullPolicy::IfMissing => docker(&["pull", image])
            .await
            .map(|_| ())
            .map_err(|why| ProduceError::Acquire(format!("pulling {image}: {why}"))),
    }
}

/// A started container, and the superuser connection to its server.
struct Container {
    id: String,
    /// The runtime's id, known once `docker run` returned it; empty before.
    immutable: String,
    connection: String,
    engine: engine::Docker,
    /// Set once `remove` confirmed it gone, so the drop guard stands down.
    removed: bool,
    password: String,
}

impl Container {
    async fn start(
        driver: Driver,
        image: &str,
        token: &str,
        owner: &mut impl Owner,
    ) -> Result<Self, ProduceError> {
        let password = format!("{:032x}", rand::random::<u128>());
        let engine = engine::docker(driver, &password)?;
        // Named by the run before it is asked for, so a `docker run` that
        // created the container and then failed, timed out or was cancelled
        // leaves one this run can still name and remove.
        let mut container = Self {
            id: format!("pbps-scratch-{token}"),
            immutable: String::new(),
            connection: String::new(),
            engine,
            removed: false,
            password: match driver {
                Driver::Mssql => pbps_mssql::resolver::scratch::password(&password),
                Driver::Postgres => password.clone(),
            },
        };
        owner
            .before_create()
            .await
            .map_err(|why| ProduceError::Acquire(format!("before starting {image}: {why}")))?;
        container.removed = owner.recoverable();
        let args = run_args(
            &container.id,
            token,
            &container.engine,
            image,
            &owner.labels(),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let started = match run_to_end(&args).await {
            Ok(started) => {
                started.map_err(|why| ProduceError::Acquire(format!("starting {image}: {why}")))
            }
            Err(why) => {
                let name = container.id.clone();
                return Err(unfinished_start(&name, &why, container.remove().await));
            }
        };
        // The id is the container's identity from here on; a name can be
        // reused, and the owner records the id before anything connects.
        let started = match started {
            Ok(printed) => {
                container.immutable = printed.trim().to_owned();
                owner
                    .created(&container.immutable)
                    .await
                    .map_err(|why| {
                        ProduceError::Acquire(format!("recording {image}'s container: {why}"))
                    })
                    .map(|()| printed)
            }
            Err(error) => Err(error),
        };
        // Its id and volumes are recorded now: the drop fallback may remove it.
        if started.is_ok() {
            container.removed = false;
        }
        let port = match started {
            Ok(_) => docker(&[
                "port",
                &container.id,
                &format!("{}/tcp", container.engine.port),
            ])
            .await
            .map_err(|why| ProduceError::Acquire(format!("its published port: {why}")))
            .and_then(|text| {
                published_port(&text, container.engine.port).map_err(ProduceError::Acquire)
            }),
            Err(error) => Err(error),
        };
        let port = match port {
            Ok(port) => port,
            Err(error) => {
                return match container.remove().await {
                    Ok(()) => Err(error),
                    Err(names) => Err(ProduceError::Cleanup(names)),
                };
            }
        };
        container.connection = container.engine.connection(port, &password)?;
        Ok(container)
    }

    /// Polls until the server answers: a container runs well before its
    /// server listens on TCP.
    /// The deadline bounds each attempt too: a server that accepts TCP and
    /// then stalls in its handshake would otherwise hold the run past it.
    async fn ready(&self, driver: Driver, within: Duration) -> Result<(), ProduceError> {
        if driver == Driver::Mssql {
            return startup::ready(&self.connection, &self.id, &self.password)
                .await
                .map_err(|e| ProduceError::Acquire(e.to_string()));
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let attempt = async {
                let mut conn = Conn::connect(driver, &self.connection)
                    .await
                    .map_err(|error| error.to_string())?;
                conn.query(self.engine.readiness_query)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            };
            let last = match tokio::time::timeout_at(deadline, attempt).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => error,
                Err(_) => "the last attempt did not finish".to_owned(),
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(ProduceError::Acquire(format!(
                    "the scratch container did not accept connections within {}s: {last}",
                    within.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Stops the container at once, then removes it with its anonymous
    /// volumes, and confirms it gone. `kill` first because `rm --force`
    /// stops through the runtime's grace period, ten seconds under Podman;
    /// `--volumes` because the image declares its data directory a volume,
    /// which `rm` alone leaves behind, a whole cluster per run (both
    /// measured on #1674).
    ///
    /// The volumes are read first and confirmed gone one by one: Docker
    /// deletes the container's record before its volumes, and only logs a
    /// volume it failed to delete, so a container gone proves nothing of
    /// them (#1843 review).
    async fn remove(mut self) -> Result<(), Vec<String>> {
        let removed = remove_named(&self.id).await;
        if removed.is_ok() {
            self.removed = true;
        }
        removed
    }
}

/// Whether the runtime says the container is being removed, by this run or
/// by another's sweep.
async fn removing(id: &str) -> bool {
    docker(&["container", "inspect", "--format", "{{.State.Status}}", id])
        .await
        .is_ok_and(|status| status.trim() == "removing")
}

/// [`Container::remove`] for a container named, not owned: a leftover of a run
/// that was killed (`recovery`).
pub async fn remove_named(id: &str) -> Result<(), Vec<String>> {
    let volumes = docker(&[
        "container",
        "inspect",
        "--format",
        "{{range .Mounts}}{{.Name}} {{end}}",
        id,
    ])
    .await
    .map(|names| {
        names
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    let _ = docker(&["kill", id]).await;
    let _ = docker(&["rm", "--force", "--volumes", id]).await;
    let mut left = Vec::new();
    // Only the runtime saying there is no such container is absence. An
    // unreachable daemon or a timeout says nothing, and the container
    // may still run. A container another run's sweep is removing is waited
    // for, with its volumes: two sweeps may find the same dead one.
    let mut waits = 0;
    let gone = loop {
        match docker(&["container", "inspect", "--format", "{{.Id}}", id]).await {
            Err(why) if no_such_container(&why) => break true,
            Ok(_) if waits < 50 && removing(id).await => {
                waits += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => break false,
        }
    };
    if !gone {
        left.push(format!("the docker container {id} and its volumes"));
    }
    match volumes {
        Ok(volumes) => {
            for volume in volumes {
                let mut tries = 0;
                loop {
                    match docker(&["volume", "inspect", "--format", "{{.Name}}", &volume]).await {
                        Err(why) if no_such_volume(&why) => {}
                        Ok(_) if gone && tries < 30 => {
                            tries += 1;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                        _ => left.push(format!("the docker volume {volume}")),
                    }
                    break;
                }
            }
        }
        // Never created, or created and never named: nothing to read.
        Err(why) if no_such_container(&why) => {}
        Err(_) if left.is_empty() => left.push(format!(
            "the volumes of the docker container {id}, which could not be read"
        )),
        Err(_) => {}
    }
    if left.is_empty() { Ok(()) } else { Err(left) }
}

/// A run that ends some other way (a panic, a cancelled future) still asks
/// for its container's removal, best effort. The command is started and not
/// waited for: an unresponsive runtime must not hold the executor.
impl Container {
    /// What the best-effort removal names: the runtime's id once `docker run`
    /// returned it, because a name can come to belong to another container
    /// (a cancelled teardown must not delete the one that took it).
    fn removal_target(&self) -> &str {
        if self.immutable.is_empty() {
            &self.id
        } else {
            &self.immutable
        }
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let _ = std::process::Command::new("docker")
            .args(["rm", "--force", "--volumes", self.removal_target()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Docker and Podman both report a missing container this way (measured on
/// #1674: Podman's CLI `Error: no such container <name>`, its compatible
/// socket `... found: no such container`, Docker's `No such container:`).
pub fn no_such_container(error: &str) -> bool {
    error.to_ascii_lowercase().contains("no such container")
}

/// And a missing volume (measured on #1674: `no such volume` on both).
pub fn no_such_volume(error: &str) -> bool {
    error.to_ascii_lowercase().contains("no such volume")
}

/// And a missing image (measured on #1674: Docker's `No such image`,
/// Podman's `image not known`).
fn no_such_image(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("no such image") || error.contains("image not known")
}

/// The label naming the process that starts a scratch container: its pid,
/// for a reader, and a value drawn once per process, since processes in
/// different PID namespaces (containers sharing the host's Docker socket)
/// can have the same pid (#2194 review).
pub(super) fn process_label() -> String {
    static TOKEN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let token = TOKEN.get_or_init(rand::random::<u64>);
    format!("pbps.resolver.process={}-{token:016x}", std::process::id())
}

/// The `docker run` that starts the scratch server.
fn run_args(
    name: &str,
    token: &str,
    engine: &engine::Docker,
    image: &str,
    extra: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--detach",
        // Any pull the policy allows was made by `acquire`; an image that
        // went away since must not be fetched here instead.
        "--pull",
        "never",
        "--name",
        name,
        "--label",
        "pbps.resolver=scratch",
        // Which process started it. Another pbps on the same host labels its
        // scratch containers the same way, so this is what tells a leftover's
        // owner, and what lets the live tests count only their own (#2095).
        "--label",
        &process_label(),
        // The run's token and its engine: what a later run reads to tell
        // whether the owner is gone (`recovery`, #2410).
        "--label",
        &format!("{}={token}", recovery::TOKEN),
        "--label",
        &format!(
            "{}={}",
            recovery::ENGINE,
            match engine.driver {
                Driver::Postgres => "postgres",
                Driver::Mssql => "mssql",
            }
        ),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for label in extra {
        args.push("--label".into());
        args.push(label.clone());
    }
    for variable in &engine.environment {
        args.push("--env".into());
        args.push(variable.clone());
    }
    // An omitted host port: Docker and Podman both pick a free one, on
    // loopback only, so nothing off this host can reach the server.
    args.extend([
        "--publish".into(),
        format!("127.0.0.1::{}", engine.port),
        image.into(),
    ]);
    args
}

/// A `docker run` that did not finish leaves the daemon's create request
/// unknown: it may complete after the removal read "no such container", and
/// the container appear afterwards. So the release is never reported clean;
/// the container is named, to be removed if it appears (#1843 review).
fn unfinished_start(name: &str, why: &str, removal: Result<(), Vec<String>>) -> ProduceError {
    let mut names = removal.err().unwrap_or_default();
    names.push(format!(
        "the docker container {name} and its volumes, if it appears: its start did not \
         finish ({why})"
    ));
    ProduceError::Cleanup(names)
}

/// Runs `docker` with `args`, and returns its standard output, or why it
/// failed.
pub async fn docker(args: &[&str]) -> Result<String, String> {
    run_to_end(args).await.and_then(|finished| finished)
}

/// As [`docker`], telling a command that ran to its end, successfully or
/// not, from one that did not finish or could not be run (the outer `Err`).
async fn run_to_end(args: &[&str]) -> Result<Result<String, String>, String> {
    let output = tokio::time::timeout(
        COMMAND,
        Command::new("docker")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        format!(
            "docker {} did not finish in {}s",
            args[0],
            COMMAND.as_secs()
        )
    })?
    .map_err(|error| format!("docker could not be run: {error}"))?;
    Ok(if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "docker {} failed ({}): {}",
            args[0],
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    })
}

/// Exactly the one loopback mapping asked for. Accepting any line, or any
/// suffix, would hide a second, public bind.
pub(super) fn published_port(text: &str, container_port: u16) -> Result<u16, String> {
    text.trim()
        .strip_prefix("127.0.0.1:")
        .filter(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            format!(
                "docker did not publish exactly one loopback port for {container_port}: {}",
                text.trim()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{
        Container, no_such_container, no_such_image, no_such_volume, published_port, run_args,
        unfinished_start,
    };

    /// A start that did not finish is never a clean release, even when the
    /// removal then found no container: the create may still land.
    #[test]
    fn an_unfinished_start_names_its_container_even_when_none_was_found() {
        let named = |error| {
            let super::ProduceError::Cleanup(names) = error else {
                panic!("not a cleanup failure: {error}");
            };
            names
        };
        let names = named(unfinished_start(
            "pbps-scratch-1",
            "docker run did not finish",
            Ok(()),
        ));
        assert_eq!(names.len(), 1);
        assert!(names[0].contains("pbps-scratch-1") && names[0].contains("if it appears"));
        // A removal that itself failed keeps what it named.
        let names = named(unfinished_start(
            "pbps-scratch-1",
            "docker run did not finish",
            Err(vec!["the docker volume v".into()]),
        ));
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn the_fallback_removal_names_the_id_once_it_is_known() {
        let mut container = Container {
            id: "pbps-scratch-t".into(),
            immutable: String::new(),
            connection: String::new(),
            engine: super::engine::docker(Driver::Postgres, "x").unwrap(),
            removed: true,
            password: String::new(),
        };
        assert_eq!(container.removal_target(), "pbps-scratch-t");
        container.immutable = "f".repeat(64);
        assert_eq!(container.removal_target(), "f".repeat(64));
    }

    #[test]
    fn only_the_runtime_saying_so_confirms_a_volume_gone() {
        for absent in [
            "docker volume failed (exit status: 1): Error response from daemon: no such volume",
            "docker volume failed (exit status: 125): Error: no such volume x",
            "docker volume failed (exit status: 1): Error: No such volume: x",
        ] {
            assert!(no_such_volume(absent), "{absent}");
        }
        // Negative: a refused socket or a timeout leaves it unknown.
        for unknown in [
            "docker volume did not finish in 120s",
            "docker volume failed (exit status: 1): permission denied while trying to connect to the Docker daemon socket",
            "docker volume failed (exit status: 1): Error: no such container x",
        ] {
            assert!(!no_such_volume(unknown), "{unknown}");
        }
    }

    /// The launch never pulls: `pull: never` is enforced where an image can
    /// be fetched, not only at the inspection before it.
    #[test]
    fn the_launch_never_pulls_and_publishes_on_loopback_only() {
        let engine = super::engine::docker(Driver::Postgres, "secret").unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let args = run_args("pbps-scratch-0", token, &engine, "postgres:18", &[]);
        let after = |flag: &str| {
            let at = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{flag}"));
            args[at + 1].as_str()
        };
        assert_eq!(after("--pull"), "never");
        assert_eq!(after("--env"), "POSTGRES_PASSWORD=secret");
        assert_eq!(engine.readiness_query, "SELECT 1 AS ok");
        assert_eq!(
            engine.connection(41417, "secret").unwrap(),
            "host=127.0.0.1 port=41417 user=postgres password=secret dbname=postgres sslmode=disable"
        );
        let mssql =
            super::engine::docker(Driver::Mssql, "0123456789abcdef0123456789abcdef").unwrap();
        let password = pbps_mssql::resolver::scratch::password("0123456789abcdef0123456789abcdef");
        assert_eq!(
            mssql.environment,
            [
                "ACCEPT_EULA=Y".to_owned(),
                format!("MSSQL_SA_PASSWORD={password}")
            ]
        );
        assert_eq!(mssql.readiness_query, "SELECT 1 AS ok");
        assert_eq!(
            mssql
                .connection(41417, "0123456789abcdef0123456789abcdef")
                .unwrap(),
            format!(
                "Server=127.0.0.1,41417;Database=master;User \
                 Id=sa;Password={password};TrustServerCertificate=true"
            )
        );
        let mssql_args = run_args("pbps-scratch-1", token, &mssql, "sql-server-image", &[]);
        assert!(
            mssql_args
                .windows(2)
                .any(|pair| pair == ["--publish", "127.0.0.1::1433"])
        );
        assert!(
            mssql_args
                .windows(2)
                .any(|pair| pair == ["--pull", "never"])
        );
        assert_eq!(after("--publish"), "127.0.0.1::5432");
        assert_eq!(after("--name"), "pbps-scratch-0");
        let labels: Vec<&str> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--label")
            .map(|(i, _)| args[i + 1].as_str())
            .collect();
        let label = super::process_label();
        assert_eq!(
            labels,
            [
                "pbps.resolver=scratch",
                label.as_str(),
                "pbps.resolver.token=0123456789abcdef0123456789abcdef",
                "pbps.resolver.engine=postgres"
            ]
        );
        // One value for the process's lifetime, which the live tests filter
        // on, and more than the pid, which another PID namespace can share.
        assert_eq!(label, super::process_label());
        let pid = format!("pbps.resolver.process={}-", std::process::id());
        assert!(
            label.starts_with(&pid) && label.len() == pid.len() + 16,
            "{label}"
        );
        assert_eq!(args.last().map(String::as_str), Some("postgres:18"));
        // Negative: no other pull policy, and no second publish.
        assert_eq!(args.iter().filter(|a| a.starts_with("--pull")).count(), 1);
        assert_eq!(args.iter().filter(|a| *a == "--publish").count(), 1);
    }

    #[test]
    fn only_the_runtime_saying_so_confirms_an_image_missing() {
        for absent in [
            "docker image failed (exit status: 1): Error response from daemon: failed to find image x: x: No such image",
            "docker image failed (exit status: 125): Error: x: image not known",
            "docker image failed (exit status: 1): Error: No such image: x",
        ] {
            assert!(no_such_image(absent), "{absent}");
        }
        // Negative: a timeout or a refused socket is no license to pull.
        for unknown in [
            "docker image did not finish in 120s",
            "docker image failed (exit status: 1): permission denied while trying to connect to the Docker daemon socket",
            "docker could not be run: No such file or directory (os error 2)",
        ] {
            assert!(!no_such_image(unknown), "{unknown}");
        }
    }
    use pbps_db::Driver;
    use std::time::Duration;

    /// A server that accepts the connection and never answers its
    /// handshake: the readiness deadline still returns, so the run reaches
    /// its cleanup.
    #[tokio::test]
    async fn readiness_gives_up_on_a_server_that_stalls_after_accepting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                open.push(socket);
            }
        });
        let container = Container {
            id: "pbps-scratch-test".into(),
            immutable: String::new(),
            connection: format!(
                "host=127.0.0.1 port={port} user=postgres password=x dbname=postgres sslmode=disable"
            ),
            engine: super::engine::docker(Driver::Postgres, "x").unwrap(),
            // Nothing was started: the drop guard has nothing to remove.
            removed: true,
            password: String::new(),
        };
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            container.ready(Driver::Postgres, Duration::from_secs(2)),
        )
        .await;
        held.abort();
        let refused = outcome.expect("the deadline bounds the stalled attempt");
        assert!(
            matches!(&refused, Err(super::ProduceError::Acquire(reason)) if reason.contains("within 2s")),
            "{refused:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn only_the_runtime_saying_so_confirms_a_container_gone() {
        for absent in [
            "docker container failed (exit status: 1): Error response from daemon: no container with name or ID \"x\" found: no such container",
            "docker container failed (exit status: 125): Error: no such container x",
            "docker container failed (exit status: 1): Error: No such container: x",
        ] {
            assert!(no_such_container(absent), "{absent}");
        }
        // Negative: an unreachable daemon, a refused permission or a timeout
        // says nothing of whether it still runs.
        for unknown in [
            "docker container failed (exit status: 1): Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
            "docker container failed (exit status: 1): permission denied while trying to connect to the Docker daemon socket",
            "docker container did not finish in 120s",
            "docker could not be run: No such file or directory (os error 2)",
        ] {
            assert!(!no_such_container(unknown), "{unknown}");
        }
    }

    #[test]
    fn only_one_loopback_mapping_is_a_published_port() {
        for (text, port) in [
            ("127.0.0.1:1", 1),
            ("127.0.0.1:41417\n", 41417),
            ("127.0.0.1:65535\r\n", 65535),
        ] {
            assert_eq!(published_port(text, 5432), Ok(port));
        }
        // Negative: a public, IPv6, zero or second bind is not.
        for text in [
            "0.0.0.0:49154",
            "invalid IP:45887",
            "[::1]:49154",
            "127.0.0.1:65536",
            "127.0.0.1:+1",
            "127.0.0.1:-1",
            "garbage",
            "127.0.0.1:41417\n127.0.0.1:41418\n",
            "[::]:49154",
            "127.0.0.1:0",
            "127.0.0.1:",
            "127.0.0.1:41417\n0.0.0.0:41417\n",
            "",
        ] {
            assert!(published_port(text, 5432).is_err(), "{text:?}");
        }
    }
}
