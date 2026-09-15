use super::*;
use crate::resolver::docker::{ImageIdentity, profile::PROFILE};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

#[derive(Default)]
struct Observations {
    requests: Vec<String>,
    container: Option<Value>,
    start_failed: bool,
    conflict: bool,
    lose_create_reply: bool,
    delay_create: bool,
    delay_inspect: bool,
    creations: usize,
    deletions: usize,
    deletion_in_progress: bool,
    deletion_reads: usize,
}

struct Fixture {
    path: PathBuf,
    seen: Arc<Mutex<Observations>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn candidate() -> CandidateImage {
    CandidateImage {
        environment_keys: Some(vec![]),
        acquisition: None,
        identity: ImageIdentity {
            image_id: format!("sha256:{}", "e".repeat(64)),
            registry_digests: Vec::new(),
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        },
    }
}

impl Fixture {
    fn new(observed: Observations) -> Self {
        let path =
            std::env::temp_dir().join(format!("pbps-lifecycle-{:032x}", rand::random::<u128>()));
        let listener = UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(observed));
        let server_seen = seen.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                while let Some((request, body)) = read_request(&mut socket).await {
                    let (status, reply, disconnect, delay) = {
                        let mut seen = server_seen.lock().unwrap();
                        seen.requests.push(request.clone());
                        answer(&mut seen, &request, body)
                    };
                    if delay {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    if disconnect {
                        break;
                    }
                    let reply = if status.starts_with("204 ") {
                        String::new()
                    } else {
                        reply.to_string()
                    };
                    if socket
                        .write_all(
                            format!(
                                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{reply}",
                                reply.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self { path, seen, server }
    }

    async fn api(&self) -> LocalApi {
        let (a, _) = UnixStream::pair().unwrap();
        LocalApi::connect_peer(&self.path, a.peer_cred().unwrap().uid())
            .await
            .unwrap()
    }

    async fn removed(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if self.seen.lock().unwrap().deletions == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned resource must be removed after cancellation or control loss");
    }
}

async fn read_request(socket: &mut UnixStream) -> Option<(String, Value)> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(socket.read_u8().await.ok()?);
        assert!(header.len() < 16384);
    }
    let header = String::from_utf8(header).unwrap();
    let length: usize = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    Some((header.lines().next().unwrap().to_owned(), body))
}

fn answer(
    seen: &mut Observations,
    request: &str,
    body: Value,
) -> (&'static str, Value, bool, bool) {
    use serde_json::json;
    let id = "a".repeat(64);
    if request.starts_with("POST /v1.47/containers/create?name=") {
        if seen.conflict {
            return ("409 Conflict", json!({}), false, false);
        }
        let name = request
            .split_once("name=")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap();
        seen.creations += 1;
        assert_eq!(body["Image"], candidate().identity.image_id);
        assert_eq!(body["Labels"]["io.pbps.resolver.profile"], PROFILE);
        let mut host = body["HostConfig"].clone();
        for key in ["PidMode", "UTSMode", "UsernsMode"] {
            host[key] = json!("");
        }
        host["IpcMode"] = json!("private");
        host["Privileged"] = json!(false);
        host["PublishAllPorts"] = json!(false);
        seen.container = Some(json!({
            "Id": id, "Name": format!("/{name}"), "Image": body["Image"],
            "Config": body, "HostConfig": host, "Mounts": [], "RestartCount": 0,
            "State": {"Running": false, "Paused": false, "Restarting": false, "Pid": 0, "StartedAt": ""}
        }));
        return (
            "201 Created",
            json!({"Id": id}),
            seen.lose_create_reply,
            seen.delay_create,
        );
    }
    if request.starts_with("GET /v1.47/containers/") {
        if seen.deletions > 0 && seen.deletion_in_progress {
            if seen.deletion_reads == 0 {
                seen.container = None;
            } else {
                seen.deletion_reads -= 1;
            }
        }
        return match &seen.container {
            Some(container) => ("200 OK", container.clone(), false, seen.delay_inspect),
            None => ("404 Not Found", json!({}), false, false),
        };
    }
    if request == format!("POST /v1.47/containers/{id}/start HTTP/1.1") {
        if seen.start_failed {
            return (
                "500 Internal Server Error",
                json!({"message":"private server error"}),
                false,
                false,
            );
        }
        let state = &mut seen.container.as_mut().unwrap()["State"];
        state["Running"] = json!(true);
        state["Pid"] = json!(345);
        state["StartedAt"] = json!("2026-01-01T00:00:00.000000001Z");
        return ("204 No Content", Value::Null, false, false);
    }
    assert_eq!(
        request,
        format!("DELETE /v1.47/containers/{id}?force=1&v=1 HTTP/1.1")
    );
    seen.deletions += 1;
    if seen.deletion_in_progress {
        seen.deletion_reads = 2;
        return (
            "409 Conflict",
            json!({"message":"removal already in progress"}),
            false,
            false,
        );
    }
    seen.container = None;
    ("204 No Content", Value::Null, false, false)
}

#[tokio::test]
async fn successful_close_confirms_only_the_created_immutable_resource_is_removed() {
    let fixture = Fixture::new(Observations::default());
    let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .unwrap();
    assert_eq!(run.container_id(), "a".repeat(64));
    run.check().await.unwrap();
    run.close().await.unwrap();
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(seen.creations, 1);
    assert_eq!(seen.deletions, 1);
    assert!(seen.container.is_none());
}

#[tokio::test]
async fn removal_already_in_progress_is_confirmed_by_absence_without_another_delete() {
    let fixture = Fixture::new(Observations {
        deletion_in_progress: true,
        ..Default::default()
    });
    let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .unwrap();
    run.close().await.unwrap();
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(seen.deletions, 1);
    assert!(seen.container.is_none());
}

#[tokio::test]
async fn dropping_a_started_run_stops_and_removes_its_owned_resource() {
    let fixture = Fixture::new(Observations::default());
    let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .unwrap();
    drop(run);
    fixture.removed().await;
}

#[tokio::test]
async fn cancelled_creation_is_cleaned_without_starting_the_engine() {
    let fixture = Fixture::new(Observations {
        delay_create: true,
        ..Default::default()
    });
    let api = fixture.api().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            CandidateRun::start(api, candidate(), Driver::Postgres)
        )
        .await
        .is_err()
    );
    fixture.removed().await;
    assert!(
        !fixture
            .seen
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.contains("/start "))
    );
}

