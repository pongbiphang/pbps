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
/// Whether a role- or database-level default is part of the context: the
/// write path's `search_path`, and every setting the compatibility rule
/// compares. The rule reads the target's effective values as the deployer,
/// which its login defaults shape, so a compared setting the context did not
/// carry — a role default for `default_text_search_config`, say — kept the
/// image's value on scratch and refused a reproducible scope; the list is
/// the compatibility module's own, not a shorter copy (finding on #688).
fn is_context_setting(name: &str) -> bool {
    name == "search_path"
        || super::compatibility::SETTINGS.contains(&name)
        || super::compatibility::OPTIONAL_SETTINGS.contains(&name)
}

/// The settings the engine stores and parses as a quoted identifier list
/// (`GUC_LIST_QUOTE`; the set pg_dump's `variable_is_guc_list_quote` names).
/// Their stored text is a rendered list — `"$user", public` — and replayed
/// as one string literal it becomes one element spelled like the list, so
/// the commonest role default of all did not reproduce and every such
/// deployer was refused (measured on 18; finding on #688). Each element goes
/// back as its own literal, which the engine renders to the same text.
const LIST_QUOTE_SETTINGS: &[&str] = &[
    "search_path",
    "session_preload_libraries",
    "shared_preload_libraries",
    "local_preload_libraries",
    "temp_tablespaces",
];

/// The right-hand side of `ALTER ROLE ... SET name = ...` that stores the
/// target's text for `name` again.
fn setting_value(name: &str, value: &str) -> Result<String, DbError> {
    if !LIST_QUOTE_SETTINGS.contains(&name) {
        return Ok(literal(value));
    }
    let elements = pbps_db::resolver::environment::guc_list(value).ok_or_else(|| {
        DbError::BadRow(format!(
            "the default for {name} is not a list the engine can read: {value}"
        ))
    })?;
    Ok(elements
        .iter()
        .map(|element| literal(element))
        .collect::<Vec<_>>()
        .join(", "))
}

/// What one in-scope schema grants: its owner, the deployer's effective
/// answers (for comparison), and the raw ACL (for reconstruction). The ACL
/// is keyed by grantee role name, with `PUBLIC` for the pseudo-role, so a
/// reconstruction can grant the same privileges to the same mapped roles and
/// the effective answer follows from the reproduced membership.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaAuthorization {
    pub owner: String,
    pub privileges: BTreeMap<String, bool>,
    pub acl: BTreeMap<String, Vec<Grant>>,
}

