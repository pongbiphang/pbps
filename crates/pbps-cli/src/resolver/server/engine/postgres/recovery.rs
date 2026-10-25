//! PostgreSQL's side of crash recovery (#2410, DEC-2410.1): what a killed
//! run-owned run leaves in the cluster, and what a killed supplied run leaves
//! in the database. `pbps_pg::resolver::recovery` holds the SQL; this is the
//! adapter the engine-neutral sweep drives, and the two places the lifecycle
//! calls it.

use std::collections::BTreeMap;

use super::super::super::scratch::recovery::{self, Adapter, Failed, Liveness, Resource};
use super::{Error, Scratch, Standard, current_database, pinned, release_supplied};
use pbps_db::Conn;
use pbps_pg::resolver::recovery as record;
use pbps_pg::resolver::scratch::{self as sql, Backend};

/// The sessions of this cluster, read on the backend the run checked. Both
/// layouts prove a run dead the same way.
async fn liveness(
    admin: &mut Conn,
    backend: &Backend,
    own: &str,
    tokens: &[String],
) -> Result<BTreeMap<String, Liveness>, String> {
    pinned(admin, backend).await.map_err(|e| e.to_string())?;
    let alive = record::alive(admin, own, tokens)
        .await
        .map_err(|error| format!("the sessions of the scratch server: {error}"))?;
    Ok(tokens
        .iter()
        .map(|token| {
            let liveness = match &alive {
                None => Liveness::Unprovable(
                    "this session cannot see its own run among the server's sessions, so it \
                     cannot say another is gone"
                        .into(),
                ),
                Some(alive) if alive.contains(token) => Liveness::Alive,
                Some(_) => Liveness::Dead,
            };
            (token.clone(), liveness)
        })
        .collect())
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A run-owned layout's leftovers: a database, run-local roles and a login
/// per token, in the cluster.
struct Cluster<'a> {
    admin: &'a mut Conn,
    backend: &'a Backend,
    own: &'a str,
}

impl Adapter for Cluster<'_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        pinned(self.admin, self.backend)
            .await
            .map_err(|e| e.to_string())?;
        let found = record::leftovers(self.admin, self.own)
            .await
            .map_err(|error| format!("the scratch server's catalog: {error}"))?;
        Ok(found
            .into_iter()
            .map(|(token, left)| {
                let mut resources = Vec::new();
                if let Some(database) = left.database {
                    resources.push(Resource::one("database", database));
                }
                if !left.roles.is_empty() {
                    resources.push(Resource {
                        kind: "role",
                        names: left.roles,
                    });
                }
                resources.push(Resource::one("login", left.login));
                (token, resources)
            })
            .collect())
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        liveness(self.admin, self.backend, self.own, tokens).await
    }

    async fn remove(&mut self, resource: &Resource) -> Result<(), Failed> {
        pinned(self.admin, self.backend)
            .await
            .map_err(|e| e.to_string())?;
        match resource.kind {
            // Without FORCE: a session the engine still shows connected to it
            // refuses the drop, whatever was decided about its owner.
            "database" => pbps_pg::resolver::drop_database(self.admin, &resource.names[0], false)
                .await
                .map_err(|error| match error.server_error_code().as_deref() {
                    Some("55006") => "a session is still connected to it, so the server has not \
                         noticed its owner is gone; the next run removes it"
                        .to_owned()
                        .into(),
                    _ => error.to_string().into(),
                }),
            "role" => {
                let names: Vec<_> = resource.names.iter().map(|n| n.as_str().into()).collect();
                let failed = pbps_pg::resolver::drop_roles(self.admin, &names).await;
                if failed.is_empty() {
                    Ok(())
                } else {
                    Err(Failed {
                        why: "the server refused to drop it, most likely for a privilege it \
                              holds in another database"
                            .into(),
                        left: Some(failed.iter().map(|role| role.raw().to_owned()).collect()),
                    })
                }
            }
            _ => pbps_pg::resolver::drop_login(self.admin, &resource.names[0])
                .await
                .map_err(|error| error.to_string().into()),
        }
    }

    fn statement(&self, resource: &Resource) -> String {
        match resource.kind {
            "database" => format!("DROP DATABASE IF EXISTS {};", quoted(&resource.names[0])),
            _ => format!(
                "DROP ROLE IF EXISTS {};",
                resource
                    .names
                    .iter()
                    .map(|name| quoted(name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// A supplied layout's leftover: whatever a run that holds the claim made in
/// the database. Its release is the layout's own, with the state the claim
/// recorded.
struct Claimed<'a> {
    admin: &'a mut Conn,
    backend: &'a Backend,
    standard: &'a Standard,
    claim: &'a record::Claim,
    database: &'a str,
    own: &'a str,
}

impl Adapter for Claimed<'_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        Ok(BTreeMap::from([(
            self.claim.token.clone(),
            vec![Resource::one("database contents", self.database)],
        )]))
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        liveness(self.admin, self.backend, self.own, tokens).await
    }

    async fn remove(&mut self, _: &Resource) -> Result<(), Failed> {
        release_supplied(
            self.admin,
            self.backend,
            self.standard,
            self.database,
            &self.claim.defaults,
            &self.claim.acl,
        )
        .await
        .map_err(|left| left.join("; ").into())
    }

    fn statement(&self, resource: &Resource) -> String {
        format!(
            "connected to database {} as the scratch account: DROP OWNED BY CURRENT_USER;",
            quoted(&resource.names[0])
        )
    }
}

