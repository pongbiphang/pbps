//! SQL Server's scratch adapter: the shared runner owns the lifecycle, and
//! the dialect owns every statement and inventory (DEC-2236.1).

use super::super::scratch::{Provenance, Provisioning, Tokens};
use super::super::{BindingRequest, Error, ProduceError, ScopeRequest};
use crate::resolver::scope;
use pbps_db::resolver::ScratchNames;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_mssql::resolver::reconstruct::{ReconstructError, Reconstruction};
use pbps_mssql::resolver::{
    self, authorization as auth, capture, dependencies, externals, scratch as sql,
};
use sql::recovery as record;

mod chained;
mod recovery;
use std::collections::{BTreeMap, BTreeSet};

enum Created {
    Nothing,
    RunOwned {
        names: ScratchNames,
        reference: Option<ScratchNames>,
    },
    Supplied {
        database: sql::Database,
        sandbox: String,
    },
}

pub(in crate::resolver::server) struct Scratch<'a> {
    connection: &'a str,
    mark: sql::SessionMark,
    backend: sql::Backend,
    created: Created,
    entered: bool,
    // Staging futures borrow Scratch across awaits; only release takes the socket.
    visibility: Option<tokio::sync::Mutex<(Conn, sql::visibility::Visibility)>>,
    acquired: Option<sql::Database>,
    externals: Option<&'a str>,
    request: &'a ScopeRequest,
    // Preserve the selected source for #2270's evidence handoff.
    _provenance: &'a Provenance,
    placement: Option<sql::Placement>,
    companions: Vec<sql::companion::Companion>,
    /// The run's token: the marker table's, and in the name of everything the
    /// run creates that can carry one (#2410).
    token: String,
    /// The companion databases recorded in the scratch database so far.
    recorded: Vec<String>,
    /// Whether this run holds the application lock on the supplied database
    /// (`recovery::lock`): from before it reads what a dead run left until its
    /// own sandbox user exists.
    locked: bool,
    chained: Option<capture::ExternalInputs>,
    deferred: bool,
}

pub(in crate::resolver::server) struct Prepared {
    pub(in crate::resolver::server) dependencies: dependencies::Rehearsals,
    /// The reproduced deployer, entered from the sandbox: the declarations
    /// compile here and nowhere else. It cannot leave its context, so a read
    /// that needs more than the deployer may see opens [`Reach::open`].
    pub(in crate::resolver::server) compile: Conn,
    pub(in crate::resolver::server) map: scope::Principals,
    pub(in crate::resolver::server) inventory: externals::Inventory,
    pub(in crate::resolver::server) staged: capture::Staged,
    pub(super) scratch_catalog: CatalogFacts,
    pub(super) differences: Vec<String>,
    reach: Reach,
    chaining: Box<chained::Chaining>,
}

impl Prepared {
    pub(super) fn dependency_placements(
        &self,
    ) -> BTreeMap<pbps_model::dependency::DependencyName, pbps_model::resolver::DependencyPlacement>
    {
        use pbps_model::resolver::{DependencyPlacement, DependencyShape};
        self.dependencies
            .iter()
            .map(|(name, rehearsal)| {
                let placement = match rehearsal {
                    dependencies::Rehearsal::BoundAgainstRealDatabase => {
                        DependencyPlacement::Bound {}
                    }
                    dependencies::Rehearsal::CompanionRendered { source, .. } => {
                        DependencyPlacement::Companion {
                            shape: match source {
                                dependencies::ShapeSource::Catalog => DependencyShape::Compared,
                                dependencies::ShapeSource::Declaration => DependencyShape::Declared,
                            },
                        }
                    }
                };
                (name.clone(), placement)
            })
            .collect()
    }

    /// A copy of what reopens the scratch: `Prepared` holds the deployer's
    /// connection, so a borrow of it cannot be held across an await.
    pub(super) fn reach(&self) -> Reach {
        self.reach.clone()
    }
}

/// What reaches the scratch database again, bounded as the sandbox is:
/// every fact `Scratch::bounded` re-verifies on a fresh connection.
#[derive(Clone)]
pub(super) struct Reach {
    connection: String,
    mark: sql::SessionMark,
    acquired: Option<sql::Database>,
    database: String,
    sandbox: String,
    databases: Vec<String>,
}

impl Reach {
    async fn open(&self) -> Result<Conn, Error> {
        let mut conn = Conn::connect(Driver::Mssql, &self.connection)
            .await
            .map_err(read)?;
        if !sql::marked(&mut conn, &self.mark).await.map_err(read)? {
            return Err(Error::Unusable(
                "the staging session does not reach the marked instance".into(),
            ));
        }
        sql::use_database(&mut conn, &self.database)
            .await
            .map_err(read)?;
        if self.acquired.as_ref() != Some(&sql::database(&mut conn).await.map_err(read)?) {
            return Err(Error::Unusable(
                "the staging database changed after acquisition".into(),
            ));
        }
        sql::enter_sandbox(&mut conn, &self.sandbox)
            .await
            .map_err(read)?;
        sql::sandbox_checks(&mut conn, &self.databases, &self.database)
            .await
            .map_err(read)?;
        Ok(conn)
    }
}

pub(in crate::resolver::server) struct Acquired {
    pub(in crate::resolver::server) owner: Conn,
    database: String,
    sandbox: String,
    databases: Vec<String>,
}

