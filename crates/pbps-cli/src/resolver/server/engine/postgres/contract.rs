//! PostgreSQL's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.

use super::super::{Binding, CompiledCapture, Staged, unsupported};
use super::{Naming, Prepared, Scratch};
use crate::resolver::contract::{
    ApplyRecheck, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Phase, Planning,
    PreparedView, Rehearsal, RehearsalRequest, Replay, ResolverEngine, Step, Support,
    sealed::Sealed,
};
use crate::resolver::scope;
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, DbError, Driver};
use pbps_model::dependency::DependencyName;
use pbps_model::resolver::{DependencyPlacement, OutsideWrites};
use pbps_model::resolver::{
    InputManifest, ObjectIdentity, ObjectOwnership, ResolverEvidence, Surface,
};
use pbps_model::{IdsFile, ModuleDeps, ModuleId, RoleName, SessionZone};
use std::collections::BTreeMap;

/// PostgreSQL's resolver: planning, connected rehearsal and the apply recheck.
/// It has no target-less dev rehearsal, whose scratch lifecycle is SQL
/// Server's alone (SPEC §9.3).
pub struct PgResolver;

impl Sealed for PgResolver {}

/// The domain the planning seals the cluster under; apply recomputes it.
const CLUSTER: ClusterDomain = ClusterDomain {
    rule: crate::resolver::server::CLUSTER_RULE,
    component: "system-identifier",
};

impl ResolverEngine for PgResolver {
    type Dialect = pbps_pg::Postgres;
    const SUPPORT: Support = Support {
        planning: Some(&Planning { cluster: CLUSTER }),
        rehearsal: Some(&Rehearsal {
            request: RehearsalRequest::Projected,
            run_key: false,
        }),
        preview: None,
        apply: Some(&PgResolver),
    };

    type Standard = pbps_pg::resolver::standard::Standard;
    type Scratch<'a> = Box<Scratch<'a>>;
    type Prepared = Prepared;

    fn standard(
        declared: Option<&pbps_config::resolver::ScratchStandard>,
    ) -> Result<Self::Standard, Error> {
        Ok(super::declared_standard(declared))
    }

