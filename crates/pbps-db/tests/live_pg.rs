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

/// `detail()` and the object identifiers `as_db_error()` carries, each
/// exercised where it says something the sentence above it does not — one
/// fixture and one error, so a run under the default parallel test harness
/// cannot race two tests over the same fixture schema.
///
/// A `NOT NULL` violation's `detail` is `"Failing row contains (null)."`
/// (measured on 18.6), which has no overlap with `message()`, so the first
/// assertion fails if `detail()` is dropped. **Measured**, the same
/// violation's `message()` names only the unqualified relation (`"of
/// relation \"nn\""`) — never the schema it lives in — while `schema()`
/// answers the fixture's own schema name; asserting on that name is an
/// assertion the identifiers block must actually have run to satisfy, where
/// `column()`'s `"required"` would not be, since `message()` already
/// contains it. The fixture uses a **named, non-temporary** schema rather
/// than the session's temporary one so there is a schema name worth pinning
/// at all: `pg_temp_NNN`'s number is assigned per backend.
///
/// The schema name carries `std::process::id()`, the convention every
/// non-temporary fixture in `crates/pbps-pg/tests/live.rs` already follows
/// (`pbps_serial_{}`, `pbps_types_{}`, and so on): two runs of this suite
/// against the same reused database would otherwise collide on a fixed name,
/// one dropping the schema out from under the other's still-running
/// assertions. `DROP SCHEMA … CASCADE` up front is safe once the name is
/// unique to this process, and it is what lets a crashed prior run of *this*
/// process leave nothing behind.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_not_null_violation_carries_a_detail_and_a_schema_the_message_does_not() {
    let mut conn = connect().await;
    let schema = format!("issue167_enrichment_{}", std::process::id());
    conn.execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .await
        .expect("clean up any previous run's schema");
    conn.execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .expect("create the fixture schema");
    conn.execute(&format!("CREATE TABLE {schema}.nn (required int NOT NULL)"))
        .await
        .expect("create the fixture table");
    let error = match conn
        .execute(&format!("INSERT INTO {schema}.nn DEFAULT VALUES"))
        .await
    {
        Ok(()) => panic!("a NOT NULL column left to its default of NULL must fail"),
        Err(e) => e,
    };
    conn.execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop the fixture schema");

    let message = error.to_string();
    assert!(
        message.contains("violates not-null constraint"),
        "expected the server's own sentence, got: {message}"
    );
    let primary_sentence = message
        .split("\nOBJECT:")
        .next()
        .expect("splitting on a literal always yields at least one piece");
    assert!(
        !primary_sentence.contains(&schema),
        "the fixture is broken: the schema must not already be in the message \
         or the detail, or the identifier assertion below would pass without \
         the identifiers block ever running: {message}"
    );
    assert!(
        message.contains("DETAIL: Failing row contains (null)."),
        "expected the detail folded in under its own label, got: {message}"
    );
    assert!(
        message.contains(&format!("OBJECT: schema \"{schema}\"")),
        "expected the schema identifier folded in under its own label, got: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("23502"));
}

/// `hint()`, exercised where the server actually sends one: a column name
/// that does not exist but closely matches one that does. **Measured** on
/// 18.6, this is also a shape with no `detail()` and no object identifiers at
/// all, so it isolates `hint()` from every other branch.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_misspelled_column_carries_the_servers_own_hint() {
    let mut conn = connect().await;
    conn.execute("CREATE TEMPORARY TABLE issue167_hint (namee text)")
        .await
        .expect("create the fixture table");
    let error = match conn.query("SELECT name FROM issue167_hint").await {
        Ok(_) => panic!("a column that does not exist must fail"),
        Err(e) => e,
    };
    let message = error.to_string();
    assert!(
        message.contains(r#"column "name" does not exist"#),
        "expected the server's own sentence, got: {message}"
    );
    assert!(
        message
            .contains(r#"HINT: Perhaps you meant to reference the column "issue167_hint.namee"."#),
        "expected the hint folded in under its own label, got: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("42703"));
}

/// `where_()` — `CONTEXT:` in `psql`'s own vocabulary — exercised for an
/// error raised inside a PL/pgSQL function. **Measured** on 18.6, this shape
/// carries `where_()` and none of `detail()`, `hint()`, or an object
/// identifier, isolating it the same way the hint case isolates `hint()`.
///
/// The function name carries `std::process::id()` for the reason
/// [`a_not_null_violation_carries_a_detail_and_a_schema_the_message_does_not`]'s
/// schema does: `CREATE OR REPLACE FUNCTION` is not temporary — there is no
/// `pg_temp` equivalent for functions this fixture uses — so a fixed name
/// would let two runs against the same database race `CREATE OR REPLACE`
/// against `DROP FUNCTION`.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn an_exception_inside_a_function_carries_its_call_stack_as_context() {
    let mut conn = connect().await;
    let function = format!("issue167_ctx_{}", std::process::id());
    conn.execute(&format!(
        "CREATE OR REPLACE FUNCTION {function}() RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
           RAISE EXCEPTION 'boom from function';
         END;
         $$"
    ))
    .await
    .expect("create the fixture function");
    let error = match conn.query(&format!("SELECT {function}()")).await {
        Ok(_) => panic!("a function that raises must fail"),
        Err(e) => e,
    };
    conn.execute(&format!("DROP FUNCTION {function}()"))
        .await
        .expect("drop the fixture function");
    let message = error.to_string();
    assert!(
        message.contains("boom from function"),
        "expected the server's own sentence, got: {message}"
    );
    assert!(
        message.contains(&format!(
            "CONTEXT: PL/pgSQL function {function}() line 3 at RAISE"
        )),
        "expected the call stack folded in under its own label, got: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("P0001"));
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
    // The exact string, not merely "non-empty": a non-empty check would pass
    // for a generic placeholder, or for the very literal `db error` this fix
    // exists to eliminate. **Measured**: `tokio_postgres::Error`'s own
    // `Display` for `Kind::Closed` is exactly `"connection closed"` — the
    // fallback branch renders that unmodified, so this is the one string
    // that pins the fallback actually ran rather than something else
    // entirely.
    assert_eq!(
        message, "connection closed",
        "the fallback must render tokio_postgres's own text for this failure"
    );
    assert_eq!(
        error.server_error_code(),
        None,
        "this failure never reached the server, so it carries no SQLSTATE"
    );
}
