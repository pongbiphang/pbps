//! Advisory environment observations, never resolver qualification (ADR-0016).

pub mod authorization;
pub mod compatibility;
pub mod environment;

use pbps_db::resolver::environment::{DatabaseRecipe, LocaleProvider};
use pbps_db::resolver::{
    Candidate, Discovery, Extension, Observation, OwnSession, ScratchNames, SessionCounter,
    SessionInventory,
};
use pbps_db::transport::StreamConn;
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

/// This session's own key, readable by any login.
///
/// PostgreSQL's backend pid is both the session key in `pg_stat_activity` and
/// the process that holds this connection's socket, so one read answers the
/// census and the correlation with the qualified runtime.
pub async fn own_session(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<OwnSession, DbError> {
    let rows = conn
        .query("SELECT pg_catalog.pg_backend_pid()::text AS key")
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "PostgreSQL expected exactly one backend row".into(),
        ));
    };
    let process = row
        .try_get::<&str>("key")?
        .and_then(|value| value.parse::<std::num::NonZeroU32>().ok())
        .ok_or_else(|| DbError::BadRow("PostgreSQL did not report this backend".into()))?;
    Ok(OwnSession {
        key: process.to_string(),
        process: pbps_db::resolver::BackendProcess::NativePid(process),
    })
}

/// The cumulative counter, its origin, and the rows it is summed over.
///
/// Callers use it on its own to close a qualification window: the inventory
/// they took at the start is only as fresh as the last read of this.
///
/// The per-database rows come back with it because the sum can be made to
/// fall: dropping a database removes its `pg_stat_database` row and its
/// share of the total, so a session could hide its own increment behind a
/// drop of a database whose count matched. A row that disappears is visible
/// even when the arithmetic is not.
pub async fn session_counter(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<SessionCounter, DbError> {
    let rows = conn
        .query(
            "\
SELECT d.datid::text AS datid, d.sessions::text AS sessions,
       (SELECT COALESCE(max(x.stats_reset)::text, 'never')
        FROM pg_catalog.pg_stat_database x) AS epoch,
       (pg_catalog.pg_has_role(current_user, 'pg_read_all_stats', 'MEMBER')
        OR COALESCE((SELECT r.rolsuper FROM pg_catalog.pg_roles r
                     WHERE r.rolname = current_user), false))::text AS complete
FROM pg_catalog.pg_stat_database d",
        )
        .await?;
    if rows.is_empty() {
        return Err(DbError::BadRow(
            "PostgreSQL reported no statistics rows at all".into(),
        ));
    }
    let mut total: u64 = 0;
    let mut epoch: Option<String> = None;
    let mut continuity = Vec::new();
    for row in &rows {
        if row.try_get::<&str>("complete")? != Some("true") {
            return Err(DbError::Refused(
                "this scratch role cannot observe every PostgreSQL backend; the server's client sessions are unreadable, not absent".into(),
            ));
        }
        let datid = row
            .try_get::<&str>("datid")?
            .ok_or_else(|| DbError::BadRow("a PostgreSQL statistics row has no datid".into()))?;
        let sessions = row
            .try_get::<&str>("sessions")?
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                DbError::BadRow("a PostgreSQL statistics row has no session count".into())
            })?;
        // The epoch is one server-computed `max` repeated on every row, not a
        // per-row value folded here: `max` over a timestamp ignores the nulls
        // a never-reset database has, where folding the text would let the
        // literal 'never' outrank every real reset and pin the epoch there.
        let reported = row.try_get::<&str>("epoch")?.ok_or_else(|| {
            DbError::BadRow("PostgreSQL did not report its statistics epoch".into())
        })?;
        if epoch.get_or_insert_with(|| reported.to_owned()) != reported {
            return Err(DbError::BadRow(
                "PostgreSQL reported two statistics epochs in one read".into(),
            ));
        }
        total = total.checked_add(sessions).ok_or_else(|| {
            DbError::BadRow("PostgreSQL's cumulative session count overflowed".into())
        })?;
        continuity.push(datid.to_owned());
    }
    continuity.sort();
    Ok(SessionCounter {
        total,
        epoch: epoch.unwrap_or_else(|| "never".into()),
        continuity,
    })
}

