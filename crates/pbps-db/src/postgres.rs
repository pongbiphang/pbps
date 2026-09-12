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

use socket2::{SockRef, TcpKeepalive};
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
    ///
    /// The text is not `e.to_string()`. **Measured**: `tokio_postgres::Error`'s
    /// `Display` keeps the server's own sentence in the error's *source*, not
    /// in its message — for a server-side failure it renders the literal `db
    /// error`, five characters wearing the clothes of a real answer while the
    /// SQLSTATE beside it is the only thing still true (issue #167). What the
    /// server actually said is behind [`tokio_postgres::Error::as_db_error`],
    /// which is `None` exactly when the failure never reached the server — a
    /// closed connection, a broken handshake — and `e.to_string()` already
    /// carries its own text for those (`"connection closed"`, and so on), so
    /// the fallback is unchanged (DECISIONS 453).
    fn from(e: tokio_postgres::Error) -> Self {
        let message = e
            .as_db_error()
            .map(server_error_message)
            .unwrap_or_else(|| e.to_string());
        DbError::Driver {
            code: e.code().map(|c| c.code().to_owned()),
            message,
        }
    }
}

/// Renders a server-side PostgreSQL error the way `psql` does, not the one
/// line `Display` on `tokio_postgres::error::DbError` gives.
///
/// `message()` alone is what the fix in issue #167 asks for and would already
/// beat `db error`. `detail()` and `hint()` are folded in on the next lines
/// because several diagnostics this workspace builds today reconstruct by
/// hand exactly what these carry — `pbps-pg`'s own `schema_changed_underneath`
/// used to guess a whole sentence from a bare SQLSTATE because the sentence
/// the server sent was unreachable through this seam. The object identifiers
/// PostgreSQL tags on for some errors — schema, table, column, data type,
/// constraint — are appended the same way for the same reason: a unique
/// violation's `detail` already names the columns, but a NOT NULL violation
/// carries no `detail` at all and the column name is only reachable through
/// `column()` (measured on 18.6, DECISIONS 453).
fn server_error_message(db: &tokio_postgres::error::DbError) -> String {
    let mut message = db.message().to_owned();
    if let Some(detail) = db.detail() {
        message.push_str("\nDETAIL: ");
        message.push_str(detail);
    }
    if let Some(hint) = db.hint() {
        message.push_str("\nHINT: ");
        message.push_str(hint);
    }
    let identifiers: Vec<String> = [
        db.schema().map(|v| format!("schema \"{v}\"")),
        db.table().map(|v| format!("table \"{v}\"")),
        db.column().map(|v| format!("column \"{v}\"")),
        db.datatype().map(|v| format!("type \"{v}\"")),
        db.constraint().map(|v| format!("constraint \"{v}\"")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !identifiers.is_empty() {
        message.push_str("\nWHERE: ");
        message.push_str(&identifiers.join(", "));
    }
    message
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
        Self::connect_as(connection_string, cfg!(target_os = "linux")).await
    }

    /// [`connect`]'s body, taking the platform as a parameter rather than
    /// reading `cfg!(target_os = "linux")` itself.
    ///
    /// The one thing this connection string's disposition depends on that
    /// `endpoint`'s own refusals do not is which platform is running it —
    /// and a test that can only observe that by actually running on the
    /// other platform cannot pin the *ordering* this function promises:
    /// that `tcp_user_timeout` is refused before `open_socket` runs, not
    /// discovered after (review of issue #113 on PR #346). Passing
    /// `is_linux` through lets a test simulate either platform here, the
    /// same reason [`tcp_user_timeout_disposition`] takes it as a
    /// parameter rather than being `#[cfg]`-gated itself.
    async fn connect_as(connection_string: &str, is_linux: bool) -> Result<Self, DbError> {
        let config: Config = connection_string
            .parse()
            .map_err(|e: tokio_postgres::Error| DbError::BadConnectionString(e.to_string()))?;
        let (host, port) = endpoint(&config)?;
        let addr = format!("{host}:{port}");
        let budget = connect_budget(&config)?;
        let shuffle = wants_random_order(&config);
        // `tcp_user_timeout` is the one of the seven socket-tuning parameters
        // whose disposition depends on the platform this binary runs on, not
        // on anything `open_socket` could discover — so its refusal belongs
        // beside `endpoint`'s own `hostaddr`/multi-host/Unix-socket refusals,
        // before any socket opens, not inside `apply_socket_options` after
        // one already has. A connection string this build will not accept
        // must be refused without a network round trip, and certainly
        // without waiting out `budget` first to learn the endpoint is also
        // unreachable — a review finding on PR #346 caught this landing
        // after `open_socket` in an earlier draft (DECISIONS 434).
        let tcp_user_timeout =
            tcp_user_timeout_disposition(is_linux, config.get_tcp_user_timeout().copied())?;

        // Opening the socket here rather than letting the driver do it is what
        // keeps `Connect` and `ConnectTimeout` apart: the driver opens the
        // socket inside its own `connect` and wraps the `io::Error` in its
        // error type, and the seam wants that error by value (ADR-0014 §3).
        // How it is opened is [`crate::open_socket`]'s, shared with the other
        // driver: every address the name resolves to gets a chance inside the
        // one budget — `budget` rather than `pbps_db::CONNECT_TIMEOUT` bare,
        // because `connect_timeout` may have asked for less than that ceiling
        // (see `connect_budget`), and `shuffle` because `load_balance_hosts
        // =random` reorders this same resolved list rather than touching a
        // socket option (issue #113).
        let tcp = crate::open_socket(&addr, budget, shuffle).await?;
        tcp.set_nodelay(true)
            .map_err(|source| DbError::Connect { addr, source })?;
        // `addr` was just moved above (see the note on `require_session`
        // below), so this and every later error in `connect` names the
        // endpoint through a freshly built string instead.
        //
        // Keepalive and `tcp_user_timeout` land here — after `open_socket`,
        // before `connect_raw` — because `connect_raw` hands the socket
        // straight to the PostgreSQL handshake and never asks it anything
        // socket-level; `Config::connect`'s own `connect_socket` is the one
        // place that does, and this seam does not call it (issue #113).
        apply_socket_options(&tcp, &config, tcp_user_timeout, &format!("{host}:{port}"))?;

        // `connect_raw`, not `connect`: the driver's own `connect` opens the
        // socket, and then the three failures above collapse into its error
        // type. The price is making the TLS connector by hand, which is also
        // where the host name for SNI is supplied — the socket is already
        // open, so nothing else would tell it which certificate to expect.
        //
        // Two branches, because a connection that has turned TLS off must not
        // depend on a TLS stack: building one reads the host's trust store, and
        // a host with none — a minimal image, which is what SPEC §11.3's single
        // binary is for — would have had `sslmode=disable` refused over
        // certificates it was never going to look at.
        let (client, driver) = if wants_tls(&config) {
            let mut maker = tls(&config)?;
            let connector = MakeTlsConnect::<TcpStream>::make_tls_connect(&mut maker, &host)
                .map_err(|e| {
                    DbError::BadConnectionString(format!("`{host}` is not a TLS host: {e}"))
                })?;
            let (client, connection) = config.connect_raw(tcp, connector).await?;
            (client, hold(connection))
        } else {
            let (client, connection) = config.connect_raw(tcp, tokio_postgres::NoTls).await?;
            (client, hold(connection))
        };
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

/// The socket budget this connection string asks for, bounded by
/// [`crate::CONNECT_TIMEOUT`].
///
/// `CONNECT_TIMEOUT`'s own doc comment calls it "short enough that a pipeline
/// blocked by a firewall reports it while someone is still watching" — a
/// ceiling pbps enforces for its own reason, not a default the connection
/// string is free to raise. A smaller request is honoured, because it serves
/// that same reason even better: a pipeline behind a dropped connection learns
/// sooner. A larger one is refused by name, with the ceiling named in the
/// message, rather than silently capped — silent capping is exactly the shape
/// issue #113 exists to remove, applied to the one parameter that already had
/// a value here before the string was read.
fn connect_budget(config: &Config) -> Result<std::time::Duration, DbError> {
    match config.get_connect_timeout() {
        Some(requested) if *requested > crate::CONNECT_TIMEOUT => {
            Err(DbError::BadConnectionString(format!(
                "`connect_timeout={}` is longer than the {}s pbps waits for a \
                 TCP connection before giving up. That bound exists so a \
                 pipeline behind a dropped connection reports it while someone \
                 is still watching, and honouring a longer request would give \
                 that up silently. Ask for {}s or less.",
                requested.as_secs(),
                crate::CONNECT_TIMEOUT.as_secs(),
                crate::CONNECT_TIMEOUT.as_secs(),
            )))
        }
        // Whole seconds only, and `connect_timeout=0` or a negative value
        // never reaches this arm: `tokio_postgres::Config`'s own string parser
        // only calls its `connect_timeout` setter for a value greater than
        // zero, so `get_connect_timeout()` already reads `None` for either —
        // the same "unset" this function's `None` arm returns
        // `CONNECT_TIMEOUT` for. Nothing here needs to re-enforce that floor.
        Some(requested) => Ok(*requested),
        None => Ok(crate::CONNECT_TIMEOUT),
    }
}

/// Whether `load_balance_hosts` asks for the resolved addresses to be tried
/// in a random order rather than resolution order.
///
/// The one connection-string setting this seam turns into an address-ordering
/// choice rather than a socket option — see [`crate::open_socket`]'s
/// `shuffle` parameter, which this feeds.
fn wants_random_order(config: &Config) -> bool {
    config.get_load_balance_hosts() == tokio_postgres::config::LoadBalanceHosts::Random
}

/// The keepalive settings this connection string asks for, or `None` when
/// `keepalives=0` turns keepalive off and there is nothing to set.
///
/// Read from `Config`'s own accessors rather than through `tokio-postgres`'s
/// private `KeepaliveConfig`, but built the same way its `connect_socket`
/// builds one: `keepalives_idle` always has a value (2h by default), so it is
/// applied unconditionally once keepalive is on — there is no "the string
/// left it unset" to preserve, because the accessor cannot tell that case
/// apart from the default either. `keepalives_interval` and
/// `keepalives_retries` default to `None` and are applied only when the
/// string set them. The `#[cfg]` exclusions are copied from
/// `tokio-postgres`'s own `keepalive.rs` rather than invented — a platform set
/// either wider or narrower than the driver's would stop being parity with it.
fn keepalive_settings(config: &Config) -> Option<TcpKeepalive> {
    if !config.get_keepalives() {
        return None;
    }
    let mut keepalive = TcpKeepalive::new().with_time(config.get_keepalives_idle());

    #[cfg(not(any(
        target_os = "aix",
        target_os = "redox",
        target_os = "solaris",
        target_os = "openbsd"
    )))]
    if let Some(interval) = config.get_keepalives_interval() {
        keepalive = keepalive.with_interval(interval);
    }

    #[cfg(not(any(
        target_os = "aix",
        target_os = "redox",
        target_os = "solaris",
        target_os = "windows",
        target_os = "openbsd"
    )))]
    if let Some(retries) = config.get_keepalives_retries() {
        keepalive = keepalive.with_retries(retries);
    }

    Some(keepalive)
}

