//! The operator-vouched resolver on a server the operator supplies
//! (DEC-1528.1; #1667, #1672).
//!
//! The operator vouches for the scratch server: its isolation, the
//! confidentiality of what is compiled on it, and the channels to it. pbps
//! observes no process, proves no containment and reads no executable, so
//! the target may be anywhere its connection string reaches. What the run
//! still establishes, before it writes anything on scratch, is that it can
//! touch nothing that is not its own:
//!
//! - the scratch database is never the target's, read from the engines and
//!   not from the two strings, which can spell one server two ways;
//! - on the target's own cluster, the scratch account must be confined to
//!   its database: no superuser, no `CREATEROLE`, no `CREATEDB`;
//! - the database it compiles in holds nothing initdb did not create.
//!
//! The account decides how the run provisions scratch. A superuser on another
//! cluster gets the measured run's layout: a
//! run-owned login and database, the deployer's authorization reproduced, the
//! declarations compiled as the reproduced deployer, all dropped afterwards.
//! Any other account compiles as itself in the database its connection names,
//! which it must own, and leaves it empty again with `DROP OWNED`. The rest is
//! the measured run's analysis, with the compatibility rule that compares
//! what each engine reports instead of what it executes.

use super::producer::{Identity, Sealing, fingerprint, request, seal};
use super::{BindingRequest, Error, ProduceError, ResolvedPlan, engine, resolution, scope};
use pbps_db::resolver::ScratchNames;
use pbps_db::resolver::environment::{CatalogFacts, Verdict};
use pbps_db::{Conn, Driver};
use pbps_model::Hints;
use pbps_model::resolver::ResolverRuntime;
use pbps_pg::resolver::vouched::{self as sql, Placement};
use std::collections::BTreeSet;

/// One run of the operator-vouched resolver for a connected plan. `scratch`
/// is the connection string the selected resolver's `url_env` holds. The run
/// releases everything it created before any result leaves, and a cleanup it
/// could not confirm names exactly what may remain.
#[allow(clippy::too_many_arguments)]
pub async fn produce(
    driver: Driver,
    scratch: &str,
    target_connection: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    write_path_extras: &[String],
    project: &pbps_config::Project,
    environment: Option<&str>,
) -> Result<ResolvedPlan, ProduceError> {
    if driver != Driver::Postgres {
        return Err(ProduceError::Run(Error::Binding(
            "the operator-vouched resolver is implemented for PostgreSQL only".into(),
        )));
    }
    if binding.base != base.schema || binding.desired != desired.schema {
        return Err(ProduceError::Run(Error::Binding(
            "the binding request and recorded planning sides disagree".into(),
        )));
    }
    let key = crate::resolver::sealing::environment_key(project, environment)
        .map_err(|error| ProduceError::Run(Error::Binding(error.to_string())))?;
    let request = request(base, desired, hints, write_path_extras).map_err(ProduceError::Run)?;
    let mut target = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| vouched(format!("the target could not be connected: {error}")))?;
    let mut admin = Conn::connect(driver, scratch)
        .await
        .map_err(|error| ProduceError::Acquire(format!("the scratch server: {error}")))?;
    // The scratch account compiles as itself. A role its defaults or its
    // connection options set would own what the run creates, out of reach
    // of the cleanup's `DROP OWNED BY SESSION_USER`, and would hide the
    // session's own backend timings (#1678 review).
    admin
        .execute("SET ROLE NONE")
        .await
        .map_err(|error| vouched(format!("the scratch session's role: {error}")))?;
    let tokens = Tokens::generate();
    let (placement, account, backend) = separation(&mut target, &mut admin, &tokens)
        .await
        .map_err(ProduceError::Run)?;
    let provisioning = match placement {
        Placement::Target => {
            return Err(vouched(
                "the scratch connection is the target's own database; name a database on another cluster, or a confined account's own database",
            ));
        }
        Placement::SameCluster if !account.confined() => {
            return Err(vouched(format!(
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
    let mut run = Run {
        driver,
        scratch,
        key: &key,
        request: &request,
        tokens: &tokens,
        backend: &backend,
        created: Created::Nothing,
    };
    let result = run
        .resolve(
            provisioning,
            &mut target,
            &mut admin,
            binding,
            base,
            desired,
            hints,
        )
        .await;
    let cleanup = run.release(&mut admin).await;
    match (result, cleanup) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (Err(refused), Ok(())) => Err(ProduceError::Run(refused)),
        (Ok(plan), Ok(())) => Ok(plan),
    }
}

fn vouched(reason: impl Into<String>) -> ProduceError {
    ProduceError::Run(Error::Vouched(reason.into()))
}

/// The run's two session tokens: run-generated, so a token found in
/// `pg_stat_activity` is this run's session and no other.
struct Tokens {
    target: String,
    scratch: String,
}

impl Tokens {
    fn generate() -> Self {
        let token = || format!("{:032x}", rand::random::<u128>());
        Self {
            target: token(),
            scratch: token(),
        }
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
        let account = sql::account(scratch).await?;
        let backend = sql::backend(scratch).await?;
        Ok::<_, pbps_db::DbError>((target_mark, scratch_mark, seen, account, backend))
    }
    .await;
    // Nothing was written in either transaction; rolling back only ends it.
    let ended = (
        target.execute("ROLLBACK").await,
        scratch.execute("ROLLBACK").await,
    );
    let (target_mark, scratch_mark, seen, account, backend) = observed.map_err(failed)?;
    ended.0.map_err(failed)?;
    ended.1.map_err(failed)?;
    let placement = sql::placement(
        (&tokens.target, target_mark),
        (&tokens.scratch, scratch_mark),
        &seen,
    )
    .map_err(|reason| Error::Vouched(reason.into()))?;
    Ok((placement, account, backend))
}

/// Refuses unless `conn`'s current transaction runs on the backend the run
/// checked: the scratch connection must keep one backend, as a direct or a
/// session-pooled one does (#1678 review).
async fn pinned(conn: &mut Conn, checked: &sql::Backend) -> Result<(), Error> {
    match sql::on_backend(conn, checked).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Vouched(
            "the scratch connection reached another server backend than the one the run \
             checked; connect to the scratch server directly or through session pooling, \
             not transaction pooling"
                .into(),
        )),
        Err(error) => Err(Error::Read(format!("the scratch backend: {error}"))),
    }
}

/// How the run provisions its scratch database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provisioning {
    /// A run-owned login and database, created and dropped by the run.
    RunOwned,
    /// The database the connection names, owned by the scratch account.
    Supplied,
}

