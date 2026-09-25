//! Engine routing for the native target's read-only identity and capture. The
//! analysis-scope reads route through `resolver::scope`.

use pbps_db::resolver::{InstanceObservation, capture::CaptureError};
use pbps_db::transport::PeerVerifiedConn;
use pbps_db::{DbError, Driver};

pub(super) fn native_executable(driver: Driver) -> &'static str {
    match driver {
        Driver::Postgres => "postgres",
        Driver::Mssql => "sqlservr",
    }
}

pub(super) async fn identity(
    connection: &mut PeerVerifiedConn,
) -> Result<InstanceObservation, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::instance_identity(connection).await,
        Driver::Mssql => pbps_mssql::resolver::instance_identity(connection).await,
    }
}

// These are opaque PostgreSQL loader operations, not a catalog report. SQL
// dispatch remains separate; no private path crosses the engine boundary.
pub(in crate::resolver::native) use pbps_pg::resolver::capture::{
    NativeLibrary, NativeLibraryReader, RuntimeInputs, RuntimeResolution, native_library_candidates,
};

// The request and opaque catalog are PostgreSQL-specific private state, like
// the engine's held-lock state (DECISIONS 417). Source-free refusals and
// comparison reports use pbps-db's shared answer types. No catalog can mint
// the separate native source capability (DEC-974.1).
pub(super) use pbps_pg::resolver::capture::{CaptureScope, CapturedInputs};

pub(super) async fn capture_with_runtime_inputs(
    connection: &mut PeerVerifiedConn,
    scope: &CaptureScope,
) -> Result<(CapturedInputs, RuntimeInputs), CaptureError> {
    match connection.driver() {
        Driver::Postgres => {
            pbps_pg::resolver::capture::capture_with_runtime_inputs(connection, scope).await
        }
        Driver::Mssql => Err(CaptureError::Unsupported {
            engine: "SQL Server",
        }),
    }
}

pub(super) async fn capture(
    connection: &mut PeerVerifiedConn,
    scope: &CaptureScope,
) -> Result<CapturedInputs, CaptureError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::capture::capture(connection, scope).await,
        Driver::Mssql => Err(CaptureError::Unsupported {
            engine: "SQL Server",
        }),
    }
}
