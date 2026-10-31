//! PostgreSQL's scratch adapter. The lifecycle calls keep their sessions and
//! transaction boundaries together so moving the dispatch cannot change which
//! backend a check or its cleanup reaches.

use super::super::producer::fingerprint;
use super::super::scratch::{Provenance, Provisioning, Tokens};
use super::super::{BindingRequest, ScopeRequest};
use super::{Error, ProduceError};
use crate::resolver::scope;
use pbps_db::resolver::ScratchNames;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_model::escape::Escaped;
use pbps_model::resolver::OutsideWrites;
use pbps_pg::resolver::capture::{self, Managed, Paths, RecordedOwnership, StageError, Staged};
use pbps_pg::resolver::recovery as record;
use pbps_pg::resolver::scratch::{self as sql, Placement};
use pbps_pg::resolver::standard::{self, Standard};
use pbps_pg::resolver::{draft, externals};
use std::collections::{BTreeMap, BTreeSet};

mod recovery;
mod release;

use pbps_pg::resolver::session::{Reset, SessionState};

struct Session {
    conn: Conn,
    state: SessionState,
}

impl Session {
    fn new(conn: Conn, role: Option<pbps_model::RoleName>) -> Self {
        Self {
            conn,
            state: SessionState::new(role),
        }
    }
}

enum Sessions {
    RunOwned {
        compile: Session,
        capture: Box<Session>,
    },
    Supplied(SessionState),
}

impl Sessions {
    fn compiling<'a>(&'a mut self, admin: &'a mut Conn) -> (&'a mut Conn, &'a mut SessionState) {
        match self {
            Self::RunOwned { compile, .. } => (&mut compile.conn, &mut compile.state),
            Self::Supplied(state) => (admin, state),
        }
    }

    fn setup<'a>(&'a mut self, admin: &'a mut Conn) -> (&'a mut Conn, &'a SessionState) {
        match self {
            Self::RunOwned { capture, .. } => (&mut capture.conn, &capture.state),
            Self::Supplied(state) => (admin, state),
        }
    }

    fn capture<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn {
        self.setup(admin).0
    }
}

/// Entry points and stop points for the tests that drive a release or a
/// sweep without a run around it.
#[cfg(test)]
pub(super) mod for_tests {
    pub(in crate::resolver::server) use super::recovery::for_tests::sweep_cluster_on;
    use super::*;

    /// Databases whose supplied release stops after the cleanup commit and
    /// before the privileges are put back, as a kill between the two would.
    static STOPPED_AFTER_CLEANUP: std::sync::Mutex<BTreeSet<String>> =
        std::sync::Mutex::new(BTreeSet::new());

    pub(in crate::resolver::server) fn stop_after_cleanup(database: &str, stop: bool) {
        let mut stopped = STOPPED_AFTER_CLEANUP.lock().unwrap();
        if stop {
            stopped.insert(database.to_owned());
        } else {
            stopped.remove(database);
        }
    }

    pub(super) fn stopped_after_cleanup(database: &str) -> bool {
        STOPPED_AFTER_CLEANUP.lock().unwrap().contains(database)
    }

    static RELEASE_HOPS: std::sync::Mutex<BTreeMap<String, Conn>> =
        std::sync::Mutex::new(BTreeMap::new());

    #[derive(Debug, PartialEq, Eq)]
    pub(in crate::resolver::server) struct ReleasePhase {
        pub principal: String,
        pub pid: String,
        pub transaction: String,
    }

    static RELEASE_PHASES: std::sync::Mutex<BTreeMap<String, Vec<ReleasePhase>>> =
        std::sync::Mutex::new(BTreeMap::new());

    pub(in crate::resolver::server) fn hop_release(database: &str, next: Conn) {
        assert!(
            RELEASE_HOPS
                .lock()
                .unwrap()
                .insert(database.into(), next)
                .is_none()
        );
    }

    pub(super) fn release_after_cleanup(conn: &mut Conn, database: &str) {
        if let Some(next) = RELEASE_HOPS.lock().unwrap().remove(database) {
            *conn = next;
        }
    }

    pub(in crate::resolver::server) fn trace_release(database: &str) {
        assert!(
            RELEASE_PHASES
                .lock()
                .unwrap()
                .insert(database.into(), Vec::new())
                .is_none()
        );
    }

    pub(super) async fn release_phase(conn: &mut Conn, database: &str) {
        release_role_phase(conn, database, false).await;
        let enabled = RELEASE_PHASES.lock().unwrap().contains_key(database);
        if !enabled {
            return;
        }
        // A real assigned transaction ID distinguishes a checked transaction
        // from separately committed statements; a backend PID alone does not.
        let rows = conn
            .query(
                "SELECT current_user::text AS principal, pg_catalog.pg_backend_pid()::text AS pid, \
             pg_catalog.pg_current_xact_id()::text AS transaction",
            )
            .await
            .unwrap();
        let value = |name| rows[0].try_get::<&str>(name).unwrap().unwrap().to_owned();
        RELEASE_PHASES
            .lock()
            .unwrap()
            .get_mut(database)
            .unwrap()
            .push(ReleasePhase {
                principal: value("principal"),
                pid: value("pid"),
                transaction: value("transaction"),
            });
    }

    pub(in crate::resolver::server) fn take_release_trace(database: &str) -> Vec<ReleasePhase> {
        RELEASE_PHASES.lock().unwrap().remove(database).unwrap()
    }

    #[derive(Debug)]
    pub(in crate::resolver::server) struct ReleaseRolePhase {
        pub principal: String,
        pub pid: String,
        pub transaction: String,
        pub locks: i64,
    }

    struct RoleTrace {
        watched: Vec<i64>,
        phases: Vec<ReleaseRolePhase>,
    }

    static ROLE_PHASES: std::sync::Mutex<BTreeMap<String, RoleTrace>> =
        std::sync::Mutex::new(BTreeMap::new());
    static ROLE_HOPS: std::sync::Mutex<BTreeMap<String, Conn>> =
        std::sync::Mutex::new(BTreeMap::new());

    pub(in crate::resolver::server) fn trace_release_roles(database: &str, watched: Vec<i64>) {
        assert!(!watched.is_empty() && watched.iter().all(|oid| *oid > 0));
        assert!(
            ROLE_PHASES
                .lock()
                .unwrap()
                .insert(
                    database.into(),
                    RoleTrace {
                        watched,
                        phases: Vec::new(),
                    }
                )
                .is_none()
        );
    }

    pub(in crate::resolver::server) fn take_release_roles_trace(
        database: &str,
    ) -> Vec<ReleaseRolePhase> {
        ROLE_PHASES.lock().unwrap().remove(database).unwrap().phases
    }

    pub(in crate::resolver::server) fn hop_release_roles(database: &str, next: Conn) {
        assert!(
            ROLE_HOPS
                .lock()
                .unwrap()
                .insert(database.into(), next)
                .is_none()
        );
    }

    pub(in crate::resolver::server) fn clear_release_roles_hop(database: &str) {
        ROLE_HOPS.lock().unwrap().remove(database);
    }

    pub(super) async fn release_role_phase(conn: &mut Conn, database: &str, after_commit: bool) {
        if after_commit {
            if let Some(next) = ROLE_HOPS.lock().unwrap().remove(database) {
                *conn = next;
            }
            return;
        }
        let watched = ROLE_PHASES
            .lock()
            .unwrap()
            .get(database)
            .map(|trace| trace.watched.clone());
        let Some(watched) = watched else {
            return;
        };
        // Capture OIDs before DROP ROLE: its own transaction no longer sees
        // their catalog rows, but their object locks must remain observable.
        let ids = watched
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let rows = conn
            .query(&format!(
                "SELECT current_user::text AS principal, pg_catalog.pg_backend_pid()::text AS pid, \
             pg_catalog.pg_current_xact_id()::text AS transaction, \
             (SELECT count(*) FROM pg_catalog.pg_locks WHERE pid = pg_catalog.pg_backend_pid() \
              AND locktype = 'object' AND classid = 'pg_catalog.pg_authid'::regclass \
              AND objid IN ({ids})) AS locks"
            ))
            .await
            .unwrap();
        let value = |name| rows[0].try_get::<&str>(name).unwrap().unwrap().to_owned();
        let phase = ReleaseRolePhase {
            principal: value("principal"),
            pid: value("pid"),
            transaction: value("transaction"),
            locks: rows[0].try_get::<i64>("locks").unwrap().unwrap(),
        };
        ROLE_PHASES
            .lock()
            .unwrap()
            .get_mut(database)
            .unwrap()
            .phases
            .push(phase);
    }

    // Key the fault to the borrowed connection, without adding an observation
    // before RESET: a script's timeout must never affect test-only I/O.
    static FAIL_RESTORE: std::sync::Mutex<BTreeSet<usize>> = std::sync::Mutex::new(BTreeSet::new());

    pub(super) fn after_externals(session: &Conn, script: &str) {
        if script == "SELECT 1 /* fail_restore_2133 */" {
            FAIL_RESTORE
                .lock()
                .unwrap()
                .insert(session as *const Conn as usize);
        }
    }

    pub(super) fn restore_fails(session: &Conn) -> bool {
        FAIL_RESTORE
            .lock()
            .unwrap()
            .remove(&(session as *const Conn as usize))
    }

    pub(in crate::resolver::server) async fn reset_for_test(session: &mut Conn) {
        let state = SessionState::checked(sql::backend(session).await.unwrap(), None);
        reset_and_restore(session, &state, Reset::All, "the test")
            .await
            .unwrap();
    }

    pub(in crate::resolver::server) async fn externals_for_test(session: &mut Conn) {
        let state = SessionState::checked(sql::backend(session).await.unwrap(), None);
        run_externals(session, &state, "SELECT 1", OutsideWrites::Refused)
            .await
            .unwrap();
    }
}

