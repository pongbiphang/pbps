//! Credentials and targets can only be derived from this run's owned handle.
//!
//! One `Session` serves every engine that owns a test scratch. What differs is
//! the engine's [`TestEngine`], carried by the run's [`OwnedTest`] and
//! selected once with it (#2798). PostgreSQL's is [`postgres`]; SQL Server's is
//! [`mssql`], whose SQL lives in `pbps_mssql::test_scratch`. Callers name no
//! engine (DEC-1165.14). The grants, fixture and assertion protocol is still
//! branched on the driver below, until it moves behind the same contract.
mod mssql;
mod postgres;
pub(crate) mod prerequisites;
pub(crate) use mssql::MssqlTest;
pub(crate) use postgres::PostgresTest;
pub(crate) use prerequisites::HopContext;

use super::{revision::Assembled, snapshot::Snapshot};
use crate::{db::Target, engine::OwnedTest, test_scratch::TestScratch};
use anyhow::{Context as _, ensure};
use pbps_db::{Conn, Driver, query::AssertionResult};
use pbps_dialect::Dialect as _;
use pbps_model::{Schema, TableName};
use std::{future::Future, pin::Pin, time::Duration};

/// A connected step of an engine's session. Boxed so that the contract is
/// object safe and neither engine's larger future grows the other's frame; the
/// outputs are common types, so nothing certified is erased by the box.
pub(crate) type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + 'a>>;

/// What an engine provisioned for one owned run, before the handle that owns
/// the run adds its own fields. The engine cannot name another engine's support
/// record or the run's key, so a session cannot be assembled from mixed parts.
pub(crate) struct Provisioned {
    /// Where the apply identity connects. It excludes infrastructure identities
    /// from the catalog read, wherever the engine has them.
    pub target: Target,
    /// The fixture identity's connection string.
    pub fixture: String,
    /// The assertion identity's connection string.
    pub assertion: String,
    /// Apply, fixture and assertion identities, in that order.
    pub roles: [String; 3],
}

/// What `pbps test` needs of an engine's owned scratch once the server runs:
/// the isolation of one run and the reads that follow from it.
///
/// An engine that advertises owned-test support owes every method, with no
/// default, because a missing one would read as a step that needed nothing. An
/// engine that cannot do one refuses it, by name, from that method. A snapshot
/// in particular is never absent and equal: a re-pin that could not read one
/// has not compared anything.
///
/// `Any` lets a test ask which implementation a record carries; a trait object
/// has no other identity.
pub(crate) trait TestEngine: std::any::Any + Sync {
    /// Provisions this run's identities and database on the owned server, and
    /// the apply target, in the engine's own order.
    fn open<'a>(&self, scratch: &'a TestScratch<'a>) -> SessionFuture<'a, Provisioned>;
    /// The administrator's connection to the owned database.
    fn admin<'a>(&self, scratch: &'a TestScratch<'_>) -> SessionFuture<'a, Conn>;
    /// Prepares only the incoming hop, before its capture/plan, from the pair of
    /// hops: preparing a future union would make those future roles unmanaged in
    /// an earlier hop, and a name alone cannot say a role is new (#2552).
    fn prepare<'a>(&self, session: &'a Session<'_>, hop: HopContext<'a>) -> SessionFuture<'a, ()>;
    /// The namespaces the declarations name, as provisioning prerequisites.
    fn namespaces<'a>(&self, session: &'a Session<'_>, schema: &'a Schema)
    -> SessionFuture<'a, ()>;
    /// The comparison database's rows and allocators for a re-pin.
    fn snapshot<'a>(
        &self,
        session: &'a Session<'_>,
        schema: &'a Schema,
    ) -> SessionFuture<'a, Snapshot>;
}

