//! SQL Server's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.
//! SQL Server has no snapshot (DEC-619.1), so its checks are plain reads
//! compared at both ends, and a change made and restored in between leaves
//! nothing to find (DEC-2144.1).

use super::super::{CapturedInputs, CompiledCapture, unsupported};
use super::{Binding, Prepared, Scratch};
use crate::resolver::contract::{
    ApplyRecheck, BindingView, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Phase,
    Planning, PreparedView, Preview, Rehearsal, RehearsalRequest, Replay, ResolverEngine, Step,
    Support, sealed::Sealed,
};
use crate::resolver::scope;
use crate::resolver::server::engine::Naming;
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, PlanningDialect, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::Assessment;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, DbError, Driver};
use pbps_model::dependency::DependencyName;
use pbps_model::resolver::{DependencyPlacement, InputManifest, OutsideWrites};
use pbps_model::{IdsFile, ModuleDeps, RoleName, SessionZone};
use std::collections::BTreeMap;

/// SQL Server's resolver: planning, both rehearsals and the apply recheck.
pub struct MssqlResolver;

impl Sealed for MssqlResolver {}

/// The domain the planning seals the cluster under; apply recomputes it.
const CLUSTER: ClusterDomain = ClusterDomain {
    rule: crate::resolver::server::MSSQL_CLUSTER_RULE,
    component: "family-guid",
};

impl ResolverEngine for MssqlResolver {
    type Dialect = pbps_mssql::Mssql;
    const SUPPORT: Support = Support {
        planning: Some(&Planning { cluster: CLUSTER }),
        rehearsal: Some(&Rehearsal {
            request: RehearsalRequest::AsItStands,
            run_key: true,
        }),
        preview: Some(&Preview),
        apply: Some(&MssqlResolver),
    };

    /// SQL Server's scratch has no standard to put it into.
    type Standard = ();
    type Scratch<'a> = Box<Scratch<'a>>;
    type Prepared = Prepared;
    type Binding = Box<Binding>;
    type Staged = pbps_mssql::resolver::capture::Staged;

    fn standard(
        declared: Option<&pbps_config::resolver::ScratchStandard>,
    ) -> Result<Self::Standard, Error> {
        if declared.is_none() {
            Ok(())
        } else {
            Err(Error::Unusable(
                "SQL Server scratch does not accept a PostgreSQL standard declaration".into(),
            ))
        }
    }

