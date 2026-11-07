//! Crash recovery of a Docker scratch on SQL Server (#2410, DEC-2410.3): the
//! run's session is its `##pbps_<token>` marker table, and the server keeps a
//! persistent mark that the session once connected. The container is made by
//! hand with the names and labels a run gives it.

use super::super::container::{Keeper, seen, swept};
use super::super::recovery::{self, Report};
use super::*;
use crate::resolver::server::engine;
use pbps_db::resolver::Kept;
use std::process::Command;
use std::time::Duration;

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

struct Made(String);

impl Drop for Made {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", "--volumes", &self.0])
            .output();
    }
}

/// Starts a SQL Server container the way a run does: named for its token,
/// labelled with it and its engine, the password in its environment, one
/// loopback port published. Returns its guard, its token and a connection.
async fn start() -> (Made, String, String, String) {
    let image = server("PBPS_TEST_RESOLVER_MSSQL_IMAGE");
    let token = format!("{:032x}", rand::random::<u128>());
    let name = format!("pbps-scratch-{token}");
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
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
        "pbps.resolver.engine=mssql",
        "--env",
        "ACCEPT_EULA=Y",
        "--env",
        &format!("MSSQL_SA_PASSWORD={password}"),
        "--publish",
        "127.0.0.1::1433",
        &image,
    ]);
    let made = Made(name.clone());
    let port = docker(&["port", &name, "1433/tcp"]);
    let port = port.strip_prefix("127.0.0.1:").unwrap().to_owned();
    let connection = format!(
        "Server=127.0.0.1,{port};Database=master;User Id=sa;Password={password};TrustServerCertificate=true"
    );
    let mut ready = false;
    for _ in 0..180 {
        if let Ok(mut conn) = Conn::connect(Driver::Mssql, &connection).await
            && conn.query("SELECT 1 AS ok;").await.is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(ready, "the container never accepted a connection");
    (made, name, token, connection)
}

