//! Resolver support for SQL Server (ADR-0016): advisory discovery, the
//! dedicated-server instance and session reads, and the analysis-scope
//! qualification — environment facts, the versioned compatibility rule and
//! the deployment authorization context (#611). Qualification is not a
//! binding adapter; SQL Server binding stays unimplemented (#619, #620).

pub mod authorization;
pub mod compatibility;
pub mod environment;

use pbps_db::resolver::environment::DatabaseRecipe;
use pbps_db::resolver::{
    Candidate, Discovery, Observation, OwnSession, ScratchNames, SessionCounter, SessionInventory,
};
use pbps_db::transport::StreamConn;
use pbps_db::{Conn, DbError};

/// Reads the analysis-scope catalog facts and the deployment authorization
/// context together, as the connection's own principal, and refuses a read
/// the target moved under.
///
/// SQL Server's catalog views and dynamic management views are not a
/// snapshot, and no isolation level makes them one — a `SNAPSHOT` transaction
/// needs a database option this tool must not turn on, and metadata reads
/// ignore it anyway. So the read is bracketed instead: everything is read
/// twice, and a difference between the two is a scope that was changing
/// while it was read, which would otherwise seal half of one state and half
/// of another. A change made and undone between the two reads is outside
/// what a bracket can see; the requalification on every check re-reads the
/// target against what was sealed, which is where a lasting change shows.
pub async fn scope_facts(
    conn: &mut impl pbps_db::transport::QueryConnection,
    scope: &environment::Scope<'_>,
    authorization_schemas: &[String],
) -> Result<
    (
        pbps_db::resolver::environment::CatalogFacts,
        authorization::AuthorizationContext,
    ),
    DbError,
> {
    let mut reads = Vec::with_capacity(2);
    for _ in 0..2 {
        let authorization = authorization::read(conn, authorization_schemas).await?;
        let catalog = environment::read(conn, scope).await?;
        reads.push((catalog, authorization));
    }
    let second = reads.pop().expect("two reads");
    let first = reads.pop().expect("two reads");
    if first != second {
        return Err(DbError::Refused(
            "the analysis scope changed while it was read; it cannot be sealed as one state".into(),
        ));
    }
    Ok(first)
}

/// A master database GUID may be copied with an installation. Separation is
/// decided using the qualified native runtime as well, never GUID inequality
/// or SERVERPROPERTY('ProcessID'), which is virtualized by the Linux PAL.
pub async fn instance_identity(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<pbps_db::resolver::InstanceObservation, DbError> {
    // `family_guid`, not `database_guid`. Measured on two SQL Server 2025
    // containers from one image: master's `database_guid` is identical in both
    // — it belongs to the image's master template, not to the instance — while
    // `family_guid` differs and survives a restart of the same instance. A key
    // that every instance from an image shares would report two separate
    // servers as one, which is a valid deployment refused as its own target.
    let rows = conn.query("SELECT CONVERT(nvarchar(36), family_guid) AS instance_key FROM master.sys.database_recovery_status WHERE database_id = 1").await?;
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

/// This session's own key, readable by any login. SQL Server's PAL gives no
/// process mapping, so the connection is correlated with the qualified
/// runtime through its relay rather than through a reported pid.
pub async fn own_session(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<OwnSession, DbError> {
    let rows = conn
        // `key` is a reserved word; an unquoted alias is a syntax error.
        .query("SELECT CONVERT(nvarchar(16), @@SPID) AS [key];")
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "SQL Server expected exactly one session row".into(),
        ));
    };
    let key = row
        .try_get::<&str>("key")?
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| DbError::BadRow("SQL Server did not report this session".into()))?;
    Ok(OwnSession {
        key: key.to_owned(),
        process: pbps_db::resolver::BackendProcess::RuntimeOnly,
    })
}

/// The cumulative counter and its origin, as one read.
///
/// Callers use it on its own to close a qualification window: the inventory
/// they took at the start is only as fresh as the last read of this.
pub async fn session_counter(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<SessionCounter, DbError> {
    let rows = conn
        .query(
            "\
SELECT CONVERT(nvarchar(32), (SELECT COUNT(*) FROM sys.dm_os_performance_counters p
            WHERE p.object_name LIKE '%General Statistics%'
              AND p.counter_name = 'Logins/sec')) AS counters,
       CONVERT(nvarchar(32), (SELECT MAX(p.cntr_value) FROM sys.dm_os_performance_counters p
            WHERE p.object_name LIKE '%General Statistics%'
              AND p.counter_name = 'Logins/sec')) AS total,
       CONVERT(nvarchar(64), (SELECT i.sqlserver_start_time FROM sys.dm_os_sys_info i), 126) AS epoch,
       CASE WHEN HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE') = 1
            THEN 'true' ELSE 'false' END AS complete;",
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "SQL Server session counters expected exactly one row".into(),
        ));
    };
    if row.try_get::<&str>("complete")? != Some("true") {
        return Err(DbError::Refused(
            "this scratch login lacks VIEW SERVER STATE; the server's client sessions are unreadable, not absent".into(),
        ));
    }
    // More than one matching counter would make MAX an arbitrary choice.
    if row.try_get::<&str>("counters")? != Some("1") {
        return Err(DbError::BadRow(
            "SQL Server did not report exactly one cumulative login counter".into(),
        ));
    }
    let total = row
        .try_get::<&str>("total")?
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            DbError::BadRow("SQL Server did not report a cumulative login count".into())
        })?;
    let epoch = row
        .try_get::<&str>("epoch")?
        .ok_or_else(|| DbError::BadRow("SQL Server did not report its start time".into()))?
        .to_owned();
    // One server-level counter, cumulative since the start time that is its
    // epoch. Nothing removes part of it the way dropping a database removes a
    // PostgreSQL row, so there is no separate continuity to keep.
    Ok(SessionCounter {
        total,
        epoch,
        continuity: Vec::new(),
    })
}

