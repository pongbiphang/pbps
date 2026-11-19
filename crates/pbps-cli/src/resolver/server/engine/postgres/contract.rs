//! PostgreSQL's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.

use super::{Binding, Closing, Naming, Prepared, Scratch};
use crate::resolver::contract::{
    ApplyRecheck, BindingView, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Never,
    Phase, Planning, PreparedView, Rehearsal, RehearsalRequest, Replay, ResolverEngine, Sealable,
    Step, Support, TargetReadStep, sealed::Sealed,
};
use crate::resolver::scope;
use crate::resolver::server::scratch::rehearse::Roles;
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, PlanningDialect, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::Observation;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::{CatalogFacts, DatabaseRecipe, ScopeReport};
use pbps_db::{Conn, DbError, Driver};
use pbps_model::dependency::DependencyName;
use pbps_model::operation::Captured;
use pbps_model::resolver::{
    AuthorizationGrant, AuthorizationReproduction, DependencyPlacement, OutsideWrites,
};
use pbps_model::resolver::{
    InputManifest, ManifestError, ObjectIdentity, ObjectOwnership, ResolverEvidence, Surface,
};
use pbps_model::role_mapping::RoleMapping;
use pbps_model::{ChangeSet, Hints, IdsFile, ModuleDeps, ModuleId, RoleName, Schema, SessionZone};
use pbps_pg::resolver::authorization as pg_auth;
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
        planning: Some(&Planning {
            cluster: CLUSTER,
            channels_rule: "pbps/pg-channels/v1",
            reproduction: AuthorizationReproduction::for_postgres_layout,
        }),
        rehearsal: Some(&Rehearsal {
            request: RehearsalRequest::Projected,
            run_key: false,
        }),
        preview: None,
        apply: Some(&PgResolver),
    };

    type Standard = pbps_pg::resolver::standard::Standard;
    type Scratch<'a> = Box<Scratch<'a>>;
    const FILTER_BINDS: bool = true;
    type Prepared = Prepared;
    type Binding = Box<Binding>;
    type Staged = pbps_pg::resolver::capture::Staged;
    type CompiledCapture = Box<Closing>;
    type CapturedInputs = pbps_pg::resolver::capture::CapturedInputs;
    type History = pbps_pg::resolver::scope_witness::Witness;
    type Authorization = pg_auth::AuthorizationContext;
    /// PostgreSQL measures no authorization condition: its after-state is the
    /// planned grants projected onto the context it read.
    type Condition = Never;

    fn binding_classes(surface: &Surface) -> Option<&'static [&'static str]> {
        pbps_pg::resolver::binding_classes(surface)
    }

    fn dependent_object(
        record: &BindingRecord,
        reached: &BTreeSet<ObjectIdentity>,
    ) -> Option<ObjectIdentity> {
        pbps_pg::resolver::dependent_object(&record.object, &record.bindings, reached)
    }

    fn retain_grants(
        target: &mut Conn,
    ) -> Step<'_, Result<Option<Box<pbps_db::catalog::Pulled>>, Error>> {
        Box::pin(async move {
            match scope::object_grants::capture(target).await {
                Ok(read) => Ok(Some(Box::new(read))),
                Err(error) => Err(Error::Read(format!("the recheck's object grants: {error}"))),
            }
        })
    }

    fn seal_grants(
        key: &EnvironmentFingerprintKey,
        changes: &ChangeSet,
        ids: &IdsFile,
        held: Option<&pbps_db::catalog::Pulled>,
    ) -> Result<Vec<AuthorizationGrant>, String> {
        scope::object_grants::seal(key, changes, ids, held)
    }

    fn reported_rule() -> Result<&'static str, Error> {
        Ok(pbps_pg::resolver::compatibility::REPORTED_RULE)
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
        authorization: &'a Self::Authorization,
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
        authorization: &'a Self::Authorization,
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

    fn scope_schemas(schemas: &[String], extras: &[String]) -> Result<Vec<String>, String> {
        let mut all = schemas.to_vec();
        for extra in extras {
            if extra != "pg_temp" && !all.contains(extra) {
                all.push(extra.clone());
            }
        }
        Ok(all)
    }

    fn read_catalog<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
    ) -> Step<'a, Result<CatalogFacts, DbError>> {
        Box::pin(async move {
            let scope = pbps_pg::resolver::environment::Scope {
                schemas,
                write_path_extras: extras,
            };
            pbps_pg::resolver::environment::read(connection, &scope).await
        })
    }

    fn read_target<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
        authorization_schemas: &'a [String],
        _: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<(CatalogFacts, Self::Authorization), DbError>> {
        Box::pin(async move {
            let scope = pbps_pg::resolver::environment::Scope {
                schemas,
                write_path_extras: extras,
            };
            let (catalog, authorization) =
                pbps_pg::resolver::scope_facts(connection, &scope, authorization_schemas).await?;
            Ok((catalog, authorization))
        })
    }

    fn read_target_with_history<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
        authorization_schemas: &'a [String],
        _: &'a [scope::PlannedGrant],
    ) -> TargetReadStep<'a, Self::Authorization, Self::History> {
        Box::pin(async move {
            let scope = pbps_pg::resolver::environment::Scope {
                schemas,
                write_path_extras: extras,
            };
            let (catalog, authorization, witness) =
                pbps_pg::resolver::witnessed_scope_facts(connection, &scope, authorization_schemas)
                    .await?;
            Ok((catalog, authorization, witness))
        })
    }

    fn recipe(catalog: &CatalogFacts) -> Result<DatabaseRecipe, String> {
        DatabaseRecipe::from_catalog(catalog).map_err(|error| error.to_string())
    }

    fn compare_reported(
        target: &CatalogFacts,
        resolver: &CatalogFacts,
    ) -> Result<ScopeReport, String> {
        Ok(pbps_pg::resolver::compatibility::compare_reported(
            target, resolver,
        ))
    }

    fn limitation(history: &Self::History) -> Option<(&'static str, &'static str)> {
        history.role_history_limitation()
    }

    fn history_differences(
        before: &Self::History,
        after: &Self::History,
    ) -> Result<Vec<String>, DbError> {
        before.differences(after)
    }

    fn same_context(a: &Self::Authorization, b: &Self::Authorization) -> bool {
        a.canonical() == b.canonical()
    }

    fn canonical(authorization: &Self::Authorization) -> Vec<u8> {
        authorization.canonical()
    }

    fn authorization_rule() -> Result<&'static str, String> {
        Ok(pg_auth::RULE)
    }

    fn transition(
        authorization: &Self::Authorization,
        planned: &[scope::PlannedGrant],
        _: Option<&Self::Condition>,
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        Ok((
            authorization.canonical(),
            pg_auth::with_planned(authorization.clone(), &scope::pg_planned(planned)).canonical(),
        ))
    }

    fn unpredictable(
        authorization: &Self::Authorization,
        planned: &[scope::PlannedGrant],
    ) -> Vec<String> {
        pg_auth::ambiguous_grantors(authorization, &scope::pg_planned(planned))
    }

    fn unusable_schemas(
        authorization: &Self::Authorization,
        planned: &[scope::PlannedGrant],
    ) -> Vec<String> {
        pg_auth::with_planned(authorization.clone(), &scope::pg_planned(planned))
            .schemas
            .into_iter()
            .filter(|(_, schema)| schema.privileges.get("USAGE") != Some(&true))
            .map(|(name, _)| name)
            .collect()
    }

    fn project_visibility(
        authorization: &Self::Authorization,
        planned: &[scope::PlannedGrant],
        schemas: &[String],
        extras: &[String],
        catalog: &mut CatalogFacts,
    ) {
        let expected = pg_auth::with_planned(authorization.clone(), &scope::pg_planned(planned));
        catalog.visibility = expected_visibility(&expected, schemas, extras);
    }

    fn authorization_condition<'a>(
        _: &'a mut Self::Prepared,
        _: &'a mut Conn,
    ) -> Step<'a, Result<Option<Self::Condition>, Error>> {
        Box::pin(async move { Ok(None) })
    }

    fn recheck_condition<'a>(
        condition: &'a Self::Condition,
        _: &'a mut Conn,
    ) -> Step<'a, Result<bool, DbError>> {
        match *condition {}
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
        <Self as ResolverEngine>::scope_schemas(schemas, extras)
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
    ) -> Step<'a, Result<(CatalogFacts, Vec<u8>), FactsError>> {
        Box::pin(async move {
            let target = &r.evidence.qualification().target_scope;
            let scope = pbps_pg::resolver::environment::Scope {
                schemas: &target.schemas,
                write_path_extras: &target.write_path_extras,
            };
            pbps_pg::resolver::scope_facts_in_transaction(conn, &scope, r.scope_schemas)
                .await
                .map(|(catalog, authorization)| (catalog, authorization.canonical()))
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
            acl: pbps_pg::resolver::authorization::SchemaAcl::Default,
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

    #[test]
    fn a_planned_revoke_makes_the_schema_one_the_supplied_login_gives_up() {
        let context = pg_auth::AuthorizationContext {
            principal: pbps_db::resolver::environment::DeploymentPrincipal {
                login: "d".into(),
                effective: "d".into(),
                superuser: false,
            },
            schemas: [(
                "app".to_owned(),
                // The deployer owns it, so its revoke from itself is one it
                // has the authority to make.
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
        };
        // Negative: with nothing planned the deployer keeps its usage.
        assert!(PgResolver::unusable_schemas(&context, &[]).is_empty());
        let revoke = [scope::PlannedGrant {
            principal: "d".into(),
            schema: "app".into(),
            privilege: "USAGE".into(),
            revoke: true,
        }];
        assert_eq!(PgResolver::unusable_schemas(&context, &revoke), ["app"]);
    }
}
