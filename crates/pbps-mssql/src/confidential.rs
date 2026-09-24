//! Who may read the protected ledger table, on SQL Server (DEC-868.1 "Who may
//! read it", #880; the method is DEC-880.1).
//!
//! [`protected_reader_problems`] answers one question from the catalog, read
//! only: is every principal that could read `dbo.__pbps_state_confidential` —
//! or the table this deployer would create there, when it is absent — one that
//! can become the deployment account or is `sysadmin`? It returns one line per
//! principal and path that is not, and one per statement capture that would
//! record the confidential write. An empty answer is the only good news; the
//! caller refuses the confidential operation on anything else. Nothing here
//! changes a grant, an audit or a session.
//!
//! The engine can answer "may I" for the deployer alone (`HAS_PERMS_BY_NAME`),
//! and asking it for anyone else means impersonating them, which the deployer
//! must not need. So the principals, their memberships and their permissions
//! are read as rows and combined here, and every approximation errs towards a
//! reader: a `DENY` never removes a reader, and it does remove a
//! qualification. That combination is only as good as the rows are complete,
//! so the first step proves the deployer is shown all of them.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};

use crate::catalog::{get, opt};
use crate::state::CONFIDENTIAL_TABLE;

/// Every problem that keeps the protected table from being qualified; empty
/// when every reader, grantor and backup principal can become the deployment
/// account or is `sysadmin`, and nothing captures the write.
pub async fn protected_reader_problems(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    // Catalog reads take shared locks, and another session's DDL can make one
    // of them the deadlock victim (1205). Measured under the live suite's
    // concurrent CREATE/DROP of logins, users and databases: the principal and
    // permission reads of a non-sysadmin deployer — whose rows the engine
    // filters one by one — lost in one run of two, and three immediate retries
    // lost together. The reads change nothing, so running them again is the
    // engine's own advice, after a pause that doubles (the engine's WAITFOR,
    // this crate having no timer of its own); a refusal for a lost deadlock
    // would refuse a valid plan. Anything else, and the last loss, is returned
    // as it came.
    const ATTEMPTS: u32 = 6;
    let mut attempt = 0;
    loop {
        match qualify(conn).await {
            Err(e)
                if e.server_error_code().as_deref() == Some("1205") && attempt + 1 < ATTEMPTS =>
            {
                let pause = 100u64 << attempt;
                conn.execute(&format!(
                    "WAITFOR DELAY '00:00:{:02}.{:03}';",
                    pause / 1000,
                    pause % 1000
                ))
                .await?;
                attempt += 1;
            }
            answer => return answer,
        }
    }
}

async fn qualify(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    let problems = visibility_problems(conn).await?;
    if !problems.is_empty() {
        return Ok(problems);
    }
    let graph = Graph::read(conn).await?;
    let mut problems = graph.principal_problems();
    problems.extend(capture_problems(conn).await?);
    Ok(problems)
}

