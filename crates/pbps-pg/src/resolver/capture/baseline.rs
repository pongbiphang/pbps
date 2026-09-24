//! Read the recorded schema and identity map on the capture's own snapshot.
//! No migration, table creation, or public checksum/source output occurs here.

use super::{
    logical::{self, Catalog},
    read::Failure,
};
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
    let mut relations = catalog.rows["pg_class"].iter().filter(|row| {
        catalog.identity("pg_class", row).is_ok_and(|id| {
            id.name
                == [
                    crate::state::LEDGER_SCHEMA,
                    pbps_db::ledger::STATE_TABLE_NAME,
                ]
        })
    });
    let Some(relation) = relations.next() else {
        return Ok(Baseline::Absent);
    };
    if relations.next().is_some()
        || logical::string(relation, "relkind") != Ok("r")
        || relation.get("relrowsecurity") != Some(&Value::Bool(false))
    {
        return Err(Failure::Incomplete);
    }
    let relation = logical::number(relation, "oid").map_err(|_| Failure::Incomplete)?;
    let mut id_number = None;
    for (name, kind) in [("id", "int8"), ("state_json", "text")] {
        let matching: Vec<_> = catalog.rows["pg_attribute"]
            .iter()
            .filter(|row| {
                logical::number(row, "attrelid") == Ok(relation)
                    && logical::string(row, "attname") == Ok(name)
                    && row.get("attisdropped") == Some(&Value::Bool(false))
            })
            .collect();
        let [attribute] = matching.as_slice() else {
            return Err(Failure::Incomplete);
        };
        let id = catalog
            .object(
                "pg_type",
                logical::number(attribute, "atttypid").map_err(|_| Failure::Incomplete)?,
            )
            .map_err(|_| Failure::Incomplete)?;
        if id.name != ["pg_catalog", kind]
            || attribute.get("attnotnull") != Some(&Value::Bool(true))
        {
            return Err(Failure::Incomplete);
        }
        if name == "id" {
            id_number =
                Some(logical::signed(attribute, "attnum").map_err(|_| Failure::Incomplete)?);
        }
    }
    let key = serde_json::json!([id_number.ok_or(Failure::Incomplete)?]);
    if !catalog.rows["pg_constraint"].iter().any(|row| {
        logical::number(row, "conrelid") == Ok(relation)
            && logical::string(row, "contype") == Ok("p")
            && row.get("conkey") == Some(&key)
            && row.get("convalidated") == Some(&Value::Bool(true))
            && row.get("condeferrable") == Some(&Value::Bool(false))
            && row
                .get("conenforced")
                .is_none_or(|value| value == &Value::Bool(true))
    }) {
        return Err(Failure::Incomplete);
    }
    let rows = conn
        .query(
            "SELECT id::text AS id, state_json FROM public.__pbps_state ORDER BY id DESC LIMIT 1",
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
    if text.len() > 32 * 1024 * 1024 {
        return Err(Failure::Incomplete);
    }
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
