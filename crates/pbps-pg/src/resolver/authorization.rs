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
use std::collections::{BTreeMap, BTreeSet};

/// The versioned name of this authorization rule, measured on 16 and 18.
pub const RULE: &str = "pg-auth-v1";

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

/// What one in-scope schema grants: its owner, the deployer's effective
/// answers (for comparison), and the raw ACL (for reconstruction). The ACL
/// is keyed by grantee role name, with `PUBLIC` for the pseudo-role, so a
/// reconstruction can grant the same privileges to the same mapped roles and
/// the effective answer follows from the reproduced membership.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaAuthorization {
    pub owner: String,
    pub privileges: BTreeMap<String, bool>,
    pub acl: BTreeMap<String, Vec<String>>,
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
                acl: schema_acl(conn, schema).await?,
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

/// The schema's grant list from `aclexplode`, keyed by grantee (the
/// pseudo-role 0 is `PUBLIC`). A schema with the default (NULL) ACL has only
/// the owner's implicit rights, which reconstruction gets from `AUTHORIZATION`
/// alone, so an empty map is a real "no explicit grants", not unreadable.
async fn schema_acl(
    conn: &mut impl QueryConnection,
    schema: &str,
) -> Result<BTreeMap<String, Vec<String>>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT CASE WHEN a.grantee = 0 THEN 'PUBLIC' \
                         ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
                    a.privilege_type AS privilege \
             FROM pg_catalog.pg_namespace n \
             CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) AS a \
             WHERE n.nspname = {} ORDER BY 1, 2",
            literal(schema)
        ))
        .await?;
    let mut acl: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        acl.entry(required(row, "grantee", "a grantee")?)
            .or_default()
            .push(required(row, "privilege", "a privilege type")?);
    }
    Ok(acl)
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

/// A run-local role reconstruction plan: each logical role in a target
/// context mapped to a unique role this run created and will drop, so nothing
/// pre-existing on a shared server is touched, shadowed, or left behind. The
/// mapping is total over the deployer, its role closure, every in-scope owner
/// and every schema grantee.
#[derive(Debug, Clone)]
pub struct RoleMap {
    to_run_local: BTreeMap<String, String>,
    run_login: String,
}

impl RoleMap {
    /// `token` is the run's unique suffix (the scratch names' token); run-local
    /// roles are `pbps_role_<n>_<token>`, which the identifier rules refuse to
    /// confuse with a production name.
    pub fn generate(context: &AuthorizationContext, run_login: &str, token: &str) -> Self {
        let mut logical = BTreeSet::new();
        logical.insert(context.principal.effective.clone());
        logical.extend(context.roles.keys().cloned());
        for schema in context.schemas.values() {
            logical.insert(schema.owner.clone());
            for grantee in schema.acl.keys() {
                if grantee != "PUBLIC" {
                    logical.insert(grantee.clone());
                }
            }
        }
        let to_run_local = logical
            .into_iter()
            .enumerate()
            .map(|(index, name)| (name, format!("pbps_role_{index}_{token}")))
            .collect();
        Self {
            to_run_local,
            run_login: run_login.to_owned(),
        }
    }

    /// The run-local name for a logical role, or `PUBLIC` unchanged. `None`
    /// for a role the map does not cover — a grant to which would silently
    /// not be reproduced, so callers refuse rather than skip it.
    fn run_local(&self, logical: &str) -> Option<String> {
        if logical == "PUBLIC" {
            return Some("PUBLIC".to_owned());
        }
        self.to_run_local.get(logical).cloned()
    }

    /// The logical role a run-local name stands for, for comparing a
    /// reconstructed context back against the target.
    fn logical_of(&self, run_local: &str) -> Option<String> {
        if run_local == "PUBLIC" {
            return Some("PUBLIC".to_owned());
        }
        self.to_run_local
            .iter()
            .find(|(_, run)| *run == run_local)
            .map(|(logical, _)| logical.clone())
    }

    /// Every run-local role name, so the run records them as recovery names
    /// before creating them and drops them on cleanup.
    pub fn run_local_names(&self) -> Vec<String> {
        self.to_run_local.values().cloned().collect()
    }

    /// The run-local role the compilation session must `SET ROLE` to: the
    /// mapping of the target's deployer principal.
    pub fn deployer(&self, context: &AuthorizationContext) -> Option<String> {
        self.run_local(&context.principal.effective)
    }
}

