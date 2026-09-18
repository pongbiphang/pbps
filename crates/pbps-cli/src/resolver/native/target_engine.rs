//! Engine routing for the native target's read-only identity check.

use pbps_db::resolver::InstanceObservation;
use pbps_db::resolver::environment::CatalogFacts;
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

/// Reads the analysis-scope catalog facts for the write paths in `schemas`,
/// with `extras` appended after each (SPEC §7.3). PostgreSQL is #610; SQL
/// Server is the twin step #611 and refuses by name until it lands, never
/// silently returning an empty scope.
pub(super) async fn environment(
    connection: &mut PeerVerifiedConn,
    schemas: &[String],
    extras: &[String],
) -> Result<CatalogFacts, DbError> {
    match connection.driver() {
        Driver::Postgres => {
            let scope = pbps_pg::resolver::environment::Scope {
                schemas,
                write_path_extras: extras,
            };
            pbps_pg::resolver::environment::read(connection, &scope).await
        }
        Driver::Mssql => Err(DbError::BadRow(
            "SQL Server analysis-scope qualification is not implemented (#611)".into(),
        )),
    }
}
