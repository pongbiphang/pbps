//! Engine routing for the dedicated-server profile.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. The binding calls below hand the PostgreSQL adapter the managed
//! declarations and return its comparison; nothing here seals evidence.

use pbps_db::Driver;

use super::Error;

// Binding resolution has one adapter, PostgreSQL's (#613); SQL Server's is
// designed and built separately (#619, #620). The reconstruction, the
// managed-name inventory and both captures are that adapter's private state,
// as the target capture's are.
pub(super) use pbps_pg::resolver::capture::{
    CaptureScope, CompiledCapture, Managed, Paths, RecordedOwnership,
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

/// Compiles `part` of the reconstruction as whatever role the session is,
/// over the run's ordinary connection.
pub(crate) async fn compile(
    reconstruction: &mut Reconstruction,
    extras: &[String],
    driver: Driver,
    zone: Option<&pbps_model::SessionZone>,
    connection: &mut impl pbps_db::transport::ExecuteConnection,
    part: pbps_pg::resolver::reconstruct::Part,
) -> Result<(), Error> {
    match (driver, zone) {
        (Driver::Postgres, None) => Err(Error::Scope(
            "the target reported no session time zone to compile in".into(),
        )),
        (Driver::Postgres, Some(zone)) => reconstruction
            .compile_part(
                &pbps_pg::Postgres::with_write_path_extras(extras.to_vec()),
                zone,
                connection,
                part,
            )
            .await
            .map_err(compile_failed),
        (Driver::Mssql, _) => Err(Error::Binding(NO_BINDING_ADAPTER.into())),
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
        ReconstructError::Zone { .. } => Error::Scope(error.to_string()),
        ReconstructError::Unsupported(_)
        | ReconstructError::Emit { .. }
        | ReconstructError::Compile { .. }
        | ReconstructError::Cycle(_) => Error::Binding(error.to_string()),
    }
}

/// Retain the final scratch read with its key and UID-qualified roots until
/// final typed planning decides the one closing seal. Only logical binding
/// records and the existing verdict leave this private capability.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn capture_desired_for_plan(
    connection: &mut impl pbps_db::transport::QueryConnection,
    base: &Managed,
    desired: &Managed,
    paths: &Paths,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    principals: &crate::resolver::scope::Principals,
    recorded: pbps_diff::Side<'_>,
    reconstruction: &Reconstruction,
    namespaces: &std::collections::BTreeSet<String>,
    dropped_signatures: &std::collections::BTreeSet<pbps_pg::resolver::capture::DroppedSignature>,
    staged: &std::collections::BTreeSet<pbps_db::resolver::capture::ObjectIdentity>,
) -> Result<(CompiledCapture, CaptureScope), Error> {
    use pbps_pg::resolver::capture;
    // The principal map names the engine: only PostgreSQL's has an adapter.
    match principals {
        crate::resolver::scope::Principals::Postgres(roles) => {
            let first = capture::capture(connection, &capture::managed_scope(desired))
                .await
                .map_err(catalog_read_failed)?;
            // What an externals file staged is read and sealed on both sides, so
            // the recheck compares the target's complete fingerprints of it
            // (#1673).
            let mut scope = capture::scope(&first, &[base, desired], paths);
            scope.retained.extend(staged.iter().cloned());
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
        | CaptureError::NoTransaction
        | CaptureError::Read
        | CaptureError::Incomplete
        | CaptureError::Changed
        | CaptureError::EnvironmentChanged
        | CaptureError::Close => Error::Read(error.to_string()),
    }
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
