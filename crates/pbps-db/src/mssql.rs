//! The SQL Server driver, and the only file in this crate that names
//! `tiberius`.
//!
//! ADR-0007 decision 5 built the seam so that `tiberius` was named in exactly
//! one file. With a second driver the rule becomes **one file per driver**:
//! nothing outside this module and its PostgreSQL sibling names either one, and
//! `lib.rs` dispatches between them without knowing what is underneath
//! (DECISIONS 225).

use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::{DbError, Param};

/// One row as this driver hands it back.
pub struct Row(tiberius::Row);

/// An open SQL Server connection.
pub struct Conn {
    client: Client<Compat<TcpStream>>,
}

impl From<tiberius::error::Error> for DbError {
    /// The driver's error, flattened to text and a code.
    ///
    /// Flattened here rather than carried, because `DbError` is the seam's
    /// error and a variant holding one driver's type would put that driver's
    /// name back in every signature the seam exists to keep it out of. The
    /// code is text for the reason `server_error_code` states.
    fn from(e: tiberius::error::Error) -> Self {
        DbError::Driver {
            code: e.code().map(|c| c.to_string()),
            message: e.to_string(),
        }
    }
}

impl Conn {
    /// Connects using an ADO.NET-style connection string
    /// (`Server=host,1433;Database=x;User Id=u;Password=p;` — the form every
    /// SQL Server tool already asks for, so users can paste what they have).
    pub(crate) async fn connect(connection_string: &str) -> Result<Self, DbError> {
        let config = Config::from_ado_string(connection_string)
            .map_err(|e| DbError::BadConnectionString(e.to_string()))?;
        let addr = config.get_addr().to_owned();
        // A firewall that drops rather than refuses leaves the OS retrying for
        // over two minutes. Waiting that long for a pipeline to say "I could
        // not reach prod" is a bad way to learn it, so the wait is bounded and
        // the message says which of the two happened.
        // Opened through [`crate::open_socket`], shared with the other driver:
        // a timeout around `TcpStream::connect(host)` bounds the whole
        // resolve-and-try-each-address loop, so one black-holed address of a
        // dual-stack name spent the entire budget and the healthy one was never
        // tried. The same shape was on both sides of the seam.
        let tcp = crate::open_socket(&addr).await?;
        tcp.set_nodelay(true).map_err(|source| DbError::Connect {
            addr: config.get_addr().to_owned(),
            source,
        })?;
        let client = Client::connect(config, tcp.compat_write()).await?;
        Ok(Self { client })
    }

    pub(crate) async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
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
    pub(crate) async fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        let stream = self.client.simple_query(sql).await?;
        stream.into_results().await?;
        Ok(())
    }

    pub(crate) async fn query_with(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
    ) -> Result<Vec<Row>, DbError> {
        let bound: Vec<&dyn tiberius::ToSql> = params.iter().map(as_sql).collect();
        let stream = self.client.query(sql, &bound).await?;
        Ok(stream
            .into_first_result()
            .await?
            .into_iter()
            .map(Row)
            .collect())
    }

    pub(crate) async fn execute_with(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
    ) -> Result<u64, DbError> {
        let bound: Vec<&dyn tiberius::ToSql> = params.iter().map(as_sql).collect();
        let result = self.client.execute(sql, &bound).await?;
        Ok(result.rows_affected().iter().sum())
    }
}

/// Borrows a bound value as something this driver can bind.
///
/// The reference points into the enum itself, so it lives exactly as long as
/// the caller's parameter slice.
fn as_sql<'a>(param: &'a Param<'a>) -> &'a dyn tiberius::ToSql {
    match param {
        Param::I32(v) => v,
        Param::I64(v) => v,
        Param::Str(v) => v,
        Param::OptStr(v) => v,
    }
}

/// The types the seam reads, each by name and by position.
///
/// Written out rather than blanket-implemented over `tiberius::FromSql`: two
/// blanket implementations, one per driver, would overlap and coherence refuses
/// them. The set was measured by the compiler rather than by reading the source
/// — a first count by eye found three and missed `bool`, `i16` and `u8`, which
/// reach the seam through `get(&row, "…")` with the type inferred from the
/// struct field it lands in and never spelled at the call site (DECISIONS 225).
impl Row {
    pub(crate) fn str_by_name<'a>(&'a self, col: &str) -> Result<Option<&'a str>, DbError> {
        Ok(self.0.try_get::<&str, _>(col)?)
    }

    pub(crate) fn str_at(&self, idx: usize) -> Result<Option<&str>, DbError> {
        Ok(self.0.try_get::<&str, _>(idx)?)
    }

    pub(crate) fn i32_by_name(&self, col: &str) -> Result<Option<i32>, DbError> {
        Ok(self.0.try_get::<i32, _>(col)?)
    }

    pub(crate) fn i32_at(&self, idx: usize) -> Result<Option<i32>, DbError> {
        Ok(self.0.try_get::<i32, _>(idx)?)
    }

    pub(crate) fn i64_by_name(&self, col: &str) -> Result<Option<i64>, DbError> {
        Ok(self.0.try_get::<i64, _>(col)?)
    }

    pub(crate) fn i64_at(&self, idx: usize) -> Result<Option<i64>, DbError> {
        Ok(self.0.try_get::<i64, _>(idx)?)
    }

    pub(crate) fn i16_by_name(&self, col: &str) -> Result<Option<i16>, DbError> {
        Ok(self.0.try_get::<i16, _>(col)?)
    }

    pub(crate) fn i16_at(&self, idx: usize) -> Result<Option<i16>, DbError> {
        Ok(self.0.try_get::<i16, _>(idx)?)
    }

    pub(crate) fn u8_by_name(&self, col: &str) -> Result<Option<u8>, DbError> {
        Ok(self.0.try_get::<u8, _>(col)?)
    }

    pub(crate) fn u8_at(&self, idx: usize) -> Result<Option<u8>, DbError> {
        Ok(self.0.try_get::<u8, _>(idx)?)
    }

    pub(crate) fn bool_by_name(&self, col: &str) -> Result<Option<bool>, DbError> {
        Ok(self.0.try_get::<bool, _>(col)?)
    }

    pub(crate) fn bool_at(&self, idx: usize) -> Result<Option<bool>, DbError> {
        Ok(self.0.try_get::<bool, _>(idx)?)
    }
}