    fn endpoint(connection: &str) -> Step<'_, Result<String, ProduceError>> {
        Box::pin(async move { Ok(connection.to_owned()) })
    }

    fn connect_scratch<'a>(
        scratch: &'a str,
        tokens: &'a Tokens,
    ) -> Step<'a, Result<Conn, ProduceError>> {
        Box::pin(async move {
            let mut admin = Conn::connect_named(
                Driver::Postgres,
                scratch,
                &format!("pbps-{}", tokens.scratch),
            )
            .await
            .map_err(|error| ProduceError::Acquire(format!("the scratch server: {error}")))?;
            super::reset_role(&mut admin).await?;
            super::keep(&mut admin).await?;
            Ok(admin)
        })
    }

    fn target_identity(target: &mut Conn) -> Step<'_, Result<String, DbError>> {
        Box::pin(super::target_identity(target))
    }

    fn acquire<'a, 'b>(
        target: &'b mut Conn,
        admin: &'b mut Conn,
        tokens: &'b Tokens,
        planned_identity: &'b str,
        scratch: &'a str,
        request: &'a ScopeRequest,
        standard: &'a Self::Standard,
        externals: Option<&'a str>,
        provenance: &'a Provenance,
    ) -> Step<'b, Result<(Provisioning, Self::Scratch<'a>), ProduceError>>
    where
        'a: 'b,
    {
        Box::pin(async move {
            let (provisioning, scratch) = super::scratch(
                Driver::Postgres,
                target,
                admin,
                tokens,
                planned_identity,
                scratch,
                request,
                standard,
                externals,
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

    fn prepare_code<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        provisioning: Provisioning,
        admin: &'a mut Conn,
        target: &'a CatalogFacts,
        authorization: &'a scope::Authorization,
        schemas: &'a [String],
    ) -> Step<'a, Result<Prepared, Error>> {
        Box::pin(async move {
            scratch
                .prepare(
                    provisioning,
                    admin,
                    target,
                    authorization,
                    schemas,
                    &Default::default(),
                )
                .await
        })
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
        extras: &'a [String],
        _: Option<&'a EnvironmentFingerprintKey>,
    ) -> Step<'a, Result<Prepared, Error>> {
        Box::pin(async move {
            let fill = super::fill(target_connection, binding, extras).await?;
            let mut prepared = scratch
                .prepare(provisioning, admin, target, authorization, schemas, &fill)
                .await?;
            let problems = pbps_pg::resolver::scratch::extension_prerequisites(
                prepared.session(admin),
                &binding.base.extensions,
            )
            .await
            .map_err(|error| Error::Read(format!("scratch extension prerequisites: {error}")))?;
            if !problems.is_empty() {
                return Err(Error::Binding(problems.join("; ")));
            }
            Ok(prepared)
        })
    }

    fn reconstruction<'a>(
        scratch: &'a Self::Scratch<'_>,
        target: &'a mut Conn,
        binding: &'a BindingRequest<'a>,
        ids: &'a IdsFile,
        dependencies: &'a ModuleDeps,
        prepared: &'a mut Prepared,
    ) -> Step<'a, Result<Binding, Error>> {
        Box::pin(async move {
            scratch
                .reconstruction(target, binding, ids, dependencies, prepared)
                .await
                .map(|binding| Binding::Postgres(Box::new(binding)))
        })
    }

    fn compare_externals<'a>(
        scratch: &'a Self::Scratch<'_>,
        provisioning: Provisioning,
        prepared: &'a mut Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        binding: &'a Binding,
    ) -> Step<'a, Result<Staged, Error>> {
        Box::pin(async move {
            match binding {
                Binding::Postgres(binding) => scratch
                    .compare_externals(provisioning, prepared, admin, target, binding)
                    .await
                    .map(Staged::Postgres),
                Binding::Mssql(_) => Err(unsupported()),
            }
        })
    }

    fn staged(_: &Self::Prepared) -> Staged {
        Staged::Postgres(Default::default())
    }

    fn completed_staged(_: &Self::Prepared) -> Option<Staged> {
        None
    }

    fn chains(binding: &mut Binding, prepared: &Self::Prepared) -> Result<Vec<String>, Error> {
        match binding {
            Binding::Postgres(binding) => Ok(binding.chains(prepared)),
            Binding::Mssql(_) => Err(unsupported()),
        }
    }

    fn compile<'a>(
        binding: &'a mut Binding,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        extras: &'a [String],
        zone: Option<&'a SessionZone>,
        outside_writes: OutsideWrites,
        staged: &'a mut Staged,
    ) -> Step<'a, Result<(), Error>> {
        Box::pin(async move {
            match (binding, staged) {
                (Binding::Postgres(binding), Staged::Postgres(staged)) => {
                    binding
                        .compile(
                            prepared,
                            admin,
                            target,
                            extras,
                            zone,
                            outside_writes,
                            staged,
                        )
                        .await
                }
                _ => Err(unsupported()),
            }
        })
    }

    fn capture_desired<'a>(
        binding: &'a Binding,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        _: &'a mut Conn,
        extras: &'a [String],
        visibility: &'a BTreeMap<String, pbps_db::resolver::Observation>,
        schemas: &'a [String],
        key: &'a EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'a>,
        staged: &'a Staged,
    ) -> Step<'a, Result<CompiledCapture, Error>> {
        Box::pin(async move {
            match (binding, staged) {
                (Binding::Postgres(binding), Staged::Postgres(staged)) => binding
                    .capture_desired(
                        prepared, admin, extras, visibility, schemas, key, desired, staged,
                    )
                    .await
                    .map(|closing| CompiledCapture::Postgres(Box::new(closing))),
                _ => Err(unsupported()),
            }
        })
    }

    /// See `Scratch::naming`.
    fn naming(scratch: &Self::Scratch<'_>) -> Result<Naming, Error> {
        Ok(scratch.naming())
    }

    /// See `Scratch::rehearsal_roles`.
    fn rehearsal_roles<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        admin: &'a mut Conn,
        roles: &'a [RoleName],
        naming: Naming,
    ) -> Step<'a, Result<Result<(), String>, Error>> {
        Box::pin(async move { scratch.rehearsal_roles(admin, roles, naming).await })
    }
}

