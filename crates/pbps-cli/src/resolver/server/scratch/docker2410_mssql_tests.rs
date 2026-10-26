//! Crash recovery of a Docker scratch on SQL Server (#2410, DEC-2410.3): the
//! run's session is its `##pbps_<token>` marker table, and the server keeps a
//! persistent mark that the session once connected. The container is made by
//! hand with the names and labels a run gives it.

use super::super::container::{Keeper, seen, swept};
use super::*;
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

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures and a container runtime"]
async fn a_sql_server_container_is_alive_while_its_keeper_is_and_removed_at_once_when_it_is_gone() {
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
    let dead = swept(&token_of_run).await;
    let removed = !exists(&name);
    drop(made);
    assert!(
        kept && live.groups.is_empty() && live.live == [token.clone()],
        "{live:?}"
    );
    assert!(gone, "the server forgot the session");
    assert_eq!(held, Some((true, true)));
    assert_eq!(after, Some((false, true)));
    assert!(
        removed
            && dead
                .groups
                .iter()
                .any(|g| g.removed.iter().any(|r| r.names == [name.clone()])),
        "{dead:?}"
    );
}
