//! Owned prerequisites follow recorded identity, not names (#2552).
//!
//! Each scenario runs on PostgreSQL 16 and 18 against an owned scratch. The
//! catalog facts are read back from the engine: a rename is proved by the role
//! keeping its oid, never by its new name merely existing.
use super::*;
use crate::deploy::test_run::whole::plan_after;
use crate::plan_test::session::{HopContext, prerequisites::complete_renames};
use pbps_db::Param;
use pbps_model::{PolicyRoles, RoleName, RowPolicy};

#[derive(Clone, Copy)]
enum Scenario {
    /// A recorded rename and a second one across hops, on a stricter policy
    /// when `strict`, where a lookalike outsider must still be refused.
    Rename { strict: bool },
    /// A name that merely resembles a retained role is a new principal.
    Lookalike,
    /// Another principal already holds the destination name.
    Occupied,
    /// Neither name is in the cluster.
    Vanished,
    /// The role evidence cannot be read.
    Unreadable,
    /// An explicitly dropped role leaves nothing the next plan refuses.
    Dropped,
    /// A later hop's namespace, created ahead of its hop, is not unmanaged.
    Namespaces,
    /// A table renamed across hops while the run's own grants stand on it.
    TableRename,
}

fn orders(fixture: &Fixture) -> TableName {
    let (schema, name) = fixture.1.get().split_once('.').expect("a qualified name");
    TableName::new(schema, name)
}

fn reader(fixture: &Fixture, file: &str, name: &str) -> anyhow::Result<()> {
    std::fs::write(
        fixture.0.path().join(format!("schema/{file}.yml")),
        format!(
            "role: {name}\ngrants:\n  {}: [select]\n  schema::app: [usage]\n",
            fixture.1.get()
        ),
    )?;
    Ok(())
}

/// A hop declaring `name` as the reader, the orders policy aimed at it, with
/// the intents the caller records. `None` declares no reader at all.
fn hop(
    fixture: &Fixture,
    previous: &IdsFile,
    name: Option<&str>,
    mut intents: Vec<Intent>,
) -> anyhow::Result<Assembled> {
    match name {
        Some(name) => reader(fixture, "reader", name)?,
        None => std::fs::remove_file(fixture.0.path().join("schema/reader.yml"))?,
    }
    let mut input = fixture.input_with(previous, false, false, std::mem::take(&mut intents))?;
    if let Some(name) = name {
        input
            .loaded
            .schema
            .tables
            .get_mut(&orders(fixture))
            .context("orders")?
            .row_policies
            .insert(
                "readers".into(),
                RowPolicy {
                    to: PolicyRoles::Roles([RoleName::from(name)].into()),
                    ..Default::default()
                },
            );
    }
    Ok(input)
}

fn rename(from: &str, to: &str) -> Vec<Intent> {
    vec![Intent::RenameRole {
        from: from.into(),
        to: to.into(),
    }]
}

fn strict(fixture: &Fixture) -> anyhow::Result<()> {
    let config = fixture.0.path().join("pbps.yml");
    std::fs::write(
        &config,
        std::fs::read_to_string(&config)?.replace("unmanaged: ignore", "unmanaged: error"),
    )?;
    Ok(())
}

async fn oid(c: &mut Conn, role: &str) -> anyhow::Result<Option<i64>> {
    let rows = c
        .query_with(
            "SELECT oid::int8 AS oid FROM pg_catalog.pg_roles WHERE rolname=$1",
            &[Param::Str(role)],
        )
        .await?;
    Ok(rows
        .first()
        .map(|r| r.try_get::<i64>("oid"))
        .transpose()?
        .flatten())
}

async fn flag(c: &mut Conn, sql: &str) -> anyhow::Result<bool> {
    let rows = c.query(sql).await?;
    rows.first()
        .context("no row")?
        .try_get::<bool>("ok")?
        .context("NULL")
}

/// Plans and applies `incoming` after `previous`, as one replay hop does, then
/// refreshes the run's grants.
async fn step(
    session: &Session<'_>,
    previous: &Assembled,
    incoming: &Assembled,
) -> anyhow::Result<()> {
    let mut phase = Phase::new("replay_plan", "recorded rename");
    let planned = plan_after(session, HopContext::after(previous, incoming), &mut phase)
        .await
        .with_context(|| format!("findings: {:?}", phase.findings))?;
    apply(
        session,
        incoming,
        &planned,
        &mut phase,
        &planned.artifact.checksum(),
        &Progress::default(),
    )
    .await?;
    session.grants(&incoming.loaded.schema).await
}

