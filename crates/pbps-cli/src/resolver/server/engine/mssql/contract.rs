//! SQL Server's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.
//! SQL Server has no snapshot (DEC-619.1), so its checks are plain reads
//! compared at both ends, and a change made and restored in between leaves
//! nothing to find (DEC-2144.1).

use super::super::unsupported;
use super::principals::principals;
use super::{Binding, Closing, Prepared, Scratch};
use crate::resolver::contract::{
    ApplyRecheck, BindingView, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Phase,
    Planning, PreparedView, Preview, Rehearsal, RehearsalRequest, Replay, ResolverEngine, Sealable,
    Step, Support, TargetRead, sealed::Sealed,
};
use crate::resolver::scope;
use crate::resolver::server::scratch::rehearse::Roles;
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, PlanningDialect, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::{CatalogFacts, DatabaseRecipe, ScopeReport};
use pbps_db::{Conn, DbError, Driver};
use pbps_model::dependency::DependencyName;
use pbps_model::operation::Captured;
use pbps_model::resolver::{
    AuthorizationGrant, AuthorizationReproduction, DependencyPlacement, InputManifest,
    ManifestError, ObjectIdentity, OutsideWrites, ScratchLayout, Surface,
};
use pbps_model::{ChangeSet, Hints, IdsFile, ModuleDeps, RoleName, Schema, SessionZone};
use pbps_pg::resolver::capture::{BindingRecord, SealError};
use std::collections::{BTreeMap, BTreeSet};

