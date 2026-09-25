//! The deployment authorization context for SQL Server, and its reproduction
//! on a scratch database (ADR-0016 case 16; SPEC §9.3.3).
//!
//! The context is read **as the deployer**, over the planning connection,
//! and every effective answer in it is the engine's own: `fn_my_permissions`
//! for what the deployer may do on the database and on each in-scope schema,
//! `IS_ROLEMEMBER` for the roles it is in. Nothing here re-derives an
//! effective permission from grant rows; DENY, role nesting, ownership and
//! fixed roles are the engine's to combine, and a second implementation of
//! that would be a second thing to keep right.
//!
//! The grant rows are read too, because they are what a reproduction replays.
//! A login sees only the rows that concern it (measured on 17.0: its own, its
//! roles', and those of principals it holds a permission on), which is
//! exactly the set that shapes its effective answers, so the replay of what
//! the deployer can see is checked against what the deployer can do.
//!
//! On scratch the principals are run-local: users `WITHOUT LOGIN` and roles
//! named `pbps_principal_<n>_<token>`, database-scoped, so they go with the
//! scratch database and no production login, SID or secret is copied. The
//! engine's own principals — `dbo`, `public`, `guest`, `sys`,
//! `INFORMATION_SCHEMA` and the fixed `db_` roles — keep their identity: a
//! clone of `db_ddladmin` would carry none of what membership in it grants
//! (DECISIONS 521).
//!
//! Unreadable is not empty: a schema that is not there, or a principal row
//! that does not come back, is an error, never a context with nothing in it.

use pbps_db::resolver::environment::DeploymentPrincipal;
use pbps_db::transport::QueryConnection;
use pbps_db::{DbError, Row};
use std::collections::{BTreeMap, BTreeSet};

/// The versioned name of this authorization rule, measured on SQL Server
/// 2025 (17.0).
pub const RULE: &str = "mssql-auth-v1";

/// Principals every database has, which a reproduction refers to by their
/// own name instead of cloning.
const BUILT_IN: &[&str] = &["dbo", "public", "guest", "sys", "INFORMATION_SCHEMA"];

/// Schemas every database has and the engine owns. They are neither created
/// nor given a replayed ACL on scratch; the deployer's effective permissions
/// on them are still read and compared.
const SYSTEM_SCHEMAS: &[&str] = &["sys", "INFORMATION_SCHEMA", "guest"];

/// One permission row as `sys.database_permissions` reports it. `state` is
/// `GRANT`, `GRANT_WITH_GRANT_OPTION` or `DENY`; the grantor is kept because
/// a revoke takes back only what that grantor granted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Grant {
    pub grantee: String,
    pub grantor: String,
    pub permission: String,
    pub state: String,
}

/// What one in-scope schema grants: its owner, the deployer's effective
/// permissions on it (the engine's answer), and the grant rows the deployer
/// can see (for reconstruction).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaAuthorization {
    pub owner: String,
    pub permissions: BTreeSet<String>,
    pub grants: Vec<Grant>,
}

/// Whether a referenced principal is a role or a user, which decides how it
/// is created on scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrincipalKind {
    User,
    Role,
}

/// The full context, canonicalised deterministically for the fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuthorizationContext {
    /// `login` is `ORIGINAL_LOGIN()`, `effective` the database user statements
    /// run as, and `superuser` whether that user is `dbo` — the database
    /// owner, which a member of `sysadmin` enters every database as.
    pub principal: DeploymentPrincipal,
    /// Where an unqualified name in an ad hoc statement is looked up first.
    pub default_schema: String,
    /// The session language, which the login's default decides and which in
    /// turn decides the date format and the first day of the week.
    pub language: String,
    pub database_permissions: BTreeSet<String>,
    /// Database-level rows for the deployer and the roles it is in.
    pub database_grants: Vec<Grant>,
    pub schemas: BTreeMap<String, SchemaAuthorization>,
    /// Roles the deployer is a member of, by the engine's `IS_ROLEMEMBER`,
    /// and whether each is one of the fixed `db_` roles.
    pub roles: BTreeMap<String, bool>,
    /// Users the deployer may `EXECUTE AS`.
    pub impersonation: BTreeSet<String>,
    /// The kind of every principal the context names that the deployer can
    /// see; one it cannot see is an owner or grantor it is not a member of,
    /// and is reproduced as a user.
    pub principals: BTreeMap<String, PrincipalKind>,
    /// Each principal a planned grant names, keyed by the plan's spelling:
    /// the catalog's spelling of it, or `None` when the catalog has no such
    /// principal ([`resolve_spellings`]). Every name is recorded, the ones the
    /// catalog spells the same way included, so a principal that exists and
    /// one that does not are sealed apart (#1013). Empty for a read that was
    /// given no planned grants, and then left out of the canonical form.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub spellings: BTreeMap<String, Option<String>>,
}

impl AuthorizationContext {
    /// A stable byte string over every field, for hashing into a fingerprint.
    pub fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("authorization context serialises")
    }

    /// The catalog's name for a principal a planned grant names: the
    /// resolved spelling, or the name as planned when the catalog has no such
    /// principal yet.
    fn catalog_name<'a>(&'a self, planned: &'a str) -> &'a str {
        spelled(&self.spellings, planned)
    }
}

