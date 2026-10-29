use super::*;
use crate::plan_test::{Revision, report::Status};
use anyhow::{Context as _, ensure};
use pbps_db::{Conn, Driver};
use pbps_diff::order::ArtifactView as _;
use pbps_model::{IdsFile, Intent, TableName};
use std::time::Duration;

struct Fixture(crate::adopt::Stage);
impl Fixture {
    fn new() -> anyhow::Result<Self> {
        let stage = crate::adopt::Stage::new("d2a1165")?;
        std::fs::create_dir(stage.path().join("schema"))?;
        std::fs::write(
            stage.path().join("pbps.yml"),
            concat!(
                "dialect: postgres\nunmanaged: ignore\nenvironments:\n",
                "  production: {url_env: D2A1165_MUST_NOT_BE_READ}\n",
                "resolve_with: borrowed\nresolvers:\n",
                "  borrowed: {kind: server, url_env: D2A1165_MUST_NOT_BE_READ}\n"
            ),
        )?;
        Ok(Self(stage))
    }
    fn input(&self, previous: &IdsFile, renamed: bool, extra: bool) -> anyhow::Result<Assembled> {
        let amount = if renamed { "total" } else { "amount" };
        let note = if extra { "  note: {type: text}\n" } else { "" };
        std::fs::write(
            self.0.path().join("schema/orders.yml"),
            format!(
                concat!(
                    "table: app.orders\ncolumns:\n",
                    "  id: {{type: bigint, nullable: false, identity: [1, 1]}}\n",
                    "  code: {{type: text, nullable: false}}\n",
                    "  {amount}: {{type: integer, nullable: false}}\n{note}",
                    "primary_key: {{name: orders_pk, columns: [id]}}\n"
                ),
                amount = amount,
                note = note
            ),
        )?;
        std::fs::write(
            self.0.path().join("schema/codes.yml"),
            concat!(
                "table: app.codes\ncolumns:\n  code: {type: text, nullable: false}\n",
                "primary_key: {name: codes_pk, columns: [code]}\ndata:\n",
                "  mode: exact\n  rows:\n    kept: {}\n"
            ),
        )?;
        let project = pbps_config::Project::load(&self.0.path().join("pbps.yml"))?;
        let mut loaded = crate::load(&project, &pbps_pg::Postgres::new())?;
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
                today: "2026-10-29".into(),
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
    fn bytes(&self) -> anyhow::Result<Vec<(std::path::PathBuf, Vec<u8>)>> {
        let mut files = pbps_load::schema_files(&self.0.path().join("schema"))?;
        files.push(self.0.path().join("pbps.yml"));
        Ok(files
            .into_iter()
            .map(|p| {
                let bytes = std::fs::read(&p).expect("the fixture just wrote this file");
                (p, bytes)
            })
            .collect())
    }
}
async fn admin(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<Conn> {
    Ok(Conn::connect(
        Driver::Postgres,
        &format!("{} dbname=pbps_test", scratch.connection()),
    )
    .await?)
}
async fn exercise(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    use pbps_pg::assertions::{self, AssertionResult};
    ensure!(scratch.acquired_image().await?.starts_with("sha256:"));
    let fixture = Fixture::new()?;
    let base = fixture.input(&IdsFile::default(), false, false)?;
    let candidate = fixture.input(&base.ids, true, false)?;
    let before = fixture.bytes()?;
    let session = Session::new(scratch, &base.loaded.schema).await?;
    ensure!(session.target().environment() == Some("test"));
    ensure!(base.config.select_resolver(None, None)?.is_some());
    ensure!(
        session
            .project(&base)
            .config
            .select_resolver(None, Some("test"))?
            .is_none(),
        "the owned context inherited a supplied-server resolver"
    );
    let key = pbps_cli::resolver::sealing::environment_key(&session.project(&base), Some("test"))?;
    ensure!(!key.id().as_str().is_empty());
    let key_path = session.project(&base).config.environments["test"]
        .fingerprint_key_file
        .clone()
        .unwrap();
    let key_bytes = std::fs::read(&key_path)?;
    ensure!(
        scratch.fingerprint_key().is_err(),
        "a second key write must not replace the pinned key"
    );
    ensure!(std::fs::read(&key_path)? == key_bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(std::fs::metadata(&key_path)?.permissions().mode() & 0o777 == 0o600);
    }
    eprintln!("D2a bootstrap starting");
    let entry = bootstrap(&session, &base).await?;
    eprintln!("D2a bootstrap committed");
    ensure!(entry > 0);
    session.grants(&base.loaded.schema).await?;
    let mut c = admin(scratch).await?;
    let apply_role = format!("pbps_test_apply_{}", scratch.run_id());
    let fixture_role = format!("pbps_test_fixture_{}", scratch.run_id());
    c.execute(&format!("CREATE FUNCTION app.d2a_helper() RETURNS integer LANGUAGE SQL RETURN 1; ALTER FUNCTION app.d2a_helper() OWNER TO {apply_role}")).await?;
    // This normal plan must load the private key to pin the non-superuser's helper.
    let mut fixtures = session.fixtures().await?;
    let role = fixtures.query("SELECT current_user::text AS role").await?;
    ensure!(role[0].try_get::<&str>("role")? == Some(fixture_role.as_str()));
    fixtures.execute("INSERT INTO app.orders(code,amount) VALUES ('kept',1164); CREATE TEMP TABLE ids AS SELECT id FROM app.orders; INSERT INTO app.orders(code,amount) SELECT 'temporary',1164 FROM ids; DELETE FROM app.orders WHERE code='temporary'").await?;
    let assume_apply = format!("SET ROLE {apply_role}");
    for sql in [
        "CREATE TABLE app.forbidden(n int)",
        "INSERT INTO app.codes VALUES ('forbidden')",
        "TRUNCATE app.orders",
        assume_apply.as_str(),
        "CREATE ROLE forbidden",
        "COPY (SELECT 1) TO PROGRAM 'true'",
    ] {
        let e = fixtures
            .execute(sql)
            .await
            .err()
            .with_context(|| format!("fixture unexpectedly admitted {sql}"))?;
        ensure!(
            e.server_error_code().as_deref() == Some("42501"),
            "{sql}: {e}"
        );
    }
    c.execute("CREATE TABLE app.forbidden(n int); DROP TABLE app.forbidden; COPY (SELECT 1) TO PROGRAM 'true'").await?;
    ensure!(fixtures.query("SELECT * FROM app.codes").await?.len() == 1);
    drop(fixtures);
    for (grant, revoke) in [
        (
            "GRANT CREATE ON SCHEMA app TO PUBLIC",
            "REVOKE CREATE ON SCHEMA app FROM PUBLIC",
        ),
        (
            "GRANT CREATE ON DATABASE pbps_test TO PUBLIC",
            "REVOKE CREATE ON DATABASE pbps_test FROM PUBLIC",
        ),
    ] {
        c.execute(grant).await?;
        let error = session
            .grants(&base.loaded.schema)
            .await
            .err()
            .context("broad permanent CREATE grant was admitted")?;
        ensure!(error.to_string().contains("PUBLIC grant"));
        c.execute(revoke).await?;
    }
    c.execute("GRANT INSERT ON app.codes TO PUBLIC").await?;
    let unsafe_grant = session
        .grants(&base.loaded.schema)
        .await
        .err()
        .context("broad PUBLIC grant was admitted")?;
    ensure!(unsafe_grant.to_string().contains("PUBLIC grant"));
    c.execute("REVOKE INSERT ON app.codes FROM PUBLIC").await?;
    let mut phase = Phase::new("candidate_plan", "rename");
    eprintln!("D2a candidate planning starting");
    let planned = plan(&session, &candidate, &mut phase).await?;
    eprintln!("D2a candidate sealed");
    ensure!(
        planned.data.changes == 1,
        "expected one recorded rename, got {}",
        planned.data.changes
    );
    ensure!(phase.statements == Some(planned.statements));
    ensure!(
        planned.artifact.wire().routine_pins.is_some(),
        "the unmanaged helper must be pinned with the private key"
    );
    let checksum = planned.artifact.checksum();
    let wire = planned.artifact.wire().wire_fingerprint();
    let mut refused = Phase::new("candidate_apply", "wrong checksum");
    let progress = Progress::default();
    let error = apply(
        &session,
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
        c.query("SELECT amount FROM app.orders").await?[0].try_get::<i32>("amount")? == Some(1164)
    );
    eprintln!("D2a accepted apply starting");
    let mut applied = Phase::new("candidate_apply", "accepted");
    let progress = Progress::default();
    apply(
        &session,
        &candidate,
        &planned,
        &mut applied,
        &checksum,
        &progress,
    )
    .await?;
    ensure!(progress.accepted.get() && progress.started.get());
    ensure!(applied.entry.is_some());
    ensure!(applied.findings.iter().any(|f| f.id == "apply.progress"));
    ensure!(
        wire == planned.artifact.wire().wire_fingerprint(),
        "execution changed the sealed wire"
    );
    session.grants(&candidate.loaded.schema).await?;
    for (sql, passes) in [
        ("SELECT total FROM app.orders WHERE total<>1164", true),
        ("SELECT total FROM app.orders WHERE total<>999", false),
    ] {
        let result = assertions::run(session.assertion(), sql, Duration::from_secs(10)).await;
        ensure!(
            matches!(
                (&result, passes),
                (AssertionResult::Pass(_), true) | (AssertionResult::Violation(_), false)
            ),
            "{result:?}"
        );
    }
    let denied = assertions::run(
        session.assertion(),
        "INSERT INTO app.orders(code,total) VALUES ('escape',1) RETURNING total",
        Duration::from_secs(10),
    )
    .await;
    ensure!(matches!(denied, AssertionResult::SqlError(_)), "{denied:?}");
    let boundary = assertions::run(session.assertion(), "COMMIT", Duration::from_secs(10)).await;
    ensure!(
        matches!(boundary, AssertionResult::BoundaryViolation),
        "{boundary:?}"
    );
    let mut empty = Phase::new("candidate_plan", "clean");
    let unchanged = plan(&session, &candidate, &mut empty).await?;
    ensure!(unchanged.data.changes == 0);
    let progress = Progress::default();
    apply(
        &session,
        &candidate,
        &unchanged,
        &mut empty,
        &unchanged.artifact.checksum(),
        &progress,
    )
    .await?;
    ensure!(progress.accepted.get() && !progress.started.get());
    for (path, bytes) in before {
        ensure!(std::fs::read(path)? == bytes);
    }
    let next = fixture.input(&candidate.ids, true, true)?;
    let mut phase = Phase::new("candidate_plan", "stale baseline");
    let stale = plan(&session, &next, &mut phase).await?;
    c.execute("ALTER TABLE app.orders ADD COLUMN outsider int")
        .await?;
    let progress = Progress::default();
    let error = apply(
        &session,
        &next,
        &stale,
        &mut phase,
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
    // The source tree is untouched; neither a plan payload nor a hidden script was published.

    ensure!(!fixture.0.path().join("plan.json").exists());
    ensure!(!std::path::Path::new("in-memory test plan").exists());
    Ok(())
}

async fn lifecycle(variable: &str) -> anyhow::Result<()> {
    let dir = crate::adopt::Stage::new("d2a1165-registry")?;
    let registry = crate::test_scratch::StateDir::open(dir.path().join("runs"))?;
    let image = std::env::var(variable).context(variable.to_owned())?;
    let scratch = crate::test_scratch::TestScratch::start(
        &registry,
        Driver::Postgres,
        &image,
        pbps_config::resolver::PullPolicy::Never,
    )
    .await?;
    let run = scratch.run_id().to_owned();

    // Keep the full lifecycle's phases off the small native test-thread stack.
    let outcome =
        tokio::time::timeout(Duration::from_secs(120), Box::pin(exercise(&scratch))).await;
    let cleanup = scratch.finish().await;
    cleanup.context("owned cleanup must succeed even after a test failure")?;
    ensure!(!registry.path().join(format!("{run}.key")).exists());
    ensure!(!registry.path().join(format!("{run}.json")).exists());
    outcome.context("lifecycle deadline")??;
    // A deadline before discovery creates neither a record nor a resource.
    let end = tokio::time::Instant::now() - Duration::from_millis(1);
    let refused = crate::test_scratch::TestScratch::start_bounded(
        &registry,
        Driver::Postgres,
        &image,
        pbps_config::resolver::PullPolicy::Never,
        Some(end),
        Duration::from_secs(30),
    )
    .await;
    match refused {
        Ok(s) => {
            s.finish().await?;
            anyhow::bail!("an expired startup deadline was ignored")
        }
        Err(e) => ensure!(e.to_string().contains("deadline"), "{e}"),
    }
    Ok(())
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg16_owns_credentials_and_executes_only_accepted_sealed_plans() {
    Box::pin(lifecycle("PBPS_TEST_PG_OLD_SCRATCH_IMAGE"))
        .await
        .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg18_owns_credentials_and_executes_only_accepted_sealed_plans() {
    Box::pin(lifecycle("PBPS_TEST_PG_SCRATCH_IMAGE"))
        .await
        .unwrap();
}

#[test]
fn d2a1165_cleanup_unanswerable_preserves_the_assertion_finding_in_the_existing_envelope() {
    use crate::plan_test::report::{Phase, TestData};
    let mut data = TestData::new(false);
    data.phases = vec![
        Phase::new("assertion", "invariant"),
        Phase::new("cleanup", "owned"),
    ];
    data.failure(0, "test.assertion.rows", "violation".into(), false);
    data.failure(1, "test.cleanup", "not confirmed".into(), true);
    // A later answered finding must not downgrade the phase's unanswerable fact.
    data.failure(1, "test.cleanup", "manual remedy".into(), false);
    let report = data.finish();
    assert!(matches!(
        report.result,
        crate::output::Outcome::Unanswerable
    ));
    assert_eq!(report.findings.len(), 3);
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["command"], "test");
    assert_eq!(value["data"]["phases"][1]["outcome"], "unanswerable");
    assert!(value.get("plan").is_none());
    let schema =
        serde_json::to_value(schemars::schema_for!(crate::output::Report<TestData>)).unwrap();
    assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&value));
    assert!(crate::output::human(&report.findings).contains("violation"));
}
#[test]
fn d2a1165_assertion_evidence_keeps_null_metadata_and_truncation_without_a_plan_payload() {
    use crate::plan_test::report::Assertion;
    use pbps_db::query::{EvidenceCell, QueryColumn, QueryEvidence};
    let mut assertion = Assertion {
        name: "n".into(),
        path: "a.sql".into(),
        sha256: "a".repeat(64),
        outcome: "violation".into(),
        columns: Vec::new(),
        rows: Vec::new(),
        truncated: false,
    };
    assertion.evidence(QueryEvidence {
        columns: vec![QueryColumn {
            name: "value".into(),
            type_name: "text".into(),
        }],
        rows: vec![
            vec![EvidenceCell::Null],
            vec![EvidenceCell::Text {
                value: "bounded".into(),
                truncated: true,
            }],
            vec![EvidenceCell::Unrenderable],
        ],
        truncated: true,
    });
    let value = serde_json::to_value(assertion).unwrap();
    assert!(value["rows"][0][0].is_null());
    assert_eq!(value["columns"][0]["type"], "text");
    assert_eq!(value["rows"][1][0]["truncated"], true);
    assert_eq!(value["rows"][2][0]["unrenderable"], true);
    assert_eq!(value["truncated"], true);
    assert!(matches!(
        Phase::new("assertion", "skipped").outcome,
        Status::Skipped
    ));
}

#[test]
fn d2a1165_a_run_with_no_cleanup_confirmation_cannot_produce_an_ok_envelope() {
    let mut data = crate::plan_test::report::TestData::new(false);
    data.run_id = Some("owned-run".into());
    let report = data.finish();
    assert!(matches!(
        report.result,
        crate::output::Outcome::Unanswerable
    ));
    assert!(report.findings.iter().any(|f| f.id == "test.cleanup"));
}

#[test]
fn d2a1165_confirmed_cleanup_keeps_a_successful_phase_in_the_existing_envelope() {
    let mut data = crate::plan_test::report::TestData::new(false);
    data.run_id = Some("owned".into());
    data.cleanup_confirmed = true;
    let mut phase = Phase::new("cleanup", "owned");
    phase.outcome = Status::Passed;
    data.phases.push(phase);
    let report = data.finish();
    assert!(matches!(report.result, crate::output::Outcome::Ok));
    assert_eq!(
        serde_json::to_value(report).unwrap()["data"]["phases"][0]["outcome"],
        "passed"
    );
}

#[test]
fn d2a1165_composed_service_futures_stay_below_256_kib() {
    // Measure storage without polling. Leave room to report an oversized
    // future as an assertion failure; the live tests keep the normal stack.
    let bytes = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let fixture = Fixture::new().unwrap();
            let input = fixture.input(&IdsFile::default(), false, false).unwrap();
            let project = pbps_config::Project {
                root: fixture.0.path().to_owned(),
                config: input.config.clone(),
            };
            let target = crate::db::Target::test_owned("never polled".into());
            let boot = super::super::bootstrap_async(
                &project,
                Some(&target),
                None,
                declarations(&input),
                crate::dialect(&project).unwrap(),
                "measure",
            );
            let bootstrap_size = std::mem::size_of_val(&boot);
            drop(boot);
            let artifact =
                crate::fixture_ordered(pbps_model::ChangeSet::default(), &pbps_pg::Postgres::new());
            let checksum = artifact.checksum();
            let allow = artifact.wire().changes.gated_risks();
            let request = super::super::ApplyRequest {
                plan_path: std::path::Path::new("not polled"),
                approved_checksum: &checksum,
                allow: &allow,
                staged: false,
                resume: false,
            };
            let dialect = crate::dialect(&project).unwrap();
            let apply = super::super::services::apply_async(
                &project,
                &target,
                &request,
                artifact.wire(),
                &checksum,
                &dialect,
                "measure",
                None,
            );
            let apply_size = std::mem::size_of_val(&apply);
            drop(apply);
            let mut findings = Vec::new();
            let future = super::super::plan_db_async(
                &project,
                &target,
                Declarations {
                    loaded: input.loaded,
                    ids: input.ids,
                },
                crate::dialect(&project).unwrap(),
                false,
                false,
                true,
                None,
                false,
                &mut findings,
                true,
            );
            (bootstrap_size, std::mem::size_of_val(&future), apply_size)
        })
        .unwrap()
        .join()
        .unwrap();
    eprintln!("bootstrap/plan/apply future bytes: {bytes:?}");
    assert!(
        bytes.0 < 256 * 1024 && bytes.1 < 256 * 1024 && bytes.2 < 256 * 1024,
        "composed bootstrap/plan/apply futures embed {bytes:?} bytes"
    );
}
