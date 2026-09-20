//! Analysis-scope catalog facts for SQL Server (ADR-0016 cases 5, 23; SPEC
//! §9.3.3): what one connection can read with SQL about the server, the
//! database and the session a deployment statement runs in. The executables
//! the engine's processes run are not here; the CLI reads those.
//!
//! Every read is this connection's own view. Read through the deployment
//! login it is the deployer's view, which is the one that matters: a
//! session's effective language, date format and SET options come from the
//! login and the driver, not from the server's defaults (measured on 17.0: a
//! login created `WITH DEFAULT_LANGUAGE = [Deutsch]` reads `Deutsch`, `dmy`
//! and `@@DATEFIRST = 1` where `sa` reads `us_english`, `mdy` and `7`).
//!
//! SQL Server's catalog views are not a snapshot and no isolation level makes
//! them one, so [`super::scope_facts`] brackets the read instead of wrapping
//! it in a transaction.

use pbps_db::DbError;
use pbps_db::resolver::Observation;
use pbps_db::resolver::environment::{CatalogFacts, ExtensionFact, SettingFact};
use pbps_db::transport::QueryConnection;
use std::collections::BTreeMap;

/// The schemas a scope covers. SQL Server has no search path: a bare name in
/// a module binds in the module's own schema and then in `dbo`
/// (`Dialect::bare_name_rank`), so there are no write-path extras to carry.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub schemas: &'a [String],
}

/// Server, product and database facts: one row, every column a string.
/// `containment` and the ANSI database options are here because they are the
/// session's fallback when a driver does not set the option itself, and
/// because `ANSI_PADDING` and `ANSI_NULL_DEFAULT` are persisted into the
/// columns a statement creates.
const DATABASE: &str = "\
SELECT CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')) AS product_version,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductLevel')) AS product_level,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductUpdateLevel')) AS product_update_level,
       CONVERT(nvarchar(128), SERVERPROPERTY('ProductUpdateReference')) AS product_update_reference,
       CONVERT(nvarchar(128), SERVERPROPERTY('Edition')) AS edition,
       CONVERT(nvarchar(128), SERVERPROPERTY('EngineEdition')) AS engine_edition,
       CONVERT(nvarchar(128), SERVERPROPERTY('Collation')) AS server_collation,
       CONVERT(nvarchar(128), d.collation_name) AS database_collation,
       CONVERT(nvarchar(128), d.compatibility_level) AS database_compatibility_level,
       CONVERT(nvarchar(128), d.containment_desc) AS database_containment,
       CONVERT(nvarchar(8), d.is_ansi_null_default_on) AS database_ansi_null_default,
       CONVERT(nvarchar(8), d.is_ansi_nulls_on) AS database_ansi_nulls,
       CONVERT(nvarchar(8), d.is_ansi_padding_on) AS database_ansi_padding,
       CONVERT(nvarchar(8), d.is_ansi_warnings_on) AS database_ansi_warnings,
       CONVERT(nvarchar(8), d.is_arithabort_on) AS database_arithabort,
       CONVERT(nvarchar(8), d.is_concat_null_yields_null_on) AS database_concat_null_yields_null,
       CONVERT(nvarchar(8), d.is_numeric_roundabort_on) AS database_numeric_roundabort,
       CONVERT(nvarchar(8), d.is_quoted_identifier_on) AS database_quoted_identifier
FROM sys.databases d WHERE d.database_id = DB_ID();";

pub(crate) const DATABASE_FIELDS: &[&str] = &[
    "product_version",
    "product_level",
    "product_update_level",
    "product_update_reference",
    "edition",
    "engine_edition",
    "server_collation",
    "database_collation",
    "database_compatibility_level",
    "database_containment",
    "database_ansi_null_default",
    "database_ansi_nulls",
    "database_ansi_padding",
    "database_ansi_warnings",
    "database_arithabort",
    "database_concat_null_yields_null",
    "database_numeric_roundabort",
    "database_quoted_identifier",
];