/// Resolves each principal the plan's grants name to the catalog's spelling
/// of it, as the deployer reading `context`, and records every answer in
/// `context.spellings` (#726) — a name the catalog does not know as `None`,
/// so its absence is sealed as distinctly as its presence (#1013).
///
/// On a case-insensitive database a role catalogued as `Readers` and a grant
/// planned to `readers` are one principal, and `PUBLIC` is `public`. Keyed as
/// planned they were two: the reconstruction made a second run-local
/// principal for the plan's spelling, the planned grant landed on it instead
/// of on the role, and a built-in name in another casing was cloned as a user.
/// Which spellings are one principal is the engine's answer, read here, not a
/// comparison repeated in Rust.
///
/// `USER_NAME(USER_ID(..))`, not a lookup in `sys.database_principals`: that
/// view shows a deployer only the principals it may see, so for an ordinary
/// deployer a role it merely grants to has no row and would stay as planned,
/// while the metadata functions answer for every principal (measured on 17.0,
/// as a user holding no permission). They are also what spells every grantee
/// in the context, so the two agree. A name with no principal behind it stays
/// as planned: the plan may be the one that creates it (DEC-726.1).
pub async fn resolve_spellings(
    conn: &mut impl QueryConnection,
    context: &mut AuthorizationContext,
    planned: &[String],
) -> Result<(), DbError> {
    for name in planned {
        let rows = conn
            .query(&format!(
                "SELECT USER_NAME(USER_ID({})) AS name;",
                literal(name)
            ))
            .await?;
        let [row] = rows.as_slice() else {
            return Err(DbError::BadRow(format!(
                "the catalog's spelling of the principal {name} expected one row"
            )));
        };
        let catalog = row.try_get::<&str>("name")?.map(str::to_owned);
        context.spellings.insert(name.clone(), catalog);
    }
    Ok(())
}

/// The catalog's name for `planned`, or `planned` itself when the catalog
/// has none (or the name was never resolved).
fn spelled<'a>(spellings: &'a BTreeMap<String, Option<String>>, planned: &'a str) -> &'a str {
    spellings
        .get(planned)
        .and_then(Option::as_deref)
        .unwrap_or(planned)
}

/// A planned authorization change the plan performs before its DDL: a schema
/// grant or revoke, the two forms the emitter produces. Applied on scratch as
/// the reproduced deployer, so one the deployer could not make on the target
/// does not take there either.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PlannedGrant {
    pub principal: String,
    pub schema: String,
    pub permission: String,
    pub revoke: bool,
}

fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

fn required(row: &Row, field: &str, what: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("the authorization context did not report {what}")))
}

/// Reads the context as the connection's current principal.
pub async fn read(
    conn: &mut impl QueryConnection,
    schemas: &[String],
) -> Result<AuthorizationContext, DbError> {
    let rows = conn
        .query(
            "SELECT ORIGINAL_LOGIN() AS login, USER_NAME() AS effective, \
                    (SELECT p.default_schema_name FROM sys.database_principals p \
                      WHERE p.principal_id = DATABASE_PRINCIPAL_ID()) AS default_schema, \
                    CONVERT(nvarchar(128), @@LANGUAGE) AS [language];",
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "the deployer principal expected one row".into(),
        ));
    };
    let effective = required(row, "effective", "the database user")?;
    let principal = DeploymentPrincipal {
        login: required(row, "login", "the login")?,
        superuser: effective == "dbo",
        effective,
    };
    // A user mapped from a Windows group has no default schema of its own;
    // the engine falls back to `dbo`, and so does the reproduction.
    let default_schema = row
        .try_get::<&str>("default_schema")?
        .unwrap_or("dbo")
        .to_owned();
    let language = required(row, "language", "the session language")?;

    let database_permissions = permissions(conn, "NULL", "DATABASE").await?;
    let mut roles = BTreeMap::new();
    // `dbo` is in no role and needs none; IS_ROLEMEMBER answers for the
    // database owner in ways that say nothing a reproduction could use.
    if !principal.superuser {
        for row in conn
            .query(
                "SELECT r.name AS name, CONVERT(nvarchar(8), r.is_fixed_role) AS fixed \
                 FROM sys.database_principals r \
                 WHERE r.type = 'R' AND r.name <> N'public' AND IS_ROLEMEMBER(r.name) = 1 \
                 ORDER BY r.name;",
            )
            .await?
        {
            roles.insert(
                required(&row, "name", "a role's name")?,
                required(&row, "fixed", "whether a role is fixed")? == "1",
            );
        }
    }

    // Everything the deployer holds a permission *through*: itself, its
    // roles, and `public`, which every user is in without being a member of
    // it — `IS_ROLEMEMBER` is not asked about it and the role list leaves it
    // out, since there is no membership to reproduce. Its grants still reach
    // the deployer (measured: a permission granted to `public` is in
    // `fn_my_permissions`), so left out of this list they were in the
    // effective answer and not in the rows replayed, and a scratch side
    // without them failed verification for an ordinary target (finding on
    // #611).
    let mut holders: BTreeSet<String> = roles.keys().cloned().collect();
    holders.insert(principal.effective.clone());
    holders.insert("public".to_owned());
    let holder_list = holders
        .iter()
        .map(|name| literal(name))
        .collect::<Vec<_>>()
        .join(", ");

    let mut context = AuthorizationContext {
        principal,
        default_schema,
        language,
        database_permissions,
        database_grants: Vec::new(),
        schemas: BTreeMap::new(),
        roles,
        impersonation: BTreeSet::new(),
        principals: BTreeMap::new(),
        spellings: BTreeMap::new(),
    };
    if !context.principal.superuser {
        context.database_grants = grants(
            conn,
            &format!(
                "p.class = 0 AND USER_NAME(p.grantee_principal_id) IN ({holder_list}) \
                 AND p.permission_name <> N'CONNECT'"
            ),
        )
        .await?;
        for row in conn
            .query(&format!(
                "SELECT USER_NAME(p.major_id) AS target FROM sys.database_permissions p \
                 WHERE p.class = 4 AND p.permission_name = N'IMPERSONATE' AND p.state IN ('G', 'W') \
                   AND USER_NAME(p.grantee_principal_id) IN ({holder_list}) ORDER BY 1;"
            ))
            .await?
        {
            context
                .impersonation
                .insert(required(&row, "target", "an impersonation target")?);
        }
    }
    for schema in schemas {
        let rows = conn
            .query(&format!(
                "SELECT s.name AS name, USER_NAME(s.principal_id) AS owner FROM sys.schemas s \
                 WHERE s.name = {};",
                literal(schema)
            ))
            .await?;
        // The schema is keyed by the name the catalog holds, not by the
        // spelling it was asked for. The lookup above compares under the
        // database's collation, so on a case-insensitive database `DBO`
        // finds `dbo` — and keyed as `DBO` it was no longer recognised as
        // the schema every database already has, the reconstruction ran
        // `CREATE SCHEMA [DBO]` into a scratch database of the same
        // collation, and a valid request ended on the engine's error 2760
        // (measured; finding on #611). Which names are one schema is the engine's
        // answer, read here once, rather than a comparison repeated in Rust.
        let (name, owner) = match rows.as_slice() {
            [row] => (
                required(row, "name", "a schema's name")?,
                required(row, "owner", "a schema's owner")?,
            ),
            // Every login sees every schema's row, so none is absence — and
            // an absent in-scope schema is a scope that cannot be established.
            [] => {
                return Err(DbError::BadRow(format!(
                    "the in-scope schema {schema} does not exist"
                )));
            }
            _ => {
                return Err(DbError::BadRow(format!(
                    "the in-scope schema {schema} is ambiguous"
                )));
            }
        };
        let permissions = permissions(conn, &literal(schema), "SCHEMA").await?;
        // For every deployer, `dbo` included: `dbo` sees every row, and a
        // grant another session makes on an in-scope schema is a target that
        // is no longer what was sealed, whoever the deployer is.
        let grants = grants(
            conn,
            &format!(
                "p.class = 3 AND p.major_id = SCHEMA_ID({})",
                literal(schema)
            ),
        )
        .await?;
        context.schemas.insert(
            name,
            SchemaAuthorization {
                owner,
                permissions,
                grants,
            },
        );
    }

    // The kind of each principal the context names, where the deployer can
    // see it.
    let named = named_principals(&context, &[]);
    for row in conn
        .query("SELECT p.name AS name, p.type AS kind FROM sys.database_principals p;")
        .await?
    {
        let name = required(&row, "name", "a principal's name")?;
        if named.contains(&name) {
            let kind = match required(&row, "kind", "a principal's type")?.as_str() {
                "R" => PrincipalKind::Role,
                _ => PrincipalKind::User,
            };
            context.principals.insert(name, kind);
        }
    }
    Ok(context)
}

