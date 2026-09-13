use super::*;
use pbps_model::{Change, ChangeSet, PlannedChange, RowKey, change::DeleteCause};

fn deletion() -> Change {
    Change::DeleteRow {
        table: TableName::new("dbo", "parent"),
        key_column: "code".into(),
        key: RowKey::from("1"),
        cause: DeleteCause::Undeclared,
        dropped: Default::default(),
        row: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
    }
}

fn plan() -> ChangeSet {
    ChangeSet {
        changes: vec![PlannedChange::new(deletion())],
    }
}

async fn number(conn: &mut Conn, sql: &str) -> i32 {
    conn.query(sql).await.unwrap()[0]
        .try_get_at(0)
        .unwrap()
        .unwrap()
}

async fn probe(conn: &mut Conn, cs: &ChangeSet) -> Result<i32, String> {
    let sql = &Mssql.preflight(cs).probes.remove(0).sql;
    let rows = conn.query(sql).await.map_err(|e| e.to_string())?;
    Ok(rows[0].try_get_at(0).unwrap().unwrap())
}

async fn visible(conn: &mut Conn, owner: i32) {
    conn.execute(&format!(
        "EXEC sys.sp_set_session_context @key=N'visible_owner', @value={owner};"
    ))
    .await
    .unwrap();
}