/// One explicit grant on a schema as `aclexplode` reports it: the privilege,
/// whether it came WITH GRANT OPTION, and the role recorded as its grantor.
/// The grantor is kept because a revoke is grantor-specific: a reproduction
/// that regranted everything as the administrator could not have the
/// deployer's own planned revoke take, and a valid plan would be refused
/// (finding on #688).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Grant {
    pub privilege: String,
    pub grantable: bool,
    pub grantor: String,
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
    }
    // In-scope object privileges are reconstructed and verified once the
    // objects exist on scratch, which is #613; a fresh scratch database has
    // none to compare here, so they are not captured in this step.
    Ok(AuthorizationContext {
        principal,
        schemas: schema_auth,
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
) -> Result<BTreeMap<String, Vec<Grant>>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT CASE WHEN a.grantee = 0 THEN 'PUBLIC' \
                         ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
                    pg_catalog.pg_get_userbyid(a.grantor)::text AS grantor, \
                    a.privilege_type AS privilege, \
                    a.is_grantable::text AS grantable \
             FROM pg_catalog.pg_namespace n \
             CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) AS a \
             WHERE n.nspname = {} ORDER BY 1, 2, 3",
            literal(schema)
        ))
        .await?;
    let mut acl: BTreeMap<String, Vec<Grant>> = BTreeMap::new();
    for row in &rows {
        // The grant option is kept, so a grantee that may re-grant is
        // distinguished from one that may not, and the grantor, so each
        // entry is replayed under it (finding on #688); a planned grant the
        // deployer lacks the option for then fails as apply would.
        acl.entry(required(row, "grantee", "a grantee")?)
            .or_default()
            .push(Grant {
                privilege: required(row, "privilege", "a privilege type")?,
                grantable: flag(row, "grantable")?,
                grantor: required(row, "grantor", "a grantor")?,
            });
    }
    for grants in acl.values_mut() {
        grants.sort();
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

/// Role- and database-scoped defaults that pin one of the rule's GUCs for
/// this session. Read from `pg_db_role_setting`: the database's, the login
/// role's (`session_user`), and their combination. The effective role's own
/// defaults are not part of the session: PostgreSQL applies role defaults at
/// login and not on `SET ROLE`, so after a switch they apply nowhere, and
/// reading them under the same key as the login's would let one overwrite
/// the other (finding on #688).
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
             WHERE (s.setrole = 0 OR s.setrole = session_user::regrole) \
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
        if is_context_setting(name) {
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

/// Whether a schema is one the engine itself provides: `information_schema`
/// and the `pg_` namespaces, created by initdb in every database, owned by
/// the bootstrap superuser (whose name varies by installation) with the
/// default ACL, and not droppable — `DROP SCHEMA information_schema` fails
/// on the objects that depend on it (measured on 18). Such a schema reaches
/// the scope as a write-path extra. It is neither recreated nor reconciled
/// by ACL on scratch, since scratch already has the engine's own; what is
/// compared is the deployer's effective privileges on it, which is what
/// binding depends on (finding on #688). The pull never manages objects in
/// these schemas (DECISIONS 276), so none is ever an in-scope schema.
pub fn is_system_schema(name: &str) -> bool {
    name == "information_schema" || name.starts_with("pg_")
}

/// Whether a role is one of the engine's predefined roles, which a
/// reproduction grants membership in rather than clones. Their capabilities
/// live in the role identity — `pg_read_all_settings` is what makes the
/// superuser-only settings visible in `pg_settings`, measured on 18: a clone
/// carrying the same attributes sees none of them, membership in the real
/// role sees all three — and the `pg_` prefix is reserved, so no target role
/// of its own can be mistaken for one (finding on #688). `pg_database_owner`
/// is the exception: it cannot have explicit members (measured: "role
/// pg_database_owner cannot have explicit members"), and what it carries is
/// ownership of `public`, which a clone reproduces by owning the schema.
fn is_predefined(role: &str) -> bool {
    role.starts_with("pg_") && role != "pg_database_owner"
}

impl RoleMap {
    /// `token` is the run's unique suffix (the scratch names' token); run-local
    /// roles are `pbps_role_<n>_<token>`, which the identifier rules refuse to
    /// confuse with a production name.
    pub fn generate(
        context: &AuthorizationContext,
        planned: &[PlannedGrant],
        run_login: &str,
        token: &str,
    ) -> Self {
        let mut logical = BTreeSet::new();
        logical.insert(context.principal.effective.clone());
        logical.extend(context.roles.keys().cloned());
        for (name, schema) in &context.schemas {
            // A system schema's owner and grantees are the engine's own and
            // nothing is replayed on it, so they need no run-local role.
            if is_system_schema(name) {
                continue;
            }
            logical.insert(schema.owner.clone());
            for (grantee, grants) in &schema.acl {
                if grantee != "PUBLIC" {
                    logical.insert(grantee.clone());
                }
                // Each entry is replayed under its grantor, so the grantor
                // needs a run-local role too, whether or not the deployer
                // ever reaches it.
                for grant in grants {
                    logical.insert(grant.grantor.clone());
                }
            }
        }
        // A plan's grant may name a role that is neither in the deployer's
        // closure nor already a grantee, so it must be mapped too or
        // `apply_planned` has no run-local role for it (finding on #688).
        for grant in planned {
            if grant.role != "PUBLIC" {
                logical.insert(grant.role.clone());
            }
        }
        let to_run_local = logical
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let run_local = if is_predefined(&name) {
                    name.clone()
                } else {
                    format!("pbps_role_{index}_{token}")
                };
                (name, run_local)
            })
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
        self.to_run_local
            .values()
            .filter(|name| !is_predefined(name))
            .cloned()
            .collect()
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
        if is_predefined(logical) {
            continue;
        }
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
    // mapped owner, with the target's ACL replayed under the mapped grantors.
    for (name, schema) in &context.schemas {
        if is_system_schema(name) {
            continue;
        }
        let owner = map
            .run_local(&schema.owner)
            .ok_or_else(|| missing(&schema.owner))?;
        // A fresh database from template0 already has `public`, with the
        // template's explicit ACL (`{pg_database_owner=UC/...,=U/...}`,
        // measured on 18). A target whose `public` was dropped and recreated
        // has a NULL ACL instead, and keeping the template's schema would
        // leave those entries in place for a target with none to replay, so
        // the reproduction was refused (finding on #688). Every schema is
        // therefore dropped if present and created afresh under its mapped
        // owner: a new schema has a NULL ACL, which is the target's commonest
        // state exactly, and the one the replay below builds on. The scratch
        // database is this run's own, fresh from template0, so `public` holds
        // nothing to lose (findings on #610 and #688).
        admin
            .query(&format!("DROP SCHEMA IF EXISTS {}", quote_ident(name)))
            .await?;
        admin
            .query(&format!(
                "CREATE SCHEMA {} AUTHORIZATION {}",
                quote_ident(name),
                quote_ident(&owner),
            ))
            .await?;
        // A schema nobody granted anything on has a NULL ACL on the target —
        // the owner's implicit rights alone, read as no entries — and a schema
        // this run just created reads the same until something touches its
        // ACL. `REVOKE ... FROM PUBLIC` is such a touch: on a NULL ACL the
        // engine first materializes the owner's own entry (measured on 18),
        // which the target does not have, so the commonest schema of all
        // would never reproduce (finding on #688). It runs only where the
        // target has entries to reproduce, an ACL the engine materialized the
        // same way when the first of them was granted. That materialized
        // entry is the owner's full default, `UC`, but the target's owner
        // may have revoked part of its own — an ACL of `{owner=U/owner}` —
        // and a replay only adds, so the owner's entry is cleared to an
        // empty ACL first and comes back from the replay exactly as the
        // target has it (measured on 18; finding on #688).
        if !schema.acl.is_empty() {
            for holder in ["PUBLIC".to_owned(), quote_ident(&owner)] {
                admin
                    .query(&format!(
                        "REVOKE ALL ON SCHEMA {} FROM {holder}",
                        quote_ident(name)
                    ))
                    .await?;
            }
        }
        // Every entry is replayed under its own grantor, because a revoke is
        // grantor-specific: the deployer's planned revoke of a privilege it
        // granted on the target must find that privilege granted by its
        // mapped self here too, not by the administrator (finding on #688).
        // The owner needs nothing to grant; any other grantor must first hold
        // the grant option, so entries run in rounds until none is left, and
        // an entry whose grantor never becomes able to grant is a context
        // this reconstruction cannot reproduce — refused, not granted as
        // somebody else.
        let mut pending: Vec<(&String, &Grant)> = schema
            .acl
            .iter()
            .flat_map(|(grantee, grants)| grants.iter().map(move |grant| (grantee, grant)))
            .collect();
        let mut able: BTreeSet<(String, String)> = BTreeSet::new();
        while !pending.is_empty() {
            let (ready, waiting): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(_, grant)| {
                grant.grantor == schema.owner
                    || able.contains(&(grant.grantor.clone(), grant.privilege.clone()))
            });
            if ready.is_empty() {
                let stuck: Vec<String> = waiting
                    .iter()
                    .map(|(grantee, grant)| {
                        format!("{} to {grantee} by {}", grant.privilege, grant.grantor)
                    })
                    .collect();
                return Err(DbError::BadRow(format!(
                    "the ACL of schema {name} names a grantor that never holds the grant option: {}",
                    stuck.join(", ")
                )));
            }
            for (grantee, grant) in ready {
                let grantor = map
                    .run_local(&grant.grantor)
                    .ok_or_else(|| missing(&grant.grantor))?;
                let recipient = map.run_local(grantee).ok_or_else(|| missing(grantee))?;
                let recipient = if recipient == "PUBLIC" {
                    "PUBLIC".to_owned()
                } else {
                    quote_ident(&recipient)
                };
                admin
                    .query(&format!("SET ROLE {}", quote_ident(&grantor)))
                    .await?;
                let outcome = admin
                    .query(&format!(
                        "GRANT {} ON SCHEMA {} TO {}{}",
                        grant.privilege,
                        quote_ident(name),
                        recipient,
                        if grant.grantable {
                            " WITH GRANT OPTION"
                        } else {
                            ""
                        }
                    ))
                    .await;
                admin.query("RESET ROLE").await?;
                outcome?;
                if grant.grantable && grantee != "PUBLIC" {
                    able.insert((grantee.clone(), grant.privilege.clone()));
                }
            }
            pending = waiting;
        }
    }
    // Settings: role-, database- and database-role-scoped, on the run login
    // and this run's own database.
    for (key, value) in &context.settings {
        let (scope, name) = key
            .split_once(':')
            .ok_or_else(|| DbError::BadRow(format!("a setting key without a scope: {key}")))?;
        // Role- and database-role defaults are the target login's, and go on
        // the run login: it is what opens the scratch session, and PostgreSQL
        // applies role defaults at login and not on SET ROLE, so this is how
        // they load on the target and the only way they load here; reading
        // the reproduction back finds them under `session_user`. On the
        // mapped deployer they would apply nowhere, since it never logs in
        // (finding on #688).
        // An empty stored text — `ALTER ROLE ... SET session_preload_libraries
        // FROM CURRENT` while the list was empty stores `name=` — has no
        // spelling on the right of `=`: no arguments is a syntax error, and
        // `''` stores `""`, one library with an empty name that fails to load
        // at login (measured on 18). `FROM CURRENT` copies the session's
        // value text verbatim, so the admin session is given the empty text
        // first and the default is copied from it (finding on #688).
        let mut statements = Vec::new();
        let assignment = if value.is_empty() {
            statements.push(format!(
                "SELECT pg_catalog.set_config({}, '', false)",
                literal(name)
            ));
            "FROM CURRENT".to_owned()
        } else {
            format!("= {}", setting_value(name, value)?)
        };
        match scope {
            "role" => statements.push(format!(
                "ALTER ROLE {} SET {} {assignment}",
                quote_ident(&map.run_login),
                quote_ident(name),
            )),
            "database" => statements.push(format!(
                "ALTER DATABASE {} SET {} {assignment}",
                quote_ident(database),
                quote_ident(name),
            )),
            "database-role" => statements.push(format!(
                "ALTER ROLE {} IN DATABASE {} SET {} {assignment}",
                quote_ident(&map.run_login),
                quote_ident(database),
                quote_ident(name),
            )),
            other => {
                return Err(DbError::BadRow(format!("unknown setting scope {other}")));
            }
        }
        for statement in statements {
            admin.query(&statement).await?;
        }
    }
    Ok(())
}