/// The engine's own list of what the current principal may do on a
/// securable, DENY, role nesting and ownership already applied.
async fn permissions(
    conn: &mut impl QueryConnection,
    securable: &str,
    class: &str,
) -> Result<BTreeSet<String>, DbError> {
    let mut found = BTreeSet::new();
    for row in conn
        .query(&format!(
            "SELECT permission_name AS permission FROM fn_my_permissions({securable}, '{class}') \
             WHERE subentity_name = N'';"
        ))
        .await?
    {
        found.insert(required(&row, "permission", "a permission's name")?);
    }
    Ok(found)
}

async fn grants(conn: &mut impl QueryConnection, filter: &str) -> Result<Vec<Grant>, DbError> {
    let mut found = Vec::new();
    for row in conn
        .query(&format!(
            "SELECT USER_NAME(p.grantee_principal_id) AS grantee, \
                    USER_NAME(p.grantor_principal_id) AS grantor, \
                    p.permission_name AS permission, p.state_desc AS state \
             FROM sys.database_permissions p WHERE {filter};"
        ))
        .await?
    {
        found.push(Grant {
            grantee: required(&row, "grantee", "a grantee")?,
            grantor: required(&row, "grantor", "a grantor")?,
            permission: required(&row, "permission", "a permission")?,
            state: required(&row, "state", "a permission's state")?,
        });
    }
    found.sort();
    Ok(found)
}

/// Every principal the context or the plan names.
fn named_principals(context: &AuthorizationContext, planned: &[PlannedGrant]) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    named.insert(context.principal.effective.clone());
    named.extend(context.roles.keys().cloned());
    named.extend(context.impersonation.iter().cloned());
    for grant in &context.database_grants {
        named.insert(grant.grantee.clone());
        named.insert(grant.grantor.clone());
    }
    for schema in context.schemas.values() {
        named.insert(schema.owner.clone());
        for grant in &schema.grants {
            named.insert(grant.grantee.clone());
            named.insert(grant.grantor.clone());
        }
    }
    named.extend(
        planned
            .iter()
            .map(|grant| context.catalog_name(&grant.principal).to_owned()),
    );
    named
}

/// Whether a principal keeps its own name on scratch: the engine's built-in
/// principals and the fixed roles, whose meaning is their identity.
fn is_built_in(context: &AuthorizationContext, name: &str) -> bool {
    BUILT_IN.contains(&name) || context.roles.get(name) == Some(&true) || is_fixed_role(name)
}

