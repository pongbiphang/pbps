//! Advisory environment observations, never resolver qualification (ADR-0016).

use pbps_db::resolver::{Candidate, Discovery, Observation};
use pbps_db::{Conn, DbError};

/// A master database GUID may be copied with an installation. Separation is
/// decided using the qualified native runtime as well, never GUID inequality
/// or SERVERPROPERTY('ProcessID'), which is virtualized by the Linux PAL.
pub async fn instance_identity(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<pbps_db::resolver::InstanceObservation, DbError> {
    let rows = conn.query("SELECT CONVERT(nvarchar(36), database_guid) AS instance_key FROM master.sys.database_recovery_status WHERE database_id = 1").await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "native SQL Server identity expected one master database row".into(),
        ));
    };
    let key = row
        .try_get::<&str>("instance_key")?
        .filter(|value| master_guid(value))
        .ok_or_else(|| DbError::BadRow("native SQL Server master identity is unreadable".into()))?;
    Ok(pbps_db::resolver::InstanceObservation {
        instance_key: key.to_ascii_lowercase(),
        process: pbps_db::resolver::BackendProcess::RuntimeOnly,
    })
}

fn master_guid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && value != "00000000-0000-0000-0000-000000000000"
}

const ENVIRONMENT: &str = "\
SELECT CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')) AS product_version,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductMajorVersion')) AS product_major_version,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductLevel')) AS product_level,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductUpdateLevel')) AS product_update_level,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductUpdateReference')) AS product_update_reference,
       CONVERT(nvarchar(128), SERVERPROPERTY('Edition')) AS edition,
       CONVERT(nvarchar(128), SERVERPROPERTY('EngineEdition')) AS engine_edition,
       CONVERT(nvarchar(128), SERVERPROPERTY('Collation')) AS server_collation,
       CONVERT(nvarchar(128), d.collation_name) AS database_collation,
       CONVERT(nvarchar(128), d.compatibility_level) AS database_compatibility_level,
       CONVERT(nvarchar(128), USER_NAME()) AS session_current_user,
       CONVERT(nvarchar(128), SCHEMA_NAME()) AS session_default_schema,
       CONVERT(nvarchar(128), @@LANGUAGE) AS session_language,
       CONVERT(nvarchar(128), @@DATEFIRST) AS session_date_first,
       CONVERT(nvarchar(128), SESSIONPROPERTY('ANSI_NULLS')) AS session_ansi_nulls,
       CONVERT(nvarchar(128), SESSIONPROPERTY('ANSI_PADDING')) AS session_ansi_padding,
       CONVERT(nvarchar(128), SESSIONPROPERTY('ANSI_WARNINGS')) AS session_ansi_warnings,
       CONVERT(nvarchar(128), SESSIONPROPERTY('ARITHABORT')) AS session_arithabort,
       CONVERT(nvarchar(128), SESSIONPROPERTY('CONCAT_NULL_YIELDS_NULL')) AS session_concat_null_yields_null,
       CONVERT(nvarchar(128), SESSIONPROPERTY('NUMERIC_ROUNDABORT')) AS session_numeric_roundabort,
       CONVERT(nvarchar(128), SESSIONPROPERTY('QUOTED_IDENTIFIER')) AS session_quoted_identifier
FROM sys.databases d WHERE d.database_id = DB_ID();";

const FIELDS: &[&str] = &[
    "product_version",
    "product_major_version",
    "product_level",
    "product_update_level",
    "product_update_reference",
    "edition",
    "engine_edition",
    "server_collation",
    "database_collation",
    "database_compatibility_level",
    "session_current_user",
    "session_default_schema",
    "session_language",
    "session_date_first",
    "session_ansi_nulls",
    "session_ansi_padding",
    "session_ansi_warnings",
    "session_arithabort",
    "session_concat_null_yields_null",
    "session_numeric_roundabort",
    "session_quoted_identifier",
];

pub async fn discover(conn: &mut Conn) -> Result<Discovery, DbError> {
    let rows = conn.query(ENVIRONMENT).await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "resolver discovery expected one current database row".into(),
        ));
    };
    let mut observations = std::collections::BTreeMap::new();
    for &field in FIELDS {
        observations.insert(
            field.into(),
            Observation::reported(row.try_get::<&str>(field)?),
        );
    }
    let candidate = candidate(
        observations["engine_edition"].value(),
        observations["product_major_version"].value(),
    );
    Ok(Discovery::unverified(observations, None, candidate))
}

fn candidate(engine_edition: Option<&str>, major: Option<&str>) -> Candidate {
    // Azure/Synapse/Edge/Fabric and unknown families have independent version
    // numbers. Developer success also cannot waive the target's edition gate.
    let release = match (engine_edition, major) {
        (Some("2" | "3" | "4"), Some("14")) => Some("2017"),
        (Some("2" | "3" | "4"), Some("15")) => Some("2019"),
        (Some("2" | "3" | "4"), Some("16")) => Some("2022"),
        (Some("2" | "3" | "4"), Some("17")) => Some("2025"),
        _ => None,
    };
    match release {
        Some(release) => Candidate::Suggested { image: format!("mcr.microsoft.com/mssql/server:{release}-latest") },
        None => Candidate::Unavailable { reason: "No justified boxed SQL Server image mapping for this product family/version; a dedicated scratch server also needs full qualification.".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_or_unknown_products_never_borrow_boxed_sql_server_versions() {
        for family in [Some("2"), Some("3"), Some("4")] {
            assert_eq!(
                candidate(family, Some("17")),
                Candidate::Suggested {
                    image: "mcr.microsoft.com/mssql/server:2025-latest".into()
                }
            );
        }
        for family in [
            None,
            Some(""),
            Some("5"),
            Some("6"),
            Some("8"),
            Some("9"),
            Some("11"),
            Some("12"),
        ] {
            assert!(matches!(
                candidate(family, Some("17")),
                Candidate::Unavailable { .. }
            ));
        }
        for major in [None, Some("13"), Some("18"), Some("17.0")] {
            assert!(matches!(
                candidate(Some("3"), major),
                Candidate::Unavailable { .. }
            ));
        }
    }
}