    fn endpoint(connection: &str) -> Step<'_, Result<String, ProduceError>> {
        Box::pin(super::endpoint(connection))
    }

    fn connect_scratch<'a>(
        scratch: &'a str,
        tokens: &'a Tokens,
    ) -> Step<'a, Result<Conn, ProduceError>> {
        Box::pin(async move {
            Conn::connect_named(Driver::Mssql, scratch, &format!("pbps-{}", tokens.scratch))
                .await
                .map_err(|error| ProduceError::Acquire(format!("the scratch server: {error}")))
        })
    }

    fn target_identity(target: &mut Conn) -> Step<'_, Result<String, DbError>> {
        Box::pin(pbps_mssql::resolver::scratch::cluster_identity(target))
    }

    fn acquire<'a, 'b>(
        target: &'b mut Conn,
        admin: &'b mut Conn,
        tokens: &'b Tokens,
        planned_identity: &'b str,
        scratch: &'a str,
        request: &'a ScopeRequest,
        _: &'a Self::Standard,
        externals: Option<&'a str>,
        provenance: &'a Provenance,
    ) -> Step<'b, Result<(Provisioning, Self::Scratch<'a>), ProduceError>>
    where
        'a: 'b,
    {
        Box::pin(async move {
            let (provisioning, scratch) = super::scratch(
                target,
                admin,
                tokens,
                planned_identity,
                scratch,
                externals,
                request,
                provenance,
            )
            .await?;
            Ok((provisioning, Box::new(scratch)))
        })
    }

    fn release<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        admin: &'a mut Conn,
    ) -> Step<'a, Result<(), Vec<String>>> {
        Box::pin(async move { scratch.release(admin).await })
    }

    /// A code vet is a PostgreSQL operation: the question is refused by name.
    fn prepare_code<'a>(
        _: &'a mut Self::Scratch<'_>,
        _: Provisioning,
        _: &'a mut Conn,
        _: &'a CatalogFacts,
        _: &'a scope::Authorization,
        _: &'a [String],
    ) -> Step<'a, Result<Prepared, Error>> {
        Box::pin(async move { Err(unsupported()) })
    }

    fn prepare<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        provisioning: Provisioning,
        admin: &'a mut Conn,
        target_connection: &'a mut Conn,
        target: &'a CatalogFacts,
        authorization: &'a scope::Authorization,
        schemas: &'a [String],
        binding: &'a BindingRequest<'a>,
        _: &'a [String],
        key: Option<&'a EnvironmentFingerprintKey>,
    ) -> Step<'a, Result<Prepared, Error>> {
        Box::pin(async move {
            scratch
                .prepare(
                    provisioning,
                    admin,
                    target,
                    target_connection,
                    authorization,
                    schemas,
                    binding,
                    key.ok_or_else(|| {
                        Error::Binding(
                        "SQL Server capture requires the configured environment fingerprint key"
                            .into(),
                    )
                    })?,
                )
                .await
        })
    }

    fn reconstruction<'a>(
        scratch: &'a Self::Scratch<'_>,
        _: &'a mut Conn,
        binding: &'a BindingRequest<'a>,
        ids: &'a IdsFile,
        dependencies: &'a ModuleDeps,
        _: &'a mut Self::Prepared,
    ) -> Step<'a, Result<Self::Binding, Error>> {
        Box::pin(async move {
            scratch
                .reconstruction(binding, ids, dependencies)
                .map(Box::new)
        })
    }

    fn compare_externals<'a>(
        _: &'a Self::Scratch<'_>,
        _: Provisioning,
        prepared: &'a mut Prepared,
        _: &'a mut Conn,
        _: &'a mut Conn,
        _: &'a Self::Binding,
    ) -> Step<'a, Result<Self::Staged, Error>> {
        Box::pin(async move { Ok(prepared.staged.clone()) })
    }

    fn staged(prepared: &Self::Prepared) -> Self::Staged {
        // Preparation always compares the externals, an empty set included.
        prepared.staged.clone()
    }

    fn completed_staged(prepared: &Self::Prepared) -> Option<Self::Staged> {
        Some(prepared.staged.clone())
    }

    fn chains(_: &mut Self::Binding, _: &Self::Prepared) -> Vec<String> {
        Vec::new()
    }

    fn compile<'a>(
        binding: &'a mut Self::Binding,
        prepared: &'a mut Self::Prepared,
        _: &'a mut Conn,
        target: &'a mut Conn,
        _: &'a [String],
        _: Option<&'a SessionZone>,
        _: OutsideWrites,
        _: &'a mut Self::Staged,
    ) -> Step<'a, Result<(), Error>> {
        Box::pin(async move {
            let len = binding.len();
            binding.compile(prepared, target, 0..len).await
        })
    }

    fn capture_desired<'a>(
        binding: &'a Self::Binding,
        prepared: &'a mut Self::Prepared,
        _: &'a mut Conn,
        target: &'a mut Conn,
        _: &'a [String],
        _: &'a BTreeMap<String, pbps_db::resolver::Observation>,
        schemas: &'a [String],
        key: &'a EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'a>,
        staged: &'a Self::Staged,
    ) -> Step<'a, Result<CompiledCapture, Error>> {
        Box::pin(async move {
            binding
                .capture_desired(prepared.reach(), target, schemas, key, desired, staged)
                .await
                .map(|closing| CompiledCapture::Mssql(Box::new(closing)))
        })
    }

    fn capture_opening<'a>(
        binding: &'a Self::Binding,
        target: &'a mut Conn,
        closing: &'a CompiledCapture,
        key: &'a EnvironmentFingerprintKey,
        base: pbps_diff::Side<'a>,
    ) -> Step<'a, Result<(CapturedInputs, InputManifest), Error>> {
        Box::pin(async move {
            match closing {
                CompiledCapture::Mssql(closing) => {
                    let (current, opening) =
                        binding.capture_opening(target, closing, key, base).await?;
                    Ok((CapturedInputs::Mssql(current), opening))
                }
                CompiledCapture::Postgres(_) => Err(unsupported()),
            }
        })
    }

    fn assess(
        binding: &mut Self::Binding,
        closing: &CompiledCapture,
        current: &CapturedInputs,
        staged: &Self::Staged,
    ) -> Result<Assessment, Error> {
        match (closing, current) {
            (CompiledCapture::Mssql(closing), CapturedInputs::Mssql(current)) => {
                Ok(binding.assess(closing, current, staged))
            }
            _ => Err(unsupported()),
        }
    }

    fn changed_since(
        staged: &Self::Staged,
        current: &CapturedInputs,
    ) -> Result<Vec<String>, Error> {
        match current {
            CapturedInputs::Mssql(current) => Ok(staged.changed_since(current)),
            CapturedInputs::Postgres(_) => Err(unsupported()),
        }
    }

    fn through_shape_view(_: &Self::Staged, error: Error) -> Error {
        error
    }

    fn dialect(_: &Self::Binding, extras: &[String]) -> Self::Dialect {
        <pbps_mssql::Mssql as PlanningDialect>::configured(extras, None)
    }

    /// SQL Server has no cluster role to name: its roles are the scratch
    /// database's own and a rehearsal asks `scratch::rehearse::principals`
    /// instead (DEC-2379.3).
    fn naming(_: &Self::Scratch<'_>) -> Result<Naming, Error> {
        Err(unsupported())
    }

    fn rehearsal_roles<'a>(
        _: &'a mut Self::Scratch<'_>,
        _: &'a mut Conn,
        _: &'a [RoleName],
        _: Naming,
    ) -> Step<'a, Result<Result<(), String>, Error>> {
        Box::pin(async move { Err(unsupported()) })
    }
}

impl BindingView for Box<Binding> {
    fn has_fill(&self) -> bool {
        false
    }
}

