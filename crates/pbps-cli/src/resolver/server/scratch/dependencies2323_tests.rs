//! The preparation handoff, owned companions and unchanged sandbox boundary.

use super::*;
use pbps_model::dependency::{
    Comparison, Dependencies, Dependency, DependencyColumn, DependencyKind,
};
use pbps_mssql::resolver::{
    capture,
    dependencies::{Limitation, ObjectName, Rehearsal, Rehearsals, ShapeSource, Verification},
};

#[derive(Clone)]
struct Case {
    target: String,
    target_admin: String,
    other: String,
    other_name: String,
    scratch_server: String,
}

async fn fixture<F, Fut>(kind: DependencyKind, unreadable: bool, check: F)
where
    F: FnOnce(Case) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (target_name, target_admin) = create_database(&target_server).await;
    let (other_name, other) = create_database(&target_server).await;
    absent(&scratch_server, &other_name).await;
    let mut owner = connection(&other).await;
    match kind {
        DependencyKind::Table => owner
            .execute("CREATE TABLE dbo.dep2323(id int NOT NULL, name nvarchar(50) NULL);")
            .await
            .unwrap(),
        DependencyKind::View => {
            owner
                .execute("CREATE TABLE dbo.backing2323(id int NOT NULL, name nvarchar(50) NULL);")
                .await
                .unwrap();
            owner
                .execute("CREATE VIEW dbo.dep2323 AS SELECT id,name FROM dbo.backing2323;")
                .await
                .unwrap();
        }
    }
    drop(owner);
    connection(&target_admin)
        .await
        .execute(&format!(
            "CREATE VIEW dbo.managed2323 AS SELECT id,name FROM [{other_name}].dbo.dep2323;"
        ))
        .await
        .unwrap();
    let login = format!("pbps2323_login_{:032x}", rand::random::<u128>());
    let target = if unreadable {
        connection(&target_server).await.execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'Pbps!Test12345', CHECK_POLICY=OFF; ALTER AUTHORIZATION ON DATABASE::[{target_name}] TO [{login}];"
        )).await.unwrap();
        format!("{target_admin};User Id={login};Password=Pbps!Test12345")
    } else {
        target_admin.clone()
    };
    let result = tokio::spawn(check(Case {
        target,
        target_admin,
        other,
        other_name: other_name.clone(),
        scratch_server: scratch_server.clone(),
    }))
    .await;
    // A regression (including the required revert experiments) must not leak
    // the fixture's companion when its verification panics before closeout.
    let remaining = connection(&scratch_server)
        .await
        .query(&format!(
            "SELECT name FROM sys.databases WHERE name=N'{other_name}';"
        ))
        .await
        .unwrap();
    if !remaining.is_empty() {
        drop_database(&scratch_server, &other_name).await;
    }
    drop_database(&target_server, &target_name).await;
    drop_database(&target_server, &other_name).await;
    if unreadable {
        connection(&target_server)
            .await
            .execute(&format!("DROP LOGIN [{login}];"))
            .await
            .unwrap();
    }
    result.unwrap();
    assert!(remaining.is_empty(), "fixture required companion recovery");
}

fn declarations(database: &str, kind: DependencyKind) -> Dependencies {
    [(
        format!("{database}.dbo.dep2323").parse().unwrap(),
        Dependency {
            kind,
            columns: [
                (
                    "id".into(),
                    DependencyColumn {
                        ty: "int".parse().unwrap(),
                        nullable: false,
                    },
                ),
                (
                    "name".into(),
                    DependencyColumn {
                        ty: "nvarchar(50)".parse().unwrap(),
                        nullable: true,
                    },
                ),
            ]
            .into(),
        },
    )]
    .into()
}

struct Observation {
    rehearsals: Rehearsals,
    columns: Vec<(String, String)>,
    kind: String,
    nonnullable: bool,
    private_tables: usize,
    write_error: Option<String>,
    view_indexes: Vec<(String, String)>,
    companion_views: Vec<String>,
}

async fn prepare(
    case: &Case,
    scratch: &str,
    declarations: Dependencies,
) -> Result<Observation, Error> {
    prepare_probe(case, scratch, declarations, None).await
}

