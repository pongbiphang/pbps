use super::{TestDb, conn_str, connect_live};
use pbps_dialect::{Dialect, PlanDialect};
use pbps_model::{DropGuard, DropRequirement, PlanMode, PlannedChange};
use pbps_mssql::{
    Mssql,
    drop_requirements::{check, vet},
};

fn guard(predicate: &str) -> DropGuard {
    DropGuard {
        table_uid: "t_000001".parse().unwrap(),
        table: "dbo.t".parse().unwrap(),
        detach_from: None,
        requirements: [(
            "c_000001".parse().unwrap(),
            DropRequirement {
                table_uid: "t_000001".parse().unwrap(),
                predicate: predicate.into(),
                required_at: "2026-10-30".into(),
                operator: "reviewer".into(),
            },
        )]
        .into(),
    }
}
async fn checked(db: &mut TestDb, predicate: &str) -> Result<(), String> {
    let frame = Mssql.transaction_framing(&pbps_model::SessionZone::utc());
    db.conn.begin(&frame).await.unwrap();
    let result = check(&mut db.conn, &guard(predicate)).await;
    db.conn.rollback(&frame).await.unwrap();
    result
}
async fn fixture(tag: &str) -> TestDb {
    let mut db = TestDb::create(tag).await;
    db.conn.execute("CREATE TABLE dbo.t (id int, amount int, cents int, status nvarchar(40)); INSERT dbo.t VALUES(1,2,200,N'active');").await.unwrap();
    db
}
fn remedy(message: &str) {
    assert!(
        message.contains("plan-time assertion") && message.contains("CHECK constraint"),
        "{message}"
    );
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirements_bind_only_target_columns_and_allowlisted_code() {
    let mut db = fixture("require2400_bind").await;
    db.conn
        .execute("CREATE TABLE dbo.other (amount int); CREATE TYPE dbo.amount_alias FROM int;")
        .await
        .unwrap();
    db.conn.execute("ALTER TABLE dbo.t ADD safe_alias dbo.amount_alias, computed AS (amount+1), node hierarchyid;").await.unwrap();
    db.conn
        .execute("CREATE FUNCTION dbo.user_check(@x int) RETURNS int AS BEGIN RETURN @x END;")
        .await
        .unwrap();
    let table = "dbo.t".parse().unwrap();
    assert_eq!(
        vet(
            &mut db.conn,
            &table,
            "cents=amount*100 AND LOWER(status)='active'"
        )
        .await
        .unwrap(),
        ["amount".into(), "cents".into(), "status".into()].into()
    );
    vet(&mut db.conn, &table, "safe_alias IS NULL AND amount=2")
        .await
        .unwrap();
    for text in [
        "GETDATE() IS NOT NULL",
        "dbo.user_check(amount)=2",
        "other.amount=amount",
        "EXISTS(SELECT 1 FROM dbo.other)",
        "1=1); DROP TABLE dbo.t; --",
        "amount=2) AND (1=1",
    ] {
        let error = vet(&mut db.conn, &table, text).await.unwrap_err();
        remedy(&error);
    }
    let error = vet(&mut db.conn, &table, "computed=3").await.unwrap_err();
    assert!(
        error.contains("is computed; use a stored column"),
        "{error}"
    );
    remedy(&error);
    let error = vet(&mut db.conn, &table, "node IS NULL").await.unwrap_err();
    assert!(
        error.contains("CLR type; use a non-CLR stored type"),
        "{error}"
    );
    remedy(&error);
    // Binding checks types without evaluating: divide-by-zero is caught by the
    // check, not by Describe. An invalid conversion signature fails to bind.
    vet(&mut db.conn, &table, "1/0=0").await.unwrap();
    let error = vet(
        &mut db.conn,
        &table,
        "DATEADD(day,1,CAST(status AS xml)) IS NOT NULL",
    )
    .await
    .unwrap_err();
    assert!(error.contains("does not bind"), "{error}");
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirements_use_actual_capabilities_and_catalog_collation() {
    for (collation, accepts) in [
        ("Latin1_General_100_CI_AS", true),
        ("Latin1_General_100_CS_AS", false),
    ] {
        let mut db = TestDb::create_collated("require2400_names", collation).await;
        db.conn
            .execute("CREATE TABLE dbo.t ([Amount] int, s nvarchar(20));")
            .await
            .unwrap();
        let table = "dbo.t".parse().unwrap();
        let result = vet(&mut db.conn, &table, "amount=1").await;
        if accepts {
            assert_eq!(result.unwrap(), ["Amount".into()].into());
        } else {
            remedy(&result.unwrap_err());
        }
        let alter = format!("ALTER DATABASE [{}] SET COMPATIBILITY_LEVEL=150;", db.name);
        db.conn.execute(&alter).await.unwrap();
        let error = vet(&mut db.conn, &table, "TRIM(LEADING 'x' FROM s)='a'")
            .await
            .unwrap_err();
        assert!(error.contains("compatibility 160"), "{error}");
        db.conn
            .execute(&format!(
                "ALTER DATABASE [{}] SET COMPATIBILITY_LEVEL=160;",
                db.name
            ))
            .await
            .unwrap();
        vet(&mut db.conn, &table, "TRIM(LEADING 'x' FROM s)='a'")
            .await
            .unwrap();
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirements_count_false_and_unknown_and_refuse_without_a_transaction() {
    let mut db = fixture("require2400_count").await;
    db.conn
        .execute("INSERT dbo.t VALUES(2,3,299,N'old'),(3,4,NULL,NULL);")
        .await
        .unwrap();
    assert_eq!(
        checked(&mut db, "cents=amount*100").await.unwrap_err(),
        "2 row(s) of dbo.t violate the row requirement `cents=amount*100`; nothing was removed"
    );
    checked(&mut db, "id IN(1,2,3)").await.unwrap();
    let error = check(&mut db.conn, &guard("1=1")).await.unwrap_err();
    assert!(
        error.contains("active, committable drop transaction"),
        "{error}"
    );
    let error = checked(&mut db, "COALESCE(1/0,0)=0").await.unwrap_err();
    assert!(
        error.contains("8134") && error.contains("never permits removal"),
        "{error}"
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirements_pin_binding_and_evaluation_and_restore_the_callers_settings() {
    let mut db = fixture("require2400_pins").await;
    db.conn.execute("SET LANGUAGE German; SET DATEFIRST 1; SET DATEFORMAT dmy; SET ANSI_WARNINGS OFF; SET ARITHABORT OFF;").await.unwrap();
    let predicate = "CAST('01/02/2026' AS date)=DATEFROMPARTS(2026,1,2) AND DATEPART(weekday,CAST('20260104' AS date))=1";
    vet(&mut db.conn, &"dbo.t".parse().unwrap(), predicate)
        .await
        .unwrap();
    checked(&mut db, predicate).await.unwrap();
    let error = checked(&mut db, "COALESCE(1/0,0)=0").await.unwrap_err();
    assert!(error.contains("8134"), "{error}");
    let rows=db.conn.query("SELECT @@DATEFIRST AS first,CASE WHEN CAST('01/02/2026' AS date)=DATEFROMPARTS(2026,2,1) THEN 1 ELSE 0 END AS dmy,COALESCE(1/0,0) AS silent").await.unwrap();
    assert_eq!(rows[0].try_get::<u8>("first").unwrap(), Some(1));
    assert_eq!(rows[0].try_get::<i32>("dmy").unwrap(), Some(1));
    assert_eq!(rows[0].try_get::<i32>("silent").unwrap(), Some(0));
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirements_refuse_filter_policies_and_unreadable_metadata_with_remedies() {
    let mut db = fixture("require2400_rls").await;
    db.conn.execute("CREATE SCHEMA hidden").await.unwrap();
    db.conn.execute("CREATE FUNCTION hidden.filter_row(@id int) RETURNS TABLE WITH SCHEMABINDING AS RETURN SELECT 1 AS allowed WHERE @id=1").await.unwrap();
    db.conn.execute("CREATE SECURITY POLICY hidden.policy ADD FILTER PREDICATE hidden.filter_row(id) ON dbo.t WITH(STATE=ON); INSERT dbo.t VALUES(2,2,199,N'hidden');").await.unwrap();
    let table = "dbo.t".parse().unwrap();
    let error = vet(&mut db.conn, &table, "cents=amount*100")
        .await
        .unwrap_err();
    assert!(
        error.contains("FILTER policy") && error.contains("disable the policy"),
        "{error}"
    );
    remedy(&error);
    assert!(
        checked(&mut db, "cents=amount*100")
            .await
            .unwrap_err()
            .contains("FILTER policy")
    );
    db.conn
        .execute("ALTER SECURITY POLICY hidden.policy WITH(STATE=OFF)")
        .await
        .unwrap();
    vet(&mut db.conn, &table, "cents=amount*100").await.unwrap();
    assert!(
        checked(&mut db, "cents=amount*100")
            .await
            .unwrap_err()
            .starts_with("1 row(s)")
    );
    db.conn.execute("DROP SECURITY POLICY hidden.policy; CREATE SECURITY POLICY hidden.block_policy ADD BLOCK PREDICATE hidden.filter_row(id) ON dbo.t AFTER INSERT WITH(STATE=ON);").await.unwrap();
    vet(&mut db.conn, &table, "id>0").await.unwrap();
    checked(&mut db, "id>0").await.unwrap();
    db.conn.execute("CREATE USER reader WITHOUT LOGIN; GRANT SELECT ON dbo.t TO reader; EXECUTE AS USER='reader';").await.unwrap();
    let error = vet(&mut db.conn, &table, "id>0").await.unwrap_err();
    assert!(error.contains("grant database VIEW DEFINITION"), "{error}");
    db.conn
        .execute("REVERT; GRANT VIEW DEFINITION TO reader; EXECUTE AS USER='reader'")
        .await
        .unwrap();
    vet(&mut db.conn, &table, "id>0").await.unwrap();
    checked(&mut db, "id>0").await.unwrap();
    db.conn
        .execute(
            "REVERT; DENY VIEW DEFINITION ON SCHEMA::hidden TO reader; EXECUTE AS USER='reader'",
        )
        .await
        .unwrap();
    let error = vet(&mut db.conn, &table, "id>0").await.unwrap_err();
    assert!(
        error.contains("effective object/schema metadata DENYs"),
        "{error}"
    );
    db.conn.execute("REVERT; REVOKE VIEW DEFINITION ON SCHEMA::hidden FROM reader; DENY SELECT ON dbo.t TO reader; EXECUTE AS USER='reader'").await.unwrap();
    let error = vet(&mut db.conn, &table, "id>0").await.unwrap_err();
    assert!(
        error.contains("does not bind") && error.contains("229"),
        "{error}"
    );
    db.conn.execute("REVERT").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirement_counts_are_current_even_inside_a_preexisting_snapshot() {
    let mut db = fixture("require2400_snapshot").await;
    db.conn.execute(&format!("ALTER DATABASE [{}] SET ALLOW_SNAPSHOT_ISOLATION ON; SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRANSACTION;",db.name)).await.unwrap();
    db.conn
        .query("SELECT COUNT_BIG(*) FROM dbo.t")
        .await
        .unwrap();
    let mut writer = connect_live(&conn_str()).await.unwrap();
    writer
        .execute(&format!(
            "USE [{}]; INSERT dbo.t VALUES(2,3,299,N'late');",
            db.name
        ))
        .await
        .unwrap();
    assert_eq!(
        check(&mut db.conn, &guard("cents=amount*100"))
            .await
            .unwrap_err(),
        "1 row(s) of dbo.t violate the row requirement `cents=amount*100`; nothing was removed"
    );
    db.conn
        .execute("ROLLBACK; SET TRANSACTION ISOLATION LEVEL READ COMMITTED;")
        .await
        .unwrap();
    drop(writer);
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn requirement_count_locks_every_partition_until_the_emitted_drop_commits() {
    // Empty and constant-true controls also have to lock: the optimizer must
    // not turn an empty/constant count into an unprotected drop.
    for empty in [false, true] {
        let mut db = TestDb::create("require2400_writer").await;
        db.conn.execute("CREATE PARTITION FUNCTION pf(int) AS RANGE RIGHT FOR VALUES(100); CREATE PARTITION SCHEME ps AS PARTITION pf ALL TO([PRIMARY]); CREATE TABLE dbo.t(id int, amount int) ON ps(id);").await.unwrap();
        if !empty {
            db.conn
                .execute("INSERT dbo.t VALUES(1,1),(101,1)")
                .await
                .unwrap();
        }
        let frame = Mssql.transaction_framing(&pbps_model::SessionZone::utc());
        db.conn.begin(&frame).await.unwrap();
        let g = guard(if empty { "1=1" } else { "amount=1" });
        check(&mut db.conn, &g).await.unwrap();
        let mut writer = connect_live(&conn_str()).await.unwrap();
        writer
            .execute(&format!("USE [{}]; SET DEADLOCK_PRIORITY LOW;", db.name))
            .await
            .unwrap();
        let sid = writer
            .query("SELECT CONVERT(int,@@SPID) AS sid")
            .await
            .unwrap()[0]
            .try_get::<i32>("sid")
            .unwrap()
            .unwrap();
        let task = tokio::spawn(async move {
            writer
                .execute("INSERT dbo.t(id,amount) VALUES(102,0)")
                .await
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let rows=db.conn.query(&format!("SELECT COUNT_BIG(*) AS n FROM sys.dm_exec_requests WHERE session_id={sid} AND blocking_session_id=@@SPID AND wait_type LIKE 'LCK_M_%'")).await.unwrap();
            if rows[0].try_get::<i64>("n").unwrap() == Some(1) {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                db.conn.rollback(&frame).await.unwrap();
                let _ = task.await;
                panic!("writer never blocked behind the guarded count");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut planned = PlannedChange::new(pbps_model::Change::DropColumn {
            uid: "c_000001".parse().unwrap(),
            column: "dbo.t.amount".parse().unwrap(),
        });
        planned.drop_guard = Some(g.clone());
        let plan = pbps_diff::order::singleton(planned, PlanMode::Transactional, &Mssql);
        let statements = Mssql.prepare_emission(&plan, PlanMode::Transactional)(0).unwrap();
        assert_eq!(statements[0].drop_guard.as_ref(), Some(&g));
        assert!(Mssql.emit_staged(&plan, 0).is_err());
        for statement in statements {
            db.conn.execute(&statement.sql).await.unwrap();
        }
        db.conn.commit(&frame).await.unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error.server_error_code().as_deref(), Some("207" | "1205")),
            "{error}"
        );
        let n = db
            .conn
            .query("SELECT COUNT_BIG(*) AS n FROM dbo.t")
            .await
            .unwrap()[0]
            .try_get::<i64>("n")
            .unwrap()
            .unwrap();
        assert_eq!(n, if empty { 0 } else { 2 });
        db.drop().await;
    }
}
