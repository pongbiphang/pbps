//! One assertion per fresh least-privilege connection (ADR-0019 §7).
//! The caller provisions SELECT-only grants; this runner additionally refuses
//! elevated logins. Target assertions can use it without a scratch dependency.

use pbps_db::{Conn, DbError, Driver, query::QueryEvidence};
use std::time::Duration;

/// Internal execution facts for the future CLI report, not a wire format.
#[derive(Debug)]
pub enum AssertionResult {
    Pass(QueryEvidence),
    Violation(QueryEvidence),
    InvalidResult { message: &'static str },
    SqlError(DbError),
    BoundaryViolation,
    Unanswerable(DbError),
}

#[derive(Debug, PartialEq, Eq)]
struct TransactionIdentity {
    pid: i32,
    started: String,
    virtual_id: String,
}

// Virtual transaction IDs distinguish successive read-only transactions
// without assigning an xid. Qualified on PG 16.15 and 18.6 (DEC-1165.2).
async fn identity(conn: &mut Conn) -> Result<Option<TransactionIdentity>, DbError> {
    let rows = conn
        .query(
            "SELECT pg_catalog.pg_backend_pid() AS pid, \
        EXTRACT(EPOCH FROM pg_catalog.transaction_timestamp())::text AS started, \
        pg_catalog.current_setting('transaction_read_only') AS read_only, \
        pg_catalog.pg_current_xact_id_if_assigned()::text AS xid, \
        (SELECT virtualtransaction FROM pg_catalog.pg_locks \
         WHERE pid = pg_catalog.pg_backend_pid() AND locktype = 'virtualxid' \
         AND granted AND mode = 'ExclusiveLock') AS virtual_id",
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::BadRow("missing assertion transaction identity".into()))?;
    if row.try_get::<&str>("read_only")? != Some("on") || row.try_get::<&str>("xid")?.is_some() {
        return Ok(None);
    }
    let (Some(pid), Some(started), Some(virtual_id)) = (
        row.try_get::<i32>("pid")?,
        row.try_get::<&str>("started")?,
        row.try_get::<&str>("virtual_id")?,
    ) else {
        return Ok(None);
    };
    Ok(Some(TransactionIdentity {
        pid,
        started: started.into(),
        virtual_id: virtual_id.into(),
    }))
}

/// Executes original SQL using the extended protocol. A single deadline covers
/// connect/setup/read/postcheck; every path drops the owned connection. No
/// connection, including a successful one, is reused for another file.
pub async fn run(connection_string: &str, sql: &str, timeout: Duration) -> AssertionResult {
    let deadline = tokio::time::Instant::now() + timeout;
    match tokio::time::timeout_at(deadline, run_inner(connection_string, sql, deadline)).await {
        Ok(result) => result,
        Err(_) => {
            AssertionResult::Unanswerable(DbError::Refused("assertion deadline exceeded".into()))
        }
    }
}

async fn run_inner(
    connection_string: &str,
    sql: &str,
    deadline: tokio::time::Instant,
) -> AssertionResult {
    let mut conn = match Conn::connect(Driver::Postgres, connection_string).await {
        Ok(conn) => conn,
        Err(error) => return AssertionResult::Unanswerable(error),
    };
    match prepare(&mut conn, deadline).await {
        Ok(()) => {}
        Err(error) => return AssertionResult::Unanswerable(error),
    }
    // This first read is deliberately before user SQL: otherwise SET
    // TRANSACTION READ WRITE can change the transaction before its first read.
    let before = match identity(&mut conn).await {
        Ok(Some(identity)) => identity,
        Ok(None) => return AssertionResult::BoundaryViolation,
        Err(error) => return AssertionResult::Unanswerable(error),
    };
    let evidence = match conn
        .query_bounded(
            sql,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
    {
        Ok(evidence) => evidence,
        Err(error) if is_sql_error(&error) => {
            return AssertionResult::SqlError(error);
        }
        Err(error) => return AssertionResult::Unanswerable(error),
    };
    // A bounded stream was closed by the driver. Only completed streams can
    // be postchecked; an empty result can pass only on that completed path.
    if !evidence.truncated {
        match identity(&mut conn).await {
            Ok(Some(after)) if after == before => {}
            Ok(_) => return AssertionResult::BoundaryViolation,
            Err(error) => return AssertionResult::Unanswerable(error),
        }
        if let Err(error) = conn.execute("ROLLBACK").await {
            return AssertionResult::Unanswerable(error);
        }
    }
    // DEC-1165.1: zero columns cannot establish a tabular assertion result.
    if evidence.columns.is_empty() {
        AssertionResult::InvalidResult {
            message: "an assertion must return at least one output column; use SELECT 1 ...",
        }
    } else if evidence.rows.is_empty() && !evidence.truncated {
        AssertionResult::Pass(evidence)
    } else {
        AssertionResult::Violation(evidence)
    }
}

async fn prepare(conn: &mut Conn, deadline: tokio::time::Instant) -> Result<(), DbError> {
    let rows = conn
        .query(
            "SELECT current_user = session_user AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles \
        WHERE (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls) \
        AND pg_catalog.pg_has_role(session_user, oid, 'MEMBER')) AS safe",
        )
        .await?;
    if rows
        .first()
        .map(|r| r.try_get::<bool>("safe"))
        .transpose()?
        .flatten()
        != Some(true)
    {
        return Err(DbError::Refused(
            "assertions require a separate non-administrator login".into(),
        ));
    }
    conn.execute("BEGIN READ ONLY").await?;
    // Closing the client socket alone does not interrupt a running backend.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let timeout_ms = remaining
        .as_nanos()
        .div_ceil(1_000_000)
        .clamp(1, i32::MAX as u128);
    conn.execute(&format!("SET LOCAL statement_timeout = '{timeout_ms}ms'"))
        .await
}

fn is_sql_error(error: &DbError) -> bool {
    error.server_error_code().is_some_and(|code| {
        !["08", "53", "58", "XX"]
            .iter()
            .any(|class| code.starts_with(class))
            && !["57014", "57P01", "57P02", "57P03"].contains(&code.as_str())
    })
}
