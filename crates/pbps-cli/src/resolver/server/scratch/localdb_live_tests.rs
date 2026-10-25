//! Windows LocalDB as the other-instance route (#2271), through the shared
//! producer.
//!
//! These run only in the owned Windows LocalDB job (`scripts/localdb_qualify.py`),
//! as the dedicated ordinary Windows user, against two private instances built
//! from one installer. Nothing here is measured anywhere else: Linux has no
//! LocalDB. The wider SQL Server resolver suites run against the same two
//! instances in that job; these cases are the ones that only exist because the
//! scratch is reached by discovery over a pipe.
//!
//! Environment (all set by the job):
//! - `PBPS_TEST_DB` / `PBPS_TEST_MSSQL_SCRATCH_DB`: the target and scratch
//!   instances as already-resolved pipe strings, which the shared suites read;
//! - `PBPS_TEST_LOCALDB_SCRATCH`: the scratch instance as a `(localdb)`
//!   selector, which these cases give the producer unresolved;
//! - `PBPS_TEST_LOCALDB_DISPOSABLE`: a third instance, as a selector, that the
//!   cases that stop and restart an instance may disturb. The two above never
//!   restart, so case order cannot matter.

use super::super::{Error, ProduceError};
use super::mssql_live_tests::{
    connection, create_database, drop_database, provenance, refusal, refusal_policy_inner, server,
};
use pbps_db::{Conn, DbError, Driver, LocalDbSelector};
use pbps_mssql::resolver::localdb::{self, Failure};
use pbps_mssql::resolver::scratch as sql;
use std::time::{Duration, Instant};

fn helper() -> std::path::PathBuf {
    localdb::helper().expect("ProgramFiles names the LocalDB install")
}

/// The instance a selector names.
fn instance(selector: &str) -> String {
    LocalDbSelector::read(selector)
        .expect("a supported selector")
        .expect("a `(localdb)` selector")
        .instance()
        .to_owned()
}

