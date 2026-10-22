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

mod postgres;

// These are logical planning records, not a raw engine capture or a verifier.
pub(super) use pbps_pg::resolver::capture::BindingRecord;

const NO_BINDING_ADAPTER: &str =
    "SQL Server has no binding adapter yet; its design and implementation are #619 and #620";

fn unsupported() -> Error {
    Error::Binding("the resolver is implemented for PostgreSQL only".into())
}

/// The standard is engine-owned; the PostgreSQL conversion keeps callers of
/// the supplied-server API from having to construct adapter state themselves.
#[derive(Clone)]
pub enum Standard {
    Postgres(pbps_pg::resolver::standard::Standard),
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
    Postgres(postgres::Scratch<'a>),
}
pub(super) enum Fill {
    Postgres(pbps_pg::resolver::draft::Fill),
}
pub(super) enum Prepared {
    Postgres(postgres::Prepared),
}
pub(super) enum Binding {
    Postgres(postgres::Binding),
}
pub(super) enum Chained {
    Postgres(pbps_pg::resolver::externals::Created),
}
pub(super) enum Staged {
    Postgres(pbps_pg::resolver::capture::Staged),
}
pub(super) enum CompiledCapture {
    Postgres(postgres::Closing),
}
pub(super) enum CapturedInputs {
    Postgres(pbps_pg::resolver::capture::CapturedInputs),
}

pub fn declared_standard(
    driver: Driver,
    declared: Option<&pbps_config::resolver::ScratchStandard>,
) -> Result<Standard, Error> {
    match driver {
        Driver::Postgres => Ok(Standard::Postgres(postgres::declared_standard(declared))),
        Driver::Mssql => Err(unsupported()),
    }
}

pub(super) async fn reset_role(driver: Driver, admin: &mut Conn) -> Result<(), ProduceError> {
    match driver {
        Driver::Postgres => postgres::reset_role(admin).await,
        Driver::Mssql => Err(ProduceError::Run(unsupported())),
    }
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
            Ok((provisioning, Scratch::Postgres(scratch)))
        }
        (Driver::Mssql, Standard::Postgres(_)) => Err(ProduceError::Run(unsupported())),
    }
}

pub(super) async fn fill(
    driver: Driver,
    target: &mut Conn,
    binding: &BindingRequest<'_>,
    extras: &[String],
) -> Result<Fill, Error> {
    match driver {
        Driver::Postgres => postgres::fill(target, binding, extras)
            .await
            .map(Fill::Postgres),
        Driver::Mssql => Err(unsupported()),
    }
}

impl Scratch<'_> {
    pub(super) async fn prepare(
        &mut self,
        provisioning: Provisioning,
        admin: &mut Conn,
        target: &CatalogFacts,
        authorization: &scope::Authorization,
        schemas: &[String],
        fill: &Fill,
    ) -> Result<Prepared, Error> {
        match (self, fill) {
            (Self::Postgres(scratch), Fill::Postgres(fill)) => scratch
                .prepare(provisioning, admin, target, authorization, schemas, fill)
                .await
                .map(Prepared::Postgres),
        }
    }

    pub(super) async fn reconstruction(
        &self,
        target: &mut Conn,
        binding: &BindingRequest<'_>,
        ids: &pbps_model::IdsFile,
        dependencies: &pbps_model::ModuleDeps,
    ) -> Result<Binding, Error> {
        match self {
            Self::Postgres(scratch) => scratch
                .reconstruction(target, binding, ids, dependencies)
                .await
                .map(Binding::Postgres),
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
            (Self::Postgres(scratch), Prepared::Postgres(prepared), Binding::Postgres(binding)) => {
                scratch
                    .compare_externals(provisioning, prepared, admin, target, binding)
                    .await
                    .map(Staged::Postgres)
            }
        }
    }

    pub(super) async fn restore_settings(
        &self,
        admin: &mut Conn,
        authorization: &scope::Authorization,
    ) -> Result<(), Error> {
        match self {
            Self::Postgres(scratch) => scratch.restore_settings(admin, authorization).await,
        }
    }

    pub(super) async fn release(&mut self, admin: &mut Conn) -> Result<(), Vec<String>> {
        match self {
            Self::Postgres(scratch) => scratch.release(admin).await,
        }
    }
}