/// No target identity or separation verdict is manufactured for a preview.
pub(in crate::resolver::server) async fn preview<'a>(
    admin: &mut Conn,
    connection: &'a str,
    request: &'a ScopeRequest,
    provenance: &'a Provenance,
) -> Result<(Provisioning, Scratch<'a>), Error> {
    let backend = sql::backend(admin).await.map_err(read)?;
    let token = format!("{:032x}", rand::random::<u128>());
    let mark = sql::mark(admin, &token).await.map_err(read)?;
    if !sql::marked(admin, &mark).await.map_err(read)? {
        return Err(Error::Unusable(
            "the scratch session cannot see its own live marker".into(),
        ));
    }
    let provisioning = if sql::provisions(admin).await.map_err(read)? {
        Provisioning::RunOwned
    } else {
        Provisioning::Supplied
    };
    let mut scratch = Scratch {
        connection,
        mark,
        backend,
        created: Created::Nothing,
        entered: false,
        visibility: None,
        acquired: None,
        externals: None,
        request,
        _provenance: provenance,
        placement: None,
        companions: Vec::new(),
        token,
        recorded: Vec::new(),
        locked: false,
        chained: None,
        deferred: false,
    };
    // Before the run creates anything: what runs killed earlier left.
    scratch.recover(provisioning, admin).await?;
    Ok((provisioning, scratch))
}

/// Resolves a LocalDB selector in the scratch connection string. Discovery
/// failures are the scratch server being unreachable, so they are the
/// acquire failure a connection error is.
pub(super) async fn endpoint(connection: &str) -> Result<String, ProduceError> {
    use pbps_mssql::resolver::localdb;
    match localdb::resolve(
        connection,
        localdb::helper().as_deref(),
        pbps_db::CONNECT_TIMEOUT,
    )
    .await
    {
        Ok(None) => Ok(connection.to_owned()),
        Ok(Some(found)) => Ok(found.connection().to_owned()),
        Err(failure) => Err(ProduceError::Acquire(format!(
            "the scratch server's LocalDB instance: {failure}"
        ))),
    }
}

fn read(error: pbps_db::DbError) -> Error {
    Error::Read(error.to_string())
}

