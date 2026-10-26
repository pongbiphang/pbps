//! What a killed run leaves in a PostgreSQL cluster, and how a later run
//! tells that the run is gone (#2410, DEC-2410.1).
//!
//! # The record is the name
//!
//! A run-owned run creates its login first, then its database, then its
//! run-local roles, and every one of them is named with the run's token
//! (`pbps_run_<T>`, `pbps_scratch_<T>`, `pbps_role_<n>_<T>`, and the roles a
//! rehearsal maps the target's to, `pbps_rehearsal_<n>_<T>`, #2379). A name needs no
//! write of its own and cannot be missed by a listing, so there is no moment
//! at which a resource exists without its record. The login is the anchor:
//! it is created before the rest and dropped after it, so a token with no
//! login has nothing the run left unfinished, and a name that merely has the
//! shape of one (another version's, another tool's) is never touched.
//!
//! # The proof is a session
//!
//! A run keeps one session open from before its first write to its release,
//! named `pbps-<T>` in `application_name` (`scratch::mark` writes the same
//! value). Every role sees every session's `application_name` in
//! `pg_stat_activity` (measured on 16 and 18), the view is cluster-wide where
//! an advisory lock is private to one database (measured: held in one
//! database, free in another), and no clock is involved. A token whose
//! session is not there is dead. A reader that cannot see its own session
//! cannot say anything about another's, and proves nothing.
//!
//! # A supplied database holds a claim
//!
//! The supplied layout creates nothing at cluster level; what it leaves is
//! inside the database the account owns. Its record is a schema, the first
//! object the run creates, whose comment carries the token and the state the
//! release needs to restore. Creating it is the arbitration between two runs
//! that start together.
//!
//! Cleaning a dead claim is not covered by that: two runs can read the same
//! one, and the second's cleanup would empty the first's objects. The
//! database lock ([`lock`]) is held from before the claim is read to the
//! moment the run's own is made. And the claim is the last thing a release
//! removes ([`remove`]): the cleanup's `DROP OWNED` puts it back ([`keep`]),
//! so a kill before the privileges are restored leaves the snapshot to
//! restore them from.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Row};

use super::standard::Entry;

/// Digits in a run's token.
pub const TOKEN_LEN: usize = 32;

/// The schema a supplied run creates first.
pub const CLAIM: &str = "pbps_claim";

/// What one dead-or-alive token left in the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leftover {
    pub login: String,
    pub database: Option<String>,
    /// Run-local roles, in the order the run made them.
    pub roles: Vec<String>,
}

/// Names of run-owned resources, by their exact shape. The shape and the
/// token are the whole of the record, so nothing else matches.
const FOUND: &str = "\
SELECT 'database' AS kind, d.datname::text AS name FROM pg_catalog.pg_database d \
 WHERE d.datname ~ '^pbps_scratch_[0-9a-f]{32}$' \
UNION ALL \
SELECT 'login', r.rolname::text FROM pg_catalog.pg_roles r \
 WHERE r.rolname ~ '^pbps_run_[0-9a-f]{32}$' \
UNION ALL \
SELECT 'role', r.rolname::text FROM pg_catalog.pg_roles r \
 WHERE r.rolname ~ '^pbps_(role|rehearsal)_[0-9]+_[0-9a-f]{32}$' \
ORDER BY 2";

fn text(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("{field} was null")))
}

/// The token a run-owned name carries: its last [`TOKEN_LEN`] digits.
fn token_of(name: &str) -> &str {
    &name[name.len() - TOKEN_LEN..]
}

/// What the runs other than `own` left in this cluster, by token. Only a
/// token with a login is a run's: the login is made first and dropped last.
pub async fn leftovers(
    conn: &mut impl QueryConnection,
    own: &str,
) -> Result<BTreeMap<String, Leftover>, DbError> {
    let mut found: BTreeMap<String, Leftover> = BTreeMap::new();
    let mut unanchored: BTreeMap<String, Leftover> = BTreeMap::new();
    for row in conn.query(FOUND).await? {
        let name = text(&row, "name")?;
        let token = token_of(&name).to_owned();
        if token == own {
            continue;
        }
        let kind = text(&row, "kind")?;
        let entry = unanchored.entry(token).or_default();
        match kind.as_str() {
            "login" => entry.login = name,
            "database" => entry.database = Some(name),
            _ => entry.roles.push(name),
        }
    }
    for (token, leftover) in unanchored {
        if !leftover.login.is_empty() {
            found.insert(token, leftover);
        }
    }
    Ok(found)
}

