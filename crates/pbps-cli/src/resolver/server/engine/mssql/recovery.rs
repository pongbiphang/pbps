//! SQL Server's side of crash recovery (#2410, DEC-2410.2): what a killed
//! run-owned run leaves on the instance, and what a killed supplied run
//! leaves in the database. `pbps_mssql::resolver::scratch::recovery` holds
//! the SQL; this is the adapter the engine-neutral sweep drives, and the one
//! place the lifecycle calls it.

use std::collections::BTreeMap;

use super::super::super::scratch::recovery::{self, Adapter, Failed, Liveness, Resource};
use super::{Created, Error, Scratch, read, record};
use pbps_db::Conn;
use pbps_mssql::resolver::scratch as sql;

/// The markers of this instance, read on the backend the run checked. Both
/// layouts prove a run dead the same way.
async fn liveness(
    admin: &mut Conn,
    backend: &sql::Backend,
    own: &str,
    tokens: &[String],
) -> Result<BTreeMap<String, Liveness>, String> {
    if !matches!(sql::on_backend(admin, backend).await, Ok(true)) {
        return Err("the scratch connection changed backend".into());
    }
    let up = record::alive(admin, own, tokens)
        .await
        .map_err(|error| format!("the markers of the scratch instance: {error}"))?;
    Ok(tokens
        .iter()
        .map(|token| {
            let liveness = match &up {
                None => Liveness::Unprovable(
                    "this session cannot see its own marker among the instance's, so it cannot \
                     say another is gone"
                        .into(),
                ),
                Some(up) if up.contains(token) => Liveness::Alive,
                Some(_) => Liveness::Dead,
            };
            (token.clone(), liveness)
        })
        .collect())
}

fn brackets(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Why a drop was refused, in the words that say what to do.
fn refused(error: &pbps_db::DbError) -> String {
    let text = error.to_string();
    if text.contains("currently in use") {
        "a session is still connected to it, so the server has not noticed its owner is gone; \
         the next run removes it"
            .into()
    } else if text.contains("does not exist or you do not have permission") {
        "it is owned by another login, or the scratch login may not drop it".into()
    } else if text.contains("owns one or more database") {
        "it still owns a database".into()
    } else {
        text
    }
}

/// A run-owned layout's leftovers: companion databases, a reference database,
/// the scratch database and the login, per token, on the instance.
struct Instance<'a> {
    admin: &'a mut Conn,
    backend: &'a sql::Backend,
    own: &'a str,
    /// Why a scratch database's record of its companions could not be read.
    unread: BTreeMap<String, String>,
}