/// A keyed fingerprint of what the engine reports about its build: product
/// version, level and update level. The rule compares the same facts
/// (`mssql-reported-scope-v2`); nothing reads an executable. An RTM build has
/// no update level, and that known absence is part of the fingerprint.
pub(super) fn reported_build(
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    catalog: &CatalogFacts,
) -> Result<String, Error> {
    let reported = |name: &str| {
        catalog
            .observations
            .get(name)
            .ok_or_else(|| Error::Scope(format!("{name} was not reported")))
    };
    let required = |name: &str| {
        reported(name)?
            .value()
            .ok_or_else(|| Error::Scope(format!("{name} was not reported")))
    };
    // An unreadable update level is not the RTM build's known absence.
    let update_level = match reported("product_update_level")? {
        pbps_db::resolver::Observation::Observed { value } => Some(value.as_str()),
        pbps_db::resolver::Observation::NotReported => None,
        pbps_db::resolver::Observation::Unknown { reason } => {
            return Err(Error::Scope(format!(
                "product_update_level could not be read: {reason}"
            )));
        }
    };
    let build = (
        required("product_version")?,
        required("product_level")?,
        update_level,
    );
    let bytes = serde_json::to_vec(&build)
        .map_err(|_| Error::Scope("the reported build cannot be encoded".into()))?;
    Ok(super::super::producer::fingerprint(
        key,
        "pbps/mssql-reported-build/v1",
        "reported",
        &bytes,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scratch<'a>(
    target: &mut Conn,
    admin: &mut Conn,
    tokens: &Tokens,
    planned_identity: &str,
    connection: &'a str,
    externals: Option<&'a str>,
    request: &'a ScopeRequest,
    provenance: &'a Provenance,
) -> Result<(Provisioning, Scratch<'a>), ProduceError> {
    let result = async {
        if provenance.same_instance.is_not_applicable() {
            return Err(Error::Unusable("SQL Server requires an explicit refused or allowed same_instance runtime value".into()));
        }
        let identity = sql::cluster_identity(target).await.map_err(read)?;
        if identity != planned_identity {
            return Err(Error::Unusable(
                "the target connection reached another master family_guid than the planning \
                 read"
                    .into(),
            ));
        }
        let target_database = sql::database(target).await.map_err(read)?;
        let backend = sql::backend(admin).await.map_err(read)?;
        let mark = sql::mark(admin, &tokens.scratch).await.map_err(read)?;
        if !sql::marked(admin, &mark).await.map_err(read)? {
            return Err(Error::Unusable(
                "the scratch session cannot see its own live marker".into(),
            ));
        }
        let seen = sql::marked(target, &mark).await.map_err(read)?;
        let placement = sql::placement(&target_database.id, &mark, seen);
        let provisioning = match placement {
            sql::Placement::Target => {
                return Err(Error::Unusable(
                    "the scratch database is the target database".into(),
                ));
            }
            sql::Placement::SameInstance if externals.is_some()
                && provenance.same_instance != pbps_model::resolver::SameInstance::Allowed => {
                return Err(Error::Binding(
                    "an externals file on the target's SQL Server instance requires `same_instance: allow` on the server resolver entry".into(),
                ));
            }
            sql::Placement::SameInstance => Provisioning::Supplied,
            sql::Placement::SeparateInstance => {
                if sql::provisions(admin).await.map_err(read)? {
                    Provisioning::RunOwned
                } else {
                    Provisioning::Supplied
                }
            }
        };
        let mut scratch = Scratch {
            connection,
            mark,
            backend,
            created: Created::Nothing,
            entered: false,
            visibility: None,
            acquired: None,
            externals,
            request,
            _provenance: provenance,
            placement: Some(placement),
            companions: Vec::new(),
            token: tokens.scratch.clone(),
            recorded: Vec::new(),
            locked: false,
        chained: None,
        deferred: false,
        };
        // After the placement verdict and before the run's first write: what
        // runs killed earlier left (#2410).
        scratch.recover(provisioning, admin).await?;
        Ok((provisioning, scratch))
    }
    .await;
    result.map_err(ProduceError::Run)
}

impl Scratch<'_> {
    /// Target-free acquisition, also used by preview rehearsal (DEC-587.1).
    pub(in crate::resolver::server) async fn acquire(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        recipe: &pbps_db::resolver::environment::DatabaseRecipe,
        dependency_inputs: Option<&mut dependencies::Inputs>,
    ) -> Result<Acquired, Error> {
        if !sql::on_backend(admin, &self.backend).await.map_err(read)? {
            return Err(Error::Unusable(
                "the scratch connection changed backend after it was marked".into(),
            ));
        }
        let (reference, database, databases) = match provisioning {
            Provisioning::RunOwned => {
                let names = super::super::names_for(&self.token)?;
                self.created = Created::RunOwned {
                    names: names.clone(),
                    reference: None,
                };
                resolver::contained_databases_are_allowed(admin, recipe)
                    .await
                    .map_err(read)?;
                resolver::create_scratch(admin, &names, recipe)
                    .await
                    .map_err(|_| Error::Scratch)?;
                let reference_names = super::super::names_for_reference(&self.token)?;
                self.created = Created::RunOwned {
                    names: names.clone(),
                    reference: Some(reference_names.clone()),
                };
                let reference = resolver::fresh_reference(admin, &reference_names)
                    .await
                    .map_err(read)?;
                self.created = Created::RunOwned {
                    names: names.clone(),
                    reference: None,
                };
                for database in dependency_inputs
                    .as_ref()
                    .map(|inputs| inputs.databases())
                    .unwrap_or_default()
                {
                    // Recorded before it exists, and marked once it does: a
                    // companion carries the dependency's own name, so the
                    // record is what lets a later run find it (`recovery`).
                    record::record_companion(
                        admin,
                        names.database(),
                        self.recorded.len(),
                        &database,
                    )
                    .await
                    .map_err(read)?;
                    self.recorded.push(database.clone());
                    let companion = sql::companion::Companion::create(admin, &database)
                        .await
                        .map_err(read)?;
                    self.companions.push(companion);
                    record::mark_companion(admin, &database, &self.token)
                        .await
                        .map_err(read)?;
                    let mut owner = Conn::connect(Driver::Mssql, self.connection)
                        .await
                        .map_err(read)?;
                    if !sql::marked(&mut owner, &self.mark).await.map_err(read)? {
                        return Err(Error::Unusable(
                            "the companion connection changed instance".into(),
                        ));
                    }
                    self.companions
                        .last_mut()
                        .expect("recorded companion")
                        .acquire(&mut owner)
                        .await
                        .map_err(read)?;
                }
                let databases = sql::database_names(admin).await.map_err(read)?;
                let mut companion_sessions = BTreeMap::new();
                for companion in &self.companions {
                    let mut owner = Conn::connect(Driver::Mssql, self.connection)
                        .await
                        .map_err(read)?;
                    if !sql::marked(&mut owner, &self.mark).await.map_err(read)? {
                        return Err(Error::Unusable(
                            "the companion staging connection changed instance".into(),
                        ));
                    }
                    companion.verify(&mut owner).await.map_err(read)?;
                    let inventory = sql::inventory::read(&mut owner, None).await.map_err(read)?;
                    if !inventory.is_empty() {
                        return Err(Error::Unusable(format!(
                            "dependency companion {} is not empty: {}",
                            companion.name(),
                            inventory.names().join(", ")
                        )));
                    }
                    let actual = sql::options::read(&mut owner, companion.name())
                        .await
                        .map_err(read)?;
                    let differences = sql::options::differences(&actual, &reference);
                    if !differences.is_empty() {
                        return Err(Error::Unusable(format!(
                            "dependency companion {} differs from a fresh database: {}",
                            companion.name(),
                            differences.join(", ")
                        )));
                    }
                    let sandbox = format!("pbps_sandbox_{:032x}", rand::random::<u128>());
                    sql::create_sandbox(&mut owner, &sandbox)
                        .await
                        .map_err(read)?;
                    sql::enter_sandbox(&mut owner, &sandbox)
                        .await
                        .map_err(read)?;
                    sql::sandbox_checks(&mut owner, &databases, companion.name())
                        .await
                        .map_err(read)?;
                    companion_sessions.insert(companion.name().to_owned(), owner);
                }
                let token = format!("{:032x}", rand::random::<u128>());
                if let Some(inputs) = dependency_inputs {
                    inputs
                        .stage(&mut companion_sessions, admin, &token)
                        .await
                        .map_err(read)?;
                }
                (reference, names.database().to_owned(), databases)
            }
            Provisioning::Supplied => {
                sql::require_owner(admin).await.map_err(read)?;
                // DENY VIEW ANY DATABASE also hides model's row. Name the
                // missing premise before attempting the reference read.
                let databases = sql::database_names(admin).await.map_err(read)?;
                let reference = sql::options::supplied_reference(admin)
                    .await
                    .map_err(read)?;
                (reference, self.mark.database.name.clone(), databases)
            }
        };
        // The source connection stays alive holding the instance marker.
        // The sandbox gets a dedicated connection whose NO REVERT context is
        // never reused for administrator cleanup.
        let mut owner = Conn::connect(Driver::Mssql, self.connection)
            .await
            .map_err(read)?;
        if !sql::marked(&mut owner, &self.mark).await.map_err(read)? {
            return Err(Error::Unusable(
                "the scratch setup connection does not reach the marked instance".into(),
            ));
        }
        sql::use_database(&mut owner, &database)
            .await
            .map_err(read)?;
        if provisioning == Provisioning::Supplied
            && sql::database(&mut owner).await.map_err(read)? != self.mark.database
        {
            return Err(Error::Unusable(
                "the supplied scratch database changed after its session was marked".into(),
            ));
        }
        self.acquired = Some(sql::database(&mut owner).await.map_err(read)?);
        sql::require_owner(&mut owner).await.map_err(read)?;
        let inventory = sql::inventory::read(&mut owner, None).await.map_err(read)?;
        if !inventory.is_empty() {
            return Err(Error::Unusable(format!(
                "the scratch database is not empty: {}",
                inventory.names().join(", ")
            )));
        }
        let actual = sql::options::read(&mut owner, &database)
            .await
            .map_err(read)?;
        let mut differences = sql::options::differences(&actual, &reference);
        // A preview has no target recipe. Fresh-state checks above still apply.
        if recipe.sql_server.is_some() {
            differences.extend(sql::options::recipe_differences(&actual, recipe).map_err(read)?);
        }
        if !differences.is_empty() {
            return Err(Error::Unusable(format!(
                "scratch database options differ from a fresh database: {}",
                differences.join(", ")
            )));
        }
        let sandbox = format!("pbps_sandbox_{}", self.token);
        if provisioning == Provisioning::Supplied {
            // Once acquired, every refusal must clean what setup created,
            // including a CREATE USER that succeeds before ADD MEMBER fails.
            self.created = Created::Supplied {
                database: sql::database(&mut owner).await.map_err(read)?,
                sandbox: sandbox.clone(),
            };
        }
        sql::create_sandbox(&mut owner, &sandbox)
            .await
            .map_err(read)?;
        // The run's own sandbox user is its record on a supplied database:
        // another run that takes the lock now finds it.
        self.unlock(admin).await;
        sql::enter_sandbox(&mut owner, &sandbox)
            .await
            .map_err(read)?;
        self.entered = true;
        sql::sandbox_checks(&mut owner, &databases, &database)
            .await
            .map_err(read)?;
        let inventory = sql::inventory::read(&mut owner, Some(&sandbox))
            .await
            .map_err(read)?;
        if !inventory.is_empty() {
            return Err(Error::Unusable(format!(
                "the sandbox inventory is not empty: {}",
                inventory.names().join(", ")
            )));
        }
        if provisioning == Provisioning::Supplied {
            let mut repair = Conn::connect(Driver::Mssql, self.connection)
                .await
                .map_err(read)?;
            let baseline =
                sql::visibility::Visibility::retain(&mut repair, &self.mark, &sandbox, &databases)
                    .await
                    .map_err(read)?;
            self.visibility = Some(tokio::sync::Mutex::new((repair, baseline)));
        }
        Ok(Acquired {
            owner,
            database,
            sandbox,
            databases,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        target: &CatalogFacts,
        target_connection: &mut Conn,
        authorization: &scope::Authorization,
        schemas: &[String],
        binding: &BindingRequest<'_>,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    ) -> Result<Prepared, Error> {
        if self.placement == Some(sql::Placement::SeparateInstance)
            && provisioning == Provisioning::Supplied
            && !binding.dependencies.is_empty()
        {
            return Err(Error::Externals(binding.dependencies.keys().map(|name| format!(
                "dependency `{}` requires a companion database; supplied scratch on another instance cannot create it", name.shown()
            )).collect()));
        }
        let sections = self
            .externals
            .map(externals::sections)
            .transpose()
            .map_err(|e| Error::Externals(vec![e.to_string()]))?;
        self.deferred = sections.as_ref().is_some_and(|s| s.deferred.is_some());
        let real = self.placement == Some(sql::Placement::SameInstance);
        let mut dependency_inputs =
            dependencies::read(target_connection, binding.dependencies, real)
                .await
                .map_err(read)?;
        let recipe = scope::recipe(Driver::Mssql, target)
            .map_err(|error| Error::Scope(error.to_string()))?;
        let Acquired {
            mut owner,
            database,
            sandbox,
            databases,
        } = self
            .acquire(provisioning, admin, &recipe, Some(&mut dependency_inputs))
            .await?;
        let managed = [binding.base, binding.desired];
        let required = capture::required_externals_declared(
            target_connection,
            &managed,
            binding.dependencies,
            key,
        )
        .await
        .map_err(capture_error)?;
        let pending = required
            .prerequisites()
            .into_keys()
            .collect::<BTreeSet<_>>();
        let excluded = required
            .managed()
            .union(&pending)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut context = match authorization {
            scope::Authorization::Mssql(context) => auth::reread(target_connection, context)
                .await
                .map_err(read)?,
            _ => {
                return Err(Error::Scope(
                    "the SQL Server authorization belongs to another engine".into(),
                ));
            }
        };
        let external_scope = capture::CaptureScope {
            retained: required.roots().clone(),
            candidates: BTreeSet::new(),
        };
        let initial = capture::capture_declared(
            target_connection,
            &external_scope,
            binding.dependencies,
            key,
        )
        .await
        .map_err(capture_error)?;
        let initial_owners = auth::OwnerPartition::for_staging(&initial, &context, &excluded)
            .map_err(|e| Error::Externals(vec![format!("{e:?}")]))?;
        auth::read_staged_permissions(target_connection, &mut context, &initial_owners)
            .await
            .map_err(read)?;
        let token = self.token.clone();
        let planned = scope::mssql_planned(&self.request.planned);
        let mut map = auth::PrincipalMap::generate(&context, &planned, &token);
        let mut setup_context = context.clone();
        let initially_held = match authorization {
            scope::Authorization::Mssql(context) => &context.schemas,
            _ => unreachable!("authorization checked above"),
        };
        setup_context
            .schemas
            .retain(|name, _| initially_held.contains_key(name));
        auth::reconstruct_database(&mut owner, &map, &setup_context)
            .await
            .map_err(read)?;
        let before = externals::inventory(&mut owner, &sandbox)
            .await
            .map_err(read)?;
        if let Some(section) = &sections {
            let script = section.immediate;
            let mut file = self.bounded(&database, &sandbox, &databases).await?;
            externals::run(&mut file, script)
                .await
                .map_err(|e| Error::Externals(vec![e.to_string()]))?;
            drop(file);
        }
        let middle = externals::inventory(&mut owner, &sandbox)
            .await
            .map_err(read)?;
        let from_file = externals::created(&before, &middle);
        // Validate all file roots before filling; absence/overlap and hidden
        // shape errors name the file's object rather than a renderer failure.
        capture::compare_staged_pending(
            &mut owner,
            target_connection,
            &from_file,
            &[],
            binding.dependencies,
            key,
            &pending,
        )
        .await
        .map_err(stage_error)?;
        let items = externals::render::missing_ready(&required, &from_file, &pending)
            .map_err(Error::Externals)?;
        auth::reconstruct_missing_schemas(&mut owner, &map, &context)
            .await
            .map_err(read)?;
        let mut render_session = self.bounded(&database, &sandbox, &databases).await?;
        for item in &items {
            for statement in &item.statements {
                render_session.execute(statement).await.map_err(|e| {
                    Error::Externals(vec![format!("rendered {:?}: {e}", item.object)])
                })?;
            }
        }
        drop(render_session);
        let after = externals::inventory(&mut owner, &sandbox)
            .await
            .map_err(read)?;
        let created = externals::created(&before, &after);
        let target_scope = capture::CaptureScope {
            // Inventory carries children with their new parent. Keep explicit
            // external roots too, including cascade/trigger barriers that
            // authorization must still compare (DEC-2270.2).
            retained: created
                .union(required.roots())
                .filter(|id| !pending.contains(*id))
                .cloned()
                .collect(),
            candidates: BTreeSet::new(),
        };
        let target_inputs =
            capture::capture_declared(target_connection, &target_scope, binding.dependencies, key)
                .await
                .map_err(capture_error)?;
        let target_owners = auth::OwnerPartition::for_staging(&target_inputs, &context, &excluded)
            .map_err(|e| Error::Externals(vec![format!("{e:?}")]))?;
        auth::read_staged_permissions(target_connection, &mut context, &target_owners)
            .await
            .map_err(read)?;
        map.extend_staged(&mut owner, &context, &token)
            .await
            .map_err(read)?;
        let rendered_objects = externals::created(&middle, &after);
        auth::reconstruct_owners(
            &mut owner,
            &map,
            &target_owners.restricted_to(&rendered_objects),
        )
        .await
        .map_err(read)?;
        let mut local = context.clone();
        if !context.principal.superuser {
            local.principal.effective = map
                .deployer(&context)
                .ok_or_else(|| Error::Scope("no mapped deployer".into()))?;
        }
        let current_inputs =
            capture::capture_declared(&mut owner, &target_scope, binding.dependencies, key)
                .await
                .map_err(capture_error)?;
        let current_owners = auth::OwnerPartition::for_staging(&current_inputs, &local, &excluded)
            .map_err(|e| Error::Externals(vec![format!("{e:?}")]))?;
        target_owners.verify(&current_owners).map_err(read)?;
        let file_schemas = from_file
            .iter()
            .filter(|id| id.class == capture::class::SCHEMA)
            .map(|id| id.name[0].clone())
            .collect::<Vec<_>>();
        auth::replay_file_schema_permissions(&mut owner, &map, &context, &file_schemas)
            .await
            .map_err(read)?;

        auth::replay_staged_permissions(
            &mut owner,
            &map,
            &context,
            &target_owners,
            &current_owners,
        )
        .await
        .map_err(read)?;
        let staged = capture::compare_staged_pending(
            &mut owner,
            target_connection,
            &created,
            &managed,
            binding.dependencies,
            key,
            &pending,
        )
        .await
        .map_err(stage_error)?;
        let mut compile = self.bounded(&database, &sandbox, &databases).await?;
        auth::enter_sandbox_deployer(
            &mut compile,
            &map,
            &context,
            target,
            &auth::SandboxScope {
                sandbox: &sandbox,
                scratch: &database,
                databases: &databases,
            },
        )
        .await
        .map_err(read)?;
        let all_schemas = context.schemas.keys().cloned().collect::<Vec<_>>();
        let differences = auth::verify_staged_authorization(
            &mut compile,
            &map,
            &context,
            &all_schemas,
            &target_owners,
            &current_owners,
        )
        .await
        .map_err(read)?;
        if !differences.is_empty() {
            return Err(Error::Externals(differences));
        }
        auth::apply_planned(&mut compile, &map, &planned)
            .await
            .map_err(read)?;
        let scratch_catalog = scope::read_catalog(&mut compile, Driver::Mssql, schemas, &[])
            .await
            .map_err(read)?;
        let managed_ids = required.managed().clone();
        self.chained = Some(required);
        let chaining = chained::Chaining {
            key: key.clone(),
            pending,
            managed_ids,
            deferred: sections.and_then(|s| s.deferred.map(str::to_owned)),
            managed: managed.iter().map(|s| (*s).clone()).collect(),
            context,
            token,
        };
        Ok(Prepared {
            dependencies: dependency_inputs.rehearsals(),
            compile,
            map: scope::Principals::Mssql(map),
            inventory: after,
            staged,
            scratch_catalog,
            differences,
            reach: self.reach(&database, &sandbox, &databases),
            chaining: Box::new(chaining),
        })
    }

    async fn bounded(
        &self,
        database: &str,
        sandbox: &str,
        databases: &[String],
    ) -> Result<Conn, Error> {
        self.reach(database, sandbox, databases).open().await
    }

    fn reach(&self, database: &str, sandbox: &str, databases: &[String]) -> Reach {
        Reach {
            connection: self.connection.to_owned(),
            mark: self.mark.clone(),
            acquired: self.acquired.clone(),
            database: database.to_owned(),
            sandbox: sandbox.to_owned(),
            databases: databases.to_vec(),
        }
    }

    pub(in crate::resolver::server) async fn release(
        &mut self,
        admin: &mut Conn,
    ) -> Result<(), Vec<String>> {
        // Whatever stopped the run before its sandbox existed, the lock goes
        // with it (the session's end would free it too).
        self.unlock(admin).await;
        // The companions first: the scratch database holds their record, and
        // the login the run's (`recovery`, #2410). Anything left of them keeps
        // both, so a later run still finds what is left.
        let mut failed = Vec::new();
        if !self.companions.is_empty() {
            if !matches!(sql::on_backend(admin, &self.backend).await, Ok(true)) {
                failed.extend(self.companions.iter().map(|db| {
                    format!(
                        "dependency companion {}: cleanup backend changed or is unreadable",
                        db.name()
                    )
                }));
            } else {
                for companion in std::mem::take(&mut self.companions) {
                    if let Err(error) = companion.release(admin).await {
                        failed.push(format!(
                            "dependency companion {} ({error})",
                            companion.name()
                        ));
                    }
                }
            }
        }
        let keep_record = !failed.is_empty();
        failed.extend(
            self.release_scratch(admin, keep_record)
                .await
                .err()
                .unwrap_or_default(),
        );
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed)
        }
    }

    async fn release_scratch(
        &mut self,
        admin: &mut Conn,
        keep_record: bool,
    ) -> Result<(), Vec<String>> {
        let entered = std::mem::take(&mut self.entered);
        match std::mem::replace(&mut self.created, Created::Nothing) {
            Created::Nothing => Ok(()),
            Created::RunOwned { names, reference } => {
                let mut failed = Vec::new();
                if !matches!(sql::on_backend(admin, &self.backend).await, Ok(true)) {
                    let mut resources = vec![format!(
                        "database {} and login {}: cleanup backend changed or is unreadable",
                        names.database(),
                        names.login()
                    )];
                    if let Some(reference) = reference {
                        resources.push(format!("reference database {}", reference.database()));
                    }
                    return Err(resources);
                }
                // The record names the reference too: a token is found by its
                // login, so a reference left with no record is never retried.
                let mut left = Vec::new();
                if keep_record {
                    left.push("companion databases");
                }
                if let Some(reference) = reference
                    && let Err(error) = resolver::drop_reference(admin, &reference).await
                {
                    failed.push(format!(
                        "reference database {} ({error})",
                        reference.database()
                    ));
                    left.push("reference database");
                }
                if !left.is_empty() {
                    failed.push(format!(
                        "database {} and login {}, kept: they record the {} left",
                        names.database(),
                        names.login(),
                        left.join(" and the ")
                    ));
                } else if let Err(error) =
                    record::drop_database(admin, names.database(), true).await
                {
                    failed.push(format!(
                        "database {} and login {} ({error})",
                        names.database(),
                        names.login()
                    ));
                } else if let Err(error) = record::drop_login(admin, names.login()).await {
                    // The login is the run's record: it goes last.
                    failed.push(format!("login {} ({error})", names.login()));
                }
                if failed.is_empty() {
                    Ok(())
                } else {
                    Err(failed)
                }
            }
            Created::Supplied { database, sandbox } => {
                let result = async {
                    let mut cleanup = Conn::connect(Driver::Mssql, self.connection)
                        .await
                        .map_err(|error| {
                            vec![format!(
                                "database {} cleanup session ({error})",
                                database.name
                            )]
                        })?;
                    let verified = async {
                        if !sql::marked(&mut cleanup, &self.mark).await?
                            || sql::database(&mut cleanup).await? != database
                        {
                            return Err(pbps_db::DbError::Refused(
                                "the supplied cleanup connection no longer reaches \
                                 the marked database"
                                    .into(),
                            ));
                        }
                        Ok(())
                    }
                    .await;
                    verified.map_err(|error| vec![error.to_string()])?;
                    if entered {
                        let result = async {
                            if let Some(retained) = self.visibility.take() {
                                let (mut bounded, baseline) = retained.into_inner();
                                baseline.restore(&mut bounded).await?;
                                return Ok(bounded);
                            }
                            let mut bounded = Conn::connect(Driver::Mssql, self.connection).await?;
                            if !sql::marked(&mut bounded, &self.mark).await?
                                || sql::database(&mut bounded).await? != database
                            {
                                return Err(pbps_db::DbError::Refused(
                                    "the sandbox cleanup connection no longer reaches \
                                     the marked database"
                                        .into(),
                                ));
                            }
                            sql::enter_sandbox(&mut bounded, &sandbox).await?;
                            Ok(bounded)
                        }
                        .await;
                        let mut bounded = result.map_err(|error| vec![error.to_string()])?;
                        sql::inventory::cleanup(&mut bounded, Some(&sandbox)).await?;
                        drop(bounded);
                    }
                    sql::inventory::drop_sandbox(&mut cleanup, &sandbox).await
                }
                .await;
                result.map_err(|mut failed| {
                    failed.push(format!(
                        "sandbox user {sandbox} (db_owner) in database {}",
                        database.name
                    ));
                    failed
                })
            }
        }
    }
}

fn capture_error(error: capture::CaptureError) -> Error {
    Error::Externals(vec![format!("{error:?}")])
}
fn stage_error(error: capture::StageError) -> Error {
    match error {
        capture::StageError::Refused(findings) => Error::Externals(findings),
        capture::StageError::Capture(error) => capture_error(error),
    }
}

/// How many times both sides are read before the scope of their runtime-bound
/// names is declared not to settle. Each pass adds only candidate sets, which
/// are finite, so a pass past the second means a candidate brought a module
/// of its own.
const SETTLING_READS: usize = 8;

/// What a failed scratch compilation is. A declaration that did not compile
/// is the binding's verdict; a statement the server or connection
/// interrupted answered nothing about it (SPEC 9.8, #1597).
pub(super) fn compile_failed(error: ReconstructError) -> Error {
    match error {
        ReconstructError::Interrupted { .. } => Error::Read(error.to_string()),
        ReconstructError::Unsupported(_)
        | ReconstructError::Emit { .. }
        | ReconstructError::Compile { .. }
        | ReconstructError::Cycle(_) => Error::Binding(error.to_string()),
    }
}

/// The private reconstruction, and what the two captures of its run read.
pub(in crate::resolver::server) struct Binding {
    reconstruction: Reconstruction,
    /// The roots both sides' declarations name.
    managed: capture::CaptureScope,
    declarations: pbps_model::dependency::Dependencies,
}

impl Closing {
    pub(super) fn planning_records(
        &self,
    ) -> Result<Vec<capture::BindingRecord>, pbps_model::resolver::ManifestError> {
        self.captured.planning_records()
    }
    pub(super) fn seal_for_plan(
        self,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        self.captured.seal_for_plan()
    }
}

/// The compiled scratch capture and the exact scope its opening read uses.
pub(in crate::resolver::server) struct Closing {
    captured: capture::CompiledCapture,
    /// The target's own roots and the candidates both sides settled on.
    target_scope: capture::CaptureScope,
    namespaces: BTreeSet<String>,
}

/// `scope` with only the roots `captured` holds. A manifest requires every
/// retained root to be among its prerequisites, and a root only one side has,
/// a new object on scratch or a dropped one on the target, is absent on the
/// other: asked for and not found, it would be sealed as an incomplete read.
fn held(
    scope: &capture::CaptureScope,
    captured: &capture::CapturedInputs,
) -> capture::CaptureScope {
    let present: BTreeSet<_> = captured.objects().collect();
    capture::CaptureScope {
        retained: scope
            .retained
            .iter()
            .filter(|root| present.contains(root))
            .cloned()
            .collect(),
        candidates: scope.candidates.clone(),
    }
}

impl Scratch<'_> {
    pub(super) fn reconstruction(
        &self,
        binding: &BindingRequest<'_>,
        ids: &pbps_model::IdsFile,
        dependencies: &pbps_model::ModuleDeps,
    ) -> Result<Binding, Error> {
        let reconstruction = Reconstruction::with_chained(
            &pbps_mssql::Mssql,
            binding.bootstrap,
            ids,
            dependencies,
            self.chained.as_ref(),
            self.deferred,
        )
        .map_err(compile_failed)?;
        Ok(Binding {
            reconstruction,
            managed: capture::managed_scope(&[binding.base, binding.desired]),
            declarations: binding.dependencies.clone(),
        })
    }
}

