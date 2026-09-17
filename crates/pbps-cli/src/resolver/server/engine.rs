//! Engine routing for the dedicated-server profile.
//!
//! Instance SQL stays in the engine crates; this file only chooses which one
//! answers. Nothing here consumes declarations or produces binding evidence.

use pbps_db::resolver::{
    InstanceObservation, OwnSession, ScratchNames, SessionCounter, SessionInventory,
};
use pbps_db::transport::StreamConn;
use pbps_db::{DbError, Driver};

pub(super) async fn identity(connection: &mut StreamConn) -> Result<InstanceObservation, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::instance_identity(connection).await,
        Driver::Mssql => pbps_mssql::resolver::instance_identity(connection).await,
    }
}

pub(super) async fn own_session(connection: &mut StreamConn) -> Result<OwnSession, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::own_session(connection).await,
        Driver::Mssql => pbps_mssql::resolver::own_session(connection).await,
    }
}

pub(super) async fn client_sessions(
    connection: &mut StreamConn,
) -> Result<SessionInventory, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::client_sessions(connection).await,
        Driver::Mssql => pbps_mssql::resolver::client_sessions(connection).await,
    }
}

pub(super) async fn session_counter(
    connection: &mut StreamConn,
) -> Result<SessionCounter, DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::session_counter(connection).await,
        Driver::Mssql => pbps_mssql::resolver::session_counter(connection).await,
    }
}

pub(super) async fn create_scratch(
    connection: &mut StreamConn,
    names: &ScratchNames,
) -> Result<(), DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::create_scratch(connection, names).await,
        Driver::Mssql => pbps_mssql::resolver::create_scratch(connection, names).await,
    }
}

pub(super) async fn drop_scratch(
    connection: &mut StreamConn,
    names: &ScratchNames,
) -> Result<(), DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::drop_scratch(connection, names).await,
        Driver::Mssql => pbps_mssql::resolver::drop_scratch(connection, names).await,
    }
}