/// Where the scratch session is relative to the target's. Each session marks
/// itself, and the scratch session looks for both marks: one it can see is a
/// backend of its own cluster (#1672's refinement of the decision on #1667).
/// Where the scratch sits, what its account may do, and the backend both
/// were read on, all from one scratch transaction.
async fn separation(
    target: &mut Conn,
    scratch: &mut Conn,
    tokens: &Tokens,
    planned_identity: &str,
) -> Result<(Placement, sql::Account, sql::Backend), Error> {
    let failed = |error: pbps_db::DbError| Error::Read(format!("the session marks: {error}"));
    // Both sessions hold a transaction across the whole check. A
    // transaction-pooling proxy releases a backend between transactions, and
    // the scratch session could then be handed the target's backend and
    // overwrite its mark, reading "another cluster" (#1678 review). The
    // marks are transaction-local, so ending the transactions clears them.
    target.execute("BEGIN").await.map_err(failed)?;
    let observed = async {
        scratch.execute("BEGIN").await?;
        let target_mark = sql::mark(target, &tokens.target).await?;
        let scratch_mark = sql::mark(scratch, &tokens.scratch).await?;
        let seen = sql::marked(scratch, &[&tokens.target, &tokens.scratch]).await?;
        let mut account = sql::account(scratch).await?;
        account.target_login =
            sql::session_login(target).await? == sql::session_login(scratch).await?;
        let backend = sql::backend(scratch).await?;
        // Last, because a refused read aborts its transaction; it is told
        // apart below so the remedy can name the grant.
        let identities = (
            sql::cluster_identity(target).await,
            sql::cluster_identity(scratch).await,
        );
        Ok::<_, pbps_db::DbError>((
            target_mark,
            scratch_mark,
            seen,
            account,
            backend,
            identities,
        ))
    }
    .await;
    // Nothing was written in either transaction; rolling back only ends it.
    let ended = (
        target.execute("ROLLBACK").await,
        scratch.execute("ROLLBACK").await,
    );
    let (target_mark, scratch_mark, seen, account, backend, identities) =
        observed.map_err(failed)?;
    ended.0.map_err(failed)?;
    ended.1.map_err(failed)?;
    let target_identity = identity(identities.0, "target")?;
    let scratch_identity = identity(identities.1, "scratch")?;
    // A name can reach more than one cluster: DNS round-robin, a balancing
    // proxy, a reader endpoint. The plan was computed against the cluster
    // the planning read reached; a check made on another would vouch for
    // the wrong one (#1685).
    if target_identity != planned_identity {
        return Err(Error::Unusable(format!(
            "the target connection reached another database cluster (system identifier \
             {target_identity}) than the one the plan was read from ({planned_identity}); \
             a target name that reaches more than one cluster, through DNS or a balancing \
             proxy, cannot be resolved, so connect to one cluster"
        )));
    }
    let placement = sql::placement(
        (&tokens.target, target_mark),
        (&tokens.scratch, scratch_mark),
        &seen,
        scratch_identity == target_identity,
    )
    .map_err(|reason| Error::Unusable(reason.into()))?;
    Ok((placement, account, backend))
}

/// A cluster identity, or the refusal that names why it could not be read.
/// Without it a scratch on the target's cluster could pass as another one,
/// so an unreadable identity refuses rather than falls back to the marks.
fn identity(read: Result<String, pbps_db::DbError>, side: &str) -> Result<String, Error> {
    match read {
        Ok(identity) => Ok(identity),
        Err(error) if error.server_error_code().as_deref() == Some("42501") => {
            Err(Error::Unusable(format!(
                "the {side} session may not read its cluster's identity ({error}); grant it \
                 with GRANT EXECUTE ON FUNCTION pg_catalog.pg_control_system() TO the {side} login"
            )))
        }
        Err(error) => Err(Error::Read(format!(
            "the {side} cluster's identity: {error}"
        ))),
    }
}

/// Refuses unless `conn`'s current transaction runs on the backend the run
/// checked: the scratch connection must keep one backend, as a direct or a
/// session-pooled one does (#1678 review).
async fn pinned(conn: &mut Conn, checked: &sql::Backend) -> Result<(), Error> {
    match sql::on_backend(conn, checked).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Unusable(
            "the scratch connection reached another server backend than the one the run \
             checked; connect to the scratch server directly or through session pooling, \
             not transaction pooling"
                .into(),
        )),
        Err(error) => Err(Error::Read(format!("the scratch backend: {error}"))),
    }
}

/// What the run has created and must release.
enum Created {
    Nothing,
    RunOwned {
        names: ScratchNames,
        roles: Vec<pbps_model::RoleName>,
    },
    /// Objects the scratch account owns in the supplied database, its own
    /// stored defaults, and the database's ACL as they were once the
    /// database was put into its standard state.
    Supplied {
        database: String,
        defaults: std::collections::BTreeSet<(String, String)>,
        acl: BTreeSet<standard::Entry>,
    },
}

pub(in crate::resolver::server) struct Scratch<'a> {
    driver: Driver,
    scratch: &'a str,
    request: &'a ScopeRequest,
    /// The scratch backend the separation was read on.
    backend: sql::Backend,
    /// The state the scratch database is put into (#1708).
    standard: &'a Standard,
    /// The resolver's externals file, as its file holds it (#1673). It is cut
    /// into statements as it runs, since a statement can change how the
    /// next is read.
    externals: Option<&'a str>,
    provenance: &'a Provenance,
    /// The run's token: in every name it creates and on its keeper (#2410).
    token: String,
    created: Created,
    /// Whether this session holds the supplied database's lock (`recovery`).
    holds_lock: bool,
    /// Where the scratch sits relative to the target's cluster: what decides how
    /// a rehearsal names the plan's roles (`Naming`).
    placement: Placement,
}

/// How a rehearsal names the roles a plan names, by where the scratch is
/// (#2432). One strategy for each placement, so no rehearsal creates, or leaves
/// on a cluster other runs use, a role it should not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::resolver::server) enum Naming {
    /// The scratch is on the target's own cluster: the roles are the target's
    /// real ones and are there. Nothing is created, recorded or asked of the
    /// account, and the scratch database's own cleanup takes every grant and
    /// policy that names them.
    Existing,
    /// The scratch is a cluster of this run's own (a container): the real
    /// names, made for the run where the cluster lacks them.
    Created,
    /// A cluster other runs and other tenants use: names of the run's own, so
    /// two runs never collide and no real role of the cluster's is touched.
    Mapped,
}

impl Naming {
    /// A container is a cluster of the run's own whichever way it is placed;
    /// otherwise the account's cluster is the target's or another.
    pub(in crate::resolver::server) fn of(
        source: &pbps_model::resolver::ScratchSource,
        placement: Placement,
    ) -> Self {
        match (source, placement) {
            (pbps_model::resolver::ScratchSource::Docker { .. }, _) => Self::Created,
            (pbps_model::resolver::ScratchSource::Server {}, Placement::SameCluster) => {
                Self::Existing
            }
            // The target's own database is refused before a rehearsal gets here.
            (
                pbps_model::resolver::ScratchSource::Server {},
                Placement::SeparateCluster | Placement::Target,
            ) => Self::Mapped,
        }
    }
}

/// The scratch side, provisioned and qualified: the session the declarations
/// compile in, the session that captures what they bound, and the scope.
pub(in crate::resolver::server) struct Prepared {
    sessions: Sessions,
    pub(super) scratch_catalog: CatalogFacts,
    pub(super) map: scope::Principals,
    /// Authorization keys the reproduction did not reproduce.
    pub(super) differences: Vec<String>,
    /// What the externals file and the fill ahead of the managed objects
    /// created (#1673, #1778).
    created: externals::Created,
    /// What fill sends once the managed tables exist, what it filled, and
    /// what it could not (#1778).
    staging: draft::Staging,
    /// What fill created ahead of the managed objects, roots and members,
    /// as `pg_describe_object` names them: a difference from the target in
    /// one of them is named as fill's.
    filled_objects: BTreeSet<String>,
    /// The role a run-owned layout gives USAGE on a schema the externals file
    /// created; `None` where the scratch account compiles as itself.
    grantee: Option<pbps_model::RoleName>,
}

