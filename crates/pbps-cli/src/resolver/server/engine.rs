//! Engine routing for the dedicated-server profile.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. The binding calls below hand the PostgreSQL adapter the managed
//! declarations and return its comparison; nothing here seals evidence.

use pbps_db::resolver::environment::DatabaseRecipe;
use pbps_db::resolver::{
    InstanceObservation, OwnSession, ScratchNames, SessionCounter, SessionInventory,
};
use pbps_db::transport::StreamConn;
use pbps_db::{DbError, Driver};

use super::Error;

pub(crate) async fn identity(connection: &mut StreamConn) -> Result<InstanceObservation, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::instance_identity(connection).await,
        Driver::Mssql => pbps_mssql::resolver::instance_identity(connection).await,
    }
}

/// Read only on an already qualified PostgreSQL administrative channel.
pub(crate) async fn postgres_version_num(connection: &mut StreamConn) -> Result<i64, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::roles::server_version_num(connection).await,
        Driver::Mssql => Err(DbError::Refused(
            "a PostgreSQL supplied-server profile cannot read a SQL Server version".into(),
        )),
    }
}

pub(crate) async fn own_session(connection: &mut StreamConn) -> Result<OwnSession, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::own_session(connection).await,
        Driver::Mssql => pbps_mssql::resolver::own_session(connection).await,
    }
}

pub(crate) async fn client_sessions(
    connection: &mut StreamConn,
) -> Result<SessionInventory, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::client_sessions(connection).await,
        Driver::Mssql => pbps_mssql::resolver::client_sessions(connection).await,
    }
}

pub(crate) async fn session_counter(
    connection: &mut StreamConn,
) -> Result<SessionCounter, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::session_counter(connection).await,
        Driver::Mssql => pbps_mssql::resolver::session_counter(connection).await,
    }
}

pub(crate) async fn create_scratch(
    connection: &mut StreamConn,
    names: &ScratchNames,
    recipe: &DatabaseRecipe,
) -> Result<(), DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::create_scratch(connection, names, recipe).await,
        Driver::Mssql => pbps_mssql::resolver::create_scratch(connection, names, recipe).await,
    }
}

pub(crate) async fn drop_roles(connection: &mut StreamConn, roles: &[String]) -> Vec<String> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::drop_roles(connection, roles).await,
        Driver::Mssql => pbps_mssql::resolver::drop_roles(connection, roles).await,
    }
}

pub(crate) async fn drop_scratch(
    connection: &mut StreamConn,
    names: &ScratchNames,
) -> Result<(), DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::drop_scratch(connection, names).await,
        Driver::Mssql => pbps_mssql::resolver::drop_scratch(connection, names).await,
    }
}

// Binding resolution has one adapter, PostgreSQL's (#613); SQL Server's is
// designed and built separately (#619, #620). The reconstruction, the
// managed-name inventory and both captures are that adapter's private state,
// as the target capture's are.
pub(super) use pbps_pg::resolver::capture::{
    CaptureScope, CapturedInputs, CompiledCapture, Managed, Paths, RecordedOwnership,
};
pub(super) use pbps_pg::resolver::reconstruct::Reconstruction;

const NO_BINDING_ADAPTER: &str =
    "SQL Server has no binding adapter yet; its design and implementation are #619 and #620";

/// Each routine the plan drops, with its declared signature spelled as the
/// plan's `DROP` spells it, for the target capture to identify (#1148).
pub(super) fn dropped_signatures(
    extras: &[String],
    routines: Vec<(pbps_model::ModuleId, pbps_model::ModuleKind)>,
) -> Result<
    Vec<(
        pbps_model::ModuleId,
        Option<pbps_pg::resolver::capture::DroppedSignature>,
    )>,
    String,
> {
    let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
    routines
        .into_iter()
        .map(|(id, kind)| {
            pbps_pg::resolver::reconstruct::dropped_signature(&dialect, &id, kind)
                .map(|signature| (id, signature))
                .map_err(|error| error.to_string())
        })
        .collect()
}

/// The desired namespace's scratch statements, for this engine.
pub(super) fn reconstruction(
    driver: Driver,
    extras: &[String],
    bootstrap: &[pbps_model::Change],
) -> Result<Reconstruction, String> {
    match driver {
        Driver::Postgres => Reconstruction::new(
            &pbps_pg::Postgres::with_write_path_extras(extras.to_vec()),
            bootstrap,
        )
        .map_err(|error| error.to_string()),
        Driver::Mssql => Err(NO_BINDING_ADAPTER.into()),
    }
}

