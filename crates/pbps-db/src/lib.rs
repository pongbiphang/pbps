//! Database connections.
//!
//! This crate owns "there is a network": opening a connection from a connection
//! string, running statements over it, and framing a transaction. What to ask
//! the database and what the answers mean is dialect knowledge and stays in
//! `pbps-mssql`.
//!
//! [`ledger`] holds the shapes of the `__pbps_state` ledger and the
//! `__pbps_lock` lock (SPEC §8.1). Only the shapes: the SQL that reads and
//! writes them is T-SQL, so it lives in `pbps_mssql::state` beside the catalog
//! queries, and Phase 4's `pbps-pg` will write its own. Keeping the types here
//! is what stops the ledger's meaning from being defined twice.
//!
//! The driver API is `tiberius`, supplied for now by the `tiberius-ng` package
//! — pure Rust, so a single static binary needs no driver installed on the
//! host (SPEC §11.3). The original crate is community-owned again and the plan
//! is to return to it once it ships a release with a current `rustls`; the
//! condition and the date it was last checked are on the dependency line in
//! the workspace `Cargo.toml` and in SPEC open question 10.
//!
//! **`tiberius` is named nowhere but this file.** That is deliberate: the driver
//! is named nowhere else, and this module remains the seam for replacing it.
//! [`Row`], [`FromColumn`] and [`Param`]
//! exist for that reason alone — re-exporting the driver's own types would be
//! shorter, and would make a replacement an API change for every caller instead
//! of an edit to one file.

use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub mod ledger;

pub use ledger::{LedgerEntry, LedgerError, LockInfo};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("the connection string is malformed: {0}")]
    BadConnectionString(String),

    #[error("cannot reach `{addr}`: {source}")]
    Connect {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "`{addr}` did not answer within {}s.\n\
         The host is unreachable or a firewall is dropping the connection rather than refusing it.",
        CONNECT_TIMEOUT.as_secs()
    )]
    ConnectTimeout { addr: String },

    #[error("{0}")]
    Driver(#[from] tiberius::error::Error),

    /// A catalog row did not have the shape the query promised — a bug in the
    /// introspection SQL, not bad data in the database.
    #[error("unexpected row shape: {0}")]
    BadRow(String),
}

impl DbError {
    /// The server's own error number, when the failure came from the server
    /// rather than from the connection.
    ///
    /// Exposed as a bare number and not interpreted here: what 208 or 229
    /// *mean* is SQL Server's vocabulary, and this crate deliberately holds
    /// none of it — a dialect's error codes belong in that dialect's crate.
    /// What this crate owns is that the number is reachable at all without a
    /// second crate naming `tiberius`.
    pub fn server_error_number(&self) -> Option<u32> {
        // Enumerated rather than wildcarded, as `wildcard_enum_match_arm`
        // requires: a variant added later must be looked at here, because the
        // answer "no number" is the one a caller reads as "not the case I am
        // asking about" and would silently apply to it.
        match self {
            DbError::Driver(e) => e.code(),
            DbError::BadConnectionString(_)
            | DbError::Connect { .. }
            | DbError::ConnectTimeout { .. }
            | DbError::BadRow(_) => None,
        }
    }
}

/// How long to wait for the TCP connection before giving up.
///
/// Long enough for a slow VPN or a container still starting, short enough that
/// a pipeline blocked by a firewall reports it while someone is still watching.
/// It bounds only the connect; a query against a reachable server is not
/// hurried, because a long-running ALTER is exactly what this tool exists to
/// run.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One row of a result set.
pub struct Row(tiberius::Row);

/// A type that can be read out of a result column.
///
/// The blanket implementation covers whatever the driver can already convert,
/// so this adds no per-type work; what it adds is a signature callers can name
/// without naming the driver.
pub trait FromColumn<'a>: Sized {
    fn from_column(row: &'a Row, col: &str) -> Result<Option<Self>, DbError>;
    fn from_index(row: &'a Row, idx: usize) -> Result<Option<Self>, DbError>;
}

impl<'a, T: tiberius::FromSql<'a>> FromColumn<'a> for T {
    fn from_column(row: &'a Row, col: &str) -> Result<Option<Self>, DbError> {
        Ok(row.0.try_get::<T, _>(col)?)
    }

    fn from_index(row: &'a Row, idx: usize) -> Result<Option<Self>, DbError> {
        Ok(row.0.try_get::<T, _>(idx)?)
    }
}

