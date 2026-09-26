//! Live qualification of the dedicated-server profile, driven by
//! `scripts/live-resolver-server.py`. Every test here is ignored unless that
//! fixture set it up: two contained supplied servers, a TLS target, and a
//! container runtime that pbps can reach as root.

use super::*;
use crate::resolver::docker::{LocalApi, forwarder::Forwarder};
use crate::resolver::native::NativeTarget;
use pbps_db::Driver;
use pbps_db::transport::{PeerVerifiedConn, StreamConn, StreamLogin};

fn fixture() {
    assert_eq!(std::env::var("PBPS_SERVER_FIXTURE").as_deref(), Ok("1"));
}

fn driver() -> Driver {
    match std::env::var("PBPS_SERVER_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        other => panic!("unknown fixture driver {other}"),
    }
}

fn endpoint(variable: &str) -> ScratchEndpoint {
    ScratchEndpoint::parse(&std::env::var(variable).unwrap()).unwrap()
}

async fn scratch_recipe(
    target: &mut NativeTarget,
) -> pbps_db::resolver::environment::DatabaseRecipe {
    target
        .database_recipe()
        .await
        .expect("the target reports a database recipe")
}

async fn native_target() -> NativeTarget {
    let peer =
        PeerVerifiedConn::connect(driver(), &std::env::var("PBPS_NATIVE_CONNECTION").unwrap())
            .await
            .unwrap();
    NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap()
}

/// A session on the supplied server that the run under test did not open,
/// reached the only way anything reaches it: through a forwarder of its own
/// in the engine's private namespace, as another pbps run or an operator's
/// tooling would.
struct Admin {
    connection: StreamConn,
    forwarder: Forwarder,
}

impl Admin {
    async fn open(endpoint: &ScratchEndpoint, database: &str) -> Self {
        let mut api = LocalApi::connect_native(&endpoint.daemon).await.unwrap();
        let state = api
            .inspect_container(&endpoint.container)
            .await
            .unwrap()
            .expect("the fixture's supplied server exists");
        let pinned = pin(&state).unwrap();
        let image = api.inspect_image(&pinned.image).await.unwrap().unwrap();
        let profile = profile::supported(&endpoint.profile, driver()).unwrap();
        let (forwarder, connection) = Forwarder::open(
            &api,
            &image,
            driver(),
            &pinned.id,
            StreamLogin {
                user: endpoint.user.clone(),
                password: endpoint.password.clone(),
                database: database.to_owned(),
            },
            profile.lifetime.as_secs(),
        )
        .await
        .unwrap();
        Self {
            connection,
            forwarder,
        }
    }

    /// Gone, confirmed: the connection is dropped first so the engine sees
    /// the session end, then the forwarder container is removed before this
    /// returns, so the next admission finds the engine's namespace clean.
    async fn close(self) {
        let Self {
            connection,
            forwarder,
        } = self;
        drop(connection);
        forwarder.close().await.unwrap();
    }
}

async fn session(endpoint: &ScratchEndpoint, database: &str) -> Admin {
    Admin::open(endpoint, database).await
}

/// Admission refuses while any session the run did not open is still there,
/// which is the point of this step. A session the fixture has just dropped
/// takes a moment to leave the engine's own view and the kernel's tables, so
/// the fixture waits for the server to be exclusive instead of the product
/// tolerating it.
async fn admit_when_exclusive(variable: &str, target: &mut NativeTarget) -> DedicatedServer {
    let mut refusals = Vec::new();
    for _ in 0..60 {
        match DedicatedServer::admit(endpoint(variable), target).await {
            Ok(server) => return server,
            Err(ServerFailure {
                cause: Error::Exclusivity(signal),
                recovery_names,
            }) if recovery_names.is_empty() => {
                refusals.push(signal);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(other) => panic!("the supported profile must admit this server: {other}"),
        }
    }
    panic!("the supplied server never became exclusive to this run: {refusals:?}");
}

/// What the engine says this session's own database is.
fn current_database() -> &'static str {
    match driver() {
        Driver::Postgres => "SELECT pg_catalog.current_database()::text AS name",
        Driver::Mssql => "SELECT CONVERT(nvarchar(128), DB_NAME()) AS name;",
    }
}

