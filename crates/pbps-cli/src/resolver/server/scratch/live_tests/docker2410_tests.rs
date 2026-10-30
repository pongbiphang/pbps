//! Crash recovery of a Docker scratch (#2410, DEC-2410.3), on the pinned
//! PostgreSQL image. A killed run leaves what it made: a running container,
//! named for its token and labelled, with or without the session its run kept
//! in the server. These tests make that state by hand with the names and
//! labels a run gives it; the real SIGKILLs of a `pbps plan` are in
//! `tests/flow_pg.rs`. The grace for a container that never had a keeper is
//! twenty seconds here, not five minutes.

use super::super::container::{Keeper, swept, swept_alone, swept_with_observation};
use super::setting;
use crate::resolver::server::engine;
use pbps_db::resolver::Kept;
use pbps_db::{Conn, Driver};
use std::process::Command;
use std::time::Duration;

/// The grace the tests that cross it sweep their own container under, with
/// `swept_alone`. Every other sweep keeps the run's own grace.
const GRACE: Duration = Duration::from_secs(20);

fn token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn image() -> String {
    setting("PBPS_TEST_PG_SCRATCH_IMAGE")
}

/// Runs `docker`, and returns its standard output.
fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker runs");
    assert!(output.status.success(), "docker {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn exists(name: &str) -> bool {
    Command::new("docker")
        .args(["container", "inspect", name])
        .output()
        .expect("docker runs")
        .status
        .success()
}

fn volumes_of(name: &str) -> Vec<String> {
    docker(&[
        "container",
        "inspect",
        "--format",
        "{{range .Mounts}}{{.Name}} {{end}}",
        name,
    ])
    .split_whitespace()
    .map(str::to_owned)
    .collect()
}

fn volume_exists(name: &str) -> bool {
    Command::new("docker")
        .args(["volume", "inspect", name])
        .output()
        .expect("docker runs")
        .status
        .success()
}

/// Removes the containers a test made, whatever it found.
struct Made(Vec<String>);

impl Drop for Made {
    fn drop(&mut self) {
        for name in &self.0 {
            let _ = Command::new("docker")
                .args(["rm", "--force", "--volumes", name])
                .output();
        }
    }
}

