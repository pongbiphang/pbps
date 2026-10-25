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
use pbps_model::Hints;
use pbps_model::resolver::ScratchLayout;

/// Runs `run` on a session of the scratch as the target's deployer, with the
/// entry's externals staged and compared with the target, and releases the
/// scratch whatever `run` returns. The layout the run used comes back beside
/// the result: it is the run's own finding, as for the resolver.
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
    run: impl AsyncFnOnce(&mut Conn) -> Result<T, E>,
) -> Result<(ScratchLayout, Result<T, E>), ProduceError> {
    // The write path's extras are the dialect's, and the CLI's PostgreSQL
    // dialect configures none, as for the resolver.
    let request = request(driver, base, desired, hints, &[]).map_err(ProduceError::Run)?;
    let standard = standard.into();
    let mut target = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Unusable(format!(
                "the target could not be connected: {error}"
            )))
        })?;
    let mut admin = Conn::connect(driver, scratch)
        .await
        .map_err(|error| ProduceError::Acquire(format!("the scratch server: {error}")))?;
    engine::reset_role(driver, &mut admin).await?;
    let tokens = Tokens::generate();
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
            )
            .await?;
        // The externals the base and the plan read are the target's, as for
        // the resolver: a file that stages something else would rehearse a
        // plan on a state the target does not hold. The comparison needs the
        // resolver's reconstruction, which says which target objects are
        // managed, so a rehearsal with nothing staged never builds it.
        if externals.is_some() || prepared.has_filled() {
            let mut reconstruction = state
                .reconstruction(&mut target, binding, desired.ids, &hints.module_deps)
                .await?;
            if prepared.split(&reconstruction).is_some() {
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
        let layout = if prepared.supplied() {
            ScratchLayout::Supplied
        } else {
            ScratchLayout::RunOwned
        };
        let session = prepared.session(&mut admin);
        Ok((layout, run(session).await))
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
