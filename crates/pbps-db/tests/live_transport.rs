//! Disposable real-engine TLS fixtures: scripts/live-transport.py provisions
//! the selected engine and process-private trust roots. No production source
//! or credentials are used. This tests the TLS hop, not runtime admission.

use pbps_db::transport::PeerVerifiedConn;
use pbps_db::{Driver, Param};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn driver() -> Driver {
    match std::env::var("PBPS_TEST_TLS_ENGINE").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("unsupported TLS fixture engine"),
    }
}

fn port() -> u16 {
    std::env::var("PBPS_TEST_TLS_PORT")
        .unwrap()
        .parse()
        .unwrap()
}

fn address(host: &str, port: u16) -> String {
    match driver() {
        Driver::Postgres => format!(
            "host={host} port={port} user=postgres password=Pbps!Test12345 dbname=postgres sslmode=require"
        ),
        Driver::Mssql => {
            format!("Server={host},{port};User Id=sa;Password=Pbps!Test12345;Encrypt=true")
        }
    }
}

async fn read_value(conn: &mut PeerVerifiedConn) {
    let rows = conn
        .query("SELECT CAST(607 AS INT) AS value")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(607));
    let sql = match driver() {
        Driver::Postgres => "SELECT CAST($1 AS INT) AS value",
        Driver::Mssql => "SELECT CAST(@P1 AS INT) AS value",
    };
    let rows = conn.query_with(sql, &[Param::I32(608)]).await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(608));
}

#[tokio::test]
#[ignore = "needs disposable TLS engine and CA; scripts/live-transport.py"]
async fn verified_round_trips_reject_wrong_peers_and_corrupted_replies() {
    let mut first = PeerVerifiedConn::connect(driver(), &address("localhost", port()))
        .await
        .unwrap();
    assert_eq!(first.driver(), driver());
    read_value(&mut first).await;
    let mut replacement = PeerVerifiedConn::connect(driver(), &address("localhost", port()))
        .await
        .unwrap();
    assert_ne!(first.id(), replacement.id());
    read_value(&mut replacement).await;
    // The certificate is valid for localhost only. This reaches the same
    // actual engine, so failure cannot be explained by DNS or a dead port.
    assert!(
        PeerVerifiedConn::connect(driver(), &address("127.0.0.1", port()))
            .await
            .is_err()
    );
    if driver() == Driver::Mssql {
        let ca = std::env::var("PBPS_TEST_TLS_CA").unwrap();
        let mut custom = PeerVerifiedConn::connect(
            driver(),
            &format!(
                "{};TrustServerCertificateCA={ca}",
                address("localhost", port())
            ),
        )
        .await
        .unwrap();
        read_value(&mut custom).await;
        assert!(
            PeerVerifiedConn::connect(
                driver(),
                &format!(
                    "{};TrustServerCertificateCA={ca}.missing",
                    address("localhost", port())
                ),
            )
            .await
            .is_err()
        );
    }

    let relay = Relay::start(port()).await;
    let mut proxied = PeerVerifiedConn::connect(driver(), &address("localhost", relay.port))
        .await
        .unwrap();
    read_value(&mut proxied).await;
    // The relay has already passed a real handshake and both query protocols.
    // Corrupt the next server record only after that positive control.
    relay.corrupt.store(true, Ordering::SeqCst);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        proxied.query("SELECT CAST(609 AS INT) AS value"),
    )
    .await
    .expect("record corruption must terminate the exchange promptly");
    assert!(result.is_err(), "altered TLS reply yielded accepted rows");
    assert!(relay.did_corrupt.load(Ordering::SeqCst));
}

#[tokio::test]
#[ignore = "needs isolated invalid trust environment; scripts/live-transport.py"]
async fn invalid_trust_cannot_yield_a_verified_connection() {
    assert!(
        PeerVerifiedConn::connect(driver(), &address("localhost", port()))
            .await
            .is_err()
    );
}

struct Relay {
    port: u16,
    corrupt: Arc<AtomicBool>,
    did_corrupt: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn start(upstream_port: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let corrupt = Arc::new(AtomicBool::new(false));
        let did_corrupt = Arc::new(AtomicBool::new(false));
        let requested = corrupt.clone();
        let observed = did_corrupt.clone();
        let task = tokio::spawn(async move {
            let (downstream, _) = listener.accept().await.unwrap();
            let upstream = tokio::net::TcpStream::connect(("127.0.0.1", upstream_port))
                .await
                .unwrap();
            let (mut request, mut reply) = downstream.into_split();
            let (mut source, mut destination) = upstream.into_split();
            let upload = async {
                let _ = tokio::io::copy(&mut request, &mut destination).await;
            };
            let download = async {
                let mut buffer = [0; 16384];
                while let Ok(n) = source.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }
                    if requested.swap(false, Ordering::SeqCst) {
                        buffer[n - 1] ^= 1;
                        observed.store(true, Ordering::SeqCst);
                    }
                    if reply.write_all(&buffer[..n]).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! { () = upload => {}, () = download => {} }
        });
        Self {
            port,
            corrupt,
            did_corrupt,
            task,
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
