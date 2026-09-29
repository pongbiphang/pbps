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

/// #861: the 0.13 driver added a 30-second command deadline. Deployment
/// statements previously had none, so a server response past that default
/// must remain usable through the ordinary connection path.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_slow_server_statement_does_not_gain_a_driver_deadline() {
    let mut conn = Conn::connect(Driver::Mssql, &conn_str())
        .await
        .expect("connect to the live server");
    let rows = conn
        .query("WAITFOR DELAY '00:00:31'; SELECT CAST(861 AS INT) AS value")
        .await
        .expect("a 31-second response must not time out");
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(861));
}

fn conn_str() -> String {
    std::env::var("PBPS_TEST_DB").expect(
        "PBPS_TEST_DB is not set; these tests need a live SQL Server (see scripts/live-tests.sh)",
    )
}

async fn application_name(connection_string: &str) -> String {
    let mut conn = Conn::connect(Driver::Mssql, connection_string)
        .await
        .expect("connect to the live server");
    conn.query("SELECT APP_NAME()").await.unwrap()[0]
        .try_get_at::<&str>(0)
        .unwrap()
        .unwrap()
        .to_owned()
}

/// #1188: a session this process opens carries its application name, so a
/// lock holder's sessions can be found; a name the connection string asks
/// for, under either key the driver reads, is kept rather than overwritten.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_session_is_named_for_this_process_unless_the_string_names_it() {
    let base = conn_str();
    let base = base.trim_end_matches(';');
    let named = application_name(base).await;
    assert_eq!(named, pbps_db::session_application_name());
    assert!(
        named.starts_with(&format!("pbps/{}/", std::process::id())),
        "{named}"
    );
    for key in ["Application Name", "ApplicationName"] {
        let own = application_name(&format!("{base};{key}=ops-dashboard")).await;
        assert_eq!(own, "ops-dashboard", "{key}");
    }
}
