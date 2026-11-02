//! Native target boundary, qualified while the database remains READ_WRITE.
use super::*;
use pbps_db::{Driver, query::EvidenceCell};
use pbps_mssql::assertions::{AssertionResult, Target, run_target};
use std::time::Duration;
// Admission now audits every accessible system database as well as the target.
// Give the native fixture room on shared hosts; the 200 ms timeout case is separate.
const DEADLINE: Duration = Duration::from_secs(30);
// Default linked-server mappings and guest-enabled databases affect every
// login on the instance. Guest-admission controls share this case guard.
static TARGET_CASE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
// Hold the guard until the caller removes the guest-enabled database: otherwise
// unrelated positive assertion cases correctly refuse its instance-wide access.
pub(super) async fn enable_guest(conn: &mut Conn) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = TARGET_CASE.lock().await;
    conn.execute("GRANT CONNECT TO guest").await.unwrap();
    guard
}

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
        // A mapped read-only user avoids guest's implicit schema ownership
        // (including permanent CREATE SEQUENCE) in accessible system databases.
        for system in ["master", "msdb", "tempdb"] {
            db.conn.execute(&format!("USE [{system}]; CREATE USER [{role}] FOR LOGIN [{role}]; GRANT VIEW DEFINITION TO [{role}]; DENY EXECUTE TO [{role}]; USE [{}]",db.name)).await.unwrap();
        }
        // Database-wide DENY in master does not deny a server-scoped system
        // procedure from another database. Object DENY in master does (4629
        // forbids changing those permissions elsewhere). SQL batches still work.
        db.conn.execute("USE master").await.unwrap();
        let procedures=db.conn.query("SELECT QUOTENAME(SCHEMA_NAME(schema_id))+'.'+QUOTENAME(name) AS name FROM sys.system_objects WHERE type IN ('P','PC','X','RF')").await.unwrap();
        let mut deny = String::from("BEGIN TRANSACTION; ");
        for row in procedures {
            let name = row.try_get::<&str>("name").unwrap().unwrap();
            deny.push_str(&format!("DENY EXECUTE ON OBJECT::{name} TO [{role}]; "));
        }
        deny.push_str(&format!("COMMIT; USE [{}]", db.name));
        db.conn.execute(&deny).await.unwrap();
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
        for system in ["master", "msdb", "tempdb"] {
            admin
                .execute(&format!("USE [{system}]; DROP USER [{}]", self.role))
                .await
                .unwrap();
        }
        admin.execute("USE master").await.unwrap();
        admin
            .execute(&format!("DROP LOGIN [{}]", self.role))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn guest_enabled_databases_do_not_overlap_target_assertion_cases() {
    let mut other = TestDb::create("assert_guest_overlap2401").await;
    let guest = enable_guest(&mut other.conn).await;
    let excluded = TARGET_CASE.try_lock().is_err();
    let mut next_case = Box::pin(async {
        let _case = TARGET_CASE.lock().await;
        let mut f = Fixture::new("assert_after_guest2401").await;
        let result = f.run("SELECT 1 WHERE 1=0").await;
        f.finish().await;
        result
    });
    let waited = tokio::time::timeout(Duration::from_millis(100), &mut next_case)
        .await
        .is_err();
    // Guest remains enabled while the next case is queued. Teardown must
    // precede releasing the case guard, even when the regression fails.
    other.drop().await;
    drop(guest);
    let result = next_case.await;
    assert!(
        excluded,
        "guest admission must hold the assertion case guard"
    );
    assert!(waited, "the assertion case must wait for guest teardown");
    assert!(matches!(result.unwrap(), AssertionResult::Pass(_)));
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_assertion_login_is_checked_by_effective_object_column_and_ddl_permissions() {
    let _case = TARGET_CASE.lock().await;
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
async fn target_batches_distinguish_rows_result_shapes_and_complete_stream_errors() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_query2401").await;
    // R1 (parenthesized operands/Unicode), R3 (keyword identifiers) and
    // R6 (a CTE followed by parenthesized operands) are native SQL, not
    // additional statements to recognize. Non-result statements are allowed.
    for sql in [
        "WITH c AS (SELECT 1 n) (SELECT n FROM c WHERE n=0) UNION ALL (SELECT n FROM c WHERE n=0)",
        "(SELECT 1 n WHERE 1=0) UNION ALL (SELECT 2 n WHERE 1=0)",
        "SELECT 1 AS 名UPDATE WHERE 1=0",
        "SELECT 1 AS THROW WHERE 1=0",
        "SET NOCOUNT ON; DECLARE @n int=1; SELECT @n WHERE @n=0; PRINT N'after'; RETURN;",
    ] {
        let caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
        let evidence = caller.query_bounded(sql, DEADLINE).await.unwrap();
        assert!(
            matches!(
                AssertionResult::from_evidence(evidence),
                AssertionResult::Pass(_)
            ),
            "native control: {sql}"
        );
        let result = f.run(sql).await.unwrap();
        assert!(
            matches!(result, AssertionResult::Pass(_)),
            "target batch: {sql}: {result:?}"
        );
        println!("native and target accepted: {sql}");
    }
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
        "SELECT 1 WHERE 1=0; THROW 50000,N'after result',1;",
        "SELECT 1 WHERE 1=0\nGO\nSELECT 2",
        ":r missing.sql",
        "!! echo client-command",
    ] {
        let result = f.run(sql).await.unwrap();
        assert!(
            matches!(result, AssertionResult::SqlError(_)),
            "{sql}: {result:?}"
        );
    }
    for sql in [
        "",
        "SET NOCOUNT ON; PRINT N'no result'",
        "SELECT 1 WHERE 1=0; SELECT 2 WHERE 1=0",
        "SELECT 1; SELECT 2",
        "SELECT 1 SELECT 2",
        "SELECT 1 WHERE 1=0; SELECT 1/0",
    ] {
        let result = f.run(sql).await.unwrap();
        assert!(
            matches!(result, AssertionResult::InvalidResult { .. }),
            "{sql}: {result:?}"
        );
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
async fn multi_statement_writes_are_refused_by_permissions_before_execution() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_batch_write2401").await;
    let sql = "SELECT 1 WHERE 1=0; INSERT INTO app.orders VALUES(2,2); PRINT N'after'";
    // An admitted login still cannot write through a batch: the engine denies
    // INSERT, and an empty earlier result never hides that error.
    let result = f.run(sql).await.unwrap();
    assert!(matches!(result, AssertionResult::SqlError(_)), "{result:?}");
    f.db.conn
        .execute(&format!("GRANT INSERT ON app.orders TO [{}]", f.role))
        .await
        .unwrap();
    // With effective INSERT, the oracle must refuse before any assertion SQL.
    // Finish even on a revert witness so the deliberately enabled writer leaves
    // no fixture state behind for another case.
    let result = f.run(sql).await;
    let rows =
        f.db.conn
            .query("SELECT COUNT(*) AS n FROM app.orders WHERE id=2")
            .await
            .unwrap();
    let writes = rows[0].try_get::<i32>("n").unwrap();
    f.finish().await;
    let error = result.unwrap_err();
    assert!(error.to_string().contains("INSERT"), "{error:?}");
    assert_eq!(writes, Some(0));
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn wrong_targets_and_incomplete_metadata_are_refused_before_assertion_sql() {
    let _case = TARGET_CASE.lock().await;
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
    let _case = TARGET_CASE.lock().await;
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
    let _case = TARGET_CASE.lock().await;
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

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn access_to_another_user_database_refuses_before_assertion_sql() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_other_access2401").await;
    let mut other = TestDb::create("assert_writable_elsewhere2401").await;
    let role = f.role.clone();
    other.conn.execute(&format!("CREATE USER [{role}] FOR LOGIN [{role}]; CREATE TABLE dbo.writes(id int); GRANT INSERT ON dbo.writes TO [{role}]")).await.unwrap();
    let write = format!("INSERT [{}].dbo.writes VALUES(2401)", other.name);
    let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
    caller.execute(&write).await.unwrap();
    drop(caller);
    let rows = other
        .conn
        .query("SELECT COUNT(*) AS n FROM dbo.writes")
        .await
        .unwrap();
    assert_eq!(
        rows[0].try_get::<i32>("n").unwrap(),
        Some(1),
        "native three-part write"
    );
    other.conn.execute("DELETE FROM dbo.writes").await.unwrap();
    for sql in [write.as_str(), "SELECT 1/0", "SELECT 1 WHERE 1=0"] {
        let error = f.run(sql).await.unwrap_err();
        assert!(error.to_string().contains(&other.name), "{error:?}");
    }
    let rows = other
        .conn
        .query("SELECT COUNT(*) AS n FROM dbo.writes")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(0));
    // A hidden accessible database must not become an empty inventory.
    f.db.conn
        .execute(&format!(
            "USE master; DENY VIEW ANY DATABASE TO [{role}]; USE [{}]",
            f.db.name
        ))
        .await
        .unwrap();
    let error = f.run("SELECT 1 WHERE 1=0").await.unwrap_err();
    assert!(error.to_string().contains("VIEW ANY DATABASE"), "{error:?}");
    f.db.conn
        .execute(&format!(
            "USE master; REVOKE VIEW ANY DATABASE FROM [{role}]; USE [{}]",
            f.db.name
        ))
        .await
        .unwrap();
    other
        .conn
        .execute(&format!("DENY CONNECT TO [{role}]"))
        .await
        .unwrap();
    assert!(matches!(
        f.run("SELECT 1 WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    other.drop().await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn user_clr_assemblies_refuse_even_through_function_and_view_wrappers() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_clr2401").await;
    let binary = include_str!("../live/scratch2236-provider.hex").trim();
    for ddl in [
        "CREATE FUNCTION dbo.fn2401() RETURNS int AS BEGIN RETURN 1 END",
        "CREATE FUNCTION dbo.if2401() RETURNS TABLE AS RETURN SELECT 1 AS value",
        "CREATE FUNCTION dbo.tf2401() RETURNS @r TABLE(value int) AS BEGIN INSERT @r VALUES(1); RETURN END",
    ] {
        f.db.conn.execute(ddl).await.unwrap();
    }
    let role = f.role.clone();
    f.db.conn.execute(&format!("GRANT EXECUTE ON dbo.fn2401 TO [{role}]; GRANT SELECT ON dbo.if2401 TO [{role}]; GRANT SELECT ON dbo.tf2401 TO [{role}]")).await.unwrap();
    for sql in [
        "SELECT dbo.fn2401() n WHERE 1=0",
        "SELECT * FROM dbo.if2401() WHERE 1=0",
        "SELECT * FROM dbo.tf2401() WHERE 1=0",
    ] {
        assert!(
            matches!(f.run(sql).await.unwrap(), AssertionResult::Pass(_)),
            "{sql}"
        );
    }
    let rows = f.db.conn.query(&format!("SELECT COUNT_BIG(*) AS n FROM sys.trusted_assemblies WHERE hash=HASHBYTES('SHA2_512',0x{binary})")).await.unwrap();
    let added_trust = rows[0].try_get::<i64>("n").unwrap() == Some(0);
    let rows = f.db.conn.query("SELECT CONVERT(int,value_in_use) AS enabled FROM sys.configurations WHERE name='clr enabled'").await.unwrap();
    let enabled = rows[0].try_get::<i32>("enabled").unwrap().unwrap();
    if added_trust {
        f.db.conn.execute(&format!("DECLARE @hash varbinary(64)=HASHBYTES('SHA2_512',0x{binary}); EXEC sys.sp_add_trusted_assembly @hash=@hash,@description=N'pbps2401 CLR admission fixture'")).await.unwrap();
    }
    // Restore server configuration and owned trust even when a revert witness
    // panics. The assembly itself belongs to this disposable database.
    let result = tokio::spawn(async move {
        f.db.conn.execute("EXEC sys.sp_configure 'clr enabled',1; RECONFIGURE").await.unwrap();
        f.db.conn.execute(&format!("CREATE ASSEMBLY assertions2401 FROM 0x{binary} WITH PERMISSION_SET=SAFE")).await.unwrap();
        // Even an unused SAFE assembly refuses; neither permission_set nor
        // direct object permissions can rule out ownership-chained execution.
        let mut observations = vec![("unused SAFE assembly", f.run("SELECT 1 WHERE 1=0").await)];
        for ddl in [
            "CREATE FUNCTION dbo.clr2401() RETURNS int AS EXTERNAL NAME assertions2401.Provider.Value",
            "CREATE FUNCTION dbo.wrapper2401() RETURNS int AS BEGIN RETURN dbo.clr2401() END",
            "CREATE VIEW dbo.wrapped2401 AS SELECT dbo.clr2401() AS value",
        ] {
            f.db.conn.execute(ddl).await.unwrap();
        }
        f.db.conn.execute(&format!("GRANT EXECUTE ON dbo.wrapper2401 TO [{role}]; GRANT SELECT ON dbo.wrapped2401 TO [{role}]")).await.unwrap();
        let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
        let Err(error) = caller.query("SELECT dbo.clr2401() AS value").await else { panic!("direct CLR call unexpectedly permitted") };
        assert_eq!(error.server_error_code().as_deref(), Some("229"), "no direct CLR EXECUTE");
        for sql in ["SELECT dbo.wrapper2401() AS value", "SELECT value FROM dbo.wrapped2401"] {
            let rows = caller.query(sql).await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(1), "native ownership-chained wrapper: {sql}");
            println!("native wrapper executes without CLR grant: {sql}");
            let observation = f.run(sql).await;
            println!("target wrapper admission: {sql}: {observation:?}");
            observations.push((sql, observation));
        }
        drop(caller);
        f.db.conn.execute("DROP VIEW dbo.wrapped2401; DROP FUNCTION dbo.wrapper2401; DROP FUNCTION dbo.clr2401; DROP ASSEMBLY assertions2401").await.unwrap();
        assert!(matches!(f.run("SELECT dbo.fn2401() n WHERE 1=0").await.unwrap(), AssertionResult::Pass(_)));
        for (sql, observation) in observations {
            let error = observation.unwrap_err();
            assert!(error.to_string().contains("assertions2401"), "{sql}: {error:?}");
        }
        f
    }).await;
    let mut admin = Conn::connect(Driver::Mssql, &conn_str()).await.unwrap();
    admin
        .execute(&format!(
            "EXEC sys.sp_configure 'clr enabled',{enabled}; RECONFIGURE"
        ))
        .await
        .unwrap();
    if added_trust {
        admin.execute(&format!("DECLARE @hash varbinary(64)=HASHBYTES('SHA2_512',0x{binary}); EXEC sys.sp_drop_trusted_assembly @hash=@hash")).await.unwrap();
    }
    drop(admin);
    result.unwrap().finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn usable_linked_server_mappings_refuse_before_assertion_sql() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_linked2401").await;
    let server = format!("{}_linked", f.db.name);
    let cleanup = server.clone();
    let admin_connection = conn_str();
    // Use the fixture password, not the local runner's password: CI differs.
    let password = admin_connection
        .split(';')
        .find_map(|p| {
            let (key, value) = p.split_once('=')?;
            matches!(key.trim().to_ascii_lowercase().as_str(), "password" | "pwd")
                .then(|| value.replace('\'', "''"))
        })
        .unwrap();
    let result = tokio::spawn(async move {
        let role = f.role.clone();
        let database = f.db.name.clone();
        f.db.conn.execute(&format!("USE master; EXEC sys.sp_addlinkedserver @server=N'{server}',@srvproduct=N'',@provider=N'MSOLEDBSQL',@datasrc=N'localhost,1433',@provstr=N'User ID=sa;Encrypt=Optional;TrustServerCertificate=Yes'; EXEC sys.sp_droplinkedsrvlogin @rmtsrvname=N'{server}',@locallogin=NULL; EXEC sys.sp_serveroption @server=N'{server}',@optname=N'rpc out',@optvalue=N'true'; GRANT VIEW ANY DEFINITION TO [{role}]; USE [{database}]")).await.unwrap();
        assert!(matches!(f.run("SELECT 1 WHERE 1=0").await.unwrap(), AssertionResult::Pass(_)), "a server without a usable mapping is allowed");
        let write = format!("EXEC ('INSERT [{}].app.orders VALUES(2,2); SELECT 1 n WHERE 1=0') AT [{server}]", f.db.name);
        let select_write = format!("SELECT * FROM OPENQUERY([{server}],'SET NOCOUNT ON; INSERT [{}].app.orders VALUES(2,2); COMMIT; SELECT 1 AS n') WHERE n<0", f.db.name);
        for local in [format!("N'{role}'"), "NULL".into()] {
            f.db.conn.execute(&format!("USE master; EXEC sys.sp_addlinkedsrvlogin @rmtsrvname=N'{server}',@useself=N'false',@locallogin={local},@rmtuser=N'sa',@rmtpassword=N'{password}'; USE [{database}]")).await.unwrap();
            let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
            caller.execute(&write).await.unwrap();
            drop(caller);
            let rows = f.db.conn.query("SELECT COUNT(*) AS n FROM app.orders WHERE id=2").await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1), "native remote write: {local}");
            f.db.conn.execute("DELETE FROM app.orders WHERE id=2").await.unwrap();
            let caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
            let evidence = caller.query_bounded(&select_write, DEADLINE).await.unwrap();
            assert!(matches!(AssertionResult::from_evidence(evidence), AssertionResult::Pass(_)), "a native empty SELECT can commit through the remote mapping");
            let rows = f.db.conn.query("SELECT COUNT(*) AS n FROM app.orders WHERE id=2").await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
            f.db.conn.execute("DELETE FROM app.orders WHERE id=2").await.unwrap();
            for sql in [select_write.as_str(), write.as_str(), "SELECT 1/0", "SELECT 1 WHERE 1=0"] {
                let error = f.run(sql).await.unwrap_err();
                assert!(error.to_string().contains(&server), "{error:?}");
            }
            let rows = f.db.conn.query("SELECT COUNT(*) AS n FROM app.orders WHERE id=2").await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(0));
            if local=="NULL" {
                f.db.conn.execute(&format!("USE master; REVOKE VIEW ANY DEFINITION FROM [{role}]; USE [{database}]")).await.unwrap();
                let error = f.run("SELECT 1 WHERE 1=0").await.unwrap_err();
                assert!(error.to_string().contains("VIEW ANY DEFINITION") && error.to_string().contains(&server), "{error:?}");
                f.db.conn.execute(&format!("USE master; GRANT VIEW ANY DEFINITION TO [{role}]; USE [{database}]")).await.unwrap();
            }
            f.db.conn.execute(&format!("USE master; EXEC sys.sp_droplinkedsrvlogin @rmtsrvname=N'{server}',@locallogin={local}; USE [{database}]")).await.unwrap();
            assert!(matches!(f.run("SELECT 1 WHERE 1=0").await.unwrap(), AssertionResult::Pass(_)));
        }
        // A mapping for another principal is not a mapping for this caller.
        f.db.conn.execute(&format!("USE master; EXEC sys.sp_addlinkedsrvlogin @rmtsrvname=N'{server}',@useself=N'false',@locallogin=N'sa',@rmtuser=N'sa',@rmtpassword=N'{password}'; USE [{database}]")).await.unwrap();
        assert!(matches!(f.run("SELECT 1 WHERE 1=0").await.unwrap(), AssertionResult::Pass(_)));
        f
    }).await;
    // A default mapping must never leak into another target case, including
    // after the intentional panic in a revert witness.
    let mut admin = Conn::connect(Driver::Mssql, &conn_str()).await.unwrap();
    admin.execute(&format!("IF EXISTS(SELECT 1 FROM sys.servers WHERE name=N'{cleanup}') EXEC sys.sp_dropserver @server=N'{cleanup}',@droplogins=N'droplogins'")).await.unwrap();
    result.unwrap().finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn native_identifier_positions_are_decided_by_the_engine() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_frame_words2401").await;
    let matrix: serde_json::Value =
        serde_json::from_str(include_str!("../live/assertions2401-identifiers.json")).unwrap();
    // Bare table references must resolve in the same schema as the qualified
    // forms. Every table/column name, accepted or reserved, is created quoted.
    f.db.conn
        .execute(&format!(
            "ALTER USER [{}] WITH DEFAULT_SCHEMA=app; GRANT SELECT ON SCHEMA::app TO [{}]",
            f.role, f.role
        ))
        .await
        .unwrap();
    for case in matrix["words"].as_array().unwrap() {
        let word = case["word"].as_str().unwrap();
        f.db.conn
            .execute(&format!("CREATE TABLE app.[{word}]([{word}] int)"))
            .await
            .unwrap();
    }
    let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
    let mut cells = 0;
    let mut admitted = 0;
    for case in matrix["words"].as_array().unwrap() {
        let word = case["word"].as_str().unwrap();
        for shape in matrix["shapes"].as_array().unwrap() {
            let sql = shape["sql"].as_str().unwrap().replace("{word}", word);
            let accepted =
                case["unquoted"].as_bool().unwrap() || shape["quoted"].as_bool().unwrap();
            match caller.query(&sql).await {
                Ok(rows) => assert!(
                    accepted && rows.is_empty(),
                    "unexpected native acceptance: {sql}"
                ),
                Err(error) => assert!(
                    !accepted && error.server_error_code().as_deref() == Some("156"),
                    "{sql}: {error:?}"
                ),
            }
            // The engine alone decides syntax. Keep the complete native
            // identifier controls from the former lexer, but audit privileges
            // only for valid SQL; invalid cells cannot establish a false refusal.
            if accepted {
                let result = f.run(&sql).await.unwrap();
                assert!(
                    matches!(result, AssertionResult::Pass(_)),
                    "{}: {sql}: {result:?}",
                    shape["name"]
                );
                admitted += 1;
            }
            cells += 1;
        }
    }
    println!("native identifier matrix: {cells} cells; {admitted} valid admissions");
    assert_eq!((cells, admitted), (300, 39));
    drop(caller);
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn system_database_writes_are_refused_before_sql_and_session_temps_stay_allowed() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_systems2401").await;
    let audit = format!("{}_audit", f.db.name);
    let role = f.role.clone();
    let target = f.db.name.clone();
    let read = "SELECT * FROM app.orders WHERE id<0";
    // Legacy DEFAULT/RULE nominal object rights are not executable rights;
    // only a schema/parent grant permits their DDL. Private temps from another
    // session are visible metadata, not a permanent-write capability either.
    f.db.conn
        .execute("CREATE TABLE #admin_session2401(n int)")
        .await
        .unwrap();
    f.db.conn.execute("USE master").await.unwrap();
    f.db.conn
        .execute(&format!("CREATE DEFAULT dbo.[{audit}_default] AS 0"))
        .await
        .unwrap();
    f.db.conn
        .execute(&format!("CREATE RULE dbo.[{audit}_rule] AS @n>0"))
        .await
        .unwrap();
    f.db.conn
        .execute(&format!("CREATE TYPE dbo.[{audit}_type] AS TABLE(n int)"))
        .await
        .unwrap();
    f.db.conn.execute(&format!("USE [{target}]")).await.unwrap();
    assert!(matches!(
        f.run(read).await.unwrap(),
        AssertionResult::Pass(_)
    ));
    for system in ["master", "msdb", "tempdb"] {
        f.db.conn
            .execute(&format!(
                "USE [{system}]; CREATE TABLE dbo.[{audit}](n int); USE [{target}]"
            ))
            .await
            .unwrap();
        f.db.conn
            .execute(&format!(
                "USE [{system}]; GRANT INSERT ON dbo.[{audit}] TO [{role}]; USE [{target}]"
            ))
            .await
            .unwrap();
        let write =
            format!("EXEC(N'INSERT [{system}].dbo.[{audit}] VALUES(2401); SELECT 1 WHERE 1=0')");
        // Bypassing admission really persists a write from this same login.
        let caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
        let evidence = caller.query_bounded(&write, DEADLINE).await.unwrap();
        assert!(matches!(
            AssertionResult::from_evidence(evidence),
            AssertionResult::Pass(_)
        ));
        let rows =
            f.db.conn
                .query(&format!(
                    "SELECT COUNT(*) AS n FROM [{system}].dbo.[{audit}]"
                ))
                .await
                .unwrap();
        assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
        f.db.conn
            .execute(&format!("DELETE FROM [{system}].dbo.[{audit}]"))
            .await
            .unwrap();
        for sql in [write.as_str(), "SELECT 1/0", read] {
            let error = f.run(sql).await.unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("INSERT") && message.contains(system) && message.contains(&audit),
                "{sql}: {error:?}"
            );
        }
        let rows =
            f.db.conn
                .query(&format!(
                    "SELECT COUNT(*) AS n FROM [{system}].dbo.[{audit}]"
                ))
                .await
                .unwrap();
        assert_eq!(
            rows[0].try_get::<i32>("n").unwrap(),
            Some(0),
            "refused before assertion SQL"
        );
        f.db.conn.execute(&format!("USE [{system}]; REVOKE INSERT ON dbo.[{audit}] FROM [{role}]; DROP TABLE dbo.[{audit}]; USE [{target}]")).await.unwrap();
        // The audit must restore the original database on its checked session.
        assert!(matches!(
            f.run(read).await.unwrap(),
            AssertionResult::Pass(_)
        ));
    }
    f.db.conn
        .execute(&format!(
            "USE tempdb; GRANT CREATE TABLE TO [{role}]; USE [{target}]"
        ))
        .await
        .unwrap();
    let error = f.run("SELECT 1/0").await.unwrap_err();
    assert!(
        error.to_string().contains("CREATE TABLE") && error.to_string().contains("tempdb"),
        "{error:?}"
    );
    f.db.conn
        .execute(&format!(
            "USE tempdb; REVOKE CREATE TABLE FROM [{role}]; USE [{target}]"
        ))
        .await
        .unwrap();
    f.db.conn
        .execute(&format!(
            "USE master; GRANT CONTROL ON TYPE::dbo.[{audit}_type] TO [{role}]; USE [{target}]"
        ))
        .await
        .unwrap();
    let error = f.run("SELECT 1/0").await.unwrap_err();
    // CONTROL expands to multiple effective rights; their enumeration order
    // is not contractual. Pin the TYPE namespace and the actual securable.
    assert!(
        error.to_string().contains("permission on TYPE")
            && error.to_string().contains(&format!("{audit}_type"))
            && error.to_string().contains("master"),
        "{error:?}"
    );
    f.db.conn.execute(&format!("USE master; REVOKE CONTROL ON TYPE::dbo.[{audit}_type] FROM [{role}]; DROP TYPE dbo.[{audit}_type]; USE [{target}]")).await.unwrap();
    let temporary = "EXEC(N'CREATE TABLE #session2401(n int); INSERT #session2401 VALUES(1); SELECT n FROM #session2401 WHERE n<0')";
    assert!(matches!(
        f.run(temporary).await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.db.conn
        .execute(&format!(
            "USE master; GRANT ALTER ON SCHEMA::dbo TO [{role}]; USE [{target}]"
        ))
        .await
        .unwrap();
    let error = f.run("SELECT 1/0").await.unwrap_err();
    assert!(
        error.to_string().contains("permission on SCHEMA")
            && error.to_string().contains("[dbo]")
            && error.to_string().contains("master"),
        "{error:?}"
    );
    f.db.conn.execute(&format!("USE master; REVOKE ALTER ON SCHEMA::dbo FROM [{role}]; DROP DEFAULT dbo.[{audit}_default]; DROP RULE dbo.[{audit}_rule]; USE [{target}]; DROP TABLE #admin_session2401")).await.unwrap();
    assert!(matches!(
        f.run(read).await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.finish().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn system_procedure_execute_is_inventoried_without_refusing_read_only_queries() {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new("assert_system_proc2401").await;
    assert!(matches!(
        f.run("SELECT 1 WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.db.conn
        .execute("CREATE FUNCTION dbo.read2401() RETURNS int AS BEGIN RETURN 2401 END")
        .await
        .unwrap();
    f.db.conn
        .execute(&format!("GRANT EXECUTE ON dbo.read2401 TO [{}]", f.role))
        .await
        .unwrap();
    assert!(matches!(
        f.run("SELECT dbo.read2401() WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    // An object grant in master is effective in the target as well. Keep the
    // other system procedures denied so the refusal pins this exact securable.
    f.db.conn.execute(&format!("USE master; REVOKE EXECUTE TO [{}]; REVOKE EXECUTE ON OBJECT::sys.sp_executesql FROM [{}]; GRANT EXECUTE ON OBJECT::sys.sp_executesql TO [{}]; USE [{}]",f.role,f.role,f.role,f.db.name)).await.unwrap();
    let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
    let rows=caller.query("SELECT (SELECT COUNT(*) FROM sys.objects WHERE name='sp_executesql') AS ordinary,(SELECT COUNT(*) FROM sys.system_objects WHERE name='sp_executesql') AS system,HAS_PERMS_BY_NAME('sys.sp_executesql','OBJECT','EXECUTE') AS executable").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("ordinary").unwrap(), Some(0));
    assert_eq!(rows[0].try_get::<i32>("system").unwrap(), Some(1));
    assert_eq!(rows[0].try_get::<i32>("executable").unwrap(), Some(1));
    let evidence = caller
        .query_bounded("EXEC sys.sp_executesql N'SELECT 1 WHERE 1=0'", DEADLINE)
        .await
        .unwrap();
    assert!(matches!(
        AssertionResult::from_evidence(evidence),
        AssertionResult::Pass(_)
    ));
    for sql in ["EXEC sys.sp_executesql N'SELECT 1 WHERE 1=0'", "SELECT 1/0"] {
        let error = f.run(sql).await.unwrap_err();
        assert!(
            error.to_string().contains("EXECUTE")
                && error.to_string().contains("[sys].[sp_executesql]"),
            "{error:?}"
        );
    }
    f.db.conn
        .execute(&format!(
            "USE master; DENY EXECUTE ON OBJECT::sys.sp_executesql TO [{}]; USE [{}]",
            f.role, f.db.name
        ))
        .await
        .unwrap();
    assert!(matches!(
        f.run("SELECT 1 WHERE 1=0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.finish().await;
}

// One capability, five securable scopes. The engine's tokens supply role
// closure; these cases pin both the grant flag and the native GRANT AS effect.
async fn delegation_is_refused(scope: &str, expected: &str, server: bool, tag: &str) {
    let _case = TARGET_CASE.lock().await;
    let mut f = Fixture::new(tag).await;
    let user = f.role.clone();
    let group = format!("{}_delegator", f.db.name);
    let receiver = format!("{}_receiver", f.db.name);
    let permission = if server {
        "VIEW ANY DEFINITION"
    } else {
        "SELECT"
    };
    let context = if server {
        "master".to_owned()
    } else {
        f.db.name.clone()
    };
    let grant = |principal: &str, option: &str| {
        format!("USE [{context}]; GRANT {permission} {scope} TO [{principal}]{option}")
    };
    let revoke = |principal: &str| {
        format!("USE [{context}]; REVOKE {permission} {scope} FROM [{principal}] CASCADE")
    };
    let count = if server {
        format!(
            "SELECT COUNT(*) AS n FROM sys.server_permissions WHERE grantee_principal_id=SUSER_ID('{receiver}') AND permission_name='{permission}'"
        )
    } else {
        format!(
            "SELECT COUNT(*) AS n FROM sys.database_permissions WHERE grantee_principal_id=DATABASE_PRINCIPAL_ID('{receiver}') AND permission_name='{permission}'"
        )
    };
    if server {
        f.db.conn.execute(&format!("CREATE SERVER ROLE [{group}]; ALTER SERVER ROLE [{group}] ADD MEMBER [{user}]; CREATE LOGIN [{receiver}] WITH PASSWORD='OnlyDelegate!2401', CHECK_POLICY=OFF")).await.unwrap();
    } else {
        f.db.conn.execute(&format!("CREATE ROLE [{group}]; ALTER ROLE [{group}] ADD MEMBER [{user}]; CREATE USER [{receiver}] WITHOUT LOGIN")).await.unwrap();
    }
    assert!(matches!(
        f.run("SELECT id FROM app.orders WHERE id<0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    let mut checks = Vec::new();
    // Direct W is unsafe even though the underlying permission is read-only.
    f.db.conn
        .execute(&grant(&user, " WITH GRANT OPTION"))
        .await
        .unwrap();
    let sql = format!("{}; SELECT 1 WHERE 1=0", grant(&receiver, ""));
    // The grant may have required master; target identity still comes from
    // the fixture's actual planning database, never the admin's last context.
    f.db.conn
        .execute(&format!("USE [{}]", f.db.name))
        .await
        .unwrap();
    let result = f.run(&sql).await;
    let named = matches!(&result,Err(e) if e.to_string().contains("WITH GRANT OPTION") && e.to_string().contains(expected));
    checks.push(("direct", named, format!("{result:?}")));
    f.db.conn
        .execute(&format!("USE [{context}]"))
        .await
        .unwrap();
    let rows = f.db.conn.query(&count).await.unwrap();
    checks.push((
        "no assertion grant",
        rows[0].try_get::<i32>("n").unwrap() == Some(0),
        format!("count={:?}", rows[0].try_get::<i32>("n").unwrap()),
    ));
    f.db.conn.execute(&revoke(&receiver)).await.unwrap();
    if scope == "ON OBJECT::app.orders" {
        // A DENY on the same principal replaces its sole W row. With no
        // delegation left, a constant read is valid and GRANT itself fails.
        f.db.conn
            .execute(&format!("DENY SELECT {scope} TO [{user}] CASCADE"))
            .await
            .unwrap();
        let result = f.run("SELECT 1 WHERE 1=0").await;
        checks.push((
            "DENY replaced sole W",
            matches!(&result, Ok(AssertionResult::Pass(_))),
            format!("{result:?}"),
        ));
        let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
        let result = caller.execute(&grant(&receiver, "")).await;
        checks.push((
            "no delegation after replacement",
            result.is_err(),
            format!("{result:?}"),
        ));
        drop(caller);
    }
    f.db.conn.execute(&revoke(&user)).await.unwrap();
    f.db.conn
        .execute(&format!(
            "USE [{}]; GRANT SELECT ON app.orders TO [{user}]",
            f.db.name
        ))
        .await
        .unwrap();
    // Denying data access does not remove a role's GRANT AS authority. A
    // database-wide SELECT DENY also makes the metadata read fail loudly.
    f.db.conn
        .execute(&grant(&group, " WITH GRANT OPTION"))
        .await
        .unwrap();
    let inherited_sql = format!("{} AS [{group}]; SELECT 1 WHERE 1=0", grant(&receiver, ""));
    f.db.conn
        .execute(&format!("USE [{}]", f.db.name))
        .await
        .unwrap();
    let inherited = f.run(&inherited_sql).await;
    let named = matches!(&inherited,Err(e) if e.to_string().contains("WITH GRANT OPTION") && e.to_string().contains(expected));
    checks.push(("inherited W without DENY", named, format!("{inherited:?}")));
    f.db.conn
        .execute(&format!("USE [{context}]"))
        .await
        .unwrap();
    let rows = f.db.conn.query(&count).await.unwrap();
    checks.push((
        "no inherited assertion grant",
        rows[0].try_get::<i32>("n").unwrap() == Some(0),
        format!("count={:?}", rows[0].try_get::<i32>("n").unwrap()),
    ));
    f.db.conn.execute(&revoke(&receiver)).await.unwrap();
    let deny_scope = if scope.contains("(total)") {
        "ON OBJECT::app.orders"
    } else {
        scope
    };
    f.db.conn
        .execute(&format!(
            "USE [{context}]; DENY {permission} {deny_scope} TO [{user}]"
        ))
        .await
        .unwrap();
    let sql = format!("{} AS [{group}]; SELECT 1 WHERE 1=0", grant(&receiver, ""));
    // The grant may have required master; target identity still comes from
    // the fixture's actual planning database, never the admin's last context.
    f.db.conn
        .execute(&format!("USE [{}]", f.db.name))
        .await
        .unwrap();
    let result = f.run(&sql).await;
    // A database SELECT DENY blocks catalog reads; a server definition DENY
    // also defeats the required database metadata grant. Neither is absence.
    let refused = match &result {
        Err(e)
            if e.to_string().contains("WITH GRANT OPTION") && e.to_string().contains(expected) =>
        {
            true
        }
        Err(e) if !server && scope.is_empty() => e.server_error_code().as_deref() == Some("229"),
        Err(e) if server => e.to_string().contains("require database VIEW DEFINITION"),
        _ => false,
    };
    checks.push(("inherited W despite DENY", refused, format!("{result:?}")));
    f.db.conn
        .execute(&format!("USE [{context}]"))
        .await
        .unwrap();
    let rows = f.db.conn.query(&count).await.unwrap();
    checks.push((
        "no assertion GRANT AS",
        rows[0].try_get::<i32>("n").unwrap() == Some(0),
        format!("count={:?}", rows[0].try_get::<i32>("n").unwrap()),
    ));
    // Native control proves the denied login can still persist that grant AS
    // its role. Clear any deliberately enabled witness effect first.
    f.db.conn.execute(&revoke(&receiver)).await.unwrap();
    let mut caller = Conn::connect(Driver::Mssql, &f.assertion).await.unwrap();
    caller.execute(&sql).await.unwrap();
    drop(caller);
    let rows = f.db.conn.query(&count).await.unwrap();
    checks.push((
        "native delegation",
        rows[0].try_get::<i32>("n").unwrap() == Some(1),
        format!("count={:?}", rows[0].try_get::<i32>("n").unwrap()),
    ));
    f.db.conn.execute(&revoke(&receiver)).await.unwrap();
    f.db.conn.execute(&revoke(&group)).await.unwrap();
    f.db.conn
        .execute(&format!(
            "REVOKE {permission} {deny_scope} FROM [{user}] CASCADE"
        ))
        .await
        .unwrap();
    if server {
        f.db.conn.execute(&format!("ALTER SERVER ROLE [{group}] DROP MEMBER [{user}]; DROP SERVER ROLE [{group}]; DROP LOGIN [{receiver}]")).await.unwrap();
    } else {
        f.db.conn.execute(&format!("ALTER ROLE [{group}] DROP MEMBER [{user}]; DROP ROLE [{group}]; DROP USER [{receiver}]")).await.unwrap();
    }
    f.db.conn
        .execute(&format!(
            "USE [{}]; GRANT SELECT ON app.orders TO [{user}]",
            f.db.name
        ))
        .await
        .unwrap();
    assert!(matches!(
        f.run("SELECT id FROM app.orders WHERE id<0").await.unwrap(),
        AssertionResult::Pass(_)
    ));
    f.finish().await;
    assert!(checks.iter().all(|(_, ok, _)| *ok), "{scope}: {checks:?}");
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn object_grant_options_refuse_before_assertion_sql() {
    delegation_is_refused(
        "ON OBJECT::app.orders",
        "[app].[orders]",
        false,
        "grantobj2401",
    )
    .await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn column_grant_options_refuse_before_assertion_sql() {
    delegation_is_refused(
        "ON OBJECT::app.orders(total)",
        "[total]",
        false,
        "grantcol2401",
    )
    .await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn schema_grant_options_refuse_before_assertion_sql() {
    delegation_is_refused("ON SCHEMA::app", "[app]", false, "grantschema2401").await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn database_grant_options_refuse_before_assertion_sql() {
    delegation_is_refused("", "DATABASE", false, "grantdb2401").await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn server_grant_options_refuse_before_assertion_sql() {
    delegation_is_refused("", "SERVER", true, "grantsrv2401").await;
}