/// Read the effects before rollback, so a broken guard cannot pass by having
/// its unintended referential action undone by the fixture itself.
async fn attempt(conn: &mut Conn, sql: &str) -> (Result<(), String>, [i32; 3]) {
    visible(conn, 0).await;
    conn.execute("BEGIN TRANSACTION;").await.unwrap();
    let result = conn
        .execute(sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    visible(conn, 1).await;
    let state = [
        number(conn, "SELECT COUNT(*) FROM dbo.parent WHERE code=1").await,
        number(conn, "SELECT COUNT(*) FROM dbo.child").await,
        number(conn, "SELECT COUNT(*) FROM dbo.child WHERE parent IS NULL").await,
    ];
    conn.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
        .await
        .unwrap();
    visible(conn, 0).await;
    (result, state)
}

async fn fixture(conn: &mut Conn, action: &str) {
    conn.execute("CREATE SCHEMA security_rules;").await.unwrap();
    conn.execute(
        "CREATE FUNCTION security_rules.only_owner(@owner_id int)
         RETURNS TABLE WITH SCHEMABINDING AS RETURN
         SELECT 1 AS allowed WHERE @owner_id = CONVERT(int, SESSION_CONTEXT(N'visible_owner'));",
    )
    .await
    .unwrap();
    conn.execute(&format!(
        "CREATE TABLE dbo.parent (code int PRIMARY KEY);
         CREATE TABLE dbo.child (id int PRIMARY KEY, parent int NULL,
             owner_id int NOT NULL, CONSTRAINT fk_child FOREIGN KEY(parent)
             REFERENCES dbo.parent(code) ON DELETE {action});
         INSERT dbo.parent VALUES (1), (2); INSERT dbo.child VALUES (1,1,1);"
    ))
    .await
    .unwrap();
    conn.execute(
        "CREATE SECURITY POLICY security_rules.children
         ADD FILTER PREDICATE security_rules.only_owner(owner_id) ON dbo.child WITH (STATE=ON);",
    )
    .await
    .unwrap();
    visible(conn, 0).await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn filtered_children_refuse_preflight_and_delete_for_both_referential_actions() {
    let mut observations = Vec::new();
    for (tag, action) in [("rls_cascade208", "CASCADE"), ("rls_null208", "SET NULL")] {
        let mut db = TestDb::create(tag).await;
        fixture(&mut db.conn, action).await;
        let sql = Mssql
            .emit(&deletion(), Default::default())
            .unwrap()
            .remove(0)
            .sql;
        let hidden = number(&mut db.conn, "SELECT COUNT(*) FROM dbo.child").await;
        let direct = attempt(&mut db.conn, "DELETE FROM dbo.parent WHERE code=1;").await;
        let filtered_probe = probe(&mut db.conn, &plan()).await;
        let filtered_guard = attempt(&mut db.conn, &sql).await;

        // Unlike PostgreSQL, this dbo session has no intrinsic RLS bypass.
        // Disabled and block-only policies, however, do not filter SELECT.
        let mut unfiltered = Vec::new();
        for (kind, alter) in [
            (
                "disabled",
                "ALTER SECURITY POLICY security_rules.children WITH (STATE=OFF);",
            ),
            (
                "block_only",
                "ALTER SECURITY POLICY security_rules.children DROP FILTER PREDICATE ON dbo.child; ALTER SECURITY POLICY security_rules.children ADD BLOCK PREDICATE security_rules.only_owner(owner_id) ON dbo.child AFTER INSERT; ALTER SECURITY POLICY security_rules.children WITH (STATE=ON);",
            ),
        ] {
            db.conn.execute(alter).await.unwrap();
            let occupied = probe(&mut db.conn, &plan()).await;
            let occupied_guard = attempt(&mut db.conn, &sql).await;
            db.conn
                .execute("UPDATE dbo.child SET parent=2;")
                .await
                .unwrap();
            let empty = probe(&mut db.conn, &plan()).await;
            let empty_guard = attempt(&mut db.conn, &sql).await;
            db.conn
                .execute("UPDATE dbo.child SET parent=1;")
                .await
                .unwrap();
            unfiltered.push((kind, occupied, occupied_guard, empty, empty_guard));
        }
        db.drop().await;
        observations.push((
            action,
            hidden,
            direct,
            filtered_probe,
            filtered_guard,
            unfiltered,
        ));
    }

    for (action, hidden, direct, filtered_probe, filtered_guard, unfiltered) in observations {
        assert_eq!(hidden, 0, "the fixture filters dbo too: {action}");
        assert!(direct.0.is_ok(), "{action}: {direct:?}");
        assert_eq!(
            direct.1,
            if action == "CASCADE" {
                [0, 0, 0]
            } else {
                [0, 1, 1]
            }
        );
        assert!(
            filtered_probe
                .as_ref()
                .is_err_and(|e| e.contains("row-level security")),
            "{action}: {filtered_probe:?}"
        );
        assert!(
            filtered_guard
                .0
                .as_ref()
                .is_err_and(|e| e.contains("row-level security")),
            "{action}: {filtered_guard:?}"
        );
        assert_eq!(
            filtered_guard.1,
            [1, 1, 0],
            "the hidden child is unchanged: {action}"
        );
        for (kind, occupied, occupied_guard, empty, empty_guard) in unfiltered {
            assert_eq!(occupied, Ok(1), "{action}/{kind}");
            assert!(
                occupied_guard
                    .0
                    .as_ref()
                    .is_err_and(|e| e.contains("is referenced")),
                "{action}/{kind}: {occupied_guard:?}"
            );
            assert_eq!(occupied_guard.1, [1, 1, 0]);
            assert_eq!(empty, Ok(0), "{action}/{kind}");
            assert!(empty_guard.0.is_ok(), "{action}/{kind}: {empty_guard:?}");
            assert_eq!(empty_guard.1, [0, 1, 0]);
        }
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn hidden_security_policy_metadata_is_not_treated_as_no_policy() {
    let mut db = TestDb::create("rls_metadata208").await;
    fixture(&mut db.conn, "CASCADE").await;
    db.conn
        .execute(
            "CREATE USER deployer WITHOUT LOGIN; CREATE ROLE policy_readers;
         ALTER ROLE policy_readers ADD MEMBER deployer;
         GRANT SELECT, UPDATE, DELETE ON dbo.parent TO deployer;
         GRANT SELECT, UPDATE, DELETE, ALTER ON dbo.child TO deployer;
         GRANT VIEW DEFINITION ON SCHEMA::dbo TO deployer;",
        )
        .await
        .unwrap();
    let sql = Mssql
        .emit(&deletion(), Default::default())
        .unwrap()
        .remove(0)
        .sql;
    let mut observations = Vec::new();
    for (tag, grants, expected_metadata, message) in [
        ("child_schema_only", "SELECT 1;", 0, "VIEW DEFINITION"),
        (
            "database_definition",
            "GRANT VIEW DEFINITION TO deployer;",
            1,
            "row-level security",
        ),
        (
            "denied_policy",
            "DENY VIEW DEFINITION ON OBJECT::security_rules.children TO deployer;",
            0,
            "metadata DENY",
        ),
        (
            "denied_policy_schema_via_role",
            "REVOKE VIEW DEFINITION ON OBJECT::security_rules.children FROM deployer; DENY VIEW DEFINITION ON SCHEMA::security_rules TO policy_readers;",
            0,
            "metadata DENY",
        ),
    ] {
        db.conn.execute(grants).await.unwrap();
        db.conn
            .execute("EXECUTE AS USER='deployer';")
            .await
            .unwrap();
        let policies = number(&mut db.conn, "SELECT COUNT(*) FROM sys.security_predicates WHERE target_object_id=OBJECT_ID(N'dbo.child')").await;
        let hidden = number(&mut db.conn, "SELECT COUNT(*) FROM dbo.child").await;
        let counted = probe(&mut db.conn, &plan()).await;
        let guarded = attempt(&mut db.conn, &sql).await;
        db.conn.execute("REVERT;").await.unwrap();
        observations.push((
            tag,
            expected_metadata,
            message,
            policies,
            hidden,
            counted,
            guarded,
        ));
    }

    // Once the catalog is readable, a disabled or planned-away FK cannot
    // cascade and supplies no reason to reject an active filter.
    db.conn
        .execute(
            "REVOKE VIEW DEFINITION ON SCHEMA::security_rules FROM policy_readers;
         ALTER TABLE dbo.child NOCHECK CONSTRAINT fk_child;
         EXECUTE AS USER='deployer';",
        )
        .await
        .unwrap();
    let disabled_probe = probe(&mut db.conn, &plan()).await;
    let disabled_guard = attempt(&mut db.conn, &sql).await;
    db.conn
        .execute("REVERT; ALTER TABLE dbo.child WITH CHECK CHECK CONSTRAINT fk_child;")
        .await
        .unwrap();
    let mut removed = plan();
    removed.changes.insert(
        0,
        PlannedChange::new(Change::DropForeignKey {
            table: TableName::new("dbo", "child"),
            name: "fk_child".into(),
        }),
    );
    db.conn
        .execute("EXECUTE AS USER='deployer';")
        .await
        .unwrap();
    let removed_probe = probe(&mut db.conn, &removed).await;
    let removed_sql = removed
        .changes
        .iter()
        .flat_map(|p| Mssql.emit(&p.change, p.strategy).unwrap())
        .map(|s| s.sql)
        .collect::<Vec<_>>()
        .join("\n");
    let removed_guard = attempt(&mut db.conn, &removed_sql).await;
    db.conn.execute("REVERT;").await.unwrap();
    db.drop().await;

    for (tag, expected_metadata, message, policies, hidden, counted, guarded) in observations {
        assert_eq!(policies, expected_metadata, "{tag}");
        assert_eq!(hidden, 0, "{tag}");
        assert!(
            counted.as_ref().is_err_and(|e| e.contains(message)),
            "{tag}: {counted:?}"
        );
        assert!(
            guarded.0.as_ref().is_err_and(|e| e.contains(message)),
            "{tag}: {guarded:?}"
        );
        assert_eq!(guarded.1, [1, 1, 0], "{tag}: the hidden child stays");
    }
    assert_eq!(disabled_probe, Ok(0));
    assert!(disabled_guard.0.is_ok(), "{disabled_guard:?}");
    assert_eq!(disabled_guard.1, [0, 1, 0]);
    assert_eq!(removed_probe, Ok(0));
    assert!(removed_guard.0.is_ok(), "{removed_guard:?}");
    assert_eq!(removed_guard.1, [0, 1, 0]);
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_invisible_referencing_key_does_not_hide_its_row_filter_from_the_guard() {
    let mut db = TestDb::create("rls_hidden_key208").await;
    fixture(&mut db.conn, "CASCADE").await;
    db.conn
        .execute(
            "CREATE USER parent_only WITHOUT LOGIN;
         GRANT SELECT, DELETE ON dbo.parent TO parent_only;
         EXECUTE AS USER='parent_only';",
        )
        .await
        .unwrap();
    let keys = number(
        &mut db.conn,
        "SELECT COUNT(*) FROM sys.foreign_keys WHERE referenced_object_id=OBJECT_ID(N'dbo.parent')",
    )
    .await;
    let policies = number(&mut db.conn, "SELECT COUNT(*) FROM sys.security_predicates").await;
    let counted = probe(&mut db.conn, &plan()).await;
    let sql = Mssql
        .emit(&deletion(), Default::default())
        .unwrap()
        .remove(0)
        .sql;
    let guarded = db
        .conn
        .execute(&sql)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    db.conn.execute("REVERT;").await.unwrap();
    visible(&mut db.conn, 1).await;
    let parents = number(&mut db.conn, "SELECT COUNT(*) FROM dbo.parent WHERE code=1").await;
    let children = number(
        &mut db.conn,
        "SELECT COUNT(*) FROM dbo.child WHERE parent=1",
    )
    .await;
    db.drop().await;

    assert_eq!(
        keys, 0,
        "this session cannot discover the referencing table"
    );
    assert_eq!(policies, 0, "the hidden table's filter is invisible too");
    assert!(
        counted
            .as_ref()
            .is_err_and(|e| e.contains("VIEW DEFINITION")),
        "{counted:?}"
    );
    assert!(
        guarded
            .as_ref()
            .is_err_and(|e| e.contains("VIEW DEFINITION")),
        "{guarded:?}"
    );
    assert_eq!(
        (parents, children),
        (1, 1),
        "neither visible parent nor hidden child was removed"
    );
}
