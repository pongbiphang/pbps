//! Supplied scratch repairs visibility, then cleans through the original sandbox.

use super::*;
use pbps_mssql::resolver::{externals, scratch::visibility::Visibility};
use std::collections::BTreeSet;

fn ident(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}
fn literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

async fn acl(conn: &mut Conn) -> BTreeSet<String> {
    conn.query(
        "SELECT CONCAT(class,N'|',major_id,N'|',minor_id,N'|',grantee_principal_id,N'|',\
         grantor_principal_id,N'|',permission_name,N'|',state) AS entry \
         FROM sys.database_permissions;",
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.try_get::<&str>("entry").unwrap().unwrap().to_owned())
    .collect()
}

async fn fixture<F, Fut>(ordinary: bool, run: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let server = server("PBPS_TEST_DB");
    let (name, on) = create_database(&server).await;
    let login = format!("pbps2314_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let on = if ordinary {
        connection(&server)
            .await
            .execute(&format!(
                "CREATE LOGIN {} WITH PASSWORD={},CHECK_POLICY=OFF; \
             ALTER AUTHORIZATION ON DATABASE::{} TO {};",
                ident(&login),
                literal(&password),
                ident(&name),
                ident(&login),
            ))
            .await
            .unwrap();
        format!("{server};Database={name};User Id={login};Password={password}")
    } else {
        on
    };
    // Keep fixture teardown outside the task, including on assertion failure.
    let result = tokio::spawn(run(on)).await;
    drop_database(&server, &name).await;
    if ordinary {
        connection(&server)
            .await
            .execute(&format!("DROP LOGIN {};", ident(&login)))
            .await
            .unwrap();
    }
    result.unwrap();
}

