//! SQL Server's side of `pbps test` (ADR-0019 §3, §6): the identities of an
//! owned scratch, the privileges they are verified to hold, and the framing of
//! fixture SQL. Only this crate supplies the SQL; the CLI holds the connections,
//! deadlines and cleanup, as it does for the resolver's scratch.
//!
//! Nothing here runs user SQL with a provisioning credential: `provision`,
//! `create_users`, `namespaces` and `grants` take the administrator's
//! connection and send only SQL this module wrote. User SQL goes through
//! [`run_fixture`] on the fixture login's own connection.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::query::AssertionResult;
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{Conn, DbError, Driver, Row};
use pbps_model::Schema;

pub mod snapshot;

use crate::ident::quote;
use crate::resolver::scratch::literal;

/// The one database a run owns. The container is the run's, so the name needs
/// no run id: nothing else lives in it.
pub const DATABASE: &str = "pbps_test";

/// The three generated logins of a run (ADR-0019 §6). Each is a login and a
/// same-named user: the apply login is `dbo` of the owned database, the other
/// two are ordinary users with no role membership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identities {
    pub apply: String,
    pub fixture: String,
    pub assertion: String,
}

impl Identities {
    pub fn for_run(run_id: &str) -> Self {
        let name = |kind: &str| format!("pbps_test_{kind}_{run_id}");
        Self {
            apply: name("apply"),
            fixture: name("fixture"),
            assertion: name("assertion"),
        }
    }
}

fn refused(error: impl std::fmt::Display) -> DbError {
    DbError::Refused(error.to_string())
}

fn name(ident: &str) -> Result<String, DbError> {
    quote(ident).map_err(refused)
}

/// Creates the logins and the database, and makes the apply login its owner.
/// `conn` is the administrator's connection to `master`. The apply login is
/// `dbo` of the database by ownership, not by role membership: the ordinary
/// deployment needs no server-level permission (measured on #1165).
pub async fn provision(
    conn: &mut impl ExecuteConnection,
    ids: &Identities,
    passwords: [&str; 3],
) -> Result<(), DbError> {
    for (login, password) in [&ids.apply, &ids.fixture, &ids.assertion]
        .into_iter()
        .zip(passwords)
    {
        // The generated password is the credential; a policy that expires or
        // locks it would end the run for a reason unrelated to the test.
        conn.execute(&format!(
            "CREATE LOGIN {} WITH PASSWORD = {}, CHECK_POLICY = OFF, \
             DEFAULT_DATABASE = [master]",
            name(login)?,
            literal(password)
        ))
        .await?;
    }
    let database = name(DATABASE)?;
    conn.execute(&format!("CREATE DATABASE {database}")).await?;
    conn.execute(&format!(
        "ALTER AUTHORIZATION ON DATABASE::{database} TO {}",
        name(&ids.apply)?
    ))
    .await
}

/// Maps the fixture and assertion logins into the owned database. `conn` is the
/// administrator's connection **to that database**: a connection to `master`
/// that ran `USE` would stay a session in it, and the assertion phase needs
/// none.
pub async fn create_users(
    conn: &mut impl ExecuteConnection,
    ids: &Identities,
) -> Result<(), DbError> {
    for login in [&ids.fixture, &ids.assertion] {
        let q = name(login)?;
        conn.execute(&format!("CREATE USER {q} FOR LOGIN {q}"))
            .await?;
    }
    Ok(())
}

/// Declared namespaces are provisioning prerequisites of ordinary bootstrap,
/// not managed changes, exactly as on PostgreSQL. Owned by `dbo` so that every
/// managed object shares one owner and ownership chaining holds between them.
pub async fn namespaces(
    conn: &mut impl ExecuteConnection,
    schemas: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(), DbError> {
    for schema in schemas {
        let schema = schema.as_ref();
        let create = format!("CREATE SCHEMA {} AUTHORIZATION [dbo]", name(schema)?);
        conn.execute(&format!(
            "IF SCHEMA_ID({}) IS NULL EXEC({})",
            literal(schema),
            literal(&create)
        ))
        .await?;
    }
    Ok(())
}

fn rows_of(rows: Vec<Row>, columns: &[&str]) -> Result<Vec<Vec<String>>, DbError> {
    rows.iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| {
                    row.try_get::<&str>(column)?
                        .map(str::to_owned)
                        .ok_or_else(|| DbError::BadRow(format!("NULL {column}")))
                })
                .collect()
        })
        .collect()
}

