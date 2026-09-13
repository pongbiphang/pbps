use super::*;
use pbps_model::{Cell, Change, Row, RowKey, Value};

fn insert(table: &str, key: &str) -> Change {
    Change::InsertRow {
        table: TableName::new("dbo", table),
        key_column: "co'de]x".into(),
        identity_key: false,
        key: RowKey::from(key),
        row: Row([("label".into(), Value::Text("a".into()))].into()),
        defaults: Default::default(),
        types: [("label".into(), ty("nvarchar(20)"))].into(),
    }
}

fn update(table: &str, key: &str) -> Change {
    Change::UpdateRow {
        table: TableName::new("dbo", table),
        key_column: "co'de]x".into(),
        key: RowKey::from(key),
        columns: [(
            "label".into(),
            (
                Cell::Value(Value::Text("a".into())),
                Cell::Value(Value::Text("b".into())),
            ),
        )]
        .into(),
        unchanged: Default::default(),
        types: [("label".into(), ty("nvarchar(20)"))].into(),
        after_types: Default::default(),
    }
}

async fn contents(conn: &mut Conn, table: &str) -> Vec<(String, String)> {
    conn.query(&format!(
        "SELECT CONVERT(nvarchar(max), [co'de]]x]), label FROM dbo.{table}"
    ))
    .await
    .unwrap()
    .iter()
    .map(|row| {
        (
            row.try_get_at::<&str>(0).unwrap().unwrap().to_owned(),
            row.try_get_at::<&str>(1).unwrap().unwrap().to_owned(),
        )
    })
    .collect()
}

