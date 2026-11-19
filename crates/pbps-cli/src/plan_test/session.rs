//! Credentials and targets can only be derived from this run's owned handle.
//!
//! One `Session` serves every engine that owns a test scratch. What differs is
//! the engine's [`TestEngine`], carried by the run's [`OwnedTest`] and
//! selected once with it (#2798). PostgreSQL's is [`postgres`]; SQL Server's is
//! [`mssql`], whose SQL lives in `pbps_mssql::test_scratch`. Callers name no
//! engine (DEC-1165.14). The grants, fixture, seal and assertion protocol is
//! behind the same contract, so nothing here branches on the driver.
mod mssql;
mod postgres;
pub(crate) mod prerequisites;
pub(crate) use mssql::MssqlTest;
pub(crate) use postgres::PostgresTest;
pub(crate) use prerequisites::HopContext;

use super::{revision::Assembled, snapshot::Snapshot};
use crate::{db::Target, engine::OwnedTest, test_scratch::TestScratch};
use pbps_db::{Conn, query::AssertionResult};
use pbps_model::Schema;
use std::{future::Future, pin::Pin, time::Duration};

/// A connected step of an engine's session. Boxed so that the contract is
/// object safe and neither engine's larger future grows the other's frame; the
/// outputs are common types, so nothing certified is erased by the box.
pub(crate) type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + 'a>>;

/// One assertion run. Its result is already an answer or an `Unanswerable`, so
/// the future carries no `Result` of its own.
pub(crate) type AssertionFuture<'a> = Pin<Box<dyn Future<Output = AssertionResult> + 'a>>;

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
    /// Takes the fixture identity's routine grants back on their permanent
    /// namespaces before a plan reads the catalog, so that the catalog does not
    /// see grants that no declaration made. An engine whose fixture is never
    /// granted a routine says so by returning at once; this is not a default.
    fn revoke_fixture_routines<'a>(&self, session: &'a Session<'_>) -> SessionFuture<'a, ()>;
    /// Grants the fixture and assertion identities what they may hold on the
    /// declared objects, and refuses when their effective privileges reach past
    /// the fixture boundary, whether granted directly, inherited or to PUBLIC.
    fn grants<'a>(&self, session: &'a Session<'_>, schema: &'a Schema) -> SessionFuture<'a, ()>;
    /// The fixture identity's own connection.
    fn fixtures<'a>(&self, session: &'a Session<'_>) -> SessionFuture<'a, Conn>;
    /// Runs one fixture file on [`Self::fixtures`]'s connection and refuses a
    /// fixture that leaves a transaction open. PostgreSQL bounds it by the
    /// server's own `statement_timeout`; SQL Server has none, so its bound is the
    /// caller's deadline dropping the connection.
    fn run_fixture<'a>(
        &self,
        conn: &'a mut Conn,
        sql: &'a str,
        timeout_ms: u128,
    ) -> SessionFuture<'a, ()>;
    /// The write boundary of the assertions, once the last grants are in. An
    /// engine whose boundary is not a database-wide mode says so by returning at
    /// once; this is not a default.
    fn seal<'a>(&self, session: &'a Session<'_>) -> SessionFuture<'a, ()>;
    /// Runs one assertion file on its own connection as the assertion identity.
    /// The seal precedes every call.
    fn run_assertion<'a>(
        &self,
        session: &'a Session<'_>,
        sql: &'a str,
        timeout: Duration,
    ) -> AssertionFuture<'a>;
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
    pub fn target(&self) -> &Target {
        &self.target
    }
    /// The assertion login's connection string, for the Linux lifecycle tests
    /// that open it themselves; the command goes through [`Self::run_assertion`].
    #[cfg(all(test, target_os = "linux"))]
    pub fn assertion(&self) -> &str {
        &self.assertion
    }
    /// The write boundary of the assertions, once the last grants are in; see
    /// [`TestEngine::seal`].
    pub async fn seal(&self) -> anyhow::Result<()> {
        self.owned.session.seal(self).await
    }
    /// Runs one assertion file on its own connection as the assertion login.
    pub async fn run_assertion(&self, sql: &str, timeout: Duration) -> AssertionResult {
        self.owned.session.run_assertion(self, sql, timeout).await
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
    /// Takes back the fixture identity's routine grants before a plan reads the
    /// catalog; see [`TestEngine::revoke_fixture_routines`].
    pub async fn revoke_fixture_routines(&self) -> anyhow::Result<()> {
        self.owned.session.revoke_fixture_routines(self).await
    }
    /// The grants fixtures and assertions may hold; see [`TestEngine::grants`].
    pub async fn grants(&self, schema: &Schema) -> anyhow::Result<()> {
        self.owned.session.grants(self, schema).await
    }
    /// The fixture identity's connection; see [`TestEngine::fixtures`].
    pub async fn fixtures(&self) -> anyhow::Result<Conn> {
        self.owned.session.fixtures(self).await
    }
    /// Runs one fixture file on `fixtures()`'s connection; see
    /// [`TestEngine::run_fixture`].
    pub async fn run_fixture(
        &self,
        conn: &mut Conn,
        sql: &str,
        timeout_ms: u128,
    ) -> anyhow::Result<()> {
        self.owned.session.run_fixture(conn, sql, timeout_ms).await
    }
}

#[cfg(all(test, target_os = "linux"))]
mod mssql_tests;

#[cfg(test)]
mod structure_tests {
    /// The first engine mark in `source`'s production code, comments aside:
    /// comments may say why an engine differs, code may not choose by it.
    ///
    /// Production code ends at the first test module. The cut is the module
    /// declaration and not its attribute, because the same attribute also
    /// marks a test-only method in the middle of `Session`, and cutting there
    /// would leave every method after it unread.
    fn engine_named(source: &str) -> Option<&'static str> {
        let production = source
            .split("\nmod mssql_tests;")
            .next()
            .unwrap_or_default();
        let code = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        ["Driver", "pbps_pg::", "pbps_mssql::", ".driver()"]
            .into_iter()
            .find(|mark| code.contains(mark))
    }

    #[test]
    fn the_session_leaves_every_engine_decision_to_the_test_engine() {
        assert_eq!(engine_named(include_str!("session.rs")), None);
    }

    #[test]
    fn the_engine_scan_sees_a_driver_branch_and_an_engine_call() {
        for (source, mark) in [
            ("fn f() { match d { Driver::Mssql => {} } }", "Driver"),
            ("fn f() { pbps_pg::assertions::run(); }", "pbps_pg::"),
            (
                "fn f() { pbps_mssql::test_scratch::seal(); }",
                "pbps_mssql::",
            ),
            ("fn f() { self.driver(); }", ".driver()"),
        ] {
            assert_eq!(engine_named(source), Some(mark), "{source}");
        }
        assert_eq!(
            engine_named("// Driver::Mssql only in a comment\nfn f() {}"),
            None
        );
        assert_eq!(
            engine_named("fn f() {}\nmod mssql_tests;\nmod t { Driver }"),
            None
        );
        // A test-only method does not end the scan: a branch after it is seen.
        assert_eq!(
            engine_named(
                "#[cfg(all(test, target_os = \"linux\"))]\nfn t() {}\nfn f() { Driver::Mssql; }"
            ),
            Some("Driver")
        );
    }
}
