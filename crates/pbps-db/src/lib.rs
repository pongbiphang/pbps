//! Database connections.
//!
//! This crate owns "there is a network" and nothing else: opening a connection
//! from a connection string and running queries over it. What to ask the
//! database and what the answers mean is dialect knowledge and stays in
//! `pbps-mssql`; `__pbps_state` access and locking arrive here in Phase 3.
//!
//! The driver is `tiberius` — pure Rust, so a single static binary needs no
//! ODBC driver installed on the host (SPEC §11.3).

use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub use tiberius::Row;

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

    #[error("{0}")]
    Driver(#[from] tiberius::error::Error),

    /// A catalog row did not have the shape the query promised — a bug in the
    /// introspection SQL, not bad data in the database.
    #[error("unexpected row shape: {0}")]
    BadRow(String),
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
        let tcp = TcpStream::connect(&addr)
            .await
            .map_err(|source| DbError::Connect { addr, source })?;
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
    /// user-controlled is interpolated into them, which is why this deliberately
    /// takes no parameters — a parameter here would mean someone is building SQL
    /// where they should not be.
    pub async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        let stream = self.client.simple_query(sql).await?;
        Ok(stream.into_first_result().await?)
    }
}
