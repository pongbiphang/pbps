//! PostgreSQL's run-local principals: the roles that stand in for the target's
//! on scratch, and how the deployer's authorization is reproduced with them
//! (moved from `scope.rs`, whose functions were this engine's alone).
//!
//! The two engines do not reproduce a plan's preceding grants the same way.
//! PostgreSQL records a grant under a grantor the engine chooses, so the
//! reproduction projects the grants onto the target's context and verifies
//! itself against that projection (`settle`), after the grants have run here
//! as the mapped deployer (`prepare`). SQL Server refuses a grant the deployer
//! cannot make outright, so its adapter verifies the reproduction against the
//! target as read and only then lets the engine run the grants as the
//! reproduced deployer, with no projection to keep in step with it
//! (DECISIONS 520, 521).

use crate::resolver::scope::PlannedGrant;
use pbps_db::DbError;
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_pg::resolver::authorization as pg_auth;

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

/// `login` is the scratch run login and `token` the run's unique suffix.
pub(super) fn generate(
    context: &pg_auth::AuthorizationContext,
    planned: &[PlannedGrant],
    login: &str,
    token: &str,
) -> pg_auth::RoleMap {
    pg_auth::RoleMap::generate(context, &pg_planned(planned), login, token)
}

/// The map of a supplied scratch login that reproduces no role: it
/// stands for the deployer (#1672).
pub(super) fn supplied(context: &pg_auth::AuthorizationContext, login: &str) -> pg_auth::RoleMap {
    pg_auth::RoleMap::supplied(context, login)
}

/// The principal the compilation session switches to.
pub(super) fn deployer(
    map: &pg_auth::RoleMap,
    context: &pg_auth::AuthorizationContext,
) -> Result<Option<pbps_model::RoleName>, &'static str> {
    map.deployer(context)
        .map(Some)
        .ok_or("no run-local deployer role was mapped")
}

/// Builds the reproduction through an administrative connection in the
/// scratch database: principals, memberships, schemas, grants and login
/// defaults, and runs the plan's grants here, as the mapped deployer.
pub(super) async fn prepare(
    admin: &mut impl ExecuteConnection,
    map: &pg_auth::RoleMap,
    context: &pg_auth::AuthorizationContext,
    planned: &[PlannedGrant],
    database: &str,
) -> Result<(), DbError> {
    pg_auth::reconstruct(admin, map, context, database).await?;
    let deployer = map
        .deployer(context)
        .ok_or_else(|| DbError::BadRow("no run-local deployer role was mapped".into()))?;
    pg_auth::apply_planned(admin, map, &deployer, &pg_planned(planned)).await
}

/// Switches the scratch session to the reproduced deployer. Idempotent, so a
/// later check may call it again on the same session.
pub(super) async fn enter(
    session: &mut impl QueryConnection,
    deployer: Option<&pbps_model::RoleName>,
) -> Result<(), DbError> {
    match deployer {
        None => Ok(()),
        Some(deployer) => session
            .query(&format!(
                "SET ROLE \"{}\"",
                deployer.raw().replace('"', "\"\"")
            ))
            .await
            .map(|_| ()),
    }
}

/// Verifies the reproduction as the reproduced deployer, which the session
/// has [`enter`]ed, and names every key that does not reproduce the target.
/// An empty result is a faithful reproduction with the plan's grants run.
pub(super) async fn settle(
    session: &mut impl QueryConnection,
    map: &pg_auth::RoleMap,
    context: &pg_auth::AuthorizationContext,
    planned: &[PlannedGrant],
    schemas: &[String],
) -> Result<Vec<String>, DbError> {
    let expected = pg_auth::with_planned(context.clone(), &pg_planned(planned));
    pg_auth::verify(session, map, &expected, schemas).await
}
