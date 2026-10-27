//! Irreversible deployer entry from an already-entered sandbox (DEC-2267.1).

use super::{AuthorizationContext, PrincipalMap, literal, required};
use pbps_db::DbError;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_model::RoleName;

/// The acquisition scope which must still hold after deployer entry.
pub struct SandboxScope<'a> {
    pub sandbox: &'a str,
    pub scratch: &'a str,
    pub databases: &'a [String],
}

/// Only successful irreversible entry constructs this value. In particular,
/// `dbo` never means that the provisioning login may keep executing DDL.
pub struct SandboxDeployer {
    pub(super) name: RoleName,
    pub(super) dbo: bool,
}

impl SandboxDeployer {
    pub fn name(&self) -> &RoleName {
        &self.name
    }

    pub fn is_dbo(&self) -> bool {
        self.dbo
    }
}

async fn trustworthy_off(conn: &mut impl QueryConnection) -> Result<(), DbError> {
    let rows = conn
        .query(
            "SELECT CONVERT(nvarchar(1),is_trustworthy_on) AS trustworthy FROM sys.databases \
         WHERE database_id=DB_ID();",
        )
        .await?;
    if rows.len() != 1 || required(&rows[0], "trustworthy", "TRUSTWORTHY")? != "0" {
        return Err(DbError::Refused(
            "scratch TRUSTWORTHY must report 0 before and after dbo entry".into(),
        ));
    }
    Ok(())
}

/// The cleanup capability uses the same irreversible owner entry as the
/// captured-dbo deployer, without inventing a target authorization context.
pub(crate) async fn enter_cleanup_owner(
    conn: &mut impl ExecuteConnection,
    scope: &SandboxScope<'_>,
) -> Result<SandboxDeployer, DbError> {
    let row = super::super::scratch::one(
        conn.query("SELECT USER_NAME() AS name, DB_NAME() AS db;")
            .await?,
        "bounded dbo entry",
    )?;
    if required(&row, "name", "the sandbox user")? != scope.sandbox
        || required(&row, "db", "the scratch database")? != scope.scratch
    {
        return Err(DbError::Refused(
            "bounded dbo entry requires the original sandbox".into(),
        ));
    }
    trustworthy_off(conn).await?;
    conn.execute("EXECUTE AS USER=N'dbo' WITH NO REVERT;")
        .await?;
    trustworthy_off(conn).await?;
    let entered = SandboxDeployer {
        name: RoleName::from("dbo"),
        dbo: true,
    };
    super::super::scratch::deployer_checks(conn, scope.databases, scope.scratch, &entered).await?;
    Ok(entered)
}

/// Enters the target's database identity and restores the captured session.
/// Both entry kinds use NO REVERT; target dbo uses scratch's own dbo, whose
/// owner exemptions a db_owner member cannot reproduce.
pub async fn enter_sandbox_deployer(
    conn: &mut impl ExecuteConnection,
    map: &PrincipalMap,
    context: &AuthorizationContext,
    facts: &CatalogFacts,
    scope: &SandboxScope<'_>,
) -> Result<SandboxDeployer, DbError> {
    let rows = conn
        .query("SELECT USER_NAME() AS name, DB_NAME() AS db;")
        .await?;
    if rows.len() != 1
        || required(&rows[0], "name", "the sandbox user")? != scope.sandbox
        || required(&rows[0], "db", "the scratch database")? != scope.scratch
    {
        return Err(DbError::Refused(
            "deployer entry requires the already-entered scratch sandbox".into(),
        ));
    }
    let dbo = context.principal.superuser;
    if dbo != (context.principal.effective.raw() == "dbo") {
        return Err(DbError::BadRow(
            "the captured dbo identity contradicts its owner flag".into(),
        ));
    }
    let entered = if dbo {
        enter_cleanup_owner(conn, scope).await?
    } else {
        let name = map
            .deployer(context)
            .ok_or_else(|| DbError::BadRow("no run-local deployer was mapped".into()))?;
        conn.execute(&format!(
            "EXECUTE AS USER = {} WITH NO REVERT;",
            literal(name.raw())
        ))
        .await?;
        let entered = SandboxDeployer { name, dbo: false };
        super::super::scratch::deployer_checks(conn, scope.databases, scope.scratch, &entered)
            .await?;
        entered
    };
    if facts.settings.get("language").map(|setting| &setting.value) != Some(&context.language) {
        return Err(DbError::BadRow(
            "the captured language and authorization context disagree".into(),
        ));
    }
    apply_session_settings(conn, facts).await?;
    Ok(entered)
}

/// Applies only the closed set the reported-facts rule captures, then reads
/// the same values back. Language comes first because it resets date inputs.
pub async fn apply_session_settings(
    conn: &mut impl QueryConnection,
    facts: &CatalogFacts,
) -> Result<(), DbError> {
    for &name in super::super::environment::SETTINGS {
        let value = &facts
            .settings
            .get(name)
            .ok_or_else(|| {
                DbError::BadRow(format!("the captured session setting {name} is absent"))
            })?
            .value;
        let setting = match name {
            "language" => format!("SET LANGUAGE {};", literal(value)),
            "datefirst" if matches!(value.as_str(), "1" | "2" | "3" | "4" | "5" | "6" | "7") => {
                format!("SET DATEFIRST {value};")
            }
            "dateformat"
                if ["mdy", "dmy", "ymd", "ydm", "myd", "dym"].contains(&value.as_str()) =>
            {
                format!("SET DATEFORMAT {value};")
            }
            "datefirst" | "dateformat" => {
                return Err(DbError::BadRow(format!(
                    "invalid captured session {name}: {value}"
                )));
            }
            _ => {
                let option = match value.as_str() {
                    "1" => "ON",
                    "0" => "OFF",
                    _ => {
                        return Err(DbError::BadRow(format!(
                            "invalid captured session {name}: {value}"
                        )));
                    }
                };
                format!("SET {name} {option};")
            }
        };
        // Separate batches matter for QUOTED_IDENTIFIER's parse-time scope.
        conn.query(&setting).await?;
    }
    let got = super::super::environment::session_settings(conn).await?;
    for &name in super::super::environment::SETTINGS {
        if got.get(name).map(|setting| &setting.value)
            != facts.settings.get(name).map(|setting| &setting.value)
        {
            return Err(DbError::Refused(format!(
                "the reproduced session setting {name} differs from the target"
            )));
        }
    }
    Ok(())
}
