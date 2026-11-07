use super::*;
use crate::deploy::test_run::boxed_future;
use crate::plan_test::{
    Revision, execution::Control, revision::Assembled, session::HopContext, session::Session,
};
use anyhow::{Context as _, ensure};
use pbps_db::{DbError, Driver, LedgerError};
use pbps_model::{DropGuard, DropRequirement, IdsFile, TableName};
use std::time::{Duration, Instant};

fn input() -> anyhow::Result<Assembled> {
    let loaded = pbps_load::load_schema_files(&[(
        "orders.yml".into(),
        concat!(
            "table: app.orders\ncolumns:\n  code: {type: text, nullable: false}\n",
            "primary_key: {name: orders_pk, columns: [code]}\ndata:\n",
            "  mode: exact\n  rows:\n    kept: {}\n"
        )
        .into(),
    )])
    .map_err(|e| anyhow::anyhow!("declarations: {e:?}"))?;
    let ids = pbps_diff::resolve_with_annotations(
        &loaded.schema,
        &IdsFile::default(),
        &[],
        0,
        &pbps_diff::Context {
            operator: "fixture".into(),
            today: "2026-11-07".into(),
        },
    )
    .map_err(|e| anyhow::anyhow!("identities: {e:?}"))?
    .ids;
    Ok(Assembled {
        revision: Revision::Working,
        config: pbps_config::Config::parse(
            "dialect: postgres\nunmanaged: ignore\n",
            std::path::Path::new("pbps.yml"),
        )?,
        loaded,
        ids,
        files: Vec::new(),
    })
}

async fn terminate_blocked(session: &Session<'_>) -> anyhow::Result<()> {
    let mut monitor = session.admin().await?;
    // Only the owned apply login can be blocked here. Wait for the real SQL
    // read/lock, so termination yields 57P01 instead of a later closed socket.
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let rows = monitor
            .query(concat!(
                "SELECT pg_terminate_backend(pid) AS terminated FROM pg_stat_activity ",
                "WHERE datname=current_database() AND wait_event_type='Lock' ",
                "AND usename=(SELECT pg_get_userbyid(datdba) FROM pg_database ",
                "WHERE datname=current_database())"
            ))
            .await?;
        if let Some(row) = rows.first() {
            ensure!(rows.len() == 1 && row.try_get::<bool>("terminated")? == Some(true));
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < until,
            "no blocked apply backend"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn ledger_seam(session: &Session<'_>, lock: bool) -> anyhow::Result<anyhow::Error> {
    let mut conn = crate::db::connect(session.target()).await?;
    // Positive controls exercise each seam before its interrupted call.
    ensure!(crate::engine::latest(&mut conn).await?.is_some());
    crate::engine::lock(&mut conn, "classifier control").await?;
    ensure!(crate::engine::unlock(&mut conn).await?);
    let mut holder = session.admin().await?;
    holder
        .execute(if lock {
            "BEGIN; LOCK TABLE public.__pbps_lock IN ACCESS EXCLUSIVE MODE"
        } else {
            "BEGIN; LOCK TABLE public.__pbps_state IN ACCESS EXCLUSIVE MODE"
        })
        .await?;
    let interrupted = async {
        if lock {
            crate::engine::lock(&mut conn, "interrupted").await
        } else {
            crate::engine::latest(&mut conn).await.map(|_| ())
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(interrupted, terminate_blocked(session))
    })
    .await;
    holder.execute("ROLLBACK").await?;
    let (result, terminated) = result?;
    terminated?;
    let error = result.err().context("a terminated ledger call succeeded")?;
    ensure!(
        matches!(&error, LedgerError::Db(db) if db.server_error_code().as_deref() == Some("57P01")),
        "{error:#}"
    );
    Ok(error.into())
}

async fn guarded_drop(session: &Session<'_>, input: &Assembled) -> anyhow::Result<anyhow::Error> {
    let uid = input.ids.tables.keys().next().context("table identity")?;
    let mut statement = pbps_dialect::Statement::new("DROP TABLE app.orders");
    statement.drop_guard = Some(DropGuard {
        table_uid: uid.clone(),
        table: TableName::new("app", "orders"),
        detach_from: None,
        requirements: BTreeMap::from([(
            uid.clone(),
            DropRequirement {
                table_uid: uid.clone(),
                predicate: "code <> 'kept'".into(),
                required_at: "2026-11-07".into(),
                operator: "fixture".into(),
            },
        )]),
    });
    let mut conn = crate::db::connect(session.target()).await?;
    conn.execute("BEGIN").await?;
    let result = crate::engine::check_drop_requirement(
        &mut conn,
        &pbps_pg::admission::Admission::default(),
        &statement,
    )
    .await;
    conn.execute("ROLLBACK").await?;
    let error = result.err().context("the violating row permitted a drop")?;
    ensure!(
        matches!(&error, DbError::Refused(message) if message.contains("1 row")),
        "{error}"
    );
    Ok(error.into())
}

async fn exercise(
    scratch: &crate::test_scratch::TestScratch<'_>,
    control: &mut Control,
    data: &mut TestData,
) -> anyhow::Result<()> {
    let session = control.provision(scratch, data, 0).await?;
    let input = input()?;
    let mut bootstrap = Phase::on(Driver::Postgres, "bootstrap", "base");
    boxed_future(|| control.bootstrap(&session, &input, &mut bootstrap)).await?;
    ensure!(bootstrap.entry.is_some());
    data.phases.push(bootstrap);

    let started = Instant::now();
    let error = guarded_drop(&session, &input).await?;
    let mut phase = Phase::on(Driver::Postgres, "candidate_apply", "guarded drop");
    phase.record(started, "test.apply", &Err::<(), _>(error));
    ensure!(matches!(phase.outcome, Status::Failed));
    data.phases.push(phase);

    for lock in [false, true] {
        let started = Instant::now();
        let error = ledger_seam(&session, lock).await?;
        let mut phase = Phase::on(
            Driver::Postgres,
            if lock {
                "candidate_apply"
            } else {
                "candidate_plan"
            },
            if lock {
                "engine::lock"
            } else {
                "engine::latest"
            },
        );
        let cause = format!("{error:#}");
        phase.record(
            started,
            if lock { "test.apply" } else { "test.plan" },
            &Err::<(), _>(error),
        );
        ensure!(matches!(phase.outcome, Status::Unanswerable), "{cause}");
        ensure!(phase.findings[0].message == cause);
        ensure!(!phase.accepted && !phase.started && phase.entry.is_none());
        data.phases.push(phase);
    }

    // Reuse the ordinary owned planner and its reference-data read. A held
    // table lock places termination inside that read without a new service hook.
    let mut holder = session.admin().await?;
    holder
        .execute("BEGIN; LOCK TABLE app.orders IN ACCESS EXCLUSIVE MODE")
        .await?;
    let mut phase = Phase::on(
        Driver::Postgres,
        "candidate_plan",
        "terminated planning backend",
    );
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(
            boxed_future(|| control.plan(&session, HopContext::first(&input), &mut phase)),
            terminate_blocked(&session)
        )
    })
    .await;
    holder.execute("ROLLBACK").await?;
    let (planned, terminated) = result?;
    terminated?;
    let error = planned.err().context("terminated planning succeeded")?;
    ensure!(
        error.chain().any(|frame| {
            frame
                .downcast_ref::<Box<DbError>>()
                .is_some_and(|db| db.server_error_code().as_deref() == Some("57P01"))
        }),
        "planning lost its typed 57P01 cause: {error:#}"
    );
    ensure!(matches!(phase.outcome, Status::Unanswerable));
    ensure!(phase.checksum.is_none() && !phase.accepted && !phase.started);
    ensure!(
        phase
            .findings
            .iter()
            .any(|f| f.message == format!("{error:#}"))
    );
    data.phases.push(phase);
    Ok(())
}

