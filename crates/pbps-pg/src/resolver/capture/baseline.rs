//! Read the recorded schema and identity map on the capture's own snapshot.
//! No migration, table creation, or public checksum/source output occurs here.

use super::{logical::Catalog, read::Failure};
use pbps_db::transport::QueryConnection;
use serde_json::Value;

#[derive(serde::Serialize)]
#[serde(tag = "status")]
pub(super) enum Baseline {
    Absent,
    Empty,
    Recorded { entry: i64, state: Value },
}

pub(super) async fn read(
    conn: &mut impl QueryConnection,
    catalog: &Catalog,
) -> Result<Baseline, Failure> {
    // Use the write path's complete recipe/editor rule, on this capture's
    // already-owned canonical snapshot. The source/type closure has qualified
    // before any of these facts may call a catalog renderer. The surrounding
    // raw/rendered/fresh witnesses cover every catalog this query consults.
    let facts = conn
        .query(&crate::state::ledger_facts())
        .await
        .map_err(|_| Failure::Read)?;
    let account = conn
        .query("SELECT current_user::text AS me")
        .await
        .map_err(|_| Failure::Read)?;
    let [account] = account.as_slice() else {
        return Err(Failure::Incomplete);
    };
    let account = account
        .try_get::<&str>("me")
        .map_err(|_| Failure::Incomplete)?
        .ok_or(Failure::Incomplete)?;
    let problems = crate::state::ledger_problems_from_facts(&facts, account)
        .map_err(|_| Failure::Incomplete)?;
    if !problems.is_empty() {
        // Recipe differences may contain private defaults or role names.
        // The capture boundary reports the failed qualification, never SQL.
        return Err(Failure::Coverage(super::Uncovered::class(
            "recorded-baseline",
            "ledger recipe or editor is not qualified",
        )));
    }
    let mut relations = catalog.rows["pg_class"].iter().filter(|row| {
        catalog.identity("pg_class", row).is_ok_and(|id| {
            id.name
                == [
                    crate::state::LEDGER_SCHEMA,
                    pbps_db::ledger::STATE_TABLE_NAME,
                ]
        })
    });
    if relations.next().is_none() {
        return Ok(Baseline::Absent);
    }
    if relations.next().is_some() {
        return Err(Failure::Incomplete);
    }
    let rows = conn
        .query(
            "SELECT id::text AS id, state_json FROM ONLY public.__pbps_state ORDER BY id DESC LIMIT 1",
        )
        .await
        .map_err(|_| Failure::Read)?;
    let [row] = rows.as_slice() else {
        return if rows.is_empty() {
            Ok(Baseline::Empty)
        } else {
            Err(Failure::Incomplete)
        };
    };
    let entry = row
        .try_get::<&str>("id")
        .map_err(|_| Failure::Incomplete)?
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .ok_or(Failure::Incomplete)?;
    let text = row
        .try_get::<&str>("state_json")
        .map_err(|_| Failure::Incomplete)?
        .ok_or(Failure::Incomplete)?;
    // Validate the version before accepting the JSON. Keep the original full
    // value for its verifier: deserializing defaults must not erase changes.
    pbps_model::StateSnapshot::read_json(text).map_err(|error| match error {
        pbps_model::Unreadable::UnsupportedVersion(_) => Failure::Coverage(
            super::Uncovered::class("recorded-baseline", "state format is not supported"),
        ),
        pbps_model::Unreadable::Malformed(_) => Failure::Incomplete,
        pbps_model::Unreadable::Denied(_) => Failure::Read,
    })?;
    let state = serde_json::from_str(text).map_err(|_| Failure::Incomplete)?;
    Ok(Baseline::Recorded { entry, state })
}

#[cfg(test)]
mod tests;