async fn respelled_key(is_update: bool, key_type: &str, append_space: bool) {
    let mut db = TestDb::create(&format!("key_{is_update}_{append_space}218")).await;
    db.conn.execute(&format!("CREATE TABLE dbo.key218 ([co'de]]x] {key_type} COLLATE SQL_Latin1_General_CP1_CI_AS PRIMARY KEY, label nvarchar(20));")).await.unwrap();
    let key = "Ne'w";
    let seed = Mssql
        .emit(&insert("key218", key), Default::default())
        .unwrap()
        .remove(0);
    let change = if is_update {
        update("key218", key)
    } else {
        insert("key218", key)
    };
    let stmt = Mssql.emit(&change, Default::default()).unwrap().remove(0);
    if is_update {
        db.conn.execute(&seed.sql).await.unwrap();
    }
    let plain = db
        .conn
        .execute(&stmt.sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    let plain_rows = contents(&mut db.conn, "key218").await;
    db.conn.execute("DELETE FROM dbo.key218;").await.unwrap();
    if is_update {
        db.conn.execute(&seed.sql).await.unwrap();
    }
    let rewrite = if append_space {
        "t.[co'de]]x] + N' '"
    } else {
        "LOWER(t.[co'de]]x])"
    };
    db.conn.execute(&format!("CREATE TRIGGER dbo.respell218 ON dbo.key218 AFTER INSERT, UPDATE AS BEGIN SET NOCOUNT ON; IF TRIGGER_NESTLEVEL() > 1 RETURN; UPDATE t SET [co'de]]x]={rewrite} FROM dbo.key218 t JOIN inserted i ON t.[co'de]]x]=i.[co'de]]x]; END;")).await.unwrap();
    let triggered = db
        .conn
        .execute(&stmt.sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    // The statement owns its transaction; inspect its rollback without an
    // outer test transaction that could undo a mistakenly accepted write.
    let triggered_rows = contents(&mut db.conn, "key218").await;
    db.drop().await;
    assert!(plain.is_ok(), "{plain:?}");
    assert_eq!(
        plain_rows,
        vec![(key.into(), if is_update { "b" } else { "a" }.into())]
    );
    let refusal = triggered.expect_err("the key spelling changed inside the write");
    assert!(refusal.contains("is not what this plan wrote"), "{refusal}");
    assert_eq!(
        triggered_rows,
        if is_update {
            vec![(key.into(), "a".into())]
        } else {
            vec![]
        }
    );
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_trigger_that_respells_the_inserted_key_rolls_the_statement_back() {
    respelled_key(false, "varchar(20)", false).await;
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_trigger_that_respells_the_updated_key_rolls_the_statement_back() {
    respelled_key(true, "varchar(20)", false).await;
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_trigger_that_appends_spaces_to_the_inserted_key_rolls_back() {
    for key_type in ["varchar(20)", "nvarchar(20)"] {
        respelled_key(false, key_type, true).await;
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_trigger_that_appends_spaces_to_the_updated_key_rolls_back() {
    for key_type in ["varchar(20)", "nvarchar(20)"] {
        respelled_key(true, key_type, true).await;
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn canonical_non_text_keys_and_padded_text_keys_still_accept_writes() {
    let mut db = TestDb::create("key_types218").await;
    let mut results = Vec::new();
    // Spellings measured through the canonical read-back on this engine.
    // Money, float and datetime render differently under generic CONVERT;
    // both sides of the additional postcondition must still agree.
    for (tag, key_type, key) in [
        ("int", "int", "7"),
        ("decimal", "decimal(5,2)", "1.50"),
        // Numeric key aliases are accepted intentionally (DECISIONS 71/101).
        ("int_alias", "int", "01"),
        ("decimal_alias", "decimal(5,2)", "1.5"),
        ("money", "money", "1.0001"),
        ("float", "float", "-2.5500000000000000e+002"),
        ("date", "date", "2026-09-04"),
        ("datetime", "datetime2(3)", "2026-09-04T12:34:56.123"),
        ("char", "char(10)", "New"),
        ("nchar", "nchar(10)", "New"),
        ("varchar", "varchar(20)", "New "),
        ("nvarchar", "nvarchar(20)", "New "),
        ("hierarchy", "hierarchyid", "/1/"),
        (
            "uuid",
            "uniqueidentifier",
            "11223344-5566-7788-99AA-BBCCDDEEFF00",
        ),
    ] {
        let table = format!("key218_{tag}");
        db.conn
            .execute(&format!(
                "CREATE TABLE dbo.{table} ([co'de]]x] {key_type} PRIMARY KEY, label nvarchar(20))"
            ))
            .await
            .unwrap();
        for change in [insert(&table, key), update(&table, key)] {
            let sql = Mssql
                .emit(&change, Default::default())
                .unwrap()
                .remove(0)
                .sql;
            let result = db
                .conn
                .execute(&sql)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            results.push((key_type, result));
        }
    }
    // Generic text rounds both money values to 1.00. Native equality must
    // still catch a changed key despite that loss of precision.
    db.conn.execute("CREATE TRIGGER dbo.money218 ON dbo.key218_money AFTER UPDATE AS BEGIN SET NOCOUNT ON; IF TRIGGER_NESTLEVEL() > 1 RETURN; UPDATE dbo.key218_money SET [co'de]]x]=1.0002; END;").await.unwrap();
    db.conn
        .execute("UPDATE dbo.key218_money SET label=N'a';")
        .await
        .unwrap();
    // Restore the baseline without firing the trigger before the attempt.
    db.conn.execute("DISABLE TRIGGER dbo.money218 ON dbo.key218_money; UPDATE dbo.key218_money SET [co'de]]x]=1.0001; ENABLE TRIGGER dbo.money218 ON dbo.key218_money;").await.unwrap();
    let sql = Mssql
        .emit(&update("key218_money", "1.0001"), Default::default())
        .unwrap()
        .remove(0)
        .sql;
    let money_refusal = db
        .conn
        .execute(&sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    let money_rows = db
        .conn
        .query("SELECT CONVERT(nvarchar(max), [co'de]]x], 2) FROM dbo.key218_money")
        .await
        .unwrap();
    let money_value = money_rows
        .first()
        .map(|row| row.try_get_at::<&str>(0).unwrap().unwrap().to_owned());
    db.drop().await;
    for (key_type, result) in results {
        assert!(result.is_ok(), "{key_type}: {result:?}");
    }
    assert!(
        money_refusal
            .unwrap_err()
            .contains("is not what this plan wrote")
    );
    assert_eq!(money_value.as_deref(), Some("1.0001"));
}

async fn lossy_key(is_update: bool, key_type: &str, stored: &str, declared: &str) {
    let mut db = TestDb::create(&format!("key_lossy_{is_update}218")).await;
    db.conn.execute(&format!("CREATE TABLE dbo.key218 ([co'de]]x] {key_type} COLLATE SQL_Latin1_General_CP1_CI_AS PRIMARY KEY, label nvarchar(20));")).await.unwrap();
    let seed = Mssql
        .emit(&insert("key218", stored), Default::default())
        .unwrap()
        .remove(0);
    let plain = db
        .conn
        .execute(&seed.sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    let plain_rows = contents(&mut db.conn, "key218").await;
    if !is_update {
        db.conn.execute("DELETE FROM dbo.key218;").await.unwrap();
    }
    let change = if is_update {
        update("key218", declared)
    } else {
        insert("key218", declared)
    };
    let stmt = Mssql.emit(&change, Default::default()).unwrap().remove(0);
    let attempted = db
        .conn
        .execute(&stmt.sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    // INSERT assigns the key and must retain its text. UPDATE only uses
    // the declared alias to find the existing key; it does not assign it.
    let after = contents(&mut db.conn, "key218").await;
    db.drop().await;
    assert!(plain.is_ok(), "{key_type}: {plain:?}");
    assert_eq!(plain_rows, vec![(stored.into(), "a".into())]);
    if is_update {
        assert!(attempted.is_ok(), "{key_type}: {attempted:?}");
        assert_eq!(after, vec![(stored.into(), "b".into())]);
    } else {
        let error = attempted.expect_err("the engine cannot retain the declared key text");
        assert!(error.contains("is not what this plan wrote"), "{error}");
        assert!(after.is_empty());
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn an_insert_refuses_keys_that_lose_declared_text() {
    for (key_type, stored, declared) in [
        ("varchar(3)", "abc", "abc "),
        ("nvarchar(3)", "abc", "abc "),
        ("varchar(3)", "A", "Ａ"),
    ] {
        lossy_key(false, key_type, stored, declared).await;
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn an_update_preserves_existing_text_key_aliases() {
    for (key_type, stored, declared) in [
        ("varchar(20)", "ne'w", "Ne'w"),
        ("nvarchar(20)", "ne'w", "Ne'w"),
        ("varchar(3)", "abc", "abc "),
        ("nvarchar(3)", "abc", "abc "),
        ("varchar(3)", "A", "Ａ"),
    ] {
        lossy_key(true, key_type, stored, declared).await;
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn an_update_refuses_a_trigger_that_rewrites_an_existing_key_alias() {
    let mut db = TestDb::create("key_alias_rewrite218").await;
    let mut results = Vec::new();
    for (tag, key_type, rewritten) in [
        ("case", "varchar(20)", "Ne'w"),
        ("space", "nvarchar(20)", "ne'w "),
    ] {
        let table = format!("key_alias218_{tag}");
        db.conn.execute(&format!("CREATE TABLE dbo.{table} ([co'de]]x] {key_type} COLLATE SQL_Latin1_General_CP1_CI_AS PRIMARY KEY, label nvarchar(20)); INSERT dbo.{table} VALUES (N'ne''w', N'a');")).await.unwrap();
        let stmt = Mssql
            .emit(&update(&table, "Ne'w"), Default::default())
            .unwrap()
            .remove(0);
        let plain = db
            .conn
            .execute(&stmt.sql)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        let plain_rows = contents(&mut db.conn, &table).await;
        db.conn
            .execute(&format!("UPDATE dbo.{table} SET label=N'a';"))
            .await
            .unwrap();
        let rewritten = rewritten.replace('\'', "''");
        // Capturing the old key must not mark that column as assigned.
        // A rewrite to the declaration itself is still a change to this row.
        db.conn.execute(&format!("CREATE TRIGGER dbo.alias218_{tag} ON dbo.{table} AFTER UPDATE AS BEGIN SET NOCOUNT ON; IF TRIGGER_NESTLEVEL() > 1 RETURN; IF UPDATE([co'de]]x]) THROW 50000, 'the statement assigned its key', 1; UPDATE dbo.{table} SET [co'de]]x]=N'{rewritten}'; END;")).await.unwrap();
        let triggered = db
            .conn
            .execute(&stmt.sql)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        let triggered_rows = contents(&mut db.conn, &table).await;
        results.push((tag, plain, plain_rows, triggered, triggered_rows));
    }
    db.drop().await;
    for (tag, plain, plain_rows, triggered, triggered_rows) in results {
        assert!(plain.is_ok(), "{tag}: {plain:?}");
        assert_eq!(plain_rows, vec![("ne'w".into(), "b".into())]);
        let refusal = triggered.expect_err("a trigger rewrote the stored alias");
        assert!(
            refusal.contains("is not what this plan wrote"),
            "{tag}: {refusal}"
        );
        assert_eq!(triggered_rows, vec![("ne'w".into(), "a".into())]);
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn exported_updates_keep_key_capture_variables_in_separate_batches() {
    let mut db = TestDb::create("key_batch218").await;
    db.conn.execute("CREATE TABLE dbo.key_batch218 ([co'de]]x] varchar(20) PRIMARY KEY, label nvarchar(20)); INSERT dbo.key_batch218 VALUES ('first', N'a'), ('second', N'a');").await.unwrap();
    let mut statements = Vec::new();
    for key in ["first", "second"] {
        statements.extend(
            Mssql
                .emit(&update("key_batch218", key), Default::default())
                .unwrap(),
        );
    }
    let script = pbps_dialect::render_script(&statements, &Mssql);
    let mut results = Vec::new();
    for batch in script
        .split("\nGO\n")
        .filter(|batch| !batch.trim().is_empty())
    {
        results.push(
            db.conn
                .execute(batch)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
        );
    }
    let mut rows = contents(&mut db.conn, "key_batch218").await;
    rows.sort();
    db.drop().await;
    for result in results {
        assert!(result.is_ok(), "{result:?}\n{script}");
    }
    assert_eq!(
        rows,
        vec![("first".into(), "b".into()), ("second".into(), "b".into())]
    );
}
