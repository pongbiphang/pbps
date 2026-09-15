//! Ownership survives cancellation of the requesting future. The worker has
//! one job: launch one candidate, supervise it, and remove only that resource.
//! It never reconnects a run. A separate connection may only perform cleanup.

use super::profile::{LIFETIME_SECS, Launch, OWNER_LABEL};
use super::{API, CandidateImage, Error, LocalApi, path_component};
use bytes::Bytes;
use hyper::{Method, StatusCode};
#[cfg(test)]
use pbps_db::Driver;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant};

#[cfg(test)]
mod tests;

/// No source, server error, image configuration or credential enters this
/// diagnostic. A recovery name is generated locally, never taken from Docker.
#[derive(Debug, thiserror::Error)]
#[error("{cause}")]
pub struct StartFailure {
    pub cause: Error,
    /// Exact locally generated names whose cleanup could not be confirmed.
    /// Do not use a label selector or delete similarly named objects.
    pub recovery_names: Vec<String>,
}

struct Owner {
    name: String,
    token: String,
    image: String,
    creation: Creation,
}

enum Creation {
    NotRequested,
    Uncertain,
    Created(String),
    Refused,
}

impl Creation {
    fn id(&self) -> Option<&str> {
        match self {
            Self::Created(id) => Some(id),
            Self::NotRequested | Self::Uncertain | Self::Refused => None,
        }
    }
}

struct Running {
    id: String,
    pid: u64,
    started: String,
}

enum Command {
    Check(oneshot::Sender<Result<(), Error>>),
    Close(oneshot::Sender<Result<(), Error>>),
}

/// A supervised candidate, explicitly **not** an admitted resolver.
///
/// The handle accepts no declarations and yields no database connection or
/// binding evidence. Dropping it requests cleanup; `close` additionally waits
/// for confirmation. The fixed engine bootstrap has an independent deadline
/// in its PID namespace for loss of the client process/runtime itself.
pub struct CandidateRun {
    name: String,
    id: String,
    pid: u64,
    commands: mpsc::Sender<Command>,
    cleaned: oneshot::Receiver<Result<(), Error>>,
}

impl CandidateRun {
    #[cfg(test)]
    pub async fn start(
        api: LocalApi,
        image: CandidateImage,
        driver: Driver,
    ) -> Result<Self, StartFailure> {
        let token = format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        );
        Self::start_owned(api, image, driver, token).await
    }

    #[cfg(test)]
    async fn start_owned(
        api: LocalApi,
        image: CandidateImage,
        driver: Driver,
        token: String,
    ) -> Result<Self, StartFailure> {
        let launch = Launch::new(&image, driver, &token).map_err(|cause| StartFailure {
            cause,
            recovery_names: Vec::new(),
        })?;
        Self::start_launch(api, image, token, launch).await
    }

    pub(super) async fn start_launch(
        api: LocalApi,
        image: CandidateImage,
        token: String,
        launch: Launch,
    ) -> Result<Self, StartFailure> {
        let owner = Owner {
            name: format!("pbps-resolver-{token}"),
            token,
            image: image.identity.image_id.clone(),
            creation: Creation::NotRequested,
        };
        let (commands, receiver) = mpsc::channel(1);
        let (ready, started) = oneshot::channel();
        let (cleanup_report, cleaned) = oneshot::channel();
        let name = owner.name.clone();
        tokio::spawn(supervise(
            api,
            owner,
            launch,
            receiver,
            ready,
            cleanup_report,
        ));
        let running = started.await.map_err(|_| StartFailure {
            cause: Error::ControlLost,
            recovery_names: vec![name.clone()],
        })??;
        Ok(Self {
            name,
            id: running.id,
            pid: running.pid,
            commands,
            cleaned,
        })
    }

    pub fn resource_name(&self) -> &str {
        &self.name
    }

    pub fn container_id(&self) -> &str {
        &self.id
    }

    pub(super) fn native_pid(&self) -> Result<u32, Error> {
        self.pid.try_into().map_err(|_| Error::RuntimeChanged)
    }

    /// Checks only candidate continuity; it grants no admission capability.
    pub async fn check(&self) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Check(reply))
            .await
            .map_err(|_| Error::ControlLost)?;
        result.await.map_err(|_| Error::ControlLost)?
    }

    pub async fn close(self) -> Result<(), Error> {
        let (reply, _result) = oneshot::channel();
        // Natural exit or an earlier continuity failure can finish cleanup
        // before close is called. Retain its actual result independently of
        // the command receiver, so success is not mistaken for an orphan.
        let _ = self.commands.send(Command::Close(reply)).await;
        self.cleaned.await.map_err(|_| Error::Cleanup)?
    }
}