/// Complete client-session inventory for the dedicated-server exclusion
/// check, with the cumulative login counter that makes a session which opened
/// and closed between two reads visible anyway.
///
/// Measured on SQL Server 2022 for Linux: `@@CONNECTIONS` also counts the
/// engine's internal connections, and rose by eighteen during one
/// `CREATE DATABASE`, so it cannot be compared for equality. The General
/// Statistics `Logins/sec` counter is cumulative despite its name and moved
/// only for actual client logins, including ones that had already
/// disconnected. Without VIEW SERVER STATE neither the counter nor
/// `sys.dm_exec_sessions` is readable; that is unreadable, never idle.
pub async fn client_sessions(
    conn: &mut impl pbps_db::transport::QueryConnection,
) -> Result<SessionInventory, DbError> {
    let counter = session_counter(conn).await?;
    let rows = conn
        .query(
            "\
SELECT CONVERT(nvarchar(16), s.session_id) AS session_key,
       CONVERT(nvarchar(16), s.is_user_process) AS user_process,
       CASE WHEN s.session_id = @@SPID THEN 'true' ELSE 'false' END AS own
FROM sys.dm_exec_sessions s;",
        )
        .await?;
    // Bracket: SQL Server's dynamic management views are not a snapshot, so a
    // session connected for the first counter read and gone by the list would
    // otherwise have its count absorbed silently.
    if session_counter(conn).await? != counter {
        return Err(DbError::Refused(
            "SQL Server's login counter moved while its session list was read; this server is not exclusively this run's".into(),
        ));
    }
    let mut own = None;
    let mut clients = Vec::new();
    for row in &rows {
        let key = row
            .try_get::<&str>("session_key")?
            .ok_or_else(|| DbError::BadRow("a SQL Server session reported no id".into()))?;
        let kind = row.try_get::<&str>("user_process")?.ok_or_else(|| {
            DbError::BadRow("a SQL Server session did not report is_user_process".into())
        })?;
        let mine = row.try_get::<&str>("own")? == Some("true");
        if mine && own.replace(key.to_owned()).is_some() {
            return Err(DbError::BadRow(
                "SQL Server reported this session id more than once".into(),
            ));
        }
        if kind == "1" {
            clients.push(key.to_owned());
        }
    }
    let own = own.ok_or_else(|| {
        DbError::BadRow("SQL Server did not report this connection's own session".into())
    })?;
    if !clients.contains(&own) {
        return Err(DbError::BadRow(
            "SQL Server did not report this connection as a user session".into(),
        ));
    }
    clients.sort();
    Ok(SessionInventory {
        own,
        clients,
        counter,
    })
}

/// Creates only this run's own login and database. Ownership is transferred
/// rather than granting the login rights on anything that already existed,
/// which also makes the run login `dbo` of its scratch database: the context
/// a `dbo` deployer is reproduced by, and the one that may impersonate the
/// run-local users every other deployer is reproduced by.
pub async fn create_scratch(
    conn: &mut StreamConn,
    names: &ScratchNames,
    recipe: &DatabaseRecipe,
) -> Result<(), DbError> {
    conn.execute(&format!(
        "CREATE LOGIN [{}] WITH PASSWORD = '{}', CHECK_POLICY = OFF;",
        names.login(),
        names.password()
    ))
    .await?;
    for statement in scratch_database_ddl(names, recipe)? {
        conn.execute(&statement).await?;
    }
    conn.execute(&format!(
        "ALTER AUTHORIZATION ON DATABASE::[{}] TO [{}];",
        names.database(),
        names.login()
    ))
    .await
}

