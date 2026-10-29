//! Live regressions for the resolver (#1672), on the pinned
//! PostgreSQL servers with no root, no Docker and no process observation.
//!
//! Each test makes its own target and scratch databases and logins, named
//! with a run token, and drops them at the end. The declarations are the
//! measured producer's motivating pair (`qualified_evidence_cases`): an
//! arriving integer overload rebinds one view and leaves the other alone.

#[cfg(test)]
use crate::certified_fixture::FixtureEmission as _;

use super::super::{Error, ProduceError, ResolvedPlan};
use super::fixtures::{self as cases, ENVIRONMENT, Inputs, ProjectKey, view};
use pbps_db::{Conn, Driver};
use pbps_model::Change;
use pbps_pg::resolver::standard::Standard;

/// The two pinned servers of `scripts/live-tests-pg.sh`, 18 and 16.
const SERVERS: &[&str] = &["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"];

/// A second PostgreSQL 18 cluster, for a scratch server that is not the
/// target's: the only placement where an account that may create roles and
/// databases is allowed.
const SCRATCH_SERVER: &str = "PBPS_TEST_PG_SCRATCH_DB";

fn setting(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must name a pinned PostgreSQL server"))
}

/// The objects, databases and roles one test made on one server.
struct Fixture {
    server: String,
    token: String,
    databases: Vec<String>,
    roles: Vec<String>,
}

impl Fixture {
    fn new(var: &str) -> Self {
        Self {
            server: setting(var),
            token: format!("{:016x}", rand::random::<u64>()),
            databases: Vec::new(),
            roles: Vec::new(),
        }
    }

    async fn admin(&self) -> Conn {
        Conn::connect(Driver::Postgres, &self.server).await.unwrap()
    }

    fn on(&self, database: &str) -> String {
        format!("{} dbname={database}", self.server)
    }

    fn as_login(&self, database: &str, (login, password): &(String, String)) -> String {
        format!(
            "{} dbname={database} user={login} password={password}",
            self.server
        )
    }

    async fn database(&mut self, purpose: &str, owner: Option<&str>) -> String {
        let name = format!("pbps_v1672_{purpose}_{}", self.token);
        let owner = owner.map(|o| format!(" OWNER {o}")).unwrap_or_default();
        self.admin()
            .await
            .execute(&format!("CREATE DATABASE {name} TEMPLATE template0{owner}"))
            .await
            .unwrap();
        self.databases.push(name.clone());
        name
    }

    async fn login(&mut self, purpose: &str, attributes: &str) -> (String, String) {
        let name = format!("pbps_v1672_{purpose}_{}", self.token);
        let password = format!("{:032x}", rand::random::<u128>());
        self.admin()
            .await
            .execute(&format!(
                "CREATE ROLE {name} LOGIN PASSWORD '{password}' {attributes}"
            ))
            .await
            .unwrap();
        self.roles.push(name.clone());
        (name, password)
    }

    /// A role that cannot log in, for a grantee.
    async fn role(&mut self, purpose: &str) -> String {
        let name = format!("pbps_v1708_{purpose}_{}", self.token);
        self.admin()
            .await
            .execute(&format!("CREATE ROLE {name} NOLOGIN"))
            .await
            .unwrap();
        self.roles.insert(0, name.clone());
        name
    }

    async fn run(&self, database: &str, statements: &[&str]) {
        let mut conn = Conn::connect(Driver::Postgres, &self.on(database))
            .await
            .unwrap();
        for statement in statements {
            conn.execute(statement).await.unwrap();
        }
    }

    /// The target database, holding the motivating pair's catalog.
    async fn target(&mut self) -> String {
        let target = self.database("t", None).await;
        self.run(&target, cases::TARGET_SETUP).await;
        target
    }

    /// A login confined to a database it owns: no superuser, no
    /// `CREATEROLE`, no `CREATEDB`. `pg_read_all_settings` lets it read the
    /// settings the compatibility rule compares, as the rule's refusal says.
    /// A confined login and the database it owns. The login may hold one
    /// connection at a time: every scratch write, cleanup included, must go
    /// through the connection the run checked (#1678 review).
    async fn confined(&mut self) -> ((String, String), String) {
        let login = self
            .login(
                "c",
                "NOSUPERUSER NOCREATEDB NOCREATEROLE CONNECTION LIMIT 1",
            )
            .await;
        self.admin()
            .await
            .execute(&format!("GRANT pg_read_all_settings TO {}", login.0))
            .await
            .unwrap();
        let database = self.database("s", Some(&login.0)).await;
        (login, database)
    }

    /// Names on this server that a run could have created: databases and
    /// roles. A refusal before any write leaves this unchanged.
    async fn inventory(&self) -> (Vec<String>, Vec<String>) {
        let mut admin = self.admin().await;
        let names = |rows: Vec<pbps_db::Row>| {
            rows.iter()
                .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let databases = admin
            .query("SELECT datname::text AS name FROM pg_catalog.pg_database ORDER BY 1")
            .await
            .unwrap();
        let roles = admin
            .query("SELECT rolname::text AS name FROM pg_catalog.pg_roles ORDER BY 1")
            .await
            .unwrap();
        (names(databases), names(roles))
    }

    async fn foreign_objects(&self, database: &str) -> (Vec<String>, usize) {
        let mut conn = Conn::connect(Driver::Postgres, &self.on(database))
            .await
            .unwrap();
        pbps_pg::resolver::scratch::foreign_objects(&mut conn)
            .await
            .unwrap()
    }

    async fn drop(self) {
        let mut admin = self.admin().await;
        for database in &self.databases {
            // A test that fails may leave a template, which cannot be
            // dropped as one.
            let _ = admin
                .execute(&format!("ALTER DATABASE {database} IS_TEMPLATE false"))
                .await;
            admin
                .execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
                .await
                .unwrap();
        }
        for role in &self.roles {
            admin
                .execute(&format!("DROP ROLE IF EXISTS {role}"))
                .await
                .unwrap();
        }
    }
}

async fn produce(
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
) -> Result<ResolvedPlan, ProduceError> {
    produce_with(scratch, target, inputs, key, &Standard::default()).await
}

async fn produce_with(
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    standard: &Standard,
) -> Result<ResolvedPlan, ProduceError> {
    // As the planning read records it: the identity of the cluster the
    // target string reached.
    let mut planning = Conn::connect(Driver::Postgres, target).await.unwrap();
    let identity = pbps_pg::resolver::scratch::cluster_identity(&mut planning)
        .await
        .unwrap();
    produce_planned_on(scratch, target, &identity, inputs, key, standard).await
}

async fn produce_planned_on(
    scratch: &str,
    target: &str,
    planned_identity: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    standard: &Standard,
) -> Result<ResolvedPlan, ProduceError> {
    super::produce(
        Driver::Postgres,
        scratch,
        target,
        planned_identity,
        &inputs.binding(),
        inputs.base(),
        inputs.desired(),
        &inputs.hints,
        &[],
        &key.project,
        Some(ENVIRONMENT),
        standard,
        None,
        &super::Provenance::server(),
    )
    .await
}

fn resolver_refusal(result: Result<ResolvedPlan, ProduceError>) -> String {
    // Never panics: the caller drops its fixture first and asserts on this,
    // so a wrong outcome does not leave databases and roles behind.
    match result {
        Err(ProduceError::Run(Error::Unusable(reason))) => reason,
        Err(other) => format!("not a resolver refusal: {other}"),
        Ok(_) => "not a resolver refusal: a plan".into(),
    }
}

/// The answer the engine gives for the motivating pair: the unqualified call
/// moves to the arriving overload and is rebuilt; the explicit numeric call
/// stays, and the resolved plan leaves it alone although ADR-0013's
/// candidate test would rebuild it.
fn assert_answers_the_overload(plan: &ResolvedPlan) {
    assert_answers_the_overload_from(plan, pbps_model::resolver::ScratchSource::Server {});
}

fn assert_answers_the_overload_from(
    plan: &ResolvedPlan,
    scratch: pbps_model::resolver::ScratchSource,
) {
    plan.evidence.validate(&plan.changes).unwrap();
    pbps_pg::resolver::validate_evidence(&plan.evidence)
        .expect("the resolver's evidence passes the artifact reader");
    let pbps_model::resolver::ResolverRuntime::Scratch {
        entry,
        scratch: sealed,
        outside_writes,
        ..
    } = &plan.evidence.qualification().runtime;
    assert_eq!(
        (entry.raw(), sealed, *outside_writes),
        (
            "scratch",
            &scratch,
            pbps_model::resolver::OutsideWrites::Refused
        ),
        "the evidence names the resolver's entry and scratch"
    );
    assert_eq!(
        plan.evidence.qualification().rule,
        pbps_pg::resolver::compatibility::REPORTED_RULE
    );
    let affected = view(&plan.evidence, "v");
    let control = view(&plan.evidence, "control");
    assert_ne!(
        affected.current.as_ref().unwrap().bindings,
        affected.desired.as_ref().unwrap().bindings
    );
    assert_eq!(
        control.current.as_ref().unwrap().bindings,
        control.desired.as_ref().unwrap().bindings
    );
    let touches = |name: &str| {
        let id: pbps_model::ModuleId = format!("{}.{name}", cases::SCHEMA).parse().unwrap();
        plan.changes.changes.iter().any(|step| {
            matches!(&step.change,
                Change::DropModule { id: m, .. } | Change::CreateModule { id: m, .. }
                    | Change::AlterModule { id: m, .. } if *m == id)
        })
    };
    assert!(touches("v"), "the rebound view is rebuilt");
    assert!(!touches("control"), "the unchanged view is left alone");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_answers_a_managed_question_without_root_or_observation() {
    // A superuser scratch on another cluster: the run-owned layout.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let before = scratch.inventory().await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let result = produce(&scratch.server, &target.on(&target_db), &inputs, &key).await;
    let after = scratch.inventory().await;
    target.drop().await;
    let plan = result.unwrap();
    assert_answers_the_overload(&plan);
    assert_layout(&plan, pbps_model::resolver::ScratchLayout::RunOwned);
    assert_eq!(
        before, after,
        "the run-owned database, login and roles are all dropped"
    );
}

/// The first part of every crowd role's name; the rest is the process id, an
/// underscore and a number.
const CROWD_STEM: &str = "pbps_crowd2049_";

/// How many roles one transaction creates or drops.
///
/// `CREATE ROLE` and `DROP ROLE` each hold a lock on the role until the
/// transaction ends (one `pg_locks` row per role, measured on 18.6). The shared
/// lock table holds `max_locks_per_transaction` x (`max_connections` +
/// `max_prepared_transactions`) entries, 6400 by default and shared with every
/// other session on the cluster, so ten thousand roles in one transaction fit
/// only by slack and fail with `out of shared memory` (#2050).
const CROWD_BATCH: u32 = 250;

/// A crowd of `NOLOGIN` roles on the target's cluster that cannot outlive the
/// test that made it.
///
/// A superuser deployer's closure is every role in its cluster, so these are
/// what the run under test reproduces. Roles belong to the cluster and not to
/// a test's database, so a crowd left behind by a failed run is read by every
/// later test that lists the cluster's roles (#2050). A `Drop` cannot `.await`
/// and the connection it would use may be the thing that failed, so the removal
/// on drop runs on its own thread, runtime and connection, and so runs on a
/// panic and on an early return alike.
struct RoleCrowd {
    server: String,
    /// `pbps_crowd2049_<pid>_`: the pid keeps one run's roles from being
    /// counted as another's.
    prefix: String,
    made: u32,
    armed: bool,
}

impl RoleCrowd {
    /// Starts the guard. Nothing is created until [`RoleCrowd::grow_to`].
    fn arm(server: &str) -> Self {
        Self {
            server: server.to_owned(),
            prefix: format!("{CROWD_STEM}{}_", std::process::id()),
            made: 0,
            armed: true,
        }
    }

    /// Brings the crowd to `count` roles, [`CROWD_BATCH`] to a transaction.
    /// Roles an earlier run left behind (any pid) are dropped first, so one bad
    /// run cannot poison the next.
    async fn grow_to(&mut self, admin: &mut Conn, count: u32) -> Result<(), String> {
        if self.made == 0 {
            drop_crowd_roles(admin, CROWD_STEM).await?;
        }
        while self.made < count {
            let first = self.made + 1;
            let last = (self.made + CROWD_BATCH).min(count);
            admin
                .execute(&format!(
                    "DO $$ BEGIN FOR i IN {first}..{last} LOOP \
                       EXECUTE format('CREATE ROLE %I NOLOGIN', '{}' || i); \
                     END LOOP; END $$",
                    self.prefix
                ))
                .await
                .map_err(|e| format!("create crowd roles {first}..{last}: {e}"))?;
            self.made = last;
        }
        Ok(())
    }

    /// Drops the crowd, and disarms the guard only once none is left.
    async fn remove(&mut self, admin: &mut Conn) -> Result<(), String> {
        drop_crowd_roles(admin, &self.prefix).await?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for RoleCrowd {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (server, prefix) = (self.server.clone(), self.prefix.clone());
        // Never a panic from here: this may already be one unwinding, and a
        // second aborts the process, taking the other tests' verdicts with it.
        let outcome = std::thread::spawn(move || -> Result<(), String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async {
                let mut conn = Conn::connect(Driver::Postgres, &server)
                    .await
                    .map_err(|e| e.to_string())?;
                drop_crowd_roles(&mut conn, &prefix).await
            })
        })
        .join();
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(why)) => eprintln!("could not drop the `{}*` roles: {why}", self.prefix),
            Err(_) => eprintln!(
                "could not drop the `{}*` roles: the cleanup panicked",
                self.prefix
            ),
        }
    }
}

/// Drops every role whose name starts with `stem`, [`CROWD_BATCH`] to a
/// transaction. The count is read again after each batch, and the rounds are
/// bounded: a role that cannot be dropped is reported and not retried forever.
async fn drop_crowd_roles(conn: &mut Conn, stem: &str) -> Result<(), String> {
    let matching = format!("left(rolname, {}) = '{stem}'", stem.len());
    let mut stalled = 0;
    let mut previous = i64::MAX;
    loop {
        let rows = conn
            .query(&format!(
                "SELECT count(*)::bigint FROM pg_roles WHERE {matching}"
            ))
            .await
            .map_err(|e| format!("count the `{stem}*` roles: {e}"))?;
        let left = rows
            .first()
            .and_then(|row| row.try_get_at::<i64>(0).ok().flatten())
            .ok_or_else(|| format!("count the `{stem}*` roles: no count came back"))?;
        if left == 0 {
            return Ok(());
        }
        // A batch that removed nothing twice running is not going to.
        stalled = if left < previous { 0 } else { stalled + 1 };
        if stalled >= 2 {
            return Err(format!("{left} `{stem}*` roles cannot be dropped"));
        }
        previous = left;
        conn.execute(&format!(
            "DO $$ DECLARE r text; BEGIN \
               FOR r IN SELECT rolname::text FROM pg_roles WHERE {matching} \
                        ORDER BY rolname LIMIT {CROWD_BATCH} LOOP \
                 EXECUTE format('DROP ROLE %I', r); \
               END LOOP; END $$"
        ))
        .await
        .map_err(|e| format!("drop the `{stem}*` roles: {e}"))?;
    }
}

/// A superuser deployer's run reproduces every cluster role. Sending each
/// CREATE, GRANT and DROP separately was the slowdown in #2049. Count driver
/// requests instead of elapsed growth: shared-host contention made both the
/// absolute gap and a same-run timing ratio unreliable (DEC-2121.1). Timings
/// remain diagnostics; the focused 20,000-role lookup guard covers the reverse
/// map's CPU shape separately. Both sizes share one growing target crowd.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_run_owned_reproduction_stays_linear_in_the_clusters_roles() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let mut admin = target.admin().await;
    // Armed before the first role exists, so a panic anywhere below, a failed
    // creation included, still removes whatever was made.
    let mut crowd = RoleCrowd::arm(&target.server);
    let before = scratch.inventory().await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let mut runs = Vec::new();
    let mut grown = Ok(());
    for count in [1_000, 10_000] {
        grown = crowd.grow_to(&mut admin, count).await;
        if grown.is_err() {
            break;
        }
        let started = std::time::Instant::now();
        let (result, requests) = pbps_db::request_count::scope(produce(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
        ))
        .await;
        runs.push((count, started.elapsed(), requests, result));
    }
    let removed = crowd.remove(&mut admin).await;
    let after = scratch.inventory().await;
    target.drop().await;
    grown.unwrap();
    removed.unwrap();
    for (count, took, requests, result) in &runs {
        eprintln!("issue 2049: {count} crowd roles: {took:?}; {requests} requests");
        assert_ne!(
            *requests, 0,
            "the PostgreSQL request counter must be active"
        );
        assert_answers_the_overload(result.as_ref().unwrap_or_else(|e| panic!("{count}: {e}")));
    }
    assert_eq!(
        before, after,
        "every role the runs made on the scratch server is dropped"
    );
    let [(_, _, small, _), (_, _, large, _)] = &runs[..] else {
        panic!("two runs were expected");
    };
    // 9,000 extra roles add 3 * (9,000 / 250) = 108 CREATE/GRANT/DROP
    // batches. Each of the 12 capture cursors gains at most 18,000 rows
    // (a role and a membership each): ceil(18,000 / 4,096) = 5 extra FETCHes,
    // or 60 more requests. Pin 108 + 60 = 168 independently of ROLE_BATCH,
    // so reducing the production batch size cannot relax its own guard.
    // Measured growth was 136 healthy and 27,028 with ROLE_BATCH=1
    // (DEC-2121.1). Cleanup and correctness checks precede this assertion.
    assert!(
        *large <= *small + 168,
        "ten thousand roles made {large} requests, a thousand made {small}: more than 168 extra"
    );
}

/// Exercise every PostgreSQL request path through the real connection seam:
/// a multi-statement batch is one request, and a failed call still counts.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn postgres_request_counts_cover_batches_failures_and_connection_startup() {
    let fixture = Fixture::new("PBPS_TEST_PG_DB");
    let mut existing = fixture.admin().await;
    existing.query("SELECT 0").await.unwrap();
    let (existing, fresh) = tokio::join!(
        pbps_db::request_count::scope(async {
            existing.execute("SELECT 1; SELECT 2").await.unwrap();
            existing.query("SELECT 3").await.unwrap();
            existing
                .query_with("SELECT $1::integer", &[pbps_db::Param::I32(4)])
                .await
                .unwrap();
            existing
                .execute_with("SELECT $1::integer", &[pbps_db::Param::I32(5)])
                .await
                .unwrap();
            existing
                .execute("SELECT 1 / 0")
                .await
                .expect_err("a failed request still counts");
        }),
        pbps_db::request_count::scope(async {
            let mut connection = fixture.admin().await;
            connection.query("SELECT 6").await.unwrap();
        }),
    );
    assert_eq!(existing.1, 5);
    // Version, TimeZone, pinned search_path, then SELECT 6.
    assert_eq!(fresh.1, 4);
    assert_eq!(pbps_db::request_count::scope(async {}).await.1, 0);
}

