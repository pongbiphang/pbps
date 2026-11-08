use super::*;
use crate::plan_test::{
    execution::Control,
    report::{Assertion, TestData},
    session::Session,
};

#[derive(Clone, Copy)]
enum Case {
    Preservation,
    Budgets,
    Violation,
    Refusals,
    CancelProvision,
    CancelApply,
    CancelBootstrap,
    CancelPlan,
    /// The build fails and the lock cannot be released (#2547).
    BootstrapBoth,
    /// The same build failure with a working release: the negative control.
    BootstrapPrimary,
    /// The build commits and only the release fails.
    BootstrapCleanup,
}

fn limits() -> pbps_config::test_manifest::Timeouts {
    pbps_config::test_manifest::Timeouts {
        startup_ms: 120000.try_into().unwrap(),
        step_ms: 30000.try_into().unwrap(),
        fixture_ms: 30000.try_into().unwrap(),
        assertion_ms: 10000.try_into().unwrap(),
        run_ms: 180000.try_into().unwrap(),
        // The longest a single docker command may take (container.rs COMMAND).
        // A removal is several commands, and under load the daemon has stalled
        // a kill for 10 s or more (#2652); the product default stays 30 s.
        cleanup_ms: 120000.try_into().unwrap(),
    }
}

/// The family's cleanup allowance outlasts a daemon stall: a removal that takes
/// 35 s of the clock is confirmed under the limits every live case here runs
/// with, where the product default of 30 s is not (see the plan_test tests).
#[tokio::test(start_paused = true)]
async fn d2b1_1165_cleanup_allowance_outlasts_a_35_second_removal() {
    let control = Control::new(limits(), std::future::pending());
    let mut data = TestData::default();
    control
        .finish_within(
            async {
                tokio::time::sleep(std::time::Duration::from_secs(35)).await;
                Ok(())
            },
            &mut data,
        )
        .await;
    assert!(data.cleanup_confirmed);
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
    if matches!(case, Case::CancelBootstrap) {
        return cancel_bootstrap(scratch, control, data, send, &session, &base).await;
    }
    if matches!(
        case,
        Case::BootstrapBoth | Case::BootstrapPrimary | Case::BootstrapCleanup
    ) {
        return bootstrap_cleanup(scratch, control, data, &session, &base, case).await;
    }
    let mut phase = Phase::new("bootstrap", "base");
    control.bootstrap(&session, &base, &mut phase).await?;
    // Each of the build's catalog reads reports the scratch's PUBLIC grant
    // once; a second drain must not repeat any of them (#2694).
    ensure!(
        public_usage_warnings(&phase) == BOOTSTRAP_READS,
        "{:?}",
        phase.findings
    );
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
    if matches!(case, Case::CancelPlan) {
        return cancel_plan(scratch, control, data, send, &session, &candidate).await;
    }
    let mut phase = Phase::new("candidate_plan", "candidate");
    let planned = control
        .plan(&session, HopContext::first(&candidate), &mut phase)
        .await?;
    ensure!(
        public_usage_warnings(&phase) == PLAN_READS,
        "{:?}",
        phase.findings
    );
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
        Case::CancelProvision
        | Case::CancelBootstrap
        | Case::CancelPlan
        | Case::BootstrapBoth
        | Case::BootstrapPrimary
        | Case::BootstrapCleanup => unreachable!(),
    }
    ensure!(
        fixture.bytes()? == before,
        "owned services changed project inputs"
    );
    ensure!(!fixture.0.path().join("plan.json").exists());
    ensure!(!std::path::Path::new("in-memory test plan").exists());
    Ok(())
}

/// Catalog reads one bootstrap makes, measured on the engine.
const BOOTSTRAP_READS: usize = 3;
/// Catalog reads one plan makes; each reports the scratch's PUBLIC grant once.
const PLAN_READS: usize = 1;

fn public_usage_warnings(phase: &Phase) -> usize {
    phase
        .findings
        .iter()
        .filter(|f| {
            f.id == "catalog.warning" && f.message.contains("PUBLIC holds USAGE on schema `public`")
        })
        .count()
}

