//! The persisted SQL Server after-state comes from the reproduced engine.
use super::*;
use pbps_mssql::resolver::{authorization as auth, capture};
use std::collections::BTreeSet;

async fn context(
    conn: &mut Conn,
    seed: Option<&auth::AuthorizationContext>,
) -> auth::AuthorizationContext {
    let mut got = match seed {
        Some(seed) => auth::reread(conn, seed).await.unwrap(),
        None => auth::read(conn, &["app".into()]).await.unwrap(),
    };
    auth::read_planned_principals(conn, &mut got, &["readers".into()])
        .await
        .unwrap();
    let captured = capture::capture(
        conn,
        &capture::CaptureScope {
            retained: BTreeSet::from([capture::object("app", "t")]),
            candidates: BTreeSet::new(),
        },
        &capture_key(),
    )
    .await
    .unwrap();
    let owners = auth::OwnerPartition::from_capture(&captured, &got).unwrap();
    auth::read_staged_permissions(conn, &mut got, &owners)
        .await
        .unwrap();
    got
}

async fn measured(
    tag: &str,
    fixture: &str,
    revoke: bool,
    expected_grantor: Option<&str>,
    rejected: bool,
    unmapped: bool,
) {
    let mut target = TestDb::create(&format!("auth2143_{tag}_t")).await;
    let mut scratch = TestDb::create(&format!("auth2143_{tag}_s")).await;
    target.conn.execute("CREATE USER deployer WITHOUT LOGIN WITH DEFAULT_SCHEMA=app; CREATE USER app_owner WITHOUT LOGIN; CREATE ROLE readers;").await.unwrap();
    target
        .conn
        .execute("CREATE SCHEMA app AUTHORIZATION app_owner;")
        .await
        .unwrap();
    target.conn.execute("CREATE TABLE app.t(id int); GRANT VIEW DEFINITION,VIEW SECURITY DEFINITION TO deployer; GRANT SELECT ON sys.sql_expression_dependencies TO deployer;").await.unwrap();
    target.conn.execute(fixture).await.unwrap();
    target
        .conn
        .execute("EXECUTE AS USER=N'deployer';")
        .await
        .unwrap();
    let before = context(&mut target.conn, None).await;
    let planned = vec![auth::PlannedGrant {
        principal: "readers".into(),
        schema: "app".into(),
        permission: "SELECT".into(),
        revoke,
    }];
    let map = auth::PrincipalMap::generate(&before, &planned, &format!("a2143_{tag}"));
    auth::reconstruct_database(&mut scratch.conn, &map, &before)
        .await
        .unwrap();
    scratch
        .conn
        .execute("CREATE TABLE app.t(id int);")
        .await
        .unwrap();
    // SQL Server's owner chooses the grantor for CONTROL. Reproduce the
    // owner's identity before the verified deployer executes the plan.
    let captured = capture::capture(
        &mut target.conn,
        &capture::CaptureScope {
            retained: BTreeSet::from([capture::object("app", "t")]),
            candidates: BTreeSet::new(),
        },
        &capture_key(),
    )
    .await
    .unwrap();
    let target_owners = auth::OwnerPartition::from_capture(&captured, &before).unwrap();
    auth::reconstruct_owners(&mut scratch.conn, &map, &target_owners)
        .await
        .unwrap();
    auth::enter(&mut scratch.conn, map.deployer(&before).as_ref())
        .await
        .unwrap();
    assert!(
        auth::verify(&mut scratch.conn, &map, &before, &["app".into()])
            .await
            .unwrap()
            .is_empty()
    );
    if unmapped {
        let mut admin = connect_live(&format!("{};Database={}", conn_str(), scratch.name))
            .await
            .unwrap();
        admin
            .execute(
                "CREATE USER alien2143 WITHOUT LOGIN; GRANT SELECT ON SCHEMA::app TO alien2143;",
            )
            .await
            .unwrap();
    }
    let applied = auth::apply_planned(&mut scratch.conn, &map, &planned).await;
    let statement = if revoke {
        "REVOKE SELECT ON SCHEMA::app FROM readers;"
    } else {
        "GRANT SELECT ON SCHEMA::app TO readers;"
    };
    if rejected {
        let error = applied.expect_err("a grant-option revoke needs CASCADE");
        assert!(error.to_string().contains("4611"), "{error}");
        let target_error = target.conn.execute(statement).await.unwrap_err();
        assert!(target_error.to_string().contains("4611"), "{target_error}");
        assert_eq!(
            context(&mut target.conn, Some(&before)).await.canonical(),
            before.canonical()
        );
    } else {
        applied.unwrap();
        let condition =
            auth::capture_condition(&mut target.conn, &mut scratch.conn, &map, &before).await;
        if unmapped {
            let error = condition.unwrap_err().to_string();
            assert!(
                error.contains("alien2143") && error.contains("cannot be mapped"),
                "{error}"
            );
        } else {
            let condition = condition.unwrap();
            let key = capture_key();
            let digest = |context: &auth::AuthorizationContext| {
                key.fingerprint(
                    "pbps/authorization-context/v1",
                    "authorization",
                    &context.canonical(),
                )
            };
            assert_eq!(digest(condition.before()), digest(&before));
            assert!(condition.recheck(&mut target.conn).await.unwrap());
            target.conn.execute(statement).await.unwrap();
            let after = context(&mut target.conn, Some(&before)).await;
            assert_eq!(
                digest(condition.after()),
                digest(&after),
                "{tag}: scratch after-state must equal the real target"
            );
            assert_eq!(condition.after(), &after);
            assert!(
                !String::from_utf8(condition.after().canonical())
                    .unwrap()
                    .contains("pbps_principal_")
            );
            if let Some(grantor) = expected_grantor {
                assert!(
                    after.schemas["app"]
                        .grants
                        .iter()
                        .any(|g| g.grantee.raw() == "readers"
                            && g.permission == "SELECT"
                            && g.grantor.raw() == grantor)
                );
            }
            if tag == "plain_wgo" {
                assert_eq!(before, after);
            }
            if tag == "deny" {
                assert!(!before.schemas["app"].permissions.contains("SELECT"));
                assert!(after.schemas["app"].permissions.contains("SELECT"));
                let id = capture::object("app", "t");
                assert!(
                    !before.staged[&id]
                        .permissions
                        .iter()
                        .any(|(_, p)| p == "SELECT")
                );
                assert!(
                    after.staged[&id]
                        .permissions
                        .iter()
                        .any(|(_, p)| p == "SELECT")
                );
            }
        }
    }
    target.conn.execute("REVERT;").await.unwrap();
    scratch.conn.execute("REVERT;").await.unwrap();
    target.drop().await;
    scratch.drop().await;
}

