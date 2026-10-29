//! Engine routing for the scratch run. The CLI keeps the lifecycle; each
//! adapter keeps the backend checks, reconstruction and private captures its
//! engine needs. Only logical records and verdicts leave those captures.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. The binding calls below hand the PostgreSQL adapter the managed
//! declarations and return its comparison; nothing here seals evidence.

use super::scratch::{Provenance, Provisioning, Tokens};
use super::{BindingRequest, Error, ProduceError, ScopeRequest};
use crate::resolver::scope;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, DbError, Driver};
use pbps_model::Hints;
use pbps_model::resolver::{InputManifest, OutsideWrites, SurfaceResolution};

mod keeper;
pub(super) mod mssql;
mod postgres;
#[cfg(test)]
pub(super) use keeper::pause_between_reads;
pub(super) use keeper::{keep_and_mark, keeper_seen};
pub(super) use postgres::Naming;
#[cfg(test)]
pub(super) use postgres::{
    for_tests::{externals_for_test, reset_for_test, stop_after_cleanup, sweep_cluster_on},
    release_run_owned,
};

// These are logical planning records, not a raw engine capture or a verifier.
pub(super) use pbps_pg::resolver::capture::BindingRecord;

const NO_BINDING_ADAPTER: &str =
    "SQL Server has no binding adapter yet; its design and implementation are #619 and #620";

/// Where a SQL Server run stops: reconstruction, the compiled capture and
/// the target's opening capture exist, and planning from their evidence is
/// #2143. Assessment exists; ordering from that evidence is the next boundary.
const PLANNING_FROM_EVIDENCE: &str =
    "SQL Server planning from evidence is not implemented yet (#2143)";

fn unsupported() -> Error {
    Error::Binding("the resolver is implemented for PostgreSQL only".into())
}

/// The standard is engine-owned; the PostgreSQL conversion keeps callers of
/// the supplied-server API from having to construct adapter state themselves.
#[derive(Clone)]
pub enum Standard {
    Postgres(pbps_pg::resolver::standard::Standard),
    Mssql,
}

impl From<&pbps_pg::resolver::standard::Standard> for Standard {
    fn from(value: &pbps_pg::resolver::standard::Standard) -> Self {
        Self::Postgres(value.clone())
    }
}

impl From<&Standard> for Standard {
    fn from(value: &Standard) -> Self {
        value.clone()
    }
}

