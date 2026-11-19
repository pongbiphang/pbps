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

use super::super::producer::{request, request_as_it_stands};
use super::super::{BindingRequest, Error, PlanningDialect, ProduceError};
use super::{Provenance, Standard, Tokens};
use crate::resolver::contract::{self, PreparedView, RehearsalRequest, ResolverEngine};
use pbps_db::Conn;
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
pub async fn with_session<D: PlanningDialect, T, E>(
    scratch: &str,
    target_connection: &str,
    planned_identity: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    standard: &Standard<D>,
    externals: Option<&str>,
    provenance: &Provenance,
    named: &BTreeSet<RoleName>,
    run: impl AsyncFnOnce(&mut Conn, Option<Conn>, Roles) -> Result<T, E>,
) -> Result<(ScratchLayout, Result<T, E>), ProduceError> {
    // An engine without a connected rehearsal is refused by name before the
    // request is built (DEC-2798.5).
    let rehearsal = contract::rehearsal_of::<D>()
        .map_err(|unsupported| ProduceError::Acquire(unsupported.to_string()))?;
    let driver = D::DRIVER;
    // The write path's extras are the dialect's, and the CLI's PostgreSQL
    // dialect configures none, as for the resolver.
    let request = match rehearsal.request {
        RehearsalRequest::Projected => {
            request::<D>(base, desired, hints, &[]).map_err(ProduceError::Run)?
        }
        RehearsalRequest::AsItStands => request_as_it_stands(base, desired, &[]),
    };
    // Every connection of the run opens the one endpoint found here
    // (DEC-2271.2), as for the resolver.
    let scratch = &<D::Resolver as ResolverEngine>::endpoint(scratch).await?;
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
    let mut admin = <D::Resolver as ResolverEngine>::connect_scratch(scratch, &tokens).await?;
    let (provisioning, mut state) = <D::Resolver as ResolverEngine>::acquire(
        &mut target,
        &mut admin,
        &tokens,
        planned_identity,
        scratch,
        &request,
        standard,
        externals,
        provenance,
    )
    .await?;
    let opened = async {
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let scope_schemas = <D::Resolver as ResolverEngine>::scope_schemas(
            &request.schemas,
            &request.write_path_extras,
        )
        .map_err(Error::Scope)?;
        let (target_catalog, authorization) = <D::Resolver as ResolverEngine>::read_target(
            &mut target,
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
        // SQL Server's captures fingerprint what they read, and a rehearsal
        // compares in memory and seals nothing: the key is the run's own.
        let key = rehearsal
            .run_key
            .then(pbps_db::fingerprint::EnvironmentFingerprintKey::for_one_run);
        let mut prepared = <D::Resolver as ResolverEngine>::prepare(
            &mut state,
            provisioning,
            &mut admin,
            &mut target,
            &target_catalog,
            &authorization,
            &scope_schemas,
            binding,
            &request.write_path_extras,
            key.as_ref(),
        )
        .await?;
        <D::Resolver as ResolverEngine>::rehearsal_base(
            &state,
            provisioning,
            &mut prepared,
            &mut admin,
            &mut target,
            binding,
            base.ids,
            desired.ids,
            hints,
            desired.schema,
            externals,
        )
        .await?;
        let reader = prepared.reader().await?;
        let roles = <D::Resolver as ResolverEngine>::rehearsal_roles(
            &mut state,
            &mut admin,
            named,
            base.schema,
            desired.schema,
            &tokens,
        )
        .await?;
        // PostgreSQL's prepared scratch knows it holds a supplied database;
        // SQL Server's layout is the provisioning the acquisition chose, as for
        // the resolver's evidence.
        let layout = match provisioning {
            super::Provisioning::Supplied => ScratchLayout::Supplied,
            super::Provisioning::RunOwned if prepared.supplied() => ScratchLayout::Supplied,
            super::Provisioning::RunOwned => ScratchLayout::RunOwned,
        };
        let session = prepared.deployer(&mut admin);
        Ok((layout, run(session, reader, roles).await))
    }
    .await;
    let released = <D::Resolver as ResolverEngine>::release(&mut state, &mut admin).await;
    drop(target);
    match (opened, released) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (Err(refused), Ok(())) => Err(ProduceError::Run(refused)),
        (Ok(done), Ok(())) => Ok(done),
    }
}

#[cfg(test)]
mod structure_tests {
    /// The first engine mark in `source`'s production code, comments aside:
    /// comments may say why an engine differs, code may not choose by it.
    ///
    /// Production code ends at this test module. The cut is the module
    /// declaration together with its attribute, which occurs once.
    fn engine_named(source: &str) -> Option<&'static str> {
        let production = source
            .split("\n#[cfg(test)]\nmod structure_tests")
            .next()
            .unwrap_or_default();
        let code = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        ["Driver::", "driver ==", "pbps_pg", "pbps_mssql"]
            .into_iter()
            .find(|mark| code.contains(mark))
    }

    #[test]
    fn the_rehearsal_leaves_every_engine_decision_to_the_engine() {
        assert_eq!(engine_named(include_str!("rehearse.rs")), None);
    }

    #[test]
    fn the_engine_scan_sees_a_driver_branch_and_an_engine_path() {
        for (source, mark) in [
            ("fn f() { if driver == Driver::Mssql {} }", "Driver::"),
            ("fn f() { if driver == other {} }", "driver =="),
            ("fn f() { pbps_pg::roles::make(); }", "pbps_pg"),
            ("fn f() { pbps_mssql::principals(); }", "pbps_mssql"),
        ] {
            assert_eq!(engine_named(source), Some(mark), "{source}");
        }
        assert_eq!(
            engine_named("// Driver::Mssql only in a comment\nfn f() {}"),
            None
        );
        assert_eq!(
            engine_named(
                "fn f() {}\n#[cfg(test)]\nmod structure_tests { fn t() { pbps_pg::x(); } }"
            ),
            None,
            "a test module is not production code"
        );
    }
}
