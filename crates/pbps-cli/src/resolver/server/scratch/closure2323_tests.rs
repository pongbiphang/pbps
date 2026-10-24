//! Recursive reconstruction and its explicit partial-verification boundary.

use super::*;
use pbps_mssql::resolver::dependencies;
use std::collections::BTreeMap;

async fn replace(case: &Case, table: &str, view: &str) {
    let mut owner = connection(&case.other).await;
    owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
    owner.execute("DROP TABLE dbo.backing2323;").await.unwrap();
    owner.execute(table).await.unwrap();
    owner.execute(view).await.unwrap();
}

fn write(case: &Case, assignments: &str) -> String {
    format!(
        "CREATE PROCEDURE dbo.write2323 AS UPDATE [{}].dbo.dep2323 SET {assignments};",
        case.other_name
    )
}

fn verification(observed: &Observation) -> &Verification {
    let Rehearsal::CompanionRendered { verification, .. } = observed
        .rehearsals
        .iter()
        .find(|(name, _)| name.object().name == "dep2323")
        .unwrap()
        .1
    else {
        panic!("wrong placement")
    };
    verification
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn hidden_temporal_columns_fall_back_under_object_only_visibility() {
    fixture(DependencyKind::Table, true, |case| async move {
        let mut owner = connection(&case.other).await;
        owner.execute("ALTER TABLE dbo.dep2323 ADD CONSTRAINT pk2323 PRIMARY KEY(id); ALTER TABLE dbo.dep2323 ADD period_start datetime2(7) GENERATED ALWAYS AS ROW START HIDDEN NOT NULL DEFAULT SYSUTCDATETIME(), period_end datetime2(7) GENERATED ALWAYS AS ROW END HIDDEN NOT NULL DEFAULT CONVERT(datetime2(7),N'9999-12-31 23:59:59.9999999'), PERIOD FOR SYSTEM_TIME(period_start,period_end); ALTER TABLE dbo.dep2323 SET (SYSTEM_VERSIONING=ON (HISTORY_TABLE=dbo.history2323));").await.unwrap();
        let periods = owner.query("SELECT generated_always_type,is_hidden FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.dep2323') AND name IN(N'period_start',N'period_end') ORDER BY generated_always_type;").await.unwrap();
        assert_eq!(periods.len(), 2);
        for (row, generated) in periods.iter().zip([1_u8, 2]) {
            assert_eq!(row.try_get::<u8>("generated_always_type").unwrap(), Some(generated));
            assert_eq!(row.try_get::<bool>("is_hidden").unwrap(), Some(true));
        }
        let temporal = owner.query("SELECT temporal_type FROM sys.tables WHERE object_id=OBJECT_ID(N'dbo.dep2323');").await.unwrap();
        assert_eq!(temporal[0].try_get::<u8>("temporal_type").unwrap(), Some(2));
        grant_catalog(&case, false).await;
        let permissions = connection(&format!("{};Database={}",case.target,case.other_name)).await.query("SELECT HAS_PERMS_BY_NAME(DB_NAME(),N'DATABASE',N'VIEW DEFINITION') AS database_definition,HAS_PERMS_BY_NAME(N'dbo.dep2323',N'OBJECT',N'VIEW DEFINITION') AS object_definition,HAS_PERMS_BY_NAME(N'dbo.dep2323',N'OBJECT',N'SELECT') AS data_read;").await.unwrap();
        for (permission, expected) in [("database_definition",0),("object_definition",1),("data_read",0)] {
            assert_eq!(permissions[0].try_get::<i32>(permission).unwrap(), Some(expected));
        }
        let mut shapes = declarations(&case.other_name, DependencyKind::Table);
        for name in ["period_start", "period_end"] {
            shapes.values_mut().next().unwrap().columns.insert(name.into(), DependencyColumn {
                ty: "datetime2(7)".parse().unwrap(), nullable: false,
            });
        }
        let probe = format!("CREATE VIEW dbo.visible_period2323 AS SELECT id,name FROM [{}].dbo.dep2323;",case.other_name);
        let result = prepare_probe(&case, &case.scratch_server, shapes, Some(probe)).await.unwrap();
        assert!(result.rehearsals.values().all(|r| matches!(r, Rehearsal::CompanionRendered { source: ShapeSource::Catalog, .. })));
        assert!(matches!(verification(&result), Verification::PartiallyUnverified { limitation: Limitation::Replay { object, code: None, reason } } if object.database==case.other_name && object.schema=="dbo" && object.name=="dep2323" && reason.contains("generated, hidden")), "{:?}",verification(&result));
        assert_eq!(result.write_error, None);
        assert_eq!(result.kind, "U");
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.private_tables, 0);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn readable_nested_views_preserve_multi_base_write_refusal() {
    fixture(DependencyKind::View, false, |case| async move {
        let mut owner = connection(&case.other).await;
        owner.execute("CREATE TABLE dbo.right2323(id int NOT NULL,name nvarchar(50) NULL);").await.unwrap();
        owner.execute("CREATE VIEW dbo.inner2323 AS SELECT l.id,r.name FROM dbo.backing2323 l JOIN dbo.right2323 r ON l.id=r.id;").await.unwrap();
        owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
        owner.execute("CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.inner2323;").await.unwrap();
        let result = prepare_probe(&case, &case.scratch_server, declarations(&case.other_name, DependencyKind::View), Some(write(&case,"id=id,name=name"))).await.unwrap();
        assert_eq!(verification(&result), &Verification::Verified);
        assert_eq!(result.write_error.as_deref(), Some("4405"));
        assert_eq!(result.private_tables,0);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn stale_view_output_metadata_records_the_mismatch_and_falls_back() {
    fixture(DependencyKind::View, false, |case| async move {
        // The view keeps nvarchar(50) metadata until refreshed, while its
        // unchanged definition replayed over the new base yields (60).
        connection(&case.other).await.execute("ALTER TABLE dbo.backing2323 ALTER COLUMN name nvarchar(60) NULL;").await.unwrap();
        let result=prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
        assert!(matches!(verification(&result),Verification::PartiallyUnverified { limitation: Limitation::MetadataMismatch { object,columns } } if object.name=="dep2323" && columns==&["name"]));
        assert_eq!(result.private_tables,1);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn readable_instead_of_verbs_preserve_combined_write_admission() {
    fixture(DependencyKind::View,false,|case| async move {
        let mut owner=connection(&case.other).await;
        owner.execute("CREATE TABLE dbo.right2323(id int NOT NULL,name nvarchar(50) NULL);").await.unwrap();
        owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
        owner.execute("CREATE VIEW dbo.dep2323 AS SELECT l.id,r.name FROM dbo.backing2323 l JOIN dbo.right2323 r ON l.id=r.id;").await.unwrap();
        owner.execute("CREATE TRIGGER dbo.instead2323 ON dbo.dep2323 INSTEAD OF UPDATE AS BEGIN SET NOCOUNT ON; RETURN; END;").await.unwrap();
        let result=prepare_probe(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View),Some(write(&case,"id=id,name=name"))).await.unwrap();
        assert_eq!(verification(&result),&Verification::Verified);
        assert_eq!(result.write_error,None);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn readable_base_identity_rowversion_and_collation_survive_reconstruction() {
    fixture(DependencyKind::View,false,|case| async move {
        connection(&case.other).await.execute("ALTER TABLE dbo.backing2323 ADD identity2323 int IDENTITY(7,2) NOT NULL, rv2323 rowversion, computed2323 AS (id*2), text2323 nvarchar(30) COLLATE Latin1_General_BIN2 NULL;").await.unwrap();
        let result=prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
        assert_eq!(verification(&result),&Verification::Verified);
        assert_eq!(result.private_tables,0);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn readable_computed_columns_preserve_write_refusal() {
    fixture(DependencyKind::View, false, |case| async move {
        replace(
            &case,
            "CREATE TABLE dbo.backing2323(id int NOT NULL,name AS CONVERT(nvarchar(50),id));",
            "CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.backing2323;",
        )
        .await;
        let result = prepare_probe(
            &case,
            &case.scratch_server,
            declarations(&case.other_name, DependencyKind::View),
            Some(write(&case, "name=name")),
        )
        .await
        .unwrap();
        assert_eq!(verification(&result), &Verification::Verified);
        assert_eq!(result.write_error.as_deref(), Some("271"));
        assert_eq!(result.private_tables, 0);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn object_only_visibility_names_hidden_inner_views_and_tables() {
    for nested in [false, true] {
        fixture(DependencyKind::View,true,move |case| async move {
            let hidden = if nested { "inner2323" } else { "backing2323" };
            let mut owner = connection(&case.other).await;
            if nested {
                owner.execute("CREATE VIEW dbo.inner2323 AS SELECT id,name FROM dbo.backing2323;").await.unwrap();
                owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
                owner.execute("CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.inner2323;").await.unwrap();
            }
            let rows = connection(&case.target).await.query("SELECT SUSER_SNAME() AS name;").await.unwrap();
            let login = rows[0].try_get::<&str>("name").unwrap().unwrap();
            owner.execute(&format!("CREATE USER [{login}] FOR LOGIN [{login}]; GRANT CONNECT TO [{login}]; GRANT VIEW DEFINITION ON OBJECT::dbo.dep2323 TO [{login}];")).await.unwrap();
            let result = prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
            assert_eq!(verification(&result),&Verification::PartiallyUnverified { limitation: Limitation::HiddenObject { object: ObjectName { database: case.other_name.clone(),schema:"dbo".into(),name:hidden.into() } } });
            assert_eq!(result.private_tables,1);
        }).await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn union_lineage_falls_back_without_claiming_verification() {
    fixture(DependencyKind::View,false,|case| async move {
        let mut owner = connection(&case.other).await;
        owner.execute("CREATE TABLE dbo.right2323(id int NOT NULL,name nvarchar(50) NULL);").await.unwrap();
        owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
        owner.execute("CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.backing2323 UNION ALL SELECT id,name FROM dbo.right2323;").await.unwrap();
        let result=prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
        assert_eq!(verification(&result),&Verification::PartiallyUnverified { limitation: Limitation::LineageUnavailable { object: ObjectName { database: case.other_name.clone(),schema:"dbo".into(),name:"dep2323".into() } } });
        assert_eq!(result.private_tables,1);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn hidden_key_types_fall_back_before_guessing_a_replay_type() {
    fixture(DependencyKind::View,true,|case| async move {
        replace(&case,"CREATE TABLE dbo.backing2323(id int NOT NULL,name nvarchar(50) NULL,nk nvarchar(30) COLLATE Latin1_General_BIN2,dk datetime2(3));","CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.backing2323 WHERE nk COLLATE Latin1_General_BIN2=N'x' AND dk>=CONVERT(datetime2(3),N'2024-01-01');").await;
        let rows=connection(&case.target).await.query("SELECT SUSER_SNAME() AS name;").await.unwrap();
        let login=rows[0].try_get::<&str>("name").unwrap().unwrap();
        connection(&case.other).await.execute(&format!("CREATE USER [{login}] FOR LOGIN [{login}]; GRANT CONNECT TO [{login}]; GRANT VIEW DEFINITION ON OBJECT::dbo.dep2323 TO [{login}];")).await.unwrap();
        let result=prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
        assert_eq!(verification(&result),&Verification::PartiallyUnverified { limitation: Limitation::HiddenObject { object: ObjectName { database:case.other_name.clone(),schema:"dbo".into(),name:"backing2323".into() } } });
        assert_eq!(result.private_tables,1);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn failed_unchanged_replay_records_its_error_and_uses_one_source_group() {
    fixture(DependencyKind::View,false,|case| async move {
        // Renaming leaves the old CREATE name in sys.sql_modules. Another
        // declared table owns that name, so unchanged replay really fails.
        let mut owner=connection(&case.other).await;
        owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
        owner.execute("CREATE VIEW dbo.a_blocker2323 AS SELECT id,name FROM dbo.backing2323;").await.unwrap();
        owner.execute("EXEC sys.sp_rename N'dbo.a_blocker2323',N'dep2323';").await.unwrap();
        owner.execute("CREATE TABLE dbo.a_blocker2323(id int NOT NULL,name nvarchar(50) NULL);").await.unwrap();
        let mut shapes=declarations(&case.other_name,DependencyKind::View);
        let mut table=shapes.values().next().unwrap().clone();table.kind=DependencyKind::Table;
        shapes.insert(format!("{}.dbo.a_blocker2323",case.other_name).parse().unwrap(),table);
        let result=prepare(&case,&case.scratch_server,shapes).await.unwrap();
        assert!(matches!(verification(&result),Verification::PartiallyUnverified { limitation: Limitation::Replay { object,code:Some(code),.. } } if object.name=="dep2323" && code=="2714"),"{:?}",verification(&result));
        assert_eq!(result.private_tables,1);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_renamed_definition_cannot_leave_an_unexpected_view_before_fallback() {
    fixture(DependencyKind::View,false,|case| async move {
        let mut owner=connection(&case.other).await;
        owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
        owner.execute("CREATE VIEW dbo.old_name2323 AS SELECT id,name FROM dbo.backing2323;").await.unwrap();
        owner.execute("EXEC sys.sp_rename N'dbo.old_name2323',N'dep2323';").await.unwrap();
        let result=prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
        assert!(matches!(verification(&result),Verification::PartiallyUnverified { limitation:Limitation::Replay { object,reason,.. } } if object.name=="dep2323" && reason.contains("expected relation")));
        assert_eq!(result.companion_views,["dep2323"]);
        assert_eq!(result.private_tables,1);
    }).await;
}

async fn indexed(case: &Case) {
    let mut owner = connection(&case.other).await;
    owner.execute("SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET NUMERIC_ROUNDABORT OFF;").await.unwrap();
    owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
    owner
        .execute(
            "CREATE VIEW dbo.dep2323 WITH SCHEMABINDING AS SELECT id,name FROM dbo.backing2323;",
        )
        .await
        .unwrap();
    owner
        .execute("CREATE UNIQUE CLUSTERED INDEX z_unique2323 ON dbo.dep2323(id ASC,name DESC);")
        .await
        .unwrap();
    owner
        .execute("CREATE NONCLUSTERED INDEX [a_hint2323]]] ON dbo.dep2323(name DESC) INCLUDE(id);")
        .await
        .unwrap();
}

async fn grant_catalog(case: &Case, base: bool) {
    let rows = connection(&case.target)
        .await
        .query("SELECT SUSER_SNAME() AS name;")
        .await
        .unwrap();
    let login = rows[0].try_get::<&str>("name").unwrap().unwrap();
    let mut owner = connection(&case.other).await;
    owner.execute(&format!("CREATE USER [{login}] FOR LOGIN [{login}]; GRANT CONNECT TO [{login}]; GRANT VIEW DEFINITION ON OBJECT::dbo.dep2323 TO [{login}];")).await.unwrap();
    if base {
        owner
            .execute(&format!(
                "GRANT VIEW DEFINITION ON OBJECT::dbo.backing2323 TO [{login}];"
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn indexed_views_keep_noexpand_name_hints_and_indexes_in_fallback() {
    for hidden in [false, true] {
        fixture(DependencyKind::View,true,move |case| async move {
            indexed(&case).await;
            grant_catalog(&case,!hidden).await;
            let probe=format!("CREATE VIEW dbo.indexed_probe2323 AS SELECT id,name FROM [{}].dbo.dep2323 WITH(NOEXPAND,INDEX([a_hint2323]]]));",case.other_name);
            let result=prepare_probe(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View),Some(probe)).await.unwrap();
            assert_eq!(result.write_error,None);
            assert_eq!(result.view_indexes,vec![("dep2323".into(),"a_hint2323]".into()),("dep2323".into(),"z_unique2323".into())]);
            if hidden {
                assert!(matches!(verification(&result),Verification::PartiallyUnverified { limitation:Limitation::HiddenObject { object } } if object.name=="backing2323"));
                assert_eq!(result.private_tables,1);
            } else {
                assert_eq!(verification(&result),&Verification::Verified);
                assert_eq!(result.private_tables,0);
            }
        }).await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn recursive_views_recreate_the_inner_view_index_set() {
    fixture(DependencyKind::View,false,|case| async move {
        let mut owner=connection(&case.other).await;
        owner.execute("SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET NUMERIC_ROUNDABORT OFF;").await.unwrap();
        owner.execute("DROP VIEW dbo.dep2323;").await.unwrap();
        owner.execute("CREATE VIEW dbo.inner2323 WITH SCHEMABINDING AS SELECT id,name FROM dbo.backing2323;").await.unwrap();
        owner.execute("CREATE UNIQUE CLUSTERED INDEX inner_unique2323 ON dbo.inner2323(id);").await.unwrap();
        owner.execute("CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.inner2323;").await.unwrap();
        let result=prepare(&case,&case.scratch_server,declarations(&case.other_name,DependencyKind::View)).await.unwrap();
        assert_eq!(verification(&result),&Verification::Verified);
        assert_eq!(result.view_indexes,vec![("inner2323".into(),"inner_unique2323".into())]);
    }).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_missing_companion_index_revokes_verification_and_names_the_index() {
    fixture(DependencyKind::View,false,|case| async move {
        indexed(&case).await;
        let mut observer=connection(&case.scratch_server).await;
        observer.execute(&format!("CREATE DATABASE [{}];",case.other_name)).await.unwrap();
        let owned=case.clone();
        let result=tokio::spawn(async move {
            let shapes=declarations(&owned.other_name,DependencyKind::View);
            let mut inputs=dependencies::read(&mut connection(&owned.target).await,&shapes,false).await.unwrap();
            let mut admin=connection(&owned.scratch_server).await;
            let databases=sql::database_names(&mut admin).await.unwrap();
            let mut owner=connection(&format!("{};Database={}",owned.scratch_server,owned.other_name)).await;
            sql::create_sandbox(&mut owner,"pbps2323_index_sandbox").await.unwrap();
            sql::enter_sandbox(&mut owner,"pbps2323_index_sandbox").await.unwrap();
            sql::sandbox_checks(&mut owner,&databases,&owned.other_name).await.unwrap();
            let mut sessions=BTreeMap::from([(owned.other_name.clone(),owner)]);
            inputs.stage(&mut sessions,&mut admin,"0123456789abcdef0123456789abcdef").await.unwrap();
            assert!(inputs.rehearsals().values().all(|r| matches!(r,Rehearsal::CompanionRendered { verification:Verification::Verified,.. })));
            sql::use_database(&mut admin,&owned.other_name).await.unwrap();
            admin.execute("DROP INDEX [a_hint2323]]] ON dbo.dep2323;").await.unwrap();
            inputs.verify(&mut admin).await.unwrap();
            assert!(inputs.rehearsals().values().all(|r| matches!(r,Rehearsal::CompanionRendered { verification:Verification::PartiallyUnverified { limitation:Limitation::Index { object,index,.. } },.. } if object.name=="dep2323" && index=="a_hint2323]")));
        }).await;
        drop_database(&case.scratch_server,&case.other_name).await;
        result.unwrap();
    }).await;
}
