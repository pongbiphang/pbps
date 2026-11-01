//! Per-case accounting on a warm MSSQL fixture. The runner keeps the instance
//! exclusive (`--test-threads=1`); a new resource in this interval is this
//! case's, never a reason to sweep a prefix belonging to another case.
use super::{connection, create_database, drop_database, server};
use std::collections::BTreeSet;
use std::future::Future;

#[derive(Debug, PartialEq)]
struct Inventory {
    databases: BTreeSet<String>,
    logins: BTreeSet<String>,
}

async fn names(conn: &mut pbps_db::Conn, query: &str) -> Result<BTreeSet<String>, String> {
    let rows = conn.query(query).await.map_err(|e| e.to_string())?;
    rows.iter()
        .map(|r| {
            r.try_get::<&str>("name")
                .map_err(|e| e.to_string())?
                .map(str::to_owned)
                .ok_or_else(|| "fixture inventory name is NULL".to_owned())
        })
        .collect()
}

impl Inventory {
    async fn read(server: &str) -> Result<Self, String> {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Mssql, server)
            .await
            .map_err(|e| format!("fixture inventory connection: {e}"))?;
        let visibility = conn
            .query("SELECT IS_SRVROLEMEMBER(N'sysadmin') AS visible;")
            .await
            .map_err(|e| e.to_string())?;
        if visibility.len() != 1
            || visibility[0]
                .try_get::<i32>("visible")
                .map_err(|e| e.to_string())?
                != Some(1)
        {
            return Err("warm fixture inventory requires sysadmin visibility".into());
        }
        Ok(Self {
            databases: names(&mut conn, "SELECT name FROM sys.databases;").await?,
            logins: names(
                &mut conn,
                "SELECT name FROM sys.server_principals WHERE type <> 'R';",
            )
            .await?,
        })
    }
}

