use super::*;
use pbps_db::Driver;

#[path = "target_proxy_tests.rs"]
mod proxy;

#[tokio::test]
#[ignore = "requires an explicitly owned TLS engine fixture and native process-inspection permissions"]
async fn native_aliases_share_one_instance_and_backend_children_cannot_claim_another() {
    let driver = match std::env::var("PBPS_NATIVE_DRIVER")
        .expect("explicit fixture engine")
        .as_str()
    {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("unsupported native fixture"),
    };
    let main_pid = std::env::var("PBPS_NATIVE_SERVICE_PID")
        .expect("owned fixture service")
        .parse()
        .unwrap();
    let primary = std::env::var("PBPS_NATIVE_CONNECTION").expect("primary fixture connection");
    let alias = std::env::var("PBPS_NATIVE_ALIAS_CONNECTION")
        .expect("alternate database and credential connection");
    let first = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
    let upstream = first.tcp_endpoints().peer();
    let started = std::time::Instant::now();
    let mut first = NativeTarget::establish(first, main_pid).await.unwrap();
    eprintln!(
        "native fixture driver={driver:?} establish_ms={}",
        started.elapsed().as_millis()
    );
    let witness = first.witness().unwrap();
    witness.check().unwrap();
    assert!(first.identity().is_ok(), "unchanged live native target");
    let second = PeerVerifiedConn::connect(driver, &alias).await.unwrap();
    assert!(
        first
            .current
            .as_ref()
            .unwrap()
            .lease
            .check(&second)
            .is_err(),
        "a separate connection cannot reuse the previous target lease"
    );
    let mut second = NativeTarget::establish(second, main_pid).await.unwrap();
    let started = std::time::Instant::now();
    assert!(
        first.same_instance(&mut second).await.unwrap(),
        "database, credential and address spelling do not establish separation"
    );
    eprintln!(
        "native fixture driver={driver:?} two_binding_checks_ms={}",
        started.elapsed().as_millis()
    );
    let connection = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
    let observed = SocketOwnerLease::capture(&connection, main_pid).unwrap();
    let backend = fixture_observer_pid(observed.owner());
    assert_ne!(
        backend, main_pid,
        "this measured fixture must exercise its real backend child"
    );
    assert!(
        NativeTarget::establish(connection, backend).await.is_err(),
        "a backend child cannot be relabeled as another service"
    );

    if driver == Driver::Postgres {
        postgres_capture_is_fresh_and_cancellation_expires_its_connection(&primary, main_pid).await;
    }

    // The supervisor owns this fixture and explicitly permits its process to
    // be suspended. No production target is accepted by this ignored test.
    let backend = fixture_observer_pid(first.current.as_ref().unwrap().lease.owner());
    let suspend = std::process::Command::new("/bin/kill")
        .args(["-STOP", &backend.to_string()])
        .status()
        .unwrap();
    assert!(suspend.success());
    let timeout = tokio::time::timeout(std::time::Duration::from_millis(100), first.check()).await;
    let resume = std::process::Command::new("/bin/kill")
        .args(["-CONT", &backend.to_string()])
        .status()
        .unwrap();
    assert!(
        resume.success(),
        "always resume the owned fixture before asserting cancellation"
    );
    assert!(
        timeout.is_err(),
        "the suspended backend must leave the identity read pending"
    );
    assert!(
        first.identity().is_err(),
        "cancellation discards the entire native binding"
    );
    assert!(
        first.check().await.is_err(),
        "restoration cannot revive the canceled binding"
    );
    assert!(
        witness.check().is_err(),
        "scratch witnesses must lose a canceled target binding"
    );
    second
        .check()
        .await
        .expect("another session's exit cannot invalidate the independent alias");
    let witness = second.witness().unwrap();
    witness.check().unwrap();
    drop(second);
    assert!(
        witness.check().is_err(),
        "a witness cannot keep a dropped target connection alive"
    );

    // A transparent relay can pass the engine's real TLS handshake and valid
    // reads. The local socket still belongs to that relay, so an unqualified
    // intermediate hop cannot inherit the backend's native service identity.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let proxy_address = match driver {
        Driver::Postgres => primary.replace(
            &format!("port={}", upstream.port()),
            &format!("port={proxy_port}"),
        ),
        Driver::Mssql => primary.replace(
            &format!(",{};", upstream.port()),
            &format!(",{proxy_port};"),
        ),
    };
    assert_ne!(proxy_address, primary);
    let proxy = tokio::spawn(async move {
        let (mut downstream, _) = listener.accept().await.unwrap();
        let mut backend = tokio::net::TcpStream::connect(upstream).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut downstream, &mut backend).await;
    });
    let mut connection = PeerVerifiedConn::connect(driver, &proxy_address)
        .await
        .unwrap();
    let rows = connection
        .query("SELECT CAST(608 AS INT) AS value")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(608));
    let refused = NativeTarget::establish(connection, main_pid).await;
    proxy.abort();
    assert!(matches!(refused, Err(NativeTargetError::SocketOwner)));

    proxy::unprotected_backend_cannot_inherit_frontend_tls(driver, &primary, upstream, main_pid)
        .await;

    let connection = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
    let mut expired = NativeTarget::establish(connection, main_pid).await.unwrap();
    let witness = expired.witness().unwrap();
    let query = match driver {
        Driver::Postgres => "SELECT pg_backend_pid() AS id",
        Driver::Mssql => "SELECT CONVERT(int, @@SPID) AS id",
    };
    let id = expired
        .current
        .as_mut()
        .unwrap()
        .connection
        .query(query)
        .await
        .unwrap()[0]
        .try_get::<i32>("id")
        .unwrap()
        .unwrap();
    let mut administrator = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
    let terminate = match driver {
        Driver::Postgres => format!("SELECT pg_terminate_backend({id}, 5000)"),
        Driver::Mssql => format!("KILL {id}"),
    };
    administrator.query(&terminate).await.unwrap();
    // Wait on independent kernel evidence of socket loss; do not invoke the
    // target's mutating check, which would invalidate the cache for us.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while witness.check().is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        expired.identity().is_err(),
        "a closed native socket cannot certify cached identity"
    );
    assert!(
        expired.current.is_none(),
        "accessor loss discards the complete binding"
    );
    let connection = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
    let mut replacement = NativeTarget::establish(connection, main_pid).await.unwrap();
    assert!(replacement.identity().is_ok());
    assert!(
        expired.identity().is_err(),
        "a new connection cannot revive the old capability"
    );
}