/// The operating system under the engine. Its own statement so that a
/// refusal leaves the rest of the read standing as an `Unknown` fact instead
/// of failing it. Measured on 17.0: any login reads this view, even one
/// denied both `VIEW SERVER STATE` and `VIEW SERVER PERFORMANCE STATE`, so
/// the refusal is for a server that answers differently, reported with the
/// engine's own words rather than a guess at the missing grant.
const HOST: &str =
    "SELECT CONVERT(nvarchar(128), host_platform) AS host_platform FROM sys.dm_os_host_info;";

/// The session's effective statement settings, as the engine reports them
/// for this session: the language and the date interpretation that follow
/// from it, and the SET options that are persisted into what a statement
/// creates (`QUOTED_IDENTIFIER` and `ANSI_NULLS` into a module, `ANSI_PADDING`
/// into a column) or that an indexed computed column or view requires.
/// `sys.dm_exec_sessions` shows a login its own session without any grant.
const SESSION: &str = "\
SELECT CONVERT(nvarchar(128), @@LANGUAGE) AS [language],
       CONVERT(nvarchar(8), @@DATEFIRST) AS datefirst,
       (SELECT CONVERT(nvarchar(8), s.date_format) FROM sys.dm_exec_sessions s
         WHERE s.session_id = @@SPID) AS dateformat,
       CONVERT(nvarchar(8), SESSIONPROPERTY('ANSI_NULLS')) AS ansi_nulls,
       CONVERT(nvarchar(8), SESSIONPROPERTY('ANSI_PADDING')) AS ansi_padding,
       CONVERT(nvarchar(8), SESSIONPROPERTY('ANSI_WARNINGS')) AS ansi_warnings,
       CONVERT(nvarchar(8), SESSIONPROPERTY('ARITHABORT')) AS arithabort,
       CONVERT(nvarchar(8), SESSIONPROPERTY('CONCAT_NULL_YIELDS_NULL')) AS concat_null_yields_null,
       CONVERT(nvarchar(8), SESSIONPROPERTY('NUMERIC_ROUNDABORT')) AS numeric_roundabort,
       CONVERT(nvarchar(8), SESSIONPROPERTY('QUOTED_IDENTIFIER')) AS quoted_identifier,
       CONVERT(nvarchar(8), CASE WHEN @@OPTIONS & 1024 = 1024 THEN 1 ELSE 0 END) AS ansi_null_dflt_on;";

/// The session settings the rule compares, in the order they are read.
pub(crate) const SETTINGS: &[&str] = &[
    "language",
    "datefirst",
    "dateformat",
    "ansi_nulls",
    "ansi_padding",
    "ansi_warnings",
    "arithabort",
    "concat_null_yields_null",
    "numeric_roundabort",
    "quoted_identifier",
    "ansi_null_dflt_on",
];

/// User-defined CLR assemblies: code the engine loads into its own process
/// for this database, with its content read from the catalog and hashed by
/// the engine. A name and a version are not identity; the bytes are.
const ASSEMBLIES: &str = "\
SELECT a.name AS name,
       CONVERT(nvarchar(128), a.clr_name) AS clr_name,
       CONVERT(nvarchar(64), a.permission_set_desc) AS permission_set,
       (SELECT CONVERT(nvarchar(64), HASHBYTES('SHA2_256', f.content), 2)
          FROM sys.assembly_files f WHERE f.assembly_id = a.assembly_id AND f.file_id = 1) AS digest
FROM sys.assemblies a WHERE a.is_user_defined = 1 ORDER BY a.name;";

