//! Reviewed target observations. SQL Server has no read-only transaction;
//! effective privileges, not query framing, are the target boundary (ADR-0019 §7).
pub use pbps_db::query::AssertionResult;
use pbps_db::{Conn, DbError, Driver};
use std::time::Duration;
mod frame;

/// Actual database identity plus the planning principal, not endpoint spelling.
#[derive(Debug)]
pub struct Target {
    cluster: String,
    database: crate::resolver::scratch::Database,
    login: String,
}
impl Target {
    pub async fn read(conn: &mut Conn) -> Result<Self, DbError> {
        if conn.driver() != Driver::Mssql {
            return Err(DbError::Refused(
                "SQL Server assertion target requires SQL Server".into(),
            ));
        }
        let cluster = crate::resolver::scratch::cluster_identity(conn).await?;
        let database = crate::resolver::scratch::database(conn).await?;
        let rows = conn
            .query("SELECT ORIGINAL_LOGIN() AS login, SUSER_SNAME() AS actor")
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::BadRow("missing assertion login identity".into()))?;
        let login = row
            .try_get::<&str>("login")?
            .ok_or_else(|| DbError::BadRow("NULL assertion login".into()))?;
        if row.try_get::<&str>("actor")? != Some(login) {
            return Err(DbError::Refused(
                "assertions require an unimpersonated login".into(),
            ));
        }
        Ok(Self {
            cluster,
            database,
            login: login.into(),
        })
    }
}

/// The checked connection is consumed by bounded execution, including on error.
/// Procedure EXECUTE is write-capable even without EXECUTE AS: ownership
/// chaining can write where the caller's direct table permissions cannot.
pub async fn run_target(
    connection: &str,
    expected: &Target,
    sql: &str,
    timeout: Duration,
) -> Result<AssertionResult, DbError> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, async {
        let mut conn = Conn::connect(Driver::Mssql, connection).await?;
        let actual = Target::read(&mut conn).await?;
        if actual.cluster != expected.cluster || actual.database != expected.database {
            return Err(DbError::Refused(
                "PBPS_ASSERTION_CONN does not identify the planning target database".into(),
            ));
        }
        if actual.login == expected.login {
            return Err(DbError::Refused(
                "PBPS_ASSERTION_CONN requires a separate assertion login".into(),
            ));
        }
        privileges(&mut conn).await?;
        if !frame::single_query(sql) {
            return Ok(AssertionResult::InvalidResult {
                message: "an assertion must contain one query, without GO or client commands"
                    .into(),
            });
        }
        Ok(
            match conn
                .query_bounded(
                    sql,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await
            {
                Ok(evidence) => AssertionResult::from_evidence(evidence),
                Err(error) if error.server_error_code().is_some() => {
                    AssertionResult::SqlError(error)
                }
                Err(error) => AssertionResult::Unanswerable(error),
            },
        )
    })
    .await
    .map_err(|_| {
        DbError::Refused("assertion deadline exceeded during target checks or execution".into())
    })?
}