/// Whether `tcp_user_timeout` is honoured, refused by name, or asked for
/// nothing at all.
///
/// A pure function of `is_linux` rather than a bare `#[cfg]`, so the refusal
/// branch is exercised by a unit test on every platform CI runs it on and not
/// only the ones the real `#[cfg(target_os = "linux")]` at each call site
/// would otherwise compile it out of.
///
/// `TCP_USER_TIMEOUT` genuinely does not exist outside Linux — the same
/// narrower gate `tokio-postgres`'s own `connect_socket.rs` uses for it,
/// narrower than `socket2`'s own (which also allows Android, Fuchsia and
/// Cygwin). Refusing a request for it elsewhere by name, instead of silently
/// dropping it, is what issue #113 is about: this is the one of the seven
/// parameters where "honour here, refuse there" is coherent, because the
/// thing missing on the other platforms is the OS feature itself, not pbps's
/// support for it.
fn tcp_user_timeout_disposition(
    is_linux: bool,
    requested: Option<std::time::Duration>,
) -> Result<Option<std::time::Duration>, DbError> {
    match (is_linux, requested) {
        (true, requested) => Ok(requested),
        (false, None) => Ok(None),
        (false, Some(_)) => Err(DbError::BadConnectionString(
            "`tcp_user_timeout` sets `TCP_USER_TIMEOUT`, a Linux-only socket \
             option, and this build is not on Linux, so it cannot be \
             honoured. Remove it from the connection string."
                .to_owned(),
        )),
    }
}