impl Prepared {
    pub(super) fn catalog(&self) -> &CatalogFacts {
        match self {
            Self::Postgres(prepared) => &prepared.scratch_catalog,
        }
    }
    #[cfg(test)]
    pub(super) fn catalog_mut(&mut self) -> &mut CatalogFacts {
        match self {
            Self::Postgres(prepared) => &mut prepared.scratch_catalog,
        }
    }
    pub(super) fn into_catalog(self) -> CatalogFacts {
        match self {
            Self::Postgres(prepared) => prepared.scratch_catalog,
        }
    }
    pub(super) fn differences(&self) -> &[String] {
        match self {
            Self::Postgres(prepared) => &prepared.differences,
        }
    }
    pub(super) fn supplied(&self) -> bool {
        match self {
            Self::Postgres(prepared) => prepared.compile.is_none() && prepared.capture.is_none(),
        }
    }
    pub(super) fn split(&mut self, binding: &Binding) -> Option<usize> {
        match (self, binding) {
            (Self::Postgres(prepared), Binding::Postgres(binding)) => prepared.split(binding),
        }
    }
    pub(super) fn has_filled(&self) -> bool {
        match self {
            Self::Postgres(prepared) => prepared.has_filled(),
        }
    }
    pub(super) fn unfillable(&self) -> Vec<String> {
        match self {
            Self::Postgres(prepared) => prepared.unfillable(),
        }
    }
    pub(super) fn waiting(&self) -> String {
        match self {
            Self::Postgres(prepared) => prepared.waiting(),
        }
    }
}

impl Binding {
    pub(super) fn chains(&mut self, prepared: &Prepared) -> Vec<String> {
        match (self, prepared) {
            (Self::Postgres(binding), Prepared::Postgres(prepared)) => binding.chains(prepared),
        }
    }
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Postgres(binding) => binding.len(),
        }
    }
    pub(super) async fn compile(
        &mut self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        extras: &[String],
        driver: Driver,
        zone: Option<&pbps_model::SessionZone>,
        steps: std::ops::Range<usize>,
    ) -> Result<(), Error> {
        match (self, prepared) {
            (Self::Postgres(binding), Prepared::Postgres(prepared)) => {
                let compile = match prepared.compile.as_mut() {
                    Some(compile) => compile,
                    None => &mut *admin,
                };
                binding.compile(extras, driver, zone, compile, steps).await
            }
        }
    }
    pub(super) async fn fill_chained(
        &mut self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        split: usize,
        outside: OutsideWrites,
    ) -> Result<Chained, Error> {
        match (self, prepared) {
            (Self::Postgres(binding), Prepared::Postgres(prepared)) => binding
                .fill_chained(prepared, admin, split, outside)
                .await
                .map(Chained::Postgres),
        }
    }
    pub(super) async fn compare_chained(
        &self,
        prepared: &mut Prepared,
        admin: &mut Conn,
        target: &mut Conn,
        chained: &Chained,
    ) -> Result<Staged, Error> {
        match (self, prepared, chained) {
            (Self::Postgres(binding), Prepared::Postgres(prepared), Chained::Postgres(chained)) => {
                binding
                    .compare_chained(prepared, admin, target, chained)
                    .await
                    .map(Staged::Postgres)
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn capture_desired(
        &self,
        prepared: &mut Prepared,
        admin: &mut Conn,
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
                    .map(CompiledCapture::Postgres)
            }
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
        }
    }
    pub(super) fn assess(
        &mut self,
        closing: &CompiledCapture,
        current: &CapturedInputs,
        staged: &Staged,
    ) -> Assessment {
        match (self, closing, current, staged) {
            (
                Self::Postgres(binding),
                CompiledCapture::Postgres(closing),
                CapturedInputs::Postgres(current),
                Staged::Postgres(staged),
            ) => binding.assess(closing, current, staged),
        }
    }
}

