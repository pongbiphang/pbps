use super::{conn_str, connect_live, create_database, login_url};
use pbps_db::Conn;
use pbps_mssql::resolver::authorization::{
    AuthorizationContext, PrincipalMap, enter, read, reconstruct, verify,
};
use std::time::{SystemTime, UNIX_EPOCH};

const PASSWORD: &str = "Pbps!Auth720";

fn ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn literal(name: &str) -> String {
    format!("N'{}'", name.replace('\'', "''"))
}

#[derive(Clone)]
struct Names {
    token: String,
    databases: [String; 4],
    deployer: String,
    run_login: String,
}

impl Names {
    fn new() -> Self {
        let token = format!(
            "{:x}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        );
        Self {
            databases: std::array::from_fn(|i| format!("pbps720_{i}_{token}")),
            deployer: format!("pbps720_dep_{token}"),
            run_login: format!("pbps720_run_{token}"),
            token,
        }
    }
}

async fn admin(database: &str) -> Conn {
    connect_live(&format!("{};Database={database}", conn_str()))
        .await
        .unwrap()
}

async fn identity(conn: &mut Conn, user: &str, login: &str) {
    let rows = conn.query("SELECT USER_NAME() AS effective, ORIGINAL_LOGIN() AS login, CONVERT(nvarchar(8), IS_SRVROLEMEMBER(N'sysadmin')) AS elevated;").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get::<&str>("effective").unwrap(), Some(user));
    assert_eq!(rows[0].try_get::<&str>("login").unwrap(), Some(login));
    assert_eq!(rows[0].try_get::<&str>("elevated").unwrap(), Some("0"));
}

async fn effective(conn: &mut Conn, other: &str, expected: bool) {
    let rows = conn.query(&format!("SELECT CONVERT(nvarchar(8), HAS_PERMS_BY_NAME({}, N'USER', N'IMPERSONATE')) AS answer;", literal(other))).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].try_get::<&str>("answer").unwrap(),
        Some(if expected { "1" } else { "0" }),
        "independent effective-permission oracle for {other}"
    );
}

async fn execution(conn: &mut Conn, other: &str, allowed: bool, user: &str, login: &str) {
    identity(conn, user, login).await;
    let result = conn
        .execute(&format!("EXECUTE AS USER = {};", literal(other)))
        .await;
    if allowed {
        result.unwrap();
        identity(conn, other, login).await;
        // Balance only the successful inner impersonation. An unconditional
        // REVERT after refusal would pop the outer mapped-deployer context.
        conn.execute("REVERT;").await.unwrap();
    } else {
        let error = result.expect_err("a known existing SQL user is denied impersonation");
        assert_eq!(error.server_error_code().as_deref(), Some("15517"));
    }
    identity(conn, user, login).await;
}

fn mapped(map: &PrincipalMap, target: &AuthorizationContext, name: &str) -> String {
    // This clone is only a lookup through the public mapping API. It is
    // never used as a read, replay recipe, or verification expectation.
    let mut lookup = target.clone();
    lookup.principal.effective = name.into();
    map.deployer(&lookup).unwrap().raw().to_owned()
}

async fn subject_exists(conn: &mut Conn, other: &str) {
    let rows = conn
        .query(&format!(
            "SELECT name, type AS kind FROM sys.database_principals WHERE name = {};",
            literal(other)
        ))
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the impersonation subject exists independently of deployer visibility"
    );
    assert_eq!(rows[0].try_get::<&str>("name").unwrap(), Some(other));
    assert_eq!(rows[0].try_get::<&str>("kind").unwrap(), Some("S"));
}

async fn deny_row(
    conn: &mut Conn,
    map: &PrincipalMap,
    target: &AuthorizationContext,
    present: bool,
) {
    let held = target
        .principal_grants
        .iter()
        .find(|g| {
            g.on.raw() == "other" && g.grant.permission == "IMPERSONATE" && g.grant.state == "DENY"
        })
        .expect("the target retains the exact raw DENY");
    let other = mapped(map, target, "other");
    let grantee = mapped(map, target, held.grant.grantee.raw());
    let grantor = mapped(map, target, held.grant.grantor.raw());
    let rows = conn.query(&format!("SELECT USER_NAME(p.major_id) AS subject, USER_NAME(p.grantee_principal_id) AS grantee, USER_NAME(p.grantor_principal_id) AS grantor, p.state_desc AS state FROM sys.database_permissions p WHERE p.class = 4 AND p.permission_name = N'IMPERSONATE' AND p.state = 'D' AND USER_NAME(p.major_id) = {} AND USER_NAME(p.grantee_principal_id) = {};", literal(&other), literal(&grantee))).await.unwrap();
    assert_eq!(
        rows.len(),
        usize::from(present),
        "mapped DENY row must exist independently of effective refusal"
    );
    if present {
        for (field, expected) in [
            ("subject", other.as_str()),
            ("grantee", grantee.as_str()),
            ("grantor", grantor.as_str()),
            ("state", "DENY"),
        ] {
            assert_eq!(rows[0].try_get::<&str>(field).unwrap(), Some(expected));
        }
    }
}

