//! The PostgreSQL driver, and the only file in this crate that names
//! `tokio_postgres`.
//!
//! The sibling of [`crate::mssql`], under the rule that module's header states:
//! one file per driver, and nothing outside them names either one.
//!
//! # The structural difference, and why it does not leak
//!
//! `tokio-postgres` returns a `Client` **and** a `Connection` future that
//! something must poll, where `tiberius` returns one client. So [`Conn`] owns a
//! task handle that has no counterpart on the other side. ADR-0014 §3 measured
//! that this is the one genuinely structural difference between the two drivers
//! and that it stays inside the crate: no caller of `crate::Conn` sees it, and
//! no signature changed for it.
//!
//! Dropping the handle is what closes the connection, so it is held rather than
//! detached: a detached task would outlive the `Conn` that owns it and keep a
//! server-side session open after the command that opened it has exited.

use tokio::net::TcpStream;
use tokio_postgres::Config;
use tokio_postgres::tls::MakeTlsConnect;

use crate::{CONNECT_TIMEOUT, DbError, Param};

/// One row as this driver hands it back.
pub struct Row(tokio_postgres::Row);

/// An open PostgreSQL connection.
pub struct Conn {
    client: tokio_postgres::Client,
    /// The polled connection future. Held, not detached — see the module header.
    _driver: tokio::task::JoinHandle<()>,
}

impl From<tokio_postgres::Error> for DbError {
    /// The driver's error, flattened to text and a SQLSTATE.
    ///
    /// `42P01` has a letter in it, which is why the seam carries the code as
    /// text: an `Option<u32>` here was a T-SQL shape under a neutral name
    /// (ADR-0014 §1, DECISIONS 193).
    fn from(e: tokio_postgres::Error) -> Self {
        DbError::Driver {
            code: e.code().map(|c| c.code().to_owned()),
            message: e.to_string(),
        }
    }
}

