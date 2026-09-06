//! Database connections.
//!
//! This crate owns "there is a network": opening a connection from a connection
//! string, running statements over it, and framing a transaction. What to ask
//! the database and what the answers mean is dialect knowledge and stays in
//! `pbps-mssql` and `pbps-pg`.
//!
//! [`ledger`] holds the shapes of the `__pbps_state` ledger and the
//! `__pbps_lock` lock (SPEC §8.1). Only the shapes: the SQL that reads and
//! writes them is each engine's, so it lives beside that dialect's catalog
//! queries. Keeping the types here is what stops the ledger's meaning from
//! being defined twice.
//!
//! # Two drivers, named in two files
//!
//! ADR-0007 decision 5 built the seam so that `tiberius` was named in exactly
//! one file, making ADBC, ODBC or a different package a change to one crate.
//! ADR-0014 then measured the seam against a real second driver and found the
//! shape held — `Row`, `FromColumn`, `Param`, the four query and execute
//! methods, the transaction framing — while correcting the claim it was built
//! on: the spike *replaced* one driver, and Phase 5 has to *keep both*.
//!
//! So the rule becomes **one file per driver**. [`mssql`] names `tiberius`,
//! [`postgres`] names `tokio_postgres`, and nothing else in the workspace names
//! either. This module dispatches between them and holds no driver type: that
//! is what keeps `&mut Conn` meaning the same thing at all 22 call sites in
//! `pbps-mssql` that already had it (DECISIONS 214).

use pbps_dialect::TransactionFraming;
pub mod ledger;
mod mssql;
mod postgres;

pub use ledger::{LedgerEntry, LedgerError, LockInfo, TimelineEntry};

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

    /// The driver reported a failure — the server refused the statement, or
    /// the protocol broke.
    ///
    /// Flat text and an optional code rather than the driver's own error type.
    /// A variant holding one driver's type would put that driver's name into
    /// every signature that mentions `DbError`, which is exactly what the seam
    /// exists to prevent, and with two drivers it could only hold one of them.
    #[error("{message}")]
    Driver {
        message: String,
        code: Option<String>,
    },

    /// A catalog row did not have the shape the query promised — a bug in the
    /// introspection SQL, not bad data in the database.
    #[error("unexpected row shape: {0}")]
    BadRow(String),
}

