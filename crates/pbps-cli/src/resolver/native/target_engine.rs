//! Engine routing for the native target's read-only identity check. The
//! analysis-scope reads route through `resolver::scope`.

use pbps_db::resolver::InstanceObservation;
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
