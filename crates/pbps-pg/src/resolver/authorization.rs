//! The deployment authorization context: who a plan deploys as, and exactly
//! what that principal may do to the schemas and objects in scope (ADR-0016
//! case 16; SPEC §9.3.3, §7.6).
//!
//! The deployer is the planning connection's `current_user`, which is the
//! login apply will use — never the scratch administrator and never the
//! discovery session. What matters for binding is the *effective* answer the
//! engine gives to "may this principal use this schema, create in it, read
//! this table": `has_schema_privilege` and friends already fold in role
//! inheritance, ownership and PUBLIC, so the context records the engine's own
//! answers rather than re-deriving them from raw ACLs. The switchable roles
//! and role attributes are kept too, because reproduction (#610's scratch
//! side) has to recreate a principal that gives the same answers, and a
//! superuser or a `SET ROLE` target changes them.
//!
//! Unreadable is not empty: a catalog a least-privilege deployer cannot read
//! is an error, never an authorization context with nothing in it.

use pbps_db::resolver::environment::DeploymentPrincipal;
use pbps_db::transport::QueryConnection;
use pbps_db::{DbError, Row};
use std::collections::BTreeMap;

/// The privileges the context measures for each kind of object. Enough to
/// bind a creation against existing objects; not an audit of every grant.
const SCHEMA_PRIVILEGES: &[&str] = &["USAGE", "CREATE"];
const TABLE_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "REFERENCES",
    "TRIGGER",
];
const FUNCTION_PRIVILEGES: &[&str] = &["EXECUTE"];

/// The GUCs a role- or database-level setting may pin that change binding or
/// loaded code; the same list the compatibility rule compares, so a
/// role-scoped `session_preload_libraries` is part of the context.
const SETTINGS: &[&str] = &[
    "search_path",
    "session_preload_libraries",
    "shared_preload_libraries",
    "local_preload_libraries",
    "dynamic_library_path",
    "check_function_bodies",
    "row_security",
];

/// What one in-scope schema grants the deployer, as the engine answers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaAuthorization {
    pub owner: String,
    pub privileges: BTreeMap<String, bool>,
}

/// What one in-scope object grants the deployer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ObjectAuthorization {
    pub kind: String,
    pub owner: String,
    pub privileges: BTreeMap<String, bool>,
}

/// A role in the deployer's authorization closure, and what about it a
/// reproduction must mirror.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RoleAttributes {
    pub superuser: bool,
    pub inherit: bool,
    pub bypass_rls: bool,
    /// Whether the deployer can `SET ROLE` to it (session_user ≠ current_user
    /// reproduction), whether it inherits its privileges, or both.
    pub can_set: bool,
    pub inherits: bool,
}

/// The full context, canonicalised deterministically for the fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuthorizationContext {
    pub principal: DeploymentPrincipal,
    pub schemas: BTreeMap<String, SchemaAuthorization>,
    pub objects: BTreeMap<String, ObjectAuthorization>,
    pub roles: BTreeMap<String, RoleAttributes>,
    /// Effective role- and database-scoped settings for the deployer, keyed
    /// `<scope>:<name>` where scope is `role`, `database` or `database-role`.
    pub settings: BTreeMap<String, String>,
}

impl AuthorizationContext {
    /// A stable byte string over every field, for hashing into a fingerprint.
    /// `BTreeMap` ordering makes it deterministic; a changed answer, owner,
    /// attribute or setting changes the bytes.
    pub fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("authorization context serialises")
    }
}

pub async fn principal(conn: &mut impl QueryConnection) -> Result<DeploymentPrincipal, DbError> {
    let rows = conn
        .query(
            "SELECT current_user::text AS effective, session_user::text AS login, \
             pg_catalog.current_setting('is_superuser') AS superuser",
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "deployer principal expected one row".into(),
        ));
    };
    Ok(DeploymentPrincipal {
        login: required(row, "login", "the session user")?,
        effective: required(row, "effective", "the current user")?,
        superuser: required(row, "superuser", "the superuser flag")? == "on",
    })
}

/// Reads the deployer's authorization for the in-scope schemas and the
/// objects in them, as the current principal. Every `has_*_privilege` is
/// evaluated by the engine as that principal, so inheritance and ownership
/// are the engine's to compute, not this reader's.
pub async fn read(
    conn: &mut impl QueryConnection,
    schemas: &[String],
) -> Result<AuthorizationContext, DbError> {
    let principal = principal(conn).await?;
    let mut schema_auth = BTreeMap::new();
    let mut objects = BTreeMap::new();
    for schema in schemas {
        let owner = schema_owner(conn, schema).await?;
        schema_auth.insert(
            schema.clone(),
            SchemaAuthorization {
                owner,
                privileges: schema_privileges(conn, schema).await?,
            },
        );
        objects.extend(object_privileges(conn, schema).await?);
    }
    Ok(AuthorizationContext {
        principal,
        schemas: schema_auth,
        objects,
        roles: role_attributes(conn).await?,
        settings: settings(conn).await?,
    })
}