impl PreparedView for Prepared {
    fn authorization_condition<'a>(
        &'a mut self,
        _: &'a mut Conn,
    ) -> Step<'a, Result<Option<Box<pbps_mssql::resolver::authorization::Condition>>, Error>> {
        Box::pin(async move { Ok(None) })
    }

    fn dependency_placements(&self) -> BTreeMap<DependencyName, DependencyPlacement> {
        Default::default()
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
        Prepared::session(self, admin)
    }

    fn deployer<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn {
        Prepared::session(self, admin)
    }

    fn reader(&self) -> Step<'_, Result<Option<Conn>, Error>> {
        Box::pin(async move { Ok(None) })
    }

    fn differences(&self) -> &[String] {
        &self.differences
    }

    fn supplied(&self) -> bool {
        Prepared::supplied(self)
    }

    fn has_filled(&self) -> bool {
        Prepared::has_filled(self)
    }

    fn unfillable(&self) -> Vec<String> {
        Prepared::unfillable(self)
    }
}

impl ApplyRecheck for PgResolver {
    fn cluster(&self) -> ClusterDomain {
        CLUSTER
    }

    fn cluster_identity<'a>(&'a self, conn: &'a mut Conn) -> Step<'a, Result<String, String>> {
        Box::pin(async move {
            pbps_pg::resolver::scratch::cluster_identity(conn)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn scope_schemas(&self, schemas: &[String], extras: &[String]) -> Result<Vec<String>, String> {
        scope::scope_schemas(Driver::Postgres, schemas, extras)
    }

    fn recheck_opening<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
    ) -> Step<'a, Result<InputManifest, String>> {
        Box::pin(async move {
            let sealed = r.evidence.before();
            pbps_pg::resolver::capture::recheck_opening(conn, sealed, r.key)
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
            let routines = routines(r.evidence);
            let recorded = pbps_pg::resolver::capture::RecordedOwnership {
                schema: closing.schema,
                ids: closing.ids,
                routines: &routines,
                dropped: &BTreeMap::new(),
                namespaces: closing.namespaces,
            };
            pbps_pg::resolver::capture::capture_closing(
                conn,
                after,
                &after.vanishable(),
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
            let scope = pbps_pg::resolver::environment::Scope {
                schemas: &target.schemas,
                write_path_extras: &target.write_path_extras,
            };
            pbps_pg::resolver::scope_facts_in_transaction(conn, &scope, r.scope_schemas)
                .await
                .map(|(catalog, authorization)| {
                    (catalog, scope::Authorization::Postgres(authorization))
                })
                .map_err(|error| FactsError::Unread(error.to_string()))
        })
    }

    fn object_grants<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
        phase: Phase,
    ) -> Step<'a, Result<GrantRecheck, String>> {
        Box::pin(async move {
            let grants = &r.evidence.authorization().object_grants;
            let read = pbps_pg::catalog::introspect_within_transaction(conn)
                .await
                .map_err(|error| error.to_string())?;
            let end = match phase {
                Phase::Before => scope::object_grants::End::Before,
                Phase::After => scope::object_grants::End::After,
            };
            scope::object_grants::recheck(r.key, r.changes, grants, &read, end)
        })
    }
}

/// Every declared routine's identity the evidence proved, by its module:
/// the closing read classifies a record it does not hold against them.
fn routines(evidence: &ResolverEvidence) -> BTreeMap<ModuleId, ObjectIdentity> {
    let mut routines = BTreeMap::new();
    let mut proved = |object: &ObjectIdentity, ownership: &ObjectOwnership| {
        if let ObjectOwnership::Surface(Surface::Module(id @ ModuleId::Routine(_))) = ownership
            && object.class == "pg_proc"
        {
            routines.insert(id.clone(), object.clone());
        }
    };
    for record in evidence
        .before()
        .prerequisites()
        .iter()
        .chain(evidence.after().prerequisites())
    {
        proved(&record.object, &record.ownership);
    }
    for transition in evidence.transitions() {
        if let Surface::Module(id @ ModuleId::Routine(_)) = &transition.surface {
            for object in transition.after.iter().filter(|o| o.class == "pg_proc") {
                routines.insert(id.clone(), object.clone());
            }
        }
    }
    routines
}