/// What `fn_my_permissions` reports for each securable, as the impersonated
/// principal. A column-level row repeats its permission, so the set is the fact.
async fn effective(
    conn: &mut impl ExecuteConnection,
    impersonate: &str,
    securables: &[(String, String, &str)],
) -> Result<BTreeMap<String, BTreeSet<String>>, DbError> {
    let mut parts = Vec::new();
    for (label, securable, class) in securables {
        let argument = if securable.is_empty() {
            "NULL".to_owned()
        } else {
            literal(securable)
        };
        parts.push(format!(
            "SELECT {} AS securable, permission_name AS permission \
             FROM sys.fn_my_permissions({argument}, N'{class}')",
            literal(label)
        ));
    }
    conn.execute(impersonate).await?;
    let found = conn.query(&parts.join(" UNION ALL ")).await;
    // The context ends before the answer is read, so an error cannot leave the
    // connection acting as the principal under test.
    let reverted = conn.execute("REVERT").await;
    let found = rows_of(found?, &["securable", "permission"])?;
    reverted?;
    let mut by_securable: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in found {
        by_securable
            .entry(row[0].clone())
            .or_default()
            .insert(row[1].clone());
    }
    Ok(by_securable)
}

fn exceeds(
    who: &str,
    found: &BTreeMap<String, BTreeSet<String>>,
    expected: &BTreeMap<String, BTreeSet<String>>,
) -> Result<(), DbError> {
    let none = BTreeSet::new();
    for label in found.keys().chain(expected.keys()).collect::<BTreeSet<_>>() {
        let (has, wants) = (
            found.get(label).unwrap_or(&none),
            expected.get(label).unwrap_or(&none),
        );
        if has != wants {
            let extra: Vec<_> = has.difference(wants).cloned().collect();
            let missing: Vec<_> = wants.difference(has).cloned().collect();
            return Err(refused(format!(
                "the effective SQL permissions of {who} on {label} differ from the fixture/assertion \
                 boundary (extra: [{}]; missing: [{}]); remove broad grants, including any public grant",
                extra.join(", "),
                missing.join(", ")
            )));
        }
    }
    Ok(())
}

/// The server-level boundary of the fixture and assertion logins: they may
/// connect and see database names, nothing else, and reach no user database
/// but the owned one. `conn` is the administrator's connection to `master`.
pub async fn verify_server(
    conn: &mut impl ExecuteConnection,
    ids: &Identities,
) -> Result<(), DbError> {
    let allowed: BTreeSet<String> = ["CONNECT SQL", "VIEW ANY DATABASE"]
        .map(str::to_owned)
        .into();
    for login in [&ids.fixture, &ids.assertion] {
        conn.execute(&format!("EXECUTE AS LOGIN = {}", literal(login)))
            .await?;
        let permissions = conn
            .query(
                "SELECT permission_name AS permission FROM sys.fn_my_permissions(NULL, N'SERVER')",
            )
            .await;
        let databases = conn
            .query(&format!(
                "SELECT name FROM sys.databases WHERE database_id > 4 AND name <> {} \
                 AND HAS_DBACCESS(name) = 1",
                literal(DATABASE)
            ))
            .await;
        let reverted = conn.execute("REVERT").await;
        let found: BTreeSet<String> = rows_of(permissions?, &["permission"])?
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
        let other = rows_of(databases?, &["name"])?;
        reverted?;
        if found != allowed {
            return Err(refused(format!(
                "the login {login} holds server permissions {found:?}; only {allowed:?} are allowed"
            )));
        }
        if let Some(database) = other.first() {
            return Err(refused(format!(
                "the login {login} can enter the user database {}; a scratch holds no other",
                database[0]
            )));
        }
    }
    Ok(())
}

