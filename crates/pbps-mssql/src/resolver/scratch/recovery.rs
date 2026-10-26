//! What a killed run leaves on a SQL Server instance, and how a later run
//! tells that the run is gone (#2410, DEC-2410.2).
//!
//! It is the same design as PostgreSQL's (`pbps_pg::resolver::recovery`), with
//! the engine's own pieces:
//!
//! - **The proof is the marker.** A run creates `##pbps_<token>` on the
//!   session it keeps open for the whole run (`scratch::mark`) before it
//!   creates anything. A global temporary table is visible instance-wide to a
//!   login with no privilege at all, disappears with the session that made it,
//!   and is gone after an instance restart (measured). The session views are
//!   not an alternative: without VIEW SERVER STATE a login sees only its own
//!   session. A reader that cannot see its own marker proves nothing.
//! - **The name is the record** for what pbps names: the login
//!   `pbps_run_<T>` (first made, last dropped), the databases
//!   `pbps_scratch_<T>` and `pbps_reference_<T>`, the sandbox user
//!   `pbps_sandbox_<T>`.
//! - **A companion database is the exception.** It must carry the
//!   dependency's own name for cross-database references to resolve, so it
//!   cannot carry the token, and a leftover one would refuse every later run
//!   that needs the name. Its record is one property of the run's scratch
//!   database per companion, written before the companion exists, and the
//!   companion is marked with its owner once it does. One property each, not
//!   one list: a property value holds at most 7,500 bytes, and a list of long
//!   names would outgrow it and refuse a valid run. A database named in the record that
//!   does not carry the run's mark is not provably the run's and is kept.
//!
//! The scratch database's own properties are part of what the emptiness check
//! reads, so the check ignores [`RUN_PROPERTY`] and every
//! [`COMPANION_PREFIX`] property.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::DbError;
use pbps_db::transport::{ExecuteConnection, QueryConnection};

use super::{ident, literal, text};

/// Digits in a run's token.
pub const TOKEN_LEN: usize = 32;

/// The database-level property on a companion that names the run that made it.
pub const RUN_PROPERTY: &str = "pbps_run";

/// The prefix of the database-level properties on a run's scratch database
/// that name, one each, the companion databases the run is about to create or
/// has created: `pbps_companion_<n>`, numbered in creation order.
pub const COMPANION_PREFIX: &str = "pbps_companion_";

fn is_token(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The token a name of the form `<prefix><token>` carries, or `None` for a
/// name of any other shape: another version's, another tool's.
fn token_after<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix).filter(|token| is_token(token))
}

/// What one run left on the instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leftover {
    pub login: String,
    pub database: Option<String>,
    pub reference: Option<String>,
}

/// What the runs other than `own` left on the instance, by token. Only a
/// token with a login is a run's: the login is made first and dropped last.
pub async fn leftovers(
    conn: &mut impl QueryConnection,
    own: &str,
) -> Result<BTreeMap<String, Leftover>, DbError> {
    let rows = conn
        .query(
            "SELECT N'database' AS kind, name FROM sys.databases \
              WHERE name LIKE N'pbps[_]scratch[_]%' \
             UNION ALL SELECT N'reference', name FROM sys.databases \
              WHERE name LIKE N'pbps[_]reference[_]%' \
             UNION ALL SELECT N'login', name FROM sys.server_principals \
              WHERE type = 'S' AND name LIKE N'pbps[_]run[_]%';",
        )
        .await?;
    let mut found: BTreeMap<String, Leftover> = BTreeMap::new();
    for row in &rows {
        let (kind, name) = (text(row, "kind")?, text(row, "name")?);
        let prefix = match kind.as_str() {
            "database" => "pbps_scratch_",
            "reference" => "pbps_reference_",
            _ => "pbps_run_",
        };
        let Some(token) = token_after(&name, prefix) else {
            continue;
        };
        if token == own {
            continue;
        }
        let entry = found.entry(token.to_owned()).or_default();
        match kind.as_str() {
            "database" => entry.database = Some(name),
            "reference" => entry.reference = Some(name),
            _ => entry.login = name,
        }
    }
    found.retain(|_, left| !left.login.is_empty());
    Ok(found)
}