fn is_fixed_role(name: &str) -> bool {
    matches!(
        name,
        "db_owner"
            | "db_accessadmin"
            | "db_securityadmin"
            | "db_ddladmin"
            | "db_backupoperator"
            | "db_datareader"
            | "db_datawriter"
            | "db_denydatareader"
            | "db_denydatawriter"
    )
}

/// Each logical principal mapped to a unique one this run creates in its own
/// scratch database, so nothing that names a production principal exists
/// there. Built-in principals map to themselves.
#[derive(Debug, Clone)]
pub struct PrincipalMap {
    to_run_local: BTreeMap<String, String>,
    /// The plan's spelling of a principal to the catalog's, so a planned
    /// grant finds the principal it names (#726).
    spellings: BTreeMap<String, Option<String>>,
}

impl PrincipalMap {
    /// `token` is the run's unique suffix; run-local principals are
    /// `pbps_principal_<n>_<token>`.
    pub fn generate(context: &AuthorizationContext, planned: &[PlannedGrant], token: &str) -> Self {
        let to_run_local = named_principals(context, planned)
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let run_local = if is_built_in(context, &name) {
                    name.clone()
                } else {
                    format!("pbps_principal_{index}_{token}")
                };
                (name, run_local)
            })
            .collect();
        Self {
            to_run_local,
            spellings: context.spellings.clone(),
        }
    }

    fn run_local(&self, logical: &str) -> Option<String> {
        let logical = spelled(&self.spellings, logical);
        self.to_run_local.get(logical).cloned()
    }

    fn logical_of(&self, run_local: &str) -> Option<String> {
        self.to_run_local
            .iter()
            .find(|(_, run)| *run == run_local)
            .map(|(logical, _)| logical.clone())
    }

    /// Every principal this run creates. They are database-scoped and go
    /// with the scratch database, so this is for the record, not for cleanup.
    pub fn run_local_names(&self) -> Vec<String> {
        self.to_run_local
            .iter()
            .filter(|(logical, run)| logical != run)
            .map(|(_, run)| run.clone())
            .collect()
    }

    /// The scratch user the compilation must `EXECUTE AS`, or `None` when
    /// the deployer is `dbo`: the run login owns its scratch database and is
    /// already `dbo` there.
    pub fn deployer(&self, context: &AuthorizationContext) -> Option<String> {
        if context.principal.superuser {
            return None;
        }
        self.run_local(&context.principal.effective)
    }
}

/// Creates the run-local principals, memberships, schemas and grants that
/// reproduce the deployer's authorization in the scratch database, through
/// an administrative connection *in that database*. `run_login` is the
/// scratch login, whose default language is what the scratch session speaks.
pub async fn reconstruct(
    admin: &mut impl QueryConnection,
    map: &PrincipalMap,
    context: &AuthorizationContext,
    run_login: &str,
) -> Result<(), DbError> {
    let missing = |logical: &str| {
        DbError::BadRow(format!(
            "the authorization reconstruction has no run-local principal for {logical}"
        ))
    };
    for (logical, run) in &map.to_run_local {
        if logical == run {
            continue;
        }
        let statement = match context.principals.get(logical) {
            Some(PrincipalKind::Role) => format!("CREATE ROLE {};", bracket(run)),
            Some(PrincipalKind::User) | None => {
                format!("CREATE USER {} WITHOUT LOGIN;", bracket(run))
            }
        };
        admin.query(&statement).await?;
    }
    if let Some(deployer) = map.deployer(context) {
        admin
            .query(&format!(
                "ALTER USER {} WITH DEFAULT_SCHEMA = {};",
                bracket(&deployer),
                bracket(&context.default_schema)
            ))
            .await?;
        for role in context.roles.keys() {
            let role = map.run_local(role).ok_or_else(|| missing(role))?;
            admin
                .query(&format!(
                    "ALTER ROLE {} ADD MEMBER {};",
                    bracket(&role),
                    bracket(&deployer)
                ))
                .await?;
        }
        for target in &context.impersonation {
            let target = map.run_local(target).ok_or_else(|| missing(target))?;
            admin
                .query(&format!(
                    "GRANT IMPERSONATE ON USER::{} TO {};",
                    bracket(&target),
                    bracket(&deployer)
                ))
                .await?;
        }
    }
    for (name, schema) in &context.schemas {
        if SYSTEM_SCHEMAS.contains(&name.as_str()) {
            continue;
        }
        let owner = map
            .run_local(&schema.owner)
            .ok_or_else(|| missing(&schema.owner))?;
        // `dbo` is in every database and cannot be created; a target whose
        // `dbo` schema changed hands has the change replayed instead.
        let statement = if name == "dbo" {
            format!(
                "ALTER AUTHORIZATION ON SCHEMA::[dbo] TO {};",
                bracket(&owner)
            )
        } else {
            format!(
                "CREATE SCHEMA {} AUTHORIZATION {};",
                bracket(name),
                bracket(&owner)
            )
        };
        if !(name == "dbo" && owner == "dbo") {
            admin.query(&statement).await?;
        }
        replay(admin, map, &schema.owner, &schema.grants, Some(name)).await?;
    }
    replay(admin, map, "dbo", &context.database_grants, None).await?;
    // The scratch session speaks the run login's default language, as the
    // deployment session speaks the deployment login's.
    admin
        .query(&format!(
            "ALTER LOGIN {} WITH DEFAULT_LANGUAGE = {};",
            bracket(run_login),
            bracket(&context.language)
        ))
        .await?;
    Ok(())
}

