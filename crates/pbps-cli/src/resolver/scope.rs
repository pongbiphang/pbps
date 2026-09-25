//! Engine routing for analysis-scope qualification (ADR-0016 cases 5, 14, 16,
//! 21, 23; SPEC §9.3.3).
//!
//! The resolver lifecycle — which session is opened when, what is sealed,
//! what a later check re-reads — is the CLI's and is the same for every
//! engine. What a scope *is* belongs to the engine crates: the facts they
//! read, the versioned rule that compares them, the deployment authorization
//! context and how it is reproduced on scratch. This module is the one place
//! that names an engine for that work, so the lifecycle in `server` and the
//! target binding in `native` name none, and adding an engine is a compile
//! error in every function here rather than a branch someone forgot
//! (docs/ARCHITECTURE.md; #714).
//!
//! The two engines do not reproduce a plan's preceding grants the same way,
//! and the difference is kept behind [`prepare`] and [`settle`]. PostgreSQL
//! records a grant under a grantor the engine chooses, so its adapter
//! projects the grants onto the target's context and verifies the
//! reproduction against that projection. SQL Server refuses a grant the
//! deployer cannot make outright, so its adapter verifies the reproduction
//! against the target as read and only then lets the engine run the grants
//! as the reproduced deployer — no projection to keep in step with it
//! (DECISIONS 520, 521).

use pbps_db::fingerprint::FingerprintKey;
use pbps_db::resolver::Observation;
use pbps_db::resolver::environment::{
    CatalogFacts, DatabaseRecipe, EnvironmentFacts, RecipeUnavailable, ScopeReport,
};
use pbps_db::transport::QueryConnection;
use pbps_db::{DbError, Driver};
use pbps_mssql::resolver::authorization as mssql_auth;
use pbps_pg::resolver::authorization as pg_auth;
use std::collections::BTreeMap;

/// A schema grant or revoke the plan performs before its DDL, in the shape
/// both emitters produce. `principal` is the role or user it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGrant {
    pub principal: String,
    pub schema: String,
    pub privilege: String,
    pub revoke: bool,
}

fn pg_planned(planned: &[PlannedGrant]) -> Vec<pg_auth::PlannedGrant> {
    planned
        .iter()
        .map(|grant| pg_auth::PlannedGrant {
            role: grant.principal.clone(),
            schema: grant.schema.clone(),
            privilege: grant.privilege.clone(),
            revoke: grant.revoke,
        })
        .collect()
}

fn mssql_planned(planned: &[PlannedGrant]) -> Vec<mssql_auth::PlannedGrant> {
    planned
        .iter()
        .map(|grant| mssql_auth::PlannedGrant {
            principal: grant.principal.clone(),
            schema: grant.schema.clone(),
            permission: grant.privilege.clone(),
            revoke: grant.revoke,
        })
        .collect()
}

/// The deployment authorization context of one engine.
#[derive(Debug, Clone)]
pub enum Authorization {
    Postgres(pg_auth::AuthorizationContext),
    Mssql(mssql_auth::AuthorizationContext),
}

/// The run-local principals that stand in for the target's on scratch.
#[derive(Debug, Clone)]
pub enum Principals {
    Postgres(pg_auth::RoleMap),
    Mssql(mssql_auth::PrincipalMap),
}