/// The layout the run used is sealed as its own (#1517).
fn assert_layout(plan: &ResolvedPlan, layout: pbps_model::resolver::ScratchLayout) {
    let pbps_model::resolver::ResolverRuntime::Scratch { layout: sealed, .. } =
        &plan.evidence.qualification().runtime;
    assert_eq!(*sealed, layout);
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_runs_in_a_precreated_database_with_a_confined_account() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        // A membership it cannot `SET ROLE` through gives it no attribute:
        // the login stays confined (#1678 review). Nor does authority over
        // its own database: an owner's grant options there once the ACL is
        // explicit, and a grant option there held by a role it inherits.
        let creator = format!("pbps_v1672_m_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {creator} NOLOGIN CREATEDB CREATEROLE; \
                 GRANT {creator} TO {login} WITH SET FALSE; \
                 REVOKE TEMPORARY ON DATABASE {scratch_db} FROM PUBLIC; \
                 GRANT CONNECT ON DATABASE {scratch_db} TO {creator} WITH GRANT OPTION",
                login = login.0
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, creator);
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
        )
        .await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        let plan = result.unwrap();
        assert_answers_the_overload(&plan);
        // Negative of the run-owned case: a precreated database is supplied.
        assert_layout(&plan, pbps_model::resolver::ScratchLayout::Supplied);
        assert_eq!(
            left.1, 0,
            "{server}: the run empties its database: {left:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_the_target_as_scratch() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let before = fixture.inventory().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        // Spelled differently from the target's string, as `localhost` and
        // an address can be: the engines, not the strings, decide.
        let spelled = format!("{} application_name=another", fixture.on(&target_db));
        let reason =
            resolver_refusal(produce(&spelled, &fixture.on(&target_db), &inputs, &key).await);
        let after = fixture.inventory().await;
        fixture.drop().await;
        assert!(
            reason.contains("target's own database"),
            "{server}: {reason}"
        );
        assert_eq!(before, after, "{server}: nothing was created");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_target_connection_on_another_cluster_than_the_plan_was_read_from() {
    // A target name that reaches two clusters, through DNS or a balancing
    // proxy: the planning read reached the one the superuser scratch sits
    // on, and the resolver's own connection the other. The marks then read
    // "another cluster", and the scratch would provision roles and a
    // database on the cluster the plan was read from (#1685).
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let mut planned = Conn::connect(Driver::Postgres, &scratch.server)
        .await
        .unwrap();
    let planned = pbps_pg::resolver::scratch::cluster_identity(&mut planned)
        .await
        .unwrap();
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let before = scratch.inventory().await;
    let reason = resolver_refusal(
        produce_planned_on(
            &scratch.server,
            &target.on(&target_db),
            &planned,
            &inputs,
            &key,
            &Standard::default(),
        )
        .await,
    );
    let after = scratch.inventory().await;
    target.drop().await;
    assert!(reason.contains("another database cluster"), "{reason}");
    assert_eq!(before, after, "nothing was created on the scratch server");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_scratch_that_may_not_read_its_cluster_identity() {
    // Without the identity a scratch on the target's cluster could pass as
    // another one, so the run refuses instead of falling back to the marks,
    // and names the grant (#1685). A function's privileges belong to each
    // database: this revokes in the scratch database alone.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        fixture
            .run(
                &scratch_db,
                &["REVOKE EXECUTE ON FUNCTION pg_catalog.pg_control_system() FROM PUBLIC"],
            )
            .await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let reason = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert!(
            reason.contains("GRANT EXECUTE ON FUNCTION pg_catalog.pg_control_system()"),
            "{server}: {reason}"
        );
        assert_eq!(left.1, 0, "{server}: nothing was written: {left:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_same_cluster_scratch_with_an_unconfined_account() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        // The fixture's superuser, in another database of the same cluster.
        let other = fixture.database("o", None).await;
        // And a login holding one of the three attributes through a role it
        // can become, which is as good as holding it.
        let (creator, password) = fixture.login("d", "NOCREATEDB").await;
        let granted = format!("pbps_v1672_g_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {granted} NOLOGIN CREATEDB; GRANT {granted} TO {creator}"
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, granted);
        let owned = fixture.database("e", Some(&creator)).await;
        // And one with none of the three that still reaches outside its
        // database: a predefined role that runs server programs, inherited
        // by a role it can become though the login itself neither inherits
        // nor can become it, and the replication attribute through a role
        // it can become, which creates cluster-wide slots (#1678 review).
        let (reacher, reacher_password) = fixture.login("x", "NOCREATEDB").await;
        let program = format!("pbps_v1672_p_{}", fixture.token);
        let slots = format!("pbps_v1672_q_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {program} NOLOGIN; \
                 GRANT pg_execute_server_program TO {program} WITH INHERIT TRUE, SET FALSE; \
                 GRANT {program} TO {reacher} WITH INHERIT FALSE, SET TRUE; \
                 CREATE ROLE {slots} NOLOGIN REPLICATION; \
                 GRANT {slots} TO {reacher} WITH INHERIT FALSE, SET TRUE"
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, program);
        fixture.roles.insert(0, slots);
        let reaching = fixture.database("x", Some(&reacher)).await;
        // And authority over shared objects of the target's cluster: a
        // database owned by a role it inherits from, `ADMIN OPTION` on a
        // role, a grant option on another database, and `ALTER SYSTEM` on a
        // parameter granted to PUBLIC (#1678 review).
        let parameter = format!("pbps.v1672_{}", fixture.token);
        // And another login role it inherits from, whose sessions in any
        // database it could cancel or terminate (#1678 review).
        let (signalled, _) = fixture.login("y", "").await;
        let owner = format!("pbps_v1672_w_{}", fixture.token);
        let admin_of = format!("pbps_v1672_v_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {owner} NOLOGIN; \
                 GRANT {owner} TO {reacher} WITH INHERIT TRUE, SET FALSE; \
                 CREATE ROLE {admin_of} NOLOGIN; \
                 GRANT {admin_of} TO {reacher} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE; \
                 GRANT CONNECT ON DATABASE {other} TO {reacher} WITH GRANT OPTION; \
                 GRANT ALTER SYSTEM ON PARAMETER {parameter} TO PUBLIC; \
                 GRANT {signalled} TO {reacher} WITH INHERIT TRUE, SET FALSE"
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, owner.clone());
        fixture.roles.insert(0, admin_of.clone());
        let owned_elsewhere = fixture.database("w", Some(&owner)).await;
        let before = fixture.inventory().await;
        let superuser = resolver_refusal(
            produce(&fixture.on(&other), &fixture.on(&target_db), &inputs, &key).await,
        );
        let member = resolver_refusal(
            produce(
                &fixture.as_login(&owned, &(creator.clone(), password.clone())),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let reach = resolver_refusal(
            produce(
                &fixture.as_login(&reaching, &(reacher.clone(), reacher_password.clone())),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let after = fixture.inventory().await;
        let left = fixture.foreign_objects(&other).await;
        let reaching_left = fixture.foreign_objects(&reaching).await;
        fixture
            .admin()
            .await
            .execute(&format!(
                "REVOKE ALTER SYSTEM ON PARAMETER {parameter} FROM PUBLIC"
            ))
            .await
            .unwrap();
        fixture.drop().await;
        assert!(superuser.contains("NOSUPERUSER"), "{server}: {superuser}");
        assert!(member.contains("NOCREATEDB"), "{server}: {member}");
        assert!(
            reach.contains("no membership in pg_execute_server_program")
                && reach.contains("NOREPLICATION")
                && reach.contains(&format!("no ownership of database {owned_elsewhere}"))
                && reach.contains(&format!("no admin option on role {admin_of}"))
                && reach.contains(&format!("no grant option on database {other}"))
                && reach.contains(&format!("no ALTER SYSTEM on parameter {parameter}"))
                && reach.contains(&format!("no use of login role {signalled}")),
            "{server}: {reach}"
        );
        assert_eq!(
            reaching_left.1, 0,
            "{server}: nothing was written: {reaching_left:?}"
        );
        assert_eq!(before, after, "{server}: no database or role was created");
        assert_eq!(left.1, 0, "{server}: nothing was written: {left:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_scratch_that_is_not_empty() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let leftover = format!(
            "CREATE TABLE public.leftover (i integer); ALTER TABLE public.leftover OWNER TO {}",
            login.0
        );
        fixture.run(&scratch_db, &[&leftover]).await;
        let reason = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let left = fixture.foreign_objects(&scratch_db).await;
        // On the target's cluster, owning another database is a reach out
        // of the scratch database, so each case's database is handed back
        // before the next one is made.
        let owner_back = format!("ALTER DATABASE {} OWNER TO CURRENT_USER", scratch_db);
        fixture.run(&target_db, &[&owner_back]).await;
        // A subscription is not in the database's own catalogs but belongs
        // to it all the same: a database holding only one is not empty.
        let subscribed = fixture.database("b", Some(&login.0)).await;
        fixture
            .run(
                &subscribed,
                &[
                    "CREATE SUBSCRIPTION leftover_sub CONNECTION 'dbname=nowhere' PUBLICATION p \
                     WITH (connect = false, enabled = false, slot_name = NONE)",
                    &format!("ALTER SUBSCRIPTION leftover_sub OWNER TO {}", login.0),
                ],
            )
            .await;
        let subscription = resolver_refusal(
            produce(
                &fixture.as_login(&subscribed, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        // A large object's OID is the creator's to choose, so no cutoff
        // tells it from initdb's, which made none.
        let owner_back = format!("ALTER DATABASE {} OWNER TO CURRENT_USER", subscribed);
        fixture.run(&target_db, &[&owner_back]).await;
        let lob = fixture.database("l", Some(&login.0)).await;
        fixture.run(&lob, &["SELECT pg_catalog.lo_create(1)"]).await;
        let large_object = resolver_refusal(
            produce(
                &fixture.as_login(&lob, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let lob_left = fixture.foreign_objects(&lob).await;
        let owner_back = format!("ALTER DATABASE {} OWNER TO CURRENT_USER", lob);
        fixture.run(&target_db, &[&owner_back]).await;
        // A database holding a subscription cannot be dropped.
        fixture
            .run(&subscribed, &["DROP SUBSCRIPTION leftover_sub"])
            .await;
        // Negative: a database the login does not own is refused too, since
        // emptying it afterwards would revoke what was granted on it.
        let foreign = fixture.database("f", None).await;
        let unowned = resolver_refusal(
            produce(
                &fixture.as_login(&foreign, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        fixture.drop().await;
        assert!(
            reason.contains("not empty") && reason.contains("leftover"),
            "{server}: {reason}"
        );
        assert!(
            left.0.iter().any(|object| object.contains("leftover")),
            "{server}: a refused run removes nothing it did not create: {left:?}"
        );
        assert!(unowned.contains("does not own"), "{server}: {unowned}");
        assert!(
            subscription.contains("not empty") && subscription.contains("leftover_sub"),
            "{server}: {subscription}"
        );
        assert!(
            large_object.contains("not empty") && large_object.contains("large object 1"),
            "{server}: {large_object}"
        );
        assert!(
            lob_left.0.iter().any(|object| object == "large object 1"),
            "{server}: the large object is kept: {lob_left:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_empties_the_supplied_database_when_the_run_refuses() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        // Without pg_read_all_settings the login cannot read the settings
        // the rule compares: the run refuses after it created the schemas.
        let login = fixture
            .login(
                "u",
                "NOSUPERUSER NOCREATEDB NOCREATEROLE CONNECTION LIMIT 1",
            )
            .await;
        let scratch_db = fixture.database("s", Some(&login.0)).await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
        )
        .await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert!(
            matches!(&result, Err(ProduceError::Run(Error::Scope(reason))) if reason.contains("setting:shared_preload_libraries")),
            "{server}: {:?}",
            result.err().map(|e| e.to_string())
        );
        assert_eq!(left.1, 0, "{server}: {left:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_supplied_login_whose_cleanup_would_revoke_grants_elsewhere() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        // The login is a member of the role that owns another database and
        // granted it a privilege there: `DROP OWNED` would revoke that grant.
        let owner = format!("pbps_v1672_w_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {owner} NOLOGIN; GRANT {owner} TO {}",
                login.0
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, owner.clone());
        let elsewhere = fixture.database("w", Some(&owner)).await;
        fixture
            .admin()
            .await
            .execute(&format!(
                "SET ROLE {owner}; GRANT CREATE ON DATABASE {elsewhere} TO {}; RESET ROLE",
                login.0
            ))
            .await
            .unwrap();
        let acl = || async {
            let rows = fixture
                .admin()
                .await
                .query(&format!(
                    "SELECT datacl::text AS acl FROM pg_catalog.pg_database WHERE datname = '{elsewhere}'"
                ))
                .await
                .unwrap();
            rows[0].try_get::<&str>("acl").unwrap().unwrap().to_owned()
        };
        let before = acl().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let reason = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let after = acl().await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert!(
            reason.contains(&format!("database {elsewhere}")),
            "{server}: {reason}"
        );
        assert_eq!(before, after, "{server}: the grant elsewhere is untouched");
        assert_eq!(left.1, 0, "{server}: nothing was written: {left:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compiles_as_a_non_superuser_provisioner_in_its_own_database() {
    // A `CREATEROLE CREATEDB` login on another cluster cannot act as the
    // roles it would create, so it takes the supplied layout, not run-owned.
    // Neither can a member of a superuser role: `SUPERUSER` is not
    // inherited, and the run never `SET ROLE`s (#1678 review).
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let login = scratch.login("p", "NOSUPERUSER CREATEDB CREATEROLE").await;
    let superuser = scratch.login("u", "SUPERUSER").await;
    scratch
        .admin()
        .await
        .execute(&format!(
            "GRANT pg_read_all_settings TO {login}; GRANT {superuser} TO {login}",
            login = login.0,
            superuser = superuser.0
        ))
        .await
        .unwrap();
    let scratch_db = scratch.database("s", Some(&login.0)).await;
    // On another cluster, owning another database there is nothing of the
    // target's, and it is outside `DROP OWNED`'s reach: no refusal for it.
    scratch.database("o", Some(&login.0)).await;
    let before = scratch.inventory().await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let result = produce(
        &scratch.as_login(&scratch_db, &login),
        &target.on(&target_db),
        &inputs,
        &key,
    )
    .await;
    let after = scratch.inventory().await;
    let left = scratch.foreign_objects(&scratch_db).await;
    target.drop().await;
    scratch.drop().await;
    assert_answers_the_overload(&result.unwrap());
    assert_eq!(before, after, "no run-owned database or role was created");
    assert_eq!(left.1, 0, "{left:?}");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_cleanup_runs_only_on_the_backend_the_run_checked() {
    // A transaction pooler can hand the cleanup another backend than the one
    // the run checked. Two connections stand in for it: the cleanup is
    // bound to the first one's backend and runs on the second.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let login = fixture
            .login("k", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
            .await;
        let scratch_db = fixture.database("s", Some(&login.0)).await;
        let leftover = format!(
            "CREATE TABLE public.kept (i integer); ALTER TABLE public.kept OWNER TO {}",
            login.0
        );
        fixture.run(&scratch_db, &[&leftover]).await;
        let as_login = fixture.as_login(&scratch_db, &login);
        let mut checked = Conn::connect(Driver::Postgres, &as_login).await.unwrap();
        let mut other = Conn::connect(Driver::Postgres, &as_login).await.unwrap();
        let backend = pbps_pg::resolver::scratch::backend(&mut checked)
            .await
            .unwrap();
        let moved = pbps_pg::resolver::scratch::drop_owned(&mut other, &backend).await;
        let after_moved = fixture.foreign_objects(&scratch_db).await;
        let same = pbps_pg::resolver::scratch::drop_owned(&mut checked, &backend).await;
        let after_same = fixture.foreign_objects(&scratch_db).await;
        drop((checked, other));
        fixture.drop().await;
        assert!(
            moved
                .as_ref()
                .is_err_and(|e| e.to_string().contains("another backend")),
            "{server}: {moved:?}"
        );
        assert!(
            after_moved.0.iter().any(|object| object.contains("kept")),
            "{server}: nothing was dropped on the other backend: {after_moved:?}"
        );
        assert!(same.is_ok(), "{server}: {same:?}");
        assert_eq!(after_same.1, 0, "{server}: {after_same:?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compiles_as_the_login_whatever_its_session_defaults() {
    // The login's own defaults: a time zone, a date style and a string
    // mode the compile pins away, and a default role it is a member of. The run still binds
    // its backend, compiles as the login and empties the database.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        // A stored default the driver's startup packet overrides on the
        // target's session: replaying it on scratch would make the two
        // differ (#1678 review).
        fixture
            .admin()
            .await
            .execute(&format!(
                "ALTER DATABASE {target_db} SET client_encoding = 'LATIN1'"
            ))
            .await
            .unwrap();
        let (login, scratch_db) = fixture.confined().await;
        let worker = format!("pbps_v1672_r_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {worker} NOLOGIN; GRANT {worker} TO {login}; \
                 GRANT CREATE ON DATABASE {scratch_db} TO {worker}; \
                 ALTER ROLE {login} IN DATABASE {scratch_db} SET role = {worker}; \
                 ALTER ROLE {login} SET TimeZone = 'Asia/Taipei'; \
                 ALTER ROLE {login} SET DateStyle = 'SQL, DMY'; \
                 ALTER ROLE {login} SET standard_conforming_strings = off",
                login = login.0
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, worker);
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
        )
        .await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        let plan = result.map_err(|error| format!("{server}: {error}"));
        assert_eq!(
            left.1, 0,
            "{server}: the run empties its database: {left:?}"
        );
        assert_answers_the_overload(&plan.unwrap());
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_answers_xml_fragments_for_a_target_whose_xmloption_is_document() {
    // `xmloption` is a pin of the dialect (DEC-1634.1): the compile, the
    // captures and every deployment run under `content`, so what the
    // target's ambient value is decides nothing the resolver reproduces
    // (#1997, DEC-520). Two ways a target reads `document`: a database
    // default, which the scratch is given a copy of, and a startup option,
    // which stands in for a server-level value (`postgresql.conf`,
    // `ALTER SYSTEM`) and is replayed nowhere. The first was refused at the
    // capture ("session inputs changed"), the second as a mismatch.
    let mut refused = Vec::new();
    for server in SERVERS {
        for how in ["a database default", "a startup option"] {
            let mut fixture = Fixture::new(server);
            let target_db = fixture.target().await;
            let target = if how == "a database default" {
                fixture
                    .admin()
                    .await
                    .execute(&format!(
                        "ALTER DATABASE {target_db} SET xmloption = document"
                    ))
                    .await
                    .unwrap();
                fixture.on(&target_db)
            } else {
                format!("{} options='-c xmloption=document'", fixture.on(&target_db))
            };
            // The premise: the planning session reads documents, so the
            // fragments the plan declares are invalid in it.
            let mut planning = Conn::connect(Driver::Postgres, &target).await.unwrap();
            let read = planning.query("SHOW xmloption").await.unwrap();
            let ambient = read[0]
                .try_get::<&str>("xmloption")
                .unwrap()
                .map(str::to_owned);
            let fragment = planning.query("SELECT '<a/><b/>'::pg_catalog.xml").await;
            drop(planning);
            let (login, scratch_db) = fixture.confined().await;
            let inputs = Inputs::from_pair(cases::pair_with_xml_fragments());
            let key = ProjectKey::new(true);
            let result = produce(
                &fixture.as_login(&scratch_db, &login),
                &target,
                &inputs,
                &key,
            )
            .await;
            let left = fixture.foreign_objects(&scratch_db).await;
            fixture.drop().await;
            assert_eq!(
                ambient.as_deref(),
                Some("document"),
                "{server} {how}: the target's session reads documents"
            );
            assert!(
                fragment.is_err(),
                "{server} {how}: a fragment is refused in that session"
            );
            assert_eq!(
                left.1, 0,
                "{server} {how}: the run empties its database: {left:?}"
            );
            let plan = match result {
                Ok(plan) => plan,
                Err(error) => {
                    refused.push(format!("{server} {how}: {error}"));
                    continue;
                }
            };
            plan.evidence.validate(&plan.changes).unwrap();
            for name in ["xv", "xf"] {
                assert!(
                    plan.changes.changes.iter().any(|step| matches!(
                        &step.change,
                        Change::CreateModule { id, .. } if id.name().starts_with(name)
                    )),
                    "{server} {how}: the plan creates {name}"
                );
            }
        }
    }
    assert!(refused.is_empty(), "a valid plan was refused: {refused:#?}");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_provisions_as_the_superuser_login_whatever_its_default_role() {
    // A superuser login whose default role cannot create roles: every
    // connection the run opens as it must act as the login itself, and so
    // must every step that switches role and back, such as replaying a
    // schema's grant as its grantor (#1678 review).
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    target
        .run(
            &target_db,
            &["GRANT USAGE ON SCHEMA pbps_evidence1274 TO PUBLIC"],
        )
        .await;
    let login = scratch.login("a", "SUPERUSER").await;
    let worker = format!("pbps_v1672_r_{}", scratch.token);
    scratch
        .admin()
        .await
        .execute(&format!(
            "CREATE ROLE {worker} NOLOGIN NOCREATEROLE NOCREATEDB; GRANT {worker} TO {login}; \
             ALTER ROLE {login} SET role = {worker}",
            login = login.0
        ))
        .await
        .unwrap();
    scratch.roles.insert(0, worker.clone());
    let before = scratch.inventory().await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let result = produce(
        &scratch.as_login("pbps_test", &login),
        &target.on(&target_db),
        &inputs,
        &key,
    )
    .await;
    let after = scratch.inventory().await;
    // The same role named by the connection string's startup options: the
    // run's own login must not inherit them.
    let optioned = format!(
        "{} options='-c role={worker}'",
        scratch.as_login("pbps_test", &login)
    );
    let through_options = produce(&optioned, &target.on(&target_db), &inputs, &key).await;
    let after_options = scratch.inventory().await;
    target.drop().await;
    scratch.drop().await;
    let plan = result.map_err(|error| error.to_string());
    let through_options = through_options.map_err(|error| error.to_string());
    assert_eq!(before, after, "the run-owned objects are all dropped");
    assert_eq!(
        before, after_options,
        "the run-owned objects are all dropped"
    );
    assert_answers_the_overload(&plan.unwrap());
    assert_answers_the_overload(&through_options.unwrap());
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_supplied_login_whose_cleanup_would_revoke_memberships_it_granted() {
    // The login holds `ADMIN OPTION` on a role and granted it to another
    // role. `DROP OWNED` would remove that membership, which lives outside
    // the scratch database (#1678 review). On the target's cluster the
    // `ADMIN OPTION` alone already refuses, and a grantor cannot lose it
    // while its grants stand, so this is a scratch server's own case.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut fixture = Fixture::new(SCRATCH_SERVER);
    let server = SCRATCH_SERVER;
    let target_db = target.target().await;
    let (login, scratch_db) = fixture.confined().await;
    let app = format!("pbps_v1672_a_{}", fixture.token);
    let user = format!("pbps_v1672_u_{}", fixture.token);
    fixture
        .admin()
        .await
        .execute(&format!(
            "CREATE ROLE {app} NOLOGIN; CREATE ROLE {user} NOLOGIN; \
             GRANT {app} TO {login} WITH ADMIN OPTION, INHERIT FALSE; \
             SET ROLE {login}; GRANT {app} TO {user}; \
             GRANT {app} TO {login} WITH INHERIT TRUE GRANTED BY {login}; RESET ROLE",
            login = login.0
        ))
        .await
        .unwrap();
    fixture.roles.insert(0, user.clone());
    fixture.roles.insert(0, app.clone());
    let granted = || async {
        let rows = fixture
            .admin()
            .await
            .query(&format!(
                "SELECT count(*)::text AS n FROM pg_catalog.pg_auth_members a \
                 JOIN pg_catalog.pg_roles m ON m.oid = a.member \
                 JOIN pg_catalog.pg_roles g ON g.oid = a.grantor \
                 WHERE m.rolname = '{user}' \
                    OR (m.rolname = '{login}' AND g.rolname = '{login}')",
                login = login.0
            ))
            .await
            .unwrap();
        rows[0].try_get::<&str>("n").unwrap().unwrap().to_owned()
    };
    let before = granted().await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let reason = resolver_refusal(
        produce(
            &fixture.as_login(&scratch_db, &login),
            &target.on(&target_db),
            &inputs,
            &key,
        )
        .await,
    );
    let after = granted().await;
    let left = fixture.foreign_objects(&scratch_db).await;
    target.drop().await;
    fixture.drop().await;
    // The membership it granted another role, and the one it granted
    // itself: `DROP OWNED` removes both by grantor.
    assert!(
        reason.contains(&format!("membership of {user} in {app}"))
            && reason.contains(&format!("membership of {} in {app}", login.0)),
        "{server}: {reason}"
    );
    assert_eq!(
        (before.as_str(), after.as_str()),
        ("2", "2"),
        "{server}: both memberships are kept"
    );
    assert_eq!(left.1, 0, "{server}: nothing was written: {left:?}");
}

/// What differs from `standard` in a supplied scratch database, read as its
/// login after the run has let go of its one connection.
async fn not_standard(
    fixture: &Fixture,
    database: &str,
    login: &(String, String),
    standard: &Standard,
    acl: Option<&std::collections::BTreeSet<pbps_pg::resolver::standard::Entry>>,
) -> Vec<String> {
    let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(database, login))
        .await
        .unwrap();
    pbps_pg::resolver::standard::verify(&mut conn, standard, acl)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_repairs_a_supplied_scratch_an_interrupted_run_left_changed() {
    // What a run that died before its release can leave, changed by the
    // login itself: the next run puts it back into its standard state
    // first, and answers (#1708).
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let other = fixture.role("o").await;
        {
            let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
                .await
                .unwrap();
            for statement in [
                "ALTER SCHEMA public RENAME TO elsewhere".to_owned(),
                format!("ALTER SCHEMA elsewhere OWNER TO {}", login.0),
                "COMMENT ON SCHEMA elsewhere IS 'changed'".to_owned(),
                format!("GRANT USAGE ON SCHEMA elsewhere TO {other} WITH GRANT OPTION"),
                format!("GRANT CREATE ON SCHEMA elsewhere TO {other}"),
                format!("ALTER DATABASE {scratch_db} CONNECTION LIMIT 5"),
                format!("ALTER DATABASE {scratch_db} IS_TEMPLATE true"),
                format!("ALTER DATABASE {scratch_db} SET statement_timeout = '7s'"),
                format!("ALTER ROLE CURRENT_USER IN DATABASE {scratch_db} SET work_mem = '8MB'"),
                format!("COMMENT ON DATABASE {scratch_db} IS 'changed'"),
            ] {
                conn.execute(&statement).await.unwrap();
            }
        }
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let result = produce(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
        )
        .await;
        let left = not_standard(&fixture, &scratch_db, &login, &Standard::default(), None).await;
        fixture.drop().await;
        assert_answers_the_overload(&result.unwrap());
        assert!(left.is_empty(), "{server}: {left:#?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_applies_a_declared_standard_and_keeps_a_hardened_database_acl() {
    // The declared settings, comments, limit and grant are applied over a
    // scratch configured otherwise, and kept through the release; the
    // database's own hardened ACL is the operator's and stays (#1708).
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let reader = fixture.role("r").await;
        fixture
            .admin()
            .await
            .execute(&format!(
                "REVOKE CONNECT, TEMPORARY ON DATABASE {scratch_db} FROM PUBLIC; \
                 COMMENT ON DATABASE {scratch_db} IS 'configured otherwise'"
            ))
            .await
            .unwrap();
        let hardened = {
            let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
                .await
                .unwrap();
            pbps_pg::resolver::standard::read_state(&mut conn)
                .await
                .unwrap()
                .acl
        };
        let declared = Standard {
            settings: [("statement_timeout".to_owned(), "5min".to_owned())].into(),
            comment: Some("pbps scratch".into()),
            connection_limit: 10,
            public_comment: Some("ours".into()),
            public_grants: [(reader.clone(), "USAGE".to_owned())].into(),
        };
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let result = produce_with(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            &declared,
        )
        .await;
        let left = not_standard(&fixture, &scratch_db, &login, &declared, Some(&hardened)).await;
        // Negative: the built-in standard would see each declared item.
        let builtin = not_standard(&fixture, &scratch_db, &login, &Standard::default(), None).await;
        fixture.drop().await;
        assert_answers_the_overload(&result.unwrap());
        assert!(left.is_empty(), "{server}: {left:#?}");
        assert_eq!(builtin.len(), 5, "{server}: {builtin:#?}");
        assert!(
            !hardened.iter().any(|entry| entry.grantee == "PUBLIC"),
            "{server}: the fixture hardened the ACL: {hardened:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_declared_setting_the_scratch_account_may_not_set_before_writing() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        // Not standard, so a write would show: the run must refuse first.
        fixture
            .admin()
            .await
            .execute(&format!("COMMENT ON DATABASE {scratch_db} IS 'untouched'"))
            .await
            .unwrap();
        let declared = Standard {
            settings: [("log_min_duration_statement".to_owned(), "5".to_owned())].into(),
            ..Standard::default()
        };
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let reason = resolver_refusal(
            produce_with(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                &declared,
            )
            .await,
        );
        let left = not_standard(&fixture, &scratch_db, &login, &Standard::default(), None).await;
        fixture.drop().await;
        assert!(
            reason.contains("log_min_duration_statement")
                && reason.contains("may not set on its database"),
            "{server}: {reason}"
        );
        assert_eq!(
            left,
            ["the database's comment is \"untouched\""],
            "{server}: nothing was written"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_declared_value_the_engine_rejects_before_writing() {
    // A permitted setting with a value the engine cannot parse: the
    // declared state is tried and rolled back first, so the run refuses
    // naming it, and the comment that would have been written first stays
    // as it was (#1708 review).
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let declared = Standard {
            settings: [("statement_timeout".to_owned(), "nonsense".to_owned())].into(),
            comment: Some("ours".into()),
            ..Standard::default()
        };
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let reason = resolver_refusal(
            produce_with(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                &declared,
            )
            .await,
        );
        let left = not_standard(&fixture, &scratch_db, &login, &Standard::default(), None).await;
        fixture.drop().await;
        assert!(
            reason.contains("statement_timeout") && reason.contains("nothing was written"),
            "{server}: {reason}"
        );
        assert!(left.is_empty(), "{server}: nothing was written: {left:#?}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_public_grant_chain_it_cannot_revoke_and_leaves_the_acl() {
    // A role granted USAGE on public with its grant option passed it on:
    // the onward entry is that role's to revoke, not the login's. The run
    // refuses naming it, and the ACL stays as it was (#1708, #1678 review).
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let other = fixture.role("o").await;
        let onward = fixture.role("w").await;
        fixture
            .run(
                &scratch_db,
                &[
                    &format!("GRANT USAGE ON SCHEMA public TO {other} WITH GRANT OPTION"),
                    &format!("SET ROLE {other}"),
                    &format!("GRANT USAGE ON SCHEMA public TO {onward}"),
                    "RESET ROLE",
                ],
            )
            .await;
        let acl = || async {
            let mut conn = Conn::connect(Driver::Postgres, &fixture.on(&scratch_db))
                .await
                .unwrap();
            pbps_pg::resolver::standard::read_state(&mut conn)
                .await
                .unwrap()
                .public
        };
        let before = acl().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let reason = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let after = acl().await;
        fixture.drop().await;
        assert!(
            reason.contains(&format!("which only {other} can revoke")) && reason.contains(&onward),
            "{server}: {reason}"
        );
        assert_eq!(before, after, "{server}: the ACL was left alone");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_release_puts_a_supplied_scratch_back_and_names_what_it_cannot() {
    // What a compiled definition may change during the run, put back by
    // the release on the run's own connection; and a setting the login may
    // not remove, named rather than reported as a clean release (#1708).
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let (login, scratch_db) = fixture.confined().await;
        let other = fixture.role("o").await;
        let standard = Standard::default();
        let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
            .await
            .unwrap();
        let backend = pbps_pg::resolver::scratch::backend(&mut conn)
            .await
            .unwrap();
        let defaults = pbps_pg::resolver::scratch::login_defaults(&mut conn)
            .await
            .unwrap();
        let acl = pbps_pg::resolver::standard::read_state(&mut conn)
            .await
            .unwrap()
            .acl;
        for statement in [
            "SET ROLE pg_database_owner".to_owned(),
            "COMMENT ON SCHEMA public IS 'changed'".to_owned(),
            format!("GRANT CREATE ON SCHEMA public TO {other}"),
            "SET ROLE NONE".to_owned(),
            "ALTER SCHEMA public RENAME TO elsewhere".to_owned(),
            // The run's own connection stays open: 0 locks out only the
            // next one, which the release must prevent.
            format!("ALTER DATABASE {scratch_db} CONNECTION LIMIT 0"),
            format!("ALTER DATABASE {scratch_db} IS_TEMPLATE true"),
            format!("ALTER DATABASE {scratch_db} SET statement_timeout = '7s'"),
            format!("COMMENT ON DATABASE {scratch_db} IS 'changed'"),
            format!("REVOKE CONNECT ON DATABASE {scratch_db} FROM PUBLIC"),
        ] {
            conn.execute(&statement).await.unwrap();
        }
        let released =
            super::release_supplied(&mut conn, &backend, &standard, &scratch_db, &defaults, &acl)
                .await;
        // Negative: one the login may not remove is named.
        fixture
            .admin()
            .await
            .execute(&format!(
                "ALTER DATABASE {scratch_db} SET log_min_duration_statement = 5"
            ))
            .await
            .unwrap();
        let unremovable =
            super::release_supplied(&mut conn, &backend, &standard, &scratch_db, &defaults, &acl)
                .await;
        drop(conn);
        let left = not_standard(&fixture, &scratch_db, &login, &standard, Some(&acl)).await;
        fixture.drop().await;
        assert_eq!(released, Ok(()), "{server}");
        let named = unremovable.expect_err("a setting the login may not remove");
        assert!(
            named.iter().any(|item| item.contains(
                "not back in its standard state: the database sets log_min_duration_statement=5"
            )) && named.iter().any(|item| item.starts_with("refused: ")),
            "{server}: {named:#?}"
        );
        assert_eq!(
            left,
            ["the database sets log_min_duration_statement=5"],
            "{server}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_run_owned_keeps_what_the_reproduction_put_on_its_database() {
    // The declared limit goes on after the run login's session opens, and
    // only the limit: the reproduction has by then rebuilt an in-scope
    // `public` and stored the target's database defaults, which a second
    // full standardization would refuse and reset (#1708 review).
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    target
        .run(
            &target_db,
            &[&format!(
                r#"ALTER DATABASE "{target_db}" SET work_mem = '8MB'"#
            )],
        )
        .await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let before = scratch.inventory().await;
    let target_url = target.on(&target_db);
    let mut planning = Conn::connect(Driver::Postgres, &target_url).await.unwrap();
    let identity = pbps_pg::resolver::scratch::cluster_identity(&mut planning)
        .await
        .unwrap();
    // `public` on the write path puts it in the reproduction's scope.
    let extras = ["public".to_owned()];
    let mut answers = Vec::new();
    for standard in [
        Standard {
            connection_limit: 1,
            ..Standard::default()
        },
        Standard::default(),
    ] {
        answers.push(
            super::produce(
                Driver::Postgres,
                &scratch.server,
                &target_url,
                &identity,
                &inputs.binding(),
                inputs.base(),
                inputs.desired(),
                &inputs.hints,
                &extras,
                &key.project,
                Some(ENVIRONMENT),
                &standard,
                None,
                &super::Provenance::server(),
            )
            .await,
        );
    }
    let unlimited = answers.pop().unwrap();
    let limited = answers.pop().unwrap();
    let after = scratch.inventory().await;
    target.drop().await;
    assert_answers_the_overload(&limited.unwrap());
    // Negative: without a declared limit nothing is put on after the
    // reproduction either.
    assert_answers_the_overload(&unlimited.unwrap());
    assert_eq!(
        before, after,
        "the run-owned database and roles are dropped"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_run_owned_compiles_under_the_declared_standard() {
    // The database a superuser scratch creates gets the same declared state
    // as a supplied one (#1708): a declared grant to a role the scratch
    // server lacks refuses before anything is compiled, and a declared
    // setting, comment and limit are applied and answered under. A limit of
    // 1 still admits the run's own two sessions (#1708 review).
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = target.target().await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let missing = format!("pbps_v1708_absent_{}", scratch.token);
    let before = scratch.inventory().await;
    let refused = resolver_refusal(
        produce_with(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &Standard {
                public_grants: [(missing.clone(), "USAGE".to_owned())].into(),
                ..Standard::default()
            },
        )
        .await,
    );
    let answered = produce_with(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &Standard {
            settings: [("statement_timeout".to_owned(), "5min".to_owned())].into(),
            comment: Some("pbps scratch".into()),
            connection_limit: 1,
            ..Standard::default()
        },
    )
    .await;
    let after = scratch.inventory().await;
    target.drop().await;
    assert!(
        refused.contains(&format!("to {missing}, a role that does not exist")),
        "{refused}"
    );
    assert_answers_the_overload(&answered.unwrap());
    assert_eq!(
        before, after,
        "the run-owned database and roles are dropped"
    );
}

// ---- The externals file (#1673) ----------------------------------------------
//
// A managed schema and an external one. The target holds external objects
// the managed views bind; the externals file stages them on scratch.

const MANAGED: &str = "pbps_v1673";
const EXTERNAL: &str = "pbps_x1673";

/// Declarations of views alone, plus any tables, in the managed schema.
fn declared(views: &[(&str, &str)], tables: &[&str]) -> pbps_model::Schema {
    let mut schema = pbps_model::Schema::default();
    for (name, definition) in views {
        schema.modules.insert(
            format!("{MANAGED}.{name}").parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: (*definition).to_owned(),
                options: Default::default(),
            },
        );
    }
    for table in tables {
        let mut declared = pbps_model::Table::default();
        declared.columns.insert(
            "id".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        schema
            .tables
            .insert(format!("{MANAGED}.{table}").parse().unwrap(), declared);
    }
    schema
}

/// A plan that keeps `a` and adds `b`, both reading what `read` names.
fn staging_inputs(a: &str, b: &str, tables: &[&str]) -> Inputs {
    Inputs::from_pair((
        declared(&[("a", a)], tables),
        declared(&[("a", a), ("b", b)], tables),
    ))
}

async fn produce_staged(
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    externals: &str,
) -> Result<ResolvedPlan, ProduceError> {
    produce_filled(scratch, target, inputs, key, Some(externals)).await
}

/// `produce_staged`, with an externals file or none.
async fn produce_filled(
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    externals: Option<&str>,
) -> Result<ResolvedPlan, ProduceError> {
    let mut planning = Conn::connect(Driver::Postgres, target).await.unwrap();
    let identity = pbps_pg::resolver::scratch::cluster_identity(&mut planning)
        .await
        .unwrap();
    super::produce(
        Driver::Postgres,
        scratch,
        target,
        &identity,
        &inputs.binding(),
        inputs.base(),
        inputs.desired(),
        &inputs.hints,
        &[],
        &key.project,
        Some(ENVIRONMENT),
        &Standard::default(),
        externals,
        &super::Provenance::server(),
    )
    .await
}

/// `produce_filled`, with write-path extras.
async fn produce_with_extras(
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    extras: &[String],
) -> Result<ResolvedPlan, ProduceError> {
    let mut planning = Conn::connect(Driver::Postgres, target).await.unwrap();
    let identity = pbps_pg::resolver::scratch::cluster_identity(&mut planning)
        .await
        .unwrap();
    super::produce(
        Driver::Postgres,
        scratch,
        target,
        &identity,
        &inputs.binding(),
        inputs.base(),
        inputs.desired(),
        &inputs.hints,
        extras,
        &key.project,
        Some(ENVIRONMENT),
        &Standard::default(),
        None,
        &super::Provenance::server(),
    )
    .await
}

/// `produce_staged`, with `sql` run on the target after the externals file was
/// compared with it and before the opening capture.
async fn produce_staged_then(
    sql: &str,
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    externals: &str,
) -> Result<ResolvedPlan, ProduceError> {
    super::AFTER_STAGING
        .scope(
            Some(sql.to_owned()),
            produce_staged(scratch, target, inputs, key, externals),
        )
        .await
}

/// The externals file's refusal, or what came instead. Never panics, so the
/// caller can drop its fixture first.
fn externals_refusal(result: Result<ResolvedPlan, ProduceError>) -> String {
    match result {
        Err(ProduceError::Run(Error::Externals(findings))) => findings.join("; "),
        Err(ProduceError::Run(Error::Creation(failure))) if failure.externals => {
            failure.to_string()
        }
        Err(other) => format!("not an externals file refusal: {other}"),
        Ok(_) => "not an externals file refusal: a plan".into(),
    }
}

/// The plan answered, and what the externals file staged was sealed on the
/// target's side.
fn assert_staged(result: Result<ResolvedPlan, ProduceError>, staged: &str, context: &str) {
    let plan = match result {
        Ok(plan) => plan,
        Err(error) => panic!("{context}: {error}"),
    };
    plan.evidence.validate(&plan.changes).unwrap();
    pbps_pg::resolver::validate_evidence(&plan.evidence).unwrap();
    let sealed = plan
        .evidence
        .before()
        .scope()
        .retained
        .iter()
        .any(|object| object.name == [EXTERNAL, staged]);
    assert!(sealed, "{context}: {staged} is sealed");
    let created =
        plan.changes.changes.iter().any(
            |step| matches!(&step.change, Change::CreateModule { id, .. } if id.name() == "b"),
        );
    assert!(created, "{context}: the plan creates b");
}

/// The external table, with what the comparison skips: a foreign key, a
/// CHECK, a default, a trigger and a plain index.
fn guarded_target() -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE TABLE {EXTERNAL}.p (id integer PRIMARY KEY)"),
        format!(
            "CREATE TABLE {EXTERNAL}.t (id integer PRIMARY KEY, name text UNIQUE, \
             p integer REFERENCES {EXTERNAL}.p, n integer DEFAULT 1 CHECK (n > 0))"
        ),
        format!("CREATE INDEX t_n ON {EXTERNAL}.t (n)"),
        format!(
            "CREATE FUNCTION {EXTERNAL}.audit() RETURNS trigger LANGUAGE plpgsql \
             AS $$BEGIN RETURN NEW; END$$"
        ),
        format!(
            "CREATE TRIGGER t_audit BEFORE INSERT ON {EXTERNAL}.t \
             FOR EACH ROW EXECUTE FUNCTION {EXTERNAL}.audit()"
        ),
        format!("CREATE VIEW {MANAGED}.a AS SELECT id FROM {EXTERNAL}.t"),
    ]
}

/// The table as an externals file writes it: its shape, nothing that guards it.
fn plain_externals(id_type: &str) -> String {
    format!(
        "-- The external table the managed views read.\n\
         CREATE SCHEMA {EXTERNAL};\n\
         CREATE TABLE {EXTERNAL}.t (id {id_type} PRIMARY KEY, name text UNIQUE, p integer, n integer);\n"
    )
}

async fn staging_target(fixture: &mut Fixture, setup: &[String]) -> String {
    let target = fixture.database("t", None).await;
    let statements: Vec<&str> = setup.iter().map(String::as_str).collect();
    fixture.run(&target, &statements).await;
    target
}

fn reading(column: &str, relation: &str) -> String {
    format!("SELECT {column} FROM {EXTERNAL}.{relation}")
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compiles_a_managed_view_over_an_externals_table() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    // Run-owned: a superuser scratch on another cluster.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let before = scratch.inventory().await;
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &plain_externals("integer"),
    )
    .await;
    let after = scratch.inventory().await;
    target.drop().await;
    assert_staged(result, "t", "run-owned");
    assert_eq!(before, after, "the run-owned database is dropped");
    // Supplied: a confined account in its own database, on both servers.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &guarded_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let result = produce_staged(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            &plain_externals("integer"),
        )
        .await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert_staged(result, "t", server);
        assert_eq!(
            left.1, 0,
            "{server}: the externals file's objects are dropped too"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_an_external_table_without_its_foreign_keys_and_triggers() {
    // The externals file leaves out the referenced table, the CHECK, the default,
    // the trigger and its function, and the plain index.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &guarded_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let inputs = staging_inputs(&reading("id", "t"), &reading("n", "t"), &[]);
        let key = ProjectKey::new(true);
        let result = produce_staged(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            &plain_externals("integer"),
        )
        .await;
        fixture.drop().await;
        assert_staged(result, "t", server);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_externals_file_that_differs_from_the_target() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let mut setup = guarded_target();
        setup.push(format!(
            "CREATE TYPE {EXTERNAL}.pair AS (a integer, b text)"
        ));
        let target_db = staging_target(&mut fixture, &setup).await;
        let (login, scratch_db) = fixture.confined().await;
        let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
        let key = ProjectKey::new(true);
        // A column of another type, a unique key left out, and a composite
        // type's attributes in another order.
        let externals = format!(
            "CREATE SCHEMA {EXTERNAL};\n\
             CREATE TABLE {EXTERNAL}.t (id bigint PRIMARY KEY, name text, p integer, n integer);\n\
             CREATE TYPE {EXTERNAL}.pair AS (b text, a integer);"
        );
        let refused = externals_refusal(
            produce_staged(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                &externals,
            )
            .await,
        );
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert!(
            refused.contains(&format!(
                "the externals file's column id of relation {EXTERNAL}.t differs from the target's in"
            )) && refused.contains("atttypid"),
            "{server}: {refused}"
        );
        assert!(
            refused.contains(&format!("the target has index {EXTERNAL}.t_name_key")),
            "{server}: {refused}"
        );
        assert!(
            refused.contains(&format!(
                "the externals file's relation {EXTERNAL}.pair differs from the target's in column_order"
            )),
            "{server}: {refused}"
        );
        assert_eq!(left.1, 0, "{server}: a refused run empties its database");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_externals_file_that_creates_a_managed_object() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let externals = format!(
        "{}CREATE VIEW {MANAGED}.b AS SELECT NULL::text AS name WHERE false;",
        plain_externals("integer")
    );
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "the externals file created view {MANAGED}.b, which is in the managed set"
        )),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_runs_the_externals_file_before_any_managed_object() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    // A statement naming a managed object fails: nothing managed exists yet.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let naming = format!(
        "{}CREATE VIEW {EXTERNAL}.w AS SELECT id FROM {MANAGED}.a;",
        plain_externals("integer")
    );
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &naming,
        )
        .await,
    );
    // A database default the externals file changes is read by the compatibility
    // qualification, as the target's is.
    let defaulting = format!(
        "{}DO $$BEGIN EXECUTE format('ALTER DATABASE %I SET search_path = {EXTERNAL}', \
         current_database()); END$$;",
        plain_externals("integer")
    );
    let incompatible = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &defaulting,
    )
    .await;
    target.drop().await;
    assert!(
        refused.contains("the engine refused it")
            && refused.contains(&format!("{MANAGED}.a"))
            && refused.contains("write such an object as a shape view"),
        "{refused}"
    );
    assert!(
        matches!(incompatible, Err(ProduceError::Run(Error::Incompatible(_)))),
        "{:?}",
        incompatible.err().map(|e| e.to_string())
    );
    // Its `SET`s stay in its session: on the supplied layout the compile
    // runs on that session afterwards, and still answers.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &guarded_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let externals = format!(
            "CREATE SCHEMA {EXTERNAL};\n\
             SET search_path = {EXTERNAL};\n\
             CREATE TABLE t (id integer PRIMARY KEY, name text UNIQUE, p integer, n integer);\n\
             SET statement_timeout = '1ms';"
        );
        let result = produce_staged(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        fixture.drop().await;
        assert_staged(result, "t", server);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_externals_file_object_left_uncompared() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let mut setup = guarded_target();
    setup.push(format!(
        "CREATE CONVERSION {EXTERNAL}.words FOR 'LATIN1' TO 'UTF8' FROM pg_catalog.iso8859_1_to_utf8"
    ));
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let externals = format!(
        "{}CREATE CONVERSION {EXTERNAL}.words FOR 'LATIN1' TO 'UTF8' \
         FROM pg_catalog.iso8859_1_to_utf8;",
        plain_externals("integer")
    );
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "the externals file created conversion {EXTERNAL}.words, which the \
             comparison with the target does not cover"
        )),
        "{refused}"
    );
}

/// `ext.e`'s column is the managed table `m`'s row type, and the managed
/// view `a` reads `ext.e`: a chain through the boundary.
fn chain_target() -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE TABLE {MANAGED}.m (id integer)"),
        format!("CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m)"),
        format!("CREATE VIEW {MANAGED}.a AS SELECT id FROM {EXTERNAL}.e"),
    ]
}

/// Fill stages `ext.e` once the managed tables exist, between the
/// compile's two parts, as the setup role of either layout (#1778,
/// DEC-1778.2). Before fill such a chain refused.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_lifts_a_chain_through_an_object_fill_stages() {
    let inputs = staging_inputs(&reading("id", "e"), &reading("m", "e"), &["m"]);
    let key = ProjectKey::new(true);
    let only_the_schema = format!("CREATE SCHEMA {EXTERNAL};");
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &chain_target()).await;
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &only_the_schema,
    )
    .await;
    target.drop().await;
    assert_staged(result, "e", "run-owned");
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &chain_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let result = produce_staged(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            &only_the_schema,
        )
        .await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert_staged(result, "e", server);
        assert_eq!(left.1, 0, "{server}: what fill created is dropped too");
    }
}

/// The body of `f`, the declared function `m`'s generated column calls.
const GENERATING: &str = "(x integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN x";

/// The target's `f` and `m`, whose generated column calls `f`: scratch
/// creates `m`'s prefix before `f` and completes its suffix after it
/// (DEC-1503.1).
fn generating_target() -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE FUNCTION {MANAGED}.f{GENERATING}"),
        format!(
            "CREATE TABLE {MANAGED}.m (id integer, \
             g integer GENERATED ALWAYS AS ({MANAGED}.f(id)) STORED)"
        ),
    ]
}

/// `schema`, declaring `f` and `m`'s generated column as the target has
/// them.
fn generating(mut schema: pbps_model::Schema) -> pbps_model::Schema {
    schema.modules.insert(
        format!("{MANAGED}.f(integer)").parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: GENERATING.into(),
            options: Default::default(),
        },
    );
    let mut generated = pbps_model::Column::new("integer".parse().unwrap());
    generated.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
        expression: format!("{MANAGED}.f(id)"),
        stored: true,
    });
    schema
        .tables
        .get_mut(&format!("{MANAGED}.m").parse().unwrap())
        .unwrap()
        .columns
        .insert("g".into(), generated);
    schema
}

/// A plan keeping `a` and adding `b`, both `definition`, beside `f` and
/// `m`.
fn generating_inputs(definition: &str) -> Inputs {
    Inputs::from_pair((
        generating(declared(&[("a", definition)], &["m"])),
        generating(declared(&[("a", definition), ("b", definition)], &["m"])),
    ))
}

/// `chain_target` over `generating_target`: scratch completes `m`'s columns
/// among the modules, and fill waits for that step (DEC-1503.1). With
/// `calling`, the managed views call `f` too, so they compile after it.
async fn after_its_function(calling: bool) -> Result<ResolvedPlan, ProduceError> {
    let filter = if calling {
        format!(" WHERE {MANAGED}.f(id) > 0")
    } else {
        String::new()
    };
    let mut setup = generating_target();
    setup.extend([
        format!("CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m)"),
        format!("CREATE VIEW {MANAGED}.a AS SELECT id FROM {EXTERNAL}.e{filter}"),
    ]);
    let a = format!("{}{filter}", reading("id", "e"));
    let b = format!("{}{filter}", reading("m", "e"));
    let inputs = Inputs::from_pair((
        generating(declared(&[("a", &a)], &["m"])),
        generating(declared(&[("a", &a), ("b", &b)], &["m"])),
    ));
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let result = produce_filled(&scratch.server, &target.on(&target_db), &inputs, &key, None).await;
    target.drop().await;
    result
}

/// The supplied layout compiles on the session chained fill ran in, and
/// fill's check ends in `RESET ALL`. A declaration after the split still
/// compiles under the deployer's settings: under the target database's
/// `array_nulls = off` the literal `{NULL}` holds the string `NULL`, which
/// is no integer, and under the default `on` it holds a null. So the plan is
/// refused there, as the target would refuse it. Negative: on a target
/// without that setting the same plan is resolved.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compiles_after_a_chained_fill_under_the_deployers_settings() {
    let filter = format!(" WHERE {MANAGED}.f(id) > 0");
    let a = format!("{}{filter}", reading("id", "e"));
    let b = format!("{a} AND '{{NULL}}'::integer[] IS NOT NULL");
    let inputs = Inputs::from_pair((
        generating(declared(&[("a", &a)], &["m"])),
        generating(declared(&[("a", &a), ("b", &b)], &["m"])),
    ));
    let key = ProjectKey::new(true);
    for (off, server) in [true, false]
        .into_iter()
        .flat_map(|off| SERVERS.iter().map(move |&server| (off, server)))
    {
        let mut setup = generating_target();
        setup.extend([
            format!("CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m)"),
            format!("CREATE VIEW {MANAGED}.a AS {a}"),
        ]);
        if off {
            setup.push(
                "DO $$BEGIN EXECUTE pg_catalog.format('ALTER DATABASE %I SET array_nulls = off', \
                 pg_catalog.current_database()); END$$"
                    .into(),
            );
        }
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &setup).await;
        let (login, scratch_db) = fixture.confined().await;
        let result = produce_filled(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            None,
        )
        .await;
        fixture.drop().await;
        if off {
            let refused = match result {
                Ok(_) => panic!("{server}: compiled with array_nulls on"),
                Err(error) => error.to_string(),
            };
            assert!(
                refused.contains("invalid input syntax for type integer"),
                "{server}: {refused}"
            );
        } else {
            assert_staged(result, "e", server);
        }
    }
}

/// A managed table `t`, created before the split, whose generated column
/// reads `ext.d(NULL)`: on the target a cast to the domain `ext.d` over
/// `m`'s row type, and on scratch, compiled before fill creates that domain,
/// a call to the externals file's `ext.d(text)`. The expression
/// compiles with its table, not as a module, and it is unresolved, never
/// sealed. Negative: without the domain the function is the binding on
/// both.
async fn generated_before_a_chained_shape(domain: bool) -> Result<ResolvedPlan, ProduceError> {
    let function = format!(
        "CREATE FUNCTION {EXTERNAL}.d(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1"
    );
    let expression = format!("{EXTERNAL}.d(NULL) IS NULL");
    let mut setup = generating_target();
    setup.push(function.clone());
    if domain {
        setup.push(format!("CREATE DOMAIN {EXTERNAL}.d AS {MANAGED}.m"));
    }
    setup.extend([
        format!(
            "CREATE TABLE {MANAGED}.t (id integer, \
             g boolean GENERATED ALWAYS AS ({expression}) STORED)"
        ),
        format!("CREATE VIEW {MANAGED}.a AS SELECT id FROM {MANAGED}.t"),
    ]);
    let with_t = |mut schema: pbps_model::Schema| {
        let mut generated = pbps_model::Column::new("boolean".parse().unwrap());
        generated.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
            expression: expression.clone(),
            stored: true,
        });
        schema
            .tables
            .get_mut(&format!("{MANAGED}.t").parse().unwrap())
            .unwrap()
            .columns
            .insert("g".into(), generated);
        generating(schema)
    };
    let a = format!("SELECT id FROM {MANAGED}.t");
    let inputs = Inputs::from_pair((
        with_t(declared(&[("a", &a)], &["m", "t"])),
        with_t(declared(&[("a", &a), ("b", &a)], &["m", "t"])),
    ));
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &format!("CREATE SCHEMA {EXTERNAL};\n{function};"),
    )
    .await;
    target.drop().await;
    result
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_leaves_unresolved_a_generated_column_a_chained_shape_would_take() {
    let plan = generated_before_a_chained_shape(false)
        .await
        .unwrap_or_else(|error| panic!("without the domain: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
    let refused = match generated_before_a_chained_shape(true).await {
        Ok(_) => panic!("a generated binding the domain would take was sealed"),
        Err(error) => error.to_string(),
    };
    assert!(
        refused.contains("the declaration was compiled before an object sharing a name it bound"),
        "{refused}"
    );
}

/// A chained table's identity column creates the sequence
/// `ext.e_id_seq`, which has no row type, so it takes no call: a view
/// compiled before the split, calling the externals file's
/// `ext.e_id_seq(text)`, binds it on both sides and is resolved. Negative:
/// a chained shape with a row type of that name leaves the call
/// unresolved.
async fn calling_a_chained_sequences_name(sequence: bool) -> Result<ResolvedPlan, ProduceError> {
    let function = format!(
        "CREATE FUNCTION {EXTERNAL}.e_id_seq(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1"
    );
    let mut setup = generating_target();
    setup.push(function.clone());
    setup.push(if sequence {
        format!(
            "CREATE TABLE {EXTERNAL}.e (id integer GENERATED ALWAYS AS IDENTITY, m {MANAGED}.m)"
        )
    } else {
        format!("CREATE TABLE {EXTERNAL}.e_id_seq (m {MANAGED}.m)")
    });
    let reader = if sequence { "e" } else { "e_id_seq" };
    let call = format!("SELECT {EXTERNAL}.e_id_seq(NULL) AS v");
    let read = format!("SELECT 1 AS v FROM {EXTERNAL}.{reader} WHERE {MANAGED}.f(1) > 0");
    setup.extend([
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
        format!("CREATE VIEW {MANAGED}.c AS {read}"),
    ]);
    let inputs = Inputs::from_pair((
        generating(declared(&[("a", &call), ("c", &read)], &["m"])),
        generating(declared(
            &[("a", &call), ("b", &call), ("c", &read)],
            &["m"],
        )),
    ));
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &format!("CREATE SCHEMA {EXTERNAL};\n{function};"),
    )
    .await;
    target.drop().await;
    result
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_resolves_a_call_named_like_a_chained_sequence() {
    let plan = calling_a_chained_sequences_name(true)
        .await
        .unwrap_or_else(|error| panic!("a sequence: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
    let refused = match calling_a_chained_sequences_name(false).await {
        Ok(_) => panic!("a call a row type would take was sealed"),
        Err(error) => error.to_string(),
    };
    assert!(
        refused.contains("the declaration was compiled before an object sharing a name it bound"),
        "{refused}"
    );
}

/// A chained fill in the supplied layout runs on a session the compile
/// pinned to the target's zone. Its outside-write check reads the cluster's
/// roles before and after, and a role's `VALID UNTIL` renders in the zone of
/// each read: both read under the same settings, and a cluster holding such
/// a role resolves the plan. The zone is one scratch's server does not
/// have.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_reads_a_roles_expiry_alike_around_a_chained_fill() {
    let role = "pbps_expiring_1778";
    let filter = format!(" WHERE {MANAGED}.f(id) > 0");
    let a = format!("{}{filter}", reading("id", "e"));
    let b = format!("{}{filter}", reading("m", "e"));
    let inputs = Inputs::from_pair((
        generating(declared(&[("a", &a)], &["m"])),
        generating(declared(&[("a", &a), ("b", &b)], &["m"])),
    ));
    let key = ProjectKey::new(true);
    for server in SERVERS {
        let mut setup = generating_target();
        setup.extend([
            format!("CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m)"),
            format!("CREATE VIEW {MANAGED}.a AS {a}"),
            "DO $$BEGIN EXECUTE pg_catalog.format('ALTER DATABASE %I SET TimeZone = %L', \
             pg_catalog.current_database(), 'Pacific/Kiritimati'); END$$"
                .into(),
        ]);
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &setup).await;
        let mut cluster = Conn::connect(Driver::Postgres, &fixture.server)
            .await
            .unwrap();
        cluster
            .execute(&format!(
                "CREATE ROLE {role} VALID UNTIL '2100-01-01 00:00:00+00'"
            ))
            .await
            .unwrap();
        let (login, scratch_db) = fixture.confined().await;
        let result = produce_filled(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            None,
        )
        .await;
        let _ = cluster
            .execute(&format!("DROP ROLE IF EXISTS {role}"))
            .await;
        fixture.drop().await;
        assert_staged(result, "e", server);
    }
}

/// A managed routine declared as `g(status)`, where `status` is an enum in
/// the write path's extra schema `shared`, is found on the target with that
/// path, so fill stages the enum it needs. Negative: the enum is what fill
/// adds; nothing else creates it.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_fills_what_a_routine_declared_through_the_extras_needs() {
    let body = "(s status) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1";
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        "CREATE SCHEMA shared".to_owned(),
        "CREATE TYPE shared.status AS ENUM ('a', 'b')".to_owned(),
        format!(
            "CREATE FUNCTION {MANAGED}.g(s shared.status) RETURNS integer \
             LANGUAGE sql IMMUTABLE RETURN 1"
        ),
    ];
    let with_g = |mut schema: pbps_model::Schema| {
        schema.modules.insert(
            format!("{MANAGED}.g(status)").parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: body.into(),
                options: Default::default(),
            },
        );
        schema
    };
    let inputs = Inputs::from_pair((
        with_g(declared(&[], &[])),
        with_g(declared(&[("b", "SELECT 1 AS x")], &[])),
    ));
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let result = produce_with_extras(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &["shared".to_owned()],
    )
    .await;
    target.drop().await;
    let plan = result.unwrap_or_else(|error| panic!("{error}"));
    plan.evidence.validate(&plan.changes).unwrap();
    assert!(
        plan.evidence
            .before()
            .scope()
            .retained
            .iter()
            .any(|object| object.name == ["shared", "status"]),
        "the enum fill staged is sealed"
    );
}

/// The outside-write check resets the session's settings, and `pbps-db`
/// pins the empty path with a plain `SET`, which the reset clears. A supplied
/// login whose own default path is `public, pg_catalog` would then resolve
/// the backend check's `::text` to the enum `public.text` that fill, or the
/// externals file, created: the path is pinned again for the session, and
/// the plan resolves either way.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_keeps_the_empty_path_after_the_outside_write_check() {
    let enum_type = "CREATE TYPE public.text AS ENUM ('a')";
    let a = "SELECT 'a'::public.text AS x";
    let inputs = Inputs::from_pair((
        declared(&[("a", a)], &[]),
        declared(&[("a", a), ("b", a)], &[]),
    ));
    let key = ProjectKey::new(true);
    for (externals, server) in
        [None, Some(format!("{enum_type};"))]
            .into_iter()
            .flat_map(|externals| {
                SERVERS
                    .iter()
                    .map(move |&server| (externals.clone(), server))
            })
    {
        let setup = vec![
            format!("CREATE SCHEMA {MANAGED}"),
            enum_type.to_owned(),
            format!("CREATE VIEW {MANAGED}.a AS {a}"),
        ];
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &setup).await;
        let (login, scratch_db) = fixture.confined().await;
        let mut cluster = Conn::connect(Driver::Postgres, &fixture.server)
            .await
            .unwrap();
        cluster
            .execute(&format!(
                "ALTER ROLE {} SET search_path = public, pg_catalog",
                login.0
            ))
            .await
            .unwrap();
        let result = produce_filled(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            externals.as_deref(),
        )
        .await;
        fixture.drop().await;
        let context = if externals.is_some() {
            "the file"
        } else {
            "fill"
        };
        let plan = result.unwrap_or_else(|error| panic!("{server}, {context}: {error}"));
        plan.evidence.validate(&plan.changes).unwrap();
    }
}

/// On the target `ext.d(NULL)` casts to the domain `ext.d`, which names
/// `m`'s row type; scratch compiles the views before fill creates the
/// domain, and they bind the externals file's `ext.d(text)` instead
/// (measured on 16 and 18). That binding is unresolved, never sealed.
/// Negative: without the domain the function is the binding on both.
async fn shadowed_by_a_chained_shape(domain: bool) -> Result<ResolvedPlan, ProduceError> {
    let function =
        format!("CREATE FUNCTION {EXTERNAL}.d(text) RETURNS text LANGUAGE sql RETURN $1");
    let mut setup = generating_target();
    setup.push(function.clone());
    if domain {
        setup.push(format!("CREATE DOMAIN {EXTERNAL}.d AS {MANAGED}.m"));
    }
    let view = format!("SELECT {EXTERNAL}.d(NULL) AS x");
    setup.push(format!("CREATE VIEW {MANAGED}.a AS {view}"));
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &generating_inputs(&view),
        &key,
        &format!("CREATE SCHEMA {EXTERNAL};\n{function};"),
    )
    .await;
    target.drop().await;
    result
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_leaves_unresolved_a_binding_a_chained_shape_would_take() {
    let plan = shadowed_by_a_chained_shape(false)
        .await
        .unwrap_or_else(|error| panic!("without the domain: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
    let refused = match shadowed_by_a_chained_shape(true).await {
        Ok(_) => panic!("a binding the domain would take was sealed"),
        Err(error) => error.to_string(),
    };
    assert!(
        refused.contains("the declaration was compiled before an object sharing a name it bound"),
        "{refused}"
    );
}

/// Views that call the function compile after it, and after the table, so
/// fill's chained shape is in place for them. Negative: views that do not
/// call it compile first, and the run refuses, naming what waits for what.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_lifts_a_chain_through_a_table_that_compiles_after_its_function() {
    assert_staged(
        after_its_function(true).await,
        "e",
        "a table after its function",
    );
    let refused = externals_refusal(after_its_function(false).await);
    assert!(
        refused.contains(&format!(
            "fill creates table {EXTERNAL}.e only once scratch has created {MANAGED}.m, and a \
             declaration scratch compiles before that cannot read it"
        )),
        "{refused}"
    );
}

/// An event trigger the externals file installed fires on fill's statements
/// too. What it writes outside the scratch database is rolled back and
/// refused, as the file's own writes are (#1783).
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_outside_write_fill_sets_off_and_leaves_none() {
    let role = "pbps_fill_escape_1778";
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let externals = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE FUNCTION {EXTERNAL}.escape() RETURNS event_trigger LANGUAGE plpgsql \
         AS $$BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = '{role}') \
         THEN EXECUTE 'CREATE ROLE {role}'; END IF; END$$;\n\
         CREATE EVENT TRIGGER pbps_fill_escape ON ddl_command_end \
         EXECUTE FUNCTION {EXTERNAL}.escape();"
    );
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await,
    );
    let mut cluster = Conn::connect(Driver::Postgres, &scratch.server)
        .await
        .unwrap();
    let left = cluster
        .query(&format!(
            "SELECT count(*)::text AS n FROM pg_catalog.pg_roles WHERE rolname = '{role}'"
        ))
        .await
        .unwrap()[0]
        .try_get::<&str>("n")
        .unwrap()
        .map(str::to_owned);
    let _ = cluster
        .execute(&format!("DROP ROLE IF EXISTS {role}"))
        .await;
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "filling what the externals file lacks changed role {role}"
        )),
        "{refused}"
    );
    assert_eq!(left.as_deref(), Some("0"), "the role was rolled back");
}

