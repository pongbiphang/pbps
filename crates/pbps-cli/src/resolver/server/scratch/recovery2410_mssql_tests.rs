//! Crash recovery on the owned SQL Server fixtures (#2410, DEC-2410.2).
//!
//! A run killed at a stage leaves exactly what the run would have made by
//! then. Most cases make that state by hand with the names a run gives it and
//! with no session holding the run's marker, which the engine shows
//! identically to a SIGKILLed client; two drop a real run's future at the
//! point where it waits, which closes its sessions and runs none of its
//! cleanup. The end-to-end kills of a real `pbps plan --dev` are in
//! `tests/flow.rs`. A live run is a session holding its marker table.

use super::super::recovery;
use super::dependencies2323::{self, Case};
use super::*;
use pbps_mssql::resolver::scratch::recovery as record;
use std::collections::BTreeSet;

/// A boxed future: the helpers below are awaited many times in one test, and
/// a debug build keeps each of their futures on the test thread's stack.
type Boxed<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// How far a killed run-owned run got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Its login, the record, and nothing else.
    Record,
    /// And its scratch database.
    Created,
    /// And objects and users in the database.
    Used,
    /// The reference database, made to read a fresh database's options.
    Reference,
    /// The companion database is recorded and not yet created.
    CompanionRecorded,
    /// The companion exists and is marked as the run's.
    CompanionMarked,
    /// The companion exists and the run was killed before it marked it.
    CompanionUnmarked,
    /// The name is recorded and a database of it exists that another run
    /// marked as its own.
    CompanionOfAnother,
}

const REMOVED: [Stage; 6] = [
    Stage::Record,
    Stage::Created,
    Stage::Used,
    Stage::Reference,
    Stage::CompanionRecorded,
    Stage::CompanionMarked,
];

fn companion_of(token: &str) -> String {
    format!("pbps2410c_{}", &token[..16])
}

fn on(scratch: &str, database: &str) -> String {
    format!("{scratch};Database={database}")
}

fn leave<'a>(scratch: &'a str, token: &'a str, stage: Stage) -> Boxed<'a, Vec<String>> {
    Box::pin(leave_inner(scratch, token, stage))
}

/// What a run-owned run killed at `stage` leaves on the instance of
/// `scratch`: the names of the databases and the login it made.
async fn leave_inner(scratch: &str, token: &str, stage: Stage) -> Vec<String> {
    let (login, database) = (format!("pbps_run_{token}"), format!("pbps_scratch_{token}"));
    let mut admin = connection(scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{}',CHECK_POLICY=ON;",
            sql::password(token)
        ))
        .await
        .unwrap();
    let mut names = vec![login];
    if stage == Stage::Record {
        return names;
    }
    admin
        .execute(&format!("CREATE DATABASE [{database}];"))
        .await
        .unwrap();
    names.push(database.clone());
    match stage {
        Stage::Used => {
            connection(&on(scratch, &database))
                .await
                .execute(&format!(
                    "CREATE TABLE dbo.left_over(i int); CREATE USER [pbps_sandbox_{token}] \
                     WITHOUT LOGIN; CREATE USER [pbps_principal_0_{token}] WITHOUT LOGIN;"
                ))
                .await
                .unwrap();
        }
        Stage::Reference => {
            let reference = format!("pbps_reference_{token}");
            admin
                .execute(&format!("CREATE DATABASE [{reference}];"))
                .await
                .unwrap();
            names.push(reference);
        }
        Stage::CompanionRecorded
        | Stage::CompanionMarked
        | Stage::CompanionUnmarked
        | Stage::CompanionOfAnother => {
            let companion = companion_of(token);
            record::record_companions(&mut admin, &database, std::slice::from_ref(&companion))
                .await
                .unwrap();
            if stage != Stage::CompanionRecorded {
                admin
                    .execute(&format!("CREATE DATABASE [{companion}];"))
                    .await
                    .unwrap();
                names.push(companion.clone());
            }
            if stage == Stage::CompanionMarked {
                record::mark_companion(&mut admin, &companion, token)
                    .await
                    .unwrap();
            }
            if stage == Stage::CompanionOfAnother {
                record::mark_companion(&mut admin, &companion, &self::token())
                    .await
                    .unwrap();
            }
        }
        Stage::Record | Stage::Created => {}
    }
    names
}

fn inventory<'a>(scratch: &'a str) -> Boxed<'a, (Vec<String>, Vec<String>)> {
    Box::pin(inventory_inner(scratch))
}