async fn privileges(conn: &mut Conn) -> Result<(), DbError> {
    let rows = conn.query("SELECT DB_NAME() AS name,HAS_PERMS_BY_NAME(NULL,NULL,'VIEW ANY DATABASE') AS databases_visible").await?;
    let database = rows
        .first()
        .ok_or_else(|| DbError::BadRow("missing assertion database".into()))?
        .try_get::<&str>("name")?
        .ok_or_else(|| DbError::BadRow("unreadable assertion database name".into()))?
        .to_owned();
    privileges_here(conn, &database).await?;
    // An accessible database can be hidden from sys.databases without this
    // read permission. Hidden databases cannot count as a confined login.
    if rows
        .first()
        .map(|r| r.try_get::<i32>("databases_visible"))
        .transpose()?
        .flatten()
        != Some(1)
    {
        return Err(DbError::Refused("assertion permission checks require server VIEW ANY DATABASE; hidden databases are not an empty access inventory".into()));
    }
    for row in conn.query("SELECT name,HAS_DBACCESS(name) AS accessible FROM sys.databases WHERE database_id>4 AND database_id<>DB_ID()").await? {
        let name = row.try_get::<&str>("name")?.ok_or_else(|| DbError::BadRow("unreadable assertion database name".into()))?;
        match row.try_get::<i32>("accessible")? {
            Some(0) => {},
            Some(1) => return Err(DbError::Refused(format!("assertion login can access other user database {}; require a separate login confined to the planning database", pbps_model::escape::Escaped(name)))),
            _ => return Err(DbError::BadRow(format!("unreadable assertion access to database {}", pbps_model::escape::Escaped(name)))),
        }
    }
    // A default mapping is hidden without server VIEW ANY DEFINITION. Only
    // a complete answer can establish that a visible server has no usable
    // mapping; an own-principal mapping already proves the capability.
    for row in conn.query("SELECT s.name,HAS_PERMS_BY_NAME(NULL,NULL,'VIEW ANY DEFINITION') AS visible,CASE WHEN EXISTS(SELECT 1 FROM sys.linked_logins l WHERE l.server_id=s.server_id AND l.local_principal_id IN (0,SUSER_ID())) THEN 1 ELSE 0 END AS mapped FROM sys.servers s WHERE s.is_linked=1").await? {
        let name = row.try_get::<&str>("name")?.ok_or_else(|| DbError::BadRow("unreadable assertion linked-server name".into()))?;
        let mapped = row.try_get::<i32>("mapped")?;
        if mapped == Some(1) {
            return Err(DbError::Refused(format!("assertion login has a usable mapping for linked server {}; require a separate login without linked-server mappings", pbps_model::escape::Escaped(name))));
        }
        if mapped != Some(0) || row.try_get::<i32>("visible")? != Some(1) {
            return Err(DbError::Refused(format!("assertion permission checks require server VIEW ANY DEFINITION to read all mappings for linked server {}; hidden mappings are not an empty inventory", pbps_model::escape::Escaped(name))));
        }
    }
    // Guest access is not a refusal. Audit the same effective capabilities
    // wherever the login can enter a system database, then restore the target
    // before user SQL. A failed audit consumes this connection without SQL.
    for row in conn.query("SELECT name,HAS_DBACCESS(name) AS accessible FROM sys.databases WHERE database_id BETWEEN 1 AND 4 AND database_id<>DB_ID()").await? {
        let name = row.try_get::<&str>("name")?.ok_or_else(|| DbError::BadRow("unreadable assertion system database name".into()))?;
        match row.try_get::<i32>("accessible")? {
            Some(0) => continue,
            Some(1) => {},
            _ => return Err(DbError::BadRow(format!("unreadable assertion access to system database {}",pbps_model::escape::Escaped(name)))),
        }
        let quoted = crate::ident::quote(name).map_err(|e| DbError::Refused(e.to_string()))?;
        conn.execute(&format!("USE {quoted}")).await?;
        privileges_here(conn, name).await?;
    }
    let quoted = crate::ident::quote(&database).map_err(|e| DbError::Refused(e.to_string()))?;
    conn.execute(&format!("USE {quoted}")).await?;
    Ok(())
}