/// What the deployer must be shown for the rows below to be all of them.
///
/// **Measured** on SQL Server 2025: a login with no grants sees its own
/// server principal, none of another user's database principals, role
/// members or permissions, and is refused `sys.dm_xe_sessions` (Msg 297);
/// `VIEW ANY DEFINITION` and `VIEW SERVER STATE` show every row, and
/// `sys.traces` still needs `ALTER TRACE` (Msg 8189), and
/// `sys.sql_expression_dependencies`, which says what code names the table,
/// its own `SELECT` (Msg 229; only `db_owner` holds it by default). A `DENY` of
/// `VIEW DEFINITION` below the database hides an object's rows whatever the
/// server grant says, so an effective one refuses too — the proof
/// `impact::key_drop_blockers` makes, and for the same reason: an invisible
/// principal is not an absent one. On the other securable classes — a
/// principal, a certificate, a key, a login — any such `DENY` refuses, since
/// those are the rows the principal graph and the signatures are built from and
/// `HAS_PERMS_BY_NAME` has no effective answer to offer for all of them.
const VISIBILITY: &str = "\
SELECT CONVERT(int, ISNULL(IS_SRVROLEMEMBER('sysadmin'), 0)) AS sysadmin,
       CONVERT(int, ISNULL(HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW ANY DEFINITION'), 0)) AS any_definition,
       CONVERT(int, ISNULL(HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE'), 0)) AS server_state,
       CONVERT(int, ISNULL(HAS_PERMS_BY_NAME(NULL, NULL, 'ALTER TRACE'), 0)) AS alter_trace,
       CONVERT(int, ISNULL(HAS_PERMS_BY_NAME(N'sys.sql_expression_dependencies', 'OBJECT',
                                             'SELECT'), 0)) AS dependencies,
       (SELECT COUNT(*) FROM sys.database_permissions dp
         WHERE dp.state = N'D' AND dp.permission_name IN (N'VIEW DEFINITION', N'CONTROL')
           AND (dp.grantee_principal_id = DATABASE_PRINCIPAL_ID()
                OR IS_MEMBER(USER_NAME(dp.grantee_principal_id)) = 1)
           AND ((dp.class = 1 AND COALESCE(HAS_PERMS_BY_NAME(
                    QUOTENAME(OBJECT_SCHEMA_NAME(dp.major_id)) + N'.' + QUOTENAME(OBJECT_NAME(dp.major_id)),
                    'OBJECT', 'VIEW DEFINITION'), 0) <> 1)
             OR (dp.class = 3 AND COALESCE(HAS_PERMS_BY_NAME(SCHEMA_NAME(dp.major_id),
                    'SCHEMA', 'VIEW DEFINITION'), 0) <> 1)
             OR (dp.class = 0 AND COALESCE(HAS_PERMS_BY_NAME(DB_NAME(),
                    'DATABASE', 'VIEW DEFINITION'), 0) <> 1)
             OR dp.class NOT IN (0, 1, 3)))
       + (SELECT COUNT(*) FROM sys.server_permissions sp
           WHERE sp.state = N'D'
             AND sp.permission_name IN (N'VIEW DEFINITION', N'CONTROL', N'VIEW ANY DEFINITION')
             AND (sp.grantee_principal_id = SUSER_ID()
                  OR IS_SRVROLEMEMBER(SUSER_NAME(sp.grantee_principal_id)) = 1)) AS hidden;";

async fn visibility_problems(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    let rows = conn.query(VISIBILITY).await?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::BadRow("the visibility probe returned no row".into()))?;
    if get::<i32>(row, "sysadmin")? == 1 {
        return Ok(Vec::new());
    }
    let mut problems = Vec::new();
    for (column, permission) in [
        ("any_definition", "VIEW ANY DEFINITION"),
        ("server_state", "VIEW SERVER STATE"),
        ("alter_trace", "ALTER TRACE"),
    ] {
        if get::<i32>(row, column)? != 1 {
            problems.push(format!(
                "the deployment account cannot establish who may read {CONFIDENTIAL_TABLE}: \
                 it lacks the server permission {permission}, without which the engine hides \
                 principals, sessions or traces it must see"
            ));
        }
    }
    if get::<i32>(row, "dependencies")? != 1 {
        problems.push(format!(
            "the deployment account cannot establish who may read {CONFIDENTIAL_TABLE}: it \
             lacks SELECT on sys.sql_expression_dependencies in this database, without which \
             the engine will not say which code reads the table"
        ));
    }
    if get::<i32>(row, "hidden")? != 0 {
        problems.push(format!(
            "the deployment account cannot establish who may read {CONFIDENTIAL_TABLE}: an \
             effective DENY of VIEW DEFINITION or CONTROL hides part of the catalog from it"
        ));
    }
    Ok(problems)
}

const SERVER_PRINCIPALS: &str = "\
SELECT principal_id, name, CONVERT(nvarchar(2), type) AS kind,
       CONVERT(varchar(172), sid, 1) AS sid, CONVERT(bit, is_fixed_role) AS fixed
  FROM sys.server_principals;";

const SERVER_ROLE_MEMBERS: &str = "\
SELECT member_principal_id AS member, role_principal_id AS role FROM sys.server_role_members;";

const SERVER_PERMISSIONS: &str = "\
SELECT CONVERT(tinyint, class) AS class, major_id AS major, 0 AS minor,
       grantee_principal_id AS grantee, permission_name AS name, CONVERT(nvarchar(1), state) AS state
  FROM sys.server_permissions;";

const DATABASE_PRINCIPALS: &str = "\
SELECT principal_id, name, CONVERT(nvarchar(2), type) AS kind,
       CONVERT(varchar(172), sid, 1) AS sid, CONVERT(bit, is_fixed_role) AS fixed,
       CONVERT(int, authentication_type) AS authentication
  FROM sys.database_principals;";

const DATABASE_ROLE_MEMBERS: &str = "\
SELECT member_principal_id AS member, role_principal_id AS role FROM sys.database_role_members;";

const DATABASE_PERMISSIONS: &str = "\
SELECT CONVERT(tinyint, class) AS class, major_id AS major, minor_id AS minor,
       grantee_principal_id AS grantee, permission_name AS name, CONVERT(nvarchar(1), state) AS state
  FROM sys.database_permissions;";

/// The protected table, its owner — the schema's when the table names none,
/// which is also who owns it before it exists (a new `dbo` table's
/// `principal_id` is NULL) — and who the deployer is.
const FACTS: &str = "\
DECLARE @t int = OBJECT_ID(N'dbo.__pbps_state_confidential', N'U');
SELECT @t AS table_id,
       COALESCE((SELECT o.principal_id FROM sys.objects o WHERE o.object_id = @t), s.principal_id)
         AS table_owner,
       s.schema_id AS dbo_schema,
       CONVERT(varchar(172), (SELECT owner_sid FROM sys.databases WHERE database_id = DB_ID()), 1)
         AS database_owner,
       DATABASE_PRINCIPAL_ID() AS deployer_user,
       SUSER_ID() AS deployer_login
  FROM sys.schemas s WHERE s.name = N'dbo';";

/// Every module of this database that runs code: its owner, its execution
/// context, and whether it names the protected table — which, owned by the
/// table's owner, is an ownership chain that skips the table's permissions.
/// A database DDL trigger is not in `sys.objects`; it fires for anyone's DDL.
/// A synonym is no code, but it chains the same way: measured, `SELECT` on a
/// `dbo` synonym for a `dbo` table reads the table with no grant on it.
const MODULES: &str = "\
DECLARE @t int = OBJECT_ID(N'dbo.__pbps_state_confidential', N'U');
WITH code AS (
  SELECT object_id, execute_as_principal_id FROM sys.sql_modules
  UNION ALL
  SELECT object_id, execute_as_principal_id FROM sys.assembly_modules)
SELECT c.object_id AS id,
       CONVERT(nvarchar(2), COALESCE(o.type, 'DT')) AS kind,
       COALESCE(o.schema_id, 0) AS schema_id,
       COALESCE(o.principal_id, s.principal_id, 1) AS owner,
       c.execute_as_principal_id AS execute_as,
       COALESCE(o.parent_object_id, 0) AS parent,
       COALESCE(p.schema_id, 0) AS parent_schema,
       COALESCE(p.principal_id, ps.principal_id, 0) AS parent_owner,
       CONVERT(int, CASE WHEN EXISTS (
           SELECT 1 FROM sys.sql_expression_dependencies d
            WHERE d.referencing_id = c.object_id
              AND (d.referenced_id = @t
                   OR (d.referenced_entity_name = N'__pbps_state_confidential'
                       AND ISNULL(d.referenced_schema_name, N'dbo') = N'dbo')))
         THEN 1 ELSE 0 END) AS names_table
  FROM code c
  LEFT JOIN sys.objects o ON o.object_id = c.object_id
  LEFT JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.objects p ON p.object_id = o.parent_object_id AND o.type = 'TR'
  LEFT JOIN sys.schemas ps ON ps.schema_id = p.schema_id
 WHERE o.object_id IS NOT NULL
    OR EXISTS (SELECT 1 FROM sys.triggers tr
                WHERE tr.object_id = c.object_id AND tr.parent_class = 0)
UNION ALL
SELECT sn.object_id, CONVERT(nvarchar(2), N'SN'), sn.schema_id,
       COALESCE(sn.principal_id, ss.principal_id, 1), CONVERT(int, NULL), 0, 0, 0,
       CONVERT(int, CASE WHEN OBJECT_ID(sn.base_object_name) = @t
                           OR (PARSENAME(sn.base_object_name, 1) = N'__pbps_state_confidential'
                               AND ISNULL(PARSENAME(sn.base_object_name, 2), N'dbo') = N'dbo'
                               AND ISNULL(PARSENAME(sn.base_object_name, 3), DB_NAME()) = DB_NAME()
                               AND PARSENAME(sn.base_object_name, 4) IS NULL)
                         THEN 1 ELSE 0 END)
  FROM sys.synonyms sn JOIN sys.schemas ss ON ss.schema_id = sn.schema_id;";

/// Which code names which other code. An ownership chain continues from one
/// module to another of the same owner, so a view over a view over the table
/// reads it as surely as the first view does.
const DEPENDENCIES: &str = "\
SELECT d.referencing_id AS module, d.referenced_id AS target
  FROM sys.sql_expression_dependencies d
 WHERE d.referenced_id IS NOT NULL
   AND EXISTS (SELECT 1 FROM sys.sql_modules m WHERE m.object_id = d.referenced_id)
UNION ALL
SELECT sn.object_id, OBJECT_ID(sn.base_object_name)
  FROM sys.synonyms sn
 WHERE OBJECT_ID(sn.base_object_name) IN (SELECT object_id FROM sys.sql_modules);";

/// A module signed by a certificate or asymmetric key runs with the
/// permissions of the principals mapped to that key, in this database and, for
/// a key also held in `master`, on the server.
const SIGNERS: &str = "\
SELECT cp.major_id AS module,
       COALESCE(dp.principal_id, 0) AS signer_user,
       COALESCE(sp.principal_id, 0) AS signer_login
  FROM sys.crypt_properties cp
  JOIN (SELECT thumbprint, sid FROM sys.certificates
        UNION ALL SELECT thumbprint, sid FROM sys.asymmetric_keys) k
    ON k.thumbprint = cp.thumbprint
  LEFT JOIN sys.database_principals dp ON dp.sid = k.sid
  LEFT JOIN sys.server_principals sp ON sp.sid = k.sid
 WHERE cp.class = 1;";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Principal {
    name: String,
    kind: String,
    sid: Option<String>,
    fixed: bool,
    /// `sys.database_principals.authentication_type`; 0 on the server side.
    authentication: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Perm {
    class: u8,
    major: i32,
    grantee: i32,
    name: String,
    state: String,
}

impl Perm {
    fn granted(&self) -> bool {
        self.state == "G" || self.state == "W"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Module {
    id: i32,
    kind: String,
    schema: i32,
    owner: i32,
    /// `sys.sql_modules.execute_as_principal_id`: NULL for the caller, -2 for
    /// `OWNER`, otherwise the principal.
    execute_as: Option<i32>,
    parent: i32,
    parent_schema: i32,
    parent_owner: i32,
    names_table: bool,
}

/// One principal a person can act as: a login, or a database principal that
/// is entered without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    Login(i32),
    User(i32),
}

/// The securable classes of `sys.database_permissions` this reads.
const DATABASE: u8 = 0;
const OBJECT: u8 = 1;
const SCHEMA: u8 = 3;
const DATABASE_PRINCIPAL: u8 = 4;
const SERVER: u8 = 100;
const SERVER_PRINCIPAL: u8 = 101;

/// Fixed database principal ids: `dbo`, `guest` and the `public` role.
const DBO: i32 = 1;
const GUEST: i32 = 2;
const PUBLIC_DATABASE_ROLE: i32 = 0;
/// The `public` server role.
const PUBLIC_SERVER_ROLE: i32 = 2;

#[derive(Debug, Default)]
struct Graph {
    server: BTreeMap<i32, Principal>,
    server_roles: Vec<(i32, i32)>,
    server_perms: Vec<Perm>,
    database: BTreeMap<i32, Principal>,
    database_roles: Vec<(i32, i32)>,
    database_perms: Vec<Perm>,
    table: Option<i32>,
    table_owner: i32,
    dbo_schema: i32,
    database_owner: Option<String>,
    deployer_user: i32,
    deployer_login: i32,
    modules: Vec<Module>,
    /// (module, signer user or 0, signer login or 0).
    signers: Vec<(i32, i32, i32)>,
    /// (module, module it names).
    dependencies: Vec<(i32, i32)>,
}

async fn principals(
    conn: &mut Conn,
    sql: &str,
    database: bool,
) -> Result<BTreeMap<i32, Principal>, DbError> {
    let mut out = BTreeMap::new();
    for row in &conn.query(sql).await? {
        out.insert(
            get::<i32>(row, "principal_id")?,
            Principal {
                name: get::<&str>(row, "name")?.to_owned(),
                kind: get::<&str>(row, "kind")?.trim().to_owned(),
                sid: opt::<&str>(row, "sid")?.map(ToOwned::to_owned),
                fixed: get::<bool>(row, "fixed")?,
                authentication: if database {
                    get::<i32>(row, "authentication")?
                } else {
                    0
                },
            },
        );
    }
    Ok(out)
}

async fn edges(conn: &mut Conn, sql: &str) -> Result<Vec<(i32, i32)>, DbError> {
    conn.query(sql)
        .await?
        .iter()
        .map(|row| Ok((get::<i32>(row, "member")?, get::<i32>(row, "role")?)))
        .collect()
}

async fn perms(conn: &mut Conn, sql: &str) -> Result<Vec<Perm>, DbError> {
    conn.query(sql)
        .await?
        .iter()
        .map(|row| {
            Ok(Perm {
                class: get::<u8>(row, "class")?,
                major: get::<i32>(row, "major")?,
                grantee: get::<i32>(row, "grantee")?,
                name: get::<&str>(row, "name")?.to_owned(),
                state: get::<&str>(row, "state")?.to_owned(),
            })
        })
        .collect()
}

impl Graph {
    async fn read(conn: &mut Conn) -> Result<Graph, DbError> {
        let mut g = Graph {
            server: principals(conn, SERVER_PRINCIPALS, false).await?,
            server_roles: edges(conn, SERVER_ROLE_MEMBERS).await?,
            server_perms: perms(conn, SERVER_PERMISSIONS).await?,
            database: principals(conn, DATABASE_PRINCIPALS, true).await?,
            database_roles: edges(conn, DATABASE_ROLE_MEMBERS).await?,
            database_perms: perms(conn, DATABASE_PERMISSIONS).await?,
            ..Graph::default()
        };
        let facts = conn.query(FACTS).await?;
        let row = facts
            .first()
            .ok_or_else(|| DbError::BadRow("this database has no dbo schema".into()))?;
        g.table = opt::<i32>(row, "table_id")?;
        g.table_owner = get::<i32>(row, "table_owner")?;
        g.dbo_schema = get::<i32>(row, "dbo_schema")?;
        g.database_owner = opt::<&str>(row, "database_owner")?.map(ToOwned::to_owned);
        g.deployer_user = get::<i32>(row, "deployer_user")?;
        g.deployer_login = get::<i32>(row, "deployer_login")?;
        for row in &conn.query(MODULES).await? {
            g.modules.push(Module {
                id: get::<i32>(row, "id")?,
                kind: get::<&str>(row, "kind")?.trim().to_owned(),
                schema: get::<i32>(row, "schema_id")?,
                owner: get::<i32>(row, "owner")?,
                execute_as: opt::<i32>(row, "execute_as")?,
                parent: get::<i32>(row, "parent")?,
                parent_schema: get::<i32>(row, "parent_schema")?,
                parent_owner: get::<i32>(row, "parent_owner")?,
                names_table: get::<i32>(row, "names_table")? == 1,
            });
        }
        for row in &conn.query(DEPENDENCIES).await? {
            g.dependencies
                .push((get::<i32>(row, "module")?, get::<i32>(row, "target")?));
        }
        for row in &conn.query(SIGNERS).await? {
            g.signers.push((
                get::<i32>(row, "module")?,
                get::<i32>(row, "signer_user")?,
                get::<i32>(row, "signer_login")?,
            ));
        }
        Ok(g)
    }

    fn closure(edges: &[(i32, i32)], start: i32, public: i32) -> BTreeSet<i32> {
        let mut seen = BTreeSet::from([start, public]);
        let mut todo = vec![start];
        while let Some(member) = todo.pop() {
            for &(m, role) in edges {
                if m == member && seen.insert(role) {
                    todo.push(role);
                }
            }
        }
        seen
    }

    fn server_closure(&self, login: i32) -> BTreeSet<i32> {
        Self::closure(&self.server_roles, login, PUBLIC_SERVER_ROLE)
    }

    fn database_closure(&self, user: i32) -> BTreeSet<i32> {
        Self::closure(&self.database_roles, user, PUBLIC_DATABASE_ROLE)
    }

    fn in_server_role(&self, closure: &BTreeSet<i32>, role: &str) -> bool {
        closure.iter().any(|id| {
            self.server
                .get(id)
                .is_some_and(|p| p.fixed && p.name == role)
        })
    }

    fn in_database_role(&self, closure: &BTreeSet<i32>, role: &str) -> bool {
        closure.iter().any(|id| {
            self.database
                .get(id)
                .is_some_and(|p| p.fixed && p.name == role)
        })
    }

    fn server_grants(&self, closure: &BTreeSet<i32>, names: &[&str], on: &[(u8, i32)]) -> bool {
        self.server_perms.iter().any(|p| {
            p.granted()
                && closure.contains(&p.grantee)
                && names.contains(&p.name.as_str())
                && on.contains(&(p.class, p.major))
        })
    }

    fn database_grants(&self, closure: &BTreeSet<i32>, names: &[&str], on: &[(u8, i32)]) -> bool {
        self.database_perms.iter().any(|p| {
            p.granted()
                && closure.contains(&p.grantee)
                && names.contains(&p.name.as_str())
                && on.contains(&(p.class, p.major))
        })
    }

    fn sysadmin(&self, login: i32) -> bool {
        self.in_server_role(&self.server_closure(login), "sysadmin")
    }

    /// `CONTROL SERVER` or `sysadmin`: every permission, and every login's.
    fn controls_server(&self, login: i32) -> bool {
        let c = self.server_closure(login);
        self.in_server_role(&c, "sysadmin")
            || self.server_grants(&c, &["CONTROL SERVER"], &[(SERVER, 0)])
    }

    fn guest_enabled(&self) -> bool {
        self.database_perms.iter().any(|p| {
            p.granted() && p.grantee == GUEST && p.class == DATABASE && p.name == "CONNECT"
        })
    }

    /// The database principal a login enters this database as: `dbo` for
    /// `sysadmin` and the database's owner, its mapped user, else `guest`
    /// when `guest` may connect.
    fn user_of(&self, login: i32) -> Option<i32> {
        let principal = self.server.get(&login)?;
        if self.controls_server(login)
            || (principal.sid.is_some() && principal.sid == self.database_owner)
        {
            return Some(DBO);
        }
        let mapped = self.database.iter().find(|(_, u)| {
            ["S", "U", "G", "E", "X"].contains(&u.kind.as_str())
                && u.sid.is_some()
                && u.sid == principal.sid
        });
        match mapped {
            Some((&id, _)) => Some(id),
            None if self.guest_enabled() => Some(GUEST),
            None => None,
        }
    }

    fn table_securables(&self) -> Vec<(u8, i32)> {
        let mut on = vec![(DATABASE, 0), (SCHEMA, self.dbo_schema)];
        on.extend(self.table.map(|t| (OBJECT, t)));
        on
    }

    /// Whether a database principal's own context reads the table.
    fn user_reads(&self, user: i32) -> bool {
        let c = self.database_closure(user);
        c.contains(&DBO)
            || c.contains(&self.table_owner)
            || self.in_database_role(&c, "db_owner")
            || self.in_database_role(&c, "db_datareader")
            || self.database_grants(&c, &["SELECT", "CONTROL"], &self.table_securables())
    }

    /// Whether a login's own context reads the table, through the server or
    /// the database principal it enters as.
    fn login_reads(&self, login: i32) -> bool {
        let c = self.server_closure(login);
        self.controls_server(login)
            || self.server_grants(&c, &["SELECT ALL USER SECURABLES"], &[(SERVER, 0)])
            || self.user_of(login).is_some_and(|u| self.user_reads(u))
    }

    /// Whether running `m` reads the table by itself, whoever runs it.
    fn module_reads(&self, m: &Module) -> bool {
        let context = match m.execute_as {
            Some(-2) => self.user_reads(m.owner),
            Some(p) if p > 0 => self.user_reads(p),
            _ => false,
        };
        let chained = m.names_table && m.owner == self.table_owner;
        let signed = self.signers.iter().any(|&(module, user, login)| {
            module == m.id
                && ((user != 0 && self.user_reads(user)) || (login != 0 && self.login_reads(login)))
        });
        context || chained || signed
    }

    /// Whether a database principal can make `m` run: execute it, or for a
    /// trigger write its table. A database DDL trigger runs for anyone's DDL.
    fn user_runs(&self, user: i32, m: &Module) -> bool {
        let c = self.database_closure(user);
        if c.contains(&DBO) || self.in_database_role(&c, "db_owner") {
            return true;
        }
        match m.kind.as_str() {
            "DT" => true,
            "TR" => {
                c.contains(&m.parent_owner)
                    || self.in_database_role(&c, "db_datawriter")
                    || self.database_grants(
                        &c,
                        &["INSERT", "UPDATE", "DELETE", "CONTROL"],
                        &[(OBJECT, m.parent), (SCHEMA, m.parent_schema), (DATABASE, 0)],
                    )
            }
            // A view, a table-valued function and a synonym are read, not
            // executed: `SELECT` on them is what runs their chain. A synonym
            // for a procedure is executed, so `EXECUTE` counts here too.
            "V" | "IF" | "TF" | "FT" | "SN" => {
                c.contains(&m.owner)
                    || self.in_database_role(&c, "db_datareader")
                    || self.database_grants(
                        &c,
                        &["SELECT", "EXECUTE", "CONTROL"],
                        &[(OBJECT, m.id), (SCHEMA, m.schema), (DATABASE, 0)],
                    )
            }
            _ => {
                c.contains(&m.owner)
                    || self.database_grants(
                        &c,
                        &["EXECUTE", "CONTROL"],
                        &[(OBJECT, m.id), (SCHEMA, m.schema), (DATABASE, 0)],
                    )
            }
        }
    }

    /// Every module whose running reads the table: by itself, or by naming
    /// one that does under the same owner — the chain continues — to a fixed
    /// point. Code of another owner adds nothing, since calling it checks the
    /// caller's own permission on it.
    fn reading_modules(&self) -> BTreeSet<i32> {
        let mut reading: BTreeSet<i32> = self
            .modules
            .iter()
            .filter(|m| self.module_reads(m))
            .map(|m| m.id)
            .collect();
        let owner = |id: i32| self.modules.iter().find(|m| m.id == id).map(|m| m.owner);
        loop {
            let more: Vec<i32> = self
                .dependencies
                .iter()
                .filter(|&&(m, target)| {
                    !reading.contains(&m)
                        && reading.contains(&target)
                        && owner(m).is_some()
                        && owner(m) == owner(target)
                })
                .map(|&(m, _)| m)
                .collect();
            if more.is_empty() {
                return reading;
            }
            reading.extend(more);
        }
    }

    fn user_reads_through_code(&self, user: i32, reading: &BTreeSet<i32>) -> bool {
        self.modules
            .iter()
            .any(|m| reading.contains(&m.id) && self.user_runs(user, m))
    }

    /// Whether `x` can `EXECUTE AS USER` `y`: `CONTROL` of the database — which
    /// `dbo` and `db_owner` hold, measured — or `IMPERSONATE`/`CONTROL` on `y`.
    fn user_becomes_user(&self, x: i32, y: i32) -> bool {
        let c = self.database_closure(x);
        x == y
            || c.contains(&DBO)
            || self.in_database_role(&c, "db_owner")
            || self.database_grants(&c, &["CONTROL"], &[(DATABASE, 0)])
            || self.database_grants(&c, &["IMPERSONATE", "CONTROL"], &[(DATABASE_PRINCIPAL, y)])
    }

    fn login_becomes_login(&self, x: i32, y: i32) -> bool {
        let c = self.server_closure(x);
        x == y
            || self.controls_server(x)
            || self.server_grants(&c, &["IMPERSONATE ANY LOGIN"], &[(SERVER, 0)])
            || self.server_grants(&c, &["IMPERSONATE", "CONTROL"], &[(SERVER_PRINCIPAL, y)])
    }

    fn becomes(&self, x: Who, y: Who) -> bool {
        match (x, y) {
            (Who::Login(a), Who::Login(b)) => self.login_becomes_login(a, b),
            (Who::Login(a), Who::User(b)) => self
                .user_of(a)
                .is_some_and(|u| self.user_becomes_user(u, b)),
            (Who::User(a), Who::User(b)) => self.user_becomes_user(a, b),
            (Who::User(_), Who::Login(_)) => false,
        }
    }

    /// Everyone who reads the table: directly, through code that reads it,
    /// or by becoming someone who does — to a fixed point, because becoming
    /// is transitive.
    fn readers(&self) -> BTreeSet<Who> {
        let everyone = self.everyone();
        let reading = self.reading_modules();
        let mut readers: BTreeSet<Who> = everyone
            .iter()
            .copied()
            .filter(|&w| match w {
                Who::Login(l) => {
                    self.login_reads(l)
                        || self
                            .user_of(l)
                            .is_some_and(|u| self.user_reads_through_code(u, &reading))
                }
                Who::User(u) => self.user_reads(u) || self.user_reads_through_code(u, &reading),
            })
            .collect();
        loop {
            let more: Vec<Who> = everyone
                .iter()
                .copied()
                .filter(|w| !readers.contains(w))
                .filter(|&w| readers.iter().any(|&r| self.becomes(w, r)))
                .collect();
            if more.is_empty() {
                return readers;
            }
            readers.extend(more);
        }
    }

    /// Whether `who` can give someone else a way to read the table.
    fn grants_reading(&self, who: Who, readers: &BTreeSet<Who>) -> bool {
        let user = match who {
            Who::Login(l) => {
                let c = self.server_closure(l);
                let reader_logins: Vec<(u8, i32)> = readers
                    .iter()
                    .filter_map(|r| match r {
                        Who::Login(id) => Some((SERVER_PRINCIPAL, *id)),
                        Who::User(_) => None,
                    })
                    .collect();
                if self.in_server_role(&c, "securityadmin")
                    || self.server_grants(
                        &c,
                        &["ALTER ANY LOGIN", "ALTER ANY SERVER ROLE"],
                        &[(SERVER, 0)],
                    )
                    || self.server_grants(&c, &["ALTER", "CONTROL"], &reader_logins)
                {
                    return true;
                }
                self.user_of(l)
            }
            Who::User(u) => Some(u),
        };
        let Some(user) = user else {
            return false;
        };
        let c = self.database_closure(user);
        let reader_principals: Vec<(u8, i32)> = self
            .database
            .keys()
            .filter(|&&id| self.user_reads(id))
            .map(|&id| (DATABASE_PRINCIPAL, id))
            .collect();
        let table = self.table_securables();
        self.in_database_role(&c, "db_owner")
            || self.in_database_role(&c, "db_securityadmin")
            || self.database_grants(
                &c,
                &[
                    "ALTER ANY ROLE",
                    "ALTER ANY USER",
                    "ALTER ANY APPLICATION ROLE",
                ],
                &[(DATABASE, 0)],
            )
            || self.database_grants(&c, &["ALTER", "CONTROL", "TAKE OWNERSHIP"], &table)
            || self.database_perms.iter().any(|p| {
                p.state == "W"
                    && c.contains(&p.grantee)
                    && ["SELECT", "CONTROL"].contains(&p.name.as_str())
                    && table.contains(&(p.class, p.major))
            })
            || self.database_grants(&c, &["ALTER", "CONTROL"], &reader_principals)
    }

    fn backs_up(&self, who: Who) -> bool {
        let user = match who {
            Who::Login(l) => self.user_of(l),
            Who::User(u) => Some(u),
        };
        user.is_some_and(|u| {
            let c = self.database_closure(u);
            c.contains(&DBO)
                || self.in_database_role(&c, "db_owner")
                || self.in_database_role(&c, "db_backupoperator")
                || self.database_grants(
                    &c,
                    &["BACKUP DATABASE", "BACKUP LOG", "CONTROL"],
                    &[(DATABASE, 0)],
                )
        })
    }

    /// Whether a `DENY` touches how `who` would become someone else. A deny
    /// never removes a reader here, but it may remove a qualification, and
    /// telling which one would be a second implementation of the engine's
    /// precedence; so any such deny withdraws it.
    fn denied(&self, who: Who) -> bool {
        const BECOMING: [&str; 4] = [
            "IMPERSONATE",
            "CONTROL",
            "IMPERSONATE ANY LOGIN",
            "CONTROL SERVER",
        ];
        let (server, user) = match who {
            Who::Login(l) => (Some(self.server_closure(l)), self.user_of(l)),
            Who::User(u) => (None, Some(u)),
        };
        server.is_some_and(|c| {
            self.server_perms.iter().any(|p| {
                p.state == "D" && c.contains(&p.grantee) && BECOMING.contains(&p.name.as_str())
            })
        }) || user.is_some_and(|u| {
            let c = self.database_closure(u);
            self.database_perms.iter().any(|p| {
                p.state == "D" && c.contains(&p.grantee) && BECOMING.contains(&p.name.as_str())
            })
        })
    }

    /// Whether `who` can become the deployment account — its login or its
    /// database user — or is `sysadmin`.
    fn qualified(&self, who: Who) -> bool {
        if let Who::Login(l) = who
            && self.sysadmin(l)
        {
            return true;
        }
        // Becoming is transitive — nested `EXECUTE AS` — so the deployer may be
        // reached through others. A principal with a `DENY` in its reach is
        // not stepped through, as it is not qualified itself.
        let targets = [
            Who::Login(self.deployer_login),
            Who::User(self.deployer_user),
        ];
        let everyone = self.everyone();
        let mut reached = BTreeSet::from([who]);
        let mut todo = vec![who];
        while let Some(x) = todo.pop() {
            if self.denied(x) {
                continue;
            }
            if targets.iter().any(|&t| self.becomes(x, t)) {
                return true;
            }
            for &y in &everyone {
                if !reached.contains(&y) && self.becomes(x, y) {
                    reached.insert(y);
                    todo.push(y);
                }
            }
        }
        false
    }

    fn everyone(&self) -> Vec<Who> {
        self.server
            .keys()
            .map(|&id| Who::Login(id))
            .chain(self.database.keys().map(|&id| Who::User(id)))
            .collect()
    }

    /// The principals a person signs in or switches to: every login but the
    /// certificate- and key-mapped ones and the engine's own `##` principals;
    /// and the database principals entered without a login — contained users,
    /// application roles, and `guest` when it may connect.
    fn actors(&self) -> Vec<Who> {
        let logins = self
            .server
            .iter()
            .filter(|(_, p)| {
                ["S", "U", "G", "E", "X"].contains(&p.kind.as_str()) && !p.name.starts_with("##")
            })
            .map(|(&id, _)| Who::Login(id));
        let login_sids: BTreeSet<&str> = self
            .server
            .values()
            .filter_map(|p| p.sid.as_deref())
            .collect();
        let users = self
            .database
            .iter()
            .filter(|&(&id, p)| {
                p.kind == "A"
                    || (id == GUEST && self.guest_enabled())
                    || (["S", "U", "G", "E", "X"].contains(&p.kind.as_str())
                        && p.authentication != 0
                        && id != DBO
                        && id != GUEST
                        && !p.sid.as_deref().is_some_and(|s| login_sids.contains(s)))
            })
            .map(|(&id, _)| Who::User(id));
        logins.chain(users).collect()
    }

    fn describe(&self, who: Who) -> String {
        match who {
            Who::Login(id) => format!(
                "login `{}`",
                self.server.get(&id).map_or("?", |p| p.name.as_str())
            ),
            Who::User(id) => {
                let p = self.database.get(&id);
                let kind = match p.map(|p| p.kind.as_str()) {
                    Some("A") => "application role",
                    _ => "database user",
                };
                format!("{kind} `{}`", p.map_or("?", |p| p.name.as_str()))
            }
        }
    }

    fn principal_problems(&self) -> Vec<String> {
        let readers = self.readers();
        let subject = if self.table.is_some() {
            CONFIDENTIAL_TABLE.to_owned()
        } else {
            format!("{CONFIDENTIAL_TABLE}, which this deployment would create,")
        };
        let mut problems = Vec::new();
        for who in self.actors() {
            let mut paths = Vec::new();
            if readers.contains(&who) {
                paths.push("can read");
            }
            if self.grants_reading(who, &readers) {
                paths.push("can grant reading");
            }
            if self.backs_up(who) {
                paths.push("can back up");
            }
            if paths.is_empty() || self.qualified(who) {
                continue;
            }
            problems.push(format!(
                "{} {} {subject} but can neither become the deployment account nor is sysadmin",
                self.describe(who),
                paths.join(", ")
            ));
        }
        problems
    }
}

/// Statement events that record a statement's text or its parameter values.
/// A running session with any of them would write the confidential INSERT
/// where readers pbps cannot qualify can read it.
const CAPTURING_EVENTS: &str = "N'rpc_starting', N'rpc_completed', N'sql_batch_starting', \
N'sql_batch_completed', N'sql_statement_starting', N'sql_statement_completed', \
N'sql_statement_recompile', N'sp_statement_starting', N'sp_statement_completed', \
N'module_start', N'module_end', N'prepare_sql', N'exec_prepared_sql', \
N'query_pre_execution_showplan', N'query_post_compilation_showplan', \
N'query_post_execution_showplan', N'query_post_execution_plan_profile', N'query_plan_profile', \
N'query_thread_profile'";

/// SQL Trace's equivalents: RPC, batch, statement and procedure events, the
/// prepared-statement events, RPC output parameters and the showplans.
const CAPTURING_TRACE_EVENTS: &str = "10, 11, 12, 13, 40, 41, 42, 43, 44, 45, 71, 72, 96, 97, 98, \
100, 122, 146, 168";

/// Audit action groups that record DML on a table or whole batches.
const CAPTURING_GROUPS: &str =
    "N'SCHEMA_OBJECT_ACCESS_GROUP', N'BATCH_STARTED_GROUP', N'BATCH_COMPLETED_GROUP'";

fn capture_query() -> String {
    format!(
        "DECLARE @t int = OBJECT_ID(N'dbo.__pbps_state_confidential', N'U');
SELECT N'extended-event session `' + s.name COLLATE DATABASE_DEFAULT + N'` records '
       + e.event_name COLLATE DATABASE_DEFAULT AS capture
  FROM sys.dm_xe_sessions s
  JOIN sys.dm_xe_session_events e ON e.event_session_address = s.address
 WHERE e.event_name IN ({CAPTURING_EVENTS})
UNION
SELECT N'trace ' + CONVERT(nvarchar(11), t.id) + N' records event '
       + CONVERT(nvarchar(11), i.eventid)
  FROM sys.traces t CROSS APPLY sys.fn_trace_geteventinfo(t.id) i
 WHERE t.status = 1 AND i.eventid IN ({CAPTURING_TRACE_EVENTS})
UNION
SELECT N'server audit specification `' + s.name COLLATE DATABASE_DEFAULT + N'` records '
       + d.audit_action_name COLLATE DATABASE_DEFAULT
  FROM sys.server_audit_specifications s
  JOIN sys.server_audit_specification_details d
    ON d.server_specification_id = s.server_specification_id
  JOIN sys.server_audits a ON a.audit_guid = s.audit_guid
 WHERE s.is_state_enabled = 1 AND a.is_state_enabled = 1
   AND d.audit_action_name IN ({CAPTURING_GROUPS})
UNION
SELECT N'database audit specification `' + s.name COLLATE DATABASE_DEFAULT + N'` records '
       + d.audit_action_name COLLATE DATABASE_DEFAULT
  FROM sys.database_audit_specifications s
  JOIN sys.database_audit_specification_details d
    ON d.database_specification_id = s.database_specification_id
  JOIN sys.server_audits a ON a.audit_guid = s.audit_guid
 WHERE s.is_state_enabled = 1 AND a.is_state_enabled = 1
   AND (d.audit_action_name IN ({CAPTURING_GROUPS})
        OR d.class = 0
        OR (d.class = 3 AND d.major_id = SCHEMA_ID(N'dbo'))
        OR (d.class = 1 AND d.major_id = @t))
UNION
SELECT N'replication publishes it' FROM sys.tables
 WHERE object_id = @t AND (is_replicated = 1 OR is_merge_published = 1)
UNION
SELECT N'change data capture tracks it' FROM sys.tables
 WHERE object_id = @t AND is_tracked_by_cdc = 1
UNION
SELECT N'change tracking tracks it' FROM sys.change_tracking_tables WHERE object_id = @t;"
    )
}

async fn capture_problems(conn: &mut Conn) -> Result<Vec<String>, DbError> {
    let mut problems = Vec::new();
    for row in &conn.query(&capture_query()).await? {
        problems.push(format!(
            "{}: it would record the confidential write to {CONFIDENTIAL_TABLE} where pbps \
             cannot qualify who reads it",
            get::<&str>(row, "capture")?
        ));
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SA: i32 = 1;
    const SYSADMIN: i32 = 3;
    const SECURITYADMIN: i32 = 4;
    const ALICE: i32 = 300;
    const BOB: i32 = 301;
    const DEPLOYER: i32 = 302;

    const U_ALICE: i32 = 5;
    const U_BOB: i32 = 6;
    const U_DEPLOYER: i32 = 7;
    const READERS_ROLE: i32 = 8;
    const DB_OWNER: i32 = 16384;
    const DB_DATAREADER: i32 = 16390;
    const DB_BACKUPOPERATOR: i32 = 16389;
    const TABLE: i32 = 900;
    const DBO_SCHEMA: i32 = 1;

    fn principal(name: &str, kind: &str, sid: Option<&str>, fixed: bool) -> Principal {
        Principal {
            name: name.into(),
            kind: kind.into(),
            sid: sid.map(Into::into),
            fixed,
            authentication: 1,
        }
    }

    fn grant(class: u8, major: i32, grantee: i32, name: &str) -> Perm {
        Perm {
            class,
            major,
            grantee,
            name: name.into(),
            state: "G".into(),
        }
    }

    /// `sa` deploys; Alice, Bob and the deployer's own login have users; one
    /// user-defined role; the fixed roles by name.
    fn graph() -> Graph {
        Graph {
            server: BTreeMap::from([
                (SA, principal("sa", "S", Some("0x01"), false)),
                (PUBLIC_SERVER_ROLE, principal("public", "R", None, false)),
                (SYSADMIN, principal("sysadmin", "R", None, true)),
                (SECURITYADMIN, principal("securityadmin", "R", None, true)),
                (ALICE, principal("alice", "S", Some("0xA1"), false)),
                (BOB, principal("bob", "S", Some("0xB0"), false)),
                (DEPLOYER, principal("deployer", "S", Some("0xD0"), false)),
            ]),
            server_roles: vec![(SA, SYSADMIN)],
            database: BTreeMap::from([
                (PUBLIC_DATABASE_ROLE, principal("public", "R", None, false)),
                (DBO, principal("dbo", "S", Some("0x01"), false)),
                (GUEST, principal("guest", "S", Some("0x00"), false)),
                (U_ALICE, principal("alice", "S", Some("0xA1"), false)),
                (U_BOB, principal("bob", "S", Some("0xB0"), false)),
                (U_DEPLOYER, principal("deployer", "S", Some("0xD0"), false)),
                (READERS_ROLE, principal("readers", "R", None, false)),
                (DB_OWNER, principal("db_owner", "R", None, true)),
                (DB_DATAREADER, principal("db_datareader", "R", None, true)),
                (
                    DB_BACKUPOPERATOR,
                    principal("db_backupoperator", "R", None, true),
                ),
            ]),
            table: Some(TABLE),
            table_owner: DBO,
            dbo_schema: DBO_SCHEMA,
            database_owner: Some("0x01".into()),
            deployer_user: U_DEPLOYER,
            deployer_login: DEPLOYER,
            ..Graph::default()
        }
    }

    fn named(g: &Graph) -> Vec<String> {
        g.principal_problems()
    }

    fn mentions(problems: &[String], who: &str) -> bool {
        problems.iter().any(|p| p.contains(&format!("`{who}`")))
    }

    #[test]
    fn a_database_where_only_the_deployer_and_sysadmin_read_qualifies() {
        assert_eq!(named(&graph()), Vec::<String>::new());
    }

    #[test]
    fn a_select_on_the_table_the_schema_or_the_database_makes_a_reader() {
        for (class, major) in [(OBJECT, TABLE), (SCHEMA, DBO_SCHEMA), (DATABASE, 0)] {
            let mut g = graph();
            g.database_perms
                .push(grant(class, major, U_ALICE, "SELECT"));
            let problems = named(&g);
            assert!(mentions(&problems, "alice"), "{class}: {problems:?}");
            assert!(!mentions(&problems, "bob"), "{class}: {problems:?}");
        }
    }

    #[test]
    fn a_grant_to_public_makes_every_user_a_reader() {
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, PUBLIC_DATABASE_ROLE, "SELECT"));
        let problems = named(&g);
        assert!(mentions(&problems, "alice") && mentions(&problems, "bob"));
        assert!(!mentions(&problems, "deployer"), "{problems:?}");
    }

    #[test]
    fn a_reader_role_reaches_its_members_and_db_datareader_is_one() {
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, READERS_ROLE, "SELECT"));
        g.database_roles.push((U_ALICE, READERS_ROLE));
        g.database_roles.push((U_BOB, DB_DATAREADER));
        let problems = named(&g);
        assert!(mentions(&problems, "alice") && mentions(&problems, "bob"));
    }

    #[test]
    fn a_denied_select_does_not_remove_a_reader() {
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, U_ALICE, "SELECT"));
        g.database_perms.push(Perm {
            state: "D".into(),
            ..grant(OBJECT, TABLE, U_ALICE, "SELECT")
        });
        assert!(mentions(&named(&g), "alice"));
    }

    #[test]
    fn a_reader_who_can_become_the_deployer_or_owns_the_database_role_qualifies() {
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, U_ALICE, "SELECT"));
        g.database_perms.push(grant(
            DATABASE_PRINCIPAL,
            U_DEPLOYER,
            U_ALICE,
            "IMPERSONATE",
        ));
        g.database_roles.push((U_BOB, DB_OWNER));
        assert_eq!(named(&g), Vec::<String>::new());
    }

    #[test]
    fn a_deny_on_impersonation_withdraws_the_qualification() {
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, U_ALICE, "SELECT"));
        g.database_perms.push(grant(
            DATABASE_PRINCIPAL,
            U_DEPLOYER,
            U_ALICE,
            "IMPERSONATE",
        ));
        g.database_perms.push(Perm {
            state: "D".into(),
            ..grant(DATABASE_PRINCIPAL, U_DEPLOYER, U_ALICE, "IMPERSONATE")
        });
        assert!(mentions(&named(&g), "alice"));
    }

    #[test]
    fn becoming_a_reader_is_reading() {
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, U_ALICE, "SELECT"));
        g.database_perms
            .push(grant(DATABASE_PRINCIPAL, U_ALICE, U_BOB, "IMPERSONATE"));
        assert!(mentions(&named(&g), "bob"));
        let mut g = graph();
        g.database_perms
            .push(grant(OBJECT, TABLE, U_ALICE, "SELECT"));
        g.server_perms
            .push(grant(SERVER_PRINCIPAL, ALICE, BOB, "IMPERSONATE"));
        assert!(mentions(&named(&g), "bob"));
    }

    #[test]
    fn select_all_user_securables_reads_every_database() {
        let mut g = graph();
        g.server_perms
            .push(grant(SERVER, 0, ALICE, "SELECT ALL USER SECURABLES"));
        assert!(mentions(&named(&g), "alice"));
    }

    fn module(id: i32, owner: i32, execute_as: Option<i32>, names_table: bool) -> Module {
        Module {
            id,
            kind: "P".into(),
            schema: DBO_SCHEMA,
            owner,
            execute_as,
            parent: 0,
            parent_schema: 0,
            parent_owner: 0,
            names_table,
        }
    }

    #[test]
    fn executing_code_that_reads_the_table_is_reading() {
        // EXECUTE AS OWNER, an ownership chain, and a signature.
        for (m, signer) in [
            (module(50, DBO, Some(-2), false), None),
            (module(50, DBO, None, true), None),
            (module(50, U_BOB, None, false), Some(U_BOB)),
        ] {
            let mut g = graph();
            g.modules.push(m.clone());
            if let Some(signer) = signer {
                g.database_perms
                    .push(grant(OBJECT, TABLE, signer, "SELECT"));
                g.signers.push((50, signer, 0));
            }
            g.database_perms.push(grant(OBJECT, 50, U_ALICE, "EXECUTE"));
            let problems = named(&g);
            assert!(mentions(&problems, "alice"), "{m:?}: {problems:?}");
        }
        // Negative: code that runs as its caller and names nothing does not.
        let mut g = graph();
        g.modules.push(module(50, DBO, None, false));
        g.database_perms.push(grant(OBJECT, 50, U_ALICE, "EXECUTE"));
        assert!(!mentions(&named(&g), "alice"));
    }

    #[test]
    fn selecting_from_a_chained_view_is_reading_and_the_chain_continues_through_views() {
        let view = |id, owner, names_table| Module {
            kind: "V".into(),
            ..module(id, owner, None, names_table)
        };
        let mut g = graph();
        g.modules.push(view(50, DBO, true));
        g.database_perms.push(grant(OBJECT, 50, U_ALICE, "SELECT"));
        assert!(mentions(&named(&g), "alice"));

        // A view over that view, under the same owner, continues the chain.
        let mut g = graph();
        g.modules.push(view(50, DBO, true));
        g.modules.push(view(60, DBO, false));
        g.dependencies.push((60, 50));
        g.database_perms.push(grant(OBJECT, 60, U_ALICE, "SELECT"));
        assert!(mentions(&named(&g), "alice"));

        // A synonym for the table chains the same way.
        let mut g = graph();
        g.modules.push(Module {
            kind: "SN".into(),
            ..module(70, DBO, None, true)
        });
        g.database_perms.push(grant(OBJECT, 70, U_ALICE, "SELECT"));
        assert!(mentions(&named(&g), "alice"));

        // Negative: under another owner the chain breaks, as the engine's does.
        let mut g = graph();
        g.modules.push(view(50, DBO, true));
        g.modules.push(view(60, U_BOB, false));
        g.dependencies.push((60, 50));
        g.database_perms.push(grant(OBJECT, 60, U_ALICE, "SELECT"));
        assert!(!mentions(&named(&g), "alice"));
    }

    #[test]
    fn a_reader_qualifies_through_a_chain_of_impersonation() {
        let chain = || {
            let mut g = graph();
            g.database_perms
                .push(grant(OBJECT, TABLE, U_ALICE, "SELECT"));
            g.database_perms
                .push(grant(DATABASE_PRINCIPAL, U_BOB, U_ALICE, "IMPERSONATE"));
            g.database_perms
                .push(grant(DATABASE_PRINCIPAL, U_DEPLOYER, U_BOB, "IMPERSONATE"));
            g
        };
        assert_eq!(named(&chain()), Vec::<String>::new());
        // Negative: a DENY on the middle principal breaks the chain.
        let mut g = chain();
        g.database_perms.push(Perm {
            state: "D".into(),
            ..grant(DATABASE_PRINCIPAL, U_DEPLOYER, U_BOB, "IMPERSONATE")
        });
        assert!(mentions(&named(&g), "alice"));
    }

    #[test]
    fn grantors_and_backup_principals_must_qualify_too() {
        let cases: [fn(&mut Graph); 7] = [
            |g: &mut Graph| {
                g.database_roles.push((U_ALICE, 16386));
                g.database
                    .insert(16386, principal("db_securityadmin", "R", None, true));
            },
            |g: &mut Graph| {
                g.database_perms
                    .push(grant(DATABASE, 0, U_ALICE, "ALTER ANY ROLE"));
            },
            |g: &mut Graph| {
                g.database_perms.push(Perm {
                    state: "W".into(),
                    ..grant(OBJECT, TABLE, U_ALICE, "SELECT")
                });
            },
            |g: &mut Graph| {
                g.database_perms
                    .push(grant(SCHEMA, DBO_SCHEMA, U_ALICE, "ALTER"));
            },
            |g: &mut Graph| {
                g.server_roles.push((ALICE, SECURITYADMIN));
            },
            |g: &mut Graph| {
                g.database_roles.push((U_ALICE, DB_BACKUPOPERATOR));
            },
            |g: &mut Graph| {
                g.database_perms
                    .push(grant(DATABASE, 0, U_ALICE, "BACKUP DATABASE"));
            },
        ];
        for set in cases {
            let mut g = graph();
            set(&mut g);
            let problems = named(&g);
            assert!(mentions(&problems, "alice"), "{problems:?}");
            assert!(!mentions(&problems, "bob"), "{problems:?}");
        }
    }

    #[test]
    fn an_absent_table_is_read_by_whoever_owns_dbo_and_reads_the_schema() {
        let mut g = graph();
        g.table = None;
        g.table_owner = U_ALICE;
        assert!(mentions(&named(&g), "alice"));
        let mut g = graph();
        g.table = None;
        g.database_perms
            .push(grant(SCHEMA, DBO_SCHEMA, U_BOB, "SELECT"));
        let problems = named(&g);
        assert!(mentions(&problems, "bob"), "{problems:?}");
        assert!(problems[0].contains("which this deployment would create"));
    }

    #[test]
    fn guest_and_application_roles_are_actors_of_their_own() {
        let mut g = graph();
        g.database_perms.push(grant(DATABASE, 0, GUEST, "CONNECT"));
        g.database_perms.push(grant(OBJECT, TABLE, GUEST, "SELECT"));
        assert!(mentions(&named(&g), "guest"));
        let mut g = graph();
        g.database
            .insert(40, principal("app", "A", Some("0x40"), false));
        g.database_perms.push(grant(OBJECT, TABLE, 40, "SELECT"));
        let problems = named(&g);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("application role `app`")),
            "{problems:?}"
        );
    }
}
