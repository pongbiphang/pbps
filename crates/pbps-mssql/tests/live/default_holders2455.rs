//! DEFAULT dependencies are catalog bindings, with unreadable distinct from empty.
use super::*;
use pbps_diff::order::catalog::{Holds, Part};
use pbps_model::ModuleId;

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn default_holder_capture_is_complete_deduplicated_and_never_hides_unreadable_evidence() {
    let mut db = TestDb::create("default_holders2455").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    db.conn
        .execute("CREATE FUNCTION app.f() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN 7; END")
        .await
        .unwrap();
    db.conn
        .execute("CREATE FUNCTION app.other() RETURNS int AS BEGIN RETURN 9; END")
        .await
        .unwrap();
    db.conn.execute("CREATE TABLE app.t (a int DEFAULT (app.f()+app.f()), b int DEFAULT (app.f()), ordinary int DEFAULT 1, unrelated int DEFAULT (app.other())); CREATE USER default_reader WITHOUT LOGIN; GRANT VIEW DEFINITION TO default_reader; GRANT SELECT ON OBJECT::sys.sql_expression_dependencies TO default_reader;").await.unwrap();
    let root = ModuleId::Named(TableName::new("APP", "F"));
    let read =
        pbps_mssql::catalog::module_default_dependents(&mut db.conn, std::slice::from_ref(&root))
            .await
            .unwrap();
    assert_eq!(read[&root].len(), 2, "{read:?}");
    let columns: Vec<_> = read[&root]
        .iter()
        .map(|d| match &d.holds {
            Holds::TablePart {
                table,
                part: Part::Default(column),
            } => {
                assert_eq!(table, &TableName::new("app", "t"));
                column.raw()
            }
            Holds::Module(_) | Holds::TablePart { .. } | Holds::Unrepresentable(_) => {
                panic!("not an ordinary DEFAULT: {d:?}")
            }
        })
        .collect();
    assert_eq!(columns, ["a", "b"]);
    db.conn
        .execute("CREATE FUNCTION app.unused() RETURNS int AS BEGIN RETURN 1; END")
        .await
        .unwrap();
    let unused = ModuleId::Named(TableName::new("app", "unused"));
    assert!(
        pbps_mssql::catalog::module_default_dependents(&mut db.conn, std::slice::from_ref(&unused))
            .await
            .unwrap()[&unused]
            .is_empty()
    );
    let absent = ModuleId::Named(TableName::new("app", "absent"));
    let error = pbps_mssql::catalog::module_default_dependents(&mut db.conn, &[absent])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("could not be read") && error.contains("app.absent"),
        "{error}"
    );
    for (deny, expected) in [
        (
            "DENY VIEW DEFINITION ON OBJECT::app.t TO default_reader",
            "hidden from this login",
        ),
        (
            "REVOKE VIEW DEFINITION ON OBJECT::app.t FROM default_reader; DENY SELECT ON OBJECT::sys.sql_expression_dependencies TO default_reader",
            "229",
        ),
        (
            "REVOKE SELECT ON OBJECT::sys.sql_expression_dependencies FROM default_reader; REVOKE VIEW DEFINITION FROM default_reader",
            "does not hold database VIEW DEFINITION",
        ),
    ] {
        db.conn.execute(deny).await.unwrap();
        db.conn
            .execute("EXECUTE AS USER='default_reader'")
            .await
            .unwrap();
        let error = pbps_mssql::catalog::module_default_dependents(
            &mut db.conn,
            std::slice::from_ref(&root),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            pbps_mssql::catalog::module_default_dependents(&mut db.conn, &[])
                .await
                .unwrap()
                .is_empty()
        );
        db.conn.execute("REVERT").await.unwrap();
        assert!(error.contains(expected), "{error}");
    }
    db.drop().await;
}
