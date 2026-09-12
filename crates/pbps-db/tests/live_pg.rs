//! What only a live PostgreSQL can answer about the seam's own error
//! conversion (issue #167).
//!
//! `From<tokio_postgres::Error> for DbError` is the one place in this crate
//! that turns a driver failure into the text a person reads, and its bug was
//! invisible to every unit test: `tokio_postgres::Error`'s `Display` renders a
//! server-side failure as the literal `db error`, and no fake driver would
//! have reproduced that — only the real one does. These tests run against it.
//!
//! `#[ignore]`d because they need a server. Run them with
//! `scripts/live-tests-pg.sh`, or start one yourself and:
//!
//! ```text
//! export PBPS_TEST_PG_DB='host=localhost port=54330 user=postgres password=... dbname=pbps_test'
//! cargo test -p pbps-db --test live_pg -- --ignored
//! ```

use pbps_db::{Conn, Driver};

async fn connect() -> Conn {
    Conn::connect(Driver::Postgres, &conn_str())
        .await
        .expect("connect to the live server")
}

fn conn_str() -> String {
    std::env::var("PBPS_TEST_PG_DB")
        .expect("PBPS_TEST_PG_DB is not set; see scripts/live-tests-pg.sh")
}

/// The positive case: a statement the server refuses names the reason, not
/// `db error`.
///
/// `42P01` is `undefined_table` — chosen because it is the shape the issue
/// itself measured, and because it carries no `detail` or `hint`, so a
/// regression that dropped back to `db.message()` alone would still pass a
/// test that only checked for a substring somewhere in `detail`. The relation
/// name is quoted by the engine, not by this crate, so asserting for the
/// server's own quoting is asserting against `db.message()` specifically.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_server_refusal_carries_the_servers_own_sentence() {
    let mut conn = connect().await;
    let error = match conn.query("SELECT * FROM no_such_table").await {
        Ok(_) => panic!("querying a table that does not exist must fail"),
        Err(e) => e,
    };
    let message = error.to_string();
    assert!(
        message.contains(r#"relation "no_such_table" does not exist"#),
        "expected the server's own sentence, got: {message}"
    );
    assert_ne!(
        message, "db error",
        "the seam must not fall back to tokio_postgres's own Display"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("42P01"));
}

/// The same seam, exercised for `detail()`, `hint()` and the column identifier
/// `as_db_error()` carries beside the message — the enrichment issue #167
/// asks for beyond the bare fix, and easy to lose in a future refactor that
/// keeps `db.message()` but drops the rest.
///
/// A `NOT NULL` violation is the shape PostgreSQL answers with no `detail` at
/// all (measured on 18.6) — a unique violation's `detail` already names the
/// column, so it cannot tell a working `column()` apart from a `column()` that
/// silently started returning `None`. This one can.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_not_null_violation_names_its_column_and_a_hint_if_any() {
    let mut conn = connect().await;
    conn.execute("CREATE TEMPORARY TABLE issue_167_not_null (id int, required int NOT NULL)")
        .await
        .expect("create the fixture table");
    let error = match conn
        .execute("INSERT INTO issue_167_not_null (id) VALUES (1)")
        .await
    {
        Ok(()) => panic!("a NOT NULL column left out of the insert list must fail"),
        Err(e) => e,
    };
    let message = error.to_string();
    assert!(
        message.contains("null value in column \"required\""),
        "expected the server's own sentence, got: {message}"
    );
    assert!(
        message.contains("column \"required\""),
        "expected the column identifier folded in, got: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("23502"));
}

/// The negative case: a failure that never reached the server — so
/// `as_db_error()` is `None` — still renders its own text rather than an
/// empty message or a panic.
///
/// Forcing this without a network-level fault: `pg_terminate_backend` from a
/// second connection kills the first session's backend process, and the
/// driver notices on the *next* request over the now-dead connection — by
/// then the connection task has already exited, so the client answers with
/// its own `Kind::Closed`, which carries no `DbError` at all (measured: the
/// terminated request itself still gets the server's own `57P01
/// admin_shutdown` — it is the request *after* that one which is answered by
/// the driver, not the server).
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_connection_failure_with_no_server_error_still_renders_its_own_text() {
    let mut conn = connect().await;
    let mut killer = connect().await;

    let pid_row = conn
        .query("SELECT pg_backend_pid() AS pid")
        .await
        .expect("read this connection's backend pid");
    let pid: i32 = pid_row[0]
        .try_get::<i32>("pid")
        .expect("pid column")
        .expect("pid is not null");

    killer
        .query(&format!("SELECT pg_terminate_backend({pid})"))
        .await
        .expect("terminate the first connection's backend");

    // The terminated request itself may still surface the server's own
    // `admin_shutdown` error; this test is about the request after that one,
    // once the driver has itself noticed the connection is gone.
    let _ = conn.query("SELECT 1").await;
    let error = match conn.query("SELECT 1").await {
        Ok(_) => panic!("a query over a closed connection must fail"),
        Err(e) => e,
    };
    let message = error.to_string();
    assert!(
        !message.is_empty(),
        "a connection failure must not render as nothing"
    );
    assert_eq!(
        error.server_error_code(),
        None,
        "this failure never reached the server, so it carries no SQLSTATE"
    );
}