/// The authorization context's digest, keyed (DEC-952.1). Held and compared
/// within this run only, so the process key serves; sealing one into a plan
/// (#614) must use the environment's key.
fn digest(bytes: &[u8]) -> String {
    FingerprintKey::process()
        .fingerprint(AUTHORIZATION_RULE, "authorization", bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The label authorization digests are keyed under; distinct from the capture
/// rule's so that no authorization digest equals a catalog one.
const AUTHORIZATION_RULE: &str = "pbps/authorization-context/v1";

/// The schemas whose authorization is read and reproduced for a request:
/// the in-scope schemas and, on PostgreSQL, the write-path extras, without
/// the `pg_temp` alias, which names no persistent schema. SQL Server has no
/// search path — a bare name binds in its own schema and then in `dbo` — so
/// a request that carries extras for it describes a path that does not
/// exist, and is refused rather than silently dropped.
pub(crate) fn scope_schemas(
    driver: Driver,
    schemas: &[String],
    extras: &[String],
) -> Result<Vec<String>, String> {
    match driver {
        Driver::Postgres => {
            let mut all = schemas.to_vec();
            for extra in extras {
                if extra != "pg_temp" && !all.contains(extra) {
                    all.push(extra.clone());
                }
            }
            Ok(all)
        }
        Driver::Mssql if extras.is_empty() => Ok(schemas.to_vec()),
        Driver::Mssql => Err(format!(
            "SQL Server has no write path, so a scope cannot carry write-path extras: {}",
            extras.join(", ")
        )),
    }
}

/// The catalog facts of one side, as the connection's current principal.
pub(crate) async fn read_catalog(
    connection: &mut impl QueryConnection,
    driver: Driver,
    schemas: &[String],
    extras: &[String],
) -> Result<CatalogFacts, DbError> {
    match driver {
        Driver::Postgres => {
            let scope = pbps_pg::resolver::environment::Scope {
                schemas,
                write_path_extras: extras,
            };
            pbps_pg::resolver::environment::read(connection, &scope).await
        }
        Driver::Mssql => {
            let scope = pbps_mssql::resolver::environment::Scope { schemas };
            pbps_mssql::resolver::environment::read(connection, &scope).await
        }
    }
}

/// The target's catalog facts and its deployer's authorization context, read
/// together so the sealed scope never holds half of a change: one catalog
/// snapshot on PostgreSQL, a bracketed double read on SQL Server, whose
/// catalog views no transaction makes a snapshot.
pub(crate) async fn read_target(
    connection: &mut impl QueryConnection,
    driver: Driver,
    schemas: &[String],
    extras: &[String],
    authorization_schemas: &[String],
    planned: &[PlannedGrant],
) -> Result<(CatalogFacts, Authorization), DbError> {
    match driver {
        Driver::Postgres => {
            let scope = pbps_pg::resolver::environment::Scope {
                schemas,
                write_path_extras: extras,
            };
            let (catalog, authorization) =
                pbps_pg::resolver::scope_facts(connection, &scope, authorization_schemas).await?;
            Ok((catalog, Authorization::Postgres(authorization)))
        }
        Driver::Mssql => {
            let scope = pbps_mssql::resolver::environment::Scope { schemas };
            // The planned grants' principals are resolved to the catalog's
            // spelling within each bracketed read (#726).
            let names: Vec<String> = planned.iter().map(|g| g.principal.clone()).collect();
            let (catalog, authorization) = pbps_mssql::resolver::scope_facts(
                connection,
                &scope,
                authorization_schemas,
                &names,
            )
            .await?;
            Ok((catalog, Authorization::Mssql(authorization)))
        }
    }
}

/// How the scratch database must be created to stand in for the target's.
pub(crate) fn recipe(
    driver: Driver,
    catalog: &CatalogFacts,
) -> Result<DatabaseRecipe, RecipeUnavailable> {
    match driver {
        Driver::Postgres => DatabaseRecipe::from_catalog(catalog),
        Driver::Mssql => DatabaseRecipe::from_sql_server_catalog(catalog),
    }
}

/// The native libraries a scope requires the engine to have loaded or be
/// able to load, by the names the engine uses for them. SQL Server names
/// none: its in-process code for a database is CLR assemblies, whose content
/// lives in the catalog and is compared there, not files a loader resolves.
pub(crate) fn required_libraries(driver: Driver, catalog: &CatalogFacts) -> Vec<String> {
    match driver {
        Driver::Postgres => super::native::executables::required_libraries(catalog),
        Driver::Mssql => Vec::new(),
    }
}

/// The directories a bare library name is searched along.
pub(crate) fn library_path(driver: Driver, catalog: &CatalogFacts) -> String {
    match driver {
        Driver::Postgres => catalog
            .settings
            .get("dynamic_library_path")
            .map(|fact| fact.value.clone())
            .unwrap_or_else(|| "$libdir".to_owned()),
        Driver::Mssql => String::new(),
    }
}

/// File suffixes, beside shared objects, that are engine code when mapped.
/// SQL Server for Linux runs its Windows binaries out of `.sfp` packages the
/// platform layer maps into the process (measured on 17.0: `sqlservr.sfp`,
/// `system.common.sfp` and others, thousands of ranges), so the ELF at
/// `/proc/<pid>/exe` is only the loader and the packages are the engine: a
/// different build with the same loader differs there and nowhere else.
pub(crate) fn engine_packages(driver: Driver) -> &'static [&'static str] {
    match driver {
        Driver::Postgres => &[],
        Driver::Mssql => &[".sfp"],
    }
}

/// The versioned compatibility rule of the engine, over two sides' facts.
pub(crate) fn compare(
    driver: Driver,
    target: &EnvironmentFacts,
    resolver: &EnvironmentFacts,
) -> ScopeReport {
    match driver {
        Driver::Postgres => pbps_pg::resolver::compatibility::compare(target, resolver, &[]),
        Driver::Mssql => pbps_mssql::resolver::compatibility::compare(target, resolver, &[]),
    }
}

impl Authorization {
    /// The name of the versioned authorization rule the context was read
    /// under.
    pub(crate) fn rule(&self) -> &'static str {
        match self {
            Self::Postgres(_) => pg_auth::RULE,
            Self::Mssql(_) => mssql_auth::RULE,
        }
    }

    /// The digest of the context as read, before any planned grant.
    pub(crate) fn digest(&self) -> String {
        match self {
            Self::Postgres(context) => digest(&context.canonical()),
            Self::Mssql(context) => digest(&context.canonical()),
        }
    }

    /// The digest the scope is sealed and fingerprinted under: the context
    /// the deployment statements are meant to run in, after the plan's
    /// preceding grants. PostgreSQL projects the grants onto the context; SQL
    /// Server seals the context as read together with the grants themselves.
    pub(crate) fn sealed_digest(&self, planned: &[PlannedGrant]) -> String {
        match self {
            Self::Postgres(context) => {
                digest(&pg_auth::with_planned(context.clone(), &pg_planned(planned)).canonical())
            }
            Self::Mssql(context) => {
                let mut bytes = context.canonical();
                bytes.extend(
                    serde_json::to_vec(&mssql_planned(planned)).expect("planned grants serialise"),
                );
                digest(&bytes)
            }
        }
    }

    /// Planned grants whose outcome the adapter cannot stand behind, named.
    /// PostgreSQL's is a grantor the engine would pick among several
    /// inherited option holders; SQL Server runs the grants through the
    /// engine and has nothing to predict.
    pub(crate) fn unpredictable(&self, planned: &[PlannedGrant]) -> Vec<String> {
        match self {
            Self::Postgres(context) => pg_auth::ambiguous_grantors(context, &pg_planned(planned)),
            Self::Mssql(_) => Vec::new(),
        }
    }

    /// Rewrites the target's visibility facts to what the deployer is meant
    /// to see once the plan's grants have run. On PostgreSQL a planned USAGE
    /// grant changes the effective search order, so it is derived from the
    /// projected context; on SQL Server a grant changes no binding order, so
    /// the facts stand as read.
    pub(crate) fn project_visibility(
        &self,
        planned: &[PlannedGrant],
        schemas: &[String],
        extras: &[String],
        catalog: &mut CatalogFacts,
    ) {
        match self {
            Self::Postgres(context) => {
                let expected = pg_auth::with_planned(context.clone(), &pg_planned(planned));
                catalog.visibility = expected_visibility(&expected, schemas, extras);
            }
            Self::Mssql(_) => {}
        }
    }
}

