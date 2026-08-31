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
//! The driver is `tiberius` — pure Rust, so a single static binary needs no
//! ODBC driver installed on the host (SPEC §11.3).

use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub mod ledger;

pub use ledger::{LedgerEntry, LedgerError, LockInfo};
pub use tiberius::{Row, ToSql};

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

/// How long to wait for the TCP connection before giving up.
///
/// Long enough for a slow VPN or a container still starting, short enough that
/// a pipeline blocked by a firewall reports it while someone is still watching.
/// It bounds only the connect; a query against a reachable server is not
/// hurried, because a long-running ALTER is exactly what this tool exists to
/// run.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
        Ok(stream.into_first_result().await?)
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
        params: &[&dyn ToSql],
    ) -> Result<Vec<Row>, DbError> {
        let stream = self.client.query(sql, params).await?;
        Ok(stream.into_first_result().await?)
    }

    /// Runs one parameterized statement and returns the number of rows affected.
    ///
    /// The ledger is the only thing that writes *data*, and everything it writes
    /// is user-supplied: a reason, an operator's name, a whole JSON snapshot.
    /// Binding is not politeness here — a `'` in an operator's name would end
    /// the statement.
    pub async fn execute_with(&mut self, sql: &str, params: &[&dyn ToSql]) -> Result<u64, DbError> {
        let result = self.client.execute(sql, params).await?;
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
