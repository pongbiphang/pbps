//! Build-independent guest/public rules for the three system databases
//! (DEC-2236.1). Shipped SELECT/EXECUTE grants are shared instance reach;
//! a permission on an administrator's own module is not.

use super::{ident, literal, text};
use pbps_db::transport::ExecuteConnection;
use pbps_db::{DbError, Row};
use pbps_model::escape::Escaped;
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

fn refused_permission(
    database: &str,
    grantee: &str,
    permission: &str,
    state: &str,
    object: &str,
    minor: i32,
    reason: &str,
) -> String {
    // This fragment contains only raw identities and fixed separators. The
    // reason is already prose and must not pass through the adapter again.
    format!(
        "{database} {grantee} {permission} {state} on {} (minor_id {minor}) \
         refuses: {reason}",
        Escaped(object)
    )
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
        return Err(DbError::Refused(refused_permission(
            database,
            &grantee,
            &permission,
            &state,
            &object,
            minor,
            reason,
        )));
    }
    Ok(())
}

// DENY ONLY entries never add grant reach. This exception is available only
// after the validated dbo entry, not to the ordinary deployer or sandbox.
fn user_token_qualifies(token: &[(String, String, String)], dbo: bool) -> bool {
    let mut base = Vec::new();
    let mut extras = BTreeSet::new();
    for (name, kind, usage) in token {
        if ["guest", "public"].contains(&name.as_str()) {
            base.push(name.as_str());
        } else if !(dbo
            && usage == "DENY ONLY"
            && ((name == "dbo" && kind == "SQL USER") || (name == "db_owner" && kind == "ROLE"))
            && extras.insert(name))
        {
            return false;
        }
    }
    base.sort();
    base == ["guest", "public"]
}

fn refused_login_token(name: &str, kind: &str, usage: &str) -> String {
    format!(
        "dbo sys.login_token entry {} ({kind}, {usage}) is not an owner DENY ONLY entry",
        Escaped(name)
    )
}

fn refused_role_membership(database: &str, role: &str) -> String {
    format!(
        "{database} guest role membership on ROLE {} is not allowed",
        Escaped(role)
    )
}

