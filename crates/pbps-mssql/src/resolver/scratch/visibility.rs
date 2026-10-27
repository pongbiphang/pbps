//! A retained, database-bounded repair capability (DEC-2314.1).
//!
//! Only visibility inputs are restored here. Object cleanup still runs as
//! the original sandbox, through the ordinary inventory and DROP ordering.

use super::{Database, SessionMark, database, ident, literal, one, text};
use pbps_db::DbError;
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use std::collections::BTreeSet;

/// Created before untrusted SQL, on the connection retained for release.
/// Its private guard binds both the connection and the original principal.
pub struct Visibility {
    guard: String,
    baseline: BTreeSet<Permission>,
    sandbox: String,
    database: Database,
    databases: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Permission {
    permission: String,
    on: String,
    grantee: String,
    grantor: String,
    state: String,
}

impl Permission {
    fn statement(&self, remove: bool) -> String {
        let verb = match (remove, self.state.as_str()) {
            (true, _) | (false, "R") => "REVOKE",
            (false, "D") => "DENY",
            _ => "GRANT",
        };
        format!(
            "{verb} {}{} {} {}{} AS {};",
            self.permission,
            self.on,
            if verb == "REVOKE" { "FROM" } else { "TO" },
            ident(&self.grantee),
            if remove {
                " CASCADE"
            } else if self.state == "W" {
                " WITH GRANT OPTION"
            } else {
                ""
            },
            ident(&self.grantor),
        )
    }
}

/// This is deliberately not the authorization replay rule. It includes only
/// permissions that can hide metadata/catalogs from the cleanup reader.
/// Accepted pre-file state contains no user objects or user-defined roles.
async fn permissions(conn: &mut impl QueryConnection) -> Result<BTreeSet<Permission>, DbError> {
    let rows = conn
        .query(
            "SELECT p.class,p.permission_name,CONVERT(nvarchar(1),p.state) AS state,\
         USER_NAME(p.grantee_principal_id) AS grantee,\
         USER_NAME(p.grantor_principal_id) AS grantor,\
         OBJECT_SCHEMA_NAME(p.major_id) AS schema_name,\
         OBJECT_NAME(p.major_id) AS object_name,COL_NAME(p.major_id,p.minor_id) AS column_name,\
         SCHEMA_NAME(p.major_id) AS on_schema FROM sys.database_permissions p WHERE \
         (p.class IN(0,3) AND p.permission_name IN(N'CONTROL',N'SELECT',N'VIEW DEFINITION',\
          N'VIEW SECURITY DEFINITION',N'VIEW PERFORMANCE DEFINITION')) OR \
         (p.class=1 AND (p.permission_name IN(N'CONTROL',N'VIEW DEFINITION') OR \
          (p.permission_name=N'SELECT' AND OBJECTPROPERTYEX(p.major_id,N'IsMSShipped')=1 \
           AND OBJECTPROPERTYEX(p.major_id,N'IsView')=1)));",
        )
        .await?;
    let mut result = BTreeSet::new();
    for row in rows {
        let permission = text(&row, "permission_name")?;
        if !matches!(
            permission.as_str(),
            "CONTROL"
                | "SELECT"
                | "VIEW DEFINITION"
                | "VIEW SECURITY DEFINITION"
                | "VIEW PERFORMANCE DEFINITION"
        ) {
            return Err(DbError::BadRow(
                "unknown cleanup visibility permission".into(),
            ));
        }
        let state = text(&row, "state")?;
        if !matches!(state.as_str(), "G" | "W" | "D" | "R") {
            return Err(DbError::BadRow("unknown cleanup visibility state".into()));
        }
        let on = match row.try_get::<u8>("class")? {
            Some(0) => String::new(),
            Some(3) => format!(" ON SCHEMA::{}", ident(&text(&row, "on_schema")?)),
            Some(1) => format!(
                " ON OBJECT::{}.{}{}",
                ident(&text(&row, "schema_name")?),
                ident(&text(&row, "object_name")?),
                row.try_get::<&str>("column_name")?
                    .map(|column| format!(" ({})", ident(column)))
                    .unwrap_or_default(),
            ),
            _ => return Err(DbError::BadRow("unknown cleanup visibility class".into())),
        };
        result.insert(Permission {
            permission,
            on,
            grantee: text(&row, "grantee")?,
            grantor: text(&row, "grantor")?,
            state,
        });
    }
    Ok(result)
}

impl Visibility {
    /// The caller owns this dedicated connection. It must never run file SQL
    /// on it or reconnect it after this irreversible transition.
    pub async fn retain(
        conn: &mut impl ExecuteConnection,
        mark: &SessionMark,
        sandbox: &str,
        databases: &[String],
    ) -> Result<Self, DbError> {
        if !super::marked(conn, mark).await? || database(conn).await? != mark.database {
            return Err(DbError::Refused(
                "visibility baseline reached another database".into(),
            ));
        }
        super::enter_sandbox(conn, sandbox).await?;
        super::sandbox_checks(conn, databases, &mark.database.name).await?;
        super::super::authorization::enter_cleanup_owner(
            conn,
            &super::super::authorization::SandboxScope {
                sandbox,
                scratch: &mark.database.name,
                databases,
            },
        )
        .await?;
        let principal = one(
            conn.query(&format!(
                "SELECT CONVERT(nvarchar(12),principal_id) AS id,\
             CONVERT(nvarchar(170),sid,2) AS sid,\
             CONVERT(nvarchar(33),create_date,126) AS created FROM sys.database_principals \
             WHERE name COLLATE Latin1_General_BIN2={} COLLATE Latin1_General_BIN2;",
                literal(sandbox),
            ))
            .await?,
            "the retained sandbox identity",
        )?;
        let owner = one(
            conn.query(
                "SELECT CONVERT(nvarchar(170),owner_sid,2) AS sid FROM sys.databases \
             WHERE database_id=DB_ID();",
            )
            .await?,
            "the retained database owner",
        )?;
        let backend = super::backend(conn).await?;
        let guard = format!(
            "IF USER_NAME() IS NULL OR USER_NAME() COLLATE Latin1_General_BIN2<>N'dbo' \
             COLLATE Latin1_General_BIN2 OR DB_NAME() COLLATE Latin1_General_BIN2<>{db} \
             COLLATE Latin1_General_BIN2 OR OBJECT_ID({marker}) IS NULL OR NOT EXISTS \
             (SELECT 1 FROM sys.dm_exec_sessions WHERE session_id=@@SPID AND \
              CONVERT(nvarchar(12),session_id)={session} AND \
              CONVERT(nvarchar(33),login_time,126)={started}) OR NOT EXISTS \
             (SELECT 1 FROM sys.databases WHERE database_id=DB_ID() AND \
              is_trustworthy_on=0 AND CONVERT(nvarchar(170),owner_sid,2)={owner}) OR NOT EXISTS \
             (SELECT 1 FROM sys.database_recovery_status WHERE database_id=DB_ID() AND \
              CONVERT(nvarchar(36),database_guid)={guid}) OR NOT EXISTS \
             (SELECT 1 FROM sys.database_principals WHERE \
              name COLLATE Latin1_General_BIN2={sandbox} COLLATE Latin1_General_BIN2 AND \
              CONVERT(nvarchar(12),principal_id)={id} AND CONVERT(nvarchar(170),sid,2)={sid} \
              AND CONVERT(nvarchar(33),create_date,126)={created}) \
             THROW 51000,N'cleanup visibility identity or TRUSTWORTHY changed',1;",
            db = literal(&mark.database.name),
            marker = literal(&format!("tempdb..{}", mark.name)),
            session = literal(&backend.session),
            started = literal(&backend.started),
            owner = literal(&text(&owner, "sid")?),
            guid = literal(&mark.database.guid),
            sandbox = literal(sandbox),
            id = literal(&text(&principal, "id")?),
            sid = literal(&text(&principal, "sid")?),
            created = literal(&text(&principal, "created")?),
        );
        conn.execute(&guard).await?;
        let baseline = permissions(conn).await?;
        Ok(Self {
            guard,
            baseline,
            sandbox: sandbox.into(),
            database: mark.database.clone(),
            databases: databases.to_vec(),
        })
    }

