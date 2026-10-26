//! SQL Server's scratch identity and acquisition checks (DEC-2236.1). The
//! CLI holds connections and cleanup state; only this crate supplies SQL.

use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Row};
use pbps_model::escape::Escaped;

pub mod companion;
pub mod inventory;
pub mod options;
pub mod recovery;
mod system;

pub(crate) fn literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}
pub(crate) fn ident(value: &str) -> String {
    format!("[{}]", value.replace(']', "]]"))
}
pub(crate) fn text(row: &Row, column: &str) -> Result<String, DbError> {
    row.try_get::<&str>(column)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("the scratch read returned NULL for {column}")))
}
pub(crate) fn one(mut rows: Vec<Row>, what: &str) -> Result<Row, DbError> {
    if rows.len() != 1 {
        return Err(DbError::BadRow(format!(
            "the scratch did not report exactly one row for {what}"
        )));
    }
    Ok(rows.pop().expect("one row"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    session: String,
    started: String,
}

pub async fn backend(conn: &mut impl QueryConnection) -> Result<Backend, DbError> {
    let row = one(
        conn.query(
            "SELECT CONVERT(nvarchar(12), session_id) AS session, CONVERT(nvarchar(33), \
             login_time, 126) AS started FROM sys.dm_exec_sessions WHERE session_id = @@SPID;",
        )
        .await?,
        "the scratch backend",
    )?;
    Ok(Backend {
        session: text(&row, "session")?,
        started: text(&row, "started")?,
    })
}

pub async fn on_backend(
    conn: &mut impl QueryConnection,
    checked: &Backend,
) -> Result<bool, DbError> {
    Ok(backend(conn).await? == *checked)
}

pub async fn cluster_identity(conn: &mut impl QueryConnection) -> Result<String, DbError> {
    let row = one(
        conn.query(
            "SELECT CONVERT(nvarchar(36), family_guid) AS [identity] FROM \
             master.sys.database_recovery_status WHERE database_id = 1;",
        )
        .await?,
        "master's family_guid",
    )?;
    text(&row, "identity")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub name: String,
    pub id: String,
    guid: String,
}

pub async fn database(conn: &mut impl QueryConnection) -> Result<Database, DbError> {
    let row = one(
        conn.query(
            "SELECT DB_NAME() AS name, CONVERT(nvarchar(12), DB_ID()) AS id, \
             CONVERT(nvarchar(36), database_guid) AS guid FROM sys.database_recovery_status \
             WHERE database_id=DB_ID();",
        )
        .await?,
        "the scratch database identity",
    )?;
    Ok(Database {
        name: text(&row, "name")?,
        id: text(&row, "id")?,
        guid: text(&row, "guid")?,
    })
}

#[derive(Debug, Clone)]
pub struct SessionMark {
    pub database: Database,
    name: String,
    /// The run's token: the marker table is named by it, and so is every
    /// resource the run creates, so a later run can tell from the marker
    /// whether the owner is gone (`recovery`, #2410).
    token: String,
}

impl SessionMark {
    pub fn token(&self) -> &str {
        &self.token
    }
}

pub async fn mark(conn: &mut impl ExecuteConnection, nonce: &str) -> Result<SessionMark, DbError> {
    if nonce.is_empty() || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) || nonce.len() > 64 {
        return Err(DbError::BadRow(
            "a scratch marker requires generated hexadecimal entropy".into(),
        ));
    }
    let database = database(conn).await?;
    let name = format!("##pbps_{nonce}");
    conn.execute(&format!(
        "CREATE TABLE {} (marker bit NOT NULL);",
        ident(&name)
    ))
    .await?;
    Ok(SessionMark {
        database,
        name,
        token: nonce.to_owned(),
    })
}

pub async fn marked(conn: &mut impl QueryConnection, mark: &SessionMark) -> Result<bool, DbError> {
    present(conn, &mark.name).await
}

async fn present(conn: &mut impl QueryConnection, name: &str) -> Result<bool, DbError> {
    let row = one(
        conn.query(&format!(
            "SELECT CONVERT(nvarchar(1), CASE WHEN OBJECT_ID({}) IS NULL THEN 0 ELSE 1 END) \
             AS seen;",
            literal(&format!("tempdb..{name}"))
        ))
        .await?,
        "the scratch marker",
    )?;
    Ok(text(&row, "seen")? == "1")
}

/// The name of a server-wide property a keeper leaves once it has connected
/// (DEC-2410.3); it outlives the session.
const KEPT: &str = "pbps-keeper";

/// Leaves the mark of the keeper of `token`, whose session mark is already made.
pub async fn mark_kept(conn: &mut impl ExecuteConnection, token: &str) -> Result<(), DbError> {
    conn.execute(&format!(
        "EXEC master.sys.sp_addextendedproperty @name = {}, @value = {};",
        literal(KEPT),
        literal(token)
    ))
    .await
}

/// Whether the session that made the marker for `nonce` (see [`mark`]) is still
/// there.
pub async fn marker_present(conn: &mut impl QueryConnection, nonce: &str) -> Result<bool, DbError> {
    present(conn, &format!("##pbps_{nonce}")).await
}

/// Whether a keeper has ever connected to this server. A reader reads this
/// before it reads the session: the mark is written only once the session
/// exists, so a mark read first can only be older than the session read after.
pub async fn kept(conn: &mut impl QueryConnection) -> Result<bool, DbError> {
    Ok(!conn
        .query(&format!(
            "SELECT 1 AS there FROM master.sys.extended_properties WHERE class = 0 \
             AND name = {};",
            literal(KEPT)
        ))
        .await?
        .is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    SeparateInstance,
    SameInstance,
    Target,
}

pub fn placement(target_database: &str, mark: &SessionMark, seen: bool) -> Placement {
    if !seen {
        Placement::SeparateInstance
    } else if target_database == mark.database.id {
        Placement::Target
    } else {
        Placement::SameInstance
    }
}

pub async fn provisions(conn: &mut impl QueryConnection) -> Result<bool, DbError> {
    let row = one(
        conn.query(
            "SELECT CONVERT(nvarchar(1), CASE WHEN HAS_PERMS_BY_NAME(NULL,NULL,N'ALTER ANY \
             LOGIN')=1 AND HAS_PERMS_BY_NAME(NULL,NULL,N'CREATE ANY DATABASE')=1 THEN 1 ELSE \
             0 END) AS allowed;",
        )
        .await?,
        "scratch acquisition privileges",
    )?;
    Ok(text(&row, "allowed")? == "1")
}

pub async fn require_owner(conn: &mut impl QueryConnection) -> Result<(), DbError> {
    let row = one(
        conn.query("SELECT CONVERT(nvarchar(1), IS_MEMBER(N'db_owner')) AS owner;")
            .await?,
        "db_owner membership",
    )?;
    if text(&row, "owner")? != "1" {
        return Err(DbError::Refused(
            "the supplied scratch account must be db_owner to read the whole inventory".into(),
        ));
    }
    Ok(())
}

pub async fn database_names(conn: &mut impl QueryConnection) -> Result<Vec<String>, DbError> {
    let row = one(
        conn.query(
            "SELECT CONVERT(nvarchar(1), HAS_PERMS_BY_NAME(NULL,NULL,N'VIEW ANY DATABASE')) \
             AS visible;",
        )
        .await?,
        "database visibility",
    )?;
    if text(&row, "visible")? != "1" {
        return Err(DbError::Refused(
            "the scratch account lacks VIEW ANY DATABASE; its database list may hide a \
             guest-admitting database; grant VIEW ANY DATABASE"
                .into(),
        ));
    }
    let names = conn
        .query("SELECT name FROM sys.databases ORDER BY name;")
        .await?
        .iter()
        .map(|row| text(row, "name"))
        .collect::<Result<Vec<_>, _>>()?;
    if names.is_empty() {
        return Err(DbError::BadRow("the scratch database list is empty".into()));
    }
    Ok(names)
}

pub async fn use_database(conn: &mut impl ExecuteConnection, name: &str) -> Result<(), DbError> {
    conn.execute(&format!("USE {};", ident(name))).await
}

pub async fn create_sandbox(conn: &mut impl ExecuteConnection, name: &str) -> Result<(), DbError> {
    conn.execute(&format!(
        "CREATE USER {} WITHOUT LOGIN; ALTER ROLE db_owner ADD MEMBER {};",
        ident(name),
        ident(name)
    ))
    .await
}

pub async fn enter_sandbox(conn: &mut impl ExecuteConnection, name: &str) -> Result<(), DbError> {
    conn.execute(&format!(
        "EXECUTE AS USER = {} WITH NO REVERT;",
        literal(name)
    ))
    .await
}

pub async fn sandbox_checks(
    conn: &mut impl ExecuteConnection,
    databases: &[String],
    scratch: &str,
) -> Result<(), DbError> {
    context_checks(conn, databases, scratch, false).await
}

pub(crate) async fn deployer_checks(
    conn: &mut impl ExecuteConnection,
    databases: &[String],
    scratch: &str,
    entered: &super::authorization::SandboxDeployer,
) -> Result<(), DbError> {
    context_checks(conn, databases, scratch, entered.is_dbo()).await
}

fn unreadable_database_access(name: &str) -> String {
    format!("HAS_DBACCESS is unreadable for database {}", Escaped(name))
}

fn guest_database_access(name: &str) -> String {
    format!(
        "database {} admits the scratch sandbox through guest; REVOKE CONNECT \
         FROM guest in that database",
        Escaped(name)
    )
}

async fn context_checks(
    conn: &mut impl ExecuteConnection,
    databases: &[String],
    scratch: &str,
    dbo: bool,
) -> Result<(), DbError> {
    // The instance setting overrides is_db_chaining_on even when the
    // scratch database's own option still reports zero.
    let chaining = async {
        let row = one(
            conn.query(
                "SELECT CONVERT(int,value_in_use) AS value_in_use FROM sys.configurations \
                 WHERE name=N'cross db ownership chaining';",
            )
            .await?,
            "cross db ownership chaining",
        )?;
        row.try_get::<i32>("value_in_use")?
            .ok_or_else(|| DbError::BadRow("value_in_use is NULL".into()))
    }
    .await
    .map_err(|error| {
        DbError::Refused(format!(
            "instance option cross db ownership chaining is unreadable: {error}"
        ))
    })?;
    if chaining != 0 {
        return Err(DbError::Refused(
            "instance option cross db ownership chaining must be 0; EXEC sp_configure \
             'cross db ownership chaining', 0; RECONFIGURE; or use a scratch instance \
             with the option disabled"
                .into(),
        ));
    }
    for name in databases {
        let row = one(
            conn.query(&format!(
                "SELECT CONVERT(nvarchar(1),HAS_DBACCESS({})) AS access;",
                literal(name)
            ))
            .await?,
            "guest admission",
        )?;
        let access =
            text(&row, "access").map_err(|_| DbError::Refused(unreadable_database_access(name)))?;
        if access != "0"
            && name != scratch
            && !["master", "tempdb", "msdb"].contains(&name.as_str())
        {
            return Err(DbError::Refused(guest_database_access(name)));
        }
    }
    system::check(conn, dbo).await
}

/// SQL Server's password policy requires more than the shared hex entropy.
/// The same derivation is used for LOGIN, Docker environment and connection.
pub fn password(entropy: &str) -> String {
    format!("P!9a{entropy}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_refusals_render_database_names_once_while_sql_stays_raw() {
        let name = "na\n\u{1b}\u{202e}\\n\\u{202e}雪']";
        assert_eq!(
            unreadable_database_access(name),
            r"HAS_DBACCESS is unreadable for database na\n\u{1b}\u{202e}\\n\\u{202e}雪']"
        );
        assert_eq!(
            guest_database_access(name),
            r"database na\n\u{1b}\u{202e}\\n\\u{202e}雪'] admits the scratch sandbox through guest; REVOKE CONNECT FROM guest in that database"
        );
        assert_eq!(
            format!("SELECT HAS_DBACCESS({});", literal(name)),
            "SELECT HAS_DBACCESS(N'na\n\u{1b}\u{202e}\\n\\u{202e}雪'']');"
        );
        assert_eq!(ident(name), "[na\n\u{1b}\u{202e}\\n\\u{202e}雪']]]");
        assert_eq!(
            unreadable_database_access("café雪"),
            "HAS_DBACCESS is unreadable for database café雪"
        );
        assert_eq!(
            guest_database_access("café雪"),
            "database café雪 admits the scratch sandbox through guest; REVOKE CONNECT FROM guest in that database"
        );
    }
}