/// Compiles the reconstruction as whatever role the session is.
pub(crate) async fn compile(
    reconstruction: &mut Reconstruction,
    extras: &[String],
    connection: &mut StreamConn,
) -> Result<(), Error> {
    match connection.driver() {
        Driver::Postgres => reconstruction
            .compile(
                &pbps_pg::Postgres::with_write_path_extras(extras.to_vec()),
                connection,
            )
            .await
            .map_err(compile_failed),
        Driver::Mssql => Err(Error::Binding(NO_BINDING_ADAPTER.into())),
    }
}

/// What a failed scratch compilation is. A declaration that did not compile
/// is the binding's verdict; a catalog read around it, a transaction that
/// would not open or commit, or a statement the connection or server
/// interrupted, answered nothing (SPEC 9.8, #1575, #1597).
pub(super) fn compile_failed(error: pbps_pg::resolver::reconstruct::ReconstructError) -> Error {
    use pbps_pg::resolver::reconstruct::ReconstructError;
    match error {
        ReconstructError::Read { .. }
        | ReconstructError::Interrupted { .. }
        | ReconstructError::Transaction(_) => Error::Read(error.to_string()),
        ReconstructError::Unsupported(_)
        | ReconstructError::Emit { .. }
        | ReconstructError::Compile { .. }
        | ReconstructError::Cycle(_) => Error::Binding(error.to_string()),
    }
}

/// Captures scratch twice: its managed objects, and then the scope their
/// bindings derive, which is the scope the target is captured under too.
pub(crate) async fn capture_desired(
    connection: &mut StreamConn,
    base: &Managed,
    desired: &Managed,
    paths: &Paths,
) -> Result<(CapturedInputs, CaptureScope), Error> {
    use pbps_pg::resolver::capture;
    match connection.driver() {
        Driver::Postgres => {
            let first = capture::capture(connection, &capture::managed_scope(desired))
                .await
                .map_err(catalog_read_failed)?;
            let scope = capture::scope(&first, &[base, desired], paths);
            let captured = capture::capture(connection, &scope)
                .await
                .map_err(catalog_read_failed)?;
            Ok((captured, scope))
        }
        Driver::Mssql => Err(Error::Binding(NO_BINDING_ADAPTER.into())),
    }
}

/// The qualified run fixes the key and principal map before either fresh
/// scratch capture. Only the second capture's facts support the verdict.
pub(crate) async fn capture_desired_sealed(
    connection: &mut StreamConn,
    base: &Managed,
    desired: &Managed,
    paths: &Paths,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    principals: &crate::resolver::scope::Principals,
) -> Result<
    (
        CapturedInputs,
        CaptureScope,
        pbps_model::resolver::InputManifest,
    ),
    Error,
> {
    use pbps_pg::resolver::capture;
    match (connection.driver(), principals) {
        (Driver::Postgres, crate::resolver::scope::Principals::Postgres(roles)) => {
            let first = capture::capture(connection, &capture::managed_scope(desired))
                .await
                .map_err(catalog_read_failed)?;
            let scope = capture::scope(&first, &[base, desired], paths);
            let (captured, manifest) = capture::capture_identifying_sealed_with_roles(
                connection,
                &scope,
                &Default::default(),
                key,
                roles,
            )
            .await
            .map_err(catalog_read_failed)?;
            Ok((captured, scope, manifest))
        }
        _ => Err(Error::Binding(NO_BINDING_ADAPTER.into())),
    }
}