#[tokio::test]
async fn failed_startup_is_cleaned_and_does_not_echo_the_server_error() {
    let fixture = Fixture::new(Observations {
        start_failed: true,
        ..Default::default()
    });
    let failure = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .err()
        .unwrap();
    assert!(matches!(failure.cause, Error::Start));
    assert!(failure.recovery_names.is_empty());
    assert!(!format!("{failure:?}").contains("private server error"));
    assert_eq!(fixture.seen.lock().unwrap().deletions, 1);
}

#[tokio::test]
async fn a_name_conflict_never_inspects_starts_or_deletes_the_existing_container() {
    let fixture = Fixture::new(Observations {
        conflict: true,
        ..Default::default()
    });
    let failure = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .err()
        .unwrap();
    assert!(matches!(failure.cause, Error::Create));
    assert!(failure.recovery_names.is_empty());
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(seen.requests.len(), 1);
    assert_eq!(seen.deletions, 0);
}

#[tokio::test]
async fn a_lost_create_reply_uses_a_new_connection_only_to_remove_the_owned_resource() {
    let fixture = Fixture::new(Observations {
        lose_create_reply: true,
        ..Default::default()
    });
    let failure = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .err()
        .unwrap();
    assert!(matches!(failure.cause, Error::ControlLost));
    assert!(failure.recovery_names.is_empty());
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(seen.creations, 1);
    assert_eq!(seen.deletions, 1);
    assert!(!seen.requests.iter().any(|r| r.contains("/start ")));
}

#[tokio::test]
async fn a_restarted_or_replaced_candidate_cannot_keep_its_handle() {
    for (field, value) in [
        ("Pid", serde_json::json!(678)),
        (
            "StartedAt",
            serde_json::json!("2026-01-01T00:00:01.000000001Z"),
        ),
        ("Running", serde_json::json!(false)),
    ] {
        let fixture = Fixture::new(Observations::default());
        let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
            .await
            .unwrap();
        fixture.seen.lock().unwrap().container.as_mut().unwrap()["State"][field] = value;
        assert!(run.check().await.is_err(), "{field}");
        fixture.removed().await;
        assert!(run.check().await.is_err(), "old handles never resume");
    }
}

#[tokio::test]
async fn ownership_mismatch_prevents_deleting_someone_elses_resource() {
    let fixture = Fixture::new(Observations::default());
    let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .unwrap();
    fixture.seen.lock().unwrap().container.as_mut().unwrap()["Config"]["Labels"][OWNER_LABEL] =
        serde_json::json!("someone-else");
    assert!(run.close().await.is_err());
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(seen.deletions, 0);
    assert!(seen.container.is_some());
}