/// The negative: the externals file holds `ext.e` itself, so it is the
/// file's, and the file runs before any managed object exists.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_chain_through_an_object_the_externals_file_holds_naming_it() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &chain_target()).await;
    let inputs = staging_inputs(&reading("id", "e"), &reading("m", "e"), &["m"]);
    let key = ProjectKey::new(true);
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &format!(
                "CREATE SCHEMA {EXTERNAL};\n\
                 CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m);"
            ),
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!("type \"{MANAGED}.m\" does not exist"))
            && refused.contains("fill stages it once the managed tables exist"),
        "{refused}"
    );
}

/// With no externals file at all, fill stages the external table the
/// managed views read.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_fills_the_external_objects_no_externals_file_stages() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let result = produce_filled(&scratch.server, &target.on(&target_db), &inputs, &key, None).await;
    target.drop().await;
    assert_staged(result, "t", "no externals file");
}

/// Fill writes no routine source: a managed view calling an external
/// function no externals file stages refuses, naming the function and the
/// remedy. Negative: the file holding the function answers.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_routine_fill_cannot_stage_naming_it() {
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE FUNCTION {EXTERNAL}.f(x integer) RETURNS integer LANGUAGE sql \
             IMMUTABLE AS 'SELECT x'"
        ),
        format!("CREATE VIEW {MANAGED}.a AS SELECT {EXTERNAL}.f(1) AS id"),
    ];
    let calling = format!("SELECT {EXTERNAL}.f(1) AS id");
    let inputs = staging_inputs(&calling, &calling, &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let refused = externals_refusal(
        produce_filled(&scratch.server, &target.on(&target_db), &inputs, &key, None).await,
    );
    let answered = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &format!(
            "CREATE SCHEMA {EXTERNAL};\n\
             CREATE FUNCTION {EXTERNAL}.f(x integer) RETURNS integer LANGUAGE sql \
             IMMUTABLE AS 'SELECT x';"
        ),
    )
    .await;
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "function {EXTERNAL}.f(integer) cannot be filled: pbps sends no routine source to \
             scratch; write it in the externals file"
        )),
        "{refused}"
    );
    if let Err(error) = answered {
        panic!("the externals file's function answers: {error}");
    }
}

