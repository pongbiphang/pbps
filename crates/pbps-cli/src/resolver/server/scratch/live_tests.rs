//! Live regressions for the resolver (#1672), on the pinned
//! PostgreSQL servers with no root, no Docker and no process observation.
//!
//! Each test makes its own target and scratch databases and logins, named
//! with a run token, and drops them at the end. The declarations are the
//! measured producer's motivating pair (`qualified_evidence_cases`): an
//! arriving integer overload rebinds one view and leaves the other alone.

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
    assert_eq!(
        plan.evidence.qualification().runtime,
        pbps_model::resolver::ResolverRuntime::Scratch {
            scratch,
            outside_writes: pbps_model::resolver::OutsideWrites::Refused,
        },
        "the evidence names the resolver's scratch"
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
    assert_eq!(
        before, after,
        "the run-owned database, login and roles are all dropped"
    );
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
        assert_answers_the_overload(&result.unwrap());
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

// ---- The baseline (#1673) ----------------------------------------------
//
// A managed schema and an external one. The target holds external objects
// the managed views bind; the baseline stages them on scratch.

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
    baseline: &str,
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
        Some(baseline),
        &super::Provenance::server(),
    )
    .await
}

/// `produce_staged`, with `sql` run on the target after the baseline was
/// compared with it and before the opening capture.
async fn produce_staged_then(
    sql: &str,
    scratch: &str,
    target: &str,
    inputs: &Inputs,
    key: &ProjectKey,
    baseline: &str,
) -> Result<ResolvedPlan, ProduceError> {
    super::AFTER_STAGING
        .scope(
            Some(sql.to_owned()),
            produce_staged(scratch, target, inputs, key, baseline),
        )
        .await
}

/// The baseline's refusal, or what came instead. Never panics, so the
/// caller can drop its fixture first.
fn baseline_refusal(result: Result<ResolvedPlan, ProduceError>) -> String {
    match result {
        Err(ProduceError::Run(Error::Baseline(findings))) => findings.join("; "),
        Err(other) => format!("not a baseline refusal: {other}"),
        Ok(_) => "not a baseline refusal: a plan".into(),
    }
}

/// The plan answered, and what the baseline staged was sealed on the
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