pub(crate) struct Session<'a> {
    scratch: &'a TestScratch<'a>,
    /// The record that admitted the run: its [`TestEngine`] and its driver.
    owned: &'static OwnedTest,
    target: Target,
    fixture: String,
    assertion: String,
    /// Apply, fixture and assertion identities, in that order, on either engine.
    roles: [String; 3],
    key: std::path::PathBuf,
}
pub(super) fn quoted(name: &str) -> anyhow::Result<String> {
    Ok(pbps_pg::Postgres::new().quote_ident(name)?)
}
pub(super) fn table(name: &TableName) -> anyhow::Result<String> {
    Ok(format!("{}.{}", quoted(&name.schema)?, quoted(&name.name)?))
}
impl<'a> Session<'a> {
    pub async fn new(scratch: &'a TestScratch<'a>) -> anyhow::Result<Self> {
        let owned = scratch.owned();
        let Provisioned {
            target,
            fixture,
            assertion,
            roles,
        } = owned.session.open(scratch).await?;
        let key = scratch.fingerprint_key()?;
        Ok(Self {
            scratch,
            owned,
            target,
            fixture,
            assertion,
            roles,
            key,
        })
    }
    pub(super) async fn admin(&self) -> anyhow::Result<Conn> {
        self.owned.session.admin(self.scratch).await
    }
    pub async fn revoke_fixture_routines(&self) -> anyhow::Result<()> {
        // A SQL Server fixture is granted tables only (ADR-0019 §6), so there is
        // no routine grant to take back before a plan reads the catalog.
        if self.driver() == Driver::Mssql {
            return Ok(());
        }
        let mut conn = self.admin().await?;
        let fixture = &self.roles[1];
        // Temporary runner grants are not declaration grants; leave the ordinary
        // rebuild ACL guard intact and refresh these only before fixture SQL.
        let sql = format!(
            concat!(
                "SELECT DISTINCT n.nspname AS schema FROM pg_catalog.pg_proc p ",
                "JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace ",
                "CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) a ",
                "WHERE a.grantee='{fixture}'::pg_catalog.regrole ",
                "AND a.privilege_type='EXECUTE'"
            ),
            fixture = fixture
        );
        for row in conn.query(&sql).await? {
            let namespace = quoted(
                row.try_get::<&str>("schema")?
                    .context("missing routine namespace")?,
            )?;
            conn.execute(&format!(
                "REVOKE EXECUTE ON ALL ROUTINES IN SCHEMA {namespace} FROM {fixture}"
            ))
            .await?;
        }
        Ok(())
    }
    /// Prepares only the incoming hop; see [`TestEngine::prepare`].
    pub async fn prepare(&self, hop: HopContext<'_>) -> anyhow::Result<()> {
        self.owned.session.prepare(self, hop).await
    }
    pub async fn namespaces(&self, schema: &Schema) -> anyhow::Result<()> {
        self.owned.session.namespaces(self, schema).await
    }
    /// The comparison database's snapshot; see [`TestEngine::snapshot`].
    pub(super) async fn snapshot(&self, schema: &Schema) -> anyhow::Result<Snapshot> {
        self.owned.session.snapshot(self, schema).await
    }
    pub(super) fn driver(&self) -> Driver {
        self.owned.container.driver
    }
    pub fn target(&self) -> &Target {
        &self.target
    }
    /// The assertion login's connection string, for the Linux lifecycle tests
    /// that open it themselves; the command goes through [`Self::run_assertion`].
    #[cfg(all(test, target_os = "linux"))]
    pub fn assertion(&self) -> &str {
        &self.assertion
    }
    /// The write boundary of the assertions, once the last grants are in.
    /// PostgreSQL's is a READ ONLY transaction each assertion opens for itself,
    /// so it has nothing to do here; SQL Server's is the database mode.
    pub async fn seal(&self) -> anyhow::Result<()> {
        match self.driver() {
            Driver::Postgres => Ok(()),
            Driver::Mssql => Box::pin(mssql::seal(self)).await,
        }
    }
    /// Runs one assertion file on its own connection as the assertion login.
    pub async fn run_assertion(&self, sql: &str, timeout: Duration) -> AssertionResult {
        match self.driver() {
            Driver::Postgres => pbps_pg::assertions::run(&self.assertion, sql, timeout).await,
            Driver::Mssql => Box::pin(mssql::assertion(self, sql, timeout)).await,
        }
    }
    pub fn project(&self, input: &Assembled) -> pbps_config::Project {
        let source = &input.config;
        // A project default may select an unowned server. Construct explicitly
        // so a new runtime setting requires a decision instead of leaking in.
        let config = pbps_config::Config {
            dialect: source.dialect,
            databases: None,
            trusted_roles: source.trusted_roles.clone(),
            pin_type_routines: source.pin_type_routines,
            schema_dir: source.schema_dir.clone(),
            ids_file: source.ids_file.clone(),
            ids_dir: source.ids_dir.clone(),
            environments: std::collections::BTreeMap::from([(
                "test".into(),
                pbps_config::Environment {
                    url_env: "PBPS_TEST_UNUSED".into(),
                    description: None,
                    resolve_with: None,
                    fingerprint_key_env: None,
                    fingerprint_key_file: Some(self.key.clone()),
                    rehearse: None,
                },
            )]),
            hooks: Default::default(),
            unmanaged: source.unmanaged,
            dev: None,
            resolvers: Default::default(),
            resolve_with: None,
            max_data_rows: source.max_data_rows,
            partition_runway: source.partition_runway.clone(),
            policies: source.policies.clone(),
            consumers: source.consumers.clone(),
        };
        pbps_config::Project {
            root: self
                .key
                .parent()
                .expect("the journal key has a parent")
                .to_owned(),
            config,
        }
    }
    pub async fn grants(&self, schema: &Schema) -> anyhow::Result<()> {
        if self.driver() == Driver::Mssql {
            return Box::pin(mssql::grants(self, schema)).await;
        }
        let mut conn = self.admin().await?;
        let [_, fixture, assertion] = &self.roles;
        // Check every permanent namespace, including those with no managed table.
        // Engine identity, not a pg_temp name-prefix guess, excludes temp helpers.
        let sql = format!(
            concat!(
                "SELECT pg_catalog.has_database_privilege('{fixture}', current_database(), 'CREATE') ",
                "OR pg_catalog.has_database_privilege('{assertion}', current_database(), 'CREATE') ",
                "OR EXISTS (SELECT 1 FROM pg_catalog.pg_namespace n ",
                "WHERE n.oid <> pg_catalog.pg_my_temp_schema() ",
                "AND NOT pg_catalog.pg_is_other_temp_schema(n.oid) AND (",
                "pg_catalog.has_schema_privilege('{fixture}', n.oid, 'CREATE') OR ",
                "pg_catalog.has_schema_privilege('{assertion}', n.oid, 'CREATE'))) AS unsafe"
            ),
            fixture = fixture,
            assertion = assertion
        );
        let rows = conn.query(&sql).await?;
        let permanent_create = rows
            .first()
            .context("missing CREATE privilege result")?
            .try_get::<bool>("unsafe")?;
        ensure!(
            permanent_create == Some(false),
            "fixture/assertion privileges permit permanent CREATE; remove broad grants, including any PUBLIC grant"
        );
        for id in schema.modules.keys() {
            let pbps_model::ModuleId::Routine(routine) = id else {
                continue;
            };
            let namespace = quoted(&routine.name.schema)?;
            let signature = format!(
                "{}.{}({})",
                namespace,
                quoted(&routine.name.name)?,
                routine
                    .args
                    .iter()
                    .map(|a| a.raw())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            // Declared routines are trusted schema code (ADR-0019 §3/§6).
            // Ordinary writers must execute invoker callees even when PostgreSQL
            // records no dependency for a string/PLpgSQL body or a domain default.
            // This changes neither PUBLIC nor the assertion identity's grants.
            conn.execute(&format!(
                concat!(
                    "GRANT USAGE ON SCHEMA {namespace} TO {fixture}; ",
                    "GRANT EXECUTE ON ROUTINE {signature} TO {fixture}"
                ),
                namespace = namespace,
                signature = signature,
                fixture = fixture
            ))
            .await?;
        }
        for (name, declared) in &schema.tables {
            let t = table(name)?;
            let namespace = quoted(&name.schema)?;
            let sql = format!(
                concat!(
                    "GRANT USAGE ON SCHEMA {namespace} TO {fixture}, {assertion}; ",
                    "REVOKE ALL ON {t} FROM {fixture}, {assertion}; ",
                    "GRANT SELECT ON {t} TO {fixture}, {assertion}"
                ),
                namespace = namespace,
                t = t,
                fixture = fixture,
                assertion = assertion
            );
            conn.execute(&sql).await?;
            if declared.data.is_none() {
                conn.execute(&format!("GRANT INSERT, UPDATE, DELETE ON {t} TO {fixture}"))
                    .await?;
                // Ownership and default dependencies name only this table's allocators.
                let literal = t.replace('\'', "''");
                let sql = format!(
                    concat!(
                        "SELECT DISTINCT n.nspname AS schema, s.relname AS name ",
                        "FROM pg_catalog.pg_class s JOIN pg_catalog.pg_namespace n ON n.oid=s.relnamespace ",
                        "WHERE s.relkind='S' AND (EXISTS (SELECT 1 FROM pg_catalog.pg_depend d ",
                        "WHERE d.classid='pg_class'::regclass AND d.objid=s.oid ",
                        "AND d.refobjid='{relation}'::regclass AND d.deptype IN ('a','i')) ",
                        "OR EXISTS (SELECT 1 FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_attrdef a ",
                        "ON d.classid='pg_attrdef'::regclass AND d.objid=a.oid ",
                        "WHERE a.adrelid='{relation}'::regclass AND d.refclassid='pg_class'::regclass ",
                        "AND d.refobjid=s.oid))"
                    ),
                    relation = literal
                );
                let rows = conn.query(&sql).await?;
                for r in rows {
                    let sequence = TableName::new(
                        r.try_get::<&str>("schema")?
                            .context("allocator schema missing")?,
                        r.try_get::<&str>("name")?
                            .context("allocator name missing")?,
                    );
                    conn.execute(&format!(
                        "GRANT USAGE ON SEQUENCE {} TO {fixture}",
                        table(&sequence)?
                    ))
                    .await?;
                }
            }
            let literal = t.replace('\'', "''");
            // Both identities use one effective test; data: controls fixture DML.
            let sql = format!(
                concat!(
                    "SELECT EXISTS (SELECT 1 FROM (VALUES ('{assertion}',true), ",
                    "('{fixture}',{reference})) actor(principal,deny_write) ",
                    "WHERE actor.deny_write AND (pg_catalog.has_table_privilege(",
                    "actor.principal::pg_catalog.name,'{relation}', ",
                    "'INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR ",
                    "pg_catalog.has_any_column_privilege(",
                    "actor.principal::pg_catalog.name,'{relation}', ",
                    "'INSERT,UPDATE,REFERENCES'))) AS unsafe"
                ),
                relation = literal,
                reference = declared.data.is_some(),
                fixture = fixture,
                assertion = assertion,
            );
            let rows = conn.query(&sql).await?;
            ensure!(
                rows.first()
                    .context("missing privilege result")?
                    .try_get::<bool>("unsafe")?
                    == Some(false),
                "the effective SQL privileges for {} exceed the fixture/assertion boundary; remove broad grants, including any PUBLIC grant",
                name.shown()
            );
        }
        Ok(())
    }
    pub async fn fixtures(&self) -> anyhow::Result<Conn> {
        Ok(Conn::connect(self.driver(), &self.fixture).await?)
    }
    /// Runs one fixture file on `fixtures()`'s connection. PostgreSQL bounds it
    /// by the server's own `statement_timeout`; SQL Server has none, so its
    /// bound is the caller's deadline dropping the connection.
    pub async fn run_fixture(
        &self,
        conn: &mut Conn,
        sql: &str,
        timeout_ms: u128,
    ) -> anyhow::Result<()> {
        match self.driver() {
            Driver::Postgres => {
                // PostgreSQL uses i32 milliseconds; zero would disable the limit.
                conn.execute(&format!("SET statement_timeout = '{timeout_ms}ms'"))
                    .await?;
                conn.execute(sql).await?;
                // A successful file can still lose its writes when the fixture
                // connection closes. Reuse the driver-safe probe (DEC-2670.1).
                anyhow::ensure!(
                    !pbps_pg::catalog::in_transaction(conn).await?,
                    "the fixture left a transaction open; COMMIT or ROLLBACK inside it, because \
                     closing its session would discard the writes silently"
                );
                Ok(())
            }
            Driver::Mssql => Box::pin(mssql::fixture(conn, sql)).await,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod mssql_tests;
