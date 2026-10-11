//! A Docker scratch: a PostgreSQL server pbps starts from the configured
//! image for one run and removes afterwards (#1674). Apart from where it
//! comes from, it is the same operator-vouched resolver as a supplied
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
    let outcome = match container.ready(driver).await {
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
            ProduceError::Run(Error::Vouched(format!(
                "the target could not be connected: {error}"
            )))
        })?;
    let identity = pbps_pg::resolver::vouched::cluster_identity(&mut target)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Read(format!(
                "the target cluster's identity: {error}"
            )))
        })?;
    if identity != planned_identity {
        return Err(ProduceError::Run(Error::Vouched(
            "the target connection reaches another database cluster than the planning read; \
             the resolver answers for the cluster the plan was read from, so nothing was \
             started"
                .into(),
        )));
    }
    Ok(())
}

/// The image is on the runner, or acquired where the policy allows it.
async fn acquire(image: &str, pull: PullPolicy) -> Result<(), ProduceError> {
    if docker(&["image", "inspect", "--format", "{{.Id}}", image])
        .await
        .is_ok()
    {
        return Ok(());
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
        // An omitted host port: Docker and Podman both pick a free one, on
        // loopback only, so nothing off this host can reach the server.
        let id = docker(&[
            "run",
            "--detach",
            "--label",
            "pbps.resolver=scratch",
            "--env",
            &format!("POSTGRES_PASSWORD={password}"),
            "--publish",
            "127.0.0.1::5432",
            image,
        ])
        .await
        .map_err(|why| ProduceError::Acquire(format!("starting {image}: {why}")))?;
        let id = id.trim().to_owned();
        if id.is_empty() {
            return Err(ProduceError::Acquire(format!(
                "starting {image}: docker reported no container"
            )));
        }
        // From here on every way out removes it.
        let mut container = Self {
            id,
            connection: String::new(),
            removed: false,
        };
        let port = docker(&["port", &container.id, "5432/tcp"])
            .await
            .map_err(|why| ProduceError::Acquire(format!("its published port: {why}")))
            .and_then(|text| published_port(&text).map_err(ProduceError::Acquire));
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
    async fn ready(&self, driver: Driver) -> Result<(), ProduceError> {
        let started = tokio::time::Instant::now();
        loop {
            let last = match Conn::connect(driver, &self.connection).await {
                Ok(mut conn) => match conn.query("SELECT 1 AS ok").await {
                    Ok(_) => return Ok(()),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            };
            if started.elapsed() > READY {
                return Err(ProduceError::Acquire(format!(
                    "the scratch container did not accept connections within {}s: {last}",
                    READY.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Stops the container at once, then removes it, and confirms it gone.
    /// `kill` first because `rm --force` stops through the runtime's grace
    /// period, ten seconds under Podman (measured on #1674).
    async fn remove(mut self) -> Result<(), Vec<String>> {
        let _ = docker(&["kill", &self.id]).await;
        let _ = docker(&["rm", "--force", &self.id]).await;
        if docker(&["container", "inspect", "--format", "{{.Id}}", &self.id])
            .await
            .is_ok()
        {
            return Err(vec![format!("the docker container {}", self.id)]);
        }
        self.removed = true;
        Ok(())
    }
}

/// A run that ends some other way (a panic, a cancelled future) still
/// removes its container, best effort and without waiting on the runtime.
impl Drop for Container {
    fn drop(&mut self) {
        if self.removed || self.id.is_empty() {
            return;
        }
        let _ = std::process::Command::new("docker")
            .args(["rm", "--force", &self.id])
            .output();
    }
}

/// Runs `docker` with `args`, and returns its standard output, or why it
/// failed.
async fn docker(args: &[&str]) -> Result<String, String> {
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
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "docker {} failed ({}): {}",
            args[0],
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
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
    use super::published_port;

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