/// Every database and login on the instance.
async fn inventory_inner(scratch: &str) -> (Vec<String>, Vec<String>) {
    let mut admin = connection(scratch).await;
    let names = async |sql: &str, admin: &mut Conn| {
        admin
            .query(sql)
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    (
        names("SELECT name FROM sys.databases ORDER BY name;", &mut admin).await,
        names(
            "SELECT name FROM sys.server_principals WHERE type = 'S' ORDER BY name;",
            &mut admin,
        )
        .await,
    )
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

/// A run's acquisition against `scratch`: it reaches the reconstruction
/// boundary, which is the run's own end, and releases what it made. Boxed,
/// as `refusal` is: several acquisitions in one debug test otherwise exhaust
/// the test thread's stack.
fn run(scratch: &str) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        let target = server("PBPS_TEST_DB");
        let outcome = refusal(&target, scratch).await;
        assert!(
            matches!(&outcome, ProduceError::Run(Error::Binding(reason)) if reason.contains("#2143")),
            "{outcome}"
        );
    })
}

fn alive<'a>(scratch: &'a str, token: &'a str) -> Boxed<'a, Conn> {
    Box::pin(alive_inner(scratch, token))
}

/// The run's marker, held by a session: a live run.
async fn alive_inner(scratch: &str, token: &str) -> Conn {
    let mut conn = connection(scratch).await;
    conn.execute(&format!(
        "CREATE TABLE ##pbps_{token} (marker bit NOT NULL);"
    ))
    .await
    .unwrap();
    conn
}

async fn marker(admin: &mut Conn, token: &str) -> bool {
    admin
        .query(&format!(
            "SELECT CASE WHEN OBJECT_ID(N'tempdb..##pbps_{token}') IS NULL THEN 0 ELSE 1 END \
             AS up;"
        ))
        .await
        .unwrap()[0]
        .try_get::<i32>("up")
        .unwrap()
        == Some(1)
}

fn gone<'a>(admin: &'a mut Conn, token: &'a str) -> Boxed<'a, ()> {
    Box::pin(gone_inner(admin, token))
}

/// Waits until the server no longer shows the marker of `token`.
async fn gone_inner(admin: &mut Conn, token: &str) {
    for _ in 0..200 {
        if !marker(admin, token).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the instance still shows the marker of {token}");
}

fn scrub<'a>(scratch: &'a str, databases: &'a [String], logins: &'a [String]) -> Boxed<'a, ()> {
    Box::pin(scrub_inner(scratch, databases, logins))
}

