//! Engine routing for the scratch run. The CLI keeps the lifecycle; each
//! adapter keeps the backend checks, reconstruction and private captures its
//! engine needs. Only logical records and verdicts leave those captures.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. The binding calls below hand the PostgreSQL adapter the managed
//! declarations and return its comparison; nothing here seals evidence.

use super::{Error, ProduceError};
use crate::resolver::contract::ResolverEngine;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_model::Hints;
use pbps_model::resolver::{InputManifest, SurfaceResolution};

pub(crate) mod container;
mod keeper;
pub(super) mod mssql;
mod postgres;
#[cfg(test)]
pub(super) use keeper::pause_between_reads;
pub(super) use keeper::{keep_and_mark, keeper_seen};
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

pub(super) fn unsupported() -> Error {
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

/// What the run prepared on its scratch, for the engine the dialect names. A
/// scratch prepared by one engine cannot be handed to another's step: it is a
/// type error, not an arm that refuses at run time.
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::Prepared;
/// fn for_sql_server(_: Prepared<pbps_mssql::Mssql>) {}
/// fn prepared_by_postgres(prepared: Prepared<pbps_pg::Postgres>) {
///     for_sql_server(prepared);
/// }
/// ```
///
/// Each engine's own prepared scratch is the type its adapter holds:
///
/// ```
/// use pbps_cli::resolver::server::scratch::Prepared;
/// fn for_postgres(prepared: Prepared<pbps_pg::Postgres>) -> Prepared<pbps_pg::Postgres> {
///     prepared
/// }
/// fn for_sql_server(prepared: Prepared<pbps_mssql::Mssql>) -> Prepared<pbps_mssql::Mssql> {
///     prepared
/// }
/// ```
pub type Prepared<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::Prepared;

/// What the run reconstructed the declarations as, for the engine the dialect
/// names. A binding of one engine cannot be handed to another's step:
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::Binding;
/// fn for_sql_server(_: Binding<pbps_mssql::Mssql>) {}
/// fn bound_by_postgres(binding: Binding<pbps_pg::Postgres>) {
///     for_sql_server(binding);
/// }
/// ```
///
/// Each engine's own binding is the type its adapter holds:
///
/// ```
/// use pbps_cli::resolver::server::scratch::Binding;
/// fn for_postgres(binding: Binding<pbps_pg::Postgres>) -> Binding<pbps_pg::Postgres> {
///     binding
/// }
/// fn for_sql_server(binding: Binding<pbps_mssql::Mssql>) -> Binding<pbps_mssql::Mssql> {
///     binding
/// }
/// ```
pub type Binding<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::Binding;

/// The shapes the externals file was compared with, for the engine the
/// dialect names. A staged set of another engine is a type error too:
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::Staged;
/// fn for_sql_server(_: Staged<pbps_mssql::Mssql>) {}
/// fn staged_by_postgres(staged: Staged<pbps_pg::Postgres>) {
///     for_sql_server(staged);
/// }
/// ```
///
/// ```
/// use pbps_cli::resolver::server::scratch::Staged;
/// fn for_postgres(staged: Staged<pbps_pg::Postgres>) -> Staged<pbps_pg::Postgres> {
///     staged
/// }
/// fn for_sql_server(staged: Staged<pbps_mssql::Mssql>) -> Staged<pbps_mssql::Mssql> {
///     staged
/// }
/// ```
pub type Staged<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::Staged;

/// What the run acquired and must release, for the engine the dialect names.
pub(super) type Scratch<'a, D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::Scratch<'a>;

/// The standard an entry declares, or the refusal of one the engine cannot
/// hold.
pub fn declared_standard<D: PlanningDialect>(
    declared: Option<&pbps_config::resolver::ScratchStandard>,
) -> Result<Standard<D>, Error> {
    <D::Resolver as ResolverEngine>::standard(declared)
}

pub enum CompiledCapture {
    Postgres(Box<postgres::Closing>),
    Mssql(Box<mssql::Closing>),
}
pub enum CapturedInputs {
    Postgres(pbps_pg::resolver::capture::CapturedInputs),
    Mssql(pbps_mssql::resolver::capture::CapturedInputs),
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

/// The ordered plan under the dialect the binding was generated under.
pub(super) fn plan<D: PlanningDialect>(
    binding: &<D::Resolver as ResolverEngine>::Binding,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    extras: &[String],
    surfaces: &[SurfaceResolution],
    captured: &pbps_model::operation::Captured,
) -> Result<pbps_diff::resolver::Ordered<pbps_diff::order::Canonical<D>>, Error> {
    let dialect = <D::Resolver as ResolverEngine>::dialect(binding, extras);
    pbps_diff::resolver::plan_captured(base, desired, hints, surfaces, &dialect, captured)
        .map_err(|error| Error::Binding(error.to_string()))
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