async fn privileges_here(conn: &mut Conn, database: &str) -> Result<(), DbError> {
    // A SELECT grant can hide every unrelated object. Demand complete metadata
    // before an empty permission inventory could mean "no write capability".
    let rows = conn
        .query("SELECT HAS_PERMS_BY_NAME(DB_NAME(),'DATABASE','VIEW DEFINITION') AS visible")
        .await?;
    if rows
        .first()
        .map(|r| r.try_get::<i32>("visible"))
        .transpose()?
        .flatten()
        != Some(1)
    {
        return Err(DbError::Refused(format!(
            "assertion permission checks require database VIEW DEFINITION on {}; hidden objects are not an empty permission inventory",
            pbps_model::escape::Escaped(database)
        )));
    }
    // Ownership-chained T-SQL functions and views can call CLR without a
    // direct CLR grant. SAFE/CAS is not a boundary under CLR strict security.
    // Complete database metadata above makes an empty assembly read conclusive.
    if let Some(row) = conn
        .query("SELECT TOP(1) name FROM sys.assemblies WHERE is_user_defined=1 ORDER BY name")
        .await?
        .first()
    {
        let name = row
            .try_get::<&str>("name")?
            .ok_or_else(|| DbError::BadRow("unreadable assertion CLR assembly name".into()))?;
        return Err(DbError::Refused(format!(
            "assertion target contains user-defined CLR assembly {} in database {}; ownership-chained wrappers can execute CLR without direct grants",
            pbps_model::escape::Escaped(name),
            pbps_model::escape::Escaped(database)
        )));
    }
    let safe = "'SELECT','EXECUTE','CONNECT','VIEW DEFINITION','VIEW SECURITY DEFINITION','VIEW PERFORMANCE DEFINITION','VIEW DATABASE STATE','VIEW DATABASE SECURITY STATE','VIEW DATABASE PERFORMANCE STATE','VIEW CHANGE TRACKING','VIEW ANY COLUMN ENCRYPTION KEY DEFINITION','VIEW ANY COLUMN MASTER KEY DEFINITION','UNMASK','SHOWPLAN','CONNECT SQL','VIEW ANY DATABASE','VIEW ANY DEFINITION','VIEW ANY SECURITY DEFINITION','VIEW ANY PERFORMANCE DEFINITION','VIEW SERVER STATE','VIEW SERVER SECURITY STATE','VIEW SERVER PERFORMANCE STATE','SELECT ALL USER SECURABLES'";
    let mut parts = vec![
        "SELECT permission_name, 'SERVER' AS object FROM sys.fn_my_permissions(NULL,'SERVER')"
            .to_owned(),
        "SELECT permission_name, DB_NAME() FROM sys.fn_my_permissions(NULL,'DATABASE')".into(),
    ];
    // Self-principal CONTROL/ALTER/IMPERSONATE are implicit for ordinary logins;
    // they do not impersonate another identity or confer target write rights.
    // A permission on a server securable makes that securable visible. A complete
    // fn_my_permissions answer with zero rows is genuinely no effective right,
    // unlike an unreadable catalog query, which still aborts admission.
    for (class, inventory) in [
        (
            "LOGIN",
            "SELECT QUOTENAME(name) AS name FROM sys.server_principals WHERE type IN ('S','U','G','E','X') AND principal_id<>SUSER_ID()",
        ),
        (
            "SERVER ROLE",
            "SELECT QUOTENAME(name) AS name FROM sys.server_principals WHERE type='R'",
        ),
        (
            "ENDPOINT",
            "SELECT QUOTENAME(name) AS name FROM sys.endpoints",
        ),
        (
            "AVAILABILITY GROUP",
            "SELECT QUOTENAME(name) AS name FROM sys.availability_groups",
        ),
        (
            "CREDENTIAL",
            "SELECT QUOTENAME(name) AS name FROM sys.credentials",
        ),
    ] {
        parts.push(format!("SELECT permission_name, ('{class} '+i.name) COLLATE DATABASE_DEFAULT FROM ({inventory}) i CROSS APPLY sys.fn_my_permissions(i.name,'{class}') p"));
    }
    // Catalog names can use different collations (e.g. external languages);
    // normalize only these diagnostic labels, never the queried object names.
    // Each namespace's own oracle accounts for inherited permissions, ownership
    // and DENY. Reading raw grant rows or role names would not do that.
    for (class, inventory) in [
        ("SCHEMA", "SELECT QUOTENAME(name) AS name FROM sys.schemas"),
        // S/IT are not OBJECT securables; legacy DEFAULT/RULE report nominal
        // rights even when the engine denies DROP/GRANT (native 3701/4606).
        // Their real DDL capability is on the schema/parent; table types use
        // the TYPE inventory, not OBJECT. Session temporary objects are not
        // permanent tempdb writes. System procedures have a separate catalog;
        // their EXECUTE check below excludes system catalog views.
        (
            "OBJECT",
            "SELECT QUOTENAME(SCHEMA_NAME(schema_id))+'.'+QUOTENAME(name) AS name,type FROM sys.objects WHERE type NOT IN ('S','IT','D','R','TT') AND (DB_ID()<>2 OR name NOT LIKE '#%')",
        ),
        (
            "TYPE",
            "SELECT QUOTENAME(SCHEMA_NAME(schema_id))+'.'+QUOTENAME(name) AS name FROM sys.types WHERE is_user_defined=1",
        ),
        (
            "XML SCHEMA COLLECTION",
            "SELECT QUOTENAME(SCHEMA_NAME(schema_id))+'.'+QUOTENAME(name) AS name FROM sys.xml_schema_collections WHERE xml_collection_id>1",
        ),
        (
            "ASSEMBLY",
            "SELECT QUOTENAME(name) AS name FROM sys.assemblies WHERE is_user_defined=1",
        ),
        (
            "USER",
            "SELECT QUOTENAME(name) AS name FROM sys.database_principals WHERE type IN ('S','U','G','E','X') AND principal_id<>USER_ID()",
        ),
        (
            "ROLE",
            "SELECT QUOTENAME(name) AS name FROM sys.database_principals WHERE type='R' AND is_fixed_role=0 AND principal_id>4",
        ),
        (
            "APPLICATION ROLE",
            "SELECT QUOTENAME(name) AS name FROM sys.database_principals WHERE type='A'",
        ),
        (
            "CERTIFICATE",
            "SELECT QUOTENAME(name) AS name FROM sys.certificates",
        ),
        (
            "ASYMMETRIC KEY",
            "SELECT QUOTENAME(name) AS name FROM sys.asymmetric_keys",
        ),
        (
            "SYMMETRIC KEY",
            "SELECT QUOTENAME(name) AS name FROM sys.symmetric_keys",
        ),
        (
            "CONTRACT",
            "SELECT QUOTENAME(name) AS name FROM sys.service_contracts WHERE service_contract_id>65535",
        ),
        (
            "MESSAGE TYPE",
            "SELECT QUOTENAME(name) AS name FROM sys.service_message_types WHERE message_type_id>65535",
        ),
        (
            "SERVICE",
            "SELECT QUOTENAME(name) AS name FROM sys.services WHERE service_id>65535",
        ),
        (
            "REMOTE SERVICE BINDING",
            "SELECT QUOTENAME(name) AS name FROM sys.remote_service_bindings",
        ),
        (
            "ROUTE",
            "SELECT QUOTENAME(name) AS name FROM sys.routes WHERE route_id>65535",
        ),
        (
            "FULLTEXT CATALOG",
            "SELECT QUOTENAME(name) AS name FROM sys.fulltext_catalogs",
        ),
        (
            "FULLTEXT STOPLIST",
            "SELECT QUOTENAME(name) AS name FROM sys.fulltext_stoplists",
        ),
        (
            "SEARCH PROPERTY LIST",
            "SELECT QUOTENAME(name) AS name FROM sys.registered_search_property_lists",
        ),
        (
            "DATABASE SCOPED CREDENTIAL",
            "SELECT QUOTENAME(name) AS name FROM sys.database_scoped_credentials",
        ),
        (
            "EXTERNAL LANGUAGE",
            "SELECT QUOTENAME(language) AS name FROM sys.external_languages WHERE principal_id IS NOT NULL",
        ),
        (
            "EXTERNAL LIBRARY",
            "SELECT QUOTENAME(name) AS name FROM sys.external_libraries",
        ),
    ] {
        // Procedure EXECUTE can persist writes through ownership chaining.
        // The oracle includes inherited rights and DENY, not just grant rows.
        let permission = if class == "OBJECT" {
            "CASE WHEN p.permission_name='EXECUTE' AND i.type IN ('P','PC','X','RF') THEN 'EXECUTE (procedure)' ELSE p.permission_name END"
        } else {
            "p.permission_name"
        };
        parts.push(format!("SELECT {permission}, ('{class} '+i.name) COLLATE DATABASE_DEFAULT FROM ({inventory}) i OUTER APPLY sys.fn_my_permissions(i.name,'{class}') p"));
    }
    // System procedures live outside sys.objects. Ask the engine for the one
    // capability being refused instead of enumerating every permission for
    // thousands of built-ins. NULL remains an unreadable inventory answer.
    parts.push("SELECT CASE WHEN permission=1 THEN 'EXECUTE (procedure)' END AS permission_name, ('OBJECT '+name) COLLATE DATABASE_DEFAULT FROM (SELECT QUOTENAME(SCHEMA_NAME(schema_id))+'.'+QUOTENAME(name) AS name,HAS_PERMS_BY_NAME(QUOTENAME(SCHEMA_NAME(schema_id))+'.'+QUOTENAME(name),'OBJECT','EXECUTE') AS permission FROM sys.system_objects WHERE type IN ('P','PC','X','RF')) i WHERE permission IS NULL OR permission<>0".into());
    let sql = format!(
        "SELECT TOP(1) permission_name,object FROM ({}) p WHERE permission_name IS NULL OR permission_name NOT IN ({safe})",
        parts.join(" UNION ALL ")
    );
    if let Some(row) = conn.query(&sql).await?.first() {
        let object = row
            .try_get::<&str>("object")?
            .ok_or_else(|| DbError::BadRow("unreadable assertion privilege object".into()))?;
        let permission = row.try_get::<&str>("permission_name")?.ok_or_else(|| {
            DbError::BadRow(format!(
                "unreadable assertion privilege on {} in database {}",
                pbps_model::escape::Escaped(object),
                pbps_model::escape::Escaped(database)
            ))
        })?;
        return Err(DbError::Refused(format!(
            "assertion login has {} permission on {} in database {}; require a separate login without write or DDL permissions",
            pbps_model::escape::Escaped(permission),
            pbps_model::escape::Escaped(object),
            pbps_model::escape::Escaped(database)
        )));
    }
    Ok(())
}
