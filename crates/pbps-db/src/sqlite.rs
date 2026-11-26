//! The SQLite driver module (#2950): the one file that names `rusqlite`.
//!
//! ARCHITECTURE constraint 9 reads "one file per driver", and this is the third
//! one. Unlike [`crate::mssql`] and [`crate::postgres`] it is public:
//! [`crate::Conn`] has a `Sqlite` variant that runs statements and
//! transactions, and no row type for it, so the engine crate `pbps-sqlite`
//! reads through [`Connection`]'s typed functions, which it reaches with
//! [`crate::Conn::sqlite`] (DEC-2950.17). No `rusqlite` type appears in any
//! signature here.
//!
//! # Synchronous on purpose
//!
//! `rusqlite::Connection` is `Send` and not `Sync`, and SQLite runs inside the
//! calling thread: there is no server to wait for. This module is therefore
//! plain blocking code, and the async [`crate::Conn`] methods call it in
//! place: a borrowed connection cannot move to a blocking thread, and a call
//! abandoned there could outlive its caller (DEC-2950.17). A transaction lives
//! in the connection, so it spans those calls.
//!
//! # What the setup refuses, and why
//!
//! Each of these was measured on a real SQLite (#2950, the feasibility study):
//!
//! - `PRAGMA foreign_keys` is per connection, off by default in a distribution
//!   build and on in the bundled one, and **silently ignored inside a
//!   transaction**. A connection that exists here has it on, read back, and a
//!   transaction does not start unless it still reads on.
//! - A path is a path. SQLite reads `""` as a private temporary file,
//!   `":memory:"` as a private database that no second connection can reach,
//!   and a name starting with `file:` as a URI (the bundled build turns URI
//!   handling on by default). All three are refused, and
//!   [`Connection::open_in_memory`] is the one way to ask for memory.
//! - Opening a file does not read it. The setup reads the schema once, so a
//!   file that is not a database is refused where it opens and not at the first
//!   query of some later step.
//! - A write-protected file opened for writing silently becomes a read-only
//!   connection. That is refused: this module opens a database to change it.

use std::io;
use std::path::Path;
use std::time::Duration;

use pbps_dialect::TransactionFraming;
use rusqlite::OpenFlags;
use rusqlite::types::{Value as DriverValue, ValueRef};

use crate::DbError;

/// Whether opening may create the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// The file must exist. An absent file is refused and nothing is created:
    /// a mistyped path must not turn into a new, empty database that every
    /// later command then reads as "no schema yet".
    ExistingOnly,
    /// A missing file is created, empty. The directory must exist.
    CreateIfMissing,
}

/// A SQLite library version, ordered by its first three components.
///
/// A fourth component (SQLite once shipped `3.8.11.1`) is accepted and
/// dropped: nothing in this tool is gated on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Reads the text `sqlite_version()` answers. Anything else is refused:
    /// a version that cannot be read must not compare as old or as new.
    pub fn parse(text: &str) -> Result<Self, DbError> {
        let bad = || DbError::BadRow("sqlite_version() did not answer major.minor.patch".into());
        let parts: Vec<u32> = text
            .split('.')
            .map(|part| part.parse::<u32>().map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        match parts[..] {
            [major, minor, patch] | [major, minor, patch, _] => Ok(Self::new(major, minor, patch)),
            _ => Err(bad()),
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One SQLite value, the five storage classes and nothing the driver adds.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "an integer",
            Value::Real(_) => "a real",
            Value::Text(_) => "text",
            Value::Blob(_) => "a blob",
        }
    }

    fn to_driver(&self) -> DriverValue {
        match self {
            Value::Null => DriverValue::Null,
            Value::Integer(n) => DriverValue::Integer(*n),
            Value::Real(x) => DriverValue::Real(*x),
            Value::Text(s) => DriverValue::Text(s.clone()),
            Value::Blob(b) => DriverValue::Blob(b.clone()),
        }
    }

    fn from_driver(value: ValueRef<'_>) -> Result<Self, DbError> {
        Ok(match value {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::Integer(n),
            ValueRef::Real(x) => Value::Real(x),
            // SQLite stores any bytes as text. Replacing the bad ones would
            // hand a caller a name that is not the one in the database.
            ValueRef::Text(bytes) => Value::Text(
                String::from_utf8(bytes.to_vec())
                    .map_err(|_| DbError::BadRow("a text value is not valid UTF-8".into()))?,
            ),
            ValueRef::Blob(bytes) => Value::Blob(bytes.to_vec()),
        })
    }
}

/// One result row. The accessors say whether a column was absent, NULL or of
/// another type, because a catalog reader must not read those as one answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Record(Vec<Value>);

impl Record {
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn column(&self, index: usize) -> Result<&Value, DbError> {
        self.0.get(index).ok_or_else(|| {
            DbError::BadRow(format!(
                "column {index} is out of range for a row of {}",
                self.0.len()
            ))
        })
    }

    pub fn text(&self, index: usize) -> Result<&str, DbError> {
        let value = self.column(index)?;
        match value {
            Value::Text(s) => Ok(s),
            Value::Null | Value::Integer(_) | Value::Real(_) | Value::Blob(_) => Err(
                DbError::BadRow(format!("column {index} is {}, expected text", value.kind())),
            ),
        }
    }

    pub fn integer(&self, index: usize) -> Result<i64, DbError> {
        let value = self.column(index)?;
        match value {
            Value::Integer(n) => Ok(*n),
            Value::Null | Value::Real(_) | Value::Text(_) | Value::Blob(_) => Err(DbError::BadRow(
                format!("column {index} is {}, expected an integer", value.kind()),
            )),
        }
    }

    /// Text, or `None` for NULL. Any other type, and a column that is not there,
    /// are errors: a nullable column is the one place NULL is an answer, and a
    /// value of another type must not read as its absence.
    pub fn optional_text(&self, index: usize) -> Result<Option<&str>, DbError> {
        match self.column(index)? {
            Value::Null => Ok(None),
            Value::Text(s) => Ok(Some(s)),
            value @ (Value::Integer(_) | Value::Real(_) | Value::Blob(_)) => Err(DbError::BadRow(
                format!("column {index} is {}, expected text or NULL", value.kind()),
            )),
        }
    }

