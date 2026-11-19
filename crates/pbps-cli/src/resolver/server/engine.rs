//! Engine routing for the scratch run. The CLI keeps the lifecycle; each
//! adapter keeps the backend checks, reconstruction and private captures its
//! engine needs. Only logical records and verdicts leave those captures.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. The binding calls below hand the PostgreSQL adapter the managed
//! declarations and return its comparison; nothing here seals evidence.

use super::scratch::Provisioning;
use super::{BindingRequest, Error, ProduceError};
use crate::resolver::contract::ResolverEngine;
use crate::resolver::scope;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_model::Hints;
use pbps_model::resolver::{InputManifest, OutsideWrites, SurfaceResolution};

pub(crate) mod container;
mod keeper;
pub(super) mod mssql;
mod postgres;
#[cfg(test)]
pub(super) use keeper::pause_between_reads;
pub(super) use keeper::{keep_and_mark, keeper_seen};
pub(super) use postgres::Naming;
#[cfg(test)]
pub(super) use postgres::{
    for_tests::{
        clear_release_roles_hop, externals_for_test, hop_release, hop_release_roles,
        reset_for_test, stop_after_cleanup, sweep_cluster_on, take_release_roles_trace,
        take_release_trace, trace_release, trace_release_roles,
    },
    release_run_owned,
};

// These are logical planning records, not a raw engine capture or a verifier.
pub(super) use pbps_pg::resolver::capture::BindingRecord;

const NO_BINDING_ADAPTER: &str =
    "SQL Server has no binding adapter yet; its design and implementation are #619 and #620";

fn unsupported() -> Error {
    Error::Binding("the resolver is implemented for PostgreSQL only".into())
}

/// Concrete planning type retained from the engine adapter through publication.
/// Configuration comes from the qualified capture, never a cross-engine cast.
pub trait PlanningDialect: pbps_dialect::Dialect + Clone + Send + Sync + 'static {
    /// The engine's resolver, chosen here once. An engine without one writes
    /// `NoResolver<Self>`, and every resolver operation it lacks is refused by
    /// name (DEC-2798.5).
    type Resolver: crate::resolver::contract::ResolverEngine<Dialect = Self>;
    const DRIVER: Driver;
    /// How an engine is named in a refusal. Duplicates `Engine::name` in the
    /// binary, which the library cannot reach; a binary test pins the two equal.
    const NAME: &'static str;
    /// Whether an index filter binds at creation; see
    /// `Dialect::filter_binds_expression`.
    const FILTER_BINDS: bool;
    fn configured(extras: &[String], version: Option<i64>) -> Self;
    fn binding_classes(surface: &pbps_model::resolver::Surface) -> Option<&'static [&'static str]>;
    fn dependent_object(
        record: &BindingRecord,
        reached: &std::collections::BTreeSet<pbps_model::resolver::ObjectIdentity>,
    ) -> Option<pbps_model::resolver::ObjectIdentity>;
}
impl PlanningDialect for pbps_pg::Postgres {
    type Resolver = postgres::PgResolver;
    const DRIVER: Driver = Driver::Postgres;
    const NAME: &'static str = "PostgreSQL";
    const FILTER_BINDS: bool = true;
    fn configured(extras: &[String], version: Option<i64>) -> Self {
        let dialect = Self::with_write_path_extras(extras.to_vec());
        version.map_or(dialect.clone(), |v| dialect.with_server_version(v))
    }
    fn binding_classes(surface: &pbps_model::resolver::Surface) -> Option<&'static [&'static str]> {
        pbps_pg::resolver::binding_classes(surface)
    }
    fn dependent_object(
        record: &BindingRecord,
        reached: &std::collections::BTreeSet<pbps_model::resolver::ObjectIdentity>,
    ) -> Option<pbps_model::resolver::ObjectIdentity> {
        pbps_pg::resolver::dependent_object(&record.object, &record.bindings, reached)
    }
}
impl PlanningDialect for pbps_mssql::Mssql {
    type Resolver = mssql::MssqlResolver;
    const DRIVER: Driver = Driver::Mssql;
    const NAME: &'static str = "SQL Server";
    const FILTER_BINDS: bool = false;
    fn configured(_: &[String], _: Option<i64>) -> Self {
        Self
    }
    fn binding_classes(surface: &pbps_model::resolver::Surface) -> Option<&'static [&'static str]> {
        pbps_mssql::resolver::binding_classes(surface)
    }
    fn dependent_object(
        record: &BindingRecord,
        reached: &std::collections::BTreeSet<pbps_model::resolver::ObjectIdentity>,
    ) -> Option<pbps_model::resolver::ObjectIdentity> {
        pbps_mssql::resolver::dependent_object(&record.object, &record.bindings, reached)
    }
}

