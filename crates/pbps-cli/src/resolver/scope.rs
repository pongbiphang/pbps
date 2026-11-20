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

pub(crate) mod object_grants;

use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Driver};
use pbps_mssql::resolver::authorization as mssql_auth;
use pbps_pg::resolver::authorization as pg_auth;

/// A schema grant or revoke the plan performs before its DDL. A GRANT keeps
/// the emitter's whole comma-separated privilege mask in one entry; REVOKE
/// has one privilege per entry because PostgreSQL emits separate statements.
/// `principal` is the role or user the statement names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGrant {
    pub principal: pbps_model::RoleName,
    pub schema: String,
    pub privilege: String,
    pub revoke: bool,
}

pub(in crate::resolver) fn pg_planned(planned: &[PlannedGrant]) -> Vec<pg_auth::PlannedGrant> {
    planned
        .iter()
        .map(|grant| pg_auth::PlannedGrant {
            role: grant.principal.raw().to_owned(),
            schema: grant.schema.clone(),
            privilege: grant.privilege.clone(),
            revoke: grant.revoke,
        })
        .collect()
}

pub(in crate::resolver) fn mssql_planned(
    planned: &[PlannedGrant],
) -> Vec<mssql_auth::PlannedGrant> {
    // SQL Server's reproduction retains its original per-permission shape.
    planned
        .iter()
        .flat_map(|grant| {
            grant
                .privilege
                .split(", ")
                .map(|permission| mssql_auth::PlannedGrant {
                    principal: grant.principal.clone(),
                    schema: grant.schema.clone(),
                    permission: permission.to_owned(),
                    revoke: grant.revoke,
                })
        })
        .collect()
}

/// The run-local principals that stand in for the target's on scratch.
#[derive(Debug, Clone)]
pub enum Principals {
    Postgres(pg_auth::RoleMap),
    Mssql(mssql_auth::PrincipalMap),
}

/// The label authorization digests are keyed under; distinct from the capture
/// rule's so that no authorization digest equals a catalog one.
const AUTHORIZATION_RULE: &str = "pbps/authorization-context/v1";

/// Seals a deployer context, given as its engine's canonical bytes, together
/// with the selected object-grant answers. Their keyed fingerprints use the
/// same endpoint as the context, and remain separate from catalog binding
/// inputs. The producer seals with it and apply compares with it, so the two
/// cannot frame a context differently.
pub(crate) fn authorization_digest(
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    canonical: &[u8],
    grants: &[String],
) -> String {
    if grants.is_empty() {
        return crate::resolver::sealing::hex(key.fingerprint(
            AUTHORIZATION_RULE,
            "authorization",
            canonical,
        ));
    }
    let combined =
        serde_json::to_vec(&(canonical, grants)).expect("authorization condition serialises");
    crate::resolver::sealing::hex(key.fingerprint(
        "pbps/authorization-with-object-grants/v1",
        "authorization",
        &combined,
    ))
}

/// The ordered schema-grant sequence whose projection the qualified scope
/// used. Compare it with the final pure planner changes before persisting
/// either authorization or target visibility.
pub(crate) fn planned_schema_grants(
    changes: &pbps_model::ChangeSet,
) -> Result<Vec<PlannedGrant>, String> {
    use pbps_model::GrantChange;
    use pbps_model::IndexChange;
    use pbps_model::PartitionChange;
    use pbps_model::{Change, GrantTarget};
    let mut planned = Vec::new();
    for p in &changes.changes {
        if matches!(p.change, Change::Role(_)) {
            return Err(
                "persistent resolver authorization cannot project role identity changes".into(),
            );
        }
        let statement = match &p.change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
                return Err(format!(
                    "blocking rewrite for `{}` is unavailable",
                    p.change.table().unwrap().shown()
                ));
            }
            Change::Permission(GrantChange::Grant {
                role,
                target: GrantTarget::Schema(schema),
                permissions,
            })
            | Change::Permission(GrantChange::Revoke {
                role,
                target: GrantTarget::Schema(schema),
                permissions,
            }) => Some((role, schema, permissions)),
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::Policy(_)
            | Change::TableProperty(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Constraint(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        };
        if let Some((role, schema, permissions)) = statement {
            let privileges = permissions
                .iter()
                .map(|permission| permission.as_str().to_ascii_uppercase().replace('-', " "))
                .collect::<Vec<_>>();
            if privileges.is_empty() {
                return Err("a planned schema grant names no permission".into());
            }
            if matches!(&p.change, Change::Permission(GrantChange::Grant { .. })) {
                planned.push(PlannedGrant {
                    principal: role.clone(),
                    schema: schema.clone(),
                    privilege: privileges.join(", "),
                    revoke: false,
                });
            } else {
                // PostgreSQL emits one REVOKE statement per permission.
                for privilege in privileges {
                    planned.push(PlannedGrant {
                        principal: role.clone(),
                        schema: schema.clone(),
                        privilege,
                        revoke: true,
                    });
                }
            }
        }
    }
    Ok(planned)
}