fn maintenance() -> &'static str {
    match driver() {
        Driver::Postgres => "postgres",
        Driver::Mssql => "master",
    }
}

async fn exists(admin: &mut StreamConn, names: &str) -> (bool, bool) {
    let (database, login) = match driver() {
        Driver::Postgres => (
            format!(
                "SELECT count(*)::text AS n FROM pg_catalog.pg_database WHERE datname LIKE '{names}%'"
            ),
            format!(
                "SELECT count(*)::text AS n FROM pg_catalog.pg_roles WHERE rolname LIKE '{names}%'"
            ),
        ),
        Driver::Mssql => (
            format!(
                "SELECT CONVERT(nvarchar(16), count(*)) AS n FROM sys.databases WHERE name LIKE '{names}%';"
            ),
            format!(
                "SELECT CONVERT(nvarchar(16), count(*)) AS n FROM sys.server_principals WHERE name LIKE '{names}%';"
            ),
        ),
    };
    let count = |rows: Vec<pbps_db::Row>| {
        rows[0]
            .try_get::<&str>("n")
            .unwrap()
            .unwrap()
            .parse::<u32>()
            .unwrap()
            > 0
    };
    (
        count(admin.query(&database).await.unwrap()),
        count(admin.query(&login).await.unwrap()),
    )
}

/// The delivery bar for this step: a real supplied server must work, not
/// merely be refused, and the run must leave the server as it found it.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_supported_dedicated_server_compiles_declarations_and_removes_only_its_own_resources() {
    fixture();
    let marker = std::env::var("PBPS_SERVER_MARKER_DATABASE").unwrap();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    // No inspection session is open here on purpose: one would be a session
    // this run did not open, and admission is required to refuse it.
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .expect("scratch resources");
    let database = run.database().to_owned();
    // Reaching into the run's own state: this step exposes no SQL surface,
    // so a declaration path cannot be opened by accident from outside it.
    run.check(&mut target).await.unwrap();
    let connection = &mut run
        .scratch
        .as_mut()
        .expect("a qualified run is live")
        .connection;
    let reported = connection.query(current_database()).await.unwrap();
    assert_eq!(
        reported[0].try_get::<&str>("name").unwrap(),
        Some(database.as_str()),
        "compilation must happen in the run's own database"
    );
    connection
        .execute("CREATE TABLE pbps_server_table (id integer)")
        .await
        .unwrap();
    connection
        .execute("CREATE VIEW pbps_server_view AS SELECT id FROM pbps_server_table")
        .await
        .unwrap();
    run.check(&mut target)
        .await
        .expect("an ordinary compilation is not drift");
    run.close().await.expect("cleanup removes both objects");
    let mut admin = session(&configured, maintenance()).await;
    assert_eq!(
        exists(&mut admin.connection, "pbps_scratch_").await,
        (false, false),
        "no run-owned scratch database or login may survive"
    );
    assert_eq!(
        exists(&mut admin.connection, "pbps_run_").await,
        (false, false),
        "no run-owned login may survive"
    );
    assert!(
        exists(&mut admin.connection, &marker).await.0,
        "a pre-existing database must be left exactly as it was"
    );
    admin.close().await;
    target.check().await.unwrap();
}