impl Row {
    /// Reads one column by name; `None` when it is NULL.
    pub fn try_get<'a, T: FromColumn<'a>>(&'a self, col: &str) -> Result<Option<T>, DbError> {
        T::from_column(self, col)
    }

    /// Reads one column by position; `None` when it is NULL.
    ///
    /// Position is the wrong way to read a catalog query and the right way to
    /// read a preflight probe: a probe's SQL is generated from the plan, so its
    /// single count column has no name anyone can rely on.
    pub fn try_get_at<'a, T: FromColumn<'a>>(&'a self, idx: usize) -> Result<Option<T>, DbError> {
        T::from_index(self, idx)
    }
}

/// A value bound into a parameterized statement.
///
/// A closed set rather than a trait: these are the only shapes the ledger and
/// the impact queries bind, and an enum is what lets the driver stay unnamed
/// outside this file. Adding a variant is deliberate work, which is the point —
/// a parameter that is not one of these usually means SQL is being built where
/// it should not be.
#[derive(Clone, Copy, Debug)]
pub enum Param<'a> {
    I32(i32),
    I64(i64),
    Str(&'a str),
    /// A string that may be NULL — a ledger entry's git sha, checksum or reason.
    OptStr(Option<&'a str>),
}

impl From<i32> for Param<'_> {
    fn from(v: i32) -> Self {
        Param::I32(v)
    }
}

impl From<i64> for Param<'_> {
    fn from(v: i64) -> Self {
        Param::I64(v)
    }
}

impl<'a> From<&'a str> for Param<'a> {
    fn from(v: &'a str) -> Self {
        Param::Str(v)
    }
}

impl<'a> From<Option<&'a str>> for Param<'a> {
    fn from(v: Option<&'a str>) -> Self {
        Param::OptStr(v)
    }
}

impl Param<'_> {
    /// Borrows the payload as something the driver can bind.
    ///
    /// The reference points into the enum itself, so it lives exactly as long
    /// as the caller's parameter slice.
    fn as_sql(&self) -> &dyn tiberius::ToSql {
        match self {
            Param::I32(v) => v,
            Param::I64(v) => v,
            Param::Str(v) => v,
            Param::OptStr(v) => v,
        }
    }
}

/// An open SQL Server connection.
pub struct Conn {
    client: Client<Compat<TcpStream>>,
}