impl Scratch<'_> {
    /// The layout for a superuser on another cluster: a
    /// run-owned login and database from `template0` with the target's
    /// recipe, the deployer's authorization reproduced in it, and a session
    /// as the reproduced deployer.
    async fn run_owned(
        &mut self,
        admin: &mut Conn,
        target_catalog: &CatalogFacts,
        authorization: &scope::Authorization,
        scope_schemas: &[String],
        fill: &draft::Fill,
    ) -> Result<Prepared, Error> {
        let driver = self.driver;
        let request = self.request;
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let recipe = scope::recipe(driver, target_catalog)
            .map_err(|error| Error::Scope(error.to_string()))?;
        let names = super::super::names_for(&self.token)?;
        let login = names.login().to_owned();
        let map = scope::Principals::generate(authorization, &request.planned, &login, &self.token);
        // Recorded before anything is created, so a failure part-way still
        // drops what was made.
        self.created = Created::RunOwned {
            names: names.clone(),
            roles: map.server_wide_names(),
        };
        pbps_pg::resolver::create_scratch(admin, &names, &recipe)
            .await
            .map_err(|_| Error::Scratch)?;
        let mut owner = Conn::connect_with(driver, self.scratch, names.database(), None)
            .await
            .map_err(db("the run-owned scratch database"))?;
        // A new connection reloads the login's default role, which the
        // first one dropped: provisioning is the login's own (#1678 review).
        owner
            .execute("SET ROLE NONE")
            .await
            .map_err(db("the run-owned scratch session's role"))?;
        refuse_foreign_objects(&mut owner).await?;
        // The run-owned database compiles under the same declared state as a
        // supplied one: settings, comments, limit and grants (#1708). The
        // limit waits for the run login's session: this superuser session
        // counts toward a database's limit though it is not held to it, so a
        // declared limit of 1 would refuse that session (measured on 16 and
        // 18). It then goes on alone, since the reproduction builds on this
        // state in between.
        put_into_standard(
            &mut owner,
            &Standard {
                connection_limit: -1,
                ..self.standard.clone()
            },
        )
        .await?;
        scope::prepare(
            &mut owner,
            &map,
            authorization,
            &request.planned,
            names.database(),
            names.login(),
        )
        .await
        .map_err(db("the deployer's reproduced authorization"))?;
        // After the reproduction, which drops and recreates each in-scope
        // schema, and before any managed object (DEC-1673.1). Its own
        // session, so its `SET`s reach nothing after it. Then what fill
        // stages ahead of the managed objects, in the same session
        // (DEC-1778.2).
        let before = inventory(&mut owner).await?;
        let mut setup = Conn::connect_with(driver, self.scratch, names.database(), None)
            .await
            .map_err(db("the externals file's session"))?;
        setup
            .execute("SET ROLE NONE")
            .await
            .map_err(db("the externals file session's role"))?;
        let setup_state = SessionState::new(None);
        if let Some(statements) = self.externals {
            run_externals(
                &mut setup,
                &setup_state,
                statements,
                self.provenance.outside_writes,
            )
            .await?;
        }
        let mid = inventory(&mut owner).await?;
        let staging = fill.staging(&identities(&externals::created(&before, &mid)));
        run_fill(
            &mut setup,
            &setup_state,
            &staging.ahead,
            self.provenance.outside_writes,
        )
        .await?;
        drop(setup);
        let after = inventory(&mut owner).await?;
        let created = externals::created(&before, &after);
        let filled_objects = descriptions(&externals::created(&mid, &after));
        // Opened after the reproduction, so the login defaults it stored
        // load into this session as they load on the target's.
        let mut session = Conn::connect_with(
            driver,
            self.scratch,
            names.database(),
            Some((names.login(), names.password())),
        )
        .await
        .map_err(db("the run login's scratch session"))?;
        standard::put_connection_limit(&mut owner, names.database(), self.standard)
            .await
            .map_err(db("the scratch database's connection limit"))?;
        let deployer = map
            .deployer(authorization)
            .map_err(|reason| Error::Scope(reason.into()))?;
        let grantee = Some(deployer.clone().unwrap_or_else(|| names.login().into()));
        scope::enter(&mut session, driver, deployer.as_ref())
            .await
            .map_err(db("the reproduced deployer"))?;
        let differences = scope::settle(
            &mut session,
            &map,
            authorization,
            &request.planned,
            scope_schemas,
        )
        .await
        .map_err(db("the reproduced authorization"))?;
        let scratch_catalog = scope::read_catalog(
            &mut session,
            driver,
            &request.schemas,
            &request.write_path_extras,
        )
        .await
        .map_err(db("the scratch analysis scope"))?;
        Ok(Prepared {
            sessions: Sessions::RunOwned {
                compile: Session::new(session, deployer),
                capture: Box::new(Session::new(owner, None)),
            },
            scratch_catalog,
            map,
            differences,
            created,
            staging,
            filled_objects,
            grantee,
        })
    }

    /// The database the scratch connection names, for any account that is
    /// not a superuser: it must own that database and find it
    /// empty, and it compiles there as itself, with the deployer's defaults.
    async fn supplied(
        &mut self,
        admin: &mut Conn,
        authorization: &scope::Authorization,
        scope_schemas: &[String],
        fill: &draft::Fill,
    ) -> Result<Prepared, Error> {
        let driver = self.driver;
        let request = self.request;
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        // The checks read on the backend the separation was read on, in one
        // transaction, so a pooler cannot answer them from elsewhere.
        admin
            .execute("BEGIN")
            .await
            .map_err(db("the scratch checks"))?;
        let checked = self.supplied_checks(admin).await;
        let ended = admin.execute("ROLLBACK").await;
        let (principal, database) = checked?;
        ended.map_err(db("the scratch checks"))?;
        // Repairs what an interrupted run left, or refuses naming it, before
        // anything is compiled; the snapshots below are then of the
        // standard database (#1708).
        put_into_standard(admin, self.standard).await?;
        admin
            .execute("BEGIN")
            .await
            .map_err(db("the scratch checks"))?;
        let reached = self.reach_checks(admin, &principal.login).await;
        let ended = admin.execute("ROLLBACK").await;
        reached?;
        ended.map_err(db("the scratch checks"))?;
        let defaults = sql::login_defaults(admin)
            .await
            .map_err(db("the scratch account's own defaults"))?;
        let acl = standard::read_state(admin)
            .await
            .map_err(db("the scratch database's privileges"))?
            .acl;
        let scope::Authorization::Postgres(context) = authorization else {
            return Err(Error::Scope(
                "the resolver is implemented for PostgreSQL only".into(),
            ));
        };
        let map =
            scope::Principals::supplied(authorization, &principal.login).map_err(Error::Scope)?;
        // The first object the run creates here, and its record: a run killed
        // after it is found and cleaned by the next one (`recovery`, #2410).
        let claimed = record::claim(
            admin,
            &record::Claim::new(&self.token, defaults.clone(), acl.clone()),
        )
        .await;
        self.unlock_database(admin).await;
        claimed.map_err(|error| match error {
            record::ClaimError::Taken => Error::Unusable(
                "another run claimed the scratch database first; wait for it to finish, or \
                 use another scratch database"
                    .into(),
            ),
            record::ClaimError::Db(error) => {
                Error::Read(format!("the scratch database's claim: {error}"))
            }
        })?;
        // Every write, the cleanup included, goes through the connection the
        // checks were made on. Another connection from the same string need
        // not reach the same server: a name may resolve to several hosts or a
        // balancing proxy, and `DROP OWNED` there would empty something else
        // (#1678 review).
        self.created = Created::Supplied {
            database,
            defaults,
            acl,
        };
        sql::create_schemas(admin, scope_schemas)
            .await
            .map_err(db("the in-scope schemas"))?;
        // After the in-scope schemas exist and before the deployer loses
        // any usage, on the checked connection like every write here
        // (DEC-1673.1). `DISCARD ALL` then keeps its `SET`s from the
        // compile that follows on this same session.
        let mut state = SessionState::checked(self.backend.clone(), None);
        let before = inventory(admin).await?;
        if let Some(statements) = self.externals {
            run_externals(admin, &state, statements, self.provenance.outside_writes).await?;
            // An allowed externals file may change the login's own defaults
            // (#1674). Those are kept, so the release compares the
            // compile's changes with what the externals file left.
            if self.provenance.outside_writes == OutsideWrites::Allowed
                && let Created::Supplied { defaults, .. } = &mut self.created
            {
                *defaults = sql::login_defaults(admin)
                    .await
                    .map_err(db("the scratch account's own defaults"))?;
            }
            pinned(admin, &self.backend).await?;
        }
        // What fill stages ahead of the managed objects, on the same checked
        // connection (DEC-1778.2).
        let mid = inventory(admin).await?;
        let staging = fill.staging(&identities(&externals::created(&before, &mid)));
        run_fill(
            admin,
            &state,
            &staging.ahead,
            self.provenance.outside_writes,
        )
        .await?;
        pinned(admin, &self.backend).await?;
        let after = inventory(admin).await?;
        let created = externals::created(&before, &after);
        let filled_objects = descriptions(&externals::created(&mid, &after));
        let unusable = authorization
            .unusable_schemas(&request.planned)
            .into_iter()
            .filter(|schema| scope_schemas.contains(schema))
            .collect::<Vec<_>>();
        let usable = sql::revoke_usage(admin, &unusable)
            .await
            .map_err(db("the schemas the deployer cannot use"))?;
        // Kept on the login's path while it leaves the deployer's, such a
        // schema would make every comparison a mismatch; name what keeps it
        // instead (#1678 review). What was given up is granted back at
        // release, which puts public back into its standard state.
        if !usable.is_empty() {
            return Err(Error::Unusable(format!(
                "the scratch account keeps USAGE on what the plan takes from the deployer ({}); \
                 remove those privileges from the scratch account",
                usable.join("; ")
            )));
        }
        state
            .install_settings(admin, context)
            .await
            .map_err(db("the deployer's settings"))?;
        let scratch_catalog =
            scope::read_catalog(admin, driver, &request.schemas, &request.write_path_extras)
                .await
                .map_err(db("the scratch analysis scope"))?;
        // A pooler that moved the connection since the checks is caught here,
        // before anything compiles; the cleanup checks again on its own.
        pinned(admin, &self.backend).await?;
        Ok(Prepared {
            sessions: Sessions::Supplied(state),
            scratch_catalog,
            map,
            differences: Vec::new(),
            created,
            staging,
            filled_objects,
            grantee: None,
        })
    }

    /// What the supplied layout requires of the scratch account and its
    /// database, before the run's first write there.
    async fn supplied_checks(
        &self,
        admin: &mut Conn,
    ) -> Result<(pbps_db::resolver::environment::DeploymentPrincipal, String), Error> {
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        pinned(admin, &self.backend).await?;
        let principal = pbps_pg::resolver::authorization::principal(admin)
            .await
            .map_err(db("the scratch account"))?;
        if !sql::owns_database(admin)
            .await
            .map_err(db("the scratch database's owner"))?
        {
            return Err(Error::Unusable(format!(
                "the scratch account {} does not own the database its connection names; \
                 run ALTER DATABASE ... OWNER TO {0}, since the run empties it with DROP OWNED",
                principal.login
            )));
        }
        refuse_foreign_objects(admin).await?;
        let database = current_database(admin).await?;
        Ok((principal, database))
    }

    /// What `DROP OWNED` would reach beyond the run's own objects, read once
    /// the database is standard: before, a `public` an interrupted run left
    /// owned by the login would be counted, which the standard hands back
    /// to `pg_database_owner` (#1708).
    async fn reach_checks(&self, admin: &mut Conn, login: &str) -> Result<(), Error> {
        pinned(admin, &self.backend).await?;
        let reach = sql::cleanup_reach(admin).await.map_err(|error| {
            Error::Read(format!(
                "what the scratch account's cleanup would reach: {error}"
            ))
        })?;
        if reach.is_empty() {
            return Ok(());
        }
        Err(Error::Unusable(format!(
            "the scratch account {login} has objects or privileges that emptying its database \
             with DROP OWNED would also drop or revoke ({}); use an account with none outside \
             that database",
            described_list(&reach)
        )))
    }

    /// How a rehearsal on this scratch names the plan's roles.
    pub(super) fn naming(&self) -> Naming {
        Naming::of(&self.provenance.scratch, self.placement)
    }

    /// Makes the roles a rehearsal needs under `naming`, for the run only.
    /// `Existing` makes none. `Created` makes those the cluster lacks. `Mapped`
    /// makes all, under names of the run's own. What is made is recorded first,
    /// in what the release drops and, in a supplied database, in the claim a
    /// later run finds the run dead by, so a failure or a kill part-way still
    /// leaves it removable. `Ok(Err(why))` is an account that may not create
    /// roles: not a failure of the run, but of what the rehearsal can say about
    /// authorization.
    pub(super) async fn rehearsal_roles(
        &mut self,
        admin: &mut Conn,
        roles: &[pbps_model::RoleName],
        naming: Naming,
    ) -> Result<Result<(), String>, Error> {
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let lacking: Vec<pbps_model::RoleName>;
        let roles = match naming {
            Naming::Existing => return Ok(Ok(())),
            Naming::Mapped => roles,
            // The bootstrap superuser, the engine's own roles and whatever the
            // externals file made are there already, and are not the run's.
            Naming::Created => {
                lacking = pbps_pg::roles::missing_roles(admin, &roles.iter().collect())
                    .await
                    .map_err(db("the roles the scratch cluster has"))?;
                &lacking
            }
        };
        if roles.is_empty() {
            return Ok(Ok(()));
        }
        pinned(admin, &self.backend).await?;
        if !pbps_pg::resolver::may_create_roles(admin)
            .await
            .map_err(db("the scratch account's attributes"))?
        {
            return Ok(Err(
                "the scratch account may not create roles: it has neither SUPERUSER nor \
                 CREATEROLE"
                    .into(),
            ));
        }
        match &mut self.created {
            Created::RunOwned { roles: made, .. } => made.extend(roles.iter().cloned()),
            Created::Supplied { .. } => {
                let names: Vec<_> = roles.iter().map(|role| role.raw().to_owned()).collect();
                record::record_roles(admin, &names)
                    .await
                    .map_err(db("the scratch database's claim"))?;
            }
            Created::Nothing => return Err(Error::Scratch),
        }
        match pbps_pg::resolver::create_rehearsal_roles(admin, roles).await {
            Ok(()) => Ok(Ok(())),
            Err(error) if error.server_error_code().as_deref() == Some("42501") => {
                Ok(Err(format!(
                    "the scratch account may not create roles ({error}); it needs CREATEROLE, \
                     or the superuser a run-owned scratch has"
                )))
            }
            Err(error) => Err(db("the rehearsal's roles")(error)),
        }
    }

    /// Drops what the run created. `Err` names exactly what may remain.
    pub(super) async fn release(&mut self, admin: &mut Conn) -> Result<(), Vec<String>> {
        match std::mem::replace(&mut self.created, Created::Nothing) {
            Created::Nothing => Ok(()),
            Created::RunOwned { names, roles } => {
                let remaining = release_run_owned(admin, &names, &roles).await;
                if remaining.is_empty() {
                    Ok(())
                } else {
                    Err(remaining)
                }
            }
            Created::Supplied {
                database,
                defaults,
                acl,
            } => {
                release_supplied(
                    admin,
                    &self.backend,
                    self.standard,
                    &database,
                    &defaults,
                    &acl,
                )
                .await
            }
        }
    }
}