impl PreparedView for Prepared {
    fn authorization_condition<'a>(
        &'a mut self,
        target: &'a mut Conn,
    ) -> Step<'a, Result<Option<Box<pbps_mssql::resolver::authorization::Condition>>, Error>> {
        Box::pin(async move {
            Prepared::authorization_condition(self, target)
                .await
                .map(Some)
        })
    }

    fn dependency_placements(&self) -> BTreeMap<DependencyName, DependencyPlacement> {
        Prepared::dependency_placements(self)
    }

    fn catalog(&self) -> &CatalogFacts {
        &self.scratch_catalog
    }

    #[cfg(test)]
    fn catalog_mut(&mut self) -> &mut CatalogFacts {
        &mut self.scratch_catalog
    }

    fn into_catalog(self) -> CatalogFacts {
        self.scratch_catalog
    }

    fn session<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn {
        admin
    }

    fn deployer<'a>(&'a mut self, _: &'a mut Conn) -> &'a mut Conn {
        &mut self.compile
    }

    fn reader(&self) -> Step<'_, Result<Option<Conn>, Error>> {
        // `Prepared` holds the deployer's connection and is not `Sync`, so the
        // reach is taken now and only it is held across the open.
        let reach = self.reach();
        Box::pin(async move { reach.open().await.map(Some) })
    }

    fn differences(&self) -> &[String] {
        &self.differences
    }

    fn supplied(&self) -> bool {
        false
    }

    fn has_filled(&self) -> bool {
        !self.staged.compared().is_empty()
    }

    fn unfillable(&self) -> Vec<String> {
        Vec::new()
    }
}

impl ApplyRecheck for MssqlResolver {
    fn cluster(&self) -> ClusterDomain {
        CLUSTER
    }

    fn cluster_identity<'a>(&'a self, conn: &'a mut Conn) -> Step<'a, Result<String, String>> {
        Box::pin(async move {
            pbps_mssql::resolver::scratch::cluster_identity(conn)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn scope_schemas(&self, schemas: &[String], extras: &[String]) -> Result<Vec<String>, String> {
        scope::scope_schemas(Driver::Mssql, schemas, extras)
    }

    fn recheck_opening<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
    ) -> Step<'a, Result<InputManifest, String>> {
        Box::pin(async move {
            let sealed = r.evidence.before();
            pbps_mssql::resolver::capture::recheck_opening(conn, sealed, r.declarations, r.key)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn capture_closing<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
        closing: ClosingInputs<'a>,
    ) -> Step<'a, Result<InputManifest, String>> {
        Box::pin(async move {
            let after = r.evidence.after();
            // No routine identity is proved apart from the declarations:
            // SQL Server routines are objects of their schema like any
            // other (DEC-2144.1).
            let recorded = pbps_mssql::resolver::capture::RecordedOwnership {
                schema: closing.schema,
                ids: closing.ids,
                namespaces: closing.namespaces,
            };
            pbps_mssql::resolver::capture::capture_closing(
                conn,
                after,
                &after.vanishable(),
                r.declarations,
                r.key,
                &recorded,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    fn scope_facts<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
    ) -> Step<'a, Result<(CatalogFacts, scope::Authorization), FactsError>> {
        Box::pin(async move {
            let target = &r.evidence.qualification().target_scope;
            // The bracketed double read the planning used, inside the apply's
            // transaction. The principals the plan's own grants name are
            // resolved to the catalog's spelling within each read.
            let planned = scope::planned_schema_grants(r.changes)
                .map_err(|why| FactsError::Refused(format!("the plan's own grants: {why}")))?;
            async {
                let (catalog, read) = scope::read_target(
                    conn,
                    Driver::Mssql,
                    &target.schemas,
                    &target.write_path_extras,
                    r.scope_schemas,
                    &planned,
                )
                .await?;
                // What the plan sealed is the deployer's answers, not the
                // catalog read alone: the engine-read effective
                // permissions of the same questions (DEC-2143.1).
                let scope::Authorization::Mssql(read) = read else {
                    unreachable!("a SQL Server read answers with SQL Server's context")
                };
                let planned_principals: Vec<_> = planned
                    .iter()
                    .map(|grant| grant.principal.clone())
                    .collect();
                let answered = pbps_mssql::resolver::capture::answered_authorization(
                    conn,
                    &read,
                    &planned_principals,
                    [r.evidence.before(), r.evidence.after()],
                    r.declarations,
                    r.key,
                )
                .await?;
                Ok::<_, DbError>((catalog, scope::Authorization::Mssql(Box::new(answered))))
            }
            .await
            .map_err(|error| FactsError::Unread(error.to_string()))
        })
    }

    fn object_grants<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _r: Replay<'a>,
        _phase: Phase,
    ) -> Step<'a, Result<GrantRecheck, String>> {
        // SQL Server's object permissions are part of the authorization
        // context the engine read (DEC-2143.1); evidence that claims
        // separate grants was not sealed by this build.
        Box::pin(async { Err("SQL Server evidence carries none".to_owned()) })
    }
}