/// A planned authorization change the plan performs before its DDL, applied
/// to scratch so the deployer's effective privileges match what apply will
/// see (SPEC §7.6). Only schema grants and revokes; role creation in a plan
/// is refused by the emitter, so it cannot reach here.
#[derive(Debug, Clone)]
pub struct PlannedGrant {
    pub role: String,
    pub schema: String,
    pub privilege: String,
    pub revoke: bool,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Creates the run-local roles, memberships, schemas and settings that
/// reproduce the target deployer's authorization on the scratch database,
/// through the admin connection. `database` is the scratch database, for the
/// database-scoped settings. Every statement names only run-local roles and
/// this run's own schemas; nothing pre-existing is altered.
pub async fn reconstruct(
    admin: &mut impl QueryConnection,
    map: &RoleMap,
    context: &AuthorizationContext,
    database: &str,
) -> Result<(), DbError> {
    let missing = |logical: &str| {
        DbError::BadRow(format!(
            "the authorization reconstruction has no run-local role for {logical}"
        ))
    };
    // Roles. A role in the closure carries its measured attributes; an owner
    // that is not in the closure is created unprivileged (it only needs to
    // own a schema).
    for (logical, run) in &map.to_run_local {
        let attrs = context.roles.get(logical);
        let superuser = attrs.is_some_and(|a| a.superuser);
        let inherit = attrs.is_none_or(|a| a.inherit);
        let bypass = attrs.is_some_and(|a| a.bypass_rls);
        admin
            .query(&format!(
                "CREATE ROLE {} NOLOGIN {} {} {}",
                quote_ident(run),
                if superuser {
                    "SUPERUSER"
                } else {
                    "NOSUPERUSER"
                },
                if inherit { "INHERIT" } else { "NOINHERIT" },
                if bypass { "BYPASSRLS" } else { "NOBYPASSRLS" },
            ))
            .await?;
    }
    // The deployer's flattened closure: a direct grant per role it inherits
    // from or can switch to, with the measured options, reproduces the same
    // `has_*_privilege` and `pg_has_role` answers.
    let deployer = map
        .run_local(&context.principal.effective)
        .ok_or_else(|| missing(&context.principal.effective))?;
    for (logical, attrs) in &context.roles {
        if *logical == context.principal.effective {
            continue;
        }
        let role = map.run_local(logical).ok_or_else(|| missing(logical))?;
        admin
            .query(&format!(
                "GRANT {} TO {} WITH INHERIT {}, SET {}",
                quote_ident(&role),
                quote_ident(&deployer),
                attrs.inherits,
                attrs.can_set,
            ))
            .await?;
    }
    // The run login can become the deployer but never inherits it, so the
    // compilation runs as the mapped deployer only under an explicit SET ROLE.
    admin
        .query(&format!(
            "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE",
            quote_ident(&deployer),
            quote_ident(&map.run_login),
        ))
        .await?;
    // Schemas, with the same name the emitter's write path uses, owned by the
    // mapped owner, with the target's ACL granted to the mapped grantees.
    for (name, schema) in &context.schemas {
        let owner = map
            .run_local(&schema.owner)
            .ok_or_else(|| missing(&schema.owner))?;
        // A fresh database from template0 already has `public`; recreating it
        // fails. Create the schema only if absent, then set its owner and
        // clear the default PUBLIC grants, so the ACL reproduced below is the
        // target's exactly and not the template's defaults (finding on #610).
        admin
            .query(&format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                quote_ident(name)
            ))
            .await?;
        admin
            .query(&format!(
                "ALTER SCHEMA {} OWNER TO {}",
                quote_ident(name),
                quote_ident(&owner),
            ))
            .await?;
        admin
            .query(&format!(
                "REVOKE ALL ON SCHEMA {} FROM PUBLIC",
                quote_ident(name)
            ))
            .await?;
        for (grantee, privileges) in &schema.acl {
            if privileges.is_empty() {
                continue;
            }
            let target = map.run_local(grantee).ok_or_else(|| missing(grantee))?;
            let recipient = if target == "PUBLIC" {
                "PUBLIC".to_owned()
            } else {
                quote_ident(&target)
            };
            admin
                .query(&format!(
                    "GRANT {} ON SCHEMA {} TO {}",
                    privileges.join(", "),
                    quote_ident(name),
                    recipient,
                ))
                .await?;
        }
    }
    // Settings: role-, database- and database-role-scoped, on the mapped
    // deployer and this run's own database.
    for (key, value) in &context.settings {
        let (scope, name) = key
            .split_once(':')
            .ok_or_else(|| DbError::BadRow(format!("a setting key without a scope: {key}")))?;
        let statement = match scope {
            "role" => format!(
                "ALTER ROLE {} SET {} = {}",
                quote_ident(&deployer),
                quote_ident(name),
                literal(value)
            ),
            "database" => format!(
                "ALTER DATABASE {} SET {} = {}",
                quote_ident(database),
                quote_ident(name),
                literal(value)
            ),
            "database-role" => format!(
                "ALTER ROLE {} IN DATABASE {} SET {} = {}",
                quote_ident(&deployer),
                quote_ident(database),
                quote_ident(name),
                literal(value)
            ),
            other => {
                return Err(DbError::BadRow(format!("unknown setting scope {other}")));
            }
        };
        admin.query(&statement).await?;
    }
    Ok(())
}

