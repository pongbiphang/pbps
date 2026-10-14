//! Advisory environment observations, never resolver qualification (ADR-0016).

pub mod authorization;
pub mod capture;
pub mod compatibility;
pub mod environment;
pub mod externals;
pub mod reconstruct;
pub mod scratch;
pub mod standard;

// Live, and in the library target on purpose: a capture refuses any
// catalog write it did not make, including another test's on the same
// server, so these run in the serial suite beside the capture's own.
#[cfg(test)]
mod binding_tests;
#[cfg(test)]
mod evidence_tests;

use pbps_db::resolver::environment::{CatalogFacts, DatabaseRecipe, LocaleProvider};
use pbps_db::resolver::{Candidate, Discovery, Extension, Observation, ScratchNames};
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{Conn, DbError};

/// Pure reader support for the evidence versions this PostgreSQL adapter
/// knows how to repeat. Neither an environment label nor an unknown newer
/// rule is accepted as compatibility with this build.
pub fn validate_evidence(
    evidence: &pbps_model::resolver::ResolverEvidence,
) -> Result<(), pbps_model::resolver::EvidenceError> {
    use pbps_model::resolver::EvidenceError;
    if evidence.before().adapter() != capture::INPUT_RULE
        || !matches!(evidence.before().engine_major(), 16 | 18)
        // The one runtime reads the facts both engines report and observes
        // no executable (#1672); evidence naming another rule claims
        // coverage it does not have.
        || evidence.qualification().rule != compatibility::REPORTED_RULE
        || evidence.authorization().rule != authorization::RULE
    {
        return Err(EvidenceError::Version);
    }
    Ok(())
}

/// Both target-side reads of an analysis scope in one `REPEATABLE READ READ
/// ONLY` snapshot: the catalog facts and the deployer's authorization are
/// sealed and compared together, so they must describe one catalog state. As
/// two autocommit sequences, an extension or a grant committed between them
/// would seal half of a change as the whole target (finding on #688).
///
/// Authorization is read first: the visibility read sets a transaction-local
/// `search_path` per write path, and inside one transaction that setting now
/// outlives its statement. The transaction ends either way, so a failed read
/// cannot leave the planning connection inside a block that the next check
/// would trip over.
pub async fn scope_facts(
    conn: &mut impl QueryConnection,
    scope: &environment::Scope<'_>,
    authorization_schemas: &[String],
) -> Result<(CatalogFacts, authorization::AuthorizationContext), DbError> {
    // A transaction already open on this connection would swallow the
    // `BEGIN` below (a warning, not an error, measured on 18): the read would
    // run inside it — under its isolation, on its snapshot if it has one,
    // seeing different commits per statement if it is read committed — and
    // the `COMMIT` that ends the read would commit whatever the caller had
    // begun (DECISIONS 253). So the read refuses instead, asked the one way
    // that reads differently in the two states through this driver: a
    // `set_config(…, is_local)` in one statement, read back in the next,
    // survives inside a transaction and not outside one — the probe the
    // catalog reader uses (finding on #688; an earlier lock-based probe saw
    // only a transaction that had already read the catalog).
    let token = crate::catalog::probe_token();
    conn.query(&crate::catalog::probe_set(&token)).await?;
    let probed = conn.query(crate::catalog::PROBE_READ).await?;
    match probed.as_slice() {
        [row] if row.try_get::<&str>("probe")? != Some(token.as_str()) => {}
        _ => {
            return Err(DbError::BadRow(
                "the planning connection has a transaction open; the scope cannot be read in a snapshot of its own"
                    .into(),
            ));
        }
    }
    conn.query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await?;
    let outcome = async {
        let authorization = authorization::read(conn, authorization_schemas).await?;
        let catalog = environment::read(conn, scope).await?;
        Ok::<_, DbError>((catalog, authorization))
    }
    .await;
    let ended = conn
        .query(if outcome.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        })
        .await;
    let facts = outcome?;
    ended?;
    Ok(facts)
}

/// Creates only this run's own resources. `CREATE DATABASE` cannot run inside
/// a transaction block, so each statement is separate and the caller removes
/// whatever was created when a later one fails.
pub async fn create_scratch(
    conn: &mut impl ExecuteConnection,
    names: &ScratchNames,
    recipe: &DatabaseRecipe,
) -> Result<(), DbError> {
    conn.execute(&format!(
        "CREATE ROLE \"{}\" LOGIN PASSWORD '{}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS",
        names.login(),
        names.password()
    ))
    .await?;
    // The scratch database reproduces the target's encoding and locale so it
    // sorts, compares and encodes the same way (SPEC §9.3.3); an owner is set
    // after creation because the locale clauses and OWNER cannot both follow
    // TEMPLATE cleanly on every version.
    conn.execute(&scratch_database_ddl(names, recipe)).await?;
    conn.execute(&format!(
        "ALTER DATABASE \"{}\" OWNER TO \"{}\"",
        names.database(),
        names.login()
    ))
    .await?;
    // Only this run's own database is narrowed. Pre-existing grants, roles
    // and server settings are never altered to make a server qualify.
    conn.execute(&format!(
        "REVOKE ALL ON DATABASE \"{}\" FROM PUBLIC",
        names.database()
    ))
    .await
}

