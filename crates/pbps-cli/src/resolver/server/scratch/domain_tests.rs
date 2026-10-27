//! The code-only goal on supplied and run-owned scratch, without plan sealing.
use super::*;
use crate::resolver::server::scratch::{Provenance, domain};
use pbps_model::{ManagedType, Schema};

fn inputs(base: &str) -> Inputs {
    Inputs::from_pair((
        Schema::default(),
        Schema {
            types: [(
                "app.d".parse().unwrap(),
                ManagedType::Domain {
                    base: base.parse().unwrap(),
                    not_null: false,
                    default: None,
                    checks: [("positive".into(), "VALUE IS NOT NULL".into())].into(),
                    collation: None,
                },
            )]
            .into(),
            ..Default::default()
        },
    ))
}

async fn vet(scratch: &str, target: &str, inputs: &Inputs) -> Result<(), domain::Refused> {
    let mut target_session = Conn::connect(Driver::Postgres, target).await.unwrap();
    let identity = crate::resolver::server::target_identity(&mut target_session)
        .await
        .unwrap();
    domain::vet(
        scratch,
        target,
        &identity,
        inputs.desired(),
        &Standard::default(),
        None,
        &Provenance::server(),
    )
    .await
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18 and a separate scratch cluster"]
async fn domain_vet2309_releases_supplied_run_owned_and_incompatible_scratch() {
    for var in SERVERS {
        let mut fixture = Fixture::new(var);
        let target = fixture.database("domain2309", None).await;
        fixture.run(&target, &["CREATE SCHEMA app"]).await;
        let (login, supplied) = fixture.confined().await;
        let before = fixture.inventory().await;
        let result = vet(
            &fixture.as_login(&supplied, &login),
            &fixture.on(&target),
            &inputs("text"),
        )
        .await;
        assert_eq!(fixture.inventory().await, before);
        assert_eq!(fixture.foreign_objects(&supplied).await.1, 0);
        fixture.drop().await;
        assert!(result.is_ok(), "{result:?}");
    }
    // Run-owned is on another PG 18 cluster; no target state is written.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let database = target.database("domain2309run", None).await;
    target.run(&database, &["CREATE SCHEMA app"]).await;
    let scratch = Fixture::new(SCRATCH_SERVER);
    let before = scratch.inventory().await;
    let result = vet(&scratch.server, &target.on(&database), &inputs("text")).await;
    assert_eq!(scratch.inventory().await, before);
    target.drop().await;
    assert!(result.is_ok(), "{result:?}");
    // Other version: a warning-class result, with the run-owned resources gone.
    let mut target = Fixture::new("PBPS_TEST_PG_OLD_DB");
    let database = target.database("domain2309old", None).await;
    target.run(&database, &["CREATE SCHEMA app"]).await;
    let before = scratch.inventory().await;
    let result = vet(&scratch.server, &target.on(&database), &inputs("text")).await;
    assert_eq!(scratch.inventory().await, before);
    target.drop().await;
    assert!(
        matches!(result, Err(domain::Refused::Unavailable(_))),
        "{result:?}"
    );
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
async fn domain_vet2309_missing_extension_types_refuse_with_the_engine_error_and_release() {
    for var in SERVERS {
        let mut fixture = Fixture::new(var);
        let target = fixture.database("domain2309extension", None).await;
        fixture
            .run(
                &target,
                &[
                    "CREATE SCHEMA app",
                    "CREATE EXTENSION hstore WITH SCHEMA public",
                ],
            )
            .await;
        let (login, supplied) = fixture.confined().await;
        let before = fixture.inventory().await;
        let result = vet(
            &fixture.as_login(&supplied, &login),
            &fixture.on(&target),
            &inputs("public.hstore"),
        )
        .await;
        assert_eq!(fixture.inventory().await, before);
        assert_eq!(fixture.foreign_objects(&supplied).await.1, 0);
        fixture.drop().await;
        let Err(domain::Refused::Code(why)) = result else {
            panic!("{result:?}");
        };
        assert!(
            why.contains("app.d")
                && why.contains("public.hstore")
                && why.contains("does not exist"),
            "{why}"
        );
    }
}