/// The table as a baseline writes it: its shape, nothing that guards it.
fn plain_baseline(id_type: &str) -> String {
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
async fn resolver_compiles_a_managed_view_over_a_baseline_table() {
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
        &plain_baseline("integer"),
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
            &plain_baseline("integer"),
        )
        .await;
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert_staged(result, "t", server);
        assert_eq!(
            left.1, 0,
            "{server}: the baseline's objects are dropped too"
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_an_external_table_without_its_foreign_keys_and_triggers() {
    // The baseline leaves out the referenced table, the CHECK, the default,
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
            &plain_baseline("integer"),
        )
        .await;
        fixture.drop().await;
        assert_staged(result, "t", server);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_baseline_that_differs_from_the_target() {
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
        let baseline = format!(
            "CREATE SCHEMA {EXTERNAL};\n\
             CREATE TABLE {EXTERNAL}.t (id bigint PRIMARY KEY, name text, p integer, n integer);\n\
             CREATE TYPE {EXTERNAL}.pair AS (b text, a integer);"
        );
        let refused = baseline_refusal(
            produce_staged(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                &baseline,
            )
            .await,
        );
        let left = fixture.foreign_objects(&scratch_db).await;
        fixture.drop().await;
        assert!(
            refused.contains(&format!(
                "the baseline's column id of relation {EXTERNAL}.t differs from the target's in"
            )) && refused.contains("atttypid"),
            "{server}: {refused}"
        );
        assert!(
            refused.contains(&format!("the target has index {EXTERNAL}.t_name_key")),
            "{server}: {refused}"
        );
        assert!(
            refused.contains(&format!(
                "the baseline's relation {EXTERNAL}.pair differs from the target's in column_order"
            )),
            "{server}: {refused}"
        );
        assert_eq!(left.1, 0, "{server}: a refused run empties its database");
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_baseline_that_creates_a_managed_object() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let baseline = format!(
        "{}CREATE VIEW {MANAGED}.b AS SELECT NULL::text AS name WHERE false;",
        plain_baseline("integer")
    );
    let refused = baseline_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "the baseline created view {MANAGED}.b, which is in the managed set"
        )),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_runs_the_baseline_before_any_managed_object() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    // A statement naming a managed object fails: nothing managed exists yet.
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let naming = format!(
        "{}CREATE VIEW {EXTERNAL}.w AS SELECT id FROM {MANAGED}.a;",
        plain_baseline("integer")
    );
    let refused = baseline_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &naming,
        )
        .await,
    );
    // A database default the baseline changes is read by the compatibility
    // qualification, as the target's is.
    let defaulting = format!(
        "{}DO $$BEGIN EXECUTE format('ALTER DATABASE %I SET search_path = {EXTERNAL}', \
         current_database()); END$$;",
        plain_baseline("integer")
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
        let baseline = format!(
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
            &baseline,
        )
        .await;
        fixture.drop().await;
        assert_staged(result, "t", server);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_baseline_object_left_uncompared() {
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let mut setup = guarded_target();
    setup.push(format!(
        "CREATE CONVERSION {EXTERNAL}.words FOR 'LATIN1' TO 'UTF8' FROM pg_catalog.iso8859_1_to_utf8"
    ));
    let target_db = staging_target(&mut target, &setup).await;
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let baseline = format!(
        "{}CREATE CONVERSION {EXTERNAL}.words FOR 'LATIN1' TO 'UTF8' \
         FROM pg_catalog.iso8859_1_to_utf8;",
        plain_baseline("integer")
    );
    let refused = baseline_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "the baseline created conversion {EXTERNAL}.words, which the \
             comparison with the target does not cover"
        )),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_chain_through_an_external_object_naming_it() {
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
    let inputs = staging_inputs(&reading("id", "e"), &reading("m", "e"), &["m"]);
    let key = ProjectKey::new(true);
    let refused = baseline_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &format!("CREATE SCHEMA {EXTERNAL};"),
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "view {MANAGED}.a binds table {EXTERNAL}.e, whose shape names the managed table \
             {MANAGED}.m"
        )) && refused.contains("adopt"),
        "{refused}"
    );
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
    let baseline = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE VIEW {EXTERNAL}.v AS SELECT NULL::integer AS id, NULL::text AS name WHERE false;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
    let baseline =
        format!("CREATE SCHEMA {EXTERNAL};\nCREATE TABLE {EXTERNAL}.v (id integer, name text);");
    let refused = baseline_refusal(
        produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await,
    );
    target.drop().await;
    assert!(
        refused.contains(&format!(
            "the baseline's table {EXTERNAL}.v differs from the target's in"
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
        },
    );
    base.modules
        .remove(&format!("{MANAGED}.w()").parse().unwrap());
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let baseline = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE VIEW {EXTERNAL}.v AS SELECT NULL::integer AS id, NULL::text AS name WHERE false;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
            "the baseline stages {EXTERNAL}.v as a view no statement can write through: stage \
             its real definition"
        )),
        "{reason}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_question_that_needs_routine_source_the_baseline_lacks() {
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
    let baseline = format!(
        "CREATE SCHEMA {EXTERNAL};\n\
         CREATE FUNCTION {EXTERNAL}.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
        reason.contains("a same-named routine on the target is not in the resolver's baseline")
            && reason.contains("select no resolver"),
        "{reason}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_what_the_baseline_adds_to_an_extension_it_did_not_create() {
    // `plpgsql` is there before the baseline runs, so a routine the
    // baseline adds to it is compared on its own, not skipped as covered by
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
    let baseline = external
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
    // routine: the baseline need not repeat the target's body (SPEC §9.3.2).
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
    let baseline = format!(
        "CREATE FUNCTION {routine}(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 0;"
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
async fn resolver_stages_a_cast_and_an_operator_over_external_types_in_the_baseline() {
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
        let baseline = external
            .iter()
            .map(|s| format!("{s};\n"))
            .collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await;
        target.drop().await;
        assert_staged(result, "pair", server);
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_compares_each_extension_member_with_the_target() {
    // The extension's version matches; one member the target altered in
    // place does not, and is named (#1766, #1783).
    let call = format!("SELECT {EXTERNAL}.soundex('pbps') AS r");
    let baseline =
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
            &baseline,
        )
        .await;
        target.drop().await;
        if altered {
            let refused = baseline_refusal(result);
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
    let baseline =
        format!("CREATE SCHEMA {EXTERNAL};\nCREATE EXTENSION postgres_fdw SCHEMA {EXTERNAL};");
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        format!("CREATE SCHEMA {EXTERNAL}"),
        format!("CREATE EXTENSION postgres_fdw SCHEMA {EXTERNAL}"),
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
        &baseline,
    )
    .await;
    target.drop().await;
    let refused = baseline_refusal(result);
    assert!(
        refused.contains("foreign-data wrapper postgres_fdw")
            && refused.contains("no plan can seal its extension"),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_stages_an_extension_that_brings_text_search_objects() {
    // `unaccent` brings a text search dictionary and template, which the
    // capture identifies and the comparison reads like any member (#1786).
    let call = format!("SELECT {EXTERNAL}.unaccent('pbps') AS r");
    let baseline =
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
        &baseline,
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
        let baseline = format!(
            "{}{};\n",
            plain_baseline("integer"),
            dictionary(staged_options)
        );
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await;
        target.drop().await;
        if staged_options.is_empty() {
            let refused = baseline_refusal(result);
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
        let baseline = family(staged_strategy)
            .iter()
            .map(|s| format!("{s};\n"))
            .collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await;
        target.drop().await;
        if staged_strategy == 1 {
            // Negative: the same members are staged.
            let plan = result.unwrap_or_else(|error| panic!("an identical family: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        } else {
            let refused = baseline_refusal(result);
            assert!(
                refused.contains("the target has") && refused.contains("the baseline does not"),
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
    let mut baseline = external
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    baseline.push_str(&format!(
        "CREATE VIEW {EXTERNAL}.v AS SELECT NULL::integer AS s WHERE false;\n"
    ));
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
    )
    .await;
    target.drop().await;
    let plan = result.unwrap_or_else(|error| panic!("a routine over a staged type: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_staged_shape_that_changes_before_the_opening_capture() {
    // A writer between the baseline's comparison and the capture the plan
    // seals: scratch compiled `SELECT *` against the compared table, and
    // the evidence would seal the table with another column (#1755).
    let inputs = staging_inputs(&reading("id", "t"), &reading("*", "t"), &[]);
    let key = ProjectKey::new(true);
    for (sql, changes) in [
        (
            format!("ALTER TABLE {EXTERNAL}.t ADD COLUMN extra integer"),
            true,
        ),
        // Negative: a write to nothing the baseline staged.
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
            &plain_baseline("integer"),
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
                Ok(_) => panic!("a plan sealed over a shape the baseline was not compared with"),
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
        let baseline = table.iter().map(|s| format!("{s};\n")).collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await;
        target.drop().await;
        if altered {
            let refused = baseline_refusal(result);
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
        let baseline = class(staged_default)
            .iter()
            .map(|s| format!("{s};\n"))
            .collect::<String>();
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await;
        target.drop().await;
        if staged_default {
            // Negative: the same class is staged.
            let plan = result.unwrap_or_else(|error| panic!("an identical class: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        } else {
            let refused = baseline_refusal(result);
            assert!(refused.contains("opcdefault"), "{refused}");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_refuses_a_baseline_that_writes_outside_the_scratch_database() {
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    // Run-owned: the baseline runs as the scratch superuser.
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
        let baseline = format!("{}{statements}\n", plain_baseline("integer"));
        let refused = baseline_refusal(
            produce_staged(
                &scratch.server,
                &target.on(&target_db),
                &inputs,
                &key,
                &baseline,
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
        let baseline = format!(
            "{}ALTER ROLE CURRENT_USER SET work_mem = '5MB';\n",
            plain_baseline("integer")
        );
        let refused = baseline_refusal(
            produce_staged(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                &baseline,
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
async fn resolver_runs_the_baseline_on_the_empty_path_every_session_pins() {
    // Every pbps session pins an empty path (DEC-1564.1), the baseline's
    // too: a file written for `psql` names its schemas or sets the path, as
    // `pg_dump` output does, and one that does neither is told so.
    let call = "SELECT public.soundex('pbps') AS r";
    let setup = vec![
        format!("CREATE SCHEMA {MANAGED}"),
        "CREATE EXTENSION fuzzystrmatch SCHEMA public".to_owned(),
        format!("CREATE VIEW {MANAGED}.a AS {call}"),
    ];
    for (baseline, staged) in [
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
            baseline,
        )
        .await;
        target.drop().await;
        if staged {
            let plan = result.unwrap_or_else(|error| panic!("{baseline}: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
        } else {
            // Negative: no schema on the path, and the refusal says why.
            let refused = baseline_refusal(result);
            assert!(
                refused.contains("it runs on an empty search_path"),
                "{refused}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_checks_a_baseline_whose_path_shadows_a_built_in_type() {
    // The baseline's `SET`s last on its session until `DISCARD ALL`, and the
    // checks after it run there: a type it names `text`, first on its path,
    // must not change what they read.
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let scratch = Fixture::new(SCRATCH_SERVER);
    let mut setup = guarded_target();
    setup.push(format!("CREATE TYPE {EXTERNAL}.text AS ENUM ('a')"));
    let target_db = staging_target(&mut target, &setup).await;
    let baseline = format!(
        "{}CREATE TYPE {EXTERNAL}.text AS ENUM ('a');\nSET search_path = {EXTERNAL}, pg_catalog;\n",
        plain_baseline("integer")
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
        let baseline = format!(
            "{}ALTER DATABASE {other} REFRESH COLLATION VERSION;\n",
            plain_baseline("integer")
        );
        let result = produce_staged(
            &scratch.server,
            &target.on(&target_db),
            &inputs,
            &key,
            &baseline,
        )
        .await;
        target.drop().await;
        scratch.drop().await;
        if stale {
            let refused = baseline_refusal(result);
            assert!(refused.contains(&format!("database {other}")), "{refused}");
        } else {
            // Negative: a refresh that changes nothing is no change.
            assert_staged(result, "t", "a refresh that changes nothing");
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_reads_the_shared_state_under_its_own_settings_not_the_baselines() {
    // A baseline's `SET TimeZone` renders a role's expiry with another
    // offset; read under it, the second read of the shared state would
    // differ from the first and refuse a baseline that changed nothing.
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
    let baseline = format!(
        "{}SET TimeZone = 'Pacific/Kiritimati';\nSET statement_timeout = '1ms';\n",
        plain_baseline("integer")
    );
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
    )
    .await;
    target.drop().await;
    scratch.drop().await;
    assert_staged(result, "t", "a baseline that sets its own time zone");
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn resolver_ignores_another_sessions_cluster_writes_during_the_baseline() {
    // Other runs share the scratch cluster: a role one creates while this
    // baseline runs is in neither of its reads, so nothing is refused.
    let inputs = staging_inputs(&reading("id", "t"), &reading("name", "t"), &[]);
    let key = ProjectKey::new(true);
    let mut target = Fixture::new("PBPS_TEST_PG_DB");
    let mut scratch = Fixture::new(SCRATCH_SERVER);
    let target_db = staging_target(&mut target, &guarded_target()).await;
    let marker = format!("pbps_v1783_marker_{}", scratch.token);
    let baseline = format!(
        "{}SELECT pg_catalog.pg_sleep(3) AS {marker};\n",
        plain_baseline("integer")
    );
    let other = format!("pbps_v1783_other_{}", scratch.token);
    let mut admin = scratch.admin().await;
    let concurrent = async {
        // Once the baseline is running, another session creates a role.
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
        produce_staged(&scratch.server, &target_url, &inputs, &key, &baseline),
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
    let refused = baseline_refusal(
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
async fn resolver_stages_an_operator_over_built_in_types_in_the_baseline() {
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
    let baseline = external
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    let result = produce_staged(
        &scratch.server,
        &target.on(&target_db),
        &inputs,
        &key,
        &baseline,
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
async fn resolver_reproduces_the_deployers_usage_of_a_schema_the_baseline_creates() {
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
        &plain_baseline("integer"),
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
    let baseline = carried
        .iter()
        .map(|s| format!("{s};\n"))
        .collect::<String>();
    let result = produce_staged(
        &scratch.server,
        &target.as_login(&target_db, &deployer),
        &inputs,
        &key,
        &baseline,
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

/// The containers a Docker scratch labels, so a test can tell what a run
/// left behind.
fn scratch_containers() -> std::collections::BTreeSet<String> {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            "label=pbps.resolver=scratch",
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
    baseline: Option<&str>,
    outside_writes: pbps_model::resolver::OutsideWrites,
) -> Result<ResolvedPlan, ProduceError> {
    let identity = match planned {
        Some(identity) => identity.to_owned(),
        None => identity_of(target).await,
    };
    let provenance = super::Provenance {
        scratch: pbps_model::resolver::ScratchSource::Docker {
            image: image.to_owned(),
        },
        outside_writes,
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
                baseline,
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
            pbps_model::resolver::ScratchSource::Docker { image },
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
            scratch: pbps_model::resolver::ScratchSource::Docker { image: tag.clone() },
            outside_writes: pbps_model::resolver::OutsideWrites::Refused,
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
async fn resolver_docker_stages_its_baseline() {
    // A managed view over a baseline table compiles on a Docker scratch.
    // Negative: a baseline table that differs from the target refuses.
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
                Some(&plain_baseline(id_type)),
                pbps_model::resolver::OutsideWrites::Refused,
            )
            .await;
            target.drop().await;
            if id_type == "integer" {
                assert_staged(result, "t", target_var);
            } else {
                let refused = baseline_refusal(result);
                assert!(refused.contains("atttypid"), "{target_var}: {refused}");
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers and a container runtime"]
async fn resolver_baseline_outside_writes_allow_keeps_a_created_role() {
    // Under `outside_writes: allow` a baseline that creates a role stages,
    // and the evidence records the choice. Negative: under the default the
    // same baseline refuses, rolled back, so it leaves no role.
    let role = format!("pbps_v1674_role_{}", std::process::id());
    let baseline = format!("{}CREATE ROLE {role} NOLOGIN;\n", plain_baseline("integer"));
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
            Some(&baseline),
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
            Some(&baseline),
            &super::Provenance {
                scratch: pbps_model::resolver::ScratchSource::Server {},
                outside_writes,
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
async fn resolver_supplied_baseline_outside_writes_allow_keeps_the_logins_own_default() {
    // A supplied account's allowed baseline changes the login's own
    // default. It is kept, and the release compares the compile's changes
    // with what the baseline left, not with what was there before it.
    // Negative: under the default the same baseline refuses.
    for outside_writes in [
        pbps_model::resolver::OutsideWrites::Allowed,
        pbps_model::resolver::OutsideWrites::Refused,
    ] {
        let mut fixture = Fixture::new("PBPS_TEST_PG_DB");
        let target_db = staging_target(&mut fixture, &guarded_target()).await;
        let (login, scratch_db) = fixture.confined().await;
        let baseline = format!(
            "{}ALTER ROLE CURRENT_USER SET work_mem = '5MB';\n",
            plain_baseline("integer")
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
            Some(&baseline),
            &super::Provenance {
                scratch: pbps_model::resolver::ScratchSource::Server {},
                outside_writes,
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
            let refused = baseline_refusal(result);
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
    baseline: Option<&str>,
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
        baseline,
        &super::Provenance::server(),
    )
    .await;
    (result, identity)
}

/// The recheck on a new session to `target`, as `plan` opens one. What it
/// refused names, or its proof when it passed. Never panics on the
/// recheck's own answer, so the caller can drop its fixture first.
async fn recheck(
    unsealed: &super::super::Unsealed,
    target: &str,
) -> Result<super::super::Rechecked, String> {
    let mut session = Conn::connect(Driver::Postgres, target).await.unwrap();
    match unsealed.recheck(&mut session).await {
        Ok(rechecked) => Ok(rechecked),
        Err(Error::Changed(changed)) => Err(changed.join("; ")),
        Err(other) => Err(format!("not a recheck refusal: {other}")),
    }
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
        let elsewhere = recheck(&unsealed, &setting(other)).await;
        let same = recheck(&unsealed, &fixture.on(&target_db)).await;
        fixture.drop().await;
        let Err(elsewhere) = elsewhere else {
            panic!("{server}: a recheck on another cluster refuses");
        };
        assert!(
            elsewhere.contains("another cluster"),
            "{server}: {elsewhere}"
        );
        let rechecked =
            same.unwrap_or_else(|refused| panic!("{server}: the same cluster passes: {refused}"));
        // The sealed evidence carries the planning read's identity, keyed.
        let environment =
            crate::resolver::sealing::environment_key(&key.project, Some(ENVIRONMENT)).unwrap();
        let changes = unsealed.changes().clone();
        let plan = unsealed
            .seal(rechecked, changes, inputs.base(), inputs.desired())
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
                    Ok(recheck(&unsealed, &target).await)
                }
                Err(error) => Err(error.to_string()),
            };
            fixture.drop().await;
            let Err(refused) =
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
            let baseline = plain_baseline("integer");
            let (result, _) = produce_unsealed(
                &fixture.as_login(&scratch_db, &login),
                &fixture.on(&target_db),
                &inputs,
                &key,
                Some(&baseline),
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
                    Ok(recheck(&unsealed, &fixture.on(&target_db)).await)
                }
                Err(error) => Err(error.to_string()),
            };
            fixture.drop().await;
            let outcome = outcome.unwrap_or_else(|error| panic!("{server}: the run: {error}"));
            if changed {
                let Err(refused) = outcome else {
                    panic!("{server}: a changed external shape refuses");
                };
                assert!(
                    refused.contains(&format!("{EXTERNAL}.t")),
                    "{server}: {refused}"
                );
            } else {
                outcome.unwrap_or_else(|refused| {
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
                            recheck(&unsealed, &target),
                        )
                        .await;
                    let unchanged = recheck(&unsealed, &target).await;
                    Ok((unsealed, upgraded, unchanged))
                }
                Err(error) => Err(error.to_string()),
            };
            fixture.drop().await;
            let (unsealed, upgraded, unchanged) =
                outcome.unwrap_or_else(|error| panic!("{server}: the run: {error}"));
            let Err(refused) = upgraded else {
                panic!("{server}: a target whose build changed refuses");
            };
            assert!(refused.contains("reported facts"), "{server}: {refused}");
            let rechecked = unchanged.unwrap_or_else(|refused| panic!("{server}: {refused}"));
            let changes = unsealed.changes().clone();
            let plan = unsealed
                .seal(rechecked, changes, inputs.base(), inputs.desired())
                .unwrap();
            let limitations = &plan.evidence.qualification().limitations;
            match scratch_build {
                Some(build) => {
                    let named = limitations
                        .get("build:server_version")
                        .unwrap_or_else(|| panic!("{server}: {limitations:?}"));
                    assert!(named.contains(build), "{server}: {named}");
                }
                None => assert!(
                    limitations.is_empty(),
                    "{server}: identical strings seal no limitation: {limitations:?}"
                ),
            }
        }
    }
}
