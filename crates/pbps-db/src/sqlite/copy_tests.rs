//! The in-memory copy of a database (`Connection::copy_to_memory`), against a
//! real SQLite: what it holds, what it leaves alone, and how it ends when the
//! source is busy, changing or too big.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

const LONG: Duration = Duration::from_secs(60);
const BIG: u64 = 1 << 30;

/// A directory of this test's own, removed when it goes out of scope.
struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file() -> (Dir, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("pbps-sqlite-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&path).expect("a temporary directory");
    let database = path.join("source.db");
    (Dir(path), database)
}

fn open(path: &Path) -> Connection {
    Connection::open(path, OpenMode::CreateIfMissing).expect("opens")
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query(sql, &[]).expect("query")[0]
        .integer(0)
        .expect("integer")
}

fn text(c: &Connection, sql: &str) -> String {
    c.query(sql, &[]).expect("query")[0]
        .text(0)
        .expect("text")
        .to_owned()
}

/// A source with a table, an index, a view and three rows.
fn source(path: &Path) -> Connection {
    let c = open(path);
    c.execute_batch(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); CREATE INDEX i ON t(b); \
         CREATE VIEW v AS SELECT b FROM t; INSERT INTO t(b) VALUES ('x'), ('y'), ('z')",
    )
    .expect("a schema and rows");
    c
}

#[test]
fn a_copy_holds_the_schema_and_every_row_and_leaves_the_source_exactly_as_it_was() {
    let (_dir, path) = file();
    let source = source(&path);
    source
        .set_busy_timeout(Duration::from_millis(1234))
        .expect("set");
    let before = std::fs::read(&path).expect("bytes");
    let copy = source.copy_to_memory(BIG, LONG).expect("copied");
    assert_eq!(count(&copy, "SELECT count(*) FROM t"), 3);
    assert_eq!(count(&copy, "SELECT count(*) FROM sqlite_schema"), 3);
    assert_eq!(text(&copy, "SELECT group_concat(b) FROM v"), "x,y,z");
    // The copy is a connection of this module's own setup.
    assert_eq!(
        count(&copy, "SELECT foreign_keys FROM pragma_foreign_keys"),
        1
    );
    assert!(!copy.in_transaction());
    // What the run does to the copy is its own: past the bound, and not the source's.
    assert!(count(&copy, "PRAGMA max_page_count") > 1_000_000);
    copy.execute_batch("INSERT INTO t(b) VALUES ('w'); DROP VIEW v")
        .expect("changes the copy");
    assert_eq!(count(&source, "SELECT count(*) FROM t"), 3);
    assert_eq!(count(&source, "SELECT count(*) FROM sqlite_schema"), 3);
    // The source: bytes, wait, transaction.
    assert_eq!(std::fs::read(&path).expect("bytes"), before);
    assert_eq!(count(&source, "PRAGMA busy_timeout"), 1234);
    assert!(!source.in_transaction());
    // Negative: the copy is not the file, so a change to the file is not in it.
    source
        .execute_batch("INSERT INTO t(b) VALUES ('later')")
        .expect("source write");
    assert_eq!(count(&copy, "SELECT count(*) FROM t"), 4, "its own 'w'");
    assert_eq!(text(&copy, "SELECT group_concat(b) FROM t"), "x,y,z,w");
}

#[test]
fn a_copy_takes_the_page_size_of_its_source() {
    for page_size in [512, 1024, 4096, 65536] {
        let (_dir, path) = file();
        let c = open(&path);
        c.execute_batch(&format!(
            "PRAGMA page_size = {page_size}; VACUUM; CREATE TABLE t(a); INSERT INTO t VALUES (1)"
        ))
        .expect("a page size");
        assert_eq!(count(&c, "PRAGMA page_size"), page_size);
        let copy = c.copy_to_memory(BIG, LONG).expect("copied");
        assert_eq!(count(&copy, "PRAGMA page_size"), page_size, "{page_size}");
        assert_eq!(copy.size().expect("size"), c.size().expect("size"));
        assert_eq!(count(&copy, "SELECT count(*) FROM t"), 1);
    }
}

#[test]
fn a_database_over_the_limit_is_refused_before_a_page_is_read_and_at_it_is_copied() {
    let (_dir, path) = file();
    let c = source(&path);
    let size = c.size().expect("size");
    match c.copy_to_memory(size - 1, LONG) {
        Err(DbError::Refused(message)) => {
            assert!(message.contains(&size.to_string()), "{message}");
            assert!(message.contains(&(size - 1).to_string()), "{message}");
        }
        other => panic!("expected a refusal: {other:?}"),
    }
    // Negative: the limit itself is allowed.
    assert!(c.copy_to_memory(size, LONG).is_ok());
}

#[test]
fn a_source_that_grew_past_the_limit_during_the_copy_is_refused_and_nothing_is_kept() {
    let (_dir, path) = file();
    let c = source(&path);
    let page_size = u64::try_from(count(&c, "PRAGMA page_size")).expect("size");
    let pages = c.size().expect("size") / page_size;
    // The size was read as `pages`; what is copied is one page more.
    match c.copy_pages(page_size, pages - 1, Instant::now() + LONG) {
        Err(DbError::Refused(message)) => assert!(message.contains("grew past"), "{message}"),
        other => panic!("expected a refusal: {other:?}"),
    }
    // Negative: at the bound it is copied whole.
    let whole = c
        .copy_pages(page_size, pages, Instant::now() + LONG)
        .expect("whole");
    assert_eq!(count(&whole, "SELECT count(*) FROM t"), 3);
}

