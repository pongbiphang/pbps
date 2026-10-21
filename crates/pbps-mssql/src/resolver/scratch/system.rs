//! Build-independent guest/public rules for the three system databases
//! (DEC-2236.1). Shipped SELECT/EXECUTE grants are shared instance reach;
//! a permission on an administrator's own module is not.

use super::{ident, literal, text};
use pbps_db::transport::ExecuteConnection;
use pbps_db::{DbError, Row};
use std::collections::BTreeSet;

const ROLE_MEMBERSHIPS: &str = "SELECT USER_NAME(role_principal_id) AS role \
    FROM sys.database_role_members WHERE member_principal_id=USER_ID(N'guest');";

const PERMISSIONS: &str = "SELECT p.class,p.class_desc,p.permission_name,p.state_desc,\
    p.major_id,p.minor_id, USER_NAME(p.grantee_principal_id) AS grantee,\
    OBJECT_SCHEMA_NAME(p.major_id) AS schema_name, OBJECT_NAME(p.major_id) AS object_name,\
    SCHEMA_NAME(p.major_id) AS on_schema, CONVERT(int,\
    OBJECTPROPERTYEX(p.major_id,N'IsMSShipped')) AS shipped FROM sys.database_permissions p \
    WHERE p.grantee_principal_id IN(USER_ID(N'guest'),USER_ID(N'public'));";

async fn read(
    conn: &mut impl ExecuteConnection,
    database: &str,
    catalog: &str,
    query: &str,
) -> Result<Vec<Row>, DbError> {
    // USE stays inside EXEC so each read returns to the sandbox's own
    // database, without ever reverting its NO REVERT impersonation.
    conn.query(&format!(
        "EXEC({});",
        literal(&format!("USE {}; {query}", ident(database)))
    ))
    .await
    .map_err(|error| {
        DbError::Refused(format!(
            "{database} guest/public cannot read sys.{catalog}: {error}"
        ))
    })
}

fn object_rule(
    grantee: &str,
    permission: &str,
    state: &str,
    minor: i32,
    shipped: Option<i32>,
) -> Result<(), &'static str> {
    match shipped {
        None => Err("the object cannot be resolved"),
        Some(1) => {
            if state == "DENY"
                || (state == "GRANT"
                    && grantee == "public"
                    && ["SELECT", "EXECUTE"].contains(&permission)
                    && minor == 0)
            {
                Ok(())
            } else {
                Err("only public's object-level SELECT/EXECUTE grants and narrowing DENYs qualify")
            }
        }
        Some(_) => Err("the object is not ms-shipped"),
    }
}

fn permission(
    row: &Row,
    database: &str,
    expected: &BTreeSet<(String, String)>,
    database_grants: &mut BTreeSet<(String, String)>,
) -> Result<(), DbError> {
    let class = row
        .try_get::<u8>("class")?
        .ok_or_else(|| DbError::BadRow(format!("{database} has an unreadable permission class")))?;
    let grantee = text(row, "grantee")?;
    let permission = text(row, "permission_name")?;
    let state = text(row, "state_desc")?;
    let major = row.try_get::<i32>("major_id")?.ok_or_else(|| {
        DbError::BadRow(format!("{database} has an unreadable permission major_id"))
    })?;
    let minor = row.try_get::<i32>("minor_id")?.ok_or_else(|| {
        DbError::BadRow(format!("{database} has an unreadable permission minor_id"))
    })?;
    let (object, rule) = match class {
        0 => {
            let key = (grantee.clone(), permission.clone());
            let allowed = state == "GRANT" && major == 0 && minor == 0 && expected.contains(&key);
            if allowed {
                database_grants.insert(key);
            }
            (
                format!("DATABASE {database}"),
                if allowed {
                    Ok(())
                } else {
                    Err("the database grant is outside the fixed guest/public permissions")
                },
            )
        }
        1 => {
            let schema = row.try_get::<&str>("schema_name")?;
            let name = row.try_get::<&str>("object_name")?;
            let object = match (schema, name) {
                (Some(schema), Some(name)) => format!("{schema}.{name}"),
                _ => format!("unresolved object major_id {major}"),
            };
            // Hidden system views are absent even from sys.all_objects.
            // The metadata function resolves them inside this database;
            // NULL is a refusal, never an absent permission.
            let rule = object_rule(
                &grantee,
                &permission,
                &state,
                minor,
                row.try_get::<i32>("shipped")?,
            );
            (
                object,
                if schema.is_some() && name.is_some() {
                    rule
                } else {
                    Err("the object name cannot be resolved")
                },
            )
        }
        3 => (
            format!("SCHEMA {}", text(row, "on_schema")?),
            Err("permissions of this class are not allowed"),
        ),
        _ => (
            format!("{} major_id {major}", text(row, "class_desc")?),
            Err("permissions of this class are not allowed"),
        ),
    };
    if let Err(reason) = rule {
        return Err(DbError::Refused(format!(
            "{database} {grantee} {permission} {state} on {object} (minor_id {minor}) \
             refuses: {reason}"
        )));
    }
    Ok(())
}