/// Aliases are the case this gate exists for: another database, another
/// login and another endpoint on the very same running instance.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_server_inside_the_target_instance_is_refused_before_any_scratch_resource() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    // Closed again before admission: an open one would be a session this run
    // did not open, and the refusal under test must be the target's identity.
    let before = {
        let mut admin = session(&configured, maintenance()).await;
        let before = exists(&mut admin.connection, "pbps_scratch_").await;
        admin.close().await;
        before
    };
    let mut target = native_target().await;
    match DedicatedServer::admit(endpoint("PBPS_SERVER_ALIAS_ENDPOINT"), &mut target).await {
        Err(ServerFailure {
            cause: Error::TargetInstance,
            recovery_names,
        }) => assert!(recovery_names.is_empty()),
        Err(other) => panic!("refused, but not as the target instance: {other}"),
        Ok(_) => panic!("an alias of the target instance must be refused as the target"),
    }
    let mut admin = session(&configured, maintenance()).await;
    assert_eq!(
        exists(&mut admin.connection, "pbps_scratch_").await,
        before,
        "a refused server must not have had a database created on it"
    );
    admin.close().await;
    target.check().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn an_unimplemented_profile_or_an_exposed_runtime_is_refused_by_name() {
    fixture();
    let mut target = native_target().await;
    let supported = std::env::var("PBPS_SERVER_ENDPOINT").unwrap();
    let unnamed = supported.replace("linux-dedicated-v1", "linux-dedicated-v2");
    match DedicatedServer::admit(ScratchEndpoint::parse(&unnamed).unwrap(), &mut target).await {
        Err(ServerFailure {
            cause: Error::UnsupportedProfile { name, implemented },
            recovery_names,
        }) => {
            assert!(recovery_names.is_empty());
            assert_eq!(name, "linux-dedicated-v2");
            assert_eq!(implemented, "linux-dedicated-v1");
        }
        Err(other) => panic!("an unimplemented profile must be refused by name: {other}"),
        Ok(_) => panic!("an unimplemented profile must never admit a server"),
    }
    // Same profile name, same engine, but the operator's container is on a
    // bridge network. A name is a claim; the daemon's record is the first
    // answer, and it names the key the recipe got wrong.
    match DedicatedServer::admit(endpoint("PBPS_SERVER_EXPOSED_ENDPOINT"), &mut target).await {
        Err(ServerFailure {
            cause: Error::Configuration(key),
            recovery_names,
        }) => {
            assert!(recovery_names.is_empty());
            assert_eq!(key, "NetworkMode");
        }
        Err(other) => panic!("refused, but not for the configuration it names: {other}"),
        Ok(_) => panic!("a server whose runtime does not enforce the named profile must refuse"),
    }
    target.check().await.unwrap();
}

/// The change-and-restore case. The intruding session is gone before the
/// check runs, so a live session list would report an idle server; the
/// engine's cumulative session counter still reports it.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_session_this_run_did_not_open_invalidates_it_even_after_it_closed() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    {
        let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
        server.check().await.expect("an untouched run stays valid");
        let mut other = session(&configured, maintenance()).await;
        other
            .connection
            .execute("CREATE TABLE pbps_restored (id integer)")
            .await
            .unwrap();
        other
            .connection
            .execute("DROP TABLE pbps_restored")
            .await
            .unwrap();
        other.close().await;
        assert!(
            matches!(
                server.check().await,
                Err(Error::Exclusivity(super::Signal::SessionCounter))
            ),
            "a session that opened and closed between checks must still end the run"
        );
        assert!(
            server.check().await.is_err(),
            "the invalidated run cannot be resumed"
        );
    }
    // The same intrusion during a scratch run invalidates it too, and its
    // resources are still removed.
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    server.check().await.unwrap();
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .unwrap();
    run.check(&mut target).await.unwrap();
    session(&configured, maintenance()).await.close().await;
    assert!(matches!(
        run.check(&mut target).await,
        Err(Error::Exclusivity(super::Signal::SessionCounter))
    ));
    assert!(
        matches!(
            run.check(&mut target).await,
            Err(Error::Exclusivity(super::Signal::SessionCounter))
        ),
        "an invalidated run keeps answering with the same cause"
    );
    let mut admin = session(&configured, maintenance()).await;
    let names = exists(&mut admin.connection, "pbps_scratch_").await;
    assert!(names.0, "the invalidated run's database still exists here");
    run.close().await.expect("cleanup still removes it");
    assert_eq!(
        exists(&mut admin.connection, "pbps_scratch_").await,
        (false, false)
    );
    admin.close().await;

    // A cancelled check is terminal for the analysis, but the run must still
    // be closable: dropping its cleanup capability would strand the database
    // and the login on someone else's server.
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    server.check().await.unwrap();
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .unwrap();
    let database = run.database().to_owned();
    assert!(
        tokio::time::timeout(std::time::Duration::from_nanos(1), run.check(&mut target))
            .await
            .is_err(),
        "the check must still have been in flight"
    );
    assert!(
        matches!(run.check(&mut target).await, Err(Error::Cancelled)),
        "a cancelled check cannot be resumed"
    );
    assert!(
        matches!(run.check(&mut target).await, Err(Error::Cancelled)),
        "asking again must not be what loses the cleanup capability"
    );
    assert_eq!(run.database(), database);
    run.close()
        .await
        .expect("a cancelled check still leaves the run closable");
    run.close()
        .await
        .expect("closing an emptied run again is a no-op");

    // A cancelled creation and a cancelled removal must both leave the caller
    // holding something that can still remove the run-owned objects.
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(1),
            server.open_scratch(&scratch_recipe(&mut target).await)
        )
        .await
        .is_err(),
        "creation must still have been in flight"
    );
    // The cancelled creation is terminal for the server, and everything but
    // `discard` says so. Asking twice must not be what loses the names.
    assert!(matches!(server.check().await, Err(Error::Cancelled)));
    assert!(matches!(server.check().await, Err(Error::Cancelled)));
    assert!(matches!(server.identity(), Err(Error::Cancelled)));
    assert!(matches!(
        server
            .open_scratch(&scratch_recipe(&mut target).await)
            .await,
        Err(ServerFailure {
            cause: Error::Cancelled,
            ..
        })
    ));
    server
        .discard()
        .await
        .expect("a cancelled creation is still removable through the server");
    let mut admin = session(&configured, maintenance()).await;
    assert_eq!(
        exists(&mut admin.connection, "pbps_scratch_").await,
        (false, false),
        "a cancelled run leaves nothing behind once it is closed"
    );
    assert_eq!(
        exists(&mut admin.connection, "pbps_run_").await,
        (false, false)
    );
    admin.close().await;
    target.check().await.unwrap();
}

