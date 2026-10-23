//! CLR binaries belong to external staging, never reported compatibility facts.

use super::super::{TestDb, connect_live};
use pbps_db::{Conn, DbError};
use pbps_mssql::resolver::{capture, environment};
use std::collections::BTreeSet;

async fn assembly_case(reached: bool) {
    let mut db = TestDb::create("reported2270").await;
    let binary = include_str!("scratch2236-provider.hex").trim();
    let found = db.conn.query(&format!("SELECT COUNT_BIG(*) AS count FROM sys.trusted_assemblies WHERE hash=HASHBYTES('SHA2_512',0x{binary});")).await.unwrap();
    let added_trust = found[0].try_get::<i64>("count").unwrap() == Some(0);
    if added_trust {
        db.conn.execute(&format!("DECLARE @hash varbinary(64)=HASHBYTES('SHA2_512',0x{binary}); EXEC sys.sp_add_trusted_assembly @hash=@hash,@description=N'pbps2270 reported fixture';")).await.unwrap();
    }
    let mut conn = connect_live(&format!(
        "{};Database={}",
        std::env::var("PBPS_TEST_DB").unwrap(),
        db.name
    ))
    .await
    .unwrap();
    // Preserve the outer fixture and trust obligation even if a regression
    // panics, as the revert-and-fail checks intentionally do.
    let result = tokio::spawn(async move {
        let result: Result<(), DbError> = async {
        let schemas = vec!["dbo".into()];
        let scope = environment::Scope { schemas: &schemas };
        let before = environment::read(&mut conn, &scope).await?;
        conn.execute(&format!("CREATE ASSEMBLY provider FROM 0x{binary} WITH PERMISSION_SET=SAFE;")).await?;
        let rows = conn.query("SELECT name FROM sys.assemblies WHERE is_user_defined=1;").await?;
        assert_eq!(rows.len(), 1, "the assembly really exists");
        let after = environment::read(&mut conn, &scope).await?;
        assert!(after.extensions.is_empty(), "an unused assembly is not a reported fact");
        assert_eq!(before, after);
        if reached {
            conn.execute("CREATE FUNCTION dbo.clr2270() RETURNS int AS EXTERNAL NAME provider.Provider.Value;").await?;
            conn.execute("CREATE VIEW dbo.managed2270 AS SELECT dbo.clr2270() AS i;").await?;
            let rows = conn.query("SELECT o.type,OBJECT_NAME(d.referenced_id) AS referenced FROM sys.objects o JOIN sys.sql_expression_dependencies d ON d.referenced_id=o.object_id WHERE d.referencing_id=OBJECT_ID(N'dbo.managed2270');").await?;
            assert_eq!(rows[0].try_get::<&str>("type")?, Some("FS"));
            assert_eq!(rows[0].try_get::<&str>("referenced")?, Some("clr2270"));
            let result = capture::capture(&mut conn, &capture::CaptureScope {
                retained: BTreeSet::from([capture::object("dbo", "managed2270")]),
                candidates: BTreeSet::new(),
            }).await;
            assert!(matches!(&result, Err(capture::CaptureError::Coverage(u)) if u.object.as_ref()==Some(&capture::object("dbo","clr2270")) && u.condition.contains("assembly")), "{:?}", result.as_ref().err());
        } else {
            conn.execute("CREATE TABLE dbo.ordinary2270(i int);").await?;
            capture::capture(&mut conn, &capture::CaptureScope {
                retained: BTreeSet::from([capture::object("dbo", "ordinary2270")]),
                candidates: BTreeSet::new(),
            }).await.expect("an unreached assembly does not block ordinary objects");
        }
        Ok(())
    }.await;
        result
    }).await;
    db.drop().await;
    if added_trust {
        let mut admin: Conn = connect_live(&std::env::var("PBPS_TEST_DB").unwrap())
            .await
            .unwrap();
        admin.execute(&format!("DECLARE @hash varbinary(64)=HASHBYTES('SHA2_512',0x{binary}); EXEC sys.sp_drop_trusted_assembly @hash=@hash;")).await.unwrap();
    }
    result.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn unreached_user_assembly_is_not_a_reported_fact() {
    assembly_case(false).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn reached_clr_object_refuses_until_binary_staging_exists() {
    assembly_case(true).await;
}
