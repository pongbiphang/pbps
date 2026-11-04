//! The owned SQL Server session on the pinned image: identities, fixture
//! framing, and the ordinary sealed plan and apply (ADR-0019 §3, §4, §6).
use super::*;
use crate::deploy::test_run::{Progress, apply, bootstrap, plan};
use crate::plan_test::{
    Revision,
    execution::Control,
    report::{Phase, TestData},
};
use pbps_model::{IdsFile, Intent, TableName};
use std::time::Duration;

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

async fn lifecycle(image: &str) -> anyhow::Result<()> {
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
    let scratch =
        crate::deploy::test_run::boxed_future(|| control.start(&registry, image, &mut data, 1))
            .await
            .context("the SQL Server scratch did not start")?;
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
            crate::deploy::test_run::boxed_future(|| exercise(&scratch, &session)),
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
    Box::pin(lifecycle(&image)).await.unwrap();
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
