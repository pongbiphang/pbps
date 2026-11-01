//! Native target boundary, qualified while the database remains READ_WRITE.
use super::*;
use pbps_db::{Driver, query::EvidenceCell};
use pbps_mssql::assertions::{AssertionResult, Target, run_target};
use std::time::Duration;
const DEADLINE: Duration = Duration::from_secs(10);
struct Fixture {
    db: TestDb,
    role: String,
    assertion: String,
}
impl Fixture {
    async fn new(tag: &str) -> Self {
        let mut db = TestDb::create(tag).await;
        let role = format!("{}_reader", db.name);
        db.conn.execute(&format!("CREATE LOGIN [{role}] WITH PASSWORD='OnlyAssert!2401',CHECK_POLICY=OFF; CREATE USER [{role}] FOR LOGIN [{role}]; GRANT VIEW DEFINITION TO [{role}]")).await.unwrap();
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute(&format!("CREATE TABLE app.orders(id int,total int); INSERT INTO app.orders VALUES(1,2401); GRANT SELECT ON app.orders TO [{role}]")).await.unwrap();
        let base = conn_str();
        let parts: Vec<_> = base
            .split(';')
            .filter(|part| {
                ![
                    "user id",
                    "userid",
                    "uid",
                    "password",
                    "pwd",
                    "database",
                    "initial catalog",
                ]
                .contains(
                    &part
                        .split('=')
                        .next()
                        .unwrap()
                        .trim()
                        .to_ascii_lowercase()
                        .as_str(),
                )
            })
            .collect();
        let assertion = format!(
            "{};User Id={role};Password=OnlyAssert!2401;Database={}",
            parts.join(";"),
            db.name
        );
        Self {
            db,
            role,
            assertion,
        }
    }
    async fn target(&mut self) -> Target {
        Target::read(&mut self.db.conn).await.unwrap()
    }
    async fn run(&mut self, sql: &str) -> Result<AssertionResult, pbps_db::DbError> {
        let target = self.target().await;
        run_target(&self.assertion, &target, sql, DEADLINE).await
    }
    async fn finish(self) {
        self.db.drop().await;
        let mut admin = Conn::connect(Driver::Mssql, &conn_str()).await.unwrap();
        admin
            .execute(&format!("DROP LOGIN [{}]", self.role))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_assertion_login_is_checked_by_effective_object_column_and_ddl_permissions() {
    let mut f = Fixture::new("assert_priv2401").await;
    assert!(matches!(
        f.run("SELECT 1 WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    let role = f.role.clone();
    f.db.conn
        .execute(&format!(
            "CREATE ROLE writer; ALTER ROLE writer ADD MEMBER [{role}]"
        ))
        .await
        .unwrap();
    for (grant, revoke, permission) in [
        (
            format!("GRANT UPDATE ON app.orders TO [{role}]"),
            format!("REVOKE UPDATE ON app.orders FROM [{role}]"),
            "UPDATE",
        ),
        (
            format!("GRANT UPDATE ON app.orders(total) TO [{role}]"),
            format!("REVOKE UPDATE ON app.orders(total) FROM [{role}]"),
            "UPDATE",
        ),
        (
            "GRANT DELETE ON app.orders TO writer".into(),
            "REVOKE DELETE ON app.orders FROM writer".into(),
            "DELETE",
        ),
        (
            format!(
                "USE master; GRANT IMPERSONATE ON LOGIN::sa TO [{role}]; USE [{}]",
                f.db.name
            ),
            format!(
                "USE master; REVOKE IMPERSONATE ON LOGIN::sa FROM [{role}]; USE [{}]",
                f.db.name
            ),
            "IMPERSONATE",
        ),
        (
            format!("GRANT IMPERSONATE ON USER::dbo TO [{role}]"),
            format!("REVOKE IMPERSONATE ON USER::dbo FROM [{role}]"),
            "IMPERSONATE",
        ),
        (
            format!("GRANT CREATE TABLE TO [{role}]"),
            format!("REVOKE CREATE TABLE FROM [{role}]"),
            "CREATE TABLE",
        ),
        (
            format!("GRANT ALTER ON SCHEMA::app TO [{role}]"),
            format!("REVOKE ALTER ON SCHEMA::app FROM [{role}]"),
            "permission on SCHEMA",
        ),
        (
            format!("ALTER AUTHORIZATION ON OBJECT::app.orders TO [{role}]"),
            "ALTER AUTHORIZATION ON OBJECT::app.orders TO dbo".into(),
            "permission on OBJECT",
        ),
        (
            format!("ALTER ROLE db_owner ADD MEMBER [{role}]"),
            format!("ALTER ROLE db_owner DROP MEMBER [{role}]"),
            "permission on",
        ),
    ] {
        f.db.conn.execute(&grant).await.unwrap();
        let error = f.run("SELECT 1/0").await.unwrap_err();
        assert!(error.to_string().contains(permission), "{grant}: {error:?}");
        f.db.conn.execute(&revoke).await.unwrap();
    }
    // An inherited grant defeated by DENY is not an effective write right.
    f.db.conn
        .execute(&format!(
            "GRANT UPDATE ON app.orders TO writer; DENY UPDATE ON app.orders TO [{role}]"
        ))
        .await
        .unwrap();
    assert!(matches!(
        f.run("SELECT 1 WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.db.conn.execute(&format!("REVOKE UPDATE ON app.orders FROM writer; REVOKE UPDATE ON app.orders FROM [{role}]; ALTER ROLE writer DROP MEMBER [{role}]; DROP ROLE writer")).await.unwrap();
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn target_queries_distinguish_zero_rows_nulls_sql_errors_and_invalid_framing() {
    let mut f = Fixture::new("assert_query2401").await;
    for sql in [
        "SELECT 1 WHERE 1=0",
        "SELECT id FROM app.orders WHERE total<0",
        ";WITH c AS (SELECT 1 n) SELECT n FROM c WHERE n=0",
        "SELECT '; COMMIT' AS [a;b] WHERE 1=0;",
        "SELECT CASE WHEN 1=1 THEN 1 END WHERE 1=0",
    ] {
        assert!(
            matches!(f.run(sql).await.unwrap(), AssertionResult::Pass(_)),
            "{sql}"
        );
    }
    for sql in [
        "SELECT NULL AS n",
        "SELECT CAST(0 AS bit)",
        "SELECT ''",
        "SELECT CAST('<a/>' AS xml)",
    ] {
        assert!(
            matches!(f.run(sql).await.unwrap(), AssertionResult::Violation(_)),
            "{sql}"
        );
    }
    let AssertionResult::Violation(e) = f
        .run("SELECT NULL n,CAST(0 AS bit) b,N'€' txt, CAST(NULL AS datetime) moment")
        .await
        .unwrap()
    else {
        panic!("missing violation")
    };
    assert_eq!(e.rows[0][0], EvidenceCell::Null);
    assert_eq!(e.rows[0][3], EvidenceCell::Null);
    for sql in [
        "SELECT 1/0",
        "SELECT * FROM app.missing",
        "INSERT INTO app.orders VALUES(2,2)",
        "SELECT 1 n INTO app.forbidden",
    ] {
        assert!(
            matches!(f.run(sql).await.unwrap(), AssertionResult::SqlError(_)),
            "{sql}"
        );
    }
    for sql in [
        "",
        "SELECT 1; SELECT 2",
        "SELECT 1 WHERE 1=0; SELECT 1/0",
        "SELECT 1 SELECT 2",
        "COMMIT; INSERT INTO app.orders VALUES(2,2)",
        "SELECT 1; EXEC app.writer",
        "SELECT 1\nGO\nSELECT 2",
    ] {
        assert!(
            matches!(
                f.run(sql).await.unwrap(),
                AssertionResult::InvalidResult { .. }
            ),
            "{sql}"
        );
    }
    for (name, body) in [
        (
            "many",
            "BEGIN SELECT 1 n WHERE 1=0; SELECT 2 n WHERE 1=0; END",
        ),
        ("none", "RETURN 0"),
    ] {
        f.db.conn
            .execute(&format!("CREATE PROCEDURE dbo.{name} AS {body}"))
            .await
            .unwrap();
        let admin = format!("{};Database={}", conn_str(), f.db.name);
        let evidence = Conn::connect(Driver::Mssql, &admin)
            .await
            .unwrap()
            .query_bounded(&format!("EXEC dbo.{name}"), DEADLINE)
            .await
            .unwrap();
        assert!(matches!(
            AssertionResult::from_evidence(evidence),
            AssertionResult::InvalidResult { .. }
        ));
    }
    let row =
        f.db.conn
            .query("SELECT COUNT(*) AS n FROM app.orders")
            .await
            .unwrap();
    assert_eq!(row[0].try_get::<i32>("n").unwrap(), Some(1));
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn wrong_targets_and_incomplete_metadata_are_refused_before_assertion_sql() {
    let mut f = Fixture::new("assert_target2401").await;
    let mut other = Fixture::new("assert_other2401").await;
    let target = other.target().await;
    assert!(
        run_target(&f.assertion, &target, "SELECT 1/0", DEADLINE)
            .await
            .unwrap_err()
            .to_string()
            .contains("target database")
    );
    let target = f.target().await;
    let admin = format!("{};Database={}", conn_str(), f.db.name);
    assert!(
        run_target(&admin, &target, "SELECT 1/0", DEADLINE)
            .await
            .unwrap_err()
            .to_string()
            .contains("separate assertion login")
    );
    f.db.conn
        .execute(&format!("REVOKE VIEW DEFINITION FROM [{}]", f.role))
        .await
        .unwrap();
    assert!(
        f.run("SELECT 1 WHERE 1=0")
            .await
            .unwrap_err()
            .to_string()
            .contains("VIEW DEFINITION")
    );
    f.db.conn.execute(&format!("GRANT VIEW DEFINITION TO [{}]; CREATE TABLE app.unmanaged(n int); GRANT UPDATE ON app.unmanaged TO [{}]",f.role,f.role)).await.unwrap();
    assert!(
        f.run("SELECT 1/0")
            .await
            .unwrap_err()
            .to_string()
            .contains("UPDATE")
    );
    f.finish().await;
    other.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn bounded_results_and_driver_timeouts_do_not_collect_or_pass_partial_streams() {
    let mut f = Fixture::new("assert_bounds2401").await;
    let AssertionResult::Violation(e) = f
        .run("SELECT TOP (1000) a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b")
        .await
        .unwrap()
    else {
        panic!("missing bounded violation")
    };
    assert_eq!(e.rows.len(), 20);
    assert!(e.truncated);
    let AssertionResult::Violation(e) = f
        .run("SELECT REPLICATE(CAST(N'€' AS nvarchar(max)),1000000) large,N'later' later")
        .await
        .unwrap()
    else {
        panic!("missing byte violation")
    };
    let EvidenceCell::Text { value, truncated } = &e.rows[0][0] else {
        panic!("text missing")
    };
    assert!(*truncated && value.len() <= 65536 && value.len() % 3 == 0);
    assert!(e.truncated);
    let conn = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
    let error = conn
        .query_bounded(
            "WAITFOR DELAY '00:00:20'; SELECT 1",
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timeout"), "{error:?}");
    assert!(matches!(
        f.run("SELECT 1 WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn procedure_execute_is_write_capable_even_through_ownership_chaining() {
    let mut f = Fixture::new("assert_proc2401").await;
    // Neither module changes execution identity. The procedure shares dbo
    // ownership with app.orders, so its caller needs no direct INSERT grant.
    f.db.conn.execute("CREATE PROCEDURE dbo.writer AS BEGIN INSERT INTO app.orders VALUES(2,2); SELECT 1 n WHERE 1=0; END").await.unwrap();
    f.db.conn.execute("CREATE FUNCTION dbo.observed() RETURNS int AS BEGIN RETURN (SELECT MAX(total) FROM app.orders); END").await.unwrap();
    let role = f.role.clone();
    f.db.conn.execute(&format!("GRANT EXECUTE ON dbo.observed TO [{role}]; CREATE ROLE executable; ALTER ROLE executable ADD MEMBER [{role}]")).await.unwrap();
    let safe = "SELECT dbo.observed() n WHERE dbo.observed()<0";
    assert!(matches!(
        f.run(safe).await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.db.conn
        .execute(&format!("GRANT EXECUTE ON dbo.writer TO [{role}]"))
        .await
        .unwrap();
    // Native positive control for the threat: an ordinary caller really can
    // persist this write through chaining when it bypasses admission.
    let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
    caller.execute("EXEC dbo.writer").await.unwrap();
    drop(caller);
    let rows =
        f.db.conn
            .query("SELECT COUNT(*) AS n FROM app.orders WHERE id=2")
            .await
            .unwrap();
    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
    f.db.conn
        .execute(&format!(
            "DELETE FROM app.orders WHERE id=2; REVOKE EXECUTE ON dbo.writer FROM [{role}]"
        ))
        .await
        .unwrap();
    for (grant, revoke) in [
        (
            format!("GRANT EXECUTE ON dbo.writer TO [{role}]"),
            format!("REVOKE EXECUTE ON dbo.writer FROM [{role}]"),
        ),
        (
            "GRANT EXECUTE ON dbo.writer TO executable".into(),
            "REVOKE EXECUTE ON dbo.writer FROM executable".into(),
        ),
        (
            format!("GRANT EXECUTE ON SCHEMA::dbo TO [{role}]"),
            format!("REVOKE EXECUTE ON SCHEMA::dbo FROM [{role}]"),
        ),
        (
            format!("GRANT EXECUTE TO [{role}]"),
            format!("REVOKE EXECUTE FROM [{role}]"),
        ),
    ] {
        f.db.conn.execute(&grant).await.unwrap();
        for sql in ["EXEC dbo.writer", "SELECT 1/0", "SELECT 1 WHERE 1=0"] {
            let error = f.run(sql).await.unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("EXECUTE") && message.contains("[dbo].[writer]"),
                "{grant}: {error:?}"
            );
        }
        let rows =
            f.db.conn
                .query("SELECT COUNT(*) AS n FROM app.orders WHERE id=2")
                .await
                .unwrap();
        assert_eq!(
            rows[0].try_get::<i32>("n").unwrap(),
            Some(0),
            "admission must precede user SQL"
        );
        f.db.conn.execute(&revoke).await.unwrap();
        assert!(matches!(
            f.run(safe).await.unwrap(),
            AssertionResult::Pass(_)
        ));
    }
    // As with ordinary DML, only an effective right is refused. Functions
    // remain callable when DENY defeats a broader grant on the procedure.
    f.db.conn
        .execute(&format!(
            "GRANT EXECUTE TO [{role}]; DENY EXECUTE ON dbo.writer TO [{role}]"
        ))
        .await
        .unwrap();
    assert!(matches!(
        f.run(safe).await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.db.conn.execute(&format!("REVOKE EXECUTE ON dbo.writer FROM [{role}]; REVOKE EXECUTE FROM [{role}]; ALTER ROLE executable DROP MEMBER [{role}]; DROP ROLE executable")).await.unwrap();
    let mode = f
        .db
        .conn
        .query("SELECT CONVERT(nvarchar(20),DATABASEPROPERTYEX(DB_NAME(),'Updateability')) AS mode")
        .await
        .unwrap();
    assert_eq!(mode[0].try_get::<&str>("mode").unwrap(), Some("READ_WRITE"));
    f.finish().await;
}
