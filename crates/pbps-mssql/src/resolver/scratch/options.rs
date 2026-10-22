//! Whole database-option comparison, with the two measured fresh-row sources
//! (DEC-2236.1). JSON retains SQL NULL, boolean and numeric values; the catalog's
//! SQL type accompanies every value, including columns a later build adds.

use super::{one, text};
use pbps_db::DbError;
use pbps_db::transport::QueryConnection;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

/// Identity and measured runtime status do not describe a database option.
const OMITTED: &[&str] = &[
    "name",
    "database_id",
    "create_date",
    "physical_database_name",
    "service_broker_guid",
    "owner_sid",
    "log_reuse_wait",
    "log_reuse_wait_desc",
];

/// These columns are compared with the target by the reported-facts rule,
/// never simultaneously with a fresh database (DEC-2236.1).
pub const REPORTED: &[&str] = &[
    "collation_name",
    "compatibility_level",
    "containment",
    "containment_desc",
    "is_ansi_null_default_on",
    "is_ansi_nulls_on",
    "is_ansi_padding_on",
    "is_ansi_warnings_on",
    "is_arithabort_on",
    "is_concat_null_yields_null_on",
    "is_numeric_roundabort_on",
    "is_quoted_identifier_on",
];

/// Measured on 17.0.4075.5: CREATE DATABASE chooses these independently of
/// model. READ_ONLY also changes model's effective snapshot state without
/// exposing the persisted bit, so that model state refuses below instead of
/// being guessed. The exhaustive live measurement pins this table.
pub const CREATION_BITS: &[(&str, bool)] = &[
    ("is_broker_enabled", true),
    ("is_data_retention_enabled", true),
    ("is_fulltext_enabled", true),
    ("is_mixed_page_allocation_on", false),
    ("is_read_only", false),
    ("is_cleanly_shutdown", false),
    ("is_trustworthy_on", false),
    ("is_db_chaining_on", false),
    ("is_honor_broker_priority_on", false),
    ("is_accelerated_database_recovery_on", false),
    ("is_optimized_locking_on", false),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseOptions {
    types: BTreeMap<String, String>,
    values: BTreeMap<String, Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    Null,
    Bool(bool),
    Text(String),
    Literal(String),
}

fn cell(value: &RawValue) -> Result<Cell, DbError> {
    Ok(match value.get() {
        "null" => Cell::Null,
        "true" => Cell::Bool(true),
        "false" => Cell::Bool(false),
        quoted if quoted.starts_with('"') => {
            Cell::Text(serde_json::from_str(quoted).map_err(|_| {
                DbError::BadRow("sys.databases returned an invalid JSON string".into())
            })?)
        }
        // Keep the engine's numeric token, including any future precision,
        // instead of first rounding it through serde_json's f64 fallback.
        literal => Cell::Literal(literal.to_owned()),
    })
}

pub async fn read(
    conn: &mut impl QueryConnection,
    database: &str,
) -> Result<DatabaseOptions, DbError> {
    let types = conn
        .query(
            "SELECT c.name, TYPE_NAME(c.system_type_id) AS type_name FROM sys.all_columns c \
         WHERE c.object_id = OBJECT_ID(N'sys.databases') ORDER BY c.column_id;",
        )
        .await?
        .iter()
        .map(|row| Ok((text(row, "name")?, text(row, "type_name")?)))
        .collect::<Result<BTreeMap<_, _>, DbError>>()?;
    if types.is_empty() {
        return Err(DbError::BadRow(
            "sys.databases has no readable columns".into(),
        ));
    }
    let row = one(
        conn.query(&format!(
            "SELECT (SELECT * FROM sys.databases WHERE name = {} \
         FOR JSON PATH, INCLUDE_NULL_VALUES) AS options;",
            super::literal(database)
        ))
        .await?,
        "the database options",
    )?;
    let json = text(&row, "options").map_err(|_| {
        DbError::Refused(format!(
            "sys.databases options for database {database} are unreadable"
        ))
    })?;
    let mut rows: Vec<BTreeMap<String, Box<RawValue>>> = serde_json::from_str(&json)
        .map_err(|_| DbError::BadRow("sys.databases did not report a JSON row".into()))?;
    if rows.len() != 1 {
        return Err(DbError::Refused(format!(
            "sys.databases did not report exactly one row for {database}"
        )));
    }
    let values = rows
        .pop()
        .expect("one row")
        .into_iter()
        .map(|(name, value)| cell(&value).map(|value| (name, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if types.keys().ne(values.keys()) {
        return Err(DbError::BadRow(
            "sys.databases columns and values disagree".into(),
        ));
    }
    Ok(DatabaseOptions { types, values })
}

pub async fn supplied_reference(
    conn: &mut impl QueryConnection,
) -> Result<DatabaseOptions, DbError> {
    let mut model = read(conn, "model").await?;
    match model.values.get("is_read_only") {
        Some(Cell::Bool(false)) => {}
        Some(Cell::Bool(true)) => {
            return Err(DbError::Refused(
                "model is READ_ONLY: its reported snapshot state does not show what a new \
             database inherits; run ALTER DATABASE model SET READ_WRITE"
                    .into(),
            ));
        }
        None | Some(Cell::Null | Cell::Text(_) | Cell::Literal(_)) => {
            return Err(DbError::BadRow("model's is_read_only is unreadable".into()));
        }
    }
    for (column, value) in CREATION_BITS {
        if model.types.get(*column).map(String::as_str) != Some("bit") {
            return Err(DbError::Refused(format!(
                "model does not report the measured bit column {column}"
            )));
        }
        model.values.insert((*column).into(), Cell::Bool(*value));
    }
    Ok(model)
}

pub fn differences(actual: &DatabaseOptions, reference: &DatabaseOptions) -> Vec<String> {
    actual
        .types
        .keys()
        .chain(reference.types.keys())
        .filter(|column| {
            !OMITTED.contains(&column.as_str()) && !REPORTED.contains(&column.as_str())
        })
        .filter(|column| {
            actual.types.get(*column) != reference.types.get(*column)
                || actual.values.get(*column) != reference.values.get(*column)
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The recipe already names exactly the database facts compared by §9.
/// Comparing these here keeps the fresh-row exclusion from admitting a
/// supplied database with different compilation options, before #2238 wires
/// the complete compatibility and assessment path.
pub fn recipe_differences(
    actual: &DatabaseOptions,
    recipe: &pbps_db::resolver::environment::DatabaseRecipe,
) -> Result<Vec<String>, DbError> {
    let settings = recipe
        .sql_server
        .as_ref()
        .ok_or_else(|| DbError::BadRow("the SQL Server target has no database recipe".into()))?;
    let level = settings
        .compatibility_level
        .parse::<u64>()
        .map_err(|_| DbError::BadRow("the target compatibility level is not an integer".into()))?;
    let containment = match settings.containment.as_str() {
        "NONE" => 0,
        "PARTIAL" => 1,
        _ => return Err(DbError::BadRow("the target containment is unknown".into())),
    };
    let mut expected = BTreeMap::from([
        (
            "collation_name".to_owned(),
            Cell::Text(settings.collation.clone()),
        ),
        (
            "compatibility_level".into(),
            Cell::Literal(level.to_string()),
        ),
        ("containment".into(), Cell::Literal(containment.to_string())),
        (
            "containment_desc".into(),
            Cell::Text(settings.containment.clone()),
        ),
    ]);
    for (key, option) in pbps_db::resolver::environment::SQL_SERVER_DATABASE_OPTIONS {
        let on = settings
            .options
            .get(*option)
            .ok_or_else(|| DbError::BadRow(format!("the target recipe lacks {option}")))?;
        expected.insert(
            format!("is_{}_on", key.trim_start_matches("database_")),
            Cell::Bool(*on),
        );
    }
    Ok(expected
        .iter()
        .filter(|(key, value)| actual.values.get(*key) != Some(*value))
        .map(|(key, _)| key.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_row_exclusions_are_exactly_the_database_facts_compared_with_the_target() {
        let columns: std::collections::BTreeSet<_> =
            super::super::super::compatibility::OBSERVATIONS
                .iter()
                .filter_map(|fact| fact.strip_prefix("database_"))
                .flat_map(|fact| match fact {
                    "collation" => vec!["collation_name".into()],
                    "compatibility_level" => vec!["compatibility_level".into()],
                    "containment" => vec!["containment".into(), "containment_desc".into()],
                    option => vec![format!("is_{option}_on")],
                })
                .collect();
        assert_eq!(
            columns,
            REPORTED.iter().map(|name| (*name).to_owned()).collect()
        );
    }

    #[test]
    fn unknown_columns_nulls_and_sql_types_remain_part_of_the_comparison() {
        let reference = DatabaseOptions {
            types: BTreeMap::from([("future_option".into(), "int".into())]),
            values: BTreeMap::from([("future_option".into(), Cell::Null)]),
        };
        assert!(differences(&reference, &reference).is_empty());
        let mut actual = reference.clone();
        actual
            .values
            .insert("future_option".into(), Cell::Literal("0".into()));
        assert_eq!(differences(&actual, &reference), ["future_option"]);
        actual = reference.clone();
        actual.types.insert("future_option".into(), "bigint".into());
        assert_eq!(differences(&actual, &reference), ["future_option"]);
    }
    #[test]
    fn numeric_tokens_and_a_textual_null_do_not_lose_their_distinctions() {
        let first = RawValue::from_string("12345678901234567890.0001".into()).unwrap();
        let second = RawValue::from_string("12345678901234567890.0002".into()).unwrap();
        assert_ne!(cell(&first).unwrap(), cell(&second).unwrap());
        let null = RawValue::from_string("null".into()).unwrap();
        let text = RawValue::from_string("\"null\"".into()).unwrap();
        assert_ne!(cell(&null).unwrap(), cell(&text).unwrap());
    }
}
