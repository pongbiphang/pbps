//! Instance SQL for the operator-vouched resolver (DEC-1528.1; #1667, #1672).
//!
//! The operator vouches for the scratch server: pbps observes no process and
//! proves no containment. What it still establishes, before any scratch
//! write, is where the scratch database sits relative to the target and what
//! the scratch account may do there, because those decide whether a run can
//! touch anything that is not its own. Each read here is a plain catalog read
//! any role may make, so a least-privilege scratch account can answer it.

use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Row};

/// The first OID a database assigns after initdb. Everything below it was
/// created by initdb itself, in every database alike; anything at or above it
/// was created afterwards, in this database or in the template it was cloned
/// from (`FirstNormalObjectId`, the boundary pg_dump uses for the same
/// question).
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// The catalogs whose rows are objects a declaration could bind to or a
/// later compilation would collide with. An object that lives on another
/// object — a column, a trigger, a constraint, a default — has its owner in
/// one of these, so the owner is what is found.
const CATALOGS: &[&str] = &[
    "pg_namespace",
    "pg_class",
    "pg_proc",
    "pg_type",
    "pg_extension",
    "pg_cast",
    "pg_operator",
    "pg_opclass",
    "pg_opfamily",
    "pg_am",
    "pg_collation",
    "pg_conversion",
    "pg_language",
    "pg_ts_config",
    "pg_ts_dict",
    "pg_ts_parser",
    "pg_ts_template",
    "pg_foreign_data_wrapper",
    "pg_foreign_server",
    "pg_event_trigger",
    "pg_default_acl",
    "pg_publication",
    "pg_statistic_ext",
    "pg_transform",
];

/// Catalogs whose OIDs a user may choose (`lo_create(1)`), so no cutoff
/// tells initdb's rows from later ones. initdb creates none, so every row is
/// foreign (#1678 review). Each with the class `pg_describe_object` names
/// its rows by: a large object's is `pg_largeobject`, and the metadata
/// catalog's own OID is refused as "unsupported object class" (measured on
/// 16 and 18).
const EVERY_ROW: &[(&str, &str)] = &[("pg_largeobject_metadata", "pg_largeobject")];

/// Shared catalogs whose rows belong to one database, keyed by that
/// database's OID: a subscription lives in the database it was created in.
const DATABASE_KEYED: &[(&str, &str)] = &[("pg_subscription", "subdbid")];

/// How many foreign objects a refusal names. The first few say what is
/// there; the count says how much.
const NAMED: usize = 10;

/// One session's mark: its backend and the database it is connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMark {
    pub pid: i64,
    pub database: u32,
}

/// Where the scratch database sits relative to the target's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Another cluster: nothing the run does there reaches the target's.
    SeparateCluster,
    /// The target's cluster, in another database.
    SameCluster,
    /// The target's own database.
    Target,
}

/// What the scratch account may do beyond its own database, through itself
/// or any role it may `SET ROLE` to. `SUPERUSER`, `CREATEROLE`, `CREATEDB`
/// and `REPLICATION` are never inherited: a role's attribute is the login's
/// to use only if the login can become that role. A membership granted
/// `SET FALSE` is still `MEMBER` but cannot be become, so it does not count
/// (measured on 16 and 18: `SET ROLE` and `CREATE DATABASE` are both
/// refused; #1678 review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Held by the login or by any role it may `SET ROLE` to.
    pub superuser: bool,
    pub create_role: bool,
    pub create_db: bool,
    /// Creates replication slots, which hold WAL for the whole cluster
    /// (measured on 16 and 18 for a non-superuser after `SET ROLE`).
    pub replication: bool,
    /// The login's own `SUPERUSER`. Membership does not pass it on, and the
    /// run never `SET ROLE`s, so only this one acts as a superuser.
    pub login_superuser: bool,
    /// Predefined roles in its [`REACH`] whose privileges
    /// act outside the database: running a server program, writing or
    /// reading server files, signalling other backends, a checkpoint, a
    /// subscription. A compiled definition can use any of them before the
    /// plan is approved (#1678 security review).
    pub predefined: Vec<String>,
    /// Authority over a shared object other than the database it is in,
    /// held by a role in its [`REACH`] or by PUBLIC: owning
    /// another database or a tablespace, `ADMIN OPTION` on a role, a grant
    /// option on a shared object, `ALTER SYSTEM` on a parameter. Each is a
    /// write outside the database a compiled definition can make (#1678
    /// review). Named as what to remove.
    pub shared: Vec<String>,
}