// The production owner lease uses the selected procfs view. Only this owned
// fixture needs an observer PID to suspend its backend; explicitly compare
// held identities instead of passing a namespace-local number to host kill.
fn fixture_observer_pid(owner: &super::super::ProcessLease) -> u32 {
    let namespace = std::fs::File::open("/proc/self/ns/pid").unwrap();
    assert!(owner.owns_namespace("pid", &namespace).unwrap());
    let pid = owner.namespace_pid();
    let candidate = super::super::ProcessLease::capture(pid).unwrap();
    assert!(candidate.same_process(owner).unwrap());
    pid
}

// A small contributed C module copied only inside this disposable engine's
// held root proves loaded evidence survives unlink; no large artifact is copied.
async fn load_unlinked_requirement(connection: &mut PeerVerifiedConn, main_pid: u32) -> String {
    use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags};
    use std::fs::File;
    let lease = super::super::ProcessLease::capture(main_pid).unwrap();
    let source_path = executables::library_directory(lease.executable_path()).join("dict_int.so");
    let mut source = lease
        .open_in_root(source_path.to_str().unwrap().trim_start_matches('/'))
        .unwrap();
    let size = source.metadata().unwrap().len();
    assert!(
        size > 0 && size < 1024 * 1024,
        "fixture module must stay small"
    );
    let name = format!("tmp/pbps_unlinked974_{main_pid}.so");
    let root = lease.root().unwrap();
    let fd = rustix::fs::openat2(
        &root,
        &name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
        ResolveFlags::IN_ROOT,
    )
    .unwrap();
    let mut destination = File::from(fd);
    assert_eq!(std::io::copy(&mut source, &mut destination).unwrap(), size);
    drop(destination);
    let path = format!("/{name}");
    connection.query(&format!("CREATE FUNCTION capture_fixture.unlinked_handler(internal) RETURNS internal AS '{path}','dintdict_init' LANGUAGE c")).await.unwrap();
    rustix::fs::unlinkat(&root, &name, AtFlags::empty()).unwrap();
    assert!(lease.open_in_root(&name).is_err());
    lease.check().unwrap();
    path
}

