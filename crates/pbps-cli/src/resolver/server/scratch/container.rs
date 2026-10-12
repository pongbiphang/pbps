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

use super::super::{Error, ProduceError};
use pbps_config::resolver::PullPolicy;
use pbps_db::{Conn, Driver};
use std::time::Duration;
use tokio::process::Command;

/// How long a started server may take to accept a connection. The image's
/// first start runs initdb, then restarts the server.
const READY: Duration = Duration::from_secs(180);

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
    acquire(image, pull).await?;
    let container = Container::start(image).await?;
    let outcome = match container.ready(driver, READY).await {
        Ok(()) => run(&container.connection).await,
        Err(error) => Err(error),
    };
    let removed = container.remove().await;
    match (outcome, removed) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (outcome, Ok(())) => outcome,
    }
}

/// The planning read's cluster, read again on a fresh target connection.
async fn bind_target(
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
    let identity = pbps_pg::resolver::scratch::cluster_identity(&mut target)
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
    connection: String,
    /// Set once `remove` confirmed it gone, so the drop guard stands down.
    removed: bool,
}

impl Container {
    async fn start(image: &str) -> Result<Self, ProduceError> {
        let password = format!("{:032x}", rand::random::<u128>());
        // Named by the run before it is asked for, so a `docker run` that
        // created the container and then failed, timed out or was cancelled
        // leaves one this run can still name and remove.
        let mut container = Self {
            id: format!("pbps-scratch-{:016x}", rand::random::<u64>()),
            connection: String::new(),
            removed: false,
        };
        let args = run_args(&container.id, &password, image);
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
        let port = match started {
            Ok(_) => docker(&["port", &container.id, "5432/tcp"])
                .await
                .map_err(|why| ProduceError::Acquire(format!("its published port: {why}")))
                .and_then(|text| published_port(&text).map_err(ProduceError::Acquire)),
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
        container.connection = format!(
            "host=127.0.0.1 port={port} user=postgres password={password} dbname=postgres \
             sslmode=disable"
        );
        Ok(container)
    }

    /// Polls until the server answers: a container runs well before its
    /// server listens on TCP.
    /// The deadline bounds each attempt too: a server that accepts TCP and
    /// then stalls in its handshake would otherwise hold the run past it.
    async fn ready(&self, driver: Driver, within: Duration) -> Result<(), ProduceError> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let attempt = async {
                let mut conn = Conn::connect(driver, &self.connection)
                    .await
                    .map_err(|error| error.to_string())?;
                conn.query("SELECT 1 AS ok")
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
        let volumes = docker(&[
            "container",
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Name}} {{end}}",
            &self.id,
        ])
        .await
        .map(|names| {
            names
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let _ = docker(&["kill", &self.id]).await;
        let _ = docker(&["rm", "--force", "--volumes", &self.id]).await;
        let mut left = Vec::new();
        // Only the runtime saying there is no such container is absence. An
        // unreachable daemon or a timeout says nothing, and the container
        // may still run.
        match docker(&["container", "inspect", "--format", "{{.Id}}", &self.id]).await {
            Err(why) if no_such_container(&why) => {}
            _ => left.push(format!("the docker container {} and its volumes", self.id)),
        }
        match volumes {
            Ok(volumes) => {
                for volume in volumes {
                    match docker(&["volume", "inspect", "--format", "{{.Name}}", &volume]).await {
                        Err(why) if no_such_volume(&why) => {}
                        _ => left.push(format!("the docker volume {volume}")),
                    }
                }
            }
            // Never created, or created and never named: nothing to read.
            Err(why) if no_such_container(&why) => {}
            Err(_) if left.is_empty() => left.push(format!(
                "the volumes of the docker container {}, which could not be read",
                self.id
            )),
            Err(_) => {}
        }
        if left.is_empty() {
            self.removed = true;
            Ok(())
        } else {
            Err(left)
        }
    }
}

/// A run that ends some other way (a panic, a cancelled future) still asks
/// for its container's removal, best effort. The command is started and not
/// waited for: an unresponsive runtime must not hold the executor.
impl Drop for Container {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let _ = std::process::Command::new("docker")
            .args(["rm", "--force", "--volumes", &self.id])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Docker and Podman both report a missing container this way (measured on
/// #1674: Podman's CLI `Error: no such container <name>`, its compatible
/// socket `... found: no such container`, Docker's `No such container:`).
fn no_such_container(error: &str) -> bool {
    error.to_ascii_lowercase().contains("no such container")
}

/// And a missing volume (measured on #1674: `no such volume` on both).
fn no_such_volume(error: &str) -> bool {
    error.to_ascii_lowercase().contains("no such volume")
}

/// And a missing image (measured on #1674: Docker's `No such image`,
/// Podman's `image not known`).
fn no_such_image(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("no such image") || error.contains("image not known")
}

/// The `docker run` that starts the scratch server.
fn run_args(name: &str, password: &str, image: &str) -> Vec<String> {
    [
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
        "--env",
        &format!("POSTGRES_PASSWORD={password}"),
        // An omitted host port: Docker and Podman both pick a free one, on
        // loopback only, so nothing off this host can reach the server.
        "--publish",
        "127.0.0.1::5432",
        image,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
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
async fn docker(args: &[&str]) -> Result<String, String> {
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
fn published_port(text: &str) -> Result<u16, String> {
    text.trim()
        .strip_prefix("127.0.0.1:")
        .filter(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            format!(
                "docker did not publish exactly one loopback port for 5432: {}",
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
        let args = run_args("pbps-scratch-0", "secret", "postgres:18");
        let after = |flag: &str| {
            let at = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{flag}"));
            args[at + 1].as_str()
        };
        assert_eq!(after("--pull"), "never");
        assert_eq!(after("--publish"), "127.0.0.1::5432");
        assert_eq!(after("--name"), "pbps-scratch-0");
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
            connection: format!(
                "host=127.0.0.1 port={port} user=postgres password=x dbname=postgres sslmode=disable"
            ),
            // Nothing was started: the drop guard has nothing to remove.
            removed: true,
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
        assert_eq!(published_port("127.0.0.1:41417\n"), Ok(41417));
        // Negative: a public, IPv6, zero or second bind is not.
        for text in [
            "0.0.0.0:49154",
            "[::]:49154",
            "127.0.0.1:0",
            "127.0.0.1:",
            "127.0.0.1:41417\n0.0.0.0:41417\n",
            "",
        ] {
            assert!(published_port(text).is_err(), "{text:?}");
        }
    }
}