impl Conn {
    /// Connects using a libpq connection string or a `postgres://` URL — the
    /// two forms every PostgreSQL tool already asks for, for the same reason
    /// the SQL Server side takes an ADO.NET string: users paste what they have.
    ///
    /// The three connection failures stay three. A malformed string, a refused
    /// socket and a firewall that drops packets need three different fixes and
    /// the CLI's diagnostics say which; a first draft of ADR-0014's spike
    /// folded the timeout into `BadConnectionString`, so a target that silently
    /// dropped the connection for thirty seconds would have been reported as a
    /// typo, and review caught it.
    pub(crate) async fn connect(connection_string: &str) -> Result<Self, DbError> {
        let config: Config = connection_string
            .parse()
            .map_err(|e: tokio_postgres::Error| DbError::BadConnectionString(e.to_string()))?;
        let (host, port) = endpoint(&config);
        let addr = format!("{host}:{port}");

        // Opening the socket here rather than letting the driver do it is what
        // keeps `Connect` and `ConnectTimeout` apart: the driver opens the
        // socket inside its own `connect` and wraps the `io::Error` in its
        // error type, and the seam wants that error by value (ADR-0014 §3).
        let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(source)) => return Err(DbError::Connect { addr, source }),
            Err(_elapsed) => return Err(DbError::ConnectTimeout { addr }),
        };
        tcp.set_nodelay(true)
            .map_err(|source| DbError::Connect { addr, source })?;

        // `connect_raw`, not `connect`: the driver's own `connect` opens the
        // socket, and then the three failures above collapse into its error
        // type. The price is making the TLS connector by hand, which is also
        // where the host name for SNI is supplied — the socket is already
        // open, so nothing else would tell it which certificate to expect.
        let mut maker = tls()?;
        let connector =
            MakeTlsConnect::<TcpStream>::make_tls_connect(&mut maker, &host).map_err(|e| {
                DbError::BadConnectionString(format!("`{host}` is not a TLS host: {e}"))
            })?;
        let (client, connection) = config.connect_raw(tcp, connector).await?;
        let driver = tokio::spawn(async move {
            // The connection future resolves when the socket closes. Its error
            // is the socket's, and the next statement reports it with the
            // context of what was being run; logging it here would print a
            // second failure beside the real one.
            let _ = connection.await;
        });
        Ok(Self {
            client,
            _driver: driver,
        })
    }

    /// Runs one query with no parameters and returns every row.
    ///
    /// `query`, not `simple_query`: the simple protocol hands back every value
    /// as text and a `SimpleQueryRow`, which is a second row type with a second
    /// set of conversions. One statement through the extended protocol keeps
    /// [`Row`] the only row this driver produces, and an introspection query is
    /// one statement.
    pub(crate) async fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        Ok(self
            .client
            .query(sql, &[])
            .await?
            .into_iter()
            .map(Row)
            .collect())
    }

    /// Runs one batch of statements and discards any results.
    ///
    /// `batch_execute` is the analogue of the other driver's `simple_query`
    /// (ADR-0014's seam table): DDL is sent as a batch exactly the way the
    /// emitter framed it, with no parameter machinery in the way.
    pub(crate) async fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        self.client.batch_execute(sql).await?;
        Ok(())
    }

    pub(crate) async fn query_with(
        &mut self,
        sql: &str,
        params: &[Param<'_>],
    ) -> Result<Vec<Row>, DbError> {
        let bound = bind(params);
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            bound.iter().map(AsRef::as_ref).collect();
        Ok(self
            .client
            .query(sql, &refs)
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
        let bound = bind(params);
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            bound.iter().map(AsRef::as_ref).collect();
        Ok(self.client.execute(sql, &refs).await?)
    }
}

/// The host and port the two connect errors name, and TLS verifies against.
///
/// From the parsed `Config`'s host and port lists rather than from one string,
/// which is the same dialect knowledge that puts parsing behind `connect`
/// (ADR-0014 §3).
fn endpoint(config: &Config) -> (String, u16) {
    let host = config
        .get_hosts()
        .iter()
        // `if let` rather than a match with a wildcard: `Host` carries a Unix
        // variant on some platforms and not others, so a wildcard arm is
        // unreachable on one of them and required on the other, and no single
        // lint expectation is right for both.
        //
        // A Unix socket is skipped rather than guessed at: this seam opens a
        // `TcpStream`, so a config naming only a socket path falls to the
        // default below and fails to connect saying so, instead of silently
        // reaching a different host.
        .find_map(|h| {
            if let tokio_postgres::config::Host::Tcp(name) = h {
                Some(name.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "localhost".to_owned());
    let port = config.get_ports().first().copied().unwrap_or(5432);
    (host, port)
}

/// The TLS stack, chosen here rather than pinned by the driver.
///
/// ADR-0014's Limits name this: the spike used `NoTls`, and a real dialect
/// needs a stack — which is where open question 10's supply-chain story
/// rejoins. `rustls` with the platform's own trust store, and the same `rustls`
/// the other driver already resolves, so the tree carries one.
fn tls() -> Result<tokio_postgres_rustls::MakeRustlsConnect, DbError> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    // A trust store that failed to load is not an empty one. Connecting with no
    // roots would refuse every certificate and report it as the server's fault,
    // which is the shape this project keeps finding: absent, empty and
    // unreadable are three different things.
    if !native.errors.is_empty() && native.certs.is_empty() {
        return Err(DbError::BadConnectionString(format!(
            "cannot read this host's certificate trust store: {}",
            native
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    roots.add_parsable_certificates(native.certs);
    // The provider is named, not looked up. `ClientConfig::builder()` asks
    // rustls for the *process* default, which exists only when exactly one
    // provider is compiled in — and it panics rather than erring when that is
    // not so. A panic inside a connection is not a failure this seam can
    // report, and which providers end up in the tree is decided by a
    // dependency's default features rather than by anything here. `tiberius`
    // names its own for the same reason, and this is the same answer on the
    // other side of the seam (DECISIONS 228).
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| DbError::BadConnectionString(format!("this build has no usable TLS: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

/// Owns each bound value for as long as the statement runs.
///
/// Boxed rather than borrowed from the `Param`s: this driver wants
/// `&dyn ToSql + Sync`, and `Param::OptStr` holds an `Option<&str>` whose
/// `ToSql` implementation is for the `Option` itself, so there is nothing in
/// the enum of the right shape to point at.
fn bind<'a>(params: &'a [Param<'a>]) -> Vec<Box<dyn tokio_postgres::types::ToSql + Sync + 'a>> {
    params
        .iter()
        .map(|p| -> Box<dyn tokio_postgres::types::ToSql + Sync + 'a> {
            match *p {
                Param::I32(v) => Box::new(v),
                Param::I64(v) => Box::new(v),
                Param::Str(v) => Box::new(v),
                Param::OptStr(v) => Box::new(v),
            }
        })
        .collect()
}

/// The three types the seam reads. See [`crate::mssql::Row`]'s note for why the
/// set is closed rather than blanket-implemented.
impl Row {
    pub(crate) fn str_by_name<'a>(&'a self, col: &str) -> Result<Option<&'a str>, DbError> {
        Ok(self.0.try_get::<_, Option<&str>>(col)?)
    }

    pub(crate) fn str_at(&self, idx: usize) -> Result<Option<&str>, DbError> {
        Ok(self.0.try_get::<_, Option<&str>>(idx)?)
    }

    pub(crate) fn i32_by_name(&self, col: &str) -> Result<Option<i32>, DbError> {
        Ok(self.0.try_get::<_, Option<i32>>(col)?)
    }

    pub(crate) fn i32_at(&self, idx: usize) -> Result<Option<i32>, DbError> {
        Ok(self.0.try_get::<_, Option<i32>>(idx)?)
    }

    pub(crate) fn i64_by_name(&self, col: &str) -> Result<Option<i64>, DbError> {
        Ok(self.0.try_get::<_, Option<i64>>(col)?)
    }

    pub(crate) fn i64_at(&self, idx: usize) -> Result<Option<i64>, DbError> {
        Ok(self.0.try_get::<_, Option<i64>>(idx)?)
    }

    pub(crate) fn i16_by_name(&self, col: &str) -> Result<Option<i16>, DbError> {
        Ok(self.0.try_get::<_, Option<i16>>(col)?)
    }

    pub(crate) fn i16_at(&self, idx: usize) -> Result<Option<i16>, DbError> {
        Ok(self.0.try_get::<_, Option<i16>>(idx)?)
    }

    pub(crate) fn bool_by_name(&self, col: &str) -> Result<Option<bool>, DbError> {
        Ok(self.0.try_get::<_, Option<bool>>(col)?)
    }

    pub(crate) fn bool_at(&self, idx: usize) -> Result<Option<bool>, DbError> {
        Ok(self.0.try_get::<_, Option<bool>>(idx)?)
    }

    /// PostgreSQL has no unsigned types, so nothing here is read as a `u8`.
    ///
    /// The trait is shared, so the method has to exist; what it must not do is
    /// invent a conversion. SQL Server reads `precision` and `scale` out of its
    /// catalog as `tinyint`, and a PostgreSQL query asking for one is a bug in
    /// that query — which is exactly what [`DbError::BadRow`] is for.
    pub(crate) fn u8_by_name(&self, col: &str) -> Result<Option<u8>, DbError> {
        Err(Self::no_u8(col))
    }

    pub(crate) fn u8_at(&self, idx: usize) -> Result<Option<u8>, DbError> {
        Err(Self::no_u8(&idx.to_string()))
    }

    fn no_u8(column: &str) -> DbError {
        DbError::BadRow(format!(
            "column `{column}` was read as an unsigned byte, which PostgreSQL has \
             no type for; the query asking for it is wrong"
        ))
    }
}
