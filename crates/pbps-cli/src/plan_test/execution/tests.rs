use super::*;
use crate::plan_test::report::{Phase, Status, TestData};

fn timeouts() -> Timeouts {
    Timeouts {
        startup_ms: 120000.try_into().unwrap(),
        step_ms: 2000.try_into().unwrap(),
        fixture_ms: 30000.try_into().unwrap(),
        assertion_ms: 10000.try_into().unwrap(),
        run_ms: 600000.try_into().unwrap(),
        cleanup_ms: 30000.try_into().unwrap(),
    }
}

#[tokio::test]
async fn d2b1_1165_one_cancellation_observation_is_sticky_across_completed_phases() {
    let (send, receive) = tokio::sync::oneshot::channel::<()>();
    let mut control = Control::new(timeouts(), async {
        receive.await.unwrap();
        Ok(())
    });
    assert_eq!(
        control
            .bounded(Duration::from_secs(1), async { Ok(7) })
            .await
            .unwrap(),
        7
    );
    send.send(()).unwrap();
    for _ in 0..2 {
        let error = control
            .bounded(
                Duration::from_secs(1),
                std::future::pending::<anyhow::Result<()>>(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }
}

#[tokio::test]
async fn d2b1_1165_each_step_gets_a_new_budget_and_the_run_deadline_still_caps_it() {
    let mut limits = timeouts();
    limits.step_ms = 2000.try_into().unwrap();
    let mut control = Control::new(limits, std::future::pending());
    control
        .bounded(ms(limits.step_ms), async { Ok(()) })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    control
        .bounded(ms(limits.step_ms), async {
            tokio::task::yield_now().await;
            Ok(())
        })
        .await
        .unwrap();
    control.end = Deadline::now() - Duration::from_secs(1);
    let polled = std::cell::Cell::new(false);
    let error = control
        .bounded(ms(limits.step_ms), async {
            polled.set(true);
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(!polled.get(), "an expired run polled the next work future");
    assert!(error.to_string().contains("deadline"));
}

#[test]
fn d2b1_1165_grant_refresh_keeps_prior_elapsed_cost_and_the_stronger_outcome() {
    let mut phase = Phase::new("replay_apply", "hop");
    phase.elapsed_ms = 60000;
    phase.distance = Some(7);
    phase.changes = Some(3);
    phase.outcome = Status::Unanswerable;
    phase
        .findings
        .push(crate::output::Finding::error("test.cleanup", "unconfirmed"));
    phase.record(Instant::now(), "test.infrastructure", &Ok(()));
    assert!(phase.elapsed_ms >= 60000);
    assert_eq!(phase.distance, Some(7));
    assert_eq!(phase.changes, Some(3));
    assert!(matches!(phase.outcome, Status::Unanswerable));
    assert_eq!(phase.findings.len(), 1);
    let mut good = Phase::new("bootstrap", "base");
    good.record(Instant::now(), "test.plan", &Ok(()));
    assert!(matches!(good.outcome, Status::Passed));
}

#[test]
fn d2b1_1165_start_cleanup_preserves_primary_resource_and_remedy_evidence() {
    let mut data = TestData::new(false);
    data.phases.push(Phase::new("provision", "owned"));
    let error = test_scratch::Error::StartCleanup {
        cause: "engine startup failed".into(),
        cleanup: Box::new(test_scratch::CleanupFailure {
            run_id: "the-owned-run".into(),
            kind: test_scratch::FailureKind::Unreadable,
            resources: vec!["the-volume".into()],
            detail: "runtime inspection failed".into(),
            remedy: "inspect the recorded container and volume".into(),
        }),
    };
    data.startup_failure(0, &error);
    let report = data.finish();
    assert!(matches!(
        report.result,
        crate::output::Outcome::Unanswerable
    ));
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.id == "test.infrastructure" && f.message.contains("engine startup failed"))
    );
    assert!(report.findings.iter().any(|f| f.id == "test.cleanup"
        && f.message.contains("the-volume")
        && f.remedy.as_deref() == Some("inspect the recorded container and volume")));
    let text = crate::output::human(&report.findings);
    assert!(text.contains("the-volume") && text.contains("inspect the recorded"));
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["data"]["run_id"], "the-owned-run");
    assert!(json["data"]["cleanup_confirmed"] == false);
    let mut clean = TestData::new(false);
    clean.phases.push(Phase::new("provision", "owned"));
    clean.startup_failure(
        0,
        &test_scratch::Error::Start("failed, cleanup confirmed".into()),
    );
    assert!(
        !clean
            .finish()
            .findings
            .iter()
            .any(|f| f.id == "test.cleanup")
    );
}

#[test]
fn d2b1_1165_every_unavailable_identity_has_a_reason_and_no_plan_payload() {
    let mut data = TestData::new(false);
    data.run_id = Some("owned".into());
    data.phases.push(Phase::new("candidate_apply", "candidate"));
    let json = serde_json::to_value(data.finish()).unwrap();
    for object in
        std::iter::once(&json["data"]).chain(json["data"]["phases"].as_array().unwrap().iter())
    {
        for (name, value) in object.as_object().unwrap() {
            if value.is_null() {
                assert!(
                    !object["unavailable"][name]
                        .as_str()
                        .unwrap_or("")
                        .is_empty(),
                    "{name}: {object}"
                );
            }
        }
        assert!(object.get("plan").is_none());
    }
    let mut cleanup = TestData::new(true);
    cleanup.cleanup_confirmed = true;
    let json = serde_json::to_value(cleanup.finish()).unwrap();
    assert!(
        json["data"]["unavailable"]["input"]
            .as_str()
            .unwrap()
            .contains("not applicable")
    );
    assert_eq!(json["result"], "ok");
}

#[tokio::test]
async fn d2b1_1165_cancellation_before_acquisition_creates_no_run_record() {
    #[cfg(target_os = "linux")]
    {
        let stage = crate::adopt::Stage::new("d2b1-cancel").unwrap();
        let dir = test_scratch::StateDir::open(stage.path().join("runs")).unwrap();
        let mut limits = timeouts();
        limits.startup_ms = 1000.try_into().unwrap();
        let mut control = Control::new(limits, std::future::ready(Ok(())));
        let mut data = TestData::new(false);
        data.phases.push(Phase::new("provision", "owned"));
        assert!(
            control
                .start(&dir, "never-acquired", &mut data, 0)
                .await
                .is_none()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(data.run_id.is_none());
        assert!(
            data.phases[0]
                .findings
                .iter()
                .any(|f| f.message.contains("cancelled"))
        );
        assert!(matches!(data.phases[0].outcome, Status::Unanswerable));
    }
}

#[tokio::test]
async fn d2b1_1165_cancelled_work_retains_borrowed_progress_and_collection_is_idempotent() {
    use crate::deploy::test_run::{Progress, collect_progress};
    let progress = Progress::default();
    let (send, receive) = tokio::sync::oneshot::channel::<()>();
    let mut control = Control::new(timeouts(), async {
        receive.await.unwrap();
        Ok(())
    });
    let work = async {
        progress.accepted.set(true);
        progress.started.set(true);
        progress
            .findings
            .borrow_mut()
            .push(crate::output::Finding::note("apply.progress", "started"));
        send.send(()).unwrap();
        std::future::pending::<anyhow::Result<()>>().await
    };
    let error = control
        .bounded(Duration::from_secs(1), work)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    let mut phase = Phase::new("candidate_apply", "candidate");
    collect_progress(&mut phase, &progress);
    collect_progress(&mut phase, &progress);
    assert!(phase.accepted && phase.started);
    assert_eq!(phase.findings.len(), 1);
    assert_eq!(phase.findings[0].message, "started");
}

#[test]
fn d2b1_1165_owned_execution_future_frames_are_bounded() {
    macro_rules! frame {
        ($function:path; $($arg:ident),*) => {{
            fn bytes<$($arg,)* F: Future>(_: impl FnOnce($($arg),*) -> F) -> usize {
                size_of::<F>()
            }
            bytes($function)
        }};
    }
    for (name, bytes) in [
        ("control.start", frame!(Control::start; A, B, C, D, E)),
        ("control.provision", frame!(Control::provision; A, B, C, D)),
        ("control.bootstrap", frame!(Control::bootstrap; A, B, C, D)),
        ("control.plan", frame!(Control::plan; A, B, C, D)),
        ("control.apply", frame!(Control::apply; A, B, C, D, E, G)),
        ("control.grants", frame!(Control::grants; A, B, C, D)),
        ("control.finish", frame!(Control::finish; A, B, C)),
        ("service.provision", frame!(Session::new; A)),
        ("service.grants", frame!(Session::grants; A, B)),
        ("service.bootstrap", frame!(test_run::bootstrap; A, B, C)),
        ("service.plan", frame!(test_run::plan; A, B, C)),
        ("service.apply", frame!(test_run::apply; A, B, C, D, E, G)),
    ] {
        eprintln!("{name}: {bytes} bytes");
        // x86_64 Linux unboxed bounded service wrappers were 38–48 KiB.
        // Boxing only their service future reduces them below 1 KiB. 8 KiB
        // leaves platform room while rejecting every unboxed wrapper.
        // Other frames stay below 128 KiB, leaving
        // headroom under the production CLI worker's existing 8 MiB budget.
        let limit = match name {
            "control.provision" | "control.bootstrap" | "control.plan" | "control.apply"
            | "control.grants" => 8 * 1024,
            _ => crate::CLI_STACK_SIZE / 64,
        };
        assert!(
            bytes <= limit,
            "{name} grew to {bytes} bytes (limit {limit})"
        );
    }
}