// Readiness cannot prevent a later TLS handshake from failing. The production
// sweep correctly keeps an unreadable server (DEC-2410.3); this test gets at
// most three cleanup observations, retaining the last diagnostic (#2689).
async fn retry_unreadable_cleanup<F: std::future::Future<Output = Report>>(
    name: &str,
    mut sweep: impl FnMut() -> F,
) -> Report {
    let mut report = sweep().await;
    for _ in 0..2 {
        let unreadable = report.groups.iter().any(|group| {
            group.kept.iter().any(|kept| {
                kept.resource.names == [name]
                    && kept.why.starts_with("its server could not be read: ")
            })
        });
        if !unreadable {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        report = sweep().await;
    }
    report
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures and a container runtime"]
async fn a_sql_server_container_is_alive_while_its_keeper_is_and_removed_at_once_when_it_is_gone() {
    let (made, name, token, connection) = start().await;
    let token_of_run = format!("{:032x}", rand::random::<u128>());
    let keeper = Keeper::open(Driver::Mssql, &connection, &token)
        .await
        .unwrap();
    // While the session is there, and the server has the mark of it.
    let held = seen(Driver::Mssql, &connection, &token_of_run, &token).await;
    let live = swept(&token_of_run).await;
    let kept = exists(&name);
    drop(keeper);
    // The marker goes with the session; the persistent mark stays.
    let mut gone = false;
    for _ in 0..100 {
        let mut conn = Conn::connect(Driver::Mssql, &connection).await.unwrap();
        let rows = conn
            .query(&format!(
                "SELECT OBJECT_ID(N'tempdb..##pbps_{token}') AS marker;"
            ))
            .await
            .unwrap();
        if rows[0].try_get::<i32>("marker").unwrap().is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Gone, and the mark that it was there stays: that, not the age, is what
    // makes the container the dead run's however young it is.
    let after = seen(Driver::Mssql, &connection, &token_of_run, &token).await;
    let dead = retry_unreadable_cleanup(&name, || swept(&token_of_run)).await;
    let removed = !exists(&name);
    drop(made);
    // Other runs on the daemon are in the same reports: only this container
    // is asked about.
    let named = |report: &crate::resolver::server::scratch::recovery::Report| {
        report.groups.iter().any(|g| {
            g.removed.iter().any(|r| r.names == [name.clone()])
                || g.kept.iter().any(|k| k.resource.names == [name.clone()])
        })
    };
    assert!(
        kept && live.live.contains(&token) && !named(&live),
        "{live:?}"
    );
    assert!(gone, "the server forgot the session");
    assert_eq!(held, Some((true, true)));
    assert_eq!(after, Some((false, true)));
    // Another run's sweep may have removed it first: it is gone, and this
    // sweep did not keep it.
    assert!(
        removed
            && !dead
                .groups
                .iter()
                .any(|g| g.kept.iter().any(|k| k.resource.names == [name.clone()])),
        "{dead:?}"
    );
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures and a container runtime"]
async fn a_sql_server_container_stopped_after_a_running_observation_is_removed_at_once() {
    let (made, name, token, connection) = start().await;
    let keeper = Keeper::open(Driver::Mssql, &connection, &token)
        .await
        .unwrap();
    let own = format!("{:032x}", rand::random::<u128>());
    let (between, go) = engine::pause_between_reads(&token);
    let (report, ()) = tokio::join!(swept(&own), async {
        // The real SQL probe has connected and read the persistent mark.
        // Killing its engine now reproduces the I/O race before the live read.
        tokio::time::timeout(Duration::from_secs(30), between)
            .await
            .expect("the keeper probe reached its live read")
            .unwrap();
        docker(&["kill", &name]);
        assert_eq!(
            docker(&[
                "container",
                "inspect",
                "--format",
                "{{.State.Status}}",
                &name
            ]),
            "exited"
        );
        go.send(()).unwrap();
    });
    let removed = !exists(&name);
    drop(keeper);
    drop(made);
    let group = report.groups.iter().find(|group| group.token == token);
    assert!(removed, "{report:?}");
    let group = group.unwrap_or_else(|| panic!("no group for {token}: {report:?}"));
    assert!(group.kept.is_empty(), "{report:?}");
    assert!(group.removed.iter().any(|r| r.names == [name.clone()]));
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures and a container runtime"]
async fn a_sql_server_keeper_that_connects_between_the_two_reads_is_alive_and_not_yet_kept() {
    let (_made, _, run, connection) = start().await;
    let (between, go) = engine::pause_between_reads(&run);
    let reader = format!("{:032x}", rand::random::<u128>());
    let (seen, keeper) = tokio::join!(
        engine::keeper_seen(Driver::Mssql, &connection, &reader, &run),
        async {
            between.await.unwrap();
            let keeper = Keeper::open(Driver::Mssql, &connection, &run)
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

mod cleanup_retry_tests {
    use super::*;

    const NAME: &str = "pbps-scratch-0123456789abcdef0123456789abcdef";
    const EOF: &str = "its server could not be read: tls handshake eof";

    fn kept(name: &str, why: &str) -> Report {
        Report {
            groups: vec![recovery::Group {
                token: "owner".into(),
                kept: vec![recovery::Kept {
                    resource: recovery::Resource::one("docker container", name),
                    why: why.into(),
                    statement: format!("docker rm --force --volumes {name}"),
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn removed() -> Report {
        Report {
            groups: vec![recovery::Group {
                token: "owner".into(),
                removed: vec![recovery::Resource::one("docker container", NAME)],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_transient_server_read_failure_is_retried_until_removal_is_observed() {
        let mut reports = [kept(NAME, EOF), removed()].into_iter();
        let report = retry_unreadable_cleanup(NAME, || {
            std::future::ready(reports.next().expect("only the failed read is retried"))
        })
        .await;
        assert_eq!(report, removed());
        assert!(reports.next().is_none());
    }

    #[tokio::test]
    async fn an_unreadable_server_is_kept_after_three_attempts_with_the_last_diagnostic() {
        let mut attempts = 0;
        let report = retry_unreadable_cleanup(NAME, || {
            attempts += 1;
            std::future::ready(kept(
                NAME,
                &format!("its server could not be read: failure {attempts}"),
            ))
        })
        .await;
        assert_eq!(attempts, 3);
        assert_eq!(
            report,
            kept(NAME, "its server could not be read: failure 3")
        );
    }

    #[tokio::test]
    async fn a_live_keeper_ends_retries_without_removing_its_container() {
        let live = || Report {
            live: vec!["owner".into()],
            ..Default::default()
        };
        let mut reports = [kept(NAME, EOF), live()].into_iter();
        let report = retry_unreadable_cleanup(NAME, || {
            std::future::ready(reports.next().expect("a live keeper ends retries"))
        })
        .await;
        assert_eq!(report, live());
        assert!(reports.next().is_none());
    }

    #[tokio::test]
    async fn other_failures_and_other_containers_are_not_retried() {
        for (name, why) in [
            (NAME, "its volumes could not be read"),
            (NAME, "its server was not read"),
            ("another-container", EOF),
        ] {
            let mut attempts = 0;
            let report = retry_unreadable_cleanup(NAME, || {
                attempts += 1;
                std::future::ready(kept(name, why))
            })
            .await;
            assert_eq!(attempts, 1, "{name}: {why}");
            assert_eq!(report, kept(name, why));
        }
    }

    #[tokio::test]
    async fn an_unreadable_listing_is_not_retried() {
        let mut attempts = 0;
        let report = retry_unreadable_cleanup(NAME, || {
            attempts += 1;
            std::future::ready(Report {
                unread: Some("the daemon could not be read".into()),
                ..Default::default()
            })
        })
        .await;
        assert_eq!(attempts, 1);
        assert_eq!(
            report.unread.as_deref(),
            Some("the daemon could not be read")
        );
    }
}
