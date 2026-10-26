//! Crash recovery on the pinned PostgreSQL servers (#2410, DEC-2410.1).
//!
//! A run killed at a stage leaves exactly what the run would have made by
//! then. These tests make that state by hand, with the names a run gives it
//! and with no session carrying the run's token, which the engine shows
//! identically to a SIGKILLed client (the end-to-end kills are in
//! `tests/flow_pg.rs`). A live run is a session carrying its token on a
//! connection the test holds open.

use std::collections::BTreeSet;

use super::super::recovery;
use super::{Fixture, Inputs, ProjectKey, SCRATCH_SERVER, SERVERS, produce, resolver_refusal};
use crate::resolver::server::engine;
use pbps_db::{Conn, Driver};
use pbps_pg::resolver::recovery as record;
use pbps_pg::resolver::scratch as sql;
use pbps_pg::resolver::standard;

fn token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// How far a killed run got.
#[derive(Clone, Copy, Debug)]
enum Stage {
    /// Its login, the record, and nothing else.
    Record,
    /// And its database and run-local roles.
    Created,
    /// And objects in the database, with privileges granted to its roles.
    Used,
}

const STAGES: [Stage; 3] = [Stage::Record, Stage::Created, Stage::Used];

/// What a run-owned run killed at `stage` leaves in `scratch`'s cluster, and
/// the names of it. Recorded in the fixture first, so a failing test still
/// cleans up.
async fn leave(scratch: &mut Fixture, token: &str, stage: Stage) -> Vec<String> {
    let login = format!("pbps_run_{token}");
    let database = format!("pbps_scratch_{token}");
    let roles = [
        format!("pbps_role_0_{token}"),
        format!("pbps_role_1_{token}"),
        format!("pbps_rehearsal_0_{token}"),
    ];
    let mut admin = scratch.admin().await;
    admin
        .execute(&format!(
            "CREATE ROLE \"{login}\" LOGIN PASSWORD '{}' NOSUPERUSER NOCREATEDB NOCREATEROLE",
            token
        ))
        .await
        .unwrap();
    scratch.roles.push(login.clone());
    let mut names = vec![login.clone()];
    if matches!(stage, Stage::Record) {
        return names;
    }
    scratch.databases.push(database.clone());
    admin
        .execute(&format!(
            "CREATE DATABASE \"{database}\" TEMPLATE template0"
        ))
        .await
        .unwrap();
    admin
        .execute(&format!(
            "ALTER DATABASE \"{database}\" OWNER TO \"{login}\""
        ))
        .await
        .unwrap();
    names.push(database.clone());
    for role in &roles {
        admin
            .execute(&format!("CREATE ROLE \"{role}\" NOLOGIN"))
            .await
            .unwrap();
        scratch.roles.push(role.clone());
        names.push(role.clone());
    }
    if matches!(stage, Stage::Used) {
        // A privilege a role holds in the database: `DROP ROLE` refuses until
        // the database is gone, so the order of removal is what is tested.
        scratch
            .run(
                &database,
                &[
                    "CREATE SCHEMA s",
                    "CREATE TABLE s.t (id integer)",
                    &format!("GRANT SELECT ON s.t TO \"{}\"", roles[0]),
                    &format!("GRANT USAGE ON SCHEMA s TO \"{}\"", roles[1]),
                    &format!("GRANT USAGE ON SCHEMA s TO \"{}\"", roles[2]),
                ],
            )
            .await;
    }
    names
}

/// A session that carries `token`, as a live run's does, on a database of
/// the cluster other than the one a sweep connects to.
async fn alive(scratch: &Fixture, token: &str) -> Conn {
    Conn::connect_named(
        Driver::Postgres,
        &scratch.on("postgres"),
        &format!("pbps-{token}"),
    )
    .await
    .unwrap()
}

