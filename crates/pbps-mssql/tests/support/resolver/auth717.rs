use super::{SINGLE_USER_GATE, conn_str, connect_live, create_database, login_url};
use pbps_db::Conn;
use pbps_mssql::resolver::authorization::{PrincipalMap, enter, read, reconstruct, verify};

const PASSWORD: &str = "Pbps!Auth717";

fn ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn literal(name: &str) -> String {
    format!("N'{}'", name.replace('\'', "''"))
}

#[derive(Clone)]
struct Names {
    token: String,
    databases: [String; 2],
    deployer: String,
    run_login: String,
}

impl Names {
    fn new() -> Self {
        let token = format!(
            "{:x}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        );
        Self {
            databases: std::array::from_fn(|i| format!("pbps717_{i}_{token}")),
            deployer: format!("pbps717_dep_{token}"),
            run_login: format!("pbps717_run_{token}"),
            token,
        }
    }
}

async fn admin(database: &str) -> Conn {
    connect_live(&format!("{};Database={database}", conn_str()))
        .await
        .unwrap()
}

async fn identity(conn: &mut Conn, expected: &str) {
    let rows = conn.query("SELECT USER_NAME() AS effective, CONVERT(nvarchar(8), IS_SRVROLEMEMBER(N'sysadmin')) AS elevated;").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].try_get::<&str>("effective").unwrap(),
        Some(expected)
    );
    assert_eq!(rows[0].try_get::<&str>("elevated").unwrap(), Some("0"));
}

async fn binding(conn: &mut Conn, expected: &str, update_allowed: bool) {
    let rows = conn
        .query("SELECT CONVERT(nvarchar(8), value) AS value FROM probe;")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get::<&str>("value").unwrap(), Some(expected));
    let result = conn.execute("UPDATE dbo.probe SET value = value;").await;
    if update_allowed {
        result.unwrap();
    } else {
        assert_eq!(
            result.unwrap_err().server_error_code().as_deref(),
            Some("229")
        );
    }
}

