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
//! being defined twice. [`catalog`], [`impact`] and [`doctor`] hold the other
//! answers a connected command consumes — what a pull found, what a rename
//! touches, what `doctor` asks about — by the same rule and for the same
//! reason: one definition each, filled by whichever engine the connection is
//! to, so that `pbps-cli` can ask one question of either (DECISIONS 417).
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
//! `pbps-mssql` that already had it (DECISIONS 225).

use pbps_dialect::TransactionFraming;
pub mod catalog;
pub mod doctor;
pub mod impact;
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

    /// The server was reached and is not the session the connection string
    /// requires (`target_session_attrs`).
    ///
    /// Its own variant because it is the one connection failure that is not
    /// about *reaching* a server: `cannot reach {addr}` would be false, and the
    /// fix is to point the string at a different server rather than to open a
    /// port. It does not make the three of ADR-0014 §3 into four — those three
    /// are how a socket can fail, and this is a server that answered.
    #[error("`{addr}` is a {found} session, and this connection string requires {wanted}")]
    WrongSession {
        addr: String,
        wanted: &'static str,
        found: &'static str,
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
            | DbError::WrongSession { .. }
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

/// Opens a TCP socket to `addr`, bounded by [`CONNECT_TIMEOUT`], giving every
/// address the name resolves to a chance inside that budget.
///
/// One place, used by both drivers, because "there is a network" is this
/// crate's and neither driver's — and because the bug this shape had was the
/// same on both sides of the seam.
///
/// `tokio::net::TcpStream::connect(host)` resolves the name and tries the
/// addresses **in turn**, returning the last error; a timeout wrapped around it
/// bounds the *whole loop*. So one address that drops packets spends the entire
/// budget and a healthy second address is never tried, and a dual-stack
/// endpoint whose IPv6 address is black-holed is reported unreachable while it
/// is reachable. Refusing work against a server that is up is the failure this
/// project puts first.
///
/// The budget is divided as it is spent — each attempt gets what is left,
/// divided by how many addresses are left — so the total is still
/// [`CONNECT_TIMEOUT`] however many addresses there are, an address that
/// refuses at once hands its share to the rest, and the last one gets whatever
/// remains. Fixed shares would have made a slow-but-answering server fail
/// behind a dead one.
pub(crate) async fn open_socket(addr: &str) -> Result<tokio::net::TcpStream, DbError> {
    let started = tokio::time::Instant::now();

    // Resolution is inside the budget too: a DNS server that does not answer is
    // one of the ways an endpoint fails to be reachable.
    let addresses: Vec<std::net::SocketAddr> =
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host(addr)).await {
            Ok(Ok(found)) => found.collect(),
            Ok(Err(source)) => {
                return Err(DbError::Connect {
                    addr: addr.to_owned(),
                    source,
                });
            }
            Err(_elapsed) => {
                return Err(DbError::ConnectTimeout {
                    addr: addr.to_owned(),
                });
            }
        };
    // A name that resolves to nothing is not a name that refused: it has to say
    // which of the two happened, or the reader goes and looks at the firewall.
    if addresses.is_empty() {
        return Err(DbError::Connect {
            addr: addr.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the name resolved to no address",
            ),
        });
    }
    let left = CONNECT_TIMEOUT
        .checked_sub(started.elapsed())
        .unwrap_or_default();
    connect_any(addr, addresses, left).await
}

/// Tries each address in turn inside one budget, and says which way it failed.
///
/// The budget is divided as it is spent — each attempt gets what is left,
/// divided by how many addresses are left — so the total is what the caller
/// gave however many addresses there are, an address that refuses at once hands
/// its share to the rest, and the last one gets whatever remains. Fixed shares
/// would make a slow-but-answering server fail behind a dead one.
///
/// Split out from [`open_socket`] so that a test can choose the order and the
/// budget: resolution order is the operating system's, and a test that depends
/// on it passes or fails by luck.
async fn connect_any(
    addr: &str,
    addresses: Vec<std::net::SocketAddr>,
    budget: std::time::Duration,
) -> Result<tokio::net::TcpStream, DbError> {
    let started = tokio::time::Instant::now();
    let mut remaining = addresses.len();
    let mut last: Option<std::io::Error> = None;
    for address in addresses {
        let Some(left) = budget.checked_sub(started.elapsed()) else {
            break;
        };
        let share = left / u32::try_from(remaining).unwrap_or(u32::MAX);
        remaining -= 1;
        match tokio::time::timeout(share, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(socket)) => return Ok(socket),
            Ok(Err(source)) => last = Some(source),
            // Out of time for *this* address, not for the endpoint: the next
            // one may answer at once.
            Err(_elapsed) => {}
        }
    }
    // A refusal from some address outranks the clock: it is the more specific
    // answer, and the one whose fix the reader can act on. Only when no address
    // ever answered at all is this the dropped-packets case.
    match last {
        Some(source) => Err(DbError::Connect {
            addr: addr.to_owned(),
            source,
        }),
        None => Err(DbError::ConnectTimeout {
            addr: addr.to_owned(),
        }),
    }
}

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
/// line here, which is the point (DECISIONS 225).
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
/// would have cost is in DECISIONS 225; the short of it is that the enum is the
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
    /// Which driver this connection is over — and so which engine it is to.
    ///
    /// The one fact a caller may read off a connection: the dialect-level
    /// dispatch in `pbps-cli` chooses an engine's catalog, ledger and impact
    /// functions by it, the way this enum chooses a driver. Reading it is not
    /// naming a driver type (constraint 9): the variant says *which*, and the
    /// client inside stays this crate's.
    #[must_use]
    pub const fn driver(&self) -> Driver {
        match self {
            Conn::Mssql(_) => Driver::Mssql,
            Conn::Postgres(_) => Driver::Postgres,
        }
    }

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

