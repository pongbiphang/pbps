//! Scratch acquisition measurements and regressions for DEC-2236.1.

use super::{TestDb, connect_live};
use pbps_mssql::resolver::scratch::{self, inventory, options};

#[path = "../support/scratch_catalogs.rs"]
mod catalog_cases;

async fn model_row(conn: &mut pbps_db::Conn) -> serde_json::Value {
    let rows = conn
        .query(
            "SELECT (SELECT * FROM sys.databases WHERE name=N'model' FOR JSON PATH,\
             INCLUDE_NULL_VALUES,WITHOUT_ARRAY_WRAPPER) AS model;",
        )
        .await
        .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_str(rows[0].try_get::<&str>("model").unwrap().unwrap()).unwrap();
    for name in ["log_reuse_wait", "log_reuse_wait_desc"] {
        value.as_object_mut().unwrap().remove(name);
    }
    value
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn model_inheritance_is_measured_and_read_only_model_refuses_with_its_remedy() {
    let server = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB")
        .expect("PBPS_TEST_MSSQL_SCRATCH_DB must name the dedicated resolver instance");
    let mut conn = connect_live(&server).await.unwrap();
    let columns = conn
        .query(
            "SELECT name FROM sys.all_columns WHERE object_id=OBJECT_ID(N'sys.databases') \
             ORDER BY column_id;",
        )
        .await
        .unwrap();
    let columns: Vec<_> = columns
        .iter()
        .map(|row| row.try_get::<&str>("name").unwrap().unwrap())
        .collect();
    assert_eq!(
        columns,
        include_str!("scratch2236-database-columns.txt")
            .lines()
            .collect::<Vec<_>>(),
        "a new database column needs a measured inheritance rule"
    );
    let original = model_row(&mut conn).await;
    assert_eq!(original["is_read_only"], false);
    assert_eq!(original["snapshot_isolation_state"], 0);
    let cases: Vec<(String, String, String)> =
        serde_json::from_str(include_str!("scratch2236-model-options.json")).unwrap();
    let name = format!("pbps2236_model_measure_{}", std::process::id());
    for (column, change, restore) in cases {
        let mut changed = false;
        // A case states one direction. Where model already holds the changed
        // value (LocalDB's model is SIMPLE recovery, a container's is FULL),
        // the other direction is the change and the stated one restores it.
        let mut swapped = false;
        let outcome: Result<_, pbps_db::DbError> = async {
            conn.execute(&change).await?;
            changed = true;
            if model_row(&mut conn).await == original {
                conn.execute(&restore).await?;
                swapped = true;
            }
            let reference = options::supplied_reference(&mut conn).await;
            conn.execute(&format!("CREATE DATABASE [{name}];")).await?;
            let actual = options::read(&mut conn, &name).await?;
            Ok((reference, actual))
        }
        .await;
        // Restore before inspecting/asserting any result. A failed creation
        // or comparison cannot strand model in the measured state.
        let drop = conn
            .execute(&format!(
                "IF DB_ID(N'{name}') IS NOT NULL DROP DATABASE [{name}];"
            ))
            .await;
        let restoration = if changed {
            conn.execute(if swapped { &change } else { &restore }).await
        } else {
            Ok(())
        };
        drop.unwrap();
        restoration.unwrap();
        assert_eq!(
            model_row(&mut conn).await,
            original,
            "model restoration after {column}"
        );
        match outcome {
            Ok((Err(error), _)) if column == "is_read_only" => {
                assert!(error.to_string().contains("READ_ONLY"), "{error}");
                assert!(
                    error
                        .to_string()
                        .contains("ALTER DATABASE model SET READ_WRITE"),
                    "{error}"
                );
            }
            Ok((reference, actual)) => assert!(
                options::differences(&actual, &reference.unwrap()).is_empty(),
                "{column}: {actual:?}"
            ),
            Err(error) => {
                let expected = match column.as_str() {
                    "is_trustworthy_on" => "15309",
                    "is_db_chaining_on" => "5600",
                    "is_broker_enabled"
                    | "is_honor_broker_priority_on"
                    | "is_mixed_page_allocation_on" => "5058",
                    "is_accelerated_database_recovery_on" | "is_optimized_locking_on" => "12122",
                    "is_data_retention_enabled" => "102",
                    "is_fulltext_enabled" => "9966",
                    _ => panic!("{column}: {error}"),
                };
                assert_eq!(
                    error.server_error_code().as_deref(),
                    Some(expected),
                    "{column}: {error}"
                );
            }
        }
    }
    assert!(options::supplied_reference(&mut conn).await.is_ok());
}

/// Express (and LocalDB, which is Express) creates every database with
/// AUTO_CLOSE ON whatever model says, so a supplied scratch's reference must
/// predict it; a prediction from model alone refuses every fresh database.
#[tokio::test]
#[ignore = "needs a second live SQL Server; set PBPS_TEST_EXPRESS_DB (see scripts/live-tests.sh)"]
async fn an_express_supplied_reference_predicts_a_fresh_database_exactly() {
    let express = std::env::var("PBPS_TEST_EXPRESS_DB").expect(
        "PBPS_TEST_EXPRESS_DB is not set; this test needs the Express server scripts/live-tests.sh starts",
    );
    let mut conn = connect_live(&express).await.unwrap();
    let name = format!("pbps2377_express_{}", std::process::id());
    let reference = options::supplied_reference(&mut conn).await.unwrap();
    conn.execute(&format!("CREATE DATABASE [{name}];"))
        .await
        .unwrap();
    // Acquisition reads the options from inside the database. Idle, Express
    // closes it and reports it cleanly shut down (measured), which describes
    // that moment, not the database.
    let inside = connect_live(&format!("{express};Database={name}")).await;
    let actual = options::read(&mut conn, &name).await;
    drop(inside);
    conn.execute(&format!(
        "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
    ))
    .await
    .unwrap();
    let actual = actual.unwrap();
    assert_eq!(
        options::differences(&actual, &reference),
        Vec::<String>::new()
    );
    // The edition rule is what made it match: model itself says OFF.
    assert_eq!(model_row(&mut conn).await["is_auto_close_on"], false);
}

/// The run-owned reference is created and read on one connection. Express
/// can close it in between (CI run 40219619692 refused its own fresh scratch
/// this way), so the read must not depend on how fast that happens: this
/// waits until the database is closed and still expects the open values.
#[tokio::test]
#[ignore = "needs a second live SQL Server; set PBPS_TEST_EXPRESS_DB (see scripts/live-tests.sh)"]
async fn an_express_database_closed_before_its_read_still_reads_as_open() {
    let express = std::env::var("PBPS_TEST_EXPRESS_DB").expect(
        "PBPS_TEST_EXPRESS_DB is not set; this test needs the Express server scripts/live-tests.sh starts",
    );
    let mut conn = connect_live(&express).await.unwrap();
    let closed = format!("pbps2377_closed_{}", std::process::id());
    let open = format!("pbps2377_open_{}", std::process::id());
    conn.execute(&format!(
        "CREATE DATABASE [{closed}]; CREATE DATABASE [{open}];"
    ))
    .await
    .unwrap();
    let inside = connect_live(&format!("{express};Database={open}")).await;
    let shut = format!(
        "SELECT CONVERT(int, is_cleanly_shutdown) AS shut FROM sys.databases WHERE name = N'{closed}';"
    );
    let mut waited = 0;
    while waited < 120
        && conn.query(&shut).await.unwrap()[0]
            .try_get::<i32>("shut")
            .unwrap()
            != Some(1)
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        waited += 1;
    }
    let outside = options::read(&mut conn, &closed).await;
    let read = options::read_inside(&mut conn, &closed).await;
    let expected = options::read(&mut conn, &open).await;
    let home = conn.query("SELECT DB_NAME() AS name;").await.unwrap()[0]
        .try_get::<&str>("name")
        .unwrap()
        .map(str::to_owned);
    drop(inside);
    for name in [&closed, &open] {
        conn.execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .unwrap();
    }
    assert!(waited < 120, "the idle database never closed");
    let (outside, read, expected) = (outside.unwrap(), read.unwrap(), expected.unwrap());
    // Negative: from outside, the closed row is what refused the scratch.
    assert!(
        options::differences(&outside, &expected).contains(&"is_cleanly_shutdown".to_owned()),
        "{:?}",
        options::differences(&outside, &expected)
    );
    assert_eq!(options::differences(&read, &expected), Vec::<String>::new());
    // The session is back where it started, so the drop above could run.
    assert_eq!(home.as_deref(), Some("master"));
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn scratch_inventory_reads_every_catalog_and_cleans_contained_dependencies() {
    let mut db = TestDb::create("inventory2236").await;
    let result = async {
        let fresh = inventory::read(&mut db.conn, None).await.unwrap();
        assert!(fresh.is_empty(), "{:?}", fresh.names());
        for sql in [
            "CREATE ROLE holder;",
            "CREATE USER member WITHOUT LOGIN; ALTER ROLE holder ADD MEMBER member;",
            "CREATE USER z_owner WITHOUT LOGIN; CREATE ROLE a_owner AUTHORIZATION z_owner; \
             CREATE ROLE z_child AUTHORIZATION a_owner;",
            "CREATE SCHEMA held AUTHORIZATION holder;",
            "CREATE TABLE held.t(i int PRIMARY KEY);",
            "CREATE VIEW held.v WITH SCHEMABINDING AS SELECT i FROM held.t;",
            "CREATE PARTITION FUNCTION pf(int) AS RANGE LEFT FOR VALUES(1);",
            "CREATE PARTITION SCHEME ps AS PARTITION pf ALL TO([PRIMARY]);",
            "CREATE COLUMN MASTER KEY cmk \
             WITH(KEY_STORE_PROVIDER_NAME=N'MSSQL_CERTIFICATE_STORE',\
             KEY_PATH=N'CurrentUser/My/pbps2236');",
            "CREATE COLUMN ENCRYPTION KEY cek WITH VALUES(COLUMN_MASTER_KEY=cmk,\
             ALGORITHM=N'RSA_OAEP',ENCRYPTED_VALUE=0x01020304);",
            "EXEC sys.sp_addextendedproperty @name=N'run_property',@value=N'left behind';",
        ] {
            db.conn.execute(sql).await.unwrap();
        }
        let dirty = inventory::read(&mut db.conn, None).await.unwrap().names();
        assert!(
            dirty.iter().any(|name| name.contains("COLUMN MASTER KEY")),
            "{dirty:?}"
        );
        assert!(
            dirty
                .iter()
                .any(|name| name.contains("COLUMN ENCRYPTION KEY")),
            "{dirty:?}"
        );
        assert!(
            dirty.iter().any(|name| name.contains("extended property")),
            "{dirty:?}"
        );
        inventory::cleanup(&mut db.conn, None).await.unwrap();
        assert!(
            inventory::read(&mut db.conn, None)
                .await
                .unwrap()
                .is_empty()
        );
        db.conn.execute("CREATE SCHEMA held;").await.unwrap();
        db.conn
            .execute("CREATE TABLE held.t(i int); CREATE ROLE holder;")
            .await
            .unwrap();
        inventory::cleanup(&mut db.conn, None).await.unwrap();
        db.conn
            .execute(
                "CREATE TABLE dbo.unknown_property(i int); CREATE INDEX ix ON \
                 dbo.unknown_property(i);",
            )
            .await
            .unwrap();
        db.conn
            .execute(
                "EXEC sys.sp_addextendedproperty @name=N'index_property',@value=N'visible',\
                 @level0type=N'SCHEMA',@level0name=N'dbo',@level1type=N'TABLE',\
                 @level1name=N'unknown_property',@level2type=N'INDEX',@level2name=N'ix';",
            )
            .await
            .unwrap();
        let unknown = inventory::read(&mut db.conn, None).await.unwrap();
        assert!(
            unknown
                .names()
                .iter()
                .any(|name| name.contains("extended property class 7"))
        );
        let cleanup = inventory::cleanup(&mut db.conn, None).await.unwrap_err();
        assert!(
            cleanup
                .iter()
                .any(|name| name.contains("extended property class 7")
                    && name.contains("no cleanup rule")),
            "{cleanup:?}"
        );
        db.conn
            .execute("DENY SELECT ON sys.columns TO public;")
            .await
            .unwrap();
        db.conn
            .execute(
                "CREATE USER denied_reader WITHOUT LOGIN; ALTER ROLE db_owner ADD MEMBER \
                 denied_reader; EXECUTE AS USER=N'denied_reader';",
            )
            .await
            .unwrap();
        let refused = inventory::read(&mut db.conn, None)
            .await
            .unwrap_err()
            .to_string();
        db.conn
            .execute("REVERT; REVOKE SELECT ON sys.columns FROM public; DROP USER denied_reader;")
            .await
            .unwrap();
        assert!(refused.contains("columns"), "{refused}");
        assert!(
            inventory::read(&mut db.conn, None)
                .await
                .unwrap()
                .is_empty()
        );
    }
    .await;
    db.drop().await;
    result
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn supplied_options_compare_every_column_and_allow_log_backup_status() {
    let mut db = TestDb::create("options2236").await;
    let reference = options::supplied_reference(&mut db.conn).await.unwrap();
    let actual = options::read(&mut db.conn, &db.name).await.unwrap();
    assert!(
        options::differences(&actual, &reference).is_empty(),
        "{:?}",
        options::differences(&actual, &reference)
    );
    // The server's own data directory exists and is writable on every
    // platform; `/tmp` does not exist on Windows (LocalDB).
    let directory = db
        .conn
        .query("SELECT CONVERT(nvarchar(4000), SERVERPROPERTY('InstanceDefaultDataPath')) AS d;")
        .await
        .unwrap()[0]
        .try_get::<&str>("d")
        .unwrap()
        .unwrap()
        .to_owned();
    let backup = format!("{directory}{}.bak", db.name);
    // A FULL-recovery model makes the database's own FULL a no-op. Where model
    // is SIMPLE (LocalDB), the recovery columns are the expected difference.
    let model = db
        .conn
        .query("SELECT recovery_model_desc AS r FROM sys.databases WHERE name = N'model';")
        .await
        .unwrap()[0]
        .try_get::<&str>("r")
        .unwrap()
        .unwrap()
        .to_owned();
    let recovery: Vec<String> = if model == "FULL" {
        Vec::new()
    } else {
        vec!["recovery_model".into(), "recovery_model_desc".into()]
    };
    db.conn
        .execute(&format!(
            "ALTER DATABASE [{}] SET RECOVERY FULL; BACKUP DATABASE [{}] TO DISK=N'{backup}' \
             WITH INIT;",
            db.name, db.name
        ))
        .await
        .unwrap();
    db.conn
        .execute(
            "CREATE TABLE dbo.log_work(i int,payload char(8000)); INSERT dbo.log_work SELECT \
             TOP(2000) ROW_NUMBER() OVER(ORDER BY(SELECT NULL)),'x' FROM sys.all_objects a \
             CROSS JOIN sys.all_objects b; DROP TABLE dbo.log_work; CHECKPOINT;",
        )
        .await
        .unwrap();
    let state = db
        .conn
        .query("SELECT log_reuse_wait_desc FROM sys.databases WHERE database_id=DB_ID();")
        .await
        .unwrap();
    let waiting = options::read(&mut db.conn, &db.name).await.unwrap();
    db.conn
        .execute(&format!("EXEC master.sys.xp_delete_files N'{backup}';"))
        .await
        .unwrap();
    assert_eq!(
        state[0].try_get::<&str>("log_reuse_wait_desc").unwrap(),
        Some("LOG_BACKUP")
    );
    assert_eq!(options::differences(&waiting, &reference), recovery);
    db.conn
        .execute(&format!("ALTER DATABASE [{}] SET AUTO_SHRINK ON;", db.name))
        .await
        .unwrap();
    let changed = options::read(&mut db.conn, &db.name).await.unwrap();
    let mut shrink = vec!["is_auto_shrink_on".to_owned()];
    shrink.extend(recovery);
    assert_eq!(options::differences(&changed, &reference), shrink);
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn sandbox_checks_all_database_names_and_the_complete_system_contract() {
    let mut db = TestDb::create("sandbox2236").await;
    let databases = scratch::database_names(&mut db.conn).await.unwrap();
    scratch::create_sandbox(&mut db.conn, "sandbox2236")
        .await
        .unwrap();
    let mut sandbox = connect_live(&super::conn_str()).await.unwrap();
    scratch::use_database(&mut sandbox, &db.name).await.unwrap();
    scratch::enter_sandbox(&mut sandbox, "sandbox2236")
        .await
        .unwrap();
    scratch::sandbox_checks(&mut sandbox, &databases, &db.name)
        .await
        .unwrap();
    sandbox
        .execute("CREATE ROLE owned_role AUTHORIZATION sandbox2236;")
        .await
        .unwrap();
    sandbox
        .execute("CREATE SCHEMA owned_schema AUTHORIZATION sandbox2236;")
        .await
        .unwrap();
    assert!(sandbox.execute("REVERT;").await.is_err());
    assert!(
        sandbox
            .execute("CREATE DATABASE sandbox_must_not_escape;")
            .await
            .is_err()
    );
    let admin = inventory::cleanup(&mut db.conn, Some("sandbox2236"))
        .await
        .unwrap_err();
    assert!(
        admin
            .iter()
            .any(|error| error.contains("must execute as the sandbox")),
        "{admin:?}"
    );
    inventory::cleanup(&mut sandbox, Some("sandbox2236"))
        .await
        .unwrap();
    drop(sandbox);
    inventory::drop_sandbox(&mut db.conn, "sandbox2236")
        .await
        .unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn system_catalog_selects_are_creation_independent_and_other_system_permissions_refuse() {
    let mut db = TestDb::create("catalog_select2236").await;
    let rows = db
        .conn
        .query(
            "SELECT COUNT_BIG(*) AS total FROM sys.database_permissions WHERE class=1 AND \
             NOT(permission_name=N'SELECT' AND CONVERT(int,OBJECTPROPERTYEX(major_id,\
             N'IsMSShipped'))=1 AND CONVERT(int,OBJECTPROPERTYEX(major_id,N'IsView'))=1);",
        )
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i64>("total").unwrap(), Some(0));
    for (sql, code) in [
        ("GRANT EXECUTE ON sys.sp_help TO public;", "4629"),
        ("GRANT SELECT ON sys.dm_exec_sessions TO public;", "4629"),
        ("GRANT CONTROL ON sys.objects TO public;", "4606"),
        ("GRANT ALTER ON sys.objects TO public;", "4606"),
        ("GRANT REFERENCES ON TYPE::sys.int TO public;", "15151"),
        (
            "CREATE VIEW dbo.bound WITH SCHEMABINDING AS SELECT object_id FROM sys.objects;",
            "2720",
        ),
    ] {
        let error = db.conn.execute(sql).await.unwrap_err();
        assert_eq!(
            error.server_error_code().as_deref(),
            Some(code),
            "{sql}: {error}"
        );
    }
    scratch::create_sandbox(&mut db.conn, "sandbox_select2236")
        .await
        .unwrap();
    let mut sandbox = connect_live(&super::conn_str()).await.unwrap();
    scratch::use_database(&mut sandbox, &db.name).await.unwrap();
    scratch::enter_sandbox(&mut sandbox, "sandbox_select2236")
        .await
        .unwrap();
    for sql in [
        "GRANT SELECT ON sys.objects TO public;",
        "DENY SELECT ON sys.objects TO public;",
        "CREATE VIEW dbo.v AS SELECT object_id FROM sys.objects;",
        "CREATE FUNCTION dbo.f() RETURNS TABLE AS RETURN SELECT object_id FROM sys.objects;",
        "CREATE PROCEDURE dbo.p AS SELECT object_id FROM sys.objects;",
        "CREATE TABLE dbo.t(i int,c AS OBJECT_NAME(i));",
    ] {
        sandbox.execute(sql).await.unwrap();
    }
    let mark_error = sandbox
        .execute("EXEC sys.sp_MS_marksystemobject N'dbo.t';")
        .await
        .unwrap_err();
    assert_eq!(mark_error.server_error_code().as_deref(), Some("229"));
    let refused = inventory::read(&mut sandbox, Some("sandbox_select2236"))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("objects"), "{refused}");
    for view in ["sys.objects", "sys.tables", "sys.columns"] {
        sandbox
            .execute(&format!("GRANT SELECT ON {view} TO public;"))
            .await
            .unwrap();
        sandbox
            .execute(&format!("DENY SELECT ON {view} TO public;"))
            .await
            .unwrap();
        sandbox
            .execute(&format!("REVOKE SELECT ON {view} FROM public;"))
            .await
            .unwrap();
    }
    inventory::cleanup(&mut sandbox, Some("sandbox_select2236"))
        .await
        .unwrap();
    drop(sandbox);
    inventory::drop_sandbox(&mut db.conn, "sandbox_select2236")
        .await
        .unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_live_marker_distinguishes_instances_and_pins_the_backend_before_any_setup() {
    let mut db = TestDb::create("marker2236").await;
    let nonce = format!(
        "{:032x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mark = scratch::mark(&mut db.conn, &nonce).await.unwrap();
    let backend = scratch::backend(&mut db.conn).await.unwrap();
    assert!(scratch::marked(&mut db.conn, &mark).await.unwrap());
    let mut same = connect_live(&super::conn_str()).await.unwrap();
    assert!(scratch::marked(&mut same, &mark).await.unwrap());
    assert!(!scratch::on_backend(&mut same, &backend).await.unwrap());
    assert!(scratch::on_backend(&mut db.conn, &backend).await.unwrap());
    assert_eq!(
        scratch::placement(&mark.database.id, &mark, true),
        scratch::Placement::Target
    );
    let other = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB")
        .expect("PBPS_TEST_MSSQL_SCRATCH_DB must name the other instance");
    let mut other = connect_live(&other).await.unwrap();
    assert!(!scratch::marked(&mut other, &mark).await.unwrap());
    assert_eq!(
        scratch::placement(&mark.database.id, &mark, false),
        scratch::Placement::SeparateInstance
    );
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn inherited_model_objects_refuse_and_the_model_is_restored_before_assertions() {
    let server = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB")
        .expect("PBPS_TEST_MSSQL_SCRATCH_DB must name the dedicated resolver instance");
    let mut conn = connect_live(&server).await.unwrap();
    let name = format!("pbps2236_model_object_{}", std::process::id());
    let statement = format!(
        "USE master; BEGIN TRY EXEC(N'USE model; CREATE TABLE dbo.{name}(i int);'); \
         EXEC(N'CREATE DATABASE [{name}];'); EXEC(N'USE model; DROP TABLE dbo.{name};'); END \
         TRY BEGIN CATCH IF OBJECT_ID(N'model.dbo.{name}') IS NOT NULL EXEC(N'USE model; \
         DROP TABLE dbo.{name};'); IF DB_ID(N'{name}') IS NOT NULL DROP DATABASE [{name}]; \
         THROW; END CATCH;"
    );
    conn.execute(&statement).await.unwrap();
    let restored = conn
        .query(&format!(
            "SELECT OBJECT_ID(N'model.dbo.{name}') AS remaining;"
        ))
        .await
        .unwrap();
    scratch::use_database(&mut conn, &name).await.unwrap();
    let dirty = inventory::read(&mut conn, None).await;
    scratch::use_database(&mut conn, "master").await.unwrap();
    conn.execute(&format!("DROP DATABASE [{name}];"))
        .await
        .unwrap();
    assert_eq!(restored[0].try_get::<i32>("remaining").unwrap(), None);
    assert!(
        dirty
            .unwrap()
            .names()
            .iter()
            .any(|object| object.contains(&name))
    );
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn failed_database_creation_and_failed_reference_reads_release_every_recorded_resource() {
    use pbps_db::resolver::ScratchNames;
    use pbps_db::resolver::environment::{DatabaseRecipe, SqlServerDatabase};
    let token = format!(
        "{:032x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let names = ScratchNames::new(
        format!("pbps_scratch_{token}"),
        format!("pbps_run_{token}"),
        token.clone(),
    )
    .unwrap();
    let mut admin = connect_live(&super::conn_str()).await.unwrap();
    let mut recipe = DatabaseRecipe::neutral();
    recipe.sql_server = Some(SqlServerDatabase {
        collation: "not_a_sql_server_collation".into(),
        compatibility_level: "170".into(),
        containment: "NONE".into(),
        options: Default::default(),
    });
    let created = pbps_mssql::resolver::create_scratch(&mut admin, &names, &recipe).await;
    let partial = admin
        .query(&format!(
            "SELECT SUSER_ID(N'{}') AS login_id;",
            names.login()
        ))
        .await
        .unwrap();
    let dropped = pbps_mssql::resolver::drop_scratch(&mut admin, &names).await;
    let remaining = admin
        .query(&format!(
            "SELECT SUSER_ID(N'{}') AS login_id,DB_ID(N'{}') AS database_id;",
            names.login(),
            names.database()
        ))
        .await
        .unwrap();
    assert!(created.is_err());
    dropped.unwrap();
    assert!(partial[0].try_get::<i32>("login_id").unwrap().is_some());
    assert_eq!(remaining[0].try_get::<i32>("login_id").unwrap(), None);
    assert_eq!(remaining[0].try_get::<i32>("database_id").unwrap(), None);

    let mut db = TestDb::create("failed_reference2236").await;
    let login = format!("pbps2236_reference_{token}");
    let password = scratch::password(&token);
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; GRANT \
             CREATE ANY DATABASE TO [{login}];"
        ))
        .await
        .unwrap();
    // The options are read inside the new reference, where its creator is
    // dbo and no database deny applies. A server-scoped view's deny is kept
    // in master (4629 elsewhere, measured) and fails the read after CREATE
    // DATABASE, the moment this case is about.
    admin
        .execute(&format!(
            "CREATE USER [{login}] FOR LOGIN [{login}]; \
             DENY SELECT ON sys.database_recovery_status TO [{login}];"
        ))
        .await
        .unwrap();
    db.conn
        .execute(&format!(
            "CREATE USER [{login}] FOR LOGIN [{login}]; ALTER ROLE db_owner ADD MEMBER \
             [{login}]; EXECUTE AS LOGIN=N'{login}';"
        ))
        .await
        .unwrap();
    let reference = pbps_mssql::resolver::fresh_reference(&mut db.conn, &names).await;
    db.conn.execute("REVERT;").await.unwrap();
    let remaining = admin
        .query(&format!(
            "SELECT DB_ID(N'{}') AS database_id;",
            names.database()
        ))
        .await
        .unwrap();
    db.drop().await;
    admin
        .execute(&format!("DROP USER [{login}]; DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert!(
        reference
            .unwrap_err()
            .to_string()
            .contains("database_recovery_status")
    );
    assert_eq!(remaining[0].try_get::<i32>("database_id").unwrap(), None);
}

async fn system_catalog_state(conn: &mut pbps_db::Conn) -> Vec<String> {
    let mut state = Vec::new();
    for database in ["master", "tempdb", "msdb"] {
        // Hash the complete changed catalogs, rather than counts: replacing
        // a grant with a DENY must not count as restoring the fixture.
        for (catalog, order) in [
            (
                "database_permissions",
                "class,major_id,minor_id,grantee_principal_id,grantor_principal_id,\
                 permission_name",
            ),
            (
                "database_role_members",
                "role_principal_id,member_principal_id",
            ),
            ("database_principals", "principal_id"),
            ("schemas", "schema_id"),
        ] {
            let sql = format!(
                "USE [{database}]; SELECT CONVERT(varchar(64),HASHBYTES('SHA2_256',(SELECT * \
                 FROM sys.{catalog} ORDER BY {order} FOR JSON PATH,INCLUDE_NULL_VALUES)),2) \
                 AS snapshot;"
            );
            let rows = conn
                .query(&format!("EXEC(N'{}');", sql.replace('\'', "''")))
                .await
                .unwrap();
            let hash = rows[0].try_get::<&str>("snapshot").unwrap().unwrap();
            state.push(format!("{database}|{catalog}|{hash}"));
        }
    }
    state
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn system_database_permissions_follow_fixed_rules_and_restore_each_change() {
    struct Case {
        name: &'static str,
        database: &'static str,
        change: String,
        restore: String,
        refused: Vec<String>,
    }
    let mut db = TestDb::create("system_rules2236").await;
    let databases = scratch::database_names(&mut db.conn).await.unwrap();
    let sandbox = "system_rules2236";
    scratch::create_sandbox(&mut db.conn, sandbox)
        .await
        .unwrap();
    let mut hidden_reader = connect_live(&super::conn_str()).await.unwrap();
    scratch::use_database(&mut hidden_reader, &db.name)
        .await
        .unwrap();
    scratch::enter_sandbox(&mut hidden_reader, sandbox)
        .await
        .unwrap();
    for database in ["master", "tempdb", "msdb"] {
        let query = format!(
            "USE [{database}]; SELECT COUNT_BIG(*) AS hidden FROM sys.database_permissions p \
             LEFT JOIN sys.all_objects o ON o.object_id=p.major_id WHERE p.class=1 AND \
             p.grantee_principal_id IN(USER_ID(N'guest'),USER_ID(N'public')) AND o.object_id \
             IS NULL AND CONVERT(int,OBJECTPROPERTYEX(p.major_id,N'IsMSShipped'))=1 AND \
             OBJECT_NAME(p.major_id) IS NOT NULL;"
        );
        let rows = hidden_reader
            .query(&format!("EXEC(N'{}');", query.replace('\'', "''")))
            .await
            .unwrap();
        assert!(
            rows[0].try_get::<i64>("hidden").unwrap().unwrap() > 0,
            "{database} must exercise grants on hidden system views"
        );
    }
    drop(hidden_reader);
    let original = system_catalog_state(&mut db.conn).await;
    let object = format!("pbps2236_system_{}", std::process::id());
    let role = format!("pbps2236_role_{}", std::process::id());
    let mut cases = Vec::new();
    for (name, database, change, restore, refused) in [
        ("pristine", "master", "", "", vec![]),
        (
            "deny_only_narrows",
            "master",
            "DENY EXECUTE ON sys.sp_helptext TO public;",
            "GRANT EXECUTE ON sys.sp_helptext TO public;",
            vec![],
        ),
        (
            "shared_shipped_reach",
            "master",
            "GRANT EXECUTE ON sys.xp_cmdshell TO public;",
            "REVOKE EXECUTE ON sys.xp_cmdshell FROM public;",
            vec![],
        ),
        (
            "extra_database_grant",
            "msdb",
            "GRANT CONTROL TO guest;",
            "REVOKE CONTROL FROM guest;",
            vec!["msdb", "guest", "CONTROL", "DATABASE"],
        ),
        (
            "missing_database_grant",
            "master",
            "REVOKE VIEW ANY COLUMN MASTER KEY DEFINITION FROM public;",
            "GRANT VIEW ANY COLUMN MASTER KEY DEFINITION TO public;",
            vec![
                "master",
                "public",
                "VIEW ANY COLUMN MASTER KEY DEFINITION",
                "DATABASE",
            ],
        ),
        (
            "guest_object_grant",
            "master",
            "GRANT EXECUTE ON sys.sp_helptext TO guest;",
            "REVOKE EXECUTE ON sys.sp_helptext FROM guest;",
            vec!["master", "guest", "EXECUTE", "sys.sp_helptext"],
        ),
        // Catalog views reject column grants (4606). This shipped table
        // accepts one, so the minor_id rule is actually exercised.
        (
            "column_grant",
            "master",
            "GRANT SELECT ON dbo.MSreplication_options(optname) TO public;",
            "REVOKE SELECT ON dbo.MSreplication_options(optname) FROM public; REVOKE SELECT \
             ON dbo.MSreplication_options FROM public;",
            vec!["master", "public", "SELECT", "dbo.MSreplication_options"],
        ),
        (
            "guest_role",
            "msdb",
            "ALTER ROLE SQLAgentOperatorRole ADD MEMBER guest;",
            "ALTER ROLE SQLAgentOperatorRole DROP MEMBER guest;",
            vec!["msdb", "guest", "SQLAgentOperatorRole"],
        ),
        (
            "other_permission_class",
            "master",
            "GRANT SELECT ON SCHEMA::dbo TO public;",
            "REVOKE SELECT ON SCHEMA::dbo FROM public;",
            vec!["master", "public", "SELECT", "dbo"],
        ),
        (
            "required_catalog_denied",
            "master",
            "DENY SELECT ON sys.database_permissions TO public;",
            "GRANT SELECT ON sys.database_permissions TO public;",
            vec!["master", "public", "SELECT", "database_permissions"],
        ),
    ] {
        cases.push(Case {
            name,
            database,
            change: change.into(),
            restore: restore.into(),
            refused: refused.into_iter().map(str::to_owned).collect(),
        });
    }
    for state in ["GRANT", "DENY"] {
        cases.push(Case {
            name: "non_system_owner_procedure",
            database: "master",
            change: format!(
                "EXEC(N'CREATE PROCEDURE dbo.{object} WITH EXECUTE AS OWNER AS SELECT 1;'); \
                 {state} EXECUTE ON dbo.{object} TO public;"
            ),
            restore: format!(
                "IF OBJECT_ID(N'dbo.{object}') IS NOT NULL DROP PROCEDURE dbo.{object};"
            ),
            refused: if state == "GRANT" {
                vec![
                    "master".into(),
                    "public".into(),
                    "EXECUTE".into(),
                    format!("dbo.{object}"),
                ]
            } else {
                // DENY hides this user object's metadata from guest. Its
                // permission row remains visible and must refuse by major_id.
                vec![
                    "master".into(),
                    "public".into(),
                    "EXECUTE".into(),
                    "unresolved object major_id".into(),
                ]
            },
        });
    }
    // Agent roles also own other roles, which the ownership check catches.
    // An empty role pins the no-membership rule independently of that reach.
    cases.push(Case {
        name: "guest_role_without_other_reach",
        database: "msdb",
        change: format!(
            "CREATE ROLE [{role}] AUTHORIZATION dbo; ALTER ROLE [{role}] ADD MEMBER guest;"
        ),
        restore: format!(
            "IF DATABASE_PRINCIPAL_ID(N'{role}') IS NOT NULL BEGIN ALTER ROLE [{role}] DROP \
             MEMBER guest; DROP ROLE [{role}]; END;"
        ),
        refused: vec!["msdb".into(), "guest".into(), role.clone()],
    });
    let mut outcomes = Vec::new();
    for case in cases {
        let changed = db
            .conn
            .execute(&format!("USE [{}]; {}", case.database, case.change))
            .await;
        let url = super::conn_str();
        let scratch_database = db.name.clone();
        let databases = databases.clone();
        // A panic in a check is a result too: restore the system catalogs
        // before asserting it, just like an ordinary refusal.
        let outcome = tokio::spawn(async move {
            let mut bounded = connect_live(&url).await?;
            scratch::use_database(&mut bounded, &scratch_database).await?;
            scratch::enter_sandbox(&mut bounded, sandbox).await?;
            scratch::sandbox_checks(&mut bounded, &databases, &scratch_database).await
        })
        .await;
        let restored = db
            .conn
            .execute(&format!("USE [{}]; {}", case.database, case.restore))
            .await;
        restored.unwrap();
        assert_eq!(
            system_catalog_state(&mut db.conn).await,
            original,
            "restoration after {}",
            case.name
        );
        let remaining = db
            .conn
            .query(&format!(
                "SELECT OBJECT_ID(N'master.dbo.{object}') AS remaining, (SELECT principal_id \
                 FROM msdb.sys.database_principals WHERE name=N'{role}') AS remaining_role;"
            ))
            .await
            .unwrap();
        assert_eq!(remaining[0].try_get::<i32>("remaining").unwrap(), None);
        assert_eq!(remaining[0].try_get::<i32>("remaining_role").unwrap(), None);
        outcomes.push((case, changed, outcome));
    }
    db.drop().await;
    for (case, changed, outcome) in outcomes {
        changed.unwrap();
        let result = outcome.unwrap();
        if case.refused.is_empty() {
            assert!(result.is_ok(), "{}: {result:?}", case.name);
        } else {
            let refusal = result.unwrap_err().to_string();
            for name in case.refused {
                assert!(
                    refusal.contains(&name),
                    "{} must name {name}: {refusal}",
                    case.name
                );
            }
        }
    }
}

/// Refusals of a catalog case's statement by an installation that lacks the
/// feature, measured on LocalDB and on the Express container: 39020, Advanced
/// Analytics Extensions not installed; 15707, automatic tuning needs
/// Enterprise; 9982, no full-text search in a LocalDB user instance.
const UNAVAILABLE: &[&str] = &["39020", "15707", "9982"];

/// Whether the engine reports the default FILESTREAM row for the current
/// database on an instance whose FILESTREAM level is 0.
async fn filestream_unrecorded(conn: &mut pbps_db::Conn) -> bool {
    let rows = conn
        .query(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM sys.database_filestream_options \
             WHERE database_id = DB_ID() AND non_transacted_access = 0 \
             AND directory_name IS NULL) \
             AND CONVERT(int, SERVERPROPERTY('FilestreamEffectiveLevel')) = 0 \
             AND CONVERT(int, SERVERPROPERTY('FilestreamConfiguredLevel')) = 0 \
             THEN 1 ELSE 0 END AS unrecorded;",
        )
        .await
        .unwrap();
    rows[0].try_get::<i32>("unrecorded").unwrap() == Some(1)
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn independent_database_catalogs_are_dirty_and_cleanup_restores_fresh_as_owner() {
    let mut db = TestDb::create("catalogs2236").await;
    let cases = catalog_cases::cases();
    db.conn
        .execute(
            "CREATE USER catalog_owner WITHOUT LOGIN; \
         ALTER ROLE db_owner ADD MEMBER catalog_owner; \
         EXECUTE AS USER=N'catalog_owner';",
        )
        .await
        .unwrap();
    let mut refused = Vec::new();
    for (expected, create) in cases.restore {
        if let Err(error) = db.conn.execute(&create).await {
            // An installation that cannot create the object cannot leave its
            // catalog dirty; only these measured refusals are accepted.
            // Any other refusal is collected, so one run reports them all.
            let code = error.server_error_code();
            if !code
                .as_deref()
                .is_some_and(|code| UNAVAILABLE.contains(&code))
            {
                refused.push(format!("{expected}: {error}"));
            }
            continue;
        }
        let names = inventory::read(&mut db.conn, Some("catalog_owner"))
            .await
            .unwrap()
            .names();
        let found = names.iter().any(|name| name.contains(&expected));
        let cleanup = inventory::cleanup(&mut db.conn, Some("catalog_owner")).await;
        let fresh = inventory::read(&mut db.conn, Some("catalog_owner"))
            .await
            .unwrap();
        // An instance without FILESTREAM (LocalDB: configured and effective
        // level 0) accepts the setting and records nothing: the engine reports
        // the default row, so the database is fresh and nothing is hidden
        // (measured on the Windows job). Only that exact report is accepted.
        if !found
            && expected == "database FILESTREAM options"
            && filestream_unrecorded(&mut db.conn).await
        {
            continue;
        }
        // Restore the owned fixture before asserting either half of the contract.
        if !found || cleanup.is_err() || !fresh.is_empty() {
            db.conn.execute("REVERT;").await.unwrap();
            db.drop().await;
            panic!(
                "{expected}: found={names:?}; cleanup={cleanup:?}; remaining={:?}",
                fresh.names()
            );
        }
    }
    if !refused.is_empty() {
        db.conn.execute("REVERT;").await.unwrap();
        db.drop().await;
        panic!("refused outside UNAVAILABLE:\n{}", refused.join("\n"));
    }
    let mut denied_reads = Vec::new();
    for catalog in [
        "database_ledger_digest_locations",
        "key_encryptions",
        "crypt_properties",
        "assembly_references",
        "column_type_usages",
    ] {
        db.conn
            .execute(&format!("DENY SELECT ON sys.{catalog} TO public;"))
            .await
            .unwrap();
        let denied = inventory::read(&mut db.conn, Some("catalog_owner")).await;
        db.conn
            .execute(&format!(
                "REVERT; REVOKE SELECT ON sys.{catalog} FROM public;"
            ))
            .await
            .unwrap();
        db.conn
            .execute("EXECUTE AS USER=N'catalog_owner';")
            .await
            .unwrap();
        denied_reads.push((catalog, denied));
    }
    db.conn
        .execute("REVERT; DROP USER catalog_owner;")
        .await
        .unwrap();
    db.drop().await;
    for (catalog, denied) in denied_reads {
        let denied = denied.unwrap_err().to_string();
        assert!(denied.contains(&format!("sys.{catalog}")), "{denied}");
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn unrestorable_built_ins_refuse_unchanged_and_out_of_set_state_passes() {
    let cases = catalog_cases::cases();
    for (refuses, (expected, create)) in cases
        .refuse
        .into_iter()
        .map(|case| (true, case))
        .chain(cases.residual.into_iter().map(|case| (false, case)))
    {
        let mut db = TestDb::create("builtin2263").await;
        let result: Result<(), String> = async {
            db.conn.execute(
                "CREATE USER catalog_owner WITHOUT LOGIN; ALTER ROLE db_owner ADD MEMBER \
                 catalog_owner; EXECUTE AS USER=N'catalog_owner';",
            ).await.map_err(|error| error.to_string())?;
            db.conn.execute(&create).await.map_err(|error| error.to_string())?;
            let before = db.conn.query(catalog_cases::BUILTIN_STATE).await
                .map_err(|error| error.to_string())?;
            let before = before[0].try_get::<&str>("state").unwrap().unwrap().to_owned();
            let names = inventory::read(&mut db.conn, Some("catalog_owner")).await
                .map_err(|error| error.to_string())?.names();
            let cleanup = inventory::cleanup(&mut db.conn, Some("catalog_owner")).await;
            let remaining = inventory::read(&mut db.conn, Some("catalog_owner")).await
                .map_err(|error| error.to_string())?.names();
            let after = db.conn.query(catalog_cases::BUILTIN_STATE).await
                .map_err(|error| error.to_string())?;
            let after = after[0].try_get::<&str>("state").unwrap().unwrap();
            let valid = if refuses {
                names.iter().any(|name| name.contains(&expected))
                    && cleanup.as_ref().is_err_and(|errors| errors.iter().any(|error|
                        error.contains(&expected) && error.contains(
                            "this database cannot be restored to a fresh state; supply a new scratch database"
                        )))
                    && remaining == names && before == after
            } else {
                names.is_empty() && cleanup.is_ok() && remaining.is_empty() && before == after
            };
            if !valid {
                return Err(format!("{expected}: names={names:?}; cleanup={cleanup:?}; remaining={remaining:?}; unchanged={}", before == after));
            }
            Ok(())
        }.await;
        // The sys owner cannot be restored even by fixture ALTER AUTHORIZATION.
        // Drop this owned database before asserting the measured refusal.
        db.conn.execute("REVERT;").await.unwrap();
        let name = db.name.clone();
        db.drop().await;
        let mut conn = connect_live(&super::conn_str()).await.unwrap();
        assert!(
            conn.query(&format!(
                "SELECT name FROM sys.databases WHERE name=N'{name}';"
            ))
            .await
            .unwrap()
            .is_empty(),
            "fixture database {name} remained"
        );
        result.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn ledger_soft_deletion_refuses_with_the_retained_object_names() {
    let mut db = TestDb::create("ledger2236").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.kept(i int) WITH(LEDGER=ON(APPEND_ONLY=ON)); DROP TABLE dbo.kept;",
        )
        .await
        .unwrap();
    let names = inventory::read(&mut db.conn, None).await.unwrap().names();
    let refused = inventory::cleanup(&mut db.conn, None).await;
    db.drop().await;
    assert!(
        names
            .iter()
            .any(|name| name.contains("MSSQL_DroppedLedgerTable_kept_")),
        "{names:?}"
    );
    assert!(
        refused
            .unwrap_err()
            .iter()
            .any(|name| name.contains("MSSQL_DroppedLedger"))
    );
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn assembly_references_drop_dependents_before_providers_as_owner() {
    let mut db = TestDb::create("assembly2236").await;
    let binaries = [
        include_str!("scratch2236-provider.hex").trim(),
        include_str!("scratch2236-consumer.hex").trim(),
    ];
    let mut owned_trust = Vec::new();
    let mut entered = false;
    let result: Result<_, pbps_db::DbError> = async {
        for binary in binaries {
            let found = db
                .conn
                .query(&format!(
                    "SELECT COUNT_BIG(*) AS count FROM sys.trusted_assemblies \
                 WHERE hash=HASHBYTES('SHA2_512',0x{binary});",
                ))
                .await?;
            if found[0].try_get::<i64>("count")? == Some(0) {
                db.conn
                    .execute(&format!(
                        "DECLARE @hash varbinary(64)=HASHBYTES('SHA2_512',0x{binary}); \
                     EXEC sys.sp_add_trusted_assembly @hash=@hash, \
                     @description=N'pbps2236 dependency fixture';",
                    ))
                    .await?;
                owned_trust.push(binary);
            }
        }
        db.conn
            .execute(
                "CREATE USER assembly_owner WITHOUT LOGIN; \
             ALTER ROLE db_owner ADD MEMBER assembly_owner; EXECUTE AS USER=N'assembly_owner';",
            )
            .await?;
        entered = true;
        for (name, binary) in ["provider", "consumer"].into_iter().zip(binaries) {
            db.conn
                .execute(&format!(
                    "CREATE ASSEMBLY [{name}] FROM 0x{binary} WITH PERMISSION_SET=SAFE;",
                ))
                .await?;
        }
        let references = db
            .conn
            .query(
                "SELECT COUNT_BIG(*) AS count FROM sys.assembly_references r \
             JOIN sys.assemblies a ON a.assembly_id=r.assembly_id \
             JOIN sys.assemblies p ON p.assembly_id=r.referenced_assembly_id \
             WHERE a.name=N'consumer' AND p.name=N'provider';",
            )
            .await?;
        let count = references[0].try_get::<i64>("count")?;
        let names = inventory::read(&mut db.conn, Some("assembly_owner"))
            .await?
            .names();
        let cleanup = inventory::cleanup(&mut db.conn, Some("assembly_owner")).await;
        let fresh = inventory::read(&mut db.conn, Some("assembly_owner"))
            .await?
            .names();
        Ok((count, names, cleanup, fresh))
    }
    .await;
    if entered {
        db.conn.execute("REVERT;").await.unwrap();
    }
    db.drop().await;
    // Exact-hash trust is only a fixture prerequisite. Preserve pre-existing
    // entries and restore the ones this case added even when cleanup fails.
    let mut admin = connect_live(&std::env::var("PBPS_TEST_DB").unwrap())
        .await
        .unwrap();
    for binary in owned_trust {
        admin
            .execute(&format!(
                "DECLARE @hash varbinary(64)=HASHBYTES('SHA2_512',0x{binary}); \
             EXEC sys.sp_drop_trusted_assembly @hash=@hash;",
            ))
            .await
            .unwrap();
    }
    let (count, names, cleanup, fresh) = result.unwrap();
    assert_eq!(count, Some(1));
    assert!(
        names
            .iter()
            .any(|name| name.contains("ASSEMBLY [consumer]")),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| name.contains("ASSEMBLY [provider]")),
        "{names:?}"
    );
    assert!(cleanup.is_ok(), "{cleanup:?}");
    assert!(fresh.is_empty(), "{fresh:?}");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn cyclic_symmetric_key_protection_refuses_by_name_and_preserves_the_keys() {
    let mut db = TestDb::create("keycycle2236").await;
    db.conn
        .execute(
            "CREATE USER cycle_owner WITHOUT LOGIN; ALTER ROLE db_owner ADD MEMBER cycle_owner; \
         EXECUTE AS USER=N'cycle_owner'; \
         CREATE SYMMETRIC KEY ka WITH ALGORITHM=AES_256 \
         ENCRYPTION BY PASSWORD='Pbps!Probe2236Secret'; \
         OPEN SYMMETRIC KEY ka DECRYPTION BY PASSWORD='Pbps!Probe2236Secret'; \
         CREATE SYMMETRIC KEY kb WITH ALGORITHM=AES_256 ENCRYPTION BY SYMMETRIC KEY ka; \
         OPEN SYMMETRIC KEY kb DECRYPTION BY SYMMETRIC KEY ka; \
         ALTER SYMMETRIC KEY ka ADD ENCRYPTION BY SYMMETRIC KEY kb; CLOSE ALL SYMMETRIC KEYS;",
        )
        .await
        .unwrap();
    let refused = inventory::cleanup(&mut db.conn, Some("cycle_owner")).await;
    let retained = inventory::read(&mut db.conn, Some("cycle_owner"))
        .await
        .unwrap()
        .names();
    // Only the fixture knows this password. Cleanup must not guess one or
    // weaken protection to turn an actual dependency cycle into a sort order.
    db.conn
        .execute(
            "OPEN SYMMETRIC KEY ka DECRYPTION BY PASSWORD='Pbps!Probe2236Secret'; \
         OPEN SYMMETRIC KEY kb DECRYPTION BY SYMMETRIC KEY ka; \
         ALTER SYMMETRIC KEY ka DROP ENCRYPTION BY SYMMETRIC KEY kb; CLOSE ALL SYMMETRIC KEYS;",
        )
        .await
        .unwrap();
    let cleanup = inventory::cleanup(&mut db.conn, Some("cycle_owner")).await;
    let fresh = inventory::read(&mut db.conn, Some("cycle_owner"))
        .await
        .unwrap()
        .names();
    db.conn.execute("REVERT;").await.unwrap();
    db.drop().await;
    let refused = refused.unwrap_err();
    for name in ["SYMMETRIC KEY [ka]", "SYMMETRIC KEY [kb]"] {
        assert!(refused.iter().any(|entry| entry == name), "{refused:?}");
        assert!(retained.iter().any(|entry| entry == name), "{retained:?}");
    }
    assert!(
        refused.iter().all(|entry| retained.contains(entry)),
        "{refused:?}"
    );
    assert!(cleanup.is_ok(), "{cleanup:?}");
    assert!(fresh.is_empty(), "{fresh:?}");
}