impl Binding {
    pub(super) fn len(&self) -> usize {
        self.reconstruction.len()
    }

    /// Compiles `steps` as the reproduced deployer, each statement in its own
    /// batch and autocommit (measured on 17.0, `reconstruct.rs`).
    pub(super) async fn compile(
        &self,
        prepared: &mut Prepared,
        target: &mut Conn,
        steps: std::ops::Range<usize>,
    ) -> Result<(), Error> {
        for at in steps {
            if self.reconstruction.auxiliary(at).is_some() {
                chained::execute(self, prepared, target, at).await?;
            } else {
                self.reconstruction
                    .compile_steps(&mut prepared.compile, at..at + 1)
                    .await
                    .map_err(compile_failed)?;
            }
        }
        Ok(())
    }

    /// Reads the compiled scratch on a fresh bounded sandbox connection: the
    /// deployer's cannot leave its context and may lack `VIEW DEFINITION`.
    ///
    /// Both captures are made under one scope. A runtime-bound name needs its
    /// candidates in the scope, and a module only the target holds, one the
    /// plan drops, binds names scratch never read, so the scope settles over
    /// what each side reads before anything is sealed.
    pub(super) async fn capture_desired(
        &self,
        reach: Reach,
        target: &mut Conn,
        schemas: &[String],
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'_>,
        staged: &capture::Staged,
    ) -> Result<Closing, Error> {
        let mut scratch = reach.open().await?;
        let mut scope = self.managed.clone();
        // The staged scope already holds every root the comparison accepted.
        scope
            .retained
            .extend(staged.scope().retained.iter().cloned());
        scope
            .candidates
            .extend(staged.scope().candidates.iter().cloned());
        let mut settled = None;
        for _ in 0..SETTLING_READS {
            let on_scratch =
                capture::capture_declared(&mut scratch, &scope, &self.declarations, key)
                    .await
                    .map_err(super::catalog_read_failed)?;
            let on_target = capture::capture_declared(target, &scope, &self.declarations, key)
                .await
                .map_err(super::catalog_read_failed)?;
            let mut next = on_scratch.runtime_scope();
            next.candidates.extend(on_target.runtime_scope().candidates);
            if next == scope {
                settled = Some((held(&scope, &on_scratch), held(&scope, &on_target)));
                break;
            }
            scope = next;
        }
        let Some((scratch_scope, target_scope)) = settled else {
            return Err(Error::Read(
                "the candidates of the runtime-bound names did not settle".into(),
            ));
        };
        let namespaces: BTreeSet<String> = schemas.iter().cloned().collect();
        let recorded = capture::RecordedOwnership {
            schema: desired.schema,
            ids: desired.ids,
            namespaces: &namespaces,
        };
        let captured = capture::capture_desired_for_plan(
            &mut scratch,
            &scratch_scope,
            &self.declarations,
            key,
            &recorded,
            &self.reconstruction,
        )
        .await
        .map_err(super::catalog_read_failed)?;
        Ok(Closing {
            captured,
            target_scope,
            namespaces,
        })
    }