/// Waits until the server no longer shows a session carrying `token`.
async fn gone(admin: &mut Conn, token: &str) {
    for _ in 0..100 {
        let rows = admin
            .query(&format!(
                "SELECT 1 FROM pg_catalog.pg_stat_activity WHERE application_name = 'pbps-{token}'"
            ))
            .await
            .unwrap();
        if rows.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the server still shows a session for {token}");
}

fn without(inventory: &(Vec<String>, Vec<String>), names: &[String]) -> (Vec<String>, Vec<String>) {
    let keep = |all: &Vec<String>| {
        all.iter()
            .filter(|name| !names.contains(name))
            .cloned()
            .collect()
    };
    (keep(&inventory.0), keep(&inventory.1))
}

async fn run(scratch: &Fixture, target: &Fixture, target_db: &str) -> String {
    let result = produce(
        &scratch.server,
        &target.on(target_db),
        &Inputs::overload(),
        &ProjectKey::new(true),
    )
    .await;
    match result {
        Ok(_) => String::new(),
        Err(error) => resolver_refusal(Err(error)),
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_run_killed_at_any_stage_is_removed_by_the_next_run() {
    for stage in STAGES {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let mut scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = target.target().await;
        let dead = token();
        let names = leave(&mut scratch, &dead, stage).await;
        let before = scratch.inventory().await;
        recovery::take();
        let refusal = run(&scratch, &target, &target_db).await;
        let collected = recovery::take();
        let after = scratch.inventory().await;
        target.drop().await;
        scratch.drop().await;
        assert_eq!(refusal, "", "{stage:?}: the next run answers");
        assert_eq!(
            after,
            without(&before, &names),
            "{stage:?}: exactly the dead run's resources are gone"
        );
        let group = collected
            .groups
            .iter()
            .find(|group| group.token == dead)
            .unwrap_or_else(|| panic!("{stage:?}: the removal is reported: {collected:?}"));
        assert!(group.kept.is_empty(), "{stage:?}: {group:?}");
        let removed: BTreeSet<_> = group
            .removed
            .iter()
            .flat_map(|resource| resource.names.iter().cloned())
            .collect();
        assert_eq!(removed, names.iter().cloned().collect(), "{stage:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_sweep_removes_a_dead_run_on_each_server() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let dead = token();
        let own = token();
        let names = leave(&mut fixture, &dead, Stage::Used).await;
        let before = fixture.inventory().await;
        let mut admin =
            Conn::connect_named(Driver::Postgres, &fixture.server, &format!("pbps-{own}"))
                .await
                .unwrap();
        let report = engine::sweep_cluster_on(&mut admin, &own).await;
        let after = fixture.inventory().await;
        drop(admin);
        fixture.drop().await;
        assert_eq!(report.unread, None, "{server}");
        assert_eq!(after, without(&before, &names), "{server}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_live_runs_resources_are_never_touched_by_another_runs_sweep() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let (live, dead) = (token(), token());
    // Its session is on another database of the cluster than the sweep's:
    // an advisory lock would read free there, the session list does not.
    let session = alive(&scratch, &live).await;
    let live_names = leave(&mut scratch, &live, Stage::Used).await;
    let dead_names = leave(&mut scratch, &dead, Stage::Used).await;
    let before = scratch.inventory().await;
    let first = run(&scratch, &target, &target_db).await;
    let during = scratch.inventory().await;
    let mut admin = scratch.admin().await;
    // The owner goes: the same state is now a dead run's.
    drop(session);
    gone(&mut admin, &live).await;
    let second = run(&scratch, &target, &target_db).await;
    let after = scratch.inventory().await;
    drop(admin);
    target.drop().await;
    scratch.drop().await;
    assert_eq!((first.as_str(), second.as_str()), ("", ""));
    assert_eq!(
        during,
        without(&before, &dead_names),
        "only the dead run's resources went while the other lived"
    );
    assert_eq!(
        after,
        without(&during, &live_names),
        "and when its owner was gone, its own went"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_name_that_is_not_a_complete_run_is_never_touched() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let t = token();
    let short = &t[..16];
    let mut admin = scratch.admin().await;
    // A database and roles with no login of their token: nothing says a run
    // left them unfinished. The older shape (16 digits) and upper case
    // digits, which no run gives.
    let orphans = [
        format!("pbps_scratch_{t}"),
        format!("pbps_scratch_{short}"),
        format!("pbps_scratch_{}", t.to_uppercase()),
    ];
    for name in &orphans {
        admin
            .execute(&format!("CREATE DATABASE \"{name}\" TEMPLATE template0"))
            .await
            .unwrap();
        scratch.databases.push(name.clone());
    }
    let roles = [
        format!("pbps_role_0_{t}"),
        format!("pbps_run_{short}"),
        format!("pbps_run_{}", t.to_uppercase()),
    ];
    for name in &roles {
        admin
            .execute(&format!("CREATE ROLE \"{name}\" NOLOGIN"))
            .await
            .unwrap();
        scratch.roles.push(name.clone());
    }
    let before = scratch.inventory().await;
    let refusal = run(&scratch, &target, &target_db).await;
    let after = scratch.inventory().await;
    // The fixture drops by unquoted name, which folds upper case away.
    for name in &orphans {
        admin
            .execute(&format!("DROP DATABASE IF EXISTS \"{name}\""))
            .await
            .unwrap();
    }
    for name in &roles {
        admin
            .execute(&format!("DROP ROLE IF EXISTS \"{name}\""))
            .await
            .unwrap();
    }
    drop(admin);
    target.drop().await;
    scratch.drop().await;
    assert_eq!(refusal, "");
    assert_eq!(after, before, "nothing that is not a run's was touched");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_database_a_dead_run_still_has_a_session_on_is_kept_and_named() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let dead = token();
    let names = leave(&mut scratch, &dead, Stage::Used).await;
    // The run's connection to its database, which the server has not yet
    // noticed is orphaned: not the keeper (that is gone), so the run reads as
    // dead, and the engine itself refuses to drop a database in use.
    let zombie = Conn::connect(
        Driver::Postgres,
        &scratch.on(&format!("pbps_scratch_{dead}")),
    )
    .await
    .unwrap();
    let before = scratch.inventory().await;
    recovery::take();
    let refusal = run(&scratch, &target, &target_db).await;
    let collected = recovery::take();
    let kept = scratch.inventory().await;
    drop(zombie);
    let refusal_after = run(&scratch, &target, &target_db).await;
    let after = scratch.inventory().await;
    target.drop().await;
    scratch.drop().await;
    assert_eq!((refusal.as_str(), refusal_after.as_str()), ("", ""));
    assert_eq!(
        kept, before,
        "nothing of the run was removed while it was in use"
    );
    let group = &collected.groups[0];
    assert_eq!(group.token, dead);
    let database = group
        .kept
        .iter()
        .find(|kept| kept.resource.kind == "database")
        .unwrap();
    assert_eq!(
        database.statement,
        format!("DROP DATABASE IF EXISTS \"pbps_scratch_{dead}\";")
    );
    assert!(database.why.contains("still connected"), "{}", database.why);
    assert!(
        group.kept.iter().any(|kept| kept.resource.kind == "login"),
        "the record stays while anything it names does: {group:?}"
    );
    assert!(group.removed.is_empty(), "{group:?}");
    assert_eq!(after, without(&before, &names), "the next run removed it");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_leftover_that_cannot_be_removed_is_named_with_the_statement_and_keeps_its_record() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let dead = token();
    let names = leave(&mut scratch, &dead, Stage::Created).await;
    // A privilege in a database that is not the run's: `DROP ROLE` refuses.
    let other = scratch.database("o", None).await;
    let stuck = format!("pbps_role_5_{dead}");
    let mut admin = scratch.admin().await;
    admin
        .execute(&format!("CREATE ROLE \"{stuck}\" NOLOGIN"))
        .await
        .unwrap();
    scratch.roles.push(stuck.clone());
    scratch
        .run(
            &other,
            &[
                "CREATE TABLE public.held (id integer)",
                &format!("GRANT SELECT ON public.held TO \"{stuck}\""),
            ],
        )
        .await;
    let before = scratch.inventory().await;
    recovery::take();
    let refusal = run(&scratch, &target, &target_db).await;
    let collected = recovery::take();
    let after = scratch.inventory().await;
    drop(admin);
    target.drop().await;
    scratch.drop().await;
    assert_eq!(refusal, "", "the leftover does not stop an unrelated run");
    let group = &collected.groups[0];
    let kept: Vec<_> = group
        .kept
        .iter()
        .map(|kept| {
            (
                kept.resource.kind,
                kept.resource.names.clone(),
                kept.statement.clone(),
            )
        })
        .collect();
    assert_eq!(
        kept[0],
        (
            "role",
            vec![stuck.clone()],
            format!("DROP ROLE IF EXISTS \"{stuck}\";")
        ),
        "{group:?}"
    );
    assert_eq!(kept[1].0, "login", "its record is kept: {group:?}");
    let removed: BTreeSet<_> = group.removed.iter().flat_map(|r| r.names.clone()).collect();
    assert!(
        removed.contains(&format!("pbps_scratch_{dead}")),
        "{group:?}"
    );
    // What went is the database and the other roles; the stuck role and the
    // login stay for the next sweep to find.
    let gone: BTreeSet<_> = names
        .iter()
        .filter(|name| **name != format!("pbps_run_{dead}"))
        .cloned()
        .collect();
    assert_eq!(
        after,
        without(&before, &gone.into_iter().collect::<Vec<_>>())
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn the_keeper_is_named_by_its_token_and_keeps_its_settings() {
    for server in SERVERS {
        let fixture = Fixture::new(server);
        let tokens = super::super::Tokens::generate();
        let mut keeper = engine::connect_scratch(Driver::Postgres, &fixture.server, &tokens)
            .await
            .unwrap();
        let show = async |conn: &mut Conn, setting: &str| {
            let rows = conn.query(&format!("SHOW {setting}")).await.unwrap();
            rows[0]
                .try_get::<&str>(setting)
                .unwrap()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            show(&mut keeper, "application_name").await,
            format!("pbps-{}", tokens.scratch),
            "{server}"
        );
        assert_eq!(show(&mut keeper, "tcp_keepalives_idle").await, "30");
        assert_eq!(show(&mut keeper, "tcp_keepalives_interval").await, "10");
        assert_eq!(show(&mut keeper, "tcp_keepalives_count").await, "3");
        assert_eq!(
            show(&mut keeper, "client_connection_check_interval").await,
            "10s"
        );
        // A reset takes the settings and leaves the name, which is why the
        // sessions that run one send the settings again.
        keeper.execute("DISCARD ALL").await.unwrap();
        assert_eq!(
            show(&mut keeper, "application_name").await,
            format!("pbps-{}", tokens.scratch),
            "{server}: the name is a startup parameter"
        );
        assert_ne!(show(&mut keeper, "tcp_keepalives_idle").await, "30");
        sql::keep(&mut keeper).await.unwrap();
        assert_eq!(show(&mut keeper, "tcp_keepalives_idle").await, "30");
        // It is seen by the sessions of the cluster, from any database, and
        // gone with the connection. A reader whose own token is not among
        // the sessions it reads proves nothing, and says so.
        let own = token();
        let mut me = Conn::connect_named(
            Driver::Postgres,
            &fixture.on("postgres"),
            &format!("pbps-{own}"),
        )
        .await
        .unwrap();
        let blind = record::alive(&mut me, &token(), std::slice::from_ref(&tokens.scratch))
            .await
            .unwrap();
        assert!(blind.is_none(), "{server}");
        let up = record::alive(&mut me, &own, std::slice::from_ref(&tokens.scratch))
            .await
            .unwrap()
            .unwrap();
        assert!(up.contains(&tokens.scratch), "{server}: the keeper is seen");
        drop(keeper);
        gone(&mut me, &tokens.scratch).await;
        let down = record::alive(&mut me, &own, std::slice::from_ref(&tokens.scratch))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !down.contains(&tokens.scratch),
            "{server}: and gone with it"
        );
    }
}

/// A supplied database a run killed after its claim left: made as the
/// confined login does, with the state the claim recorded.
async fn leave_claimed(
    fixture: &Fixture,
    scratch_db: &str,
    login: &(String, String),
    token: &str,
) -> Conn {
    let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(scratch_db, login))
        .await
        .unwrap();
    let defaults = sql::login_defaults(&mut conn).await.unwrap();
    let acl = standard::read_state(&mut conn).await.unwrap().acl;
    record::claim(&mut conn, &record::Claim::new(token, defaults, acl))
        .await
        .unwrap();
    conn
}

async fn schema_exists(fixture: &Fixture, database: &str, schema: &str) -> bool {
    let mut conn = Conn::connect(Driver::Postgres, &fixture.on(database))
        .await
        .unwrap();
    !conn
        .query(&format!(
            "SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = '{schema}'"
        ))
        .await
        .unwrap()
        .is_empty()
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_supplied_run_killed_after_its_claim_is_cleaned_by_the_next_run() {
    for server in SERVERS {
        for stage in ["claim only", "objects", "the database's privileges changed"] {
            let mut fixture = Fixture::new(server);
            let target_db = fixture.target().await;
            let (login, scratch_db) = fixture.confined().await;
            let dead = token();
            let mut conn = leave_claimed(&fixture, &scratch_db, &login, &dead).await;
            let grantee = fixture.role("g").await;
            match stage {
                "objects" => {
                    conn.execute("CREATE SCHEMA left_over").await.unwrap();
                    conn.execute("CREATE TABLE left_over.t (id integer)")
                        .await
                        .unwrap();
                }
                "the database's privileges changed" => {
                    conn.execute(&format!(
                        "GRANT CREATE ON DATABASE {scratch_db} TO {grantee}"
                    ))
                    .await
                    .unwrap();
                }
                _ => {}
            }
            // The run's connection is gone: the login holds one at a time.
            drop(conn);
            recovery::take();
            let result = produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &Inputs::overload(),
                &ProjectKey::new(true),
            )
            .await;
            let collected = recovery::take();
            let left = fixture.foreign_objects(&scratch_db).await;
            let can_create = fixture
                .admin()
                .await
                .query(&format!(
                    "SELECT has_database_privilege('{grantee}', '{scratch_db}', 'CREATE') AS can"
                ))
                .await
                .unwrap()[0]
                .try_get::<bool>("can")
                .unwrap()
                .unwrap();
            fixture.drop().await;
            assert!(result.is_ok(), "{server}, {stage}: {:?}", result.err());
            assert_eq!(left.1, 0, "{server}, {stage}: {left:?}");
            assert!(
                !can_create,
                "{server}, {stage}: the privileges are as it found them"
            );
            assert_eq!(
                collected.groups.len(),
                1,
                "{server}, {stage}: {collected:?}"
            );
            assert_eq!(collected.groups[0].token, dead);
            assert!(collected.groups[0].kept.is_empty(), "{server}, {stage}");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_supplied_database_claimed_by_a_live_run_is_refused_and_left_alone() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let live = token();
        let mut conn = leave_claimed(&fixture, &scratch_db, &login, &live).await;
        conn.execute("CREATE SCHEMA in_use").await.unwrap();
        drop(conn);
        // The live run's session is the keeper, on a connection of its own.
        let session = alive(&fixture, &live).await;
        recovery::take();
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &Inputs::overload(),
            &ProjectKey::new(true),
        )
        .await;
        let collected = recovery::take();
        let kept = (
            schema_exists(&fixture, &scratch_db, record::CLAIM).await,
            schema_exists(&fixture, &scratch_db, "in_use").await,
        );
        drop(session);
        fixture.drop().await;
        let reason = resolver_refusal(result);
        assert!(
            reason.contains("in use by a run that is still running"),
            "{server}: {reason}"
        );
        assert_eq!(
            kept,
            (true, true),
            "{server}: its claim and objects are untouched"
        );
        assert!(collected.groups.is_empty(), "{server}: {collected:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn objects_with_no_claim_are_still_refused_and_never_cleaned() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        // Someone's own work in the database the operator named.
        fixture
            .run(
                &scratch_db,
                &["CREATE SCHEMA theirs", "CREATE TABLE theirs.t (id integer)"],
            )
            .await;
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &Inputs::overload(),
            &ProjectKey::new(true),
        )
        .await;
        let kept = schema_exists(&fixture, &scratch_db, "theirs").await;
        fixture.drop().await;
        let reason = resolver_refusal(result);
        assert!(reason.contains("not empty"), "{server}: {reason}");
        assert!(kept, "{server}: it is not pbps's to remove");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_dead_supplied_claim_whose_objects_cannot_be_dropped_is_named_with_the_statement() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let dead = token();
        drop(leave_claimed(&fixture, &scratch_db, &login, &dead).await);
        // An object another role owns: `DROP OWNED BY` the login does not
        // reach it.
        fixture
            .run(
                &scratch_db,
                &[
                    "CREATE SCHEMA other_owned",
                    "CREATE TABLE other_owned.t (id integer)",
                ],
            )
            .await;
        recovery::take();
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &Inputs::overload(),
            &ProjectKey::new(true),
        )
        .await;
        let collected = recovery::take();
        fixture.drop().await;
        let reason = resolver_refusal(result);
        assert!(reason.contains("not empty"), "{server}: {reason}");
        let kept = &collected.groups[0].kept[0];
        assert_eq!(kept.resource.kind, "database contents", "{server}");
        assert!(
            kept.why.contains("owned by a role other than"),
            "{server}: {}",
            kept.why
        );
        assert!(
            kept.statement.contains("DROP OWNED BY CURRENT_USER"),
            "{server}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn two_runs_that_start_together_cannot_both_claim_the_database() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let (login, scratch_db) = fixture.confined().await;
        let first = leave_claimed(&fixture, &scratch_db, &login, &token()).await;
        drop(first);
        let mut second = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
            .await
            .unwrap();
        let taken = record::claim(
            &mut second,
            &record::Claim::new(&token(), Default::default(), Default::default()),
        )
        .await;
        fixture.drop().await;
        assert!(
            matches!(taken, Err(record::ClaimError::Taken)),
            "{server}: {taken:?}"
        );
    }
}

/// How long a test waits for something it expects to happen.
const WAIT: std::time::Duration = std::time::Duration::from_secs(20);

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_run_that_reads_a_dead_claim_waits_for_the_recovery_in_progress_and_then_refuses() {
    // Two runs read the same dead claim: the first cleans it and makes its
    // own claim and objects; the second's cleanup, had it run, would empty
    // them. The database lock makes the second wait before it reads a claim,
    // so it finds the first's.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        // No connection limit: two runs of one account are the case.
        let login = fixture
            .login("u", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
            .await;
        fixture
            .admin()
            .await
            .execute(&format!("GRANT pg_read_all_settings TO {}", login.0))
            .await
            .unwrap();
        let scratch_db = fixture.database("s", Some(&login.0)).await;
        drop(leave_claimed(&fixture, &scratch_db, &login, &token()).await);
        let first_token = token();
        let mut first = Conn::connect_named(
            Driver::Postgres,
            &fixture.as_login(&scratch_db, &login),
            &format!("pbps-{first_token}"),
        )
        .await
        .unwrap();
        record::lock(&mut first).await.unwrap();
        let waiting = async {
            let mut admin = fixture.admin().await;
            let started = std::time::Instant::now();
            while started.elapsed() < WAIT {
                let rows = admin
                    .query(&format!(
                        "SELECT 1 FROM pg_catalog.pg_locks l \
                          WHERE l.locktype = 'advisory' AND NOT l.granted \
                            AND l.database = (SELECT oid FROM pg_catalog.pg_database \
                                               WHERE datname = '{scratch_db}')"
                    ))
                    .await
                    .unwrap();
                if !rows.is_empty() {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            false
        };
        // The first run goes on while the second waits: it cleans the dead
        // claim, makes its own and its objects, then lets go of the lock.
        let first_run = async {
            let blocked = waiting.await;
            let claim = record::read_claim(&mut first).await.unwrap();
            if let Some(Ok(dead)) = claim {
                let backend = sql::backend(&mut first).await.unwrap();
                let _ = engine::release_supplied(
                    &mut first,
                    &backend,
                    &pbps_pg::resolver::standard::Standard::default(),
                    &scratch_db,
                    &dead.defaults,
                    &dead.acl,
                )
                .await;
            }
            let defaults = sql::login_defaults(&mut first).await.unwrap();
            let acl = standard::read_state(&mut first).await.unwrap().acl;
            let claimed =
                record::claim(&mut first, &record::Claim::new(&first_token, defaults, acl)).await;
            let _ = first.execute("CREATE SCHEMA first_runs").await;
            record::unlock(&mut first).await.unwrap();
            (blocked, claimed.is_ok())
        };
        let second_run = async {
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &Inputs::overload(),
                &ProjectKey::new(true),
            )
            .await
        };
        let ((blocked, claimed), second) = tokio::join!(first_run, second_run);
        let kept = (
            schema_exists(&fixture, &scratch_db, record::CLAIM).await,
            schema_exists(&fixture, &scratch_db, "first_runs").await,
        );
        drop(first);
        fixture.drop().await;
        assert!(blocked, "{server}: the second run waited on the lock");
        assert!(claimed, "{server}: the first run made its claim");
        let reason = resolver_refusal(second);
        assert!(
            reason.contains("in use by a run that is still running"),
            "{server}: {reason}"
        );
        assert_eq!(
            kept,
            (true, true),
            "{server}: the first run's claim and objects survive"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn the_keepers_settings_survive_the_two_places_that_reset_a_session() {
    for server in SERVERS {
        let fixture = Fixture::new(server);
        let tokens = super::super::Tokens::generate();
        let mut keeper = engine::connect_scratch(Driver::Postgres, &fixture.server, &tokens)
            .await
            .unwrap();
        let idle = async |conn: &mut Conn| {
            let rows = conn.query("SHOW tcp_keepalives_idle").await.unwrap();
            rows[0]
                .try_get::<&str>("tcp_keepalives_idle")
                .unwrap()
                .unwrap()
                .to_owned()
        };
        // `RESET ALL`, which the shared-state reads send on the session
        // that compiles in a supplied database.
        keeper.execute("SET tcp_keepalives_idle = 0").await.unwrap();
        engine::reset_for_test(&mut keeper).await;
        assert_eq!(idle(&mut keeper).await, "30", "{server}: after RESET ALL");
        // `DISCARD ALL`, which the externals file's session ends with.
        keeper.execute("SET tcp_keepalives_idle = 0").await.unwrap();
        engine::externals_for_test(&mut keeper).await;
        assert_eq!(idle(&mut keeper).await, "30", "{server}: after DISCARD ALL");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn a_release_that_cannot_finish_keeps_the_login_as_the_record() {
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let run = token();
    let names = leave(&mut scratch, &run, Stage::Created).await;
    let stuck = format!("pbps_role_5_{run}");
    let other = scratch.database("o", None).await;
    let mut admin = scratch.admin().await;
    admin
        .execute(&format!("CREATE ROLE \"{stuck}\" NOLOGIN"))
        .await
        .unwrap();
    scratch.roles.push(stuck.clone());
    scratch
        .run(
            &other,
            &[
                "CREATE TABLE public.held (id integer)",
                &format!("GRANT SELECT ON public.held TO \"{stuck}\""),
            ],
        )
        .await;
    let created = pbps_db::resolver::ScratchNames::for_run(&run, "0".repeat(32)).unwrap();
    let roles: Vec<pbps_model::RoleName> = names
        .iter()
        .filter(|name| name.starts_with("pbps_role_"))
        .chain([&stuck])
        .map(|name| name.as_str().into())
        .collect();
    let remaining = engine::release_run_owned(&mut admin, &created, &roles).await;
    let left = scratch.inventory().await;
    let present = |name: &str| left.0.iter().chain(&left.1).any(|n| n == name);
    let state = (
        present(&format!("pbps_scratch_{run}")),
        present(&format!("pbps_role_0_{run}")),
        present(&stuck),
        present(&format!("pbps_run_{run}")),
    );
    drop(admin);
    scratch.drop().await;
    assert_eq!(
        state,
        (false, false, true, true),
        "the database and the roles that can go went; the stuck role and the record stay"
    );
    assert_eq!(
        remaining,
        [format!("role {stuck}"), format!("role pbps_run_{run}")],
        "and both are named"
    );
}
