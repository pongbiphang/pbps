//! The owned SQL Server session on the pinned image: identities, fixture
//! framing, and the ordinary sealed plan and apply (ADR-0019 §3, §4, §6).
use super::*;
use crate::deploy::test_run::whole::{bootstrap, plan};
use crate::deploy::test_run::{Progress, apply};
use crate::plan_test::{
    Revision,
    execution::Control,
    report::{Phase, TestData},
};
use pbps_db::query::AssertionResult;
use pbps_model::{IdsFile, Intent, TableName};
use std::time::Duration;

mod snapshot_tests;

struct Fixture(crate::adopt::Stage);
impl Fixture {
    fn new() -> anyhow::Result<Self> {
        let stage = crate::adopt::Stage::new("m1165")?;
        std::fs::create_dir(stage.path().join("schema"))?;
        std::fs::write(stage.path().join("pbps.yml"), "dialect: mssql\n")?;
        Ok(Self(stage))
    }
    fn input(&self, previous: &IdsFile, renamed: bool, extra: bool) -> anyhow::Result<Assembled> {
        let amount = if renamed { "total" } else { "amount" };
        let note = if extra {
            "  note: {type: nvarchar(20)}\n"
        } else {
            ""
        };
        let write = |name: &str, text: String| {
            std::fs::write(self.0.path().join("schema").join(name), text)
        };
        write(
            "orders.yml",
            format!(
                concat!(
                    "table: app.orders\ncolumns:\n",
                    "  id: {{type: int, nullable: false, identity: [1, 1]}}\n",
                    "  code: {{type: nvarchar(40), nullable: false}}\n",
                    "  {amount}: {{type: int, nullable: false}}\n{note}",
                    "primary_key: {{name: orders_pk, columns: [id]}}\n",
                    "unique:\n  orders_code: [code]\n"
                ),
                amount = amount,
                note = note
            ),
        )?;
        write(
            "lines.yml",
            concat!(
                "table: app.lines\ncolumns:\n  order_id: {type: int}\n  qty: {type: int}\n",
                "foreign_keys:\n  lines_order_fk:\n    columns: [order_id]\n",
                "    references: app.orders(id)\n"
            )
            .into(),
        )?;
        write(
            "codes.yml",
            concat!(
                "table: app.codes\ncolumns:\n  code: {type: nvarchar(40), nullable: false}\n",
                "primary_key: {name: codes_pk, columns: [code]}\ndata:\n",
                "  mode: exact\n  rows:\n    kept: {}\n"
            )
            .into(),
        )?;
        let project = pbps_config::Project::load(&self.0.path().join("pbps.yml"))?;
        let mut loaded = crate::load(&project, &pbps_mssql::Mssql)?;
        if renamed {
            loaded.intents.push(Intent::RenameColumn {
                table: TableName::new("app", "orders"),
                from: "amount".into(),
                to: "total".into(),
            });
        }
        let ids = pbps_diff::resolve_with_annotations(
            &loaded.schema,
            previous,
            &loaded.intents,
            loaded.intents.len(),
            &pbps_diff::Context {
                operator: "fixture".into(),
                today: "2026-11-04".into(),
            },
        )
        .map_err(|e| anyhow::anyhow!("identity fixture: {e:?}"))?
        .ids;
        Ok(Assembled {
            revision: Revision::Working,
            config: project.config,
            loaded,
            ids,
            files: Vec::new(),
        })
    }
}

