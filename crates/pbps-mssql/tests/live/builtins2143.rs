//! Only what the engine ships is a built-in a capture's closure may hand to
//! another capture as a root (DEC-2143.6).
use super::*;
use pbps_mssql::resolver::capture;
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn engine_builtins_are_the_shipped_types_and_never_a_user_defined_one() {
    let mut db = TestDb::create("builtins2143").await;
    db.conn
        .execute("CREATE TYPE dbo.amount2143 FROM int NOT NULL;")
        .await
        .unwrap();
    db.conn
        .execute("CREATE TABLE dbo.t2143(a dbo.amount2143, b int, c nvarchar(10));")
        .await
        .unwrap();
    let captured = capture::capture(
        &mut db.conn,
        &capture::CaptureScope {
            retained: BTreeSet::from([capture::object("dbo", "t2143")]),
            candidates: BTreeSet::new(),
        },
        &capture_key(),
    )
    .await
    .unwrap();
    let builtins: BTreeSet<String> = captured
        .engine_builtins()
        .iter()
        .map(|o| o.name.join("."))
        .collect();
    assert!(
        builtins.contains("sys.int") && builtins.contains("sys.nvarchar"),
        "{builtins:?}"
    );
    assert!(!builtins.contains("dbo.amount2143"), "{builtins:?}");
    // A retained root is not handed on again.
    let again = capture::capture(
        &mut db.conn,
        &capture::CaptureScope {
            retained: BTreeSet::from([capture::object("dbo", "t2143")])
                .union(&captured.engine_builtins())
                .cloned()
                .collect(),
            candidates: BTreeSet::new(),
        },
        &capture_key(),
    )
    .await
    .unwrap();
    assert!(again.engine_builtins().is_empty());
    db.drop().await;
}
