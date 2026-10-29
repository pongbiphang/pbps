//! Each delayed shape has its own committed readiness frontier (DEC-2134.1).
use super::*;

fn interleaved_inputs(bad: bool) -> (Inputs, Vec<String>) {
    let early = format!("SELECT id FROM {EXTERNAL}.c");
    let late = format!("SELECT id FROM {EXTERNAL}.e WHERE {MANAGED}.f(id) > 0");
    let next = if bad {
        "SELECT 'not an integer'::integer AS id"
    } else {
        &early
    };
    let inputs = Inputs::from_pair((
        generating(declared(&[("a", &early), ("z", &late)], &["m", "n"])),
        generating(declared(
            &[("a", &early), ("b", next), ("z", &late)],
            &["m", "n"],
        )),
    ));
    let mut setup = generating_target();
    setup.extend([
        format!("CREATE TABLE {MANAGED}.n(id integer)"),
        format!("CREATE TABLE {EXTERNAL}.c(id integer, n {MANAGED}.n)"),
        format!("CREATE TABLE {EXTERNAL}.e(id integer, m {MANAGED}.m)"),
        format!("CREATE VIEW {MANAGED}.a AS {early}"),
        format!("CREATE VIEW {MANAGED}.z AS {late}"),
    ]);
    (inputs, setup)
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18 and Docker"]
async fn resolver_interleaves_independent_fills_at_their_own_ready_points() {
    let (inputs, setup) = interleaved_inputs(false);
    let key = ProjectKey::new(true);
    for (server, image) in DOCKER_PAIRS {
        let mut target = Fixture::new(server);
        let target_db = staging_target(&mut target, &setup).await;
        let (login, scratch_db) = target.confined().await;
        let supplied = produce_filled(
            &target.as_login(&scratch_db, &login),
            &target.on(&target_db),
            &inputs,
            &key,
            None,
        )
        .await;
        let left = target.foreign_objects(&scratch_db).await;
        let containers = scratch_containers();
        let owned = produce_in_docker(
            &setting(image),
            pbps_config::resolver::PullPolicy::Never,
            &target.on(&target_db),
            None,
            &inputs,
            &key,
            None,
            pbps_model::resolver::OutsideWrites::Refused,
        )
        .await;
        let containers_after = scratch_containers();
        target.drop().await;
        for (layout, result) in [("supplied", supplied), ("run-owned Docker", owned)] {
            let plan = result.unwrap_or_else(|error| panic!("{server}/{layout}: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
            assert!(
                plan.changes
                    .changes
                    .iter()
                    .all(|change| !matches!(&change.change,
                Change::CreateTable { name, .. } if name.schema == EXTERNAL)),
                "Fill never becomes a deployment change"
            );
        }
        assert_eq!(left.1, 0, "{server}: supplied cleanup");
        assert_eq!(containers, containers_after, "{server}: owned cleanup");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18 and Docker"]
async fn resolver_cleans_committed_fill_prefixes_after_a_later_compile_failure() {
    let (inputs, setup) = interleaved_inputs(true);
    let key = ProjectKey::new(true);
    for (server, image) in DOCKER_PAIRS {
        let mut target = Fixture::new(server);
        let target_db = staging_target(&mut target, &setup).await;
        let (login, scratch_db) = target.confined().await;
        let supplied = produce_filled(
            &target.as_login(&scratch_db, &login),
            &target.on(&target_db),
            &inputs,
            &key,
            None,
        )
        .await;
        let left = target.foreign_objects(&scratch_db).await;
        let containers = scratch_containers();
        let owned = produce_in_docker(
            &setting(image),
            pbps_config::resolver::PullPolicy::Never,
            &target.on(&target_db),
            None,
            &inputs,
            &key,
            None,
            pbps_model::resolver::OutsideWrites::Refused,
        )
        .await;
        let containers_after = scratch_containers();
        target.drop().await;
        for (layout, result) in [("supplied", supplied), ("run-owned Docker", owned)] {
            let error = match result {
                Ok(_) => panic!("{server}/{layout}: invalid declaration was sealed"),
                Err(e) => e.to_string(),
            };
            assert!(
                error.contains("not an integer") && error.contains(&format!("{MANAGED}.b")),
                "{server}/{layout}: {error}"
            );
        }
        assert_eq!(
            left.1, 0,
            "{server}: committed prefix removed after failure"
        );
        assert_eq!(
            containers, containers_after,
            "{server}: owned failure cleanup"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18 and Docker"]
async fn resolver_cleans_earlier_committed_fills_after_a_second_setup_failure() {
    let (inputs, setup) = interleaved_inputs(false);
    let key = ProjectKey::new(true);
    let failure = (
        format!("table {EXTERNAL}.e"),
        format!(
            "DO $probe$ BEGIN IF pg_catalog.to_regclass('{EXTERNAL}.c') IS NULL THEN RAISE EXCEPTION 'earlier Fill was not committed'; END IF; RAISE EXCEPTION 'injected second Fill setup failure'; END $probe$"
        ),
    );
    for (server, image) in DOCKER_PAIRS {
        let mut target = Fixture::new(server);
        let target_db = staging_target(&mut target, &setup).await;
        let (login, scratch_db) = target.confined().await;
        let supplied = super::super::FILL_FAILURE2134
            .scope(
                Some(failure.clone()),
                produce_filled(
                    &target.as_login(&scratch_db, &login),
                    &target.on(&target_db),
                    &inputs,
                    &key,
                    None,
                ),
            )
            .await;
        let left = target.foreign_objects(&scratch_db).await;
        let containers = scratch_containers();
        let owned = super::super::FILL_FAILURE2134
            .scope(
                Some(failure.clone()),
                produce_in_docker(
                    &setting(image),
                    pbps_config::resolver::PullPolicy::Never,
                    &target.on(&target_db),
                    None,
                    &inputs,
                    &key,
                    None,
                    pbps_model::resolver::OutsideWrites::Refused,
                ),
            )
            .await;
        let containers_after = scratch_containers();
        target.drop().await;
        for (layout, result) in [("supplied", supplied), ("run-owned Docker", owned)] {
            let error = match result {
                Ok(_) => panic!("{server}/{layout}: failed setup was sealed"),
                Err(e) => e.to_string(),
            };
            assert!(
                error.contains("injected second Fill setup failure"),
                "{server}/{layout}: {error}"
            );
        }
        assert_eq!(left.1, 0, "{server}: earlier committed Fill removed");
        assert_eq!(
            containers, containers_after,
            "{server}: owned setup failure cleanup"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18 and Docker"]
async fn resolver_refuses_an_earlier_fill_shape_changed_by_a_later_setup() {
    let (inputs, setup) = interleaved_inputs(false);
    let key = ProjectKey::new(true);
    let mutation = (
        format!("table {EXTERNAL}.e"),
        format!("ALTER TABLE {EXTERNAL}.c ADD COLUMN unexpected_later_column integer"),
    );
    for (server, image) in DOCKER_PAIRS {
        let mut target = Fixture::new(server);
        let target_db = staging_target(&mut target, &setup).await;
        let (login, scratch_db) = target.confined().await;
        let supplied = super::super::FILL_FAILURE2134
            .scope(
                Some(mutation.clone()),
                produce_filled(
                    &target.as_login(&scratch_db, &login),
                    &target.on(&target_db),
                    &inputs,
                    &key,
                    None,
                ),
            )
            .await;
        let left = target.foreign_objects(&scratch_db).await;
        let containers = scratch_containers();
        let owned = super::super::FILL_FAILURE2134
            .scope(
                Some(mutation.clone()),
                produce_in_docker(
                    &setting(image),
                    pbps_config::resolver::PullPolicy::Never,
                    &target.on(&target_db),
                    None,
                    &inputs,
                    &key,
                    None,
                    pbps_model::resolver::OutsideWrites::Refused,
                ),
            )
            .await;
        let containers_after = scratch_containers();
        target.drop().await;
        for (layout, result) in [("supplied", supplied), ("run-owned Docker", owned)] {
            assert!(
                result.is_err(),
                "{server}/{layout}: the changed earlier Fill shape was sealed using a stale comparison"
            );
        }
        assert_eq!(left.1, 0, "{server}: shape refusal cleanup");
        assert_eq!(
            containers, containers_after,
            "{server}: owned shape refusal cleanup"
        );
    }
}