/// Removes by hand what a test left on purpose, whatever it is.
async fn scrub_inner(scratch: &str, databases: &[String], logins: &[String]) {
    let mut admin = connection(scratch).await;
    for database in databases {
        let _ = record::drop_database(&mut admin, database, true).await;
    }
    for login in logins {
        let _ = record::drop_login(&mut admin, login).await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_run_killed_at_any_stage_is_removed_by_the_next_run() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    for stage in REMOVED {
        let dead = token();
        let names = leave(&scratch, &dead, stage).await;
        let before = inventory(&scratch).await;
        recovery::take();
        run(&scratch).await;
        let collected = recovery::take();
        let after = inventory(&scratch).await;
        scrub(&scratch, &names, &names).await;
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
        let expected: BTreeSet<_> = names.iter().cloned().collect();
        // A recorded companion that never came to exist is named in the
        // record and is not a leftover.
        assert_eq!(
            removed
                .iter()
                .filter(|name| !name.starts_with("pbps2410c_") || expected.contains(*name))
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected,
            "{stage:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_companion_the_run_did_not_mark_is_kept_with_its_run() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    for stage in [Stage::CompanionUnmarked, Stage::CompanionOfAnother] {
        let dead = token();
        let names = leave(&scratch, &dead, stage).await;
        let before = inventory(&scratch).await;
        recovery::take();
        run(&scratch).await;
        let collected = recovery::take();
        let after = inventory(&scratch).await;
        scrub(&scratch, &names, &names).await;
        assert_eq!(
            after, before,
            "nothing of the run goes while a database in it is unproved"
        );
        let group = &collected.groups[0];
        assert!(group.removed.is_empty(), "{group:?}");
        let companion = companion_of(&dead);
        let first = &group.kept[0];
        assert_eq!(first.resource.kind, "companion database");
        assert_eq!(first.statement, format!("DROP DATABASE [{companion}];"));
        assert!(
            first.why.contains("not marked as this run's"),
            "{}",
            first.why
        );
        let kinds: Vec<_> = group.kept.iter().map(|kept| kept.resource.kind).collect();
        assert_eq!(
            kinds,
            ["companion database", "database", "login"],
            "the record stays"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_live_runs_resources_are_never_touched_by_another_runs_sweep() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (live, dead) = (token(), token());
    let session = alive(&scratch, &live).await;
    let live_names = leave(&scratch, &live, Stage::CompanionMarked).await;
    let dead_names = leave(&scratch, &dead, Stage::CompanionMarked).await;
    let before = inventory(&scratch).await;
    run(&scratch).await;
    let during = inventory(&scratch).await;
    let mut admin = connection(&scratch).await;
    // The owner goes: the same state is now a dead run's.
    drop(session);
    gone(&mut admin, &live).await;
    run(&scratch).await;
    let after = inventory(&scratch).await;
    scrub(&scratch, &live_names, &live_names).await;
    scrub(&scratch, &dead_names, &dead_names).await;
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
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_name_that_is_not_a_complete_run_is_never_touched() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (t, other) = (token(), token());
    let short = &t[..16];
    let upper = other.to_uppercase();
    let mut admin = connection(&scratch).await;
    // A database with no login of its token; the older shape (16 digits)
    // with its login; and upper case digits, which no run gives.
    let databases = [
        format!("pbps_scratch_{t}"),
        format!("pbps_scratch_{short}"),
        format!("pbps_scratch_{upper}"),
    ];
    let logins = [format!("pbps_run_{short}"), format!("pbps_run_{upper}")];
    for database in &databases {
        admin
            .execute(&format!("CREATE DATABASE [{database}];"))
            .await
            .unwrap();
    }
    for login in &logins {
        admin
            .execute(&format!(
                "CREATE LOGIN [{login}] WITH PASSWORD=N'{}',CHECK_POLICY=ON;",
                sql::password(&t)
            ))
            .await
            .unwrap();
    }
    let before = inventory(&scratch).await;
    run(&scratch).await;
    let after = inventory(&scratch).await;
    scrub(&scratch, &databases, &logins).await;
    assert_eq!(after, before, "nothing that is not a run's was touched");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_database_a_dead_run_still_has_a_session_on_is_kept_and_named() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let dead = token();
    let names = leave(&scratch, &dead, Stage::Used).await;
    // A connection to the run's database that the server has not noticed is
    // orphaned: the run reads as dead, and the engine itself refuses to drop
    // a database in use (error 3702).
    let zombie = connection(&on(&scratch, &format!("pbps_scratch_{dead}"))).await;
    let before = inventory(&scratch).await;
    recovery::take();
    run(&scratch).await;
    let collected = recovery::take();
    let kept = inventory(&scratch).await;
    drop(zombie);
    run(&scratch).await;
    let after = inventory(&scratch).await;
    scrub(&scratch, &names, &names).await;
    assert_eq!(
        kept, before,
        "nothing of the run was removed while it was in use"
    );
    let group = &collected.groups[0];
    let database = group
        .kept
        .iter()
        .find(|kept| kept.resource.kind == "database")
        .unwrap();
    assert_eq!(
        database.statement,
        format!("DROP DATABASE [pbps_scratch_{dead}];")
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
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_leftover_another_login_owns_is_named_with_the_statement_and_keeps_its_record() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let dead = token();
    let names = leave(&scratch, &dead, Stage::Created).await;
    let (owner, provisioner) = (
        format!("pbps2410_owner_{}", &dead[..16]),
        format!("pbps2410_prov_{}", &dead[..16]),
    );
    let password = sql::password(&token());
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{owner}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; \
             ALTER AUTHORIZATION ON DATABASE::[pbps_scratch_{dead}] TO [{owner}]; \
             CREATE LOGIN [{provisioner}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; \
             GRANT ALTER ANY LOGIN TO [{provisioner}]; \
             GRANT CREATE ANY DATABASE TO [{provisioner}];"
        ))
        .await
        .unwrap();
    // The scratch account of the run-owned layout: it may make logins and
    // databases, and may drop only what it made.
    let account = format!("{scratch};Database=master;User Id={provisioner};Password={password}");
    let before = inventory(&scratch).await;
    recovery::take();
    run(&account).await;
    let collected = recovery::take();
    let after = inventory(&scratch).await;
    scrub(&scratch, &names, &names).await;
    admin
        .execute(&format!(
            "IF SUSER_ID(N'{owner}') IS NOT NULL DROP LOGIN [{owner}]; \
             IF SUSER_ID(N'{provisioner}') IS NOT NULL DROP LOGIN [{provisioner}];"
        ))
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "the run proceeded, and removed nothing it may not"
    );
    let group = &collected.groups[0];
    assert!(group.removed.is_empty(), "{group:?}");
    let by_kind = |kind: &str| group.kept.iter().find(|kept| kept.resource.kind == kind);
    assert_eq!(
        by_kind("database").map(|kept| kept.statement.clone()),
        Some(format!("DROP DATABASE [pbps_scratch_{dead}];")),
        "{group:?}"
    );
    assert_eq!(
        by_kind("login").map(|kept| kept.statement.clone()),
        Some(format!("DROP LOGIN [pbps_run_{dead}];")),
        "the record is kept: {group:?}"
    );
}

/// Drops a real run's future at the point where it waits inside its
/// externals file, and returns the token of the run it left. The sessions of
/// the run close with the future and none of its cleanup runs. `inside` is
/// the supplied database to read the run's sandbox user from; without it the
/// token is the one of the run-owned scratch database that exists.
fn dropped_while_waiting<'a>(
    target: &'a str,
    scratch: &'a str,
    script: &'a str,
    inside: Option<&'a str>,
) -> Boxed<'a, String> {
    Box::pin(dropped_while_waiting_inner(target, scratch, script, inside))
}

async fn dropped_while_waiting_inner(
    target: &str,
    scratch: &str,
    script: &str,
    inside: Option<&str>,
) -> String {
    let mut watcher = connection(&server("PBPS_TEST_MSSQL_SCRATCH_DB")).await;
    let mut database = match inside {
        Some(url) => Some(connection(url).await),
        None => None,
    };
    let waiting = async {
        loop {
            let rows = watcher
                .query(
                    "SELECT COUNT(*) AS n FROM sys.dm_exec_requests WHERE wait_type = \
                     N'WAITFOR';",
                )
                .await
                .unwrap();
            if rows[0].try_get_at::<i32>(0).unwrap() > Some(0) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        // The marker the run's session holds is named by the token the
        // run's own resources carry: the proof of death is read from the same
        // string the names are.
        if let Some(inside) = database.as_mut() {
            let tokens = record::sandboxes(inside, "").await.unwrap();
            let token = tokens
                .into_iter()
                .next()
                .expect("the run's sandbox exists while it waits");
            assert!(marker(&mut watcher, &token).await, "no marker for {token}");
            return token;
        }
        let rows = watcher
            .query(
                "SELECT name FROM sys.databases WHERE name LIKE N'pbps[_]scratch[_]%' ORDER \
                 BY name;",
            )
            .await
            .unwrap();
        let name = rows
            .iter()
            .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
            .last()
            .expect("the run's database exists while it waits");
        let token = name[name.len() - 32..].to_owned();
        assert!(marker(&mut watcher, &token).await, "no marker for {token}");
        token
    };
    tokio::select! {
        outcome = Box::pin(refusal_file_inner(target, scratch, Some(script))) => {
            panic!("the run ended before it waited: {outcome}")
        }
        token = waiting => token,
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_run_dropped_while_using_its_scratch_leaves_a_marker_named_by_its_token_and_is_recovered()
{
    let (target, scratch) = (server("PBPS_TEST_DB"), server("PBPS_TEST_MSSQL_SCRATCH_DB"));
    let mut admin = connection(&scratch).await;
    let before = inventory(&scratch).await;
    let token = dropped_while_waiting(&target, &scratch, "WAITFOR DELAY '00:00:20';", None).await;
    // The names a run gave its resources and the marker its session holds
    // are one token: the proof of death is read from the same string.
    let left = inventory(&scratch).await;
    let named = (
        left.0.contains(&format!("pbps_scratch_{token}")),
        left.1.contains(&format!("pbps_run_{token}")),
    );
    gone(&mut admin, &token).await;
    run(&scratch).await;
    let after = inventory(&scratch).await;
    scrub(
        &scratch,
        &[format!("pbps_scratch_{token}")],
        &[format!("pbps_run_{token}")],
    )
    .await;
    assert_eq!(named, (true, true));
    assert_eq!(after, before, "the next run removed exactly what it left");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_companion_a_run_created_is_recorded_marked_and_removed_by_the_next_run() {
    dependencies2323::fixture(
        pbps_model::dependency::DependencyKind::Table,
        false,
        |case: Case| async move {
            let scratch = case.scratch_server.clone();
            let mut admin = connection(&scratch).await;
            let before = inventory(&scratch).await;
            let (token, record_seen, mark_seen) = killed_after_prepare(&case, &scratch).await;
            gone(&mut admin, &token).await;
            let left = inventory(&scratch).await;
            run(&scratch).await;
            let after = inventory(&scratch).await;
            scrub(
                &scratch,
                &[format!("pbps_scratch_{token}"), case.other_name.clone()],
                &[format!("pbps_run_{token}")],
            )
            .await;
            assert_eq!(
                record_seen,
                vec![case.other_name.clone()],
                "recorded before it existed"
            );
            assert_eq!(mark_seen, token, "marked with the run that made it");
            assert!(
                left.0.contains(&case.other_name),
                "the companion is left behind"
            );
            assert_eq!(after, before, "the next run removed it and the run's own");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_release_that_cannot_remove_a_companion_keeps_the_record_that_names_it() {
    dependencies2323::fixture(
        pbps_model::dependency::DependencyKind::Table,
        false,
        |case: Case| async move {
            let scratch = case.scratch_server.clone();
            let (released, token) = released_after_swap(&case, &scratch).await;
            let left = inventory(&scratch).await;
            let kept = (
                left.0.contains(&format!("pbps_scratch_{token}")),
                left.1.contains(&format!("pbps_run_{token}")),
            );
            scrub(
                &scratch,
                &[format!("pbps_scratch_{token}"), case.other_name.clone()],
                &[format!("pbps_run_{token}")],
            )
            .await;
            let failed = released.expect_err("the swapped companion cannot be released");
            assert!(
                failed
                    .iter()
                    .any(|f| f.contains(&format!("dependency companion {}", case.other_name))),
                "{failed:?}"
            );
            assert!(
                failed
                    .iter()
                    .any(|f| f.contains("kept: they record the companion databases")),
                "{failed:?}"
            );
            assert_eq!(
                kept,
                (true, true),
                "the database and the login are the record"
            );
        },
    )
    .await;
}

/// A run with a dependency, taken to the end of `prepare` and then dropped
/// with no release: its sessions close, nothing else is done. Returns its
/// token, the companions its scratch database recorded, and the run that
/// marked the companion.
async fn killed_after_prepare(case: &Case, scratch: &str) -> (String, Vec<String>, String) {
    let (token, recorded, marked, _) = prepared(case, scratch, false).await;
    (token, recorded, marked)
}

async fn released_after_swap(case: &Case, scratch: &str) -> (Result<(), Vec<String>>, String) {
    let (token, _, _, released) = prepared(case, scratch, true).await;
    (released.expect("released"), token)
}

async fn prepared(
    case: &Case,
    scratch: &str,
    swap: bool,
) -> (String, Vec<String>, String, Option<Result<(), Vec<String>>>) {
    let mut target = connection(&case.target).await;
    let identity = sql::cluster_identity(&mut target).await.unwrap();
    let mut schema = Schema::default();
    schema.modules.insert(
        "dbo.managed2323".parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: format!("SELECT id,name FROM [{}].dbo.dep2323", case.other_name),
            options: Default::default(),
        },
    );
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let bootstrap =
        pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default()).unwrap();
    let declarations = dependencies2323::declarations(
        &case.other_name,
        pbps_model::dependency::DependencyKind::Table,
    );
    let binding = BindingRequest {
        bootstrap: &bootstrap,
        base: &schema,
        desired: &schema,
        dependencies: &declarations,
    };
    let request = super::super::super::ScopeRequest {
        schemas: vec!["dbo".into()],
        ..Default::default()
    };
    let tokens = super::super::Tokens::generate();
    let p = provenance();
    let mut admin = connection(scratch).await;
    let (provisioning, mut state) = super::super::engine::scratch(
        Driver::Mssql,
        &mut target,
        &mut admin,
        &tokens,
        &identity,
        scratch,
        &request,
        &Standard::Mssql,
        None,
        &p,
    )
    .await
    .unwrap();
    let (catalog, authorization) = crate::resolver::scope::read_target(
        &mut target,
        Driver::Mssql,
        &request.schemas,
        &[],
        &request.schemas,
        &[],
    )
    .await
    .unwrap();
    let prepared = Box::pin(state.prepare(
        provisioning,
        &mut admin,
        &mut target,
        &catalog,
        &authorization,
        &request.schemas,
        &binding,
        &[],
    ))
    .await;
    assert!(prepared.is_ok(), "the run prepares its companion");
    let token = tokens.scratch.clone();
    let mut observer = connection(scratch).await;
    let recorded = record::recorded_companions(&mut observer, &format!("pbps_scratch_{token}"))
        .await
        .unwrap();
    let marked = match record::owner_of(&mut observer, &case.other_name)
        .await
        .unwrap()
    {
        record::Owner::Run(owner) => owner,
        other @ (record::Owner::Missing | record::Owner::Unmarked) => format!("{other:?}"),
    };
    if swap {
        // A different database under the companion's name: the run's release
        // finds that it is not the one it made, and keeps it.
        let mut sa = connection(scratch).await;
        drop(prepared);
        sa.execute(&format!(
            "USE master; ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP \
             DATABASE [{0}]; CREATE DATABASE [{0}];",
            case.other_name
        ))
        .await
        .unwrap();
        let released = state.release(&mut admin).await;
        return (token, recorded, marked, Some(released));
    }
    // Killed: nothing is released, and the sessions close with their owners.
    drop(prepared);
    drop(state);
    drop(admin);
    drop(target);
    (token, recorded, marked, None)
}

/// A database the supplied account owns on the instance of `server`.
struct Supplied {
    name: String,
    login: String,
    admin: String,
    connection: String,
    server: String,
}

impl Supplied {
    async fn new(server: &str) -> Self {
        let (name, admin) = create_database(server).await;
        let login = format!("pbps2410_owner_{:032x}", rand::random::<u128>());
        let password = sql::password(&token());
        connection(server)
            .await
            .execute(&format!(
                "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; ALTER \
                 AUTHORIZATION ON DATABASE::[{name}] TO [{login}];"
            ))
            .await
            .unwrap();
        let connection = format!("{server};Database={name};User Id={login};Password={password}");
        Self {
            name,
            login,
            admin,
            connection,
            server: server.to_owned(),
        }
    }

    async fn leave_sandbox(&self, token: &str, objects: bool) {
        let mut admin = connection(&self.admin).await;
        admin
            .execute(&format!(
                "CREATE USER [pbps_sandbox_{token}] WITHOUT LOGIN; ALTER ROLE db_owner ADD \
                 MEMBER [pbps_sandbox_{token}];"
            ))
            .await
            .unwrap();
        if objects {
            admin
                .execute(&format!(
                    "CREATE TABLE dbo.left_over(i int); CREATE USER [pbps_principal_0_{token}] \
                     WITHOUT LOGIN;"
                ))
                .await
                .unwrap();
        }
    }

    async fn names(&self) -> Vec<String> {
        sql::inventory::read(&mut connection(&self.admin).await, None)
            .await
            .unwrap()
            .names()
    }

    async fn drop(self) {
        drop_database(&self.server, &self.name).await;
        connection(&self.server)
            .await
            .execute(&format!("DROP LOGIN [{}];", self.login))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_supplied_run_killed_after_its_sandbox_is_cleaned_by_the_next_run() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    for objects in [false, true] {
        let supplied = Supplied::new(&scratch).await;
        let dead = token();
        supplied.leave_sandbox(&dead, objects).await;
        let seen = supplied.names().await;
        recovery::take();
        run(&supplied.connection).await;
        let collected = recovery::take();
        let left = supplied.names().await;
        supplied.drop().await;
        assert!(!seen.is_empty(), "{objects}: the leftover is there to find");
        assert_eq!(
            left,
            Vec::<String>::new(),
            "{objects}: the database is empty again"
        );
        assert_eq!(collected.groups.len(), 1, "{objects}: {collected:?}");
        assert_eq!(collected.groups[0].token, dead);
        assert!(collected.groups[0].kept.is_empty(), "{objects}");
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_supplied_database_a_live_run_holds_is_refused_and_left_alone() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let supplied = Supplied::new(&scratch).await;
    let live = token();
    supplied.leave_sandbox(&live, true).await;
    let session = alive(&scratch, &live).await;
    let seen = supplied.names().await;
    recovery::take();
    let outcome = refusal(&server("PBPS_TEST_DB"), &supplied.connection)
        .await
        .to_string();
    let collected = recovery::take();
    let left = supplied.names().await;
    drop(session);
    supplied.drop().await;
    assert!(
        outcome.contains("in use by a run that is still running"),
        "{outcome}"
    );
    assert_eq!(left, seen, "its sandbox and objects are untouched");
    assert!(collected.groups.is_empty(), "{collected:?}");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn objects_with_no_sandbox_are_still_refused_and_never_cleaned() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let supplied = Supplied::new(&scratch).await;
    connection(&supplied.admin)
        .await
        .execute("CREATE TABLE dbo.theirs(i int);")
        .await
        .unwrap();
    let seen = supplied.names().await;
    let outcome = refusal(&server("PBPS_TEST_DB"), &supplied.connection)
        .await
        .to_string();
    let left = supplied.names().await;
    supplied.drop().await;
    assert!(outcome.contains("not empty"), "{outcome}");
    assert_eq!(left, seen, "it is not pbps's to remove");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_supplied_run_dropped_while_using_its_database_is_cleaned_by_the_next_run() {
    let (target, scratch) = (server("PBPS_TEST_DB"), server("PBPS_TEST_MSSQL_SCRATCH_DB"));
    let supplied = Box::pin(Supplied::new(&scratch)).await;
    let mut admin = connection(&scratch).await;
    // Its externals file has made a table when the run is dropped.
    let token = dropped_while_waiting(
        &target,
        &supplied.connection,
        "CREATE TABLE dbo.mid_use(i int);\nWAITFOR DELAY '00:00:20';",
        Some(&supplied.admin),
    )
    .await;
    gone(&mut admin, &token).await;
    let seen = supplied.names().await;
    recovery::take();
    run(&supplied.connection).await;
    let collected = recovery::take();
    let left = supplied.names().await;
    Box::pin(supplied.drop()).await;
    assert!(
        seen.iter().any(|name| name.contains("mid_use")) || !seen.is_empty(),
        "the dead run left its table and its sandbox: {seen:?}"
    );
    assert_eq!(left, Vec::<String>::new(), "the database is empty again");
    assert_eq!(collected.groups[0].token, token);
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn the_marker_is_seen_by_a_login_with_no_privilege_and_goes_with_its_session() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let login = format!("pbps2410_low_{}", &token()[..16]);
    let password = sql::password(&token());
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON;"
        ))
        .await
        .unwrap();
    let mut low = connection(&format!("{scratch};User Id={login};Password={password}")).await;
    let own = token();
    let live = token();
    let holder = alive(&scratch, &live).await;
    let seen = record::alive(&mut low, &own, std::slice::from_ref(&live))
        .await
        .unwrap();
    // A reader whose own marker the instance does not show proves nothing.
    let mut mine = alive(&scratch, &own).await;
    let blind = record::alive(&mut low, &token(), std::slice::from_ref(&live))
        .await
        .unwrap();
    let up = record::alive(&mut low, &own, std::slice::from_ref(&live))
        .await
        .unwrap();
    drop(holder);
    gone(&mut admin, &live).await;
    let down = record::alive(&mut low, &own, std::slice::from_ref(&live))
        .await
        .unwrap();
    mine.execute("SELECT 1;").await.unwrap();
    drop(mine);
    drop(low);
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert!(seen.is_none(), "no marker of its own: nothing proved");
    assert!(blind.is_none());
    assert!(up.unwrap().contains(&live), "visible with no privilege");
    assert!(!down.unwrap().contains(&live), "and gone with its session");
}

/// Two runs that read the same dead sandbox would both clean it, and the late
/// one would destroy what the first, already working, had made. The first is
/// played by hand: it holds the application lock the way a run does while it
/// recovers, and releases it once its own sandbox and its work exist.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_late_recoverer_waits_for_the_lock_then_finds_a_live_sandbox_and_refuses() {
    let (target, scratch) = (server("PBPS_TEST_DB"), server("PBPS_TEST_MSSQL_SCRATCH_DB"));
    let supplied = Box::pin(Supplied::new(&scratch)).await;
    let (dead, live) = (token(), token());
    supplied.leave_sandbox(&dead, true).await;
    let mut first = connection(&supplied.admin).await;
    record::lock(&mut first, 0).await.unwrap();
    let mut late = refusal(&target, &supplied.connection);
    let waiting = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut late)
        .await
        .is_err();
    // The first recoverer cleans the dead sandbox and starts working.
    let session = alive(&scratch, &live).await;
    first
        .execute(&format!(
            "DROP TABLE dbo.left_over; DROP USER [pbps_principal_0_{dead}]; ALTER ROLE db_owner \
             DROP MEMBER [pbps_sandbox_{dead}]; DROP USER [pbps_sandbox_{dead}]; CREATE USER \
             [pbps_sandbox_{live}] WITHOUT LOGIN; ALTER ROLE db_owner ADD MEMBER \
             [pbps_sandbox_{live}]; CREATE TABLE dbo.first_work(i int);"
        ))
        .await
        .unwrap();
    record::unlock(&mut first).await.unwrap();
    let outcome = late.await.to_string();
    let names = supplied.names().await;
    drop(session);
    Box::pin(supplied.drop()).await;
    assert!(
        waiting,
        "the late run waits for the lock instead of reading"
    );
    assert!(
        outcome.contains("in use by a run that is still running"),
        "{outcome}"
    );
    assert!(
        names.iter().any(|name| name.contains("first_work")),
        "the first run's objects survived the late one: {names:?}"
    );
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn two_runs_that_start_together_on_one_supplied_database_exactly_one_proceeds() {
    let (target, scratch) = (server("PBPS_TEST_DB"), server("PBPS_TEST_MSSQL_SCRATCH_DB"));
    for dead_sandbox in [false, true] {
        let supplied = Box::pin(Supplied::new(&scratch)).await;
        if dead_sandbox {
            supplied.leave_sandbox(&token(), true).await;
        }
        recovery::take();
        let (a, b) = tokio::join!(
            refusal(&target, &supplied.connection),
            refusal(&target, &supplied.connection)
        );
        let collected = recovery::take();
        let left = supplied.names().await;
        Box::pin(supplied.drop()).await;
        let outcomes = [a, b];
        let proceeded = outcomes
            .iter()
            .filter(|o| matches!(o, ProduceError::Run(Error::Binding(r)) if r.contains("#2143")))
            .count();
        let refused = outcomes
            .iter()
            .filter(|o| {
                o.to_string()
                    .contains("in use by a run that is still running")
            })
            .count();
        assert_eq!(
            (proceeded, refused),
            (1, 1),
            "{dead_sandbox}: exactly one proceeds and the other is refused: {outcomes:?}"
        );
        assert_eq!(
            left,
            Vec::<String>::new(),
            "{dead_sandbox}: the one that ran emptied it"
        );
        assert_eq!(
            collected.groups.len(),
            usize::from(dead_sandbox),
            "{dead_sandbox}: the dead sandbox is recovered once"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_run_that_cannot_get_the_lock_refuses_naming_it_and_touches_nothing() {
    let (target, scratch) = (server("PBPS_TEST_DB"), server("PBPS_TEST_MSSQL_SCRATCH_DB"));
    let supplied = Box::pin(Supplied::new(&scratch)).await;
    supplied.leave_sandbox(&token(), true).await;
    let mut holder = connection(&supplied.admin).await;
    record::lock(&mut holder, 0).await.unwrap();
    let seen = supplied.names().await;
    let started = std::time::Instant::now();
    let outcome = refusal(&target, &supplied.connection).await.to_string();
    let waited = started.elapsed();
    let kept = supplied.names().await;
    record::unlock(&mut holder).await.unwrap();
    drop(holder);
    Box::pin(supplied.drop()).await;
    assert!(outcome.contains(record::LOCK), "{outcome}");
    assert!(outcome.contains("did not release it"), "{outcome}");
    assert!(
        waited >= std::time::Duration::from_secs(5),
        "it waited: {waited:?}"
    );
    assert_eq!(kept, seen, "it read and removed nothing");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn the_lock_is_the_sessions_and_a_dropped_session_frees_it() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let supplied = Box::pin(Supplied::new(&scratch)).await;
    let mut holder = connection(&supplied.admin).await;
    record::lock(&mut holder, 0).await.unwrap();
    let mut other = connection(&supplied.admin).await;
    let busy = matches!(
        record::lock(&mut other, 0).await,
        Err(record::LockError::Busy)
    );
    // Another database of the instance is another lock.
    let mut elsewhere = connection(&scratch).await;
    let apart = record::lock(&mut elsewhere, 0).await.is_ok();
    record::unlock(&mut elsewhere).await.unwrap();
    drop(holder);
    let mut freed = false;
    for _ in 0..60 {
        if record::lock(&mut other, 0).await.is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    drop(other);
    Box::pin(supplied.drop()).await;
    assert!(busy, "a second session in the database cannot take it");
    assert!(apart, "it is scoped to the database");
    assert!(freed, "a killed session frees it");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_refused_acquisition_releases_the_lock_on_its_own_session() {
    let (target_url, scratch) = (server("PBPS_TEST_DB"), server("PBPS_TEST_MSSQL_SCRATCH_DB"));
    let supplied = Box::pin(Supplied::new(&scratch)).await;
    let live = token();
    supplied.leave_sandbox(&live, false).await;
    let session = alive(&scratch, &live).await;
    let schema = Schema::default();
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let request =
        super::super::super::producer::request(Driver::Mssql, side, side, &Hints::default(), &[])
            .unwrap();
    let mut target = connection(&target_url).await;
    let identity = sql::cluster_identity(&mut target).await.unwrap();
    // The session stays open: the lock must be free without its end.
    let mut admin = connection(&supplied.connection).await;
    let tokens = super::super::Tokens::generate();
    let provenance = provenance();
    let refused = super::super::engine::scratch(
        Driver::Mssql,
        &mut target,
        &mut admin,
        &tokens,
        &identity,
        &supplied.connection,
        &request,
        &Standard::Mssql,
        None,
        &provenance,
    )
    .await
    .err()
    .map(|error| error.to_string());
    let mut other = connection(&supplied.admin).await;
    let free = record::lock(&mut other, 0).await.is_ok();
    drop(admin);
    drop(session);
    Box::pin(supplied.drop()).await;
    assert!(
        refused.is_some_and(|reason| reason.contains("still running")),
        "the live sandbox refuses the acquisition"
    );
    assert!(free, "and its lock is free while its session lives");
}
