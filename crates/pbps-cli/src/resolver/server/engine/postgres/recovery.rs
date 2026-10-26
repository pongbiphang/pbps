//! PostgreSQL's side of crash recovery (#2410, DEC-2410.1): the adapters the
//! engine-neutral sweep drives, and the two places the lifecycle calls it.
//! `pbps_pg::resolver::recovery` holds the SQL.

use std::collections::BTreeMap;

use super::super::super::scratch::recovery::{self, Adapter, Failed, Liveness, Resource};
use super::{Error, Scratch, Standard, current_database, pinned, release_supplied};
use pbps_db::Conn;
use pbps_pg::resolver::recovery as record;
use pbps_pg::resolver::scratch::{self as sql, Backend};

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The cluster a run checked: the run-owned adapter, and the liveness proof
/// both layouts use.
struct Server<'a> {
    admin: &'a mut Conn,
    backend: &'a Backend,
    own: &'a str,
}

impl Server<'_> {
    /// Reads and writes only on the backend the run checked.
    async fn pin(&mut self) -> Result<(), String> {
        pinned(self.admin, self.backend)
            .await
            .map_err(|e| e.to_string())
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        self.pin().await?;
        let alive = record::alive(self.admin, self.own, tokens)
            .await
            .map_err(|error| format!("the sessions of the scratch server: {error}"))?;
        Ok(tokens
            .iter()
            .map(|token| {
                let liveness = match &alive {
                    None => Liveness::Unprovable(
                        "this session cannot see its own run among the server's sessions, so \
                         it cannot say another is gone"
                            .into(),
                    ),
                    Some(alive) if alive.contains(token) => Liveness::Alive,
                    Some(_) => Liveness::Dead,
                };
                (token.clone(), liveness)
            })
            .collect())
    }
}

impl Adapter for Server<'_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        self.pin().await?;
        let found = record::leftovers(self.admin, self.own)
            .await
            .map_err(|error| format!("the scratch server's catalog: {error}"))?;
        Ok(found
            .into_iter()
            .map(|(token, left)| {
                // The login is the record: last.
                let resources = left
                    .database
                    .map(|database| Resource::one("database", database))
                    .into_iter()
                    .chain((!left.roles.is_empty()).then_some(Resource {
                        kind: "role",
                        names: left.roles,
                    }))
                    .chain([Resource::one("login", left.login)])
                    .collect();
                (token, resources)
            })
            .collect())
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        Server::liveness(self, tokens).await
    }

    async fn remove(&mut self, resource: &Resource) -> Result<(), Failed> {
        self.pin().await?;
        match resource.kind {
            // Without FORCE: a session still connected refuses the drop.
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
        let names = resource
            .names
            .iter()
            .map(|name| quoted(name))
            .collect::<Vec<_>>()
            .join(", ");
        match resource.kind {
            "database" => format!("DROP DATABASE IF EXISTS {names};"),
            _ => format!("DROP ROLE IF EXISTS {names};"),
        }
    }
}

/// A supplied layout's leftover: what a run that holds the claim made in the
/// database, released as the layout releases, with the state the claim holds.
struct Claimed<'a> {
    server: Server<'a>,
    standard: &'a Standard,
    claim: &'a record::Claim,
    database: &'a str,
}

impl Adapter for Claimed<'_> {
    async fn leftovers(&mut self) -> Result<BTreeMap<String, Vec<Resource>>, String> {
        Ok(BTreeMap::from([(
            self.claim.token.clone(),
            vec![Resource::one("database contents", self.database)],
        )]))
    }

    async fn liveness(&mut self, tokens: &[String]) -> Result<BTreeMap<String, Liveness>, String> {
        self.server.liveness(tokens).await
    }

    async fn remove(&mut self, _: &Resource) -> Result<(), Failed> {
        release_supplied(
            self.server.admin,
            self.server.backend,
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
    /// Removes what killed runs left, before this run creates anything. It
    /// never fails the run (D4 of #2410).
    pub(super) async fn sweep_cluster(&mut self, admin: &mut Conn) {
        let mut server = Server {
            admin,
            backend: &self.backend,
            own: &self.token,
        };
        recovery::emit(&recovery::sweep(&mut server).await);
    }

    async fn lock_database(&mut self, admin: &mut Conn) -> Result<(), Error> {
        if !record::lock(admin)
            .await
            .map_err(|error| Error::Read(format!("the scratch database's lock: {error}")))?
        {
            return Err(Error::Unusable(
                "another run has been starting in the scratch database, or cleaning what a \
                 killed run left in it, for over a minute; wait for it, or use another \
                 scratch database"
                    .into(),
            ));
        }
        self.holds_lock = true;
        Ok(())
    }

    /// Releases the lock if held; an ended session releases it anyway.
    pub(super) async fn unlock_database(&mut self, admin: &mut Conn) {
        if std::mem::take(&mut self.holds_lock) {
            let _ = admin.execute("ROLLBACK").await;
            let _ = record::unlock(admin).await;
        }
    }

    /// Refuses when the claim's run is still running, cleans it when that run
    /// is gone. Objects with no claim are left to the emptiness check.
    pub(super) async fn recover_supplied(&mut self, admin: &mut Conn) -> Result<(), Error> {
        pinned(admin, &self.backend).await?;
        let read = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        // The checks that follow refuse an account that does not own it.
        if !sql::owns_database(admin)
            .await
            .map_err(read("the scratch database's owner"))?
        {
            return Ok(());
        }
        self.lock_database(admin).await?;
        let claim = match record::read_claim(admin)
            .await
            .map_err(read("the scratch database's claim"))?
        {
            None => return Ok(()),
            Some(Ok(claim)) => claim,
            Some(Err(why)) => {
                return Err(Error::Unusable(format!(
                    "the scratch database holds a `{0}` schema that {why}; it is not removed, \
                     since nothing says whose it is. Drop it with DROP SCHEMA {0} CASCADE if it \
                     is not in use",
                    record::CLAIM
                )));
            }
        };
        let database = current_database(admin).await?;
        let mut claimed = Claimed {
            server: Server {
                admin,
                backend: &self.backend,
                own: &self.token,
            },
            standard: self.standard,
            claim: &claim,
            database: &database,
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

/// Entry points for the tests that drive a sweep without a run around it.
#[cfg(test)]
pub(super) mod for_tests {
    use super::*;

    pub(in crate::resolver::server) async fn sweep_cluster_on(
        admin: &mut Conn,
        own: &str,
    ) -> recovery::Report {
        let backend = sql::backend(admin).await.unwrap();
        recovery::sweep(&mut Server {
            admin,
            backend: &backend,
            own,
        })
        .await
    }
}