/// Drops a run-owned run's database, roles and login, in that order, and names
/// what is left. The login is the record: it stays while anything else does.
pub(in crate::resolver::server) async fn release_run_owned(
    admin: &mut Conn,
    names: &ScratchNames,
    roles: &[pbps_model::RoleName],
) -> Vec<String> {
    let mut remaining = Vec::new();
    if pbps_pg::resolver::drop_database(admin, names.database(), true)
        .await
        .is_err()
    {
        remaining.push(format!("database {}", names.database()));
    }
    remaining.extend(
        pbps_pg::resolver::drop_roles(admin, roles)
            .await
            .into_iter()
            .map(|role| format!("role {}", role.shown())),
    );
    if !remaining.is_empty()
        || pbps_pg::resolver::drop_login(admin, names.login())
            .await
            .is_err()
    {
        remaining.push(format!("role {}", names.login()));
    }
    remaining
}

pub(in crate::resolver::server) use release::supplied as release_supplied;

/// Puts the scratch database into `standard`, or refuses naming what the
/// scratch account cannot apply or cannot put back, before anything is
/// compiled. Whatever would stop it refuses before the first write: the
/// whole declared state is first tried in a transaction that is rolled
/// back, so the engine, not a model of it, judges each declared value.
async fn put_into_standard(conn: &mut Conn, declared: &Standard) -> Result<(), Error> {
    let db =
        |what: &'static str| move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"));
    let unappliable = standard::unappliable(conn, declared)
        .await
        .map_err(db("the scratch database's declared standard"))?;
    if !unappliable.is_empty() {
        return Err(Error::Unusable(format!(
            "the scratch account cannot apply the resolver's declared standard: {}",
            unappliable.join("; ")
        )));
    }
    // Already standard, as a release leaves it: no statement, no trial.
    if standard::verify(conn, declared, None)
        .await
        .map_err(db("the scratch database's standard state"))?
        .is_empty()
    {
        return Ok(());
    }
    let refused = standard::trial(conn, declared)
        .await
        .map_err(db("the scratch database's standard state"))?;
    if !refused.is_empty() {
        return Err(Error::Unusable(format!(
            "the scratch database cannot be put into its declared standard state, so nothing \
             was written ({}); its operator must repair the database or the declaration",
            refused.join("; ")
        )));
    }
    let failed = standard::enforce(conn, declared, None)
        .await
        .map_err(db("the scratch database's standard state"))?;
    let left = standard::verify(conn, declared, None)
        .await
        .map_err(db("the scratch database's standard state"))?;
    if left.is_empty() {
        return Ok(());
    }
    Err(Error::Unusable(format!(
        "the scratch database is not in its standard state and the scratch account cannot \
         put it there ({}){}; its operator must repair it",
        left.join("; "),
        if failed.is_empty() {
            String::new()
        } else {
            format!(", because {}", failed.join("; "))
        }
    )))
}

/// The resolver entry's declared standard over the built-in one: what
/// `CREATE DATABASE ... TEMPLATE template0` makes (#1708).
pub fn declared_standard(declared: Option<&pbps_config::resolver::ScratchStandard>) -> Standard {
    let mut standard = Standard::default();
    let Some(declared) = declared else {
        return standard;
    };
    standard.settings = declared
        .settings
        .iter()
        .map(|(name, value)| (name.to_lowercase(), value.0.clone()))
        .collect();
    // The engine removes a comment given as the empty string, so a declared
    // `""` is read back as no comment and declares exactly that (measured on
    // 16 and 18, for the database and for `public`).
    let comment = |comment: &String| Some(comment.clone()).filter(|c| !c.is_empty());
    if let Some(declared) = &declared.comment {
        standard.comment = comment(declared);
    }
    if let Some(limit) = declared.connection_limit {
        standard.connection_limit = limit;
    }
    if let Some(public) = &declared.public {
        if let Some(declared) = &public.comment {
            standard.public_comment = comment(declared);
        }
        standard.public_grants = public
            .grants
            .iter()
            .flat_map(|grant| {
                grant
                    .privileges
                    .iter()
                    .map(|privilege| (grant.to.clone(), privilege.as_str().to_owned()))
            })
            .collect();
    }
    standard
}