/// Applies the socket-level settings `Config::connect` would have applied
/// through its own `connect_socket` — keepalive and `tcp_user_timeout` — and
/// which this seam's `connect_raw` call never reaches, because it is handed
/// an already-open socket instead of opening one itself (see the module
/// header and ADR-0014 §3).
///
/// Takes `&TcpStream` rather than ownership and runs between `open_socket`
/// and `connect_raw` — the same shape `set_nodelay` already uses a few lines
/// up, for the same reason: these are properties of the socket, not of the
/// PostgreSQL session that has not started yet.
///
/// `tcp_user_timeout` arrives already resolved by `connect`'s own call to
/// [`tcp_user_timeout_disposition`], made before `open_socket` so an
/// unsupported request is refused before any I/O rather than discovered
/// here, after a socket this function is now holding open (review of issue
/// #113 on PR #346). `Some` on a platform that cannot honour it would mean
/// `connect` let a refusal through, not something this function should try
/// to paper over.
fn apply_socket_options(
    tcp: &TcpStream,
    config: &Config,
    tcp_user_timeout: Option<std::time::Duration>,
    addr: &str,
) -> Result<(), DbError> {
    let sock = SockRef::from(tcp);

    if let Some(keepalive) = keepalive_settings(config) {
        sock.set_tcp_keepalive(&keepalive)
            .map_err(|source| DbError::Connect {
                addr: addr.to_owned(),
                source,
            })?;
    }

    if let Some(timeout) = tcp_user_timeout {
        #[cfg(target_os = "linux")]
        sock.set_tcp_user_timeout(Some(timeout))
            .map_err(|source| DbError::Connect {
                addr: addr.to_owned(),
                source,
            })?;
        #[cfg(not(target_os = "linux"))]
        unreachable!(
            "`connect` refuses `tcp_user_timeout` before opening a socket on \
             every platform that cannot honour it, so `Some` cannot reach \
             this function except on Linux: {timeout:?}"
        );
    }

    Ok(())
}

