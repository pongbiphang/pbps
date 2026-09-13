use super::*;
use pbps_model::{Change, ChangeSet, PlannedChange, RowKey, change::DeleteCause};

async fn number(conn: &mut Conn, sql: &str) -> i32 {
    conn.query(sql).await.unwrap()[0]
        .try_get_at(0)
        .unwrap()
        .unwrap()
}

// Inspect effects before rollback, so a mistakenly permitted cascade cannot
// pass because the fixture undoes it before checking the surviving rows.
async fn attempt(conn: &mut Conn, sql: &str) -> (Result<(), String>, [i32; 2]) {
    conn.execute("BEGIN TRANSACTION;").await.unwrap();
    let result = conn
        .execute(sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    let remaining = [
        number(conn, "SELECT COUNT(*) FROM dbo.node").await,
        number(conn, "SELECT COUNT(*) FROM dbo.child").await,
    ];
    conn.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
        .await
        .unwrap();
    (result, remaining)
}

async fn self_reference_case(tag: &str, key: &str) {
    let mut db = TestDb::create(tag).await;
    // The referenced tuple deliberately need not be the plan's key. The
    // latter needs both identifier and nested string-literal escaping.
    db.conn
        .execute(&format!(
            "CREATE TABLE dbo.node (
                 [co'de]]x] nvarchar(20) NOT NULL PRIMARY KEY,
                 grp int NOT NULL, sub int NOT NULL,
                 parent_code nvarchar(20) NULL, parent_grp int NULL, parent_sub int NULL,
                 CONSTRAINT uq_node UNIQUE (grp, sub), {key});
             CREATE TABLE dbo.child (
                 [co'de]]x] nvarchar(20) NOT NULL PRIMARY KEY,
                 parent nvarchar(20) NULL REFERENCES dbo.node([co'de]]x]) ON DELETE CASCADE);
             INSERT INTO dbo.node VALUES
                 (N'keep', 1, 1, NULL, NULL, NULL),
                 (N'lo''op', 2, 1, N'lo''op', 2, 1);"
        ))
        .await
        .unwrap();
    let change = Change::DeleteRow {
        table: TableName::new("dbo", "node"),
        key_column: "co'de]x".into(),
        key: RowKey::from("lo'op"),
        cause: DeleteCause::Undeclared,
        dropped: Default::default(),
        row: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
    };
    let cs = ChangeSet {
        changes: vec![PlannedChange::new(change.clone())],
    };
    let probe = Mssql.preflight(&cs).probes.remove(0);
    let stmt = Mssql.emit(&change, Default::default()).unwrap().remove(0);
    let direct = attempt(
        &mut db.conn,
        "DELETE FROM dbo.node WHERE [co'de]]x]=N'lo''op'",
    )
    .await;
    let self_probe = number(&mut db.conn, &probe.sql).await;
    let self_guard = attempt(&mut db.conn, &stmt.sql).await;

    // A second row on this table survives the attempted delete. It must be
    // counted even though it reaches the same FK as the doomed row itself.
    db.conn
        .execute("INSERT INTO dbo.node VALUES (N'leaf', 2, 2, N'lo''op', 2, 1)")
        .await
        .unwrap();
    let leaf_probe = number(&mut db.conn, &probe.sql).await;
    let leaf_guard = attempt(&mut db.conn, &stmt.sql).await;
    db.conn
        .execute("DELETE FROM dbo.node WHERE [co'de]]x]=N'leaf'")
        .await
        .unwrap();

    // Equal key spelling in another table is a different row. Over-broad
    // exclusion would let this cascade succeed and silently delete the child.
    db.conn
        .execute("INSERT INTO dbo.child VALUES (N'lo''op', N'lo''op')")
        .await
        .unwrap();
    let child_probe = number(&mut db.conn, &probe.sql).await;
    let child_guard = attempt(&mut db.conn, &stmt.sql).await;
    db.drop().await;

    assert!(direct.0.is_ok(), "{direct:?}");
    assert_eq!(direct.1, [1, 0]);
    assert_eq!(self_probe, 0);
    assert!(self_guard.0.is_ok(), "{self_guard:?}");
    assert_eq!(self_guard.1, [1, 0]);
    assert_eq!(leaf_probe, 1);
    assert_eq!(child_probe, 1);
    for (guard, remaining) in [(leaf_guard, [3, 0]), (child_guard, [2, 1])] {
        let refusal = guard.0.expect_err("another referencing row must refuse");
        assert!(
            refusal.contains("is referenced by row(s)"),
            "the pbps guard must refuse before the engine constraint: {refusal}"
        );
        assert_eq!(guard.1, remaining);
    }
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_row_that_references_only_itself_can_be_deleted() {
    self_reference_case(
        "self_reference213",
        "CONSTRAINT fk_self FOREIGN KEY (parent_code) REFERENCES dbo.node([co'de]]x])",
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_self_reference_to_another_unique_key_excludes_only_the_deleted_row() {
    self_reference_case(
        "self_unique213",
        "CONSTRAINT fk_self FOREIGN KEY (parent_grp, parent_sub) REFERENCES dbo.node(grp, sub)",
    )
    .await;
}