/// Refuses a scratch database holding anything initdb did not create: a
/// leftover, a polluted template, or someone else's work. Compiling over it
/// would bind to whatever it is.
impl Scratch<'_> {
    /// The OIDs on the target of the routines `schema` declares, each found
    /// by its declared signature under the path its declaration compiles
    /// with.
    async fn routine_oids(
        &self,
        target: &mut Conn,
        schema: &pbps_model::Schema,
    ) -> Result<BTreeSet<u32>, Error> {
        let declared = schema
            .modules
            .iter()
            .filter(|(id, _)| matches!(id, pbps_model::ModuleId::Routine(_)))
            .map(|(id, module)| (id.clone(), module.kind))
            .collect();
        let signatures: Vec<_> = dropped_signatures(&self.request.write_path_extras, declared)
            .map_err(Error::Binding)?
            .into_iter()
            .filter_map(|(_, signature)| signature)
            .collect();
        externals::routine_oids(target, &signatures)
            .await
            .map_err(|error| Error::Read(format!("the target's managed routines: {error}")))
    }

    /// Compares what the externals file created with the target, and reproduces
    /// the target deployer's USAGE on each schema it created (#1673).
    pub(super) async fn compare_externals(
        &self,
        provisioning: Provisioning,
        prepared: &mut Prepared,
        admin: &mut Conn,
        target: &mut Conn,
        binding: &Binding,
    ) -> Result<Staged, Error> {
        let managed = &[&binding.base_managed, &binding.desired_managed];
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let capture = prepared.sessions.capture(admin);
        let staged = pbps_pg::resolver::capture::compare_staged(
            capture,
            target,
            &prepared.created,
            managed,
            &prepared.filled_objects,
        )
        .await
        .map_err(|error| match error {
            StageError::Capture(error) => super::catalog_read_failed(error),
            StageError::Refused(findings) => Error::Externals(findings),
        })?;
        // Every schema the externals file created, an extension's member too
        // (`pg_cron`'s `cron`): a member is no root, but the deployer's
        // calls into it need USAGE all the same.
        let schemas = externals::schema_names(capture, &prepared.created.schemas)
            .await
            .map_err(db("the schemas the externals file created"))?;
        if schemas.is_empty() {
            return Ok(staged);
        }
        // The deployer's answer, read on the target as the deployer.
        let wanted = externals::usage(target, &schemas).await.map_err(db(
            "the target deployer's usage of the externals file's schemas",
        ))?;
        match (provisioning, &prepared.grantee, &mut prepared.sessions) {
            (Provisioning::RunOwned, Some(grantee), Sessions::RunOwned { capture: owner, .. }) => {
                externals::reproduce_usage(&mut owner.conn, grantee, &wanted)
                    .await
                    .map_err(db("the deployer's usage of the externals file's schemas"))?;
            }
            (Provisioning::Supplied, None, Sessions::Supplied(_)) => {
                // The account owns what its externals file created, so it already
                // has USAGE; it gives up what the deployer lacks.
                let unusable: Vec<String> = wanted
                    .iter()
                    .filter(|(_, usable)| !**usable)
                    .map(|(schema, _)| schema.clone())
                    .collect();
                let kept = sql::revoke_usage(admin, &unusable)
                    .await
                    .map_err(db("the externals file's schemas the deployer cannot use"))?;
                if !kept.is_empty() {
                    return Err(Error::Unusable(format!(
                        "the scratch account keeps USAGE on a schema the target's deployer \
                         cannot use ({}); remove those privileges from the scratch account",
                        kept.join("; ")
                    )));
                }
            }
            _ => {
                return Err(Error::Scope(
                    "the scratch layout and its sessions disagree".into(),
                ));
            }
        }
        let compile = prepared.sessions.compiling(admin).0;
        let reproduced = externals::usage(compile, &schemas)
            .await
            .map_err(db("the reproduced usage of the externals file's schemas"))?;
        let differences: Vec<String> = wanted
            .iter()
            .filter(|(schema, usable)| reproduced.get(*schema) != Some(usable))
            .map(|(schema, usable)| {
                format!(
                    "the target's deployer {} USAGE on schema {schema}, and the reproduction \
                     does not",
                    if *usable { "has" } else { "lacks" }
                )
            })
            .collect();
        if differences.is_empty() {
            Ok(staged)
        } else {
            Err(Error::Externals(differences))
        }
    }
}

/// A compile that failed writing through a view the externals file staged names
/// the remedy: a shape view reads like the target's, but no statement can
/// write through it, and a routine's body is checked against it when it is
/// created (measured on 18: `cannot insert into view`, SQLSTATE 55000).
pub(super) fn through_shape_view(error: Error, staged: &Staged) -> Error {
    if let Error::Creation(mut failure) = error {
        let original = failure.report.to_string();
        if let Error::Binding(reason) = through_shape_view(Error::Binding(original.clone()), staged)
            && let Some(note) = reason.strip_prefix(&original)
            && !note.is_empty()
        {
            std::sync::Arc::make_mut(&mut failure)
                .context
                .push(note.to_owned());
        }
        return Error::Creation(failure);
    }
    let Error::Binding(reason) = error else {
        return error;
    };
    let written = ["insert into", "update", "delete from", "merge into"]
        .iter()
        .find_map(|verb| {
            let (_, rest) = reason.split_once(&format!("cannot {verb} view \""))?;
            rest.split_once('"').map(|(view, _)| view.to_owned())
        });
    let staged_view = written.and_then(|view| {
        staged
            .compared()
            .iter()
            .find(|root| root.class == "pg_class" && root.name.last() == Some(&view))
            .map(|root| root.name.join("."))
    });
    match staged_view {
        Some(view) => Error::Binding(format!(
            "{reason}; the externals file stages {view} as a view no statement can write through: \
             stage its real definition in the externals file instead, with what it reads"
        )),
        None => Error::Binding(reason),
    }
}

/// What `created` holds, by catalog and description, as fill tells the
/// externals file's objects from its own.
fn identities(created: &externals::Created) -> BTreeSet<(String, String)> {
    created
        .roots
        .iter()
        .chain(&created.members)
        .map(|object| (object.catalog.clone(), object.described.clone()))
        .collect()
}

/// The roots and extension members in `created`, as `pg_describe_object`
/// names them.
fn descriptions(created: &externals::Created) -> BTreeSet<String> {
    created
        .roots
        .iter()
        .chain(&created.members)
        .map(|object| object.described.clone())
        .collect()
}

/// The engine owns the reset recipe; lifecycle failures still use the run's
/// ordinary cleanup path and preserve backend-replacement refusals.
async fn reset_and_restore(
    session: &mut Conn,
    state: &SessionState,
    reset: Reset,
    which: &str,
) -> Result<(), Error> {
    #[cfg(test)]
    if for_tests::restore_fails(session) {
        return Err(Error::Read("injected session restoration failure".into()));
    }
    pbps_pg::resolver::session::reset_and_restore(session, state, reset)
        .await
        .map_err(|error| match error {
            pbps_pg::resolver::session::RestoreError::MovedBackend => {
                Error::Unusable(error.to_string())
            }
            pbps_pg::resolver::session::RestoreError::Database(error) => {
                Error::Read(format!("{which}'s settings: {error}"))
            }
        })
}

/// Sends fill's statements on `session` in one `REPEATABLE READ`
/// transaction, under the empty path every pbps session pins (#1778). pbps
/// renders them from the target's catalog, so a refusal is the target's
/// shape not reproducing here, and is named as fill's. They are held to
/// the externals file's outside-write rule (#1783): an event trigger the
/// file installed fires on fill's statements too, and what it changes
/// outside the scratch database is rolled back and refused like the file's
/// own.
async fn run_fill(
    session: &mut Conn,
    state: &SessionState,
    sql: &str,
    outside_writes: OutsideWrites,
) -> Result<(), Error> {
    if sql.is_empty() {
        return Ok(());
    }
    let checked = outside_writes == OutsideWrites::Refused;
    let read =
        |what: &'static str| move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"));
    session
        .execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .map_err(read("the fill's transaction"))?;
    let outcome = async {
        // Both reads render under the same settings: a chained fill runs on
        // a session the compile pinned to the target's zone, which the
        // second read's reset drops, and a role's `VALID UNTIL` would then
        // read as changed.
        if checked {
            reset_and_restore(
                session,
                &state.observation(),
                Reset::All,
                "the fill session",
            )
            .await?;
        }
        let before = if checked {
            externals::shared_state(session)
                .await
                .map_err(read("the cluster's shared state"))?
        } else {
            externals::SharedState::new()
        };
        session
            .execute("SELECT pg_catalog.set_config('search_path', '', true)")
            .await
            .map_err(read("the fill session's path"))?;
        externals::run(session, sql).await.map_err(|failed| {
            Error::Externals(vec![format!(
                "filling what the externals file lacks from the target failed: the engine \
                 refused it: {}; write the object it names in the externals file",
                failed.error
            )])
        })?;
        if !checked {
            return Ok(());
        }
        reset_and_restore(
            session,
            &state.observation(),
            Reset::All,
            "the fill session",
        )
        .await?;
        let after = externals::shared_state(session)
            .await
            .map_err(read("the cluster's shared state"))?;
        let changed = externals::shared_changes(&before, &after);
        if changed.is_empty() {
            Ok(())
        } else {
            Err(Error::Externals(
                changed
                    .into_iter()
                    .map(|name| {
                        format!(
                            "filling what the externals file lacks changed {name}, outside the \
                             scratch database: something the externals file created, such as \
                             an event trigger, ran on fill's statements; an externals file \
                             creates only objects in the database, unless the resolver sets \
                             `outside_writes: allow`",
                            name = Escaped(&name)
                        )
                    })
                    .collect(),
            ))
        }
    }
    .await;
    if let Err(error) = outcome {
        let _ = session.execute("ROLLBACK").await;
        return Err(error);
    }
    session
        .execute("COMMIT")
        .await
        .map_err(read("the fill's transaction"))?;
    // A `SET` an event trigger made on fill's statements outlasts the
    // commit, as the externals file's own do, and what runs next on this
    // session must not inherit it; the role is left as it is.
    reset_and_restore(session, state, Reset::All, "the fill session").await
}

/// Every object in the scratch database initdb did not create.
async fn inventory(conn: &mut Conn) -> Result<externals::Inventory, Error> {
    externals::inventory(conn)
        .await
        .map_err(|error| Error::Read(format!("the scratch database's objects: {error}")))
}