#[tokio::test]
async fn cancelling_a_continuity_read_invalidates_the_run_and_enters_cleanup() {
    let fixture = Fixture::new(Observations::default());
    let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .unwrap();
    fixture.seen.lock().unwrap().delay_inspect = true;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), run.check())
            .await
            .is_err()
    );
    fixture.removed().await;
    assert!(run.check().await.is_err());
}

#[tokio::test]
async fn weakened_limits_or_host_access_invalidate_the_candidate() {
    for (key, value) in [
        ("ReadonlyRootfs", serde_json::json!(false)),
        ("Memory", serde_json::json!(0)),
        ("NanoCpus", serde_json::json!(0)),
        ("Binds", serde_json::json!(["/:/host"])),
        ("Devices", serde_json::json!([{"PathOnHost":"/dev/sda"}])),
        ("PidMode", serde_json::json!("host")),
    ] {
        let fixture = Fixture::new(Observations::default());
        let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
            .await
            .unwrap();
        fixture.seen.lock().unwrap().container.as_mut().unwrap()["HostConfig"][key] = value;
        assert!(run.check().await.is_err(), "{key}");
        fixture.removed().await;
    }
}

#[tokio::test]
async fn inherited_startup_hooks_or_added_environment_invalidate_the_run() {
    for (key, value) in [
        (
            "Healthcheck",
            serde_json::json!({"Test": ["CMD-SHELL", "echo hook"]}),
        ),
        (
            "Env",
            serde_json::json!(["PATH=/usr/bin:/bin", "BASH_ENV=/tmp/hook"]),
        ),
    ] {
        let fixture = Fixture::new(Observations::default());
        let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
            .await
            .unwrap();
        fixture.seen.lock().unwrap().container.as_mut().unwrap()["Config"][key] = value;
        assert!(run.check().await.is_err(), "{key}");
        fixture.removed().await;
    }
}

#[tokio::test]
async fn cleanup_completed_before_close_is_still_confirmed() {
    let fixture = Fixture::new(Observations::default());
    let run = CandidateRun::start(fixture.api().await, candidate(), Driver::Postgres)
        .await
        .unwrap();
    fixture.seen.lock().unwrap().container.as_mut().unwrap()["State"]["Running"] =
        serde_json::json!(false);
    assert!(run.check().await.is_err());
    fixture.removed().await;
    run.close()
        .await
        .expect("the finished supervisor's successful cleanup must remain observable");
}