impl Adapter for Instance<'_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        if !matches!(sql::on_backend(self.admin, self.backend).await, Ok(true)) {
            return Err("the scratch connection changed backend".into());
        }
        let found = record::leftovers(self.admin, self.own)
            .await
            .map_err(|error| format!("the scratch instance's catalog: {error}"))?;
        let mut all = BTreeMap::new();
        for (token, left) in found {
            let mut resources = Vec::new();
            if let Some(database) = &left.database {
                match record::recorded_companions(self.admin, database).await {
                    Ok(names) if names.is_empty() => {}
                    Ok(names) => resources.push(Resource {
                        kind: "companion database",
                        names,
                    }),
                    // Without its record, what it names is not known, and the
                    // database that holds it must not go.
                    Err(error) => {
                        self.unread.insert(database.clone(), error.to_string());
                        resources.push(Resource::one("companion record", database));
                    }
                }
            }
            if let Some(reference) = left.reference {
                resources.push(Resource::one("reference database", reference));
            }
            if let Some(database) = left.database {
                resources.push(Resource::one("database", database));
            }
            resources.push(Resource::one("login", left.login));
            all.insert(token, resources);
        }
        Ok(all)
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        liveness(self.admin, self.backend, self.own, tokens).await
    }

    async fn remove(&mut self, token: &str, resource: &Resource) -> Result<(), Failed> {
        if !matches!(sql::on_backend(self.admin, self.backend).await, Ok(true)) {
            return Err("the scratch connection changed backend".to_owned().into());
        }
        match resource.kind {
            "companion record" => Err(format!(
                "the record of its companion databases could not be read ({})",
                self.unread
                    .get(&resource.names[0])
                    .map_or("unknown", String::as_str)
            )
            .into()),
            // A database that carries a dependency's own name is the run's
            // only if the run marked it; one that does not is kept.
            "companion database" => {
                let mut left = Vec::new();
                let mut why = String::new();
                for name in &resource.names {
                    match record::owner_of(self.admin, name).await {
                        Ok(record::Owner::Missing) => {}
                        Ok(record::Owner::Run(owner)) if owner == token => {
                            if let Err(error) = record::drop_database(self.admin, name, false).await
                            {
                                why = refused(&error);
                                left.push(name.clone());
                            }
                        }
                        Ok(_) => {
                            why = "it is not marked as this run's (a database of that name may \
                                   be someone else's)"
                                .into();
                            left.push(name.clone());
                        }
                        Err(error) => {
                            why = error.to_string();
                            left.push(name.clone());
                        }
                    }
                }
                if left.is_empty() {
                    Ok(())
                } else {
                    Err(Failed {
                        why,
                        left: Some(left),
                    })
                }
            }
            "login" => record::drop_login(self.admin, &resource.names[0])
                .await
                .map_err(|error| refused(&error).into()),
            _ => record::drop_database(self.admin, &resource.names[0], false)
                .await
                .map_err(|error| refused(&error).into()),
        }
    }

    fn statement(&self, resource: &Resource) -> String {
        match resource.kind {
            "login" => format!("DROP LOGIN {};", brackets(&resource.names[0])),
            "companion record" => format!(
                "inspect the extended property pbps_companions of {}, then drop the databases it \
                 names that carry the property pbps_run",
                brackets(&resource.names[0])
            ),
            _ => format!(
                "DROP DATABASE {};",
                resource
                    .names
                    .iter()
                    .map(|name| brackets(name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// A supplied layout's leftover: whatever a run whose sandbox user is in the
/// database made there. Its release is the layout's own, as the sandbox
/// finds it.
struct Sandboxed<'a, 'b> {
    scratch: &'a mut Scratch<'b>,
    admin: &'a mut Conn,
    database: sql::Database,
}

impl Adapter for Sandboxed<'_, '_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        let tokens = record::sandboxes(self.admin, &self.scratch.token)
            .await
            .map_err(|error| format!("the scratch database's users: {error}"))?;
        Ok(tokens
            .into_iter()
            .map(|token| {
                (
                    token,
                    vec![Resource::one("database contents", &self.database.name)],
                )
            })
            .collect())
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        liveness(
            self.admin,
            &self.scratch.backend,
            &self.scratch.token,
            tokens,
        )
        .await
    }

    async fn remove(&mut self, token: &str, _: &Resource) -> Result<(), Failed> {
        self.scratch.created = Created::Supplied {
            database: self.database.clone(),
            sandbox: format!("pbps_sandbox_{token}"),
        };
        self.scratch.entered = true;
        self.scratch
            .release_scratch(self.admin, false)
            .await
            .map_err(|left| left.join("; ").into())
    }

    fn statement(&self, resource: &Resource) -> String {
        format!(
            "in database {}, as its owner: drop the objects and the users named pbps_sandbox_*",
            brackets(&resource.names[0])
        )
    }
}

impl Scratch<'_> {
    /// Removes what runs killed earlier left, before this run creates
    /// anything. On a supplied database it refuses when the run that holds the
    /// sandbox user there is still running. Never fails the run for what it
    /// cannot remove (D4 of #2410).
    pub(super) async fn recover(
        &mut self,
        provisioning: super::super::super::scratch::Provisioning,
        admin: &mut Conn,
    ) -> Result<(), Error> {
        use super::super::super::scratch::Provisioning;
        match provisioning {
            Provisioning::RunOwned => {
                let mut instance = Instance {
                    admin,
                    backend: &self.backend,
                    own: &self.token,
                    unread: BTreeMap::new(),
                };
                recovery::emit(&recovery::sweep(&mut instance).await);
                Ok(())
            }
            Provisioning::Supplied => {
                let database = sql::database(admin).await.map_err(read)?;
                let live = {
                    let mut sandboxed = Sandboxed {
                        scratch: self,
                        admin,
                        database,
                    };
                    let report = recovery::sweep(&mut sandboxed).await;
                    recovery::emit(&report);
                    report.live
                };
                if live.is_empty() {
                    return Ok(());
                }
                Err(Error::Unusable(format!(
                    "the scratch database is in use by a run that is still running (token {}…); \
                     wait for it to finish, or use another scratch database",
                    &live[0][..8]
                )))
            }
        }
    }
}