/// Applies the plan's preceding grants to scratch, mapped to run-local roles,
/// so the deployer's effective privileges are what apply will act under.
pub async fn apply_planned(
    admin: &mut impl QueryConnection,
    map: &RoleMap,
    deployer: &str,
    grants: &[PlannedGrant],
) -> Result<(), DbError> {
    if grants.is_empty() {
        return Ok(());
    }
    // Run the plan's grants as the mapped deployer, not the reconstruction
    // administrator: a grant the deployer lacks ownership or grant option for
    // must fail here exactly as it would at apply (finding on #688).
    admin
        .query(&format!("SET ROLE {}", quote_ident(deployer)))
        .await?;
    let outcome = apply_planned_grants(admin, map, grants).await;
    admin.query("RESET ROLE").await?;
    outcome
}

async fn apply_planned_grants(
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
/// §7.6). Each grant or revoke is recorded under the grantor the engine
/// would record — a revoke takes only what that grantor granted, and the
/// option with it — and the deployer's effective schema answers are then
/// recomputed from the updated ACL, ownership and membership, the same
/// inputs the engine uses for `has_schema_privilege`.
pub fn with_planned(
    mut context: AuthorizationContext,
    grants: &[PlannedGrant],
) -> AuthorizationContext {
    for grant in grants {
        // A grant the deployer holds no option for is still recorded, as the
        // deployer's own: the engine would refuse or ignore it, so the
        // reproduction lacks it, and the difference refuses the plan rather
        // than certifying a grant that did not take.
        let grantor = grantor_for(&context, &grant.schema, &grant.privilege)
            .unwrap_or_else(|| context.principal.effective.clone());
        let Some(schema) = context.schemas.get_mut(&grant.schema) else {
            continue;
        };
        // A schema nobody granted anything on has a NULL ACL, read as no
        // entries. The engine's first grant or revoke on it materializes the
        // owner's default entries beside whatever it changes (measured on 18:
        // `{owner=UC/owner,=U/owner}` after one `GRANT USAGE ... TO PUBLIC`),
        // so the expected context must gain them too, or a valid first grant
        // reads as an unexpected owner entry on scratch (finding on #688).
        if schema.acl.is_empty() {
            let owner = schema.owner.clone();
            let mut defaults: Vec<Grant> = ["CREATE", "USAGE"]
                .into_iter()
                .map(|privilege| Grant {
                    privilege: privilege.into(),
                    grantable: false,
                    grantor: owner.clone(),
                })
                .collect();
            defaults.sort();
            schema.acl.insert(owner, defaults);
        }
        let entry = schema.acl.entry(grant.role.clone()).or_default();
        let had_option = entry
            .iter()
            .any(|g| g.privilege == grant.privilege && g.grantor == grantor && g.grantable);
        entry.retain(|g| !(g.privilege == grant.privilege && g.grantor == grantor));
        if !grant.revoke {
            // A plain re-grant to a holder of the option changes nothing on
            // the engine, so the option stays.
            entry.push(Grant {
                privilege: grant.privilege.clone(),
                grantable: had_option,
                grantor: grantor.clone(),
            });
            entry.sort();
        }
        if entry.is_empty() {
            schema.acl.remove(&grant.role);
        }
    }
    recompute_schema_effective(&mut context);
    context
}

/// The role the engine records as grantor when the deployer grants or
/// revokes `privilege` on `schema`: the owner when the deployer is the owner,
/// a superuser, or inherits the owner; otherwise the deployer itself when it
/// holds the option directly, or else the inherited role it holds it through.
/// `None` when it holds no option at all.
fn grantor_for(context: &AuthorizationContext, schema: &str, privilege: &str) -> Option<String> {
    let deployer = &context.principal.effective;
    let schema = context.schemas.get(schema)?;
    let inherited: BTreeSet<&String> = context
        .roles
        .iter()
        .filter(|(_, attrs)| attrs.inherits)
        .map(|(role, _)| role)
        .collect();
    if context.principal.superuser || &schema.owner == deployer || inherited.contains(&schema.owner)
    {
        return Some(schema.owner.clone());
    }
    let holds = |role: &String| {
        schema.acl.get(role).is_some_and(|grants| {
            grants
                .iter()
                .any(|g| g.privilege == privilege && g.grantable)
        })
    };
    if holds(deployer) {
        return Some(deployer.clone());
    }
    inherited.into_iter().find(|role| holds(role)).cloned()
}

/// The planned grants whose grantor the engine would choose among several
/// inherited option holders. `grantor_for` names one of them, but the
/// engine's `select_best_grantor` walks the deployer's memberships in
/// catalog order — measured on 18: of two inherited roles both holding
/// `USAGE` with the option, the one created first is recorded, not the
/// first by name — and the reproduction's roles are new catalog identities,
/// so which one the target would record cannot be predicted from here. Such
/// a context is refused before anything is built rather than guessed at: a
/// wrong guess reads as an unexpected grantor on scratch and refuses the
/// plan anyway, or fingerprints it under a grantor the target would not
/// record (finding on #688). Each entry names the grant and the holders.
pub fn ambiguous_grantors(context: &AuthorizationContext, grants: &[PlannedGrant]) -> Vec<String> {
    let deployer = &context.principal.effective;
    let inherited: BTreeSet<&String> = context
        .roles
        .iter()
        .filter(|(_, attrs)| attrs.inherits)
        .map(|(role, _)| role)
        .collect();
    let mut seen = BTreeSet::new();
    let mut ambiguous = Vec::new();
    for grant in grants {
        if !seen.insert((grant.schema.clone(), grant.privilege.clone())) {
            continue;
        }
        let Some(schema) = context.schemas.get(&grant.schema) else {
            continue;
        };
        if context.principal.superuser
            || &schema.owner == deployer
            || inherited.contains(&schema.owner)
        {
            continue;
        }
        let holds = |role: &String| {
            schema.acl.get(role).is_some_and(|grants| {
                grants
                    .iter()
                    .any(|g| g.privilege == grant.privilege && g.grantable)
            })
        };
        if holds(deployer) {
            continue;
        }
        let holders: Vec<&str> = inherited
            .iter()
            .filter(|role| holds(role))
            .map(|role| role.as_str())
            .collect();
        if holders.len() > 1 {
            ambiguous.push(format!(
                "{} on schema {} is held with the grant option through {}",
                grant.privilege,
                grant.schema,
                holders.join(" and ")
            ));
        }
    }
    ambiguous
}

/// Recomputes the deployer's effective USAGE/CREATE for each schema from the
/// ACL, ownership and the deployer's role closure, the way the engine answers
/// `has_schema_privilege`: a superuser has everything; on a NULL ACL (read
/// as no entries) the owner, or a deployer inheriting the owner, has
/// everything implicitly; once the ACL has entries the owner's own ordinary
/// privileges are the entries recorded for it and nothing more — measured on
/// 18, `{owner=U/owner}` answers CREATE false for the owner itself. An
/// ownership shortcut that granted both regardless expected CREATE where the
/// owner had revoked it from itself, and a reproduction that faithfully kept
/// the revoke was refused (finding on #688). Otherwise a privilege is held
/// through PUBLIC, the deployer itself, or a role it inherits.
fn recompute_schema_effective(context: &mut AuthorizationContext) {
    let deployer = context.principal.effective.clone();
    let superuser = context.principal.superuser;
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
        let owns_default = schema.acl.is_empty()
            && (schema.owner == deployer || inherited.contains(&schema.owner));
        for privilege in ["USAGE", "CREATE"] {
            let granted = schema.acl.iter().any(|(grantee, grants)| {
                holders(grantee) && grants.iter().any(|g| g.privilege == privilege)
            });
            schema
                .privileges
                .insert(privilege.to_owned(), superuser || owns_default || granted);
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
        if got.privileges != want.privileges {
            differences.push(format!("schema:{name}:privileges"));
        }
        // A system schema is the engine's own on both sides: its owner is
        // each installation's bootstrap superuser and its ACL is not
        // replayed, so only the deployer's effective privileges above are
        // the reproduction's to match.
        if is_system_schema(name) {
            continue;
        }
        if map.logical_of(&got.owner).as_deref() != Some(want.owner.as_str()) {
            differences.push(format!("schema:{name}:owner"));
        }
        let got_acl: BTreeMap<String, Vec<Grant>> = got
            .acl
            .iter()
            .map(|(grantee, grants)| {
                let mut grants: Vec<Grant> = grants
                    .iter()
                    .map(|g| Grant {
                        privilege: g.privilege.clone(),
                        grantable: g.grantable,
                        grantor: map
                            .logical_of(&g.grantor)
                            .unwrap_or_else(|| g.grantor.clone()),
                    })
                    .collect();
                grants.sort();
                (
                    map.logical_of(grantee).unwrap_or_else(|| grantee.clone()),
                    grants,
                )
            })
            .collect();
        if got_acl != want.acl {
            differences.push(format!("schema:{name}:acl"));
        }
    }
    // Role attributes, mapped back to logical names: a reproduction must give
    // the deployer the same closure with the same superuser/inherit/switch
    // standing, or a privilege answer could match today and diverge later.
    let got_roles: BTreeMap<String, &RoleAttributes> = reproduced
        .roles
        .iter()
        .filter_map(|(name, attrs)| map.logical_of(name).map(|logical| (logical, attrs)))
        .collect();
    for (name, want) in &target.roles {
        match got_roles.get(name) {
            Some(got) if *got == want => {}
            Some(_) => differences.push(format!("role:{name}")),
            None => differences.push(format!("role:{name}:absent")),
        }
    }
    // And in the other direction: a role the reproduction still has and the
    // target no longer does — a membership revoked between checks — is a
    // broader authorization than the target's, not a match (finding on #688).
    for name in got_roles.keys() {
        if !target.roles.contains_key(name) {
            differences.push(format!("role:{name}:extra"));
        }
    }
    // Role- and database-scoped settings must reproduce exactly; their keys
    // name a scope and GUC, not a role, so they compare directly.
    if reproduced.settings != target.settings {
        differences.push("settings".into());
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

    #[test]
    fn every_compared_setting_is_part_of_the_context() {
        for name in super::super::compatibility::SETTINGS
            .iter()
            .chain(super::super::compatibility::OPTIONAL_SETTINGS)
            .chain(&["search_path"])
        {
            assert!(is_context_setting(name), "{name}");
        }
        assert!(!is_context_setting("work_mem"));
        assert!(
            !is_context_setting("datestyle"),
            "the stored spelling is the canonical one"
        );
    }

    #[test]
    fn a_list_setting_is_replayed_element_by_element() {
        // Measured on 18: the stored text of
        // `ALTER ROLE r SET search_path = "$user", public, "odd name"`,
        // and the literal-per-element form that stores the same text again.
        assert_eq!(
            setting_value("search_path", r#""$user", public, "odd name""#).unwrap(),
            "'$user', 'public', 'odd name'"
        );
        assert_eq!(
            setting_value("session_preload_libraries", r#""foo,bar", auto_explain"#).unwrap(),
            "'foo,bar', 'auto_explain'"
        );
        // A scalar, and a list the engine does not quote, keep the whole
        // text as one literal; a quote in it cannot end the literal.
        assert_eq!(
            setting_value("DateStyle", "ISO, MDY").unwrap(),
            "'ISO, MDY'"
        );
        assert_eq!(
            setting_value("default_text_search_config", "it's").unwrap(),
            "'it''s'"
        );
        // A stored list the engine could not read is refused, not replayed.
        assert!(setting_value("search_path", r#""open"#).is_err());
    }

    fn grant(privilege: &str, grantable: bool, grantor: &str) -> Grant {
        Grant {
            privilege: privilege.into(),
            grantable,
            grantor: grantor.into(),
        }
    }

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
                    acl: [(
                        "app_reader".to_owned(),
                        vec![grant("USAGE", false, "app_owner")],
                    )]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
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
    fn a_predefined_role_is_granted_not_cloned() {
        let mut ctx = context();
        for role in ["pg_read_all_settings", "pg_database_owner"] {
            ctx.roles.insert(
                role.to_owned(),
                RoleAttributes {
                    superuser: false,
                    inherit: true,
                    bypass_rls: false,
                    can_set: true,
                    inherits: true,
                },
            );
        }
        let map = RoleMap::generate(&ctx, &[], "pbps_run_x", "tok");
        // The capability role keeps its identity both ways; the database
        // owner pseudo-role, which cannot have members, is cloned like any
        // other; and cleanup never names a predefined role.
        assert_eq!(
            map.run_local("pg_read_all_settings").as_deref(),
            Some("pg_read_all_settings")
        );
        assert_eq!(
            map.logical_of("pg_read_all_settings").as_deref(),
            Some("pg_read_all_settings")
        );
        assert!(
            map.run_local("pg_database_owner")
                .is_some_and(|run| run.starts_with("pbps_role_"))
        );
        assert!(
            map.run_local("dep")
                .is_some_and(|run| run.starts_with("pbps_role_"))
        );
        assert!(
            map.run_local_names()
                .iter()
                .all(|name| name.starts_with("pbps_role_")),
            "{:?}",
            map.run_local_names()
        );
    }

    /// Measured on 18: with two inherited roles both holding the option, the
    /// engine records the one created first (`zz_first`, the lower oid), not
    /// the first by name (`aa_second`); the reproduction cannot know which,
    /// so the context is refused, while one holder or the owner is not.
    #[test]
    fn a_grant_with_several_inherited_option_holders_is_refused_not_guessed() {
        let mut two = context();
        for role in ["aa_second", "zz_first"] {
            two.roles.insert(
                role.to_owned(),
                RoleAttributes {
                    superuser: false,
                    inherit: true,
                    bypass_rls: false,
                    can_set: true,
                    inherits: true,
                },
            );
            two.schemas
                .get_mut("app")
                .unwrap()
                .acl
                .insert(role.to_owned(), vec![grant("USAGE", true, "app_owner")]);
        }
        let planned = [PlannedGrant {
            role: "app_reader".into(),
            schema: "app".into(),
            privilege: "USAGE".into(),
            revoke: false,
        }];
        let refused = ambiguous_grantors(&two, &planned);
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("aa_second and zz_first"), "{refused:?}");
        // One holder is not ambiguous; nor is a deployer that holds the
        // option itself, owns the schema, or is a superuser; and a revoke of
        // a privilege nobody holds the option for is not a grantor question.
        let mut one = two.clone();
        one.schemas.get_mut("app").unwrap().acl.remove("aa_second");
        assert!(ambiguous_grantors(&one, &planned).is_empty());
        let mut direct = two.clone();
        direct
            .schemas
            .get_mut("app")
            .unwrap()
            .acl
            .insert("dep".into(), vec![grant("USAGE", true, "app_owner")]);
        assert!(ambiguous_grantors(&direct, &planned).is_empty());
        let mut owns = two.clone();
        owns.schemas.get_mut("app").unwrap().owner = "dep".into();
        assert!(ambiguous_grantors(&owns, &planned).is_empty());
        let mut superuser = two.clone();
        superuser.principal.superuser = true;
        assert!(ambiguous_grantors(&superuser, &planned).is_empty());
        let create = [PlannedGrant {
            role: "app_reader".into(),
            schema: "app".into(),
            privilege: "CREATE".into(),
            revoke: true,
        }];
        assert!(ambiguous_grantors(&two, &create).is_empty());
    }

    /// The engine's owner rule, as `has_schema_privilege` answers it
    /// (measured on 18): everything on a NULL ACL, only the recorded entries
    /// once the ACL has any, and everything for a superuser regardless.
    #[test]
    fn an_owner_keeps_only_what_its_acl_records_once_the_acl_has_entries() {
        let mut owned = context();
        let schema = owned.schemas.get_mut("app").unwrap();
        schema.owner = "dep".into();
        // `REVOKE CREATE ON SCHEMA app FROM dep` by the owner itself leaves
        // `{dep=U/dep}`, and the engine reads CREATE false for the owner.
        schema.acl = [("dep".to_owned(), vec![grant("USAGE", false, "dep")])]
            .into_iter()
            .collect();
        schema.privileges = [("USAGE".to_owned(), true), ("CREATE".to_owned(), false)]
            .into_iter()
            .collect();
        // No planned grants, and a planned grant to somebody else: neither
        // restores what the owner revoked from itself.
        let unchanged = with_planned(owned.clone(), &[]);
        assert!(unchanged.schemas["app"].privileges["USAGE"]);
        assert!(!unchanged.schemas["app"].privileges["CREATE"]);
        let granted = with_planned(
            owned.clone(),
            &[PlannedGrant {
                role: "app_reader".into(),
                schema: "app".into(),
                privilege: "USAGE".into(),
                revoke: false,
            }],
        );
        assert!(!granted.schemas["app"].privileges["CREATE"]);
        assert_eq!(
            granted.schemas["app"].acl["app_reader"],
            vec![grant("USAGE", false, "dep")]
        );
        // A NULL ACL is the owner's implicit everything, and a superuser has
        // it whatever the ACL says.
        let mut fresh = owned.clone();
        fresh.schemas.get_mut("app").unwrap().acl.clear();
        assert!(with_planned(fresh, &[]).schemas["app"].privileges["CREATE"]);
        owned.principal.superuser = true;
        assert!(with_planned(owned, &[]).schemas["app"].privileges["CREATE"]);
    }

    /// Planned grants and revokes are recorded under the grantor the engine
    /// would record, so the expected context matches a reproduction that
    /// replayed each entry under its grantor: a deployer's revoke takes only
    /// what the deployer granted; an owner's takes the owner's; a grant the
    /// deployer holds no option for is recorded as its own, so the
    /// reproduction differs and the plan is refused; and a holder's
    /// grant-option entry is still the privilege it names.
    #[test]
    fn planned_grants_and_revokes_follow_the_engines_grantor_rules() {
        let planned = |privilege: &str, role: &str, revoke: bool| PlannedGrant {
            role: role.into(),
            schema: "app".into(),
            privilege: privilege.into(),
            revoke,
        };
        // The deployer holds USAGE with the option from the owner, and both
        // it and the owner granted USAGE to the reader.
        let mut base = context();
        base.roles.clear();
        base.schemas.get_mut("app").unwrap().acl = [
            ("dep".to_owned(), vec![grant("USAGE", true, "app_owner")]),
            (
                "app_reader".to_owned(),
                vec![
                    grant("USAGE", false, "app_owner"),
                    grant("USAGE", false, "dep"),
                ],
            ),
        ]
        .into_iter()
        .collect();
        let same = with_planned(base.clone(), &[]);
        assert!(
            same.schemas["app"].privileges["USAGE"],
            "a grant-option entry lost its privilege"
        );
        // The deployer's revoke removes its own grant and leaves the owner's.
        let revoked = with_planned(base.clone(), &[planned("USAGE", "app_reader", true)]);
        assert_eq!(
            revoked.schemas["app"].acl["app_reader"],
            vec![grant("USAGE", false, "app_owner")]
        );
        // The deployer re-granting to itself adds an item under its own
        // grantorship beside the owner's, as the engine keys ACL items by
        // grantor (measured on 18); the owner's option entry stays.
        let regranted = with_planned(base.clone(), &[planned("USAGE", "dep", false)]);
        assert_eq!(
            regranted.schemas["app"].acl["dep"],
            vec![
                grant("USAGE", false, "dep"),
                grant("USAGE", true, "app_owner")
            ]
        );
        // CREATE, which the deployer holds no option for, is recorded as the
        // deployer's own grant: the reproduction will not have it.
        let unauthorized = with_planned(base.clone(), &[planned("CREATE", "app_reader", false)]);
        assert!(
            unauthorized.schemas["app"].acl["app_reader"].contains(&grant("CREATE", false, "dep"))
        );
        // As the owner, the deployer's revoke takes the owner's grant instead.
        let mut as_owner = base;
        as_owner.principal.effective = "app_owner".into();
        let owner_revoked = with_planned(as_owner, &[planned("USAGE", "app_reader", true)]);
        assert_eq!(
            owner_revoked.schemas["app"].acl["app_reader"],
            vec![grant("USAGE", false, "dep")]
        );
    }

    /// The first planned grant on a schema with no explicit grants gains
    /// the owner's default entries beside the granted one, as the engine
    /// materializes them (measured on 18); a revoke on one gains them and
    /// changes nothing else; a schema that already has entries gains none.
    #[test]
    fn a_first_planned_grant_on_a_default_acl_materializes_the_owners_entries() {
        let planned = |role: &str, revoke: bool| PlannedGrant {
            role: role.into(),
            schema: "app".into(),
            privilege: "USAGE".into(),
            revoke,
        };
        let mut base = context();
        base.principal.effective = "app_owner".into();
        base.roles.clear();
        base.schemas.get_mut("app").unwrap().acl.clear();
        let defaults = vec![
            grant("CREATE", false, "app_owner"),
            grant("USAGE", false, "app_owner"),
        ];
        let granted = with_planned(base.clone(), &[planned("app_reader", false)]);
        assert_eq!(granted.schemas["app"].acl["app_owner"], defaults);
        assert_eq!(
            granted.schemas["app"].acl["app_reader"],
            vec![grant("USAGE", false, "app_owner")]
        );
        let revoked = with_planned(base.clone(), &[planned("app_reader", true)]);
        assert_eq!(revoked.schemas["app"].acl.len(), 1);
        assert_eq!(revoked.schemas["app"].acl["app_owner"], defaults);
        // Already explicit: the owner's entry is whatever the target has,
        // here none, and a grant adds only itself.
        let mut explicit = base;
        explicit
            .schemas
            .get_mut("app")
            .unwrap()
            .acl
            .insert("other".into(), vec![grant("USAGE", false, "app_owner")]);
        let added = with_planned(explicit, &[planned("app_reader", false)]);
        assert!(!added.schemas["app"].acl.contains_key("app_owner"));
        assert_eq!(added.schemas["app"].acl.len(), 2);
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
