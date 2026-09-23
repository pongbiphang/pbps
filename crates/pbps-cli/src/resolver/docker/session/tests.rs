use super::*;
use hyper::{Method, StatusCode};
use std::path::PathBuf;

#[tokio::test]
async fn an_unqualified_daemon_cannot_start_a_private_candidate() {
    let fixture = super::super::tests::Fixture::new();
    let api = fixture.client().await;
    let image = CandidateImage {
        environment_keys: Some(vec![]),
        acquisition: None,
        identity: super::super::ImageIdentity {
            image_id: format!("sha256:{}", "e".repeat(64)),
            registry_digests: vec![],
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        },
    };
    assert!(matches!(
        super::super::ReservedSession::reserve(api, image, Driver::Postgres).await,
        Err(StartFailure {
            cause: Error::NativeDaemon,
            ..
        })
    ));
}

#[tokio::test]
#[ignore = "requires explicit owned Docker fixtures; exercises transport, not full native admission"]
async fn the_private_channel_compiles_declarations_and_control_loss_discards_the_run() {
    let path =
        PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").expect("explicit test socket"));
    let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").expect("explicit trusted test image");
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("unsupported fixture engine"),
    };
    let mut observer = LocalApi::connect(&path).await.unwrap();
    let image = observer
        .inspect_image(&image)
        .await
        .unwrap()
        .expect("no fixture pull is allowed");
    for lose_control in [false, true] {
        // Only tests can supply these unqualified channels. The production
        // factory requires three independently authenticated native peers.
        let mut session = CandidateSession::start_channels(
            LocalApi::connect(&path).await.unwrap(),
            LocalApi::connect(&path).await.unwrap(),
            LocalApi::connect(&path).await.unwrap(),
            image.clone(),
            driver,
        )
        .await
        .unwrap();
        session.check().await.unwrap();
        let state = session.state.as_mut().unwrap();
        let control = state.control.container_id().to_owned();
        let workload = state.workload.container_id().to_owned();
        let (_, inspect) = observer
            .request(
                Method::GET,
                &format!("{}/containers/{workload}/json", super::super::API),
            )
            .await
            .unwrap();
        let inspect: serde_json::Value = serde_json::from_slice(&inspect).unwrap();
        let prefix = match driver {
            Driver::Postgres => "PBPS_BOOTSTRAP_PASSWORD=",
            Driver::Mssql => "MSSQL_SA_PASSWORD=",
        };
        let password = inspect["Config"]["Env"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .find_map(|entry| entry.strip_prefix(prefix))
            .unwrap();
        let compile = async {
            state
                .connection
                .execute("CREATE TABLE pbps_channel_table (id integer NOT NULL)")
                .await?;
            state
                .connection
                .execute("CREATE VIEW pbps_channel_view AS SELECT id FROM pbps_channel_table")
                .await?;
            let rows = state
                .connection
                .query("SELECT COUNT(*) AS rows FROM pbps_channel_view")
                .await?;
            let one_row = rows.len() == 1;
            let filesystem_write = match driver {
                Driver::Postgres => {
                    state.connection.execute("COPY (SELECT 'private fixture') TO '/tmp/pbps-sql-storage'").await?;
                    state.connection.execute("COPY (SELECT 'outside fixture') TO '/var/tmp/pbps-sql-storage'").await
                }
                Driver::Mssql => {
                    state.connection.execute("BACKUP DATABASE master TO DISK='/tmp/pbps-sql-storage.bak' WITH INIT").await?;
                    state.connection.execute("BACKUP DATABASE master TO DISK='/var/tmp/pbps-sql-storage.bak' WITH INIT").await
                }
            };
            let network = match driver {
                Driver::Postgres => {
                    state.connection.execute("CREATE EXTENSION dblink").await?;
                    state.connection.query(&format!("SELECT dblink_connect('host=127.0.0.1 port=5432 user=postgres password={password} dbname=postgres connect_timeout=2')")).await
                }
                Driver::Mssql => {
                    state.connection.execute("EXEC sp_addlinkedserver @server=N'pbps_channel_network', @srvproduct=N'', @provider=N'MSOLEDBSQL', @datasrc=N'127.0.0.1,1433', @provstr=N'Encrypt=Optional;TrustServerCertificate=yes'").await?;
                    state.connection.execute(&format!("EXEC sp_addlinkedsrvlogin @rmtsrvname=N'pbps_channel_network', @useself=N'False', @rmtuser=N'sa', @rmtpassword=N'{password}'")).await?;
                    state.connection.execute("EXEC sp_serveroption @server=N'pbps_channel_network', @optname=N'connect timeout', @optvalue=N'2'").await?;
                    state.connection.query("EXEC sp_testlinkedserver N'pbps_channel_network'").await
                }
            };
            Ok::<_, pbps_db::DbError>((one_row, filesystem_write.is_err(), network.is_err()))
        }
        .await;
        // Exercise Fast Open with a real socket and the running engine's
        // loopback address, not only the bootstrap's harmless invalid operands.
        // A normal response still uses the existing private connection.
        let fast_open = tokio::process::Command::new("docker")
            .args([
                "--host", &format!("unix://{}", path.display()), "exec", &workload,
                "/usr/bin/perl", "-e",
                r#"use strict; use warnings; use Socket; use Errno qw(EPERM);
                    socket(my $socket, AF_INET, SOCK_STREAM, 0) or die "socket";
                    my $fd=fileno($socket); my $address=sockaddr_in($ARGV[0],inet_aton("127.0.0.1"));
                    my $payload="x";
                    my $result=syscall(44,$fd,$payload,1,0x20000000,$address,length($address));
                    die "sendto did not deny Fast Open" unless $result==-1 && $!==EPERM;
                    my $iov=pack("J J",unpack("J",pack("P",$payload)),1);
                    my $header=pack("J L x4 J J J J L x4",unpack("J",pack("P",$address)),length($address),unpack("J",pack("P",$iov)),1,0,0,0);
                    $result=syscall(46,$fd,$header,0x20000000);
                    die "sendmsg did not deny Fast Open" unless $result==-1 && $!==EPERM;
                    my $messages=$header.pack("L x4",0);
                    $result=syscall(307,$fd,$messages,1,0x20000000);
                    die "sendmmsg did not deny Fast Open" unless $result==-1 && $!==EPERM;
                "#,
                &engine::private_channel_profile(driver).port.to_string(),
            ])
            .output().await.unwrap();
        if lose_control {
            let (status, _) = observer
                .request(
                    Method::POST,
                    &format!(
                        "{}/containers/{control}/kill?signal=KILL",
                        super::super::API
                    ),
                )
                .await
                .unwrap();
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert!(session.check().await.is_err());
            assert!(session.identity().is_err());
            assert!(
                session.check().await.is_err(),
                "a closed stream cannot reconnect or regain its old identity"
            );
        } else {
            session.close().await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let mut absent = true;
                for id in [&control, &workload] {
                    absent &= observer
                        .request(
                            Method::GET,
                            &format!("{}/containers/{id}/json", super::super::API),
                        )
                        .await
                        .unwrap()
                        .0
                        == StatusCode::NOT_FOUND;
                }
                if absent {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("both exact owned resources must be removed");
        assert!(
            fast_open.status.success(),
            "real-socket Fast Open restriction: {}",
            String::from_utf8_lossy(&fast_open.stderr)
        );
        let (one_row, filesystem_blocked, network_blocked) = compile.unwrap();
        assert!(one_row);
        assert!(
            filesystem_blocked,
            "the database administrator cannot write outside private run storage"
        );
        assert!(
            network_blocked,
            "the database administrator cannot open another connection even to its own engine"
        );
    }
}

#[path = "tests/host_files.rs"]
mod host_files;

#[path = "tests/uts.rs"]
mod uts;
