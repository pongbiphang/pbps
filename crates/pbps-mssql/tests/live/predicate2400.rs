//! Each allowlisted entry is compared with native TRUE/FALSE/UNKNOWN results.
use super::TestDb;
use pbps_dialect::predicate::{Capabilities, parse};
use pbps_model::ColumnName;
use pbps_mssql::Mssql;

fn regenerate(sql: &str, caps: Capabilities) -> Result<String, String> {
    parse(sql, Mssql)
        .and_then(|p| {
            p.vet(
                |n| {
                    ["amount", "s", "j", "path", "odd]name", "a\"b"]
                        .into_iter()
                        .find(|v| *v == n.raw())
                        .map(ColumnName::new)
                },
                caps,
            )
        })
        .and_then(|v| v.sql())
        .map_err(|e| e.to_string())
}
async fn facts(db: &mut TestDb) -> Capabilities {
    let rows = db.conn.query("SELECT CONVERT(int,SERVERPROPERTY('ProductMajorVersion')) AS major, compatibility_level AS compat FROM sys.databases WHERE name=DB_NAME()").await.unwrap();
    Capabilities {
        major: rows[0]
            .try_get::<i32>("major")
            .unwrap()
            .unwrap()
            .try_into()
            .unwrap(),
        compatibility: rows[0].try_get::<u8>("compat").unwrap().unwrap().into(),
    }
}
async fn fixture(tag: &str) -> TestDb {
    let mut db = TestDb::create(tag).await;
    db.conn.execute(r#"SET QUOTED_IDENTIFIER ON;
        CREATE TABLE dbo.t (amount int, s nvarchar(100), j nvarchar(100), path nvarchar(20), [odd]]name] nvarchar(100), [a"b] nvarchar(100));
        INSERT dbo.t VALUES (1,N'abc',N'{"x":"value"}',N'$.x',N'x''; DROP TABLE dbo.t; --',N'α'),
        (2,N' xyz ',N'{"x":"other"}',N'$.x',N'other',N'other'),
        (NULL,NULL,NULL,N'$.x',NULL,NULL);"#).await.unwrap();
    db
}
async fn equivalent(db: &mut TestDb, sql: &str) -> Vec<i32> {
    let caps = facts(db).await;
    let regenerated = regenerate(sql, caps).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let roundtrip = regenerate(&regenerated, caps).unwrap();
    assert_eq!(regenerated, roundtrip);
    let rows = db.conn.query(&format!("SELECT CASE WHEN ({sql}) THEN 1 WHEN NOT ({sql}) THEN 0 ELSE -1 END AS native, CASE WHEN ({regenerated}) THEN 1 WHEN NOT ({regenerated}) THEN 0 ELSE -1 END AS regenerated FROM dbo.t ORDER BY amount")).await.unwrap_or_else(|e| panic!("{sql}\n{regenerated}\n{e}"));
    rows.iter()
        .map(|row| {
            let native = row.try_get::<i32>("native").unwrap().unwrap();
            assert_eq!(
                Some(native),
                row.try_get::<i32>("regenerated").unwrap(),
                "{sql}\n{regenerated}"
            );
            native
        })
        .collect()
}
async fn entry(tag: &str, positive: &str, negative: &str) {
    let mut db = fixture(tag).await;
    let observations = equivalent(&mut db, positive).await;
    assert_eq!(observations.len(), 3);
    assert!(
        observations.contains(&1),
        "positive must be true for a fixture row"
    );
    assert!(
        observations.iter().any(|v| *v != 1),
        "negative or NULL control must differ"
    );
    let error = regenerate(negative, facts(&mut db).await).unwrap_err();
    assert!(
        error.contains("plan-time assertion") && error.contains("CHECK constraint"),
        "{error}"
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn cast_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_cast",
        r#"CAST(s AS varchar)='abc'"#,
        r#"CAST(s AS varchar(0)) IS NULL"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn convert_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_convert",
        r#"CONVERT(varchar,s,0)='abc'"#,
        r#"CONVERT(varchar,s,amount) IS NULL"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn coalesce_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_coalesce",
        r#"COALESCE(amount,0)=0"#,
        r#"COALESCE(amount)=0"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn isnull_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_isnull",
        r#"ISNULL(amount,0)=0"#,
        r#"ISNULL(amount)=0"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn nullif_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_nullif",
        r#"NULLIF(amount,1)=2"#,
        r#"NULLIF(amount)=0"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn left_regeneration_matches_native_three_valued_logic() {
    entry("predicate2400_left", r#"LEFT(s,1)='a'"#, r#"LEFT(s)='a'"#).await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn right_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_right",
        r#"RIGHT(s,1)='c'"#,
        r#"RIGHT(s,1,2)='c'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn substring_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_substring",
        r#"SUBSTRING(s,2,2)='bc'"#,
        r#"SUBSTRING(s)='bc'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn charindex_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_charindex",
        r#"CHARINDEX('a',s,1)=1"#,
        r#"CHARINDEX('a')=1"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn replace_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_replace",
        r#"REPLACE(s,'a','z')='zbc'"#,
        r#"REPLACE(s,'a')='zbc'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn lower_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_lower",
        r#"LOWER(s)='abc'"#,
        r#"LOWER(s,s)='abc'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn upper_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_upper",
        r#"UPPER(s)='ABC'"#,
        r#"UPPER()='ABC'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn ltrim_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_ltrim",
        r#"LTRIM(s)='abc'"#,
        r#"LTRIM(s,'x','y')='abc'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn rtrim_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_rtrim",
        r#"RTRIM(s)='abc'"#,
        r#"RTRIM(s,'x','y')='abc'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn trim_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_trim",
        r#"TRIM('x' FROM s)='abc'"#,
        r#"TRIM(s,s)='abc'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn len_regeneration_matches_native_three_valued_logic() {
    entry("predicate2400_len", r#"LEN(s)=3"#, r#"LEN(s,s)=3"#).await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn concat_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_concat",
        r#"CONCAT(s,'!')='abc!'"#,
        r#"CONCAT(s)='abc!'"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn round_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_round",
        r#"ROUND(amount,0,1)=1"#,
        r#"ROUND(amount)=1"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn floor_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_floor",
        r#"FLOOR(amount)=1"#,
        r#"FLOOR(amount,1)=1"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn ceiling_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_ceiling",
        r#"CEILING(amount)=1"#,
        r#"CEILING()=1"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn abs_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_abs",
        r#"ABS(amount)=1"#,
        r#"ABS(amount,1)=1"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn dateadd_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_dateadd",
        r#"DATEADD(day,amount,CAST('20260101' AS date))=CAST('20260102' AS date)"#,
        r#"DATEADD(bad,1,'20260101') IS NULL"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn datediff_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_datediff",
        r#"DATEDIFF(day,'20260101',DATEADD(day,amount,CAST('20260101' AS date)))=1"#,
        r#"DATEDIFF(tzoffset,'20260101','20260102')=1"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn datepart_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_datepart",
        r#"DATEPART(day,DATEADD(day,amount,CAST('20260101' AS date)))=2"#,
        r#"DATEPART('year','20260101')=2026"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn datefromparts_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_datefromparts",
        r#"DATEFROMPARTS(2026,1,amount)=CAST('20260101' AS date)"#,
        r#"DATEFROMPARTS(2026,1) IS NULL"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn json_value_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_json_value",
        r#"JSON_VALUE(j,'$.x')='value'"#,
        r#"JSON_VALUE(j) IS NULL"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn case_regeneration_matches_native_three_valued_logic() {
    entry(
        "predicate2400_case",
        r#"CASE amount WHEN 1 THEN 10 ELSE CASE WHEN amount IS NULL THEN 0 ELSE 20 END END=0"#,
        r#"CASE WHEN amount THEN 1 ELSE 0 END=0"#,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn grouping_quoting_and_plain_strings_preserve_native_results() {
    let mut db = fixture("predicate2400_grammar").await;
    for sql in [
        "1+2*3=7",
        "1.0/3>0",
        "1/3=0",
        "1E0/3>0",
        "'1'+'2'='12'",
        "'a_b' LIKE 'a!_b' ESCAPE '!'",
        "-1*2+3=1",
        // Overflow distinguishes sign binding from algebraically equal regrouping.
        "-CAST(-2147483648 AS int)*0=0",
        "8/2/2=2",
        "1=1 OR 0=1 AND NOT 2=3",
        "NOT amount IN (1,2)",
        "1=0 OR 5 BETWEEN 1 AND 10 AND 0=1",
        "amount NOT BETWEEN 1 AND 2 OR amount IS NULL",
        "amount != 1 AND amount !< 0 OR amount !> 1",
        "s + N'!' LIKE N'a!_%' ESCAPE N'!'",
        "s NOT LIKE N'%z%'",
        "[odd]]name] = N'x''; DROP TABLE dbo.t; --'",
        "\"a\"\"b\"=N'α'",
        "CAST('α' AS nvarchar(10))=N'α'",
        "CAST(N'α' AS nvarchar(10))=N'α'",
        "LEN(CAST('123456789012345678901234567890123456' AS varchar))=30",
        "LEN(CONVERT(varchar,'123456789012345678901234567890123456'))=30",
        "CASE WHEN amount=1 THEN 1 ELSE 0 END=1",
    ] {
        equivalent(&mut db, sql).await;
    }
    assert_eq!(
        equivalent(&mut db, "CAST('α' AS nvarchar(10))=N'α'").await,
        vec![0; 3]
    );
    assert_eq!(
        equivalent(&mut db, "CAST(N'α' AS nvarchar(10))=N'α'").await,
        vec![1; 3]
    );
    for sql in ["amount=1", "amount=2", "amount=NULL"] {
        let values = equivalent(&mut db, sql).await;
        assert!(values.contains(&-1));
    }
    let mut nested = "1".to_owned();
    for _ in 0..10 {
        nested = format!("CASE WHEN 1=1 THEN {nested} ELSE 0 END");
    }
    let ten = format!("{nested}=1");
    let regenerated = regenerate(&ten, facts(&mut db).await).unwrap();
    // The truth-comparison helper itself adds a CASE. Measure the engine's
    // ten-level boundary in WHERE so the fixture does not add an eleventh.
    for sql in [&ten, &regenerated] {
        assert_eq!(
            db.conn
                .query(&format!("SELECT 1 WHERE {sql}"))
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let eleven = format!("CASE WHEN 1=1 THEN {nested} ELSE 0 END=1");
    assert!(regenerate(&eleven, facts(&mut db).await).is_err());
    assert!(
        db.conn
            .query(&format!("SELECT 1 WHERE {eleven}"))
            .await
            .err()
            .expect("the engine must refuse eleven nested CASE expressions")
            .to_string()
            .contains("125")
    );
    let caps = facts(&mut db).await;
    for sql in [
        "1=1); DROP TABLE dbo.t; --",
        "other=1",
        "dbo.t.amount=1",
        "EXISTS(SELECT 1)",
        "0x01=0x01",
        "1&1=1",
    ] {
        assert!(regenerate(sql, caps).is_err(), "{sql}");
    }
    assert_eq!(db.conn.query("SELECT * FROM dbo.t").await.unwrap().len(), 3);
    db.drop().await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn current_engine_and_compatibility_facts_gate_new_function_shapes() {
    let mut db = fixture("predicate2400_versions").await;
    let caps = facts(&mut db).await;
    assert_eq!(
        caps.major, 17,
        "update the capability test for a newly pinned image"
    );
    for sql in [
        "TRIM(LEADING s)='abc'",
        "TRIM(TRAILING FROM s)='abc'",
        "TRIM(BOTH s)='abc'",
    ] {
        assert!(regenerate(sql, caps).is_err(), "{sql}");
        assert!(
            db.conn
                .query(&format!("SELECT 1 WHERE {sql}"))
                .await
                .is_err(),
            "{sql}"
        );
    }
    for sql in [
        "SUBSTRING(s,2)='bc'",
        "JSON_VALUE(j,path)='value'",
        "TRIM(LEADING 'x' FROM s)='abc'",
        "LTRIM(s,'x')='abc'",
        "RTRIM(s,'x')='abc'",
    ] {
        equivalent(&mut db, sql).await;
    }
    db.conn
        .execute(&format!(
            "ALTER DATABASE [{}] SET COMPATIBILITY_LEVEL=150",
            db.name
        ))
        .await
        .unwrap();
    let caps = facts(&mut db).await;
    for sql in [
        "TRIM(LEADING 'x' FROM s)='abc'",
        "TRIM(TRAILING ' ' FROM s)='abc'",
        "TRIM(BOTH ' ' FROM s)='abc'",
        "LTRIM(s,'x')='abc'",
        "RTRIM(s,'x')='abc'",
    ] {
        assert!(regenerate(sql, caps).is_err(), "{sql}");
        assert!(
            db.conn
                .query(&format!("SELECT 1 WHERE {sql}"))
                .await
                .is_err(),
            "engine admitted {sql} at 150"
        );
    }
    equivalent(&mut db, "TRIM('x' FROM s)='abc'").await;
    equivalent(&mut db, "LTRIM(s)='abc'").await;
    db.conn
        .execute(&format!(
            "ALTER DATABASE [{}] SET COMPATIBILITY_LEVEL=160",
            db.name
        ))
        .await
        .unwrap();
    for sql in [
        "TRIM(LEADING 'x' FROM s)='abc'",
        "TRIM(TRAILING ' ' FROM s)='abc'",
        "TRIM(BOTH ' ' FROM s)='abc'",
        "LTRIM(s,'x')='abc'",
        "RTRIM(s,'x')='abc'",
    ] {
        equivalent(&mut db, sql).await;
    }
    db.drop().await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn datefirst_and_dateformat_remain_session_dependent() {
    let mut db = fixture("predicate2400_session").await;
    db.conn
        .execute("SET DATEFIRST 1; SET DATEFORMAT mdy;")
        .await
        .unwrap();
    assert_eq!(
        equivalent(&mut db, "DATEPART(weekday,CAST('20260104' AS date))=7").await,
        vec![1; 3]
    );
    assert_eq!(
        equivalent(
            &mut db,
            "CAST('01/02/2026' AS date)=CAST('20260102' AS date)"
        )
        .await,
        vec![1; 3]
    );
    db.conn
        .execute("SET DATEFIRST 7; SET DATEFORMAT dmy;")
        .await
        .unwrap();
    assert_eq!(
        equivalent(&mut db, "DATEPART(weekday,CAST('20260104' AS date))=7").await,
        vec![0; 3]
    );
    assert_eq!(
        equivalent(
            &mut db,
            "CAST('01/02/2026' AS date)=CAST('20260102' AS date)"
        )
        .await,
        vec![0; 3]
    );
    db.drop().await;
}