/// The ADR §10 fixture, with a `GO` and a temporary procedure in the middle.
const FIXTURE: &str = "\
CREATE TABLE #ids(id int NOT NULL, code nvarchar(40) NOT NULL);
INSERT INTO app.orders(code, amount)
OUTPUT inserted.id, inserted.code INTO #ids(id, code)
VALUES (N'kept', 1164);
INSERT INTO app.lines(order_id, qty)
SELECT id, 2 FROM #ids WHERE code = N'kept';
GO
CREATE PROCEDURE #bump AS UPDATE app.lines SET qty = qty + 1;
GO
EXEC #bump;
-- a separator-looking line inside a string is data, not framing
INSERT INTO app.orders(code, amount) VALUES (N'a
GO
b', 1);
";

async fn code(conn: &mut Conn, sql: &str) -> anyhow::Result<String> {
    let error = conn
        .execute(sql)
        .await
        .err()
        .with_context(|| format!("the fixture login unexpectedly ran {sql}"))?;
    error
        .server_error_code()
        .with_context(|| format!("{sql}: no server error: {error}"))
}

async fn count(conn: &mut Conn, sql: &str) -> anyhow::Result<i32> {
    conn.query(sql).await?[0]
        .try_get::<i32>("n")?
        .context("NULL count")
}

async fn exercise(scratch: &TestScratch<'_>, session: &Session<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let base = fixture.input(&IdsFile::default(), false, false)?;
    let candidate = fixture.input(&base.ids, true, false)?;
    ensure!(session.target().environment() == Some("test"));
    ensure!(session.target().driver() == Driver::Mssql);
    let mut c = session.admin().await?;
    let mut phase = Phase::new("bootstrap", "base");
    let entry = bootstrap(session, &base, &mut phase).await?;
    ensure!(entry > 0 && phase.entry == Some(entry));
    session.grants(&base.loaded.schema).await?;

    // Identities: each connection is the login it says, and none is sysadmin.
    let mut fixtures = session.fixtures().await?;
    let who = fixtures
        .query("SELECT SUSER_SNAME() AS n, IS_SRVROLEMEMBER('sysadmin') AS s")
        .await?;
    ensure!(
        who[0].try_get::<&str>("n")?
            == Some(format!("pbps_test_fixture_{}", scratch.run_id()).as_str())
    );
    ensure!(who[0].try_get::<i32>("s")? == Some(0));
    let dbo = c
        .query("SELECT SUSER_SNAME(owner_sid) AS n FROM sys.databases WHERE name = DB_NAME()")
        .await?;
    ensure!(
        dbo[0].try_get::<&str>("n")?
            == Some(format!("pbps_test_apply_{}", scratch.run_id()).as_str())
    );

    // The ADR fixture: generated ids through #ids, a temporary procedure across
    // a GO, and a GO inside a string literal.
    session.run_fixture(&mut fixtures, FIXTURE, 10_000).await?;
    ensure!(count(&mut c, "SELECT COUNT(*) AS n FROM app.lines WHERE qty = 3").await? == 1);
    ensure!(
        count(
            &mut c,
            "SELECT COUNT(*) AS n FROM app.orders WHERE code = N'a'+CHAR(10)+N'GO'+CHAR(10)+N'b'"
        )
        .await?
            == 1
    );

    // The privilege boundary, by the engine's own error numbers.
    for (statement, errors) in [
        ("CREATE TABLE app.forbidden(n int)", &["262"][..]),
        ("CREATE PROCEDURE app.forbidden AS SELECT 1", &["262"]),
        ("CREATE SCHEMA forbidden", &["15247", "2759"]),
        ("INSERT INTO app.codes VALUES (N'forbidden')", &["229"]),
        (
            "INSERT INTO app.orders(id, code, amount) VALUES (99, N'x', 1)",
            &["544"],
        ),
        ("SET IDENTITY_INSERT app.orders ON", &["1088"]),
        ("TRUNCATE TABLE app.orders", &["1088"]),
        ("ALTER TABLE app.orders ADD forbidden int", &["1088"]),
        (
            "CREATE LOGIN forbidden WITH PASSWORD = N'F0rbidden!pw1'",
            &["15247"],
        ),
        (
            "SELECT * FROM OPENROWSET(BULK '/etc/hostname', SINGLE_CLOB) AS x",
            &["4834"],
        ),
    ] {
        let got = code(&mut fixtures, statement).await?;
        ensure!(errors.contains(&got.as_str()), "{statement}: error {got}");
    }
    // The control: the administrator may read that file, so the refusal above
    // is the login's, not the path's.
    c.query("SELECT * FROM OPENROWSET(BULK '/etc/hostname', SINGLE_CLOB) AS x")
        .await?;
    ensure!(
        count(
            &mut c,
            "SELECT COUNT(*) AS n FROM sys.tables WHERE name = N'forbidden'"
        )
        .await?
            == 0
    );

    // A failing statement, an unsupported client command and an open transaction
    // are each refused as the fixture's own failure.
    let mut again = session.fixtures().await?;
    let fk = session
        .run_fixture(
            &mut again,
            "INSERT INTO app.lines(order_id, qty) VALUES (9999, 1)",
            10_000,
        )
        .await
        .err()
        .context("a foreign key violation passed")?;
    ensure!(
        fk.downcast_ref::<pbps_db::DbError>()
            .and_then(|e| e.server_error_code())
            .as_deref()
            == Some("547")
    );
    let go_count = session
        .run_fixture(&mut again, "DELETE app.lines\nGO 2\n", 10_000)
        .await;
    ensure!(go_count.is_err() && count(&mut c, "SELECT COUNT(*) AS n FROM app.lines").await? == 1);
    let open = session
        .run_fixture(
            &mut again,
            "BEGIN TRANSACTION; INSERT INTO app.orders(code, amount) VALUES (N'open', 1);",
            10_000,
        )
        .await
        .err()
        .context("an open transaction passed")?;
    ensure!(open.to_string().contains("transaction"), "{open:#}");
    drop(again);
    ensure!(
        count(
            &mut c,
            "SELECT COUNT(*) AS n FROM app.orders WHERE code = N'open'"
        )
        .await?
            == 0
    );
    drop(fixtures);

    // Effective grants are read, not assumed: a broad grant the refresh does not
    // own is found and refused. A direct grant on a managed table is the refresh's
    // own to reset, so a stale or missing one is repaired instead.
    let run = scratch.run_id();
    let (fixture_user, assertion_user) = (
        format!("pbps_test_fixture_{run}"),
        format!("pbps_test_assertion_{run}"),
    );
    for (grant, revoke, repaired) in [
        (
            "GRANT CREATE TABLE TO public".to_owned(),
            "REVOKE CREATE TABLE FROM public".to_owned(),
            false,
        ),
        (
            "GRANT INSERT ON app.codes TO public".to_owned(),
            "REVOKE INSERT ON app.codes FROM public".to_owned(),
            false,
        ),
        (
            format!("GRANT ALTER ON SCHEMA::app TO {fixture_user}"),
            format!("REVOKE ALTER ON SCHEMA::app FROM {fixture_user}"),
            false,
        ),
        (
            format!("ALTER ROLE db_datawriter ADD MEMBER {assertion_user}"),
            format!("ALTER ROLE db_datawriter DROP MEMBER {assertion_user}"),
            false,
        ),
        (
            format!("GRANT UPDATE ON app.orders TO {assertion_user}"),
            "SELECT 1".to_owned(),
            true,
        ),
        (
            format!("REVOKE INSERT ON app.orders FROM {fixture_user}"),
            "SELECT 1".to_owned(),
            true,
        ),
    ] {
        c.execute(&grant).await?;
        let refresh = session.grants(&base.loaded.schema).await;
        if repaired {
            ensure!(refresh.is_ok(), "{grant}: {refresh:?}");
        } else {
            let error = refresh
                .err()
                .with_context(|| format!("{grant} was admitted"))?;
            ensure!(error.to_string().contains("boundary"), "{grant}: {error:#}");
        }
        c.execute(&revoke).await?;
    }
    session.grants(&base.loaded.schema).await?;

    // The server-level boundary is read too: an extra server right, or a login
    // that can enter another user database, is refused (and the controls pass).
    let ids = pbps_mssql::test_scratch::Identities::for_run(run);
    let mut master = Conn::connect(Driver::Mssql, scratch.connection()).await?;
    pbps_mssql::test_scratch::verify_server(&mut master, &ids).await?;
    master
        .execute(&format!("GRANT VIEW SERVER STATE TO [{}]", ids.fixture))
        .await?;
    let wider = pbps_mssql::test_scratch::verify_server(&mut master, &ids).await;
    ensure!(wider.is_err_and(|e| e.to_string().contains("VIEW SERVER STATE")));
    master
        .execute(&format!("REVOKE VIEW SERVER STATE FROM [{}]", ids.fixture))
        .await?;
    master.execute("CREATE DATABASE other1165").await?;
    master
        .execute(&format!(
            "ALTER AUTHORIZATION ON DATABASE::other1165 TO [{}]",
            ids.assertion
        ))
        .await?;
    let other = pbps_mssql::test_scratch::verify_server(&mut master, &ids).await;
    ensure!(other.is_err_and(|e| e.to_string().contains("other1165")));
    master.execute("DROP DATABASE other1165").await?;
    pbps_mssql::test_scratch::verify_server(&mut master, &ids).await?;
    drop(master);

    // The candidate: ordinary sealed plan, wrong checksum, accepted apply.
    let mut phase = Phase::new("candidate_plan", "rename");
    let planned = plan(session, &candidate, &mut phase).await?;
    ensure!(
        planned.data.changes == 1,
        "expected one rename, got {}",
        planned.data.changes
    );
    let checksum = planned.artifact.checksum();
    let wire = planned.artifact.wire().wire_fingerprint();
    let mut refused = Phase::new("candidate_apply", "wrong checksum");
    let progress = Progress::default();
    let error = apply(
        session,
        &candidate,
        &planned,
        &mut refused,
        &"0".repeat(64),
        &progress,
    )
    .await
    .err()
    .context("wrong checksum was admitted")?;
    ensure!(format!("{error:#}").contains("checksum"));
    ensure!(!progress.accepted.get() && !progress.started.get());
    ensure!(
        count(
            &mut c,
            "SELECT COUNT(*) AS n FROM app.orders WHERE amount = 1164"
        )
        .await?
            == 1
    );
    let mut applied = Phase::new("candidate_apply", "accepted");
    let progress = Progress::default();
    apply(
        session,
        &candidate,
        &planned,
        &mut applied,
        &checksum,
        &progress,
    )
    .await?;
    ensure!(progress.accepted.get() && progress.started.get() && applied.entry.is_some());
    ensure!(
        wire == planned.artifact.wire().wire_fingerprint(),
        "execution changed the sealed wire"
    );
    session.grants(&candidate.loaded.schema).await?;

    // The assertion login sees the renamed column, and cannot write.
    let mut reader = Conn::connect(Driver::Mssql, session.assertion()).await?;
    ensure!(
        count(
            &mut reader,
            "SELECT COUNT(*) AS n FROM app.orders WHERE total = 1164"
        )
        .await?
            == 1
    );
    ensure!(
        code(
            &mut reader,
            "INSERT INTO app.orders(code, total) VALUES (N'escape', 1)"
        )
        .await?
            == "229"
    );
    drop(reader);

    // Nothing left to do is an accepted empty plan; a baseline that moved is refused.
    let mut empty = Phase::new("candidate_plan", "clean");
    let unchanged = plan(session, &candidate, &mut empty).await?;
    ensure!(unchanged.data.changes == 0);
    let next = fixture.input(&candidate.ids, true, true)?;
    let mut stale_phase = Phase::new("candidate_plan", "stale baseline");
    let stale = plan(session, &next, &mut stale_phase).await?;
    c.execute("ALTER TABLE app.orders ADD outsider int").await?;
    let progress = Progress::default();
    let error = apply(
        session,
        &next,
        &stale,
        &mut stale_phase,
        &stale.artifact.checksum(),
        &progress,
    )
    .await
    .err()
    .context("stale baseline was admitted")?;
    let message = format!("{error:#}");
    ensure!(
        message.contains("drift") || message.contains("baseline"),
        "{message}"
    );
    ensure!(!progress.started.get());
    ensure!(!fixture.0.path().join("plan.json").exists());
    Ok(())
}

/// What the phases recorded. A start that fails records its cause as a finding
/// and returns nothing, so a test that reports only "did not start" hides the
/// one thing that tells a crash from a clash from a missing image (#2683).
fn recorded(data: &TestData) -> String {
    let all: Vec<String> = data
        .phases
        .iter()
        .flat_map(|phase| phase.findings.iter())
        .map(|finding| format!("{}: {}", finding.id, finding.message))
        .collect();
    if all.is_empty() {
        "no finding was recorded".to_owned()
    } else {
        all.join("; ")
    }
}

#[derive(Clone, Copy)]
enum Case {
    Identities,
    Sealed,
    Snapshot,
    RawSnapshotNames,
}

async fn lifecycle(image: &str, case: Case) -> anyhow::Result<()> {
    let dir = crate::adopt::Stage::new("m1165-registry")?;
    let registry = crate::test_scratch::StateDir::open(dir.path().join("runs"))?;
    let timeouts = pbps_config::test_manifest::Timeouts {
        startup_ms: 180_000.try_into()?,
        step_ms: 120_000.try_into()?,
        fixture_ms: 30_000.try_into()?,
        assertion_ms: 10_000.try_into()?,
        run_ms: 600_000.try_into()?,
        cleanup_ms: 30_000.try_into()?,
    };
    let mut control = Control::new(timeouts, std::future::pending()).with_driver(Driver::Mssql);
    let mut data = TestData::new(false);
    data.phases.push(Phase::new("input", "none"));
    data.phases
        .push(Phase::new("provision", "owned SQL Server scratch"));
    let started =
        crate::deploy::test_run::boxed_future(|| control.start(&registry, image, &mut data, 1))
            .await;
    let scratch = started
        .with_context(|| format!("the SQL Server scratch did not start: {}", recorded(&data)))?;
    ensure!(scratch.driver() == Driver::Mssql);
    let run = scratch.run_id().to_owned();
    let outcome = async {
        let session = control.provision(&scratch, &mut data, 1).await?;
        ensure!(
            data.acquired_image
                .as_deref()
                .is_some_and(|i| i.starts_with("sha256:"))
        );
        let build = data.engine_build.as_deref().context("no engine build")?;
        ensure!(
            build.contains("SQL Server") && build.contains("server collation"),
            "{build}"
        );
        tokio::time::timeout(
            Duration::from_secs(300),
            crate::deploy::test_run::boxed_future(|| async {
                match case {
                    Case::Identities => exercise(&scratch, &session).await,
                    Case::Sealed => exercise_sealed(&session).await,
                    Case::Snapshot => snapshot_tests::exercise_snapshot(&session).await,
                    Case::RawSnapshotNames => snapshot_tests::raw_table_name(&session).await,
                }
            }),
        )
        .await
        .context("lifecycle deadline")?
    }
    .await;
    control.finish(scratch, &mut data).await;
    ensure!(data.cleanup_confirmed, "owned cleanup was not confirmed");
    ensure!(!registry.path().join(format!("{run}.key")).exists());
    ensure!(!registry.path().join(format!("{run}.json")).exists());
    outcome
}

#[tokio::test]
#[ignore = "needs docker and the pinned SQL Server scratch image"]
async fn m1165_mssql_session_owns_least_privilege_identities_and_executes_only_accepted_sealed_plans()
 {
    let image = std::env::var("PBPS_TEST_DEV_IMAGE").unwrap();
    Box::pin(lifecycle(&image, Case::Identities)).await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker and the pinned SQL Server scratch image"]
async fn m1165_mssql_sealed_database_bounds_every_assertion_and_refuses_a_held_session() {
    let image = std::env::var("PBPS_TEST_DEV_IMAGE").unwrap();
    Box::pin(lifecycle(&image, Case::Sealed)).await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker and the pinned SQL Server scratch image"]
async fn m1165_mssql_snapshot_reads_typed_rows_and_the_engines_own_next_values_as_the_apply_login()
{
    let image = std::env::var("PBPS_TEST_DEV_IMAGE").unwrap();
    Box::pin(lifecycle(&image, Case::Snapshot)).await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker and the pinned SQL Server scratch image"]
async fn m1165_mssql_snapshot_serializes_raw_table_names() {
    let image = std::env::var("PBPS_TEST_DEV_IMAGE").unwrap();
    Box::pin(lifecycle(&image, Case::RawSnapshotNames))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs docker"]
async fn m1165_mssql_a_scratch_that_cannot_start_reports_its_recorded_cause() {
    // A PostgreSQL image under a SQL Server run cannot serve it, so the start
    // fails and records why; the error must carry that, not a fixed string.
    let error = lifecycle(
        "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
        Case::Identities,
    )
    .await
    .expect_err("an image of the other engine started");
    let text = format!("{error:#}");
    assert!(
        text.contains("did not start") && text.contains("test.infrastructure"),
        "{text}"
    );
}

fn kind(result: &AssertionResult) -> &'static str {
    match result {
        AssertionResult::Pass(_) => "pass",
        AssertionResult::Violation(_) => "violation",
        AssertionResult::InvalidResult { .. } => "invalid",
        AssertionResult::SqlError(_) => "sql_error",
        AssertionResult::BoundaryViolation => "boundary",
        AssertionResult::Unanswerable(_) => "unanswerable",
    }
}

async fn assert_kind(session: &Session<'_>, sql: &str, want: &str) -> anyhow::Result<()> {
    let result = session.run_assertion(sql, Duration::from_secs(10)).await;
    ensure!(kind(&result) == want, "{sql}: {result:?}");
    Ok(())
}

async fn assert_error_code(session: &Session<'_>, sql: &str, want: &str) -> anyhow::Result<()> {
    match session.run_assertion(sql, Duration::from_secs(10)).await {
        AssertionResult::SqlError(e) => {
            ensure!(e.server_error_code().as_deref() == Some(want), "{sql}: {e}");
            Ok(())
        }
        other @ (AssertionResult::Pass(_)
        | AssertionResult::Violation(_)
        | AssertionResult::InvalidResult { .. }
        | AssertionResult::BoundaryViolation
        | AssertionResult::Unanswerable(_)) => {
            anyhow::bail!("{sql}: expected error {want}, got {other:?}")
        }
    }
}

async fn mode(conn: &mut Conn) -> anyhow::Result<String> {
    Ok(conn
        .query("SELECT CONVERT(nvarchar(60), DATABASEPROPERTYEX(N'pbps_test', N'Updateability')) AS mode")
        .await?[0]
        .try_get::<&str>("mode")?
        .context("NULL mode")?
        .to_owned())
}

async fn exercise_sealed(session: &Session<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let base = fixture.input(&IdsFile::default(), false, false)?;
    bootstrap(session, &base, &mut Phase::new("bootstrap", "base")).await?;
    session.grants(&base.loaded.schema).await?;
    let mut loader = session.fixtures().await?;
    session.run_fixture(&mut loader, FIXTURE, 30_000).await?;
    drop(loader);
    // A writer the assertion login may execute: ownership chaining lets it
    // write where the caller's own table rights cannot, so only the database
    // mode stops it. `grants` would refuse this EXECUTE, so it is not re-run.
    let login = session.roles[2].clone();
    let mut admin = session.admin().await?;
    admin
        .execute(
            "CREATE PROCEDURE app.writer AS INSERT INTO app.orders(code, amount) VALUES (N'w', 1)",
        )
        .await?;
    admin
        .execute(&format!("GRANT EXECUTE ON app.writer TO [{login}]"))
        .await?;
    // The setup connection is closed: any open session blocks the seal.
    drop(admin);
    // The observer stays in `master`, so it is no session of the owned database.
    let mut admin = Conn::connect(Driver::Mssql, session.scratch.connection()).await?;
    let orders = "SELECT COUNT(*) AS n FROM pbps_test.app.orders";
    let before = count(&mut admin, orders).await?;

    // Before the seal an assertion connection is refused outright: nothing of
    // the file runs, so a write that the login could not make is not even tried.
    let early = session
        .run_assertion(
            "INSERT INTO app.orders(code, amount) VALUES (N'early', 1); SELECT 1 AS a WHERE 1 = 0",
            Duration::from_secs(10),
        )
        .await;
    ensure!(kind(&early) == "unanswerable", "{early:?}");
    ensure!(count(&mut admin, orders).await? == before);

    // A held session is refused, never killed, and the mode does not change.
    let mut held = session.fixtures().await?;
    let started = std::time::Instant::now();
    let refused = tokio::time::timeout(Duration::from_secs(20), session.seal())
        .await
        .context("the seal waited on a held session instead of refusing")?
        .err()
        .context("the seal succeeded over a held session")?;
    ensure!(
        started.elapsed() < Duration::from_secs(15)
            && format!("{refused:#}").contains("open session"),
        "{refused:#}"
    );
    ensure!(mode(&mut admin).await? == "READ_WRITE");
    ensure!(
        count(&mut held, "SELECT 1 AS n").await? == 1,
        "the held session was ended"
    );
    drop(held);

    session.seal().await?;
    ensure!(mode(&mut admin).await? == "READ_ONLY");
    // Sealing again is a no-op, not a failure.
    session.seal().await?;

    // Zero rows pass; any row, even of NULLs, is a violation.
    assert_kind(
        session,
        "SELECT code FROM app.orders WHERE code = N'kept' AND amount <> 1164",
        "pass",
    )
    .await?;
    assert_kind(
        session,
        "SELECT code FROM app.orders WHERE code = N'kept'",
        "violation",
    )
    .await?;
    assert_kind(session, "SELECT NULL AS x", "violation").await?;
    // Result shapes that are not one tabular result are invalid, not passes.
    assert_kind(session, "DECLARE @x int = 1", "invalid").await?;
    assert_kind(
        session,
        "SELECT 1 AS a WHERE 1 = 0; SELECT 2 AS b",
        "invalid",
    )
    .await?;
    // Temporary objects work in a read-only database; persistent writes do not.
    assert_kind(
        session,
        "CREATE TABLE #t(a int); INSERT #t VALUES (1); SELECT a FROM #t WHERE a = 2",
        "pass",
    )
    .await?;
    assert_error_code(session, "SELECT 1 / 0 AS x", "8134").await?;
    assert_error_code(
        session,
        "INSERT INTO app.orders(code, amount) VALUES (N'x', 1); SELECT 1 AS a WHERE 1 = 0",
        "229",
    )
    .await?;
    assert_error_code(
        session,
        "EXEC app.writer; SELECT 1 AS a WHERE 1 = 0",
        "3906",
    )
    .await?;
    // The batch separator is not interpreted; the engine refuses it.
    assert_error_code(
        session,
        "SELECT 1 AS a WHERE 1 = 0\nGO\nSELECT 1 AS a",
        "102",
    )
    .await?;
    // A statement that outlives its deadline never answered.
    let slow = session
        .run_assertion(
            "WAITFOR DELAY '00:00:05'; SELECT 1 AS a WHERE 1 = 0",
            Duration::from_secs(1),
        )
        .await;
    ensure!(kind(&slow) == "unanswerable", "{slow:?}");
    ensure!(
        count(&mut admin, orders).await? == before,
        "an assertion persisted a row"
    );
    Ok(())
}

const ADMIN: &str =
    "Server=127.0.0.1,14390;Database=master;User Id=sa;Password=P!9aa;TrustServerCertificate=true";

#[test]
fn a_login_connection_names_the_owned_server_database_and_only_that_login() {
    let url = super::mssql::as_login(ADMIN, "pbps_test_fixture_x", "secret").unwrap();
    assert_eq!(
        url,
        "Server=127.0.0.1,14390;Database=pbps_test;User Id=pbps_test_fixture_x;\
         Password=secret;TrustServerCertificate=true"
    );
    // The administrator's credentials never reach a login's string.
    assert!(!url.contains("sa;") && !url.contains("P!9aa"));
    let admin = super::mssql::admin_database(ADMIN).unwrap();
    assert!(admin.contains("Database=pbps_test;") && admin.contains("User Id=sa"));
}

#[test]
fn a_connection_that_is_not_the_owned_administrators_is_refused() {
    // No server to take the host from, and no master database to move out of.
    assert!(super::mssql::as_login("Database=master;User Id=sa", "l", "p").is_err());
    assert!(super::mssql::admin_database("Server=h,1;Database=pbps_test;User Id=sa").is_err());
    assert!(super::mssql::admin_database("host=127.0.0.1 port=1 dbname=postgres").is_err());
}