/// Every role whose privileges the scratch login can use: a role it can
/// `SET ROLE` to, itself included, and every role one of those inherits
/// from. `SET` chains start at the session user, so a role reached by `SET`
/// and then inherited counts though neither `USAGE` nor `SET` from the
/// login reaches it (measured on 16 and 18: `COPY ... TO PROGRAM` after
/// `SET ROLE` to a role inheriting `pg_execute_server_program`). PUBLIC, as
/// OID 0, is in it: a privilege granted to PUBLIC is the login's too
/// (#1678 review).
const REACH: &str = "SELECT r.oid FROM pg_catalog.pg_roles r \
                      WHERE EXISTS (SELECT FROM pg_catalog.pg_roles b \
                                     WHERE pg_catalog.pg_has_role(session_user, b.oid, 'SET') \
                                       AND pg_catalog.pg_has_role(b.oid, r.oid, 'USAGE')) \
                     UNION ALL SELECT 0::pg_catalog.oid";

/// The predefined roles whose privileges stay inside the database the
/// session is in, or only read statistics and settings. Every other one is
/// refused, so a role a later version adds is refused until it is known.
/// `pg_database_owner` is the owner of the current database, which the
/// supplied login is.
pub const CONTAINED_PREDEFINED: &[&str] = &[
    "pg_database_owner",
    "pg_read_all_data",
    "pg_write_all_data",
    "pg_maintain",
    "pg_monitor",
    "pg_read_all_settings",
    "pg_read_all_stats",
    "pg_stat_scan_tables",
    "pg_use_reserved_connections",
];

impl Account {
    /// Confined to the databases it is given: it can create no role, no
    /// database and no replication slot, is no superuser (the decision on
    /// #1667), and holds no predefined role that acts outside the database.
    pub fn confined(&self) -> bool {
        self.excess().is_empty()
    }

    /// May create the run's own login, roles and database, and act as each
    /// of them. Only a login that is itself a superuser; a member of a
    /// superuser role runs without it (#1678 review). Not a `CREATEROLE`
    /// one: a `CREATEROLE` login is granted `ADMIN` on the roles it creates
    /// but not `SET` (the default `createrole_self_grant`), so it cannot
    /// hand them the database or replay grants as them, and it cannot
    /// reproduce a superuser deployer.
    pub fn provisions(&self) -> bool {
        self.login_superuser
    }

    /// What makes it unconfined: each attribute as `ALTER ROLE` spells its
    /// removal, then each predefined role it reaches.
    pub fn excess(&self) -> Vec<String> {
        [
            (self.superuser, "NOSUPERUSER"),
            (self.create_role, "NOCREATEROLE"),
            (self.create_db, "NOCREATEDB"),
            (self.replication, "NOREPLICATION"),
        ]
        .into_iter()
        .filter(|(held, _)| *held)
        .map(|(_, removal)| removal.to_owned())
        .chain(
            self.predefined
                .iter()
                .map(|role| format!("no membership in {role}")),
        )
        .chain(
            self.shared
                .iter()
                .map(|authority| format!("no {authority}")),
        )
        .collect()
    }
}

/// A run-generated session token: lowercase hexadecimal, so it reaches the
/// statement as a literal nothing can end.
fn token_literal(token: &str) -> Result<String, DbError> {
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(DbError::BadRow(
            "a session token must be run-generated lowercase hexadecimal".into(),
        ));
    }
    Ok(format!("'pbps-{token}'"))
}

fn text(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("the vouched resolver read no {field}")))
}

fn one(rows: Vec<Row>, what: &str) -> Result<Row, DbError> {
    let mut rows = rows.into_iter();
    match (rows.next(), rows.next()) {
        (Some(row), None) => Ok(row),
        _ => Err(DbError::BadRow(format!("{what} did not return one row"))),
    }
}

