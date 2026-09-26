//! Supplied workloads and their two forwarders have independent UTS views.

use super::*;
use crate::resolver::docker::uts_fixture::ChangedUts;

#[tokio::test]
#[ignore = "requires scripts/live-resolver-server.py's owned native fixture"]
async fn kernel_name_loss_refuses_admission_and_discards_each_live_view() {
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
    let empty = std::env::var("PBPS_SERVER_EMPTY_RUNTIME_FILES").unwrap() == "1";
    {
        for (field, value) in [
            ("hostname", "pbps-804-host.invalid"),
            ("domainname", "pbps-804-domain.invalid"),
        ] {
            let changed = ChangedUts::capture(&mut api, &container).await;
            changed.replace(field, value);
            let refused =
                match DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target).await {
                    Err(ServerFailure {
                        cause: Error::Unqualified(reason),
                        recovery_names,
                    }) if recovery_names.is_empty() => reason.contains("kernel UTS names"),
                    Ok(mut server) => {
                        server.discard().await.unwrap();
                        false
                    }
                    Err(error) => panic!("unexpected admission refusal: {error}"),
                };
            changed.restore();
            if !refused {
                failures.push(format!("empty={empty} admission {field}"));
            }
            for subject in ["workload", "control", "scratch"] {
                let mut run = open_when_exclusive(&mut target).await;
                let connection = &mut run.scratch.as_mut().unwrap().connection;
                connection
                    .execute("CREATE TABLE pbps_uts_table (id integer)")
                    .await
                    .unwrap();
                connection
                    .execute("CREATE VIEW pbps_uts_view AS SELECT id FROM pbps_uts_table")
                    .await
                    .unwrap();
                run.check(&mut target).await.unwrap();
                let name = match subject {
                    "workload" => container.as_str(),
                    "control" => run
                        .inner
                        .control
                        .session
                        .as_ref()
                        .unwrap()
                        .forwarder
                        .resource_name(),
                    "scratch" => run.scratch.as_ref().unwrap().forwarder.resource_name(),
                    _ => unreachable!(),
                };
                let changed = ChangedUts::capture(&mut api, name).await;
                changed.replace(field, value);
                let analysis = run.inner.analysis.as_ref().unwrap();
                let control = run.inner.control.session.as_ref().unwrap();
                let scratch = run.scratch.as_ref().unwrap();
                let refused_view = if subject == "workload" {
                    analysis
                        .runtime
                        .check(&[&control.guard, &scratch.guard])
                        .is_err()
                } else {
                    let session = if subject == "control" {
                        control
                    } else {
                        scratch
                    };
                    session.check_kernel(analysis.runtime.init()).is_err()
                };
                let refused_run = run.check(&mut target).await.is_err();
                changed.restore_or_confirm_removed(&mut api).await;
                let terminal = run.check(&mut target).await.is_err() && run.inner.live().is_err();
                run.close()
                    .await
                    .expect("restoration permits owned cleanup, never revives analysis");
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
                    "supplied UTS empty={empty} field={field} subject={subject}, admission_refused={refused}, view_refused={refused_view}, run_refused={refused_run}, terminal={terminal}, cleanup=confirmed"
                );
                if !(refused_view && refused_run && terminal) {
                    failures.push(format!("empty={empty} {subject} {field}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "kernel names were admitted: {failures:?}"
    );
}