/// A process the engine did not start, sharing its namespaces, is exactly
/// what a walk of the engine's own descendants never reaches — and a root one
/// keeps every privilege the profile says the runtime has taken away.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_process_the_engine_did_not_start_refuses_its_namespaces() {
    fixture();
    let mut target = native_target().await;
    match DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target).await {
        Err(ServerFailure {
            cause: Error::Containment(super::Premise::Occupants),
            recovery_names,
        }) => assert!(recovery_names.is_empty()),
        Err(other) => panic!("refused, but not as a containment failure: {other}"),
        Ok(_) => panic!("a privileged sibling in the engine's namespaces must refuse"),
    }
    target.check().await.unwrap();
}

/// A container joined to the engine's network namespace is in none of its
/// process listings and every bit as able to open a session. The census by
/// namespace is what reaches it: anything sharing that namespace has to be
/// in the engine's PID namespace or be one of this run's own forwarders.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_container_joined_to_the_engines_network_refuses_the_run() {
    fixture();
    let mut target = native_target().await;
    match DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target).await {
        Err(ServerFailure {
            cause: Error::Containment(super::Premise::Accounting),
            recovery_names,
        }) => assert!(recovery_names.is_empty()),
        Err(other) => panic!("refused, but not for the unaccounted namespace occupant: {other}"),
        Ok(_) => panic!("a container sharing the engine's network namespace must refuse"),
    }
    target.check().await.unwrap();
}

/// What a decoy database's statistics row is worth to the cumulative total.
/// On an engine whose counter is one server-level value there is no share to
/// take, so a single intruding session is what the run must catch unaided.
async fn decoy_share(admin: &mut StreamConn, name: &str) -> u64 {
    match driver() {
        Driver::Postgres => {
            let rows = admin
                .query(&format!(
                    "SELECT d.sessions::text AS n FROM pg_catalog.pg_stat_database d \
                     JOIN pg_catalog.pg_database b ON b.oid = d.datid WHERE b.datname = '{name}'"
                ))
                .await
                .unwrap();
            rows[0]
                .try_get::<&str>("n")
                .unwrap()
                .unwrap()
                .parse()
                .unwrap()
        }
        Driver::Mssql => 1,
    }
}

