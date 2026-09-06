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

use crate::{DbError, Param};

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
        let (host, port) = endpoint(&config)?;
        let addr = format!("{host}:{port}");

        // Opening the socket here rather than letting the driver do it is what
        // keeps `Connect` and `ConnectTimeout` apart: the driver opens the
        // socket inside its own `connect` and wraps the `io::Error` in its
        // error type, and the seam wants that error by value (ADR-0014 §3).
        // How it is opened is [`crate::open_socket`]'s, shared with the other
        // driver: every address the name resolves to gets a chance inside the
        // one budget.
        let tcp = crate::open_socket(&addr).await?;
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
        let mut conn = Self {
            client,
            _driver: driver,
        };
        // The endpoint is spelled again rather than kept: `addr` is moved into
        // whichever of the two connect errors above fires.
        conn.require_session(config.get_target_session_attrs(), &format!("{host}:{port}"))
            .await?;
        Ok(conn)
    }

    /// Enforces `target_session_attrs`, which `connect_raw` does not.
    ///
    /// `Config::connect` runs this probe itself, *after* the handshake; this
    /// seam calls `connect_raw` — which is what keeps the three connection
    /// failures three — and inherits none of it. Dropping the constraint
    /// silently would point DDL at a primary the connection string excluded.
    ///
    /// Reproduced rather than refused: `target_session_attrs=read-write` is an
    /// ordinary thing to write in front of a replica set, and refusing it would
    /// refuse a string that works. `SHOW transaction_read_only` is the question
    /// the driver asks and the one libpq asks.
    ///
    /// The only other thing `Config::connect` does past `connect_raw` is record
    /// a socket config for `CancelToken`. Nothing here cancels a query and no
    /// token is ever built, so there is nothing to reproduce.
    async fn require_session(
        &mut self,
        wanted: tokio_postgres::config::TargetSessionAttrs,
        addr: &str,
    ) -> Result<(), DbError> {
        use tokio_postgres::config::TargetSessionAttrs;

        if wanted == TargetSessionAttrs::Any {
            return Ok(());
        }
        // `if` rather than `match`: the enum is `#[non_exhaustive]`, so a match
        // needs a wildcard arm, and a wildcard over a foreign enum is the
        // shape `wildcard_enum_match_arm` exists to refuse.
        let wanted_read_only = if wanted == TargetSessionAttrs::ReadOnly {
            true
        } else if wanted == TargetSessionAttrs::ReadWrite {
            false
        } else {
            // A value a later driver added, which this seam has never checked.
            // An unchecked constraint must not read as a satisfied one.
            return Err(DbError::BadConnectionString(format!(
                "`target_session_attrs` is set to {wanted:?}, which this build \
                 does not know how to check"
            )));
        };
        let rows = self.query("SHOW transaction_read_only").await?;
        let answer = rows.first().map(|r| r.str_at(0)).transpose()?.flatten();
        let read_only = match answer {
            Some("on") => true,
            Some("off") => false,
            // Absent, empty and unreadable are three different things, and none
            // of them is "the session is acceptable".
            Some(other) => {
                return Err(DbError::BadRow(format!(
                    "`SHOW transaction_read_only` answered `{other}`, which is \
                     neither `on` nor `off`"
                )));
            }
            None => {
                return Err(DbError::BadRow(
                    "`SHOW transaction_read_only` returned no row".to_owned(),
                ));
            }
        };
        if read_only == wanted_read_only {
            return Ok(());
        }
        let name = |read_only: bool| if read_only { "read-only" } else { "read-write" };
        Err(DbError::WrongSession {
            addr: addr.to_owned(),
            wanted: name(wanted_read_only),
            found: name(read_only),
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

/// The one TCP endpoint this seam will dial, or a refusal that names why it
/// will not dial anything.
///
/// From the parsed `Config`'s host and port lists rather than from one string,
/// which is the same dialect knowledge that puts parsing behind `connect`
/// (ADR-0014 §3).
///
/// A `Result`, and that is a correction. This used to fall back to `localhost`
/// for anything it did not recognise, with a comment claiming the connection
/// would then "fail to connect saying so". It would not: `localhost:5432` is
/// exactly where a machine configured with a Unix socket keeps a real server,
/// so the fallback reached a different endpoint under a different
/// authentication method and reported **success**. "This endpoint" and "some
/// endpoint" are two different things, and a guess is not one of them.
fn endpoint(config: &Config) -> Result<(String, u16), DbError> {
    // `hostaddr` is what the driver would dial, leaving `host` for TLS and for
    // the error text. This seam opens the socket itself, so honouring one and
    // not the other would dial a server the diagnostics do not name.
    if !config.get_hostaddrs().is_empty() {
        return Err(DbError::BadConnectionString(
            "`hostaddr` is not supported: pbps opens the socket itself, so the \
             address it dials has to be the one its errors name and TLS verifies \
             against. Give `host` alone."
                .to_owned(),
        ));
    }
    let hosts = config.get_hosts();
    if hosts.len() > 1 {
        return Err(DbError::BadConnectionString(format!(
            "this connection string names {} hosts; pbps connects to exactly one. \
             Trying them in turn is the driver's own failover, and this seam opens \
             the socket itself so that a refused connection and a dropped one stay \
             two different errors. Name one host.",
            hosts.len()
        )));
    }
    let Some(host) = hosts.first() else {
        return Err(DbError::BadConnectionString(
            "this connection string names no host".to_owned(),
        ));
    };
    // A `match` with a `cfg`-gated arm rather than an `if let`: `Host::Unix`
    // exists only on Unix, so an `if let Host::Tcp(_)` is irrefutable on
    // Windows — a warning there and nowhere else, under a CI that denies them.
    // This shape is exhaustive on both platforms and wildcards on neither.
    let name = match host {
        tokio_postgres::config::Host::Tcp(name) => name.clone(),
        #[cfg(unix)]
        tokio_postgres::config::Host::Unix(path) => {
            return Err(DbError::BadConnectionString(format!(
                "the Unix socket `{}` cannot be used: pbps opens a `TcpStream` \
                 itself, so that a refused connection and a firewall that drops \
                 packets stay two different errors. Give `host=<name>` and a TCP \
                 port.",
                path.display()
            )));
        }
    };
    // The driver's own path refuses a port list that does not line up with the
    // host list, so taking the first of several would turn a string it calls
    // malformed into a connection to whichever one came first.
    let ports = config.get_ports();
    if ports.len() > 1 {
        return Err(DbError::BadConnectionString(format!(
            "this connection string names one host and {} ports. The driver \
             refuses that, because a port list has to line up with a host list, \
             and taking the first would connect somewhere the string did not \
             unambiguously name.",
            ports.len()
        )));
    }
    let port = ports.first().copied().unwrap_or(5432);
    Ok((name, port))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint_of(connection: &str) -> Result<(String, u16), DbError> {
        endpoint(&connection.parse::<Config>().expect("a config parses"))
    }

    /// The ordinary case, and the two things the seam takes from the config
    /// rather than from the string: the host and port an error will name.
    #[test]
    fn a_single_tcp_host_is_the_endpoint_and_the_port_defaults() {
        assert_eq!(
            endpoint_of("host=db.example port=6543 user=u").expect("an endpoint"),
            ("db.example".to_owned(), 6543)
        );
        assert_eq!(
            endpoint_of("host=db.example user=u").expect("an endpoint"),
            ("db.example".to_owned(), 5432)
        );
    }

    /// Everything this seam cannot dial is **refused**, not redirected. Each of
    /// these used to fall through to `localhost:5432`, which on the machines
    /// that write them is a real server — so pbps would have connected to a
    /// different endpoint, under a different authentication method, and called
    /// it success.
    #[test]
    fn an_endpoint_this_seam_cannot_dial_is_refused_and_never_guessed() {
        for connection in [
            #[cfg(unix)]
            "host=/var/run/postgresql user=u",
            "hostaddr=10.0.0.5 host=db.example user=u",
            "host=first.example,second.example user=u",
            // One host and two ports: the driver's own path calls this
            // malformed, so taking the first would turn a string it refuses
            // into a connection to whichever port came first.
            "host=db.example port=5432,6432 user=u",
        ] {
            let error = endpoint_of(connection).expect_err(connection);
            assert!(
                matches!(error, DbError::BadConnectionString(_)),
                "{connection}: {error:?}"
            );
            // Never the fallback that made this a bug: the message has to say
            // what is wrong with the string, not name a host nobody asked for.
            assert!(
                !error.to_string().contains("localhost"),
                "{connection}: {error}"
            );
        }
    }
}