/// SQL Server reproduces the deployer's authorization whichever layout the
/// run used: its scratch enters the sandbox as the reproduced deployer.
fn reproduced(_: ScratchLayout) -> AuthorizationReproduction {
    AuthorizationReproduction::Reproduced
}

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
        planning: Some(&Planning {
            cluster: CLUSTER,
            channels_rule: "pbps/mssql-channels/v1",
            reproduction: reproduced,
        }),
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
    const FILTER_BINDS: bool = false;
    type Prepared = Prepared;
    type Binding = Box<Binding>;
    type Staged = pbps_mssql::resolver::capture::Staged;
    type CompiledCapture = Box<Closing>;
    type CapturedInputs = pbps_mssql::resolver::capture::CapturedInputs;
    type History = ();

    fn binding_classes(surface: &Surface) -> Option<&'static [&'static str]> {
        pbps_mssql::resolver::binding_classes(surface)
    }

    fn dependent_object(
        record: &BindingRecord,
        reached: &BTreeSet<ObjectIdentity>,
    ) -> Option<ObjectIdentity> {
        pbps_mssql::resolver::dependent_object(&record.object, &record.bindings, reached)
    }

    fn retain_grants(
        _: &mut Conn,
    ) -> Step<'_, Result<Option<Box<pbps_db::catalog::Pulled>>, Error>> {
        // SQL Server's object permissions are part of the engine-read
        // authorization context (DEC-2143.1); no PostgreSQL grant reader runs
        // on that engine.
        Box::pin(std::future::ready(Ok(None)))
    }

    fn seal_grants(
        _: &EnvironmentFingerprintKey,
        _: &ChangeSet,
        _: &IdsFile,
        _: Option<&pbps_db::catalog::Pulled>,
    ) -> Result<Vec<AuthorizationGrant>, String> {
        Ok(Vec::new())
    }

    fn reported_rule() -> Result<&'static str, Error> {
        Ok(pbps_mssql::resolver::compatibility::REPORTED_RULE)
    }

    fn reported_build(
        key: &EnvironmentFingerprintKey,
        catalog: &CatalogFacts,
    ) -> Result<String, Error> {
        super::reported_build(key, catalog)
    }

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
    ) -> Step<'a, Result<Self::CompiledCapture, Error>> {
        Box::pin(async move {
            binding
                .capture_desired(prepared.reach(), target, schemas, key, desired, staged)
                .await
                .map(Box::new)
        })
    }

    fn capture_opening<'a>(
        binding: &'a Self::Binding,
        target: &'a mut Conn,
        closing: &'a Self::CompiledCapture,
        key: &'a EnvironmentFingerprintKey,
        base: pbps_diff::Side<'a>,
    ) -> Step<'a, Result<(Self::CapturedInputs, InputManifest), Error>> {
        Box::pin(async move { binding.capture_opening(target, closing, key, base).await })
    }

    fn assess(
        binding: &mut Self::Binding,
        closing: &Self::CompiledCapture,
        current: &Self::CapturedInputs,
        staged: &Self::Staged,
    ) -> Result<Assessment, Error> {
        Ok(binding.assess(closing, current, staged))
    }

    fn changed_since(staged: &Self::Staged, current: &Self::CapturedInputs) -> Vec<String> {
        staged.changed_since(current)
    }

    fn recapture<'a>(
        target: &'a mut Conn,
        current: &'a Self::CapturedInputs,
    ) -> Step<'a, Result<Vec<CaptureDifference>, CaptureError>> {
        Box::pin(async move {
            pbps_mssql::resolver::capture::recapture(target, current)
                .await
                .map(|(_, differences)| differences)
        })
    }

    fn captured_bindings(
        compiled: &Self::CompiledCapture,
        current: &Self::CapturedInputs,
        base: &Schema,
        desired: &Schema,
    ) -> Captured {
        Captured {
            before: current.schema_bound_callees(base),
            after: compiled.schema_bound_callees(desired),
        }
    }

    fn scope_schemas(schemas: &[String], extras: &[String]) -> Result<Vec<String>, String> {
        if extras.is_empty() {
            Ok(schemas.to_vec())
        } else {
            Err(format!(
                "SQL Server has no write path, so a scope cannot carry write-path extras: {}",
                extras.join(", ")
            ))
        }
    }

    fn read_catalog<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        _: &'a [String],
    ) -> Step<'a, Result<CatalogFacts, DbError>> {
        Box::pin(async move {
            let scope = pbps_mssql::resolver::environment::Scope { schemas };
            pbps_mssql::resolver::environment::read(connection, &scope).await
        })
    }

    fn read_target<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        _: &'a [String],
        authorization_schemas: &'a [String],
        planned: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<(CatalogFacts, scope::Authorization), DbError>> {
        Box::pin(async move {
            let scope = pbps_mssql::resolver::environment::Scope { schemas };
            // The planned grants' principals are resolved to the catalog's
            // spelling within each bracketed read (#726).
            let names: Vec<pbps_model::RoleName> =
                planned.iter().map(|g| g.principal.clone()).collect();
            let (catalog, authorization) = pbps_mssql::resolver::scope_facts(
                connection,
                &scope,
                authorization_schemas,
                &names,
            )
            .await?;
            Ok((
                catalog,
                scope::Authorization::Mssql(Box::new(authorization)),
            ))
        })
    }

    fn read_target_with_history<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
        authorization_schemas: &'a [String],
        planned: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<TargetRead<Self::History>, DbError>> {
        Box::pin(async move {
            let (catalog, authorization) =
                Self::read_target(connection, schemas, extras, authorization_schemas, planned)
                    .await?;
            Ok((catalog, authorization, ()))
        })
    }

    fn recipe(catalog: &CatalogFacts) -> Result<DatabaseRecipe, String> {
        DatabaseRecipe::from_sql_server_catalog(catalog).map_err(|error| error.to_string())
    }

    fn compare_reported(
        target: &CatalogFacts,
        resolver: &CatalogFacts,
    ) -> Result<ScopeReport, String> {
        Ok(pbps_mssql::resolver::compatibility::compare_reported(
            target, resolver,
        ))
    }

    fn limitation(_: &Self::History) -> Option<(&'static str, &'static str)> {
        None
    }

    fn history_differences(_: &Self::History, _: &Self::History) -> Result<Vec<String>, DbError> {
        Ok(Vec::new())
    }

    fn through_shape_view(_: &Self::Staged, error: Error) -> Error {
        error
    }

    fn dialect(_: &Self::Binding, extras: &[String]) -> Self::Dialect {
        <pbps_mssql::Mssql as PlanningDialect>::configured(extras, None)
    }

    fn rehearsal_base<'a>(
        scratch: &'a Self::Scratch<'_>,
        _: Provisioning,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        binding: &'a BindingRequest<'a>,
        base_ids: &'a IdsFile,
        _: &'a IdsFile,
        hints: &'a Hints,
        desired: &'a Schema,
        _: Option<&'a str>,
    ) -> Step<'a, Result<(), Error>> {
        Box::pin(async move {
            // The base is the target's own managed namespace, compiled the way
            // the resolver compiles the declarations: through the engine's
            // creation trials, because a default or computed column that calls
            // a function the base also holds has no order the plain bootstrap
            // can write (DEC-2379.3). Its externals were staged and compared
            // with the target by `prepare`.
            let mut built = Self::reconstruction(
                scratch,
                target,
                binding,
                base_ids,
                &hints.annotations.modules(desired),
                prepared,
            )
            .await?;
            let mut staged = Self::completed_staged(prepared).ok_or_else(|| {
                Error::Scope("the prepared scratch holds no staged inventory".into())
            })?;
            Self::compile(
                &mut built,
                prepared,
                admin,
                target,
                &[],
                None,
                OutsideWrites::Refused,
                &mut staged,
            )
            .await?;
            Ok(())
        })
    }

    /// SQL Server has no cluster role to name: its roles are the scratch
    /// database's own, and the principals a plan names are checked against the
    /// declarations instead (DEC-2379.3).
    fn rehearsal_roles<'a>(
        _: &'a mut Self::Scratch<'_>,
        _: &'a mut Conn,
        named: &'a BTreeSet<RoleName>,
        base: &'a Schema,
        desired: &'a Schema,
        _: &'a Tokens,
    ) -> Step<'a, Result<Roles, Error>> {
        Box::pin(async move { Ok(principals(named, base, desired)) })
    }
}

impl Sealable for Box<Closing> {
    fn planning_records(&self) -> Result<Vec<BindingRecord>, ManifestError> {
        Ok(Closing::planning_records(self)?
            .into_iter()
            .map(|record| BindingRecord {
                object: record.object,
                ownership: record.ownership,
                bindings: record.bindings,
            })
            .collect())
    }

    fn seal_for_plan(self) -> Result<InputManifest, SealError> {
        Closing::seal_for_plan(*self).map_err(Into::into)
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
        <Self as ResolverEngine>::scope_schemas(schemas, extras)
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
                let (catalog, read) = <Self as ResolverEngine>::read_target(
                    conn,
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