    /// An integer, or `None` for NULL; see [`Record::optional_text`].
    pub fn optional_integer(&self, index: usize) -> Result<Option<i64>, DbError> {
        match self.column(index)? {
            Value::Null => Ok(None),
            Value::Integer(n) => Ok(Some(*n)),
            value @ (Value::Real(_) | Value::Text(_) | Value::Blob(_)) => {
                Err(DbError::BadRow(format!(
                    "column {index} is {}, expected an integer or NULL",
                    value.kind()
                )))
            }
        }
    }
}

/// One action SQLite's authorizer reported while preparing a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthEvent {
    Select,
    Read {
        database: Option<String>,
        table: String,
        column: String,
        accessor: Option<String>,
    },
    Function {
        name: String,
    },
    Other(String),
}

/// What preparing a statement under an authorizer observed, without stepping it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedAudit {
    pub events: Vec<AuthEvent>,
    pub parameters: usize,
}

type ClearAuth = fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization;

struct PrepareGuard<'a> {
    conn: &'a mut rusqlite::Connection,
    active: bool,
    restore_dqs: Option<bool>,
}

impl PrepareGuard<'_> {
    fn restore(&mut self) -> Result<(), DbError> {
        let auth = if std::mem::take(&mut self.active) {
            self.conn
                .authorizer(None::<ClearAuth>)
                .map_err(|e| driver_error(e).context("removing the SQLite authorizer"))
        } else {
            Ok(())
        };
        let dqs = self.restore_dqs.take().map_or(Ok(()), |prev| {
            self.conn
                .set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML, prev)
                .map(|_| ())
                .map_err(|e| driver_error(e).context("restoring SQLite double-quoted string mode"))
        });
        auth?;
        dqs
    }
}

impl Drop for PrepareGuard<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// An open SQLite database, with foreign keys enforced.
///
/// `Send` and not `Sync`, as the library's own connection is.
pub struct Connection {
    inner: rusqlite::Connection,
    /// Declared after `inner`, so the database is closed before what is
    /// parked here is dropped.
    parked: Option<Box<dyn std::any::Any + Send>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("inner", &self.inner)
            .field("parked", &self.parked.is_some())
            .finish()
    }
}

impl Connection {
    /// Opens a database file read-write.
    ///
    /// Three outcomes are kept apart, because each means something else to a
    /// command: an absent file (`Connect` carrying `NotFound`, under
    /// [`OpenMode::ExistingOnly`]), an empty database (opens; its catalog is
    /// empty) and one that cannot be read (`Connect` for an I/O failure,
    /// `Driver` with code 26 for a file that is not a database, `Refused` for a
    /// write-protected one).
    ///
    /// The busy timeout is 0: a second deployer is refused at once. The
    /// library's own default waits 5 seconds, and whether and how long to wait
    /// for the apply lock is a decision of the state layer, not of an open.
    pub fn open(path: &Path, mode: OpenMode) -> Result<Self, DbError> {
        let shown = path.display().to_string();
        if shown.is_empty() || shown == ":memory:" {
            return Err(DbError::BadConnectionString(
                "an empty path or `:memory:` is not a database file; \
                 use `open_in_memory` for a private memory database"
                    .into(),
            ));
        }
        // The bundled library is built with URI handling on by default, so a
        // name that starts with `file:` would open something other than the
        // file it names (`?mode=memory` opens a private memory database).
        if shown
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
        {
            return Err(DbError::BadConnectionString(format!(
                "`{shown}` starts with `file:`, which SQLite would read as a URI; \
                 write the path as `./{shown}`"
            )));
        }
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {
                return Err(DbError::BadConnectionString(format!(
                    "`{shown}` is a directory, not a database file"
                )));
            }
            Ok(_) => {}
            Err(reason)
                if reason.kind() == io::ErrorKind::NotFound
                    && mode == OpenMode::CreateIfMissing => {}
            Err(reason) => {
                return Err(DbError::Connect {
                    addr: shown,
                    reason,
                });
            }
        }
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if mode == OpenMode::CreateIfMissing {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let inner = rusqlite::Connection::open_with_flags(path, flags).map_err(|e| {
            driver_error(e).context(format!("opening the SQLite database `{shown}`"))
        })?;
        Self::set_up(inner, &shown)
    }