/// Whether this connection needs a TLS stack at all.
///
/// `sslmode=disable` is the one answer that needs none — and needing none is
/// not the same as having one that goes unused, because building one reads the
/// host's certificate store and fails where there is not one to read.
fn wants_tls(config: &Config) -> bool {
    config.get_ssl_mode() != tokio_postgres::config::SslMode::Disable
}

/// The protocols to advertise over ALPN, which is empty except for one case.
///
/// **Measured on PostgreSQL 18.6.** A direct SSL connection that offers no ALPN
/// is refused — `received direct SSL connection request without ALPN protocol
/// negotiation extension` in the server log — even though the TLS handshake
/// itself completes, so the failure arrives after it and reads as the
/// connection dropping. Neither `tokio-postgres` nor `tokio-postgres-rustls`
/// sets this, so without it `sslnegotiation=direct` could never connect at all.
///
/// Only for `Direct`: the `SSLRequest` negotiation the default uses asks for no
/// ALPN, and libpq offers none there either.
fn alpn(config: &Config) -> Vec<Vec<u8>> {
    if config.get_ssl_negotiation() == tokio_postgres::config::SslNegotiation::Direct {
        vec![b"postgresql".to_vec()]
    } else {
        Vec::new()
    }
}

/// Polls the connection future for as long as the client lives.
///
/// Held, not detached — see the module header. Generic because the future's
/// type carries the transport, and the transport is the one thing the two
/// branches of [`Conn::connect`] do not share.
fn hold<S, T>(connection: tokio_postgres::Connection<S, T>) -> tokio::task::JoinHandle<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        // The connection future resolves when the socket closes. Its error is
        // the socket's, and the next statement reports it with the context of
        // what was being run; logging it here would print a second failure
        // beside the real one.
        let _ = connection.await;
    })
}