#[tokio::test]
#[ignore = "requires explicit PBPS_RESOLVER_TEST_SOCKET, PBPS_RESOLVER_TEST_IMAGE and PBPS_RESOLVER_TEST_DRIVER"]
async fn fixed_launch_survives_bootstrap_and_explicit_close_removes_the_live_candidate() {
    let socket =
        PathBuf::from(std::env::var_os("PBPS_RESOLVER_TEST_SOCKET").expect("explicit socket"));
    let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").expect("explicit trusted image");
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER")
        .expect("explicit engine")
        .as_str()
    {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("unsupported fixture driver"),
    };
    let mut api = LocalApi::connect(&socket).await.unwrap();
    let image = api
        .acquire(&pbps_config::resolver::ResolverProfile::Docker {
            image,
            pull: pbps_config::resolver::PullPolicy::Never,
        })
        .await
        .unwrap();
    let run = CandidateRun::start(api, image, driver).await.unwrap();
    let id = run.container_id().to_owned();
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        run.check().await.unwrap();
    }
    let mut observer = LocalApi::connect(&socket).await.unwrap();
    let (uid, port) = match driver {
        Driver::Postgres => ("999:999", 5432),
        Driver::Mssql => ("10001:0", 1433),
    };
    let network_probe = format!(
        "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then exit 91; fi; if kill -KILL 1 2>/dev/null; then exit 92; fi; test ! -e /var/run/docker.sock; test ! -w /etc/passwd; if (echo sentinel > /var/tmp/pbps-readonly-probe) 2>/dev/null; then exit 93; fi; cp /bin/true /tmp/pbps-execution-probe; chmod 700 /tmp/pbps-execution-probe; if /tmp/pbps-execution-probe 2>/dev/null; then exit 94; fi; rm /tmp/pbps-execution-probe; if dd if=/dev/zero of=/tmp/pbps-storage-probe bs=1048576 count=80 status=none 2>/dev/null; then exit 95; fi; size=$(wc -c < /tmp/pbps-storage-probe); test \"$size\" -le 67108864; test \"$size\" -gt 0; rm /tmp/pbps-storage-probe"
    );
    let probe = tokio::process::Command::new("docker")
        .args([
            "--host",
            &format!("unix://{}", socket.display()),
            "exec",
            "--user",
            uid,
            &id,
            "/bin/bash",
            "-ec",
            &network_probe,
        ])
        .output()
        .await
        .unwrap();
    if !probe.status.success() {
        let cleanup = run.close().await;
        assert!(cleanup.is_ok(), "failed protection must still clean up");
        panic!("workload connection, guard-kill, socket or host-file protection failed");
    }
    // On the fixed amd64 profile, sendto(44) with MSG_FASTOPEN initiates TCP
    // without connect(42). Exercise the real syscall, not the profile JSON.
    let fast_open = format!(
        r#"my $fd = syscall(41, 2, 1, 0); die "socket" if $fd < 0; my $address = pack('Sna4x8', 2, {port}, pack('C4', 127, 0, 0, 1)); my $data = "\0"; my $result = syscall(44, $fd, $data, 1, 0x20000000, $address, 16); die "implicit connection was not refused" unless $result == -1 && $! == 1;"#
    );
    let fast_open = tokio::process::Command::new("docker")
        .args([
            "--host",
            &format!("unix://{}", socket.display()),
            "exec",
            "--user",
            uid,
            &id,
            "/usr/bin/timeout",
            "5s",
            "/usr/bin/perl",
            "-e",
            &fast_open,
        ])
        .output()
        .await
        .unwrap();
    run.close().await.unwrap();
    assert!(inspect(&mut observer, &id).await.unwrap().is_none());
    assert!(
        fast_open.status.success(),
        "TCP Fast Open must not create a second session: {}",
        String::from_utf8_lossy(&fast_open.stderr)
    );
}

#[tokio::test]
#[ignore = "requires explicit Docker fixture image; verifies a shortened root deadline against a detached descendant"]
async fn the_root_deadline_removes_even_a_detached_descendant() {
    let socket = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit fixture engine required"),
    };
    let mut api = LocalApi::connect(&socket).await.unwrap();
    let image = api.inspect_image(&image).await.unwrap().unwrap();
    let owner = format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    );
    let mut launch = Launch::new(&image, driver, &owner).unwrap();
    // Shorten only the fixture's external deadline; the supervisor retains
    // its full production timeout. setsid detaches from the shell's group.
    launch.body["Cmd"][1] = serde_json::json!("2s");
    *launch.body["Cmd"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap() = serde_json::json!("setsid /bin/sleep 600 & wait");
    let run = CandidateRun::start_launch(api, image, owner, launch)
        .await
        .unwrap();
    let host = format!("unix://{}", socket.display());
    let descendants = tokio::process::Command::new("docker")
        .args([
            "--host",
            &host,
            "top",
            run.container_id(),
            "-eo",
            "pid,comm",
        ])
        .output()
        .await
        .unwrap();
    let mut observer = LocalApi::connect(&socket).await.unwrap();
    let expired = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if inspect(&mut observer, run.container_id())
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    let closed = run.close().await;
    closed.unwrap();
    assert!(descendants.status.success());
    assert!(
        String::from_utf8_lossy(&descendants.stdout)
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some("sleep")),
        "the detached descendant must actually run"
    );
    assert!(
        expired.is_ok(),
        "the root deadline must remove the complete PID namespace without a client close request"
    );
}

#[tokio::test]
#[ignore = "requires explicit Docker fixture image; measures owned DNS, metadata, file and mock runtime-socket sentinels"]
async fn external_sentinels_are_unreachable_until_the_corresponding_boundary_is_removed() {
    use tokio::io::AsyncWriteExt as _;
    let socket = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let reference = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit fixture engine required"),
    };
    let mut api = LocalApi::connect(&socket).await.unwrap();
    let image = api.inspect_image(&reference).await.unwrap().unwrap();
    let launch = Launch::new(&image, driver, "owned-sentinel-fixture").unwrap();
    let helper =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/resolver-sentinels.py");
    let mut child = tokio::process::Command::new("python3")
        .arg(helper)
        .arg(socket)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&launch.body).unwrap())
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 4);
}
