use super::*;
use pbps_model::{ModuleId, ObjectName};
use pbps_mssql::doctor::{self, GrantTargets, Held, Securable};

fn targets() -> GrantTargets {
    GrantTargets {
        objects: vec![ObjectName::new("app", "t"), ObjectName::new("app", "v")],
        roles: vec!["reader".into()],
        managed_modules: [ModuleId::Named(ObjectName::new("app", "v"))]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

async fn holdings(conn: &mut Conn, targets: &GrantTargets, tables: &[ObjectName]) -> Held {
    doctor::permissions(
        conn,
        tables,
        &["app".into()],
        &Default::default(),
        targets,
        &Default::default(),
        &Default::default(),
        &IdsFile::default(),
    )
    .await
    .unwrap()
}

fn control_gaps(held: &Held) -> Vec<Securable> {
    doctor::missing(held)
        .into_iter()
        .filter(|g| g.permission == "CONTROL")
        .map(|g| g.securable)
        .collect()
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn future_grants_match_real_creation_authority_for_admin_and_narrow_logins() {
    for authority in ["sa", "owner", "control", "narrow"] {
        let mut db = TestDb::create(&format!("future_grant_{authority}")).await;
        db.conn.execute("CREATE USER schema_owner WITHOUT LOGIN; EXEC('CREATE SCHEMA app AUTHORIZATION schema_owner'); CREATE ROLE reader;").await.unwrap();
        let login = format!("pbps_future_{authority}_{}", std::process::id());
        let password = "pbpsFutureGrant!1806";
        let mut connection = if authority == "sa" {
            connect_live(&format!(
                "{};Database={}",
                conn_str().trim_end_matches(';'),
                db.name
            ))
            .await
            .unwrap()
        } else {
            db.conn.execute(&format!("CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF; CREATE USER [{login}] FOR LOGIN [{login}];")).await.unwrap();
            let rights = match authority {
                "owner" => format!("ALTER ROLE db_owner ADD MEMBER [{login}];"),
                "control" => format!(
                    "GRANT CREATE TABLE, CREATE VIEW TO [{login}]; GRANT CONTROL ON SCHEMA::app TO [{login}];"
                ),
                _ => format!(
                    "GRANT CREATE TABLE, CREATE VIEW TO [{login}]; GRANT ALTER, VIEW DEFINITION ON SCHEMA::app TO [{login}];"
                ),
            };
            db.conn.execute(&rights).await.unwrap();
            let base = conn_str()
                .split(';')
                .filter(|part| {
                    !matches!(
                        part.split('=')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_ascii_lowercase()
                            .as_str(),
                        "user id" | "uid" | "password" | "pwd" | "database"
                    )
                })
                .collect::<Vec<_>>()
                .join(";");
            connect_live(&format!(
                "{};User Id={login};Password={password};Database={}",
                base.trim_end_matches(';'),
                db.name
            ))
            .await
            .unwrap()
        };
        let sysadmin: i32 = connection
            .query("SELECT IS_SRVROLEMEMBER('sysadmin') AS yes;")
            .await
            .unwrap()[0]
            .try_get("yes")
            .unwrap()
            .unwrap();
        let before = holdings(&mut connection, &targets(), &[ObjectName::new("app", "t")]).await;
        connection
            .execute("CREATE TABLE app.t (id int NOT NULL);")
            .await
            .unwrap();
        connection
            .execute("CREATE VIEW app.v AS SELECT id FROM app.t;")
            .await
            .unwrap();
        let table_grant = connection.execute("GRANT SELECT ON app.t TO reader;").await;
        let view_grant = connection.execute("GRANT SELECT ON app.v TO reader;").await;
        let owners = db.conn.query("SELECT USER_NAME(COALESCE(o.principal_id, s.principal_id)) AS owner FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE s.name = N'app' AND o.name IN (N't', N'v');").await.unwrap();
        let owners: Vec<String> = owners
            .iter()
            .map(|row| row.try_get::<&str>("owner").unwrap().unwrap().to_owned())
            .collect();
        let no_create = if authority == "narrow" {
            db.conn
                .execute(&format!("REVOKE CREATE TABLE FROM [{login}];"))
                .await
                .unwrap();
            Some(holdings(&mut connection, &targets(), &[ObjectName::new("app", "t")]).await)
        } else {
            None
        };
        drop(connection);
        if authority != "sa" {
            db.conn
                .execute(&format!("DROP USER [{login}]; DROP LOGIN [{login}];"))
                .await
                .unwrap();
        }
        db.drop().await;

        assert_eq!(sysadmin, i32::from(authority == "sa"));
        assert_eq!(owners, ["schema_owner", "schema_owner"]);
        if let Some(held) = no_create {
            assert!(
                doctor::missing(&held)
                    .iter()
                    .any(|g| g.permission == "CREATE TABLE"),
                "{held:?}"
            );
        }
        if authority == "narrow" {
            for result in [table_grant, view_grant] {
                assert!(result.unwrap_err().to_string().contains("15151"));
            }
            assert_eq!(control_gaps(&before), [Securable::Schema("app".into())]);
        } else {
            table_grant.unwrap();
            view_grant.unwrap();
            assert!(control_gaps(&before).is_empty(), "{authority}: {before:?}");
        }
        if matches!(authority, "narrow" | "control") {
            let gaps = doctor::missing(&before);
            for permission in ["CREATE ROLE", "ALTER ANY ROLE"] {
                assert!(gaps.iter().any(|g| g.permission == permission), "{gaps:?}");
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn hidden_denied_recorded_and_undeclared_grant_targets_keep_object_gaps() {
    let mut db = TestDb::create("future_grant_negatives").await;
    db.conn.execute("EXEC('CREATE SCHEMA app'); CREATE TABLE app.t (id int); CREATE USER deployer WITHOUT LOGIN; GRANT ALTER, VIEW DEFINITION ON SCHEMA::app TO deployer; DENY VIEW DEFINITION ON app.t TO deployer;").await.unwrap();
    db.conn
        .execute("EXECUTE AS USER = 'deployer';")
        .await
        .unwrap();
    let hidden = holdings(&mut db.conn, &targets(), &[ObjectName::new("app", "t")]).await;
    db.conn.execute("REVERT; REVOKE VIEW DEFINITION ON app.t FROM deployer; GRANT CONTROL ON SCHEMA::app TO deployer; DENY CONTROL ON app.t TO deployer; GRANT VIEW DEFINITION ON app.t TO deployer;").await.unwrap();
    db.conn
        .execute("EXECUTE AS USER = 'deployer';")
        .await
        .unwrap();
    let denied = holdings(&mut db.conn, &targets(), &[ObjectName::new("app", "t")]).await;
    db.conn.execute("REVERT; DROP TABLE app.t;").await.unwrap();
    // A name in the previous state remains an existing deployment obligation,
    // even if no uid was recorded and the physical object has disappeared.
    let mut recorded = Schema::default();
    recorded
        .tables
        .insert(ObjectName::new("app", "t"), Table::default());
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .unwrap();
    pbps_mssql::state::record(
        &mut db.conn,
        &StateSnapshot::new(
            pbps_model::StateKind::Apply,
            recorded,
            IdsFile::default(),
            "live-test",
        ),
    )
    .await
    .unwrap();
    let recorded = holdings(&mut db.conn, &targets(), &[ObjectName::new("app", "t")]).await;
    let undeclared = holdings(
        &mut db.conn,
        &GrantTargets {
            objects: vec![ObjectName::new("app", "outside")],
            ..Default::default()
        },
        &[],
    )
    .await;
    db.drop().await;
    for held in [&hidden, &denied, &recorded] {
        assert!(
            control_gaps(held).contains(&Securable::Object(ObjectName::new("app", "t"))),
            "{held:?}"
        );
    }
    assert_eq!(
        control_gaps(&undeclared),
        [Securable::Object(ObjectName::new("app", "outside"))]
    );
    // A hidden object's unreadable DENY also prevents claiming that a different
    // invisible name is a future view merely because schema metadata is held.
    assert!(control_gaps(&hidden).contains(&Securable::Object(ObjectName::new("app", "v"))));
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn future_grant_proof_respects_collation_missing_schemas_and_recorded_modules() {
    let mut db =
        TestDb::create_collated("future_grant_membership", "Latin1_General_100_CI_AS").await;
    db.conn.execute("EXEC('CREATE SCHEMA app');").await.unwrap();
    let mut grants = targets();
    grants.objects = vec![ObjectName::new("APP", "T"), ObjectName::new("APP", "V")];
    let aliases = holdings(&mut db.conn, &grants, &[ObjectName::new("app", "t")]).await;
    // Missing schema and present-but-ungranted schema are distinct results.
    let absent = doctor::permissions(
        &mut db.conn,
        &[ObjectName::new("absent", "t")],
        &["absent".into()],
        &Default::default(),
        &GrantTargets {
            objects: vec![ObjectName::new("absent", "t")],
            ..Default::default()
        },
        &Default::default(),
        &Default::default(),
        &IdsFile::default(),
    )
    .await
    .unwrap();

    let mut recorded = Schema::default();
    recorded.modules.insert(
        ModuleId::Named(ObjectName::new("app", "v")),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: "SELECT 1 AS id".into(),
        },
    );
    recorded.roles.insert(
        "reader".into(),
        pbps_model::Role {
            grants: [(
                pbps_model::GrantTarget::Object(ObjectName::new("app", "t")),
                [pbps_model::Permission::Select].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    );
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .unwrap();
    pbps_mssql::state::record(
        &mut db.conn,
        &StateSnapshot::new(
            pbps_model::StateKind::Apply,
            recorded,
            IdsFile::default(),
            "live-test",
        ),
    )
    .await
    .unwrap();
    let recorded = holdings(&mut db.conn, &grants, &[ObjectName::new("app", "t")]).await;
    db.conn
        .execute("UPDATE dbo.__pbps_state SET state_json = '{broken';")
        .await
        .unwrap();
    let unreadable = holdings(&mut db.conn, &targets(), &[ObjectName::new("app", "t")]).await;
    db.drop().await;
    assert!(control_gaps(&aliases).is_empty(), "{aliases:?}");
    assert!(aliases.absent_schemas.is_empty(), "{aliases:?}");
    assert!(absent.absent_schemas.contains("absent"), "{absent:?}");
    assert!(control_gaps(&absent).is_empty(), "{absent:?}");
    for name in [ObjectName::new("APP", "T"), ObjectName::new("APP", "V")] {
        assert!(
            control_gaps(&recorded).contains(&Securable::Object(name)),
            "{recorded:?}"
        );
    }
    assert_eq!(control_gaps(&unreadable).len(), 2, "{unreadable:?}");
    assert!(
        control_gaps(&unreadable)
            .iter()
            .all(|g| matches!(g, Securable::Object(_)))
    );
}