async fn replanned_clean(session: &Session<'_>, settled: &Assembled) -> anyhow::Result<()> {
    let mut phase = Phase::new("replay_plan", "clean");
    let planned = plan_after(session, HopContext::after(settled, settled), &mut phase).await?;
    ensure!(planned.data.changes == 0, "replanning was not clean");
    Ok(())
}

/// The refusal of a hop the ordinary checks cannot accept, as text.
async fn refused(
    session: &Session<'_>,
    previous: &Assembled,
    incoming: &Assembled,
) -> anyhow::Result<String> {
    let mut phase = Phase::new("replay_plan", "refused");
    let result = plan_after(session, HopContext::after(previous, incoming), &mut phase).await;
    let error = result.err().context("the hop was admitted")?;
    Ok(format!(
        "{error:#} {}",
        serde_json::to_string(&phase.findings)?
    ))
}

/// The first hop: the reader `app_reader`, bootstrapped into a fresh scratch.
async fn started<'a>(
    scratch: &'a crate::test_scratch::TestScratch<'a>,
    fixture: &Fixture,
) -> anyhow::Result<(Session<'a>, Assembled, Conn)> {
    let first = hop(fixture, &IdsFile::default(), Some("app_reader"), Vec::new())?;
    let session = Session::new(scratch).await?;
    bootstrap(&session, &first, &mut Phase::new("bootstrap", "base")).await?;
    Ok((session, first, admin(scratch).await?))
}

/// Everything a rename must keep, read from the engine under the role's name.
async fn kept(c: &mut Conn, name: &str, held: i64, extras: bool) -> anyhow::Result<()> {
    ensure!(
        oid(c, name).await? == Some(held),
        "{name} is a new principal"
    );
    let alone = flag(
        c,
        "SELECT count(*) = 1 AS ok FROM pg_catalog.pg_roles \
         WHERE rolname LIKE 'app_reader%' AND rolname NOT LIKE '%_other'",
    )
    .await?;
    ensure!(
        alone,
        "a second reader principal exists after renaming to {name}"
    );
    let grant =
        format!("SELECT pg_catalog.has_table_privilege('{name}','app.orders','SELECT') AS ok");
    ensure!(flag(c, &grant).await?, "{name} lost its table grant");
    let policy = format!(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_policy p \
         WHERE '{name}'::pg_catalog.regrole::oid = ANY(p.polroles)) AS ok"
    );
    ensure!(flag(c, &policy).await?, "the policy lost its recipient");
    if extras {
        let held = format!(
            "SELECT (r.rolconnlimit = 7 AND EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m \
             WHERE m.member = r.oid AND m.roleid = 'app_group'::pg_catalog.regrole) \
             AND (SELECT nspowner FROM pg_catalog.pg_namespace WHERE nspname='keep') = r.oid) AS ok \
             FROM pg_catalog.pg_roles r WHERE r.rolname = '{name}'"
        );
        ensure!(
            flag(c, &held).await?,
            "{name} lost an attribute, a membership or a schema"
        );
    }
    Ok(())
}

async fn rename_chain(
    scratch: &crate::test_scratch::TestScratch<'_>,
    strict_policy: bool,
) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    if strict_policy {
        strict(&fixture)?;
    }
    let (session, first, mut c) = started(scratch, &fixture).await?;
    let held = oid(&mut c, "app_reader").await?.context("bootstrap role")?;
    // Attributes, a membership and a schema the role owns all follow the oid:
    // they are what a drop-and-create would lose. Strict policy would refuse
    // these unmanaged objects, so that variant carries none of them.
    if !strict_policy {
        c.execute(concat!(
            "ALTER ROLE app_reader CONNECTION LIMIT 7; CREATE ROLE app_group NOLOGIN; ",
            "GRANT app_group TO app_reader; CREATE SCHEMA keep AUTHORIZATION app_reader"
        ))
        .await?;
    }
    let second = hop(
        &fixture,
        &first.ids,
        Some("app_reader2"),
        rename("app_reader", "app_reader2"),
    )?;
    if strict_policy {
        // A near-match is no more owned for the recorded rename.
        let outsider = "app_reader2_other";
        c.execute(&format!("CREATE ROLE {outsider}")).await?;
        let why = refused(&session, &first, &second).await?;
        ensure!(why.contains(outsider), "{why}");
        c.execute(&format!("DROP ROLE {outsider}")).await?;
    }
    step(&session, &first, &second).await?;
    kept(&mut c, "app_reader2", held, !strict_policy).await?;
    replanned_clean(&session, &second).await?;
    // A second rename, in the next hop, is the same principal again.
    let third = hop(
        &fixture,
        &second.ids,
        Some("app_reader3"),
        rename("app_reader2", "app_reader3"),
    )?;
    step(&session, &second, &third).await?;
    kept(&mut c, "app_reader3", held, !strict_policy).await?;
    replanned_clean(&session, &third).await
}

