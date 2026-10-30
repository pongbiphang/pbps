use super::*;
use crate::plan_test::{Revision, report::Status};
use anyhow::{Context as _, ensure};
use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect as _;
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
    let session = Session::new(scratch).await?;
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
    let mut bootstrap_phase = Phase::new("bootstrap", "base");
    let entry = bootstrap(&session, &base, &mut bootstrap_phase).await?;
    ensure!(bootstrap_phase.entry == Some(entry));
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

#[derive(Clone, Copy)]
enum Admission {
    Unmanaged,
    Defaults,
    DeclaredRoles,
    LaterRoles,
    PolicyRoles,
}
fn write_default_routines(fixture: &Fixture) -> anyhow::Result<()> {
    for (name, dependencies, language, body) in [
        ("fixture_leaf()", "", "sql", "RETURN 41"),
        (
            "fixture_default()",
            "depends_on: [app.fixture_leaf(), app.fixture_definer()]\n",
            "sql",
            "RETURN app.fixture_leaf() + app.fixture_definer()",
        ),
        (
            "fixture_definer()",
            "depends_on: [app.private_leaf()]\n",
            "sql",
            concat!(
                "SECURITY DEFINER SET search_path = app, pg_temp ",
                "RETURN app.private_leaf()"
            ),
        ),
        ("private_leaf()", "", "sql", "RETURN 1"),
        ("not_default()", "", "sql", "RETURN 99"),
        (
            "string_default()",
            "depends_on: [app.fixture_leaf()]\n",
            "sql",
            "AS $$ SELECT app.fixture_leaf() + 1 $$",
        ),
        (
            "plpgsql_default()",
            "depends_on: [app.fixture_leaf()]\n",
            "plpgsql",
            "AS $$ BEGIN RETURN app.fixture_leaf() + 1; END $$",
        ),
    ] {
        std::fs::write(
            fixture.0.path().join("schema").join(format!("{name}.yml")),
            format!(
                concat!(
                    "function: app.{name}\n{dependencies}",
                    "definition: () RETURNS integer LANGUAGE {language} IMMUTABLE {body}\n"
                ),
                name = name,
                dependencies = dependencies,
                language = language,
                body = body
            ),
        )?;
    }
    std::fs::write(
        fixture.0.path().join("schema/not_default_proc.yml"),
        concat!(
            "procedure: app.not_default_proc()\ndefinition: ",
            "() LANGUAGE sql BEGIN ATOMIC SELECT 1; END\n"
        ),
    )?;
    Ok(())
}

async fn prepare_reader_roles(
    scratch: &crate::test_scratch::TestScratch<'_>,
    fixture: &Fixture,
) -> anyhow::Result<()> {
    let mut c = Conn::connect(Driver::Postgres, scratch.connection()).await?;
    c.execute("CREATE ROLE app_existing NOLOGIN NOINHERIT CONNECTION LIMIT 7")
        .await?;
    for (i, name) in ["app_reader", "app\"reader", "app_existing"]
        .iter()
        .enumerate()
    {
        std::fs::write(
            fixture.0.path().join(format!("schema/role-{i}.yml")),
            format!(
                "role: {}\ngrants:\n  app.orders: [select]\n  schema::app: [usage]\n",
                serde_json::to_string(name)?
            ),
        )?;
    }
    Ok(())
}

async fn check_reader_role_attributes(
    scratch: &crate::test_scratch::TestScratch<'_>,
) -> anyhow::Result<()> {
    let mut c = admin(scratch).await?;
    for name in ["app_reader", "app\"reader", "app_existing"] {
        let rows = c
            .query_with(
                concat!(
                    "SELECT rolcanlogin, rolsuper, rolcreatedb, rolcreaterole, rolbypassrls, ",
                    "rolinherit, rolconnlimit FROM pg_catalog.pg_roles WHERE rolname=$1"
                ),
                &[pbps_db::Param::Str(name)],
            )
            .await?;
        ensure!(rows.len() == 1, "declared role missing: {name}");
        for flag in [
            "rolcanlogin",
            "rolsuper",
            "rolcreatedb",
            "rolcreaterole",
            "rolbypassrls",
        ] {
            ensure!(
                rows[0].try_get::<bool>(flag)? == Some(false),
                "unsafe role: {name} {flag}"
            );
        }
        if name == "app_existing" {
            ensure!(rows[0].try_get::<bool>("rolinherit")? == Some(false));
            ensure!(rows[0].try_get::<i32>("rolconnlimit")? == Some(7));
        }
    }
    Ok(())
}