/// One statement of a replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step<'a> {
    /// The row itself, under its own grantor.
    Row(&'a Grant),
    /// The grant option a grantor must hold before its rows can be replayed,
    /// which the deployment session could not see on the target.
    Enable {
        grantor: &'a str,
        permission: &'a str,
    },
}

/// The order grant rows can be replayed in under their own grantors.
///
/// A grantor other than the securable's owner must hold the permission with
/// the grant option first, so rows run in rounds. Whether it does is usually
/// **not visible**: a session sees only the permission rows that concern it,
/// so a deployer granted `EXECUTE` by `wgo` sees that row and not the one
/// that let `wgo` grant it — and a replay that waited for the invisible row
/// refused an ordinary target (finding on #611). It need not be seen to be
/// known. Measured on 17.0: a grant made through `CONTROL`, `db_owner` or
/// `db_securityadmin` records the securable's *owner* as grantor, a
/// database-level grant option cannot grant on a schema at all (15151), and
/// taking a grant option back cascades to what was granted through it. So a
/// row naming any other grantor proves that grantor holds exactly this
/// permission with the grant option on exactly this securable, and the
/// replay gives it that — only once no visible row can, so a chain the
/// deployer can see is replayed as it is. The added row is as invisible to
/// the scratch deployer as the target's is to the real one, and `verify`
/// refuses the reconstruction if it ever is not.
fn replay_order<'a>(owner: &str, grants: &'a [Grant]) -> Result<Vec<Step<'a>>, DbError> {
    let mut steps = Vec::new();
    let mut pending: Vec<&Grant> = grants.iter().collect();
    let mut able: BTreeSet<(&str, &str)> = BTreeSet::new();
    while !pending.is_empty() {
        let (ready, waiting): (Vec<_>, Vec<_>) = pending.into_iter().partition(|grant| {
            grant.grantor == owner
                || grant.grantor == "dbo"
                || able.contains(&(grant.grantor.as_str(), grant.permission.as_str()))
        });
        if ready.is_empty() {
            // The grantors nothing still pending could enable. A set with
            // none is a cycle of grant options, which no engine state is.
            let roots: BTreeSet<(&str, &str)> = waiting
                .iter()
                .filter(|grant| {
                    !waiting.iter().any(|other| {
                        other.grantee == grant.grantor
                            && other.permission == grant.permission
                            && other.state == "GRANT_WITH_GRANT_OPTION"
                    })
                })
                .map(|grant| (grant.grantor.as_str(), grant.permission.as_str()))
                .collect();
            if roots.is_empty() {
                let stuck: Vec<String> = waiting
                    .iter()
                    .map(|g| format!("{} to {} by {}", g.permission, g.grantee, g.grantor))
                    .collect();
                return Err(DbError::BadRow(format!(
                    "grant rows name each other as the source of their grant option: {}",
                    stuck.join(", ")
                )));
            }
            for (grantor, permission) in roots {
                steps.push(Step::Enable {
                    grantor,
                    permission,
                });
                able.insert((grantor, permission));
            }
            pending = waiting;
            continue;
        }
        for grant in ready {
            steps.push(Step::Row(grant));
            if grant.state == "GRANT_WITH_GRANT_OPTION" {
                able.insert((grant.grantee.as_str(), grant.permission.as_str()));
            }
        }
        pending = waiting;
    }
    Ok(steps)
}

/// Replays grant rows in [`replay_order`], each under its own grantor, so
/// the scratch side records who granted what as the target does.
async fn replay(
    admin: &mut impl QueryConnection,
    map: &PrincipalMap,
    owner: &str,
    grants: &[Grant],
    schema: Option<&str>,
) -> Result<(), DbError> {
    // `dbo` owns the database-level securable and is named by no row of a
    // context that grants nothing through it, so it may be absent from the map.
    let local = |logical: &str| {
        if logical == "dbo" {
            return Ok("dbo".to_owned());
        }
        map.run_local(logical).ok_or_else(|| {
            DbError::BadRow(format!(
                "the authorization reconstruction has no run-local principal for {logical}"
            ))
        })
    };
    let on = schema.map_or(String::new(), |name| {
        format!(" ON SCHEMA::{}", bracket(name))
    });
    for step in replay_order(owner, grants)? {
        let statement = match step {
            Step::Enable {
                grantor,
                permission,
            } => format!(
                "GRANT {permission}{on} TO {} WITH GRANT OPTION AS {};",
                bracket(&local(grantor)?),
                bracket(&local(owner)?)
            ),
            Step::Row(grant) => {
                let (verb, option) = match grant.state.as_str() {
                    "GRANT" => ("GRANT", ""),
                    "GRANT_WITH_GRANT_OPTION" => ("GRANT", " WITH GRANT OPTION"),
                    "DENY" => ("DENY", ""),
                    other => {
                        return Err(DbError::BadRow(format!(
                            "a permission row has the unknown state {other}"
                        )));
                    }
                };
                format!(
                    "{verb} {}{on} TO {}{option} AS {};",
                    grant.permission,
                    bracket(&local(&grant.grantee)?),
                    bracket(&local(&grant.grantor)?)
                )
            }
        };
        admin.query(&statement).await?;
    }
    Ok(())
}

/// Switches the connection to the reproduced deployer, once: a second call
/// on a session already running as it is a no-op. Every check re-enters, a
/// user may `EXECUTE AS` itself, and each entry would stack one more
/// impersonation context on the last until the engine refuses the 33rd
/// (measured on 17.0: error 15159, "maximum impersonation nesting level
/// exceeded (limit 32)"), ending a long run on its own bookkeeping.
pub async fn enter(conn: &mut impl QueryConnection, deployer: Option<&str>) -> Result<(), DbError> {
    if let Some(deployer) = deployer {
        conn.query(&format!(
            "IF USER_NAME() <> {name} EXECUTE AS USER = {name};",
            name = literal(deployer)
        ))
        .await?;
    }
    Ok(())
}