/// Sets this session's `application_name` to the run's token for the current
/// transaction, and reports its backend and database. The caller holds the
/// transaction open across the whole check, so a pooler cannot hand the
/// backend to another session meanwhile, and ending it clears the mark.
/// Nothing in the database changes.
pub async fn mark(conn: &mut impl QueryConnection, token: &str) -> Result<SessionMark, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.set_config('application_name', {}, true) AS marked, \
                    pg_catalog.pg_backend_pid()::text AS pid, \
                    (SELECT d.oid FROM pg_catalog.pg_database d \
                      WHERE d.datname = pg_catalog.current_database())::text AS database",
            token_literal(token)?
        ))
        .await?;
    let row = one(rows, "a session mark")?;
    let number = |field: &str| -> Result<String, DbError> { text(&row, field) };
    Ok(SessionMark {
        pid: number("pid")?
            .parse()
            .map_err(|_| DbError::BadRow("a backend pid was not a number".into()))?,
        database: number("database")?
            .parse()
            .map_err(|_| DbError::BadRow("a database OID was not a number".into()))?,
    })
}

/// The server backend a session runs on: its process, when it started, and
/// when its postmaster started. A session acting as its own login reads its
/// own row of `pg_stat_activity`, timings included (measured on 16 and 18);
/// under another role the timings are hidden, so the run drops any role
/// first. Another backend, on this cluster or another, does not share all
/// three. The times are read as epochs: their text follows `TimeZone` and
/// `DateStyle`, which the compile pins and would make one backend read as
/// two (#1678 review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pid: String,
    started: String,
    postmaster: String,
}

pub async fn backend(conn: &mut impl QueryConnection) -> Result<Backend, DbError> {
    let rows = conn
        .query(
            "SELECT pg_catalog.pg_backend_pid()::text AS pid, \
                    (SELECT pg_catalog.extract('epoch', a.backend_start) \
                       FROM pg_catalog.pg_stat_activity a \
                      WHERE a.pid = pg_catalog.pg_backend_pid())::text AS started, \
                    pg_catalog.extract('epoch', pg_catalog.pg_postmaster_start_time()) \
                      ::text AS postmaster",
        )
        .await?;
    let row = one(rows, "this session's backend")?;
    Ok(Backend {
        pid: text(&row, "pid")?,
        started: text(&row, "started")?,
        postmaster: text(&row, "postmaster")?,
    })
}

/// Whether this transaction runs on `checked`. A connection keeps its socket
/// to whatever it reached, not its backend: a transaction-pooling proxy may
/// hand each transaction another backend, possibly on another cluster, so
/// what the run checked holds only on the backend it was checked on.
pub async fn on_backend(
    conn: &mut impl QueryConnection,
    checked: &Backend,
) -> Result<bool, DbError> {
    Ok(backend(conn).await? == *checked)
}

/// The backends this session can see carrying each of `tokens`. Any role
/// sees every session's `pid` and `application_name` in `pg_stat_activity`;
/// only the query text and timings are hidden (measured on 16 and 18).
pub async fn marked(
    conn: &mut impl QueryConnection,
    tokens: &[&str],
) -> Result<Vec<(String, i64)>, DbError> {
    let literals = tokens
        .iter()
        .map(|token| token_literal(token))
        .collect::<Result<Vec<_>, _>>()?;
    let rows = conn
        .query(&format!(
            "SELECT a.application_name AS token, a.pid::text AS pid \
               FROM pg_catalog.pg_stat_activity a \
              WHERE a.application_name IN ({})",
            literals.join(", ")
        ))
        .await?;
    rows.iter()
        .map(|row| {
            let token = text(row, "token")?;
            let pid = text(row, "pid")?
                .parse()
                .map_err(|_| DbError::BadRow("a backend pid was not a number".into()))?;
            Ok((token.trim_start_matches("pbps-").to_owned(), pid))
        })
        .collect()
}