async fn check_reader_grants(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let mut c = admin(scratch).await?;
    for role in ["app_reader", "app\"reader", "app_existing"] {
        let role = pbps_pg::Postgres::new().quote_ident(role)?;
        c.execute(&format!("SET ROLE {role}")).await?;
        c.execute("SELECT total FROM app.orders").await?;
        let e = c
            .execute("INSERT INTO app.orders(code,total) VALUES ('forbidden',1)")
            .await
            .err()
            .context("a declared reader could write")?;
        ensure!(e.server_error_code().as_deref() == Some("42501"));
        c.execute("RESET ROLE").await?;
    }
    Ok(())
}

async fn check_fixture_defaults(
    scratch: &crate::test_scratch::TestScratch<'_>,
    session: &Session<'_>,
    candidate: &mut crate::plan_test::revision::Assembled,
    fixtures: &mut Conn,
) -> anyhow::Result<()> {
    fixtures
        .execute("INSERT INTO app.orders(code) VALUES ('default')")
        .await?;
    ensure!(
        fixtures.query("SELECT amount FROM app.orders").await?[0].try_get::<i32>("amount")?
            == Some(42)
    );
    for sql in [
        "SELECT app.not_default()",
        "SELECT app.private_leaf()",
        "CALL app.not_default_proc()",
    ] {
        fixtures.execute(sql).await?;
    }
    // String and PLpgSQL bodies do not record their invoker callees
    // in pg_depend; both remain valid defaults for an ordinary writer.
    for routine in ["app.string_default()", "app.plpgsql_default()"] {
        candidate
            .loaded
            .schema
            .tables
            .get_mut(&TableName::new("app", "orders"))
            .unwrap()
            .columns
            .get_mut("amount")
            .unwrap()
            .value_source = pbps_model::ValueSource::Default(routine.into());
        let mut phase = Phase::new("candidate_plan", "opaque default");
        let planned = plan(session, candidate, &mut phase).await?;
        apply(
            session,
            candidate,
            &planned,
            &mut phase,
            &planned.artifact.checksum(),
            &Progress::default(),
        )
        .await?;
        session.grants(&candidate.loaded.schema).await?;
        fixtures
            .execute(&format!(
                "INSERT INTO app.orders(code) VALUES ('{routine}')"
            ))
            .await?;
    }
    // A domain's own default is not a column pg_attrdef at all.
    let mut c = admin(scratch).await?;
    c.execute(concat!(
        "CREATE DOMAIN app.amount_type AS integer DEFAULT app.fixture_leaf() CHECK (VALUE > 0); ",
        "CREATE TABLE app.domain_default (amount app.amount_type)"
    ))
    .await?;
    let fixture_role = format!("pbps_test_fixture_{}", scratch.run_id());
    c.execute(&format!(
        "GRANT INSERT, SELECT ON app.domain_default TO {fixture_role}"
    ))
    .await?;
    fixtures
        .execute("INSERT INTO app.domain_default DEFAULT VALUES")
        .await?;
    ensure!(
        fixtures
            .query("SELECT amount::integer AS amount FROM app.domain_default")
            .await?[0]
            .try_get::<i32>("amount")?
            == Some(41)
    );
    c.execute("DROP TABLE app.domain_default; DROP DOMAIN app.amount_type")
        .await?;
    ensure!(
        fixtures
            .query("SELECT amount FROM app.orders WHERE amount <> 42")
            .await?
            .is_empty()
    );
    let mut assertions = Conn::connect(Driver::Postgres, session.assertion()).await?;
    let error = assertions
        .execute("SELECT app.fixture_default()")
        .await
        .err()
        .context("assertion role received fixture EXECUTE")?;
    ensure!(error.server_error_code().as_deref() == Some("42501"));
    let mut c = admin(scratch).await?;
    let rows = c
        .query(concat!(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.aclexplode(",
            "coalesce(proacl, pg_catalog.acldefault('f', proowner))) a ",
            "WHERE a.grantee=0 AND a.privilege_type='EXECUTE') AS allowed ",
            "FROM pg_catalog.pg_proc WHERE oid='app.fixture_default()'::regprocedure"
        ))
        .await?;
    ensure!(rows[0].try_get::<bool>("allowed")? == Some(false));
    candidate
        .loaded
        .schema
        .tables
        .get_mut(&TableName::new("app", "orders"))
        .unwrap()
        .columns
        .get_mut("amount")
        .unwrap()
        .value_source = pbps_model::ValueSource::None;
    let mut phase = Phase::new("candidate_plan", "remove default");
    let planned = plan(session, candidate, &mut phase).await?;
    apply(
        session,
        candidate,
        &planned,
        &mut phase,
        &planned.artifact.checksum(),
        &Progress::default(),
    )
    .await?;
    session.grants(&candidate.loaded.schema).await?;
    fixtures.execute("SELECT app.fixture_default()").await?;
    Ok(())
}

async fn admission_steps<'a>(
    scratch: &'a crate::test_scratch::TestScratch<'a>,
    case: Admission,
) -> anyhow::Result<Box<AdmissionSetup<'a>>> {
    let routine_defaults = matches!(case, Admission::Defaults);
    let declared_roles = matches!(case, Admission::DeclaredRoles);
    let fixture = Fixture::new()?;
    if routine_defaults {
        write_default_routines(&fixture)?;
    } else {
        let config = fixture.0.path().join("pbps.yml");
        let text =
            std::fs::read_to_string(&config)?.replace("unmanaged: ignore", "unmanaged: error");
        std::fs::write(config, text)?;
    }
    if declared_roles {
        Box::pin(prepare_reader_roles(scratch, &fixture)).await?;
    }
    let base = fixture.input(&IdsFile::default(), false, false)?;
    let session = Session::new(scratch).await?;
    ensure!(session.project(&base).config.unmanaged == base.config.unmanaged);
    bootstrap(&session, &base, &mut Phase::new("bootstrap", "base")).await?;
    if declared_roles {
        Box::pin(check_reader_role_attributes(scratch)).await?;
    }
    let mut candidate = fixture.input(&base.ids, !routine_defaults, false)?;
    if routine_defaults {
        candidate
            .loaded
            .schema
            .tables
            .get_mut(&TableName::new("app", "orders"))
            .unwrap()
            .columns
            .get_mut("amount")
            .unwrap()
            .value_source = pbps_model::ValueSource::Default("app.fixture_default()".into());
    } else if matches!(case, Admission::Unmanaged) {
        // A near-match is not owned infrastructure, even with the run id.
        let outsider = format!("pbps_test_fixture_{}_other", scratch.run_id());
        let mut c = admin(scratch).await?;
        c.execute(&format!("CREATE ROLE {outsider}")).await?;
        let mut phase = Phase::new("candidate_plan", "unmanaged outsider");
        let refused = plan(&session, &candidate, &mut phase).await;
        ensure!(refused.is_err(), "unmanaged:error admitted an unowned role");
        let evidence = format!(
            "{:?} {}",
            refused.err(),
            serde_json::to_string(&phase.findings)?
        );
        ensure!(evidence.contains(&outsider), "{evidence}");
        c.execute(&format!("DROP ROLE {outsider}")).await?;
    }
    let mut phase = Phase::new("candidate_plan", "admitted");
    let planned = plan(&session, &candidate, &mut phase)
        .await
        .with_context(|| format!("candidate findings: {:?}", phase.findings))?;
    let progress = Progress::default();
    apply(
        &session,
        &candidate,
        &planned,
        &mut phase,
        &planned.artifact.checksum(),
        &progress,
    )
    .await?;
    ensure!(progress.accepted.get() && progress.started.get());
    session.grants(&candidate.loaded.schema).await?;
    Ok(Box::new(AdmissionSetup {
        _fixture: fixture,
        session,
        candidate,
    }))
}

