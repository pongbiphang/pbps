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