impl DbError {
    /// The server's own error code, when the failure came from the server
    /// rather than from the connection.
    ///
    /// Exposed as text and not interpreted here: what `208` or `42P01` *mean*
    /// is one engine's vocabulary, and this crate deliberately holds none of
    /// either —
    /// a dialect's error codes belong in that dialect's crate. What this crate
    /// owns is that the code is reachable at all without a second crate naming
    /// `tiberius`.
    ///
    /// Text rather than a number, because a number is one engine's shape too:
    /// PostgreSQL's SQLSTATE is five characters that may be letters (`42P01`),
    /// and an `Option<u32>` here was a T-SQL type under a neutral name
    /// (ADR-0014 §1, DECISIONS 193). Owned, because one driver hands back a
    /// number and there is no string in the error for a `&str` to borrow.
    pub fn server_error_code(&self) -> Option<String> {
        // Enumerated rather than wildcarded, as `wildcard_enum_match_arm`
        // requires: a variant added later must be looked at here, because the
        // answer "no code" is the one a caller reads as "not the case I am
        // asking about" and would silently apply to it.
        match self {
            DbError::Driver { code, .. } => code.clone(),
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

/// One row of a result set, from whichever driver produced it.
pub enum Row {
    Mssql(mssql::Row),
    Postgres(postgres::Row),
}

/// A type that can be read out of a result column.
///
/// A **closed set**, like [`Param`], and for the same reason turned the other
/// way round. It used to be a blanket implementation over whatever the driver
/// could convert, which cost no per-type work — but two blanket implementations,
/// one per driver, overlap, and coherence refuses them.
///
/// Written out, the set is what the workspace actually reads: `&str`, `i32`,
/// `i64`, `i16`, `u8` and `bool`. That list came from the compiler and not from
/// reading the source, which matters — counted by eye it looked like three,
/// because `bool`, `i16` and `u8` reach the seam through
/// `get(&row, "max_length")` with the type inferred from the struct field it
/// lands in and never spelled at the call site. What this buys is a read surface
/// as deliberate as the bind surface: a column of some other type now needs a
/// line here, which is the point (DECISIONS 214).
///
/// The set is not engine-neutral, and pretending otherwise would be the bug it
/// exists to prevent. `u8` is SQL Server's `tinyint`, read for a column's
/// precision and scale; PostgreSQL has no unsigned type, so its arm refuses
/// rather than converting — see `postgres::Row::u8_by_name`.
pub trait FromColumn<'a>: Sized {
    fn from_column(row: &'a Row, col: &str) -> Result<Option<Self>, DbError>;
    fn from_index(row: &'a Row, idx: usize) -> Result<Option<Self>, DbError>;
}

impl<'a> FromColumn<'a> for &'a str {
    fn from_column(row: &'a Row, col: &str) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.str_by_name(col),
            Row::Postgres(r) => r.str_by_name(col),
        }
    }

    fn from_index(row: &'a Row, idx: usize) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.str_at(idx),
            Row::Postgres(r) => r.str_at(idx),
        }
    }
}

impl FromColumn<'_> for i32 {
    fn from_column(row: &Row, col: &str) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.i32_by_name(col),
            Row::Postgres(r) => r.i32_by_name(col),
        }
    }

    fn from_index(row: &Row, idx: usize) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.i32_at(idx),
            Row::Postgres(r) => r.i32_at(idx),
        }
    }
}

impl FromColumn<'_> for i16 {
    fn from_column(row: &Row, col: &str) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.i16_by_name(col),
            Row::Postgres(r) => r.i16_by_name(col),
        }
    }

    fn from_index(row: &Row, idx: usize) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.i16_at(idx),
            Row::Postgres(r) => r.i16_at(idx),
        }
    }
}

impl FromColumn<'_> for u8 {
    fn from_column(row: &Row, col: &str) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.u8_by_name(col),
            Row::Postgres(r) => r.u8_by_name(col),
        }
    }

    fn from_index(row: &Row, idx: usize) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.u8_at(idx),
            Row::Postgres(r) => r.u8_at(idx),
        }
    }
}

impl FromColumn<'_> for bool {
    fn from_column(row: &Row, col: &str) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.bool_by_name(col),
            Row::Postgres(r) => r.bool_by_name(col),
        }
    }

    fn from_index(row: &Row, idx: usize) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.bool_at(idx),
            Row::Postgres(r) => r.bool_at(idx),
        }
    }
}

impl FromColumn<'_> for i64 {
    fn from_column(row: &Row, col: &str) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.i64_by_name(col),
            Row::Postgres(r) => r.i64_by_name(col),
        }
    }

    fn from_index(row: &Row, idx: usize) -> Result<Option<Self>, DbError> {
        match row {
            Row::Mssql(r) => r.i64_at(idx),
            Row::Postgres(r) => r.i64_at(idx),
        }
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

/// Which driver a connection speaks.
///
/// Not `DialectName`: that is `pbps.yml`'s word for which SQL a project is
/// written in, and this crate holds no SQL and does not read the project's
/// configuration. The two correspond one to one today and are still different
/// questions, and keeping them apart is what stops `pbps-db` from depending on
/// `pbps-config`. The CLI maps one to the other in a match the compiler makes
/// exhaustive, so a third engine cannot be added without answering this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    Mssql,
    Postgres,
}

