//! A session must retain the IPC filesystem belonging to its own forwarder.

use super::*;
use crate::resolver::docker::pseudo_fixture::ChangedPseudo;
use std::cell::RefCell;
use std::rc::Rc;

tokio::task_local! {
    static CHANGE: Rc<RefCell<Change>>;
}

struct Change {
    socket: String,
    skip: usize,
    replace: bool,
    id: Option<String>,
    view: Option<ChangedPseudo>,
}

pub(in crate::resolver::server) fn after_open(forwarder: &Forwarder) {
    let _ = CHANGE.try_with(|change| {
        let mut change = change.borrow_mut();
        if change.skip > 0 {
            change.skip -= 1;
            return;
        }
        if change.view.is_some() {
            return;
        }
        let view = ChangedPseudo::from_pid(
            &change.socket,
            forwarder.container_id(),
            forwarder.pid().unwrap(),
            "mqueue",
        );
        if change.replace {
            view.replace();
        }
        change.id = Some(forwarder.container_id().to_owned());
        change.view = Some(view);
    });
}

async fn restore(change: &Rc<RefCell<Change>>, api: &mut LocalApi) -> String {
    let id = change
        .borrow()
        .id
        .clone()
        .expect("the selected session opened");
    // Refusal may already have removed the owned forwarder. Never follow its
    // old PID to restore a mount on a different process.
    let view = change.borrow_mut().view.take().unwrap();
    view.restore_or_confirm_removed(api).await;
    id
}

async fn cleaned(configured: &ScratchEndpoint, api: &mut LocalApi, id: &str) {
    assert!(api.inspect_container(id).await.unwrap().is_none());
    let mut admin = session(configured, maintenance()).await;
    for prefix in ["pbps_scratch_", "pbps_run_"] {
        assert_eq!(exists(&mut admin.connection, prefix).await, (false, false));
    }
    let marker = std::env::var("PBPS_SERVER_MARKER_DATABASE").unwrap();
    assert!(exists(&mut admin.connection, &marker).await.0);
    admin.close().await;
}

#[tokio::test]
#[ignore = "requires scripts/live-resolver-server.py's owned native fixture"]
async fn every_session_qualifies_its_forwarders_mqueue_before_returning() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
    let mut missed = Vec::new();
    for stage in ["admission", "scratch", "reconstruction", "reopened"] {
        for replace in [false, true] {
            let change = Rc::new(RefCell::new(Change {
                socket: configured.daemon.to_str().unwrap().to_owned(),
                skip: usize::from(stage == "reopened"),
                replace,
                id: None,
                view: None,
            }));
            let refused;
            let reason;
            let terminal;
            let id;
            if stage == "admission" {
                let result = CHANGE
                    .scope(
                        change.clone(),
                        DedicatedServer::admit(endpoint("PBPS_SERVER_ENDPOINT"), &mut target),
                    )
                    .await;
                id = restore(&change, &mut api).await;
                match result {
                    Ok(mut server) => {
                        refused = false;
                        reason = String::new();
                        terminal = true;
                        server.check().await.unwrap();
                        server.discard().await.unwrap();
                    }
                    Err(error) => {
                        assert!(
                            error.recovery_names.is_empty(),
                            "owned removal was confirmed"
                        );
                        refused = true;
                        reason = error.to_string();
                        terminal = true;
                    }
                }
            } else {
                let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
                let recipe = scratch_recipe(&mut target).await;
                if stage == "scratch" {
                    let result = CHANGE
                        .scope(change.clone(), server.open_scratch(&recipe))
                        .await;
                    id = restore(&change, &mut api).await;
                    match result {
                        Ok(mut run) => {
                            refused = false;
                            reason = String::new();
                            terminal = true;
                            run.check(&mut target).await.unwrap();
                            run.close().await.unwrap();
                        }
                        Err(error) => {
                            assert!(
                                error.recovery_names.is_empty(),
                                "owned removal was confirmed"
                            );
                            refused = true;
                            reason = error.to_string();
                            terminal = server.check().await.is_err();
                            server.discard().await.unwrap();
                        }
                    }
                } else {
                    let mut run = server.open_scratch(&recipe).await.unwrap();
                    let request = ScopeRequest::default();
                    let result = CHANGE
                        .scope(change.clone(), run.qualify(&mut target, &request))
                        .await;
                    refused = result.is_err();
                    reason = result
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default();
                    id = restore(&change, &mut api).await;
                    terminal = !refused
                        || (run.inner.live().is_err() && run.check(&mut target).await.is_err());
                    run.close().await.unwrap();
                }
            }
            cleaned(&configured, &mut api, &id).await;
            assert!(!reason.contains(&configured.password));
            if refused {
                assert!(reason.contains("mqueue"), "unexpected refusal: {reason}");
            }
            eprintln!(
                "forwarder mqueue initial stage={stage} replaced={replace}: refused={refused}, terminal={terminal}, cleanup=confirmed"
            );
            if refused != replace || !terminal {
                missed.push((stage, replace));
            }
        }
    }
    target.check().await.unwrap();
    assert!(
        missed.is_empty(),
        "foreign forwarder mqueue admitted: {missed:?}"
    );
}

