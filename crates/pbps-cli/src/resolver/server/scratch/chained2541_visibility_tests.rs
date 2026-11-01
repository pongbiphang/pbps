//! DENY coverage and real absence, with the same non-owner on both routes.
use super::*;

async fn survey(
    reader: &mut pbps_db::Conn,
    schema: &Schema,
) -> Result<capture::ExternalInputs, capture::CaptureError> {
    capture::required_externals(reader, &[schema], &capture_key()).await
}

async fn fixture(target: &str) -> (pbps_db::Conn, pbps_db::Conn, Schema, String) {
    let mut owner = connection(target).await;
    for name in ["app", "plan2541", "tenant_other", "quoted'2541"] {
        owner
            .execute(&format!("CREATE SCHEMA [{name}] AUTHORIZATION dbo;"))
            .await
            .unwrap();
    }
    owner.execute("CREATE TABLE dbo.t(i int);").await.unwrap();
    for name in ["dbo", "app", "plan2541", "tenant_other", "quoted'2541"] {
        owner
            .execute(&format!("CREATE VIEW [{name}].v AS SELECT i FROM dbo.t;"))
            .await
            .unwrap();
    }
    let mut plan = declared(
        ModuleKind::View,
        "Plan2541",
        "SELECT CAST(NULL AS int) AS i",
    );
    install(target, &plan.desired).await;
    plan.desired.modules.values_mut().next().unwrap().definition = "SELECT i FROM v".into();
    let login =
        reconstruct2374::deployer(target, target, "deny2541", "WITH DEFAULT_SCHEMA=app").await;
    owner.execute(&format!("CREATE ROLE deny_role2541; CREATE ROLE deny_parent2541; ALTER ROLE deny_role2541 ADD MEMBER [{login}]; ALTER ROLE deny_parent2541 ADD MEMBER deny_role2541; CREATE USER other2541 WITHOUT LOGIN;")).await.unwrap();
    let reader = connection(&format!("{target};User Id={login};Password=Pbps!Test12345")).await;
    (owner, reader, plan.desired, login)
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_deny_inventory_preserves_absence_and_off_path_schemas() {
    fn case<'a>(target: &'a str, _: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (mut owner, mut reader, desired, login) = fixture(target).await;
            let baseline = survey(&mut reader, &desired).await.unwrap();
            assert!(baseline.roots().contains(&capture::object("plan2541", "v")));
            let mut absent = desired.clone();
            absent.modules.values_mut().next().unwrap().definition =
                "SELECT i FROM absent2541".into();
            let missing = survey(&mut reader, &absent).await.unwrap();
            assert!(
                !missing
                    .roots()
                    .contains(&capture::object("plan2541", "absent2541"))
            );

            // Every subject is in sys.user_token, including a nested role.
            for grantee in [&login, "deny_role2541", "deny_parent2541", "public"] {
                let cases = [
                    (
                        "VIEW DEFINITION",
                        " ON OBJECT::dbo.v",
                        true,
                        "hidden major_id=",
                    ),
                    (
                        "CONTROL",
                        " ON OBJECT::tenant_other.v",
                        true,
                        "hidden major_id=",
                    ),
                    ("VIEW DEFINITION", " ON SCHEMA::tenant_other", false, ""),
                    ("CONTROL", " ON SCHEMA::tenant_other", false, ""),
                    ("VIEW DEFINITION", " ON SCHEMA::app", true, "app"),
                    ("VIEW DEFINITION", " ON SCHEMA::dbo", true, "dbo"),
                    ("VIEW DEFINITION", " ON SCHEMA::plan2541", true, "plan2541"),
                    ("CONTROL", " ON SCHEMA::plan2541", true, "plan2541"),
                    ("VIEW PERFORMANCE DEFINITION", "", true, "database"),
                ];
                for (permission, scope, refuse, securable) in cases {
                    owner
                        .execute(&format!("DENY {permission}{scope} TO [{grantee}];"))
                        .await
                        .unwrap();
                    let result = survey(&mut reader, &desired).await;
                    owner
                        .execute(&format!("REVOKE {permission}{scope} FROM [{grantee}];"))
                        .await
                        .unwrap();
                    if refuse {
                        let error = match result {
                            Ok(_) => {
                                panic!("DENY {permission}{scope} to {grantee} was read as absence")
                            }
                            Err(error) => error.to_string(),
                        };
                        assert!(
                            error.contains(grantee)
                                && error.contains(permission)
                                && error.contains(securable)
                                && error.contains("pbps principal only")
                                && error.contains("separate catalog-reader account"),
                            "{error}"
                        );
                    } else {
                        let result = result.unwrap();
                        assert!(result.roots().contains(&capture::object("plan2541", "v")));
                        assert!(baseline.captured().compare(result.captured()).is_empty());
                    }
                    assert!(
                        survey(&mut reader, &desired)
                            .await
                            .unwrap()
                            .roots()
                            .contains(&capture::object("plan2541", "v"))
                    );
                }
            }
            // Visible DENYs for someone outside the token do not affect us.
            owner
                .execute("DENY VIEW DEFINITION ON OBJECT::dbo.v TO other2541;")
                .await
                .unwrap();
            survey(&mut reader, &desired).await.unwrap();
            owner
                .execute("REVOKE VIEW DEFINITION ON OBJECT::dbo.v FROM other2541;")
                .await
                .unwrap();
            owner
                .execute(&format!(
                    "DENY SELECT ON OBJECT::sys.database_permissions TO [{login}];"
                ))
                .await
                .unwrap();
            let unreadable = survey(&mut reader, &desired).await;
            owner
                .execute(&format!(
                    "REVOKE SELECT ON OBJECT::sys.database_permissions FROM [{login}];"
                ))
                .await
                .unwrap();
            let error = match unreadable {
                Ok(_) => panic!("an unreadable DENY catalog became empty"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("sys.database_permissions") && error.contains("could not read"),
                "{error}"
            );

            // A quoted plan schema joins the path without changing SQL syntax.
            let quoted = declared(ModuleKind::View, "quoted'2541", "SELECT i FROM v").desired;
            owner
                .execute(&format!(
                    "DENY VIEW DEFINITION ON SCHEMA::[quoted'2541] TO [{login}];"
                ))
                .await
                .unwrap();
            let quoted_result = survey(&mut reader, &quoted).await;
            owner
                .execute(&format!(
                    "REVOKE VIEW DEFINITION ON SCHEMA::[quoted'2541] FROM [{login}];"
                ))
                .await
                .unwrap();
            let error = match quoted_result {
                Ok(_) => panic!("a quoted on-path schema was ignored"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("quoted'2541") && error.contains(&login),
                "{error}"
            );
            survey(&mut reader, &desired).await.unwrap();
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_security_definition_qualifies_the_deny_inventory() {
    fn case<'a>(target: &'a str, _: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (mut owner, mut reader, desired, login) = fixture(target).await;
            survey(&mut reader, &desired).await.unwrap();
            // Direct security DENY hides both its own row and this object DENY.
            owner.execute(&format!("DENY VIEW SECURITY DEFINITION TO [{login}]; DENY VIEW DEFINITION ON OBJECT::dbo.v TO [{login}];")).await.unwrap();
            let blind = survey(&mut reader, &desired).await;
            owner.execute(&format!("REVOKE VIEW SECURITY DEFINITION FROM [{login}]; REVOKE VIEW DEFINITION ON OBJECT::dbo.v FROM [{login}];")).await.unwrap();
            let error = match blind {
                Ok(_) => panic!("an unreadable DENY list proved absence"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("VIEW SECURITY DEFINITION")
                    && error.contains(&login)
                    && error.contains("pbps principal only")
                    && error.contains("separate catalog-reader account"),
                "{error}"
            );
            survey(&mut reader, &desired).await.unwrap();
        })
    }
    reconstruct2374::both(case).await;
}