    pub(super) fn assess(
        &self,
        closing: &Closing,
        current: &capture::CapturedInputs,
        staged: &capture::Staged,
    ) -> pbps_db::resolver::capture::Assessment {
        closing
            .captured
            .assess_staged(current, &self.managed, &self.reconstruction, staged)
    }

    pub(super) async fn capture_opening(
        &self,
        target: &mut Conn,
        closing: &Closing,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        base: pbps_diff::Side<'_>,
    ) -> Result<(capture::CapturedInputs, pbps_model::resolver::InputManifest), Error> {
        let recorded = capture::RecordedOwnership {
            schema: base.schema,
            ids: base.ids,
            namespaces: &closing.namespaces,
        };
        capture::capture_qualified_declared(
            target,
            &closing.target_scope,
            &self.declarations,
            key,
            &recorded,
        )
        .await
        .map_err(super::catalog_read_failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::resolver::Observation;
    use pbps_db::resolver::environment::Verdict;

    fn facts(entries: &[(&str, Option<&str>)]) -> CatalogFacts {
        CatalogFacts {
            observations: entries
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Observation::reported(*value)))
                .collect(),
            extensions: Vec::new(),
            available_extensions: BTreeMap::new(),
            collations: Vec::new(),
            settings: BTreeMap::new(),
            visibility: BTreeMap::new(),
        }
    }