pub(super) async fn check(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    let expected = BTreeSet::from([
        ("guest".to_owned(), "CONNECT".to_owned()),
        (
            "public".into(),
            "VIEW ANY COLUMN MASTER KEY DEFINITION".into(),
        ),
        (
            "public".into(),
            "VIEW ANY COLUMN ENCRYPTION KEY DEFINITION".into(),
        ),
    ]);
    for database in ["master", "tempdb", "msdb"] {
        if let Some(row) = read(conn, database, "database_role_members", ROLE_MEMBERSHIPS)
            .await?
            .first()
        {
            return Err(DbError::Refused(format!(
                "{database} guest role membership on ROLE {} is not allowed",
                text(row, "role")?
            )));
        }
        let token = read(
            conn,
            database,
            "user_token",
            "SELECT name FROM sys.user_token ORDER BY name;",
        )
        .await?
        .iter()
        .map(|row| text(row, "name"))
        .collect::<Result<Vec<_>, _>>()?;
        if token != ["guest", "public"] {
            return Err(DbError::Refused(format!(
                "{database} guest/public token membership on DATABASE {database} differs: {}",
                token.join(", ")
            )));
        }
        let mut database_grants = BTreeSet::new();
        for row in read(conn, database, "database_permissions", PERMISSIONS).await? {
            permission(&row, database, &expected, &mut database_grants)?;
        }
        if let Some((grantee, permission)) = expected.difference(&database_grants).next() {
            return Err(DbError::Refused(format!(
                "{database} {grantee} {permission} GRANT on DATABASE {database} is missing"
            )));
        }
        let mut owned = BTreeSet::new();
        for catalog in super::inventory::OWNERSHIP_CATALOGS {
            let name = if *catalog == "external_languages" {
                "language"
            } else {
                "name"
            };
            let owner = if *catalog == "database_principals" {
                "owning_principal_id"
            } else {
                "principal_id"
            };
            let query = format!(
                "IF COL_LENGTH(N'sys.{catalog}',N'{owner}') IS NOT NULL EXEC(N'SELECT {name} \
                 AS name,USER_NAME({owner}) AS owner FROM sys.{catalog} WHERE {owner} IN \
                 (SELECT principal_id FROM sys.user_token)');"
            );
            for row in read(conn, database, catalog, &query).await? {
                owned.insert(format!(
                    "{}|{catalog}:{}",
                    text(&row, "owner")?,
                    text(&row, "name")?
                ));
            }
        }
        if owned != BTreeSet::from(["guest|schemas:guest".into()]) {
            return Err(DbError::Refused(format!(
                "{database} guest/public ownership must be only guest's SCHEMA guest: {}",
                owned.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::object_rule;

    #[test]
    fn only_shared_shipped_grants_or_narrowing_denials_qualify() {
        assert!(object_rule("public", "SELECT", "GRANT", 0, Some(1)).is_ok());
        assert!(object_rule("public", "EXECUTE", "GRANT", 0, Some(1)).is_ok());
        for (grantee, permission, state, minor) in [
            ("guest", "EXECUTE", "GRANT", 0),
            ("public", "SELECT", "GRANT", 1),
            ("public", "CONTROL", "GRANT", 0),
            ("public", "SELECT", "GRANT_WITH_GRANT_OPTION", 0),
            ("public", "SELECT", "REVOKE", 1),
        ] {
            assert!(object_rule(grantee, permission, state, minor, Some(1)).is_err());
        }
        assert!(object_rule("public", "CONTROL", "DENY", 0, Some(1)).is_ok());
        assert!(object_rule("guest", "SELECT", "DENY", 1, Some(1)).is_ok());
    }

    #[test]
    fn unresolved_or_non_system_objects_refuse_even_when_denied() {
        for shipped in [None, Some(0)] {
            for state in ["GRANT", "DENY"] {
                assert!(object_rule("public", "EXECUTE", state, 0, shipped).is_err());
            }
        }
        assert_eq!(
            object_rule("public", "SELECT", "GRANT", 0, None),
            Err("the object cannot be resolved")
        );
    }
}