/// A summed counter can be made to come back down: PostgreSQL keeps a row per
/// database, and dropping one takes its share of the sum away. An intruding
/// session that costs the total exactly what a decoy database is worth leaves
/// the arithmetic where admission left it, and a live session list sees
/// nothing once that session has closed. The rows the total was summed over
/// are what remains: one of them disappearing is the refusal.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_removed_statistics_row_cannot_pay_for_an_intruding_session() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let decoy = "pbps_decoy_of_this_run";
    let mut admin = session(&configured, maintenance()).await;
    admin
        .connection
        .execute(&format!("CREATE DATABASE {decoy}"))
        .await
        .unwrap();
    admin.close().await;
    // Sessions of its own, all closed before admission: what the counter
    // remembers is the only thing this is supposed to leave behind.
    session(&configured, decoy).await.close().await;
    let mut admin = session(&configured, maintenance()).await;
    let paid = decoy_share(&mut admin.connection, decoy).await;
    admin.close().await;
    assert!(paid >= 1, "the decoy must be worth something to spend");

    let mut target = native_target().await;
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    server.check().await.unwrap();

    // Exactly as many intruding sessions as the decoy's row is worth, so
    // dropping it puts the total back where admission read it.
    let mut intruders = Vec::new();
    for _ in 0..paid {
        intruders.push(session(&configured, maintenance()).await);
    }
    let before = super::engine::session_counter(&mut intruders[0].connection)
        .await
        .unwrap();
    intruders
        .last_mut()
        .unwrap()
        .connection
        .execute(&format!("DROP DATABASE {decoy}"))
        .await
        .unwrap();
    let after = super::engine::session_counter(&mut intruders[0].connection)
        .await
        .unwrap();
    match driver() {
        Driver::Postgres => {
            assert_eq!(
                after.total + paid,
                before.total,
                "dropping the decoy must take exactly the intruders' increment off the sum"
            );
            assert_eq!(
                after.continuity.len() + 1,
                before.continuity.len(),
                "the dropped database's row is what the total lost"
            );
        }
        Driver::Mssql => {
            assert_eq!(
                after.total, before.total,
                "one server-level counter has no per-database share to remove"
            );
            assert!(
                before.continuity.is_empty() && after.continuity.is_empty(),
                "a counter nothing can take from has no rows to keep"
            );
        }
    }
    // Closed, so the engine's live session list and the kernel's table both
    // report an idle server again.
    for intruder in intruders {
        intruder.close().await;
    }
    // Which signal depends on what the engine's counter is made of. Where it
    // is a sum over rows, the arithmetic is back where admission left it and
    // the missing row is the only thing that says otherwise. Where it is one
    // server-level value, nothing paid for the intruder and the total says so
    // by itself — the case is still that the session ends the run.
    let expected = match driver() {
        Driver::Postgres => super::Signal::CounterRows,
        Driver::Mssql => super::Signal::SessionCounter,
    };
    match server.check().await {
        Err(Error::Exclusivity(signal)) => assert_eq!(signal, expected),
        Err(other) => panic!("refused, but not by the counter: {other}"),
        Ok(()) => panic!("a session paid for with a removed row must still end the run"),
    }
    target.check().await.unwrap();
}

/// A session that is present while the baseline is taken must be refused
/// there: absorbing its count and then finding a clean list would admit a
/// server this run did not have to itself.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_session_present_at_admission_is_refused_rather_than_counted() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    let held = session(&configured, maintenance()).await;
    // The engine listens only on its own loopback, so an intruder present at
    // admission reaches it through a forwarder in the engine's network
    // namespace — exactly what the kernel census accounts for, and it runs
    // before the first session list read. So the census names this refusal;
    // the engine's session list is the same conclusion by a second route. A
    // present intruder is refused either way, which is what this test is for:
    // it is refused, not absorbed into the baseline count.
    match DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target).await {
        Err(ServerFailure {
            cause:
                Error::Containment(super::Premise::Accounting)
                | Error::Exclusivity(super::Signal::SessionList),
            recovery_names,
        }) => assert!(recovery_names.is_empty()),
        Err(other) => panic!("refused, but not for the session the intruder opened: {other}"),
        Ok(_) => panic!("another client was connected while the baseline was read"),
    }
    held.close().await;
    // The same server admits once it really is this run's alone.
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    server.check().await.unwrap();
    target.check().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn replacing_or_dropping_the_target_binding_discards_the_run() {
    fixture();
    let mut target = native_target().await;
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    server.check().await.unwrap();
    let mut replacement = native_target().await;
    assert!(
        target.same_instance(&mut replacement).await.unwrap(),
        "the replacement is the same target instance"
    );
    drop(target);
    assert!(
        server.identity().is_err(),
        "a dropped target binding cannot answer for the run"
    );
    assert!(
        server.identity().is_err(),
        "an observed failure discards the server rather than being retried"
    );
    assert!(
        server.check().await.is_err(),
        "a replacement connection cannot revive a discarded run"
    );
    replacement.check().await.unwrap();
}

