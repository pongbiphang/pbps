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

/// `detail()` and `hint()` never reach the message, even though the object
/// identifiers `as_db_error()` carries beside them do.
///
/// Ready-phase review of PR #464 found the fix's first draft folding
/// `detail()` in unconditionally — and a `NOT NULL` violation's `detail`,
/// `"Failing row contains (null)."`, is one measured shape; a unique
/// violation's is `"Key (id)=(1) already exists."` (DECISIONS 453). Both are
/// **data**, and this crate's caller writes what this function returns to
/// stderr and to the deployment ledger's `reason` column — nowhere a
/// deployer's row values belong. So this asserts the opposite of what an
/// earlier draft of this test did: the detail must be *absent*, specifically
/// (not merely "the test no longer checks for it" — a regression that put it
/// back would still pass a test that only stopped looking), while the schema
/// identifier `message()` never states on its own must still be *present*,
/// proving the redaction removed the data-bearing field without silently
/// disabling the safe one beside it.
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
async fn a_not_null_violations_detail_never_reaches_the_message() {
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
    assert!(
        !message.contains("DETAIL") && !message.contains("Failing row"),
        "the server's DETAIL carries a data-bearing row value and must not \
         reach the message this tool logs and writes into the ledger: {message}"
    );
    assert!(
        message.contains(&format!("OBJECT: schema \"{schema}\"")),
        "expected the schema identifier to survive the redaction, got: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("23502"));
}

/// `hint()` never reaches the message either, measured against a case where
/// the server's hint is ordinary advice — the risk is not that advice is
/// itself sensitive, but that PostgreSQL sets no boundary on what a `hint`
/// can say: `RAISE ... USING HINT = format(...)` in a data trigger this
/// tool's own guard exists to police (`crates/pbps-pg/src/data_triggers.rs`)
/// can interpolate anything, including a rejected row's own value, and this
/// seam cannot tell that shape apart from `psql`'s example (**measured** on
/// 18.6: `HINT:  the offending value was alice@example.com` from
/// `RAISE EXCEPTION '...' USING HINT = format('the offending value was %s',
/// v)`).
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_misspelled_columns_hint_never_reaches_the_message() {
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
        !message.contains("HINT") && !message.contains("Perhaps you meant"),
        "a hint is free text a user's own PL/pgSQL controls and must not \
         reach the message this tool logs and writes into the ledger: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("42703"));
}

/// `where_()` never reaches the message, exercised first for the safe-looking
/// shape — a bare call stack, `"PL/pgSQL function f() line N at RAISE"`, with
/// no embedded statement text at all — to prove the redaction is
/// unconditional and not a filter that happens to catch only the dangerous
/// shape.
///
/// The function name carries `std::process::id()` for the reason
/// [`a_not_null_violations_detail_never_reaches_the_message`]'s schema does:
/// `CREATE OR REPLACE FUNCTION` is not temporary — there is no `pg_temp`
/// equivalent for functions this fixture uses — so a fixed name would let two
/// runs against the same database race `CREATE OR REPLACE` against
/// `DROP FUNCTION`.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn an_exceptions_call_stack_never_reaches_the_message() {
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
        !message.contains("CONTEXT") && !message.contains("PL/pgSQL function"),
        "the call stack must not reach the message even in this shape, which \
         carries no embedded statement text at all: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("P0001"));
}

/// `where_()`'s dangerous shape, measured directly: a statement that fails
/// *inside* a function carries that statement's own text, literals and all
/// (`CONTEXT:  SQL statement "INSERT INTO t VALUES ('secret')"`), and this
/// seam holds no SQL grammar to strip the literal while keeping the frame —
/// constraint 9, the same reason `data_triggers.rs` parses no SQL either.
/// This is the shape the ready-phase review named; the fixture writes a
/// distinctive value expressly so the assertion below cannot pass by
/// accident of the value being short or common.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_statement_literal_inside_a_function_never_reaches_the_message() {
    let mut conn = connect().await;
    let schema = format!("issue167_leak_{}", std::process::id());
    conn.execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .await
        .expect("clean up any previous run's schema");
    conn.execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .expect("create the fixture schema");
    conn.execute(&format!(
        "CREATE TABLE {schema}.secrets (secret text PRIMARY KEY)"
    ))
    .await
    .expect("create the fixture table");
    conn.execute(&format!(
        "CREATE FUNCTION {schema}.leak() RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO {schema}.secrets VALUES ('super-secret-value-167');
           INSERT INTO {schema}.secrets VALUES ('super-secret-value-167');
         END;
         $$"
    ))
    .await
    .expect("create the fixture function");
    let error = match conn.query(&format!("SELECT {schema}.leak()")).await {
        Ok(_) => panic!("a duplicate key inside the function must fail"),
        Err(e) => e,
    };
    conn.execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop the fixture schema");

    let message = error.to_string();
    assert!(
        message.contains("duplicate key value violates unique constraint"),
        "expected the server's own sentence, got: {message}"
    );
    assert!(
        !message.contains("super-secret-value-167"),
        "the row's own value must never reach the message this tool logs \
         and writes into the ledger: {message}"
    );
    assert!(
        !message.contains("DETAIL") && !message.contains("CONTEXT"),
        "neither of the two fields that could have carried the value above \
         may reach the message at all: {message}"
    );
    assert!(
        message.contains(&format!("OBJECT: schema \"{schema}\""))
            && message.contains("table \"secrets\"")
            && message.contains("constraint \"secrets_pkey\""),
        "expected the identifiers to survive the redaction, got: {message}"
    );
    assert_eq!(error.server_error_code().as_deref(), Some("23505"));
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