/// A phase whose cancellation arrived before it started has observed nothing:
/// no warning, and still failed without an answer.
async fn cancelled_before_any_warning(
    session: &Session<'_>,
    input: &Assembled,
    plan: bool,
) -> anyhow::Result<()> {
    let mut control = Control::new(limits(), async { Ok(()) });
    let mut phase = Phase::new("early", "cancelled");
    let error = if plan {
        control
            .plan(session, HopContext::first(input), &mut phase)
            .await
            .err()
    } else {
        control.bootstrap(session, input, &mut phase).await.err()
    };
    ensure!(
        error
            .context("a cancelled phase succeeded")?
            .to_string()
            .contains("cancelled"),
        "{:?}",
        phase.findings
    );
    ensure!(public_usage_warnings(&phase) == 0, "{:?}", phase.findings);
    ensure!(matches!(phase.outcome, Status::Unanswerable));
    Ok(())
}

/// Returns once the owned apply role waits on a lock: the service has then made
/// every catalog read that precedes the blocked work.
async fn apply_role_waits(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let mut monitor = admin(scratch).await?;
    let until = tokio::time::Instant::now() + Duration::from_secs(25);
    let sql = format!(
        "SELECT pid FROM pg_stat_activity WHERE usename='pbps_test_apply_{}' AND wait_event_type='Lock'",
        scratch.run_id()
    );
    loop {
        if !monitor.query(&sql).await?.is_empty() {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < until,
            "the phase never reached the blocked work"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Sends the cancellation once the owned apply role waits on a lock.
async fn cancel_when_blocked(
    scratch: &crate::test_scratch::TestScratch<'_>,
    send: tokio::sync::oneshot::Sender<()>,
) -> anyhow::Result<()> {
    apply_role_waits(scratch).await?;
    send.send(()).unwrap();
    Ok(())
}

async fn cancel_bootstrap(
    scratch: &crate::test_scratch::TestScratch<'_>,
    control: &mut Control,
    data: &mut TestData,
    send: tokio::sync::oneshot::Sender<()>,
    session: &Session<'_>,
    base: &Assembled,
) -> anyhow::Result<()> {
    cancelled_before_any_warning(session, base, false).await?;
    // The first table the build creates waits on a lock this test holds, which
    // is after the existing-state read has reported its warning.
    let mut holder = admin(scratch).await?;
    holder.execute("SELECT pg_advisory_lock(2694)").await?;
    let mut ddl = admin(scratch).await?;
    ddl.execute(concat!(
        "CREATE FUNCTION public.hold_2694() RETURNS event_trigger LANGUAGE plpgsql AS $$ ",
        "BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() ",
        "WHERE object_identity = 'app.orders') THEN PERFORM pg_advisory_xact_lock(2694); ",
        "END IF; END $$; ",
        "CREATE EVENT TRIGGER hold_2694 ON ddl_command_end WHEN TAG IN ('CREATE TABLE') ",
        "EXECUTE FUNCTION public.hold_2694()"
    ))
    .await?;
    let mut phase = Phase::new("bootstrap", "base");
    let (result, observed) = tokio::join!(
        control.bootstrap(session, base, &mut phase),
        cancel_when_blocked(scratch, send)
    );
    holder.execute("SELECT pg_advisory_unlock(2694)").await?;
    observed?;
    ensure!(result.unwrap_err().to_string().contains("cancelled"));
    ensure!(phase.entry.is_none());
    ensure!(
        public_usage_warnings(&phase) == 1,
        "the cancelled bootstrap lost or repeated its warning: {:?}",
        phase.findings
    );
    ensure!(matches!(phase.outcome, Status::Unanswerable));
    data.phases.push(phase);
    Ok(())
}

const CAUSE_2547: &str = "bootstrap cause 2547";

/// The build fails at its first table and/or the deployment lock cannot be
/// released. The release is made to fail by taking DELETE on the lock table
/// from its owner while the build waits on a lock this test holds: a trigger or
/// rule on that table would be refused as an untrusted ledger before the
/// release, and the owner's privilege is not part of that check.
async fn bootstrap_cleanup(
    scratch: &crate::test_scratch::TestScratch<'_>,
    control: &mut Control,
    data: &mut TestData,
    session: &Session<'_>,
    base: &Assembled,
    case: Case,
) -> anyhow::Result<()> {
    let fails = !matches!(case, Case::BootstrapCleanup);
    let blocks_release = !matches!(case, Case::BootstrapPrimary);
    let mut holder = admin(scratch).await?;
    if blocks_release {
        holder.execute("SELECT pg_advisory_lock(2547)").await?;
    }
    let mut ddl = admin(scratch).await?;
    ddl.execute(&format!(
        concat!(
            "CREATE FUNCTION public.fault_2547() RETURNS event_trigger LANGUAGE plpgsql AS $$ ",
            "BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() ",
            "WHERE object_identity = 'app.orders') THEN {hold}{raise} END IF; END $$; ",
            "CREATE EVENT TRIGGER fault_2547 ON ddl_command_end WHEN TAG IN ('CREATE TABLE') ",
            "EXECUTE FUNCTION public.fault_2547()"
        ),
        hold = if blocks_release {
            "PERFORM pg_advisory_xact_lock(2547); "
        } else {
            ""
        },
        raise = if fails {
            format!("RAISE EXCEPTION '{CAUSE_2547}'; ")
        } else {
            String::new()
        },
    ))
    .await?;
    let revoke = format!(
        "REVOKE DELETE ON public.__pbps_lock FROM pbps_test_apply_{}",
        scratch.run_id()
    );
    let release = async {
        if blocks_release {
            // The lock table exists once the build waits; the apply role cannot
            // delete its row afterwards.
            apply_role_waits(scratch).await.context("waiting")?;
            ddl.execute(&revoke).await.context("revoke")?;
            holder
                .execute("SELECT pg_advisory_unlock(2547)")
                .await
                .context("unlock")?;
        }
        anyhow::Ok(())
    };
    let mut phase = Phase::new("bootstrap", "base");
    let (result, observed) = tokio::join!(control.bootstrap(session, base, &mut phase), release);
    observed.context("releasing the held build")?;
    let error = format!(
        "{:#}",
        result.err().context("the failed bootstrap succeeded")?
    );
    let ids = |id: &str| phase.findings.iter().filter(|f| f.id == id).count();
    if matches!(case, Case::BootstrapCleanup) {
        ensure!(phase.entry.is_some(), "the committed entry was lost");
        // The release error is the phase's error: nothing else failed.
        ensure!(error.contains("permission denied"), "{error}");
    } else {
        ensure!(phase.entry.is_none());
        ensure!(error.contains(CAUSE_2547), "{error}");
        ensure!(
            phase
                .findings
                .iter()
                .any(|f| f.message.contains(CAUSE_2547)),
            "the original cause left the report: {:?}",
            phase.findings
        );
    }
    if blocks_release {
        ensure!(ids("test.cleanup") == 1, "{:?}", phase.findings);
        ensure!(matches!(phase.outcome, Status::Unanswerable));
    } else {
        ensure!(ids("test.cleanup") == 0, "{:?}", phase.findings);
        ensure!(matches!(phase.outcome, Status::Failed));
    }
    data.phases.push(phase);
    Ok(())
}

async fn cancel_plan(
    scratch: &crate::test_scratch::TestScratch<'_>,
    control: &mut Control,
    data: &mut TestData,
    send: tokio::sync::oneshot::Sender<()>,
    session: &Session<'_>,
    candidate: &Assembled,
) -> anyhow::Result<()> {
    cancelled_before_any_warning(session, candidate, true).await?;
    // The exact data scope on `app.codes` is read after the catalog warning;
    // the lock holds that read.
    let mut holder = admin(scratch).await?;
    holder
        .execute("BEGIN; LOCK TABLE app.codes IN ACCESS EXCLUSIVE MODE")
        .await?;
    let mut phase = Phase::new("candidate_plan", "candidate");
    let (result, observed) = tokio::join!(
        control.plan(session, HopContext::first(candidate), &mut phase),
        cancel_when_blocked(scratch, send)
    );
    holder.execute("ROLLBACK").await?;
    observed?;
    ensure!(
        result
            .err()
            .context("a cancelled plan succeeded")?
            .to_string()
            .contains("cancelled")
    );
    ensure!(phase.checksum.is_none());
    ensure!(
        public_usage_warnings(&phase) == PLAN_READS,
        "the cancelled plan lost or repeated its warning: {:?}",
        phase.findings
    );
    ensure!(matches!(phase.outcome, Status::Unanswerable));
    data.phases.push(phase);
    Ok(())
}

// The run record may disappear during teardown. Capture only resource names,
// never the daemon endpoint, credentials or the adjacent fingerprint key (#2652).
#[derive(serde::Deserialize)]
struct CleanupResources {
    container: CleanupContainer,
    volumes: Vec<String>,
}

#[derive(serde::Deserialize)]
struct CleanupContainer {
    id: String,
    name: String,
}

fn cleanup_resources(path: &std::path::Path) -> anyhow::Result<CleanupResources> {
    let bytes = std::fs::read(path).context("owned resource record could not be read")?;
    serde_json::from_slice(&bytes).context("owned resource record could not be decoded")
}

fn confirm_cleanup(
    data: &TestData,
    run: &str,
    work: &anyhow::Result<()>,
    resources: &anyhow::Result<CleanupResources>,
) -> anyhow::Result<()> {
    if data.cleanup_confirmed {
        return Ok(());
    }
    let identity = match resources {
        Ok(resources) => format!(
            "container_id={}; container_name={}; volumes={:?}",
            resources.container.id, resources.container.name, resources.volumes
        ),
        Err(error) => format!("resource identity unavailable: {error:#}"),
    };
    // A cleanup error must not hide either the work failure or the existing
    // test.cleanup finding (including an elapsed timeout and its phase).
    anyhow::bail!(
        "cleanup failed at phase cleanup; run={run}; {identity}; work={work:?}; phases={}",
        serde_json::to_string(&data.phases)?
    )
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
    let resources = cleanup_resources(&registry.path().join(format!("{run}.json")));
    let outcome =
        boxed_future(|| exercise_control(&scratch, &mut control, &mut data, send, case)).await;
    control.finish(scratch, &mut data).await;
    confirm_cleanup(&data, &run, &outcome, &resources)?;
    ensure!(!registry.path().join(format!("{run}.key")).exists());
    ensure!(!registry.path().join(format!("{run}.json")).exists());
    outcome?;
    let report = data.finish();
    let expected = match case {
        Case::Preservation | Case::Budgets => crate::output::Outcome::Ok,
        Case::Violation | Case::Refusals => crate::output::Outcome::Findings,
        Case::CancelProvision
        | Case::CancelApply
        | Case::CancelBootstrap
        | Case::CancelPlan
        | Case::BootstrapBoth
        | Case::BootstrapCleanup => crate::output::Outcome::Unanswerable,
        Case::BootstrapPrimary => crate::output::Outcome::Findings,
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
    if matches!(
        case,
        Case::BootstrapBoth | Case::BootstrapPrimary | Case::BootstrapCleanup
    ) {
        let human = crate::output::human(&report.findings);
        let wire = json["findings"].to_string();
        let wants_cause = !matches!(case, Case::BootstrapCleanup);
        let wants_cleanup = !matches!(case, Case::BootstrapPrimary);
        for (what, text) in [("human", human), ("json", wire)] {
            ensure!(
                text.contains(CAUSE_2547) == wants_cause,
                "{what} cause: {text}"
            );
            ensure!(
                text.contains("deployment lock release failed") == wants_cleanup,
                "{what} cleanup: {text}"
            );
        }
        ensure!(
            json["findings"]
                .as_array()
                .context("findings")?
                .iter()
                .any(|f| f["id"] == "test.cleanup")
                == wants_cleanup,
            "the cleanup finding id: {json}"
        );
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
#[ignore = "needs docker and a pinned PostgreSQL scratch image"]
async fn d2694_pg18_cancelled_bootstrap_keeps_its_catalog_warning_once() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Case::CancelBootstrap,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and a pinned PostgreSQL scratch image"]
async fn d2694_pg18_cancelled_plan_keeps_its_catalog_warning_once() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::CancelPlan))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and a pinned PostgreSQL scratch image"]
async fn bootstrap_failure_and_unlock_failure_both_survive_in_the_report() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE", Case::BootstrapBoth))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and a pinned PostgreSQL scratch image"]
async fn a_bootstrap_failure_alone_reports_no_cleanup_finding() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Case::BootstrapPrimary,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and a pinned PostgreSQL scratch image"]
async fn a_committed_bootstrap_entry_survives_a_failed_lock_release() {
    Box::pin(lifecycle(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Case::BootstrapCleanup,
    ))
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

fn unconfirmed_cleanup(message: &str) -> TestData {
    let mut data = TestData::new(false);
    let mut phase = Phase::offline("cleanup", "owned resources");
    phase.outcome = Status::Unanswerable;
    phase
        .findings
        .push(crate::output::Finding::error("test.cleanup", message));
    data.phases.push(phase);
    data
}

#[test]
fn d2b1_2652_cleanup_failure_keeps_the_stage_work_and_only_owned_resource_identity() {
    let stage = crate::adopt::Stage::new("cleanup-diagnostic").unwrap();
    let path = stage.path().join("run.json");
    std::fs::write(&path, serde_json::json!({
        "container": {"id": "owned-container-2652", "name": "pbps-scratch-owned-2652", "password": "PRIVATE_CONTAINER_FIELD"},
        "volumes": ["owned-volume-2652"],
        "daemon": {"endpoint": "PRIVATE_DAEMON_ENDPOINT"},
        "fingerprint_key": "PRIVATE_KEY_FIELD"
    }).to_string()).unwrap();
    let resources = cleanup_resources(&path);
    // The snapshot must outlive record deletion on a partial/failed teardown.
    std::fs::remove_file(path).unwrap();
    let work = Err(anyhow::anyhow!("statement refused").context("candidate apply failed"));
    let data = unconfirmed_cleanup("volume inspection failed at runtime.users");
    let message = confirm_cleanup(&data, "run-2652", &work, &resources)
        .unwrap_err()
        .to_string();
    for expected in [
        "phase cleanup",
        "test.cleanup",
        "runtime.users",
        "run-2652",
        "owned-container-2652",
        "pbps-scratch-owned-2652",
        "owned-volume-2652",
        "candidate apply failed",
        "statement refused",
    ] {
        assert!(message.contains(expected), "missing {expected}: {message}");
    }
    for secret in [
        "PRIVATE_CONTAINER_FIELD",
        "PRIVATE_DAEMON_ENDPOINT",
        "PRIVATE_KEY_FIELD",
    ] {
        assert!(
            !message.contains(secret),
            "unselected record fields escaped"
        );
    }
}

#[test]
fn d2b1_2652_cleanup_timeout_keeps_the_existing_finding_and_successful_work_outcome() {
    let data = unconfirmed_cleanup(
        "owned cleanup was not confirmed: Err(Elapsed(())); run pbps test --cleanup",
    );
    let resources = Ok(CleanupResources {
        container: CleanupContainer {
            id: "container-2652".into(),
            name: "scratch-2652".into(),
        },
        volumes: vec![],
    });
    let message = confirm_cleanup(&data, "run-2652", &Ok(()), &resources)
        .unwrap_err()
        .to_string();
    for expected in [
        "test.cleanup",
        "Elapsed(())",
        "pbps test --cleanup",
        "work=Ok(())",
        "volumes=[]",
        "container-2652",
    ] {
        assert!(message.contains(expected), "missing {expected}: {message}");
    }
}

#[test]
fn d2b1_2652_unreadable_resource_identity_is_reported_without_failing_confirmed_cleanup() {
    let stage = crate::adopt::Stage::new("cleanup-unreadable").unwrap();
    let path = stage.path().join("run.json");
    let mut data = unconfirmed_cleanup("container inspection failed");
    for bytes in [None, Some("{}"), Some("not json")] {
        if let Some(bytes) = bytes {
            std::fs::write(&path, bytes).unwrap();
        }
        let resources = cleanup_resources(&path);
        assert!(resources.is_err());
        let message = confirm_cleanup(&data, "run-2652", &Ok(()), &resources)
            .unwrap_err()
            .to_string();
        assert!(message.contains("resource identity unavailable"));
        assert!(message.contains("container inspection failed"));
    }
    // A report/record observation must not change a confirmed teardown's result.
    data.cleanup_confirmed = true;
    let resources = cleanup_resources(&path);
    assert!(confirm_cleanup(&data, "run-2652", &Ok(()), &resources).is_ok());
}