/// Where the scratch session is, from what it saw. The scratch session must
/// see its own mark: a cluster that does not show it cannot show the
/// target's either, and its absence would then read as "another cluster".
pub fn placement(
    target: (&str, SessionMark),
    scratch: (&str, SessionMark),
    seen: &[(String, i64)],
) -> Result<Placement, &'static str> {
    let sees = |(token, mark): (&str, SessionMark)| {
        seen.iter()
            .any(|(found, pid)| found == token && *pid == mark.pid)
    };
    if !sees(scratch) {
        return Err(
            "the scratch session cannot see its own session in pg_stat_activity, so whether it shares the target's cluster cannot be read",
        );
    }
    Ok(if !sees(target) {
        Placement::SeparateCluster
    } else if scratch.1.database == target.1.database {
        Placement::Target
    } else {
        Placement::SameCluster
    })
}

/// The scratch login's cluster-wide attributes, through every role it may
/// `SET ROLE` to, and the predefined roles outside
/// [`CONTAINED_PREDEFINED`] it inherits or may `SET ROLE` to: a predefined
/// role's privileges are inherited, so `USAGE` reaches them too (measured
/// on 16 and 18: `COPY ... TO PROGRAM` through an `INHERIT TRUE, SET FALSE`
/// membership of `pg_execute_server_program`).
pub async fn account(conn: &mut impl QueryConnection) -> Result<Account, DbError> {
    let rows = conn
        .query(
            "SELECT coalesce(bool_or(r.rolsuper), false)::text AS superuser, \
                    coalesce(bool_or(r.rolcreaterole), false)::text AS create_role, \
                    coalesce(bool_or(r.rolcreatedb), false)::text AS create_db, \
                    coalesce(bool_or(r.rolreplication), false)::text AS replication, \
                    coalesce(bool_or(r.rolsuper) FILTER (WHERE r.rolname = session_user), \
                             false)::text AS login_superuser \
               FROM pg_catalog.pg_roles r \
              WHERE pg_catalog.pg_has_role(session_user, r.oid, 'SET')",
        )
        .await?;
    let contained = CONTAINED_PREDEFINED
        .iter()
        .map(|role| format!("'{role}'"))
        .collect::<Vec<_>>()
        .join(", ");
    // Predefined roles are those initdb made, below FirstNormalObjectId;
    // the bootstrap superuser among them is already counted above.
    let predefined = conn
        .query(&format!(
            "SELECT r.rolname::text AS role FROM pg_catalog.pg_roles r \
              WHERE r.oid < {FIRST_NORMAL_OBJECT_ID} AND NOT r.rolsuper \
                AND r.rolname NOT IN ({contained}) \
                AND r.oid IN ({REACH}) \
              ORDER BY 1"
        ))
        .await?
        .iter()
        .map(|row| text(row, "role"))
        .collect::<Result<Vec<_>, _>>()?;
    // A subscription is the exception among shared objects: only a session
    // in its own database can alter or drop it (measured on 16 and 18), and
    // one in this database is the emptiness check's. An owner's, a role
    // admin's and a grantor's authority pass to every role in [`REACH`]
    // (measured on 16 and 18: `ALTER DATABASE` through an `INHERIT TRUE,
    // SET FALSE` membership of its owner).
    let shared = conn
        .query(&format!(
            "WITH reach AS ({REACH}), \
                  here AS (SELECT d.oid FROM pg_catalog.pg_database d \
                            WHERE d.datname = pg_catalog.current_database()) \
             SELECT 'ownership of ' || pg_catalog.pg_describe_object(s.classid, s.objid, 0) \
                    AS authority \
               FROM pg_catalog.pg_shdepend s, here \
              WHERE s.dbid = 0 AND s.deptype = 'o' \
                AND s.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass \
                AND s.refobjid IN (SELECT oid FROM reach) \
                AND NOT (s.classid = 'pg_catalog.pg_database'::pg_catalog.regclass \
                         AND s.objid = here.oid) \
                AND s.classid <> 'pg_catalog.pg_subscription'::pg_catalog.regclass \
             UNION \
             SELECT 'admin option on role ' || r.rolname \
               FROM pg_catalog.pg_auth_members a \
               JOIN pg_catalog.pg_roles r ON r.oid = a.roleid \
              WHERE a.admin_option AND a.member IN (SELECT oid FROM reach) \
             UNION \
             SELECT 'grant option on database ' || d.datname \
               FROM pg_catalog.pg_database d, pg_catalog.aclexplode(d.datacl) x \
              WHERE x.is_grantable AND x.grantee IN (SELECT oid FROM reach) \
             UNION \
             SELECT 'grant option on tablespace ' || t.spcname \
               FROM pg_catalog.pg_tablespace t, pg_catalog.aclexplode(t.spcacl) x \
              WHERE x.is_grantable AND x.grantee IN (SELECT oid FROM reach) \
             UNION \
             SELECT x.privilege_type || CASE WHEN x.is_grantable THEN ' with grant option' \
                                             ELSE '' END || ' on parameter ' || p.parname \
               FROM pg_catalog.pg_parameter_acl p, pg_catalog.aclexplode(p.paracl) x \
              WHERE (x.is_grantable OR x.privilege_type = 'ALTER SYSTEM') \
                AND x.grantee IN (SELECT oid FROM reach) \
             ORDER BY 1"
        ))
        .await?
        .iter()
        .map(|row| text(row, "authority"))
        .collect::<Result<Vec<_>, _>>()?;
    let row = one(rows, "the scratch account's attributes")?;
    let flag = |field: &str| -> Result<bool, DbError> {
        match text(&row, field)?.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(DbError::BadRow(format!(
                "{field} was neither true nor false: {other}"
            ))),
        }
    };
    Ok(Account {
        superuser: flag("superuser")?,
        create_role: flag("create_role")?,
        create_db: flag("create_db")?,
        replication: flag("replication")?,
        login_superuser: flag("login_superuser")?,
        predefined,
        shared,
    })
}