async fn prepare_probe(
    case: &Case,
    scratch: &str,
    declarations: Dependencies,
    write_probe: Option<String>,
) -> Result<Observation, Error> {
    let mut target = connection(&case.target).await;
    let identity = sql::cluster_identity(&mut target).await.unwrap();
    let mut schema = Schema::default();
    schema.modules.insert(
        "dbo.managed2323".parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: format!("SELECT id,name FROM [{}].dbo.dep2323", case.other_name),
        },
    );
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let bootstrap =
        pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default()).unwrap();
    let binding = BindingRequest {
        bootstrap: &bootstrap,
        base: &schema,
        desired: &schema,
        dependencies: &declarations,
    };
    let request = super::super::super::ScopeRequest {
        schemas: vec!["dbo".into()],
        ..Default::default()
    };
    let tokens = super::super::Tokens::generate();
    let p = provenance();
    let mut admin = connection(scratch).await;
    let (provisioning, mut state) = super::super::engine::scratch(
        Driver::Mssql,
        &mut target,
        &mut admin,
        &tokens,
        &identity,
        scratch,
        &request,
        &Standard::Mssql,
        None,
        &p,
    )
    .await
    .unwrap();
    let (catalog, authorization) = crate::resolver::scope::read_target(
        &mut target,
        Driver::Mssql,
        &request.schemas,
        &[],
        &request.schemas,
        &[],
    )
    .await
    .unwrap();
    let prepared = Box::pin(state.prepare(
        provisioning,
        &mut admin,
        &mut target,
        &catalog,
        &authorization,
        &request.schemas,
        &binding,
        &[],
    ))
    .await;
    let database = case.other_name.clone();
    let observer_url = format!("{scratch};Database={database}");
    let verified = tokio::spawn(async move {
        let super::super::engine::Prepared::Mssql(mut prepared) = prepared? else { panic!("wrong engine"); };
        // This connection is the companion's managed scratch, never target.
        let write_error = if let Some(sql) = write_probe {
            prepared.compile.execute(&sql).await.err().map(|e| e.server_error_code().unwrap_or_else(|| e.to_string()))
        } else { None };
        prepared.compile.execute(&format!("CREATE VIEW dbo.rehearsed2323 AS SELECT * FROM [{database}].dbo.dep2323;")).await.unwrap();
        let scope = capture::CaptureScope {
            retained: [capture::object("dbo", "rehearsed2323")].into(), candidates: Default::default(),
        };
        capture::capture_declared(&mut prepared.compile, &scope, &declarations).await.unwrap();
        // Binding succeeds without giving the compiler permission to read data.
        assert!(prepared.compile.query(&format!("SELECT TOP(0) * FROM [{database}].dbo.dep2323;")).await.is_err());
        assert!(prepared.compile.execute("REVERT;").await.is_err());
        let rows = prepared.compile.query("SELECT c.name,t.name AS type_name FROM sys.columns c JOIN sys.types t ON t.user_type_id=c.user_type_id WHERE c.object_id=OBJECT_ID(N'dbo.rehearsed2323') ORDER BY c.column_id;").await.unwrap();
        let columns = rows.iter().map(|row| (
            row.try_get::<&str>("name").unwrap().unwrap().to_owned(),
            row.try_get::<&str>("type_name").unwrap().unwrap().to_owned(),
        )).collect();
        let mut observer = connection(&observer_url).await;
        let rows = observer.query("SELECT o.type AS kind,c.is_nullable FROM sys.objects o JOIN sys.columns c ON c.object_id=o.object_id WHERE o.object_id=OBJECT_ID(N'dbo.dep2323') AND c.name=N'id';").await.unwrap();
        assert_eq!(rows.len(), 1);
        let kind = rows[0].try_get::<&str>("kind").unwrap().unwrap().trim().to_owned();
        let nonnullable = rows[0].try_get::<bool>("is_nullable").unwrap() == Some(false);
        let private_tables = observer.query("SELECT name FROM sys.tables WHERE name LIKE N'[_][_]pbps[_]shape[_]%';").await.unwrap().len();
        let rows=observer.query("SELECT v.name AS view_name,i.name AS index_name FROM sys.views v JOIN sys.indexes i ON i.object_id=v.object_id WHERE v.is_ms_shipped=0 AND i.index_id>0 ORDER BY v.name,i.name;").await.unwrap();
        let view_indexes=rows.iter().map(|r| (r.try_get::<&str>("view_name").unwrap().unwrap().to_owned(),r.try_get::<&str>("index_name").unwrap().unwrap().to_owned())).collect();
        let companion_views=observer.query("SELECT name FROM sys.views WHERE is_ms_shipped=0 ORDER BY name;").await.unwrap().iter().map(|r|r.try_get::<&str>("name").unwrap().unwrap().to_owned()).collect();
        Ok(Observation { rehearsals: prepared.dependencies, columns, kind, nonnullable, private_tables, write_error, view_indexes, companion_views })
    }).await;
    state.release(&mut admin).await.unwrap();
    verified.unwrap()
}

