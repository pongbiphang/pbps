//! Advisory retention answers are consumed before immutable planning.
use super::Entry;
use pbps_model::{Hints, RetainedExpression, Schema};

#[allow(clippy::too_many_arguments)]
pub async fn compare(
    entry: &mut Entry<'_>,
    conn: &mut pbps_db::Conn,
    target: &crate::db::Target,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
    live: &Schema,
    hints: &Hints,
    requests: &[RetainedExpression],
) -> anyhow::Result<Vec<RetainedExpression>> {
    use pbps_cli::resolver::server::{ProduceError, scratch};
    let Ok(identity) = pbps_cli::resolver::server::target_identity(conn).await else {
        return Ok(Vec::new());
    };
    let Ok(open) = entry.open(conn.driver(), &identity).await else {
        return Ok(Vec::new());
    };
    match scratch::retained::compare(
        &open.connection,
        target.connection(),
        &identity,
        before,
        after,
        live,
        hints,
        requests,
        &open.standard,
        open.externals.as_deref(),
        &open.provenance,
    )
    .await
    {
        Ok(facts) => Ok(facts),
        Err(scratch::domain::Refused::Unavailable(error @ ProduceError::Cleanup(_))) => {
            Err(error.into())
        }
        Err(_) => Ok(Vec::new()),
    }
}