impl Principals {
    /// `login` is the scratch run login and `token` the run's unique suffix.
    pub(crate) fn generate(
        authorization: &Authorization,
        planned: &[PlannedGrant],
        login: &str,
        token: &str,
    ) -> Self {
        match authorization {
            Authorization::Postgres(context) => Self::Postgres(pg_auth::RoleMap::generate(
                context,
                &pg_planned(planned),
                login,
                token,
            )),
            Authorization::Mssql(context) => Self::Mssql(mssql_auth::PrincipalMap::generate(
                context,
                &mssql_planned(planned),
                token,
            )),
        }
    }

    /// The server-wide principals this run creates and must drop. SQL
    /// Server's are database-scoped and go with the scratch database.
    pub(crate) fn server_wide_names(&self) -> Vec<String> {
        match self {
            Self::Postgres(map) => map.run_local_names(),
            Self::Mssql(_) => Vec::new(),
        }
    }

    /// The principal the compilation session switches to, or `None` when the
    /// session is already it (a SQL Server `dbo` deployer: the run login owns
    /// its scratch database).
    pub(crate) fn deployer(
        &self,
        authorization: &Authorization,
    ) -> Result<Option<String>, &'static str> {
        match (self, authorization) {
            (Self::Postgres(map), Authorization::Postgres(context)) => map
                .deployer(context)
                .map(Some)
                .ok_or("no run-local deployer role was mapped"),
            (Self::Mssql(map), Authorization::Mssql(context)) => {
                if context.principal.superuser {
                    Ok(None)
                } else {
                    map.deployer(context)
                        .map(Some)
                        .ok_or("no run-local deployer user was mapped")
                }
            }
            (Self::Postgres(_), Authorization::Mssql(_))
            | (Self::Mssql(_), Authorization::Postgres(_)) => {
                Err("the principal map belongs to another engine")
            }
        }
    }
}

