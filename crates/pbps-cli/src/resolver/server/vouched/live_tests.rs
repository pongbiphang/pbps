//! Live regressions for the operator-vouched resolver (#1672), on the pinned
//! PostgreSQL servers with no root, no Docker and no process observation.
//!
//! Each test makes its own target and scratch databases and logins, named
//! with a run token, and drops them at the end. The declarations are the
//! measured producer's motivating pair (`qualified_evidence_cases`): an
//! arriving integer overload rebinds one view and leaves the other alone.

use super::super::qualified_evidence_cases as cases;
use super::super::qualified_evidence_tests::{ENVIRONMENT, Inputs, ProjectKey, view};
use super::super::{Error, ProduceError, ResolvedPlan};
use pbps_db::{Conn, Driver};
use pbps_model::Change;

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
        pbps_pg::resolver::vouched::foreign_objects(&mut conn)
            .await
            .unwrap()
    }

    async fn drop(self) {
        let mut admin = self.admin().await;
        for database in &self.databases {
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
    super::produce(
        Driver::Postgres,
        scratch,
        target,
        &inputs.binding(),
        inputs.base(),
        inputs.desired(),
        &inputs.hints,
        &[],
        &key.project,
        Some(ENVIRONMENT),
    )
    .await
}

fn vouched_refusal(result: Result<ResolvedPlan, ProduceError>) -> String {
    // Never panics: the caller drops its fixture first and asserts on this,
    // so a wrong outcome does not leave databases and roles behind.
    match result {
        Err(ProduceError::Run(Error::Vouched(reason))) => reason,
        Err(other) => format!("not a vouched refusal: {other}"),
        Ok(_) => "not a vouched refusal: a plan".into(),
    }
}

/// The answer the engine gives for the motivating pair: the unqualified call
/// moves to the arriving overload and is rebuilt; the explicit numeric call
/// stays, and the resolved plan leaves it alone although ADR-0013's
/// candidate test would rebuild it.
fn assert_answers_the_overload(plan: &ResolvedPlan) {
    plan.evidence.validate(&plan.changes).unwrap();
    pbps_pg::resolver::validate_evidence(&plan.evidence)
        .expect("the vouched evidence passes the artifact reader");
    assert_eq!(
        plan.evidence.qualification().runtime,
        pbps_model::resolver::ResolverRuntime::Vouched,
        "the evidence names the operator-vouched resolver"
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
async fn vouched_answers_a_managed_question_without_root_or_observation() {
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
    assert_eq!(
        before, after,
        "the run-owned database, login and roles are all dropped"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn vouched_runs_in_a_precreated_database_with_a_confined_account() {
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
        let (login, scratch_db) = fixture.confined().await;
        // A membership it cannot `SET ROLE` through gives it no attribute:
        // the login stays confined (#1678 review).
        let creator = format!("pbps_v1672_m_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {creator} NOLOGIN CREATEDB CREATEROLE; \
                 GRANT {creator} TO {} WITH SET FALSE",
                login.0
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, creator);
        // Owning another database is outside `DROP OWNED`'s reach: no
        // refusal for it.
        fixture.database("o", Some(&login.0)).await;
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
        assert_answers_the_overload(&result.unwrap());
        assert_eq!(
            left.1, 0,
            "{server}: the run empties its database: {left:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn vouched_refuses_the_target_as_scratch() {
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
            vouched_refusal(produce(&spelled, &fixture.on(&target_db), &inputs, &key).await);
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
async fn vouched_refuses_a_same_cluster_scratch_with_an_unconfined_account() {
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
        // without SET, and the replication attribute through a role it can
        // become, which creates cluster-wide slots (#1678 security review).
        let (reacher, reacher_password) = fixture.login("x", "NOCREATEDB").await;
        let program = format!("pbps_v1672_p_{}", fixture.token);
        let slots = format!("pbps_v1672_q_{}", fixture.token);
        fixture
            .admin()
            .await
            .execute(&format!(
                "CREATE ROLE {program} NOLOGIN; GRANT pg_execute_server_program TO {program}; \
                 GRANT {program} TO {reacher} WITH INHERIT TRUE, SET FALSE; \
                 CREATE ROLE {slots} NOLOGIN REPLICATION; \
                 GRANT {slots} TO {reacher} WITH INHERIT FALSE, SET TRUE"
            ))
            .await
            .unwrap();
        fixture.roles.insert(0, program);
        fixture.roles.insert(0, slots);
        let reaching = fixture.database("x", Some(&reacher)).await;
        let before = fixture.inventory().await;
        let superuser = vouched_refusal(
            produce(&fixture.on(&other), &fixture.on(&target_db), &inputs, &key).await,
        );
        let member = vouched_refusal(
            produce(
                &fixture.as_login(&owned, &(creator.clone(), password.clone())),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let reach = vouched_refusal(
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
        fixture.drop().await;
        assert!(superuser.contains("NOSUPERUSER"), "{server}: {superuser}");
        assert!(member.contains("NOCREATEDB"), "{server}: {member}");
        assert!(
            reach.contains("no membership in pg_execute_server_program")
                && reach.contains("NOREPLICATION"),
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
async fn vouched_refuses_a_scratch_that_is_not_empty() {
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
        let reason = vouched_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let left = fixture.foreign_objects(&scratch_db).await;
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
        let subscription = vouched_refusal(
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
        let lob = fixture.database("l", Some(&login.0)).await;
        fixture.run(&lob, &["SELECT pg_catalog.lo_create(1)"]).await;
        let large_object = vouched_refusal(
            produce(
                &fixture.as_login(&lob, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let lob_left = fixture.foreign_objects(&lob).await;
        // A database holding a subscription cannot be dropped.
        fixture
            .run(&subscribed, &["DROP SUBSCRIPTION leftover_sub"])
            .await;
        // Negative: a database the login does not own is refused too, since
        // emptying it afterwards would revoke what was granted on it.
        let foreign = fixture.database("f", None).await;
        let unowned = vouched_refusal(
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
async fn vouched_empties_the_supplied_database_when_the_run_refuses() {
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
async fn vouched_refuses_a_supplied_login_whose_cleanup_would_revoke_grants_elsewhere() {
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
        let reason = vouched_refusal(
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
async fn vouched_compiles_as_a_non_superuser_provisioner_in_its_own_database() {
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
async fn vouched_cleanup_runs_only_on_the_backend_the_run_checked() {
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
        let backend = pbps_pg::resolver::vouched::backend(&mut checked)
            .await
            .unwrap();
        let moved = pbps_pg::resolver::vouched::drop_owned(&mut other, &backend).await;
        let after_moved = fixture.foreign_objects(&scratch_db).await;
        let same = pbps_pg::resolver::vouched::drop_owned(&mut checked, &backend).await;
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
async fn vouched_compiles_as_the_login_whatever_its_session_defaults() {
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
async fn vouched_provisions_as_the_superuser_login_whatever_its_default_role() {
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
async fn vouched_refuses_a_supplied_login_whose_cleanup_would_revoke_memberships_it_granted() {
    // The login holds `ADMIN OPTION` on a role and granted it to another
    // role. `DROP OWNED` would remove that membership, which lives outside
    // the scratch database (#1678 review).
    for server in SERVERS {
        let mut fixture = Fixture::new(server);
        let target_db = fixture.target().await;
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
        let reason = vouched_refusal(
            produce(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
            )
            .await,
        );
        let after = granted().await;
        let left = fixture.foreign_objects(&scratch_db).await;
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
}
