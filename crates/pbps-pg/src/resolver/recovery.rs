//! What a killed run leaves in a PostgreSQL cluster, and how a later run
//! tells that the run is gone (#2410; DEC-2410.1 has the reasoning).
//!
//! A run-owned run names its login, database and run-local roles with its
//! token, so none needs a record of its own; the login is created first and
//! dropped last, so it anchors the rest. The proof of death is a session named
//! `pbps-<T>` in `application_name`, which every role sees in the
//! cluster-wide `pg_stat_activity`. A supplied run's record is the
//! [`CLAIM`] schema, the first object it creates.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::resolver::is_run_token;
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
    for row in conn.query(FOUND).await? {
        let name = text(&row, "name")?;
        let token = token_of(&name).to_owned();
        if token == own {
            continue;
        }
        let kind = text(&row, "kind")?;
        let entry = found.entry(token).or_default();
        match kind.as_str() {
            "login" => entry.login = name,
            "database" => entry.database = Some(name),
            _ => entry.roles.push(name),
        }
    }
    found.retain(|_, leftover| !leftover.login.is_empty());
    Ok(found)
}

/// Which of `tokens` still have a session, or `None` when this session cannot
/// see its own `own` among them, which proves nothing about the others.
pub async fn alive(
    conn: &mut impl QueryConnection,
    own: &str,
    tokens: &[String],
) -> Result<Option<BTreeSet<String>>, DbError> {
    if !is_run_token(own) || !tokens.iter().all(|token| is_run_token(token)) {
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

/// The mark a keeper leaves once it has connected (DEC-2410.3): the comment on
/// the `postgres` database of the server the run started, which outlives the
/// session.
const KEPT: &str = "pbps-keeper";

/// Leaves the mark of a keeper that has connected.
pub async fn mark_kept(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    conn.execute(&format!("COMMENT ON DATABASE postgres IS '{KEPT}'"))
        .await
}

/// Whether a keeper has ever connected to this server. A reader reads this
/// before it reads the session: the mark is written only once the session
/// exists, so a mark read first can only be older than the session read after.
pub async fn kept(conn: &mut impl QueryConnection) -> Result<bool, DbError> {
    let marks = conn
        .query(
            "SELECT pg_catalog.shobj_description(oid, 'pg_database') AS comment \
               FROM pg_catalog.pg_database WHERE datname = 'postgres'",
        )
        .await?;
    Ok(marks
        .first()
        .is_some_and(|row| row.try_get::<&str>("comment").ok().flatten() == Some(KEPT)))
}

/// The advisory lock that serialises runs in one supplied database: `"pbps"`
/// and the issue. It is private to its database, the scope wanted here.
pub const LOCK: (i32, i32) = (0x7062_7073, 2410);

/// How long a run waits for the lock before it refuses.
const LOCK_WAIT: &str = "60s";

fn advisory(function: &str) -> String {
    let (class, object) = LOCK;
    format!("SELECT pg_catalog.{function}({class}, {object})")
}

/// Runs `body` in a transaction, rolled back when it fails.
async fn transaction<C: ExecuteConnection>(
    conn: &mut C,
    body: impl AsyncFnOnce(&mut C) -> Result<(), DbError>,
) -> Result<(), DbError> {
    conn.execute("BEGIN").await?;
    match body(conn).await {
        Ok(()) => conn.execute("COMMIT").await,
        Err(error) => {
            let _ = conn.execute("ROLLBACK").await;
            Err(error)
        }
    }
}

/// Takes the lock for the session: `false` when another run held it for all
/// of [`LOCK_WAIT`]. The `lock_timeout` is the transaction's own.
pub async fn lock(conn: &mut impl ExecuteConnection) -> Result<bool, DbError> {
    match transaction(conn, async |conn| {
        conn.execute(&format!("SET LOCAL lock_timeout = '{LOCK_WAIT}'"))
            .await?;
        conn.execute(&advisory("pg_advisory_lock")).await
    })
    .await
    {
        Err(error) if error.server_error_code().as_deref() == Some("55P03") => Ok(false),
        taken => taken.map(|()| true),
    }
}

/// Releases the lock; a session that does not hold it has nothing to release.
pub async fn unlock(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    conn.execute(&advisory("pg_advisory_unlock")).await
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
    /// The cluster-wide roles the run made, recorded before it makes them:
    /// they are not in the database, so emptying it does not reach them
    /// (#2379). Left out when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
}

impl Claim {
    pub fn new(token: &str, defaults: BTreeSet<(String, String)>, acl: BTreeSet<Entry>) -> Self {
        Self {
            v: 1,
            token: token.to_owned(),
            defaults,
            acl,
            roles: Vec::new(),
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
    transaction(conn, async |conn| create(conn, &json).await)
        .await
        .map_err(|error| match error.server_error_code().as_deref() {
            Some("42P06") => ClaimError::Taken,
            _ => ClaimError::Db(error),
        })
}

/// The claim schema and its comment, inside the caller's transaction. A
/// cleanup that took the claim uses it to put the same one back.
pub(crate) async fn create(
    conn: &mut impl ExecuteConnection,
    comment: &str,
) -> Result<(), DbError> {
    conn.execute(&format!("CREATE SCHEMA {CLAIM}")).await?;
    set_comment(conn, comment).await
}

async fn set_comment(conn: &mut impl ExecuteConnection, comment: &str) -> Result<(), DbError> {
    // The literal below doubles quotes and nothing else, which is whole only
    // while backslashes are literal.
    conn.execute("SET LOCAL standard_conforming_strings = on")
        .await?;
    conn.execute(&format!(
        "COMMENT ON SCHEMA {CLAIM} IS '{}'",
        comment.replace('\'', "''")
    ))
    .await
}

/// Records the login's defaults as they now stand in the claim, in the
/// caller's transaction (DEC-2410.1). The schema stays: a new one would show
/// as the externals file's. No claim, no move.
pub async fn rebase(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    let Some(Ok(mut claim)) = read_claim(conn).await? else {
        return Ok(());
    };
    claim.defaults = super::scratch::login_defaults(conn).await?;
    let json = serde_json::to_string(&claim).map_err(|error| DbError::BadRow(error.to_string()))?;
    conn.execute("SET LOCAL ROLE NONE").await?;
    set_comment(conn, &json).await
}

/// Adds the roles the run is about to create to its claim, so that a release,
/// or the run that finds the claim dead, drops them too. The schema stays.
pub async fn record_roles(
    conn: &mut impl ExecuteConnection,
    roles: &[String],
) -> Result<(), DbError> {
    transaction(conn, async |conn| {
        let Some(Ok(mut claim)) = read_claim(conn).await? else {
            return Err(DbError::BadRow(
                "the run's claim is not in its scratch database".into(),
            ));
        };
        for role in roles {
            if !claim.roles.contains(role) {
                claim.roles.push(role.clone());
            }
        }
        let json =
            serde_json::to_string(&claim).map_err(|error| DbError::BadRow(error.to_string()))?;
        conn.execute("SET LOCAL ROLE NONE").await?;
        set_comment(conn, &json).await
    })
    .await
}

/// Removes the claim: the last step of a release.
pub async fn remove(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    conn.execute(&format!("DROP SCHEMA IF EXISTS {CLAIM}"))
        .await
}

/// The claim schema's comment: `None` when there is no claim, `Some(None)`
/// when it has none.
pub(crate) async fn comment(
    conn: &mut impl QueryConnection,
) -> Result<Option<Option<String>>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment \
               FROM pg_catalog.pg_namespace n WHERE n.nspname = '{CLAIM}'"
        ))
        .await?;
    rows.first()
        .map(|row| Ok(row.try_get::<&str>("comment")?.map(str::to_owned)))
        .transpose()
}

/// The claim in this database: `None` when there is none, `Err` with why when
/// there is one this version cannot read.
pub async fn read_claim(
    conn: &mut impl QueryConnection,
) -> Result<Option<Result<Claim, String>>, DbError> {
    let Some(comment) = comment(conn).await? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_str::<Claim>(&comment.unwrap_or_default())
            .map_err(|e| format!("its comment is not a claim this version reads ({e})"))
            .and_then(|claim| match claim.v {
                1 => Ok(claim),
                v => Err(format!(
                    "it is version {v}, which this version does not read"
                )),
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

    /// A rehearsal's roles are the cluster's, so the claim names them for the
    /// release and for the run that finds the claim dead; a claim without any
    /// is the claim it always was, on the wire and when read.
    #[test]
    fn a_claim_names_the_roles_a_rehearsal_made_and_omits_none() {
        let t = "0123456789abcdef0123456789abcdef";
        let mut claim = Claim::new(t, BTreeSet::new(), BTreeSet::new());
        let plain = serde_json::to_value(&claim).unwrap();
        assert!(plain.get("roles").is_none(), "{plain}");
        claim.roles.push(format!("pbps_rehearsal_0_{t}"));
        let named = serde_json::to_string(&claim).unwrap();
        assert_eq!(serde_json::from_str::<Claim>(&named).unwrap(), claim);
        // Negative: what an earlier version wrote still reads, with no roles.
        let earlier = serde_json::from_value::<Claim>(plain).unwrap();
        assert!(earlier.roles.is_empty());
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
