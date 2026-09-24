//! The actual cgroup and IPC filesystem roots qualify the supplied runtime.

use super::*;
use crate::resolver::docker::pseudo_fixture::ChangedPseudo;

#[tokio::test]
#[ignore = "requires scripts/live-resolver-server.py's owned native fixture"]
async fn foreign_pseudo_roots_refuse_admission_and_discard_live_analysis() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let marker = std::env::var("PBPS_SERVER_MARKER_DATABASE").unwrap();
    let mut target = native_target().await;
    let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
    let record = api
        .inspect_container(&configured.container)
        .await
        .unwrap()
        .unwrap();
    let container = pin(&record).unwrap().id;
    let mut failures = Vec::new();
    for kind in ["cgroup", "mqueue"] {
        let changed = ChangedPseudo::capture(&mut api, &container, kind).await;
        changed.replace();
        let admission =
            match DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target).await {
                Err(ServerFailure {
                    cause: Error::Containment(Premise::Anchors),
                    recovery_names,
                }) if recovery_names.is_empty() => true,
                Ok(mut server) => {
                    server.discard().await.unwrap();
                    false
                }
                Err(error) => panic!("unexpected admission refusal: {error}"),
            };
        changed.restore();
        let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
        let mut run = server
            .open_scratch(&scratch_recipe(&mut target).await)
            .await
            .unwrap();
        let connection = &mut run.scratch.as_mut().unwrap().connection;
        connection
            .execute("CREATE TABLE pbps_pseudo_table (id integer)")
            .await
            .unwrap();
        connection
            .execute("CREATE VIEW pbps_pseudo_view AS SELECT id FROM pbps_pseudo_table")
            .await
            .unwrap();
        run.check(&mut target).await.unwrap();
        changed.replace();
        let analysis = run.inner.analysis.as_ref().unwrap();
        let control = run.inner.control.session.as_ref().unwrap();
        let scratch = run.scratch.as_ref().unwrap();
        // The old kind/flag/duplicate-row checks still accept this exact
        // substitution. Only a positive root identity can reject its origin.
        let rows = crate::resolver::native::mount_rows(analysis.runtime.init()).unwrap();
        profile::contained(
            &rows,
            profile::supported(&configured.profile, driver()).unwrap(),
        )
        .unwrap();
        let retained = analysis
            .runtime
            .check(&[&control.guard, &scratch.guard])
            .is_err();
        let refused_run = run.check(&mut target).await.is_err();
        changed.restore();
        let terminal = run.check(&mut target).await.is_err() && run.inner.live().is_err();
        run.close()
            .await
            .expect("restoration permits cleanup without reviving analysis");
        let mut admin = session(&configured, maintenance()).await;
        assert_eq!(
            exists(&mut admin.connection, "pbps_scratch_").await,
            (false, false)
        );
        assert_eq!(
            exists(&mut admin.connection, "pbps_run_").await,
            (false, false)
        );
        assert!(exists(&mut admin.connection, &marker).await.0);
        admin.close().await;
        eprintln!(
            "pseudo root {kind}: admission={admission}, retained={retained}, run={refused_run}, terminal={terminal}, cleanup=confirmed"
        );
        if !(admission && retained && refused_run && terminal) {
            failures.push(kind);
        }
    }
    assert!(
        failures.is_empty(),
        "foreign filesystem roots admitted: {failures:?}"
    );
}