/// The `CREATE DATABASE` that reproduces the target's encoding and locale
/// (SPEC §9.3.3): `template0`, because a template with a different locale
/// cannot be cloned into one; the provider's own locale clause for ICU and
/// the builtin provider; and the libc collate/ctype in every case, which
/// PostgreSQL requires even when another provider sorts. Rules are passed
/// only when the target has them. Measured on 16 (ICU) and 18 (ICU, builtin).
pub fn scratch_database_ddl(names: &ScratchNames, recipe: &DatabaseRecipe) -> String {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let mut ddl = format!(
        "CREATE DATABASE \"{}\" TEMPLATE template0 ENCODING {}",
        names.database(),
        literal(&recipe.encoding)
    );
    match (recipe.provider, recipe.locale.as_deref()) {
        (LocaleProvider::Libc, _) => ddl.push_str(" LOCALE_PROVIDER libc"),
        (LocaleProvider::Icu, Some(locale)) => {
            ddl.push_str(&format!(
                " LOCALE_PROVIDER icu ICU_LOCALE {}",
                literal(locale)
            ));
            if let Some(rules) = &recipe.icu_rules {
                ddl.push_str(&format!(" ICU_RULES {}", literal(rules)));
            }
        }
        (LocaleProvider::Builtin, Some(locale)) => {
            ddl.push_str(&format!(
                " LOCALE_PROVIDER builtin BUILTIN_LOCALE {}",
                literal(locale)
            ));
        }
        // `DatabaseRecipe::from_catalog` refuses these; rendering them would
        // silently produce a libc database for an ICU target.
        (LocaleProvider::Icu | LocaleProvider::Builtin, None) => {
            unreachable!("a non-libc recipe always carries its locale")
        }
    }
    ddl.push_str(&format!(
        " LC_COLLATE {} LC_CTYPE {}",
        literal(&recipe.collate),
        literal(&recipe.ctype)
    ));
    ddl
}

/// Removes exactly the two run-owned objects. FORCE closes this run's own
/// scratch sessions; it cannot reach anything the run did not create.
/// Drops the run-local authorization roles, after the scratch database they
/// owned is gone. Each drop is independent: a role that cannot be dropped is
/// returned so the caller can report it, and does not stop the others. Uses
/// `IF EXISTS` so a role a retry already removed is not an error.
pub async fn drop_roles(conn: &mut impl ExecuteConnection, roles: &[String]) -> Vec<String> {
    let mut failed = Vec::new();
    for role in roles {
        let statement = format!("DROP ROLE IF EXISTS \"{}\"", role.replace('"', "\"\""));
        if conn.execute(&statement).await.is_err() {
            failed.push(role.clone());
        }
    }
    failed
}

pub async fn drop_scratch(
    conn: &mut impl ExecuteConnection,
    names: &ScratchNames,
) -> Result<(), DbError> {
    let database = conn
        .execute(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            names.database()
        ))
        .await;
    let login = conn
        .execute(&format!("DROP ROLE IF EXISTS \"{}\"", names.login()))
        .await;
    database.and(login)
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

    fn names() -> ScratchNames {
        ScratchNames::new(
            "pbps_scratch_0123456789abcdef".into(),
            "pbps_run_0123456789abcdef".into(),
            "0".repeat(32),
        )
        .unwrap()
    }

    #[test]
    fn the_scratch_database_reproduces_the_targets_encoding_and_locale_provider() {
        let libc = DatabaseRecipe {
            encoding: "UTF8".into(),
            provider: LocaleProvider::Libc,
            collate: "en_US.utf8".into(),
            ctype: "en_US.utf8".into(),
            locale: None,
            icu_rules: None,
            sql_server: None,
        };
        assert_eq!(
            scratch_database_ddl(&names(), &libc),
            "CREATE DATABASE \"pbps_scratch_0123456789abcdef\" \
             TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER libc LC_COLLATE 'en_US.utf8' LC_CTYPE 'en_US.utf8'"
        );
        let icu = DatabaseRecipe {
            provider: LocaleProvider::Icu,
            locale: Some("en-US".into()),
            icu_rules: Some("&a < b".into()),
            ..libc.clone()
        };
        assert_eq!(
            scratch_database_ddl(&names(), &icu),
            "CREATE DATABASE \"pbps_scratch_0123456789abcdef\" \
             TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER icu ICU_LOCALE 'en-US' ICU_RULES '&a < b' \
             LC_COLLATE 'en_US.utf8' LC_CTYPE 'en_US.utf8'"
        );
        // A quote in a locale string cannot end the literal.
        let odd = DatabaseRecipe {
            provider: LocaleProvider::Builtin,
            locale: Some("C.UTF-8'; DROP".into()),
            ..libc
        };
        assert!(scratch_database_ddl(&names(), &odd).contains("BUILTIN_LOCALE 'C.UTF-8''; DROP'"));
    }
}