/// Retain the final scratch read with its key and UID-qualified roots until
/// final typed planning decides the one closing seal. Only logical binding
/// records and the existing verdict leave this private capability.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn capture_desired_for_plan(
    connection: &mut StreamConn,
    base: &Managed,
    desired: &Managed,
    paths: &Paths,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    principals: &crate::resolver::scope::Principals,
    recorded: pbps_diff::Side<'_>,
    reconstruction: &Reconstruction,
    namespaces: &std::collections::BTreeSet<String>,
    dropped_signatures: &std::collections::BTreeSet<pbps_pg::resolver::capture::DroppedSignature>,
) -> Result<(CompiledCapture, CaptureScope), Error> {
    use pbps_pg::resolver::capture;
    match (connection.driver(), principals) {
        (Driver::Postgres, crate::resolver::scope::Principals::Postgres(roles)) => {
            let first = capture::capture(connection, &capture::managed_scope(desired))
                .await
                .map_err(catalog_read_failed)?;
            let scope = capture::scope(&first, &[base, desired], paths);
            let routines = recorded
                .schema
                .modules
                .keys()
                .filter_map(|id| {
                    reconstruction
                        .created(id)
                        .map(|object| (id.clone(), object.clone()))
                })
                .collect();
            let dropped = Default::default();
            let ownership = capture::RecordedOwnership {
                schema: recorded.schema,
                ids: recorded.ids,
                routines: &routines,
                dropped: &dropped,
                namespaces,
            };
            // The same lookups the opening sealed: the closing manifest records
            // what each dropped signature names after the plan, normally none.
            let captured = capture::capture_identifying_for_plan(
                connection,
                &scope,
                dropped_signatures,
                key,
                roles,
                &ownership,
            )
            .await
            .map_err(catalog_read_failed)?;
            Ok((captured, scope))
        }
        _ => Err(Error::Binding(NO_BINDING_ADAPTER.into())),
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
        | CaptureError::Read
        | CaptureError::Incomplete
        | CaptureError::Changed
        | CaptureError::EnvironmentChanged
        | CaptureError::Close => Error::Read(error.to_string()),
    }
}

/// What a failed target capture is, by the same rule as
/// [`catalog_read_failed`]. A target without its qualified connection, or one
/// whose inputs changed under the read, answered nothing either; executable
/// content that is not qualified did.
pub(super) fn target_capture_failed(failure: crate::resolver::native::CaptureFailure) -> Error {
    use crate::resolver::native::CaptureFailure;
    match failure {
        CaptureFailure::Catalog(error) => catalog_read_failed(error),
        CaptureFailure::Binding | CaptureFailure::Changed => Error::Read(failure.to_string()),
        CaptureFailure::Executables => Error::Binding(failure.to_string()),
    }
}

pub(super) fn assess(
    target: &CapturedInputs,
    desired: &CapturedInputs,
    base: &Managed,
    paths: &Paths,
    reconstruction: &Reconstruction,
) -> pbps_db::resolver::capture::Assessment {
    pbps_pg::resolver::capture::assess(target, desired, base, paths, reconstruction)
}

/// Each in-scope schema's effective path as the qualified scope measured it,
/// with the configured extras for any schema it did not. An unreadable
/// measurement is left out, which falls back to the whole write path.
pub(super) fn paths(
    extras: &[String],
    visibility: &std::collections::BTreeMap<String, pbps_db::resolver::Observation>,
) -> Paths {
    let effective = visibility
        .iter()
        .filter_map(|(schema, observed)| {
            let path: Vec<String> = serde_json::from_str(observed.value()?).ok()?;
            Some((schema.clone(), path))
        })
        .collect();
    Paths::new(extras.to_vec(), effective)
}

#[cfg(test)]
mod tests {
    use super::{Error, catalog_read_failed, compile_failed, target_capture_failed};
    use crate::resolver::native::CaptureFailure;
    use pbps_db::resolver::capture::{CaptureError, Uncovered};
    use pbps_pg::resolver::reconstruct::ReconstructError;

    /// #1575: a capture that failed to read answered nothing about the
    /// bindings, and is not reported as their verdict (SPEC 9.8).
    #[test]
    fn a_failed_catalog_read_is_a_read_failure_and_an_uncovered_input_a_verdict() {
        for read in [
            CaptureError::CallerTransaction,
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

    #[test]
    fn a_target_capture_that_lost_its_connection_or_input_is_a_read_failure() {
        for read in [
            CaptureFailure::Binding,
            CaptureFailure::Changed,
            CaptureFailure::Catalog(CaptureError::Close),
        ] {
            assert!(matches!(target_capture_failed(read), Error::Read(_)));
        }
        for verdict in [
            CaptureFailure::Executables,
            CaptureFailure::Catalog(CaptureError::Version),
        ] {
            assert!(matches!(target_capture_failed(verdict), Error::Binding(_)));
        }
    }
}