/// The TLS stack, chosen here rather than pinned by the driver.
///
/// ADR-0014's Limits name this: the spike used `NoTls`, and a real dialect
/// needs a stack — which is where open question 10's supply-chain story
/// rejoins. `rustls` with the platform's own trust store, and the same `rustls`
/// the other driver already resolves, so the tree carries one.
fn tls(config: &Config) -> Result<tokio_postgres_rustls::MakeRustlsConnect, DbError> {
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
    let mut client = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| DbError::BadConnectionString(format!("this build has no usable TLS: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    client.alpn_protocols = alpn(config);
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(client))
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
        endpoint(&config_of(connection))
    }

    fn config_of(connection: &str) -> Config {
        connection.parse::<Config>().expect("a config parses")
    }

    /// A connection that has turned TLS off must not depend on a TLS stack.
    /// Building one reads the host's certificate store, so on a minimal image
    /// with none this was `sslmode=disable` refused over certificates it was
    /// never going to look at.
    #[test]
    fn only_a_connection_that_may_use_tls_asks_for_a_tls_stack() {
        assert!(!wants_tls(&config_of("host=db.example sslmode=disable")));
        assert!(wants_tls(&config_of("host=db.example sslmode=prefer")));
        assert!(wants_tls(&config_of("host=db.example sslmode=require")));
        // The default is `prefer`, so saying nothing still wants one.
        assert!(wants_tls(&config_of("host=db.example")));
    }

    /// ALPN is offered for direct SSL and for nothing else. Measured on
    /// PostgreSQL 18.6: a direct SSL connection offering none is refused —
    /// `received direct SSL connection request without ALPN protocol
    /// negotiation extension` — although the TLS handshake itself completes,
    /// so the failure arrives after it and reads as the connection dropping.
    #[test]
    fn alpn_is_offered_for_a_direct_ssl_connection_and_for_no_other() {
        assert_eq!(
            alpn(&config_of(
                "host=db.example sslmode=require sslnegotiation=direct"
            )),
            vec![b"postgresql".to_vec()]
        );
        assert!(
            alpn(&config_of("host=db.example sslmode=require")).is_empty(),
            "the SSLRequest negotiation asks for no ALPN, and libpq offers none"
        );
        assert!(alpn(&config_of("host=db.example sslmode=disable")).is_empty());
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

    /// `connect_timeout` below the ceiling is honoured exactly; saying
    /// nothing still gets `CONNECT_TIMEOUT`. Neither of these two is the shape
    /// this issue is about — the refusal below it.
    #[test]
    fn a_connect_timeout_within_the_ceiling_is_honoured_and_absent_is_the_ceiling() {
        assert_eq!(
            connect_budget(&config_of("host=db.example connect_timeout=5 user=u"))
                .expect("within the ceiling"),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            connect_budget(&config_of("host=db.example connect_timeout=30 user=u"))
                .expect("exactly the ceiling"),
            crate::CONNECT_TIMEOUT
        );
        assert_eq!(
            connect_budget(&config_of("host=db.example user=u")).expect("nothing asked"),
            crate::CONNECT_TIMEOUT
        );
    }

    /// A `connect_timeout` past the ceiling is refused by name, naming the
    /// ceiling — not silently capped. Silent capping is the shape a parameter
    /// neither applied nor refused belongs to (issue #113, PITFALLS), applied
    /// to the one parameter pbps already had an opinion about before the
    /// string was read.
    #[test]
    fn a_connect_timeout_past_the_ceiling_is_refused_and_names_it() {
        let error = connect_budget(&config_of("host=db.example connect_timeout=60 user=u"))
            .expect_err("60s exceeds the 30s ceiling");
        assert!(
            matches!(error, DbError::BadConnectionString(_)),
            "{error:?}"
        );
        assert!(error.to_string().contains("30"), "{error}");
        assert!(error.to_string().contains("connect_timeout"), "{error}");
    }

    /// `connect_timeout=0` and a negative value are `tokio_postgres::Config`'s
    /// own way of saying "unset" — its string parser never calls the setter
    /// for either — so both fall back to `CONNECT_TIMEOUT` exactly like saying
    /// nothing, rather than this seam inventing a zero-second budget or a
    /// floor of its own.
    #[test]
    fn connect_timeout_zero_or_negative_is_the_parsers_own_unset_and_falls_back() {
        assert_eq!(
            connect_budget(&config_of("host=db.example connect_timeout=0 user=u"))
                .expect("zero is unset"),
            crate::CONNECT_TIMEOUT
        );
        assert_eq!(
            connect_budget(&config_of("host=db.example connect_timeout=-5 user=u"))
                .expect("negative is unset"),
            crate::CONNECT_TIMEOUT
        );
    }

    /// `Random` is the only value that reorders; the default and an explicit
    /// `Disable` both leave resolution order alone.
    #[test]
    fn only_load_balance_hosts_random_asks_for_reordering() {
        assert!(wants_random_order(&config_of(
            "host=db.example load_balance_hosts=random user=u"
        )));
        assert!(!wants_random_order(&config_of(
            "host=db.example load_balance_hosts=disable user=u"
        )));
        assert!(!wants_random_order(&config_of("host=db.example user=u")));
    }

    /// `keepalives=0` turns keepalive off and there is nothing to set; the
    /// default (saying nothing) keeps it on, which is the gap this issue is
    /// about — the driver's default and pbps's used to disagree.
    #[test]
    fn keepalives_off_asks_for_nothing_and_the_default_asks_for_something() {
        assert!(
            keepalive_settings(&config_of("host=db.example keepalives=0 user=u")).is_none(),
            "keepalives=0 must set nothing rather than the OS default"
        );
        assert!(
            keepalive_settings(&config_of("host=db.example user=u")).is_some(),
            "the driver's own default is keepalives on"
        );
    }

    /// The refusal branch, proved directly rather than trusted from a
    /// `#[cfg]` this machine's CI may never compile the other side of:
    /// honoured on Linux, refused by name (mentioning why) everywhere else,
    /// and nothing asked for is never refused anywhere.
    #[test]
    fn tcp_user_timeout_is_honoured_on_linux_and_refused_by_name_elsewhere() {
        let requested = Some(std::time::Duration::from_secs(5));

        assert_eq!(
            tcp_user_timeout_disposition(true, requested).expect("linux honours it"),
            requested
        );
        assert_eq!(
            tcp_user_timeout_disposition(true, None).expect("linux, nothing asked"),
            None
        );
        assert_eq!(
            tcp_user_timeout_disposition(false, None).expect("nothing asked, nothing refused"),
            None
        );

        let error = tcp_user_timeout_disposition(false, requested)
            .expect_err("a non-Linux target cannot honour TCP_USER_TIMEOUT");
        assert!(
            matches!(error, DbError::BadConnectionString(_)),
            "{error:?}"
        );
        assert!(error.to_string().contains("Linux"), "{error}");
        assert!(error.to_string().contains("tcp_user_timeout"), "{error}");
    }

    /// The ordering fix itself: a platform that cannot honour
    /// `tcp_user_timeout` refuses it before `open_socket` runs, not after —
    /// a review finding on PR #346 caught an earlier draft doing the second
    /// one, which meant an unsupported option on an unreachable endpoint was
    /// reported as a network failure instead of the promised named
    /// configuration refusal.
    ///
    /// `is_linux: false` regardless of the platform actually running this
    /// test — the point is the *ordering*, which `connect_as` lets a test
    /// pin without needing to run on the other platform for real. The host
    /// is `.invalid` (RFC 2606): never resolved, because nothing here
    /// reaches far enough to resolve it if the refusal runs where it
    /// should.
    #[tokio::test]
    async fn a_tcp_user_timeout_the_platform_cannot_honour_is_refused_before_any_socket_opens() {
        let started = std::time::Instant::now();
        // A `match` rather than `expect_err`, because `Conn` has no `Debug` —
        // a connection is not a value to print (the same reason
        // `crates/pbps-pg/tests/live.rs`'s `refusal` helper does the same).
        let error = match Conn::connect_as(
            "host=nowhere.invalid port=5432 user=u tcp_user_timeout=5",
            false,
        )
        .await
        {
            Ok(_) => panic!("this connection string must be refused, not connected"),
            Err(error) => error,
        };
        let elapsed = started.elapsed();

        assert!(
            matches!(error, DbError::BadConnectionString(_)),
            "{error:?}"
        );
        assert!(error.to_string().contains("tcp_user_timeout"), "{error}");
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "a refusal that runs before any socket opens cannot be waiting on \
             anything network-shaped: took {elapsed:?}"
        );
    }

    /// The negative case beside the one above: with no `tcp_user_timeout` in
    /// the string, the same unreachable endpoint still reaches `open_socket`
    /// and fails as an ordinary network error — proving the ordering fix
    /// refuses only what it is supposed to, rather than swallowing a genuine
    /// `Connect`/`ConnectTimeout` into a configuration refusal it was never
    /// meant to produce.
    ///
    /// The endpoint is a real, bound-then-dropped local listener rather than
    /// a black hole: nothing is listening on it by the time this dials, so
    /// the OS refuses the connection immediately instead of dropping it,
    /// which proves the point just as well without a thirty-second wait.
    #[tokio::test]
    async fn the_same_unreachable_endpoint_without_tcp_user_timeout_still_fails_as_a_network_error()
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let addr = listener.local_addr().expect("the bound address");
        drop(listener);

        let error = match Conn::connect_as(
            &format!("host={} port={} user=u", addr.ip(), addr.port()),
            false,
        )
        .await
        {
            Ok(_) => panic!("nothing is listening on this port anymore"),
            Err(error) => error,
        };

        assert!(
            matches!(
                error,
                DbError::Connect { .. } | DbError::ConnectTimeout { .. }
            ),
            "an ordinary network failure must still be one, not a configuration \
             refusal the ordering fix was never meant to produce: {error:?}"
        );
    }

    /// A connected loopback pair, so [`apply_socket_options`] has a real
    /// socket to act on without needing a live PostgreSQL server — these
    /// assertions are about what the OS did, not about the protocol.
    ///
    /// `#[cfg(target_os = "linux")]` for the same reason as its one caller,
    /// `keepalive_settings_land_on_the_real_socket_not_only_the_parsed_config`
    /// below: gated out on every other platform, so an ungated helper would
    /// compile with no caller there and fail `-D warnings`' `dead_code` lint
    /// (issue #113 review).
    #[cfg(target_os = "linux")]
    async fn connected_pair() -> (TcpStream, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let addr = listener.local_addr().expect("the bound address");
        let client = TcpStream::connect(addr).await.expect("connect");
        (client, listener)
    }

    /// The socket's own keepalive state after `apply_socket_options` — read
    /// back through `SockRef`, not through the `Config` that was just parsed.
    /// A test that only checked the parsed config would prove that parsing
    /// works and nothing about whether anything reached the OS, which is
    /// exactly the gap issue #113 is about.
    ///
    /// Positive: `keepalives=1` with an explicit idle turns `SO_KEEPALIVE` on
    /// and sets the idle time asked for. Negative: `keepalives=0` must leave
    /// the socket at the OS default (off on Linux) rather than at the
    /// driver's own default (on) — the exact conflation this issue exists to
    /// remove.
    ///
    /// `#[cfg(target_os = "linux")]` because `tcp_keepalive_time`'s getter is
    /// not available on every platform (`socket2`'s own gate excludes
    /// Windows), not because the setting itself is Linux-only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn keepalive_settings_land_on_the_real_socket_not_only_the_parsed_config() {
        let (client, _listener) = connected_pair().await;
        let on = config_of("host=db.example keepalives=1 keepalives_idle=45 user=u");
        apply_socket_options(&client, &on, None, "test").expect("keepalive settings apply");
        let sock = SockRef::from(&client);
        assert!(
            sock.keepalive().expect("read keepalive back"),
            "keepalives=1 must turn SO_KEEPALIVE on"
        );
        assert_eq!(
            sock.tcp_keepalive_time().expect("read the idle time back"),
            std::time::Duration::from_secs(45),
            "keepalives_idle must reach the real socket, not only the parsed config"
        );

        let (client_off, _listener_off) = connected_pair().await;
        let off = config_of("host=db.example keepalives=0 user=u");
        apply_socket_options(&client_off, &off, None, "test")
            .expect("keepalives=0 applies cleanly");
        let sock_off = SockRef::from(&client_off);
        assert!(
            !sock_off.keepalive().expect("read keepalive back"),
            "keepalives=0 must leave the OS default off, not the driver's own \
             default of on"
        );
    }
}