/// Grants the current managed tables to the fixture and assertion users and
/// verifies what each can actually do. Re-run after every hop: a table dropped
/// and created by a hop starts with no grants. Revoked first, so a stale grant
/// is not trusted as current. `conn` is the administrator's connection to the
/// owned database.
///
/// The check reads effective permissions as each user, so a role, a `public`
/// grant or an inherited right is found as readily as a direct one (ADR-0019
/// §6: "verify effective grants, not just the names of roles").
pub async fn grants(
    conn: &mut impl ExecuteConnection,
    ids: &Identities,
    schema: &Schema,
) -> Result<(), DbError> {
    let (fixture, assertion) = (name(&ids.fixture)?, name(&ids.assertion)?);
    let mut sql = Vec::new();
    let mut tables = Vec::new();
    for (table, declared) in &schema.tables {
        let object = format!("{}.{}", name(&table.schema)?, name(&table.name)?);
        sql.push(format!(
            "REVOKE SELECT, INSERT, UPDATE, DELETE, REFERENCES ON OBJECT::{object} \
             FROM {fixture}, {assertion}; GRANT SELECT ON OBJECT::{object} TO {fixture}, {assertion}"
        ));
        // A `data:` table is bootstrapped from its declarations; a fixture
        // reads it and cannot change it.
        let reference = declared.data.is_some();
        if !reference {
            sql.push(format!(
                "GRANT INSERT, UPDATE, DELETE ON OBJECT::{object} TO {fixture}"
            ));
        }
        tables.push((object, reference));
    }
    if !sql.is_empty() {
        conn.execute(&sql.join("; ")).await?;
    }
    // Every schema the database has, not only the declared ones: a CREATE or
    // ALTER right on any of them lets a fixture make a permanent object.
    let schemas = rows_of(
        conn.query(
            "SELECT name FROM sys.schemas WHERE schema_id < 16384 \
             AND name NOT IN (N'sys', N'INFORMATION_SCHEMA')",
        )
        .await?,
        &["name"],
    )?;
    let database: BTreeSet<String> = [
        "CONNECT",
        "VIEW ANY COLUMN ENCRYPTION KEY DEFINITION",
        "VIEW ANY COLUMN MASTER KEY DEFINITION",
    ]
    .map(str::to_owned)
    .into();
    for (login, writes) in [(&ids.fixture, true), (&ids.assertion, false)] {
        let mut securables = vec![("database".to_owned(), String::new(), "DATABASE")];
        let mut expected = BTreeMap::from([("database".to_owned(), database.clone())]);
        for row in &schemas {
            securables.push((format!("schema {}", row[0]), row[0].clone(), "SCHEMA"));
        }
        for (object, reference) in &tables {
            securables.push((object.clone(), object.clone(), "OBJECT"));
            let mut allowed = vec!["SELECT"];
            if writes && !reference {
                allowed.extend(["INSERT", "UPDATE", "DELETE"]);
            }
            expected.insert(
                object.clone(),
                allowed.into_iter().map(str::to_owned).collect(),
            );
        }
        let found = effective(
            conn,
            &format!("EXECUTE AS USER = {}", literal(login)),
            &securables,
        )
        .await?;
        exceeds(login, &found, &expected)?;
    }
    Ok(())
}

/// The engine build as a report records it: the first line of `@@VERSION` and
/// the server collation. Two comparison databases of a re-pin must agree on
/// both, and the collation is the one setting the image chooses that changes
/// how SQL compares text (DEC-1165.14).
pub async fn engine_build(conn: &mut impl QueryConnection) -> Result<String, DbError> {
    let rows = rows_of(
        conn.query(
            "SELECT @@VERSION AS version, \
             CONVERT(nvarchar(128), SERVERPROPERTY('Collation')) AS collation",
        )
        .await?,
        &["version", "collation"],
    )?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::BadRow("the engine build is unreadable".into()))?;
    let line = row[0].lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        return Err(DbError::BadRow("the engine build is empty".into()));
    }
    Ok(format!("{line}; server collation {}", row[1]))
}