    async fn statement(&self, conn: &mut impl ExecuteConnection, sql: &str) -> Result<(), DbError> {
        // A file-created DDL trigger runs inside the bounded dbo context.
        // Verify again afterwards before permitting the next mutation.
        conn.execute(&format!("{} {sql} {}", self.guard, self.guard))
            .await
    }

    /// Consumes the repair capability before irreversibly returning to the
    /// original sandbox. Failure never falls back to provisioning-login DDL.
    pub async fn restore(self, conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
        conn.execute(&self.guard).await?;
        super::context_checks(conn, &self.databases, &self.database.name, true).await?;
        loop {
            let current = permissions(conn).await?;
            let Some(extra) = current.difference(&self.baseline).next() else {
                break;
            };
            self.statement(conn, &extra.statement(true)).await?;
            if permissions(conn).await?.difference(&self.baseline).count()
                >= current.difference(&self.baseline).count()
            {
                return Err(DbError::Refused(
                    "cleanup visibility revoke made no progress".into(),
                ));
            }
        }
        // Removing a grant option can cascade through other baseline rows.
        // Retry only this recorded set until its grantors have their grants.
        loop {
            let current = permissions(conn).await?;
            let missing: Vec<_> = self.baseline.difference(&current).collect();
            if missing.is_empty() {
                break;
            }
            let missing_count = missing.len();
            let mut failure = None;
            for permission in missing {
                if let Err(error) = self.statement(conn, &permission.statement(false)).await {
                    failure = Some(error);
                }
            }
            if self.baseline.difference(&permissions(conn).await?).count() >= missing_count {
                return Err(failure.unwrap_or_else(|| {
                    DbError::Refused("cleanup visibility restore made no progress".into())
                }));
            }
        }
        let memberships = conn
            .query(&format!(
                "SELECT r.name FROM sys.database_role_members m JOIN sys.database_principals r \
             ON r.principal_id=m.role_principal_id WHERE m.member_principal_id=USER_ID({});",
                literal(&self.sandbox),
            ))
            .await?;
        let mut owner = false;
        for row in memberships {
            let role = text(&row, "name")?;
            if role == "db_owner" {
                owner = true;
            } else {
                self.statement(
                    conn,
                    &format!(
                        "ALTER ROLE {} DROP MEMBER {};",
                        ident(&role),
                        ident(&self.sandbox)
                    ),
                )
                .await?;
            }
        }
        if !owner {
            self.statement(
                conn,
                &format!("ALTER ROLE [db_owner] ADD MEMBER {};", ident(&self.sandbox)),
            )
            .await?;
        }
        if permissions(conn).await? != self.baseline {
            return Err(DbError::Refused(
                "cleanup visibility differs from its recorded baseline".into(),
            ));
        }
        conn.execute(&self.guard).await?;
        super::context_checks(conn, &self.databases, &self.database.name, true).await?;
        super::enter_sandbox(conn, &self.sandbox).await?;
        super::sandbox_checks(conn, &self.databases, &self.database.name).await?;
        Ok(())
    }
}