async fn pristine(server: &str) -> Result<serde_json::Value, String> {
    let inventory = Inventory::read(server).await?;
    let expected_databases = ["master", "model", "msdb", "tempdb"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if inventory.databases != expected_databases {
        return Err(format!(
            "unexpected warm fixture databases: {:?}",
            inventory.databases
        ));
    }
    // These are engine principals, including the Windows service identities
    // shipped in the Linux image. An added application login is never baseline.
    let unexpected: Vec<_> = inventory
        .logins
        .iter()
        .filter(|name| {
            !matches!(
                name.as_str(),
                "sa" | "BUILTIN\\Administrators"
                    | "NT AUTHORITY\\SYSTEM"
                    | "NT AUTHORITY\\NETWORK SERVICE"
            ) && !(name.starts_with("##MS_") && name.ends_with("##"))
        })
        .collect();
    if !unexpected.is_empty() {
        return Err(format!("unexpected warm fixture logins: {unexpected:?}"));
    }
    let mut conn = connection(server).await;
    super::sql::use_database(&mut conn, "model")
        .await
        .map_err(|e| e.to_string())?;
    let model = super::sql::inventory::read(&mut conn, None)
        .await
        .map_err(|e| e.to_string())?;
    if !model.is_empty() {
        return Err(format!(
            "unexpected warm fixture model state: {:?}",
            model.names()
        ));
    }
    // Use structural inventory rules, not the output of a particular image
    // build. Snapshot all catalog fields only after opening model consistently.
    super::sql::use_database(&mut conn, "master")
        .await
        .map_err(|e| e.to_string())?;
    let rows = conn
        .query(include_str!("fixtures/warm2585-pristine.sql"))
        .await
        .map_err(|e| e.to_string())?;
    if rows.len() != 1 {
        return Err("the fixture catalog snapshot is unreadable".into());
    }
    let text = rows[0]
        .try_get::<&str>("catalog")
        .map_err(|e| e.to_string())?
        .ok_or("the fixture catalog snapshot is NULL")?;
    let actual: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let configuration = actual["server_configuration"]
        .as_array()
        .ok_or("the fixture configuration is unreadable")?;
    let expected: BTreeSet<_> = [
        "contained database authentication",
        "cross db ownership chaining",
        "user options",
    ]
    .into_iter()
    .collect();
    let observed: BTreeSet<_> = configuration
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    if observed != expected
        || configuration.len() != 3
        || configuration.iter().any(|v| v["value"].as_i64() != Some(0))
    {
        return Err("warm fixture server configuration is not at engine defaults".into());
    }
    // Inventory recognizes shipped SELECTs. Require their original public/dbo
    // grant shape too, so a changed permission on a system view is not accepted.
    let permissions = actual["model_permissions"]
        .as_array()
        .ok_or("the model permission snapshot is unreadable")?;
    if permissions.iter().any(|p| {
        p["class"] == 1
            && (p["grantee"] != "public"
                || p["grantor"] != "dbo"
                || p["state_desc"] != "GRANT"
                || p["permission_name"] != "SELECT")
    }) {
        return Err("unexpected warm fixture model system permissions".into());
    }
    Ok(actual)
}

/// Never let residue supply an object, principal or default the case meant
/// to create. The runner grants this entire interval exclusive fixture use.
pub(super) fn run<'a, F>(
    servers: &'a [&'a str],
    body: F,
) -> impl Future<Output = Result<(), String>> + 'a
where
    F: Future<Output = ()> + Send + 'static,
{
    // Box before constructing the future so nested contract cases do not
    // multiply their initial frames while the entry catalog is being read.
    let body = Box::pin(body);
    async move {
        let mut baselines = Vec::new();
        for server in servers {
            let before = pristine(server)
                .await
                .map_err(|e| format!("warm fixture is not pristine before case: {e}"))?;
            baselines.push((server, before));
        }
        let mut failures: Vec<_> = checked(servers, body).await.err().into_iter().collect();
        for (server, before) in baselines {
            match pristine(server).await {
                Ok(after) if after != before => {
                    let changed: Vec<_> = before
                        .as_object()
                        .unwrap()
                        .keys()
                        .filter(|key| after[*key] != before[*key])
                        .collect();
                    failures.push(format!(
                        "warm fixture catalog changed after case: {changed:?}"
                    ));
                }
                Ok(_) => {}
                Err(error) => {
                    failures.push(format!("warm fixture was not restored after case: {error}"))
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

pub(super) async fn guest_context(server: &str) -> BTreeSet<String> {
    // Metadata text uses different built-in collations even on one instance.
    // Compare the three projections under the connection's single collation.
    names(
        &mut connection(server).await,
        r#"
SELECT (N'permission:' + CONVERT(nvarchar(12),p.class)
    + N':' + CONVERT(nvarchar(12),p.major_id) + N':' + CONVERT(nvarchar(12),p.minor_id)
    + N':' + p.permission_name + N':' + p.state
    + N':' + CONVERT(nvarchar(12),p.grantor_principal_id)) COLLATE DATABASE_DEFAULT AS name
FROM msdb.sys.database_permissions p
JOIN msdb.sys.database_principals u ON u.principal_id=p.grantee_principal_id
WHERE u.name=N'guest'
UNION ALL
SELECT (N'role:' + r.name) COLLATE DATABASE_DEFAULT
FROM msdb.sys.database_role_members m
JOIN msdb.sys.database_principals r ON r.principal_id=m.role_principal_id
JOIN msdb.sys.database_principals u ON u.principal_id=m.member_principal_id
WHERE u.name=N'guest'
UNION ALL
SELECT (N'schema:' + s.name) COLLATE DATABASE_DEFAULT
FROM msdb.sys.schemas s
JOIN msdb.sys.database_principals u ON u.principal_id=s.principal_id
WHERE u.name=N'guest'
ORDER BY name;
"#,
    )
    .await
    .unwrap()
}

/// Check release before panic recovery: deleting a leaked resource cannot turn
/// its originating test green. Exact before/after names preserve other owners.
pub(super) async fn checked<F>(servers: &[&str], run: F) -> Result<(), String>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut baselines = Vec::new();
    for server in servers {
        baselines.push((server.to_string(), Inventory::read(server).await?));
    }
    let result = tokio::spawn(run).await;
    let mut failures = Vec::new();
    if let Err(error) = result {
        failures.push(format!("fixture callback panicked: {error}"));
    }
    for (server, before) in baselines {
        let after = Inventory::read(&server).await?;
        let databases: Vec<_> = after
            .databases
            .difference(&before.databases)
            .cloned()
            .collect();
        let logins: Vec<_> = after.logins.difference(&before.logins).cloned().collect();
        if after != before {
            failures.push(format!("fixture release differs: leaked databases {databases:?}; leaked logins {logins:?}; missing databases {:?}; missing logins {:?}",
                before.databases.difference(&after.databases).collect::<Vec<_>>(),
                before.logins.difference(&after.logins).collect::<Vec<_>>()));
        }
        // The exclusive interval is the ownership proof for these exact names.
        // Database dependencies go first, including reference and companion DBs.
        for database in databases {
            let quoted = database.replace(']', "]]");
            let cleanup = connection(&server).await.execute(&format!(
                "USE master; ALTER DATABASE [{quoted}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{quoted}];"
            )).await;
            if let Err(error) = cleanup {
                failures.push(format!(
                    "fixture database {database} recovery failed: {error}"
                ));
            }
        }
        for login in logins {
            if let Err(error) = connection(&server)
                .await
                .execute(&format!("DROP LOGIN [{}];", login.replace(']', "]]")))
                .await
            {
                failures.push(format!("fixture login {login} recovery failed: {error}"));
            }
        }
        if Inventory::read(&server).await? != before {
            failures.push("fixture inventory was not restored".into());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Shared state must restore even when an assertion or acquisition panics.
/// Callers additionally verify the catalog state they changed.
pub(super) async fn restoring<F, T>(
    server: &str,
    change: &str,
    restore: &str,
    run: F,
) -> Result<T, tokio::task::JoinError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut admin = connection(server).await;
    admin.execute(change).await.unwrap();
    let result = tokio::spawn(run).await;
    admin.execute(restore).await.unwrap();
    result
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_warm_case_reports_and_recovers_database_and_login_leaks_without_touching_a_baseline() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (baseline_name, _) = create_database(&scratch).await;
    let baseline_login = format!("pbps2585_keep_{:032x}", rand::random::<u128>());
    connection(&scratch)
        .await
        .execute(&format!(
            "CREATE LOGIN [{baseline_login}] WITH PASSWORD=N'Pbps!Test12345',CHECK_POLICY=OFF;"
        ))
        .await
        .unwrap();
    let leaked_login = format!("pbps2585_leaked_{:032x}", rand::random::<u128>());
    let leaked_for_run = leaked_login.clone();
    let leaked_database = format!("pbps2585_leaked_{:032x}", rand::random::<u128>());
    let database_for_run = leaked_database.clone();
    let source = scratch.clone();
    let result =
        checked(&[&scratch], async move {
            connection(&source)
                .await
                .execute(&format!("CREATE DATABASE [{database_for_run}];"))
                .await
                .unwrap();
            connection(&source).await.execute(&format!(
            "CREATE LOGIN [{leaked_for_run}] WITH PASSWORD=N'Pbps!Test12345',CHECK_POLICY=OFF;"
        )).await.unwrap();
        })
        .await;
    let remaining = Inventory::read(&scratch).await.unwrap();
    // Keep revert experiments clean without hiding the measured leftovers.
    if remaining.databases.contains(&leaked_database) {
        drop_database(&scratch, &leaked_database).await;
    }
    if remaining.logins.contains(&leaked_login) {
        connection(&scratch)
            .await
            .execute(&format!("DROP LOGIN [{leaked_login}];"))
            .await
            .unwrap();
    }
    let clean = checked(&[&scratch], async {}).await;
    drop_database(&scratch, &baseline_name).await;
    connection(&scratch)
        .await
        .execute(&format!("DROP LOGIN [{baseline_login}];"))
        .await
        .unwrap();
    let error = result.unwrap_err();
    assert!(
        error.contains(&leaked_database) && error.contains(&leaked_login),
        "{error}"
    );
    assert!(!remaining.databases.contains(&leaked_database));
    assert!(!remaining.logins.contains(&leaked_login));
    assert!(remaining.databases.contains(&baseline_name));
    assert!(remaining.logins.contains(&baseline_login));
    clean.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_warm_case_recovers_its_owned_resources_after_a_callback_panic() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let before = Inventory::read(&scratch).await.unwrap();
    let leaked_database = format!("pbps2585_panic_{:032x}", rand::random::<u128>());
    let database_for_run = leaked_database.clone();
    let source = scratch.clone();
    let result = checked(&[&scratch], async move {
        connection(&source)
            .await
            .execute(&format!("CREATE DATABASE [{database_for_run}];"))
            .await
            .unwrap();
        panic!("intentional warm callback panic");
    })
    .await;
    let after = Inventory::read(&scratch).await.unwrap();
    if after.databases.contains(&leaked_database) {
        drop_database(&scratch, &leaked_database).await;
    }
    let error = result.unwrap_err();
    assert!(
        error.contains("intentional warm callback panic") && error.contains("leaked databases"),
        "{error}"
    );
    assert_eq!(after, before);
    checked(&[&scratch], async {}).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn the_warm_model_is_restored_before_a_callback_panic_is_reported() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let marker = format!("pbps2585_model_{:032x}", rand::random::<u128>());
    let change = format!("EXEC(N'USE model; CREATE TABLE dbo.{marker}(i int);');");
    let restore = format!("EXEC(N'USE model; DROP TABLE dbo.{marker};');");
    let result = restoring(&scratch, &change, &restore, async {
        panic!("intentional model callback panic");
    })
    .await;
    let remaining = connection(&scratch)
        .await
        .query(&format!(
            "SELECT OBJECT_ID(N'model.dbo.{marker}') AS remaining;"
        ))
        .await
        .unwrap()[0]
        .try_get::<i32>("remaining")
        .unwrap();
    // Revert checks repair the deliberately broken restoration before asserting.
    if remaining.is_some() {
        connection(&scratch).await.execute(&restore).await.unwrap();
    }
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("intentional model callback panic")
    );
    assert_eq!(remaining, None);
    let target = server("PBPS_TEST_DB");
    let outcome = super::outcome(&target, &scratch).await;
    assert!(outcome.is_ok(), "{}", super::why(&outcome));
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn limited_catalog_visibility_never_counts_as_an_empty_warm_inventory() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let login = format!("pbps2585_hidden_{:032x}", rand::random::<u128>());
    connection(&scratch)
        .await
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'Pbps!Test12345',CHECK_POLICY=OFF;"
        ))
        .await
        .unwrap();
    let limited = format!("{scratch};User Id={login};Password=Pbps!Test12345");
    let result = checked(&[&limited], async {
        panic!("an unreadable fixture must not run a case");
    })
    .await;
    connection(&scratch)
        .await
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert_eq!(
        result.unwrap_err(),
        "warm fixture inventory requires sysadmin visibility"
    );
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn the_warm_system_context_is_restored_before_a_callback_panic_is_reported() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let before = guest_context(&scratch).await;
    assert!(
        !before
            .iter()
            .any(|name| name.starts_with("permission:0:0:0:CONTROL:"))
    );
    let restore = "USE msdb; REVOKE CONTROL FROM guest;";
    let result = restoring(
        &scratch,
        "USE msdb; GRANT CONTROL TO guest;",
        restore,
        async {
            panic!("intentional system callback panic");
        },
    )
    .await;
    let after = guest_context(&scratch).await;
    // Leave the fixture clean even when restoration is deliberately reverted.
    if after != before {
        connection(&scratch).await.execute(restore).await.unwrap();
    }
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("intentional system callback panic")
    );
    assert_eq!(after, before);
    let target = server("PBPS_TEST_DB");
    let outcome = super::outcome(&target, &scratch).await;
    assert!(outcome.is_ok(), "{}", super::why(&outcome));
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_dirty_warm_instance_refuses_before_its_case_can_run() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    pristine(&scratch).await.unwrap();
    for kind in [
        "database",
        "login",
        "model",
        "principal",
        "permission",
        "configuration",
    ] {
        let name = format!("pbps2585_dirty_{:032x}", rand::random::<u128>());
        let (change, restore) = match kind {
            "database" => (
                format!("CREATE DATABASE [{name}];"),
                format!(
                    "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
                ),
            ),
            "login" => (
                format!("CREATE LOGIN [{name}] WITH PASSWORD=N'Pbps!Test12345',CHECK_POLICY=OFF;"),
                format!("DROP LOGIN [{name}];"),
            ),
            "model" => (
                format!("EXEC(N'USE model; CREATE TABLE dbo.{name}(i int);');"),
                format!("EXEC(N'USE model; DROP TABLE dbo.{name};');"),
            ),
            "principal" => (
                format!("EXEC(N'USE model; CREATE USER [{name}] WITHOUT LOGIN;');"),
                format!("EXEC(N'USE model; DROP USER [{name}];');"),
            ),
            "permission" => (
                "EXEC(N'USE model; GRANT CREATE TABLE TO guest;');".into(),
                "EXEC(N'USE model; REVOKE CREATE TABLE FROM guest;');".into(),
            ),
            "configuration" => (
                "EXEC sys.sp_configure N'user options', 8; RECONFIGURE;".into(),
                "EXEC sys.sp_configure N'user options', 0; RECONFIGURE;".into(),
            ),
            _ => unreachable!(),
        };
        connection(&scratch).await.execute(&change).await.unwrap();
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inside = called.clone();
        let result = run(&[&scratch], async move {
            inside.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
        connection(&scratch).await.execute(&restore).await.unwrap();
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "{kind} residue was accepted"
        );
        assert!(
            result.unwrap_err().contains("not pristine before case"),
            "{kind}"
        );
        pristine(&scratch).await.unwrap();
    }
    run(&[&scratch], async {}).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_warm_case_reports_changed_builtin_state_against_its_own_snapshot() {
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let before = pristine(&scratch).await.unwrap();
    let source = scratch.clone();
    let result = run(&[&scratch], async move {
        connection(&source)
            .await
            .execute("ALTER LOGIN sa WITH DEFAULT_LANGUAGE=British;")
            .await
            .unwrap();
    })
    .await;
    // Repair even when the snapshot comparison is deliberately reverted.
    connection(&scratch)
        .await
        .execute("ALTER LOGIN sa WITH DEFAULT_LANGUAGE=us_english;")
        .await
        .unwrap();
    assert_eq!(pristine(&scratch).await.unwrap(), before);
    let error = result.unwrap_err();
    assert!(
        error.contains("catalog changed after case") && error.contains("logins"),
        "{error}"
    );
    run(&[&scratch], async {}).await.unwrap();
}