async fn target_setup(database: &str, names: &Names) -> Conn {
    let mut setup = admin(database).await;
    setup.execute(&format!("CREATE USER {} FOR LOGIN {} WITH DEFAULT_SCHEMA = app; CREATE USER other WITHOUT LOGIN; CREATE USER observer WITHOUT LOGIN; CREATE USER aux WITHOUT LOGIN;", ident(&names.deployer), ident(&names.deployer))).await.unwrap();
    setup
        .execute("CREATE SCHEMA app AUTHORIZATION dbo;")
        .await
        .unwrap();
    setup.execute(&format!("GRANT SELECT ON SCHEMA::app TO {}; GRANT IMPERSONATE ON USER::other TO public; DENY IMPERSONATE ON USER::other TO observer; DENY VIEW DEFINITION ON USER::aux TO {};", ident(&names.deployer), ident(&names.deployer))).await.unwrap();
    subject_exists(&mut setup, "other").await;
    setup
}

async fn scratch_setup(
    database: &str,
    token: &str,
    names: &Names,
    context: &AuthorizationContext,
) -> (Conn, Conn, PrincipalMap, String, String) {
    let mut setup = admin(database).await;
    setup
        .execute(&format!(
            "ALTER AUTHORIZATION ON DATABASE::{} TO {};",
            ident(database),
            ident(&names.run_login)
        ))
        .await
        .unwrap();
    let map = PrincipalMap::generate(context, &[], token);
    reconstruct(&mut setup, &map, context, &names.run_login)
        .await
        .unwrap();
    let other = mapped(&map, context, "other");
    subject_exists(&mut setup, &other).await;
    let user = map.deployer(context).unwrap().raw().to_owned();
    let mut run = connect_live(&login_url(&names.run_login, PASSWORD, database))
        .await
        .unwrap();
    enter(&mut run, Some(&user.clone().into())).await.unwrap();
    identity(&mut run, &user, &names.run_login).await;
    (setup, run, map, user, other)
}