/// An open connection.
///
/// An enum rather than a trait object or a type parameter, decided with both
/// implementations in hand as ADR-0007 decision 3 asks. What each of the three
/// would have cost is in DECISIONS 214; the short of it is that the enum is the
/// only one of them that leaves `&mut Conn` meaning the same thing at the 22
/// call sites that already had it, and that it is the shape [`Param`] chose in
/// this same file, for this same reason, before there was a second driver.
pub enum Conn {
    // Boxed because the two clients are 1,416 and 168 bytes: unboxed, every
    // `Conn` — and every `Result` carrying one — would be the larger of them
    // whichever driver is in use.
    Mssql(Box<mssql::Conn>),
    Postgres(Box<postgres::Conn>),
}

impl Conn {
    /// Connects, in whichever string form the driver's own tooling uses.
    ///
    /// ADR-0014's seam table said this signature was unchanged. That was
    /// measured by *replacing* one driver with another; keeping both needs the
    /// caller to say which, and the ADR's own verdict makes the correction —
    /// "the spike replaced one driver with another behind the same shape. Phase
    /// 5 has to keep both, selected per project, and that is not the same
    /// test."
    pub async fn connect(driver: Driver, connection_string: &str) -> Result<Self, DbError> {
        match driver {
            Driver::Mssql => Ok(Conn::Mssql(Box::new(
                mssql::Conn::connect(connection_string).await?,
            ))),
            Driver::Postgres => Ok(Conn::Postgres(Box::new(
                postgres::Conn::connect(connection_string).await?,
            ))),
        }
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
        match self {
            Conn::Mssql(c) => Ok(c.query(sql).await?.into_iter().map(Row::Mssql).collect()),
            Conn::Postgres(c) => Ok(c.query(sql).await?.into_iter().map(Row::Postgres).collect()),
        }
    }

    /// Runs one batch of statements and discards any results.
    pub async fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        match self {
            Conn::Mssql(c) => c.execute(sql).await,
            Conn::Postgres(c) => c.execute(sql).await,
        }
    }

    /// Runs one parameterized query and returns every row.
    ///
    /// The placeholder syntax is the driver's — `@P1` on SQL Server, `$1` on
    /// PostgreSQL — because the SQL around it is the dialect's too.
    pub async fn query_with(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
    ) -> Result<Vec<Row>, DbError> {
        match self {
            Conn::Mssql(c) => Ok(c
                .query_with(sql, params)
                .await?
                .into_iter()
                .map(Row::Mssql)
                .collect()),
            Conn::Postgres(c) => Ok(c
                .query_with(sql, params)
                .await?
                .into_iter()
                .map(Row::Postgres)
                .collect()),
        }
    }

    /// Runs one parameterized statement and returns the number of rows affected.
    ///
    /// The ledger is the only thing that writes *data*, and everything it writes
    /// is user-supplied: a reason, an operator's name, a whole JSON snapshot.
    /// Binding is not politeness here — a `'` in an operator's name would end
    /// the statement.
    pub async fn execute_with(&mut self, sql: &str, params: &[Param<'_>]) -> Result<u64, DbError> {
        match self {
            Conn::Mssql(c) => c.execute_with(sql, params).await,
            Conn::Postgres(c) => c.execute_with(sql, params).await,
        }
    }

    /// Opens a transaction with the dialect's own statement.
    ///
    /// What the statement has to say is the dialect's business — SQL Server's
    /// carries `SET XACT_ABORT ON`, without which a failed statement halfway
    /// through a plan leaves the earlier ones committable — and it is asked
    /// for rather than held here, so this crate keeps to what it owns: that a
    /// transaction is opened, and closed again on every path (ADR-0014 §2).
    pub async fn begin(&mut self, framing: TransactionFraming) -> Result<(), DbError> {
        self.execute(framing.begin).await
    }

    pub async fn commit(&mut self, framing: TransactionFraming) -> Result<(), DbError> {
        self.execute(framing.commit).await
    }

    /// Rolls back. The dialect's statement is expected to tolerate a
    /// transaction the server has already killed, so that its own error cannot
    /// replace the real failure — the statement that broke — with a second one.
    pub async fn rollback(&mut self, framing: TransactionFraming) -> Result<(), DbError> {
        self.execute(framing.rollback).await
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
