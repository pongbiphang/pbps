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
    recipe: &DatabaseRecipe,
) -> Result<(), DbError> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::create_scratch(connection, names, recipe).await,
        Driver::Mssql => pbps_mssql::resolver::create_scratch(connection, names, recipe).await,
    }
}

pub(super) async fn drop_roles(connection: &mut StreamConn, roles: &[String]) -> Vec<String> {
    match connection.driver() {
        Driver::Postgres => pbps_pg::resolver::drop_roles(connection, roles).await,
        Driver::Mssql => pbps_mssql::resolver::drop_roles(connection, roles).await,
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

// Binding resolution has one adapter, PostgreSQL's (#613); SQL Server's is
// designed and built separately (#619, #620). The reconstruction, the
// managed-name inventory and both captures are that adapter's private state,
// as the target capture's are.
pub(super) use pbps_pg::resolver::capture::{CaptureScope, CapturedInputs, Managed, Paths};
pub(super) use pbps_pg::resolver::reconstruct::Reconstruction;

const NO_BINDING_ADAPTER: &str =
    "SQL Server has no binding adapter yet; its design and implementation are #619 and #620";

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
pub(super) async fn compile(
    reconstruction: &mut Reconstruction,
    extras: &[String],
    connection: &mut StreamConn,
) -> Result<(), String> {
    match connection.driver() {
        Driver::Postgres => reconstruction
            .compile(
                &pbps_pg::Postgres::with_write_path_extras(extras.to_vec()),
                connection,
            )
            .await
            .map_err(|error| error.to_string()),
        Driver::Mssql => Err(NO_BINDING_ADAPTER.into()),
    }
}

/// Captures scratch twice: its managed objects, and then the scope their
/// bindings derive, which is the scope the target is captured under too.
pub(super) async fn capture_desired(
    connection: &mut StreamConn,
    base: &Managed,
    desired: &Managed,
    paths: &Paths,
) -> Result<(CapturedInputs, CaptureScope), String> {
    use pbps_pg::resolver::capture;
    match connection.driver() {
        Driver::Postgres => {
            let first = capture::capture(connection, &capture::managed_scope(desired))
                .await
                .map_err(|error| error.to_string())?;
            let scope = capture::scope(&first, &[base, desired], paths);
            let captured = capture::capture(connection, &scope)
                .await
                .map_err(|error| error.to_string())?;
            Ok((captured, scope))
        }
        Driver::Mssql => Err(NO_BINDING_ADAPTER.into()),
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