/// Cleanup that cannot be confirmed reports the exact run-owned names, and
/// nothing else, so a human can remove them. The supplied server is stopped
/// under a live run: the held session is gone, a fresh one cannot reach the
/// pinned container, and nothing here may pretend the objects are gone.
/// Last in the fixture's order, since the server does not come back.
#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn an_unconfirmed_cleanup_reports_only_the_run_owned_names() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .unwrap();
    let database = run.database().to_owned();
    let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
    api.stop_container(&configured.container).await.unwrap();
    let failure = run
        .close()
        .await
        .expect_err("a stopped server cannot confirm removal");
    assert!(matches!(failure.cause, Error::Cleanup));
    assert!(failure.recovery_names.contains(&database));
    assert!(
        failure
            .recovery_names
            .iter()
            .any(|name| name.starts_with("pbps_run_")),
        "the login is reported with the database: {:?}",
        failure.recovery_names
    );
    assert!(
        failure.recovery_names.iter().all(|name| {
            name.starts_with("pbps_scratch_")
                || name.starts_with("pbps_run_")
                || name.starts_with("pbps-resolver-")
        }),
        "only generated run-owned names are reported: {:?}",
        failure.recovery_names
    );
    assert!(
        !failure
            .recovery_names
            .iter()
            .any(|name| name.contains(&configured.password)),
        "a credential is never a recovery name"
    );
    target.check().await.unwrap();
}