async fn schema_owner(conn: &mut impl QueryConnection, schema: &str) -> Result<String, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.pg_get_userbyid(n.nspowner) AS owner \
             FROM pg_catalog.pg_namespace n WHERE n.nspname = {}",
            literal(schema)
        ))
        .await?;
    match rows.as_slice() {
        [row] => required(row, "owner", "a schema's owner"),
        // A schema the deployer cannot see is not an owner of nothing: it is
        // a scope the context could not be established for.
        [] => Err(DbError::BadRow(format!(
            "the in-scope schema {schema} is not visible to the deployer"
        ))),
        _ => Err(DbError::BadRow(format!(
            "the in-scope schema {schema} is ambiguous"
        ))),
    }
}

async fn schema_privileges(
    conn: &mut impl QueryConnection,
    schema: &str,
) -> Result<BTreeMap<String, bool>, DbError> {
    let mut privileges = BTreeMap::new();
    for privilege in SCHEMA_PRIVILEGES {
        let rows = conn
            .query(&format!(
                "SELECT pg_catalog.has_schema_privilege({}, {})::text AS allowed",
                literal(schema),
                literal(privilege)
            ))
            .await?;
        privileges.insert((*privilege).to_owned(), boolean(&rows, "allowed")?);
    }
    Ok(privileges)
}

async fn object_privileges(
    conn: &mut impl QueryConnection,
    schema: &str,
) -> Result<BTreeMap<String, ObjectAuthorization>, DbError> {
    let mut objects = BTreeMap::new();
    // Relations: tables, views, materialised views, partitioned tables.
    let relations = conn
        .query(&format!(
            "SELECT c.relname::text AS name, c.relkind::text AS kind, \
                    pg_catalog.pg_get_userbyid(c.relowner) AS owner \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {} AND c.relkind IN ('r','v','m','p') ORDER BY 1",
            literal(schema)
        ))
        .await?;
    for row in &relations {
        let name = required(row, "name", "a relation's name")?;
        let qualified = format!("{schema}.{name}");
        let mut privileges = BTreeMap::new();
        for privilege in TABLE_PRIVILEGES {
            let rows = conn
                .query(&format!(
                    "SELECT pg_catalog.has_table_privilege({}, {})::text AS allowed",
                    literal(&qualified_literal(schema, &name)),
                    literal(privilege)
                ))
                .await?;
            privileges.insert((*privilege).to_owned(), boolean(&rows, "allowed")?);
        }
        objects.insert(
            qualified,
            ObjectAuthorization {
                kind: required(row, "kind", "a relation's kind")?,
                owner: required(row, "owner", "a relation's owner")?,
                privileges,
            },
        );
    }
    // Routines, addressed by OID so overloads stay distinct.
    let routines = conn
        .query(&format!(
            "SELECT p.oid::bigint AS oid, \
                    (p.proname || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')') AS name, \
                    pg_catalog.pg_get_userbyid(p.proowner) AS owner \
             FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = {} ORDER BY 2",
            literal(schema)
        ))
        .await?;
    for row in &routines {
        let oid: i64 = required(row, "oid", "a routine's oid")?
            .parse()
            .map_err(|_| DbError::BadRow("a routine oid was not an integer".into()))?;
        let name = required(row, "name", "a routine's identity")?;
        let mut privileges = BTreeMap::new();
        for privilege in FUNCTION_PRIVILEGES {
            let rows = conn
                .query(&format!(
                    "SELECT pg_catalog.has_function_privilege({}::oid, {})::text AS allowed",
                    oid,
                    literal(privilege)
                ))
                .await?;
            privileges.insert((*privilege).to_owned(), boolean(&rows, "allowed")?);
        }
        objects.insert(
            format!("{schema}.{name}"),
            ObjectAuthorization {
                kind: "routine".into(),
                owner: required(row, "owner", "a routine's owner")?,
                privileges,
            },
        );
    }
    Ok(objects)
}

/// Every role the deployer inherits from or can switch to, with what a
/// reproduction must mirror. `pg_has_role` gives the engine's own answer for
/// inheritance (`USAGE`) and switching (`MEMBER`).
async fn role_attributes(
    conn: &mut impl QueryConnection,
) -> Result<BTreeMap<String, RoleAttributes>, DbError> {
    let rows = conn
        .query(
            "SELECT r.rolname::text AS name, r.rolsuper::text AS superuser, r.rolinherit::text AS inherit, \
                    r.rolbypassrls::text AS bypass_rls, \
                    pg_catalog.pg_has_role(r.oid, 'MEMBER')::text AS can_set, \
                    pg_catalog.pg_has_role(r.oid, 'USAGE')::text AS inherits \
             FROM pg_catalog.pg_roles r \
             WHERE pg_catalog.pg_has_role(r.oid, 'MEMBER') OR pg_catalog.pg_has_role(r.oid, 'USAGE') \
             ORDER BY 1",
        )
        .await?;
    let mut roles = BTreeMap::new();
    for row in &rows {
        roles.insert(
            required(row, "name", "a role's name")?,
            RoleAttributes {
                superuser: flag(row, "superuser")?,
                inherit: flag(row, "inherit")?,
                bypass_rls: flag(row, "bypass_rls")?,
                can_set: flag(row, "can_set")?,
                inherits: flag(row, "inherits")?,
            },
        );
    }
    if roles.is_empty() {
        // The deployer is a member of at least itself; an empty answer is a
        // read that did not run, not a principal with no roles.
        return Err(DbError::BadRow(
            "the deployer's role membership could not be read".into(),
        ));
    }
    Ok(roles)
}