impl Staged {
    pub(super) fn extend(&mut self, other: Self) {
        match (self, other) {
            (Self::Postgres(staged), Self::Postgres(other)) => staged.extend(other),
        }
    }
    pub(super) fn changed_since(&self, current: &CapturedInputs) -> Vec<String> {
        match (self, current) {
            (Self::Postgres(staged), CapturedInputs::Postgres(current)) => {
                staged.changed_since(current)
            }
        }
    }
    pub(super) fn through_shape_view(&self, error: Error) -> Error {
        match self {
            Self::Postgres(staged) => postgres::through_shape_view(error, staged),
        }
    }
}

pub(super) fn staged(driver: Driver) -> Result<Staged, Error> {
    match driver {
        Driver::Postgres => Ok(Staged::Postgres(Default::default())),
        Driver::Mssql => Err(unsupported()),
    }
}

impl CompiledCapture {
    pub(super) fn planning_records(
        &self,
    ) -> Result<Vec<BindingRecord>, pbps_model::resolver::ManifestError> {
        match self {
            Self::Postgres(closing) => closing.planning_records(),
        }
    }
    pub(super) fn seal_for_plan(
        self,
    ) -> Result<InputManifest, pbps_pg::resolver::capture::SealError> {
        match self {
            Self::Postgres(closing) => closing.seal_for_plan(),
        }
    }
}

pub(super) async fn target_identity(driver: Driver, target: &mut Conn) -> Result<String, DbError> {
    match driver {
        Driver::Postgres => postgres::target_identity(target).await,
        Driver::Mssql => Err(DbError::BadRow(NO_BINDING_ADAPTER.into())),
    }
}

pub(super) async fn recapture(
    target: &mut Conn,
    current: &CapturedInputs,
) -> Result<Vec<CaptureDifference>, CaptureError> {
    match current {
        CapturedInputs::Postgres(current) => postgres::recapture(target, current).await,
    }
}

pub(super) fn ordinary(
    driver: Driver,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    extras: &[String],
) -> Result<pbps_diff::order::PlanningPlan, Error> {
    match driver {
        Driver::Postgres => postgres::ordinary(base, desired, hints, extras),
        Driver::Mssql => Err(unsupported()),
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
    ) -> Result<pbps_diff::resolver::Ordered, Error> {
        match self {
            Self::Postgres(binding) => binding.plan(base, desired, hints, extras, surfaces),
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
        Driver::Mssql => Err(Error::Binding(NO_BINDING_ADAPTER.into())),
    }
}

pub(super) fn reported_build(
    driver: Driver,
    key: &EnvironmentFingerprintKey,
    catalog: &CatalogFacts,
) -> Result<String, Error> {
    match driver {
        Driver::Postgres => postgres::reported_build(key, catalog),
        Driver::Mssql => Err(Error::Scope("the resolver has no SQL Server rule".into())),
    }
}

pub(super) fn reported_rule(driver: Driver) -> Result<&'static str, Error> {
    match driver {
        Driver::Postgres => Ok(pbps_pg::resolver::compatibility::REPORTED_RULE),
        Driver::Mssql => Err(Error::Scope("the resolver has no SQL Server rule".into())),
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
pub(super) use postgres::{generation_version, reconstruction, release_supplied};

/// Values the runner needs to start and reach one engine's Docker image.
/// Acquisition, deadlines and confirmed removal remain the runner's work.
pub(super) struct Docker {
    driver: Driver,
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
        Driver::Mssql => Err(ProduceError::Run(unsupported())),
    }
}

impl Docker {
    pub(super) fn connection(&self, port: u16, password: &str) -> Result<String, ProduceError> {
        match self.driver {
            Driver::Postgres => Ok(format!(
                "host=127.0.0.1 port={port} user=postgres password={password} dbname=postgres \
                 sslmode=disable"
            )),
            Driver::Mssql => Err(ProduceError::Run(unsupported())),
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