/// Which of `tokens` still have a session, or `None` when this session cannot
/// see its own `own` among them, which proves nothing about the others.
pub async fn alive(
    conn: &mut impl QueryConnection,
    own: &str,
    tokens: &[String],
) -> Result<Option<BTreeSet<String>>, DbError> {
    let hex = |token: &str| {
        token.len() == TOKEN_LEN
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !hex(own) || !tokens.iter().all(|token| hex(token)) {
        return Err(DbError::BadRow(
            "a run token was not 32 lowercase hexadecimal digits".into(),
        ));
    }
    let names = tokens
        .iter()
        .map(String::as_str)
        .chain([own])
        .map(|token| format!("'pbps-{token}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = conn
        .query(&format!(
            "SELECT a.application_name::text AS name, a.pid::text AS pid, \
                    pg_catalog.pg_backend_pid()::text AS me \
               FROM pg_catalog.pg_stat_activity a \
              WHERE a.application_name IN ({names})"
        ))
        .await?;
    let mut sessions = BTreeSet::new();
    let mut own_seen = false;
    for row in &rows {
        let name = text(row, "name")?;
        let token = name.trim_start_matches("pbps-").to_owned();
        if token == own && text(row, "pid")? == text(row, "me")? {
            own_seen = true;
        }
        sessions.insert(token);
    }
    Ok(own_seen.then_some(sessions))
}

/// The advisory lock that serialises runs in one supplied database: a pair of
/// integers, `"pbps"` and the issue that introduced it. An advisory lock is
/// private to the database it is taken in, which is why #2410 rejected it for
/// what is cluster-wide (liveness) and is the scope this needs.
pub const LOCK: (i32, i32) = (0x7062_7073, 2410);

/// Why the database lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another run held it for the whole wait.
    Busy,
    Db(DbError),
}

/// How long a run waits for the lock before it refuses. The holder is a run
/// between its start and its claim, or one cleaning a dead claim.
const LOCK_WAIT: &str = "60s";

/// Takes the lock, or gives up after [`LOCK_WAIT`]. Session-level: it
/// outlives the transaction it is asked in, and a session that ends releases
/// it. `lock_timeout` is local to that transaction, so the session's own
/// settings are not touched.
pub async fn lock(conn: &mut impl ExecuteConnection) -> Result<(), LockError> {
    let (class, object) = LOCK;
    conn.execute("BEGIN").await.map_err(LockError::Db)?;
    let taken = async {
        conn.execute(&format!("SET LOCAL lock_timeout = '{LOCK_WAIT}'"))
            .await?;
        conn.execute(&format!(
            "SELECT pg_catalog.pg_advisory_lock({class}, {object})"
        ))
        .await
    }
    .await;
    match taken {
        Ok(()) => conn.execute("COMMIT").await.map_err(LockError::Db),
        Err(error) => {
            let _ = conn.execute("ROLLBACK").await;
            // lock_not_available
            if error.server_error_code().as_deref() == Some("55P03") {
                Err(LockError::Busy)
            } else {
                Err(LockError::Db(error))
            }
        }
    }
}

/// Releases the lock this session holds. A session that never held it, or
/// lost it, has nothing to release.
pub async fn unlock(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    let (class, object) = LOCK;
    conn.execute(&format!(
        "SELECT pg_catalog.pg_advisory_unlock({class}, {object})"
    ))
    .await
}

/// What a supplied run records in its database before it creates anything
/// else there: who it is, and the state its release puts back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Claim {
    pub v: u8,
    pub token: String,
    /// The scratch account's own stored defaults as the run found them.
    pub defaults: BTreeSet<(String, String)>,
    /// The database's ACL as the run found it.
    pub acl: BTreeSet<Entry>,
}

impl Claim {
    pub fn new(token: &str, defaults: BTreeSet<(String, String)>, acl: BTreeSet<Entry>) -> Self {
        Self {
            v: 1,
            token: token.to_owned(),
            defaults,
            acl,
        }
    }
}

/// Why a claim could not be made.
#[derive(Debug)]
pub enum ClaimError {
    /// A claim is already there: another run started first.
    Taken,
    Db(DbError),
}

/// Creates the claim, atomically with its comment: a schema with no comment
/// cannot exist. The first writer wins; the other gets [`ClaimError::Taken`].
pub async fn claim(conn: &mut impl ExecuteConnection, claim: &Claim) -> Result<(), ClaimError> {
    let json =
        serde_json::to_string(claim).map_err(|e| ClaimError::Db(DbError::BadRow(e.to_string())))?;
    conn.execute("BEGIN").await.map_err(ClaimError::Db)?;
    match create(conn, &json).await {
        Ok(()) => conn.execute("COMMIT").await.map_err(ClaimError::Db),
        Err(error) => {
            let _ = conn.execute("ROLLBACK").await;
            if error.server_error_code().as_deref() == Some("42P06") {
                Err(ClaimError::Taken)
            } else {
                Err(ClaimError::Db(error))
            }
        }
    }
}

/// The claim schema and its comment, inside the transaction the caller
/// opened.
async fn create(conn: &mut impl ExecuteConnection, comment: &str) -> Result<(), DbError> {
    // The literal below doubles quotes and nothing else, which is whole only
    // while backslashes are literal.
    conn.execute("SET LOCAL standard_conforming_strings = on")
        .await?;
    conn.execute(&format!("CREATE SCHEMA {CLAIM}")).await?;
    conn.execute(&format!(
        "COMMENT ON SCHEMA {CLAIM} IS '{}'",
        comment.replace('\'', "''")
    ))
    .await
}

/// The text of the claim's comment, `None` when there is no claim or it has
/// none. Read inside the cleanup's transaction, so [`keep`] can put the same
/// claim back.
pub async fn comment(conn: &mut impl QueryConnection) -> Result<Option<String>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment \
               FROM pg_catalog.pg_namespace n WHERE n.nspname = '{CLAIM}'"
        ))
        .await?;
    Ok(match rows.first() {
        Some(row) => row.try_get::<&str>("comment")?.map(str::to_owned),
        None => None,
    })
}

