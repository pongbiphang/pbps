//! Crash recovery of a Docker scratch (#2410, DEC-2410.3), on the pinned
//! PostgreSQL image. A killed run leaves what it made: a running container,
//! named for its token and labelled, with or without the session its run kept
//! in the server. These tests make that state by hand with the names and
//! labels a run gives it; the real SIGKILLs of a `pbps plan` are in
//! `tests/flow_pg.rs`. The grace for a container that never had a keeper is
//! four seconds here, not five minutes.

use super::super::container::{Keeper, swept};
use super::setting;
use pbps_db::{Conn, Driver};
use std::process::Command;
use std::time::Duration;

/// The grace the tests run under (`container::recovery::GRACE`).
const GRACE: Duration = Duration::from_secs(4);

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

fn removed_names(report: &super::super::recovery::Report) -> Vec<String> {
    report
        .groups
        .iter()
        .flat_map(|group| group.removed.iter().flat_map(|r| r.names.clone()))
        .collect()
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
    assert_eq!(
        removed_names(&report),
        std::slice::from_ref(&name),
        "{report:?}"
    );
    assert!(!still, "removed well inside the grace");
    assert_eq!(gone, 0, "with its volumes: {volumes:?}");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn a_container_that_never_connected_is_kept_inside_the_grace_and_removed_after_it() {
    let mut made = Made(Vec::new());
    let dead = token();
    let (name, _) = start(&mut made, &dead).await;
    let inside = swept(&token()).await;
    let kept = exists(&name);
    tokio::time::sleep(GRACE + Duration::from_millis(1500)).await;
    let after = swept(&token()).await;
    let gone = !exists(&name);
    assert!(kept, "a run that has not connected yet may still");
    assert!(
        inside.groups.is_empty() && inside.live == [dead.clone()],
        "{inside:?}"
    );
    assert_eq!(removed_names(&after), [name], "{after:?}");
    assert!(gone);
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
    let report = swept(&token()).await;
    let kept = exists(&name);
    drop(keeper);
    assert!(kept, "its keeper is in the server");
    assert!(
        report.groups.is_empty() && report.live == [live],
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
    assert_eq!(
        removed_names(&report),
        std::slice::from_ref(&name),
        "{report:?}"
    );
    assert!(!exists(&name));
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
        "pbps-scratch-0123456789abcdef",
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
        report.groups.is_empty() && report.live.is_empty(),
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
    let group = &report.groups[0];
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