async fn lifecycle(on: String) {
    let server = server("PBPS_TEST_DB");
    let (target_name, target) = create_database(&server).await;
    let result=tokio::spawn(async move {
        let p=Provenance {same_instance:pbps_model::resolver::SameInstance::Allowed,..provenance()};
        let mut ordinary=connection(&on).await;
        ordinary.execute(
            "GRANT SELECT ON sys.objects TO guest WITH GRANT OPTION AS dbo; \
             GRANT SELECT ON sys.objects TO public WITH GRANT OPTION AS guest; \
             GRANT SELECT ON sys.types TO guest AS dbo;",
        ).await.unwrap();
        let baseline=acl(&mut ordinary).await;
        for (mutation,needle) in [
            ("DECLARE @s nvarchar(max)=N'DENY VIEW DEFINITION TO '+QUOTENAME(USER_NAME()); EXEC(@s);", ""),
            ("DENY VIEW DEFINITION TO public;", "VIEW DEFINITION"),
            ("DECLARE @s nvarchar(max)=N'DENY SELECT ON sys.objects TO '+QUOTENAME(USER_NAME()); EXEC(@s);", ""),
            ("DENY SELECT ON sys.objects TO public CASCADE;", "objects"),
            ("DENY SELECT ON sys.database_permissions TO public;", "database_permissions"),
            ("CREATE ROLE hidden2314; DECLARE @s nvarchar(max)=N'ALTER ROLE hidden2314 ADD MEMBER '+QUOTENAME(USER_NAME()); EXEC(@s); DENY VIEW DEFINITION TO hidden2314; DENY SELECT ON sys.types TO hidden2314;", ""),
            ("REVOKE SELECT ON sys.objects FROM public CASCADE AS guest; REVOKE SELECT ON sys.objects FROM guest CASCADE AS dbo;", ""),
            ("DECLARE @s nvarchar(max)=N'ALTER ROLE db_owner DROP MEMBER '+QUOTENAME(USER_NAME()); EXEC(@s);", ""),
            ("DENY CONTROL TO public;", ""),
            ("DENY VIEW DEFINITION TO public;\nGO\nTHROW 51000,'partial2314',1;", "partial2314"),
        ] {
            let file=format!("CREATE TABLE dbo.file2314(i int);\nGO\n{mutation}");
            let outcome=Box::pin(refusal_policy_inner(&target,&on,Some(&file),&p)).await;
            assert!(matches!(&outcome,ProduceError::Run(_)),"{mutation}: {outcome}");
            assert!(outcome.to_string().contains(needle),"{mutation}: {outcome}");
            let mut fresh=connection(&on).await;
            assert!(sql::inventory::read(&mut fresh,None).await.unwrap().is_empty(),"{mutation}");
            assert_eq!(acl(&mut fresh).await,baseline,"{mutation}");
            let again=Box::pin(outcome_policy_inner(&target,&on,None,&p)).await;
            assert!(again.is_ok(),"{}",why(&again));
        }
    }).await;
    drop_database(&server, &target_name).await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn supplied_visibility_repair_restores_exact_acl_and_reuses_the_database() {
    for ordinary in [false, true] {
        fixture(ordinary, lifecycle).await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn cancelled_file_before_inventory_uses_retained_visibility() {
    fixture(true, |on| async move {
        use super::super::super::engine;
        let mut admin = connection(&on).await;
        let request = super::super::super::ScopeRequest::default();
        let p = provenance();
        let (_, mut state) = engine::mssql::preview(&mut admin, &on, &request, &p)
            .await
            .unwrap();
        let mut acquired = state
            .acquire(
                super::super::Provisioning::Supplied,
                &mut admin,
                &pbps_db::resolver::environment::DatabaseRecipe::neutral(),
                None,
            )
            .await
            .unwrap();
        let file = tokio::spawn(async move {
            externals::run(
                &mut acquired.owner,
                "CREATE TABLE dbo.cancel2314(i int); DENY VIEW DEFINITION TO public; \
                 DENY SELECT ON sys.database_permissions TO public; WAITFOR DELAY '00:01:00';",
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if !admin
                    .query(
                        "SELECT 1 AS present FROM sys.database_permissions WHERE \
                    permission_name=N'SELECT' AND state=N'D' AND \
                    major_id=OBJECT_ID(N'sys.database_permissions');",
                    )
                    .await
                    .unwrap()
                    .is_empty()
                {
                    break;
                }
                assert!(
                    !file.is_finished(),
                    "workload ended before its cancellation barrier"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        file.abort();
        assert!(file.await.unwrap_err().is_cancelled());
        state.release(&mut admin).await.unwrap();
        assert!(
            sql::inventory::read(&mut connection(&on).await, None)
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

async fn retained(on: &str) -> (Conn, Conn, Visibility, String, sql::SessionMark) {
    let mut admin = connection(on).await;
    let mark = sql::mark(&mut admin, &format!("{:032x}", rand::random::<u128>()))
        .await
        .unwrap();
    let sandbox = format!("pbps_sandbox_{:032x}", rand::random::<u128>());
    sql::create_sandbox(&mut admin, &sandbox).await.unwrap();
    let databases = sql::database_names(&mut admin).await.unwrap();
    let mut repair = connection(on).await;
    let baseline = Visibility::retain(&mut repair, &mark, &sandbox, &databases)
        .await
        .unwrap();
    (admin, repair, baseline, sandbox, mark)
}

async fn kill_connection(admin: &mut Conn, conn: &mut Conn) {
    let rows = conn
        .query("SELECT CONVERT(nvarchar(12),@@SPID) AS id;")
        .await
        .unwrap();
    let id = rows[0]
        .try_get::<&str>("id")
        .unwrap()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    admin.execute(&format!("KILL {id};")).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn changed_identity_marker_trust_or_connection_never_certifies_repair() {
    fixture(false, |on| async move {
        let mut admin = connection(&on).await;
        let mark = sql::mark(&mut admin, &format!("{:032x}", rand::random::<u128>()))
            .await
            .unwrap();
        let sandbox = "pbps_sandbox_baseline2314";
        sql::create_sandbox(&mut admin, sandbox).await.unwrap();
        let databases = sql::database_names(&mut admin).await.unwrap();
        let before = acl(&mut admin).await;
        let mut unreadable = connection(&on).await;
        kill_connection(&mut admin, &mut unreadable).await;
        assert!(
            Visibility::retain(&mut unreadable, &mark, sandbox, &databases)
                .await
                .is_err()
        );
        assert_eq!(
            acl(&mut admin).await,
            before,
            "an unreadable baseline changed accepted state"
        );
    })
    .await;
    for case in [
        "principal",
        "marker",
        "database",
        "trustworthy",
        "connection",
        "lost",
        "trigger",
    ] {
        fixture(false,move |on|async move {
            let (mut admin,mut repair,baseline,sandbox,mark)=retained(&on).await;
            match case {
                "principal" => admin.execute(&format!("DROP USER {}; CREATE USER {} WITHOUT LOGIN; ALTER ROLE db_owner ADD MEMBER {};",ident(&sandbox),ident(&sandbox),ident(&sandbox))).await.unwrap(),
                "marker" => {drop(admin);admin=connection(&on).await;},
                "database" => sql::use_database(&mut repair,"master").await.unwrap(),
                "trustworthy" => {
                    let db=sql::database(&mut admin).await.unwrap();
                    admin.execute(&format!("ALTER DATABASE {} SET TRUSTWORTHY ON;",ident(&db.name))).await.unwrap();
                },
                "connection" => {repair=connection(&on).await;},
                "lost" => {
                    kill_connection(&mut admin,&mut repair).await;
                },
                "trigger" => {
                    admin.execute("DENY VIEW DEFINITION TO public;").await.unwrap();
                    admin.execute("CREATE TRIGGER broken2314 ON DATABASE FOR DDL_DATABASE_LEVEL_EVENTS AS THROW 51000,'repair must fail2314',1;").await.unwrap();
                },
                _=>unreachable!(),
            }
            assert!(baseline.restore(&mut repair).await.is_err(),"{case}");
            // The guard refusal must leave the run's record, not certify empty.
            assert!(!admin.query(&format!("SELECT name FROM sys.database_principals WHERE name={};",literal(&sandbox))).await.unwrap().is_empty());
            drop(mark);
        }).await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn repair_triggers_are_bounded_and_drops_return_to_the_original_sandbox() {
    for ordinary in [false, true] {
        fixture(ordinary,|on|async move {
            let server=server("PBPS_TEST_DB");
            let (outside_name,outside)=create_database(&server).await;
            let cleanup_server=server.clone();
            let cleanup_outside=outside_name.clone();
            let result=tokio::spawn(async move {
                let mut original=connection(&on).await;
                let owner=original.query("SELECT ORIGINAL_LOGIN() AS name;").await.unwrap();
                let owner=owner[0].try_get::<&str>("name").unwrap().unwrap().to_owned();
                connection(&server).await.execute(&format!(
                    "ALTER AUTHORIZATION ON DATABASE::{} TO {};",
                    ident(&outside_name),ident(&owner),
                )).await.unwrap();
                if owner!="sa" {connection(&server).await.execute(&format!("GRANT ALTER ANY LOGIN TO {};",ident(&owner))).await.unwrap();}
                connection(&outside).await.execute("CREATE TABLE dbo.sentinel2314(i int); INSERT dbo.sentinel2314 VALUES(2314);").await.unwrap();
                let outside_name=sql::database(&mut connection(&outside).await).await.unwrap().name;
                let control=original.query(&format!("SELECT HAS_DBACCESS({}) AS outside_access,HAS_PERMS_BY_NAME(NULL,NULL,N'ALTER ANY LOGIN') AS alter_login;",literal(&outside_name))).await.unwrap();
                assert_eq!(control[0].try_get::<i32>("outside_access").unwrap(),Some(1));
                assert_eq!(control[0].try_get::<i32>("alter_login").unwrap(),Some(1));
                let (mut admin,mut repair,baseline,sandbox,_mark)=retained(&on).await;
                for probe in [
                    "REVERT;".to_owned(),
                    "CREATE LOGIN escape2314 WITH PASSWORD='Pbps!Escape2314';".into(),
                    "CREATE DATABASE escape2314;".into(),
                    format!("SELECT * FROM {}.dbo.sentinel2314;",ident(&outside_name)),
                ] {assert!(repair.execute(&probe).await.is_err(),"bounded dbo escaped: {probe}");}
                let mut file=connection(&on).await;
                sql::enter_sandbox(&mut file,&sandbox).await.unwrap();
                file.execute("CREATE TABLE dbo.authority2314(event nvarchar(128),principal nvarchar(128)); CREATE TABLE dbo.file2314(i int);").await.unwrap();
                file.execute(&format!(
                    "CREATE TRIGGER authority2314 ON DATABASE FOR DDL_DATABASE_LEVEL_EVENTS AS \
                     BEGIN SET NOCOUNT ON; IF IS_SRVROLEMEMBER(N'sysadmin')=1 OR \
                     HAS_PERMS_BY_NAME(NULL,NULL,N'ALTER ANY LOGIN')=1 OR HAS_DBACCESS({})<>0 \
                     THROW 51000,N'cleanup trigger gained outside authority',1; \
                     INSERT dbo.authority2314 VALUES(EVENTDATA().value(N'(/EVENT_INSTANCE/EventType)[1]',N'nvarchar(128)'),USER_NAME()); END;",
                    literal(&outside_name),
                )).await.unwrap();
                file.execute("DENY VIEW DEFINITION TO public; DENY SELECT ON sys.database_permissions TO public;").await.unwrap();
                drop(file);
                baseline.restore(&mut repair).await.unwrap();
                repair.execute("DROP TABLE dbo.file2314;").await.unwrap();
                let observations=admin.query("SELECT event,principal FROM dbo.authority2314;").await.unwrap();
                assert!(observations.iter().any(|r|r.try_get::<&str>("principal").unwrap()==Some("dbo")));
                assert!(observations.iter().any(|r|r.try_get::<&str>("event").unwrap()==Some("DROP_TABLE") && r.try_get::<&str>("principal").unwrap()==Some(sandbox.as_str())));
                for probe in ["REVERT;".to_owned(),format!("SELECT * FROM {}.dbo.sentinel2314;",ident(&outside_name)),"CREATE LOGIN escape2314 WITH PASSWORD='Pbps!Escape2314';".into(),"CREATE DATABASE escape2314;".into()] {
                    assert!(repair.execute(&probe).await.is_err(),"sandbox escaped: {probe}");
                }
                repair.execute("DROP TRIGGER authority2314 ON DATABASE; DROP TABLE dbo.authority2314;").await.unwrap();
                sql::inventory::cleanup(&mut repair,Some(&sandbox)).await.unwrap();
                drop(repair);
                sql::inventory::drop_sandbox(&mut admin,&sandbox).await.unwrap();
                let sentinel=connection(&outside).await.query("SELECT i FROM dbo.sentinel2314;").await.unwrap();
                assert_eq!(sentinel[0].try_get::<i32>("i").unwrap(),Some(2314));
                if owner!="sa" {connection(&server).await.execute(&format!("REVOKE ALTER ANY LOGIN FROM {};",ident(&owner))).await.unwrap();}
            }).await;
            drop_database(&cleanup_server,&cleanup_outside).await;
            result.unwrap();
        }).await;
    }
}