/// Runs the plan's preceding grants on scratch as the connection's current
/// principal, which the caller has [`enter`]ed as the reproduced deployer —
/// so a grant it could not make on the target is refused here by the engine
/// too (measured on 17.0: error 15151, not a warning), rather than taking
/// under the scratch owner's rights. They run only after the reproduction
/// has been verified against the target as read: the engine then applies the
/// same statements to an equivalent state, and no model of how it records a
/// grantor or combines a DENY has to predict the result.
pub async fn apply_planned(
    deployer: &mut impl QueryConnection,
    map: &PrincipalMap,
    grants: &[PlannedGrant],
) -> Result<(), DbError> {
    for grant in grants {
        let principal = map.run_local(&grant.principal).ok_or_else(|| {
            DbError::BadRow(format!(
                "a planned grant names the unmapped principal {}",
                grant.principal
            ))
        })?;
        let statement = if grant.revoke {
            format!(
                "REVOKE {} ON SCHEMA::{} FROM {};",
                grant.permission,
                bracket(&grant.schema),
                bracket(&principal)
            )
        } else {
            format!(
                "GRANT {} ON SCHEMA::{} TO {};",
                grant.permission,
                bracket(&grant.schema),
                bracket(&principal)
            )
        };
        deployer.query(&statement).await?;
    }
    Ok(())
}

/// Reads the reproduced context as the current principal — the reproduced
/// deployer, which the caller has entered — and names every key that does
/// not reproduce the target, after mapping run-local names back to logical
/// ones. An empty result is a faithful reproduction (ADR-0016 case 16).
pub async fn verify(
    deployer: &mut impl QueryConnection,
    map: &PrincipalMap,
    target: &AuthorizationContext,
    schemas: &[String],
) -> Result<Vec<String>, DbError> {
    let reproduced = read(deployer, schemas).await?;
    Ok(differences(map, target, &reproduced))
}

