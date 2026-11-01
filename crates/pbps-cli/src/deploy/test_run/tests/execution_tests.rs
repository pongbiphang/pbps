use super::*;
use crate::plan_test::{
    execution::Control,
    report::{Assertion, TestData},
};

#[derive(Clone, Copy)]
enum Case {
    Preservation,
    Budgets,
    Violation,
    Refusals,
    CancelProvision,
    CancelApply,
}

fn limits() -> pbps_config::test_manifest::Timeouts {
    pbps_config::test_manifest::Timeouts {
        startup_ms: 120000.try_into().unwrap(),
        step_ms: 30000.try_into().unwrap(),
        fixture_ms: 30000.try_into().unwrap(),
        assertion_ms: 10000.try_into().unwrap(),
        run_ms: 180000.try_into().unwrap(),
        cleanup_ms: 30000.try_into().unwrap(),
    }
}

async fn exercise_control(
    scratch: &crate::test_scratch::TestScratch<'_>,
    control: &mut Control,
    data: &mut TestData,
    send: tokio::sync::oneshot::Sender<()>,
    case: Case,
) -> anyhow::Result<()> {
    if matches!(case, Case::CancelProvision) {
        send.send(()).unwrap();
        ensure!(control.provision(scratch, data, 0).await.is_err());
        let mut admin = Conn::connect(Driver::Postgres, scratch.connection()).await?;
        let rows = admin
            .query("SELECT datname FROM pg_database WHERE datname='pbps_test'")
            .await?;
        ensure!(
            rows.is_empty(),
            "cancelled provisioning reached the owned service"
        );
        ensure!(data.acquired_image.is_none());
        return Ok(());
    }
    let fixture = Fixture::new()?;
    let base = fixture.input(&IdsFile::default(), false, false)?;
    let candidate = fixture.input(&base.ids, true, true)?;
    let before = fixture.bytes()?;
    let session = control.provision(scratch, data, 0).await?;
    let mut phase = Phase::new("bootstrap", "base");
    control.bootstrap(&session, &base, &mut phase).await?;
    let entry = phase.entry.context("bootstrap entry absent")?;
    phase.elapsed_ms = 60000;
    control.grants(&session, &base, &mut phase).await?;
    ensure!(phase.entry == Some(entry) && phase.elapsed_ms >= 60000);
    data.phases.push(phase);
    let mut writer = session.fixtures().await?;
    writer
        .execute("INSERT INTO app.orders(code,amount) VALUES ('kept',1164)")
        .await?;
    drop(writer);
    let mut phase = Phase::new("candidate_plan", "candidate");
    let planned = control.plan(&session, &candidate, &mut phase).await?;
    let checksum = planned.artifact.checksum();
    ensure!(phase.checksum.as_deref() == Some(checksum.as_str()));
    data.candidate_sealed_checksum = Some(checksum.clone());
    data.phases.push(phase);
    let mut phase = Phase::new("candidate_apply", "candidate");
    match case {
        Case::Refusals => {
            let error = control
                .apply(&session, &candidate, &planned, &mut phase, &"f".repeat(64))
                .await
                .unwrap_err();
            ensure!(error.to_string().contains("checksum"), "{error:#}");
            ensure!(!phase.accepted && !phase.started && phase.entry.is_none());
            data.phases.push(phase);
            let mut admin = admin(scratch).await?;
            admin
                .execute("ALTER TABLE app.orders ADD COLUMN intruder int")
                .await?;
            let mut stale = Phase::new("candidate_apply", "stale baseline");
            let error = control
                .apply(&session, &candidate, &planned, &mut stale, &checksum)
                .await
                .unwrap_err();
            let message = format!("{error:#}");
            ensure!(
                message.contains("drift") || message.contains("baseline"),
                "{message}"
            );
            ensure!(stale.accepted && !stale.started && stale.entry.is_none());
            data.phases.push(stale);
        }
        Case::CancelApply => {
            // A reader allows the baseline/probes but holds ACCESS SHARE until
            // the DDL requests ACCESS EXCLUSIVE, after the real submission marker.
            let mut reader = admin(scratch).await?;
            reader.execute("BEGIN; SELECT * FROM app.orders").await?;
            let mut monitor = admin(scratch).await?;
            let watch = async {
                let until = tokio::time::Instant::now() + Duration::from_secs(25);
                loop {
                    // The emitted DDL batch starts with its scoped path setup,
                    // so pg_stat_activity does not show ALTER TABLE as its prefix.
                    let sql = format!(
                        "SELECT pid FROM pg_stat_activity WHERE usename='pbps_test_apply_{}' AND wait_event_type='Lock' AND ltrim(query) LIKE 'SET search_path = %ALTER TABLE%'",
                        scratch.run_id()
                    );
                    if !monitor.query(&sql).await?.is_empty() {
                        send.send(()).unwrap();
                        break;
                    }
                    ensure!(
                        tokio::time::Instant::now() < until,
                        "apply never reached the blocked DDL"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                anyhow::Ok(())
            };
            let (applied, observed) = tokio::join!(
                control.apply(&session, &candidate, &planned, &mut phase, &checksum),
                watch
            );
            reader.execute("ROLLBACK").await?;
            observed?;
            ensure!(applied.unwrap_err().to_string().contains("cancelled"));
            ensure!(phase.accepted && phase.started && phase.entry.is_none());
            ensure!(
                phase
                    .findings
                    .iter()
                    .any(|f| f.id == "apply.progress" && f.message.contains("Applying")),
                "cancelled future lost ordinary findings"
            );
            data.candidate_accepted_checksum = Some(checksum.clone());
            data.candidate_executed_checksum = Some(checksum);
            let rows = reader
                .query(concat!(
                    "SELECT column_name FROM information_schema.columns ",
                    "WHERE table_schema='app' AND table_name='orders' AND column_name='total'"
                ))
                .await?;
            ensure!(rows.is_empty(), "cancelled DDL committed");
            data.phases.push(phase);
        }
        Case::Preservation | Case::Budgets | Case::Violation => {
            if matches!(case, Case::Budgets) {
                tokio::time::sleep(Duration::from_secs(31)).await;
            }
            control
                .apply(&session, &candidate, &planned, &mut phase, &checksum)
                .await?;
            ensure!(phase.accepted && phase.started && phase.entry.is_some());
            let cost = phase.elapsed_ms;
            control.grants(&session, &candidate, &mut phase).await?;
            ensure!(phase.elapsed_ms >= cost);
            data.candidate_accepted_checksum = Some(checksum.clone());
            data.candidate_executed_checksum = Some(checksum);
            data.phases.push(phase);
            let expected = if matches!(case, Case::Violation) {
                999
            } else {
                1164
            };
            let sql = format!("SELECT code,total FROM app.orders WHERE total <> {expected}");
            let result =
                pbps_pg::assertions::run(session.assertion(), &sql, Duration::from_secs(10)).await;
            let mut assertion = Assertion {
                name: "preserved".into(),
                path: "assertion.sql".into(),
                sha256: "a".repeat(64),
                outcome: "pass".into(),
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
            };
            let mut phase = Phase::new("assertion", "preserved");
            phase.outcome = Status::Passed;
            match result {
                pbps_pg::assertions::AssertionResult::Pass(e)
                    if matches!(case, Case::Preservation | Case::Budgets) =>
                {
                    assertion.evidence(e)
                }
                pbps_pg::assertions::AssertionResult::Violation(e)
                    if matches!(case, Case::Violation) =>
                {
                    assertion.outcome = "violation".into();
                    assertion.evidence(e);
                    phase.outcome = Status::Failed;
                    phase.findings.push(crate::output::Finding::error(
                        "test.assertion.rows",
                        "preserved returned a violation row",
                    ));
                }
                other @ (pbps_pg::assertions::AssertionResult::Pass(_)
                | pbps_pg::assertions::AssertionResult::Violation(_)
                | pbps_pg::assertions::AssertionResult::InvalidResult { .. }
                | pbps_pg::assertions::AssertionResult::SqlError(_)
                | pbps_pg::assertions::AssertionResult::BoundaryViolation
                | pbps_pg::assertions::AssertionResult::Unanswerable(_)) => {
                    anyhow::bail!("unexpected assertion result: {other:?}")
                }
            }
            data.assertions.push(assertion);
            data.phases.push(phase);
        }
        Case::CancelProvision => unreachable!(),
    }
    ensure!(
        fixture.bytes()? == before,
        "owned services changed project inputs"
    );
    ensure!(!fixture.0.path().join("plan.json").exists());
    ensure!(!std::path::Path::new("in-memory test plan").exists());
    Ok(())
}

async fn lifecycle(variable: &str, case: Case) -> anyhow::Result<()> {
    let stage = crate::adopt::Stage::new("d2b1-execution")?;
    let registry = crate::test_scratch::StateDir::open(stage.path().join("runs"))?;
    let image = std::env::var(variable).context(variable.to_owned())?;
    let (send, receive) = tokio::sync::oneshot::channel::<()>();
    // Keep the sender alive through successful work; closing it is an error,
    // not a cancellation signal used to conceal a missed lifecycle observation.
    let mut limits = limits();
    if matches!(case, Case::Budgets) {
        limits.step_ms = 30000.try_into().unwrap();
    }
    let mut control = Control::new(limits, async {
        receive.await.map_err(std::io::Error::other)
    });
    let mut data = TestData::new(false);
    data.phases
        .push(Phase::new("provision", "owned PostgreSQL"));
    let scratch = boxed_future(|| control.start(&registry, &image, &mut data, 0))
        .await
        .ok_or_else(|| anyhow::anyhow!("owned start failed: {:?}", data.phases[0].findings))?;
    let run = scratch.run_id().to_owned();
    let outcome =
        boxed_future(|| exercise_control(&scratch, &mut control, &mut data, send, case)).await;
    control.finish(scratch, &mut data).await;
    ensure!(data.cleanup_confirmed, "cleanup failed");
    ensure!(!registry.path().join(format!("{run}.key")).exists());
    ensure!(!registry.path().join(format!("{run}.json")).exists());
    outcome?;
    let report = data.finish();
    let expected = match case {
        Case::Preservation | Case::Budgets => crate::output::Outcome::Ok,
        Case::Violation | Case::Refusals => crate::output::Outcome::Findings,
        Case::CancelProvision | Case::CancelApply => crate::output::Outcome::Unanswerable,
    };
    ensure!(
        report.result == expected,
        "{}",
        serde_json::to_string_pretty(&report)?
    );
    let json = serde_json::to_value(&report)?;
    ensure!(json["data"]["cleanup_confirmed"] == true);
    ensure!(json.get("plan").is_none());
    let schema = serde_json::to_value(schemars::schema_for!(crate::output::Report<TestData>))?;
    ensure!(jsonschema::validator_for(&schema)?.is_valid(&json));
    if !matches!(case, Case::Preservation | Case::Budgets) {
        ensure!(!crate::output::human(&report.findings).is_empty());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg16_owned_services_preserve_rows_and_confirm_cleanup() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Case::Preservation,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg18_owned_services_preserve_rows_and_confirm_cleanup() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::Preservation))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg16_violation_keeps_evidence_after_confirmed_cleanup() {
    Box::pin(lifecycle("PBPS_TEST_PG_OLD_SCRATCH_IMAGE", Case::Violation))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg18_violation_keeps_evidence_after_confirmed_cleanup() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::Violation))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg16_wrong_checksum_and_stale_baseline_keep_ordinary_refusals() {
    Box::pin(lifecycle("PBPS_TEST_PG_OLD_SCRATCH_IMAGE", Case::Refusals))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg18_wrong_checksum_and_stale_baseline_keep_ordinary_refusals() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::Refusals))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg16_cancellation_during_provision_reaches_no_sql() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Case::CancelProvision,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg18_cancellation_during_provision_reaches_no_sql() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Case::CancelProvision,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg16_cancelled_apply_retains_admission_submission_and_findings() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Case::CancelApply,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg18_cancelled_apply_retains_admission_submission_and_findings() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::CancelApply))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg16_plan_and_apply_have_independent_step_budgets() {
    Box::pin(lifecycle("PBPS_TEST_PG_OLD_SCRATCH_IMAGE", Case::Budgets))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2b1_1165_pg18_plan_and_apply_have_independent_step_budgets() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::Budgets))
        .await
        .unwrap();
}

