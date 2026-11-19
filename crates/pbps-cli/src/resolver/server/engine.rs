//! Engine routing for the scratch run. The CLI keeps the lifecycle; each
//! adapter keeps the backend checks, reconstruction and private captures its
//! engine needs. Only logical records and verdicts leave those captures.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. The binding calls below hand the PostgreSQL adapter the managed
//! declarations and return its comparison; nothing here seals evidence.

use super::{Error, ProduceError};
use crate::resolver::contract::ResolverEngine;
use pbps_db::Driver;
use pbps_model::Hints;
use pbps_model::resolver::SurfaceResolution;

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
    fn configured(extras: &[String], version: Option<i64>) -> Self;
}
impl PlanningDialect for pbps_pg::Postgres {
    type Resolver = postgres::PgResolver;
    const DRIVER: Driver = Driver::Postgres;
    const NAME: &'static str = "PostgreSQL";
    fn configured(extras: &[String], version: Option<i64>) -> Self {
        let dialect = Self::with_write_path_extras(extras.to_vec());
        version.map_or(dialect.clone(), |v| dialect.with_server_version(v))
    }
}
impl PlanningDialect for pbps_mssql::Mssql {
    type Resolver = mssql::MssqlResolver;
    const DRIVER: Driver = Driver::Mssql;
    const NAME: &'static str = "SQL Server";
    fn configured(_: &[String], _: Option<i64>) -> Self {
        Self
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

/// The run-local history of the target beside its reported scope, for the
/// engine the dialect names. The history of one engine cannot be compared with
/// another's:
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::History;
/// fn for_sql_server(_: History<pbps_mssql::Mssql>) {}
/// fn read_by_postgres(history: History<pbps_pg::Postgres>) {
///     for_sql_server(history);
/// }
/// ```
///
/// ```
/// use pbps_cli::resolver::server::scratch::History;
/// fn for_postgres(history: History<pbps_pg::Postgres>) -> History<pbps_pg::Postgres> {
///     history
/// }
/// fn for_sql_server(history: History<pbps_mssql::Mssql>) -> History<pbps_mssql::Mssql> {
///     history
/// }
/// ```
pub type History<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::History;

/// What the run acquired and must release, for the engine the dialect names.
/// What the scratch's compile left to seal, for the engine the dialect names.
/// It is the type alone: nothing outside this crate can make, read or seal one,
/// and the capture of one engine cannot be handed to another's step:
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::CompiledCapture;
/// fn for_sql_server(_: CompiledCapture<pbps_mssql::Mssql>) {}
/// fn compiled_by_postgres(compiled: CompiledCapture<pbps_pg::Postgres>) {
///     for_sql_server(compiled);
/// }
/// ```
///
/// Each engine's own capture is the type its adapter holds:
///
/// ```
/// use pbps_cli::resolver::server::scratch::CompiledCapture;
/// fn for_postgres(compiled: CompiledCapture<pbps_pg::Postgres>) -> CompiledCapture<pbps_pg::Postgres> {
///     compiled
/// }
/// fn for_sql_server(compiled: CompiledCapture<pbps_mssql::Mssql>) -> CompiledCapture<pbps_mssql::Mssql> {
///     compiled
/// }
/// ```
pub type CompiledCapture<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::CompiledCapture;

/// The opening capture the recheck captures again, for the engine the dialect
/// names. The opening capture of one engine cannot be rechecked by another:
///
/// ```compile_fail
/// use pbps_cli::resolver::server::scratch::CapturedInputs;
/// fn for_sql_server(_: CapturedInputs<pbps_mssql::Mssql>) {}
/// fn captured_by_postgres(current: CapturedInputs<pbps_pg::Postgres>) {
///     for_sql_server(current);
/// }
/// ```
///
/// ```
/// use pbps_cli::resolver::server::scratch::CapturedInputs;
/// fn for_postgres(current: CapturedInputs<pbps_pg::Postgres>) -> CapturedInputs<pbps_pg::Postgres> {
///     current
/// }
/// fn for_sql_server(current: CapturedInputs<pbps_mssql::Mssql>) -> CapturedInputs<pbps_mssql::Mssql> {
///     current
/// }
/// ```
pub type CapturedInputs<D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::CapturedInputs;

pub(super) type Scratch<'a, D> = <<D as PlanningDialect>::Resolver as ResolverEngine>::Scratch<'a>;

/// The standard an entry declares, or the refusal of one the engine cannot
/// hold.
pub fn declared_standard<D: PlanningDialect>(
    declared: Option<&pbps_config::resolver::ScratchStandard>,
) -> Result<Standard<D>, Error> {
    <D::Resolver as ResolverEngine>::standard(declared)
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