#[test]
fn a_connection_inside_a_write_transaction_is_busy_after_the_wait_and_never_hangs() {
    for wal in [false, true] {
        let (_dir, path) = file();
        let c = source(&path);
        if wal {
            assert_eq!(text(&c, "PRAGMA journal_mode = WAL"), "wal");
        }
        c.execute_batch("BEGIN IMMEDIATE")
            .expect("held by the source itself");
        let wait = Duration::from_millis(150);
        let started = Instant::now();
        let error = c.copy_to_memory(BIG, wait).expect_err("busy");
        // It waited the whole wait, and a long bound says it ended: a loaded
        // machine makes it later, not never.
        assert!(started.elapsed() >= wait, "wal {wal}");
        assert!(started.elapsed() < LONG, "wal {wal}");
        assert_eq!(error.server_error_code().as_deref(), Some("5"), "{error}");
        c.execute_batch("ROLLBACK").expect("ended");
        // Negative: free again, it is copied.
        assert!(c.copy_to_memory(BIG, LONG).is_ok(), "wal {wal}");
    }
}

#[test]
fn another_connections_uncommitted_write_is_not_in_the_copy_and_a_read_transaction_is_copied() {
    for wal in [false, true] {
        let (_dir, path) = file();
        let c = source(&path);
        if wal {
            assert_eq!(text(&c, "PRAGMA journal_mode = WAL"), "wal");
        }
        let other = open(&path);
        other
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO t(b) VALUES ('pending')")
            .expect("an open write");
        c.execute_batch("BEGIN").expect("a read transaction");
        assert_eq!(count(&c, "SELECT count(*) FROM t"), 3);
        let copy = c.copy_to_memory(BIG, LONG).expect("copied");
        assert_eq!(count(&copy, "SELECT count(*) FROM t"), 3, "wal {wal}");
        c.execute_batch("ROLLBACK").expect("ended");
        other.execute_batch("COMMIT").expect("committed");
        // Negative: once committed, a later copy has it.
        let later = c.copy_to_memory(BIG, LONG).expect("copied");
        assert_eq!(count(&later, "SELECT count(*) FROM t"), 4, "wal {wal}");
    }
}

#[test]
fn a_writer_in_the_middle_never_shows_a_copy_half_a_transaction() {
    for wal in [false, true] {
        let (_dir, path) = file();
        let c = open(&path);
        if wal {
            assert_eq!(text(&c, "PRAGMA journal_mode = WAL"), "wal");
        }
        c.execute_batch(
            "CREATE TABLE a(n); CREATE TABLE b(n); INSERT INTO a VALUES (0); INSERT INTO b VALUES (0)",
        )
        .expect("two counters");
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (stop, path) = (Arc::clone(&stop), path.clone());
            thread::spawn(move || {
                let w = open(&path);
                w.set_busy_timeout(LONG).expect("wait");
                w.execute_batch("PRAGMA synchronous = OFF")
                    .expect("fast commits");
                let mut commits = 0;
                // Bounded: it ends when told to, and at most this many commits.
                while !stop.load(Ordering::Relaxed) && commits < 2000 {
                    w.execute_batch(
                        "BEGIN IMMEDIATE; UPDATE a SET n = n + 1; UPDATE b SET n = n + 1; COMMIT",
                    )
                    .expect("a commit");
                    commits += 1;
                }
                commits
            })
        };
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..15 {
            let copy = c.copy_to_memory(BIG, LONG).expect("copied");
            let (a, b) = (
                count(&copy, "SELECT n FROM a"),
                count(&copy, "SELECT n FROM b"),
            );
            assert_eq!(a, b, "a copy holds whole transactions only (wal {wal})");
            seen.insert(a);
        }
        stop.store(true, Ordering::Relaxed);
        let commits = writer.join().expect("the writer ended");
        assert!(commits > 0, "the writer really was writing (wal {wal})");
        assert!(!seen.is_empty());
    }
}

/// The copy's bound is counted in pages of the size read before it, so nothing
/// may change that size between the read and the copy.
#[test]
fn while_a_copy_is_made_no_other_connection_can_change_the_page_size() {
    let (_dir, path) = file();
    let c = source(&path);
    let other = open(&path);
    let seen = c
        .pinned(Instant::now() + LONG, || {
            let refused = other
                .execute_batch("PRAGMA page_size = 65536; VACUUM")
                .expect_err("the copy's read lock is held");
            assert_eq!(
                refused.server_error_code().as_deref(),
                Some("5"),
                "{refused}"
            );
            c.number("page_size")
        })
        .expect("pinned");
    assert!(!c.in_transaction(), "the connection is left as it was");
    assert_eq!(c.number("page_size").expect("size"), seen);
    // Inside the caller's own transaction nothing is opened or closed.
    c.execute_batch("BEGIN").expect("the caller's");
    c.pinned(Instant::now() + LONG, || Ok(())).expect("pinned");
    assert!(c.in_transaction());
    c.execute_batch("ROLLBACK").expect("ended");
    // Negative: with no copy in progress the same statement is taken.
    other
        .execute_batch("PRAGMA page_size = 65536; VACUUM")
        .expect("free");
    // (A connection answers with the size it last read: read the file first.)
    count(&c, "SELECT count(*) FROM sqlite_schema");
    assert_eq!(c.number("page_size").expect("size"), 65536);
    // And a size that did change is refused, never copied under a wrong bound.
    match c.copy_pages(seen, u64::from(u32::MAX), Instant::now() + LONG) {
        Err(DbError::Refused(message)) => {
            assert!(message.contains("page size changed"), "{message}")
        }
        other => panic!("expected a refusal: {other:?}"),
    }
}
