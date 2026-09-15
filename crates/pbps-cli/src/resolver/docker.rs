//! Docker acquisition over a pinned local API connection.
//!
//! A root-owned Unix peer authenticates this immediate API hop only. Neither
//! an API reply nor an acquired image is runtime admission (ADR-0016, 4–5).
//! Engine processes, downstream hops, mounts and execution controls still
//! require independent qualification before any declarations are transferred.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::resolver::native::DaemonLease;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::client::conn::http1::SendRequest;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use pbps_config::resolver::{PullPolicy, ResolverProfile};
use serde::Deserialize;
use tokio::net::UnixStream;

mod channel;
mod lifecycle;
mod profile;
mod reserved;
mod session;

pub use channel::AttachStream;
pub(crate) use lifecycle::CandidateRun;
pub use lifecycle::StartFailure;
pub use reserved::ReservedSession;
pub use session::CandidateSession;

const API: &str = "/v1.47";
const REQUEST_BUDGET: Duration = Duration::from_secs(15);
const MAX_REPLY: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("resolver Docker control requires an absolute local Unix socket path")]
    SocketPath,
    #[error("cannot authenticate the resolver Docker API's local root-owned peer")]
    Peer,
    #[error("resolver Docker control connection was lost; discard the run and acquire a fresh one")]
    ControlLost,
    #[error("resolver Docker API returned an unreadable or unsupported response")]
    Response,
    #[error("resolver Docker acquisition requires an explicitly configured image profile")]
    Profile,
    #[error("the configured resolver image is missing and pull policy forbids acquisition")]
    MissingNoPull,
    #[error(
        "the configured resolver image could not be pulled; check image availability and registry credentials"
    )]
    Pull,
    #[error("the acquired resolver image has no complete immutable content/platform identity")]
    ImageIdentity,
    #[error("the selected image/platform has no implemented resolver launch profile")]
    UnsupportedLaunch,
    #[error("resolver container creation failed; no declarations were transferred")]
    Create,
    #[error("resolver container startup failed; no declarations were transferred")]
    Start,
    #[error("the resolver container changed or stopped; discard this run")]
    RuntimeChanged,
    #[error("resolver cleanup could not be confirmed; inspect the reported run-owned resource")]
    Cleanup,
    #[error("resolver admission requires a direct, protected native Linux Docker daemon channel")]
    NativeDaemon,
}

/// Content identity, distinct from a mutable tag or a runtime qualification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageIdentity {
    /// Docker's content-addressed image ID, kept distinct from registry
    /// manifest digests. Its representation depends on the image store;
    /// running filesystem and loaded-code identity are separate premises.
    pub image_id: String,
    pub registry_digests: Vec<String>,
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

/// Acquisition output has no verified/admitted state and accepts no source.
#[derive(Clone)]
pub struct CandidateImage {
    identity: ImageIdentity,
    environment_keys: Option<Vec<String>>,
    acquisition: Option<[u8; 32]>,
}

impl std::fmt::Debug for CandidateImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CandidateImage")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl CandidateImage {
    pub fn identity(&self) -> &ImageIdentity {
        &self.identity
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInspect {
    id: String,
    repo_digests: Option<Vec<String>>,
    os: String,
    architecture: String,
    variant: Option<String>,
    config: Option<ImageConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageConfig {
    env: Option<Vec<String>>,
}

impl TryFrom<ImageInspect> for CandidateImage {
    type Error = Error;

    fn try_from(raw: ImageInspect) -> Result<Self, Error> {
        if !sha256(&raw.id) || !platform_word(&raw.os) || !platform_word(&raw.architecture) {
            return Err(Error::ImageIdentity);
        }
        let mut digests = raw.repo_digests.unwrap_or_default();
        if digests.iter().any(|digest| {
            !digest
                .rsplit_once('@')
                .is_some_and(|(name, digest)| image_reference(name) && sha256(digest))
        }) {
            return Err(Error::ImageIdentity);
        }
        digests.sort();
        digests.dedup();
        let variant = raw.variant.filter(|v| !v.is_empty());
        if variant.as_deref().is_some_and(|v| !platform_word(v)) {
            return Err(Error::ImageIdentity);
        }
        Ok(Self {
            identity: ImageIdentity {
                image_id: raw.id,
                registry_digests: digests,
                os: raw.os,
                architecture: raw.architecture,
                variant,
            },
            environment_keys: raw
                .config
                .and_then(|config| environment_keys(config.env.unwrap_or_default())),
            acquisition: None,
        })
    }
}

fn environment_keys(environment: Vec<String>) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    for entry in environment {
        let (key, _) = entry.split_once('=')?;
        if key.is_empty()
            || key.len() > 256
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return None;
        }
        keys.push(key.to_owned());
    }
    keys.sort();
    keys.dedup();
    Some(keys)
}