    /// Opens a private in-memory database. Every call is a different, empty
    /// database; two connections never share one.
    pub fn open_in_memory() -> Result<Self, DbError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let inner = rusqlite::Connection::open_with_flags(":memory:", flags)
            .map_err(|e| driver_error(e).context("opening an in-memory SQLite database"))?;
        Self::set_up(inner, "the in-memory database")
    }

    fn set_up(inner: rusqlite::Connection, shown: &str) -> Result<Self, DbError> {
        inner
            .busy_timeout(Duration::ZERO)
            .map_err(|e| driver_error(e).context("setting the SQLite busy timeout"))?;
        if inner.is_readonly(rusqlite::MAIN_DB).map_err(|e| {
            driver_error(e).context("asking whether the SQLite database is read-only")
        })? {
            return Err(DbError::Refused(format!(
                "{shown} can only be opened read-only (the file is write-protected); \
                 pbps opens a database to change it"
            )));
        }
        enable_foreign_keys(&inner)?;
        inner
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| driver_error(e).context(format!("reading the schema of {shown}")))?;
        Ok(Self {
            inner,
            parked: None,
        })
    }

    /// How long a statement waits for another connection's lock before it
    /// fails with a busy error (extended code 5). Zero means not at all.
    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<(), DbError> {
        self.inner
            .busy_timeout(timeout)
            .map_err(|e| driver_error(e).context("setting the SQLite busy timeout"))
    }

    /// Whether a transaction is open on this connection.
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        !self.inner.is_autocommit()
    }

    /// The linked library's version, from `sqlite_version()`.
    pub fn version(&self) -> Result<Version, DbError> {
        let rows = self.query("SELECT sqlite_version()", &[])?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::BadRow("sqlite_version() returned no row".into()))?;
        Version::parse(row.text(0)?)
    }

    /// The database's size in bytes, `page_count * page_size`.
    pub fn size(&self) -> Result<u64, DbError> {
        Ok(self.number("page_count")? * self.number("page_size")?)
    }

    fn number(&self, pragma: &str) -> Result<u64, DbError> {
        let rows = self.query(&format!("PRAGMA {pragma}"), &[])?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::BadRow(format!("`PRAGMA {pragma}` returned no row")))?;
        u64::try_from(row.integer(0)?)
            .map_err(|_| DbError::BadRow(format!("`PRAGMA {pragma}` is negative")))
    }

    /// A private in-memory copy of this database's pages, schema and rows alike,
    /// for a run that must change something and leave this database as it is.
    ///
    /// **Bounded twice.** The size is read first and a database over `limit` is
    /// refused before a page is read. The file can grow between that read and the
    /// copy, so the copy's own `max_page_count` is `limit` in pages: a source that
    /// has grown past it stops the copy with a refusal, and what was copied so far
    /// is dropped, never kept as a partial copy.
    ///
    /// **One consistent snapshot.** All pages go in one step under the one read
    /// lock it takes, so a writer waits (delete mode) or commits after the
    /// snapshot (WAL) and the copy is never restarted. (The library would also
    /// restart a copy made page by page if the source changed between steps;
    /// measured to give the same copies, so nothing here relies on it, and a test
    /// cannot tell the two apart.) A source that is busy, another connection's
    /// `EXCLUSIVE` lock or **this connection's own write transaction** (measured:
    /// it is busy for as long as it lasts, and the library's own retry loop never
    /// returns), is retried every 10 ms for `wait` and then fails with the
    /// library's busy code (5), so a caller's busy test answers. It never hangs.
    ///
    /// This connection is left as it was: no setting, no transaction, and its own
    /// busy wait, which is zero while the copy is made, is put back. The
    /// copy is opened through the ordinary setup, so `foreign_keys` is on (the
    /// pragma is the connection's and is not copied).
    pub fn copy_to_memory(&self, limit: u64, wait: Duration) -> Result<Connection, DbError> {
        let deadline = std::time::Instant::now() + wait;
        // `wait` is the whole bound, and it is kept by the retries below. The
        // library's own wait would run inside each read before a retry is
        // asked, so a connection set to wait longer than `wait` would wait
        // that long instead: it is zero while the copy is made, and put back.
        let before = Duration::from_millis(self.number("busy_timeout")?);
        self.set_busy_timeout(Duration::ZERO)?;
        let copied = self.pinned(deadline, || {
            let (size, page_size) = (self.size()?, self.number("page_size")?);
            if size > limit {
                return Err(DbError::Refused(format!(
                    "the database is {size} bytes and a copy is limited to {limit}"
                )));
            }
            self.copy_pages(page_size, limit / page_size, deadline)
        });
        // A copy that was made is not kept on a connection left changed.
        let restored = self.set_busy_timeout(before);
        let copy = copied?;
        restored?;
        Ok(copy)
    }

    /// Runs `work` while this connection holds a read lock on the database, so
    /// what `work` reads first and what it copies after are one snapshot.
    ///
    /// The page bound of a copy is counted in pages of the size read before it.
    /// Without the lock another connection could `VACUUM` the file to a larger
    /// page size between the two, and the same number of pages would be many
    /// times the bytes. With it that `VACUUM` is busy until the copy is done
    /// (delete mode), or is not allowed at all (a write-ahead file keeps its
    /// page size).
    ///
    /// Inside the caller's own transaction nothing is opened or closed. Outside
    /// one, a read transaction is opened and rolled back, and the connection is
    /// left as it was. The caller has set this connection's own wait to zero, and a writer that is
    /// committing makes even the first read busy: it is retried until `deadline`.
    fn pinned<T>(
        &self,
        deadline: std::time::Instant,
        work: impl FnOnce() -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let own = !self.in_transaction();
        if own {
            self.inner.execute_batch("BEGIN").map_err(driver_error)?;
        }
        // The read that takes the lock; a `BEGIN` alone takes none.
        let done = retry_busy(deadline, || {
            self.query("SELECT count(*) FROM sqlite_schema", &[])
        })
        .and_then(|_| work());
        if own {
            // A read transaction: there is nothing to undo, and a failure to end
            // it cannot replace the answer.
            let _ = self.inner.execute_batch("ROLLBACK");
        }
        done
    }

    fn copy_pages(
        &self,
        page_size: u64,
        max_pages: u64,
        deadline: std::time::Instant,
    ) -> Result<Connection, DbError> {
        use rusqlite::backup::{Backup, StepResult};
        let mut copy = Self::open_in_memory()?;
        // The copy takes the source's page size by itself (measured, 512 to 65536),
        // and its `max_page_count` counts those pages.
        copy.inner
            .execute_batch(&format!("PRAGMA max_page_count = {max_pages}"))
            .map_err(driver_error)?;
        {
            let backup = Backup::new(&self.inner, &mut copy.inner).map_err(driver_error)?;
            loop {
                match backup.step(-1) {
                    Ok(StepResult::Done) => break,
                    Ok(StepResult::Busy | StepResult::Locked) => {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            return Err(DbError::Driver {
                                message: "database is locked: it stayed busy for the whole wait \
                                          while it was copied"
                                    .into(),
                                code: Some("5".into()),
                                diagnostic: None,
                            });
                        }
                        std::thread::sleep(left.min(Duration::from_millis(10)));
                    }
                    Ok(_) => {}
                    Err(rusqlite::Error::SqliteFailure(failure, _))
                        if failure.code == rusqlite::ErrorCode::DiskFull =>
                    {
                        return Err(DbError::Refused(format!(
                            "the database grew past {} bytes while it was copied",
                            max_pages * page_size
                        )));
                    }
                    Err(e) => return Err(driver_error(e)),
                }
            }
        }
        // The bound counted pages of `page_size`. The read lock held around this
        // makes another size impossible; were it not, the bound meant nothing.
        let copied = copy.number("page_size")?;
        if copied != page_size {
            return Err(DbError::Refused(format!(
                "the database's page size changed from {page_size} to {copied} while it was copied"
            )));
        }
        // The bound was for the copy; what a run does to the copy is its own.
        copy.inner
            .execute_batch("PRAGMA max_page_count = 4294967294")
            .map_err(driver_error)?;
        Ok(copy)
    }

    /// Prepares one statement under a temporary authorizer without stepping it,
    /// disabling `DQS_DML` during prepare so mistyped quoted columns fail
    /// instead of becoming strings. Both are restored on every return path.
    pub fn prepare_authorized(
        &mut self,
        sql: &str,
        mut authorize: impl FnMut(&AuthEvent) -> Result<(), String> + Send + 'static,
    ) -> Result<PreparedAudit, DbError> {
        use rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML;
        use rusqlite::hooks::{AuthAction, Authorization};
        let prev = self
            .inner
            .db_config(SQLITE_DBCONFIG_DQS_DML)
            .map_err(|e| driver_error(e).context("reading SQLite double-quoted string mode"))?;
        let restore_dqs = prev.then_some(prev);
        if prev {
            self.inner
                .set_db_config(SQLITE_DBCONFIG_DQS_DML, false)
                .map_err(|e| {
                    driver_error(e).context("disabling SQLite double-quoted string mode")
                })?;
        }
        let mut guard = PrepareGuard {
            conn: &mut self.inner,
            active: false,
            restore_dqs,
        };
        let state = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), None::<String>)));
        let sink = std::sync::Arc::clone(&state);
        guard
            .conn
            .authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
                let event = if let AuthAction::Select = ctx.action {
                    AuthEvent::Select
                } else if let AuthAction::Read {
                    table_name,
                    column_name,
                } = ctx.action
                {
                    AuthEvent::Read {
                        database: ctx.database_name.map(str::to_owned),
                        table: table_name.to_owned(),
                        column: column_name.to_owned(),
                        accessor: ctx.accessor.map(str::to_owned),
                    }
                } else if let AuthAction::Function { function_name } = ctx.action {
                    AuthEvent::Function {
                        name: function_name.to_owned(),
                    }
                } else {
                    AuthEvent::Other(format!("{:?}", ctx.action))
                };
                let decision = authorize(&event);
                let mut lock = sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                lock.0.push(event);
                match decision {
                    Ok(()) => Authorization::Allow,
                    Err(reason) => {
                        lock.1.get_or_insert(reason);
                        Authorization::Deny
                    }
                }
            }))
            .map_err(|e| driver_error(e).context("installing the SQLite authorizer"))?;
        guard.active = true;

        let prepared = guard.conn.prepare(sql).map(|stmt| stmt.parameter_count());
        let restored = guard.restore();
        let mut lock = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (events, denied) = (std::mem::take(&mut lock.0), lock.1.take());
        drop(lock);
        restored?;
        if let Some(reason) = denied {
            return Err(DbError::Refused(reason));
        }
        Ok(PreparedAudit {
            events,
            parameters: prepared.map_err(driver_error)?,
        })
    }

    /// Runs one or more statements that return no rows.
    pub fn execute_batch(&self, sql: &str) -> Result<(), DbError> {
        self.inner.execute_batch(sql).map_err(driver_error)
    }

    /// Runs one statement with bound parameters and returns the rows it
    /// changed. A statement that returns rows is an error here; use
    /// [`Connection::query`].
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        let changed = self
            .inner
            .execute(
                sql,
                rusqlite::params_from_iter(params.iter().map(Value::to_driver)),
            )
            .map_err(driver_error)?;
        Ok(changed as u64)
    }

    /// Runs one statement with bound parameters and returns every row.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Record>, DbError> {
        let mut statement = self.inner.prepare(sql).map_err(driver_error)?;
        let width = statement.column_count();
        let mut rows = statement
            .query(rusqlite::params_from_iter(
                params.iter().map(Value::to_driver),
            ))
            .map_err(driver_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(driver_error)? {
            let mut values = Vec::with_capacity(width);
            for index in 0..width {
                values.push(Value::from_driver(
                    row.get_ref(index).map_err(driver_error)?,
                )?);
            }
            out.push(Record(values));
        }
        Ok(out)
    }

    /// The bytes of the file `main` was opened from, empty for a database with
    /// none (in memory, or a temporary one).
    ///
    /// Asked of the statement `PRAGMA database_list` and not of the
    /// table-valued `pragma_database_list`: a table of that name in any schema
    /// takes the table-valued form's place, with whatever rows it holds
    /// (measured), and no object does that to the statement. The bytes are
    /// returned as stored because a file name on Unix need not be UTF-8, which
    /// [`Connection::query`] refuses in a text value.
    pub fn main_database_file(&self) -> Result<Vec<u8>, DbError> {
        let mut statement = self
            .inner
            .prepare("PRAGMA database_list")
            .map_err(driver_error)?;
        let mut rows = statement.query([]).map_err(driver_error)?;
        while let Some(row) = rows.next().map_err(driver_error)? {
            let name = match row.get_ref(1).map_err(driver_error)? {
                ValueRef::Text(name) => name,
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => {
                    return Err(DbError::BadRow("a database's name is not text".into()));
                }
            };
            if name != b"main" {
                continue;
            }
            return match row.get_ref(2).map_err(driver_error)? {
                ValueRef::Text(file) => Ok(file.to_vec()),
                ValueRef::Null => Ok(Vec::new()),
                ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Blob(_) => Err(
                    DbError::BadRow("the database's file name is not text".into()),
                ),
            };
        }
        Err(DbError::BadRow(
            "`PRAGMA database_list` does not list `main`".into(),
        ))
    }

    /// Parks one value on this connection, to live exactly as long as it.
    ///
    /// For the engine's lock: SQLite's apply lock is an operating-system lock
    /// that lasts as long as a guard value, and the command that took it holds
    /// the connection and nothing else across its steps. This crate cannot
    /// name the guard's type, so the slot is type-erased; whoever parks a
    /// value is the only one who knows what to take out. One slot, not a map:
    /// a second value while one is parked is refused and handed back, never
    /// swapped in, because replacing a guard would release the lock it holds.
    pub fn park(
        &mut self,
        value: Box<dyn std::any::Any + Send>,
    ) -> Result<(), Box<dyn std::any::Any + Send>> {
        if self.parked.is_some() {
            return Err(value);
        }
        self.parked = Some(value);
        Ok(())
    }

    /// Takes the parked value out, leaving the slot empty. `None` when
    /// nothing is parked, which is a different answer from a value of an
    /// unexpected type: the caller downcasts what it gets.
    pub fn unpark(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        self.parked.take()
    }

    /// Opens a transaction.
    ///
    /// What the transaction statements say is the dialect's (ADR-0014 section
    /// 2), so they are a parameter. Before the begin, foreign keys are set and
    /// read back again: the pragma cannot be changed inside a transaction, so
    /// this is the last point at which it can be checked, and a caller that
    /// switched it off through [`Connection::execute_batch`] is not carried
    /// into a transaction that silently does not enforce it. A transaction
    /// already open is refused for the same reason.
    pub fn begin(&mut self, framing: &TransactionFraming) -> Result<(), DbError> {
        if self.in_transaction() {
            return Err(DbError::Refused(
                "a SQLite transaction is already open on this connection; \
                 foreign keys cannot be checked inside it"
                    .into(),
            ));
        }
        enable_foreign_keys(&self.inner)?;
        self.inner
            .execute_batch(&framing.begin)
            .map_err(|e| driver_error(e).context("opening a SQLite transaction"))
    }

    /// Commits. A commit that fails is rolled back here, so the caller is
    /// never left to guess whether the transaction is still open; if even the
    /// rollback leaves the connection inside it, the error says so.
    pub fn commit(&mut self, framing: &TransactionFraming) -> Result<(), DbError> {
        match self.inner.execute_batch(&framing.commit) {
            Ok(()) => Ok(()),
            Err(e) => {
                let failure = driver_error(e).context("committing the SQLite transaction");
                Err(self.abandon(framing, failure))
            }
        }
    }

    /// Rolls back. A rollback that fails because the engine already ended the
    /// transaction is not an error: there is nothing left to undo, and its
    /// message must not replace the failure that led here. One that leaves
    /// the connection inside the transaction is.
    pub fn rollback(&mut self, framing: &TransactionFraming) -> Result<(), DbError> {
        match self.inner.execute_batch(&framing.rollback) {
            Err(e) if self.in_transaction() => Err(driver_error(e)
                .context("the rollback failed and the connection is still inside the transaction")),
            Ok(()) | Err(_) => Ok(()),
        }
    }

    /// Runs `work` in a transaction: commit if it returns `Ok`, roll back and
    /// return its error if not. The closure form of [`Connection::begin`],
    /// [`Connection::commit`] and [`Connection::rollback`], with their checks.
    ///
    /// A rollback that fails because the engine already ended the transaction
    /// does not replace the error that caused it.
    pub fn transaction<T>(
        &mut self,
        framing: &TransactionFraming,
        work: impl FnOnce(&Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        self.begin(framing)?;
        match work(self) {
            Ok(value) => self.commit(framing).map(|()| value),
            Err(failure) => Err(self.abandon(framing, failure)),
        }
    }

    /// Rolls back after `failure` and returns it. If the rollback leaves the
    /// connection inside the transaction, the failure says so.
    fn abandon(&self, framing: &TransactionFraming, failure: DbError) -> DbError {
        match self.inner.execute_batch(&framing.rollback) {
            Err(_) if self.in_transaction() => failure
                .context("the rollback failed and the connection is still inside the transaction"),
            Ok(()) | Err(_) => failure,
        }
    }
}