/// Which of `tokens` still have their marker, or `None` when this session
/// cannot see its own `own`, which proves nothing about the others.
pub async fn alive(
    conn: &mut impl QueryConnection,
    own: &str,
    tokens: &[String],
) -> Result<Option<BTreeSet<String>>, DbError> {
    if !is_token(own) || !tokens.iter().all(|token| is_token(token)) {
        return Err(DbError::BadRow(
            "a run token was not 32 lowercase hexadecimal digits".into(),
        ));
    }
    let all: Vec<&str> = tokens.iter().map(String::as_str).chain([own]).collect();
    let query = all
        .iter()
        .map(|token| {
            format!(
                "SELECT N'{token}' AS token, CASE WHEN OBJECT_ID(N'tempdb..##pbps_{token}') \
                 IS NULL THEN 0 ELSE 1 END AS up"
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let mut up = BTreeSet::new();
    for row in conn.query(&format!("{query};")).await? {
        if row.try_get::<i32>("up")? == Some(1) {
            up.insert(text(&row, "token")?);
        }
    }
    Ok(up.contains(own).then_some(up))
}

/// The application lock that serialises the runs on one supplied database,
/// held from before the first read of what a dead run left until the run's
/// own sandbox user exists. Two runs that read the same dead sandbox would
/// both clean it, and the late one would destroy what the first, already
/// working, had made; two fresh runs would both pass the emptiness check.
/// Application locks are scoped to the database they are taken in (measured:
/// held in one database, free in another), which is the scope needed here.
/// The lock is the session's, so a killed run frees it with its session
/// (measured: within a poll of 215 ms).
pub const LOCK: &str = "pbps_scratch_acquire";

/// Why the lock was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Another session held it for the whole wait.
    Busy,
    Db(DbError),
}

/// Takes the lock on the connected database for this session, waiting at most
/// `wait_ms` for whoever holds it.
pub async fn lock(conn: &mut impl QueryConnection, wait_ms: u32) -> Result<(), LockError> {
    let rows = conn
        .query(&format!(
            "DECLARE @r int; EXEC @r = sys.sp_getapplock @Resource = {}, @LockMode = N'Exclusive', \
             @LockOwner = N'Session', @LockTimeout = {wait_ms}; SELECT @r AS result;",
            literal(LOCK)
        ))
        .await
        .map_err(LockError::Db)?;
    match rows
        .first()
        .map(|row| row.try_get::<i32>("result"))
        .transpose()
        .map_err(LockError::Db)?
        .flatten()
    {
        // Granted at once, or after the wait.
        Some(0 | 1) => Ok(()),
        Some(-1) => Err(LockError::Busy),
        other => Err(LockError::Db(DbError::BadRow(format!(
            "sp_getapplock answered {other:?}"
        )))),
    }
}

/// Releases it. Releasing what is not held is an error (1223): a caller that
/// does not know whether it holds it ignores the answer, since the session's
/// end releases it anyway.
pub async fn unlock(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    conn.execute(&format!(
        "EXEC sys.sp_releaseapplock @Resource = {}, @LockOwner = N'Session';",
        literal(LOCK)
    ))
    .await
}

/// The tokens of the sandbox users in the connected database, `own` apart.
pub async fn sandboxes(conn: &mut impl QueryConnection, own: &str) -> Result<Vec<String>, DbError> {
    let rows = conn
        .query(
            "SELECT name FROM sys.database_principals \
              WHERE name LIKE N'pbps[_]sandbox[_]%' ORDER BY name;",
        )
        .await?;
    let mut tokens = Vec::new();
    for row in &rows {
        let name = text(row, "name")?;
        if let Some(token) = token_after(&name, "pbps_sandbox_")
            && token != own
        {
            tokens.push(token.to_owned());
        }
    }
    Ok(tokens)
}

/// Whether the sandbox user `name` of the current database is a member of
/// `db_owner`. A run creates the user and then adds it to the role, so a run
/// killed in between leaves a user it never acted as, which therefore owns
/// nothing to clean up as itself.
pub async fn sandbox_owns(conn: &mut impl QueryConnection, name: &str) -> Result<bool, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT CONVERT(int, ISNULL(IS_ROLEMEMBER(N'db_owner', {}), 0)) AS owns;",
            literal(name)
        ))
        .await?;
    Ok(rows
        .first()
        .map(|row| row.try_get::<i32>("owns"))
        .transpose()?
        .flatten()
        == Some(1))
}