/// The persisted authorization condition: what the plan's schema grants and
/// revokes do to the deployer's context, digested before and after. The shell
/// is the same for every engine; what the context becomes is the engine's
/// (`transition`): PostgreSQL projects the planned grants onto it, and SQL
/// Server takes what the verified reproduction measured, and refuses without it.
pub(crate) fn persisted_after<E: crate::resolver::contract::ResolverEngine>(
    authorization: &E::Authorization,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    changes: &pbps_model::ChangeSet,
    object_grants: Vec<pbps_model::resolver::AuthorizationGrant>,
    measured: Option<&E::Condition>,
) -> Result<pbps_model::resolver::AuthorizationCondition, String> {
    use pbps_model::Change;
    let planned = planned_schema_grants(changes)?;
    if !E::unpredictable(authorization, &planned).is_empty() {
        return Err("the adapter cannot derive the approved authorization transition".into());
    }
    let (before, after) = E::transition(authorization, &planned, measured)?;
    let before_grants: Vec<_> = object_grants
        .iter()
        .map(|g| g.before.fingerprint.clone())
        .collect();
    let after_grants: Vec<_> = object_grants
        .iter()
        .map(|g| g.after.fingerprint.clone())
        .collect();
    Ok(pbps_model::resolver::AuthorizationCondition {
        rule: E::authorization_rule()?.into(),
        before: authorization_digest(key, &before, &before_grants),
        after: authorization_digest(key, &after, &after_grants),
        object_grants,
        changes: changes
            .changes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                matches!(
                    p.change,
                    Change::Permission(_)
                        | Change::TypeValue(_)
                        | Change::Type(_)
                        | Change::Role(_)
                )
            })
            .map(|(i, _)| i)
            .collect(),
    })
}

impl Principals {
    /// `login` is the scratch run login and `token` the run's unique suffix.
    pub(crate) fn generate(
        context: &pg_auth::AuthorizationContext,
        planned: &[PlannedGrant],
        login: &str,
        token: &str,
    ) -> Self {
        Self::Postgres(pg_auth::RoleMap::generate(
            context,
            &pg_planned(planned),
            login,
            token,
        ))
    }

    /// The map of a supplied scratch login that reproduces no role: it
    /// stands for the deployer (#1672).
    pub(crate) fn supplied(context: &pg_auth::AuthorizationContext, login: &str) -> Self {
        Self::Postgres(pg_auth::RoleMap::supplied(context, login))
    }

    /// The server-wide principals this run creates and must drop. SQL
    /// Server's are database-scoped and go with the scratch database.
    pub(crate) fn server_wide_names(&self) -> Vec<pbps_model::RoleName> {
        match self {
            Self::Postgres(map) => map.run_local_names(),
            Self::Mssql(_) => Vec::new(),
        }
    }

    /// The principal the compilation session switches to.
    pub(crate) fn deployer(
        &self,
        context: &pg_auth::AuthorizationContext,
    ) -> Result<Option<pbps_model::RoleName>, &'static str> {
        match self {
            Self::Postgres(map) => map
                .deployer(context)
                .map(Some)
                .ok_or("no run-local deployer role was mapped"),
            Self::Mssql(_) => Err("the principal map belongs to another engine"),
        }
    }
}

const MIXED: &str = "the authorization context and the principal map belong to different engines";

/// Builds the reproduction through an administrative connection in the
/// scratch database: principals, memberships, schemas, grants and login
/// defaults, and runs the plan's grants here, as the mapped deployer.
pub(crate) async fn prepare(
    admin: &mut impl ExecuteConnection,
    principals: &Principals,
    context: &pg_auth::AuthorizationContext,
    planned: &[PlannedGrant],
    database: &str,
) -> Result<(), DbError> {
    match principals {
        Principals::Postgres(map) => {
            pg_auth::reconstruct(admin, map, context, database).await?;
            let deployer = map
                .deployer(context)
                .ok_or_else(|| DbError::BadRow("no run-local deployer role was mapped".into()))?;
            pg_auth::apply_planned(admin, map, &deployer, &pg_planned(planned)).await
        }
        Principals::Mssql(_) => Err(DbError::BadRow(MIXED.into())),
    }
}