/// Complete client-session inventory for the dedicated-server exclusion
/// check, with the cumulative session counter that makes a session which
/// opened and closed between two reads visible anyway.
///
/// A client is a backend something connected to, which `client_port` says and
/// `backend_type` does not: measured on PostgreSQL 18.6, every background
/// process reports a null port while both an ordinary session and a
/// `replication=database` one report -1 over the Unix socket. Naming the
/// backend types instead would be a list to keep current, and a type added by
/// a later release or an extension would read as a background process.
///
/// Measured on the same engine: autovacuum workers, launched parallel workers
/// and this run's own DDL leave `pg_stat_database.sessions` unchanged, while
/// an ordinary client session increments it and is still counted once it has
/// disconnected. A `walsender` is the exception and moves it by nothing at
/// all, so a replication connection is caught here while it is open and by
/// nothing once it has closed (#651).
///
/// A role that cannot see other backends returns an error: an incomplete view
/// of `pg_stat_activity` must never read as an idle server.
pub async fn client_sessions(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<SessionInventory, DbError> {
    let counter = session_counter(conn).await?;
    let rows = conn
        .query(
            "\
SELECT a.pid::text AS session_key, a.backend_type AS backend_type,
       (a.client_port IS NOT NULL)::text AS client,
       (a.pid = pg_catalog.pg_backend_pid())::text AS own
FROM pg_catalog.pg_stat_activity a",
        )
        .await?;
    // Bracket: a session that was connected for the first counter read and
    // gone by the list would otherwise have its count absorbed silently.
    if session_counter(conn).await? != counter {
        return Err(DbError::Refused(
            "PostgreSQL's session counter moved while its session list was read; this server is not exclusively this run's".into(),
        ));
    }
    let mut own = None;
    let mut clients = Vec::new();
    for row in &rows {
        let key = row
            .try_get::<&str>("session_key")?
            .ok_or_else(|| DbError::BadRow("a PostgreSQL backend reported no pid".into()))?;
        // NULL here means the row exists but its kind is hidden. Treating it
        // as "not a client" would let an unreadable session pass as absence.
        // The value itself decides nothing; `client_port` does.
        row.try_get::<&str>("backend_type")?.ok_or_else(|| {
            DbError::BadRow("a PostgreSQL backend reported no backend_type".into())
        })?;
        let client = row.try_get::<&str>("client")?.ok_or_else(|| {
            DbError::BadRow("a PostgreSQL backend reported no client port".into())
        })?;
        let mine = row.try_get::<&str>("own")? == Some("true");
        if mine && own.replace(key.to_owned()).is_some() {
            return Err(DbError::BadRow(
                "PostgreSQL reported this backend pid more than once".into(),
            ));
        }
        if client == "true" {
            clients.push(key.to_owned());
        }
    }
    let own = own.ok_or_else(|| {
        DbError::BadRow("PostgreSQL did not report this connection's own backend".into())
    })?;
    if !clients.contains(&own) {
        return Err(DbError::BadRow(
            "PostgreSQL did not report this connection as a client of its own".into(),
        ));
    }
    clients.sort();
    Ok(SessionInventory {
        own,
        clients,
        counter,
    })
}

/// Creates only this run's own resources. `CREATE DATABASE` cannot run inside
/// a transaction block, so each statement is separate and the caller removes
/// whatever was created when a later one fails.
pub async fn create_scratch(
    conn: &mut StreamConn,
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
pub async fn drop_roles(conn: &mut StreamConn, roles: &[String]) -> Vec<String> {
    let mut failed = Vec::new();
    for role in roles {
        let statement = format!("DROP ROLE IF EXISTS \"{}\"", role.replace('"', "\"\""));
        if conn.execute(&statement).await.is_err() {
            failed.push(role.clone());
        }
    }
    failed
}

pub async fn drop_scratch(conn: &mut StreamConn, names: &ScratchNames) -> Result<(), DbError> {
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