async fn lookalike(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let (session, first, mut c) = started(scratch, &fixture).await?;
    c.execute("ALTER ROLE app_reader CONNECTION LIMIT 7")
        .await?;
    let held = oid(&mut c, "app_reader").await?;
    // The retained reader plus a second role whose name resembles it, with no
    // recorded rename between them.
    reader(&fixture, "lookalike", "app_reader2")?;
    let second = hop(&fixture, &first.ids, Some("app_reader"), Vec::new())?;
    step(&session, &first, &second).await?;
    ensure!(
        oid(&mut c, "app_reader").await? == held,
        "the original changed"
    );
    let created = oid(&mut c, "app_reader2").await?.context("not created")?;
    ensure!(Some(created) != held);
    let limit = "SELECT rolconnlimit = 7 AS ok FROM pg_catalog.pg_roles WHERE rolname='app_reader'";
    ensure!(flag(&mut c, limit).await?);
    Ok(())
}

async fn occupied(
    scratch: &crate::test_scratch::TestScratch<'_>,
    vanished: bool,
) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let (session, first, mut c) = started(scratch, &fixture).await?;
    if vanished {
        c.execute("DROP OWNED BY app_reader; DROP ROLE app_reader")
            .await?;
    } else {
        c.execute("CREATE ROLE app_reader2 NOLOGIN CONNECTION LIMIT 9")
            .await?;
    }
    let (old, other) = (
        oid(&mut c, "app_reader").await?,
        oid(&mut c, "app_reader2").await?,
    );
    let second = hop(
        &fixture,
        &first.ids,
        Some("app_reader2"),
        rename("app_reader", "app_reader2"),
    )?;
    let why = refused(&session, &first, &second).await?;
    let expected = if vanished {
        "no principal to rename"
    } else {
        "different principal"
    };
    ensure!(why.contains(expected), "{why}");
    // Nothing was altered or created on the way to the refusal.
    ensure!(oid(&mut c, "app_reader").await? == old);
    ensure!(oid(&mut c, "app_reader2").await? == other);
    if !vanished {
        let limit =
            "SELECT rolconnlimit = 9 AS ok FROM pg_catalog.pg_roles WHERE rolname='app_reader2'";
        ensure!(flag(&mut c, limit).await?);
    }
    Ok(())
}

async fn unreadable(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let (_session, _first, mut c) = started(scratch, &fixture).await?;
    let held = oid(&mut c, "app_reader").await?;
    let mut doomed = admin(scratch).await?;
    let pid = doomed
        .query("SELECT pg_catalog.pg_backend_pid()::int8 AS pid")
        .await?[0]
        .try_get::<i64>("pid")?
        .context("pid")?;
    c.execute(&format!("SELECT pg_catalog.pg_terminate_backend({pid})"))
        .await?;
    let renames = [(RoleName::from("app_reader"), RoleName::from("app_reader2"))];
    ensure!(
        complete_renames(&mut doomed, &renames).await.is_err(),
        "unreadable evidence was read as absence"
    );
    ensure!(oid(&mut c, "app_reader").await? == held);
    ensure!(oid(&mut c, "app_reader2").await?.is_none());
    Ok(())
}

/// Measured: under `unmanaged: error` a hop that drops a role is refused by the
/// ordinary plan itself (the role is live and undeclared, and dropping it first
/// is read as drift), owned or not; so the property is pinned under `ignore`.
async fn dropped(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let (session, first, _c) = started(scratch, &fixture).await?;
    let drop = vec![Intent::DropRole {
        role: "app_reader".into(),
        reason: pbps_model::Reason::new("retired")?,
    }];
    let second = hop(&fixture, &first.ids, None, drop)?;
    step(&session, &first, &second)
        .await
        .context("the dropping hop")?;
    replanned_clean(&session, &second)
        .await
        .context("the replan")
}