impl Scratch<'_> {
    /// Removes what runs killed on this cluster left, before this run
    /// creates anything. Never fails the run: what cannot be removed is a
    /// warning that names it (D4 of #2410).
    pub(super) async fn sweep_cluster(&mut self, admin: &mut Conn) {
        let mut cluster = Cluster {
            admin,
            backend: &self.backend,
            own: &self.token,
        };
        recovery::emit(&recovery::sweep(&mut cluster).await);
    }

    /// What a run killed in the supplied database left: refuses when the
    /// claim's run is still running, cleans it when that run is gone. A
    /// database that holds objects but no claim is not touched here, and the
    /// emptiness check refuses it as it always has.
    pub(super) async fn recover_supplied(&mut self, admin: &mut Conn) -> Result<(), Error> {
        pinned(admin, &self.backend).await?;
        let read = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        // A scratch account that does not own its database is refused by the
        // checks that follow; there is nothing here it could clean.
        if !sql::owns_database(admin)
            .await
            .map_err(read("the scratch database's owner"))?
        {
            return Ok(());
        }
        let claim = match record::read_claim(admin)
            .await
            .map_err(read("the scratch database's claim"))?
        {
            None => return Ok(()),
            Some(Ok(claim)) => claim,
            Some(Err(why)) => {
                return Err(Error::Unusable(format!(
                    "the scratch database holds a `{}` schema that {why}; it is not removed, \
                     since nothing says whose it is. Drop it with DROP SCHEMA {} CASCADE if it \
                     is not in use",
                    record::CLAIM,
                    record::CLAIM
                )));
            }
        };
        let database = current_database(admin).await?;
        let mut claimed = Claimed {
            admin,
            backend: &self.backend,
            standard: self.standard,
            claim: &claim,
            database: &database,
            own: &self.token,
        };
        let report = recovery::sweep(&mut claimed).await;
        recovery::emit(&report);
        if report.live.contains(&claim.token) {
            return Err(Error::Unusable(format!(
                "the scratch database is in use by a run that is still running (token {}…); \
                 wait for it to finish, or use another scratch database",
                &claim.token[..8]
            )));
        }
        Ok(())
    }
}

/// A sweep of the cluster `admin` is connected to, for the tests that run it
/// on each server without a whole run around it.
#[cfg(test)]
pub(in crate::resolver::server) async fn sweep_cluster_on(
    admin: &mut Conn,
    own: &str,
) -> recovery::Report {
    let backend = sql::backend(admin).await.unwrap();
    recovery::sweep(&mut Cluster {
        admin,
        backend: &backend,
        own,
    })
    .await
}