/// The statements that create the scratch database the way the target's is:
/// its collation and containment at creation, then its compatibility level
/// and the database-level ANSI options. Every value is checked against the
/// shape the engine reports it in before it reaches a statement, because a
/// collation or a level is spliced, not quoted. A recipe without SQL Server
/// settings creates the server's default database, which the compatibility
/// rule then compares like any other.
pub fn scratch_database_ddl(
    names: &ScratchNames,
    recipe: &DatabaseRecipe,
) -> Result<Vec<String>, DbError> {
    let database = names.database();
    let Some(settings) = &recipe.sql_server else {
        return Ok(vec![format!("CREATE DATABASE [{database}];")]);
    };
    let refuse = |what: &str, value: &str| {
        DbError::BadRow(format!(
            "the target reported a {what} this tool will not splice into a statement: {value}"
        ))
    };
    let collation = &settings.collation;
    if collation.is_empty()
        || !collation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(refuse("collation", collation));
    }
    let level = &settings.compatibility_level;
    if level.is_empty() || level.len() > 3 || !level.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refuse("compatibility level", level));
    }
    if !matches!(settings.containment.as_str(), "NONE" | "PARTIAL") {
        return Err(refuse("containment", &settings.containment));
    }
    let mut statements = vec![
        format!(
            "CREATE DATABASE [{database}] CONTAINMENT = {} COLLATE {collation};",
            settings.containment
        ),
        format!("ALTER DATABASE [{database}] SET COMPATIBILITY_LEVEL = {level};"),
    ];
    for (option, on) in &settings.options {
        if !pbps_db::resolver::environment::SQL_SERVER_DATABASE_OPTIONS
            .iter()
            .any(|(_, known)| known == option)
        {
            return Err(refuse("database option", option));
        }
        statements.push(format!(
            "ALTER DATABASE [{database}] SET {option} {};",
            if *on { "ON" } else { "OFF" }
        ));
    }
    Ok(statements)
}

/// Removes exactly the two run-owned objects. SINGLE_USER only rolls back
/// sessions inside this run's own scratch database.
/// SQL Server's run-local principals are users without a login and roles,
/// both database-scoped: they go with the scratch database, so nothing
/// outlives it for this to drop. The signature matches PostgreSQL's, whose
/// roles are cluster-wide, so the router does not branch on more than the
/// engine.
pub async fn drop_roles(_conn: &mut StreamConn, _roles: &[String]) -> Vec<String> {
    Vec::new()
}

pub async fn drop_scratch(conn: &mut StreamConn, names: &ScratchNames) -> Result<(), DbError> {
    let database = conn
        .execute(&format!(
            "IF DB_ID(N'{name}') IS NOT NULL BEGIN ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}]; END;",
            name = names.database()
        ))
        .await;
    let login = conn
        .execute(&format!(
            "IF EXISTS (SELECT 1 FROM sys.server_principals WHERE name = N'{name}') DROP LOGIN [{name}];",
            name = names.login()
        ))
        .await;
    database.and(login)
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
    fn the_scratch_database_is_created_the_way_the_targets_is() {
        use pbps_db::resolver::environment::SqlServerDatabase;
        let names = ScratchNames::new(
            "pbps_scratch_0123456789abcdef".into(),
            "pbps_login_0123456789abcdef".into(),
            "0123456789abcdef0123456789abcdef".into(),
        )
        .unwrap();
        let recipe = |collation: &str, level: &str, containment: &str| DatabaseRecipe {
            sql_server: Some(SqlServerDatabase {
                collation: collation.into(),
                compatibility_level: level.into(),
                containment: containment.into(),
                options: [
                    ("ANSI_PADDING".to_owned(), true),
                    ("ARITHABORT".to_owned(), false),
                ]
                .into_iter()
                .collect(),
            }),
            ..DatabaseRecipe::neutral()
        };
        assert_eq!(
            scratch_database_ddl(&names, &recipe("Latin1_General_100_CS_AS", "160", "NONE"))
                .unwrap(),
            [
                "CREATE DATABASE [pbps_scratch_0123456789abcdef] CONTAINMENT = NONE COLLATE Latin1_General_100_CS_AS;",
                "ALTER DATABASE [pbps_scratch_0123456789abcdef] SET COMPATIBILITY_LEVEL = 160;",
                "ALTER DATABASE [pbps_scratch_0123456789abcdef] SET ANSI_PADDING ON;",
                "ALTER DATABASE [pbps_scratch_0123456789abcdef] SET ARITHABORT OFF;",
            ]
        );
        // The values are spliced, so anything but the shape the engine
        // reports them in is refused, never quoted around.
        for (collation, level, containment) in [
            ("Latin1_General_CI_AS; DROP DATABASE x", "160", "NONE"),
            ("", "160", "NONE"),
            ("Latin1_General_CI_AS", "160; SHUTDOWN", "NONE"),
            ("Latin1_General_CI_AS", "", "NONE"),
            ("Latin1_General_CI_AS", "160", "FULL"),
        ] {
            assert!(
                scratch_database_ddl(&names, &recipe(collation, level, containment)).is_err(),
                "{collation} {level} {containment}"
            );
        }
        let mut odd = recipe("Latin1_General_CI_AS", "160", "NONE");
        odd.sql_server
            .as_mut()
            .unwrap()
            .options
            .insert("TRUSTWORTHY".into(), true);
        assert!(scratch_database_ddl(&names, &odd).is_err());
        // No SQL Server settings is the server's own default database.
        assert_eq!(
            scratch_database_ddl(&names, &DatabaseRecipe::neutral()).unwrap(),
            ["CREATE DATABASE [pbps_scratch_0123456789abcdef];"]
        );
    }

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