pub async fn read(
    conn: &mut impl QueryConnection,
    scope: &Scope<'_>,
) -> Result<CatalogFacts, DbError> {
    let rows = conn.query(DATABASE).await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "analysis-scope facts expected one current database row".into(),
        ));
    };
    let mut observations = BTreeMap::new();
    for &field in DATABASE_FIELDS {
        observations.insert(
            field.to_owned(),
            Observation::reported(row.try_get::<&str>(field)?),
        );
    }
    // Unreadable is not absent: a login the view refuses leaves the platform
    // unknown, and the rule refuses on it by name.
    let host = match conn.query(HOST).await {
        Ok(rows) => match rows.as_slice() {
            [row] => Observation::reported(row.try_get::<&str>("host_platform")?),
            _ => Observation::Unknown {
                reason: "sys.dm_os_host_info did not report exactly one host".into(),
            },
        },
        Err(error) => Observation::Unknown {
            reason: format!("sys.dm_os_host_info is not readable by this login: {error}"),
        },
    };
    observations.insert("host_platform".to_owned(), host);

    let rows = conn.query(SESSION).await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "analysis-scope facts expected one session settings row".into(),
        ));
    };
    let mut settings = BTreeMap::new();
    for &name in SETTINGS {
        let value = row.try_get::<&str>(name)?.ok_or_else(|| {
            DbError::BadRow(format!(
                "analysis-scope facts did not report the session's {name}"
            ))
        })?;
        settings.insert(
            name.to_owned(),
            SettingFact {
                value: value.to_owned(),
                source: "effective".into(),
                context: "session".into(),
            },
        );
    }

    // Reported through the extension shape: in-process code the database
    // names, with its content digest where a native library path would be.
    let mut extensions = Vec::new();
    for row in conn.query(ASSEMBLIES).await? {
        let text = |field: &str, what: &str| -> Result<String, DbError> {
            row.try_get::<&str>(field)?
                .map(str::to_owned)
                .ok_or_else(|| DbError::BadRow(format!("a CLR assembly did not report {what}")))
        };
        extensions.push(ExtensionFact {
            name: text("name", "its name")?,
            version: text("clr_name", "its CLR name")?,
            schema: text("permission_set", "its permission set")?,
            requires: Vec::new(),
            libraries: vec![text("digest", "its content digest")?.to_ascii_lowercase()],
        });
    }

    let mut visibility = BTreeMap::new();
    for schema in scope.schemas {
        visibility.insert(
            schema.clone(),
            Observation::reported(Some(&binding_schemas(conn, schema).await?)),
        );
    }

    Ok(CatalogFacts {
        observations,
        extensions,
        available_extensions: BTreeMap::new(),
        collations: Vec::new(),
        settings,
        visibility,
    })
}

/// The schemas a bare name inside `schema` binds through, of those that
/// exist: the schema itself and then `dbo`, which is this engine's whole
/// resolution order for a module (measured; `Dialect::bare_name_rank`). Every
/// login sees every schema's name — metadata visibility hides principals and
/// objects, not schemas (measured on 17.0: a login with no permission on a
/// schema still reads its row in `sys.schemas`) — so what differs between two
/// sides is existence, and a schema the scratch database lacks shows here.
/// Rendered as the JSON array the PostgreSQL rule uses for the same fact.
async fn binding_schemas(conn: &mut impl QueryConnection, schema: &str) -> Result<String, DbError> {
    let mut order = vec![schema.to_owned()];
    if !schema.eq_ignore_ascii_case("dbo") {
        order.push("dbo".to_owned());
    }
    let mut present = Vec::new();
    for name in order {
        let rows = conn
            .query(&format!(
                "SELECT CONVERT(nvarchar(8), COUNT(*)) AS found FROM sys.schemas WHERE name = N'{}';",
                name.replace('\'', "''")
            ))
            .await?;
        let [row] = rows.as_slice() else {
            return Err(DbError::BadRow(
                "a schema lookup expected exactly one row".into(),
            ));
        };
        if row.try_get::<&str>("found")? == Some("1") {
            present.push(name);
        }
    }
    Ok(pbps_db::resolver::environment::render_visibility(&present))
}
