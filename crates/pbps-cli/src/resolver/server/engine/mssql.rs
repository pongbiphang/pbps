//! SQL Server's scratch adapter: the shared runner owns the lifecycle, and
//! the dialect owns every statement and inventory (DEC-2236.1).

use super::super::scratch::{Provisioning, Tokens};
use super::super::{Error, ProduceError};
use crate::resolver::scope;
use pbps_db::resolver::ScratchNames;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_mssql::resolver::{self, scratch as sql};

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
}

fn read(error: pbps_db::DbError) -> Error {
    Error::Read(error.to_string())
}

pub(super) async fn scratch<'a>(
    target: &mut Conn,
    admin: &mut Conn,
    tokens: &Tokens,
    planned_identity: &str,
    connection: &'a str,
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
            },
        ))
    }
    .await;
    result.map_err(ProduceError::Run)
}

impl Scratch<'_> {
    pub(super) async fn prepare(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        target: &CatalogFacts,
    ) -> Result<std::convert::Infallible, Error> {
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
        Err(Error::Binding(
            "SQL Server resolver externals are not implemented yet (#2237)".into(),
        ))
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
