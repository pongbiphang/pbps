use super::*;

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_releases_scratch_when_session_restoration_fails() {
    for (server, supplied) in [
        ("PBPS_TEST_PG_DB", false),
        ("PBPS_TEST_PG_DB", true),
        ("PBPS_TEST_PG_OLD_DB", true),
    ] {
        let mut target = Fixture::new(server);
        let target_db = target.target().await;
        let mut scratch = Fixture::new(if supplied { server } else { SCRATCH_SERVER });
        let scratch_url = if supplied {
            let (login, database) = scratch.confined().await;
            scratch.as_login(&database, &login)
        } else {
            scratch.server.clone()
        };
        let before = scratch.inventory().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let failed = produce_staged(
            &scratch_url,
            &target.on(&target_db),
            &inputs,
            &key,
            "SELECT 1 /* fail_restore_2133 */",
        )
        .await;
        let after_failure = scratch.inventory().await;
        let answered = produce_staged(
            &scratch_url,
            &target.on(&target_db),
            &inputs,
            &key,
            "SELECT 1",
        )
        .await;
        let after_success = scratch.inventory().await;
        target.drop().await;
        scratch.drop().await;
        let error = failed
            .err()
            .expect("no evidence after a failed restoration");
        assert!(
            error
                .to_string()
                .contains("injected session restoration failure"),
            "{server}, supplied={supplied}: {error}"
        );
        assert_eq!(
            before, after_failure,
            "failed restoration releases run-owned resources"
        );
        assert_eq!(
            before, after_success,
            "a later successful run also releases its resources"
        );
        assert_answers_the_overload(&answered.unwrap());
    }
}