async fn exercise(names: Names) {
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
    let schemas = ["app".to_owned()];
    let _positive_admin = target_setup(&names.databases[0], &names).await;
    let mut denied_admin = target_setup(&names.databases[1], &names).await;
    let mut positive = connect_live(&login_url(&names.deployer, PASSWORD, &names.databases[0]))
        .await
        .unwrap();
    identity(&mut positive, &names.deployer, &names.deployer).await;
    effective(&mut positive, "other", true).await;
    execution(
        &mut positive,
        "other",
        true,
        &names.deployer,
        &names.deployer,
    )
    .await;
    let positive_context = read(&mut positive, &schemas).await.unwrap();
    assert!(
        positive_context.impersonation.contains("other"),
        "the unrelated observer's DENY cannot suppress the deployer's positive right"
    );
    assert!(
        positive_context
            .principal_grants
            .iter()
            .any(|g| g.on.raw() == "aux"
                && g.grant.permission == "VIEW DEFINITION"
                && g.grant.state == "DENY")
    );
    let (mut positive_setup, mut positive_run, positive_map, positive_user, positive_other) =
        scratch_setup(
            &names.databases[2],
            &format!("p_{}", names.token),
            &names,
            &positive_context,
        )
        .await;
    effective(&mut positive_run, &positive_other, true).await;
    execution(
        &mut positive_run,
        &positive_other,
        true,
        &positive_user,
        &names.run_login,
    )
    .await;
    assert!(
        verify(
            &mut positive_run,
            &positive_map,
            &positive_context,
            &schemas
        )
        .await
        .unwrap()
        .is_empty()
    );

    let mut denied = connect_live(&login_url(&names.deployer, PASSWORD, &names.databases[1]))
        .await
        .unwrap();
    effective(&mut denied, "other", true).await;
    execution(&mut denied, "other", true, &names.deployer, &names.deployer).await;
    denied_admin
        .execute(&format!(
            "DENY IMPERSONATE ON USER::other TO {};",
            ident(&names.deployer)
        ))
        .await
        .unwrap();
    identity(&mut denied, &names.deployer, &names.deployer).await;
    effective(&mut denied, "other", false).await;
    execution(
        &mut denied,
        "other",
        false,
        &names.deployer,
        &names.deployer,
    )
    .await;
    let denied_context = read(&mut denied, &schemas).await.unwrap();
    assert!(
        !denied_context.impersonation.contains("other"),
        "a denied impersonation must not be recorded as an effective right"
    );
    assert!(denied_context.impersonation.is_empty());
    let (mut denied_setup, mut denied_run, denied_map, denied_user, denied_other) = scratch_setup(
        &names.databases[3],
        &format!("d_{}", names.token),
        &names,
        &denied_context,
    )
    .await;
    deny_row(&mut denied_setup, &denied_map, &denied_context, true).await;
    effective(&mut denied_run, &denied_other, false).await;
    execution(
        &mut denied_run,
        &denied_other,
        false,
        &denied_user,
        &names.run_login,
    )
    .await;
    assert!(
        verify(&mut denied_run, &denied_map, &denied_context, &schemas)
            .await
            .unwrap()
            .is_empty()
    );

    // Row drift remains detectable when no latent positive grant was replayed
    // and removing DENY therefore leaves the effective answer unchanged.
    denied_setup
        .execute(&format!(
            "REVOKE IMPERSONATE ON USER::{} FROM {};",
            ident(&denied_other),
            ident(&denied_user)
        ))
        .await
        .unwrap();
    deny_row(&mut denied_setup, &denied_map, &denied_context, false).await;
    effective(&mut denied_run, &denied_other, false).await;
    execution(
        &mut denied_run,
        &denied_other,
        false,
        &denied_user,
        &names.run_login,
    )
    .await;
    let differences = verify(&mut denied_run, &denied_map, &denied_context, &schemas)
        .await
        .unwrap();
    assert!(
        differences.contains(&"principal:grants".into()),
        "raw DENY drift must be named independently: {differences:?}"
    );
    denied_setup
        .execute(&format!(
            "DENY IMPERSONATE ON USER::{} TO {};",
            ident(&denied_other),
            ident(&denied_user)
        ))
        .await
        .unwrap();
    assert!(
        verify(&mut denied_run, &denied_map, &denied_context, &schemas)
            .await
            .unwrap()
            .is_empty()
    );

    // A separate target/scratch pair retains the healthy positive baseline.
    // The effective-set diagnostic must survive beside the new raw DENY row.
    effective(&mut positive, "other", true).await;
    positive_setup
        .execute(&format!(
            "DENY IMPERSONATE ON USER::{} TO {};",
            ident(&positive_other),
            ident(&positive_user)
        ))
        .await
        .unwrap();
    effective(&mut positive_run, &positive_other, false).await;
    execution(
        &mut positive_run,
        &positive_other,
        false,
        &positive_user,
        &names.run_login,
    )
    .await;
    let differences = verify(
        &mut positive_run,
        &positive_map,
        &positive_context,
        &schemas,
    )
    .await
    .unwrap();
    assert!(
        differences.contains(&"impersonation".into()),
        "effective drift must be named independently: {differences:?}"
    );
    positive_setup
        .execute(&format!(
            "REVOKE IMPERSONATE ON USER::{} FROM {}; GRANT IMPERSONATE ON USER::{} TO {};",
            ident(&positive_other),
            ident(&positive_user),
            ident(&positive_other),
            ident(&positive_user)
        ))
        .await
        .unwrap();
    effective(&mut positive_run, &positive_other, true).await;
    assert!(
        verify(
            &mut positive_run,
            &positive_map,
            &positive_context,
            &schemas
        )
        .await
        .unwrap()
        .is_empty()
    );
}

pub(super) async fn impersonation_answers_preserve_denials() {
    let names = Names::new();
    let mut root = admin("master").await;
    let version = root.query("SELECT CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')) AS version, CONVERT(nvarchar(128), SERVERPROPERTY('Edition')) AS edition;").await.unwrap();
    let build = version[0]
        .try_get::<&str>("version")
        .unwrap()
        .unwrap()
        .to_owned();
    let edition = version[0]
        .try_get::<&str>("edition")
        .unwrap()
        .unwrap()
        .to_owned();
    for (sql, planned) in [
        ("SELECT name FROM sys.databases;", names.databases.to_vec()),
        (
            "SELECT name FROM sys.server_principals;",
            vec![names.deployer.clone(), names.run_login.clone()],
        ),
    ] {
        let rows = root.query(sql).await.unwrap();
        assert!(
            !rows.iter().any(|row| planned
                .iter()
                .any(|name| row.try_get::<&str>("name").unwrap() == Some(name.as_str()))),
            "an existing name is not ours to clean"
        );
    }
    // The outer task owns cleanup even if setup or an assertion panics. All
    // generated principals are database-scoped except these two known logins.
    let result = tokio::spawn(exercise(names.clone())).await;
    let _gate = super::SINGLE_USER_GATE.read().await;
    let mut cleanup = Vec::new();
    for database in &names.databases {
        if let Err(error) = root.execute(&format!("IF DB_ID({}) IS NOT NULL BEGIN ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {}; END;", literal(database), ident(database), ident(database))).await {
            cleanup.push(error.to_string());
        }
    }
    for login in [&names.deployer, &names.run_login] {
        if let Err(error) = root
            .execute(&format!(
                "IF SUSER_ID({}) IS NOT NULL DROP LOGIN {};",
                literal(login),
                ident(login)
            ))
            .await
        {
            cleanup.push(error.to_string());
        }
    }
    assert!(
        cleanup.is_empty(),
        "case {result:?}; owned cleanup: {cleanup:?}"
    );
    result.unwrap();
    println!("auth720 completed SQL Server {build} {edition}");
}