async fn postgres_capture_is_fresh_and_cancellation_expires_its_connection(
    primary: &str,
    main_pid: u32,
) {
    use pbps_pg::resolver::capture::{CandidateClass, CandidateSet, CaptureScope};
    let mut connection = PeerVerifiedConn::connect(Driver::Postgres, primary)
        .await
        .unwrap();
    for statement in [
        "CREATE SCHEMA capture_fixture",
        "CREATE TABLE capture_fixture.t(id integer)",
        "CREATE VIEW capture_fixture.v AS SELECT id+1 AS id FROM capture_fixture.t",
        "CREATE EXTENSION hstore WITH SCHEMA capture_fixture",
        "CREATE FUNCTION capture_fixture.standalone_handler() RETURNS language_handler AS '$libdir/../lib/plpgsql','plpgsql_call_handler' LANGUAGE c",
        "LOAD 'auto_explain'",
        "SET session_preload_libraries='auto_explain'",
        "SET dynamic_library_path='/pbps_private_path876:$libdir'",
    ] {
        connection.query(statement).await.unwrap();
    }
    let unlinked = if std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref() == Ok("1") {
        Some(load_unlinked_requirement(&mut connection, main_pid).await)
    } else {
        None
    };
    let mut scope = CaptureScope {
        retained: Default::default(),
        candidates: [
            CandidateSet {
                class: CandidateClass::Relation,
                namespace: Some("capture_fixture".into()),
                name: None,
            },
            CandidateSet {
                class: CandidateClass::Routine,
                namespace: Some("capture_fixture".into()),
                name: Some("standalone_handler".into()),
            },
            CandidateSet {
                class: CandidateClass::Extension,
                namespace: None,
                name: Some("hstore".into()),
            },
        ]
        .into_iter()
        .collect(),
    };
    if unlinked.is_some() {
        scope.candidates.insert(CandidateSet {
            class: CandidateClass::Routine,
            namespace: Some("capture_fixture".into()),
            name: Some("unlinked_handler".into()),
        });
    }
    let mut target = NativeTarget::establish(connection, main_pid).await.unwrap();
    let captured = target.capture_postgres(&scope).await;
    if std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref() == Ok("1") {
        let captured =
            captured.expect("native capture must qualify actual loaded executable content");
        // The ordinary qualifier already normalizes a required alias to its
        // loaded object. The opaque transfer must preserve that same census.
        let mut ordinary_requirements = vec![
            "$libdir/../lib/plpgsql".into(),
            "$libdir/hstore".into(),
            "auto_explain".into(),
        ];
        ordinary_requirements.push(unlinked.as_ref().unwrap().clone());
        let ordinary = executables::executables(
            target.current.as_ref().unwrap().lease.owner(),
            &ordinary_requirements,
            "/pbps_private_path876:$libdir",
            &[],
        )
        .await
        .unwrap();
        assert!(
            ordinary.libraries.iter().any(|library| {
                library.path == *unlinked.as_ref().unwrap()
                    && library.provenance
                        == pbps_db::resolver::environment::Provenance::LoadedContent
                    && library.disk_differs_from_loaded == Some(true)
                    && library.digest.is_some()
            }),
            "the removed fixture must be backed by actual loaded content"
        );
        assert_eq!(
            captured.native_library_count(),
            ordinary.libraries.len(),
            "opaque transfer must not duplicate a loaded library under a required alias"
        );
        let (_, unchanged) = target.recapture_postgres(&captured).await.unwrap();
        assert!(
            unchanged.is_empty(),
            "an unchanged native target must recapture consistently: {unchanged:?}"
        );
        let mut writer = PeerVerifiedConn::connect(Driver::Postgres, primary)
            .await
            .unwrap();
        // The unlink positive belongs to this backend. Remove its catalog
        // requirement before later controls deliberately expire that backend.
        writer
            .query("DROP FUNCTION capture_fixture.unlinked_handler(internal)")
            .await
            .unwrap();
        // An extension-free C routine must still name its native prerequisite.
        // Alter only this owned fixture's row, then restore it before asserting.
        for missing_or_unreadable in ["/pbps_private_missing876", "/tmp"] {
            writer.query(&format!("UPDATE pg_catalog.pg_proc SET probin='{missing_or_unreadable}' WHERE oid='capture_fixture.standalone_handler()'::regprocedure")).await.unwrap();
            let unavailable = target.capture_postgres(&scope).await;
            writer.query("UPDATE pg_catalog.pg_proc SET probin='$libdir/../lib/plpgsql' WHERE oid='capture_fixture.standalone_handler()'::regprocedure").await.unwrap();
            let error = match unavailable {
                Err(error) => error,
                Ok(_) => panic!("an unreadable standalone C prerequisite was accepted"),
            };
            assert!(matches!(error, CaptureFailure::Executables));
            for report in [format!("{error}"), format!("{error:?}")] {
                assert!(
                    !report.contains(missing_or_unreadable)
                        && !report.contains("pbps_private_path876")
                        && !report.contains(unlinked.as_ref().unwrap())
                );
            }
            assert!(
                target.identity().is_err(),
                "unreadable native inputs expire the binding"
            );
            let connection = PeerVerifiedConn::connect(Driver::Postgres, primary)
                .await
                .unwrap();
            target = NativeTarget::establish(connection, main_pid).await.unwrap();
        }
        writer
            .query("ALTER TABLE capture_fixture.t ADD COLUMN added integer")
            .await
            .unwrap();
        let (_, changed) = target.recapture_postgres(&captured).await.unwrap();
        assert!(
            !changed.is_empty(),
            "a native recapture cannot reuse old catalog facts"
        );
        let report = serde_json::to_string(&changed).unwrap();
        assert!(
            !report.contains("pbps_private_path876")
                && !report.contains("$libdir/../lib/plpgsql")
                && !report.contains(unlinked.as_ref().unwrap())
        );
        // This server belongs solely to the native fixture. Hold one catalog
        // lock to put a real reload after session pinning and before rendering;
        // never reload the shared development/CI database service.
        let rows = target
            .current
            .as_mut()
            .unwrap()
            .connection
            .query("SELECT pg_backend_pid()::text AS pid")
            .await
            .unwrap();
        let backend: u32 = rows[0]
            .try_get::<&str>("pid")
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        writer.query("BEGIN").await.unwrap();
        writer
            .query("LOCK TABLE pg_catalog.pg_cast IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let trigger_reload = async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut reloaded = false;
            while tokio::time::Instant::now() < deadline {
                let sql = format!(
                    "SELECT count(*)::text AS waiting FROM pg_stat_activity WHERE pid={backend} AND wait_event_type='Lock'"
                );
                let rows = writer.query(&sql).await.unwrap();
                if rows[0].try_get::<&str>("waiting").unwrap() == Some("1") {
                    writer.query("SELECT pg_reload_conf()").await.unwrap();
                    reloaded = true;
                    break;
                }
                // pg_stat_activity caches its snapshot within a transaction.
                writer
                    .query("SELECT pg_stat_clear_snapshot()")
                    .await
                    .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            writer.query("COMMIT").await.unwrap();
            reloaded
        };
        let (capture, reloaded) = tokio::join!(
            tokio::time::timeout(
                std::time::Duration::from_secs(45),
                target.capture_postgres(&scope)
            ),
            trigger_reload,
        );
        assert!(
            reloaded,
            "the owned fixture must reload during the capture interval"
        );
        assert!(
            matches!(
                capture,
                Ok(Err(CaptureFailure::Catalog(
                    pbps_pg::resolver::capture::CaptureError::EnvironmentChanged
                )))
            ),
            "a reload during rendering cannot qualify a coherent capture"
        );
        assert!(
            target.identity().is_err(),
            "a failed capture expires its native binding"
        );
    } else {
        assert!(
            matches!(captured, Err(CaptureFailure::Executables)),
            "a disk-only library observation cannot qualify native target capture"
        );
        assert!(
            target.identity().is_err(),
            "failed build qualification expires its native binding"
        );
    }
    let connection = PeerVerifiedConn::connect(Driver::Postgres, primary)
        .await
        .unwrap();
    let mut target = NativeTarget::establish(connection, main_pid).await.unwrap();
    let witness = target.witness().unwrap();
    let backend = fixture_observer_pid(target.current.as_ref().unwrap().lease.owner());
    assert!(
        std::process::Command::new("/bin/kill")
            .args(["-STOP", &backend.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let timed = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        target.capture_postgres(&scope),
    )
    .await;
    let resumed = std::process::Command::new("/bin/kill")
        .args(["-CONT", &backend.to_string()])
        .status()
        .unwrap();
    assert!(
        resumed.success(),
        "resume the owned backend before asserting cancellation"
    );
    assert!(timed.is_err());
    assert!(
        target.identity().is_err(),
        "cancelled capture must expire the complete connection"
    );
    assert!(
        witness.check().is_err(),
        "cancelled capture cannot leave a reusable scratch witness"
    );
    let mut administrator = PeerVerifiedConn::connect(Driver::Postgres, primary)
        .await
        .unwrap();
    administrator
        .query("DROP SCHEMA capture_fixture CASCADE")
        .await
        .unwrap();
}
