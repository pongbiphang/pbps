//! Supplied runtimes and both run-owned forwarders qualify the same private files.

use super::*;
use crate::resolver::docker::file_fixture::{ChangedFile, mutations};

#[tokio::test]
#[ignore = "requires the explicit owned dedicated-server fixture"]
async fn host_file_loss_refuses_admission_and_discards_live_analysis() {
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
    for (path, mode) in [
        ("/etc/resolv.conf", "trailing-comment"),
        ("/etc/hosts", "host-entry"),
        ("/etc/hostname", "host-name"),
    ] {
        let changed = ChangedFile::capture(&mut api, &container, path).await;
        let (_, text) = mutations(path, changed.original())
            .into_iter()
            .find(|(name, _)| *name == mode)
            .unwrap();
        changed.replace(&text);
        let refused =
            match DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target).await {
                Err(Error::Unqualified(reason)) => reason.contains("host and DNS files"),
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
            .execute("CREATE TABLE pbps_host_file_table (id integer)")
            .await
            .unwrap();
        connection
            .execute("CREATE VIEW pbps_host_file_view AS SELECT id FROM pbps_host_file_table")
            .await
            .unwrap();
        run.check(&mut target).await.unwrap();
        changed.replace(&text);
        let analysis = run.inner.analysis.as_ref().unwrap();
        let control = run.inner.control.session.as_ref().unwrap();
        let scratch = run.scratch.as_ref().unwrap();
        let runtime_refused = analysis
            .runtime
            .check(&[&control.guard, &scratch.guard])
            .is_err();
        let forwarders_refused = [control, scratch].iter().all(|session| {
            check_kernel_parts(
                analysis.runtime.init(),
                &session.guard,
                &session.pair,
                &session.backend,
            )
            .is_err()
        });
        let recheck_refused = run.check(&mut target).await.is_err();
        changed.restore();
        let terminal = run.check(&mut target).await.is_err() && run.inner.live().is_err();
        run.close()
            .await
            .expect("restoring the fixture must permit owned cleanup, never revive analysis");
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
            "supplied host files mode={mode}, admission_refused={refused}, runtime_refused={runtime_refused}, forwarders_refused={forwarders_refused}, recheck_refused={recheck_refused}, terminal={terminal}, cleanup=confirmed"
        );
        if !(refused && runtime_refused && forwarders_refused && recheck_refused && terminal) {
            failures.push(mode);
        }
    }
    assert!(
        failures.is_empty(),
        "supplied host information was admitted: {failures:?}"
    );
}
