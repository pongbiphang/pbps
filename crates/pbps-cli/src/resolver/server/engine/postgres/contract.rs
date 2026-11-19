//! PostgreSQL's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.

use super::{Binding, Closing, Naming, Prepared, Scratch};
use crate::resolver::contract::{
    ApplyRecheck, BindingView, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Phase,
    Planning, PreparedView, Rehearsal, RehearsalRequest, Replay, ResolverEngine, Sealable, Step,
    Support, sealed::Sealed,
};
use crate::resolver::scope;
use crate::resolver::server::scratch::rehearse::Roles;
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, PlanningDialect, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, DbError, Driver};
use pbps_model::dependency::DependencyName;
use pbps_model::operation::Captured;
use pbps_model::resolver::{DependencyPlacement, OutsideWrites};
use pbps_model::resolver::{
    InputManifest, ManifestError, ObjectIdentity, ObjectOwnership, ResolverEvidence, Surface,
};
use pbps_model::role_mapping::RoleMapping;
use pbps_model::{Hints, IdsFile, ModuleDeps, ModuleId, RoleName, Schema, SessionZone};
use pbps_pg::resolver::capture::{BindingRecord, SealError};
use std::collections::{BTreeMap, BTreeSet};

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
    type Binding = Box<Binding>;
    type Staged = pbps_pg::resolver::capture::Staged;
    type CompiledCapture = Box<Closing>;
    type CapturedInputs = pbps_pg::resolver::capture::CapturedInputs;

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
    ) -> Step<'a, Result<Self::Binding, Error>> {
        Box::pin(async move {
            scratch
                .reconstruction(target, binding, ids, dependencies, prepared)
                .await
                .map(Box::new)
        })
    }

    fn compare_externals<'a>(
        scratch: &'a Self::Scratch<'_>,
        provisioning: Provisioning,
        prepared: &'a mut Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        binding: &'a Self::Binding,
    ) -> Step<'a, Result<Self::Staged, Error>> {
        Box::pin(async move {
            scratch
                .compare_externals(provisioning, prepared, admin, target, binding)
                .await
        })
    }

    fn staged(_: &Self::Prepared) -> Self::Staged {
        Default::default()
    }

    fn completed_staged(_: &Self::Prepared) -> Option<Self::Staged> {
        None
    }

    fn chains(binding: &mut Self::Binding, prepared: &Self::Prepared) -> Vec<String> {
        binding.chains(prepared)
    }

    fn compile<'a>(
        binding: &'a mut Self::Binding,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        extras: &'a [String],
        zone: Option<&'a SessionZone>,
        outside_writes: OutsideWrites,
        staged: &'a mut Self::Staged,
    ) -> Step<'a, Result<(), Error>> {
        Box::pin(async move {
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
        })
    }

    fn capture_desired<'a>(
        binding: &'a Self::Binding,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        _: &'a mut Conn,
        extras: &'a [String],
        visibility: &'a BTreeMap<String, pbps_db::resolver::Observation>,
        schemas: &'a [String],
        key: &'a EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'a>,
        staged: &'a Self::Staged,
    ) -> Step<'a, Result<Self::CompiledCapture, Error>> {
        Box::pin(async move {
            binding
                .capture_desired(
                    prepared, admin, extras, visibility, schemas, key, desired, staged,
                )
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
        Box::pin(super::recapture(target, current))
    }

    fn captured_bindings(
        _: &Self::CompiledCapture,
        _: &Self::CapturedInputs,
        _: &Schema,
        _: &Schema,
    ) -> Captured {
        Default::default()
    }

    fn through_shape_view(staged: &Self::Staged, error: Error) -> Error {
        super::through_shape_view(error, staged)
    }

    fn dialect(binding: &Self::Binding, extras: &[String]) -> Self::Dialect {
        <pbps_pg::Postgres as PlanningDialect>::configured(
            extras,
            Some(binding.generation_version()),
        )
    }

    fn rehearsal_base<'a>(
        scratch: &'a Self::Scratch<'_>,
        provisioning: Provisioning,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        binding: &'a BindingRequest<'a>,
        _: &'a IdsFile,
        desired_ids: &'a IdsFile,
        hints: &'a Hints,
        desired: &'a Schema,
        externals: Option<&'a str>,
    ) -> Step<'a, Result<(), Error>> {
        Box::pin(async move {
            if externals.is_some() || prepared.has_filled() {
                let mut reconstruction = Self::reconstruction(
                    scratch,
                    target,
                    binding,
                    desired_ids,
                    &hints.annotations.modules(desired),
                    prepared,
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
                Self::chains(&mut reconstruction, prepared);
                Self::compare_externals(
                    scratch,
                    provisioning,
                    prepared,
                    admin,
                    target,
                    &reconstruction,
                )
                .await?;
            }
            Ok(())
        })
    }

    /// The roles of the run, by the naming its placement decides. Here and in the
    /// one place the plan is lowered again (`OrderedPlan::under_role_names`) is all
    /// the code that tells the three apart.
    fn rehearsal_roles<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        admin: &'a mut Conn,
        named: &'a BTreeSet<RoleName>,
        _: &'a Schema,
        _: &'a Schema,
        tokens: &'a Tokens,
    ) -> Step<'a, Result<Roles, Error>> {
        Box::pin(async move {
            // The engine's own roles keep their names: they are the scratch's too.
            let own = |role: &RoleName| role.raw().starts_with("pg_");
            let naming = scratch.naming();
            let mapping = match naming {
                Naming::Existing => RoleMapping::default(),
                Naming::Created => {
                    RoleMapping::unchanged(named.iter().filter(|role| !own(role)).cloned())
                }
                Naming::Mapped => {
                    RoleMapping::generate(named, "pbps_rehearsal", &tokens.scratch, own)
                }
            };
            Ok(
                match scratch
                    .rehearsal_roles(admin, mapping.created(), naming)
                    .await?
                {
                    Ok(()) => Ok(mapping),
                    Err(why) => Err(why),
                },
            )
        })
    }
}

impl Sealable for Box<Closing> {
    fn planning_records(&self) -> Result<Vec<BindingRecord>, ManifestError> {
        Closing::planning_records(self)
    }

    fn seal_for_plan(self) -> Result<InputManifest, SealError> {
        Closing::seal_for_plan(*self)
    }
}

impl BindingView for Box<Binding> {
    fn has_fill(&self) -> bool {
        Binding::has_fill(self)
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