/// Puts back a claim the cleanup's `DROP OWNED` took with everything else,
/// in the same transaction, so no moment shows a database without it: the
/// run's record stays until its last step has succeeded ([`remove`]).
pub async fn keep(conn: &mut impl ExecuteConnection, comment: &str) -> Result<(), DbError> {
    create(conn, comment).await
}

/// Removes the claim: the last thing a release does, once every other step
/// has succeeded.
pub async fn remove(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    conn.execute(&format!("DROP SCHEMA IF EXISTS {CLAIM}"))
        .await
}

/// The claim in this database: `None` when there is none, `Err` with why when
/// there is one this version cannot read.
pub async fn read_claim(
    conn: &mut impl QueryConnection,
) -> Result<Option<Result<Claim, String>>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment \
               FROM pg_catalog.pg_namespace n WHERE n.nspname = '{CLAIM}'"
        ))
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let comment = row.try_get::<&str>("comment")?.unwrap_or_default();
    Ok(Some(
        serde_json::from_str::<Claim>(comment)
            .map_err(|e| format!("its comment is not a claim this version reads ({e})"))
            .and_then(|claim| {
                if claim.v == 1 {
                    Ok(claim)
                } else {
                    Err(format!(
                        "it is version {}, which this version does not read",
                        claim.v
                    ))
                }
            }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_the_last_digits_of_any_run_owned_name() {
        let t = "0123456789abcdef0123456789abcdef";
        for name in [
            format!("pbps_run_{t}"),
            format!("pbps_scratch_{t}"),
            format!("pbps_role_17_{t}"),
            format!("pbps_rehearsal_3_{t}"),
        ] {
            assert_eq!(token_of(&name), t);
        }
    }

    #[test]
    fn a_claim_round_trips_and_an_unknown_version_is_not_read() {
        let claim = Claim::new(
            "0123456789abcdef0123456789abcdef",
            BTreeSet::from([("db".into(), "work_mem=4MB".into())]),
            BTreeSet::new(),
        );
        let json = serde_json::to_string(&claim).unwrap();
        assert_eq!(serde_json::from_str::<Claim>(&json).unwrap(), claim);
        assert!(serde_json::from_str::<Claim>("not json").is_err());
        assert!(serde_json::from_str::<Claim>(r#"{"v":1}"#).is_err());
    }
}