/// Role- and database-scoped settings that pin one of the rule's GUCs for
/// this deployer. Read from `pg_db_role_setting`, restricted to the deployer,
/// the current database, and their combination.
async fn settings(conn: &mut impl QueryConnection) -> Result<BTreeMap<String, String>, DbError> {
    let rows = conn
        .query(
            "SELECT CASE \
                      WHEN s.setdatabase <> 0 AND s.setrole <> 0 THEN 'database-role' \
                      WHEN s.setdatabase <> 0 THEN 'database' \
                      ELSE 'role' END AS scope, \
                    e.entry AS entry \
             FROM pg_catalog.pg_db_role_setting s \
             CROSS JOIN LATERAL pg_catalog.unnest(s.setconfig) AS e(entry) \
             WHERE (s.setrole = 0 OR s.setrole = current_user::regrole) \
               AND (s.setdatabase = 0 OR s.setdatabase = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database()))",
        )
        .await?;
    let mut settings = BTreeMap::new();
    for row in &rows {
        let scope = required(row, "scope", "a setting's scope")?;
        let entry = required(row, "entry", "a setting entry")?;
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if SETTINGS.contains(&name) {
            settings.insert(format!("{scope}:{name}"), value.to_owned());
        }
    }
    Ok(settings)
}

/// A SQL string literal with single quotes doubled: the schema, object and
/// privilege names reach `has_*_privilege` as text arguments, and the scratch
/// stream has no parameter binding, so quoting is what keeps a name from
/// ending the literal.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `has_table_privilege` takes one text argument naming the relation; a
/// schema or table containing a dot or a quote is quoted as two identifiers
/// so it resolves to exactly that object.
fn qualified_literal(schema: &str, name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema.replace('"', "\"\""),
        name.replace('"', "\"\"")
    )
}

fn boolean(rows: &[Row], field: &str) -> Result<bool, DbError> {
    match rows {
        [row] => flag(row, field),
        _ => Err(DbError::BadRow(format!(
            "an authorization check for {field} did not return one row"
        ))),
    }
}

fn flag(row: &Row, field: &str) -> Result<bool, DbError> {
    match required(row, field, field)?.as_str() {
        "t" | "true" | "on" => Ok(true),
        "f" | "false" | "off" => Ok(false),
        other => Err(DbError::BadRow(format!(
            "authorization flag {field} was neither true nor false: {other}"
        ))),
    }
}

fn required(row: &Row, field: &str, what: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("the authorization context did not report {what}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> AuthorizationContext {
        AuthorizationContext {
            principal: DeploymentPrincipal {
                login: "dep".into(),
                effective: "dep".into(),
                superuser: false,
            },
            schemas: [(
                "app".to_owned(),
                SchemaAuthorization {
                    owner: "app_owner".into(),
                    privileges: [("USAGE".to_owned(), true), ("CREATE".to_owned(), false)]
                        .into_iter()
                        .collect(),
                },
            )]
            .into_iter()
            .collect(),
            objects: BTreeMap::new(),
            roles: [(
                "app_reader".to_owned(),
                RoleAttributes {
                    superuser: false,
                    inherit: true,
                    bypass_rls: false,
                    can_set: true,
                    inherits: true,
                },
            )]
            .into_iter()
            .collect(),
            settings: [(
                "database-role:search_path".to_owned(),
                "app, public".to_owned(),
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn the_canonical_form_changes_when_any_field_changes() {
        let base = context().canonical();
        // A privilege answer flipping.
        let mut lost_usage = context();
        lost_usage
            .schemas
            .get_mut("app")
            .unwrap()
            .privileges
            .insert("USAGE".into(), false);
        assert_ne!(lost_usage.canonical(), base);
        // The deployer becoming a superuser.
        let mut superuser = context();
        superuser.principal.superuser = true;
        assert_ne!(superuser.canonical(), base);
        // An owner changing.
        let mut owner = context();
        owner.schemas.get_mut("app").unwrap().owner = "someone_else".into();
        assert_ne!(owner.canonical(), base);
        // A role losing its switch option.
        let mut no_set = context();
        no_set.roles.get_mut("app_reader").unwrap().can_set = false;
        assert_ne!(no_set.canonical(), base);
        // A settings pin changing.
        let mut setting = context();
        setting
            .settings
            .insert("database-role:search_path".into(), "public".into());
        assert_ne!(setting.canonical(), base);
        // The same context twice is the same bytes: ordering is stable.
        assert_eq!(context().canonical(), base);
    }
}
