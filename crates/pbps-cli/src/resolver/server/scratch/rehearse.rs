//! The scratch session of a connected rehearsal (#2379).
//!
//! A rehearsal acquires the same scratch the resolver does, with the same
//! separation, layout, emptiness and externals checks, and releases it the
//! same way. What it does not do is qualify the scratch against the target or
//! seal anything: it asks whether the plan's statements run, in order, on a
//! reproduction of the target's managed state, and nothing it reads leaves
//! as evidence. So no fingerprint key is needed and no compatibility verdict
//! is reached; the scratch is the operator's choice, and a rehearsal on
//! another build proves syntax and convergence, never the build's behaviour.

use super::super::producer::request;
use super::super::{BindingRequest, Error, ProduceError, engine};
use super::{Provenance, Tokens};
use crate::resolver::scope;
use pbps_db::{Conn, Driver};
use pbps_model::resolver::ScratchLayout;
use pbps_model::role_mapping::RoleMapping;
use pbps_model::{Hints, RoleName};
use std::collections::BTreeSet;

/// The roles the plan names, as the run can have them: under their own names
/// where the scratch is the target's cluster or a cluster of the run's own, and
/// under names of the run's own on a cluster other runs use (`Naming`). `Err`
/// is why a rehearsal there cannot run the plan's authorization, and says so.
pub type Roles = Result<RoleMapping, String>;

/// Runs `run` on a session of the scratch as the target's deployer, with the
/// entry's externals staged and compared with the target, and releases the
/// scratch whatever `run` returns. The layout the run used comes back beside
/// the result: it is the run's own finding, as for the resolver.
///
/// `named` are the roles the plan, or the base it builds, names. How they are
/// named on this scratch is its placement's (`Naming`): the target's own cluster
/// already has them; a container gets those it lacks, under their names; a
/// cluster others use gets a role of the run's own for each. What the run makes
/// is recorded before it is made and removed with the scratch, and `run` is told
/// which names the roles have; or, when it cannot rehearse them, why not.
#[allow(clippy::too_many_arguments)]
pub async fn with_session<T, E>(
    driver: Driver,
    scratch: &str,
    target_connection: &str,
    planned_identity: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    standard: impl Into<engine::Standard>,
    externals: Option<&str>,
    provenance: &Provenance,
    named: &BTreeSet<RoleName>,
    run: impl AsyncFnOnce(&mut Conn, Roles) -> Result<T, E>,
) -> Result<(ScratchLayout, Result<T, E>), ProduceError> {
    // The write path's extras are the dialect's, and the CLI's PostgreSQL
    // dialect configures none, as for the resolver.
    let request = request(driver, base, desired, hints, &[]).map_err(ProduceError::Run)?;
    let standard = standard.into();
    // Every connection of the run opens the one endpoint found here
    // (DEC-2271.2), as for the resolver.
    let scratch = &engine::endpoint(driver, scratch).await?;
    let mut target = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Unusable(format!(
                "the target could not be connected: {error}"
            )))
        })?;
    // The token first: the scratch session carries it from its first packet,
    // and a killed rehearsal is recovered by it (#2410).
    let tokens = Tokens::generate();
    let mut admin = engine::connect_scratch(driver, scratch, &tokens).await?;
    let (provisioning, mut state) = engine::scratch(
        driver,
        &mut target,
        &mut admin,
        &tokens,
        planned_identity,
        scratch,
        &request,
        &standard,
        externals,
        provenance,
    )
    .await?;
    let opened = async {
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let scope_schemas =
            scope::scope_schemas(driver, &request.schemas, &request.write_path_extras)
                .map_err(Error::Scope)?;
        let (target_catalog, authorization) = scope::read_target(
            &mut target,
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
        let mut prepared = state
            .prepare(
                provisioning,
                &mut admin,
                &mut target,
                &target_catalog,
                &authorization,
                &scope_schemas,
                binding,
                &request.write_path_extras,
                None,
            )
            .await?;
        // The externals the base and the plan read are the target's, as for
        // the resolver: a file that stages something else would rehearse a
        // plan on a state the target does not hold. The comparison needs the
        // resolver's reconstruction, which says which target objects are
        // managed, so a rehearsal with nothing staged never builds it.
        if externals.is_some() || prepared.has_filled() {
            let mut reconstruction = state
                .reconstruction(
                    &mut target,
                    binding,
                    desired.ids,
                    &hints.module_deps,
                    &mut prepared,
                )
                .await?;
            if reconstruction.has_fill() {
                return Err(Error::Externals(vec![
                    "the externals fill reads managed relations, which a rehearsal cannot \
                     create between its statements; turn the rehearsal off with \
                     `rehearse: false` or `--no-rehearse`"
                        .into(),
                ]));
            }
            reconstruction.chains(&prepared);
            state
                .compare_externals(
                    provisioning,
                    &mut prepared,
                    &mut admin,
                    &mut target,
                    &reconstruction,
                )
                .await?;
        }
        let roles = roles(&mut state, &mut admin, named, &tokens).await?;
        let layout = if prepared.supplied() {
            ScratchLayout::Supplied
        } else {
            ScratchLayout::RunOwned
        };
        let session = prepared.session(&mut admin);
        Ok((layout, run(session, roles).await))
    }
    .await;
    let released = state.release(&mut admin).await;
    drop(target);
    match (opened, released) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (Err(refused), Ok(())) => Err(ProduceError::Run(refused)),
        (Ok(done), Ok(())) => Ok(done),
    }
}

/// The roles of the run, by the naming its placement decides. Here and in the
/// one place the plan is lowered again (`OrderedPlan::under_role_names`) is all
/// the code that tells the three apart.
async fn roles(
    state: &mut engine::Scratch<'_>,
    admin: &mut Conn,
    named: &BTreeSet<RoleName>,
    tokens: &Tokens,
) -> Result<Roles, Error> {
    // The engine's own roles keep their names: they are the scratch's too.
    let own = |role: &RoleName| role.raw().starts_with("pg_");
    let naming = state.naming()?;
    let mapping = match naming {
        engine::Naming::Existing => RoleMapping::default(),
        engine::Naming::Created => {
            RoleMapping::unchanged(named.iter().filter(|role| !own(role)).cloned())
        }
        engine::Naming::Mapped => {
            RoleMapping::generate(named, "pbps_rehearsal", &tokens.scratch, own)
        }
    };
    Ok(
        match state
            .rehearsal_roles(admin, mapping.created(), naming)
            .await?
        {
            Ok(()) => Ok(mapping),
            Err(why) => Err(why),
        },
    )
}