/// Sets `PRAGMA foreign_keys` and reads it back; refuses if it reads anything
/// but on. The read-back is the point: inside a transaction the setting is
/// ignored without an error, and a build compiled without foreign keys answers
/// no row at all.
fn enable_foreign_keys(inner: &rusqlite::Connection) -> Result<(), DbError> {
    inner
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(|e| driver_error(e).context("turning on SQLite foreign key enforcement"))?;
    let state = match inner.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0)) {
        Ok(state) => Some(state),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            return Err(driver_error(e).context("reading back SQLite foreign key enforcement"));
        }
    };
    match state {
        Some(1) => Ok(()),
        Some(_) | None => Err(DbError::Refused(format!(
            "SQLite foreign key enforcement is off on this connection (`PRAGMA foreign_keys` reads {}); \
             pbps refuses to run without it",
            state.map_or_else(|| "nothing".to_string(), |n| n.to_string())
        ))),
    }
}

/// Runs `read` again, every 10 ms, while it fails busy and `deadline` is ahead;
/// a busy failure at the deadline is the answer. Any other failure is at once.
fn retry_busy<T>(
    deadline: std::time::Instant,
    mut read: impl FnMut() -> Result<T, DbError>,
) -> Result<T, DbError> {
    loop {
        match read() {
            Err(error)
                if error
                    .server_error_code()
                    .and_then(|code| code.parse::<u32>().ok())
                    .is_some_and(|code| code & 0xff == 5) =>
            {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Err(error);
                }
                std::thread::sleep(left.min(Duration::from_millis(10)));
            }
            other => return other,
        }
    }
}