/// Whether the scratch login owns the database it is connected to. The
/// supplied database is cleaned with `DROP OWNED`, which also revokes what
/// was granted to the login; only an owner keeps its rights through that.
pub async fn owns_database(conn: &mut impl QueryConnection) -> Result<bool, DbError> {
    let rows = conn
        .query(
            "SELECT (d.datdba = (SELECT r.oid FROM pg_catalog.pg_roles r \
                                  WHERE r.rolname = session_user))::text AS owns \
               FROM pg_catalog.pg_database d \
              WHERE d.datname = pg_catalog.current_database()",
        )
        .await?;
    match text(&one(rows, "the scratch database's owner")?, "owns")?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(DbError::BadRow(format!(
            "database ownership was neither true nor false: {other}"
        ))),
    }
}

/// The objects in the connected database that initdb did not create, the
/// first [`NAMED`] described by the engine, and how many there are. A fresh
/// database from `template0` has none; one cloned from a `template1` that
/// was written to has that template's.
pub async fn foreign_objects(
    conn: &mut impl QueryConnection,
) -> Result<(Vec<String>, usize), DbError> {
    let union = CATALOGS
        .iter()
        .map(|catalog| {
            format!(
                "SELECT 'pg_catalog.{catalog}'::pg_catalog.regclass AS classid, o.oid AS objid \
                   FROM pg_catalog.{catalog} o WHERE o.oid >= {FIRST_NORMAL_OBJECT_ID}"
            )
        })
        .chain(EVERY_ROW.iter().map(|(catalog, class)| {
            format!(
                "SELECT 'pg_catalog.{class}'::pg_catalog.regclass AS classid, o.oid AS objid \
                   FROM pg_catalog.{catalog} o"
            )
        }))
        .chain(DATABASE_KEYED.iter().map(|(catalog, database)| {
            format!(
                "SELECT 'pg_catalog.{catalog}'::pg_catalog.regclass AS classid, o.oid AS objid \
                   FROM pg_catalog.{catalog} o WHERE o.oid >= {FIRST_NORMAL_OBJECT_ID} \
                    AND o.{database} = (SELECT d.oid FROM pg_catalog.pg_database d \
                                         WHERE d.datname = pg_catalog.current_database())"
            )
        }))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.pg_describe_object(f.classid, f.objid, 0) AS object, \
                    pg_catalog.count(*) OVER ()::text AS total \
               FROM ({union}) f ORDER BY 1 LIMIT {NAMED}"
        ))
        .await?;
    let total = match rows.first() {
        Some(row) => text(row, "total")?
            .parse()
            .map_err(|_| DbError::BadRow("an object count was not a number".into()))?,
        None => 0,
    };
    let named = rows
        .iter()
        .map(|row| text(row, "object"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((named, total))
}

/// Everything outside the run's own database that `DROP OWNED BY
/// SESSION_USER` would reach, read where `DROP OWNED` itself reads it: the
/// shared dependencies on the login, in this database and in the shared
/// catalogs. Each is named; the supplied layout refuses while any exists,
/// since the cleanup would drop or revoke it (#1678 review).
///
/// One entry is the run's own and expected: the login's ownership of, or a
/// privilege on, the database it compiles in, which `DROP OWNED` keeps.
/// Ownership of a shared object is left out too: `DROP OWNED` drops no
/// database, tablespace or subscription, so a login that owns another
/// scratch database is not refused for it. Only the privileges and
/// memberships among the shared rows are revoked.
/// Measured on 16 and 18, this one read covers every case found one by one
/// before it: a grant to the login on another database, tablespace or
/// parameter; a role membership the login granted, to another role or to
/// itself; an object it owns here, a large object with a chosen low OID
/// included. Objects the login owns in other databases are out of `DROP
/// OWNED`'s reach from here and are not listed.
pub async fn cleanup_reach(conn: &mut impl QueryConnection) -> Result<Vec<String>, DbError> {
    let rows = conn
        .query(
            "WITH login AS (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = session_user), \
                  here AS (SELECT d.oid FROM pg_catalog.pg_database d \
                            WHERE d.datname = pg_catalog.current_database()) \
             SELECT CASE WHEN s.classid = 'pg_catalog.pg_auth_members'::pg_catalog.regclass \
                         THEN (SELECT 'membership of ' || m.rolname || ' in ' || r.rolname \
                                 FROM pg_catalog.pg_auth_members a \
                                 JOIN pg_catalog.pg_roles r ON r.oid = a.roleid \
                                 JOIN pg_catalog.pg_roles m ON m.oid = a.member \
                                WHERE a.oid = s.objid) \
                         ELSE pg_catalog.pg_describe_object(s.classid, s.objid, s.objsubid) \
                    END AS object \
               FROM pg_catalog.pg_shdepend s, login, here \
              WHERE s.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass \
                AND s.refobjid = login.oid \
                AND (s.dbid = here.oid OR (s.dbid = 0 AND s.deptype <> 'o')) \
                AND NOT (s.classid = 'pg_catalog.pg_database'::pg_catalog.regclass \
                         AND s.objid = here.oid) \
             ORDER BY 1",
        )
        .await?;
    rows.iter()
        .map(|row| {
            // A row the describe cannot name is still a row: never absent.
            Ok(text(row, "object").unwrap_or_else(|_| "an unnamed dependency".to_owned()))
        })
        .collect()
}

/// Creates each in-scope schema the supplied database lacks, owned by the
/// scratch login. The run-owned path reproduces schemas with their owners
/// and grants instead; a confined login can create no owner to give them.
pub async fn create_schemas(
    conn: &mut impl ExecuteConnection,
    schemas: &[String],
) -> Result<(), DbError> {
    for schema in schemas {
        if super::authorization::is_system_schema(schema) {
            continue;
        }
        conn.execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            schema.replace('"', "\"\"")
        ))
        .await?;
    }
    Ok(())
}

