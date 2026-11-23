//! `Conn` over a SQLite file, against a real SQLite: what opens and what
//! creates, which calls answer and which refuse, the transaction's checks
//! through the three separate calls, and the slot a lock guard is parked in.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pbps_dialect::TransactionFraming;

use crate::{Conn, DbError, Driver, Param};

const IMMEDIATE: TransactionFraming = TransactionFraming {
    begin: Cow::Borrowed("BEGIN IMMEDIATE"),
    commit: Cow::Borrowed("COMMIT"),
    rollback: Cow::Borrowed("ROLLBACK"),
};

/// A directory of this test's own, removed when it goes out of scope.
struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path() -> (Dir, String) {
    let dir = std::env::temp_dir().join(format!("pbps-conn-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&dir).expect("a temporary directory");
    let file = dir
        .join("app.db")
        .to_str()
        .expect("a UTF-8 path")
        .to_owned();
    (Dir(dir), file)
}

fn created() -> (Dir, Conn) {
    let (dir, file) = path();
    let conn = Conn::create(Driver::Sqlite, &file).expect("creates");
    (dir, conn)
}

fn integer(conn: &mut Conn, sql: &str) -> i64 {
    conn.sqlite()
        .expect("a SQLite connection")
        .query(sql, &[])
        .expect("query")[0]
        .integer(0)
        .expect("integer")
}

fn refused(result: Result<impl Sized, DbError>) -> String {
    match result {
        Err(DbError::Refused(message)) => message,
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("expected a refusal, got an answer"),
    }
}

#[tokio::test]
async fn connecting_never_creates_the_file_and_creating_is_its_own_call() {
    let (_dir, file) = path();
    // A mistyped path stays a refusal, and nothing appears on disk.
    assert!(Conn::connect(Driver::Sqlite, &file).await.is_err());
    assert!(
        Conn::connect_named(Driver::Sqlite, &file, "pbps")
            .await
            .is_err()
    );
    assert!(!std::path::Path::new(&file).exists());

    let mut conn = Conn::create(Driver::Sqlite, &file).expect("creates");
    assert_eq!(conn.driver(), Driver::Sqlite);
    assert!(conn.session_zone().is_none());
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .await
        .expect("ddl");
    drop(conn);

    // Now it exists, and both entries open it as it is.
    let mut again = Conn::connect(Driver::Sqlite, &file).await.expect("opens");
    assert_eq!(integer(&mut again, "SELECT count(*) FROM sqlite_schema"), 1);
    drop(again);
    let mut kept = Conn::create(Driver::Sqlite, &file).expect("opens an existing file");
    assert_eq!(integer(&mut kept, "SELECT count(*) FROM sqlite_schema"), 1);

    // Negative: a server's database is not created here, and SQLite has no
    // other database to connect to on "the same server".
    for server in [Driver::Postgres, Driver::Mssql] {
        assert!(refused(Conn::create(server, "anything")).contains("SQLite only"));
    }
    assert!(
        refused(Conn::connect_with(Driver::Sqlite, &file, "other", None).await)
            .contains("PostgreSQL only")
    );
}

#[tokio::test]
async fn statements_run_and_bind_and_every_row_returning_call_refuses_by_name() {
    let (_dir, mut conn) = created();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, note TEXT)")
        .await
        .expect("ddl");
    let changed = conn
        .execute_with(
            "INSERT INTO t (id, name, note) VALUES (?1, ?2, ?3), (?4, ?5, ?6)",
            &[
                Param::I32(1),
                Param::Str("o'brien"),
                Param::OptStr(None),
                Param::I64(1 << 40),
                Param::Str("b"),
                Param::OptStr(Some("kept")),
            ],
        )
        .await
        .expect("bound insert");
    assert_eq!(changed, 2);
    assert_eq!(
        integer(
            &mut conn,
            "SELECT count(*) FROM t WHERE name = 'o''brien' AND note IS NULL"
        ),
        1
    );
    assert_eq!(
        integer(&mut conn, "SELECT id FROM t WHERE note = 'kept'"),
        1 << 40
    );

    // No `Row` exists for SQLite, so each of these says so instead of
    // returning something read by a name the record does not have.
    for message in [
        refused(conn.query("SELECT 1").await),
        refused(conn.query_with("SELECT ?1", &[Param::I32(1)]).await),
        refused(conn.prepare_query("SELECT 1").await.map(|_| ())),
    ] {
        assert!(message.contains("SQLite connection"), "{message}");
    }
    let message = refused(
        conn.query_bounded("SELECT 1", std::time::Duration::from_secs(1))
            .await,
    );
    assert!(message.contains("SQLite connection"), "{message}");
}

#[tokio::test]
async fn the_three_transaction_calls_keep_the_checks_of_the_closure_form() {
    let (_dir, mut conn) = created();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .await
        .expect("ddl");

    // Foreign keys switched off are back on before the transaction opens:
    // inside it the pragma would be ignored without an error.
    conn.execute("PRAGMA foreign_keys = OFF")
        .await
        .expect("pragma");
    assert_eq!(integer(&mut conn, "PRAGMA foreign_keys"), 0);
    conn.begin(&IMMEDIATE).await.expect("begins");
    assert_eq!(integer(&mut conn, "PRAGMA foreign_keys"), 1);

    // A second begin is refused, and the first transaction is still open.
    assert!(refused(conn.begin(&IMMEDIATE).await).contains("already open"));
    conn.execute("INSERT INTO t VALUES (1)")
        .await
        .expect("insert");
    conn.rollback(&IMMEDIATE).await.expect("rolls back");
    assert_eq!(integer(&mut conn, "SELECT count(*) FROM t"), 0);

    conn.begin(&IMMEDIATE).await.expect("begins again");
    conn.execute("INSERT INTO t VALUES (2)")
        .await
        .expect("insert");
    conn.commit(&IMMEDIATE).await.expect("commits");
    assert_eq!(integer(&mut conn, "SELECT count(*) FROM t"), 1);

    // Negative: with nothing open, a rollback has nothing to undo and is not
    // an error, so it cannot replace the failure that led to it; a commit is.
    conn.rollback(&IMMEDIATE).await.expect("tolerated");
    assert!(conn.commit(&IMMEDIATE).await.is_err());
    assert!(!conn.sqlite().expect("sqlite").in_transaction());
}

/// Sets its flag when dropped, as a lock guard releases its lock.
struct Guard(Arc<AtomicBool>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_parked_value_lives_as_long_as_the_connection_and_is_never_replaced() {
    let (_dir, mut conn) = created();
    let (first, second) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let sqlite = conn.sqlite().expect("a SQLite connection");
    assert!(sqlite.unpark().is_none());
    assert!(sqlite.park(Box::new(Guard(first.clone()))).is_ok());

    // A second value is handed back, not swapped in: replacing a guard would
    // release the lock it holds.
    let back = sqlite
        .park(Box::new(Guard(second.clone())))
        .expect_err("refused");
    assert!(!first.load(Ordering::SeqCst));
    drop(back);
    assert!(second.load(Ordering::SeqCst) && !first.load(Ordering::SeqCst));

    // Taken out, it is the value that went in, and the slot is free again.
    let taken = sqlite.unpark().expect("parked");
    assert!(taken.downcast_ref::<Guard>().is_some());
    assert!(!first.load(Ordering::SeqCst));
    assert!(sqlite.park(taken).is_ok());

    // Dropping the connection drops what is parked on it.
    drop(conn);
    assert!(first.load(Ordering::SeqCst));
}
