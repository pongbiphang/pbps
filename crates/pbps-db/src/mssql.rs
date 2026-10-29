//! The SQL Server driver, and the only file in this crate that names
//! `tiberius`.
//!
//! ADR-0007 decision 5 built the seam so that `tiberius` was named in exactly
//! one file. With a second driver the rule becomes **one file per driver**:
//! nothing outside this module and its PostgreSQL sibling names either one, and
//! `lib.rs` dispatches between them without knowing what is underneath
//! (DECISIONS 225).

use tiberius::{Client, Config};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::{DbError, Param};

/// Gives the session this process's application name, unless the connection
/// string names one under either key the driver reads (#1188).
///
/// The driver's parsed `Config` does not say whether the name came from the
/// string, so the options this module already read are asked.
fn name_session(config: &mut Config, options: &Options) {
    if options.get("application name").is_none() && options.get("applicationname").is_none() {
        config.application_name(crate::session_application_name());
    }
}

/// The options of an ADO.NET connection string, read exactly as the driver
/// reads them (#2271).
///
/// Admission decides what the connection is from these, so they must be the
/// options the driver would act on. Two parsers that accept the same string
/// can still disagree about it (`Description=x{;Integrated Security=SSPI;a=}`
/// hides the setting from a parser that opens a brace anywhere, and shows it
/// to one that opens a brace only at the start of a value), and the one that
/// is wrong about it decides to send the Windows identity over TCP. The driver
/// exposes no accessor for its parsed options, so this is a port of its
/// reader (`tiberius` 0.13.0, pinned exactly) and the driver is never handed
/// the caller's text: it is handed [`Options::canonical`], which any reader
/// of this grammar must read as these options. A disagreement between this
/// port and the driver therefore changes what a string means, never what was
/// admitted; `driver_and_port_read_the_same_options` pins that they agree.
#[derive(Debug, PartialEq, Eq)]
struct Options(std::collections::BTreeMap<String, String>);