async fn inspect(api: &mut LocalApi, resource: &str) -> Result<Option<Value>, Error> {
    let (status, body) = api
        .request(
            Method::GET,
            &format!("{API}/containers/{}/json", path_component(resource)),
        )
        .await?;
    match status {
        StatusCode::OK => serde_json::from_slice(&body)
            .map(Some)
            .map_err(|_| Error::Response),
        StatusCode::NOT_FOUND => Ok(None),
        _ => Err(Error::Response),
    }
}

fn owned_id<'a>(owner: &Owner, state: &'a Value) -> Result<&'a str, Error> {
    let id = state["Id"].as_str().ok_or(Error::RuntimeChanged)?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || owner.creation.id().is_some_and(|expected| expected != id)
        || state["Name"].as_str() != Some(&format!("/{}", owner.name))
        || state["Config"]["Labels"][OWNER_LABEL].as_str() != Some(&owner.token)
        || state["Image"].as_str() != Some(&owner.image)
    {
        return Err(Error::RuntimeChanged);
    }
    Ok(id)
}

fn running(owner: &Owner, state: &Value) -> Result<Running, Error> {
    let id = owned_id(owner, state)?.to_owned();
    let pid = state["State"]["Pid"]
        .as_u64()
        .filter(|pid| *pid > 0)
        .ok_or(Error::RuntimeChanged)?;
    let started = state["State"]["StartedAt"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(Error::RuntimeChanged)?
        .to_owned();
    if state["State"]["Running"] != true
        || state["State"]["Restarting"] != false
        || state["State"]["Paused"] != false
        || state["RestartCount"] != 0
    {
        return Err(Error::RuntimeChanged);
    }
    Ok(Running { id, pid, started })
}

async fn check(
    api: &mut LocalApi,
    owner: &Owner,
    pinned: &Running,
    launch: &Launch,
) -> Result<(), Error> {
    let state = inspect(api, &pinned.id)
        .await?
        .ok_or(Error::RuntimeChanged)?;
    let current = running(owner, &state)?;
    launch.check_configuration(&state)?;
    if current.pid != pinned.pid || current.started != pinned.started {
        return Err(Error::RuntimeChanged);
    }
    Ok(())
}

async fn create_start(
    api: &mut LocalApi,
    owner: &mut Owner,
    launch: &Launch,
    ready: &oneshot::Sender<Result<Running, StartFailure>>,
) -> Result<Running, Error> {
    let body = serde_json::to_vec(&launch.body).map_err(|_| Error::Create)?;
    owner.creation = Creation::Uncertain;
    let (status, body) = api
        .request_body(
            Method::POST,
            &format!("{API}/containers/create?name={}", owner.name),
            Bytes::from(body),
        )
        .await?;
    if status != StatusCode::CREATED {
        // A conflict never grants ownership of the pre-existing name.
        if matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::CONFLICT
        ) {
            owner.creation = Creation::Refused;
        }
        return Err(Error::Create);
    }
    let reply: Value = serde_json::from_slice(&body).map_err(|_| Error::Create)?;
    let id = reply["Id"]
        .as_str()
        .filter(|id| {
            id.len() == 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or(Error::Create)?;
    owner.creation = Creation::Created(id.to_owned());
    let state = inspect(api, id).await?.ok_or(Error::RuntimeChanged)?;
    owned_id(owner, &state)?;
    launch.check_configuration(&state)?;
    if ready.is_closed() {
        return Err(Error::ControlLost);
    }
    let (status, _) = api
        .request(Method::POST, &format!("{API}/containers/{id}/start"))
        .await?;
    if status != StatusCode::NO_CONTENT {
        return Err(Error::Start);
    }
    let state = inspect(api, id).await?.ok_or(Error::Start)?;
    running(owner, &state)
}

async fn cleanup_with(api: &mut LocalApi, owner: &Owner) -> Result<(), Error> {
    let resource = owner.creation.id().unwrap_or(&owner.name);
    let Some(state) = inspect(api, resource).await? else {
        // An unacknowledged create can still be running at the daemon. A
        // subsequent 404 does not prove it cannot appear later.
        return if owner.creation.id().is_some() {
            Ok(())
        } else {
            Err(Error::Cleanup)
        };
    };
    let id = owned_id(owner, &state).map_err(|_| Error::Cleanup)?;
    let (_status, _) = api
        .request(
            Method::DELETE,
            &format!("{API}/containers/{id}?force=1&v=1"),
        )
        .await?;
    // AutoRemove can already be deleting an exited run, and a successful
    // delete can precede final removal. The reply code alone is not cleanup
    // evidence. Wait for this exact owned ID to become absent; never retry a
    // delete against a name or accept a changed ownership marker.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match inspect(api, id).await? {
                None => return Ok(()),
                Some(state) => {
                    owned_id(owner, &state).map_err(|_| Error::Cleanup)?;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| Error::Cleanup)?
}

async fn cleanup(api: &mut LocalApi, owner: &Owner) -> Result<(), Error> {
    if matches!(owner.creation, Creation::NotRequested | Creation::Refused) {
        return Ok(());
    }
    if api.sender.is_some() {
        let result = cleanup_with(api, owner).await;
        if api.sender.is_some() {
            return result.map_err(|_| Error::Cleanup);
        }
    }
    // Reconnection is cleanup-only. It cannot restart supervision or transfer
    // declarations, and must reach the same immediate peer. Exact ID plus the
    // random ownership marker protects unrelated resources on that runtime.
    let mut janitor = if api.native_daemon.is_some() {
        api.additional().await
    } else {
        LocalApi::connect_peer(&api.socket_path, api.peer.0).await
    }
    .map_err(|_| Error::Cleanup)?;
    if janitor.peer != api.peer {
        return Err(Error::Cleanup);
    }
    cleanup_with(&mut janitor, owner)
        .await
        .map_err(|_| Error::Cleanup)
}

fn report_cleanup_failure(owner: &Owner) {
    eprintln!(
        "resolver cleanup could not be confirmed; inspect run-owned resource {}",
        owner.name
    );
}

async fn supervise(
    mut api: LocalApi,
    mut owner: Owner,
    launch: Launch,
    mut commands: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<Running, StartFailure>>,
    cleaned: oneshot::Sender<Result<(), Error>>,
) {
    if ready.is_closed() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(LIFETIME_SECS);
    // The operation keeps its ownership context even if the caller cancels
    // while Docker is creating/starting it. HTTP itself has a bounded timeout.
    let start = create_start(&mut api, &mut owner, &launch, &ready).await;
    // Credentials are not surfaced until the full channel admission exists.
    // Keeping them out of CandidateRun also prevents accidental Debug output.
    let pinned = match start {
        Ok(pinned) => pinned,
        Err(cause) => {
            let recovered = cleanup(&mut api, &owner).await.is_ok();
            let failure = StartFailure {
                cause,
                recovery_names: (!recovered)
                    .then(|| owner.name.clone())
                    .into_iter()
                    .collect(),
            };
            if ready.send(Err(failure)).is_err() && !recovered {
                report_cleanup_failure(&owner);
            }
            return;
        }
    };
    let observed = Running {
        id: pinned.id.clone(),
        pid: pinned.pid,
        started: pinned.started.clone(),
    };
    if ready.send(Ok(observed)).is_err() {
        if cleanup(&mut api, &owner).await.is_err() {
            report_cleanup_failure(&owner);
        }
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let close = loop {
        tokio::select! {
            biased;
            message = commands.recv() => match message {
                None => break None,
                Some(Command::Close(reply)) => break Some(reply),
                Some(Command::Check(mut reply)) => {
                    let result = tokio::select! {
                        biased;
                        _ = reply.closed() => break None,
                        result = check(&mut api, &owner, &pinned, &launch) => result,
                    };
                    let failed = result.is_err();
                    if reply.send(result).is_err() || failed { break None; }
                }
            },
            _ = tokio::time::sleep_until(deadline) => break None,
            _ = interval.tick() => if check(&mut api, &owner, &pinned, &launch).await.is_err() { break None; },
        }
    };
    let result = cleanup(&mut api, &owner).await;
    let _ = cleaned.send(result.clone());
    if let Some(reply) = close {
        if let Err(result) = reply.send(result)
            && result.is_err()
        {
            report_cleanup_failure(&owner);
        }
    } else if result.is_err() {
        report_cleanup_failure(&owner);
    }
}