#[test]
fn d2b1_1165_live_driver_future_frames_are_bounded() {
    fn frame2<A, B, F: std::future::Future>(_: impl FnOnce(A, B) -> F) -> usize {
        size_of::<F>()
    }
    fn frame5<A, B, C, D, E, F: std::future::Future>(_: impl FnOnce(A, B, C, D, E) -> F) -> usize {
        size_of::<F>()
    }
    for (name, bytes, limit) in [
        ("live.lifecycle", frame2(lifecycle), 16 * 1024),
        ("live.exercise", frame5(exercise_control), 64 * 1024),
    ] {
        eprintln!("{name}: {bytes} bytes");
        assert!(
            bytes <= limit,
            "{name} grew to {bytes} bytes (limit {limit})"
        );
    }
}

fn cli_lifecycle(variable: &'static str) {
    // Exercise the real production boundary separately: every other lifecycle
    // case still runs on the ordinary test-thread stack.
    crate::run_on_cli_thread(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(boxed_future(|| lifecycle(variable, Case::Preservation)))
    })
    .unwrap();
}

#[test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
fn d2b1_1165_pg16_cli_thread_executes_the_owned_service_boundary() {
    cli_lifecycle("PBPS_TEST_PG_OLD_SCRATCH_IMAGE");
}

#[test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
fn d2b1_1165_pg18_cli_thread_executes_the_owned_service_boundary() {
    cli_lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE");
}