const MIXED: &str = "the authorization context and the principal map belong to different engines";

/// Builds the reproduction through an administrative connection in the
/// scratch database: principals, memberships, schemas, grants and login
/// defaults. PostgreSQL also runs the plan's grants here, as the mapped
/// deployer; SQL Server runs them in [`settle`], after the reproduction has
/// been verified.
pub(crate) async fn prepare(
    admin: &mut impl QueryConnection,
    principals: &Principals,
    authorization: &Authorization,
    planned: &[PlannedGrant],
    database: &str,
    run_login: &str,
) -> Result<(), DbError> {
    match (principals, authorization) {
        (Principals::Postgres(map), Authorization::Postgres(context)) => {
            pg_auth::reconstruct(admin, map, context, database).await?;
            let deployer = map
                .deployer(context)
                .ok_or_else(|| DbError::BadRow("no run-local deployer role was mapped".into()))?;
            pg_auth::apply_planned(admin, map, &deployer, &pg_planned(planned)).await
        }
        (Principals::Mssql(map), Authorization::Mssql(context)) => {
            mssql_auth::reconstruct(admin, map, context, run_login).await
        }
        _ => Err(DbError::BadRow(MIXED.into())),
    }
}

/// Switches the scratch session to the reproduced deployer. Idempotent, so a
/// later check may call it again on the same session.
pub(crate) async fn enter(
    session: &mut impl QueryConnection,
    driver: Driver,
    deployer: Option<&str>,
) -> Result<(), DbError> {
    match (driver, deployer) {
        (_, None) => Ok(()),
        (Driver::Postgres, Some(deployer)) => session
            .query(&format!("SET ROLE \"{}\"", deployer.replace('"', "\"\"")))
            .await
            .map(|_| ()),
        (Driver::Mssql, Some(deployer)) => mssql_auth::enter(session, Some(deployer)).await,
    }
}

