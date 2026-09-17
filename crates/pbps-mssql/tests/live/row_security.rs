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

/// `DENY VIEW SECURITY DEFINITION` is not what decides whether a policy can be
/// seen, and a count that refused on it alone would refuse safe deletes.
///
/// A review read the permission's name and concluded that an effective denial
/// of it could leave database `VIEW DEFINITION` granted while hiding a policy,
/// so that the shared delete count would accept a filtered zero. The premise is
/// measured here instead of reasoned about, in the two shapes a real deployment
/// account collects such a denial in — directly, and through a role — because
/// only the engine can settle it (issue #524, DECISIONS 505).
///
/// What the engine says: the permission exists at **DATABASE** scope alone and
/// is covered by `VIEW DEFINITION`, denying it leaves both the foreign key and
/// the enabled FILTER predicate visible, the count refuses the active policy in
/// every case, and with the policy disabled and the child detached every case
/// counts a real zero and the guarded delete runs. Refusing on the denial alone
/// would reject those last three — a valid plan refused, which is the one thing
/// this count may not do.
///
/// Denying the permission on the policy object or its schema is not a narrower
/// way to reproduce the premise either: there is no such scope, and the attempt
/// is a syntax error. That is asserted rather than described, so that a future
/// engine which grows the scope fails this test rather than passing it silently.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn denying_the_security_definition_permission_hides_no_policy_and_refuses_no_safe_delete() {
    let mut db = TestDb::create("rls_secdef524").await;
    fixture(&mut db.conn, "CASCADE").await;
    db.conn
        .execute(
            "CREATE USER deployer WITHOUT LOGIN; CREATE ROLE definition_deniers;
             ALTER ROLE definition_deniers ADD MEMBER deployer;
             GRANT SELECT, DELETE ON dbo.parent TO deployer;
             GRANT SELECT ON dbo.child TO deployer;
             GRANT VIEW DEFINITION TO deployer;",
        )
        .await
        .unwrap();

    // The permission's scopes, from the engine's own list rather than from the
    // documentation: one row, at the database.
    let scopes: Vec<String> = db
        .conn
        .query(
            "SELECT class_desc FROM sys.fn_builtin_permissions(DEFAULT) \
             WHERE permission_name = N'VIEW SECURITY DEFINITION' ORDER BY class_desc;",
        )
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.try_get::<&str>("class_desc")
                .unwrap()
                .unwrap()
                .trim()
                .to_owned()
        })
        .collect();

    // And therefore what happens when it is denied anywhere narrower. 102 is
    // the parser's, which is the answer this asserts: no such securable.
    let mut narrower = Vec::new();
    for on in ["OBJECT::security_rules.children", "SCHEMA::security_rules"] {
        narrower.push((
            on,
            number(
                &mut db.conn,
                &format!(
                    "BEGIN TRY EXEC(N'DENY VIEW SECURITY DEFINITION ON {on} TO deployer'); \
                     SELECT 0; END TRY BEGIN CATCH SELECT ERROR_NUMBER(); END CATCH"
                ),
            )
            .await,
        ));
    }

    let sql = Mssql
        .emit(&deletion(), Default::default())
        .unwrap()
        .remove(0)
        .sql;
    let mut observations = Vec::new();
    for (state, change) in [
        ("baseline", "SELECT 1;"),
        (
            "direct database denial",
            "DENY VIEW SECURITY DEFINITION TO deployer;",
        ),
        (
            "role-inherited database denial",
            "REVOKE VIEW SECURITY DEFINITION TO deployer; \
             DENY VIEW SECURITY DEFINITION TO definition_deniers;",
        ),
    ] {
        db.conn.execute(change).await.unwrap();
        db.conn
            .execute("EXECUTE AS USER='deployer';")
            .await
            .unwrap();
        let held = [
            number(
                &mut db.conn,
                "SELECT COALESCE(HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DEFINITION'), 0)",
            )
            .await,
            number(
                &mut db.conn,
                "SELECT COALESCE(HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', \
                 'VIEW SECURITY DEFINITION'), 0)",
            )
            .await,
        ];
        let visible = [
            number(
                &mut db.conn,
                "SELECT COUNT(*) FROM sys.foreign_keys \
                 WHERE referenced_object_id = OBJECT_ID(N'dbo.parent')",
            )
            .await,
            number(
                &mut db.conn,
                "SELECT COUNT(*) FROM sys.security_predicates sp \
                 JOIN sys.security_policies pol ON pol.object_id = sp.object_id \
                 WHERE sp.target_object_id = OBJECT_ID(N'dbo.child') \
                   AND sp.predicate_type = 0 AND pol.is_enabled = 1",
            )
            .await,
        ];
        let counted = probe(&mut db.conn, &plan()).await;
        let guarded = attempt(&mut db.conn, &sql).await;
        db.conn.execute("REVERT;").await.unwrap();
        observations.push((state, held, visible, counted, guarded));
    }

    // The safe half, under the same three permission states: nothing filters,
    // nothing references, and the delete the operator asked for must run.
    db.conn
        .execute(
            "ALTER SECURITY POLICY security_rules.children WITH (STATE=OFF);
             UPDATE dbo.child SET parent = 2;",
        )
        .await
        .unwrap();
    let mut safe = Vec::new();
    for (state, change) in [
        (
            "baseline",
            "REVOKE VIEW SECURITY DEFINITION TO definition_deniers;",
        ),
        (
            "direct database denial",
            "DENY VIEW SECURITY DEFINITION TO deployer;",
        ),
        (
            "role-inherited database denial",
            "REVOKE VIEW SECURITY DEFINITION TO deployer; \
             DENY VIEW SECURITY DEFINITION TO definition_deniers;",
        ),
    ] {
        db.conn.execute(change).await.unwrap();
        db.conn
            .execute("EXECUTE AS USER='deployer';")
            .await
            .unwrap();
        let counted = probe(&mut db.conn, &plan()).await;
        let guarded = attempt(&mut db.conn, &sql).await;
        db.conn.execute("REVERT;").await.unwrap();
        safe.push((state, counted, guarded));
    }
    db.drop().await;

    assert_eq!(scopes, ["DATABASE"], "the permission's scopes");
    for (on, error) in narrower {
        assert_eq!(error, 102, "denying it on {on} must be a syntax error");
    }
    for (state, held, visible, counted, guarded) in observations {
        assert_eq!(held[0], 1, "{state}: database VIEW DEFINITION is held");
        assert_eq!(
            held[1],
            i32::from(state == "baseline"),
            "{state}: the denial is in force"
        );
        assert_eq!(
            visible,
            [1, 1],
            "{state}: the key and the enabled predicate are both still visible"
        );
        assert!(
            counted
                .as_ref()
                .is_err_and(|e| e.contains("row-level security")),
            "{state}: {counted:?}"
        );
        assert!(
            guarded
                .0
                .as_ref()
                .is_err_and(|e| e.contains("row-level security")),
            "{state}: {guarded:?}"
        );
        assert_eq!(guarded.1, [1, 1, 0], "{state}: the child is unchanged");
    }
    for (state, counted, guarded) in safe {
        assert_eq!(counted, Ok(0), "{state}: nothing references the parent row");
        assert!(
            guarded.0.is_ok(),
            "{state}: a safe delete was refused: {guarded:?}"
        );
        assert_eq!(guarded.1, [0, 1, 0], "{state}: the parent row is gone");
    }
}