/// Runs the externals file on `session` as one script (#1769), and refuses with
/// the engine's error when the engine refuses it. Runs the externals file inside one `REPEATABLE READ` transaction pbps opens,
/// and refuses it, rolled back so it leaves nothing, when it changed the
/// cluster outside the scratch database (#1783). One snapshot serves both
/// reads of the shared state, so another session's writes during the
/// script are in neither. The script cannot end the transaction itself:
/// [`externals::run`] refuses transaction control before it runs.
/// Under [`OutsideWrites::Allowed`] the shared state is not read: what the
/// file changes outside the database is kept, and is the operator's (#1674).
async fn run_externals(
    session: &mut Conn,
    state: &SessionState,
    sql: &str,
    outside_writes: OutsideWrites,
) -> Result<(), Error> {
    let checked = outside_writes == OutsideWrites::Refused;
    let read =
        |what: &'static str| move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"));
    session
        .execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .map_err(read("the externals file's transaction"))?;
    let outcome = async {
        if checked {
            reset_and_restore(session, &state.observation(), Reset::All, "the externals file session").await?;
        }
        let before = if checked {
            externals::shared_state(session)
                .await
                .map_err(read("the cluster's shared state"))?
        } else {
            externals::SharedState::new()
        };
        if let Err(failed) = externals::run(session, sql).await {
            let mut finding = format!("the engine refused it: {}", failed.error);
            if failed.no_schema() {
                finding.push_str(
                    "; it runs on an empty search_path, as every pbps session does, so name each \
                     object's schema, or set the path in the file as pg_dump output does",
                );
            }
            if failed.unsupported() {
                finding.push_str(
                    "; if it controls its transaction, leave out BEGIN, COMMIT and ROLLBACK: \
                     pbps runs the whole externals file as one transaction",
                );
            }
            if failed.names_something_missing() {
                finding.push_str(
                    "; the externals file runs before any managed object, so a statement that names \
                     one fails: write such an object as a shape view (a view of typed NULLs \
                     that returns no row), or leave it out of the externals file, and fill \
                     stages it once the managed tables exist",
                );
            }
            return Err(Error::Externals(vec![finding]));
        }
        #[cfg(test)]
        for_tests::after_externals(session, sql);
        // Under the settings the first read had, not the script's: its
        // `SET`s last on this session until `DISCARD ALL`, and a time zone
        // renders a role's expiry differently, a timeout can cancel the
        // read. Every setting is reset rather than the ones known to matter,
        // so one no one listed cannot refuse a valid externals file. The path is
        // pinned again, empty as every pbps session's (DEC-1564.1), since a
        // type the script creates may be named `text`: for the session, not
        // the transaction, as what runs after the commit relies on it too.
        reset_and_restore(session, &state.observation(), Reset::All, "the externals file session").await?;
        if !checked {
            return record::rebase(session)
                .await
                .map_err(read("the scratch database's claim"));
        }
        let after = externals::shared_state(session)
            .await
            .map_err(read("the cluster's shared state"))?;
        let changed = externals::shared_changes(&before, &after);
        if changed.is_empty() {
            Ok(())
        } else {
            Err(Error::Externals(
                changed
                    .into_iter()
                    .map(|name| {
                        format!(
                            "it changed {name}, outside the scratch database; an externals file \
                             creates only objects in the database, unless the resolver \
                             sets `outside_writes: allow`",
                            name = Escaped(&name)
                        )
                    })
                    .collect(),
            ))
        }
    }
    .await;
    if let Err(error) = outcome {
        let _ = session.execute("ROLLBACK").await;
        return Err(error);
    }
    session
        .execute("COMMIT")
        .await
        .map_err(read("the externals file's transaction"))?;
    reset_and_restore(session, state, Reset::Discard, "the externals file session").await
}

// The readers also serve raw comparisons. Only the descriptions entering a
// sentence are shown; the separators remain tool prose (DEC-2084.1).
fn described_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| Escaped(name).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

async fn refuse_foreign_objects(conn: &mut Conn) -> Result<(), Error> {
    let (named, total) = sql::foreign_objects(conn)
        .await
        .map_err(|error| Error::Read(format!("the scratch database's contents: {error}")))?;
    if total == 0 {
        return Ok(());
    }
    Err(Error::Unusable(format!(
        "the scratch database is not empty: {total} object(s) initdb did not create, including {}",
        described_list(&named)
    )))
}

async fn current_database(conn: &mut Conn) -> Result<String, Error> {
    let rows = conn
        .query("SELECT pg_catalog.current_database()::text AS name")
        .await
        .map_err(|error| Error::Read(format!("the scratch database's name: {error}")))?;
    rows.first()
        .and_then(|row| row.try_get::<&str>("name").ok().flatten())
        .map(str::to_owned)
        .ok_or_else(|| Error::Read("the scratch database's name".into()))
}

/// A keyed fingerprint of what one engine reports about its build: its
/// version number and build string, and the extensions installed, at their
/// versions. The measured
/// profiles fingerprint executable content here; nothing reads executables
/// on this resolver, and the field says only what was reported.
pub(super) fn reported_build(
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    catalog: &CatalogFacts,
) -> Result<String, Error> {
    let reported = |name: &str| {
        catalog
            .observations
            .get(name)
            .and_then(|observed| observed.value())
            .ok_or_else(|| Error::Scope(format!("{name} was not reported")))
    };
    let version = (reported("server_version_num")?, reported("server_version")?);
    let mut extensions: Vec<(&str, &str)> = catalog
        .extensions
        .iter()
        .map(|extension| (extension.name.as_str(), extension.version.as_str()))
        .collect();
    extensions.sort_unstable();
    let bytes = serde_json::to_vec(&(version, extensions))
        .map_err(|_| Error::Scope("the reported build cannot be encoded".into()))?;
    Ok(fingerprint(
        key,
        "pbps/pg-reported-build/v1",
        "reported",
        &bytes,
    ))
}

use pbps_pg::resolver::capture::{CaptureScope, CompiledCapture};
use pbps_pg::resolver::reconstruct::Reconstruction;

/// Each routine the plan drops, with its declared signature spelled as the
/// plan's `DROP` spells it, for the target capture to identify (#1148).
pub(super) fn dropped_signatures(
    extras: &[String],
    routines: Vec<(pbps_model::ModuleId, pbps_model::ModuleKind)>,
) -> Result<
    Vec<(
        pbps_model::ModuleId,
        Option<pbps_pg::resolver::capture::DroppedSignature>,
    )>,
    String,
> {
    let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
    routines
        .into_iter()
        .map(|(id, kind)| {
            pbps_pg::resolver::reconstruct::dropped_signature(&dialect, &id, kind)
                .map(|signature| (id, signature))
                .map_err(|error| error.to_string())
        })
        .collect()
}

/// The desired namespace's scratch statements, for the no-Fill live oracle.
#[cfg(test)]
pub(in crate::resolver::server) fn reconstruction(
    driver: Driver,
    extras: &[String],
    bootstrap: &pbps_diff::order::DiffPlan,
    ids: &pbps_model::IdsFile,
    dependencies: &pbps_model::ModuleDeps,
    version: i64,
) -> Result<Reconstruction, String> {
    match driver {
        Driver::Postgres => Reconstruction::with_dependencies(
            &pbps_pg::Postgres::with_write_path_extras(extras.to_vec())
                .with_server_version(version),
            bootstrap,
            ids,
            dependencies,
        )
        .map_err(|error| error.to_string()),
        Driver::Mssql => Err(super::NO_BINDING_ADAPTER.into()),
    }
}

/// Read once for both scratch expansion and the sealed producer's plan.
pub(in crate::resolver::server) async fn generation_version(
    target: &mut Conn,
) -> Result<i64, String> {
    pbps_pg::roles::server_version_num(target)
        .await
        .map_err(|error| error.to_string())
}

/// What a failed scratch compilation is. A declaration that did not compile
/// is the binding's verdict; a catalog read around it, a transaction that
/// would not open or commit, or a statement the connection or server
/// interrupted, answered nothing (SPEC 9.8, #1575, #1597).
pub(super) fn compile_failed(error: pbps_pg::resolver::reconstruct::ReconstructError) -> Error {
    use pbps_pg::resolver::reconstruct::ReconstructError;
    match error {
        ReconstructError::Refused(report) => Error::Creation(std::sync::Arc::new(
            super::super::CreationFailure::new(report),
        )),
        ReconstructError::Read { .. }
        | ReconstructError::Interrupted { .. }
        | ReconstructError::Transaction(_) => Error::Read(error.to_string()),
        ReconstructError::Zone { .. } => Error::Scope(error.to_string()),
        ReconstructError::Unsupported(_)
        | ReconstructError::Emit { .. }
        | ReconstructError::Compile { .. }
        | ReconstructError::Cycle(_) => Error::Binding(error.to_string()),
    }
}

/// Retain the final scratch read with its key and UID-qualified roots until
/// final typed planning decides the one closing seal. Only logical binding
/// records and the existing verdict leave this private capability.
#[allow(clippy::too_many_arguments)]
pub(super) async fn capture_desired_for_plan(
    connection: &mut impl pbps_db::transport::QueryConnection,
    base: &Managed,
    desired: &Managed,
    paths: &Paths,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    principals: &crate::resolver::scope::Principals,
    recorded: pbps_diff::Side<'_>,
    reconstruction: &Reconstruction,
    namespaces: &std::collections::BTreeSet<String>,
    dropped_signatures: &std::collections::BTreeSet<pbps_pg::resolver::capture::DroppedSignature>,
    staged: &std::collections::BTreeSet<pbps_db::resolver::capture::ObjectIdentity>,
) -> Result<(CompiledCapture, CaptureScope), Error> {
    use pbps_pg::resolver::capture;
    // The principal map names the engine: only PostgreSQL's has an adapter.
    match principals {
        crate::resolver::scope::Principals::Postgres(roles) => {
            let first = capture::capture(connection, &capture::managed_scope(desired))
                .await
                .map_err(super::catalog_read_failed)?;
            // What an externals file staged is read and sealed on both sides, so
            // the recheck compares the target's complete fingerprints of it
            // (#1673).
            let mut scope = capture::scope(&first, &[base, desired], paths);
            scope.retained.extend(staged.iter().cloned());
            let routines = recorded
                .schema
                .modules
                .keys()
                .filter_map(|id| {
                    reconstruction
                        .created(id)
                        .map(|object| (id.clone(), object.clone()))
                })
                .collect();
            let dropped = Default::default();
            let ownership = capture::RecordedOwnership {
                schema: recorded.schema,
                ids: recorded.ids,
                routines: &routines,
                dropped: &dropped,
                namespaces,
            };
            // The same lookups the opening sealed: the closing manifest records
            // what each dropped signature names after the plan, normally none.
            let captured = capture::capture_identifying_for_plan(
                connection,
                &scope,
                dropped_signatures,
                key,
                roles,
                &ownership,
            )
            .await
            .map_err(super::catalog_read_failed)?;
            Ok((captured, scope))
        }
        crate::resolver::scope::Principals::Mssql(_) => {
            Err(Error::Binding(super::NO_BINDING_ADAPTER.into()))
        }
    }
}