fn refused_user_token(database: &str, token: &[(String, String, String)]) -> String {
    format!(
        "{database} guest/public token membership on DATABASE {database} differs: {}",
        token
            .iter()
            .map(|(name, kind, usage)| format!("{} ({kind}, {usage})", Escaped(name)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn refused_ownership(database: &str, owned: &BTreeSet<String>) -> String {
    format!(
        "{database} guest/public ownership must be only guest's SCHEMA guest: {}",
        owned
            .iter()
            .map(|key| Escaped(key).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

// The engine constructs the server-role entries from the entered owner's
// login token. No GRANT-capable entry qualifies, even public. A Windows
// owner's token also carries its Windows groups (measured on LocalDB:
// `\\Everyone`), and impersonation marks them DENY ONLY too: they can only
// subtract reach, so scratch is never wider than the owner, while a
// GRANT-capable group still refuses (DEC-2271.2).
fn login_token_entry_qualifies(kind: &str, usage: &str, is_owner: bool) -> bool {
    usage == "DENY ONLY" && (is_owner || kind == "SERVER ROLE" || kind == "WINDOWS GROUP")
}

async fn login_token(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    let owner = super::one(
        conn.query(
            "SELECT CONVERT(varchar(170),owner_sid,1) AS sid \
        FROM sys.databases WHERE database_id=DB_ID();",
        )
        .await?,
        "scratch owner SID",
    )?;
    let owner = text(&owner, "sid")?;
    let mut found_owner = false;
    let mut found_public = false;
    for row in conn
        .query("SELECT name,type,RTRIM(usage) AS usage,CONVERT(varchar(170),sid,1) AS sid FROM sys.login_token;")
        .await?
    {
        let name = text(&row, "name")?;
        let kind = text(&row, "type")?;
        let usage = text(&row, "usage")?;
        let is_owner =
            text(&row, "sid")? == owner && ["SQL LOGIN", "WINDOWS LOGIN"].contains(&kind.as_str());
        if !login_token_entry_qualifies(&kind, &usage, is_owner) {
            return Err(DbError::Refused(refused_login_token(&name, &kind, &usage)));
        }
        found_owner |= is_owner;
        found_public |= name == "public" && kind == "SERVER ROLE";
    }
    if !found_owner || !found_public {
        return Err(DbError::Refused(
            "dbo sys.login_token did not report the scratch owner and public".into(),
        ));
    }
    Ok(())
}

pub(super) async fn check(conn: &mut impl ExecuteConnection, dbo: bool) -> Result<(), DbError> {
    if dbo {
        login_token(conn).await?;
    }
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
            return Err(DbError::Refused(refused_role_membership(
                database,
                &text(row, "role")?,
            )));
        }
        let token = read(
            conn,
            database,
            "user_token",
            "SELECT name,type,RTRIM(usage) AS usage FROM sys.user_token ORDER BY name;",
        )
        .await?
        .iter()
        .map(|row| Ok((text(row, "name")?, text(row, "type")?, text(row, "usage")?)))
        .collect::<Result<Vec<_>, DbError>>()?;
        if !user_token_qualifies(&token, dbo) {
            return Err(DbError::Refused(refused_user_token(database, &token)));
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
                 (SELECT principal_id FROM sys.user_token WHERE name IN(N''guest'',N''public''))');"
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
            return Err(DbError::Refused(refused_ownership(database, &owned)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::object_rule;

    #[test]
    fn permission_and_role_refusals_render_raw_fragments_once() {
        let name = "na\n\u{1b}\u{202e}\\n\\u{202e}雪";
        assert_eq!(
            super::refused_permission(
                "master",
                "public",
                "SELECT",
                "GRANT",
                &format!("app.{name}"),
                7,
                r"already shown \n"
            ),
            r"master public SELECT GRANT on app.na\n\u{1b}\u{202e}\\n\\u{202e}雪 (minor_id 7) refuses: already shown \n"
        );
        assert_eq!(
            super::refused_permission(
                "master",
                "guest",
                "CONTROL",
                "DENY",
                &format!("SCHEMA {name}"),
                0,
                "permissions of this class are not allowed"
            ),
            r"master guest CONTROL DENY on SCHEMA na\n\u{1b}\u{202e}\\n\\u{202e}雪 (minor_id 0) refuses: permissions of this class are not allowed"
        );
        assert_eq!(
            super::refused_role_membership("msdb", name),
            r"msdb guest role membership on ROLE na\n\u{1b}\u{202e}\\n\\u{202e}雪 is not allowed"
        );
        assert_eq!(
            super::refused_permission(
                "master",
                "public",
                "SELECT",
                "GRANT",
                "app.café雪",
                0,
                "reason"
            ),
            "master public SELECT GRANT on app.café雪 (minor_id 0) refuses: reason"
        );
        assert_eq!(
            super::refused_role_membership("msdb", "café雪"),
            "msdb guest role membership on ROLE café雪 is not allowed"
        );
    }

    #[test]
    fn only_deny_only_owner_role_and_windows_group_entries_qualify() {
        use super::login_token_entry_qualifies as qualifies;
        assert!(qualifies("WINDOWS LOGIN", "DENY ONLY", true));
        assert!(qualifies("SERVER ROLE", "DENY ONLY", false));
        assert!(qualifies("WINDOWS GROUP", "DENY ONLY", false));
        // A group able to grant would widen scratch past its owner.
        assert!(!qualifies("WINDOWS GROUP", "GRANT OR DENY", false));
        assert!(!qualifies("SERVER ROLE", "GRANT OR DENY", false));
        // Another login, or a SQL login that is not the owner, never qualifies.
        assert!(!qualifies("WINDOWS LOGIN", "DENY ONLY", false));
        assert!(!qualifies("SQL LOGIN", "DENY ONLY", false));
        assert!(!qualifies("CERTIFICATE", "DENY ONLY", false));
        assert!(!qualifies("WINDOWS LOGIN", "GRANT OR DENY", true));
    }

    #[test]
    fn token_refusals_render_names_once_without_changing_qualification() {
        let name = "na\n\u{1b}\u{202e}\\n\\u{202e}雪";
        assert_eq!(
            super::refused_login_token(name, "SQL LOGIN", "GRANT OR DENY"),
            r"dbo sys.login_token entry na\n\u{1b}\u{202e}\\n\\u{202e}雪 (SQL LOGIN, GRANT OR DENY) is not an owner DENY ONLY entry"
        );
        assert_eq!(
            super::refused_login_token("café雪", "SERVER ROLE", "GRANT OR DENY"),
            "dbo sys.login_token entry café雪 (SERVER ROLE, GRANT OR DENY) is not an owner DENY ONLY entry"
        );
        let mut token = vec![
            ("guest".into(), "SQL USER".into(), "GRANT OR DENY".into()),
            ("public".into(), "ROLE".into(), "GRANT OR DENY".into()),
        ];
        assert!(super::user_token_qualifies(&token, true));
        token.push((name.into(), "ROLE".into(), "DENY ONLY".into()));
        token.push((r"literal\n".into(), "ROLE".into(), "DENY ONLY".into()));
        let before = token.clone();
        assert!(!super::user_token_qualifies(&token, true));
        assert_eq!(
            super::refused_user_token("tempdb", &token),
            r"tempdb guest/public token membership on DATABASE tempdb differs: guest (SQL USER, GRANT OR DENY), public (ROLE, GRANT OR DENY), na\n\u{1b}\u{202e}\\n\\u{202e}雪 (ROLE, DENY ONLY), literal\\n (ROLE, DENY ONLY)"
        );
        assert_eq!(token, before);
        assert!(!super::user_token_qualifies(&token, true));
        assert_eq!(
            super::refused_user_token(
                "msdb",
                &[("café雪".into(), "ROLE".into(), "DENY ONLY".into())]
            ),
            "msdb guest/public token membership on DATABASE msdb differs: café雪 (ROLE, DENY ONLY)"
        );
    }

    #[test]
    fn ownership_refusal_preserves_raw_keys_and_their_order() {
        let owned = std::collections::BTreeSet::from([
            "public|objects:na\n\u{1b}\u{202e}\\n\\u{202e}雪".to_owned(),
            r"public|objects:na\n".to_owned(),
        ]);
        let before = owned.clone();
        // The real LF sorts before the literal backslash; sorting the shown
        // forms would reverse these entries. They remain distinct raw keys.
        assert_eq!(owned.len(), 2);
        assert_eq!(
            owned.first().unwrap(),
            "public|objects:na\n\u{1b}\u{202e}\\n\\u{202e}雪"
        );
        assert_eq!(
            super::refused_ownership("msdb", &owned),
            r"msdb guest/public ownership must be only guest's SCHEMA guest: public|objects:na\n\u{1b}\u{202e}\\n\\u{202e}雪, public|objects:na\\n"
        );
        assert_eq!(owned, before);
        assert_ne!(
            owned,
            std::collections::BTreeSet::from(["guest|schemas:guest".into()])
        );
        assert_eq!(
            super::refused_ownership(
                "msdb",
                &std::collections::BTreeSet::from(["public|schemas:café雪".into()])
            ),
            "msdb guest/public ownership must be only guest's SCHEMA guest: public|schemas:café雪"
        );
    }

    #[test]
    fn dbo_system_tokens_admit_only_the_measured_deny_only_extras() {
        let base = vec![
            ("guest".into(), "SQL USER".into(), "GRANT OR DENY".into()),
            ("public".into(), "ROLE".into(), "GRANT OR DENY".into()),
        ];
        assert!(super::user_token_qualifies(&base, false));
        for (name, kind) in [("dbo", "SQL USER"), ("db_owner", "ROLE")] {
            let mut token = base.clone();
            token.push((name.into(), kind.into(), "DENY ONLY".into()));
            assert!(super::user_token_qualifies(&token, true));
            assert!(!super::user_token_qualifies(&token, false));
            token.last_mut().unwrap().2 = "GRANT OR DENY".into();
            assert!(!super::user_token_qualifies(&token, true));
        }
        let mut other = base.clone();
        other.push(("db_securityadmin".into(), "ROLE".into(), "DENY ONLY".into()));
        assert!(!super::user_token_qualifies(&other, true));
        assert!(!super::user_token_qualifies(&base[..1], true));
    }

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