async fn namespaces(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    strict(&fixture)?;
    let (session, first, mut c) = started(scratch, &fixture).await?;
    std::fs::write(
        fixture.0.path().join("schema/later-table.yml"),
        "table: future.items\ncolumns:\n  id: {type: integer}\n",
    )?;
    let later = hop(&fixture, &first.ids, Some("app_reader"), Vec::new())?;
    // As the runner does at provision, for every revision it will replay.
    session.namespaces(&later.loaded.schema).await?;
    let there =
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname='future') AS ok";
    ensure!(flag(&mut c, there).await?);
    replanned_clean(&session, &first).await
}

async fn table_rename(scratch: &crate::test_scratch::TestScratch<'_>) -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let (session, first, mut c) = started(scratch, &fixture).await?;
    session.grants(&first.loaded.schema).await?;
    fixture.1.set("app.orders_v2");
    let table = Intent::RenameTable {
        from: TableName::new("app", "orders"),
        to: TableName::new("app", "orders_v2"),
    };
    let second = hop(&fixture, &first.ids, Some("app_reader"), vec![table])?;
    step(&session, &first, &second).await?;
    let mut writer = session.fixtures().await?;
    writer
        .execute("INSERT INTO app.orders_v2(code, amount) VALUES ('kept', 1)")
        .await?;
    let reads =
        "SELECT pg_catalog.has_table_privilege('app_reader','app.orders_v2','SELECT') AS ok";
    ensure!(flag(&mut c, reads).await?);
    replanned_clean(&session, &second).await
}

async fn run(variable: &str, scenario: Scenario) -> anyhow::Result<()> {
    let dir = crate::adopt::Stage::new("d2a1165-prerequisites")?;
    let registry = crate::test_scratch::StateDir::open(dir.path().join("runs"))?;
    let image = std::env::var(variable).context(variable.to_owned())?;
    let scratch = boxed_future(|| {
        crate::test_scratch::TestScratch::start(
            &registry,
            Driver::Postgres,
            &image,
            pbps_config::resolver::PullPolicy::Never,
        )
    })
    .await?;
    let work: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + '_>> =
        match scenario {
            Scenario::Rename { strict } => Box::pin(rename_chain(&scratch, strict)),
            Scenario::Lookalike => Box::pin(lookalike(&scratch)),
            Scenario::Occupied => Box::pin(occupied(&scratch, false)),
            Scenario::Vanished => Box::pin(occupied(&scratch, true)),
            Scenario::Unreadable => Box::pin(unreadable(&scratch)),
            Scenario::Dropped => Box::pin(dropped(&scratch)),
            Scenario::Namespaces => Box::pin(namespaces(&scratch)),
            Scenario::TableRename => Box::pin(table_rename(&scratch)),
        };
    let result = tokio::time::timeout(Duration::from_secs(180), work).await;
    scratch.finish().await.context("prerequisite cleanup")?;
    result.context("prerequisite deadline")??;
    Ok(())
}

macro_rules! live {
    ($($name:ident => $scenario:expr),* $(,)?) => {$(
        mod $name {
            use super::*;
            #[tokio::test]
            #[ignore = "needs docker and both pinned PostgreSQL scratch images"]
            async fn d2a1165_pg16() {
                Box::pin(run("PBPS_TEST_PG_OLD_SCRATCH_IMAGE", $scenario)).await.unwrap();
            }
            #[tokio::test]
            #[ignore = "needs docker and both pinned PostgreSQL scratch images"]
            async fn d2a1165_pg18() {
                Box::pin(run("PBPS_TEST_PG_SCRATCH_IMAGE", $scenario)).await.unwrap();
            }
        }
    )*};
}
live! {
    a_recorded_role_rename_keeps_the_principal_across_hops => Scenario::Rename { strict: false },
    a_recorded_role_rename_under_strict_unmanaged_still_refuses_a_lookalike => Scenario::Rename { strict: true },
    a_lookalike_without_a_recorded_rename_is_a_new_principal => Scenario::Lookalike,
    an_occupied_rename_destination_is_refused_and_both_principals_stay => Scenario::Occupied,
    a_rename_with_neither_name_present_creates_nothing => Scenario::Vanished,
    unreadable_role_evidence_changes_nothing => Scenario::Unreadable,
    a_dropped_role_leaves_nothing_for_the_next_plan_to_refuse => Scenario::Dropped,
    a_later_namespace_created_ahead_is_not_unmanaged => Scenario::Namespaces,
    a_table_renamed_while_the_run_grants_stand_on_it_is_planned_and_used => Scenario::TableRename,
}