impl Conn {
    /// Connects using an ADO.NET-style connection string
    /// (`Server=host,1433;Database=x;User Id=u;Password=p;` — the form every
    /// SQL Server tool already asks for, so users can paste what they have).
    pub async fn connect(connection_string: &str) -> Result<Self, DbError> {
        let config = Config::from_ado_string(connection_string)
            .map_err(|e| DbError::BadConnectionString(e.to_string()))?;
        let addr = config.get_addr().to_owned();
        // A firewall that drops rather than refuses leaves the OS retrying for
        // over two minutes. Waiting that long for a pipeline to say "I could
        // not reach prod" is a bad way to learn it, so the wait is bounded and
        // the message says which of the two happened.
        let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(source)) => return Err(DbError::Connect { addr, source }),
            Err(_elapsed) => return Err(DbError::ConnectTimeout { addr }),
        };
        tcp.set_nodelay(true).map_err(|source| DbError::Connect {
            addr: config.get_addr().to_owned(),
            source,
        })?;
        let client = Client::connect(config, tcp.compat_write()).await?;
        Ok(Self { client })
    }

    /// Runs one query with no parameters and returns every row.
    ///
    /// Introspection queries are static SQL against the catalog views; nothing
    /// user-controlled is interpolated into them, which is why this takes no
    /// parameters — a parameter here would mean someone is building SQL where
    /// they should not be. Where a *value* genuinely has to reach the server —
    /// a state snapshot, an operator's name — that is [`Conn::query_with`] and
    /// [`Conn::execute_with`], which bind it rather than paste it.
    pub async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        let stream = self.client.simple_query(sql).await?;
        Ok(stream
            .into_first_result()
            .await?
            .into_iter()
            .map(Row)
            .collect())
    }

    /// Runs one batch of statements and discards any results.
    ///
    /// This is `simple_query`, not `execute`: DDL is sent as a batch exactly the
    /// way the emitter framed it, with no parameter machinery in the way.
    pub async fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        let stream = self.client.simple_query(sql).await?;
        stream.into_results().await?;
        Ok(())
    }

    /// Runs one parameterized query (`@P1`, `@P2`, ...) and returns every row.
    pub async fn query_with(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
    ) -> Result<Vec<Row>, DbError> {
        let bound: Vec<&dyn tiberius::ToSql> = params.iter().map(Param::as_sql).collect();
        let stream = self.client.query(sql, &bound).await?;
        Ok(stream
            .into_first_result()
            .await?
            .into_iter()
            .map(Row)
            .collect())
    }

    /// Runs one parameterized statement and returns the number of rows affected.
    ///
    /// The ledger is the only thing that writes *data*, and everything it writes
    /// is user-supplied: a reason, an operator's name, a whole JSON snapshot.
    /// Binding is not politeness here — a `'` in an operator's name would end
    /// the statement.
    pub async fn execute_with(&mut self, sql: &str, params: &[Param<'_>]) -> Result<u64, DbError> {
        let bound: Vec<&dyn tiberius::ToSql> = params.iter().map(Param::as_sql).collect();
        let result = self.client.execute(sql, &bound).await?;
        Ok(result.rows_affected().iter().sum())
    }

    /// Opens a transaction for "one plan, one transaction, all or nothing"
    /// (SPEC §7.5).
    ///
    /// `XACT_ABORT ON` is what makes that promise true rather than merely
    /// intended: without it, SQL Server keeps a transaction running after many
    /// statement-level errors, so a failed statement halfway through a plan
    /// would leave the earlier ones committable. With it, any such error dooms
    /// the transaction and the rollback is total.
    pub async fn begin(&mut self) -> Result<(), DbError> {
        self.execute("SET XACT_ABORT ON; BEGIN TRANSACTION;").await
    }

    pub async fn commit(&mut self) -> Result<(), DbError> {
        self.execute("COMMIT TRANSACTION;").await
    }

    /// Rolls back, tolerating a transaction the server has already killed.
    ///
    /// After `XACT_ABORT` doomed the transaction, `ROLLBACK` may find nothing to
    /// roll back and error with "no corresponding BEGIN TRANSACTION". Reporting
    /// that error would replace the real failure — the statement that broke —
    /// with a confusing second one, so the guard checks `@@TRANCOUNT` instead.
    pub async fn rollback(&mut self) -> Result<(), DbError> {
        self.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversions are what call sites rely on to stay readable
    /// (`snapshot.reason.as_deref().into()`), so each shape has to land in its
    /// own variant rather than the nearest one that compiles.
    #[test]
    fn every_bound_shape_converts_to_its_own_variant() {
        assert!(matches!(Param::from(7_i32), Param::I32(7)));
        assert!(matches!(Param::from(7_i64), Param::I64(7)));
        assert!(matches!(Param::from("dbo"), Param::Str("dbo")));
        assert!(matches!(
            Param::from(Some("abc")),
            Param::OptStr(Some("abc"))
        ));
    }

    /// A ledger entry with no git sha must reach the server as NULL, not as an
    /// empty string: `status` and the drift report both distinguish "this state
    /// was recorded outside a checkout" from "recorded at commit ''". Collapsing
    /// the two here would be invisible until someone read the ledger.
    #[test]
    fn an_absent_string_is_not_an_empty_one() {
        let absent = Param::from(None::<&str>);
        let empty = Param::from("");
        assert!(matches!(absent, Param::OptStr(None)));
        assert!(matches!(empty, Param::Str("")));
    }
}