/// Switches the scratch session to the reproduced deployer. Idempotent, so a
/// later check may call it again on the same session.
pub(crate) async fn enter(
    session: &mut impl QueryConnection,
    driver: Driver,
    deployer: Option<&pbps_model::RoleName>,
) -> Result<(), DbError> {
    match (driver, deployer) {
        (_, None) => Ok(()),
        (Driver::Postgres, Some(deployer)) => session
            .query(&format!(
                "SET ROLE \"{}\"",
                deployer.raw().replace('"', "\"\"")
            ))
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
    context: &pg_auth::AuthorizationContext,
    planned: &[PlannedGrant],
    schemas: &[String],
) -> Result<Vec<String>, DbError> {
    match principals {
        Principals::Postgres(map) => {
            let expected = pg_auth::with_planned(context.clone(), &pg_planned(planned));
            pg_auth::verify(session, map, &expected, schemas).await
        }
        Principals::Mssql(_) => Err(DbError::BadRow(MIXED.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::server::PlanningDialect;
    use std::collections::BTreeMap;

    type Pg = <pbps_pg::Postgres as PlanningDialect>::Resolver;

    #[cfg(unix)]
    fn fixed_key() -> pbps_db::fingerprint::EnvironmentFingerprintKey {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path =
            std::env::temp_dir().join(format!("pbps2798-digest-key-{}", rand::random::<u64>()));
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

    pub(super) fn pg_fixture() -> pg_auth::AuthorizationContext {
        pg_auth::AuthorizationContext {
            principal: pbps_db::resolver::environment::DeploymentPrincipal {
                login: "d".into(),
                effective: "d".into(),
                superuser: true,
            },
            schemas: [(
                "app".to_owned(),
                pg_auth::SchemaAuthorization {
                    owner: "d".into(),
                    privileges: BTreeMap::from([("USAGE".into(), true), ("CREATE".into(), true)]),
                    acl: pbps_pg::resolver::authorization::SchemaAcl::Default,
                },
            )]
            .into_iter()
            .collect(),
            roles: BTreeMap::new(),
            settings: BTreeMap::new(),
        }
    }

    /// What apply compares the sealed digest with: the shared framing over the
    /// engine's canonical bytes, with and without object-grant fingerprints.
    /// The expected values were recorded from the union's `digest` before it
    /// was removed (its equality with this framing was asserted then), for a
    /// fixed context of each engine and the fixed key.
    #[cfg(unix)]
    #[test]
    fn the_digest_of_each_engines_context_is_the_shared_framing_over_its_canonical_bytes() {
        let key = fixed_key();
        let pg = pg_fixture().canonical();
        let ms = mssql_auth::fixture_context().canonical();
        let two = ["aa".to_owned(), "bb".to_owned()];
        for (name, canonical, grants, expected) in [
            (
                "postgres",
                &pg,
                &[][..],
                "f0e9dc9ac0184ca6faac24ff7a4601d4c5caaf34332aaeead452f284a5d193e4",
            ),
            (
                "postgres",
                &pg,
                &two[..],
                "c82ec2630df9af38dbe54a2b05b2d1ae474579f3a68a397340daa2b4998451dd",
            ),
            (
                "mssql",
                &ms,
                &[][..],
                "531c26ef913988967c07f2f11ceca3def7e717ce4928570740dae14af64e1a30",
            ),
            (
                "mssql",
                &ms,
                &two[..],
                "cf58e5d043593850885fa7a3ef4f5dd7706003174935a7adeedfd0eefcd81cb7",
            ),
        ] {
            assert_eq!(
                authorization_digest(&key, canonical, grants),
                expected,
                "{name} with {} object grants",
                grants.len()
            );
        }
        // Negative: the two engines' contexts never digest alike.
        assert_ne!(
            authorization_digest(&key, &pg, &[]),
            authorization_digest(&key, &ms, &[])
        );
    }

    /// The persisted after-digest moves with a planned grant, and the
    /// before-digest does not: the two are different facts.
    #[cfg(unix)]
    #[test]
    fn a_planned_grant_moves_the_after_digest_and_not_the_before() {
        let postgres = pg_fixture();
        assert_eq!(
            <Pg as crate::resolver::contract::ResolverEngine>::authorization_rule().unwrap(),
            "pg-auth-v2"
        );
        let key = fixed_key();
        let persisted = |changes: &pbps_model::ChangeSet| {
            persisted_after::<Pg>(&postgres, &key, changes, Vec::new(), None)
        };
        let unchanged = persisted(&pbps_model::ChangeSet::default()).unwrap();
        assert_eq!(unchanged.before, unchanged.after);
        let changes = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::Permission(pbps_model::GrantChange::Grant {
                    role: "PUBLIC".into(),
                    target: pbps_model::GrantTarget::Schema("app".into()),
                    permissions: std::collections::BTreeSet::from([pbps_model::Permission::Usage]),
                }),
            )],
        };
        let projected = persisted(&changes).unwrap();
        assert_eq!(projected.before, unchanged.before);
        assert_ne!(projected.before, projected.after);
        assert_eq!(projected.changes, std::collections::BTreeSet::from([0]));
        let unsupported = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Role(
                pbps_model::RoleChange::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "new_role".into(),
                },
            ))],
        };
        assert!(persisted(&unsupported).is_err());
    }

    /// SQL Server's after-state is what the verified reproduction measured; a
    /// seal without that measurement is refused, never read as "unchanged".
    #[cfg(unix)]
    #[test]
    fn sql_server_refuses_to_persist_an_authorization_it_did_not_measure() {
        type Ms = <pbps_mssql::Mssql as PlanningDialect>::Resolver;
        let context = Box::new(mssql_auth::fixture_context());
        let refused = persisted_after::<Ms>(
            &context,
            &fixed_key(),
            &pbps_model::ChangeSet::default(),
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            refused,
            "the SQL Server authorization after-state was not read from the verified reproduction"
        );
    }

    fn facts() -> pbps_db::resolver::environment::CatalogFacts {
        pbps_db::resolver::environment::CatalogFacts {
            observations: BTreeMap::new(),
            extensions: Vec::new(),
            available_extensions: BTreeMap::new(),
            collations: Vec::new(),
            settings: BTreeMap::new(),
            visibility: BTreeMap::from([(
                "app".to_owned(),
                pbps_db::resolver::Observation::reported(Some("as read")),
            )]),
        }
    }

    /// The recheck compares each engine's complete context by its canonical
    /// bytes, the predicate it had: an unchanged read is the same context, and
    /// a changed field of either engine's is not.
    #[test]
    fn each_engine_compares_its_own_contexts_by_their_canonical_bytes() {
        use crate::resolver::contract::ResolverEngine;
        type Ms = <pbps_mssql::Mssql as PlanningDialect>::Resolver;
        let pg = pg_fixture();
        assert!(Pg::same_context(&pg, &pg.clone()));
        let mut revoked = pg.clone();
        revoked
            .schemas
            .get_mut("app")
            .unwrap()
            .privileges
            .insert("USAGE".into(), false);
        assert!(!Pg::same_context(&pg, &revoked));
        let ms = Box::new(mssql_auth::fixture_context());
        assert!(Ms::same_context(&ms, &ms.clone()));
        let mut moved = ms.clone();
        moved.language = "german".into();
        assert!(!Ms::same_context(&ms, &moved));
    }

    /// PostgreSQL derives the deployer's visibility from the projected
    /// context; SQL Server's grants change no binding order, so its facts
    /// stand as read. Nothing planned on SQL Server is "unpredictable".
    #[test]
    fn only_postgres_rewrites_visibility_and_only_it_can_find_a_grant_unpredictable() {
        use crate::resolver::contract::ResolverEngine;
        type Ms = <pbps_mssql::Mssql as PlanningDialect>::Resolver;
        let planned = [PlannedGrant {
            principal: "d".into(),
            schema: "app".into(),
            privilege: "USAGE".into(),
            revoke: true,
        }];
        let schemas = ["app".to_owned()];
        let mut postgres = facts();
        // A superuser keeps USAGE whatever is revoked; the deployer here does not.
        let mut deployer = pg_fixture();
        deployer.principal.superuser = false;
        Pg::project_visibility(&deployer, &planned, &schemas, &[], &mut postgres);
        assert_eq!(
            postgres.visibility["app"],
            pbps_db::resolver::Observation::reported(Some(r#"["pg_catalog"]"#)),
            "the revoked USAGE leaves only pg_catalog on the path"
        );
        let mut sql_server = facts();
        Ms::project_visibility(
            &Box::new(mssql_auth::fixture_context()),
            &planned,
            &schemas,
            &[],
            &mut sql_server,
        );
        assert_eq!(sql_server, facts());
        assert!(Ms::unpredictable(&Box::new(mssql_auth::fixture_context()), &planned).is_empty());
        assert!(
            Ms::unusable_schemas(&Box::new(mssql_auth::fixture_context()), &planned).is_empty()
        );
    }
}
