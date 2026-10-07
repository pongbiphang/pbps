//! Resolver lifecycle, separate from environment and binding qualification.
//!
//! Acquiring an image never authorizes declaration transfer. The planning
//! command may use these resources only after the complete runtime admission
//! and engine-specific qualification required by ADR-0016.

#[cfg(target_os = "linux")]
pub mod docker;

#[cfg(target_os = "linux")]
pub mod native;

#[cfg(target_os = "linux")]
pub mod scope;

#[cfg(target_os = "linux")]
pub mod server;

pub mod sealing;

/// Resolver fixtures name what they create unqualified, in `public` or
/// `dbo`. Every PostgreSQL session opens on an empty path (DEC-1564.1), where
/// such a `CREATE` has no schema to go to, so a fixture moves its own session
/// to `public` first. SQL Server has no path and is left as it is.
#[cfg(all(test, target_os = "linux"))]
pub(crate) async fn fixture_on_public(
    connection: &mut pbps_db::transport::StreamConn,
) -> Result<(), pbps_db::DbError> {
    if connection.driver() == pbps_db::Driver::Postgres {
        connection.execute("SET search_path = public").await?;
    }
    Ok(())
}
