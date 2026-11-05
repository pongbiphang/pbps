//! The SQL Server half of [`Session`]. It holds connections and builds
//! connection strings; the SQL is `pbps_mssql::test_scratch`'s.
use super::{Conn, Driver, Session, TestScratch};
use anyhow::{Context as _, ensure};
use pbps_db::query::AssertionResult;
use pbps_model::Schema;
use pbps_mssql::test_scratch as sql;

/// The owned server's `Server=host,port` part, from the administrator's string.
fn server(admin: &str) -> anyhow::Result<&str> {
    admin
        .split(';')
        .find(|part| part.starts_with("Server="))
        .context("the owned SQL Server connection names no server")
}

/// A connection to the owned database as `login`.
pub(super) fn as_login(admin: &str, login: &str, password: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{};Database={};User Id={login};Password={password};TrustServerCertificate=true",
        server(admin)?,
        sql::DATABASE
    ))
}

/// The administrator's connection string, moved from `master` into the owned database.
pub(super) fn admin_database(admin: &str) -> anyhow::Result<String> {
    let moved = admin.replacen("Database=master", &format!("Database={}", sql::DATABASE), 1);
    ensure!(
        moved != admin,
        "the owned administrator connection names no master database"
    );
    Ok(moved)
}

pub(super) async fn admin(scratch: &TestScratch<'_>) -> anyhow::Result<Conn> {
    Ok(Conn::connect(Driver::Mssql, &admin_database(scratch.connection())?).await?)
}

fn identities(session: &Session<'_>) -> sql::Identities {
    let [apply, fixture, assertion] = session.roles.clone();
    sql::Identities {
        apply,
        fixture,
        assertion,
    }
}

pub(super) async fn new<'a>(scratch: &'a TestScratch<'a>) -> anyhow::Result<Session<'a>> {
    let admin = scratch.connection();
    let ids = sql::Identities::for_run(scratch.run_id());
    let passwords = [(); 3].map(|()| {
        pbps_mssql::resolver::scratch::password(&format!("{:032x}", rand::random::<u128>()))
    });
    let mut conn = Conn::connect(Driver::Mssql, admin).await?;
    sql::provision(
        &mut conn,
        &ids,
        [&passwords[0], &passwords[1], &passwords[2]],
    )
    .await?;
    sql::verify_server(&mut conn, &ids).await?;
    drop(conn);
    let mut conn = self::admin(scratch).await?;
    sql::create_users(&mut conn, &ids).await?;
    drop(conn);
    let key = scratch.fingerprint_key()?;
    Ok(Session {
        scratch,
        // The apply login is dbo of the owned database; no managed identity is
        // a role on SQL Server, so nothing is excluded from the catalog read.
        target: crate::db::Target::test_owned_with_driver(
            Driver::Mssql,
            as_login(admin, &ids.apply, &passwords[0])?,
            Vec::new(),
        ),
        fixture: as_login(admin, &ids.fixture, &passwords[1])?,
        assertion: as_login(admin, &ids.assertion, &passwords[2])?,
        roles: [ids.apply, ids.fixture, ids.assertion],
        key,
        driver: Driver::Mssql,
    })
}

/// Namespaces for every schema the declarations name; also a `prepare`, since
/// SQL Server has no cluster roles to create first.
pub(super) async fn namespaces(session: &Session<'_>, schema: &Schema) -> anyhow::Result<()> {
    let mut conn = session.admin().await?;
    sql::namespaces(
        &mut conn,
        crate::deploy::schemas_declared(schema).into_keys(),
    )
    .await?;
    Ok(())
}

pub(super) async fn grants(session: &Session<'_>, schema: &Schema) -> anyhow::Result<()> {
    let mut conn = session.admin().await?;
    sql::grants(&mut conn, &identities(session), schema).await?;
    Ok(())
}

/// A refusal of the fixture's text or a transaction it left open is the user's
/// to fix; only a database error keeps its code, so that the report classifies it.
pub(super) async fn fixture(conn: &mut Conn, text: &str) -> anyhow::Result<()> {
    sql::run_fixture(conn, text)
        .await
        .map_err(|error| match error {
            sql::FixtureError::Db(db) => anyhow::Error::new(db),
            other @ (sql::FixtureError::Unsupported(_) | sql::FixtureError::OpenTransaction(_)) => {
                anyhow::anyhow!("{other}")
            }
        })
}

/// Switches the owned database read-only from the administrator's connection to
/// `master`; the caller's deadline bounds it (the statement itself has none).
pub(super) async fn seal(session: &Session<'_>) -> anyhow::Result<()> {
    let mut conn = Conn::connect(Driver::Mssql, session.scratch.connection()).await?;
    sql::seal(&mut conn).await?;
    Ok(())
}

/// A failed setup read is not an assertion result: it is unanswerable.
pub(super) async fn assertion(
    session: &Session<'_>,
    text: &str,
    timeout: std::time::Duration,
) -> AssertionResult {
    sql::run_assertion(&session.assertion, text, timeout)
        .await
        .unwrap_or_else(AssertionResult::Unanswerable)
}
