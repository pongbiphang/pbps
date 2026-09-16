use super::*;
use std::path::PathBuf;

#[tokio::test]
#[ignore = "requires explicit Docker engine fixtures; proves the startup gate, not complete native admission"]
async fn a_reserved_runtime_has_no_database_until_its_fixed_bootstrap_gate_opens() {
    let path = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("unsupported fixture"),
    };
    let mut api = LocalApi::connect(&path).await.unwrap();
    let image = api.inspect_image(&image).await.unwrap().unwrap();
    for action in ["cancel", "invalid", "release"] {
        let mut reserved = ReservedSession::reserve_channels(
            LocalApi::connect(&path).await.unwrap(),
            LocalApi::connect(&path).await.unwrap(),
            LocalApi::connect(&path).await.unwrap(),
            LocalApi::connect(&path).await.unwrap(),
            LocalApi::connect(&path).await.unwrap(),
            image.clone(),
            driver,
        )
        .await
        .unwrap();
        let storage = engine::workload_limits(driver).storage_path;
        let id = reserved.workload.container_id().to_owned();
        let probe = tokio::process::Command::new("docker").args([
            "--host", &format!("unix://{}", path.display()), "exec", &id, "/bin/bash", "-ec",
            &format!(r#"test -z "$(ls -A {storage})"; test "$(ps -eo comm= | awk '$1 == "postgres" || $1 == "sqlservr" {{ count++ }} END {{ print count+0 }}')" = 0; test "$(tr '\0' '\n' </proc/1/environ | awk -F= '$1 == "MSSQL_PID" || $1 == "MSSQL_RPC_PORT" || $1 == "PGDATA" {{ count++ }} END {{ print count+0 }}')" = 0"#)
        ]).output().await.unwrap();
        if !probe.status.success() {
            reserved.close().await.unwrap();
            panic!("database initialization crossed the unopened gate");
        }
        if action == "release" {
            // The test alone can release without native target qualification.
            // Public start must pass its native target/kernel checks first.
            let mut session = reserved.release().await.unwrap();
            session.check().await.unwrap();
            session.close().await.unwrap();
        } else if action == "invalid" {
            reserved
                .bootstrap
                .write_all(b"unexpected-bootstrap-command\n")
                .await
                .unwrap();
            reserved.bootstrap.flush().await.unwrap();
            let refused = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if api
                        .request(hyper::Method::GET, &format!("/v1.47/containers/{id}/json"))
                        .await
                        .unwrap()
                        .0
                        == hyper::StatusCode::NOT_FOUND
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            })
            .await;
            reserved.close().await.unwrap();
            assert!(
                refused.is_ok(),
                "an unrecognized command must terminate before engine initialization"
            );
        } else {
            reserved.close().await.unwrap();
        }
        let (status, _) = api
            .request(hyper::Method::GET, &format!("/v1.47/containers/{id}/json"))
            .await
            .unwrap();
        assert_eq!(status, hyper::StatusCode::NOT_FOUND);
    }
}