/// Starts a PostgreSQL container the way a run does: named for its token,
/// labelled with it and its engine, the password in its environment, one
/// loopback port published. Returns its name and a connection to its server.
async fn start(made: &mut Made, token: &str) -> (String, String) {
    let name = format!("pbps-scratch-{token}");
    docker(&[
        "run",
        "--detach",
        "--pull",
        "never",
        "--name",
        &name,
        "--label",
        "pbps.resolver=scratch",
        "--label",
        &format!("pbps.resolver.token={token}"),
        "--label",
        "pbps.resolver.engine=postgres",
        "--env",
        "POSTGRES_PASSWORD=secret2410",
        "--publish",
        "127.0.0.1::5432",
        &image(),
    ]);
    made.0.push(name.clone());
    let port = docker(&["port", &name, "5432/tcp"]);
    let port = port.trim().strip_prefix("127.0.0.1:").unwrap().to_owned();
    let connection = format!(
        "host=127.0.0.1 port={port} user=postgres password=secret2410 dbname=postgres sslmode=disable"
    );
    for _ in 0..240 {
        if let Ok(mut conn) = Conn::connect(Driver::Postgres, &connection).await
            && conn.query("SELECT 1 AS ok").await.is_ok()
        {
            return (name, connection);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("the container {name} never accepted a connection");
}

/// What a sweep reported of one container. Other runs on the same daemon have
/// containers of their own in the same report, and they are not a test's
/// business (concurrent runs on one daemon are a supported case).
fn removed(report: &super::super::recovery::Report, name: &str) -> bool {
    report
        .groups
        .iter()
        .flat_map(|group| &group.removed)
        .any(|resource| resource.names.iter().any(|n| n == name))
}

fn kept_why(report: &super::super::recovery::Report, name: &str) -> Vec<String> {
    report
        .groups
        .iter()
        .flat_map(|group| &group.kept)
        .filter(|kept| kept.resource.names.iter().any(|n| n == name))
        .map(|kept| kept.why.clone())
        .collect()
}

/// Whether a sweep found the owner of `token` alive.
fn alive(report: &super::super::recovery::Report, token: &str) -> bool {
    report.live.iter().any(|live| live == token)
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_whose_keeper_connected_and_is_gone_is_removed_at_once() {
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, connection) = start(&mut made, &dead).await;
    let volumes = volumes_of(&name);
    // The run's session, and then its death: the server shows it gone, and
    // knows it was there.
    drop(
        Keeper::open(Driver::Postgres, &connection, &dead)
            .await
            .unwrap(),
    );
    let report = swept(&token()).await;
    let (still, gone) = (
        exists(&name),
        volumes.iter().filter(|v| volume_exists(v)).count(),
    );
    // Another run's sweep may have removed it first: what is asked is that it
    // is gone, at once, and that this sweep did not keep or spare it.
    assert!(!still, "removed well inside the grace: {report:?}");
    assert_eq!(gone, 0, "with its volumes: {volumes:?}");
    assert!(
        kept_why(&report, &name).is_empty() && !alive(&report, &dead),
        "{report:?}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_keeper_that_connects_between_the_two_reads_is_alive_and_not_yet_kept() {
    let mut made = Made(Vec::new());
    let run = token();
    let (_, connection) = start(&mut made, &run).await;
    // The reader stops between its reads while the run's keeper connects and
    // marks the server: a keeper that is there and a mark that is not yet
    // read, never a session that was missed and a mark that was seen.
    let (between, go) = engine::pause_between_reads(&run);
    let reader = token();
    let (seen, keeper) = tokio::join!(
        engine::keeper_seen(Driver::Postgres, &connection, &reader, &run),
        async {
            between.await.unwrap();
            let keeper = Keeper::open(Driver::Postgres, &connection, &run)
                .await
                .unwrap();
            go.send(()).unwrap();
            keeper
        }
    );
    let kept = Kept {
        alive: true,
        ever: false,
    };
    assert_eq!(seen.unwrap(), Some(kept));
    drop(keeper);
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_that_never_connected_is_kept_inside_the_grace_and_removed_after_it() {
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, _) = start(&mut made, &dead).await;
    let inside = swept_alone(&token(), &dead, GRACE).await;
    let kept = exists(&name);
    tokio::time::sleep(GRACE + Duration::from_millis(1500)).await;
    let after = swept_alone(&token(), &dead, GRACE).await;
    let gone = !exists(&name);
    assert!(kept, "a run that has not connected yet may still");
    assert!(
        alive(&inside, &dead) && !removed(&inside, &name) && kept_why(&inside, &name).is_empty(),
        "{inside:?}"
    );
    assert!(gone, "removed after the grace: {after:?}");
    assert!(!alive(&after, &dead), "{after:?}");
    assert!(
        after.groups.iter().all(|group| group.token == dead),
        "a confined sweep judges no other run's container: {after:?}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_live_runs_container_is_never_touched_even_past_the_grace() {
    let mut made = Made(Vec::new());
    let live = token();
    let (name, connection) = start(&mut made, &live).await;
    let keeper = Keeper::open(Driver::Postgres, &connection, &live)
        .await
        .unwrap();
    tokio::time::sleep(GRACE + Duration::from_millis(1500)).await;
    let report = swept_alone(&token(), &live, GRACE).await;
    let kept = exists(&name);
    drop(keeper);
    assert!(kept, "its keeper is in the server");
    assert!(
        alive(&report, &live) && !removed(&report, &name) && kept_why(&report, &name).is_empty(),
        "{report:?}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_that_has_exited_is_removed_at_once() {
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, _) = start(&mut made, &dead).await;
    docker(&["kill", &name]);
    let report = swept(&token()).await;
    assert!(!exists(&name), "{report:?}");
    assert!(
        kept_why(&report, &name).is_empty() && !alive(&report, &dead),
        "{report:?}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_stopped_after_a_running_observation_is_removed_at_once() {
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, _) = start(&mut made, &dead).await;
    let volumes = volumes_of(&name);
    let report = swept_with_observation(&token(), &dead, "running", async |id| {
        let running = docker(&["container", "inspect", "--format", "{{json .}}", id]);
        // The keeper probe loses its server after observing a running runtime.
        // Re-reading an exited runtime proves death without a grace period.
        docker(&["kill", id]);
        assert_eq!(
            docker(&["container", "inspect", "--format", "{{.State.Status}}", id]),
            "exited"
        );
        running
    })
    .await;
    assert!(!exists(&name), "{report:?}");
    assert!(volumes.iter().all(|v| !volume_exists(v)), "{volumes:?}");
    assert!(removed(&report, &name), "{report:?}");
    assert!(kept_why(&report, &name).is_empty(), "{report:?}");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_from_another_tool_or_without_a_token_is_never_touched() {
    let mut made = Made(Vec::new());
    let (a, b, c, d) = (token(), token(), token(), token());
    // Each sleeps, so the image's server need not start: what is judged is the
    // name and the labels. None is a run's container.
    let sleeping = |made: &mut Made, name: &str, labels: &[String]| {
        let mut args = vec![
            "run".to_owned(),
            "--detach".into(),
            "--pull".into(),
            "never".into(),
        ];
        args.extend(["--name".into(), name.to_owned()]);
        for label in labels {
            args.extend(["--label".into(), label.clone()]);
        }
        args.extend(["--entrypoint".into(), "sleep".into(), image(), "600".into()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&args);
        made.0.push(name.to_owned());
    };
    // No labels at all: another tool's, even with the name's shape.
    sleeping(&mut made, &format!("pbps-scratch-{a}"), &[]);
    // The scratch label of an earlier version: no token.
    sleeping(
        &mut made,
        &format!("pbps-scratch-{}", &token()[..16]),
        &["pbps.resolver=scratch".to_owned()],
    );
    // A token that is not the name's.
    sleeping(
        &mut made,
        &format!("pbps-scratch-{b}"),
        &[
            "pbps.resolver=scratch".to_owned(),
            format!("pbps.resolver.token={c}"),
        ],
    );
    // Another tool's name carrying the label.
    sleeping(
        &mut made,
        &format!("other-{d}"),
        &[
            "pbps.resolver=scratch".to_owned(),
            format!("pbps.resolver.token={d}"),
        ],
    );
    let report = swept(&token()).await;
    let present = made.0.iter().all(|name| exists(name));
    assert!(present, "all four are still there");
    assert!(
        made.0
            .iter()
            .all(|name| { !removed(&report, name) && kept_why(&report, name).is_empty() })
            && [&a, &b, &c, &d].iter().all(|token| !alive(&report, token)),
        "{report:?}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_whose_server_cannot_be_read_is_kept_and_named_with_the_command() {
    let mut made = Made(Vec::new());
    let dead = token();
    let name = format!("pbps-scratch-{dead}");
    // Running, a run's by name and labels, and no port published: this run
    // cannot reach a server to ask it anything.
    docker(&[
        "run",
        "--detach",
        "--pull",
        "never",
        "--name",
        &name,
        "--label",
        "pbps.resolver=scratch",
        "--label",
        &format!("pbps.resolver.token={dead}"),
        "--label",
        "pbps.resolver.engine=postgres",
        "--env",
        "POSTGRES_PASSWORD=secret2410",
        "--entrypoint",
        "sleep",
        &image(),
        "600",
    ]);
    made.0.push(name.clone());
    let report = swept(&token()).await;
    let kept = exists(&name);
    assert!(kept, "it is not judged");
    let group = report
        .groups
        .iter()
        .find(|group| group.token == dead)
        .unwrap_or_else(|| panic!("no group for {dead}: {report:?}"));
    assert!(group.removed.is_empty(), "{group:?}");
    assert_eq!(
        group.kept[0].statement,
        format!("docker rm --force --volumes {name}")
    );
    assert!(
        group.kept[0].why.contains("could not be read"),
        "{}",
        group.kept[0].why
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn recovery_confirms_another_remover_started_between_listing_and_liveness() {
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, _) = start(&mut made, &dead).await;
    let volumes = volumes_of(&name);
    let mut remover = None;
    let report = swept_with_observation(&token(), &dead, "removing", async |id| {
        // Make native writable-layer deletion observable, then gate on the
        // daemon's actual removing state rather than a guessed delay.
        docker(&[
            "exec",
            id,
            "sh",
            "-c",
            "mkdir /pbps2513; seq 1 60000 | xargs -n 1000 sh -c 'touch \"$@\"' sh",
        ]);
        remover = Some(
            Command::new("docker")
                .args(["rm", "--force", "--volumes", id])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let output = Command::new("docker")
                .args(["container", "inspect", "--format", "{{json .}}", id])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "the removing observation must precede absence: {output:?}"
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let observed: serde_json::Value = serde_json::from_str(&text).unwrap();
            if observed["State"]["Status"] == "removing" {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the other remover never started"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    let output = remover.unwrap().wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!exists(&name), "{report:?}");
    assert!(volumes.iter().all(|v| !volume_exists(v)), "{volumes:?}");
    assert!(removed(&report, &name), "{report:?}");
    assert!(kept_why(&report, &name).is_empty(), "{report:?}");
    assert!(report.render().unwrap().contains("confirmed removal of"));
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn recovery_keeps_unconfirmed_volumes_named_after_the_container_is_gone() {
    // Drop the containers before this last-resort anonymous-volume cleanup.
    struct Volume(Option<String>);
    impl Drop for Volume {
        fn drop(&mut self) {
            if let Some(name) = &self.0 {
                let _ = Command::new("docker").args(["volume", "rm", name]).output();
            }
        }
    }
    let mut cleanup = Volume(None);
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, connection) = start(&mut made, &dead).await;
    let volumes = volumes_of(&name);
    let volume = volumes.first().expect("the pinned image has a data volume");
    cleanup.0 = Some(volume.clone());
    let holder = format!("pbps2513-volume-holder-{}", token());
    docker(&[
        "run",
        "--detach",
        "--pull",
        "never",
        "--name",
        &holder,
        "--mount",
        &format!("type=volume,src={volume},dst=/held"),
        "--entrypoint",
        "sleep",
        &image(),
        "600",
    ]);
    made.0.push(holder);
    drop(
        Keeper::open(Driver::Postgres, &connection, &dead)
            .await
            .unwrap(),
    );
    let report = swept_alone(&token(), &dead, GRACE).await;
    assert!(!exists(&name), "{report:?}");
    assert!(
        volume_exists(volume),
        "the other owned container still holds it"
    );
    assert!(
        !removed(&report, &name),
        "container absence alone is not recovery: {report:?}"
    );
    assert!(
        kept_why(&report, &name)
            .iter()
            .any(|why| why.contains(volume)),
        "{report:?}"
    );
}