/// The external view over its table, and a managed view reading it.
fn view_target() -> Vec<String> {
    let mut setup = guarded_target();
    setup.push(format!(
        "CREATE VIEW {EXTERNAL}.v AS SELECT id, name FROM {EXTERNAL}.t WHERE n > 0"
    ));
    setup.push(format!(
        "CREATE VIEW {MANAGED}.c AS SELECT id FROM {EXTERNAL}.v"
    ));
    setup
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compiles_a_managed_view_over_an_external_view_staged_as_a_shape_view() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &view_target()).await;
    let inputs = staging_inputs(&reading("id", "v"), &reading("name", "v"), &[]);
    let key = ProjectKey::new(true);
    // Only the view, as its output columns over typed NULLs.
    let externals = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE VIEW {EXTERNAL}.v AS SELECT NULL::integer AS id, NULL::text AS name WHERE false;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    assert_staged(result, "v", "a shape view");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_external_view_staged_as_a_table() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &view_target()).await;
    let inputs = staging_inputs(&reading("id", "v"), &reading("name", "v"), &[]);
    let key = ProjectKey::new(true);
    let externals =
        format!("CREATE SCHEMA {EXTERNAL};\nCREATE TABLE {EXTERNAL}.v (id integer, name text);");
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "the externals file's table {EXTERNAL}.v differs from the target's in"
        )) && refused.contains("relkind"),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_reports_a_write_through_a_shape_view_with_the_real_definition_remedy() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &view_target()).await;
    // A managed routine that writes through the external view.
    let mut base = declared(&[("a", &reading("id", "v"))], &[]);
    let mut desired = base.clone();
    desired.modules.insert(
        format!("{MANAGED}.w()").parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: format!(
                "() RETURNS void LANGUAGE sql BEGIN ATOMIC \
                 INSERT INTO {EXTERNAL}.v (id, name) VALUES (1, 'x'); END"
            ),
            options: Default::default(),
        },
    );
    base.modules
        .remove(&format!("{MANAGED}.w()").parse().unwrap());
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let externals = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE VIEW {EXTERNAL}.v AS SELECT NULL::integer AS id, NULL::text AS name WHERE false;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let reason = match result {
        Err(ProduceError::Run(Error::Creation(failure))) if !failure.externals => {
            failure.to_string()
        }
        Err(ProduceError::Run(Error::Binding(reason))) => reason,
        other => panic!(
            "not a binding refusal: {:?}",
            other.err().map(|e| e.to_string())
        ),
    };
    assert!(
        reason.contains(&format!(
            "the externals file stages {EXTERNAL}.v as a view no statement can write through: stage \
             its real definition"
        )),
        "{reason}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_question_that_needs_routine_source_the_externals_file_lacks() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE FUNCTION {EXTERNAL}.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1"
        ),
        format!(
            "CREATE FUNCTION {EXTERNAL}.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1"
        ),
        format!("CREATE VIEW {MANAGED}.a AS SELECT {EXTERNAL}.f(1) AS r"),
    ];
    let target_db = staging_target(&mut target, &setup).await;
    let call = format!("SELECT {EXTERNAL}.f(1) AS r");
    let inputs = staging_inputs(&call, &call, &[]);
    let key = ProjectKey::new(true);
    // The integer overload the target's call binds is not staged.
    let externals = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE FUNCTION {EXTERNAL}.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let reason = match result {
        Err(ProduceError::Run(Error::Binding(reason))) => reason,
        other => panic!(
            "not a binding refusal: {:?}",
            other.err().map(|e| e.to_string())
        ),
    };
    assert!(
        reason.contains(&format!(
            "routine {EXTERNAL}.f(pg_catalog.int4) on the target is not in the resolver's \
             externals file, and \
             relation {MANAGED}.a could bind it; add it to the externals file"
        )) && reason.contains("select no resolver"),
        "{reason}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_what_the_externals_file_adds_to_an_extension_it_did_not_create() {
    // `plpgsql` is there before the externals file runs, so a routine the
    // externals file adds to it is compared on its own, not skipped as covered by
    // an extension version nobody compared.
    let external = [
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE FUNCTION {EXTERNAL}.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1"
        ),
        format!("ALTER EXTENSION plpgsql ADD FUNCTION {EXTERNAL}.f(integer)"),
    ];
    let call = format!("SELECT {EXTERNAL}.f(1) AS r");
    let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
    setup.extend(external.iter().cloned());
    setup.push(format!("CREATE VIEW {MANAGED}.a AS {call}"));
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&call, &call, &[]);
    let key = ProjectKey::new(true);
    let externals = external
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let plan = match result {
        Ok(plan) => plan,
        Err(error) => panic!("a routine added to plpgsql: {error}"),
    };
    plan.evidence.validate(&plan.changes).unwrap();
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_stages_a_routine_in_a_system_schema_by_its_header() {
    // A superuser may create a routine in `information_schema` (measured on
    // 16 and 18). Staged there, it is compared by its header like any staged
    // routine: the externals file need not repeat the target's body (SPEC §9.3.2).
    let routine = "information_schema.pbps_staged_1754";
    let call = format!("SELECT {routine}(1) AS r");
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!(
            "CREATE FUNCTION {routine}(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 + 1"
        ),
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
    ];
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&call, &call, &[]);
    let key = ProjectKey::new(true);
    let externals = format!(
        "CREATE FUNCTION {routine}(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 0;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let plan = match result {
        Ok(plan) => plan,
        Err(error) => panic!("a staged system-schema routine: {error}"),
    };
    plan.evidence.validate(&plan.changes).unwrap();
    let created =
        plan.changes.changes.iter().any(
            |step| matches!(&step.change, Change::CreateModule { id, .. } if id.name() == "b"),
        );
    assert!(created, "the plan creates b");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_stages_a_cast_and_an_operator_over_external_types_in_the_externals_file() {
    let external = [
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE TYPE {EXTERNAL}.pair AS (a integer, b integer)"),
        format!(
            "CREATE FUNCTION {EXTERNAL}.total({EXTERNAL}.pair) RETURNS integer LANGUAGE sql \
             IMMUTABLE RETURN ($1).a + ($1).b"
        ),
        format!(
            "CREATE CAST ({EXTERNAL}.pair AS integer) WITH FUNCTION {EXTERNAL}.total({EXTERNAL}.pair)"
        ),
        format!(
            "CREATE FUNCTION {EXTERNAL}.join_pairs({EXTERNAL}.pair, {EXTERNAL}.pair) \
             RETURNS integer LANGUAGE sql IMMUTABLE RETURN ($1).a + ($2).b"
        ),
        format!(
            "CREATE OPERATOR {EXTERNAL}.<%> (LEFTARG = {EXTERNAL}.pair, RIGHTARG = {EXTERNAL}.pair, \
             FUNCTION = {EXTERNAL}.join_pairs)"
        ),
    ];
    let a = format!(
        "SELECT ROW(1, 2)::{EXTERNAL}.pair OPERATOR({EXTERNAL}.<%>) ROW(3, 4)::{EXTERNAL}.pair AS s"
    );
    let b = format!("SELECT (ROW(1, 2)::{EXTERNAL}.pair)::integer AS i");
    let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
    setup.extend(external.iter().cloned());
    setup.push(format!("CREATE VIEW {MANAGED}.a AS {a}"));
    for (server, scratch_server) in [("PBPS_TEST_PG_DB", SCRATCH_SERVER)] {
        let mut target = Fixture::new(server);
        let scratch = Fixture::new(scratch_server);
        let target_db = staging_target(&mut target, &setup).await;
        let inputs = staging_inputs(&a, &b, &[]);
        let key = ProjectKey::new(true);
        let externals = external
            .iter()
            .map(|s| format!("{s};\n"))
            .collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        assert_staged(result, "pair", server);
    }
}