async fn absent(server: &str, database: &str) {
    let rows = connection(server)
        .await
        .query(&format!(
            "SELECT name FROM sys.databases WHERE name=N'{database}';"
        ))
        .await
        .unwrap();
    assert!(rows.is_empty(), "companion {database} leaked");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn catalog_companions_preserve_tables_and_views_and_release_every_support_object() {
    for kind in [DependencyKind::Table, DependencyKind::View] {
        fixture(kind, false, move |case| async move {
            let shapes = declarations(&case.other_name, kind);
            let name = shapes.keys().next().unwrap().clone();
            let observed = prepare(&case, &case.scratch_server, shapes).await.unwrap();
            assert_eq!(
                observed.rehearsals,
                [(
                    name,
                    Rehearsal::CompanionRendered {
                        source: ShapeSource::Catalog,
                        verification: Verification::Verified,
                    }
                )]
                .into()
            );
            assert_eq!(
                observed.columns,
                [
                    ("id".into(), "int".into()),
                    ("name".into(), "nvarchar".into())
                ]
            );
            assert_eq!(
                observed.kind,
                if kind == DependencyKind::View {
                    "V"
                } else {
                    "U"
                }
            );
            assert!(observed.nonnullable);
            assert_eq!(observed.private_tables, 0);
            absent(&case.scratch_server, &case.other_name).await;
        })
        .await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn unreadable_dependencies_use_declarations_without_claiming_comparison() {
    for kind in [DependencyKind::Table, DependencyKind::View] {
        fixture(kind, true, move |case| async move {
            let shapes = declarations(&case.other_name, kind);
            let compared =
                pbps_mssql::dependencies::compare(&mut connection(&case.target).await, &shapes)
                    .await
                    .unwrap();
            assert!(
                compared
                    .values()
                    .all(|d| d.comparison == Comparison::NotCompared)
            );
            let name = shapes.keys().next().unwrap().clone();
            let observed = prepare(&case, &case.scratch_server, shapes).await.unwrap();
            assert_eq!(
                observed.rehearsals,
                [(
                    name.clone(),
                    Rehearsal::CompanionRendered {
                        source: ShapeSource::Declaration,
                        verification: Verification::PartiallyUnverified {
                            limitation: Limitation::HiddenObject {
                                object: ObjectName::from(&name)
                            },
                        },
                    }
                )]
                .into()
            );
            assert_eq!(observed.columns.len(), 2);
            assert_eq!(
                observed.kind,
                if kind == DependencyKind::View {
                    "V"
                } else {
                    "U"
                }
            );
            assert!(observed.nonnullable);
            assert_eq!(
                observed.private_tables,
                usize::from(kind == DependencyKind::View)
            );
            absent(&case.scratch_server, &case.other_name).await;
        })
        .await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn same_instance_binds_the_real_shape_and_leaves_the_other_database_untouched() {
    for unreadable in [false, true] {
        fixture(DependencyKind::Table, unreadable, move |case| async move {
            let (scratch_name, scratch) = create_database(&case.target_admin).await;
            let shapes = declarations(&case.other_name, DependencyKind::Table);
            if unreadable {
                connection(&case.other).await.execute("ALTER TABLE dbo.dep2323 ALTER COLUMN id bigint NOT NULL; ALTER TABLE dbo.dep2323 ADD extra int NULL;").await.unwrap();
            }
            let name = shapes.keys().next().unwrap().clone();
            let observed = prepare(&case, &scratch, shapes).await;
            drop_database(&case.target_admin, &scratch_name).await;
            let observed = observed.unwrap();
            assert_eq!(observed.rehearsals, [(name, Rehearsal::BoundAgainstRealDatabase)].into());
            assert_eq!(observed.columns.len(), if unreadable { 3 } else { 2 });
            assert_eq!(observed.columns[0].1, if unreadable { "bigint" } else { "int" });
            let objects = connection(&case.other).await.query("SELECT name FROM sys.objects WHERE is_ms_shipped=0 AND parent_object_id=0;").await.unwrap();
            assert_eq!(objects.len(), 1, "same-instance staging created support objects");
        }).await;
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn readable_mismatches_and_undeclared_references_refuse_by_name() {
    fixture(DependencyKind::Table, false, |case| async move {
        let mut shapes = declarations(&case.other_name, DependencyKind::Table);
        shapes
            .values_mut()
            .next()
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        let error = match prepare(&case, &case.scratch_server, shapes).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("mismatch admitted"),
        };
        assert!(
            error.contains(&case.other_name) && error.contains("type differs"),
            "{error}"
        );
        let error = match prepare(&case, &case.scratch_server, Default::default()).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("undeclared reference admitted"),
        };
        assert!(
            error.contains(&case.other_name) && error.contains("another database"),
            "{error}"
        );
        absent(&case.scratch_server, &case.other_name).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_companion_collision_never_replaces_or_drops_the_existing_database() {
    fixture(DependencyKind::Table, false, |case| async move {
        connection(&case.scratch_server)
            .await
            .execute(&format!("CREATE DATABASE [{}];", case.other_name))
            .await
            .unwrap();
        let occupied = format!("{};Database={}", case.scratch_server, case.other_name);
        connection(&occupied)
            .await
            .execute("CREATE TABLE dbo.keep2323(id int);")
            .await
            .unwrap();
        let outcome = prepare(
            &case,
            &case.scratch_server,
            declarations(&case.other_name, DependencyKind::Table),
        )
        .await;
        let retained = connection(&occupied)
            .await
            .query("SELECT name FROM sys.tables WHERE name=N'keep2323';")
            .await
            .unwrap();
        drop_database(&case.scratch_server, &case.other_name).await;
        let error = match outcome {
            Err(error) => error.to_string(),
            Ok(_) => panic!("collision admitted"),
        };
        assert!(
            error.contains(&case.other_name) && error.contains("already exists"),
            "{error}"
        );
        assert_eq!(retained.len(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn supplied_scratch_on_another_instance_refuses_the_named_dependency() {
    fixture(DependencyKind::Table, false, |case| async move {
        let (scratch_name, scratch) = create_database(&case.scratch_server).await;
        let login = format!("pbps2323_supplied_{:032x}", rand::random::<u128>());
        connection(&case.scratch_server).await.execute(&format!("CREATE LOGIN [{login}] WITH PASSWORD=N'Pbps!Test12345',CHECK_POLICY=OFF; ALTER AUTHORIZATION ON DATABASE::[{scratch_name}] TO [{login}];")).await.unwrap();
        let supplied = format!("{scratch};User Id={login};Password=Pbps!Test12345");
        let outcome = prepare(&case, &supplied, declarations(&case.other_name, DependencyKind::Table)).await;
        let empty = sql::inventory::read(&mut connection(&scratch).await, None).await.unwrap().is_empty();
        drop_database(&case.scratch_server, &scratch_name).await;
        connection(&case.scratch_server).await.execute(&format!("DROP LOGIN [{login}];")).await.unwrap();
        let error = match outcome { Err(error) => error.to_string(), Ok(_) => panic!("unsupported supplied scratch admitted") };
        assert!(error.contains(&case.other_name) && error.contains("supplied scratch on another instance"), "{error}");
        assert!(empty);
        absent(&case.scratch_server, &case.other_name).await;
    }).await;
}

#[path = "closure2323_tests.rs"]
mod closure;