#[tokio::test]
#[ignore = "requires scripts/live-resolver-server.py's owned native fixture"]
async fn a_foreign_forwarder_mqueue_discards_each_live_view_permanently() {
    fixture();
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let mut target = native_target().await;
    let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
    let mut missed = Vec::new();
    for stage in ["identity", "control", "scratch"] {
        let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
        server.identity().unwrap();
        if stage == "identity" {
            let session = server
                .inner
                .as_ref()
                .unwrap()
                .control
                .session
                .as_ref()
                .unwrap();
            let id = session.forwarder.container_id().to_owned();
            let view = ChangedPseudo::capture(&mut api, &id, "mqueue").await;
            view.replace();
            guarded_tasks(&session.guard, FORWARDER_PRIVILEGES).unwrap();
            session.forwarder.check().await.unwrap();
            let refused = server.identity().is_err();
            view.restore_or_confirm_removed(&mut api).await;
            let terminal = server.identity().is_err() && server.check().await.is_err();
            server.discard().await.unwrap();
            cleaned(&configured, &mut api, &id).await;
            eprintln!(
                "forwarder mqueue retained stage={stage}: refused={refused}, terminal={terminal}, cleanup=confirmed"
            );
            if !(refused && terminal) {
                missed.push(stage);
            }
            continue;
        }
        let mut run = server
            .open_scratch(&scratch_recipe(&mut target).await)
            .await
            .unwrap();
        run.check(&mut target).await.unwrap();
        let selected = if stage == "control" {
            run.inner.control.session.as_ref().unwrap()
        } else {
            run.scratch.as_ref().unwrap()
        };
        let id = selected.forwarder.container_id().to_owned();
        let view = ChangedPseudo::capture(&mut api, &id, "mqueue").await;
        view.replace();
        // The old task/limit and daemon predicates still accept this exact
        // substitution; only the IPC root comparison rejects its provenance.
        guarded_tasks(&selected.guard, FORWARDER_PRIVILEGES).unwrap();
        selected.forwarder.check().await.unwrap();
        let retained = selected
            .check_kernel(run.inner.live().unwrap().runtime.init())
            .is_err();
        let refused = run.check(&mut target).await.is_err();
        view.restore_or_confirm_removed(&mut api).await;
        let terminal = run.check(&mut target).await.is_err() && run.inner.live().is_err();
        if stage == "control" {
            assert!(
                tokio::time::timeout(std::time::Duration::from_nanos(1), run.close())
                    .await
                    .is_err(),
                "cleanup was interrupted in flight"
            );
        }
        run.close().await.unwrap();
        cleaned(&configured, &mut api, &id).await;
        eprintln!(
            "forwarder mqueue retained stage={stage}: retained={retained}, refused={refused}, terminal={terminal}, cleanup=confirmed"
        );
        if !(retained && refused && terminal) {
            missed.push(stage);
        }
    }
    target.check().await.unwrap();
    assert!(
        missed.is_empty(),
        "foreign forwarder mqueue retained: {missed:?}"
    );
}