async fn lifecycle(variable: &str) -> anyhow::Result<()> {
    let stage = crate::adopt::Stage::new("classifier2579")?;
    let registry = crate::test_scratch::StateDir::open(stage.path().join("runs"))?;
    let image = std::env::var(variable).with_context(|| variable.to_owned())?;
    let limits = pbps_config::test_manifest::Timeouts {
        startup_ms: 120000.try_into()?,
        step_ms: 30000.try_into()?,
        fixture_ms: 30000.try_into()?,
        assertion_ms: 10000.try_into()?,
        run_ms: 180000.try_into()?,
        cleanup_ms: 30000.try_into()?,
    };
    let mut control = Control::new(limits, std::future::pending());
    let mut data = TestData::new(false);
    data.phases
        .push(Phase::on(Driver::Postgres, "provision", "owned"));
    let scratch = control
        .start(&registry, &image, &mut data, 0)
        .await
        .context("start failed")?;
    let run = scratch.run_id().to_owned();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        boxed_future(|| exercise(&scratch, &mut control, &mut data)),
    )
    .await;
    control.finish(scratch, &mut data).await;
    ensure!(data.cleanup_confirmed, "owned cleanup failed");
    ensure!(!registry.path().join(format!("{run}.json")).exists());
    ensure!(!registry.path().join(format!("{run}.key")).exists());
    result??;
    // Publish the refusal by itself after the same confirmed cleanup, so the
    // later interruptions cannot hide an incorrectly unanswerable refusal.
    let mut refusal = TestData::new(false);
    refusal.run_id.clone_from(&data.run_id);
    refusal.cleanup_confirmed = data.cleanup_confirmed;
    refusal.phases.push(data.phases.remove(2));
    let refused = refusal.finish();
    ensure!(refused.result == crate::output::Outcome::Findings);
    let json = serde_json::to_value(&refused)?;
    ensure!(json["result"] == "findings" && json["data"]["cleanup_confirmed"] == true);
    ensure!(json["data"]["phases"][0]["outcome"] == "failed");
    ensure!(crate::output::human(&refused.findings).contains("1 row"));
    let report = data.finish();
    ensure!(report.result == crate::output::Outcome::Unanswerable);
    let json = serde_json::to_value(&report)?;
    ensure!(json["result"] == "unanswerable" && json["data"]["cleanup_confirmed"] == true);
    ensure!(json["data"]["phases"][1]["outcome"] == "passed");
    ensure!(json["data"]["phases"][1]["entry"].is_i64());
    ensure!(
        json["data"]["phases"]
            .as_array()
            .context("phases")?
            .last()
            .context("cleanup")?["outcome"]
            == "passed"
    );
    let human = crate::output::human(&report.data.as_ref().context("data")?.summary(true));
    ensure!(human.contains("unanswerable") && human.contains("cleanup confirmed: true"));
    ensure!(crate::output::human(&report.findings).contains("terminating connection"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs docker and the pinned PostgreSQL 16 scratch image"]
async fn pg16_interrupted_planning_and_ledger_seams_keep_the_cause_and_cleanup() {
    lifecycle("PBPS_TEST_PG_OLD_SCRATCH_IMAGE").await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker and the pinned PostgreSQL 18 scratch image"]
async fn pg18_interrupted_planning_and_ledger_seams_keep_the_cause_and_cleanup() {
    lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE").await.unwrap();
}
