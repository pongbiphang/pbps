//! Advisory environment observations, never resolver qualification (ADR-0016).

use pbps_db::resolver::{Candidate, Discovery, Extension, Observation};
use pbps_db::{Conn, DbError};

/// The cluster identifier is observed alongside, never instead of, qualified
/// process provenance. Cloned clusters can share a system identifier.
pub async fn instance_identity(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<pbps_db::resolver::InstanceObservation, DbError> {
    let rows = conn.query("SELECT system_identifier::text AS instance_key, pg_catalog.pg_backend_pid()::text AS backend_pid FROM pg_catalog.pg_control_system()").await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "native PostgreSQL identity expected one control row".into(),
        ));
    };
    let key = row
        .try_get::<&str>("instance_key")?
        .filter(|value| value.parse::<i64>().is_ok_and(|id| id != 0))
        .ok_or_else(|| {
            DbError::BadRow("native PostgreSQL cluster identity is unreadable".into())
        })?;
    let process = row
        .try_get::<&str>("backend_pid")?
        .and_then(|value| value.parse::<std::num::NonZeroU32>().ok())
        .ok_or_else(|| DbError::BadRow("native PostgreSQL backend process is unreadable".into()))?;
    Ok(pbps_db::resolver::InstanceObservation {
        instance_key: key.to_owned(),
        process: pbps_db::resolver::BackendProcess::NativePid(process),
    })
}

// Version-dependent locale fields are projected by name from this one catalog
// row: PostgreSQL 17 renamed daticulocale to datlocale. A missing/NULL field
// stays unreported, never an empty locale certified as equivalent.
const ENVIRONMENT: &str = "\
SELECT pg_catalog.current_setting('server_version') AS server_version,
       pg_catalog.current_setting('server_version_num') AS server_version_num,
       pg_catalog.pg_encoding_to_char(d.encoding) AS database_encoding,
       d.datcollate AS database_collate, d.datctype AS database_ctype,
       pg_catalog.to_jsonb(d)->>'datlocprovider' AS database_locale_provider,
       COALESCE(pg_catalog.to_jsonb(d)->>'datlocale',
                pg_catalog.to_jsonb(d)->>'daticulocale') AS database_locale,
       pg_catalog.to_jsonb(d)->>'datcollversion' AS database_collation_recorded_version,
       pg_catalog.to_jsonb(d)->>'daticurules' AS database_icu_rules,
       current_user::text AS session_current_user,
       session_user::text AS session_authenticated_user,
       pg_catalog.current_setting('search_path') AS session_search_path,
       pg_catalog.current_schemas(true)::text AS session_effective_schemas,
       pg_catalog.current_setting('DateStyle') AS session_date_style,
       pg_catalog.current_setting('TimeZone') AS session_time_zone,
       pg_catalog.current_setting('IntervalStyle') AS session_interval_style,
       pg_catalog.current_setting('standard_conforming_strings') AS session_standard_conforming_strings,
       pg_catalog.current_setting('check_function_bodies') AS session_check_function_bodies,
       pg_catalog.current_setting('default_text_search_config') AS session_text_search_config,
       pg_catalog.current_setting('row_security') AS session_row_security
FROM pg_catalog.pg_database d WHERE d.datname = pg_catalog.current_database()";

const FIELDS: &[&str] = &[
    "server_version",
    "server_version_num",
    "database_encoding",
    "database_collate",
    "database_ctype",
    "database_locale_provider",
    "database_locale",
    "database_collation_recorded_version",
    "database_icu_rules",
    "session_current_user",
    "session_authenticated_user",
    "session_search_path",
    "session_effective_schemas",
    "session_date_style",
    "session_time_zone",
    "session_interval_style",
    "session_standard_conforming_strings",
    "session_check_function_bodies",
    "session_text_search_config",
    "session_row_security",
];

/// Catalog-only discovery, usable inside a read-only transaction. Its reads
/// are advisory observations; they must not be reused as a sealed capture.
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
    let candidate = candidate(observations["server_version_num"].value());
    // LEFT JOIN preserves an extension whose namespace could not be read;
    // an inner join would silently certify an incomplete inventory.
    let extensions = conn
        .query(
            "\
SELECT e.extname::text AS name, n.nspname::text AS schema, e.extversion AS version
FROM pg_catalog.pg_extension e
LEFT JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
ORDER BY e.extname",
        )
        .await?
        .iter()
        .map(|row| {
            let required = |field| -> Result<String, DbError> {
                row.try_get::<&str>(field)?
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        DbError::BadRow(format!(
                            "resolver extension inventory did not report {field}"
                        ))
                    })
            };
            Ok(Extension {
                name: required("name")?,
                schema: required("schema")?,
                version: required("version")?,
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    Ok(Discovery::unverified(
        observations,
        Some(extensions),
        candidate,
    ))
}

fn candidate(version: Option<&str>) -> Candidate {
    let major = version
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v / 10_000);
    match major {
        // A bounded official-image family suggestion, not a promise that
        // every patch, extension, vendor build or platform is available.
        Some(major @ 14..=18) => Candidate::Suggested { image: format!("postgres:{major}") },
        _ => Candidate::Unavailable { reason: "No known official image family for the reported PostgreSQL version; a dedicated scratch server also needs full qualification.".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_postgres_image_families_are_suggested() {
        for (version, image) in [("160015", "postgres:16"), ("180006", "postgres:18")] {
            assert_eq!(
                candidate(Some(version)),
                Candidate::Suggested {
                    image: image.into()
                }
            );
        }
        for version in [
            None,
            Some(""),
            Some("18.6"),
            Some("-1"),
            Some("190001"),
            Some("90600"),
        ] {
            assert!(matches!(candidate(version), Candidate::Unavailable { .. }));
        }
    }
}