/// The standard the scratch is put into, for the engine the dialect names. A
/// standard for another engine is a type error, not a runtime refusal:
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::Standard;
/// fn for_sql_server(_: Standard<pbps_mssql::Mssql>) {}
/// for_sql_server(pbps_pg::resolver::standard::Standard::default());
/// ```
///
/// Each engine's own standard is the type its adapter holds:
///
/// ```
/// use pbps_cli::resolver::server::scratch::Standard;
/// fn for_postgres(_: Standard<pbps_pg::Postgres>) {}
/// for_postgres(pbps_pg::resolver::standard::Standard::default());
/// let _: Standard<pbps_mssql::Mssql> = ();
/// ```
pub type Standard<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::Standard;

pub enum Scratch<'a> {
    Postgres(Box<postgres::Scratch<'a>>),
    Mssql(Box<mssql::Scratch<'a>>),
}
pub(super) enum Prepared {
    Postgres(postgres::Prepared),
    Mssql(mssql::Prepared),
}
pub(super) enum Binding {
    Postgres(Box<postgres::Binding>),
    Mssql(Box<mssql::Binding>),
}
pub(super) enum Staged {
    Postgres(pbps_pg::resolver::capture::Staged),
    Mssql(pbps_mssql::resolver::capture::Staged),
}
pub(super) enum CompiledCapture {
    Postgres(Box<postgres::Closing>),
    Mssql(Box<mssql::Closing>),
}
pub(super) enum CapturedInputs {
    Postgres(pbps_pg::resolver::capture::CapturedInputs),
    Mssql(pbps_mssql::resolver::capture::CapturedInputs),
}

/// The standard an entry declares, or the refusal of one the engine cannot
/// hold.
pub fn declared_standard<D: PlanningDialect>(
    declared: Option<&pbps_config::resolver::ScratchStandard>,
) -> Result<Standard<D>, Error> {
    <D::Resolver as ResolverEngine>::standard(declared)
}

impl Scratch<'_> {
    /// A code vet stages only the operator's externals file. Automatic fill
    /// could install an extension whose absence must be named by CREATE.
    pub(super) async fn prepare_code(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        target: &CatalogFacts,
        authorization: &scope::Authorization,
        schemas: &[String],
    ) -> Result<Prepared, Error> {
        match self {
            Self::Postgres(scratch) => scratch
                .prepare(
                    provisioning,
                    admin,
                    target,
                    authorization,
                    schemas,
                    &Default::default(),
                )
                .await
                .map(Prepared::Postgres),
            Self::Mssql(_) => Err(unsupported()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        target_connection: &mut Conn,
        target: &CatalogFacts,
        authorization: &scope::Authorization,
        schemas: &[String],
        binding: &BindingRequest<'_>,
        extras: &[String],
        key: Option<&EnvironmentFingerprintKey>,
    ) -> Result<Prepared, Error> {
        match self {
            Self::Postgres(scratch) => {
                let fill = postgres::fill(target_connection, binding, extras).await?;
                let mut prepared = scratch
                    .prepare(provisioning, admin, target, authorization, schemas, &fill)
                    .await?;
                let problems = pbps_pg::resolver::scratch::extension_prerequisites(
                    prepared.session(admin),
                    &binding.base.extensions,
                )
                .await
                .map_err(|error| {
                    Error::Read(format!("scratch extension prerequisites: {error}"))
                })?;
                if !problems.is_empty() {
                    return Err(Error::Binding(problems.join("; ")));
                }
                Ok(Prepared::Postgres(prepared))
            }
            Self::Mssql(scratch) => scratch
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
                .map(Prepared::Mssql),
        }
    }

    pub(super) async fn reconstruction(
        &self,
        target: &mut Conn,
        binding: &BindingRequest<'_>,
        ids: &pbps_model::IdsFile,
        dependencies: &pbps_model::ModuleDeps,
        prepared: &mut Prepared,
    ) -> Result<Binding, Error> {
        match self {
            Self::Mssql(scratch) => scratch
                .reconstruction(binding, ids, dependencies)
                .map(|binding| Binding::Mssql(Box::new(binding))),
            Self::Postgres(scratch) => {
                let Prepared::Postgres(prepared) = prepared else {
                    return Err(unsupported());
                };
                scratch
                    .reconstruction(target, binding, ids, dependencies, prepared)
                    .await
                    .map(|binding| Binding::Postgres(Box::new(binding)))
            }
        }
    }

    pub(super) async fn compare_externals(
        &self,
        provisioning: Provisioning,
        prepared: &mut Prepared,
        admin: &mut Conn,
        target: &mut Conn,
        binding: &Binding,
    ) -> Result<Staged, Error> {
        match (self, prepared, binding) {
            (Self::Mssql(_), Prepared::Mssql(prepared), _) => {
                Ok(Staged::Mssql(prepared.staged.clone()))
            }
            (Self::Mssql(_), Prepared::Postgres(_), _)
            | (Self::Postgres(_), Prepared::Mssql(_), _) => Err(unsupported()),
            (Self::Postgres(scratch), Prepared::Postgres(prepared), Binding::Postgres(binding)) => {
                scratch
                    .compare_externals(provisioning, prepared, admin, target, binding)
                    .await
                    .map(Staged::Postgres)
            }
            (Self::Postgres(_), Prepared::Postgres(_), Binding::Mssql(_)) => Err(unsupported()),
        }
    }

    /// See `postgres::Scratch::naming`. SQL Server has no cluster role to
    /// name: its roles are the scratch database's own and a rehearsal asks
    /// `scratch::rehearse::principals` instead (DEC-2379.3).
    pub(super) fn naming(&self) -> Result<Naming, Error> {
        match self {
            Self::Postgres(scratch) => Ok(scratch.naming()),
            Self::Mssql(_) => Err(unsupported()),
        }
    }

    /// See `postgres::Scratch::rehearsal_roles`.
    pub(super) async fn rehearsal_roles(
        &mut self,
        admin: &mut Conn,
        roles: &[pbps_model::RoleName],
        naming: Naming,
    ) -> Result<Result<(), String>, Error> {
        match self {
            Self::Postgres(scratch) => scratch.rehearsal_roles(admin, roles, naming).await,
            Self::Mssql(_) => Err(unsupported()),
        }
    }

    pub(super) async fn release(&mut self, admin: &mut Conn) -> Result<(), Vec<String>> {
        match self {
            Self::Postgres(scratch) => scratch.release(admin).await,
            Self::Mssql(scratch) => scratch.release(admin).await,
        }
    }
}

impl Prepared {
    pub(super) async fn authorization_condition(
        &mut self,
        target: &mut Conn,
    ) -> Result<Option<Box<pbps_mssql::resolver::authorization::Condition>>, Error> {
        match self {
            Self::Postgres(_) => Ok(None),
            Self::Mssql(prepared) => prepared.authorization_condition(target).await.map(Some),
        }
    }
    pub(super) fn completed_staged(&self) -> Option<Staged> {
        match self {
            Self::Mssql(prepared) => Some(Staged::Mssql(prepared.staged.clone())),
            Self::Postgres(_) => None,
        }
    }

    pub(super) fn dependency_placements(
        &self,
    ) -> std::collections::BTreeMap<
        pbps_model::dependency::DependencyName,
        pbps_model::resolver::DependencyPlacement,
    > {
        match self {
            Self::Postgres(_) => Default::default(),
            Self::Mssql(prepared) => prepared.dependency_placements(),
        }
    }

    pub(super) fn catalog(&self) -> &CatalogFacts {
        match self {
            Self::Postgres(prepared) => &prepared.scratch_catalog,
            Self::Mssql(prepared) => &prepared.scratch_catalog,
        }
    }
    #[cfg(test)]
    pub(super) fn catalog_mut(&mut self) -> &mut CatalogFacts {
        match self {
            Self::Postgres(prepared) => &mut prepared.scratch_catalog,
            Self::Mssql(prepared) => &mut prepared.scratch_catalog,
        }
    }
    pub(super) fn into_catalog(self) -> CatalogFacts {
        match self {
            Self::Postgres(prepared) => prepared.scratch_catalog,
            Self::Mssql(prepared) => prepared.scratch_catalog,
        }
    }
    /// The session the scratch's statements run on: the reproduced
    /// deployer's where the run made its own login, the checked connection
    /// where the scratch account compiles as itself.
    pub(super) fn session<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn {
        match self {
            Self::Postgres(prepared) => prepared.session(admin),
            Self::Mssql(_) => admin,
        }
    }

    /// The session a rehearsal's statements run in: the reproduced deployer's,
    /// as an apply's are, where the scratch has one (SQL Server), and the
    /// checked connection otherwise.
    pub(super) fn deployer<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn {
        match self {
            Self::Postgres(prepared) => prepared.session(admin),
            Self::Mssql(prepared) => &mut prepared.compile,
        }
    }

    /// A connection that reads the scratch database as a whole, for the
    /// comparison after the plan ran: the deployer's cannot leave its context
    /// and may lack `VIEW DEFINITION`. `None` where the session reads it itself.
    pub(super) async fn reader(&self) -> Result<Option<Conn>, Error> {
        match self {
            Self::Postgres(_) => Ok(None),
            Self::Mssql(prepared) => prepared.reach().open().await.map(Some),
        }
    }
    pub(super) fn differences(&self) -> &[String] {
        match self {
            Self::Postgres(prepared) => &prepared.differences,
            Self::Mssql(prepared) => &prepared.differences,
        }
    }
    pub(super) fn supplied(&self) -> bool {
        match self {
            Self::Postgres(prepared) => prepared.supplied(),
            Self::Mssql(_) => false,
        }
    }
    pub(super) fn has_filled(&self) -> bool {
        match self {
            Self::Postgres(prepared) => prepared.has_filled(),
            Self::Mssql(prepared) => !prepared.staged.compared().is_empty(),
        }
    }
    pub(super) fn unfillable(&self) -> Vec<String> {
        match self {
            Self::Postgres(prepared) => prepared.unfillable(),
            Self::Mssql(_) => Vec::new(),
        }
    }
}

impl Binding {
    pub(super) fn has_fill(&self) -> bool {
        match self {
            Self::Postgres(binding) => binding.has_fill(),
            Self::Mssql(_) => false,
        }
    }
    pub(super) fn chains(&mut self, prepared: &Prepared) -> Vec<String> {
        match (self, prepared) {
            (Self::Postgres(binding), Prepared::Postgres(prepared)) => binding.chains(prepared),
            (Self::Mssql(_), Prepared::Mssql(_)) => Vec::new(),
            (Self::Postgres(_), Prepared::Mssql(_)) | (Self::Mssql(_), Prepared::Postgres(_)) => {
                unreachable!("a binding and its preparation come from one engine")
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compile(
        &mut self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        target: &mut Conn,
        extras: &[String],
        zone: Option<&pbps_model::SessionZone>,
        outside: OutsideWrites,
        staged: &mut Staged,
    ) -> Result<(), Error> {
        match (self, prepared, staged) {
            (Self::Postgres(binding), Prepared::Postgres(prepared), Staged::Postgres(staged)) => {
                binding
                    .compile(prepared, admin, target, extras, zone, outside, staged)
                    .await
            }
            (Self::Mssql(binding), Prepared::Mssql(prepared), Staged::Mssql(_)) => {
                let len = binding.len();
                binding.compile(prepared, target, 0..len).await
            }
            _ => Err(unsupported()),
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn capture_desired(
        &self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        target: &mut Conn,
        extras: &[String],
        visibility: &std::collections::BTreeMap<String, pbps_db::resolver::Observation>,
        schemas: &[String],
        key: &EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'_>,
        staged: &Staged,
    ) -> Result<CompiledCapture, Error> {
        match (self, prepared, staged) {
            (Self::Postgres(binding), Prepared::Postgres(prepared), Staged::Postgres(staged)) => {
                binding
                    .capture_desired(
                        prepared, admin, extras, visibility, schemas, key, desired, staged,
                    )
                    .await
                    .map(|closing| CompiledCapture::Postgres(Box::new(closing)))
            }
            (Self::Mssql(binding), Prepared::Mssql(prepared), Staged::Mssql(staged)) => binding
                .capture_desired(prepared.reach(), target, schemas, key, desired, staged)
                .await
                .map(|closing| CompiledCapture::Mssql(Box::new(closing))),
            (Self::Postgres(_), ..) | (Self::Mssql(_), ..) => Err(unsupported()),
        }
    }
    pub(super) async fn capture_opening(
        &self,
        target: &mut Conn,
        closing: &CompiledCapture,
        key: &EnvironmentFingerprintKey,
        base: pbps_diff::Side<'_>,
    ) -> Result<(CapturedInputs, InputManifest), Error> {
        match (self, closing) {
            (Self::Postgres(binding), CompiledCapture::Postgres(closing)) => {
                let (current, opening) =
                    binding.capture_opening(target, closing, key, base).await?;
                Ok((CapturedInputs::Postgres(current), opening))
            }
            (Self::Mssql(binding), CompiledCapture::Mssql(closing)) => {
                let (current, opening) =
                    binding.capture_opening(target, closing, key, base).await?;
                Ok((CapturedInputs::Mssql(current), opening))
            }
            (Self::Postgres(_), CompiledCapture::Mssql(_))
            | (Self::Mssql(_), CompiledCapture::Postgres(_)) => Err(unsupported()),
        }
    }
    pub(super) fn assess(
        &mut self,
        closing: &CompiledCapture,
        current: &CapturedInputs,
        staged: &Staged,
    ) -> Result<Assessment, Error> {
        match (self, closing, current, staged) {
            (
                Self::Postgres(binding),
                CompiledCapture::Postgres(closing),
                CapturedInputs::Postgres(current),
                Staged::Postgres(staged),
            ) => Ok(binding.assess(closing, current, staged)),
            (
                Self::Mssql(binding),
                CompiledCapture::Mssql(closing),
                CapturedInputs::Mssql(current),
                Staged::Mssql(staged),
            ) => Ok(binding.assess(closing, current, staged)),
            (Self::Mssql(_), ..) => Err(unsupported()),
            (Self::Postgres(_), ..) => Err(unsupported()),
        }
    }
}

impl Staged {
    pub(super) fn changed_since(&self, current: &CapturedInputs) -> Vec<String> {
        match (self, current) {
            (Self::Postgres(staged), CapturedInputs::Postgres(current)) => {
                staged.changed_since(current)
            }
            (Self::Mssql(staged), CapturedInputs::Mssql(current)) => staged.changed_since(current),
            (Self::Postgres(_), CapturedInputs::Mssql(_))
            | (Self::Mssql(_), CapturedInputs::Postgres(_)) => {
                unreachable!("a staged set and its capture come from one engine")
            }
        }
    }
    pub(super) fn through_shape_view(&self, error: Error) -> Error {
        match self {
            Self::Postgres(staged) => postgres::through_shape_view(error, staged),
            Self::Mssql(_) => error,
        }
    }
}

pub(super) fn staged(driver: Driver, prepared: &Prepared) -> Result<Staged, Error> {
    match (driver, prepared) {
        (Driver::Postgres, _) => Ok(Staged::Postgres(Default::default())),
        // Preparation always compares the externals, an empty set included.
        (Driver::Mssql, Prepared::Mssql(prepared)) => Ok(Staged::Mssql(prepared.staged.clone())),
        (Driver::Mssql, Prepared::Postgres(_)) => Err(unsupported()),
    }
}

impl CompiledCapture {
    pub(super) fn planning_records(
        &self,
    ) -> Result<Vec<BindingRecord>, pbps_model::resolver::ManifestError> {
        match self {
            Self::Postgres(closing) => closing.planning_records(),
            Self::Mssql(closing) => Ok(closing
                .planning_records()?
                .into_iter()
                .map(|record| BindingRecord {
                    object: record.object,
                    ownership: record.ownership,
                    bindings: record.bindings,
                })
                .collect()),
        }
    }
    pub(super) fn seal_for_plan(
        self,
    ) -> Result<InputManifest, pbps_pg::resolver::capture::SealError> {
        match self {
            Self::Postgres(closing) => closing.seal_for_plan(),
            Self::Mssql(closing) => closing.seal_for_plan().map_err(Into::into),
        }
    }
}

pub(super) async fn recapture(
    target: &mut Conn,
    current: &CapturedInputs,
) -> Result<Vec<CaptureDifference>, CaptureError> {
    match current {
        CapturedInputs::Postgres(current) => postgres::recapture(target, current).await,
        CapturedInputs::Mssql(current) => pbps_mssql::resolver::capture::recapture(target, current)
            .await
            .map(|(_, differences)| differences),
    }
}

pub(super) fn ordinary<D: PlanningDialect>(
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    extras: &[String],
) -> Result<pbps_diff::order::DiffPlan, Error> {
    pbps_diff::resolver::ordinary(base, desired, &D::configured(extras, None), hints).map_err(
        |errors| Error::Binding(format!("ordinary typed plan cannot be formed: {errors:?}")),
    )
}

impl Binding {
    pub(super) fn plan<D: PlanningDialect>(
        &self,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        hints: &Hints,
        extras: &[String],
        surfaces: &[SurfaceResolution],
        captured: &pbps_model::operation::Captured,
    ) -> Result<pbps_diff::resolver::Ordered<pbps_diff::order::Canonical<D>>, Error> {
        let version = match self {
            Self::Postgres(binding) if D::DRIVER == Driver::Postgres => {
                Some(binding.generation_version())
            }
            Self::Mssql(_) if D::DRIVER == Driver::Mssql => None,
            Self::Postgres(_) | Self::Mssql(_) => {
                return Err(Error::Binding(
                    "the planning dialect and qualified capture belong to different engines".into(),
                ));
            }
        };
        let dialect = D::configured(extras, version);
        pbps_diff::resolver::plan_captured(base, desired, hints, surfaces, &dialect, captured)
            .map_err(|error| Error::Binding(error.to_string()))
    }
}

/// What the run bound at creation, for ordering: the schema-bound callees of
/// each module the capture read whole, on the target now and on the scratch
/// database the declarations compiled on (DEC-2143.8). PostgreSQL has no
/// such lexical guess to replace, so it brings none.
pub(super) fn captured_bindings(
    compiled: &CompiledCapture,
    current: &CapturedInputs,
    base: &pbps_model::Schema,
    desired: &pbps_model::Schema,
) -> pbps_model::operation::Captured {
    match (compiled, current) {
        (CompiledCapture::Mssql(compiled), CapturedInputs::Mssql(current)) => {
            pbps_model::operation::Captured {
                before: current.schema_bound_callees(base),
                after: compiled.schema_bound_callees(desired),
            }
        }
        _ => Default::default(),
    }
}

pub(super) fn surfaces<D: PlanningDialect>(
    driver: Driver,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    opening: &[BindingRecord],
    compiled: &[BindingRecord],
    assessment: &Assessment,
) -> Result<Vec<SurfaceResolution>, Error> {
    if driver != D::DRIVER {
        return Err(unsupported());
    }
    super::resolution::from_sides::<D>(base, desired, opening, compiled, assessment)
}

pub(super) fn reported_build(
    driver: Driver,
    key: &EnvironmentFingerprintKey,
    catalog: &CatalogFacts,
) -> Result<String, Error> {
    match driver {
        Driver::Postgres => postgres::reported_build(key, catalog),
        Driver::Mssql => mssql::reported_build(key, catalog),
    }
}

pub(super) fn reported_rule(driver: Driver) -> Result<&'static str, Error> {
    match driver {
        Driver::Postgres => Ok(pbps_pg::resolver::compatibility::REPORTED_RULE),
        Driver::Mssql => Ok(pbps_mssql::resolver::compatibility::REPORTED_RULE),
    }
}

/// What a failed scratch capture is. Only an answer about the declarations
/// stays a binding verdict; a read that failed, came back incomplete or was
/// overtaken by a change answered nothing (SPEC 9.8, #1575).
pub(super) fn catalog_read_failed(error: pbps_db::resolver::capture::CaptureError) -> Error {
    use pbps_db::resolver::capture::CaptureError;
    match error {
        CaptureError::Unsupported { .. } | CaptureError::Coverage(_) | CaptureError::Version => {
            Error::Binding(error.to_string())
        }
        CaptureError::CallerTransaction
        | CaptureError::NoTransaction
        | CaptureError::Read
        | CaptureError::Engine { .. }
        | CaptureError::Incomplete
        | CaptureError::Changed
        | CaptureError::Unsettled { .. }
        | CaptureError::EnvironmentChanged
        | CaptureError::Close => Error::Read(error.to_string()),
    }
}

#[cfg(test)]
use postgres::compile_failed;

#[cfg(test)]
pub(super) use postgres::reconstruction;
#[cfg(test)]
pub(super) use postgres::{generation_version, release_supplied};

#[cfg(test)]
mod tests {
    use super::{Error, catalog_read_failed, compile_failed};
    use pbps_db::resolver::capture::{CaptureError, Uncovered};
    use pbps_pg::resolver::reconstruct::ReconstructError;

    /// #1575: a capture that failed to read answered nothing about the
    /// bindings, and is not reported as their verdict (SPEC 9.8).
    #[test]
    fn a_failed_catalog_read_is_a_read_failure_and_an_uncovered_input_a_verdict() {
        for read in [
            CaptureError::CallerTransaction,
            CaptureError::NoTransaction,
            CaptureError::Read,
            CaptureError::Engine {
                number: Some("1222".into()),
                text: "lock request time out period exceeded".into(),
            },
            CaptureError::Incomplete,
            CaptureError::Changed,
            CaptureError::Unsettled {
                attempts: 5,
                seconds: 10,
            },
            CaptureError::EnvironmentChanged,
            CaptureError::Close,
        ] {
            assert!(matches!(catalog_read_failed(read), Error::Read(_)));
        }
        for verdict in [
            CaptureError::Unsupported {
                engine: "SQL Server",
            },
            CaptureError::Version,
            CaptureError::Coverage(Uncovered {
                class: "pg_proc".into(),
                object: None,
                condition: "an uncovered routine",
            }),
        ] {
            assert!(matches!(catalog_read_failed(verdict), Error::Binding(_)));
        }
    }

    /// The security review of #1596: compilation reads the catalog back
    /// around each step, and that read failing is not the declaration's
    /// verdict either.
    #[test]
    fn a_failed_read_around_compilation_is_a_read_failure_and_a_failed_statement_a_verdict() {
        let read = |reason: &str| ReconstructError::Read {
            declaration: "app.f()".into(),
            reason: reason.into(),
        };
        assert!(matches!(
            compile_failed(read("connection closed")),
            Error::Read(_)
        ));
        assert!(matches!(
            compile_failed(ReconstructError::Transaction("commit")),
            Error::Read(_)
        ));
        assert!(matches!(
            compile_failed(ReconstructError::Interrupted {
                declaration: "app.t".into(),
                reason: "terminating connection due to administrator command".into(),
            }),
            Error::Read(_)
        ));
        for verdict in [
            ReconstructError::Compile {
                declaration: "app.f()".into(),
                reason: "function app.g() does not exist".into(),
            },
            ReconstructError::Unsupported("DropColumn".into()),
            ReconstructError::Cycle("app.t".into()),
        ] {
            assert!(matches!(compile_failed(verdict), Error::Binding(_)));
        }
    }
}