/// Fill installs the extension the target's current view needs, from the
/// target's dependencies, whether or not the desired view still does. The
/// target's member was changed in place (made `VOLATILE`, or renamed),
/// which a new extension cannot reproduce, so the comparison refuses,
/// naming fill and the remedy. With the remedy written in the externals file
/// (the extension and the change), the file holds it, fill leaves it, and
/// the plan resolves.
#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_names_fills_altered_extension_member_and_its_remedy() {
    let done = "SELECT 'done'::text AS r";
    let cases = [
        (
            "VOLATILE",
            "soundex",
            format!(
                "fill created function {EXTERNAL}.soundex(text), which differs from the target's \
                 in provolatile; the target's was changed after it was created"
            ),
        ),
        (
            "RENAME TO soundex_renamed",
            "soundex_renamed",
            format!(
                "fill created function {EXTERNAL}.soundex(text), which the target does not have: \
                 the target's was changed after it was created"
            ),
        ),
    ];
    for (change, called, named) in cases {
        let call = format!("SELECT {EXTERNAL}.{called}('pbps') AS r");
        let altered = format!("ALTER FUNCTION {EXTERNAL}.soundex(text) {change}");
        let setup = vec![
            format!("CREATE SCHEMA {MANAGED}"),
            format!("CREATE SCHEMA {EXTERNAL}"),
            format!("CREATE EXTENSION fuzzystrmatch SCHEMA {EXTERNAL}"),
            altered.clone(),
            format!("CREATE VIEW {MANAGED}.a AS {call}"),
        ];
        let inputs = Inputs::from_pair((
            declared(&[("a", &call)], &[]),
            declared(&[("a", done)], &[]),
        ));
        let key = ProjectKey::new(true);
        let remedy = format!(
            "CREATE SCHEMA {EXTERNAL};\nCREATE EXTENSION fuzzystrmatch SCHEMA {EXTERNAL};\n\
             {altered};"
        );
        for externals in [None, Some(remedy.as_str())] {
            let mut target = Fixture::new("PBPS_TEST_PG_DB");
            let scratch = Fixture::new(SCRATCH_SERVER);
            let target_db = staging_target(&mut target, &setup).await;
            let result = produce_filled(
                &scratch.server,
                &target.on(&target_db),
                &inputs,
                &key,
                externals,
            )
            .await;
            target.drop().await;
            match externals {
                None => {
                    let refused = externals_refusal(result);
                    assert!(
                        refused.contains(&named)
                            && refused.contains("in the externals file as the target has it"),
                        "{change}: {refused}"
                    );
                }
                Some(_) => {
                    let plan =
                        result.unwrap_or_else(|error| panic!("{change}, with the remedy: {error}"));
                    plan.evidence.validate(&plan.changes).unwrap();
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_each_extension_member_with_the_target() {
    // The extension's version matches; one member the target altered in
    // place does not, and is named (#1766, #1783).
    let call = format!("SELECT {EXTERNAL}.soundex('pbps') AS r");
    let externals =
        format!("CREATE SCHEMA {EXTERNAL};\nCREATE EXTENSION fuzzystrmatch SCHEMA {EXTERNAL};");
    for altered in [true, false] {
        let mut setup = vec![
            format!("CREATE SCHEMA {MANAGED}"),
            format!("CREATE SCHEMA {EXTERNAL}"),
            format!("CREATE EXTENSION fuzzystrmatch SCHEMA {EXTERNAL}"),
        ];
        if altered {
            setup.push(format!("ALTER FUNCTION {EXTERNAL}.soundex(text) VOLATILE"));
        }
        setup.push(format!("CREATE VIEW {MANAGED}.a AS {call}"));
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let inputs = staging_inputs(&call, &call, &[]);
        let key = ProjectKey::new(true);
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        if altered {
            let refused = externals_refusal(result);
            assert!(
                refused.contains(&format!(
                    "{EXTERNAL}.soundex(text) differs from the target's in provolatile"
                )),
                "{refused}"
            );
        } else {
            // Negative: the same extension, unaltered, is staged.
            let plan = result.unwrap_or_else(|error| panic!("an unaltered extension: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_extension_with_a_member_no_capture_identifies() {
    // `postgres_fdw` brings a foreign-data wrapper, a class the capture has
    // no identity for. Sealing a plan that reads the extension would need
    // it, so the comparison names it first.
    let call = format!("SELECT {EXTERNAL}.postgres_fdw_disconnect_all() AS r");
    let externals =
        format!("CREATE SCHEMA {EXTERNAL};\nCREATE EXTENSION postgres_fdw SCHEMA {EXTERNAL};");
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE EXTENSION postgres_fdw SCHEMA {EXTERNAL}"),
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
    ];
    let inputs = staging_inputs(&call, &call, &[]);
    let key = ProjectKey::new(true);
    // Written in the externals file, and filled from the target's
    // dependencies when there is none: each refusal names who created it.
    for (externals, who) in [
        (Some(externals.as_str()), "the externals file"),
        (None, "fill"),
    ] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let result = produce_filled(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            externals,
        )
        .await;
        target.drop().await;
        let refused = externals_refusal(result);
        assert!(
            refused.contains(&format!("{who} created foreign-data wrapper postgres_fdw"))
                && refused.contains("no plan can seal its extension"),
            "{who}: {refused}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_stages_an_extension_that_brings_text_search_objects() {
    // `unaccent` brings a text search dictionary and template, which the
    // capture identifies and the comparison reads like any member (#1786).
    let call = format!("SELECT {EXTERNAL}.unaccent('pbps') AS r");
    let externals =
        format!("CREATE SCHEMA {EXTERNAL};\nCREATE EXTENSION unaccent SCHEMA {EXTERNAL};");
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE EXTENSION unaccent SCHEMA {EXTERNAL}"),
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
    ];
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&call, &call, &[]);
    let key = ProjectKey::new(true);
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let plan = result.unwrap_or_else(|error| panic!("unaccent: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_a_text_search_dictionary_with_the_target() {
    // A dictionary's options are its shape; one that differs refuses,
    // naming it, and one that matches stages (#1786).
    // With a configuration mapped to it, whose mapping rows are its parts.
    let dictionary = |options: &str| {
        format!(
            "CREATE TEXT SEARCH DICTIONARY {EXTERNAL}.words \
             (TEMPLATE = pg_catalog.simple{options}); \
             CREATE TEXT SEARCH CONFIGURATION {EXTERNAL}.cfg (PARSER = pg_catalog.\"default\"); \
             ALTER TEXT SEARCH CONFIGURATION {EXTERNAL}.cfg \
             ADD MAPPING FOR asciiword WITH {EXTERNAL}.words"
        )
    };
    let call = format!("SELECT {EXTERNAL}.t.id FROM {EXTERNAL}.t");
    for staged_options in [", STOPWORDS = english", ""] {
        let mut setup = guarded_target();
        setup.push(dictionary(", STOPWORDS = english"));
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let inputs = staging_inputs(&reading("id", "t"), &call, &[]);
        let key = ProjectKey::new(true);
        let externals = format!(
            "{}{};\n",
            plain_externals("integer"),
            dictionary(staged_options)
        );
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        if staged_options.is_empty() {
            let refused = externals_refusal(result);
            assert!(
                refused.contains(&format!("text search dictionary {EXTERNAL}.words"))
                    && refused.contains("dictinitoption"),
                "{refused}"
            );
        } else {
            // Negative: the same dictionary stages.
            assert_staged(result, "t", "an identical dictionary");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_an_operator_familys_members_with_the_target() {
    // A row comparison binds by the strategy its operator serves in a
    // btree family; the same family header with another strategy is a
    // different family to it (#1768, #1783).
    let family = |strategy: u8| {
        vec![
            format!("CREATE SCHEMA {EXTERNAL}"),
            format!(
                "CREATE FUNCTION {EXTERNAL}.lt(integer, integer) RETURNS boolean \
                 LANGUAGE sql IMMUTABLE RETURN $1 < $2"
            ),
            format!(
                "CREATE OPERATOR {EXTERNAL}.<% (LEFTARG = integer, RIGHTARG = integer, \
                 FUNCTION = {EXTERNAL}.lt)"
            ),
            format!("CREATE OPERATOR FAMILY {EXTERNAL}.fam USING btree"),
            format!(
                "ALTER OPERATOR FAMILY {EXTERNAL}.fam USING btree \
                 ADD OPERATOR {strategy} {EXTERNAL}.<% (integer, integer)"
            ),
        ]
    };
    let call = format!("SELECT ROW(1, 2) OPERATOR({EXTERNAL}.<%) ROW(3, 4) AS r");
    for staged_strategy in [2, 1] {
        let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
        setup.extend(family(1));
        setup.push(format!("CREATE VIEW {MANAGED}.a AS {call}"));
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let inputs = staging_inputs(&call, &call, &[]);
        let key = ProjectKey::new(true);
        let externals = family(staged_strategy)
            .iter()
            .map(|s| format!("{s};\n"))
            .collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        if staged_strategy == 1 {
            // Negative: the same members are staged.
            let plan = result.unwrap_or_else(|error| panic!("an identical family: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        } else {
            let refused = externals_refusal(result);
            assert!(
                refused.contains("the target has")
                    && refused.contains("the externals file does not"),
                "{refused}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_takes_a_routine_over_a_staged_type_as_no_part_of_it() {
    // A routine's signature names its argument types; that does not make it
    // a part of them. The target's view calls one the shape view need not
    // stage.
    let external = [
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE TYPE {EXTERNAL}.pair AS (a integer, b integer)"),
    ];
    let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
    setup.extend(external.iter().cloned());
    setup.extend([
        format!(
            "CREATE FUNCTION {EXTERNAL}.total({EXTERNAL}.pair) RETURNS integer \
             LANGUAGE sql IMMUTABLE RETURN ($1).a + ($1).b"
        ),
        format!(
            "CREATE VIEW {EXTERNAL}.v AS SELECT {EXTERNAL}.total(ROW(1, 2)::{EXTERNAL}.pair) AS s"
        ),
    ]);
    let call = format!("SELECT s FROM {EXTERNAL}.v");
    setup.push(format!("CREATE VIEW {MANAGED}.a AS {call}"));
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&call, &call, &[]);
    let key = ProjectKey::new(true);
    let mut externals = external
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    externals.push_str(&format!(
        "CREATE VIEW {EXTERNAL}.v AS SELECT NULL::integer AS s WHERE false;\n"
    ));
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let plan = result.unwrap_or_else(|error| panic!("a routine over a staged type: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_staged_shape_that_changes_before_the_opening_capture() {
    // A writer between the externals file's comparison and the capture the plan
    // seals: scratch compiled `SELECT *` against the compared table, and
    // the evidence would seal the table with another column (#1755).
    let inputs = staging_inputs(&reading("id", "t"), &reading("*", "t"), &[]);
    let key = ProjectKey::new(true);
    for (sql, changes) in [
        (
            format!("ALTER TABLE {EXTERNAL}.t ADD COLUMN extra integer"),
            true,
        ),
        // Negative: a write to nothing the externals file staged.
        (
            format!("CREATE TABLE {EXTERNAL}.unrelated (x integer)"),
            false,
        ),
    ] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &guarded_target()).await;
        let result = produce_staged_then(
            &sql,
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &plain_externals("integer"),
        )
        .await;
        target.drop().await;
        if changes {
            match result {
                Err(ProduceError::Run(Error::Read(reason))) => {
                    assert!(reason.contains("the target changed"), "{reason}");
                    assert!(reason.contains("extra"), "{reason}");
                }
                Err(other) => panic!("not a changed-target refusal: {other}"),
                Ok(_) => {
                    panic!("a plan sealed over a shape the externals file was not compared with")
                }
            }
        } else {
            assert_staged(result, "t", "an unrelated write");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_a_sequence_its_table_owns() {
    // An identity column's sequence is a part of its table by a dependency
    // that no list names; it is compared all the same (#1783).
    let table = [
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE TABLE {EXTERNAL}.t (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
             name text UNIQUE)"
        ),
    ];
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    for altered in [true, false] {
        let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
        setup.extend(table.iter().cloned());
        if altered {
            setup.push(format!(
                "ALTER TABLE {EXTERNAL}.t ALTER COLUMN id SET INCREMENT BY 5"
            ));
        }
        setup.push(format!("CREATE VIEW {MANAGED}.a AS {}", reading("id", "t")));
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let key = ProjectKey::new(true);
        let externals = table.iter().map(|s| format!("{s};\n")).collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        if altered {
            let refused = externals_refusal(result);
            assert!(refused.contains("seqincrement"), "{refused}");
        } else {
            // Negative: the same sequence is staged.
            assert_staged(result, "t", "an unaltered sequence");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_an_operator_class_with_its_family() {
    // The class lives on its family automatically; whether it is its type's
    // default is in its own header, compared with the family (#1783).
    let class = |default: bool| {
        let default = if default { "DEFAULT " } else { "" };
        vec![
            format!("CREATE SCHEMA {EXTERNAL}"),
            format!("CREATE TYPE {EXTERNAL}.e AS ENUM ('a', 'b')"),
            format!(
                "CREATE OPERATOR CLASS {EXTERNAL}.c {default}FOR TYPE {EXTERNAL}.e USING hash \
                 AS OPERATOR 1 = (anyenum, anyenum), FUNCTION 1 hashenum(anyenum)"
            ),
        ]
    };
    let call = format!("SELECT 'a'::{EXTERNAL}.e AS v");
    for staged_default in [false, true] {
        let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
        setup.extend(class(true));
        setup.push(format!("CREATE VIEW {MANAGED}.a AS {call}"));
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let inputs = staging_inputs(&call, &call, &[]);
        let key = ProjectKey::new(true);
        let externals = class(staged_default)
            .iter()
            .map(|s| format!("{s};\n"))
            .collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        if staged_default {
            // Negative: the same class is staged.
            let plan = result.unwrap_or_else(|error| panic!("an identical class: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        } else {
            let refused = externals_refusal(result);
            assert!(refused.contains("opcdefault"), "{refused}");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_an_externals_file_that_writes_outside_the_scratch_database() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    // Run-owned: the externals file runs as the scratch superuser.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let role = format!("pbps_v1783_r_{}", scratch.token);
    let cases = [
        (
            format!("CREATE ROLE {role};\nGRANT pg_monitor TO {role};"),
            vec![
                format!("it changed role {role}, outside the scratch database"),
                format!("the membership of role {role} in role pg_monitor"),
            ],
        ),
        (
            "ALTER ROLE ALL SET work_mem = '5MB';".to_owned(),
            vec!["the settings of every role in every database".to_owned()],
        ),
        (
            "DO $$BEGIN EXECUTE format('COMMENT ON ROLE %I IS ''pbps''', current_user); END$$;"
                .to_owned(),
            vec!["the comment on role".to_owned()],
        ),
        (
            "GRANT SET ON PARAMETER log_statement TO PUBLIC;".to_owned(),
            vec!["the privileges on parameter log_statement".to_owned()],
        ),
        // Transaction control would commit the role before any check:
        // refused before it runs, and the role is never created.
        (
            format!("CREATE ROLE {role};\nCOMMIT;"),
            vec!["leave out BEGIN, COMMIT and ROLLBACK".to_owned()],
        ),
    ];
    for (statements, named) in &cases {
        let externals = format!("{}{statements}\n", plain_externals("integer"));
        let refused = externals_refusal(
            produce_staged(
                &scratch.server,
                &target.on(&target_db),
                &inputs,
                &key,
                &externals,
            )
            .await,
        );
        for name in named {
            assert!(refused.contains(name.as_str()), "{statements}: {refused}");
        }
    }
    target.drop().await;
    // Refused by rolling back: the role is not left on the scratch cluster.
    let (_, roles) = scratch.inventory().await;
    assert!(!roles.contains(&role), "{role} was left behind");
    // Supplied: the confined account changes its own role's settings.
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &guarded_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let externals = format!(
            "{}ALTER ROLE CURRENT_USER SET work_mem = '5MB';\n",
            plain_externals("integer")
        );
        let refused = externals_refusal(
            produce_staged(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                &externals,
            )
            .await,
        );
        let left = fixture
            .admin()
            .await
            .query(&format!(
                "SELECT 1 FROM pg_catalog.pg_db_role_setting s \
                   JOIN pg_catalog.pg_roles r ON r.oid = s.setrole WHERE r.rolname = '{}'",
                login.0
            ))
            .await
            .unwrap()
            .len();
        fixture.drop().await;
        assert!(
            refused.contains(&format!(
                "the settings of role {} in every database",
                login.0
            )),
            "{server}: {refused}"
        );
        assert_eq!(left, 0, "{server}: the setting is rolled back");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_runs_the_externals_file_on_the_empty_path_every_session_pins() {
    // Every pbps session pins an empty path (DEC-1564.1), the externals file's
    // too: a file written for `psql` names its schemas or sets the path, as
    // `pg_dump` output does, and one that does neither is told so.
    let call = "SELECT public.soundex('pbps') AS r";
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        "CREATE EXTENSION fuzzystrmatch SCHEMA public".to_owned(),
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
    ];
    for (externals, staged) in [
        (
            "SET search_path = public;\nCREATE EXTENSION fuzzystrmatch;",
            true,
        ),
        ("CREATE EXTENSION fuzzystrmatch;", false),
    ] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &setup).await;
        let inputs = staging_inputs(call, call, &[]);
        let key = ProjectKey::new(true);
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            externals,
        )
        .await;
        target.drop().await;
        if staged {
            let plan = result.unwrap_or_else(|error| panic!("{externals}: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        } else {
            // Negative: no schema on the path, and the refusal says why.
            let refused = externals_refusal(result);
            assert!(
                refused.contains("it runs on an empty search_path"),
                "{refused}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_checks_an_externals_file_whose_path_shadows_a_built_in_type() {
    // The externals file's `SET`s last on its session until `DISCARD ALL`, and the
    // checks after it run there: a type it names `text`, first on its path,
    // must not change what they read.
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let mut setup = guarded_target();
    setup.push(format!("CREATE TYPE {EXTERNAL}.text AS ENUM ('a')"));
    let target_db = staging_target(&mut target, &setup).await;
    let externals = format!(
        "{}CREATE TYPE {EXTERNAL}.text AS ENUM ('a');\nSET search_path = {EXTERNAL}, pg_catalog;\n",
        plain_externals("integer")
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    assert_staged(result, "t", "a path that shadows text");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_every_attribute_of_another_database() {
    // A collation version no list of columns named: refreshing another
    // database's is a write outside the scratch database like any other.
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    for stale in [true, false] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let mut scratch = Fixture::new(SCRATCH_SERVER);
        let target_db = staging_target(&mut target, &guarded_target()).await;
        let other = format!("pbps_v1783_other_{}", scratch.token);
        let mut admin = scratch.admin().await;
        admin
            .execute(&format!("CREATE DATABASE {other}"))
            .await
            .unwrap();
        scratch.databases.push(other.clone());
        if stale {
            admin
                .execute(&format!(
                    "UPDATE pg_catalog.pg_database SET datcollversion = '0' \
                      WHERE datname = '{other}'"
                ))
                .await
                .unwrap();
        }
        let externals = format!(
            "{}ALTER DATABASE {other} REFRESH COLLATION VERSION;\n",
            plain_externals("integer")
        );
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &externals,
        )
        .await;
        target.drop().await;
        scratch.drop().await;
        if stale {
            let refused = externals_refusal(result);
            assert!(refused.contains(&format!("database {other}")), "{refused}");
        } else {
            // Negative: a refresh that changes nothing is no change.
            assert_staged(result, "t", "a refresh that changes nothing");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_reads_the_shared_state_under_its_own_settings_not_the_externals_files() {
    // An externals file's `SET TimeZone` renders a role's expiry with another
    // offset; read under it, the second read of the shared state would
    // differ from the first and refuse an externals file that changed nothing.
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let expiring = format!("pbps_v1783_expiring_{}", scratch.token);
    scratch
        .admin()
        .await
        .execute(&format!(
            "CREATE ROLE {expiring} NOLOGIN VALID UNTIL '2100-01-01 00:00:00+00'"
        ))
        .await
        .unwrap();
    scratch.roles.push(expiring);
    let externals = format!(
        "{}SET TimeZone = 'Pacific/Kiritimati';\nSET statement_timeout = '1ms';\n",
        plain_externals("integer")
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    scratch.drop().await;
    assert_staged(result, "t", "an externals file that sets its own time zone");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_ignores_another_sessions_cluster_writes_during_the_externals_file() {
    // Other runs share the scratch cluster: a role one creates while this
    // externals file runs is in neither of its reads, so nothing is refused.
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let marker = format!("pbps_v1783_marker_{}", scratch.token);
    let externals = format!(
        "{}SELECT pg_catalog.pg_sleep(3) AS {marker};\n",
        plain_externals("integer")
    );
    let other = format!("pbps_v1783_other_{}", scratch.token);
    let mut admin = scratch.admin().await;
    let concurrent = async {
        // Once the externals file is running, another session creates a role.
        loop {
            let running = admin
                .query(&format!(
                    "SELECT 1 FROM pg_catalog.pg_stat_activity \
                      WHERE query LIKE '%{marker}%' AND pid <> pg_catalog.pg_backend_pid()"
                ))
                .await
                .unwrap();
            if !running.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        admin
            .execute(&format!("CREATE ROLE {other} NOLOGIN"))
            .await
            .unwrap();
    };
    let target_url = target.on(&target_db);
    let (result, ()) = tokio::join!(
        produce_staged(&scratch.server, &target_url, &inputs, &key, &externals),
        concurrent
    );
    scratch.roles.push(other);
    target.drop().await;
    scratch.drop().await;
    assert_staged(result, "t", "a concurrent role");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_an_extension_member_in_a_system_schema_in_full() {
    // An extension's member is compared on its own, here in a system
    // schema: one the target altered in place is refused by name, not
    // accepted for its extension's version (#1783).
    let call = "SELECT pg_catalog.soundex('pbps') AS r";
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        "CREATE EXTENSION fuzzystrmatch SCHEMA pg_catalog".to_owned(),
        "ALTER FUNCTION pg_catalog.soundex(text) VOLATILE".to_owned(),
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
    ];
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(call, call, &[]);
    let key = ProjectKey::new(true);
    let refused = externals_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            "CREATE EXTENSION fuzzystrmatch SCHEMA pg_catalog;",
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains("soundex(text) differs from the target's in provolatile"),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_stages_an_operator_over_built_in_types_in_the_externals_file() {
    // An operator's identity hangs off its operand types; over built-in
    // types those are not staged, so the operator itself is.
    let external = [
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE FUNCTION {EXTERNAL}.join_ints(integer, integer) RETURNS integer \
             LANGUAGE sql IMMUTABLE RETURN $1 + $2"
        ),
        format!(
            "CREATE OPERATOR {EXTERNAL}.<%> (LEFTARG = integer, RIGHTARG = integer, \
             FUNCTION = {EXTERNAL}.join_ints)"
        ),
    ];
    let a = format!("SELECT 1 OPERATOR({EXTERNAL}.<%>) 2 AS s");
    let mut setup = vec![format!("CREATE SCHEMA {MANAGED}")];
    setup.extend(external.iter().cloned());
    setup.push(format!("CREATE VIEW {MANAGED}.a AS {a}"));
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&a, &a, &[]);
    let key = ProjectKey::new(true);
    let externals = external
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    let plan = match result {
        Ok(plan) => plan,
        Err(error) => panic!("an operator over built-in types: {error}"),
    };
    plan.evidence.validate(&plan.changes).unwrap();
    let created =
        plan.changes.changes.iter().any(
            |step| matches!(&step.change, Change::CreateModule { id, .. } if id.name() == "b"),
        );
    assert!(created, "the plan creates b");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_reproduces_the_deployers_usage_of_a_schema_the_externals_file_creates() {
    // A deployer that is no superuser: without its USAGE on the external
    // schema reproduced, its views there would not compile on scratch.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let deployer = target.login("d", "NOSUPERUSER").await;
    let mut setup = vec![
        format!("CREATE SCHEMA {MANAGED} AUTHORIZATION {}", deployer.0),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE TABLE {EXTERNAL}.t (id integer PRIMARY KEY, name text UNIQUE, p integer, n integer)"
        ),
        format!("GRANT USAGE ON SCHEMA {EXTERNAL} TO {}", deployer.0),
        format!("GRANT SELECT ON {EXTERNAL}.t TO {}", deployer.0),
        // The compatibility rule compares settings only this role reads.
        format!("GRANT pg_read_all_settings TO {}", deployer.0),
    ];
    let target_db = staging_target(&mut target, &setup).await;
    setup.clear();
    let mut as_deployer = Conn::connect(Driver::Postgres, &target.as_login(&target_db, &deployer))
        .await
        .unwrap();
    as_deployer
        .execute(&format!(
            "CREATE VIEW {MANAGED}.a AS {}",
            reading("id", "t")
        ))
        .await
        .unwrap();
    drop(as_deployer);
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let result = produce_staged(
        &scratch.server,
        &target.as_login(&target_db, &deployer),
        &inputs,
        &key,
        &plain_externals("integer"),
    )
    .await;
    target.drop().await;
    assert_staged(result, "t", "a deployer with USAGE");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_reproduces_the_deployers_usage_of_a_schema_an_extension_carries() {
    // An extension can create its own schema (`pg_cron`'s `cron`); built
    // here with `ALTER EXTENSION ... ADD SCHEMA`. It is the extension's
    // member, not a root, yet the deployer's USAGE on it must be reproduced.
    let carried = [
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!(
            "CREATE TABLE {EXTERNAL}.t (id integer PRIMARY KEY, name text UNIQUE, p integer, n integer)"
        ),
        "CREATE EXTENSION fuzzystrmatch SCHEMA pg_catalog".to_owned(),
        format!("ALTER EXTENSION fuzzystrmatch ADD SCHEMA {EXTERNAL}"),
    ];
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let deployer = target.login("d", "NOSUPERUSER").await;
    let mut setup = vec![format!(
        "CREATE SCHEMA {MANAGED} AUTHORIZATION {}",
        deployer.0
    )];
    setup.extend(carried.iter().cloned());
    setup.extend([
        format!("GRANT USAGE ON SCHEMA {EXTERNAL} TO {}", deployer.0),
        format!("GRANT SELECT ON {EXTERNAL}.t TO {}", deployer.0),
        format!("GRANT pg_read_all_settings TO {}", deployer.0),
    ]);
    let target_db = staging_target(&mut target, &setup).await;
    let mut as_deployer = Conn::connect(Driver::Postgres, &target.as_login(&target_db, &deployer))
        .await
        .unwrap();
    as_deployer
        .execute(&format!(
            "CREATE VIEW {MANAGED}.a AS {}",
            reading("id", "t")
        ))
        .await
        .unwrap();
    drop(as_deployer);
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let externals = carried
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    let result = produce_staged(
        &scratch.server,
        &target.as_login(&target_db, &deployer),
        &inputs,
        &key,
        &externals,
    )
    .await;
    target.drop().await;
    assert_staged(
        result,
        "t",
        "a deployer with USAGE on an extension's schema",
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_takes_an_unmanaged_overload_of_a_managed_routine_for_no_chain() {
    // `f(integer)` is managed; `f(e)` shares its name and is not, and binds
    // an external table whose column is of a managed table's row type.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE TABLE {MANAGED}.m (id integer)"),
        format!("CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m)"),
        format!("CREATE FUNCTION {MANAGED}.f(integer) RETURNS integer LANGUAGE sql RETURN $1"),
        format!(
            "CREATE FUNCTION {MANAGED}.f({EXTERNAL}.e) RETURNS integer LANGUAGE sql RETURN ($1).id"
        ),
        format!("CREATE VIEW {MANAGED}.a AS SELECT 1 AS x"),
    ];
    let target_db = staging_target(&mut target, &setup).await;
    let routine = |mut schema: pbps_model::Schema| {
        schema.modules.insert(
            format!("{MANAGED}.f(integer)").parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: "(integer) RETURNS integer LANGUAGE sql RETURN $1".into(),
                options: Default::default(),
            },
        );
        schema
    };
    let inputs = Inputs::from_pair((
        routine(declared(&[("a", "SELECT 1 AS x")], &["m"])),
        routine(declared(
            &[("a", "SELECT 1 AS x"), ("b", "SELECT 2 AS y")],
            &["m"],
        )),
    ));
    let key = ProjectKey::new(true);
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &format!("CREATE SCHEMA {EXTERNAL};"),
    )
    .await;
    target.drop().await;
    if let Err(error) = result {
        panic!("{error}");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_answers_a_plan_that_takes_a_view_off_a_chain() {
    // The target's `a` reads through a chain; the desired `a` reads the
    // managed table itself, so the compile never needs the external table.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE TABLE {MANAGED}.m (id integer)"),
        format!("CREATE TABLE {EXTERNAL}.e (id integer, m {MANAGED}.m)"),
        format!("CREATE VIEW {MANAGED}.a AS SELECT id FROM {EXTERNAL}.e"),
    ];
    let target_db = staging_target(&mut target, &setup).await;
    let direct = format!("SELECT id FROM {MANAGED}.m");
    let inputs = Inputs::from_pair((
        declared(&[("a", &reading("id", "e"))], &["m"]),
        declared(&[("a", &direct), ("b", &direct)], &["m"]),
    ));
    let key = ProjectKey::new(true);
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &format!("CREATE SCHEMA {EXTERNAL};"),
    )
    .await;
    target.drop().await;
    if let Err(error) = result {
        panic!("{error}");
    }
}

// ---- A Docker scratch (#1674) ----

/// Each target with the image its Docker scratch starts from: the same
/// release, already on the runner, as the default `pull: never` needs. CI
/// names the digests its PostgreSQL services pulled;
/// `scripts/live-tests-pg.sh` the digests it runs.
const DOCKER_PAIRS: &[(&str, &str)] = &[
    ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_SCRATCH_IMAGE"),
    ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"),
];

/// The containers a Docker scratch of this process labels, so a test can
/// tell what a run left behind. Only this process's: another session's
/// resolver run on the same host starts and removes scratch containers under
/// the same scratch label meanwhile (#2095).
fn scratch_containers() -> std::collections::BTreeSet<String> {
    let process = format!("label={}", super::container::process_label());
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            "label=pbps.resolver=scratch",
            "--filter",
            &process,
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("docker runs");
    assert!(output.status.success(), "docker ps: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The volumes of the one scratch container that is not in `before`.
fn new_container_volumes(before: &std::collections::BTreeSet<String>) -> Vec<String> {
    let started: Vec<String> = scratch_containers().difference(before).cloned().collect();
    assert_eq!(started.len(), 1, "one container started: {started:?}");
    let output = std::process::Command::new("docker")
        .args([
            "container",
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Name}} {{end}}",
            &started[0],
        ])
        .output()
        .expect("docker runs");
    assert!(output.status.success(), "docker inspect: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn volume_exists(name: &str) -> bool {
    std::process::Command::new("docker")
        .args(["volume", "inspect", "--format", "{{.Name}}", name])
        .output()
        .expect("docker runs")
        .status
        .success()
}

async fn identity_of(target: &str) -> String {
    let mut planning = Conn::connect(Driver::Postgres, target).await.unwrap();
    pbps_pg::resolver::scratch::cluster_identity(&mut planning)
        .await
        .unwrap()
}

/// A run on a Docker scratch, as `resolution` makes it: the target bound by
/// `planned` (the target's own identity when `None`).
#[allow(clippy::too_many_arguments)]
async fn produce_in_docker(
    image: &str,
    pull: pbps_config::resolver::PullPolicy,
    target: &str,
    planned: Option<&str>,
    inputs: &Inputs,
    key: &ProjectKey,
    externals: Option<&str>,
    outside_writes: pbps_model::resolver::OutsideWrites,
) -> Result<ResolvedPlan, ProduceError> {
    let identity = match planned {
        Some(identity) => identity.to_owned(),
        None => identity_of(target).await,
    };
    let provenance = super::Provenance {
        entry: "scratch".into(),
        scratch: pbps_model::resolver::ScratchSource::Docker {
            image: image.into(),
        },
        outside_writes,
        same_instance: pbps_model::resolver::SameInstance::NotApplicable,
    };
    super::container::with_container(
        Driver::Postgres,
        target,
        &identity,
        image,
        pull,
        async |scratch: &str| {
            super::produce(
                Driver::Postgres,
                scratch,
                target,
                &identity,
                &inputs.binding(),
                inputs.base(),
                inputs.desired(),
                &inputs.hints,
                &[],
                &key.project,
                Some(ENVIRONMENT),
                &Standard::default(),
                externals,
                &provenance,
            )
            .await
        },
    )
    .await
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_starts_and_removes_its_container_without_root() {
    // Rootless Podman in the local gate, Docker's group in CI: the runner
    // is never root, and the run's container exists only during the run.
    let uid = std::process::Command::new("id").arg("-u").output().unwrap();
    assert_ne!(String::from_utf8_lossy(&uid.stdout).trim(), "0", "not root");
    for (target, image) in DOCKER_PAIRS {
        let (target, image) = (setting(target), setting(image));
        let identity = identity_of(&target).await;
        let before = scratch_containers();
        let during = super::container::with_container(
            Driver::Postgres,
            &target,
            &identity,
            &image,
            Default::default(),
            async |scratch: &str| {
                let mut conn = Conn::connect(Driver::Postgres, scratch).await.unwrap();
                conn.query("SELECT 1 AS ok").await.unwrap();
                Ok(new_container_volumes(&before))
            },
        )
        .await
        .unwrap();
        // The image declares its data directory a volume: the run's own
        // cluster, which `rm` without `--volumes` leaves behind.
        assert!(
            !during.is_empty(),
            "{image}: the data directory is a volume"
        );
        assert_eq!(before, scratch_containers(), "{image}: and removed");
        for volume in &during {
            assert!(
                !volume_exists(volume),
                "{image}: {volume} is removed with it"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_removes_its_container_when_the_run_fails() {
    for (target, image) in DOCKER_PAIRS {
        let (target, image) = (setting(target), setting(image));
        let identity = identity_of(&target).await;
        let before = scratch_containers();
        let mut ran = false;
        let mut volumes = Vec::new();
        let result: Result<(), ProduceError> = super::container::with_container(
            Driver::Postgres,
            &target,
            &identity,
            &image,
            Default::default(),
            async |_: &str| {
                ran = true;
                volumes = new_container_volumes(&before);
                Err(ProduceError::Run(Error::Binding("a failed run".into())))
            },
        )
        .await;
        assert!(ran, "the run was reached");
        for volume in &volumes {
            assert!(
                !volume_exists(volume),
                "{image}: {volume} is removed with it"
            );
        }
        assert!(
            matches!(&result, Err(ProduceError::Run(Error::Binding(reason))) if reason == "a failed run"),
            "{result:?}"
        );
        assert_eq!(
            before,
            scratch_containers(),
            "{image}: the failed run's container is removed"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_removes_its_container_when_the_run_is_cancelled() {
    // A run dropped mid-way, as a cancelled future or a panic drops it,
    // still has its container and volumes removed, by the drop guard.
    let target = setting("PBPS_TEST_PG_DB");
    let image = setting("PBPS_TEST_PG_SCRATCH_IMAGE");
    let identity = identity_of(&target).await;
    let before = scratch_containers();
    let (started, running) = tokio::sync::oneshot::channel();
    let run = super::container::with_container(
        Driver::Postgres,
        &target,
        &identity,
        &image,
        Default::default(),
        async |_: &str| -> Result<(), ProduceError> {
            let _ = started.send(new_container_volumes(&before));
            std::future::pending().await
        },
    );
    let volumes = tokio::select! {
        _ = run => panic!("the run never ends by itself"),
        volumes = running => volumes.unwrap(),
    };
    // The guard asks for the removal and does not wait for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while scratch_containers() != before && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(
        before,
        scratch_containers(),
        "the cancelled run's container is removed"
    );
    for volume in &volumes {
        assert!(!volume_exists(volume), "{volume} is removed with it");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_names_a_volume_its_removal_left() {
    // A second container holding the scratch's data volume: `rm --volumes`
    // succeeds and the container is gone, but the volume stays (measured
    // under Podman; Docker only logs it). The run names the volume rather
    // than reporting a clean release.
    let target = setting("PBPS_TEST_PG_DB");
    let image = setting("PBPS_TEST_PG_SCRATCH_IMAGE");
    let identity = identity_of(&target).await;
    let before = scratch_containers();
    let holder = format!("pbps-v1674-holder-{:016x}", rand::random::<u64>());
    let mut volumes = Vec::new();
    let result = super::container::with_container(
        Driver::Postgres,
        &target,
        &identity,
        &image,
        Default::default(),
        async |_: &str| {
            volumes = new_container_volumes(&before);
            let scratch: Vec<String> = scratch_containers().difference(&before).cloned().collect();
            let held = std::process::Command::new("docker")
                .args([
                    "create",
                    "--name",
                    &holder,
                    "--volumes-from",
                    &scratch[0],
                    &image,
                ])
                .output()
                .unwrap();
            assert!(held.status.success(), "docker create: {held:?}");
            Ok(())
        },
    )
    .await;
    let _ = std::process::Command::new("docker")
        .args(["rm", "--force", "--volumes", &holder])
        .output();
    for volume in &volumes {
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", volume])
            .output();
    }
    assert_eq!(
        before,
        scratch_containers(),
        "the scratch container itself is gone"
    );
    match result {
        Err(ProduceError::Cleanup(names)) => {
            assert!(!volumes.is_empty());
            for volume in &volumes {
                assert!(
                    names.contains(&format!("the docker volume {volume}")),
                    "{names:?}"
                );
            }
        }
        other => panic!("a release that left a volume reported {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_answers_a_managed_question() {
    // The question a server scratch answers, answered the same way on a
    // container started for the run.
    for (target_var, image) in DOCKER_PAIRS {
        let image = setting(image);
        let mut target = Fixture::new(target_var);
        let target_db = target.target().await;
        let result = produce_in_docker(
            &image,
            Default::default(),
            &target.on(&target_db),
            None,
            &Inputs::overload(),
            &ProjectKey::new(true),
            None,
            pbps_model::resolver::OutsideWrites::Refused,
        )
        .await;
        target.drop().await;
        let plan = result.unwrap_or_else(|error| panic!("{target_var}: {error}"));
        assert_answers_the_overload_from(
            &plan,
            pbps_model::resolver::ScratchSource::Docker {
                image: image.into(),
            },
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_records_the_image_as_configured_and_unverified() {
    // A tag, as most configurations name an image: the evidence records the
    // reference as written and no digest, since nothing verified one.
    let source = setting("PBPS_TEST_PG_SCRATCH_IMAGE");
    let tag = format!(
        "localhost/pbps-v1674-{:016x}:configured",
        rand::random::<u64>()
    );
    let tagged = std::process::Command::new("docker")
        .args(["tag", &source, &tag])
        .status()
        .unwrap();
    assert!(tagged.success(), "docker tag");
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let target_db = target.target().await;
    let result = produce_in_docker(
        &tag,
        Default::default(),
        &target.on(&target_db),
        None,
        &Inputs::overload(),
        &ProjectKey::new(true),
        None,
        pbps_model::resolver::OutsideWrites::Refused,
    )
    .await;
    target.drop().await;
    let _ = std::process::Command::new("docker")
        .args(["rmi", &tag])
        .output();
    let plan = result.unwrap();
    assert_eq!(
        plan.evidence.qualification().runtime,
        pbps_model::resolver::ResolverRuntime::Scratch {
            entry: "scratch".into(),
            scratch: pbps_model::resolver::ScratchSource::Docker {
                image: tag.as_str().into(),
            },
            // A container's superuser provisions its own database.
            layout: pbps_model::resolver::ScratchLayout::RunOwned,
            outside_writes: pbps_model::resolver::OutsideWrites::Refused,
            same_instance: pbps_model::resolver::SameInstance::NotApplicable,
        }
    );
    let runtime = serde_json::to_string(&plan.evidence.qualification().runtime).unwrap();
    // Negative: no digest is claimed, neither the source's nor any other.
    assert!(!runtime.contains("sha256"), "{runtime}");
    assert!(!runtime.contains("digest"), "{runtime}");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_binds_the_target_by_its_cluster_identity() {
    // A target connection that reaches another cluster than the planning
    // read refuses before anything is started.
    let target = setting("PBPS_TEST_PG_DB");
    let other = identity_of(&setting("PBPS_TEST_PG_OLD_DB")).await;
    let before = scratch_containers();
    let result = produce_in_docker(
        &setting("PBPS_TEST_PG_SCRATCH_IMAGE"),
        Default::default(),
        &target,
        Some(&other),
        &Inputs::overload(),
        &ProjectKey::new(true),
        None,
        pbps_model::resolver::OutsideWrites::Refused,
    )
    .await;
    let refused = resolver_refusal(result);
    assert!(
        refused.contains("another database cluster") && refused.contains("nothing was started"),
        "{refused}"
    );
    assert_eq!(before, scratch_containers(), "nothing was started");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_refuses_a_missing_image_it_may_not_pull() {
    // `pull: never` with no such image: refused by name, nothing started.
    let target = setting("PBPS_TEST_PG_DB");
    let image = format!("localhost/pbps-absent-{:016x}:1", rand::random::<u64>());
    let before = scratch_containers();
    let result = produce_in_docker(
        &image,
        pbps_config::resolver::PullPolicy::Never,
        &target,
        None,
        &Inputs::overload(),
        &ProjectKey::new(true),
        None,
        pbps_model::resolver::OutsideWrites::Refused,
    )
    .await;
    match result {
        Err(ProduceError::Acquire(reason)) => {
            assert!(
                reason.contains(&image) && reason.contains("never"),
                "{reason}"
            );
        }
        Err(other) => panic!("not an acquisition refusal: {other}"),
        Ok(_) => panic!("a plan from an image that is not there"),
    }
    assert_eq!(before, scratch_containers(), "nothing was started");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_docker_stages_its_externals_file() {
    // A managed view over a table the externals file stages compiles on a Docker scratch.
    // Negative: a table the externals file stages that differs from the target refuses.
    for (target_var, image) in DOCKER_PAIRS {
        let image = setting(image);
        for id_type in ["integer", "bigint"] {
            let mut target = Fixture::new(target_var);
            let target_db = staging_target(&mut target, &guarded_target()).await;
            let result = produce_in_docker(
                &image,
                Default::default(),
                &target.on(&target_db),
                None,
                &staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]),
                &ProjectKey::new(true),
                Some(&plain_externals(id_type)),
                pbps_model::resolver::OutsideWrites::Refused,
            )
            .await;
            target.drop().await;
            if id_type == "integer" {
                assert_staged(result, "t", target_var);
            } else {
                let refused = externals_refusal(result);
                assert!(refused.contains("atttypid"), "{target_var}: {refused}");
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_externals_outside_writes_allow_keeps_a_created_role() {
    // Under `outside_writes: allow` an externals file that creates a role stages,
    // and the evidence records the choice. Negative: under the default the
    // same externals file refuses, rolled back, so it leaves no role.
    let role = format!("pbps_v1674_role_{}", std::process::id());
    let externals = format!(
        "{}CREATE ROLE {role} NOLOGIN;\n",
        plain_externals("integer")
    );
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    // A Docker scratch: the role goes with the container.
    for outside_writes in [
        pbps_model::resolver::OutsideWrites::Allowed,
        pbps_model::resolver::OutsideWrites::Refused,
    ] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let target_db = staging_target(&mut target, &guarded_target()).await;
        let result = produce_in_docker(
            &setting("PBPS_TEST_PG_SCRATCH_IMAGE"),
            Default::default(),
            &target.on(&target_db),
            None,
            &inputs,
            &key,
            Some(&externals),
            outside_writes,
        )
        .await;
        target.drop().await;
        assert_outside_writes(result, outside_writes, &role);
    }
    // A server scratch: under `allow` the role stays, the operator's to
    // drop; under the default it was never committed.
    let scratch = Fixture::new(SCRATCH_SERVER);
    for outside_writes in [
        pbps_model::resolver::OutsideWrites::Allowed,
        pbps_model::resolver::OutsideWrites::Refused,
    ] {
        let mut target = Fixture::new("PBPS_TEST_PG_DB");
        let target_db = staging_target(&mut target, &guarded_target()).await;
        let target_url = target.on(&target_db);
        let identity = identity_of(&target_url).await;
        let result = super::produce(
            Driver::Postgres,
            &scratch.server,
            &target_url,
            &identity,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
            &Standard::default(),
            Some(&externals),
            &super::Provenance {
                entry: "scratch".into(),
                scratch: pbps_model::resolver::ScratchSource::Server {},
                outside_writes,
                same_instance: pbps_model::resolver::SameInstance::NotApplicable,
            },
        )
        .await;
        target.drop().await;
        let mut admin = Conn::connect(Driver::Postgres, &scratch.server)
            .await
            .unwrap();
        let kept = !admin
            .query(&format!(
                "SELECT 1 AS kept FROM pg_catalog.pg_roles WHERE rolname = '{role}'"
            ))
            .await
            .unwrap()
            .is_empty();
        admin
            .execute(&format!("DROP ROLE IF EXISTS {role}"))
            .await
            .unwrap();
        assert_eq!(
            kept,
            outside_writes == pbps_model::resolver::OutsideWrites::Allowed,
            "the role stays only under allow"
        );
        assert_outside_writes(result, outside_writes, &role);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_supplied_externals_outside_writes_allow_keeps_the_logins_own_default() {
    // A supplied account's allowed externals file changes the login's own
    // default. It is kept, and the release compares the compile's changes
    // with what the externals file left, not with what was there before it.
    // Negative: under the default the same externals file refuses.
    for outside_writes in [
        pbps_model::resolver::OutsideWrites::Allowed,
        pbps_model::resolver::OutsideWrites::Refused,
    ] {
        let mut fixture = Fixture::new("PBPS_TEST_PG_DB");
        let target_db = staging_target(&mut fixture, &guarded_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let externals = format!(
            "{}ALTER ROLE CURRENT_USER SET work_mem = '5MB';\n",
            plain_externals("integer")
        );
        let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
        let key = ProjectKey::new(true);
        let target_url = fixture.on(&target_db);
        let identity = identity_of(&target_url).await;
        let result = super::produce(
            Driver::Postgres,
            &fixture.as_login(&scratch_db, &login),
            &target_url,
            &identity,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
            &Standard::default(),
            Some(&externals),
            &super::Provenance {
                entry: "scratch".into(),
                scratch: pbps_model::resolver::ScratchSource::Server {},
                outside_writes,
                same_instance: pbps_model::resolver::SameInstance::NotApplicable,
            },
        )
        .await;
        fixture.drop().await;
        assert_outside_writes(result, outside_writes, &login.0);
    }
}

fn assert_outside_writes(
    result: Result<ResolvedPlan, ProduceError>,
    outside_writes: pbps_model::resolver::OutsideWrites,
    role: &str,
) {
    match outside_writes {
        pbps_model::resolver::OutsideWrites::Allowed => {
            let plan = result.unwrap_or_else(|error| panic!("allowed: {error}"));
            assert!(matches!(
                plan.evidence.qualification().runtime,
                pbps_model::resolver::ResolverRuntime::Scratch {
                    outside_writes: pbps_model::resolver::OutsideWrites::Allowed,
                    ..
                }
            ));
        }
        pbps_model::resolver::OutsideWrites::Refused => {
            let refused = externals_refusal(result);
            assert!(
                refused.contains(role) && refused.contains("outside_writes: allow"),
                "{refused}"
            );
        }
    }
}

/// A run's answer before it is sealed, as the publishing `plan` holds it
/// (#1516), with the cluster identity its planning read recorded.
async fn produce_unsealed(
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    externals: Option<&str>,
) -> (Result<super::super::Unsealed, ProduceError>, String) {
    let mut planning = Conn::connect(Driver::Postgres, target).await.unwrap();
    let identity = pbps_pg::resolver::scratch::cluster_identity(&mut planning)
        .await
        .unwrap();
    let result = super::produce_unsealed(
        Driver::Postgres,
        scratch,
        target,
        &identity,
        &inputs.binding(),
        inputs.base(),
        inputs.desired(),
        &inputs.hints,
        &[],
        &key.project,
        Some(ENVIRONMENT),
        &Standard::default(),
        externals,
        &super::Provenance::server(),
    )
    .await;
    (result, identity)
}

/// The recheck on a new session to `target`, as `plan` opens one. What it
/// refused names, with the run handed back, or its proof when it passed.
/// Never panics on the recheck's own answer, so the caller can drop its
/// fixture first.
async fn recheck(
    unsealed: super::super::Unsealed,
    target: &str,
) -> Result<super::super::Rechecked, (String, Box<super::super::Unsealed>)> {
    let mut session = Conn::connect(Driver::Postgres, target).await.unwrap();
    unsealed.recheck(&mut session).await.map_err(|refused| {
        let super::super::Refused { error, unsealed } = *refused;
        let said = if let Error::Changed(changed) = &error {
            changed.join("; ")
        } else {
            format!("not a recheck refusal: {error}")
        };
        (said, Box::new(unsealed))
    })
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_recheck_binds_the_target_by_engine_identity() {
    // Each server's target, rechecked through a session that reaches the
    // other server: one target name resolving to two clusters (#1516,
    // DEC-1685.1).
    for (server, other) in [(SERVERS[0], SERVERS[1]), (SERVERS[1], SERVERS[0])] {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let (result, identity) = produce_unsealed(
            &fixture.as_login(&scratch_db, &login),
            &fixture.on(&target_db),
            &inputs,
            &key,
            None,
        )
        .await;
        let unsealed = result.unwrap_or_else(|error| panic!("{server}: {error}"));
        let (elsewhere, same) = match recheck(unsealed, &setting(other)).await {
            Ok(_) => (None, None),
            Err((refused, unsealed)) => (
                Some(refused),
                Some(recheck(*unsealed, &fixture.on(&target_db)).await),
            ),
        };
        fixture.drop().await;
        let elsewhere =
            elsewhere.unwrap_or_else(|| panic!("{server}: a recheck on another cluster refuses"));
        assert!(
            elsewhere.contains("another cluster"),
            "{server}: {elsewhere}"
        );
        let rechecked = same
            .unwrap()
            .unwrap_or_else(|(refused, _)| panic!("{server}: the same cluster passes: {refused}"));
        // The sealed evidence carries the planning read's identity, keyed.
        let environment =
            crate::resolver::sealing::environment_key(&key.project, Some(ENVIRONMENT)).unwrap();
        let changes = rechecked.changes().clone();
        let plan = rechecked
            .seal(changes, inputs.base(), inputs.desired())
            .unwrap();
        assert_eq!(
            plan.evidence.qualification().target_cluster,
            super::super::producer::fingerprint(
                &environment,
                super::super::producer::CLUSTER_RULE,
                "system-identifier",
                identity.as_bytes(),
            ),
            "{server}"
        );
        assert_answers_the_overload(&plan);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_recheck_refuses_a_target_changed_after_the_run() {
    // Each written from a second connection after the run released its
    // scratch and before the recheck. The changed object or fact is named.
    let cases: &[(&str, &str, &str)] = &[
        // A prerequisite arrives: the overload the plan creates, created
        // out of band.
        (
            "a prerequisite",
            "CREATE FUNCTION pbps_evidence1274.f(integer) RETURNS numeric \
             LANGUAGE sql IMMUTABLE RETURN $1",
            "pbps_evidence1274.f",
        ),
        // An in-place mutation of a captured routine.
        (
            "an in-place mutation",
            "CREATE OR REPLACE FUNCTION pbps_evidence1274.f(numeric) RETURNS numeric \
             LANGUAGE sql IMMUTABLE RETURN $1 + 0",
            "pbps_evidence1274.f",
        ),
        // A setting the rule compares, for every new session.
        (
            "a compared setting",
            "ALTER DATABASE {db} SET array_nulls = off",
            "reported facts",
        ),
        // An extension the rule compares.
        (
            "an extension",
            "CREATE SCHEMA pbps_trgm_1516; CREATE EXTENSION pg_trgm SCHEMA pbps_trgm_1516",
            "reported facts",
        ),
        // The deployer's authorization: who owns a schema the plan writes.
        (
            "an authorization",
            "ALTER SCHEMA pbps_evidence1274 OWNER TO {role}",
            "deployer's authorization",
        ),
    ];
    for server in SERVERS {
        for (case, sql, named) in cases {
            let mut fixture = Fixture::new(server);
            let target_db = fixture.target().await;
            let (login, scratch_db) = fixture.confined().await;
            let inputs = Inputs::overload();
            let key = ProjectKey::new(true);
            let (result, _) = produce_unsealed(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                None,
            )
            .await;
            let target = fixture.on(&target_db);
            let role = fixture.role("o").await;
            let outcome = match result {
                Ok(unsealed) => {
                    let sql = sql.replace("{db}", &target_db).replace("{role}", &role);
                    fixture.run(&target_db, &[&sql]).await;
                    Ok(recheck(unsealed, &target).await)
                }
                Err(error) => Err(error.to_string()),
            };
            fixture.drop().await;
            let Err((refused, _)) =
                outcome.unwrap_or_else(|error| panic!("{server} {case}: the run: {error}"))
            else {
                panic!("{server} {case}: a changed target refuses");
            };
            assert!(refused.contains(named), "{server} {case}: {refused}");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_recheck_refuses_an_external_shape_changed_after_planning() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    for server in SERVERS {
        for changed in [false, true] {
            let mut fixture = Fixture::new(server);
            let key = ProjectKey::new(true);
            let target_db = staging_target(&mut fixture, &guarded_target()).await;
            let (login, scratch_db) = fixture.confined().await;
            let externals = plain_externals("integer");
            let (result, _) = produce_unsealed(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                Some(&externals),
            )
            .await;
            let outcome = match result {
                Ok(unsealed) => {
                    if changed {
                        fixture
                            .run(
                                &target_db,
                                &[&format!(
                                    "ALTER TABLE {EXTERNAL}.t ADD COLUMN extra integer"
                                )],
                            )
                            .await;
                    }
                    Ok(recheck(unsealed, &fixture.on(&target_db)).await)
                }
                Err(error) => Err(error.to_string()),
            };
            fixture.drop().await;
            let outcome = outcome.unwrap_or_else(|error| panic!("{server}: the run: {error}"));
            if changed {
                let Err((refused, _)) = outcome else {
                    panic!("{server}: a changed external shape refuses");
                };
                assert!(
                    refused.contains(&format!("{EXTERNAL}.t")),
                    "{server}: {refused}"
                );
            } else {
                outcome.unwrap_or_else(|(refused, _)| {
                    panic!("{server}: the unchanged shape publishes: {refused}")
                });
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_seals_an_accepted_build_limitation_and_rechecks_it() {
    // A scratch packaged differently from the target reports another build
    // string of the same version: accepted, named, sealed (DEC-1672.1,
    // #1700). A target whose string then changes is refused by the recheck.
    for server in SERVERS {
        for scratch_build in [None, Some("pbps test packaging")] {
            let mut fixture = Fixture::new(server);
            let target_db = fixture.target().await;
            let (login, scratch_db) = fixture.confined().await;
            let inputs = Inputs::overload();
            let key = ProjectKey::new(true);
            let (result, _) = super::SCRATCH_BUILD
                .scope(
                    scratch_build.map(str::to_owned),
                    produce_unsealed(
                        &fixture.as_login(&scratch_db, &login),
                        &fixture.on(&target_db),
                        &inputs,
                        &key,
                        None,
                    ),
                )
                .await;
            let target = fixture.on(&target_db);
            let outcome = match result {
                Ok(unsealed) => {
                    let upgraded = super::super::producer::RECHECK_BUILD
                        .scope(
                            Some("pbps test upgrade".to_owned()),
                            recheck(unsealed, &target),
                        )
                        .await;
                    Ok(match upgraded {
                        Ok(rechecked) => (None, Ok(rechecked)),
                        Err((refused, unsealed)) => {
                            (Some(refused), recheck(*unsealed, &target).await)
                        }
                    })
                }
                Err(error) => Err(error.to_string()),
            };
            fixture.drop().await;
            let (upgraded, unchanged) =
                outcome.unwrap_or_else(|error| panic!("{server}: the run: {error}"));
            let refused = upgraded
                .unwrap_or_else(|| panic!("{server}: a target whose build changed refuses"));
            assert!(refused.contains("reported facts"), "{server}: {refused}");
            let rechecked = unchanged.unwrap_or_else(|(refused, _)| panic!("{server}: {refused}"));
            let changes = rechecked.changes().clone();
            let plan = rechecked
                .seal(changes, inputs.base(), inputs.desired())
                .unwrap();
            let limitations = &plan.evidence.qualification().limitations;
            match scratch_build {
                Some(build) => {
                    let named = limitations
                        .get("build:server_version")
                        .unwrap_or_else(|| panic!("{server}: {limitations:?}"));
                    assert!(named.raw().contains(build), "{server}: {:?}", named.raw());
                }
                None => assert!(
                    limitations.is_empty(),
                    "{server}: identical strings seal no limitation: {limitations:?}"
                ),
            }
        }
    }
}

// ---- An externals file drafted from the target (#1778) ---------------------

/// The managed view `a` over external objects of every class the draft
/// writes, and what guards them, which it leaves out.
fn drafting_target(a: &str) -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE EXTENSION citext SCHEMA {EXTERNAL}"),
        format!("CREATE TYPE {EXTERNAL}.mood AS ENUM ('sad', 'ok', 'it''s fine')"),
        format!("CREATE DOMAIN {EXTERNAL}.score AS integer NOT NULL CHECK (VALUE >= 0)"),
        format!("CREATE TYPE {EXTERNAL}.pair AS (a integer, b text COLLATE \"C\", m integer[][])"),
        format!("CREATE DOMAIN {EXTERNAL}.grid AS integer[][]"),
        format!("CREATE TABLE {EXTERNAL}.p (id integer PRIMARY KEY)"),
        format!(
            "CREATE TABLE {EXTERNAL}.t (id integer PRIMARY KEY, \
             k bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 10), \
             j bigint GENERATED ALWAYS AS IDENTITY, \
             name {EXTERNAL}.citext UNIQUE, mood {EXTERNAL}.mood NOT NULL, \
             score {EXTERNAL}.score, pair {EXTERNAL}.pair, tags text[], \
             grid {EXTERNAL}.grid, \
             amount numeric(10,2) DEFAULT 0 CHECK (amount >= 0), \
             p integer REFERENCES {EXTERNAL}.p) WITH (fillfactor = 70)"
        ),
        // Owned, as `serial` makes one, but read by no default: the capture
        // refuses a `regclass` constant, whose output depends on the path.
        format!("CREATE SEQUENCE {EXTERNAL}.t_id_seq AS integer OWNED BY {EXTERNAL}.t.id"),
        // An identity sequence given a type of its own.
        format!("ALTER SEQUENCE {EXTERNAL}.t_j_seq AS integer"),
        // And one given a persistence of its own.
        format!("ALTER SEQUENCE {EXTERNAL}.t_k_seq SET UNLOGGED"),
        format!("CREATE UNIQUE INDEX t_k ON {EXTERNAL}.t (k) WHERE k > 0"),
        format!("CREATE INDEX t_amount ON {EXTERNAL}.t (amount)"),
        format!("ALTER TABLE {EXTERNAL}.t ALTER COLUMN tags SET STORAGE MAIN"),
        format!(
            "CREATE VIEW {EXTERNAL}.v WITH (security_barrier) AS \
             SELECT id, name, mood FROM {EXTERNAL}.t WHERE score > 0"
        ),
        format!("CREATE MATERIALIZED VIEW {EXTERNAL}.mv AS SELECT id, tags FROM {EXTERNAL}.t"),
        format!("ALTER MATERIALIZED VIEW {EXTERNAL}.mv ALTER COLUMN tags SET STORAGE MAIN"),
        format!("CREATE UNIQUE INDEX mv_id ON {EXTERNAL}.mv (id)"),
        format!("ALTER MATERIALIZED VIEW {EXTERNAL}.mv CLUSTER ON mv_id"),
        format!("CREATE VIEW {MANAGED}.a AS {a}"),
    ]
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_stages_an_externals_file_drafted_from_the_target_unchanged() {
    let a = format!(
        "SELECT t.id, t.k, t.name, t.mood, t.score, t.pair, t.tags, t.grid, t.amount, \
         v.mood AS seen, mv.tags AS kept \
         FROM {EXTERNAL}.t t JOIN {EXTERNAL}.v v ON v.id = t.id \
         JOIN {EXTERNAL}.mv mv ON mv.id = t.id"
    );
    let b = reading("name", "v");
    let inputs = staging_inputs(&a, &b, &[]);
    let declarations = declared(&[("a", &a), ("b", &b)], &[]);
    let key = ProjectKey::new(true);
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(&mut fixture, &drafting_target(&a)).await;
        let mut target = Conn::connect(Driver::Postgres, &fixture.on(&target_db))
            .await
            .unwrap();
        let draft = pbps_pg::resolver::draft::draft(&mut target, &declarations, &[]).await;
        drop(target);
        let (login, scratch_db) = fixture.confined().await;
        let result = match &draft {
            Ok(draft) => Some(
                produce_staged(
                    &fixture.as_login(&scratch_db, &login),
                    &fixture.on(&target_db),
                    &inputs,
                    &key,
                    &draft.sql,
                )
                .await,
            ),
            Err(_) => None,
        };
        fixture.drop().await;
        let draft = draft.unwrap_or_else(|error| panic!("{server}: the draft: {error}"));
        assert!(
            draft.sql.contains(&format!(
                "CREATE VIEW {EXTERNAL}.v WITH (security_barrier=true) AS SELECT \
                 NULL::integer AS id, NULL::{EXTERNAL}.citext AS name, \
                 NULL::{EXTERNAL}.mood AS mood WHERE false;\n"
            )),
            "{server}: a shape view\n{}",
            draft.sql
        );
        // Negative: what only guards the table, and the view's query.
        for left_out in [
            format!("CREATE TABLE {EXTERNAL}.p "),
            "CHECK".into(),
            "t_amount".into(),
            "score > 0".into(),
        ] {
            assert!(
                !draft.sql.contains(&left_out),
                "{server}: {left_out} is left out\n{}",
                draft.sql
            );
        }
        assert!(draft.listed.is_empty(), "{server}: {:?}", draft.listed);
        assert_staged(
            result.expect("the draft was staged"),
            "t",
            &format!("{server}, staging the draft\n{}", draft.sql),
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_draft_lists_a_routine_and_a_chain_and_writes_neither() {
    let a = format!(
        "SELECT {EXTERNAL}.f(1) AS one, {EXTERNAL}.g() AS two, {MANAGED}.h(3) AS three, c.m, u.x \
         FROM {EXTERNAL}.c c, {EXTERNAL}.u u"
    );
    let mut declarations = declared(&[("a", &a)], &["m"]);
    // A declared routine: its lookup runs under its declaration's path, and
    // the reads after it must still spell every name under the empty one.
    declarations.modules.insert(
        format!("{MANAGED}.h(integer)").parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
            options: Default::default(),
        },
    );
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = staging_target(
            &mut fixture,
            &[
                format!("CREATE SCHEMA {MANAGED}"),
                format!("CREATE SCHEMA {EXTERNAL}"),
                format!(
                    "CREATE FUNCTION {EXTERNAL}.f(integer) RETURNS integer \
                     LANGUAGE sql IMMUTABLE RETURN $1 + 1"
                ),
                format!("CREATE TABLE {MANAGED}.m (id integer)"),
                format!(
                    "CREATE FUNCTION {MANAGED}.h(integer) RETURNS integer \
                     LANGUAGE sql IMMUTABLE RETURN $1"
                ),
                // Its body reads the managed table; a body is not compared,
                // so that is no chain (SPEC §9.3.2).
                format!(
                    "CREATE FUNCTION {EXTERNAL}.g() RETURNS integer LANGUAGE sql STABLE \
                     RETURN (SELECT count(*)::integer FROM {MANAGED}.m)"
                ),
                format!("CREATE TABLE {EXTERNAL}.c (m {MANAGED}.m)"),
                // Its default names the managed table; a default is not
                // compared, so that is no chain either.
                format!(
                    "CREATE DOMAIN {EXTERNAL}.d AS oid \
                     DEFAULT ('{MANAGED}.m'::pg_catalog.regclass)::oid"
                ),
                format!("CREATE TABLE {EXTERNAL}.u (x {EXTERNAL}.d)"),
                format!("CREATE VIEW {MANAGED}.a AS {a}"),
            ],
        )
        .await;
        let mut target = Conn::connect(Driver::Postgres, &fixture.on(&target_db))
            .await
            .unwrap();
        let draft = pbps_pg::resolver::draft::draft(&mut target, &declarations, &[]).await;
        drop(target);
        fixture.drop().await;
        let draft = draft.unwrap_or_else(|error| panic!("{server}: the draft: {error}"));
        assert!(
            draft.listed.iter().any(|line| line.starts_with(&format!(
                "function {EXTERNAL}.f(integer): listed, never written"
            ))),
            "{server}: {:?}",
            draft.listed
        );
        assert!(
            draft.listed.iter().any(|line| line.starts_with(&format!(
                "table {EXTERNAL}.c cannot be staged: its shape names the managed table \
                 {MANAGED}.m"
            ))),
            "{server}: {:?}",
            draft.listed
        );
        assert!(
            draft
                .listed
                .iter()
                .any(|line| line
                    .starts_with(&format!("function {EXTERNAL}.g(): listed, never written"))),
            "{server}: {:?}",
            draft.listed
        );
        assert!(
            !draft
                .listed
                .iter()
                .any(|line| line.starts_with(&format!("function {EXTERNAL}.g() cannot"))),
            "{server}: a routine's body is no chain: {:?}",
            draft.listed
        );
        assert!(
            draft
                .sql
                .contains(&format!("CREATE DOMAIN {EXTERNAL}.d AS oid;\n")),
            "{server}: a domain's default is no chain\n{}",
            draft.sql
        );
        // Negative: the managed routine is neither listed nor written.
        assert!(
            !draft.sql.contains(&format!("{MANAGED}.h")),
            "{server}\n{}",
            draft.sql
        );
        // Negative: neither is written, and no routine source in any form.
        assert!(
            !draft.sql.contains(&format!("CREATE TABLE {EXTERNAL}.c")),
            "{server}\n{}",
            draft.sql
        );
        assert!(!draft.sql.contains("$1 + 1"), "{server}\n{}", draft.sql);
        assert!(
            !draft.sql.to_uppercase().contains("CREATE FUNCTION"),
            "{server}\n{}",
            draft.sql
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_generated_table_parts_seal_and_close_plain_exception_and_row_shapes() {
    use crate::resolver::generated_fixtures::shape;
    for server in SERVERS {
        for case in [
            "plain",
            "chain",
            "chain-pure",
            "chain-cycle",
            "partition",
            "later",
            "rows",
            "prefix-later",
            "prefix-only",
            "prefix-forward",
            "composite-check",
        ] {
            let mut fixture = Fixture::new(server);
            let target_db = fixture.database("g1503", None).await;
            fixture.run(&target_db, &["CREATE SCHEMA app"]).await;
            let (login, scratch_db) = fixture.confined().await;
            let mut desired = shape(case);
            // Bound-node capture qualification is independent of creation
            // parts. Ordinary apply below also measures a real child.
            if case == "partition" {
                desired
                    .tables
                    .remove(&pbps_model::TableName::new("app", "a_default"));
            }
            let inputs = Inputs::from_pair((pbps_model::Schema::default(), desired));
            let key = ProjectKey::new(true);
            let result: anyhow::Result<()> = async {
                let mut target = Conn::connect(Driver::Postgres, &fixture.on(&target_db)).await?;
                let version = target.query("SHOW server_version_num").await?;
                let modern = version[0]
                    .try_get::<&str>("server_version_num")?
                    .unwrap()
                    .parse::<i64>()?
                    >= 170_000;
                let planned = produce(
                    &fixture.as_login(&scratch_db, &login),
                    &fixture.on(&target_db),
                    &inputs,
                    &key,
                )
                .await;
                if case == "chain-cycle" || (case == "chain" && !modern) {
                    let Err(why) = planned else {
                        panic!("{case}: a named cycle must be refused")
                    };
                    let why = why.to_string();
                    assert!(
                        why.contains("app.a") || (why.contains("app.f") && why.contains("app.g")),
                        "{why}"
                    );
                    return Ok(());
                }
                let plan = planned.map_err(|e| anyhow::anyhow!("{case}: {e}"))?;
                if case == "chain" || case == "chain-pure" {
                    assert_eq!(
                        plan.changes.changes.iter().any(|p| matches!(
                            p.change,
                            pbps_model::Change::AlterColumnExpression { .. }
                        )),
                        case == "chain"
                    );
                }
                plan.evidence.validate(&plan.changes)?;
                let saved = pbps_model::SavedPlan::new(
                    pbps_model::PlanOrigin::Database,
                    "postgres",
                    "2026-10-19",
                    pbps_model::PlanBaseline {
                        description: "empty fixture".into(),
                        checksum: String::new(),
                        database_collation: None,
                        time_zone: None,
                    },
                    plan.changes.changes().clone(),
                    inputs.desired().ids.clone(),
                )
                .with_resolution(plan.evidence)?;
                let saved: pbps_model::SavedPlan =
                    serde_json::from_slice(&serde_json::to_vec(&saved)?)?;
                // Validators after seed rows retain constraint approval;
                // only parts before the first write finish an empty CREATE.
                let expected = if case == "composite-check" || case == "rows" {
                    [pbps_model::RiskClass::Constraint].into()
                } else {
                    Default::default()
                };
                assert_eq!(
                    saved.changes.gated_risks(),
                    expected,
                    "{case}: {:?}",
                    saved.changes
                );
                let risks = pbps_diff::order::generated::risks(
                    &saved.changes,
                    inputs.desired().ids,
                    &pbps_pg::Postgres::new(),
                );
                for (part, expected) in saved.changes.changes.iter().zip(risks) {
                    assert_eq!(part.risks, expected, "{case}: {:?}", part.change);
                }
                let keyed = crate::resolver::apply::Keyed::check(
                    &saved,
                    crate::resolver::sealing::environment_key(&key.project, Some(ENVIRONMENT)),
                    "issue-1503",
                )?
                .unwrap();
                let mut conn = Conn::connect(Driver::Postgres, &fixture.on(&target_db)).await?;
                let guard = keyed.open(&mut conn).await?;
                conn.execute("BEGIN").await?;
                guard.before_statements(&mut conn).await?;
                let pg = pbps_pg::Postgres::new();
                for p in &saved.changes.changes {
                    for statement in pg.fixture_emit(&p.change, p.strategy)? {
                        conn.execute(&statement.sql).await?;
                    }
                }
                guard
                    .before_recording(&mut conn, inputs.desired().schema, inputs.desired().ids)
                    .await?;
                conn.execute("COMMIT").await?;
                Ok(())
            }
            .await;
            fixture.drop().await;
            result.unwrap_or_else(|e| panic!("{server}, {case}: {e:#}"));
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn scratch_generated_table_parts_compile_literal_and_collision_names_before_functions() {
    use crate::resolver::generated_fixtures::shape;
    use crate::resolver::server::engine;
    for server in SERVERS {
        for case in ["literal", "reverse", "array", "collision", "missing"] {
            let mut fixture = Fixture::new(server);
            let database = fixture.database("s1503", None).await;
            fixture.run(&database, &["CREATE SCHEMA app"]).await;
            let mut desired = shape(case);
            if case == "missing" {
                desired
                    .tables
                    .get_mut(&pbps_model::TableName::new("app", "a"))
                    .unwrap()
                    .columns
                    .get_mut("g")
                    .unwrap()
                    .value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
                    expression: "app.missing(id)".into(),
                    stored: true,
                });
            }
            let inputs = Inputs::from_pair((pbps_model::Schema::default(), desired));
            let result: anyhow::Result<()> = async {
                let mut conn = Conn::connect(Driver::Postgres, &fixture.on(&database)).await?;
                let version = engine::generation_version(&mut conn).await.map_err(anyhow::Error::msg)?;
                let pg = pbps_pg::Postgres::new().with_server_version(version);
                let mut reconstruction = engine::reconstruction(Driver::Postgres, &[],
                    inputs.binding().bootstrap, inputs.desired().ids, &inputs.hints.module_deps, version)
                    .map_err(anyhow::Error::msg)?;
                let compiled = reconstruction.compile(&pg, &pbps_model::SessionZone::utc(), &mut conn).await;
                if case == "missing" {
                    let Err(pbps_pg::resolver::reconstruct::ReconstructError::Refused(report)) = compiled else {
                        anyhow::bail!("a missing function must remain an engine compile refusal");
                    };
                    anyhow::ensure!(report.pending.iter().filter_map(|pending| pending.last.as_ref()).any(|last|
                        last.error.server_error_code().as_deref() == Some("42883")
                            && last.error.to_string().contains("app.missing")
                            && last.error.to_string().contains("does not exist")), "{report}");
                    anyhow::ensure!(reconstruction.observations().is_none(), "a failed compile grants no binding observations");
                    let rows = conn.query("SELECT to_regclass('app.a') IS NULL AS absent").await?;
                    anyhow::ensure!(rows[0].try_get::<bool>("absent")? == Some(true),
                        "failed compilation must leave no table");
                    return Ok(());
                }
                compiled?;
                let columns = conn.query("SELECT attname::text AS name FROM pg_attribute WHERE attrelid='app.a'::regclass AND attnum>0 AND NOT attisdropped ORDER BY attnum").await?;
                assert_eq!(columns.iter().map(|r| r.try_get::<&str>("name").unwrap().unwrap()).collect::<Vec<_>>(),
                    inputs.desired().schema.tables[&pbps_model::TableName::new("app", "a")].columns.keys().map(pbps_model::ColumnName::raw).collect::<Vec<_>>());
                if case == "collision" {
                    let rows = conn.query("SELECT t.typname::text AS name FROM pg_type r JOIN pg_type t ON t.oid=r.typarray WHERE r.oid='app.a'::regtype").await?;
                    assert_eq!(rows[0].try_get::<&str>("name")?.unwrap(), "_a_1");
                }
                Ok(())
            }.await;
            fixture.drop().await;
            result.unwrap_or_else(|e| panic!("{server}, {case}: {e:#}"));
        }
    }
}

const CATALOG_RAW2382: &str = "a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
const CATALOG_SHOWN2382: &str = r"a\n\u{1b}\u{202e}\\n\\u{1b}é*/";

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn catalog_prose_shared_comments_are_shown_once_by_externals_and_fill() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let role = format!("r2382_{}_{}", scratch.token, CATALOG_RAW2382);
    let quoted = format!("\"{role}\"");
    scratch
        .admin()
        .await
        .execute(&format!("CREATE ROLE {quoted} NOLOGIN"))
        .await
        .unwrap();
    scratch.roles.push(quoted.clone());
    let before = target.foreign_objects(&target_db).await;
    let control = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &plain_externals("integer"),
    )
    .await;
    let comment = format!("COMMENT ON ROLE {quoted} IS 'changed'");
    let mut results = Vec::new();
    for fill in [false, true] {
        let externals = if fill {
            format!(
                "CREATE SCHEMA {EXTERNAL}; CREATE FUNCTION {EXTERNAL}.comment2382() RETURNS event_trigger LANGUAGE plpgsql AS $f$BEGIN IF tg_tag = 'CREATE TABLE' THEN EXECUTE $comment2382${comment}$comment2382$; END IF; END$f$; CREATE EVENT TRIGGER comment2382 ON ddl_command_end EXECUTE FUNCTION {EXTERNAL}.comment2382();"
            )
        } else {
            format!("{}{comment};", plain_externals("integer"))
        };
        let refusal = externals_refusal(
            produce_staged(
                &scratch.server,
                &target.on(&target_db),
                &inputs,
                &key,
                &externals,
            )
            .await,
        );
        let row = scratch.admin().await.query(&format!("SELECT pg_catalog.shobj_description(oid, 'pg_authid') AS comment FROM pg_catalog.pg_roles WHERE rolname = $role2382${role}$role2382$" )).await.unwrap();
        let left = row[0]
            .try_get::<&str>("comment")
            .unwrap()
            .map(str::to_owned);
        results.push((fill, refusal, left));
    }
    // Read the same real shared-catalog producer directly, without changing
    // either comparison row: its name stays raw until a finding renders it.
    scratch.admin().await.execute(&comment).await.unwrap();
    let shared = pbps_pg::resolver::externals::shared_state(&mut scratch.admin().await)
        .await
        .unwrap();
    scratch
        .admin()
        .await
        .execute(&format!("COMMENT ON ROLE {quoted} IS NULL"))
        .await
        .unwrap();
    let after = target.foreign_objects(&target_db).await;
    let displayed = format!(
        "the comment on role r2382_{}_{}",
        scratch.token, CATALOG_SHOWN2382
    );
    target.drop().await;
    scratch.drop().await;
    assert_staged(control, "t", "unchanged shared objects");
    assert_eq!(
        before, after,
        "the target, including its absent ledger, is untouched"
    );
    assert!(
        shared
            .values()
            .any(|(_, name)| name == &format!("the comment on role {role}"))
    );
    assert!(pbps_pg::resolver::externals::shared_changes(&shared, &shared).is_empty());
    for (fill, refusal, left) in results {
        let expected = if fill {
            format!(
                "filling what the externals file lacks changed {displayed}, outside the scratch database: something the externals file created, such as an event trigger, ran on fill's statements; an externals file creates only objects in the database, unless the resolver sets `outside_writes: allow`"
            )
        } else {
            format!(
                "it changed {displayed}, outside the scratch database; an externals file creates only objects in the database, unless the resolver sets `outside_writes: allow`"
            )
        };
        assert_eq!(refusal, expected, "fill={fill}");
        assert_eq!(left, None, "the refused comment was rolled back");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn catalog_prose_scratch_refusals_show_descriptions_without_changing_authority() {
    use pbps_pg::resolver::{scratch as sql, standard};
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        let inputs = Inputs::overload();
        let key = ProjectKey::new(true);
        let before = fixture.foreign_objects(&target_db).await;
        let function = format!("public.\"{}\"", CATALOG_RAW2382);
        fixture.run(&scratch_db, &[&format!("CREATE FUNCTION {function}() RETURNS integer LANGUAGE sql RETURN 7; ALTER FUNCTION {function}() OWNER TO {}", login.0)]).await;
        let foreign_raw = fixture.foreign_objects(&scratch_db).await;
        let nonempty = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let foreign_kept = fixture.foreign_objects(&scratch_db).await;
        fixture
            .run(&scratch_db, &[&format!("DROP FUNCTION {function}()")])
            .await;
        let other = fixture.database("a", Some(&login.0)).await;
        let database = format!("d2382_{}_{}", fixture.token, CATALOG_RAW2382);
        let quoted = format!("\"{database}\"");
        fixture
            .admin()
            .await
            .execute(&format!("ALTER DATABASE {other} RENAME TO {quoted}"))
            .await
            .unwrap();
        *fixture.databases.iter_mut().find(|d| **d == other).unwrap() = quoted.clone();
        let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
            .await
            .unwrap();
        let account = sql::account(&mut conn).await.unwrap();
        drop(conn);
        let authority = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        fixture.admin().await.execute(&format!("ALTER DATABASE {quoted} OWNER TO CURRENT_USER; GRANT CREATE ON DATABASE {quoted} TO {}", login.0)).await.unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
            .await
            .unwrap();
        let reach_raw = sql::cleanup_reach(&mut conn).await.unwrap();
        drop(conn);
        let reach = resolver_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        fixture
            .admin()
            .await
            .execute(&format!(
                "REVOKE CREATE ON DATABASE {quoted} FROM {}",
                login.0
            ))
            .await
            .unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &fixture.as_login(&scratch_db, &login))
            .await
            .unwrap();
        let backend = sql::backend(&mut conn).await.unwrap();
        let defaults = sql::login_defaults(&mut conn).await.unwrap();
        let acl = standard::read_state(&mut conn).await.unwrap().acl;
        // This role owns the database's objects but DROP OWNED by the login
        // cannot remove them: the leftover report must show the same scalar.
        conn.execute(&format!("SET ROLE pg_database_owner; CREATE FUNCTION {function}() RETURNS integer LANGUAGE sql RETURN 7; SET ROLE NONE")).await.unwrap();
        let released = super::release_supplied(
            &mut conn,
            &backend,
            &Standard::default(),
            &scratch_db,
            &defaults,
            &acl,
        )
        .await;
        conn.execute(&format!(
            "SET ROLE pg_database_owner; DROP FUNCTION {function}(); SET ROLE NONE"
        ))
        .await
        .unwrap();
        let success = super::release_supplied(
            &mut conn,
            &backend,
            &Standard::default(),
            &scratch_db,
            &defaults,
            &acl,
        )
        .await;
        drop(conn);
        let after = fixture.foreign_objects(&target_db).await;
        let shown_function = format!("function public.\"{}\"()", CATALOG_SHOWN2382);
        let shown_database = format!("database d2382_{}_{}", fixture.token, CATALOG_SHOWN2382);
        fixture.drop().await;
        assert_eq!(foreign_raw.0, [format!("function {function}()")]);
        assert_eq!(foreign_raw, foreign_kept, "the refusal removes nothing");
        assert_eq!(
            nonempty,
            format!(
                "the scratch database is not empty: 1 object(s) initdb did not create, including {shown_function}"
            )
        );
        assert_eq!(
            account.shared,
            [format!("ownership of database {database}")]
        );
        assert_eq!(
            authority,
            format!(
                "the scratch account shares the target's cluster and is not confined to its database (no ownership of {shown_database}); remove them from it, or use a scratch server on another cluster"
            )
        );
        assert_eq!(reach_raw, [format!("database {database}")]);
        assert_eq!(
            reach,
            format!(
                "the scratch account {} has objects or privileges that emptying its database with DROP OWNED would also drop or revoke ({shown_database}); use an account with none outside that database",
                login.0
            )
        );
        assert_eq!(
            released,
            Err(vec![format!(
                "1 object(s) in database {scratch_db} owned by a role other than the scratch account ({shown_function})"
            )])
        );
        assert_eq!(success, Ok(()));
        assert_eq!(before, after, "no target write or ledger recording");
    }
}

mod recovery2410_tests;

mod docker2410_tests;

#[path = "domain_tests.rs"]
mod domain_tests;
mod session2133_tests;

mod acceptance621_tests;