#[cfg(test)]
mod socket_tests {
    // Gated with the tests that use it. Two of the three are Linux-only, and
    // `-D warnings` turns an import nothing uses into an error — on the other
    // platform only, which is a failure this machine cannot see.
    #[cfg(target_os = "linux")]
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::*;

    /// A socket that accepts nothing and whose accept queue is full, so the
    /// kernel drops every further SYN — the same black hole the PostgreSQL live
    /// suite builds, and for the same reason: a reserved address is not one, it
    /// drops the first SYN of a run and answers `EHOSTUNREACH` for the rest.
    ///
    /// Linux-only, because this is Linux's overflow behaviour with
    /// `tcp_abort_on_overflow` at its default of 0. Windows sends an RST, which
    /// is the *refused* category, so these tests are absent there rather than
    /// asserting something the platform does not do.
    /// The listener is handed back with the queued connections: dropping it
    /// closes the port, and a closed port **refuses** instead of dropping. An
    /// earlier version of this helper let it fall out of scope, and the test
    /// that should have proved the black hole passed because the address
    /// refused quickly — a test passing for the wrong reason, caught by its
    /// sibling asserting the other category.
    #[cfg(target_os = "linux")]
    async fn black_hole() -> (
        SocketAddr,
        tokio::net::TcpListener,
        Vec<tokio::net::TcpStream>,
    ) {
        let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
        socket
            .bind("127.0.0.1:0".parse().expect("an address"))
            .expect("bind");
        let listener = socket.listen(1).expect("listen");
        let addr = listener.local_addr().expect("the bound address");
        let mut queued = Vec::new();
        for _ in 0..16 {
            match tokio::time::timeout(
                Duration::from_millis(250),
                tokio::net::TcpStream::connect(addr),
            )
            .await
            {
                Ok(Ok(stream)) => queued.push(stream),
                Ok(Err(e)) => panic!("the loopback refused a connection: {e}"),
                Err(_) => break,
            }
        }
        assert!(!queued.is_empty(), "the queue was never filled");
        (addr, listener, queued)
    }

    /// An address that drops packets must not spend the whole budget: a name
    /// that resolves to several addresses is dual-stack far more often than it
    /// is broken, and reporting a reachable endpoint unreachable is refusing
    /// work against a server that is up.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_later_address_is_tried_when_an_earlier_one_drops_packets() {
        let (dead, _listener, _queued) = black_hole().await;
        let alive = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let good = alive.local_addr().expect("the bound address");

        let socket = connect_any("name:0", vec![dead, good], Duration::from_millis(600))
            .await
            .expect("the healthy address answers");
        assert_eq!(socket.peer_addr().expect("a peer"), good);
    }

    /// And when nothing answers, the two ways of not answering stay two. A
    /// refusal is the more specific report and outranks the clock; only when no
    /// address ever answered is it the dropped-packets case.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_refusal_outranks_the_clock_and_only_silence_is_a_timeout() {
        let (dead, _listener, _queued) = black_hole().await;
        // Bound and dropped: nothing is listening, so the port refuses.
        let refused: SocketAddr = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a listener");
            listener.local_addr().expect("the bound address")
        };

        let error = connect_any("name:0", vec![dead, refused], Duration::from_millis(600))
            .await
            .expect_err("neither address answers");
        assert!(matches!(error, DbError::Connect { .. }), "{error:?}");

        // Three of them, and the budget is the budget: it is divided as it is
        // spent, so the whole attempt is bounded however many addresses there
        // are. A full budget per address would take three times as long, and
        // `CONNECT_TIMEOUT` would stop meaning what it says.
        let budget = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let error = connect_any("name:0", vec![dead, dead, dead], budget)
            .await
            .expect_err("no address answers");
        assert!(matches!(error, DbError::ConnectTimeout { .. }), "{error:?}");
        let waited = started.elapsed();
        assert!(waited < budget * 2, "the budget was not shared: {waited:?}");
    }

    /// A name that resolves to nothing is not a name that refused, and neither
    /// is one that resolves to something unreachable.
    #[tokio::test]
    async fn a_name_that_resolves_to_no_address_says_so() {
        let error = connect_any("name:0", Vec::new(), Duration::from_millis(50))
            .await
            .expect_err("there is nothing to connect to");
        assert!(matches!(error, DbError::ConnectTimeout { .. }), "{error:?}");
    }
}