/// Each in-scope schema's effective path as the qualified scope measured it,
/// with the configured extras for any schema it did not. An unreadable
/// measurement is left out, which falls back to the whole write path.
pub(super) fn paths(
    extras: &[String],
    visibility: &std::collections::BTreeMap<String, pbps_db::resolver::Observation>,
) -> Paths {
    let effective = visibility
        .iter()
        .filter_map(|(schema, observed)| {
            let path: Vec<String> = serde_json::from_str(observed.value()?).ok()?;
            Some((schema.clone(), path))
        })
        .collect();
    Paths::new(extras.to_vec(), effective)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scratch<'a>(
    driver: Driver,
    target: &mut Conn,
    admin: &mut Conn,
    tokens: &Tokens,
    planned_identity: &str,
    scratch: &'a str,
    request: &'a ScopeRequest,
    standard: &'a Standard,
    externals: Option<&'a str>,
    provenance: &'a Provenance,
) -> Result<(Provisioning, Scratch<'a>), ProduceError> {
    let (placement, account, backend) = separation(target, admin, tokens, planned_identity)
        .await
        .map_err(ProduceError::Run)?;
    let provisioning = match placement {
        Placement::Target => {
            return Err(unusable(
                "the scratch connection is the target's own database; name a database on another cluster, or a confined account's own database",
            ));
        }
        Placement::SameCluster if !account.confined() => {
            return Err(unusable(format!(
                "the scratch account shares the target's cluster and is not confined to its database \
                 ({}); remove them from it, or use a scratch server on another cluster",
                account.excess().join(", ")
            )));
        }
        Placement::SeparateCluster if account.provisions() => Provisioning::RunOwned,
        // Never a superuser here: one provisions on another cluster and is
        // refused on the target's, so `DROP OWNED` never runs as one, where
        // it would reach every object initdb made (DEC-1672.1).
        Placement::SeparateCluster | Placement::SameCluster => Provisioning::Supplied,
    };
    Ok((
        provisioning,
        Scratch {
            driver,
            scratch,
            request,
            backend,
            standard,
            externals,
            provenance,
            token: tokens.scratch.clone(),
            created: Created::Nothing,
            holds_lock: false,
            placement,
        },
    ))
}

fn unusable(reason: impl Into<String>) -> ProduceError {
    ProduceError::Run(Error::Unusable(reason.into()))
}

impl Scratch<'_> {
    pub(super) async fn prepare(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        target_catalog: &CatalogFacts,
        authorization: &scope::Authorization,
        scope_schemas: &[String],
        fill: &draft::Fill,
    ) -> Result<Prepared, Error> {
        match provisioning {
            Provisioning::RunOwned => {
                self.sweep_cluster(admin).await;
                self.run_owned(admin, target_catalog, authorization, scope_schemas, fill)
                    .await
            }
            Provisioning::Supplied => {
                let prepared = async {
                    self.recover_supplied(admin).await?;
                    self.supplied(admin, authorization, scope_schemas, fill)
                        .await
                }
                .await;
                // Released after the claim; this covers an error before it.
                self.unlock_database(admin).await;
                prepared
            }
        }
    }
}

/// The private reconstruction and the target links it is compared against.
pub(in crate::resolver::server) struct Binding {
    base_managed: Managed,
    desired_managed: Managed,
    links: Vec<externals::Link>,
    routines: externals::ManagedRoutines,
    reconstruction: Reconstruction,
    // Reconstruction and final planning must use the same measured capability.
    generation_version: i64,
}

/// The fixed-key closing capture and the exact scope its opening read uses.
pub(in crate::resolver::server) struct Closing {
    captured: CompiledCapture,
    scope: CaptureScope,
    paths: Paths,
    dropped: Vec<(pbps_model::ModuleId, Option<capture::DroppedSignature>)>,
    signatures: BTreeSet<capture::DroppedSignature>,
    namespaces: BTreeSet<String>,
}

impl Scratch<'_> {
    pub(super) async fn reconstruction(
        &self,
        target: &mut Conn,
        binding: &BindingRequest<'_>,
        ids: &pbps_model::IdsFile,
        dependencies: &pbps_model::ModuleDeps,
        prepared: &mut Prepared,
    ) -> Result<Binding, Error> {
        let base_managed = Managed::from_schema(binding.base);
        let desired_managed = Managed::from_schema(binding.desired);
        // Read before anything compiles: a chain through the boundary fails
        // the compile on an object no externals file can stage, and that failure
        // is then named as the chain (SPEC §9.3.2). Only a failure refuses:
        // the target's dependencies are the current declarations', and a
        // desired one may no longer read through the chain (#1754 review).
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let links = externals::links(target)
            .await
            .map_err(db("the target's recorded dependencies"))?;
        let kept = self.routine_oids(target, binding.desired).await?;
        let mut any = self.routine_oids(target, binding.base).await?;
        any.extend(kept.iter().copied());
        let routines = externals::ManagedRoutines { desired: kept, any };
        let extras = &self.request.write_path_extras;
        let generation_version = generation_version(target).await.map_err(Error::Binding)?;
        let reconstruction = Reconstruction::with_fills(
            &pbps_pg::Postgres::with_write_path_extras(extras.to_vec())
                .with_server_version(generation_version),
            binding.bootstrap,
            ids,
            dependencies,
            std::mem::take(&mut prepared.staging.steps),
        )
        .map_err(compile_failed)?;
        for name in reconstruction.omitted_fills() {
            prepared.staging.filled.remove(name);
            prepared.staging.unfillable.push(format!("{} is not filled: its shape names a managed relation the scratch compile does not create", Escaped(name)));
        }
        Ok(Binding {
            base_managed,
            desired_managed,
            links,
            routines,
            reconstruction,
            generation_version,
        })
    }
}

impl Prepared {
    pub(super) fn supplied(&self) -> bool {
        matches!(&self.sessions, Sessions::Supplied(_))
    }

    pub(super) fn session<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn {
        self.sessions.compiling(admin).0
    }

    pub(super) fn compiling<'a>(
        &'a mut self,
        admin: &'a mut Conn,
        zone: Option<&pbps_model::SessionZone>,
    ) -> Result<&'a mut Conn, Error> {
        let zone = zone.ok_or_else(|| {
            Error::Scope("the target reported no session time zone to compile in".into())
        })?;
        let (conn, state) = self.sessions.compiling(admin);
        state.compiling(zone);
        Ok(conn)
    }

    pub(super) fn has_filled(&self) -> bool {
        !self.staging.filled.is_empty()
    }
    pub(super) fn unfillable(&self) -> Vec<String> {
        self.staging.unfillable.clone()
    }
}

impl Binding {
    pub(super) fn has_fill(&self) -> bool {
        self.reconstruction.has_fill()
    }
    pub(super) fn chains(&mut self, prepared: &Prepared) -> Vec<String> {
        // A chain through a filled object is lifted: fill created it once
        // the managed relations it names existed.
        let links: Vec<_> = std::mem::take(&mut self.links)
            .into_iter()
            .filter(|link| !prepared.staging.filled.contains(&link.middle.described))
            .collect();
        externals::chains(
            &links,
            &self.desired_managed,
            &self.base_managed,
            &self.routines,
        )
    }