/// A failure the library reported. The code is the extended result code as
/// decimal text (5 busy, 26 not a database, 787 foreign key), which is what
/// SQLite documents; a name would hide the number a person looks up.
fn driver_error(error: rusqlite::Error) -> DbError {
    let code = if let rusqlite::Error::SqlInputError { error, .. } = &error {
        Some(error.extended_code.to_string())
    } else {
        error
            .sqlite_extended_error_code()
            .map(|code| code.to_string())
    };
    DbError::Driver {
        code,
        message: error.to_string(),
        diagnostic: None,
    }
}

#[cfg(test)]
mod copy_tests;

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::time::Instant;

    use super::*;

    const IMMEDIATE: TransactionFraming = TransactionFraming {
        begin: Cow::Borrowed("BEGIN IMMEDIATE"),
        commit: Cow::Borrowed("COMMIT"),
        rollback: Cow::Borrowed("ROLLBACK"),
    };

    /// A directory of this test's own, removed when it goes out of scope.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir() -> TempDir {
        let path =
            std::env::temp_dir().join(format!("pbps-sqlite-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).expect("a temporary directory");
        TempDir(path)
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query(sql, &[]).expect("query")[0]
            .integer(0)
            .expect("integer")
    }

    /// Every frame of the chain, as a chain renderer joins them.
    fn chain(error: &dyn std::error::Error) -> String {
        let mut text = error.to_string();
        let mut next = error.source();
        while let Some(frame) = next {
            text.push_str(": ");
            text.push_str(&frame.to_string());
            next = frame.source();
        }
        text
    }

    #[test]
    fn a_missing_file_without_create_is_refused_and_not_created() {
        let dir = dir();
        let path = dir.path().join("absent.db");
        match Connection::open(&path, OpenMode::ExistingOnly) {
            Err(DbError::Connect { reason, .. }) => {
                assert_eq!(reason.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected an absent-file refusal, got {other:?}"),
        }
        assert!(!path.exists(), "a refused open must not create the file");
    }

    #[test]
    fn create_if_missing_creates_the_file_and_an_existing_open_finds_it() {
        let dir = dir();
        let path = dir.path().join("new.db");
        Connection::open(&path, OpenMode::CreateIfMissing)
            .expect("create")
            .execute_batch("CREATE TABLE t (a)")
            .expect("ddl");
        assert!(path.exists());
        let again = Connection::open(&path, OpenMode::ExistingOnly).expect("reopen");
        assert_eq!(count(&again, "SELECT count(*) FROM sqlite_schema"), 1);
    }

    #[test]
    fn an_empty_file_is_an_empty_database_not_an_error() {
        let dir = dir();
        let path = dir.path().join("empty.db");
        std::fs::write(&path, b"").expect("write");
        let c = Connection::open(&path, OpenMode::ExistingOnly).expect("open");
        assert_eq!(count(&c, "SELECT count(*) FROM sqlite_schema"), 0);
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused_where_it_opens() {
        let dir = dir();
        let path = dir.path().join("text.db");
        std::fs::write(
            &path,
            b"this is not a SQLite database, only text of some length\n".repeat(20),
        )
        .expect("write");
        let error = Connection::open(&path, OpenMode::ExistingOnly).expect_err("must refuse");
        assert_eq!(error.server_error_code().as_deref(), Some("26"));
        assert!(
            chain(&error).contains("reading the schema"),
            "{}",
            chain(&error)
        );
    }

    #[test]
    fn a_write_protected_file_is_refused_not_opened_read_only() {
        let dir = dir();
        let path = dir.path().join("locked.db");
        Connection::open(&path, OpenMode::CreateIfMissing).expect("create");
        let mut permissions = std::fs::metadata(&path).expect("meta").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).expect("chmod");
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            eprintln!("skipped: this process can write to a write-protected file (root?)");
            return;
        }
        match Connection::open(&path, OpenMode::ExistingOnly) {
            Err(DbError::Refused(message)) => assert!(message.contains("read-only"), "{message}"),
            other => panic!("expected a refusal of the write-protected file, got {other:?}"),
        }
        // Windows will not delete a read-only file.
        let mut permissions = std::fs::metadata(&path).expect("meta").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).expect("restore");
    }

    #[test]
    fn paths_with_a_special_meaning_to_sqlite_are_not_taken_as_paths() {
        for text in ["", ":memory:"] {
            let error = Connection::open(Path::new(text), OpenMode::CreateIfMissing)
                .expect_err("must refuse");
            assert!(
                matches!(error, DbError::BadConnectionString(_)),
                "{text:?}: {error:?}"
            );
        }
        let dir = dir();
        let error = Connection::open(dir.path(), OpenMode::ExistingOnly).expect_err("a directory");
        assert!(
            matches!(error, DbError::BadConnectionString(_)),
            "{error:?}"
        );
        // With URI handling on, this would open a private memory database and
        // succeed; it is refused before SQLite sees it, and nothing is created.
        let error = Connection::open(
            Path::new("file:pbps_uri_probe?mode=memory"),
            OpenMode::ExistingOnly,
        )
        .expect_err("a file: path must not be read as a URI");
        assert!(
            matches!(error, DbError::BadConnectionString(_)),
            "{error:?}"
        );
    }

    #[test]
    fn foreign_keys_are_on_after_open_and_enforced() {
        let c = Connection::open_in_memory().expect("open");
        assert_eq!(count(&c, "PRAGMA foreign_keys"), 1);
        c.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (p INTEGER REFERENCES parent (id));
             INSERT INTO parent VALUES (1);
             INSERT INTO child VALUES (1);",
        )
        .expect("valid rows");
        let error = c
            .execute_batch("INSERT INTO child VALUES (2)")
            .expect_err("violation");
        assert_eq!(error.server_error_code().as_deref(), Some("787"));
    }

    #[test]
    fn the_setup_refuses_when_the_pragma_reads_off() {
        // The measured silent no-op: inside a transaction `PRAGMA foreign_keys = ON`
        // is ignored without an error, so a connection that had it off reads 0.
        let raw = rusqlite::Connection::open_in_memory().expect("raw");
        raw.execute_batch("PRAGMA foreign_keys = OFF; BEGIN")
            .expect("setup");
        match enable_foreign_keys(&raw) {
            Err(DbError::Refused(message)) => assert!(message.contains("reads 0"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // Control: outside a transaction the same function succeeds.
        raw.execute_batch("ROLLBACK").expect("rollback");
        enable_foreign_keys(&raw).expect("on outside a transaction");
    }

    #[test]
    fn a_transaction_turns_foreign_keys_back_on_and_refuses_to_nest() {
        let mut c = Connection::open_in_memory().expect("open");
        c.execute_batch("PRAGMA foreign_keys = OFF").expect("off");
        assert_eq!(count(&c, "PRAGMA foreign_keys"), 0);
        c.transaction(&IMMEDIATE, |tx| {
            assert_eq!(count(tx, "PRAGMA foreign_keys"), 1);
            Ok(())
        })
        .expect("transaction");

        c.execute_batch("BEGIN").expect("manual begin");
        match c.transaction(&IMMEDIATE, |_| Ok(())) {
            Err(DbError::Refused(message)) => {
                assert!(message.contains("already open"), "{message}")
            }
            other => panic!("expected a refusal to nest, got {other:?}"),
        }
        assert!(
            c.in_transaction(),
            "the refused call must leave the caller's transaction alone"
        );
    }

    #[test]
    fn ddl_rolls_back_with_its_transaction_and_persists_on_commit() {
        let mut c = Connection::open_in_memory().expect("open");
        let failure = c.transaction(&IMMEDIATE, |tx| {
            tx.execute_batch("CREATE TABLE gone (a)")?;
            tx.execute_batch("CREATE TABLE gone (a)")
        });
        assert!(failure.is_err());
        assert!(!c.in_transaction());
        assert_eq!(
            count(&c, "SELECT count(*) FROM sqlite_schema WHERE name = 'gone'"),
            0
        );

        c.transaction(&IMMEDIATE, |tx| tx.execute_batch("CREATE TABLE kept (a)"))
            .expect("commit");
        assert_eq!(
            count(&c, "SELECT count(*) FROM sqlite_schema WHERE name = 'kept'"),
            1
        );
    }

    #[test]
    fn a_failed_rollback_does_not_replace_the_error_that_caused_it() {
        let mut c = Connection::open_in_memory().expect("open");
        let error = c
            .transaction(&IMMEDIATE, |tx| -> Result<(), DbError> {
                // The engine's own end of the transaction, so the later
                // ROLLBACK fails with "no transaction is active".
                tx.execute_batch("ROLLBACK")?;
                Err(DbError::Refused("the real failure".into()))
            })
            .expect_err("must fail");
        assert!(
            matches!(&error, DbError::Refused(m) if m == "the real failure"),
            "{error:?}"
        );
        assert!(!c.in_transaction());
    }

    #[test]
    fn the_version_is_read_from_the_library_and_malformed_text_is_refused() {
        let c = Connection::open_in_memory().expect("open");
        let version = c.version().expect("version");
        eprintln!("bundled SQLite version seen by the test: {version}");
        assert!(
            version >= Version::new(3, 35, 0),
            "SQLite {version} is below 3.35.0"
        );
        assert_eq!(
            Version::parse("3.54.1").expect("parse"),
            Version::new(3, 54, 1)
        );
        assert_eq!(
            Version::parse("3.8.11.1").expect("parse"),
            Version::new(3, 8, 11)
        );
        for bad in ["", "3", "3.54", "3.x.1", "3.54.1.2.3", "v3.54.1"] {
            assert!(Version::parse(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(
            Version::new(3, 9, 0) < Version::new(3, 10, 0),
            "compared as numbers, not text"
        );
    }

    #[test]
    fn two_connections_to_one_file_see_each_others_commits_and_refuse_a_second_writer() {
        let dir = dir();
        let path = dir.path().join("shared.db");
        let mut first = Connection::open(&path, OpenMode::CreateIfMissing).expect("first");
        let mut second = Connection::open(&path, OpenMode::ExistingOnly).expect("second");
        assert_eq!(
            count(&second, "PRAGMA busy_timeout"),
            0,
            "the library's 5 s default is overridden"
        );

        first
            .transaction(&IMMEDIATE, |tx| {
                tx.execute_batch("CREATE TABLE t (a)")?;
                // The reader sees the catalog as it was.
                assert_eq!(count(&second, "SELECT count(*) FROM sqlite_schema"), 0);
                // A second writer is refused at once with `database is locked`.
                let busy = second
                    .transaction(&IMMEDIATE, |_| Ok(()))
                    .expect_err("busy");
                assert_eq!(busy.server_error_code().as_deref(), Some("5"), "{busy:?}");
                // With a timeout it waits, then fails the same way.
                second.set_busy_timeout(Duration::from_millis(300))?;
                let started = Instant::now();
                let busy = second
                    .transaction(&IMMEDIATE, |_| Ok(()))
                    .expect_err("busy");
                assert_eq!(busy.server_error_code().as_deref(), Some("5"), "{busy:?}");
                assert!(
                    started.elapsed() >= Duration::from_millis(200),
                    "{:?}",
                    started.elapsed()
                );
                Ok(())
            })
            .expect("first transaction");
        assert_eq!(count(&second, "SELECT count(*) FROM sqlite_schema"), 1);
    }

    #[test]
    fn values_round_trip_through_parameters_and_columns_say_what_they_are() {
        let c = Connection::open_in_memory().expect("open");
        c.execute_batch("CREATE TABLE v (i, r, t, b, n)")
            .expect("ddl");
        let changed = c
            .execute(
                "INSERT INTO v VALUES (?1, ?2, ?3, ?4, ?5)",
                &[
                    Value::Integer(-7),
                    Value::Real(1.5),
                    Value::Text("naïve".into()),
                    Value::Blob(vec![0, 255]),
                    Value::Null,
                ],
            )
            .expect("insert");
        assert_eq!(changed, 1);
        let rows = c.query("SELECT i, r, t, b, n FROM v", &[]).expect("select");
        let row = &rows[0];
        assert_eq!(row.len(), 5);
        assert_eq!(row.integer(0).expect("i"), -7);
        assert_eq!(row.text(2).expect("t"), "naïve");
        // Negative cases: NULL, another type and a missing column are three errors.
        for (result, wanted) in [
            (row.text(4).map(drop), "NULL"),
            (row.integer(2).map(drop), "text"),
            (row.text(9).map(drop), "out of range"),
        ] {
            match result {
                Err(DbError::BadRow(message)) => assert!(message.contains(wanted), "{message}"),
                other => panic!("expected a row-shape error mentioning {wanted}, got {other:?}"),
            }
        }
        assert!(
            c.execute("SELECT 1", &[]).is_err(),
            "a statement that returns rows is not an execute"
        );
        // SQLite keeps any bytes as text; they are refused, not repaired.
        match c.query("SELECT CAST(x'ff' AS TEXT)", &[]) {
            Err(DbError::BadRow(message)) => assert!(message.contains("UTF-8"), "{message}"),
            other => panic!("expected a refusal of text that is not UTF-8, got {other:?}"),
        }
    }

    #[test]
    fn optional_columns_read_null_as_absent_and_nothing_else_as_absent() {
        let c = Connection::open_in_memory().expect("open");
        let rows = c
            .query("SELECT NULL, 'a', 7, 1.5, x'00', '' ", &[])
            .expect("select");
        let row = &rows[0];
        assert_eq!(row.optional_text(0).expect("null"), None);
        assert_eq!(row.optional_text(1).expect("text"), Some("a"));
        assert_eq!(
            row.optional_text(5).expect("empty"),
            Some(""),
            "empty is not NULL"
        );
        assert_eq!(row.optional_integer(0).expect("null"), None);
        assert_eq!(row.optional_integer(2).expect("integer"), Some(7));
        // Negative cases: another type and an absent column are errors, never `None`.
        for (result, wanted) in [
            (row.optional_text(2).map(drop), "an integer"),
            (row.optional_text(3).map(drop), "a real"),
            (row.optional_text(4).map(drop), "a blob"),
            (row.optional_integer(1).map(drop), "text"),
            (row.optional_integer(3).map(drop), "a real"),
            (row.optional_text(9).map(drop), "out of range"),
            (row.optional_integer(9).map(drop), "out of range"),
        ] {
            match result {
                Err(DbError::BadRow(message)) => assert!(message.contains(wanted), "{message}"),
                other => panic!("expected a row-shape error mentioning {wanted}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_connection_can_move_to_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Connection>();
        let c = Connection::open_in_memory().expect("open");
        let n = std::thread::spawn(move || count(&c, "SELECT 41 + 1"))
            .join()
            .expect("join");
        assert_eq!(n, 42);
    }

    #[test]
    fn prepare_authorized_reports_events_and_parameters_without_reading_rows() {
        let dir = dir();
        let path = dir.path().join("auth.db");
        let mut c = Connection::open(&path, OpenMode::CreateIfMissing).expect("open");
        c.execute_batch(
            "CREATE TABLE MixedTbl (ColA INTEGER, txt TEXT);
             CREATE TABLE other (x INTEGER);
             INSERT INTO MixedTbl VALUES (1, 'not-valid-json');",
        )
        .expect("setup");

        // Column reads carry the stored catalog spelling and parameter count is exact.
        let audit = c
            .prepare_authorized(
                "SELECT CASE WHEN (cola = ?1 AND txt LIKE 'a%') THEN 0 ELSE 1 END FROM main.\"mixedtbl\"",
                |_| Ok(()),
            )
            .expect("prepare");
        assert_eq!(audit.parameters, 1);
        assert!(audit.events.contains(&AuthEvent::Select));
        assert!(audit.events.contains(&AuthEvent::Function {
            name: "like".into(),
        }));
        assert!(audit.events.contains(&AuthEvent::Read {
            database: Some("main".into()),
            table: "MixedTbl".into(),
            column: "ColA".into(),
            accessor: None,
        }));
        assert!(audit.events.contains(&AuthEvent::Read {
            database: Some("main".into()),
            table: "MixedTbl".into(),
            column: "txt".into(),
            accessor: None,
        }));

        // Preparing never steps: `json(txt)` fails on the stored row when stepped,
        // yet preparing it succeeds.
        let runtime_sql =
            "SELECT CASE WHEN (json(txt) IS NOT NULL) THEN 0 ELSE 1 END FROM main.\"MixedTbl\"";
        let prepared = c
            .prepare_authorized(runtime_sql, |_| Ok(()))
            .expect("prepare does not evaluate rows");
        assert!(prepared.events.contains(&AuthEvent::Function {
            name: "json".into(),
        }));
        assert!(
            c.query(runtime_sql, &[]).is_err(),
            "stepping the same SQL fails on the row"
        );

        // Negative: double-quoted unknown names fail with SQLite's code rather than
        // falling back to string literals, and the connection's DQS_DML setting is restored.
        let before_dqs = c
            .inner
            .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML)
            .expect("dqs before");
        let typo = c
            .prepare_authorized(
                "SELECT CASE WHEN (\"nope\" = 'x') THEN 0 ELSE 1 END FROM main.\"MixedTbl\"",
                |_| Ok(()),
            )
            .expect_err("double-quoted typo refused");
        assert_eq!(typo.server_error_code().as_deref(), Some("1"), "{typo:?}");
        assert_eq!(
            c.inner
                .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DQS_DML)
                .expect("dqs after"),
            before_dqs
        );
    }

    #[test]
    fn prepare_authorized_denies_rejected_events_and_always_removes_the_authorizer() {
        let dir = dir();
        let path = dir.path().join("deny.db");
        let mut c = Connection::open(&path, OpenMode::CreateIfMissing).expect("open");
        c.execute_batch("CREATE TABLE t (a INTEGER)").expect("ddl");

        // Denial returns the callback's refusal reason as `DbError::Refused`.
        let denied = c
            .prepare_authorized("SELECT random() FROM main.t", |event| match event {
                AuthEvent::Function { name } => Err(format!("function `{name}` is denied")),
                AuthEvent::Select | AuthEvent::Read { .. } | AuthEvent::Other(_) => Ok(()),
            })
            .expect_err("denied");
        let DbError::Refused(msg) = denied else {
            panic!("expected DbError::Refused, got {denied:?}");
        };
        assert!(msg.contains("function `random` is denied"), "{msg}");

        // After denial, the authorizer is gone: ordinary queries and DDL run freely.
        assert_eq!(count(&c, "SELECT count(*) FROM main.t"), 0);
        c.execute_batch("CREATE TABLE after_deny (x INTEGER); INSERT INTO after_deny VALUES (1)")
            .expect("ordinary DDL and DML work after denial");

        // Negative: after a syntax/prepare error, the authorizer is also gone.
        let bad = c
            .prepare_authorized("SELECT FROM", |_| Ok(()))
            .expect_err("syntax error");
        assert_eq!(bad.server_error_code().as_deref(), Some("1"), "{bad:?}");
        c.execute_batch("INSERT INTO after_deny VALUES (2)")
            .expect("ordinary DML works after prepare error");
        assert_eq!(count(&c, "SELECT count(*) FROM after_deny"), 2);
    }
}
