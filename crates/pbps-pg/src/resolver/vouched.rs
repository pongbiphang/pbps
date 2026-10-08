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
    "pg_largeobject_metadata",
];

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

/// What the scratch account may do beyond its own database, through any role
/// it is a member of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    pub superuser: bool,
    pub create_role: bool,
    pub create_db: bool,
}

impl Account {
    /// Confined to the databases it is given: it can create no role and no
    /// database, and is no superuser (the decision on #1667).
    pub fn confined(&self) -> bool {
        !(self.superuser || self.create_role || self.create_db)
    }

    /// May create the run's own login, roles and database, and act as each
    /// of them. Only a superuser: a `CREATEROLE` login is granted `ADMIN` on
    /// the roles it creates but not `SET` (the default
    /// `createrole_self_grant`), so it cannot hand them the database or
    /// replay grants as them, and it cannot reproduce a superuser deployer.
    pub fn provisions(&self) -> bool {
        self.superuser
    }

    /// The attributes that make it unconfined, as `ALTER ROLE` spells their
    /// removal.
    pub fn excess(&self) -> Vec<&'static str> {
        [
            (self.superuser, "NOSUPERUSER"),
            (self.create_role, "NOCREATEROLE"),
            (self.create_db, "NOCREATEDB"),
        ]
        .into_iter()
        .filter_map(|(held, removal)| held.then_some(removal))
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

/// The scratch login's cluster-wide attributes, through every role it is a
/// member of: a membership it can `SET ROLE` to, or inherit from, is as good
/// as holding the attribute.
pub async fn account(conn: &mut impl QueryConnection) -> Result<Account, DbError> {
    let rows = conn
        .query(
            "SELECT coalesce(bool_or(r.rolsuper), false)::text AS superuser, \
                    coalesce(bool_or(r.rolcreaterole), false)::text AS create_role, \
                    coalesce(bool_or(r.rolcreatedb), false)::text AS create_db \
               FROM pg_catalog.pg_roles r \
              WHERE pg_catalog.pg_has_role(session_user, r.oid, 'MEMBER')",
        )
        .await?;
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

/// Every privilege granted directly to the scratch login on a shared object
/// other than its own database: another database, a tablespace, a
/// configuration parameter. `DROP OWNED`, which empties the supplied
/// database, revokes such a grant wherever the login has the authority to,
/// as a member of the granting role can (measured on 18), and that is a
/// write outside the run's own database. The supplied layout refuses while
/// any exists.
pub async fn shared_grants(conn: &mut impl QueryConnection) -> Result<Vec<String>, DbError> {
    let rows = conn
        .query(
            "WITH login AS (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = session_user) \
             SELECT 'database ' || d.datname AS object \
               FROM pg_catalog.pg_database d, pg_catalog.aclexplode(d.datacl) a, login \
              WHERE a.grantee = login.oid AND d.datname <> pg_catalog.current_database() \
             UNION \
             SELECT 'tablespace ' || t.spcname \
               FROM pg_catalog.pg_tablespace t, pg_catalog.aclexplode(t.spcacl) a, login \
              WHERE a.grantee = login.oid \
             UNION \
             SELECT 'parameter ' || p.parname \
               FROM pg_catalog.pg_parameter_acl p, pg_catalog.aclexplode(p.paracl) a, login \
              WHERE a.grantee = login.oid \
             ORDER BY 1",
        )
        .await?;
    rows.iter().map(|row| text(row, "object")).collect()
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
pub async fn drop_owned(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    conn.execute("DROP OWNED BY SESSION_USER").await
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
    fn an_account_is_confined_only_without_all_three_attributes() {
        let none = Account {
            superuser: false,
            create_role: false,
            create_db: false,
        };
        assert!(none.confined() && !none.provisions());
        assert!(none.excess().is_empty());
        let db_only = Account {
            create_db: true,
            ..none
        };
        assert!(!db_only.confined() && !db_only.provisions());
        assert_eq!(db_only.excess(), ["NOCREATEDB"]);
        // Negative: both attributes without superuser cannot act as the
        // roles it would create, so it does not provision.
        let both = Account {
            create_role: true,
            create_db: true,
            ..none
        };
        assert!(!both.provisions() && !both.confined());
        let superuser = Account {
            superuser: true,
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