/// Records, in the run's scratch database and before it exists, the `index`th
/// companion database the run creates.
pub async fn record_companion(
    conn: &mut impl ExecuteConnection,
    scratch: &str,
    index: usize,
    name: &str,
) -> Result<(), DbError> {
    conn.execute(&format!(
        "EXEC {}.sys.sp_addextendedproperty @name = {}, @value = {};",
        ident(scratch),
        literal(&format!("{COMPANION_PREFIX}{index}")),
        literal(name)
    ))
    .await
}

/// Marks a companion database with the run that made it.
pub async fn mark_companion(
    conn: &mut impl ExecuteConnection,
    companion: &str,
    token: &str,
) -> Result<(), DbError> {
    conn.execute(&format!(
        "EXEC {}.sys.sp_addextendedproperty @name = {}, @value = {};",
        ident(companion),
        literal(RUN_PROPERTY),
        literal(token)
    ))
    .await
}

/// The companion databases a run's scratch database records, in order.
pub async fn recorded_companions(
    conn: &mut impl QueryConnection,
    scratch: &str,
) -> Result<Vec<String>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT CONVERT(nvarchar(max), value) AS value FROM {}.sys.extended_properties \
              WHERE class = 0 AND name LIKE N'pbps[_]companion[_]%' \
              ORDER BY TRY_CONVERT(int, SUBSTRING(name, {}, 10));",
            ident(scratch),
            COMPANION_PREFIX.len() + 1
        ))
        .await?;
    rows.iter().map(|row| text(row, "value")).collect()
}

/// Whose a database is: not there, marked by `token` or another, or unmarked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Missing,
    Run(String),
    Unmarked,
}

pub async fn owner_of(conn: &mut impl QueryConnection, database: &str) -> Result<Owner, DbError> {
    let exists = conn
        .query(&format!(
            "SELECT CASE WHEN DB_ID({}) IS NULL THEN 0 ELSE 1 END AS there;",
            literal(database)
        ))
        .await?;
    if exists
        .first()
        .map(|r| r.try_get::<i32>("there"))
        .transpose()?
        .flatten()
        != Some(1)
    {
        return Ok(Owner::Missing);
    }
    let rows = conn
        .query(&format!(
            "SELECT CONVERT(nvarchar(128), value) AS value FROM {}.sys.extended_properties \
              WHERE class = 0 AND name = {};",
            ident(database),
            literal(RUN_PROPERTY)
        ))
        .await?;
    Ok(match rows.first() {
        Some(row) => Owner::Run(text(row, "value")?),
        None => Owner::Unmarked,
    })
}

/// Drops a database if it is there. `force` closes the sessions still
/// connected to it, which only the run that owns it may do; a later run
/// removing a dead run's database does not, so a session the engine still
/// shows refuses the drop (error 3702, measured) whatever was decided about
/// its owner.
pub async fn drop_database(
    conn: &mut impl ExecuteConnection,
    database: &str,
    force: bool,
) -> Result<(), DbError> {
    let (db, name) = (ident(database), literal(database));
    let single = if force {
        format!("ALTER DATABASE {db} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; ")
    } else {
        String::new()
    };
    conn.execute(&format!(
        "USE master; IF DB_ID({name}) IS NOT NULL BEGIN {single}DROP DATABASE {db}; END;"
    ))
    .await
}

/// Drops the run's login. It goes last: it is the record by which a later run
/// finds what an interrupted release left.
pub async fn drop_login(conn: &mut impl ExecuteConnection, login: &str) -> Result<(), DbError> {
    conn.execute(&format!(
        "IF SUSER_ID({}) IS NOT NULL DROP LOGIN {};",
        literal(login),
        ident(login)
    ))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn only_a_complete_run_shaped_name_carries_a_token() {
        assert_eq!(token_after(&format!("pbps_run_{T}"), "pbps_run_"), Some(T));
        for bad in [
            "pbps_run_0123456789abcdef",
            &format!("pbps_run_{}", T.to_uppercase()),
            &format!("pbps_run_{T}0"),
            &format!("pbps_scratch_{T}"),
            "pbps_run_",
        ] {
            assert_eq!(token_after(bad, "pbps_run_"), None, "{bad}");
        }
    }

    #[test]
    fn the_companion_record_is_a_json_list_of_names_that_may_hold_quotes() {
        let names = vec!["Reporting".to_owned(), "it's [odd]".to_owned()];
        let json = serde_json::to_string(&names).unwrap();
        assert_eq!(serde_json::from_str::<Vec<String>>(&json).unwrap(), names);
        assert!(literal(&json).contains("it''s"), "quotes are doubled");
    }
}
