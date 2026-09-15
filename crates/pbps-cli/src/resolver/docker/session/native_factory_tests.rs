use super::*;
use pbps_config::resolver::{PullPolicy, ResolverProfile};
use pbps_db::transport::PeerVerifiedConn;

#[tokio::test]
#[ignore = "requires a disposable native Linux host, direct root Docker channel and an explicitly owned loopback TLS target"]
async fn native_factory_qualifies_before_bootstrap_and_rejects_rebound_target_connections() {
    assert_eq!(
        std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref(),
        Ok("1")
    );
    let driver = match std::env::var("PBPS_NATIVE_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit fixture engine required"),
    };
    let socket = std::path::PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let primary = std::env::var("PBPS_NATIVE_CONNECTION").unwrap();
    let service = std::env::var("PBPS_NATIVE_SERVICE_PID")
        .unwrap()
        .parse()
        .unwrap();
    let profile = ResolverProfile::Docker {
        image: std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap(),
        pull: PullPolicy::Never,
    };
    for action in ["close", "rebound", "drop-target"] {
        let peer = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
        let mut target = Some(NativeTarget::establish(peer, service).await.unwrap());
        let mut api = LocalApi::connect_native(&socket).await.unwrap();
        let image = api.acquire(&profile).await.unwrap();
        let different = LocalApi::connect_native(&socket).await.unwrap();
        assert!(
            matches!(
                super::super::ReservedSession::reserve(different, image.clone(), driver).await,
                Err(StartFailure {
                    cause: Error::Profile,
                    ..
                })
            ),
            "image provenance cannot move to another API connection"
        );
        let mut session = CandidateSession::start(api, image, target.as_mut().unwrap())
            .await
            .expect("the complete native factory must pass its real startup and backend gates");
        session.check().await.unwrap();
        session
            .verify_target_separation(target.as_mut().unwrap())
            .await
            .unwrap();
        let state = session.state.as_mut().unwrap();
        assert!(state.native.is_some());
        assert!(state.target.is_some());
        let resources = [
            state.control.container_id().to_owned(),
            state.workload.container_id().to_owned(),
        ];
        state
            .connection
            .execute("CREATE TABLE pbps_native_factory_table (id integer)")
            .await
            .unwrap();
        state
            .connection
            .execute(
                "CREATE VIEW pbps_native_factory_view AS SELECT id FROM pbps_native_factory_table",
            )
            .await
            .unwrap();
        session.check().await.unwrap();
        if action == "rebound" {
            let peer = PeerVerifiedConn::connect(driver, &primary).await.unwrap();
            let mut replacement = NativeTarget::establish(peer, service).await.unwrap();
            assert!(
                target
                    .as_mut()
                    .unwrap()
                    .same_instance(&mut replacement)
                    .await
                    .unwrap()
            );
            assert!(
                session
                    .verify_target_separation(&mut replacement)
                    .await
                    .is_err()
            );
            assert!(session.identity().is_err());
            assert!(
                session.check().await.is_err(),
                "target replacement cannot revive partial work"
            );
        } else if action == "drop-target" {
            drop(target.take());
            assert!(session.identity().is_err());
            assert!(
                session.check().await.is_err(),
                "target loss must discard scratch without a separate target check"
            );
            assert!(
                session.check().await.is_err(),
                "the discarded run cannot resume"
            );
        } else {
            session.close().await.unwrap();
        }
        let mut observer = LocalApi::connect_native(&socket).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let mut absent = true;
                for id in &resources {
                    absent &= observer
                        .request(hyper::Method::GET, &format!("/v1.47/containers/{id}/json"))
                        .await
                        .unwrap()
                        .0
                        == hyper::StatusCode::NOT_FOUND;
                }
                if absent {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("only the complete run's two owned resources must be removed");
        if let Some(mut target) = target {
            target.check().await.unwrap();
        }
    }
}
