//! Advisory environment observations, never resolver qualification (ADR-0016).

pub mod authorization;
pub mod capture;
pub mod compatibility;
pub mod draft;
pub mod environment;
pub mod externals;
pub mod reconstruct;
pub mod recovery;
pub mod scratch;
pub mod session;
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
    // The one runtime reads the facts both engines report and observes no
    // executable (#1672); evidence naming another rule claims coverage it does
    // not have, and the identifier moves with the set of facts compared, so an
    // earlier one is refused by name (DEC-2012.1).
    let rule = &evidence.qualification().rule;
    if !compatibility::ACCEPTED_RULES.contains(&rule.as_str()) {
        return Err(EvidenceError::Rule(rule.clone()));
    }
    if evidence.before().adapter() != capture::INPUT_RULE
        || !matches!(evidence.before().engine_major(), 16 | 18)
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

/// [`scope_facts`] inside the caller's open transaction: apply's checks
/// before its first statement and after its last, which must see that
/// transaction's own DDL and grants (#616). Refused outside a transaction,
/// where [`scope_facts`] is the read.
///
/// Its statements read the commits before each, not one snapshot. Every
/// fact is compared with the one the plan sealed, so a change that lasts
/// is found at the read that follows it; one made and undone between two
/// statements leaves nothing the plan's statements or the closing read
/// can tell from no change (SPEC 7.6).
///
/// Under a read-only savepoint, rolled back on every path, so whatever the
/// readers set for themselves goes with it (DECISIONS 418).
pub async fn scope_facts_in_transaction(
    conn: &mut impl QueryConnection,
    scope: &environment::Scope<'_>,
    authorization_schemas: &[String],
) -> Result<(CatalogFacts, authorization::AuthorizationContext), DbError> {
    let token = crate::catalog::probe_token();
    conn.query(&crate::catalog::probe_set(&token)).await?;
    let probed = conn.query(crate::catalog::PROBE_READ).await?;
    match probed.as_slice() {
        [row] if row.try_get::<&str>("probe")? == Some(token.as_str()) => {}
        _ => {
            return Err(DbError::BadRow(
                "the apply connection has no transaction open; its checks read inside it".into(),
            ));
        }
    }
    conn.query("SAVEPOINT pbps_scope").await?;
    let outcome = async {
        conn.query("SET LOCAL transaction_read_only = on").await?;
        let authorization = authorization::read(conn, authorization_schemas).await?;
        let catalog = environment::read(conn, scope).await?;
        Ok::<_, DbError>((catalog, authorization))
    }
    .await;
    let unwound = match conn.query("ROLLBACK TO SAVEPOINT pbps_scope").await {
        Ok(_) => conn.query("RELEASE SAVEPOINT pbps_scope").await,
        Err(error) => Err(error),
    };
    let facts = outcome?;
    unwound?;
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

/// How many run-local roles one round trip creates, grants or drops (#2049,
/// DEC-2049.1).
/// A superuser deployer's closure is every role in its cluster, and one round
/// trip each cost about 9 ms a role, almost all of it waiting. A batch is one
/// implicit transaction, and `CREATE ROLE` and `DROP ROLE` each hold a lock on
/// the role until it ends (one `pg_locks` row per role, measured on 18.6),
/// against a lock table of `max_locks_per_transaction` x (`max_connections` +
/// `max_prepared_transactions`) entries, 6400 by default and shared with every
/// other session on the cluster (#2050). 250 is 4% of that and still leaves
/// 40 round trips for ten thousand roles.
pub const ROLE_BATCH: usize = 250;

/// Drops the run-local authorization roles, after the scratch database they
/// owned is gone. Each drop is independent: a role that cannot be dropped is
/// returned so the caller can report it, and does not stop the others. Uses
/// `IF EXISTS` so a role a retry already removed is not an error.
///
/// Roles go [`ROLE_BATCH`] to a statement. A batch is one transaction, so the
/// one role that cannot be dropped (it owns something) fails every drop in its
/// batch; that batch is then retried a role at a time, which names exactly the
/// roles that cannot go and drops all the others.
pub async fn drop_roles(
    conn: &mut impl ExecuteConnection,
    roles: &[pbps_model::RoleName],
) -> Vec<pbps_model::RoleName> {
    let mut failed = Vec::new();
    for batch in roles.chunks(ROLE_BATCH) {
        let all = batch.iter().map(quoted_role).collect::<Vec<_>>().join(", ");
        if conn
            .execute(&format!("DROP ROLE IF EXISTS {all}"))
            .await
            .is_ok()
        {
            continue;
        }
        for role in batch {
            let statement = format!("DROP ROLE IF EXISTS {}", quoted_role(role));
            if conn.execute(&statement).await.is_err() {
                failed.push(role.clone());
            }
        }
    }
    failed
}

/// Whether the session's role may create roles: `SUPERUSER` or `CREATEROLE`.
/// Asked before the run records the roles it means to create, since dropping a
/// role takes the same attribute and a run that never could create them could
/// not drop what it had recorded either.
pub async fn may_create_roles(conn: &mut impl QueryConnection) -> Result<bool, DbError> {
    let rows = conn
        .query(
            "SELECT rolsuper OR rolcreaterole AS allowed \
               FROM pg_catalog.pg_roles WHERE rolname = current_user",
        )
        .await?;
    Ok(rows
        .first()
        .map(|row| row.try_get::<bool>("allowed"))
        .transpose()?
        .flatten()
        .unwrap_or(false))
}

/// Creates the roles a rehearsal maps a plan's roles to: a name and nothing
/// else, so they hold no privilege the plan did not give them. Each is created
/// on its own, so the first the account may not create stops the rest and the
/// error says it was that one (`42501` when the account lacks `CREATEROLE`).
pub async fn create_rehearsal_roles(
    conn: &mut impl ExecuteConnection,
    roles: &[pbps_model::RoleName],
) -> Result<(), DbError> {
    for role in roles {
        conn.execute(&format!("CREATE ROLE {} NOLOGIN", quoted_role(role)))
            .await?;
    }
    Ok(())
}

/// A role name as an identifier in a statement: the one place a run's role
/// names are quoted, for creating them and for dropping them.
fn quoted_role(role: &pbps_model::RoleName) -> String {
    format!("\"{}\"", role.raw().replace('"', "\"\""))
}

/// Removes exactly the two run-owned objects. FORCE closes this run's own
/// scratch sessions; it cannot reach anything the run did not create.
pub async fn drop_scratch(
    conn: &mut impl ExecuteConnection,
    names: &ScratchNames,
) -> Result<(), DbError> {
    let database = drop_database(conn, names.database(), true).await;
    let login = drop_login(conn, names.login()).await;
    database.and(login)
}

/// Drops a run-owned database. Only the owning run forces: a later run
/// removing a dead run's does not, so a session still connected refuses the
/// drop, a second guard behind the decision that the run is gone (#2410).
pub async fn drop_database(
    conn: &mut impl ExecuteConnection,
    database: &str,
    force: bool,
) -> Result<(), DbError> {
    conn.execute(&format!(
        "DROP DATABASE IF EXISTS \"{database}\"{}",
        if force { " WITH (FORCE)" } else { "" }
    ))
    .await
}

/// Drops the run's login, its record: it goes last (`recovery`).
pub async fn drop_login(conn: &mut impl ExecuteConnection, login: &str) -> Result<(), DbError> {
    conn.execute(&format!("DROP ROLE IF EXISTS \"{login}\""))
        .await
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