/// Why fixture SQL did not run to completion.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error(transparent)]
    Db(#[from] DbError),
    /// SQL that only a client tool understands. Refused before anything runs,
    /// so it cannot half-apply.
    #[error("{0}")]
    Unsupported(String),
    /// Closing the session would roll the writes back with no error: the
    /// fixture would pass and load nothing.
    #[error(
        "the fixture left {0} transaction(s) open; COMMIT or ROLLBACK inside it, because closing \
         its session would discard the writes silently"
    )]
    OpenTransaction(i64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lex {
    Normal,
    Quote(char),
    Bracket,
    Block(u32),
}

/// Advances the lexical state across one line. A `--` comment ends with the
/// line, so it never carries over.
fn advance(line: &str, state: &mut Lex) {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match *state {
            Lex::Normal => match c {
                '\'' | '"' => *state = Lex::Quote(c),
                '[' => *state = Lex::Bracket,
                '-' if chars.peek() == Some(&'-') => return,
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    *state = Lex::Block(1);
                }
                _ => {}
            },
            Lex::Quote(quote) if c == quote => {
                if chars.peek() == Some(&quote) {
                    chars.next();
                } else {
                    *state = Lex::Normal;
                }
            }
            Lex::Bracket if c == ']' => {
                if chars.peek() == Some(&']') {
                    chars.next();
                } else {
                    *state = Lex::Normal;
                }
            }
            Lex::Block(depth) if c == '*' && chars.peek() == Some(&'/') => {
                chars.next();
                *state = if depth == 1 {
                    Lex::Normal
                } else {
                    Lex::Block(depth - 1)
                };
            }
            Lex::Block(depth) if c == '/' && chars.peek() == Some(&'*') => {
                chars.next();
                *state = Lex::Block(depth + 1);
            }
            Lex::Quote(_) | Lex::Bracket | Lex::Block(_) => {}
        }
    }
}

/// Whether a line outside any literal or comment is a batch separator.
fn separator(line: &str) -> Result<bool, FixtureError> {
    let line = line.trim();
    let sqlcmd = line.starts_with(':') && line[1..].starts_with(|c: char| c.is_ascii_alphabetic());
    if sqlcmd || line.starts_with("!!") {
        return Err(FixtureError::Unsupported(format!(
            "`{line}` is a SQLCMD command; fixtures are sent to the server as T-SQL and no client \
             commands, includes or shell execution are supported"
        )));
    }
    if !line
        .get(..2)
        .is_some_and(|go| go.eq_ignore_ascii_case("go"))
    {
        return Ok(false);
    }
    let rest = &line[2..];
    if rest.is_empty() {
        return Ok(true);
    }
    if !rest.starts_with(char::is_whitespace) {
        return Ok(false);
    }
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with("--") {
        return Ok(true);
    }
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(FixtureError::Unsupported(format!(
            "`{line}` repeats a batch; `GO <count>` is not supported in a fixture"
        )));
    }
    Ok(false)
}

/// Splits fixture SQL into batches at a `GO` line that is outside every string,
/// quoted identifier and comment. This is framing, not parsing: nothing else
/// about the text is interpreted, and a `GO` the line test cannot see is sent to
/// the server, which refuses it.
pub fn batches(sql: &str) -> Result<Vec<String>, FixtureError> {
    let mut batches = Vec::new();
    let mut current = String::new();
    let mut state = Lex::Normal;
    for line in sql.split_inclusive('\n') {
        if state == Lex::Normal && separator(line)? {
            batches.push(std::mem::take(&mut current));
            continue;
        }
        advance(line, &mut state);
        current.push_str(line);
    }
    batches.push(current);
    batches.retain(|batch| !batch.trim().is_empty());
    Ok(batches)
}

/// Runs one fixture file on the fixture login's connection, batch by batch in
/// one session so `#temp` objects survive a `GO`. The text is split before
/// anything runs, and a transaction the file left open is an error: closing the
/// session would discard it without one.
pub async fn run_fixture(conn: &mut impl ExecuteConnection, sql: &str) -> Result<(), FixtureError> {
    let batches = batches(sql)?;
    for batch in &batches {
        conn.execute(batch).await?;
    }
    let rows = rows_of(
        conn.query("SELECT CONVERT(nvarchar(20), @@TRANCOUNT) AS trancount")
            .await?,
        &["trancount"],
    )?;
    let open: i64 = rows
        .first()
        .and_then(|row| row[0].parse().ok())
        .ok_or_else(|| DbError::BadRow("@@TRANCOUNT is unreadable".into()))?;
    if open != 0 {
        return Err(FixtureError::OpenTransaction(open));
    }
    Ok(())
}