fn differences(
    map: &PrincipalMap,
    target: &AuthorizationContext,
    reproduced: &AuthorizationContext,
) -> Vec<String> {
    let logical = |name: &str| map.logical_of(name).unwrap_or_else(|| name.to_owned());
    let mapped = |grants: &[Grant]| -> Vec<Grant> {
        let mut grants: Vec<Grant> = grants
            .iter()
            .map(|g| Grant {
                grantee: logical(&g.grantee),
                grantor: logical(&g.grantor),
                permission: g.permission.clone(),
                state: g.state.clone(),
            })
            .collect();
        grants.sort();
        grants
    };
    let mut found = Vec::new();
    if logical(&reproduced.principal.effective) != target.principal.effective {
        found.push("principal".to_owned());
    }
    if reproduced.principal.superuser != target.principal.superuser {
        found.push("principal:dbo".to_owned());
    }
    if reproduced.default_schema != target.default_schema {
        found.push("default-schema".to_owned());
    }
    if reproduced.language != target.language {
        found.push("language".to_owned());
    }
    if reproduced.database_permissions != target.database_permissions {
        found.push("database:permissions".to_owned());
    }
    if mapped(&reproduced.database_grants) != target.database_grants {
        found.push("database:grants".to_owned());
    }
    for (name, want) in &target.schemas {
        let Some(got) = reproduced.schemas.get(name) else {
            found.push(format!("schema:{name}:absent"));
            continue;
        };
        if got.permissions != want.permissions {
            found.push(format!("schema:{name}:permissions"));
        }
        // The engine's own schemas keep the engine's owner on both sides.
        if SYSTEM_SCHEMAS.contains(&name.as_str()) {
            continue;
        }
        if logical(&got.owner) != want.owner {
            found.push(format!("schema:{name}:owner"));
        }
        if mapped(&got.grants) != want.grants {
            found.push(format!("schema:{name}:grants"));
        }
    }
    let roles: BTreeMap<String, bool> = reproduced
        .roles
        .iter()
        .map(|(name, fixed)| (logical(name), *fixed))
        .collect();
    for name in roles
        .keys()
        .chain(target.roles.keys())
        .collect::<BTreeSet<_>>()
    {
        if roles.get(name) != target.roles.get(name) {
            found.push(format!("role:{name}"));
        }
    }
    let impersonation: BTreeSet<String> = reproduced
        .impersonation
        .iter()
        .map(|n| logical(n))
        .collect();
    if impersonation != target.impersonation {
        found.push("impersonation".to_owned());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(grantee: &str, grantor: &str, permission: &str, state: &str) -> Grant {
        Grant {
            grantee: grantee.into(),
            grantor: grantor.into(),
            permission: permission.into(),
            state: state.into(),
        }
    }

    fn rows(steps: &[Step<'_>]) -> Vec<String> {
        steps
            .iter()
            .map(|step| match step {
                Step::Row(g) => format!(
                    "{} {} to {} by {}",
                    g.state, g.permission, g.grantee, g.grantor
                ),
                Step::Enable {
                    grantor,
                    permission,
                } => format!("enable {permission} for {grantor}"),
            })
            .collect()
    }

    #[test]
    fn a_grantor_whose_grant_option_is_invisible_is_given_it_and_a_visible_chain_is_not() {
        // What a deployer sees when `wgo` granted it EXECUTE: its own row
        // only. The engine records a grantor other than the owner for a
        // grant-option holder and nobody else (measured), so the row is
        // replayable once `wgo` holds exactly that.
        let invisible = [grant("dep", "wgo", "EXECUTE", "GRANT")];
        assert_eq!(
            rows(&replay_order("app_owner", &invisible).unwrap()),
            ["enable EXECUTE for wgo", "GRANT EXECUTE to dep by wgo"]
        );

        // A chain the deployer can see is replayed as it is, in dependency
        // order whatever order the rows were read in, with nothing added.
        let visible = [
            grant("dep", "wgo", "EXECUTE", "GRANT"),
            grant("wgo", "app_owner", "EXECUTE", "GRANT_WITH_GRANT_OPTION"),
            grant("dep", "app_owner", "SELECT", "DENY"),
        ];
        assert_eq!(
            rows(&replay_order("app_owner", &visible).unwrap()),
            [
                "GRANT_WITH_GRANT_OPTION EXECUTE to wgo by app_owner",
                "DENY SELECT to dep by app_owner",
                "GRANT EXECUTE to dep by wgo",
            ]
        );

        // Only the root of a half-visible chain is enabled: `mid` gets its
        // grant option from the row that names `root`, never a second one
        // from the owner, which the deployer would see and the target lacks.
        let half = [
            grant("dep", "mid", "EXECUTE", "GRANT"),
            grant("mid", "root", "EXECUTE", "GRANT_WITH_GRANT_OPTION"),
        ];
        assert_eq!(
            rows(&replay_order("app_owner", &half).unwrap()),
            [
                "enable EXECUTE for root",
                "GRANT_WITH_GRANT_OPTION EXECUTE to mid by root",
                "GRANT EXECUTE to dep by mid",
            ]
        );

        // The grant option is per permission: holding it for one does not
        // make the grantor able for another.
        let other = [
            grant("wgo", "app_owner", "SELECT", "GRANT_WITH_GRANT_OPTION"),
            grant("dep", "wgo", "EXECUTE", "GRANT"),
        ];
        assert_eq!(
            rows(&replay_order("app_owner", &other).unwrap()),
            [
                "GRANT_WITH_GRANT_OPTION SELECT to wgo by app_owner",
                "enable EXECUTE for wgo",
                "GRANT EXECUTE to dep by wgo",
            ]
        );
    }

    #[test]
    fn grant_options_that_only_name_each_other_are_refused() {
        let cycle = [
            grant("a", "b", "SELECT", "GRANT_WITH_GRANT_OPTION"),
            grant("b", "a", "SELECT", "GRANT_WITH_GRANT_OPTION"),
        ];
        let refused = replay_order("app_owner", &cycle).unwrap_err().to_string();
        assert!(refused.contains("name each other"), "{refused}");
    }

    fn context() -> AuthorizationContext {
        AuthorizationContext {
            principal: DeploymentPrincipal {
                login: "deploy_login".into(),
                effective: "dep".into(),
                superuser: false,
            },
            default_schema: "app".into(),
            language: "us_english".into(),
            database_permissions: ["CONNECT", "CREATE TABLE"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            database_grants: vec![grant("dep", "dbo", "CREATE TABLE", "GRANT")],
            schemas: [(
                "app".to_owned(),
                SchemaAuthorization {
                    owner: "app_owner".into(),
                    permissions: ["ALTER", "SELECT"].into_iter().map(str::to_owned).collect(),
                    grants: vec![
                        grant("dep", "app_owner", "ALTER", "GRANT_WITH_GRANT_OPTION"),
                        grant("readers", "app_owner", "SELECT", "GRANT"),
                    ],
                },
            )]
            .into_iter()
            .collect(),
            roles: [
                ("readers".to_owned(), false),
                ("db_ddladmin".to_owned(), true),
            ]
            .into_iter()
            .collect(),
            impersonation: ["other".to_owned()].into_iter().collect(),
            principals: [
                ("dep".to_owned(), PrincipalKind::User),
                ("readers".to_owned(), PrincipalKind::Role),
                ("other".to_owned(), PrincipalKind::User),
            ]
            .into_iter()
            .collect(),
            spellings: BTreeMap::new(),
        }
    }

    /// #726: a planned grant that spells a catalogued principal differently
    /// is the same principal once the engine has named the catalog's
    /// spelling. It gets no run-local principal of its own, the grant lands on
    /// the one the catalog's spelling maps to, and a built-in in another
    /// casing keeps its identity instead of being cloned as a user.
    #[test]
    fn a_planned_principal_in_another_casing_is_the_catalogs_principal() {
        let planned = [
            PlannedGrant {
                principal: "READERS".into(),
                schema: "app".into(),
                permission: "SELECT".into(),
                revoke: false,
            },
            PlannedGrant {
                principal: "PUBLIC".into(),
                schema: "app".into(),
                permission: "SELECT".into(),
                revoke: false,
            },
        ];
        let mut resolved = context();
        resolved.spellings = [
            ("READERS".to_owned(), Some("readers".to_owned())),
            ("PUBLIC".to_owned(), Some("public".to_owned())),
        ]
        .into_iter()
        .collect();
        let map = PrincipalMap::generate(&resolved, &planned, "tok");
        assert_eq!(map.run_local("READERS"), map.run_local("readers"));
        assert_eq!(map.run_local("PUBLIC").as_deref(), Some("public"));
        // dep, readers, app_owner, other: nothing for either planned spelling.
        assert_eq!(
            map.run_local_names().len(),
            4,
            "{:?}",
            map.run_local_names()
        );

        // Unresolved, the same plan names two more principals, which is the
        // clone the grant used to land on.
        let map = PrincipalMap::generate(&context(), &planned, "tok");
        assert_ne!(map.run_local("READERS"), map.run_local("readers"));
        assert_ne!(map.run_local("PUBLIC").as_deref(), Some("public"));
        assert_eq!(
            map.run_local_names().len(),
            6,
            "{:?}",
            map.run_local_names()
        );
    }

    #[test]
    fn built_in_principals_keep_their_identity_and_everything_else_is_run_local() {
        let planned = [PlannedGrant {
            principal: "auditors".into(),
            schema: "app".into(),
            permission: "SELECT".into(),
            revoke: false,
        }];
        let map = PrincipalMap::generate(&context(), &planned, "tok");
        for built_in in ["dbo", "db_ddladmin"] {
            assert_eq!(map.run_local(built_in).as_deref(), Some(built_in));
            assert_eq!(map.logical_of(built_in).as_deref(), Some(built_in));
        }
        for logical in ["dep", "readers", "app_owner", "other", "auditors"] {
            let run = map.run_local(logical).expect(logical);
            assert!(
                run.starts_with("pbps_principal_") && run.ends_with("_tok"),
                "{run}"
            );
            assert_eq!(map.logical_of(&run).as_deref(), Some(logical));
        }
        // No production name is created on scratch, and no built-in is
        // recorded as something this run made.
        let created = map.run_local_names();
        assert_eq!(created.len(), 5, "{created:?}");
        assert!(
            created
                .iter()
                .all(|name| name.starts_with("pbps_principal_"))
        );
        assert_eq!(
            map.deployer(&context()).as_deref(),
            map.run_local("dep").as_deref()
        );
    }

    #[test]
    fn a_dbo_deployer_needs_no_impersonation_on_scratch() {
        let mut owner = context();
        owner.principal.effective = "dbo".into();
        owner.principal.superuser = true;
        let map = PrincipalMap::generate(&owner, &[], "tok");
        assert_eq!(map.deployer(&owner), None);
    }

    #[test]
    fn a_faithful_reproduction_has_no_differences_and_each_field_is_named_when_it_moves() {
        let target = context();
        let map = PrincipalMap::generate(&target, &[], "tok");
        let run = |name: &str| map.run_local(name).unwrap();
        // The reproduction as scratch reports it: run-local names throughout.
        let mut reproduced = target.clone();
        reproduced.principal.login = "pbps_run_login".into();
        reproduced.principal.effective = run("dep");
        reproduced.database_grants = vec![grant(&run("dep"), "dbo", "CREATE TABLE", "GRANT")];
        reproduced.roles = [(run("readers"), false), ("db_ddladmin".to_owned(), true)]
            .into_iter()
            .collect();
        reproduced.impersonation = [run("other")].into_iter().collect();
        let schema = reproduced.schemas.get_mut("app").unwrap();
        schema.owner = run("app_owner");
        schema.grants = vec![
            grant(
                &run("dep"),
                &run("app_owner"),
                "ALTER",
                "GRANT_WITH_GRANT_OPTION",
            ),
            grant(&run("readers"), &run("app_owner"), "SELECT", "GRANT"),
        ];
        assert_eq!(
            differences(&map, &target, &reproduced),
            Vec::<String>::new()
        );

        let moved = |change: &dyn Fn(&mut AuthorizationContext)| {
            let mut other = reproduced.clone();
            change(&mut other);
            differences(&map, &target, &other)
        };
        assert_eq!(
            moved(&|c| c.default_schema = "dbo".into()),
            ["default-schema"]
        );
        assert_eq!(moved(&|c| c.language = "Deutsch".into()), ["language"]);
        assert_eq!(
            moved(&|c| {
                c.database_permissions.insert("CREATE VIEW".into());
            }),
            ["database:permissions"]
        );
        assert_eq!(moved(&|c| c.database_grants.clear()), ["database:grants"]);
        assert_eq!(
            moved(&|c| {
                c.schemas
                    .get_mut("app")
                    .unwrap()
                    .permissions
                    .remove("ALTER");
            }),
            ["schema:app:permissions"]
        );
        assert_eq!(
            moved(&|c| c.schemas.get_mut("app").unwrap().owner = "dbo".into()),
            ["schema:app:owner"]
        );
        assert_eq!(
            moved(&|c| c.schemas.get_mut("app").unwrap().grants[0].state = "GRANT".into()),
            ["schema:app:grants"]
        );
        assert_eq!(
            moved(&|c| {
                c.schemas.clear();
            }),
            ["schema:app:absent"]
        );
        assert_eq!(
            moved(&|c| {
                c.roles.insert("db_owner".into(), true);
            }),
            ["role:db_owner"]
        );
        assert_eq!(moved(&|c| c.impersonation.clear()), ["impersonation"]);
        assert_eq!(
            moved(&|c| {
                c.principal.effective = "dbo".into();
                c.principal.superuser = true;
            }),
            ["principal", "principal:dbo"]
        );
    }

    #[test]
    fn the_canonical_form_changes_when_any_field_changes() {
        let base = context().canonical();
        let mut other = context();
        other.schemas.get_mut("app").unwrap().grants[1].state = "DENY".into();
        assert_ne!(other.canonical(), base);
        let mut other = context();
        other.language = "Deutsch".into();
        assert_ne!(other.canonical(), base);
        let mut other = context();
        other.impersonation.clear();
        assert_ne!(other.canonical(), base);
    }
}
