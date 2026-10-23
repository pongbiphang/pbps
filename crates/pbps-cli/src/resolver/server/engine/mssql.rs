//! SQL Server's scratch adapter: the shared runner owns the lifecycle, and
//! the dialect owns every statement and inventory (DEC-2236.1).

use super::super::scratch::{Provenance, Provisioning, Tokens};
use super::super::{BindingRequest, Error, ProduceError, ScopeRequest};
use crate::resolver::scope;
use pbps_db::resolver::ScratchNames;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_mssql::resolver::{self, authorization as auth, capture, externals, scratch as sql};
use std::collections::BTreeSet;

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
    acquired: Option<sql::Database>,
    externals: Option<&'a str>,
    request: &'a ScopeRequest,
    // Preserve the selected source for #2270's evidence handoff.
    _provenance: &'a Provenance,
    _placement: sql::Placement,
}

pub(in crate::resolver::server) struct Prepared {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the compile connection handoff is consumed by #2238"
        )
    )]
    pub(in crate::resolver::server) compile: Conn,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the principal map handoff is consumed by #2238")
    )]
    pub(in crate::resolver::server) map: scope::Principals,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the accepted inventory handoff is consumed by #2238"
        )
    )]
    pub(in crate::resolver::server) inventory: externals::Inventory,
    pub(in crate::resolver::server) staged: capture::Staged,
    pub(super) scratch_catalog: CatalogFacts,
    pub(super) differences: Vec<String>,
}

fn read(error: pbps_db::DbError) -> Error {
    Error::Read(error.to_string())
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
            sql::Placement::SameInstance if externals.is_some() => {
                return Err(Error::Binding(
                    "a same-instance externals file waits for the explicit policy (#2270)".into(),
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
        Ok((
            provisioning,
            Scratch {
                connection,
                mark,
                backend,
                created: Created::Nothing,
                entered: false,
                acquired: None,
                externals,
                request,
                _provenance: provenance,
                _placement: placement,
            },
        ))
    }
    .await;
    result.map_err(ProduceError::Run)
}

impl Scratch<'_> {
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
    ) -> Result<Prepared, Error> {
        if !sql::on_backend(admin, &self.backend).await.map_err(read)? {
            return Err(Error::Unusable(
                "the scratch connection changed backend after separation".into(),
            ));
        }
        let recipe = scope::recipe(Driver::Mssql, target)
            .map_err(|error| Error::Scope(error.to_string()))?;
        let (reference, database, databases) = match provisioning {
            Provisioning::RunOwned => {
                let names = super::super::generated_names()?;
                self.created = Created::RunOwned {
                    names: names.clone(),
                    reference: None,
                };
                resolver::contained_databases_are_allowed(admin, &recipe)
                    .await
                    .map_err(read)?;
                resolver::create_scratch(admin, &names, &recipe)
                    .await
                    .map_err(|_| Error::Scratch)?;
                let reference_names = super::super::generated_names()?;
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
                let databases = sql::database_names(admin).await.map_err(read)?;
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
                "the supplied scratch database changed after separation".into(),
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
        differences.extend(sql::options::recipe_differences(&actual, &recipe).map_err(read)?);
        if !differences.is_empty() {
            return Err(Error::Unusable(format!(
                "scratch database options differ from a fresh database: {}",
                differences.join(", ")
            )));
        }
        let sandbox = format!("pbps_sandbox_{:032x}", rand::random::<u128>());
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
        let managed = [binding.base, binding.desired];
        let required = capture::required_externals(target_connection, &managed)
            .await
            .map_err(capture_error)?;
        let waiting = required.waiting();
        if !waiting.is_empty() {
            return Err(Error::Externals(waiting));
        }
        let mut context = match authorization {
            scope::Authorization::Mssql(context) => auth::read(
                target_connection,
                &context.schemas.keys().cloned().collect::<Vec<_>>(),
            )
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
        let initial = capture::capture(target_connection, &external_scope)
            .await
            .map_err(capture_error)?;
        let initial_owners = auth::OwnerPartition::from_capture(&initial, &context)
            .map_err(|e| Error::Externals(vec![format!("{e:?}")]))?;
        auth::read_staged_permissions(target_connection, &mut context, &initial_owners)
            .await
            .map_err(read)?;
        let token = format!("{:032x}", rand::random::<u128>());
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
        if let Some(script) = self.externals {
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
        capture::compare_staged(&mut owner, target_connection, &from_file, &[])
            .await
            .map_err(stage_error)?;
        let items = externals::render::missing(&required, &from_file).map_err(Error::Externals)?;
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
            retained: created.clone(),
            candidates: BTreeSet::new(),
        };
        let target_inputs = capture::capture(target_connection, &target_scope)
            .await
            .map_err(capture_error)?;
        let target_owners = auth::OwnerPartition::from_capture(&target_inputs, &context)
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
        let current_inputs = capture::capture(&mut owner, &target_scope)
            .await
            .map_err(capture_error)?;
        let current_owners = auth::OwnerPartition::from_capture(&current_inputs, &local)
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
        let staged = capture::compare_staged(&mut owner, target_connection, &created, &managed)
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
        Ok(Prepared {
            compile,
            map: scope::Principals::Mssql(map),
            inventory: after,
            staged,
            scratch_catalog,
            differences,
        })
    }

    async fn bounded(
        &self,
        database: &str,
        sandbox: &str,
        databases: &[String],
    ) -> Result<Conn, Error> {
        let mut conn = Conn::connect(Driver::Mssql, self.connection)
            .await
            .map_err(read)?;
        if !sql::marked(&mut conn, &self.mark).await.map_err(read)? {
            return Err(Error::Unusable(
                "the staging session does not reach the marked instance".into(),
            ));
        }
        sql::use_database(&mut conn, database).await.map_err(read)?;
        if self.acquired.as_ref() != Some(&sql::database(&mut conn).await.map_err(read)?) {
            return Err(Error::Unusable(
                "the staging database changed after acquisition".into(),
            ));
        }
        sql::enter_sandbox(&mut conn, sandbox).await.map_err(read)?;
        sql::sandbox_checks(&mut conn, databases, database)
            .await
            .map_err(read)?;
        Ok(conn)
    }

    pub(super) async fn release(&mut self, admin: &mut Conn) -> Result<(), Vec<String>> {
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
                if let Some(reference) = reference
                    && let Err(error) = resolver::drop_reference(admin, &reference).await
                {
                    failed.push(format!(
                        "reference database {} ({error})",
                        reference.database()
                    ));
                }
                if let Err(error) = resolver::drop_scratch(admin, &names).await {
                    failed.push(format!(
                        "database {} and login {} ({error})",
                        names.database(),
                        names.login()
                    ));
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