const WGO: &str = "GRANT SELECT ON SCHEMA::app TO deployer WITH GRANT OPTION; EXECUTE AS USER=N'deployer'; GRANT SELECT ON SCHEMA::app TO readers WITH GRANT OPTION; REVERT;";
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn schema_control_records_the_schema_owner_as_grantor() {
    measured(
        "control",
        "GRANT CONTROL ON SCHEMA::app TO deployer;",
        false,
        Some("app_owner"),
        false,
        false,
    )
    .await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn grant_option_records_the_deployer_as_grantor() {
    measured(
        "wgo",
        "GRANT SELECT ON SCHEMA::app TO deployer WITH GRANT OPTION;",
        false,
        Some("deployer"),
        false,
        false,
    )
    .await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_plain_grant_preserves_an_existing_grant_option() {
    measured("plain_wgo", WGO, false, Some("deployer"), false, false).await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn revoking_grant_option_without_cascade_refuses_before_a_condition_exists() {
    measured("revoke_wgo", WGO, true, None, true, false).await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn revoking_a_role_deny_restores_engine_reported_schema_and_column_permissions() {
    measured("deny","GRANT CONTROL ON SCHEMA::app TO deployer; ALTER ROLE readers ADD MEMBER deployer; EXECUTE AS USER=N'deployer'; DENY SELECT ON SCHEMA::app TO readers; REVERT;",true,None,false,false).await;
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_unmapped_scratch_principal_is_named_and_never_sealed() {
    measured(
        "unmapped",
        "GRANT CONTROL ON SCHEMA::app TO deployer;",
        false,
        None,
        false,
        true,
    )
    .await;
}