/// Verifies the reproduction as the reproduced deployer, which the session
/// has [`enter`]ed, and names every key that does not reproduce the target.
/// An empty result is a faithful reproduction with the plan's grants run.
pub(crate) async fn settle(
    session: &mut impl QueryConnection,
    principals: &Principals,
    authorization: &Authorization,
    planned: &[PlannedGrant],
    schemas: &[String],
) -> Result<Vec<String>, DbError> {
    match (principals, authorization) {
        (Principals::Postgres(map), Authorization::Postgres(context)) => {
            let expected = pg_auth::with_planned(context.clone(), &pg_planned(planned));
            pg_auth::verify(session, map, &expected, schemas).await
        }
        (Principals::Mssql(map), Authorization::Mssql(context)) => {
            let differences = mssql_auth::verify(session, map, context, schemas).await?;
            if differences.is_empty() {
                mssql_auth::apply_planned(session, map, &mssql_planned(planned)).await?;
            }
            Ok(differences)
        }
        _ => Err(DbError::BadRow(MIXED.into())),
    }
}

/// The digest of the scratch side's own authorization context, as the
/// reproduced deployer reads it once the scope has settled.
pub(crate) async fn seal_scratch(
    session: &mut impl QueryConnection,
    driver: Driver,
    schemas: &[String],
) -> Result<String, DbError> {
    Ok(match driver {
        Driver::Postgres => digest(&pg_auth::read(session, schemas).await?.canonical()),
        Driver::Mssql => digest(&mssql_auth::read(session, schemas).await?.canonical()),
    })
}

/// A later check of the scratch side against a freshly read target context.
/// PostgreSQL verifies the reproduction against the projection again. SQL
/// Server's scratch has had the plan's grants run on it, so it no longer
/// equals the target as read; it is compared with what was sealed when the
/// scope settled instead, and any drift is named.
pub(crate) async fn recheck(
    session: &mut impl QueryConnection,
    principals: &Principals,
    authorization: &Authorization,
    planned: &[PlannedGrant],
    schemas: &[String],
    sealed_scratch: &str,
) -> Result<Vec<String>, DbError> {
    match (principals, authorization) {
        (Principals::Postgres(map), Authorization::Postgres(context)) => {
            let expected = pg_auth::with_planned(context.clone(), &pg_planned(planned));
            pg_auth::verify(session, map, &expected, schemas).await
        }
        (Principals::Mssql(_), Authorization::Mssql(_)) => {
            let current = digest(&mssql_auth::read(session, schemas).await?.canonical());
            Ok(if current == sealed_scratch {
                Vec::new()
            } else {
                vec!["scratch".to_owned()]
            })
        }
        _ => Err(DbError::BadRow(MIXED.into())),
    }
}