impl Options {
    /// Keys are lower-cased and trimmed, the last duplicate wins, and a value
    /// is unquoted, `'...'` or `"..."` (a doubled quote is a literal one) or
    /// `{...}` (to the first `}`), where the quote or brace counts only as the
    /// first character of a value. Non-ASCII text is refused up front, as the
    /// driver does.
    fn parse(input: &str) -> Result<Self, &'static str> {
        if !input.is_ascii() {
            return Err("the connection string contains non-ASCII characters");
        }
        let b = input.as_bytes();
        let n = b.len();
        let mut i = 0;
        let mut pairs = std::collections::BTreeMap::new();
        while i < n {
            while i < n && (b[i] == b';' || b[i].is_ascii_whitespace()) {
                i += 1;
            }
            if i >= n {
                break;
            }
            // The key runs to the first single `=`; a `==` is a literal `=`.
            let mut key = String::new();
            loop {
                if i >= n || b[i] == b';' {
                    return Err("key-value pairs must be joined by a `=`");
                }
                if b[i] == b'=' {
                    if i + 1 < n && b[i + 1] == b'=' {
                        key.push('=');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                key.push(char::from(b[i]));
                i += 1;
            }
            let key = key
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_ascii_lowercase();
            if key.is_empty() {
                return Err("a key in the connection string is empty");
            }
            while i < n && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < n && (b[i] == b'\'' || b[i] == b'"') {
                let quote = b[i];
                i += 1;
                let mut v = String::new();
                loop {
                    if i >= n {
                        return Err("the connection string has an unclosed quote");
                    }
                    if b[i] == quote {
                        if i + 1 < n && b[i + 1] == quote {
                            v.push(char::from(quote));
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    v.push(char::from(b[i]));
                    i += 1;
                }
                skip_trailing(b, &mut i)?;
                v
            } else if i < n && b[i] == b'{' {
                i += 1;
                let start = i;
                while i < n && b[i] != b'}' {
                    i += 1;
                }
                if i >= n {
                    return Err("the connection string has an unclosed brace");
                }
                let v = input[start..i].to_owned();
                i += 1;
                skip_trailing(b, &mut i)?;
                v
            } else {
                let start = i;
                while i < n && b[i] != b';' {
                    i += 1;
                }
                input[start..i]
                    .trim_end_matches(|c: char| c.is_ascii_whitespace())
                    .to_owned()
            };
            pairs.insert(key, value);
            if i < n && b[i] == b';' {
                i += 1;
            }
        }
        Ok(Self(pairs))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// These options in the one spelling that has no second reading: every
    /// value single-quoted, so it is read as a value whatever it holds, and
    /// every `=` in a key doubled.
    fn canonical(&self) -> String {
        self.0
            .iter()
            .map(|(key, value)| {
                format!("{}='{}'", key.replace('=', "=="), value.replace('\'', "''"))
            })
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// After a closing quote or brace only whitespace may precede the next `;`.
fn skip_trailing(b: &[u8], i: &mut usize) -> Result<(), &'static str> {
    while *i < b.len() && b[*i] != b';' {
        if !b[*i].is_ascii_whitespace() {
            return Err("unexpected characters after a quoted value");
        }
        *i += 1;
    }
    Ok(())
}

/// The transport a connection string asks for, decided before anything opens
/// (#2271).
///
/// The driver defaults silently: a server it cannot read as a host becomes a
/// host name, and an `Integrated Security` it was not built to honour becomes
/// SQL authentication with an empty login. Both would turn "use this pipe, as
/// this Windows user" into a different connection, so the string is admitted
/// here first and anything that is not exactly one of the two supported
/// shapes is refused by name. No refusal falls back to the other transport or
/// to the other kind of authentication (DEC-2271.1).
#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    /// A host and port, with SQL authentication.
    Tcp,
    /// A local named pipe (`\\.\pipe\...`), with the current Windows user's
    /// integrated authentication. This is the only way LocalDB accepts a
    /// connection.
    Pipe(String),
}

const SERVER_KEYS: [&str; 2] = ["server", "data source"];
/// Every spelling of "use Windows authentication" that ADO.NET knows. The
/// driver reads only the first two.
const INTEGRATED_KEYS: [&str; 3] = [
    "integrated security",
    "integratedsecurity",
    "trusted_connection",
];
const CREDENTIAL_KEYS: [&str; 6] = ["uid", "username", "user", "user id", "password", "pwd"];
fn refuse(reason: &str) -> DbError {
    DbError::Refused(reason.to_owned())
}

impl Endpoint {
    /// `windows` is a parameter so the Windows rules are testable anywhere;
    /// production passes `cfg!(windows)`.
    fn admit(options: &Options, windows: bool) -> Result<Self, DbError> {
        let value = |keys: &[&str]| keys.iter().find_map(|key| options.get(key));

        let pipe = match value(&SERVER_KEYS).map(|server| server.trim()) {
            Some(server) if server.to_ascii_lowercase().contains("(localdb)") => {
                return Err(refuse(
                    "a LocalDB `(localdb)` selector is not a driver endpoint; \
                     connect to the instance's named pipe with `Server=np:\\\\.\\pipe\\<name>`",
                ));
            }
            Some(server)
                if server
                    .get(..3)
                    .is_some_and(|p| p.eq_ignore_ascii_case("np:")) =>
            {
                Some(local_pipe(&server[3..])?)
            }
            Some(server) if server.starts_with("\\\\") => {
                return Err(refuse(
                    "a pipe server must be written `np:\\\\.\\pipe\\<name>`",
                ));
            }
            _ => None,
        };

        let spelled: Vec<&str> = INTEGRATED_KEYS
            .into_iter()
            .filter(|key| options.contains_key(key))
            .collect();
        let integrated = match spelled[..] {
            [] => false,
            [key] => match options
                .get(key)
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "true" | "yes" | "sspi" => true,
                "false" | "no" => false,
                _ => {
                    return Err(refuse(
                        "`Integrated Security` is not true, false, yes, no or SSPI",
                    ));
                }
            },
            // The driver reads the first spelling it finds, so two that
            // disagree would be read as SQL authentication.
            _ => {
                return Err(refuse(
                    "the connection string spells `Integrated Security` more than once",
                ));
            }
        };

        if pipe.is_some() && !windows {
            return Err(refuse(
                "a named-pipe connection is only available on Windows",
            ));
        }
        if integrated && CREDENTIAL_KEYS.iter().any(|key| options.contains_key(key)) {
            return Err(refuse(
                "`Integrated Security` uses the current Windows user and cannot be \
                 combined with a user name or password",
            ));
        }
        match (pipe, integrated) {
            (None, false) => Ok(Self::Tcp),
            (None, true) => Err(refuse(
                "integrated authentication is only supported over a local named pipe, \
                 `Server=np:\\\\.\\pipe\\<name>`",
            )),
            (Some(_), false) => Err(refuse(
                "a named pipe requires `Integrated Security=SSPI`; \
                 SQL authentication is not offered over a pipe",
            )),
            (Some(path), true) => {
                if spelled == ["trusted_connection"] {
                    return Err(refuse(
                        "spell it `Integrated Security=SSPI`; the driver does not read `Trusted_Connection`",
                    ));
                }
                Ok(Self::Pipe(path))
            }
        }
    }
}

/// `\\.\pipe\<segments>` on this machine, and nothing else.
///
/// A remote pipe would send the Windows identity to another host, and a
/// segment such as `..` would leave the pipe namespace, so the path is spelled
/// out from a short alphabet rather than screened for known-bad pieces.
fn local_pipe(path: &str) -> Result<String, DbError> {
    const PREFIX: &str = "\\\\.\\pipe\\";
    let name = path
        .get(..PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        .map(|_| &path[PREFIX.len()..])
        .ok_or_else(|| refuse("only a local pipe, `np:\\\\.\\pipe\\<name>`, is supported"))?;
    let allowed = |c: char| c.is_ascii_alphanumeric() || "$#_-.@~".contains(c);
    let ok = name.split('\\').all(|segment| {
        !segment.is_empty() && !segment.chars().all(|c| c == '.') && segment.chars().all(allowed)
    });
    if !ok {
        return Err(refuse(
            "the pipe name must be non-empty segments of letters, digits and `$#_-.@~`",
        ));
    }
    Ok(path.to_owned())
}

/// A `Server=(localdb)\<instance>` selector: the instance an operator named,
/// before anything has said where its pipe is (#2271).
///
/// The driver cannot open it and refuses it by name. Resolving it is not the
/// driver's job either (LocalDB discovery belongs to `pbps-mssql`), but
/// rewriting the string is, because only this module reads a connection string
/// the way the driver does. So the caller learns the instance from
/// [`LocalDbSelector::instance`], finds the pipe, and gets the string back from
/// [`LocalDbSelector::with_pipe`]; the result is an ordinary pipe string that
/// [`Conn::connect`] admits like any other, so a selector never skips a
/// check.
pub struct LocalDbSelector {
    instance: String,
    options: Options,
}

/// The instance only: the other settings may hold a password.
impl std::fmt::Debug for LocalDbSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalDbSelector")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

/// A stand-in for the pipe a selector will be given: only its shape matters
/// to the admission it is run through.
const PLACEHOLDER_PIPE: &str = "np:\\\\.\\pipe\\LOCALDB#00000000\\tsql\\query";

impl LocalDbSelector {
    /// `Ok(None)` is a string that names no `(localdb)` server, or one this
    /// module cannot read; opening it reports the latter by the driver's own
    /// words. An error is a LocalDB request that cannot be honoured: a form
    /// other than `(localdb)\<name>`, or a setting that would make the
    /// resolved connection something other than a pipe with integrated
    /// authentication. Both are decided before any helper runs.
    pub fn read(connection_string: &str) -> Result<Option<Self>, DbError> {
        Self::read_on(connection_string, cfg!(windows))
    }

    fn read_on(connection_string: &str, windows: bool) -> Result<Option<Self>, DbError> {
        let Ok(options) = Options::parse(connection_string) else {
            return Ok(None);
        };
        let Some(server) = SERVER_KEYS.iter().find_map(|key| options.get(key)) else {
            return Ok(None);
        };
        let server = server.trim();
        if !server.to_ascii_lowercase().contains("(localdb)") {
            return Ok(None);
        }
        const PREFIX: &str = "(localdb)\\";
        let instance = server
            .get(..PREFIX.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
            .map(|_| &server[PREFIX.len()..])
            .filter(|name| {
                (1..=128).contains(&name.len())
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .ok_or_else(|| {
                refuse(
                    "only a named LocalDB instance is supported, `Server=(localdb)\\<name>` with \
                     letters, digits and `_`; the shared, default and versioned forms are not",
                )
            })?;
        let selector = Self {
            instance: instance.to_owned(),
            options,
        };
        Endpoint::admit(&selector.rewritten(PLACEHOLDER_PIPE)?, windows)?;
        Ok(Some(selector))
    }

    /// The instance the connection string names.
    #[must_use]
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The connection string with `pipe` (`np:\\.\pipe\...`) as its server and
    /// every other setting as it was. The pipe is held to the same local-pipe
    /// rule as one the operator wrote.
    pub fn with_pipe(&self, pipe: &str) -> Result<String, DbError> {
        Ok(self.rewritten(pipe)?.canonical())
    }

    fn rewritten(&self, pipe: &str) -> Result<Options, DbError> {
        let path = pipe
            .get(..3)
            .filter(|prefix| prefix.eq_ignore_ascii_case("np:"))
            .map(|_| &pipe[3..])
            .ok_or_else(|| refuse("a LocalDB pipe must be written `np:\\\\.\\pipe\\<name>`"))?;
        local_pipe(path)?;
        let mut pairs = self.options.0.clone();
        for key in SERVER_KEYS {
            pairs.remove(key);
        }
        pairs.insert("server".to_owned(), pipe.to_owned());
        Ok(Options(pairs))
    }
}

/// One row as this driver hands it back.
pub struct Row(tiberius::Row);

/// An open SQL Server connection.
pub struct Conn {
    client: Client<Compat<crate::transport::BoxedStream>>,
}

impl From<tiberius::error::Error> for DbError {
    /// The driver's error, flattened to text and a code.
    ///
    /// Flattened here rather than carried, because `DbError` is the seam's
    /// error and a variant holding one driver's type would put that driver's
    /// name back in every signature the seam exists to keep it out of. The
    /// code is text for the reason `server_error_code` states.
    ///
    /// `e.to_string()`, unlike the PostgreSQL side, already carries the
    /// server's own sentence: issue #167 asked this shape be checked here too
    /// before it closed, and **measured** on 17.0.4075.5, `tiberius::Error`'s
    /// `Display` for `Error::Server` is `TokenError`'s own `Display`, which
    /// interpolates its `message` field straight in — there is no `Kind::Db`
    /// standing in for it the way `tokio_postgres::Error` has one. Pinned by
    /// the live suite alongside the PostgreSQL fix, as a regression guard
    /// rather than a second fix (DECISIONS 454).
    fn from(e: tiberius::error::Error) -> Self {
        DbError::Driver {
            diagnostic: None,
            code: e.code().map(|c| c.to_string()),
            message: e.to_string(),
        }
    }
}

impl Conn {
    /// Connects using an ADO.NET-style connection string
    /// (`Server=host,1433;Database=x;User Id=u;Password=p;` — the form every
    /// SQL Server tool already asks for, so users can paste what they have).
    ///
    /// `Server=np:\\.\pipe\<name>;Integrated Security=SSPI` connects over a
    /// local named pipe as the current Windows user instead (#2271); see
    /// [`Endpoint`] for what is refused.
    pub(crate) async fn connect(connection_string: &str) -> Result<Self, DbError> {
        let options = match Options::parse(connection_string) {
            Ok(options) => options,
            // The driver's own message, with its quoting hint, when it agrees.
            // If it does not, the two read this string differently and
            // nothing may be admitted from it.
            Err(_) => {
                return Err(match Config::from_ado_string(connection_string) {
                    Err(e) => DbError::BadConnectionString(e.to_string()),
                    Ok(_) => refuse("the connection string is read differently by the driver"),
                });
            }
        };
        let endpoint = Endpoint::admit(&options, cfg!(windows))?;
        // The driver reads the canonical spelling, never the caller's text.
        let mut config = Config::from_ado_string(&options.canonical())
            .map_err(|e| DbError::BadConnectionString(e.to_string()))?;
        name_session(&mut config, &options);
        match endpoint {
            Endpoint::Tcp => Self::connect_config(config).await,
            Endpoint::Pipe(path) => {
                // The driver read the pipe as a host name. Nothing dials it,
                // but TLS verifies the server certificate against the host, so
                // it is set to a real name; `HostNameInCertificate` still
                // overrides it.
                config.host("localhost");
                Self::connect_pipe(config, &path, crate::CONNECT_TIMEOUT).await
            }
        }
    }

    async fn connect_config(config: Config) -> Result<Self, DbError> {
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
        //
        // `CONNECT_TIMEOUT` unconditionally: an ADO.NET connection string has
        // no `connect_timeout` of its own to honour or refuse (issue #113;
        // `tiberius` 0.13.0's `Config::from_ado_string` parses no such key), and
        // `false` for the same reason — no `load_balance_hosts` equivalent
        // either, so resolution order is never reshuffled here.
        let tcp = crate::open_socket(&addr, crate::CONNECT_TIMEOUT, false).await?;
        tcp.set_nodelay(true).map_err(|reason| DbError::Connect {
            addr: config.get_addr().to_owned(),
            reason,
        })?;
        Self::handshake(config, Box::new(tcp)).await
    }

    async fn connect_pipe(
        config: Config,
        path: &str,
        budget: std::time::Duration,
    ) -> Result<Self, DbError> {
        Self::handshake(config, open_pipe(path, budget).await?).await
    }

    async fn handshake(
        mut config: Config,
        stream: crate::transport::BoxedStream,
    ) -> Result<Self, DbError> {
        // A 30-second per-response timeout was added in tiberius 0.13. Long
        // DDL and lock waits were unbounded before this driver switch. Keep
        // that deployment behavior explicit; the transport and handshake
        // bounds remain independent of statement execution (DEC-861.1).
        config.command_timeout(None);
        let client = Client::connect(config, stream.compat_write()).await?;
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

/// Opens the local pipe, waiting out a busy one.
///
/// A pipe whose instances are all in use refuses with `ERROR_PIPE_BUSY` until a
/// server instance is free, which is the documented cue to try again; any
/// other failure, a missing pipe included, is final. The wait shares one
/// `budget` so a pipe that stays busy ends like an address that stays silent.
#[cfg(windows)]
async fn open_pipe(
    path: &str,
    budget: std::time::Duration,
) -> Result<crate::transport::BoxedStream, DbError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let attempts = async {
        loop {
            match ClientOptions::new().open(path) {
                Ok(pipe) => return Ok(pipe),
                Err(busy) if busy.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(reason) => {
                    return Err(DbError::Connect {
                        addr: path.to_owned(),
                        reason,
                    });
                }
            }
        }
    };
    match tokio::time::timeout(budget, attempts).await {
        Ok(opened) => Ok(Box::new(opened?)),
        Err(_) => Err(DbError::ConnectTimeout {
            addr: path.to_owned(),
            after: budget,
        }),
    }
}

/// [`Endpoint::admit`] refuses a pipe off Windows first; this is the same
/// refusal for a caller that reaches the opener some other way.
#[cfg(not(windows))]
async fn open_pipe(
    _path: &str,
    _budget: std::time::Duration,
) -> Result<crate::transport::BoxedStream, DbError> {
    Err(refuse(
        "a named-pipe connection is only available on Windows",
    ))
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

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[tokio::test]
    async fn a_stalled_handshake_is_reported_as_a_driver_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });
        let mut config = Config::from_ado_string(&format!(
            "Server=127.0.0.1,{port};User Id=synthetic;Password=marker;Encrypt=true"
        ))
        .unwrap();
        config.handshake_timeout(Some(std::time::Duration::from_millis(100)));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            Conn::connect_config(config),
        )
        .await
        .expect("the driver handshake must terminate within its bound");
        server.abort();
        match result {
            Err(DbError::Driver { message, .. }) => {
                assert!(message.contains("connection handshake"), "{message}");
                assert!(
                    message.contains("did not complete within 100ms"),
                    "{message}"
                );
            }
            Err(other) => panic!("expected a driver timeout, got: {other}"),
            Ok(_) => panic!("a stalled handshake must not connect"),
        }
    }

    /// #861: a string naming no `Encrypt` requires encryption, and a server
    /// that declines it in PRELOGIN never receives a plaintext LOGIN7.
    #[tokio::test]
    async fn tds_encryption_refusal_cannot_elicit_a_plaintext_login() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = [0; 8];
            socket.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], 0x12); // PRELOGIN, never LOGIN7.
            let mut payload = vec![0; usize::from(u16::from_be_bytes([header[2], header[3]])) - 8];
            socket.read_exact(&mut payload).await.unwrap();
            // A PRELOGIN reply with ENCRYPT_NOT_SUP. The client may reject
            // immediately or insist on TLS, but must never send LOGIN7.
            socket
                .write_all(&[0x04, 1, 0, 15, 0, 0, 1, 0, 1, 0, 6, 0, 1, 0xff, 2])
                .await
                .unwrap();
            let mut next = [0];
            if socket.read(&mut next).await.unwrap() != 0 {
                assert_eq!(
                    next[0], 0x12,
                    "plaintext authentication followed TLS refusal"
                );
            }
        });
        assert!(
            Conn::connect(&format!(
                "Server=localhost,{port};User Id=synthetic;Password=marker"
            ),)
            .await
            .is_err()
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    const PIPE: &str = r"\\.\pipe\LOCALDB#8A1B2C3D\tsql\query";

    fn admit(connection_string: &str, windows: bool) -> Result<Endpoint, String> {
        let options = Options::parse(connection_string).expect("a readable string");
        Endpoint::admit(&options, windows).map_err(|e| e.to_string())
    }

    fn refusal(connection_string: &str, windows: bool) -> String {
        let options = Options::parse(connection_string).expect("a readable string");
        match Endpoint::admit(&options, windows) {
            Err(DbError::Refused(reason)) => reason,
            other => panic!("expected a refusal for `{connection_string}`, got {other:?}"),
        }
    }

    #[test]
    fn a_host_with_sql_authentication_stays_tcp_on_every_platform() {
        for windows in [false, true] {
            for string in [
                "Server=tcp:db.example,1433;User Id=u;Password=p",
                "Data Source=db.example\\inst;uid=u;pwd=p;Encrypt=true",
                "Server=db;Integrated Security=false;User Id=u;Password=p",
                "Server=db;IntegratedSecurity=no;User Id=u;Password=p",
                "Server=np-host.example,1433;User Id=u;Password=p",
            ] {
                assert_eq!(admit(string, windows), Ok(Endpoint::Tcp), "{string}");
            }
        }
    }

    #[test]
    fn a_local_pipe_with_integrated_authentication_is_admitted_on_windows() {
        for string in [
            format!("Server=np:{PIPE};Integrated Security=SSPI"),
            format!("server=NP:{PIPE};IntegratedSecurity=true;Encrypt=false"),
            format!("Data Source=np:{PIPE};Integrated Security=yes;Database=scratch"),
            r"Server=np:\\.\PIPE\sql\query;Integrated Security=sspi".to_owned(),
            r"Server=np:\\.\pipe\MSSQL$SQLEXPRESS\sql\query;Integrated Security=SSPI".to_owned(),
        ] {
            assert!(
                matches!(admit(&string, true), Ok(Endpoint::Pipe(_))),
                "{string}"
            );
        }
        let admitted = admit(&format!("Server=np:{PIPE};Integrated Security=SSPI"), true);
        assert_eq!(admitted, Ok(Endpoint::Pipe(PIPE.to_owned())));
    }

    #[test]
    fn a_pipe_request_is_refused_off_windows_without_reaching_a_host() {
        let reason = refusal(&format!("Server=np:{PIPE};Integrated Security=SSPI"), false);
        assert!(reason.contains("only available on Windows"), "{reason}");
        // Windows-only is decided before authentication is looked at, so the
        // refusal names the platform even for a string that is also wrong.
        let reason = refusal(&format!("Server=np:{PIPE};User Id=u;Password=p"), false);
        assert!(reason.contains("only available on Windows"), "{reason}");
    }

    #[test]
    fn a_pipe_that_is_not_a_local_pipe_is_refused() {
        for server in [
            r"np:\\remote\pipe\sql\query",
            r"np:\\.\mailslot\x",
            r"np:\\?\pipe\x",
            r"np:pipe\sql\query",
            r"np:\\.\pipe\\",
            r"np:\\.\pipe\a\..\b",
            r"np:\\.\pipe\..\b",
            r"np:\\.\pipe\a\.\b",
            r"np:\\.\pipe\a\\b",
            r"np:\\.\pipe\a/b",
            r"np:\\.\pipe\a,1433",
            r"np:\\.\pipe\a b",
            r"np:\\.\pipe\",
            r"np:",
        ] {
            let string = format!("Server={server};Integrated Security=SSPI");
            let reason = refusal(&string, true);
            assert!(reason.contains("pipe"), "{server}: {reason}");
        }
    }

    #[test]
    fn a_pipe_must_be_spelled_with_its_protocol_prefix() {
        let reason = refusal(r"Server=\\.\pipe\sql\query;Integrated Security=SSPI", true);
        assert!(reason.contains("np:"), "{reason}");
    }

    #[test]
    fn a_localdb_selector_is_not_a_driver_endpoint() {
        for server in [
            r"(localdb)\MSSQLLocalDB",
            r"(LocalDB)\.\Shared",
            r"np:(localdb)\x",
            r"tcp:(localdb)\x",
        ] {
            for windows in [false, true] {
                let reason = refusal(
                    &format!("Server={server};Integrated Security=SSPI"),
                    windows,
                );
                assert!(reason.contains("LocalDB"), "{server}: {reason}");
            }
        }
    }

    #[test]
    fn sql_credentials_cannot_accompany_integrated_authentication() {
        for credential in [
            "uid=u",
            "User Id=u",
            "username=u",
            "user=u",
            "Password=p",
            "pwd=p",
            "pwd=",
        ] {
            let string = format!("Server=np:{PIPE};Integrated Security=SSPI;{credential}");
            let reason = refusal(&string, true);
            assert!(
                reason.contains("cannot be combined"),
                "{credential}: {reason}"
            );
        }
    }

    #[test]
    fn a_refusal_never_repeats_a_secret_from_the_string() {
        let string = format!(
            "Server=np:{PIPE};Integrated Security=SSPI;User Id=svc-account;Password=hunter2"
        );
        let reason = refusal(&string, true);
        assert!(
            !reason.contains("hunter2") && !reason.contains("svc-account"),
            "{reason}"
        );
    }

    #[test]
    fn a_pipe_without_integrated_authentication_is_refused() {
        for auth in [
            "",
            ";Integrated Security=false",
            ";Integrated Security=no",
            ";User Id=u;Password=p",
        ] {
            let reason = refusal(&format!("Server=np:{PIPE}{auth}"), true);
            assert!(
                reason.contains("requires `Integrated Security=SSPI`"),
                "{auth}: {reason}"
            );
        }
    }

    #[test]
    fn integrated_authentication_over_tcp_is_refused_rather_than_read_as_sql() {
        for auth in [
            "Integrated Security=SSPI",
            "IntegratedSecurity=true",
            "Integrated Security=yes",
            "Trusted_Connection=yes",
        ] {
            for windows in [false, true] {
                let reason = refusal(&format!("Server=db.example,1433;{auth}"), windows);
                assert!(
                    reason.contains("only supported over a local named pipe"),
                    "{auth}: {reason}"
                );
            }
        }
    }

    #[test]
    fn an_integrated_setting_the_driver_would_misread_is_refused() {
        // Two spellings: the driver reads one of them and ignores the other.
        let reason = refusal(
            &format!("Server=np:{PIPE};IntegratedSecurity=false;Integrated Security=SSPI"),
            true,
        );
        assert!(reason.contains("more than once"), "{reason}");
        // An unreadable value is neither true nor false.
        for value in ["maybe", "", "1"] {
            let reason = refusal(&format!("Server=db;Integrated Security={value}"), true);
            assert!(
                reason.contains("not true, false, yes, no or SSPI"),
                "{value:?}: {reason}"
            );
        }
        // The driver does not read this spelling, so on a pipe it would
        // silently authenticate with SQL authentication.
        let reason = refusal(&format!("Server=np:{PIPE};Trusted_Connection=yes"), true);
        assert!(reason.contains("Trusted_Connection"), "{reason}");
    }

    /// The class behind a review finding: a parser that opens a quote or a
    /// brace anywhere in a value hides, from admission, a setting the driver
    /// reads as its own. Each of these was admitted as plain TCP or as a
    /// credential-free pipe when admission used a different parser.
    #[test]
    fn a_quote_or_brace_inside_a_value_cannot_hide_a_setting_from_admission() {
        for windows in [false, true] {
            for hidden in [
                "Description=x{;Integrated Security=SSPI;ignored=}",
                "Description=x';Integrated Security=SSPI;a=b'",
                "Description=x\";Integrated Security=SSPI;a=b\"",
                "Description=a{b;Trusted_Connection=yes;c=}",
            ] {
                let reason = refusal(&format!("Server=db.example;{hidden}"), windows);
                assert!(
                    reason.contains("only supported over a local named pipe"),
                    "{hidden}: {reason}"
                );
            }
        }
        for hidden in [
            "Description=x{;User Id=u;Password=p;ignored=}",
            "Description=x';pwd=p;a=b'",
            "Description=x\";uid=u;a=b\"",
        ] {
            let reason = refusal(
                &format!("Server=np:{PIPE};Integrated Security=SSPI;{hidden}"),
                true,
            );
            assert!(reason.contains("cannot be combined"), "{hidden}: {reason}");
        }
        let reason = refusal(
            &format!("Description=x{{;Server=np:{PIPE};a=}};Integrated Security=SSPI"),
            false,
        );
        assert!(reason.contains("only available on Windows"), "{reason}");
    }

    #[test]
    fn a_setting_inside_a_properly_quoted_value_is_only_text() {
        for quoted in [
            "Description='x;Integrated Security=SSPI'",
            "Description=\"x;Integrated Security=SSPI\"",
            "Description={x;Integrated Security=SSPI}",
            "Description='it''s;User Id=u'",
        ] {
            assert_eq!(
                admit(&format!("Server=db;User Id=u;Password=p;{quoted}"), true),
                Ok(Endpoint::Tcp),
                "{quoted}"
            );
        }
    }

    /// The port and the driver must read a string alike, or admission judges
    /// something the driver does not do. `Config`'s `Debug` shows the options
    /// the driver took from the string (secrets redacted), so reading the
    /// string itself and reading its canonical spelling must give the same.
    #[test]
    fn driver_and_port_read_the_same_options() {
        for string in [
            "Server=db,1433;Database=app;User Id=u;Password=p;Application Name=a",
            "  SERVER = db,1433 ; database = app ;; user id = u ",
            "Server=db;Database='a;b';User Id=\"u;v\";Application Name={x;y}",
            "Server=db;Database='it''s';Application Name=\"say \"\"hi\"\"\"",
            "Server=db;Application Name='  padded  '",
            "Server=db;Application Name=  padded  ",
            "Server=db;Description=x{;Database=hidden;ignored=};User Id=u",
            "Server=db;Description=x';Database=hidden;a=b'",
            "Server=db;Description=x\";Database=hidden;a=b\"",
            "Server=db;Database={a'b\"c};User Id=u",
            "Server=db;a==b=c;Database=app",
            "Server=db;Database=app;Database=last",
            "Server=db;Database=\tapp\t;User\tId=tab",
            "Server=db;Database=line\nbreak",
            "Server=db\\inst;Database=app",
            "Server=tcp:db,1433;Encrypt=false;TrustServerCertificate=true",
        ] {
            let options = Options::parse(string).expect(string);
            let canonical = options.canonical();
            assert_eq!(
                Options::parse(&canonical).as_ref(),
                Ok(&options),
                "{string}"
            );
            let driver = |s: &str| format!("{:?}", Config::from_ado_string(s));
            assert!(
                !driver(string).contains("Err("),
                "the driver accepts `{string}`: {}",
                driver(string)
            );
            assert_eq!(driver(string), driver(&canonical), "{string}");
        }
    }

    #[test]
    fn a_string_the_port_cannot_read_is_one_the_driver_cannot_read() {
        for string in [
            "Server=db;Database='unclosed",
            "Server=db;Database=\"unclosed",
            "Server=db;Database={unclosed",
            "Server=db;Database='a'b",
            "Server=db;Database={a}b",
            "Server=db;nokey",
            "Server=db;=value",
            "Server=db;k==v",
            "Server=db;Database=\u{e9}",
        ] {
            assert!(Options::parse(string).is_err(), "{string}");
            assert!(Config::from_ado_string(string).is_err(), "{string}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_string_is_the_drivers_own_error_and_opens_nothing() {
        let result = Conn::connect("Server=db;Integrated Security=SSPI;Password='open").await;
        match result {
            Err(DbError::BadConnectionString(reason)) => {
                assert!(reason.contains("unclosed"), "{reason}");
            }
            Err(other) => panic!("expected a malformed-string error, got: {other}"),
            Ok(_) => panic!("an unreadable string must not connect"),
        }
    }

    #[tokio::test]
    async fn connecting_never_falls_back_when_admission_refuses() {
        // A listener on the pipe's would-be host proves no TCP attempt is
        // made, and the integrated string must not be sent as SQL login.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = tokio::spawn(async move { listener.accept().await.is_ok() });
        for string in [
            format!("Server=127.0.0.1,{port};Integrated Security=SSPI"),
            format!("Server=np:{PIPE},{port};Integrated Security=SSPI"),
            format!("Server=np:{PIPE};User Id=u;Password=p;Integrated Security=SSPI"),
        ] {
            assert!(
                matches!(Conn::connect(&string).await, Err(DbError::Refused(_))),
                "{string}"
            );
        }
        assert!(
            !accepted.is_finished(),
            "a refused string must not open a socket"
        );
        accepted.abort();
    }
}

/// The pipe transport itself. Only Windows has named pipes, so these run only
/// there; the admission rules above run everywhere.
#[cfg(all(test, windows))]
mod pipe_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    fn unique_pipe(tag: &str) -> String {
        format!(r"\\.\pipe\pbps-test-{}-{tag}", std::process::id())
    }

    #[tokio::test]
    async fn a_missing_pipe_is_a_connect_error_and_never_a_tcp_attempt() {
        // The listener stands where a fallback would dial. The pipe's own
        // name is the only address the failure may mention.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let name = unique_pipe("missing");
        let started = Instant::now();
        let result = Conn::connect(&format!(
            "Server=np:{name},{};Integrated Security=SSPI",
            listener.local_addr().unwrap().port()
        ))
        .await;
        match result {
            Err(DbError::Refused(_)) => {}
            Err(other) => panic!("a comma-suffixed pipe is not a pipe name, got: {other}"),
            Ok(_) => panic!("a comma-suffixed pipe must not connect"),
        }
        let result = Conn::connect(&format!("Server=np:{name};Integrated Security=SSPI")).await;
        match result {
            Err(DbError::Connect { addr, reason }) => {
                assert_eq!(addr, name);
                assert_eq!(reason.kind(), std::io::ErrorKind::NotFound);
            }
            Err(other) => panic!("expected a connect error, got: {other}"),
            Ok(_) => panic!("a missing pipe must not connect"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "nothing may dial TCP after a pipe fails"
        );
    }

    #[tokio::test]
    async fn a_busy_pipe_ends_at_the_budget_instead_of_waiting_forever() {
        let name = unique_pipe("busy");
        let _server = ServerOptions::new().max_instances(1).create(&name).unwrap();
        // The only server instance is taken by this client, so a second open
        // sees `ERROR_PIPE_BUSY` until the budget runs out.
        let _holder = ClientOptions::new().open(&name).unwrap();
        let started = Instant::now();
        let config = Config::from_ado_string("Server=localhost;User Id=u;Password=p").unwrap();
        let result = Conn::connect_pipe(config, &name, Duration::from_millis(300)).await;
        match result {
            Err(DbError::ConnectTimeout { addr, after }) => {
                assert_eq!(addr, name);
                assert_eq!(after, Duration::from_millis(300));
            }
            Err(other) => panic!("expected a connect timeout, got: {other}"),
            Ok(_) => panic!("a busy pipe must not connect"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_stalled_handshake_over_a_pipe_is_reported_as_a_driver_timeout() {
        let name = unique_pipe("stalled");
        let server = ServerOptions::new().create(&name).unwrap();
        let held = tokio::spawn(async move {
            server.connect().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let mut config = Config::from_ado_string(
            "Server=localhost;User Id=synthetic;Password=marker;Encrypt=true",
        )
        .unwrap();
        config.handshake_timeout(Some(Duration::from_millis(100)));
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            Conn::connect_pipe(config, &name, Duration::from_secs(1)),
        )
        .await
        .expect("the driver handshake must terminate within its bound");
        held.abort();
        match result {
            Err(DbError::Driver { message, .. }) => {
                assert!(message.contains("connection handshake"), "{message}");
            }
            Err(other) => panic!("expected a driver timeout, got: {other}"),
            Ok(_) => panic!("a stalled handshake must not connect"),
        }
    }

    /// The first byte of a PRELOGIN is the TDS packet type; a plaintext LOGIN7
    /// (0x10) after a server that declines encryption would leak the login.
    #[tokio::test]
    async fn tds_encryption_refusal_over_a_pipe_cannot_elicit_a_plaintext_login() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let name = unique_pipe("noencrypt");
        let mut server = ServerOptions::new().create(&name).unwrap();
        let served = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut header = [0; 8];
            server.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], 0x12);
            let mut payload = vec![0; usize::from(u16::from_be_bytes([header[2], header[3]])) - 8];
            server.read_exact(&mut payload).await.unwrap();
            server
                .write_all(&[0x04, 1, 0, 15, 0, 0, 1, 0, 1, 0, 6, 0, 1, 0xff, 2])
                .await
                .unwrap();
            let mut next = [0];
            if server.read(&mut next).await.unwrap() != 0 {
                assert_eq!(
                    next[0], 0x12,
                    "plaintext authentication followed TLS refusal"
                );
            }
        });
        let config =
            Config::from_ado_string("Server=localhost;User Id=synthetic;Password=marker").unwrap();
        assert!(
            Conn::connect_pipe(config, &name, Duration::from_secs(1))
                .await
                .is_err()
        );
        served.await.unwrap();
    }
}

#[cfg(test)]
mod localdb_selector_tests {
    use super::*;

    const PIPE: &str = r"np:\\.\pipe\LOCALDB#ABCD1234\tsql\query";

    fn read(connection: &str) -> Result<Option<LocalDbSelector>, String> {
        LocalDbSelector::read_on(connection, true).map_err(|e| e.to_string())
    }

    #[test]
    fn a_named_instance_is_selected_and_keeps_every_other_setting() {
        let selector = read(
            r"Server=(localdb)\pbps_scratch;Integrated Security=SSPI;Encrypt=true;Database=x;TrustServerCertificate=true",
        )
        .unwrap()
        .expect("a selector");
        assert_eq!(selector.instance(), "pbps_scratch");
        let rewritten = selector.with_pipe(PIPE).unwrap();
        let options = Options::parse(&rewritten).unwrap();
        assert_eq!(options.get("server"), Some(PIPE));
        for (key, value) in [
            ("integrated security", "SSPI"),
            ("encrypt", "true"),
            ("database", "x"),
            ("trustservercertificate", "true"),
        ] {
            assert_eq!(options.get(key), Some(value), "{key}");
        }
        // The rewrite is what the driver admits, not a second reading of it.
        assert_eq!(
            Endpoint::admit(&options, true).unwrap(),
            Endpoint::Pipe(PIPE[3..].to_owned())
        );
    }

    #[test]
    fn the_selector_may_be_spelled_with_either_server_key_and_any_case() {
        let selector = read(r"Data Source=(LocalDB)\Inst;Integrated Security=true")
            .unwrap()
            .unwrap();
        assert_eq!(selector.instance(), "Inst");
        // The pipe replaces the server under whichever key named it.
        let options = Options::parse(&selector.with_pipe(PIPE).unwrap()).unwrap();
        assert_eq!(options.get("server"), Some(PIPE));
        assert_eq!(options.get("data source"), None);
        // A space inside the parentheses is not the LocalDB spelling.
        assert!(
            read(r"server=( localdb )\x;Integrated Security=true")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_string_that_names_no_localdb_server_is_not_a_selector() {
        for text in [
            "Server=db,1433;User Id=sa;Password=x",
            r"Server=np:\\.\pipe\x;Integrated Security=SSPI",
            "Integrated Security=SSPI",
            "",
            // The port cannot read it, so opening it reports the driver's words.
            "Server=(localdb)\\x;a='b",
        ] {
            assert!(read(text).unwrap().is_none(), "{text:?}");
        }
    }

    #[test]
    fn unsupported_localdb_forms_are_refused_by_name_before_any_helper_runs() {
        for text in [
            r"Server=(localdb)\.;Integrated Security=SSPI",
            r"Server=(localdb)\.\SHARED;Integrated Security=SSPI",
            r"Server=(localdb)\v11.0;Integrated Security=SSPI",
            r"Server=(localdb);Integrated Security=SSPI",
            r"Server=(localdb)\;Integrated Security=SSPI",
            r"Server=(localdb)\-x;Integrated Security=SSPI",
            r"Server=(localdb)\a b;Integrated Security=SSPI",
            r"Server=np:(localdb)\x;Integrated Security=SSPI",
            r"Server=tcp:(localdb)\x;Integrated Security=SSPI",
            r"Server=host\(localdb)\x;Integrated Security=SSPI",
        ] {
            let why = read(text).unwrap_err();
            assert!(why.contains("named LocalDB instance"), "{text}: {why}");
        }
        let long = format!(
            r"Server=(localdb)\{};Integrated Security=SSPI",
            "a".repeat(129)
        );
        assert!(read(&long).is_err());
    }

    #[test]
    fn a_selector_without_integrated_authentication_or_with_credentials_is_refused() {
        for (text, needle) in [
            (r"Server=(localdb)\x", "requires `Integrated Security=SSPI`"),
            (
                r"Server=(localdb)\x;Integrated Security=false",
                "requires `Integrated Security=SSPI`",
            ),
            (
                r"Server=(localdb)\x;Integrated Security=SSPI;User Id=sa;Password=p@ss",
                "cannot be combined",
            ),
            (
                r"Server=(localdb)\x;Trusted_Connection=yes",
                "does not read `Trusted_Connection`",
            ),
        ] {
            let why = read(text).unwrap_err();
            assert!(why.contains(needle), "{text}: {why}");
            assert!(!why.contains("p@ss"), "a refusal repeated a secret: {why}");
        }
    }

    #[test]
    fn a_selector_is_refused_off_windows_without_naming_a_host() {
        let why = LocalDbSelector::read_on(r"Server=(localdb)\x;Integrated Security=SSPI", false)
            .expect_err("refused")
            .to_string();
        assert!(why.contains("only available on Windows"), "{why}");
    }

    #[test]
    fn only_a_local_pipe_can_stand_in_for_the_selector() {
        let selector = read(r"Server=(localdb)\x;Integrated Security=SSPI")
            .unwrap()
            .unwrap();
        for pipe in [
            r"\\.\pipe\LOCALDB#1\tsql\query",
            r"np:\\host\pipe\x",
            r"np:\\.\pipe\..\x",
            r"np:\\.\pipe\a;Integrated Security=false",
            "tcp:host,1433",
            "",
        ] {
            assert!(selector.with_pipe(pipe).is_err(), "{pipe:?}");
        }
    }

    #[test]
    fn a_pipe_containing_quotes_cannot_add_a_setting() {
        // The alphabet has no quote or semicolon, so the rewrite cannot be
        // made to carry a second setting past admission.
        let selector = read(r"Server=(localdb)\x;Integrated Security=SSPI")
            .unwrap()
            .unwrap();
        assert!(
            selector
                .with_pipe(r"np:\\.\pipe\a';Encrypt=off;b='")
                .is_err()
        );
    }

    #[test]
    fn a_selector_never_prints_the_settings_it_holds() {
        let selector =
            read(r"Server=(localdb)\x;Integrated Security=SSPI;Description=kept-private")
                .unwrap()
                .unwrap();
        let shown = format!("{selector:?}");
        assert!(shown.contains("\"x\""), "{shown}");
        assert!(!shown.contains("kept-private"), "{shown}");
    }
}