pub(super) enum Scratch<'a> {
    Postgres(Box<postgres::Scratch<'a>>),
    Mssql(Box<mssql::Scratch<'a>>),
}
pub(super) enum Prepared {
    Postgres(postgres::Prepared),
    Mssql(mssql::Prepared),
}
pub(super) enum Binding {
    Postgres(Box<postgres::Binding>),
    Mssql(mssql::Binding),
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

pub fn declared_standard(
    driver: Driver,
    declared: Option<&pbps_config::resolver::ScratchStandard>,
) -> Result<Standard, Error> {
    match driver {
        Driver::Postgres => Ok(Standard::Postgres(postgres::declared_standard(declared))),
        Driver::Mssql if declared.is_none() => Ok(Standard::Mssql),
        Driver::Mssql => Err(Error::Unusable(
            "SQL Server scratch does not accept a PostgreSQL standard declaration".into(),
        )),
    }
}

/// The string the scratch server is reached by for this run. An ordinary
/// string is itself; a SQL Server `(localdb)` selector is resolved to the pipe
/// its instance listens on now, once, so that every connection of the run
/// opens the same endpoint and a pipe that has gone is a failure rather than a
/// reason to look again (DEC-2271.2).
pub(super) async fn endpoint(driver: Driver, connection: &str) -> Result<String, ProduceError> {
    match driver {
        Driver::Postgres => Ok(connection.to_owned()),
        Driver::Mssql => mssql::endpoint(connection).await,
    }
}

/// The scratch session that stays open for the whole run: named by the run's
/// token from its first packet, acting as itself, and told to let the server
/// notice if its client vanishes (#2410).
pub(super) async fn connect_scratch(
    driver: Driver,
    scratch: &str,
    tokens: &Tokens,
) -> Result<Conn, ProduceError> {
    let mut admin = Conn::connect_named(driver, scratch, &format!("pbps-{}", tokens.scratch))
        .await
        .map_err(|error| ProduceError::Acquire(format!("the scratch server: {error}")))?;
    if driver == Driver::Postgres {
        postgres::reset_role(&mut admin).await?;
        postgres::keep(&mut admin).await?;
    }
    Ok(admin)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn scratch<'a>(
    driver: Driver,
    target: &mut Conn,
    admin: &mut Conn,
    tokens: &Tokens,
    planned_identity: &str,
    scratch: &'a str,
    request: &'a ScopeRequest,
    standard: &'a Standard,
    externals: Option<&'a str>,
    provenance: &'a Provenance,
) -> Result<(Provisioning, Scratch<'a>), ProduceError> {
    match (driver, standard) {
        (Driver::Postgres, Standard::Postgres(standard)) => {
            let (provisioning, scratch) = postgres::scratch(
                driver,
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
            Ok((provisioning, Scratch::Postgres(Box::new(scratch))))
        }
        (Driver::Mssql, Standard::Mssql) => {
            let (provisioning, scratch) = mssql::scratch(
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
            Ok((provisioning, Scratch::Mssql(Box::new(scratch))))
        }
        (Driver::Mssql, Standard::Postgres(_)) | (Driver::Postgres, Standard::Mssql) => {
            Err(ProduceError::Run(Error::Unusable(
                "scratch standard and engine disagree".into(),
            )))
        }
    }
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
                scratch
                    .prepare(provisioning, admin, target, authorization, schemas, &fill)
                    .await
                    .map(Prepared::Postgres)
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
                .map(Binding::Mssql),
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

    /// See `postgres::Scratch::naming`. SQL Server has no connected rehearsal
    /// yet.
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
                binding.compile(prepared, 0..len).await
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

pub(super) async fn target_identity(driver: Driver, target: &mut Conn) -> Result<String, DbError> {
    match driver {
        Driver::Postgres => postgres::target_identity(target).await,
        Driver::Mssql => pbps_mssql::resolver::scratch::cluster_identity(target).await,
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

pub(super) fn ordinary(
    driver: Driver,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    extras: &[String],
) -> Result<pbps_diff::order::DiffPlan, Error> {
    match driver {
        Driver::Postgres => postgres::ordinary(base, desired, hints, extras),
        Driver::Mssql => pbps_diff::resolver::ordinary(base, desired, &pbps_mssql::Mssql, hints)
            .map_err(|errors| {
                Error::Binding(format!("ordinary typed plan cannot be formed: {errors:?}"))
            }),
    }
}

impl Binding {
    pub(super) fn plan(
        &self,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        hints: &Hints,
        extras: &[String],
        surfaces: &[SurfaceResolution],
    ) -> Result<pbps_diff::resolver::Ordered<pbps_diff::order::Canonical<pbps_pg::Postgres>>, Error>
    {
        match self {
            Self::Postgres(binding) => binding.plan(base, desired, hints, extras, surfaces),
            Self::Mssql(_) => Err(Error::Binding(PLANNING_FROM_EVIDENCE.into())),
        }
    }
}

pub(super) fn surfaces(
    driver: Driver,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    opening: &[BindingRecord],
    compiled: &[BindingRecord],
    assessment: &Assessment,
) -> Result<Vec<SurfaceResolution>, Error> {
    match driver {
        Driver::Postgres => {
            super::resolution::from_sides(base, desired, opening, compiled, assessment)
        }
        Driver::Mssql => Err(Error::Binding(PLANNING_FROM_EVIDENCE.into())),
    }
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
        | CaptureError::Incomplete
        | CaptureError::Changed
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

/// Values the runner needs to start and reach one engine's Docker image.
/// Acquisition, deadlines and confirmed removal remain the runner's work.
pub(super) struct Docker {
    pub driver: Driver,
    pub environment: Vec<String>,
    pub port: u16,
    pub readiness_query: &'static str,
}

pub(super) fn docker(driver: Driver, password: &str) -> Result<Docker, ProduceError> {
    match driver {
        Driver::Postgres => Ok(Docker {
            driver,
            environment: vec![format!("POSTGRES_PASSWORD={password}")],
            port: 5432,
            readiness_query: "SELECT 1 AS ok",
        }),
        Driver::Mssql => Ok(Docker {
            driver,
            environment: vec![
                "ACCEPT_EULA=Y".into(),
                format!(
                    "MSSQL_SA_PASSWORD={}",
                    pbps_mssql::resolver::scratch::password(password)
                ),
            ],
            port: 1433,
            readiness_query: "SELECT 1 AS ok",
        }),
    }
}

impl Docker {
    pub(super) fn connection(&self, port: u16, password: &str) -> Result<String, ProduceError> {
        match self.driver {
            Driver::Postgres => Ok(format!(
                "host=127.0.0.1 port={port} user=postgres password={password} dbname=postgres \
                 sslmode=disable"
            )),
            Driver::Mssql => Ok(format!(
                "Server=127.0.0.1,{port};Database=master;User \
                 Id=sa;Password={};TrustServerCertificate=true",
                pbps_mssql::resolver::scratch::password(password)
            )),
        }
    }
}

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
            CaptureError::Incomplete,
            CaptureError::Changed,
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