    /// Walk the one program, borrowing the supplied session sequentially.
    /// Managed trials cannot cross the compiler-issued Fill boundaries.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compile(
        &mut self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        target: &mut Conn,
        extras: &[String],
        zone: Option<&pbps_model::SessionZone>,
        outside_writes: OutsideWrites,
        staged: &mut Staged,
    ) -> Result<(), Error> {
        let zone = zone.ok_or_else(|| {
            Error::Scope("the target reported no session time zone to compile in".into())
        })?;
        let pg = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
        // Inventory is creation provenance, not another placement authority.
        // Compare after all work: later setup must not stale an earlier proof.
        let mut created = externals::Created::default();
        while self.reconstruction.observations().is_none() {
            if let Some(fill) = self.reconstruction.next_fill() {
                let (setup, state) = prepared.sessions.setup(admin);
                #[cfg(test)]
                if let Ok(Some((name, sql))) =
                    crate::resolver::server::scratch::FILL_FAILURE2134.try_with(Clone::clone)
                    && name == fill.name()
                {
                    setup
                        .execute(&sql)
                        .await
                        .map_err(|e| Error::Read(format!("the injected fill setup: {e}")))?;
                }
                let before = inventory(setup).await?;
                run_fill(setup, state, fill.sql(), outside_writes).await?;
                let after = inventory(setup).await?;
                let delta = externals::created(&before, &after);
                self.reconstruction
                    .finish_fill(&before, &after)
                    .map_err(compile_failed)?;
                created.roots.extend(delta.roots);
                created.members.extend(delta.members);
                created.schemas.extend(delta.schemas);
            } else {
                let compile = prepared.compiling(admin, Some(zone))?;
                if let Err(error) = self.reconstruction.compile_next(&pg, zone, compile).await {
                    let mut error = compile_failed(error);
                    if let Some(waiting) = self.reconstruction.waiting() {
                        match &mut error {
                            Error::Creation(failure) => {
                                let failure = std::sync::Arc::make_mut(failure);
                                failure.externals = true;
                                failure.context.push(waiting);
                            }
                            Error::Binding(reason) => {
                                error = Error::Externals(vec![waiting, reason.clone()]);
                            }
                            Error::Scratch
                            | Error::Scope(_)
                            | Error::Incompatible(_)
                            | Error::Read(_)
                            | Error::Unusable(_)
                            | Error::Externals(_)
                            | Error::Changed(_) => {}
                        }
                    }
                    return Err(error);
                }
            }
        }
        if !created.roots.is_empty() || !created.members.is_empty() {
            staged.extend(
                capture::compare_staged(
                    prepared.sessions.capture(admin),
                    target,
                    &created,
                    &[&self.base_managed, &self.desired_managed],
                    &descriptions(&created),
                )
                .await
                .map_err(stage_failed)?,
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn capture_desired(
        &self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        extras: &[String],
        visibility: &BTreeMap<String, pbps_db::resolver::Observation>,
        schemas: &[String],
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'_>,
        staged: &Staged,
    ) -> Result<Closing, Error> {
        if self.reconstruction.observations().is_none() {
            return Err(Error::Binding(
                "scratch compilation has no complete committed observations".into(),
            ));
        }
        let capture = prepared.sessions.capture(admin);
        let dropped =
            dropped_signatures(extras, self.base_managed.dropped_by(&self.desired_managed))
                .map_err(Error::Binding)?;
        let signatures: BTreeSet<_> = dropped.iter().filter_map(|(_, s)| s.clone()).collect();
        let paths = paths(extras, visibility);
        let namespaces: BTreeSet<String> = schemas.iter().chain(extras).cloned().collect();
        let (compiled, capture_scope) = capture_desired_for_plan(
            capture,
            &self.base_managed,
            &self.desired_managed,
            &paths,
            key,
            &prepared.map,
            desired,
            &self.reconstruction,
            &namespaces,
            &signatures,
            staged.compared(),
        )
        .await?;
        Ok(Closing {
            captured: compiled,
            scope: capture_scope,
            paths,
            dropped,
            signatures,
            namespaces,
        })
    }

    pub(super) async fn capture_opening(
        &self,
        target: &mut Conn,
        closing: &Closing,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        base: pbps_diff::Side<'_>,
    ) -> Result<(capture::CapturedInputs, pbps_model::resolver::InputManifest), Error> {
        let routines = base
            .schema
            .modules
            .keys()
            .filter_map(|id| {
                self.reconstruction
                    .created(id)
                    .map(|object| (id.clone(), object.clone()))
            })
            .collect();
        let dropped_roots = closing
            .dropped
            .iter()
            .filter_map(|(id, signature)| {
                signature.clone().map(|signature| (id.clone(), signature))
            })
            .collect();
        let ownership = RecordedOwnership {
            schema: base.schema,
            ids: base.ids,
            routines: &routines,
            dropped: &dropped_roots,
            namespaces: &closing.namespaces,
        };
        pbps_pg::resolver::capture::capture_identifying_qualified(
            target,
            &closing.scope,
            &closing.signatures,
            key,
            None,
            &ownership,
        )
        .await
        .map_err(super::catalog_read_failed)
    }

    pub(super) fn assess(
        &mut self,
        closing: &Closing,
        current: &capture::CapturedInputs,
        staged: &Staged,
    ) -> pbps_db::resolver::capture::Assessment {
        let identified = current.dropped();
        self.reconstruction.identified(
            closing
                .dropped
                .iter()
                .cloned()
                .map(|(id, signature)| {
                    let identity = signature.and_then(|s| identified.get(&s).cloned().flatten());
                    (id, identity)
                })
                .collect(),
        );
        closing.captured.assess_staged(
            current,
            &self.base_managed,
            &closing.paths,
            &self.reconstruction,
            staged,
        )
    }
}

impl Closing {
    pub(super) fn planning_records(
        &self,
    ) -> Result<Vec<capture::BindingRecord>, pbps_model::resolver::ManifestError> {
        self.captured.planning_records()
    }
    pub(super) fn seal_for_plan(
        self,
    ) -> Result<pbps_model::resolver::InputManifest, capture::SealError> {
        self.captured.seal_for_plan()
    }
}

pub(super) fn stage_failed(error: StageError) -> Error {
    match error {
        StageError::Capture(error) => super::catalog_read_failed(error),
        StageError::Refused(findings) => Error::Externals(findings),
    }
}

pub(super) async fn fill(
    target: &mut Conn,
    binding: &BindingRequest<'_>,
    extras: &[String],
) -> Result<draft::Fill, Error> {
    draft::fill(target, binding.desired, binding.base, extras)
        .await
        .map_err(|error| match error {
            draft::Error::Declaration(reason) => Error::Binding(reason),
            error @ (draft::Error::AbsentSchema(_) | draft::Error::Read { .. }) => {
                Error::Read(format!("the externals to fill: {error}"))
            }
        })
}

pub(super) fn ordinary(
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &pbps_model::Hints,
    extras: &[String],
) -> Result<pbps_diff::order::DiffPlan, Error> {
    let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
    pbps_diff::resolver::ordinary(base, desired, &dialect, hints).map_err(|errors| {
        Error::Binding(format!("ordinary typed plan cannot be formed: {errors:?}"))
    })
}

impl Binding {
    pub(super) fn plan(
        &self,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        hints: &pbps_model::Hints,
        extras: &[String],
        surfaces: &[pbps_model::resolver::SurfaceResolution],
    ) -> Result<pbps_diff::resolver::Ordered<pbps_diff::order::Canonical<pbps_pg::Postgres>>, Error>
    {
        let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec())
            .with_server_version(self.generation_version);
        pbps_diff::resolver::plan(base, desired, hints, surfaces, &dialect)
            .map_err(|error| Error::Binding(error.to_string()))
    }
}

/// Makes the scratch session the run's keeper (#2410).
pub(super) async fn keep(admin: &mut Conn) -> Result<(), ProduceError> {
    sql::keep(admin)
        .await
        .map_err(|error| unusable(format!("the scratch session's keepalive settings: {error}")))
}

pub(super) async fn reset_role(admin: &mut Conn) -> Result<(), ProduceError> {
    // The scratch account compiles as itself. A role its defaults or its
    // connection options set would own what the run creates, out of reach
    // of the cleanup's `DROP OWNED BY SESSION_USER`, and would hide the
    // session's own backend timings (#1678 review).
    admin
        .execute("SET ROLE NONE")
        .await
        .map_err(|error| unusable(format!("the scratch session's role: {error}")))?;
    Ok(())
}

pub(super) async fn target_identity(target: &mut Conn) -> Result<String, pbps_db::DbError> {
    sql::cluster_identity(target).await
}

pub(super) async fn recapture(
    target: &mut Conn,
    current: &capture::CapturedInputs,
) -> Result<
    Vec<pbps_db::resolver::capture::CaptureDifference>,
    pbps_db::resolver::capture::CaptureError,
> {
    capture::recapture(target, current)
        .await
        .map(|(_, differences)| differences)
}

#[cfg(test)]
mod naming_tests {
    use super::{Naming, Placement};
    use pbps_model::resolver::ScratchSource;

    /// One strategy for each placement: the target's own cluster has the roles,
    /// a container is the run's own cluster, and any other cluster is shared.
    #[test]
    fn the_naming_follows_the_placement_of_the_scratch() {
        let server = ScratchSource::Server {};
        let docker = ScratchSource::Docker {
            image: "postgres:18".into(),
        };
        assert_eq!(
            Naming::of(&server, Placement::SameCluster),
            Naming::Existing
        );
        assert_eq!(
            Naming::of(&server, Placement::SeparateCluster),
            Naming::Mapped
        );
        // A container keeps its real names however its cluster is placed.
        for placement in [Placement::SeparateCluster, Placement::SameCluster] {
            assert_eq!(Naming::of(&docker, placement), Naming::Created);
        }
        // Negative: a server on another cluster is never given the real names,
        // which other runs and other tenants may hold.
        assert_ne!(
            Naming::of(&server, Placement::SeparateCluster),
            Naming::Created
        );
        assert_ne!(
            Naming::of(&server, Placement::SeparateCluster),
            Naming::Existing
        );
    }
}

#[cfg(test)]
mod declared_standard_tests {
    use super::declared_standard;

    /// The engine stores `TimeZone` under its own spelling whatever the
    /// declaration wrote, and the scratch's state is read keyed by
    /// lowercase name: a declared name is keyed the same way, or a declared
    /// `TimeZone` never reads back as declared.
    #[test]
    fn a_declared_setting_name_is_keyed_as_the_scratch_state_is_read() {
        let config = pbps_config::Config::parse(
            "dialect: postgres\nresolvers:\n  s:\n    kind: server\n    url_env: S\n    \
             standard: {settings: {TimeZone: UTC, work_mem: 64}}\n",
            std::path::Path::new("pbps.yml"),
        )
        .unwrap();
        let pbps_config::resolver::ResolverProfile::Server {
            standard: Some(declared),
            ..
        } = &config.resolvers["s"]
        else {
            panic!("a server profile with a standard");
        };
        let standard = declared_standard(Some(declared));
        assert_eq!(
            standard.settings.keys().collect::<Vec<_>>(),
            ["timezone", "work_mem"]
        );
        // Negative: nothing declared is the built-in standard, with no
        // settings of its own.
        assert!(declared_standard(None).settings.is_empty());
    }

    fn declared(standard: &str) -> pbps_pg::resolver::standard::Standard {
        let config = pbps_config::Config::parse(
            &format!(
                "dialect: postgres\nresolvers:\n  s:\n    kind: server\n    url_env: S\n    \
                 standard: {standard}\n"
            ),
            std::path::Path::new("pbps.yml"),
        )
        .unwrap();
        let pbps_config::resolver::ResolverProfile::Server {
            standard: Some(declared),
            ..
        } = &config.resolvers["s"]
        else {
            panic!("a server profile with a standard");
        };
        declared_standard(Some(declared))
    }

    /// `COMMENT ... IS ''` removes the comment, so the state read back has
    /// none: a declared empty comment is declared as none, or every run is
    /// refused for a comment the engine will never store (#1708 review).
    #[test]
    fn a_declared_empty_comment_is_declared_as_none() {
        let standard = declared(r#"{comment: "", public: {comment: ""}}"#);
        assert_eq!(standard.comment, None);
        assert_eq!(standard.public_comment, None);
        // Negative: a comment with text is kept as written.
        let standard = declared("{comment: ours, public: {comment: theirs}}");
        assert_eq!(standard.comment.as_deref(), Some("ours"));
        assert_eq!(standard.public_comment.as_deref(), Some("theirs"));
    }
}