/// `SqlLocalDB.exe <verb> <instance> [flags]` as the dedicated user, the way
/// an operator provides and disturbs an instance.
fn operate(verb: &str, name: &str, flags: &[&str]) {
    let output = std::process::Command::new(helper())
        .arg(verb)
        .arg(name)
        .args(flags)
        .output()
        .expect("SqlLocalDB.exe runs");
    assert!(
        output.status.success(),
        "SqlLocalDB.exe {verb} {name}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

async fn resolved(selector: &str) -> localdb::Resolved {
    localdb::resolve(selector, Some(&helper()), pbps_db::CONNECT_TIMEOUT)
        .await
        .expect("discovery")
        .expect("a selector")
}

async fn scalar(conn: &mut Conn, sql: &str) -> String {
    conn.query(sql).await.unwrap()[0]
        .try_get_at::<&str>(0)
        .unwrap()
        .unwrap_or_default()
        .to_owned()
}

/// What an instance says about itself, to be compared between the two.
async fn facts(conn: &mut Conn) -> Vec<String> {
    let mut facts = Vec::new();
    for query in [
        "SELECT CONVERT(nvarchar(64),SERVERPROPERTY('IsLocalDB'))",
        "SELECT CONVERT(nvarchar(64),SERVERPROPERTY('ProductVersion'))",
        "SELECT CONVERT(nvarchar(64),SERVERPROPERTY('ProductMajorVersion'))",
        "SELECT CONVERT(nvarchar(128),SERVERPROPERTY('Collation'))",
        "SELECT CONVERT(nvarchar(128),SERVERPROPERTY('Edition'))",
    ] {
        facts.push(scalar(conn, query).await);
    }
    facts
}

#[tokio::test]
#[ignore = "needs the owned Windows LocalDB job"]
async fn localdb2271_two_instances_are_distinct_live_instances_of_one_build_run_as_the_dedicated_user()
 {
    let target_url = server("PBPS_TEST_DB");
    let scratch_url = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let found = resolved(&server("PBPS_TEST_LOCALDB_SCRATCH")).await;
    // Discovery found the instance the shared suites were pointed at, so the
    // selector route and the resolved-string route are one instance.
    assert!(
        found
            .connection()
            .to_lowercase()
            .contains(&found.pipe().to_lowercase()),
        "the rewritten string names the discovered pipe"
    );
    let mut target = connection(&target_url).await;
    let mut scratch = connection(found.connection()).await;
    let mut again = connection(&scratch_url).await;
    let (target_identity, scratch_identity) = (
        sql::cluster_identity(&mut target).await.unwrap(),
        sql::cluster_identity(&mut scratch).await.unwrap(),
    );
    assert_ne!(
        target_identity, scratch_identity,
        "two LocalDB instances must be two clusters"
    );
    assert_eq!(
        sql::cluster_identity(&mut again).await.unwrap(),
        scratch_identity,
        "the selector and the resolved string reach one instance"
    );
    let (on_target, on_scratch) = (facts(&mut target).await, facts(&mut scratch).await);
    assert_eq!(on_target[0], "1", "the target is LocalDB: {on_target:?}");
    assert_eq!(
        on_target, on_scratch,
        "the instances report one build, collation and edition"
    );
    // Integrated authentication ran as the current Windows user and not as a
    // SQL login of the same name.
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap(),
        std::env::var("USERNAME").unwrap()
    );
    for conn in [&mut target, &mut scratch] {
        assert_eq!(
            scalar(conn, "SELECT SUSER_SNAME()").await.to_lowercase(),
            user.to_lowercase()
        );
        let scheme = scalar(
            conn,
            "SELECT CONVERT(nvarchar(16),auth_scheme) FROM sys.dm_exec_connections \
             WHERE session_id=@@SPID",
        )
        .await;
        assert!(["NTLM", "KERBEROS"].contains(&scheme.as_str()), "{scheme}");
    }
}

#[tokio::test]
#[ignore = "needs the owned Windows LocalDB job"]
async fn localdb2271_a_selector_run_reaches_reconstruction_and_leaves_both_instances_clean() {
    let target = server("PBPS_TEST_DB");
    let selector = server("PBPS_TEST_LOCALDB_SCRATCH");
    let scratch_url = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let inventory = async |url: &str| {
        connection(url)
            .await
            .query(
                "SELECT name FROM sys.databases WHERE name LIKE N'pbps_scratch_%' UNION ALL \
                 SELECT name FROM sys.server_principals WHERE name LIKE N'pbps_run_%' ORDER \
                 BY name;",
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let (before_target, before_scratch) = (inventory(&target).await, inventory(&scratch_url).await);
    // Which layout the real privileges select is a finding of this job.
    let mut admin = connection(&scratch_url).await;
    println!(
        "LocalDB scratch layout: {}",
        if sql::provisions(&mut admin).await.unwrap() {
            "run-owned (ALTER ANY LOGIN and CREATE ANY DATABASE are held)"
        } else {
            "supplied"
        }
    );
    let outcome = refusal(&target, &selector).await;
    assert!(
        matches!(outcome, ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2238")),
        "the run reaches the reconstruction boundary it reaches on every other instance: {outcome}"
    );
    assert_eq!(inventory(&target).await, before_target);
    assert_eq!(inventory(&scratch_url).await, before_scratch);
}

#[tokio::test]
#[ignore = "needs the owned Windows LocalDB job"]
async fn localdb2271_the_marker_separates_instances_and_same_instance_controls_hold() {
    let target = server("PBPS_TEST_DB");
    let selector = server("PBPS_TEST_LOCALDB_SCRATCH");
    let found = resolved(&selector).await;
    let mut on_target = connection(&target).await;
    let mut on_scratch = connection(found.connection()).await;
    let target_database = sql::database(&mut on_target).await.unwrap();
    let mark = sql::mark(&mut on_scratch, &format!("{:032x}", rand::random::<u128>()))
        .await
        .unwrap();
    assert!(sql::marked(&mut on_scratch, &mark).await.unwrap());
    assert!(
        !sql::marked(&mut on_target, &mark).await.unwrap(),
        "a marker on one LocalDB instance must not be visible on the other"
    );
    assert_eq!(
        sql::placement(&target_database.id, &mark, false),
        sql::Placement::SeparateInstance
    );
    // Same-instance controls, on the target's own instance.
    let (name, on) = create_database(&target).await;
    let itself = refusal(&on, &on).await;
    let policy = refusal_policy_inner(&target, &on, Some(""), &provenance())
        .await
        .to_string();
    let allowed = refusal_policy_inner(
        &target,
        &on,
        Some(""),
        &super::Provenance {
            same_instance: pbps_model::resolver::SameInstance::Allowed,
            ..provenance()
        },
    )
    .await;
    drop_database(&target, &name).await;
    assert!(
        matches!(itself, ProduceError::Run(Error::Unusable(ref why)) if why.contains("target database")),
        "{itself}"
    );
    assert!(policy.contains("same_instance: allow"), "{policy}");
    assert!(
        matches!(allowed, ProduceError::Run(Error::Binding(ref why)) if why.contains("#2238")),
        "{allowed}"
    );
}

#[tokio::test]
#[ignore = "needs the owned Windows LocalDB job"]
async fn localdb2271_a_missing_or_stopped_instance_fails_by_name_within_the_bound() {
    let target = server("PBPS_TEST_DB");
    let disposable = server("PBPS_TEST_LOCALDB_DISPOSABLE");
    let missing = r"Server=(localdb)\pbps_never_created;Integrated Security=SSPI";
    let name = instance(&disposable);
    operate("stop", &name, &["-k"]);
    let mut target_session = connection(&target).await;
    let identity = sql::cluster_identity(&mut target_session).await.unwrap();
    for (selector, expected) in [
        (missing, "pbps_never_created"),
        (disposable.as_str(), "no named pipe"),
    ] {
        let started = Instant::now();
        let failure = localdb::resolve(selector, Some(&helper()), pbps_db::CONNECT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(started.elapsed() < pbps_db::CONNECT_TIMEOUT, "{failure}");
        match (&failure, expected) {
            (Failure::Exit(_), _) | (Failure::NoUsablePipe, "no named pipe") => {}
            _ => panic!("unexpected discovery failure for {selector}: {failure:?}"),
        }
        // Through the producer it is an acquire failure, the target is
        // untouched, and nothing was opened over TCP.
        let outcome = refusal(&target, selector).await;
        assert!(matches!(outcome, ProduceError::Acquire(_)), "{outcome}");
        assert!(
            outcome.to_string().contains("LocalDB instance"),
            "{outcome}"
        );
        assert_eq!(
            sql::cluster_identity(&mut target_session).await.unwrap(),
            identity
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned Windows LocalDB job"]
async fn localdb2271_a_restart_invalidates_the_old_run_and_a_fresh_run_requalifies() {
    let target = server("PBPS_TEST_DB");
    let disposable = server("PBPS_TEST_LOCALDB_DISPOSABLE");
    let name = instance(&disposable);
    operate("start", &name, &[]);
    let before = resolved(&disposable).await;
    let mut admin = connection(before.connection()).await;
    let mark = sql::mark(&mut admin, &format!("{:032x}", rand::random::<u128>()))
        .await
        .unwrap();
    assert!(sql::marked(&mut admin, &mark).await.unwrap());
    let identity = sql::cluster_identity(&mut admin).await.unwrap();

    operate("stop", &name, &["-k"]);
    // The pipe is gone: opening the old string ends at the budget or sooner,
    // as a connect failure, never as a retry on another transport.
    let started = Instant::now();
    let gone: Result<Conn, DbError> = Conn::connect(Driver::Mssql, before.connection()).await;
    assert!(started.elapsed() < pbps_db::CONNECT_TIMEOUT + Duration::from_secs(5));
    match gone {
        Err(DbError::Connect { .. } | DbError::ConnectTimeout { .. }) => {}
        Err(other) => panic!("a gone pipe is a connect failure: {other}"),
        Ok(_) => panic!("the stopped instance accepted a connection"),
    }
    // The session that held the marker is not a session of anything now.
    assert!(admin.query("SELECT 1").await.is_err());
    drop(admin);

    operate("start", &name, &[]);
    let after = resolved(&disposable).await;
    let mut fresh = connection(after.connection()).await;
    assert_eq!(
        sql::cluster_identity(&mut fresh).await.unwrap(),
        identity,
        "a restart is the same instance with the same databases"
    );
    assert!(
        !sql::marked(&mut fresh, &mark).await.unwrap(),
        "tempdb, and with it the marker, does not survive a restart; the old run \
         cannot continue on the restarted instance"
    );
    drop(fresh);

    // A new invocation discovers the instance again and qualifies it anew.
    let outcome = refusal(&target, &disposable).await;
    assert!(
        matches!(outcome, ProduceError::Run(Error::Binding(ref why)) if why.contains("#2238")),
        "{outcome}"
    );
}

#[tokio::test]
#[ignore = "needs the owned Windows LocalDB job"]
async fn localdb2271_the_requested_certificate_policy_is_kept_over_the_pipe() {
    let found = resolved(&server("PBPS_TEST_LOCALDB_SCRATCH")).await;
    let trusted = found.connection().to_owned();
    // The job's strings ask to trust the instance's self-signed certificate.
    // Asking for encryption without that trust must not connect: the pipe
    // does not change what the string requested.
    let strict = {
        let selector = LocalDbSelector::read(&format!(
            "Server=(localdb)\\{};Integrated Security=SSPI;Encrypt=true",
            found.instance()
        ))
        .unwrap()
        .unwrap();
        selector.with_pipe(found.pipe()).unwrap()
    };
    assert!(
        Conn::connect(Driver::Mssql, &trusted).await.is_ok(),
        "the trusting string connects"
    );
    let refused = Conn::connect(Driver::Mssql, &strict).await;
    assert!(
        refused.is_err(),
        "encryption without trust of a self-signed certificate must be refused, not downgraded"
    );
}