/// Applies the plan's preceding grants to scratch, mapped to run-local roles,
/// so the deployer's effective privileges are what apply will act under.
pub async fn apply_planned(
    admin: &mut impl QueryConnection,
    map: &RoleMap,
    grants: &[PlannedGrant],
) -> Result<(), DbError> {
    for grant in grants {
        let role = map.run_local(&grant.role).ok_or_else(|| {
            DbError::BadRow(format!(
                "a planned grant to {} has no run-local role",
                grant.role
            ))
        })?;
        let recipient = if role == "PUBLIC" {
            "PUBLIC".to_owned()
        } else {
            quote_ident(&role)
        };
        let statement = if grant.revoke {
            format!(
                "REVOKE {} ON SCHEMA {} FROM {}",
                grant.privilege,
                quote_ident(&grant.schema),
                recipient
            )
        } else {
            format!(
                "GRANT {} ON SCHEMA {} TO {}",
                grant.privilege,
                quote_ident(&grant.schema),
                recipient
            )
        };
        admin.query(&statement).await?;
    }
    Ok(())
}

/// Applies the plan's preceding grants to a copy of the target context, so a
/// scratch reproduction that ran those grants is compared against what the
/// deployer's authorization is *meant* to be after them, not before (SPEC
/// §7.6). The ACL is updated for each grant or revoke, and the deployer's
/// effective schema answers are recomputed from the updated ACL, ownership
/// and membership — the same inputs the engine uses for `has_schema_privilege`.
pub fn with_planned(
    mut context: AuthorizationContext,
    grants: &[PlannedGrant],
) -> AuthorizationContext {
    for grant in grants {
        let Some(schema) = context.schemas.get_mut(&grant.schema) else {
            continue;
        };
        let entry = schema.acl.entry(grant.role.clone()).or_default();
        entry.retain(|p| p != &grant.privilege);
        if !grant.revoke {
            entry.push(grant.privilege.clone());
            entry.sort();
        }
        if entry.is_empty() {
            schema.acl.remove(&grant.role);
        }
    }
    recompute_schema_effective(&mut context);
    context
}

/// Recomputes the deployer's effective USAGE/CREATE for each schema from the
/// ACL, ownership and the deployer's role closure: a schema is usable if the
/// deployer owns it, inherits its owner, or holds the privilege through
/// PUBLIC, itself, or a role it inherits. This mirrors what the engine answers
/// for schema privileges, so an expected context stays comparable to one read
/// back from a real server.
fn recompute_schema_effective(context: &mut AuthorizationContext) {
    let deployer = context.principal.effective.clone();
    let inherited: BTreeSet<String> = context
        .roles
        .iter()
        .filter(|(_, attrs)| attrs.inherits)
        .map(|(role, _)| role.clone())
        .collect();
    let holders = |grantee: &str| -> bool {
        grantee == "PUBLIC" || grantee == deployer || inherited.contains(grantee)
    };
    for schema in context.schemas.values_mut() {
        let owns = schema.owner == deployer || inherited.contains(&schema.owner);
        for privilege in ["USAGE", "CREATE"] {
            let granted = schema
                .acl
                .iter()
                .any(|(grantee, privs)| holders(grantee) && privs.iter().any(|p| p == privilege));
            schema
                .privileges
                .insert(privilege.to_owned(), owns || granted);
        }
    }
}

/// Reads the reconstructed context as the mapped deployer and names every
/// key that does not reproduce the target — owner, effective privilege, ACL
/// grantee or setting — after mapping run-local role names back to logical
/// ones. An empty result is a faithful reproduction; the run compiles only
/// when it is empty (ADR-0016 case 16).
pub async fn verify(
    deployer: &mut impl QueryConnection,
    map: &RoleMap,
    target: &AuthorizationContext,
    schemas: &[String],
) -> Result<Vec<String>, DbError> {
    let reproduced = read(deployer, schemas).await?;
    let mut differences = Vec::new();
    // The compilation must run as the mapped deployer, and its superuser
    // standing must match the target's.
    if map.logical_of(&reproduced.principal.effective).as_deref()
        != Some(target.principal.effective.as_str())
    {
        differences.push("principal".into());
    }
    if reproduced.principal.superuser != target.principal.superuser {
        differences.push("principal:superuser".into());
    }
    for (name, want) in &target.schemas {
        let Some(got) = reproduced.schemas.get(name) else {
            differences.push(format!("schema:{name}:absent"));
            continue;
        };
        if map.logical_of(&got.owner).as_deref() != Some(want.owner.as_str()) {
            differences.push(format!("schema:{name}:owner"));
        }
        if got.privileges != want.privileges {
            differences.push(format!("schema:{name}:privileges"));
        }
        let got_acl: BTreeMap<String, Vec<String>> = got
            .acl
            .iter()
            .map(|(grantee, privs)| {
                (
                    map.logical_of(grantee).unwrap_or_else(|| grantee.clone()),
                    privs.clone(),
                )
            })
            .collect();
        if got_acl != want.acl {
            differences.push(format!("schema:{name}:acl"));
        }
    }
    Ok(differences)
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
                    acl: [("app_reader".to_owned(), vec!["USAGE".to_owned()])]
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