/// #610: a run reads the target's environment and deployer authorization,
/// reproduces them on its scratch database, and compares the two through the
/// lifecycle. Both the target and the supplied server are the same pinned
/// image, so a fresh scratch reproduces the target and the scope verifies;
/// the scope is bound to distinct connections, requalifies on a later check,
/// and cleanup drops the run-local roles it created with nothing left over.
#[tokio::test]
#[ignore = "needs the dedicated-server fixture; run scripts/live-resolver-server.py"]
async fn a_run_qualifies_its_analysis_scope_against_the_target() {
    fixture();
    // What differs by engine is the scope and the statements another session
    // changes the target with; the lifecycle under test is the same.
    struct Case {
        schemas: Vec<String>,
        extras: Vec<String>,
        setup: &'static [&'static str],
        /// A change outside the plan, and the section the refusal names.
        drift: (&'static str, &'static str, &'static str),
        planned: PlannedGrant,
        /// The same grant, as another session runs it on the target.
        performed: &'static str,
        cleanup: &'static [&'static str],
    }
    let case = match driver() {
        // A write-path extra outside `schemas`: its authorization is part of
        // the scope (SPEC §7.3), so verification must cover it too, or a
        // faithful reproduction is refused as missing the schema (finding on
        // #688). The drift is an extension installed after `qualify`.
        Driver::Postgres => Case {
            schemas: vec!["public".to_owned()],
            extras: vec!["pbps_extra_688".to_owned()],
            setup: &[
                "DROP SCHEMA IF EXISTS pbps_extra_688",
                "CREATE SCHEMA pbps_extra_688",
            ],
            drift: (
                "CREATE EXTENSION IF NOT EXISTS pgcrypto",
                "extensions",
                "DROP EXTENSION pgcrypto",
            ),
            planned: PlannedGrant {
                principal: "PUBLIC".into(),
                schema: "pbps_extra_688".into(),
                privilege: "USAGE".into(),
                revoke: false,
            },
            performed: "GRANT USAGE ON SCHEMA pbps_extra_688 TO PUBLIC",
            cleanup: &["DROP SCHEMA pbps_extra_688"],
        },
        // SQL Server has no write path. The deployer is `sa`, which is `dbo`
        // in every database, reproduced by the run login that owns its
        // scratch database; the drift is a grant another session makes on
        // the in-scope schema (#611).
        Driver::Mssql => Case {
            schemas: vec!["dbo".to_owned()],
            extras: Vec::new(),
            setup: &[],
            drift: (
                "GRANT EXECUTE ON SCHEMA::dbo TO public",
                "authorization",
                "REVOKE EXECUTE ON SCHEMA::dbo FROM public",
            ),
            planned: PlannedGrant {
                principal: "public".into(),
                schema: "dbo".into(),
                privilege: "SELECT".into(),
                revoke: false,
            },
            performed: "GRANT SELECT ON SCHEMA::dbo TO public",
            cleanup: &["REVOKE SELECT ON SCHEMA::dbo FROM public"],
        },
    };
    let mut setup =
        PeerVerifiedConn::connect(driver(), &std::env::var("PBPS_NATIVE_CONNECTION").unwrap())
            .await
            .unwrap();
    for statement in case.setup {
        setup.query(statement).await.unwrap();
    }
    let mut target = native_target().await;
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .expect("scratch resources");

    let request = crate::resolver::server::ScopeRequest {
        schemas: case.schemas.clone(),
        write_path_extras: case.extras.clone(),
        planned: Vec::new(),
    };
    let verdict = run
        .qualify(&mut target, &request)
        .await
        .expect("qualify runs");
    assert_eq!(
        verdict,
        pbps_db::resolver::environment::Verdict::Verified,
        "the scratch reproduces the target: {verdict:?}"
    );
    // The scope is sealed with a fingerprint and bound to two distinct
    // connections, so a reopened session cannot present it as its own.
    assert!(run.authorization_fingerprint().is_some());
    let (target_conn, scratch_conn) = run.scope_connections().expect("bound connections");
    assert_ne!(target_conn, scratch_conn);

    // A later check requalifies the sealed scope and holds.
    run.check(&mut target).await.expect("requalification holds");
    // A change on the target after `qualify` by another session is a scope
    // that no longer exists, even though the scratch side is still
    // compatible with it; the next check refuses and says what moved
    // (finding on #688; DECISIONS 520).
    let (change, section, undo) = case.drift;
    setup.query(change).await.unwrap();
    let refused = run
        .check(&mut target)
        .await
        .expect_err("a target that moved since qualify is refused");
    assert!(
        refused.to_string().contains("changed under the run")
            && refused.to_string().contains(section),
        "{refused}"
    );
    setup.query(undo).await.unwrap();

    // Cleanup removes the scratch objects and every run-local role it created,
    // reporting nothing left over.
    run.close()
        .await
        .expect("cleanup confirms the run-local roles gone");

    // A second run, with a planned grant. Another session performing that
    // very grant on the target after `qualify` leaves the projected
    // authorization unchanged — the projection is idempotent — but the
    // target is not what was sealed, and the next check must say so
    // (finding on #688). An admission opens one run, so the server is
    // admitted again for it, once the first run's resources are gone.
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .expect("scratch resources for the second run");
    let request = crate::resolver::server::ScopeRequest {
        schemas: case.schemas.clone(),
        write_path_extras: case.extras.clone(),
        planned: vec![case.planned.clone()],
    };
    let verdict = run
        .qualify(&mut target, &request)
        .await
        .expect("qualify runs with a planned grant");
    assert_eq!(
        verdict,
        pbps_db::resolver::environment::Verdict::Verified,
        "{verdict:?}"
    );
    run.check(&mut target).await.expect("requalification holds");
    setup.query(case.performed).await.unwrap();
    let refused = run
        .check(&mut target)
        .await
        .expect_err("a target that performed the planned grant itself is refused");
    assert!(
        refused.to_string().contains("changed under the run")
            && refused.to_string().contains("authorization"),
        "{refused}"
    );
    run.close()
        .await
        .expect("cleanup confirms the run-local roles gone");
    for statement in case.cleanup {
        setup.query(statement).await.unwrap();
    }
}

#[path = "live_tests/guard_limits.rs"]
mod guard_limits;

#[path = "live_tests/bindings.rs"]
mod bindings;

#[path = "live_tests/host_files.rs"]
mod host_files;

#[path = "live_tests/uts.rs"]
mod uts;

#[path = "live_tests/pseudo.rs"]
mod pseudo;

#[path = "live_tests/admission_recovery.rs"]
pub(super) mod admission_recovery;

#[path = "live_tests/forwarder_mqueue.rs"]
pub(super) mod forwarder_mqueue;