/// The effective schema order a PostgreSQL deployer would see on each
/// in-scope path, derived from the (post-plan) authorization: `pg_catalog`
/// first, then each of the path's schemas the deployer has USAGE on, in
/// order. This is what `current_schemas(true)` returns once the temporary
/// namespaces are stripped, so a reproduction that grants the same USAGE
/// matches it, and a planned USAGE change is compared against its intended
/// effect.
fn expected_visibility(
    auth: &pg_auth::AuthorizationContext,
    paths: &[String],
    extras: &[String],
) -> BTreeMap<String, Observation> {
    paths
        .iter()
        .map(|start| {
            let mut visible = vec!["pg_catalog".to_owned()];
            for schema in std::iter::once(start).chain(extras.iter()) {
                let usable = auth
                    .schemas
                    .get(schema)
                    .and_then(|s| s.privileges.get("USAGE"))
                    .copied()
                    .unwrap_or(false);
                // `current_schemas` lists each namespace once, at its first
                // occurrence, so a schema repeated on the path (or an extra
                // equal to the start) is not duplicated here (finding on #688).
                if usable && !visible.contains(schema) {
                    visible.push(schema.clone());
                }
            }
            let rendered = pbps_db::resolver::environment::render_visibility(&visible);
            (start.clone(), Observation::reported(Some(&rendered)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_visibility_is_pg_catalog_then_the_usable_path_schemas_in_order() {
        use pbps_pg::resolver::authorization::{AuthorizationContext, SchemaAuthorization};
        let schema = |usage: bool| SchemaAuthorization {
            owner: "o".into(),
            privileges: [("USAGE".to_owned(), usage), ("CREATE".to_owned(), false)]
                .into_iter()
                .collect(),
            acl: BTreeMap::new(),
        };
        let context = AuthorizationContext {
            principal: pbps_db::resolver::environment::DeploymentPrincipal {
                login: "d".into(),
                effective: "d".into(),
                superuser: false,
            },
            schemas: [
                ("app".to_owned(), schema(true)),
                ("secret".to_owned(), schema(false)),
                ("ext".to_owned(), schema(true)),
            ]
            .into_iter()
            .collect(),
            roles: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        let extras = vec!["ext".to_owned()];
        let visibility =
            expected_visibility(&context, &["app".to_owned(), "secret".to_owned()], &extras);
        // app is usable and ext (an extra) is usable, in path order after pg_catalog.
        assert_eq!(
            visibility["app"],
            Observation::reported(Some(r#"["pg_catalog","app","ext"]"#))
        );
        // secret is not usable, so only pg_catalog and the usable extra remain.
        assert_eq!(
            visibility["secret"],
            Observation::reported(Some(r#"["pg_catalog","ext"]"#))
        );
    }

    /// SQL Server has no write path; PostgreSQL's carries its extras into
    /// the authorization scope without the `pg_temp` alias or a duplicate.
    #[test]
    fn the_authorization_scope_is_the_engines_own() {
        let schemas = ["app".to_owned()];
        let extras = ["ext".to_owned(), "pg_temp".to_owned(), "app".to_owned()];
        assert_eq!(
            scope_schemas(Driver::Postgres, &schemas, &extras).unwrap(),
            ["app", "ext"]
        );
        assert_eq!(
            scope_schemas(Driver::Mssql, &schemas, &[]).unwrap(),
            ["app"]
        );
        let refused = scope_schemas(Driver::Mssql, &schemas, &extras).unwrap_err();
        assert!(refused.contains("no write path"), "{refused}");
        // The engine's code beside shared objects is the engine's to name.
        assert!(engine_packages(Driver::Postgres).is_empty());
        assert_eq!(engine_packages(Driver::Mssql), [".sfp"]);
    }

    /// The sealed digest moves with a planned grant on either engine, and
    /// the raw digest does not: the two are different facts.
    #[test]
    fn a_planned_grant_moves_the_sealed_digest_and_not_the_raw_one() {
        let planned = [PlannedGrant {
            principal: "PUBLIC".into(),
            schema: "app".into(),
            privilege: "USAGE".into(),
            revoke: false,
        }];
        let postgres = Authorization::Postgres(pg_auth::AuthorizationContext {
            principal: pbps_db::resolver::environment::DeploymentPrincipal {
                login: "d".into(),
                effective: "d".into(),
                superuser: true,
            },
            schemas: [(
                "app".to_owned(),
                pg_auth::SchemaAuthorization {
                    owner: "d".into(),
                    privileges: BTreeMap::new(),
                    acl: BTreeMap::new(),
                },
            )]
            .into_iter()
            .collect(),
            roles: BTreeMap::new(),
            settings: BTreeMap::new(),
        });
        assert_ne!(
            postgres.sealed_digest(&planned),
            postgres.sealed_digest(&[])
        );
        assert_eq!(postgres.digest(), postgres.clone().digest());
        assert_eq!(postgres.rule(), "pg-auth-v1");
    }
}