/// Removes everything the scratch login owns in the supplied database, so
/// the next run finds it empty. Shared objects are untouched; the database
/// itself stays, owned by the login (`owns_database`).
///
/// The connection is the one the run compiled on, so it may be inside a
/// failed transaction or under a role a declaration set; both are ended
/// first: `SET ROLE NONE`, not `RESET ROLE`, which returns to a role the
/// login's defaults set. Neither statement fails when there is nothing to
/// end. The drop
/// runs in one transaction with a check that it is on the backend the run
/// checked, and is never sent anywhere else.
pub async fn drop_owned(
    conn: &mut impl ExecuteConnection,
    checked: &Backend,
) -> Result<(), DbError> {
    conn.execute("ROLLBACK").await?;
    conn.execute("SET ROLE NONE").await?;
    conn.execute("BEGIN").await?;
    let bound = on_backend(conn, checked).await;
    if !matches!(bound, Ok(true)) {
        conn.execute("ROLLBACK").await?;
        bound?;
        return Err(DbError::BadRow(
            "the connection now reaches another backend than the one the run checked".into(),
        ));
    }
    conn.execute("DROP OWNED BY SESSION_USER").await?;
    conn.execute("COMMIT").await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(pid: i64, database: u32) -> SessionMark {
        SessionMark { pid, database }
    }

    #[test]
    fn a_target_session_the_scratch_session_sees_is_the_same_cluster() {
        let target = ("aa", mark(10, 5));
        let seen = vec![("aa".to_owned(), 10), ("bb".to_owned(), 20)];
        assert_eq!(
            placement(target, ("bb", mark(20, 7)), &seen),
            Ok(Placement::SameCluster)
        );
        assert_eq!(
            placement(target, ("bb", mark(20, 5)), &seen),
            Ok(Placement::Target)
        );
    }

    #[test]
    fn an_unseen_target_session_is_another_cluster_only_when_the_scratch_sees_itself() {
        let target = ("aa", mark(10, 5));
        let scratch = ("bb", mark(20, 5));
        // Same database OID on two clusters is ordinary: the OID says nothing.
        assert_eq!(
            placement(target, scratch, &[("bb".to_owned(), 20)]),
            Ok(Placement::SeparateCluster)
        );
        // Negative: nothing seen is unreadable, never "another cluster".
        assert!(placement(target, scratch, &[]).is_err());
        // Negative: the target's token on another backend is not its session.
        assert_eq!(
            placement(
                target,
                scratch,
                &[("bb".to_owned(), 20), ("aa".to_owned(), 11)]
            ),
            Ok(Placement::SeparateCluster)
        );
    }

    #[test]
    fn an_account_is_confined_only_without_any_reach_outside_its_database() {
        let none = Account {
            superuser: false,
            create_role: false,
            create_db: false,
            replication: false,
            login_superuser: false,
            predefined: Vec::new(),
            shared: Vec::new(),
        };
        assert!(none.confined() && !none.provisions());
        assert!(none.excess().is_empty());
        let db_only = Account {
            create_db: true,
            ..none.clone()
        };
        assert!(!db_only.confined() && !db_only.provisions());
        assert_eq!(db_only.excess(), ["NOCREATEDB"]);
        // Negative: a replication slot holds WAL for the whole cluster.
        let replication = Account {
            replication: true,
            ..none.clone()
        };
        assert_eq!(replication.excess(), ["NOREPLICATION"]);
        // Negative: a predefined role acting outside the database, with no
        // attribute at all.
        let program = Account {
            predefined: vec!["pg_execute_server_program".into()],
            ..none.clone()
        };
        assert!(!program.confined() && !program.provisions());
        assert_eq!(
            program.excess(),
            ["no membership in pg_execute_server_program"]
        );
        // Negative: authority over another shared object, with no
        // attribute and no predefined role.
        let owner = Account {
            shared: vec!["ownership of database other".into()],
            ..none.clone()
        };
        assert_eq!(owner.excess(), ["no ownership of database other"]);
        // Negative: both attributes without superuser cannot act as the
        // roles it would create, so it does not provision.
        let both = Account {
            create_role: true,
            create_db: true,
            ..none.clone()
        };
        assert!(!both.provisions() && !both.confined());
        // Negative: a member of a superuser role is not confined, but it does
        // not act as a superuser, so it does not provision either.
        let member = Account {
            superuser: true,
            ..none.clone()
        };
        assert!(!member.provisions() && !member.confined());
        let superuser = Account {
            superuser: true,
            login_superuser: true,
            ..none
        };
        assert!(superuser.provisions() && !superuser.confined());
    }

    #[test]
    fn a_token_that_is_not_generated_hex_is_refused() {
        assert!(token_literal("0a9f").is_ok());
        for bad in ["", "0A9F", "x'); DROP", "a b"] {
            assert!(token_literal(bad).is_err(), "{bad}");
        }
    }
}