/// Switches the owned database READ_ONLY, the scratch write boundary of an
/// assertion (ADR-0019 §7). `conn` is the administrator's connection to
/// `master`; the caller bounds the whole call with a deadline.
///
/// It refuses while any other user session is on the database and never evicts
/// one: `ROLLBACK IMMEDIATE` would hide a leaked client. The refusal comes
/// first because a plain `ALTER DATABASE … SET READ_ONLY` waits without limit
/// behind a held session, and neither `LOCK_TIMEOUT` nor `NO_WAIT` bounds it
/// (measured on #1165). A deadline that drops the connection leaves that
/// statement pending server-side, so a failure here ends the run.
pub async fn seal(conn: &mut impl ExecuteConnection) -> Result<(), DbError> {
    let database = name(DATABASE)?;
    let holders = rows_of(
        conn.query(&format!(
            "SELECT CONVERT(nvarchar(20), session_id) AS session, \
             ISNULL(login_name, N'') AS login FROM sys.dm_exec_sessions \
             WHERE is_user_process = 1 AND database_id = DB_ID({}) \
             AND session_id <> @@SPID ORDER BY session_id",
            literal(DATABASE)
        ))
        .await?,
        &["session", "login"],
    )?;
    if !holders.is_empty() {
        return Err(refused(format!(
            "the owned database still has {} open session(s) ({}); it is not \
             switched read-only while anything is connected",
            holders.len(),
            holders
                .iter()
                .map(|h| format!("{} as {}", h[0], h[1]))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    conn.execute(&format!("ALTER DATABASE {database} SET READ_ONLY"))
        .await?;
    let mode = rows_of(
        conn.query(&format!(
            "SELECT CONVERT(nvarchar(60), DATABASEPROPERTYEX({}, N'Updateability')) AS mode",
            literal(DATABASE)
        ))
        .await?,
        &["mode"],
    )?;
    match mode.first().map(|row| row[0].as_str()) {
        Some("READ_ONLY") => Ok(()),
        other => Err(refused(format!(
            "the owned database is not read-only after the switch (updateability {})",
            other.unwrap_or("unreadable")
        ))),
    }
}

/// Runs one assertion file as the assertion login, in the sealed database. The
/// connection is fresh and consumed, so no assertion shares a session. The mode
/// is read again here: a zero-row result is a pass only when nothing could have
/// written, so an unsealed database must not reach the user's SQL.
pub async fn run_assertion(
    connection: &str,
    sql: &str,
    timeout: std::time::Duration,
) -> Result<AssertionResult, DbError> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, async {
        let mut conn = Conn::connect(Driver::Mssql, connection).await?;
        let rows = rows_of(
            conn.query(
                "SELECT DB_NAME() AS name, \
                 CONVERT(nvarchar(60), DATABASEPROPERTYEX(DB_NAME(), N'Updateability')) AS mode",
            )
            .await?,
            &["name", "mode"],
        )?;
        if rows.first().map(|r| (r[0].as_str(), r[1].as_str())) != Some((DATABASE, "READ_ONLY")) {
            return Err(refused(
                "the assertion connection is not on the sealed owned database",
            ));
        }
        // The engine parses the batch (DEC-2401.2): result shape and the
        // deadline bound execution, and READ_ONLY is the write boundary.
        Ok(
            match conn
                .query_bounded(
                    sql,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await
            {
                Ok(evidence) => AssertionResult::from_evidence(evidence),
                Err(error) if error.server_error_code().is_some() => {
                    AssertionResult::SqlError(error)
                }
                Err(error) => AssertionResult::Unanswerable(error),
            },
        )
    })
    .await
    .map_err(|_| DbError::DeadlineExceeded("assertion deadline exceeded"))?
}

/// Whether a SQL Server error number is the engine or its host failing rather
/// than the user's SQL. These are numbers, not SQLSTATEs: a user `THROW` may
/// use any number from 50000 up, so a PostgreSQL-style prefix rule would read
/// one as a fault. Only the faults a test run can meet are listed: resources
/// exhausted (701, 802, 1101, 1105, 1204, 8645, 9001, 9002), storage failing
/// (823, 824, 825) and a session the server ended (596, 6005). Anything else
/// the user's SQL can cause, a permission (229) or a write to the read-only
/// database (3906) among them, is a finding.
pub fn infrastructure_fault(code: &str) -> bool {
    matches!(
        code,
        "596"
            | "701"
            | "802"
            | "823"
            | "824"
            | "825"
            | "1101"
            | "1105"
            | "1204"
            | "6005"
            | "8645"
            | "9001"
            | "9002"
    )
}

#[cfg(test)]
mod tests;