struct AdmissionSetup<'a> {
    _fixture: Fixture,
    session: Session<'a>,
    candidate: crate::plan_test::revision::Assembled,
}
// Return setup before later assertions so its poll frame is gone when an
// assertion requests another plan through the test-only SQL oracle.
async fn admission_work(
    scratch: &crate::test_scratch::TestScratch<'_>,
    case: Admission,
) -> anyhow::Result<()> {
    let routine_defaults = matches!(case, Admission::Defaults);
    let declared_roles = matches!(case, Admission::DeclaredRoles);
    let mut setup = Box::pin(admission_steps(scratch, case)).await?;
    let session = &setup.session;
    let candidate = &mut setup.candidate;
    if declared_roles {
        Box::pin(check_reader_grants(scratch)).await?;
    }
    let mut fixtures = session.fixtures().await?;
    if routine_defaults {
        Box::pin(check_fixture_defaults(
            scratch,
            session,
            candidate,
            &mut fixtures,
        ))
        .await?;
    }
    let assume_apply = format!("SET ROLE pbps_test_apply_{}", scratch.run_id());
    for sql in [
        "CREATE TABLE app.forbidden(n integer)",
        "CREATE FUNCTION app.forbidden() RETURNS integer LANGUAGE sql RETURN 1",
        "CREATE ROLE forbidden",
        "CREATE DATABASE forbidden",
        assume_apply.as_str(),
    ] {
        let error = fixtures
            .execute(sql)
            .await
            .err()
            .with_context(|| format!("fixture could run {sql}"))?;
        ensure!(
            error.server_error_code().as_deref() == Some("42501"),
            "{sql}: {error}"
        );
    }
    let mut phase = Phase::new("candidate_plan", "clean");
    ensure!(plan(session, candidate, &mut phase).await?.data.changes == 0);
    Ok::<_, anyhow::Error>(())
}
// Keep independently composed scenarios in separate poll frames; one large
// assertion body otherwise exhausts the normal native test-thread stack.
async fn prerequisite_steps(
    scratch: &crate::test_scratch::TestScratch<'_>,
    policy: bool,
) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    if !policy {
        let config = fixture.0.path().join("pbps.yml");
        std::fs::write(
            &config,
            std::fs::read_to_string(&config)?.replace("unmanaged: ignore", "unmanaged: error"),
        )?;
    }
    let mut base = fixture.input(&IdsFile::default(), false, false)?;
    if policy {
        base.loaded
            .schema
            .tables
            .get_mut(&TableName::new("app", "orders"))
            .unwrap()
            .row_policies
            .insert(
                "base_policy".into(),
                pbps_model::RowPolicy {
                    to: pbps_model::PolicyRoles::Roles(["app_policy".into()].into()),
                    ..Default::default()
                },
            );
        ensure!(base.loaded.schema.roles.is_empty());
    }
    let session = Session::new(scratch).await?;
    bootstrap(&session, &base, &mut Phase::new("bootstrap", "base")).await?;
    if !policy {
        let mut c = admin(scratch).await?;
        ensure!(
            c.query("SELECT 1 FROM pg_catalog.pg_roles WHERE rolname='later_reader'")
                .await?
                .is_empty()
        );
        ensure!(
            c.query("SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname='future'")
                .await?
                .is_empty()
        );
        std::fs::write(
            fixture.0.path().join("schema/later-role.yml"),
            "role: later_reader\ngrants:\n  future.items: [select]\n  schema::future: [usage]\n",
        )?;
        std::fs::write(
            fixture.0.path().join("schema/later-table.yml"),
            "table: future.items\ncolumns:\n  id: {type: integer}\n",
        )?;
    }
    let mut candidate = fixture.input(&base.ids, true, false)?;
    if policy {
        let policies = &mut candidate
            .loaded
            .schema
            .tables
            .get_mut(&TableName::new("app", "orders"))
            .unwrap()
            .row_policies;
        *policies = base.loaded.schema.tables[&TableName::new("app", "orders")]
            .row_policies
            .clone();
        policies.insert(
            "later_policy".into(),
            pbps_model::RowPolicy {
                to: pbps_model::PolicyRoles::Roles(["later_policy_reader".into()].into()),
                ..Default::default()
            },
        );
    }
    let mut phase = Phase::new("candidate_plan", "later prerequisites");
    let planned = plan(&session, &candidate, &mut phase)
        .await
        .with_context(|| format!("candidate findings: {:?}", phase.findings))?;
    let progress = Progress::default();
    apply(
        &session,
        &candidate,
        &planned,
        &mut phase,
        &planned.artifact.checksum(),
        &progress,
    )
    .await?;
    ensure!(progress.accepted.get() && progress.started.get());
    if !policy {
        let mut c = admin(scratch).await?;
        c.execute("SET ROLE later_reader; SELECT id FROM future.items")
            .await?;
        let error = c
            .execute("INSERT INTO future.items(id) VALUES (1)")
            .await
            .err()
            .context("the later reader could write")?;
        ensure!(error.server_error_code().as_deref() == Some("42501"));
    }
    let mut phase = Phase::new("candidate_plan", "clean");
    ensure!(plan(&session, &candidate, &mut phase).await?.data.changes == 0);
    Ok(())
}
async fn admission_case(variable: &str, case: Admission) -> anyhow::Result<()> {
    let dir = crate::adopt::Stage::new("d2a1165-admission")?;
    let registry = crate::test_scratch::StateDir::open(dir.path().join("runs"))?;
    let image = std::env::var(variable).context(variable.to_owned())?;
    let scratch = crate::test_scratch::TestScratch::start(
        &registry,
        Driver::Postgres,
        &image,
        pbps_config::resolver::PullPolicy::Never,
    )
    .await?;
    let result = if matches!(case, Admission::LaterRoles | Admission::PolicyRoles) {
        tokio::time::timeout(
            Duration::from_secs(120),
            Box::pin(prerequisite_steps(
                &scratch,
                matches!(case, Admission::PolicyRoles),
            )),
        )
        .await
    } else {
        tokio::time::timeout(
            Duration::from_secs(120),
            Box::pin(admission_work(&scratch, case)),
        )
        .await
    };
    scratch
        .finish()
        .await
        .context("admission regression cleanup")?;
    result.context("admission regression deadline")??;
    Ok(())
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg16_unmanaged_error_excludes_only_owned_run_roles() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Admission::Unmanaged,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg18_unmanaged_error_excludes_only_owned_run_roles() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Admission::Unmanaged,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg16_fixture_defaults_execute_managed_routines_as_an_ordinary_writer() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Admission::Defaults,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg18_fixture_defaults_execute_managed_routines_as_an_ordinary_writer() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Admission::Defaults,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg16_declared_role_prerequisites_preserve_grants_and_existing_roles() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Admission::DeclaredRoles,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg18_declared_role_prerequisites_preserve_grants_and_existing_roles() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Admission::DeclaredRoles,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg16_later_revisions_prepare_only_their_role_and_namespace_prerequisites() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Admission::LaterRoles,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg16_policy_recipient_prerequisites_use_the_schema_named_role_inventory() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE",
        Admission::PolicyRoles,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg18_later_revisions_prepare_only_their_role_and_namespace_prerequisites() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Admission::LaterRoles,
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "needs docker and both pinned PostgreSQL scratch images"]
async fn d2a1165_pg18_policy_recipient_prerequisites_use_the_schema_named_role_inventory() {
    Box::pin(admission_case(
        "PBPS_TEST_PG_SCRATCH_IMAGE",
        Admission::PolicyRoles,
    ))
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
            let target = crate::db::Target::test_owned("never polled".into(), Vec::new());
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

#[test]
fn d2a1165_apply_failure_and_unlock_failure_both_survive_in_the_report() {
    let mut data = crate::plan_test::report::TestData::new(false);
    data.cleanup_confirmed = true;
    data.phases.push(Phase::new("candidate_apply", "candidate"));
    let error = finish_attempt(
        &mut data.phases[0],
        Attempt::Failed {
            error: anyhow::anyhow!("primary deployment cause"),
            released: Err(pbps_db::DbError::BadRow("unlock cause".into())),
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("primary deployment cause"),
        "{error:#}"
    );
    data.failure(0, "test.apply", error.to_string(), false);
    let report = data.finish();
    assert!(matches!(
        report.result,
        crate::output::Outcome::Unanswerable
    ));
    assert_eq!(report.findings.len(), 2);
    let json = serde_json::to_string(&report).unwrap();
    let human = crate::output::human(&report.findings);
    for text in [json, human] {
        assert!(text.contains("primary deployment cause"));
        assert!(text.contains("unlock cause"));
    }
}

#[test]
fn d2a1165_committed_entries_survive_lock_cleanup_failure() {
    let mut phase = Phase::new("bootstrap", "base");
    assert!(
        finish_attempt(
            &mut phase,
            Attempt::Applied {
                entry: 42,
                released: Err(pbps_db::DbError::BadRow("unlock cause".into())),
            }
        )
        .is_err()
    );
    assert_eq!(phase.entry, Some(42));
    assert!(matches!(phase.outcome, Status::Unanswerable));
    assert_eq!(phase.findings[0].id, "test.cleanup");
    let mut phase = Phase::new("candidate_apply", "candidate");
    finish_attempt(
        &mut phase,
        Attempt::Applied {
            entry: 43,
            released: Ok(true),
        },
    )
    .unwrap();
    assert_eq!(phase.entry, Some(43));
    assert!(phase.findings.is_empty());
    assert!(
        finish_attempt(
            &mut phase,
            Attempt::Failed {
                error: anyhow::anyhow!("primary alone"),
                released: Ok(true),
            }
        )
        .unwrap_err()
        .to_string()
        .contains("primary alone")
    );
}