/// What the run has created and must release.
enum Created {
    Nothing,
    RunOwned {
        names: ScratchNames,
        roles: Vec<String>,
    },
    /// Objects the scratch account owns in the supplied database.
    Supplied {
        database: String,
    },
}

struct Run<'a> {
    driver: Driver,
    scratch: &'a str,
    key: &'a pbps_db::fingerprint::EnvironmentFingerprintKey,
    request: &'a super::ScopeRequest,
    tokens: &'a Tokens,
    /// The scratch backend the separation was read on.
    backend: &'a sql::Backend,
    created: Created,
}

/// The scratch side, provisioned and qualified: the session the declarations
/// compile in, the session that captures what they bound, and the scope.
struct Prepared {
    /// `None` when the run compiles on the scratch connection its checks
    /// were made on.
    compile: Option<Conn>,
    /// `None` when captures read through the compiling session itself.
    capture: Option<Conn>,
    scratch_catalog: CatalogFacts,
    map: scope::Principals,
    /// Authorization keys the reproduction did not reproduce.
    differences: Vec<String>,
}

impl Run<'_> {
    #[allow(clippy::too_many_arguments)]
    async fn resolve(
        &mut self,
        provisioning: Provisioning,
        target: &mut Conn,
        admin: &mut Conn,
        binding: &BindingRequest<'_>,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        hints: &Hints,
    ) -> Result<ResolvedPlan, Error> {
        let driver = self.driver;
        let request = self.request;
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let scope_schemas =
            scope::scope_schemas(driver, &request.schemas, &request.write_path_extras)
                .map_err(Error::Scope)?;
        // The target as its own deployer sees it, in one snapshot.
        let (mut target_catalog, authorization) = scope::read_target(
            target,
            driver,
            &request.schemas,
            &request.write_path_extras,
            &scope_schemas,
            &request.planned,
        )
        .await
        .map_err(db("the target's analysis scope"))?;
        let ambiguous = authorization.unpredictable(&request.planned);
        if !ambiguous.is_empty() {
            return Err(Error::Scope(format!(
                "the grantor of a planned grant cannot be predicted on the target: {}",
                ambiguous.join("; ")
            )));
        }
        let opening_catalog = target_catalog.clone();
        let mut prepared = match provisioning {
            Provisioning::RunOwned => {
                self.run_owned(admin, &target_catalog, &authorization, &scope_schemas)
                    .await?
            }
            Provisioning::Supplied => self.supplied(admin, &authorization, &scope_schemas).await?,
        };
        // The expected visibility is the deployer's after the plan's own
        // grants, as the measured run compares it.
        authorization.project_visibility(
            &request.planned,
            &request.schemas,
            &request.write_path_extras,
            &mut target_catalog,
        );
        let mut report =
            scope::compare_reported(driver, &target_catalog, &prepared.scratch_catalog)
                .map_err(Error::Scope)?;
        for key in &prepared.differences {
            report.facts.insert(
                format!("authorization:{key}"),
                pbps_db::resolver::environment::FactStatus::Mismatch {
                    target: "the target deployer's authorization".into(),
                    resolver: "not reproduced on scratch".into(),
                },
            );
        }
        match report.verdict() {
            Verdict::Verified => {}
            Verdict::Mismatch(facts) => return Err(Error::Incompatible(facts)),
            Verdict::Unknown(facts) => {
                return Err(Error::Scope(format!(
                    "the analysis scope could not be established: {}",
                    facts.join("; ")
                )));
            }
        }
        let extras = &request.write_path_extras;
        let mut reconstruction =
            engine::reconstruction(driver, extras, binding.bootstrap).map_err(Error::Binding)?;
        let compile = match prepared.compile.as_mut() {
            Some(compile) => compile,
            None => &mut *admin,
        };
        engine::compile(&mut reconstruction, extras, driver, compile).await?;
        let base_managed = engine::Managed::from_schema(binding.base);
        let desired_managed = engine::Managed::from_schema(binding.desired);
        let dropped = engine::dropped_signatures(extras, base_managed.dropped_by(&desired_managed))
            .map_err(Error::Binding)?;
        let signatures: BTreeSet<_> = dropped.iter().filter_map(|(_, s)| s.clone()).collect();
        let paths = engine::paths(extras, &target_catalog.visibility);
        let namespaces: BTreeSet<String> = request
            .schemas
            .iter()
            .chain(&request.write_path_extras)
            .cloned()
            .collect();
        let capture = match (prepared.capture.as_mut(), prepared.compile.as_mut()) {
            (Some(capture), _) | (None, Some(capture)) => capture,
            (None, None) => &mut *admin,
        };
        let (compiled, capture_scope) = engine::capture_desired_for_plan(
            capture,
            &base_managed,
            &desired_managed,
            &paths,
            self.key,
            &prepared.map,
            desired,
            &reconstruction,
            &namespaces,
            &signatures,
        )
        .await?;
        let routines = base
            .schema
            .modules
            .keys()
            .filter_map(|id| {
                reconstruction
                    .created(id)
                    .map(|object| (id.clone(), object.clone()))
            })
            .collect();
        let dropped_roots = dropped
            .iter()
            .filter_map(|(id, signature)| {
                signature.clone().map(|signature| (id.clone(), signature))
            })
            .collect();
        let ownership = engine::RecordedOwnership {
            schema: base.schema,
            ids: base.ids,
            routines: &routines,
            dropped: &dropped_roots,
            namespaces: &namespaces,
        };
        let (current, opening) = pbps_pg::resolver::capture::capture_identifying_qualified(
            target,
            &capture_scope,
            &signatures,
            self.key,
            None,
            &ownership,
        )
        .await
        .map_err(engine::catalog_read_failed)?;
        let identified = current.dropped();
        reconstruction.identified(
            dropped
                .into_iter()
                .map(|(id, signature)| {
                    let identity = signature.and_then(|s| identified.get(&s).cloned().flatten());
                    (id, identity)
                })
                .collect(),
        );
        let assessment = compiled.assess(&current, &base_managed, &paths, &reconstruction);
        let records = compiled
            .planning_records()
            .map_err(|error| Error::Binding(error.to_string()))?;
        let surfaces = resolution::from_sides(
            base,
            desired,
            &resolution::records(&opening),
            &records,
            &assessment,
        )?;
        let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
        let ordered = pbps_diff::resolver::plan(base, desired, hints, &surfaces, &dialect)
            .map_err(|error| Error::Binding(error.to_string()))?;
        let identity = Identity {
            rule: pbps_pg::resolver::compatibility::REPORTED_RULE,
            resolver_build: reported_build(self.key, &prepared.scratch_catalog)?,
            channels: format!("vouched|{}|{}", self.tokens.target, self.tokens.scratch),
            runtime: ResolverRuntime::Vouched,
        };
        let producer = super::ProducerOutcome {
            compiled,
            opening_build: reported_build(self.key, &opening_catalog)?,
            surfaces,
        };
        seal(
            self.key,
            &Sealing {
                planned: &request.planned,
                opening_catalog: &opening_catalog,
                target_catalog: &target_catalog,
                scratch_catalog: &prepared.scratch_catalog,
                authorization: &authorization,
                schemas: &request.schemas,
                write_path_extras: &request.write_path_extras,
            },
            identity,
            producer,
            opening,
            ordered,
            base,
            desired,
        )
    }

    /// The measured run's layout for a superuser on another cluster: a
    /// run-owned login and database from `template0` with the target's
    /// recipe, the deployer's authorization reproduced in it, and a session
    /// as the reproduced deployer.
    async fn run_owned(
        &mut self,
        admin: &mut Conn,
        target_catalog: &CatalogFacts,
        authorization: &scope::Authorization,
        scope_schemas: &[String],
    ) -> Result<Prepared, Error> {
        let driver = self.driver;
        let request = self.request;
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let recipe = scope::recipe(driver, target_catalog)
            .map_err(|error| Error::Scope(error.to_string()))?;
        let names = super::generated_names()?;
        let login = names.login().to_owned();
        let token = login[login.len().saturating_sub(16)..].to_owned();
        let map = scope::Principals::generate(authorization, &request.planned, &login, &token);
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
        let deployer = map
            .deployer(authorization)
            .map_err(|reason| Error::Scope(reason.into()))?;
        scope::enter(&mut session, driver, deployer.as_deref())
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
            compile: Some(session),
            capture: Some(owner),
            scratch_catalog,
            map,
            differences,
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
        let scope::Authorization::Postgres(context) = authorization else {
            return Err(Error::Scope(
                "the operator-vouched resolver is implemented for PostgreSQL only".into(),
            ));
        };
        let map =
            scope::Principals::supplied(authorization, &principal.login).map_err(Error::Scope)?;
        // Every write, the cleanup included, goes through the connection the
        // checks were made on. Another connection from the same string need
        // not reach the same server: a name may resolve to several hosts or a
        // balancing proxy, and `DROP OWNED` there would empty something else
        // (#1678 review).
        self.created = Created::Supplied { database };
        sql::create_schemas(admin, scope_schemas)
            .await
            .map_err(db("the in-scope schemas"))?;
        pbps_pg::resolver::authorization::apply_session_settings(admin, context)
            .await
            .map_err(db("the deployer's settings"))?;
        let scratch_catalog =
            scope::read_catalog(admin, driver, &request.schemas, &request.write_path_extras)
                .await
                .map_err(db("the scratch analysis scope"))?;
        // A pooler that moved the connection since the checks is caught here,
        // before anything compiles; the cleanup checks again on its own.
        pinned(admin, self.backend).await?;
        Ok(Prepared {
            compile: None,
            capture: None,
            scratch_catalog,
            map,
            differences: Vec::new(),
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
        pinned(admin, self.backend).await?;
        let principal = pbps_pg::resolver::authorization::principal(admin)
            .await
            .map_err(db("the scratch account"))?;
        if !sql::owns_database(admin)
            .await
            .map_err(db("the scratch database's owner"))?
        {
            return Err(Error::Vouched(format!(
                "the scratch account {} does not own the database its connection names; \
                 run ALTER DATABASE ... OWNER TO {0}, since the run empties it with DROP OWNED",
                principal.login
            )));
        }
        refuse_foreign_objects(admin).await?;
        let reach = sql::cleanup_reach(admin)
            .await
            .map_err(db("what the scratch account's cleanup would reach"))?;
        if !reach.is_empty() {
            return Err(Error::Vouched(format!(
                "the scratch account {} has objects or privileges that emptying its database \
                 with DROP OWNED would also drop or revoke ({}); use an account with none \
                 outside that database",
                principal.login,
                reach.join(", ")
            )));
        }
        let database = current_database(admin).await?;
        Ok((principal, database))
    }

    /// Drops what the run created. `Err` names exactly what may remain.
    async fn release(&mut self, admin: &mut Conn) -> Result<(), Vec<String>> {
        match std::mem::replace(&mut self.created, Created::Nothing) {
            Created::Nothing => Ok(()),
            Created::RunOwned { names, roles } => {
                let mut remaining = Vec::new();
                if pbps_pg::resolver::drop_scratch(admin, &names)
                    .await
                    .is_err()
                {
                    remaining.push(format!("database {}", names.database()));
                    remaining.push(format!("role {}", names.login()));
                }
                remaining.extend(
                    pbps_pg::resolver::drop_roles(admin, &roles)
                        .await
                        .into_iter()
                        .map(|role| format!("role {role}")),
                );
                if remaining.is_empty() {
                    Ok(())
                } else {
                    Err(remaining)
                }
            }
            Created::Supplied { database } => {
                // The checked connection, never a new one, and only on the
                // checked backend: see `supplied`.
                sql::drop_owned(admin, self.backend).await.map_err(|error| {
                    vec![format!(
                        "every object the scratch account owns in database {database} ({error})"
                    )]
                })
            }
        }
    }
}

/// Refuses a scratch database holding anything initdb did not create: a
/// leftover, a polluted template, or someone else's work. Compiling over it
/// would bind to whatever it is.
async fn refuse_foreign_objects(conn: &mut Conn) -> Result<(), Error> {
    let (named, total) = sql::foreign_objects(conn)
        .await
        .map_err(|error| Error::Read(format!("the scratch database's contents: {error}")))?;
    if total == 0 {
        return Ok(());
    }
    Err(Error::Vouched(format!(
        "the scratch database is not empty: {total} object(s) initdb did not create, including {}",
        named.join(", ")
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
fn reported_build(
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

#[cfg(test)]
mod live_tests;
