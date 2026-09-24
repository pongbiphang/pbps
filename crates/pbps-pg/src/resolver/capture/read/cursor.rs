//! Fetch bounded groups of rows from one server-side query snapshot. Catalog
//! bodies are individual objects, never a whole-catalog JSON aggregate.

use super::Failure;
use pbps_db::transport::QueryConnection;

pub(super) async fn read(
    conn: &mut impl QueryConnection,
    sql: &str,
    mut consume: impl FnMut(pbps_db::Row) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let cursor = format!(
        "pbps_capture_{}",
        crate::catalog::probe_token().replace('-', "_")
    );
    conn.query(&format!("DECLARE {cursor} NO SCROLL CURSOR FOR {sql}"))
        .await
        .map_err(|_| Failure::Read)?;
    let fetch = format!("FETCH FORWARD 256 FROM {cursor}");
    loop {
        let rows = conn.query(&fetch).await.map_err(|_| Failure::Read)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            consume(row)?;
        }
    }
    // Every caller owns a transaction. On failure its rollback also closes
    // this cursor; cancellation makes the native owner drop the connection.
    conn.query(&format!("CLOSE {cursor}"))
        .await
        .map_err(|_| Failure::Read)?;
    Ok(())
}