    // A key file's owner-only permission is provable only on Unix, and the
    // environment route would need `set_var`, which the workspace forbids as
    // unsafe; scope.rs gates its key-backed case the same way.
    #[cfg(unix)]
    fn key() -> pbps_db::fingerprint::EnvironmentFingerprintKey {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!("pbps2374-key-{}", rand::random::<u64>()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
            .unwrap();
        let key = pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        key
    }

    /// The run asks the SQL Server rule, not the PostgreSQL one and not an
    /// error: a hosted family is named, and nothing maps it to a boxed build.
    #[test]
    fn a_hosted_family_refuses_by_name_through_the_run_scope_check() {
        let boxed = facts(&[
            ("product_version", Some("17.0.4075.5")),
            ("engine_edition", Some("3")),
        ]);
        let hosted = facts(&[
            ("product_version", Some("17.0.4075.5")),
            ("engine_edition", Some("5")),
        ]);
        let report = scope::compare_reported(Driver::Mssql, &boxed, &hosted).unwrap();
        assert_eq!(report.rule.as_str(), resolver::compatibility::REPORTED_RULE);
        let Verdict::Unknown(unknown) = report.verdict() else {
            panic!(
                "a hosted family is not a boxed build: {:?}",
                report.verdict()
            );
        };
        assert_eq!(unknown, ["engine_edition"], "{:?}", report.facts);
        // A boxed edition difference is a recorded limitation, not a fact
        // that differs: the target's own edition checks stay the authority.
        let express = facts(&[
            ("product_version", Some("17.0.4075.5")),
            ("engine_edition", Some("4")),
            ("edition", Some("Express Edition (64-bit)")),
        ]);
        let developer = facts(&[
            ("product_version", Some("17.0.4075.5")),
            ("engine_edition", Some("3")),
            ("edition", Some("Enterprise Developer Edition (64-bit)")),
        ]);
        let report = scope::compare_reported(Driver::Mssql, &developer, &express).unwrap();
        assert!(
            report.limitations["edition"].contains("Express Edition"),
            "{:?}",
            report.limitations
        );
        assert!(!report.facts.values().any(|fact| matches!(
            fact,
            pbps_db::resolver::environment::FactStatus::Mismatch { .. }
        )));
        // Another engine's rule is not this one.
        assert_ne!(
            super::super::reported_rule(Driver::Postgres).unwrap(),
            super::super::reported_rule(Driver::Mssql).unwrap()
        );
    }

    /// The build is what the rule compares: version, level and update level.
    /// An RTM build's absent update level is a known absence; an unreadable
    /// one is no fingerprint at all.
    #[cfg(unix)]
    #[test]
    fn the_reported_build_moves_with_version_level_and_update_and_refuses_unreadable_ones() {
        let key = key();
        let build = |version: &str, level: &str, update: Option<&str>| {
            reported_build(
                &key,
                &facts(&[
                    ("product_version", Some(version)),
                    ("product_level", Some(level)),
                    ("product_update_level", update),
                ]),
            )
        };
        let rtm = build("17.0.4075.5", "RTM", None).unwrap();
        assert_eq!(rtm, build("17.0.4075.5", "RTM", None).unwrap());
        for different in [
            build("17.0.4075.6", "RTM", None),
            build("17.0.4075.5", "CU1", None),
            build("17.0.4075.5", "RTM", Some("CU1")),
        ] {
            assert_ne!(rtm, different.unwrap());
        }
        let mut unreadable = facts(&[
            ("product_version", Some("17.0.4075.5")),
            ("product_level", Some("RTM")),
        ]);
        unreadable.observations.insert(
            "product_update_level".into(),
            Observation::Unknown {
                reason: "denied".into(),
            },
        );
        assert!(matches!(
            reported_build(&key, &unreadable),
            Err(Error::Scope(_))
        ));
        let missing = facts(&[("product_level", Some("RTM"))]);
        assert!(matches!(
            reported_build(&key, &missing),
            Err(Error::Scope(_))
        ));
    }

    /// A statement the server or connection interrupted answered nothing
    /// about the declaration, so it is a read failure and not a verdict.
    #[test]
    fn an_interrupted_compile_is_a_read_failure_and_a_failed_statement_a_verdict() {
        assert!(matches!(
            compile_failed(ReconstructError::Interrupted {
                declaration: "view dbo.v".into(),
                reason: "deadlock victim".into(),
            }),
            Error::Read(_)
        ));
        for verdict in [
            ReconstructError::Compile {
                declaration: "view dbo.v".into(),
                reason: "Invalid object name".into(),
            },
            ReconstructError::Unsupported("drop_table".into()),
            ReconstructError::Cycle("dbo.t".into()),
        ] {
            assert!(matches!(compile_failed(verdict), Error::Binding(_)));
        }
    }
}