async fn exercise(names: Names, absent: bool) {
    let mut root = admin("master").await;
    for database in &names.databases {
        create_database(&mut root, &format!("CREATE DATABASE {};", ident(database)))
            .await
            .unwrap();
    }
    for login in [&names.deployer, &names.run_login] {
        root.execute(&format!(
            "CREATE LOGIN {} WITH PASSWORD = '{PASSWORD}', CHECK_POLICY = OFF;",
            ident(login)
        ))
        .await
        .unwrap();
    }
    let mut target_admin = admin(&names.databases[0]).await;
    let default = if absent { "missing'[]" } else { "outside" };
    target_admin.execute(&format!("CREATE USER {} FOR LOGIN {} WITH DEFAULT_SCHEMA = {}; CREATE USER schema_owner WITHOUT LOGIN;", ident(&names.deployer), ident(&names.deployer), ident(default))).await.unwrap();
    target_admin
        .execute("CREATE SCHEMA app AUTHORIZATION dbo;")
        .await
        .unwrap();
    target_admin.execute("CREATE TABLE dbo.probe (value int NOT NULL); INSERT dbo.probe VALUES (11); GRANT SELECT, UPDATE ON SCHEMA::dbo TO public;").await.unwrap();
    if !absent {
        target_admin
            .execute("CREATE SCHEMA outside AUTHORIZATION schema_owner;")
            .await
            .unwrap();
        target_admin.execute("CREATE TABLE outside.probe (value int NOT NULL); INSERT outside.probe VALUES (22); GRANT SELECT ON SCHEMA::outside TO public;").await.unwrap();
    }
    target_admin
        .execute(&format!(
            "DENY UPDATE ON SCHEMA::dbo TO {}; GRANT SELECT ON SCHEMA::app TO {};",
            ident(&names.deployer),
            ident(&names.deployer)
        ))
        .await
        .unwrap();
    let mut target = connect_live(&login_url(&names.deployer, PASSWORD, &names.databases[0]))
        .await
        .unwrap();
    identity(&mut target, &names.deployer).await;
    binding(&mut target, if absent { "11" } else { "22" }, false).await;
    let rows = target
        .query("SELECT SCHEMA_NAME() AS effective_default;")
        .await
        .unwrap();
    assert_eq!(
        rows[0].try_get::<&str>("effective_default").unwrap(),
        if absent { None } else { Some("outside") }
    );
    let schemas = ["APP".to_owned(), "app".to_owned()];
    let context = read(&mut target, &schemas).await.unwrap();
    assert!(
        context.schemas.contains_key("dbo"),
        "the unrequested dbo fallback must be captured"
    );
    assert_eq!(context.default_schema, default);
    if absent {
        assert!(!context.schemas.contains_key(default));
        let canonical: serde_json::Value = serde_json::from_slice(&context.canonical()).unwrap();
        assert_eq!(
            canonical["schema_domain"][default], "absent",
            "known absence is sealed independently of the replay schema map"
        );
    } else {
        assert!(
            context.schemas.contains_key("outside"),
            "the existing unrequested default must be captured"
        );
    }
    let round = pbps_mssql::resolver::scope_facts(
        &mut target,
        &pbps_mssql::resolver::environment::Scope { schemas: &schemas },
        &schemas,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        context.canonical(),
        round.1.canonical(),
        "both complete bracketed reads carry the fallback domain"
    );
    let mut scratch_admin = admin(&names.databases[1]).await;
    scratch_admin
        .execute(&format!(
            "ALTER AUTHORIZATION ON DATABASE::{} TO {};",
            ident(&names.databases[1]),
            ident(&names.run_login)
        ))
        .await
        .unwrap();
    let map = PrincipalMap::generate(&context, &[], &names.token);
    reconstruct(&mut scratch_admin, &map, &context, &names.run_login)
        .await
        .unwrap();
    scratch_admin
        .execute("CREATE TABLE dbo.probe (value int NOT NULL); INSERT dbo.probe VALUES (11);")
        .await
        .unwrap();
    if !absent {
        scratch_admin.execute("CREATE TABLE outside.probe (value int NOT NULL); INSERT outside.probe VALUES (22);").await.unwrap();
    }
    let user = map.deployer(&context).unwrap();
    let mut scratch = connect_live(&login_url(&names.run_login, PASSWORD, &names.databases[1]))
        .await
        .unwrap();
    enter(&mut scratch, Some(&user)).await.unwrap();
    identity(&mut scratch, user.raw()).await;
    binding(&mut scratch, if absent { "11" } else { "22" }, false).await;
    assert!(
        verify(&mut scratch, &map, &context, &schemas)
            .await
            .unwrap()
            .is_empty()
    );

    // Removing the replayed DENY exposes an executable extra permission.
    scratch_admin
        .execute(&format!(
            "REVOKE UPDATE ON SCHEMA::dbo FROM {};",
            ident(user.raw())
        ))
        .await
        .unwrap();
    binding(&mut scratch, if absent { "11" } else { "22" }, true).await;
    let differences = verify(&mut scratch, &map, &context, &schemas)
        .await
        .unwrap();
    assert!(
        differences.contains(&"schema:dbo:grants".to_owned()),
        "{differences:?}"
    );
    assert!(
        differences.contains(&"schema:dbo:permissions".to_owned()),
        "{differences:?}"
    );
    scratch_admin
        .execute(&format!(
            "DENY UPDATE ON SCHEMA::dbo TO {};",
            ident(user.raw())
        ))
        .await
        .unwrap();
    assert!(
        verify(&mut scratch, &map, &context, &schemas)
            .await
            .unwrap()
            .is_empty()
    );

    if absent {
        scratch_admin
            .execute(&format!("CREATE SCHEMA {};", ident(default)))
            .await
            .unwrap();
        scratch_admin
            .execute(&format!(
                "ALTER USER {} WITH DEFAULT_SCHEMA = dbo;",
                ident(user.raw())
            ))
            .await
            .unwrap();
        let differences = verify(&mut scratch, &map, &context, &schemas)
            .await
            .unwrap();
        assert!(
            differences.contains(&format!("schema:{default}:presence")),
            "a target-absent default cannot be invented: {differences:?}"
        );
        scratch_admin
            .execute(&format!("DROP SCHEMA {};", ident(default)))
            .await
            .unwrap();
    } else {
        scratch_admin
            .execute("DROP TABLE outside.probe; DROP SCHEMA outside;")
            .await
            .unwrap();
        let differences = verify(&mut scratch, &map, &context, &schemas)
            .await
            .unwrap();
        assert!(
            differences.contains(&"schema:outside:absent".to_owned()),
            "a missing fallback names its target-qualified key: {differences:?}"
        );
    }
    identity(&mut target, &names.deployer).await;
    binding(&mut target, if absent { "11" } else { "22" }, false).await;
    if absent {
        target_admin
            .execute(&format!(
                "ALTER USER {} WITH DEFAULT_SCHEMA = dbo;",
                ident(&names.deployer)
            ))
            .await
            .unwrap();
        target_admin
            .execute(&format!("CREATE SCHEMA {};", ident(default)))
            .await
            .unwrap();
        let current = pbps_mssql::resolver::authorization::reread(&mut target, &context)
            .await
            .unwrap();
        let facts: serde_json::Value = serde_json::from_slice(&current.canonical()).unwrap();
        assert_eq!(
            facts["schema_domain"][default], "present",
            "target rereads retain a previously absent question after the current default changes"
        );
    }
}

async fn run(absent: bool) {
    let names = Names::new();
    let mut root = admin("master").await;
    for (sql, proposed) in [
        ("SELECT name FROM sys.databases;", names.databases.to_vec()),
        (
            "SELECT name FROM sys.server_principals;",
            vec![names.deployer.clone(), names.run_login.clone()],
        ),
    ] {
        let rows = root.query(sql).await.unwrap();
        assert!(
            !rows.iter().any(|r| proposed
                .iter()
                .any(|name| r.try_get::<&str>("name").unwrap() == Some(name.as_str()))),
            "an existing name is not ours to clean"
        );
    }
    // The parent owns cleanup through every assertion failure in the child.
    let result = tokio::spawn(exercise(names.clone(), absent)).await;
    let _gate = SINGLE_USER_GATE.read().await;
    for database in &names.databases {
        root.execute(&format!("IF DB_ID({}) IS NOT NULL BEGIN ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {}; END;", literal(database), ident(database), ident(database))).await.unwrap();
    }
    for login in [&names.deployer, &names.run_login] {
        root.execute(&format!(
            "IF SUSER_ID({}) IS NOT NULL DROP LOGIN {};",
            literal(login),
            ident(login)
        ))
        .await
        .unwrap();
    }
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_unrequested_fallback_schema_keeps_its_denials_and_identity() {
    run(false).await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_known_absent_default_stays_absent_through_replay_and_recheck() {
    run(true).await;
}
