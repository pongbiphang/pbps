//! What only a live SQL Server can answer about this seam's error conversion
//! (issue #167 asked for this side to be checked, not assumed).
//!
//! Unlike `postgres.rs`, `From<tiberius::error::Error> for DbError` already
//! renders the server's own sentence: `tiberius`'s `TokenError` carries its
//! `message` field straight into its own `Display`, so `e.to_string()` was
//! never `db error` here. This is the regression guard for that fact, not a
//! fix — see `mssql.rs` for where the two drivers were compared.
//!
//! `#[ignore]`d because it needs a server. Run it with `scripts/live-tests.sh`,
//! or start one yourself and:
//!
//! ```text
//! export PBPS_TEST_DB='Server=localhost,14340;User Id=sa;Password=...;TrustServerCertificate=true'
//! cargo test -p pbps-db --test live_mssql -- --ignored
//! ```

use pbps_db::{Conn, Driver};

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_server_refusal_already_carries_the_servers_own_sentence() {
    let mut conn = Conn::connect(Driver::Mssql, &conn_str())
        .await
        .expect("connect to the live server");
    let error = match conn.query("SELECT * FROM no_such_table").await {
        Ok(_) => panic!("querying a table that does not exist must fail"),
        Err(e) => e,
    };
    let message = error.to_string();
    assert!(
        message.contains("no_such_table"),
        "expected the server's own sentence naming the missing object, got: {message}"
    );
    assert_ne!(
        message, "db error",
        "tiberius's Display was never `db error`; this pins that it stays that way"
    );
    // 208: `invalid object name` (DECISIONS 193's own example of this seam's
    // text SQLSTATE-shaped code).
    assert_eq!(error.server_error_code().as_deref(), Some("208"));
}

fn conn_str() -> String {
    std::env::var("PBPS_TEST_DB").expect(
        "PBPS_TEST_DB is not set; these tests need a live SQL Server (see scripts/live-tests.sh)",
    )
}
