//! SQL Server's acquisition boundary, exercised through the shared producer.

use super::super::{BindingRequest, Error, ProduceError};
use super::fixtures::{ENVIRONMENT, ProjectKey};
use super::{Provenance, Standard};
use pbps_db::{Conn, Driver};
use pbps_model::{Hints, IdsFile, Schema};
use pbps_mssql::resolver::scratch as sql;

mod catalog_cases {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-mssql/tests/support/scratch_catalogs.rs"
    ));
}

fn server(variable: &str) -> String {
    std::env::var(variable)
        .unwrap_or_else(|_| panic!("{variable} must name the owned SQL Server resolver fixture"))
}

async fn connection(url: &str) -> Conn {
    Conn::connect(Driver::Mssql, url).await.unwrap()
}

// Repeated acquisitions inside nested Docker fixtures otherwise copy their
// futures into a debug poll frame that exhausts the default test-thread stack.
fn refusal<'a>(target: &'a str, scratch: &'a str) -> impl Future<Output = ProduceError> + 'a {
    Box::pin(refusal_inner(target, scratch))
}

async fn refusal_inner(target: &str, scratch: &str) -> ProduceError {
    let mut target_session = connection(target).await;
    let identity = sql::cluster_identity(&mut target_session).await.unwrap();
    let mut key = ProjectKey::new(true);
    let config = "dialect: mssql\nenvironments:\n  fixture:\n    url_env: PBPS_2236_UNUSED\n    \
        fingerprint_key_file: key\n";
    let path = key.project.root.join("pbps.yml");
    std::fs::write(&path, config).unwrap();
    key.project.config = pbps_config::Config::parse(config, &path).unwrap();
    let schema = Schema::default();
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    match super::produce_unsealed(
        Driver::Mssql,
        scratch,
        target,
        &identity,
        &BindingRequest {
            bootstrap: &[],
            base: &schema,
            desired: &schema,
        },
        side,
        side,
        &Hints::default(),
        &[],
        &key.project,
        Some(ENVIRONMENT),
        Standard::Mssql,
        None,
        &Provenance::server(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("SQL Server must refuse at the #2237 externals boundary"),
    }
}

async fn create_database(url: &str) -> (String, String) {
    let name = format!("pbps2236_{:032x}", rand::random::<u128>());
    connection(url)
        .await
        .execute(&format!("CREATE DATABASE [{name}];"))
        .await
        .unwrap();
    let on = format!("{url};Database={name}");
    assert_eq!(
        sql::database(&mut connection(&on).await)
            .await
            .unwrap()
            .name,
        name
    );
    (name, on)
}

async fn drop_database(url: &str, name: &str) {
    connection(url)
        .await
        .execute(&format!(
            "USE master; ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
             DROP DATABASE [{name}];"
        ))
        .await
        .unwrap();
}

async fn supplied_contract(target: &str, server: &str) {
    let (name, on) = create_database(server).await;
    let login = format!("pbps2236_owner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(server).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; ALTER \
             AUTHORIZATION ON DATABASE::[{name}] TO [{login}];"
        ))
        .await
        .unwrap();
    let supplied = format!("{server};Database={name};User Id={login};Password={password}");
    let positive = refusal(target, &supplied).await;
    let clean = sql::inventory::read(&mut connection(&on).await, None)
        .await
        .unwrap();
    let (guest_name, guest) = create_database(server).await;
    connection(&guest)
        .await
        .execute("GRANT CONNECT TO guest;")
        .await
        .unwrap();
    let guest_refusal = refusal(target, &supplied).await.to_string();
    connection(&guest)
        .await
        .execute("REVOKE CONNECT FROM guest;")
        .await
        .unwrap();
    drop_database(server, &guest_name).await;
    admin
        .execute(&format!("DENY VIEW ANY DATABASE TO [{login}];"))
        .await
        .unwrap();
    let hidden = refusal(target, &supplied).await.to_string();
    admin
        .execute(&format!("REVOKE VIEW ANY DATABASE FROM [{login}];"))
        .await
        .unwrap();
    let mut dirty = connection(&on).await;
    let backup = format!("/tmp/{name}.bak");
    dirty
        .execute(&format!(
            "ALTER DATABASE [{name}] SET RECOVERY FULL; BACKUP DATABASE [{name}] TO \
             DISK=N'{backup}' WITH INIT;"
        ))
        .await
        .unwrap();
    dirty
        .execute(
            "CREATE TABLE dbo.log_work(i int,payload char(8000)); INSERT dbo.log_work SELECT \
             TOP(2000) ROW_NUMBER() OVER(ORDER BY(SELECT NULL)),'x' FROM sys.all_objects a \
             CROSS JOIN sys.all_objects b; DROP TABLE dbo.log_work; CHECKPOINT;",
        )
        .await
        .unwrap();
    let status = dirty
        .query("SELECT log_reuse_wait_desc FROM sys.databases WHERE database_id=DB_ID();")
        .await
        .unwrap();
    let full_recovery = refusal(target, &supplied).await;
    dirty
        .execute(&format!(
            "EXEC master.sys.xp_delete_files N'{backup}'; ALTER DATABASE [{name}] SET \
             AUTO_SHRINK ON;"
        ))
        .await
        .unwrap();
    let persistent = refusal(target, &supplied).await.to_string();
    dirty
        .execute(&format!("ALTER DATABASE [{name}] SET AUTO_SHRINK OFF;"))
        .await
        .unwrap();
    dirty
        .execute(
            "CREATE COLUMN MASTER KEY cmk WITH(KEY_STORE_PROVIDER_NAME=N'MSSQL_CERTIFICATE_STORE',\
             KEY_PATH=N'CurrentUser/My/pbps2236'); CREATE COLUMN ENCRYPTION KEY cek WITH \
             VALUES(COLUMN_MASTER_KEY=cmk,ALGORITHM=N'RSA_OAEP',ENCRYPTED_VALUE=0x01020304); EXEC \
             sys.sp_addextendedproperty @name=N'run_property',@value=N'dirty';",
        )
        .await
        .unwrap();
    let dirty_refusal = refusal(target, &supplied).await.to_string();
    let untouched = sql::inventory::read(&mut dirty, None)
        .await
        .unwrap()
        .names();
    sql::inventory::cleanup(&mut dirty, None).await.unwrap();
    let rechecked = sql::inventory::read(&mut dirty, None).await.unwrap();
    drop(dirty);
    let mut system_refusals = Vec::new();
    for (change, restore) in [
        (
            "USE msdb; GRANT CONTROL TO guest;",
            "USE msdb; REVOKE CONTROL FROM guest;",
        ),
        (
            "USE msdb; ALTER ROLE SQLAgentOperatorRole ADD MEMBER guest;",
            "USE msdb; ALTER ROLE SQLAgentOperatorRole DROP MEMBER guest;",
        ),
        (
            "USE msdb; EXEC(N'CREATE SCHEMA pbps2236_guest_owned AUTHORIZATION guest');",
            "USE msdb; DROP SCHEMA pbps2236_guest_owned;",
        ),
    ] {
        admin.execute(change).await.unwrap();
        let refusal = refusal(target, &supplied).await.to_string();
        let restored = admin.execute(restore).await;
        restored.unwrap();
        system_refusals.push(refusal);
    }
    drop_database(server, &name).await;
    admin
        .execute(&format!("USE master; DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert!(
        matches!(
            positive,
            ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2237")
        ),
        "{positive}"
    );
    assert!(clean.is_empty(), "{:?}", clean.names());
    assert!(guest_refusal.contains(&guest_name), "{guest_refusal}");
    assert!(hidden.contains("VIEW ANY DATABASE"), "{hidden}");
    assert_eq!(
        status[0].try_get::<&str>("log_reuse_wait_desc").unwrap(),
        Some("LOG_BACKUP")
    );
    assert!(
        matches!(
            full_recovery,
            ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2237")
        ),
        "{full_recovery}"
    );
    assert!(persistent.contains("is_auto_shrink_on"), "{persistent}");
    assert!(
        dirty_refusal.contains("COLUMN MASTER KEY")
            && dirty_refusal.contains("COLUMN ENCRYPTION KEY")
            && dirty_refusal.contains("extended property"),
        "{dirty_refusal}"
    );
    for kind in [
        "COLUMN MASTER KEY",
        "COLUMN ENCRYPTION KEY",
        "extended property",
    ] {
        assert!(
            untouched.iter().any(|name| name.contains(kind)),
            "{untouched:?}"
        );
    }
    assert!(rechecked.is_empty(), "{:?}", rechecked.names());
    for refusal in system_refusals {
        assert!(refusal.contains("msdb"), "{refusal}");
    }
}

async fn inherited_model_refusal(target: &str, server: &str) {
    let name = format!("pbps2236_inherited_{:032x}", rand::random::<u128>());
    let mut admin = connection(server).await;
    admin
        .execute(&format!(
            "EXEC(N'USE model; CREATE TABLE dbo.{name}(i int);');"
        ))
        .await
        .unwrap();
    let target = target.to_owned();
    let source = server.to_owned();
    let result = tokio::spawn(async move { refusal(&target, &source).await }).await;
    let restored = admin
        .execute(&format!("EXEC(N'USE model; DROP TABLE dbo.{name};');"))
        .await;
    restored.unwrap();
    let row = admin
        .query(&format!(
            "SELECT OBJECT_ID(N'model.dbo.{name}') AS remaining;"
        ))
        .await
        .unwrap();
    assert_eq!(row[0].try_get::<i32>("remaining").unwrap(), None);
    let refusal = result.unwrap().to_string();
    assert!(refusal.contains(&name), "{refusal}");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_owner_needs_no_creation_and_changed_visibility_or_system_context_refuses() {
    let primary = server("PBPS_TEST_DB");
    supplied_contract(&primary, &primary).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_docker_supplied_inventory_and_sandbox_use_the_same_contract() {
    let target = server("PBPS_TEST_DB");
    let image = server("PBPS_TEST_RESOLVER_MSSQL_IMAGE");
    let identity = sql::cluster_identity(&mut connection(&target).await)
        .await
        .unwrap();
    let target_for_run = target.clone();
    super::container::with_container(
        Driver::Mssql,
        &target,
        &identity,
        &image,
        pbps_config::resolver::PullPolicy::Never,
        async move |url: &str| {
            let same=refusal(url,url).await;
            assert!(matches!(
            same,
            ProduceError::Run(Error::Unusable(ref reason)) if reason.contains("target database")
        ),"{same}");
            let owned=refusal(&target_for_run,url).await;
            assert!(matches!(
            owned,
            ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2237")
        ),"{owned}");
            supplied_contract(&target_for_run, url).await;
            inherited_model_refusal(&target_for_run,url).await;
            Ok(())
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_run_owned_acquisition_needs_only_login_and_database_creation_privileges() {
    let target = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let login = format!("pbps2236_provisioner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; GRANT ALTER \
             ANY LOGIN TO [{login}]; GRANT CREATE ANY DATABASE TO [{login}];"
        ))
        .await
        .unwrap();
    let source = format!("{scratch};Database=master;User Id={login};Password={password}");
    let outcome = refusal(&target, &source).await;
    let remaining = admin
        .query(&format!(
            "SELECT name FROM sys.databases WHERE owner_sid=SUSER_SID(N'{login}');"
        ))
        .await
        .unwrap();
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert!(
        matches!(outcome,ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2237")),
        "{outcome}"
    );
    assert!(remaining.is_empty());
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_cleanup_names_the_retained_sandbox_after_failure() {
    let primary = server("PBPS_TEST_DB");
    let schema = Schema::default();
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let request =
        super::super::producer::request(Driver::Mssql, side, side, &Hints::default(), &[]).unwrap();
    for missing_marker in [false, true] {
        let (database, on) = create_database(&primary).await;
        let outcome: Result<_, pbps_db::DbError> = async {
            let mut target = Conn::connect(Driver::Mssql, &primary).await?;
            let mut admin = Conn::connect(Driver::Mssql, &on).await?;
            let identity = sql::cluster_identity(&mut target).await?;
            let tokens = super::Tokens::generate();
            let provenance = Provenance::server();
            let (provisioning, mut state) = super::engine::scratch(
                Driver::Mssql,
                &mut target,
                &mut admin,
                &tokens,
                &identity,
                &on,
                &request,
                &Standard::Mssql,
                None,
                &provenance,
            )
            .await
            .map_err(|error| pbps_db::DbError::Refused(error.to_string()))?;
            let (catalog, authorization) =
                crate::resolver::scope::read_target(&mut target, Driver::Mssql, &[], &[], &[], &[])
                    .await?;
            let prepared = state
                .prepare(
                    provisioning,
                    &mut admin,
                    &mut target,
                    &catalog,
                    &authorization,
                    &[],
                    &BindingRequest {
                        bootstrap: &[],
                        base: &schema,
                        desired: &schema,
                    },
                    &[],
                )
                .await
                .err();
            let rows = admin
                .query(
                    "SELECT name FROM sys.database_principals \
                     WHERE name LIKE N'pbps[_]sandbox[_]%';",
                )
                .await?;
            let sandbox = rows
                .first()
                .ok_or_else(|| pbps_db::DbError::BadRow("no sandbox was created".into()))?
                .try_get::<&str>("name")?
                .ok_or_else(|| pbps_db::DbError::BadRow("sandbox name is NULL".into()))?
                .to_owned();
            if missing_marker {
                admin
                    .execute(&format!("DROP TABLE [##pbps_{}];", tokens.scratch))
                    .await?;
            } else {
                // An index property has no cleanup rule. The privileged final
                // drop must be skipped, and its retained user must be named.
                admin
                    .execute(
                        "CREATE TABLE dbo.cleanup_refusal(i int); \
                         CREATE INDEX ix ON dbo.cleanup_refusal(i); \
                         EXEC sys.sp_addextendedproperty @name=N'cleanup_property',@value=N'held',\
                         @level0type=N'SCHEMA',@level0name=N'dbo',@level1type=N'TABLE',\
                         @level1name=N'cleanup_refusal',@level2type=N'INDEX',@level2name=N'ix';",
                    )
                    .await?;
            }
            let cleanup = state.release(&mut admin).await;
            let retained = admin
                .query(&format!(
                    "SELECT IS_ROLEMEMBER(N'db_owner',name) AS membership \
                     FROM sys.database_principals WHERE name=N'{sandbox}';"
                ))
                .await?;
            Ok((prepared, cleanup, sandbox, retained))
        }
        .await;
        // Restore before assertions, including when the regression is tested
        // with the reporting fix reverted.
        drop_database(&primary, &database).await;
        let (prepared, cleanup, sandbox, retained) = outcome.unwrap();
        assert!(matches!(prepared, Some(Error::Binding(reason)) if reason.contains("#2237")));
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].try_get::<i32>("membership").unwrap(), Some(1));
        let failed = cleanup.unwrap_err();
        let expected = format!("sandbox user {sandbox} (db_owner) in database {database}");
        assert_eq!(
            failed.iter().filter(|name| **name == expected).count(),
            1,
            "{failed:?}"
        );
        let original = if missing_marker {
            "no longer reaches the marked database"
        } else {
            "extended property class 7 cleanup_property"
        };
        assert!(
            failed.iter().any(|name| name.contains(original)),
            "{failed:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_acquisition_refuses_target_and_cleans_after_the_externals_refusal() {
    let primary = server("PBPS_TEST_DB");
    let (name, on) = create_database(&primary).await;
    let target = refusal(&on, &on).await;
    let boundary = refusal(&primary, &on).await;
    let empty = sql::inventory::read(&mut connection(&on).await, None)
        .await
        .unwrap();
    drop_database(&primary, &name).await;
    assert!(
        matches!(
            target,
            ProduceError::Run(Error::Unusable(ref reason)) if reason.contains("target database")
        ),
        "{target}"
    );
    assert!(
        matches!(
            boundary,
            ProduceError::Run(Error::Binding(ref reason))
                if reason == "SQL Server resolver externals are not implemented yet (#2237)"
        ),
        "{boundary}"
    );
    assert!(empty.is_empty(), "{:?}", empty.names());
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn instance_wide_ownership_chaining_refuses_the_scratch_sandbox_and_cleans_setup() {
    let primary = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, supplied) = create_database(&scratch).await;
    let mut admin = connection(&scratch).await;
    let option =
        |value| format!("EXEC sp_configure 'cross db ownership chaining', {value}; RECONFIGURE;");
    // This group runs serially on its dedicated instances. Joining a task
    // catches a panic in acquisition too, so restoration precedes assertions.
    let measured = tokio::spawn(async move {
        let inventory = async || {
            connection(&scratch)
                .await
                .query(
                    "SELECT N'database:' + name AS name FROM sys.databases UNION ALL \
                     SELECT N'login:' + name FROM sys.server_principals ORDER BY name;",
                )
                .await
                .unwrap()
                .iter()
                .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let before = inventory().await;
        connection(&scratch)
            .await
            .execute(&option(1))
            .await
            .unwrap();
        // A separate target chooses run-owned provisioning; a target on the
        // scratch instance forces the supplied database path, even as sa.
        let mut refused = Vec::new();
        for (target, source) in [(&primary, &scratch), (&scratch, &supplied)] {
            refused.push(refusal(target, source).await);
        }
        let after_refusal = inventory().await;
        let supplied_after_refusal = sql::inventory::read(&mut connection(&supplied).await, None)
            .await
            .unwrap();
        connection(&scratch)
            .await
            .execute(&option(0))
            .await
            .unwrap();
        let mut allowed = Vec::new();
        for (target, source) in [(&primary, &scratch), (&scratch, &supplied)] {
            allowed.push(refusal(target, source).await);
        }
        let after_control = inventory().await;
        let supplied_after_control = sql::inventory::read(&mut connection(&supplied).await, None)
            .await
            .unwrap();
        (
            before,
            after_refusal,
            after_control,
            supplied_after_refusal,
            supplied_after_control,
            refused,
            allowed,
        )
    })
    .await;
    let restored = admin.execute(&option(0)).await;
    let setting = admin
        .query(
            "SELECT CONVERT(int,value_in_use) AS value_in_use FROM sys.configurations \
             WHERE name=N'cross db ownership chaining';",
        )
        .await;
    drop_database(&server("PBPS_TEST_MSSQL_SCRATCH_DB"), &name).await;
    restored.unwrap();
    assert_eq!(
        setting.unwrap()[0].try_get::<i32>("value_in_use").unwrap(),
        Some(0)
    );
    let (
        before,
        after_refusal,
        after_control,
        supplied_refused,
        supplied_control,
        refused,
        allowed,
    ) = measured.unwrap();
    assert_eq!(
        after_refusal, before,
        "refused setup left a database or login"
    );
    assert_eq!(
        after_control, before,
        "control setup left a database or login"
    );
    for inventory in [supplied_refused, supplied_control] {
        assert!(inventory.is_empty(), "{:?}", inventory.names());
    }
    for outcome in refused {
        assert!(
            matches!(outcome, ProduceError::Run(Error::Read(_))),
            "{outcome}"
        );
        let message = outcome.to_string();
        assert!(message.contains("cross db ownership chaining"), "{message}");
        assert!(
            message.contains("EXEC sp_configure 'cross db ownership chaining', 0; RECONFIGURE;"),
            "{message}"
        );
    }
    for outcome in allowed {
        assert!(
            matches!(outcome, ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2237")),
            "{outcome}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_run_owned_acquisition_removes_its_login_database_and_reference_on_refusal() {
    let primary = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let inventory = async || {
        let rows = connection(&scratch)
            .await
            .query(
                "SELECT name FROM sys.databases WHERE name LIKE N'pbps_scratch_%' UNION ALL \
                 SELECT name FROM sys.server_principals WHERE name LIKE N'pbps_run_%' ORDER \
                 BY name;",
            )
            .await
            .unwrap();
        rows.iter()
            .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let before = inventory().await;
    let outcome = refusal(&primary, &scratch).await;
    let after = inventory().await;
    inherited_model_refusal(&primary, &scratch).await;
    assert!(
        matches!(outcome,ProduceError::Run(Error::Binding(ref reason)) if reason.contains("#2237")),
        "{outcome}"
    );
    assert_eq!(after, before);
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_guest_admission_dirty_inventory_and_persistent_options_refuse_by_name() {
    let primary = server("PBPS_TEST_DB");
    let (scratch_name, scratch) = create_database(&primary).await;
    let (guest_name, guest) = create_database(&primary).await;
    connection(&guest)
        .await
        .execute("GRANT CONNECT TO guest;")
        .await
        .unwrap();
    let guest_refusal = refusal(&primary, &scratch).await.to_string();
    connection(&guest)
        .await
        .execute("REVOKE CONNECT FROM guest;")
        .await
        .unwrap();
    connection(&scratch)
        .await
        .execute("CREATE TABLE dbo.left_behind(i int);")
        .await
        .unwrap();
    let dirty = refusal(&primary, &scratch).await.to_string();
    connection(&scratch)
        .await
        .execute("DROP TABLE dbo.left_behind;")
        .await
        .unwrap();
    connection(&scratch)
        .await
        .execute(&format!(
            "ALTER DATABASE [{scratch_name}] SET AUTO_SHRINK ON;"
        ))
        .await
        .unwrap();
    let options = refusal(&primary, &scratch).await.to_string();
    drop_database(&primary, &scratch_name).await;
    drop_database(&primary, &guest_name).await;
    assert!(guest_refusal.contains(&guest_name), "{guest_refusal}");
    assert!(dirty.contains("left_behind"), "{dirty}");
    assert!(options.contains("is_auto_shrink_on"), "{options}");
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_docker_removes_the_container_on_success_failure_and_cancellation() {
    let target = server("PBPS_TEST_DB");
    let image = server("PBPS_TEST_RESOLVER_MSSQL_IMAGE");
    let identity = sql::cluster_identity(&mut connection(&target).await)
        .await
        .unwrap();
    for fails in [false, true] {
        let name = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let observed = name.clone();
        let result = super::container::with_container(
            Driver::Mssql,
            &target,
            &identity,
            &image,
            pbps_config::resolver::PullPolicy::Never,
            async move |url: &str| {
                let mut conn = connection(url).await;
                let rows = conn
                    .query("SELECT CONVERT(nvarchar(128),SERVERPROPERTY('ServerName')) AS name;")
                    .await
                    .unwrap();
                let server = rows[0].try_get::<&str>("name").unwrap().unwrap();
                // Docker's default hostname is the container ID prefix.
                *observed.lock().await = server.to_owned();
                if fails {
                    Err(ProduceError::Run(Error::Binding(
                        "intentional fixture refusal".into(),
                    )))
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(result.is_err(), fails);
        let id = name.lock().await.clone();
        let inspect = tokio::process::Command::new("docker")
            .args(["inspect", &id])
            .output()
            .await
            .unwrap();
        assert!(!inspect.status.success(), "container {id} remained");
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        super::container::with_container(
            Driver::Mssql,
            &target,
            &identity,
            &image,
            pbps_config::resolver::PullPolicy::Never,
            async move |url: &str| {
                let rows = connection(url)
                    .await
                    .query("SELECT CONVERT(nvarchar(128),SERVERPROPERTY('ServerName')) AS name;")
                    .await
                    .unwrap();
                sender
                    .send(rows[0].try_get::<&str>("name").unwrap().unwrap().to_owned())
                    .unwrap();
                std::future::pending::<Result<(), ProduceError>>().await
            },
        )
        .await
    });
    let id = receiver.await.unwrap();
    task.abort();
    let _ = task.await;
    let mut removed = false;
    for _ in 0..100 {
        let inspect = tokio::process::Command::new("docker")
            .args(["inspect", &id])
            .output()
            .await
            .unwrap();
        if !inspect.status.success() {
            removed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(removed, "cancelled container {id} remained");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_independent_catalogs_refuse_untouched_and_cleanup_restores_fresh() {
    let target = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, on) = create_database(&scratch).await;
    let cases = catalog_cases::cases();
    let login = format!("pbps2236_owner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; \
         ALTER AUTHORIZATION ON DATABASE::[{name}] TO [{login}];",
        ))
        .await
        .unwrap();
    let supplied = format!("{on};User Id={login};Password={password}");
    let mut conn = connection(&supplied).await;
    let result: Result<(), String> = async {
        if sql::provisions(&mut conn)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("the fixture owner unexpectedly has instance creation privileges".into());
        }
        for (expected, create) in cases.restore {
            conn.execute(&create)
                .await
                .map_err(|error| error.to_string())?;
            let refused = refusal(&target, &supplied).await.to_string();
            let untouched = sql::inventory::read(&mut conn, None)
                .await
                .map_err(|error| error.to_string())?
                .names();
            let cleanup = sql::inventory::cleanup(&mut conn, None).await;
            let fresh = sql::inventory::read(&mut conn, None)
                .await
                .map_err(|error| error.to_string())?;
            if !refused.contains(&expected)
                || !untouched.iter().any(|item| item.contains(&expected))
                || cleanup.is_err()
                || !fresh.is_empty()
            {
                return Err(format!(
                    "{expected}: refusal={refused}; untouched={untouched:?}; \
                     cleanup={cleanup:?}; remaining={:?}",
                    fresh.names(),
                ));
            }
        }
        Ok(())
    }
    .await;
    drop(conn);
    drop_database(&scratch, &name).await;
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_built_in_refusals_preserve_state_and_residuals_reach_rehearsal() {
    let target = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let cases = catalog_cases::cases();
    let login = format!("pbps2263_owner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON;"
        ))
        .await
        .unwrap();
    let mut results = Vec::new();
    for (refuses, (expected, create)) in cases
        .refuse
        .into_iter()
        .map(|case| (true, case))
        .chain(cases.residual.into_iter().map(|case| (false, case)))
    {
        let (name, on) = create_database(&scratch).await;
        admin
            .execute(&format!(
                "ALTER AUTHORIZATION ON DATABASE::[{name}] TO [{login}];"
            ))
            .await
            .unwrap();
        let supplied = format!("{on};User Id={login};Password={password}");
        let mut conn = connection(&supplied).await;
        let result: Result<(), String> = async {
            conn.execute(&create).await.map_err(|error| error.to_string())?;
            let before = conn.query(catalog_cases::BUILTIN_STATE).await
                .map_err(|error| error.to_string())?;
            let before = before[0].try_get::<&str>("state").unwrap().unwrap().to_owned();
            let refused = refusal(&target, &supplied).await.to_string();
            let names = sql::inventory::read(&mut conn, None).await
                .map_err(|error| error.to_string())?.names();
            let cleanup = sql::inventory::cleanup(&mut conn, None).await;
            let remaining = sql::inventory::read(&mut conn, None).await
                .map_err(|error| error.to_string())?.names();
            let after = conn.query(catalog_cases::BUILTIN_STATE).await
                .map_err(|error| error.to_string())?;
            let after = after[0].try_get::<&str>("state").unwrap().unwrap();
            let valid = if refuses {
                refused.contains(&expected)
                    && names.iter().any(|name| name.contains(&expected))
                    && cleanup.as_ref().is_err_and(|errors| errors.iter().any(|error|
                        error.contains(&expected) && error.contains(
                            "this database cannot be restored to a fresh state; supply a new scratch database"
                        )))
                    && remaining == names && before == after
            } else {
                refused.contains("#2237") && names.is_empty() && cleanup.is_ok()
                    && remaining.is_empty() && before == after
            };
            if !valid {
                return Err(format!("{expected}: refusal={refused}; names={names:?}; cleanup={cleanup:?}; remaining={remaining:?}; unchanged={}", before == after));
            }
            Ok(())
        }.await;
        drop(conn);
        // An owned database is the only measured way to discard changed sys
        // ownership; perform it before asserting and keep the login until last.
        drop_database(&scratch, &name).await;
        let absent = admin
            .query(&format!(
                "SELECT name FROM sys.databases WHERE name=N'{name}';"
            ))
            .await
            .unwrap()
            .is_empty();
        results.push((name, absent, result));
    }
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    for (name, absent, result) in results {
        assert!(absent, "fixture database {name} remained");
        result.unwrap();
    }
}
