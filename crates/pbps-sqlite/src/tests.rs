//! The real-SQLite harness for this crate's tests (#2950, item 1).
//!
//! There is nothing to start: SQLite is a library in the test process, so these
//! tests are ordinary tests and not `#[ignore]`d live ones, and they need no
//! owner in `scripts/ignored-test-rules.json`. Each test asks for a fresh
//! database and owns it; a file database lives in a temporary directory that is
//! removed when the [`Fresh`] is dropped.

use std::path::{Path, PathBuf};

use crate::{DEFAULT_VERSION_FLOOR, TRANSACTION, Version};
use pbps_db::DbError;
use pbps_db::sqlite::{Connection, OpenMode};

/// A directory of this test's own, removed when it goes out of scope.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fresh database for one test.
pub(crate) struct Fresh {
    /// Held so the directory outlives the connections opened in it.
    _dir: Option<TempDir>,
    path: Option<PathBuf>,
    connection: Connection,
}

impl Fresh {
    /// A private in-memory database. A second `memory()` is another database.
    pub(crate) fn memory() -> Self {
        Self {
            _dir: None,
            path: None,
            connection: Connection::open_in_memory().expect("an in-memory database opens"),
        }
    }

    /// A new, empty database file in a directory of its own.
    pub(crate) fn file() -> Self {
        let root =
            std::env::temp_dir().join(format!("pbps-sqlite-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&root).expect("a temporary directory");
        let dir = TempDir(root);
        let path = dir.0.join("fresh.db");
        let connection =
            Connection::open(&path, OpenMode::CreateIfMissing).expect("a new database file opens");
        Self {
            _dir: Some(dir),
            path: Some(path),
            connection,
        }
    }

    pub(crate) fn connection(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// The file's path, for a test that opens a second connection.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_deref().expect("a file database has a path")
    }

    /// Closes the connection and opens the file again, as a later command
    /// would.
    pub(crate) fn reopen(&mut self) {
        let path = self.path().to_path_buf();
        self.connection =
            Connection::open(&path, OpenMode::ExistingOnly).expect("the file reopens");
    }
}

fn count(connection: &Connection, sql: &str) -> i64 {
    connection.query(sql, &[]).expect("query")[0]
        .integer(0)
        .expect("integer")
}

#[test]
fn two_in_memory_databases_are_separate_databases() {
    let mut a = Fresh::memory();
    let mut b = Fresh::memory();
    a.connection()
        .execute_batch("CREATE TABLE only_in_a (x)")
        .expect("ddl");
    assert_eq!(
        count(a.connection(), "SELECT count(*) FROM sqlite_schema"),
        1
    );
    assert_eq!(
        count(b.connection(), "SELECT count(*) FROM sqlite_schema"),
        0
    );
}

#[test]
fn a_file_database_keeps_its_schema_across_a_reopen() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch("CREATE TABLE kept (x)")
        .expect("ddl");
    fresh.reopen();
    assert_eq!(
        count(
            fresh.connection(),
            "SELECT count(*) FROM sqlite_schema WHERE name = 'kept'"
        ),
        1
    );
}

#[test]
fn the_apply_framing_takes_the_write_lock_and_rolls_ddl_back_on_a_real_file() {
    assert_eq!(TRANSACTION.begin, "BEGIN IMMEDIATE");
    let mut fresh = Fresh::file();
    let mut second =
        Connection::open(fresh.path(), OpenMode::ExistingOnly).expect("second connection");
    let failed = fresh.connection().transaction(&TRANSACTION, |tx| {
        tx.execute_batch("CREATE TABLE doomed (x)")?;
        // `BEGIN IMMEDIATE` is what makes a concurrent apply fail here, before
        // it has changed anything.
        let busy = second
            .transaction(&TRANSACTION, |_| Ok(()))
            .expect_err("busy");
        assert_eq!(busy.server_error_code().as_deref(), Some("5"));
        Err::<(), _>(DbError::Refused("stop".into()))
    });
    assert!(failed.is_err());
    assert_eq!(
        count(
            &second,
            "SELECT count(*) FROM sqlite_schema WHERE name = 'doomed'"
        ),
        0
    );
}

#[test]
fn the_bundled_library_is_at_or_above_the_default_floor() {
    let mut fresh = Fresh::memory();
    let version = fresh.connection().version().expect("version");
    eprintln!("bundled SQLite version seen by the harness test: {version}");
    assert!(
        version >= DEFAULT_VERSION_FLOOR,
        "SQLite {version} is below {DEFAULT_VERSION_FLOOR}"
    );
    // Negative control: the comparison does refuse an older library.
    assert!(Version::new(3, 34, 1) < DEFAULT_VERSION_FLOOR);
}

/// Whether the engine runs `sql` (a fresh in-memory database each time).
pub(crate) fn accepts(sql: &str) -> bool {
    Fresh::memory().connection().execute_batch(sql).is_ok()
}

/// The engine's own words for refusing `sql`, empty if it ran.
pub(crate) fn refusal(sql: &str) -> String {
    Fresh::memory()
        .connection()
        .execute_batch(sql)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}