fn sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn platform_word(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn image_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:/@-".contains(&b))
        && !value.contains("://")
}

fn path_component(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            write!(out, "%{b:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

/// Owns one HTTP connection to one kernel-authenticated Unix peer.
///
/// There is no retry or reconnect. Cancellation, timeout, truncation and
/// oversized responses permanently invalidate the connection. The API peer
/// may itself be a runtime proxy; downstream runtime admission is separate.
pub struct LocalApi {
    id: [u8; 32],
    socket_path: PathBuf,
    sender: Option<SendRequest<Full<Bytes>>>,
    driver: tokio::task::JoinHandle<()>,
    peer: (u32, i32),
    native_daemon: Option<DaemonLease>,
}

impl Drop for LocalApi {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl LocalApi {
    async fn additional(&self) -> Result<Self, Error> {
        let lease = self.native_daemon.as_ref().ok_or(Error::NativeDaemon)?;
        lease.check().map_err(|_| Error::NativeDaemon)?;
        let other = Self::connect_native(&self.socket_path).await?;
        if self.peer != other.peer
            || !lease
                .same_process(other.native_daemon.as_ref().ok_or(Error::NativeDaemon)?)
                .map_err(|_| Error::NativeDaemon)?
        {
            return Err(Error::NativeDaemon);
        }
        Ok(other)
    }
    pub async fn connect(socket_path: &Path) -> Result<Self, Error> {
        if !socket_path.is_absolute() {
            return Err(Error::SocketPath);
        }
        Self::connect_peer(socket_path, 0).await
    }

    /// Qualifies this local Docker API peer, not a database or analysis run.
    /// Runtime proxies require their own every-hop profile and cannot enter
    /// this direct-daemon path merely by returning plausible Docker metadata.
    pub async fn connect_native(socket_path: &Path) -> Result<Self, Error> {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        if !socket_path.is_absolute() {
            return Err(Error::SocketPath);
        }
        let metadata = std::fs::metadata(socket_path).map_err(|_| Error::NativeDaemon)?;
        if !metadata.file_type().is_socket() || metadata.uid() != 0 || metadata.mode() & 0o007 != 0
        {
            return Err(Error::NativeDaemon);
        }
        for parent in socket_path.ancestors().skip(1) {
            let metadata = std::fs::metadata(parent).map_err(|_| Error::NativeDaemon)?;
            if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err(Error::NativeDaemon);
            }
        }
        Self::connect_profile(socket_path, 0, true).await
    }

    async fn connect_peer(socket_path: &Path, required_uid: u32) -> Result<Self, Error> {
        Self::connect_profile(socket_path, required_uid, false).await
    }

    async fn connect_profile(
        socket_path: &Path,
        required_uid: u32,
        native: bool,
    ) -> Result<Self, Error> {
        let stream = tokio::time::timeout(REQUEST_BUDGET, UnixStream::connect(socket_path))
            .await
            .map_err(|_| Error::ControlLost)?
            .map_err(|_| Error::ControlLost)?;
        let peer = stream.peer_cred().map_err(|_| Error::Peer)?;
        if peer.uid() != required_uid || peer.pid().is_none_or(|pid| pid <= 0) {
            return Err(Error::Peer);
        }
        let native_daemon = if native {
            Some(
                DaemonLease::capture(&stream)
                    .await
                    .map_err(|_| Error::NativeDaemon)?,
            )
        } else {
            None
        };
        let (sender, connection) = tokio::time::timeout(
            REQUEST_BUDGET,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|_| Error::ControlLost)?
        .map_err(|_| Error::ControlLost)?;
        let driver = tokio::spawn(async move {
            // The request side reports failures. The task must not log server
            // responses, credentials, or future source-bearing diagnostics.
            let _ = connection.with_upgrades().await;
        });
        Ok(Self {
            id: rand::random(),
            socket_path: socket_path.to_owned(),
            sender: Some(sender),
            driver,
            peer: (peer.uid(), peer.pid().ok_or(Error::Peer)?),
            native_daemon,
        })
    }

    async fn request(&mut self, method: Method, path: &str) -> Result<(StatusCode, Bytes), Error> {
        self.request_body(method, path, Bytes::new()).await
    }

    async fn request_body(
        &mut self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<(StatusCode, Bytes), Error> {
        // The guard also runs when the caller cancels this future. Merely
        // wrapping send_request in timeout would leave a pending response on
        // a reusable connection and allow a later request to inherit the run.
        let mut guard = RequestGuard {
            api: self,
            completed: false,
        };
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "docker")
            .header("Content-Type", "application/json")
            .body(Full::new(body))
            .map_err(|_| Error::Response)?;
        let exchange = async {
            if let Some(lease) = &guard.api.native_daemon {
                lease.check().map_err(|_| Error::NativeDaemon)?;
            }
            let sender = guard.api.sender.as_mut().ok_or(Error::ControlLost)?;
            let reply = sender
                .send_request(request)
                .await
                .map_err(|_| Error::ControlLost)?;
            let status = reply.status();
            let body = Limited::new(reply.into_body(), MAX_REPLY)
                .collect()
                .await
                .map_err(|_| Error::Response)?
                .to_bytes();
            if let Some(lease) = &guard.api.native_daemon {
                lease.check().map_err(|_| Error::NativeDaemon)?;
            }
            Ok((status, body))
        };
        let result = tokio::time::timeout(REQUEST_BUDGET, exchange)
            .await
            .map_err(|_| Error::ControlLost)?;
        if result.is_ok() {
            guard.completed = true;
        }
        result
    }

    pub async fn inspect_image(&mut self, image: &str) -> Result<Option<CandidateImage>, Error> {
        if !image_reference(image) {
            return Err(Error::Profile);
        }
        let path = format!("{API}/images/{}/json", path_component(image));
        let (status, body) = self.request(Method::GET, &path).await?;
        match status {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => {
                let raw: ImageInspect =
                    serde_json::from_slice(&body).map_err(|_| Error::ImageIdentity)?;
                let mut image: CandidateImage = raw.try_into()?;
                image.acquisition = Some(self.id);
                Ok(Some(image))
            }
            _ => Err(Error::Response),
        }
    }

    pub async fn acquire(&mut self, profile: &ResolverProfile) -> Result<CandidateImage, Error> {
        self.acquire_using(profile, pull_image).await
    }

    async fn acquire_using<P, F>(
        &mut self,
        profile: &ResolverProfile,
        pull_image: P,
    ) -> Result<CandidateImage, Error>
    where
        P: FnOnce(PathBuf, String) -> F,
        F: std::future::Future<Output = Result<(), Error>>,
    {
        let ResolverProfile::Docker { image, pull } = profile else {
            return Err(Error::Profile);
        };
        if let Some(candidate) = self.inspect_image(image).await? {
            return Ok(candidate);
        }
        if *pull == PullPolicy::Never {
            return Err(Error::MissingNoPull);
        }
        pull_image(self.socket_path.clone(), image.clone()).await?;
        self.inspect_image(image).await?.ok_or(Error::Pull)
    }
}

async fn pull_image(socket_path: PathBuf, image: String) -> Result<(), Error> {
    // Acquisition happens before declaration transfer. Docker's existing
    // credential helpers support configured internal registries; the API
    // connection subsequently observes the acquired immutable identity.
    // An explicit endpoint prevents an ambient context selecting a second
    // daemon. CLI output is not evidence and never enters diagnostics.
    let mut host = std::ffi::OsString::from("unix://");
    host.push(socket_path);
    let mut child = tokio::process::Command::new("docker")
        .args(["--host"])
        .arg(host)
        .args(["image", "pull", "--", &image])
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .env("DOCKER_API_VERSION", "1.47")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::Pull)?;
    let status = tokio::time::timeout(Duration::from_secs(300), child.wait())
        .await
        .map_err(|_| Error::Pull)?
        .map_err(|_| Error::Pull)?;
    if !status.success() {
        return Err(Error::Pull);
    }
    Ok(())
}

struct RequestGuard<'a> {
    api: &'a mut LocalApi,
    completed: bool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.api.sender = None;
            self.api.driver.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::UnixListener;

    pub(super) struct Fixture {
        path: PathBuf,
        pub(super) listener: UnixListener,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pbps-api-{:032x}", rand::random::<u128>()));
            let listener = UnixListener::bind(&path).unwrap();
            Self { path, listener }
        }

        pub(super) async fn client(&self) -> LocalApi {
            // Only the private test entry point accepts this process's UID.
            // The production constructor always requires the root-owned peer.
            let (socket, _) = UnixStream::pair().unwrap();
            let uid = socket.peer_cred().unwrap().uid();
            LocalApi::connect_peer(&self.path, uid).await.unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub(super) async fn request_line(socket: &mut UnixStream) -> String {
        let mut data = Vec::new();
        loop {
            let b = socket.read_u8().await.unwrap();
            data.push(b);
            assert!(data.len() < 16384);
            if data.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(data)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned()
    }

    async fn reply(socket: &mut UnixStream, status: &str, body: &str) {
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    fn image_body() -> String {
        serde_json::json!({
            "Id": format!("sha256:{}", "a".repeat(64)),
            "RepoDigests": null,
            "Os": "linux",
            "Architecture": "amd64",
            "Variant": ""
            , "Config": {"Env": []}
        })
        .to_string()
    }

    fn profile(pull: PullPolicy) -> ResolverProfile {
        ResolverProfile::Docker {
            image: "registry.example/team/engine:chosen".into(),
            pull,
        }
    }

    #[tokio::test]
    async fn missing_no_pull_and_unreadable_images_never_acquire() {
        for (status, missing) in [
            ("404 Not Found", true),
            ("500 Internal Server Error", false),
        ] {
            let fixture = Fixture::new();
            let mut client = fixture.client().await;
            let (mut socket, _) = fixture.listener.accept().await.unwrap();
            let server = tokio::spawn(async move {
                assert_eq!(
                    request_line(&mut socket).await,
                    "GET /v1.47/images/registry.example%2Fteam%2Fengine%3Achosen/json HTTP/1.1"
                );
                reply(&mut socket, status, "{}").await;
            });
            let result = client
                .acquire_using(&profile(PullPolicy::Never), |_, _| async {
                    panic!("missing and unreadable images must not trigger acquisition")
                })
                .await;
            assert!(if missing {
                matches!(result, Err(Error::MissingNoPull))
            } else {
                matches!(result, Err(Error::Response))
            });
            server.await.unwrap();
        }
        // An unreadable response must not become an if-missing pull either.
        let fixture = Fixture::new();
        let mut client = fixture.client().await;
        let (mut socket, _) = fixture.listener.accept().await.unwrap();
        let server = tokio::spawn(async move {
            request_line(&mut socket).await;
            reply(&mut socket, "403 Forbidden", "{}").await;
        });
        assert!(matches!(
            client
                .acquire_using(&profile(PullPolicy::IfMissing), |_, _| async {
                    panic!("permission failure is not a missing image")
                })
                .await,
            Err(Error::Response)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn acquisition_is_lazy_and_pins_the_observed_content() {
        for missing in [false, true] {
            let fixture = Fixture::new();
            let mut client = fixture.client().await;
            let (mut socket, _) = fixture.listener.accept().await.unwrap();
            let server = tokio::spawn(async move {
                request_line(&mut socket).await;
                if missing {
                    reply(&mut socket, "404 Not Found", "{}").await;
                    request_line(&mut socket).await;
                }
                reply(&mut socket, "200 OK", &image_body()).await;
            });
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let socket_path = fixture.path.clone();
            let candidate = client
                .acquire_using(
                    &profile(PullPolicy::IfMissing),
                    move |path, image| async move {
                        assert_eq!(path, socket_path);
                        assert_eq!(image, "registry.example/team/engine:chosen");
                        counter.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::Relaxed), usize::from(missing));
            assert_eq!(
                candidate.identity().image_id,
                format!("sha256:{}", "a".repeat(64))
            );
            assert!(candidate.identity().registry_digests.is_empty());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_discards_the_actual_connection_without_a_retry() {
        let fixture = Fixture::new();
        let mut client = fixture.client().await;
        let (mut socket, _) = fixture.listener.accept().await.unwrap();
        let server = tokio::spawn(async move {
            request_line(&mut socket).await;
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), socket.read(&mut byte))
                    .await
                    .expect("cancellation must close the peer socket")
                    .unwrap(),
                0
            );
        });
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                client.inspect_image("postgres:18")
            )
            .await
            .is_err()
        );
        assert!(matches!(
            client.inspect_image("postgres:18").await,
            Err(Error::ControlLost)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), fixture.listener.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_and_oversized_replies_poison_control() {
        let mut oversized: serde_json::Value = serde_json::from_str(&image_body()).unwrap();
        oversized["UnneededConfig"] = serde_json::json!("x".repeat(MAX_REPLY));
        let oversized = oversized.to_string();
        for (length, body) in [(10, "{".to_owned()), (oversized.len(), oversized)] {
            let fixture = Fixture::new();
            let mut client = fixture.client().await;
            let (mut socket, _) = fixture.listener.accept().await.unwrap();
            let server = tokio::spawn(async move {
                request_line(&mut socket).await;
                let _ = socket
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n{body}")
                            .as_bytes(),
                    )
                    .await;
                if length > MAX_REPLY {
                    // Keep-alive must not mask a missing client-side abort.
                    // Closing the fixture here would make the next request
                    // fail even if the poisoned connection remained reusable.
                    let mut byte = [0];
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(1), socket.read(&mut byte))
                            .await
                            .expect("oversized replies must close control")
                            .unwrap(),
                        0
                    );
                }
            });
            assert!(matches!(
                client.inspect_image("postgres:18").await,
                Err(Error::Response)
            ));
            assert!(matches!(
                client.inspect_image("postgres:18").await,
                Err(Error::ControlLost)
            ));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn wrong_unix_peer_is_rejected_before_api_bytes() {
        let fixture = Fixture::new();
        let (socket, _) = UnixStream::pair().unwrap();
        let wrong_uid = socket.peer_cred().unwrap().uid().wrapping_add(1);
        assert!(matches!(
            LocalApi::connect_peer(&fixture.path, wrong_uid).await,
            Err(Error::Peer)
        ));
        let (mut socket, _) = fixture.listener.accept().await.unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore = "requires PBPS_RESOLVER_TEST_SOCKET and an explicitly trusted PBPS_RESOLVER_TEST_IMAGE"]
    async fn local_image_acquisition_observes_immutable_content_without_pulling() {
        let path = PathBuf::from(
            std::env::var_os("PBPS_RESOLVER_TEST_SOCKET").expect("explicit test socket"),
        );
        let image =
            std::env::var("PBPS_RESOLVER_TEST_IMAGE").expect("explicit trusted local image");
        let mut api = LocalApi::connect(&path).await.unwrap();
        let configured = ResolverProfile::Docker {
            image,
            pull: PullPolicy::Never,
        };
        let first = api.acquire(&configured).await.unwrap();
        let second = api.acquire(&configured).await.unwrap();
        assert_eq!(first.identity(), second.identity());
        assert!(sha256(&first.identity().image_id));
        let missing = ResolverProfile::Docker {
            image: format!(
                "localhost:9/pbps-absent-{:032x}:test",
                rand::random::<u128>()
            ),
            pull: PullPolicy::Never,
        };
        assert!(matches!(
            api.acquire(&missing).await,
            Err(Error::MissingNoPull)
        ));
    }

    #[tokio::test]
    #[ignore = "requires a root-owned native Docker socket, explicit test image and a disposable root test runner"]
    async fn direct_native_daemon_is_accepted_but_a_root_owned_proxy_is_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = PathBuf::from(
            std::env::var_os("PBPS_RESOLVER_TEST_SOCKET").expect("explicit native socket"),
        );
        let image =
            std::env::var("PBPS_RESOLVER_TEST_IMAGE").expect("explicit trusted local image");
        let mut api = LocalApi::connect_native(&path).await.unwrap();
        assert!(api.inspect_image(&image).await.unwrap().is_some());
        let path = PathBuf::from(format!(
            "/pbps-native-proxy-{:032x}",
            rand::random::<u128>()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let proxy = Fixture { path, listener };
        assert!(matches!(
            LocalApi::connect_native(&proxy.path).await,
            Err(Error::NativeDaemon)
        ));
        let (mut peer, _) = proxy.listener.accept().await.unwrap();
        let mut byte = [0];
        assert_eq!(
            peer.read(&mut byte).await.unwrap(),
            0,
            "unqualified proxies receive no API requests"
        );
    }

    #[test]
    fn missing_or_malformed_content_identity_cannot_be_a_candidate() {
        let body: serde_json::Value = serde_json::from_str(&image_body()).unwrap();
        for (key, value) in [
            ("Id", serde_json::json!("postgres:18")),
            ("Id", serde_json::json!("sha256:abc")),
            ("Os", serde_json::json!("")),
            ("Architecture", serde_json::Value::Null),
            ("RepoDigests", serde_json::json!(["registry.example/pg:18"])),
        ] {
            let mut bad = body.clone();
            bad[key] = value;
            let result = serde_json::from_value::<ImageInspect>(bad)
                .map_err(|_| Error::ImageIdentity)
                .and_then(CandidateImage::try_from);
            assert!(result.is_err(), "{key}");
        }
        assert_eq!(
            path_component("internal:5000/team/image@sha256:abc"),
            "internal%3A5000%2Fteam%2Fimage%40sha256%3Aabc"
        );
        assert!(!image_reference("https://user:password@registry/image"));
        assert!(!image_reference("image?arbitrary=true"));
    }
}
