//! The A-prime creation-order hints against both native scratch routes.
use super::*;
use pbps_model::{Column, Module, ModuleId, ModuleKind, Table, TableName};
use pbps_mssql::resolver::{
    capture,
    reconstruct::{AuxiliaryStep, Reconstruction},
};
use std::{collections::BTreeSet, pin::Pin};

fn declared(kind: ModuleKind, home: &str, body: &str) -> Declared {
    let id: ModuleId = match kind {
        ModuleKind::View => format!("{home}.consumer2541"),
        ModuleKind::Function | ModuleKind::Procedure => format!("{home}.consumer2541()"),
        ModuleKind::Trigger => "dbo.t.consumer2541".into(),
    }
    .parse()
    .unwrap();
    let desired = Schema {
        tables: [(
            TableName::new("dbo", "t"),
            Table {
                columns: [("i".into(), Column::new("int".parse().unwrap()))].into(),
                ..Default::default()
            },
        )]
        .into(),
        modules: [(
            id,
            Module {
                kind,
                definition: body.into(),
                description: None,
                options: Default::default(),
            },
        )]
        .into(),
        ..Default::default()
    };
    Declared {
        base: desired.clone(),
        desired,
    }
}

fn consumer(schema: &Schema) -> Reconstruction {
    let plan = pbps_diff::order::propose(pbps_model::ChangeSet {
        changes: schema
            .modules
            .iter()
            .map(|(id, module)| {
                pbps_model::PlannedChange::new(pbps_model::Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                })
            })
            .collect(),
    });
    Reconstruction::new(
        &pbps_mssql::Mssql,
        &plan,
        &IdsFile::default(),
        &Default::default(),
    )
    .unwrap()
}

async fn install(target: &str, schema: &Schema) {
    let compiled = consumer(schema);
    compiled
        .compile_steps(&mut connection(target).await, 0..compiled.len())
        .await
        .unwrap();
}

async fn observe(
    target: &str,
    scratch: &str,
    schema: &Declared,
    file: Option<&str>,
) -> Result<BTreeSet<capture::ObjectIdentity>, Error> {
    Box::pin(super::chained2376::observe(
        target,
        scratch,
        schema,
        file,
        &Default::default(),
    ))
    .await
    .map(|(roots, _)| roots)
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_one_part_views_and_inline_tvfs_precede_the_deferred_suffix() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target)
                .await
                .execute("CREATE TABLE dbo.t(i int);")
                .await
                .unwrap();
            connection(target)
                .await
                .execute("CREATE SCHEMA fallback AUTHORIZATION dbo;")
                .await
                .unwrap();
            for home in ["dbo", "fallback"] {
                connection(target)
                    .await
                    .execute(&format!("CREATE VIEW {home}.v AS SELECT i FROM dbo.t;"))
                    .await
                    .unwrap();
                for (kind, body, drop_kind) in [
                    // Install directly first: the engine, independently of
                    // chained hints, decides whether AS is optional here.
                    (
                        ModuleKind::Function,
                        "() RETURNS TABLE RETURN (SELECT i FROM v)",
                        "FUNCTION",
                    ),
                    (ModuleKind::View, "SELECT i FROM v", "VIEW"),
                    (
                        ModuleKind::Function,
                        "() RETURNS TABLE AS RETURN (SELECT i FROM v)",
                        "FUNCTION",
                    ),
                ] {
                    let schema = declared(kind, home, body);
                    install(target, &schema.desired).await;
                    for file in [None, Some("-- pbps:deferred\nSELECT 1;")] {
                        let roots = observe(target, scratch, &schema, file).await.unwrap();
                        assert!(
                            roots.contains(&capture::object(home, "v")),
                            "{home}/{kind}: {roots:?}"
                        );
                    }
                    connection(target)
                        .await
                        .execute(&format!("DROP {drop_kind} {home}.consumer2541;"))
                        .await
                        .unwrap();
                }
                connection(target)
                    .await
                    .execute(&format!("DROP VIEW {home}.v;"))
                    .await
                    .unwrap();
            }
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_nearer_managed_and_prefix_objects_mask_dbo() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target)
                .await
                .execute("CREATE TABLE dbo.t(i int);")
                .await
                .unwrap();
            connection(target)
                .await
                .execute("CREATE SCHEMA fallback AUTHORIZATION dbo;")
                .await
                .unwrap();
            // dbo.v really depends on the consumer. A false fallback edge
            // therefore makes a cycle; the nearer object must suppress it.
            for managed in [false, true] {
                connection(target)
                    .await
                    .execute("CREATE VIEW fallback.v AS SELECT CAST(NULL AS int) AS i;")
                    .await
                    .unwrap();
                let mut schema = declared(ModuleKind::View, "fallback", "SELECT i FROM v");
                install(target, &schema.desired).await;
                connection(target)
                    .await
                    .execute("CREATE VIEW dbo.v AS SELECT i FROM fallback.consumer2541;")
                    .await
                    .unwrap();
                if managed {
                    schema.desired.modules.insert(
                        "fallback.v".parse().unwrap(),
                        Module {
                            kind: ModuleKind::View,
                            definition: "SELECT CAST(NULL AS int) AS i".into(),
                            description: None,
                            options: Default::default(),
                        },
                    );
                    schema.base = schema.desired.clone();
                }
                let file = if managed {
                    "-- pbps:deferred\nSELECT 1;"
                } else {
                    "CREATE VIEW fallback.v AS SELECT CAST(NULL AS int) AS i;\n-- pbps:deferred\nSELECT 1;"
                };
                for file in [None, Some(file)] {
                    let roots = observe(target, scratch, &schema, file).await.unwrap();
                    assert!(roots.contains(&capture::object("dbo", "v")), "{roots:?}");
                }
                connection(target)
                    .await
                    .execute(
                        "DROP VIEW dbo.v; DROP VIEW fallback.consumer2541; DROP VIEW fallback.v;",
                    )
                    .await
                    .unwrap();
            }
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_deferred_function_bodies_do_not_invent_cycles() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target)
                .await
                .execute("CREATE TABLE dbo.t(i int);")
                .await
                .unwrap();
            for (body, view) in [
                (
                    "() RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
                    "SELECT dbo.consumer2541() AS i",
                ),
                (
                    "() RETURNS @r TABLE(i int) AS BEGIN INSERT @r SELECT i FROM dbo.v; RETURN; END",
                    "SELECT i FROM dbo.consumer2541()",
                ),
            ] {
                let schema = declared(ModuleKind::Function, "dbo", body);
                // This legal engine order is the independent cycle witness:
                // the function CREATE succeeds before the view exists.
                install(target, &schema.desired).await;
                connection(target)
                    .await
                    .execute(&format!("CREATE VIEW dbo.v AS {view};"))
                    .await
                    .unwrap();
                for file in [None, Some("-- pbps:deferred\nSELECT 1;")] {
                    let roots = observe(target, scratch, &schema, file).await.unwrap();
                    assert!(roots.contains(&capture::object("dbo", "v")), "{roots:?}");
                }
                connection(target)
                    .await
                    .execute("DROP VIEW dbo.v; DROP FUNCTION dbo.consumer2541;")
                    .await
                    .unwrap();
            }
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_aliases_stay_acyclic_and_unknown_text_refuses_by_name() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target)
                .await
                .execute("CREATE TABLE dbo.t(i int);")
                .await
                .unwrap();
            let mut schema = declared(
                ModuleKind::View,
                "dbo",
                "WITH local AS (SELECT CAST(NULL AS int) AS i) SELECT v.i FROM local AS v",
            );
            install(target, &schema.desired).await;
            connection(target)
                .await
                .execute("CREATE VIEW dbo.v AS SELECT i FROM dbo.consumer2541;")
                .await
                .unwrap();
            // Keep v in the target survey independently of the alias/CTE
            // being tested. Otherwise no renderer participates in this case.
            schema.desired.modules.insert(
                "dbo.keep2541".parse().unwrap(),
                Module {
                    kind: ModuleKind::View,
                    definition: "SELECT i FROM dbo.v".into(),
                    description: None,
                    options: Default::default(),
                },
            );
            schema.base = schema.desired.clone();
            connection(target)
                .await
                .execute("CREATE VIEW dbo.keep2541 AS SELECT i FROM dbo.v;")
                .await
                .unwrap();
            let roots = observe(
                target,
                scratch,
                &schema,
                Some("-- pbps:deferred\nSELECT 1;"),
            )
            .await
            .unwrap();
            assert!(roots.contains(&capture::object("dbo", "v")), "{roots:?}");
            // A CTE named v is a documented lexical over-inclusion. Its
            // genuine counter-edge must be a named refusal, never evidence.
            schema
                .desired
                .modules
                .values_mut()
                .next()
                .unwrap()
                .definition = "WITH v AS (SELECT CAST(NULL AS int) AS i) SELECT i FROM v".into();
            let error = observe(target, scratch, &schema, None)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("consumer2541") && error.contains("v"),
                "{error}"
            );
            assert!(
                error.contains("order") || error.contains("cycle"),
                "{error}"
            );
            schema
                .desired
                .modules
                .values_mut()
                .next()
                .unwrap()
                .definition = "SELECT i FROM dbo.v /* unfinished".into();
            let error = observe(target, scratch, &schema, None)
                .await
                .unwrap_err()
                .to_string();
            for text in ["consumer2541", "cannot be lexed"] {
                assert!(error.contains(text), "{error}");
            }
            assert!(
                !error.contains("unfinished"),
                "source text must stay private: {error}"
            );
            // Failure must release supplied scratch too: a healthy next run
            // reaches the same final-catalog boundary.
            schema
                .desired
                .modules
                .values_mut()
                .next()
                .unwrap()
                .definition = schema
                .base
                .modules
                .values()
                .next()
                .unwrap()
                .definition
                .clone();
            observe(target, scratch, &schema, None).await.unwrap();
        })
    }
    reconstruct2374::both(case).await;
}

async fn prediction(target: &str, schema: &Schema) -> bool {
    let ids = pbps_diff::resolve(
        schema,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "chain2541-noexec".into(),
            today: "2026-10-31".into(),
        },
    )
    .unwrap()
    .ids;
    let empty = Schema::default();
    let empty_ids = IdsFile::default();
    let plan = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &empty_ids,
        },
        pbps_diff::Side { schema, ids: &ids },
        &pbps_mssql::Mssql,
        &Hints::default(),
    )
    .unwrap();
    let inputs =
        capture::required_externals(&mut connection(target).await, &[schema], &capture_key())
            .await
            .unwrap();
    let reconstruction = Reconstruction::with_chained(
        &pbps_mssql::Mssql,
        &plan,
        &ids,
        &Default::default(),
        Some(&inputs),
        true,
    )
    .unwrap();
    let consumer = reconstruction.created()[&capture::object("dbo", "consumer2541")];
    let rendered = (0..reconstruction.len()).find(|at| matches!(reconstruction.auxiliary(*at), Some(AuxiliaryStep::Rendered(id)) if *id == capture::object("dbo", "v")));
    let rendered =
        rendered.expect("the oracle requires dbo.v to be in the target survey's chained roots");
    rendered < consumer
}

// A test-only independent compiler oracle. NOEXEC is never used by the
// production scanner, and a failed compile is reset before observing state.
async fn noexec(conn: &mut Conn, consumer: &Reconstruction) -> bool {
    assert_eq!(
        consumer.len(),
        1,
        "the oracle compiles exactly one consumer"
    );
    conn.execute("SET NOEXEC ON;").await.unwrap();
    let result = consumer.compile_steps(conn, 0..1).await;
    conn.execute("SET NOEXEC OFF;").await.unwrap();
    if let Err(error) = &result {
        let text = error.to_string();
        assert!(
            text.contains("Invalid object name") || text.contains("Cannot find either column"),
            "not a missing-reference verdict: {text}"
        );
    }
    assert_eq!(
        conn.query("SELECT 1 AS i;").await.unwrap()[0]
            .try_get::<i32>("i")
            .unwrap(),
        Some(1)
    );
    result.is_ok()
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_noexec_differential_has_an_independent_negative_control() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target)
                .await
                .execute("CREATE TABLE dbo.t(i int);")
                .await
                .unwrap();
            connection(target)
                .await
                .execute("CREATE VIEW dbo.v AS SELECT i FROM dbo.t;")
                .await
                .unwrap();
            for (kind, body, drop_kind, hard) in [
                (ModuleKind::View, "SELECT i FROM v", "VIEW", true),
                (
                    ModuleKind::Function,
                    "() RETURNS int BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
                    "FUNCTION",
                    false,
                ),
                (
                    ModuleKind::Function,
                    "() RETURNS @r TABLE(i int) BEGIN INSERT @r SELECT i FROM dbo.v; RETURN; END",
                    "FUNCTION",
                    false,
                ),
                (
                    ModuleKind::Function,
                    "() RETURNS TABLE RETURN (SELECT i FROM v)",
                    "FUNCTION",
                    true,
                ),
                (ModuleKind::View, "SELECT i FROM dbo.v", "VIEW", true),
                (
                    ModuleKind::Function,
                    "() RETURNS TABLE AS RETURN (SELECT i FROM v)",
                    "FUNCTION",
                    true,
                ),
                (
                    ModuleKind::Function,
                    "() RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.v); END",
                    "FUNCTION",
                    false,
                ),
                (
                    ModuleKind::Function,
                    "() RETURNS @r TABLE(i int) AS BEGIN INSERT @r SELECT i FROM dbo.v; RETURN; END",
                    "FUNCTION",
                    false,
                ),
                (
                    ModuleKind::Procedure,
                    "AS SELECT i FROM dbo.v",
                    "PROCEDURE",
                    false,
                ),
                (
                    ModuleKind::Trigger,
                    "AFTER INSERT AS SELECT i FROM dbo.v",
                    "TRIGGER",
                    false,
                ),
            ] {
                let schema = declared(kind, "dbo", body);
                install(target, &schema.desired).await;
                let predicted = prediction(target, &schema.desired).await;
                assert_eq!(predicted, hard, "{kind}: {body}");
                let compiled = consumer(&schema.desired);
                let (name, url) = create_database(scratch).await;
                let outcome = tokio::spawn(async move {
                    let mut conn = connection(&url).await;
                    conn.execute("CREATE TABLE dbo.t(i int);").await.unwrap();
                    let without = noexec(&mut conn, &compiled).await;
                    let agrees = |hint| hint != without;
                    assert!(agrees(predicted), "scanner and NOEXEC disagree: {kind}");
                    // Deliberately drop the scanner's positive prediction.
                    // The same oracle must reject this corrupted prediction.
                    if predicted {
                        assert!(!agrees(false), "negative control failed");
                    }
                    conn.execute("CREATE VIEW dbo.v AS SELECT i FROM dbo.t;")
                        .await
                        .unwrap();
                    assert!(noexec(&mut conn, &compiled).await, "{kind}");
                    assert_eq!(
                        conn.query("SELECT OBJECT_ID(N'dbo.consumer2541') AS id;")
                            .await
                            .unwrap()[0]
                            .try_get::<i32>("id")
                            .unwrap(),
                        None,
                        "NOEXEC must not create the consumer"
                    );
                })
                .await;
                drop_database(scratch, &name).await;
                outcome.unwrap();
                connection(target)
                    .await
                    .execute(&format!("DROP {drop_kind} dbo.consumer2541;"))
                    .await
                    .unwrap();
            }
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain2541_target_domain_closes_freshly_and_hidden_is_not_absent() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = connection(target).await;
            conn.execute("CREATE TABLE dbo.t(i int);").await.unwrap();
            conn.execute("CREATE VIEW dbo.w AS SELECT i FROM dbo.t;")
                .await
                .unwrap();
            conn.execute("CREATE VIEW dbo.v AS SELECT i FROM w;")
                .await
                .unwrap();
            conn.execute("CREATE VIEW dbo.unrelated2541 WITH ENCRYPTION AS SELECT 1 AS i;")
                .await
                .unwrap();
            let mut schema = declared(ModuleKind::View, "dbo", "SELECT CAST(NULL AS int) AS i");
            install(target, &schema.desired).await;
            schema
                .desired
                .modules
                .values_mut()
                .next()
                .unwrap()
                .definition = "SELECT i FROM v".into();
            let survey = capture::required_externals(
                &mut conn,
                &[&schema.base, &schema.desired],
                &capture_key(),
            )
            .await
            .unwrap();
            for name in ["v", "w"] {
                assert!(
                    survey.roots().contains(&capture::object("dbo", name)),
                    "missing {name}"
                );
            }
            assert!(
                !survey
                    .roots()
                    .contains(&capture::object("dbo", "unrelated2541"))
            );
            assert!(
                !survey
                    .captured()
                    .bound_objects(&capture::object("dbo", "v"))
                    .unwrap()
                    .any(|id| *id == capture::object("dbo", "w"))
            );
            assert!(
                survey.prerequisites()[&capture::object("dbo", "w")]
                    .contains(&capture::object("dbo", "t"))
            );
            // The target's old consumer body has no v reference. Desired text
            // must seed the survey, independently of existing dependency rows.
            observe(target, scratch, &schema, None).await.unwrap();
            conn.execute("ALTER VIEW dbo.w AS SELECT CAST(NULL AS int) AS i;")
                .await
                .unwrap();
            let fresh = capture::required_externals(&mut conn, &[&schema.desired], &capture_key())
                .await
                .unwrap();
            assert!(
                !fresh
                    .prerequisites()
                    .contains_key(&capture::object("dbo", "w"))
            );
            assert!(!survey.captured().compare(fresh.captured()).is_empty());
            conn.execute("ALTER VIEW dbo.w AS SELECT i FROM dbo.t;")
                .await
                .unwrap();
            let fresh = capture::required_externals(&mut conn, &[&schema.desired], &capture_key())
                .await
                .unwrap();
            assert!(
                fresh.prerequisites()[&capture::object("dbo", "w")]
                    .contains(&capture::object("dbo", "t"))
            );
            // A separate login fixes the principal for every driver call;
            // a batch-local EXECUTE AS is not evidence about later reads.
            let login = format!("dep2374_hidden2541_{:016x}", rand::random::<u64>());
            conn.execute(&format!("CREATE LOGIN [{login}] WITH PASSWORD=N'Pbps!Test12345', CHECK_POLICY=OFF; CREATE USER [{login}] FOR LOGIN [{login}]; GRANT VIEW DEFINITION TO [{login}]; GRANT SELECT ON sys.sql_expression_dependencies TO [{login}]; DENY VIEW DEFINITION ON OBJECT::dbo.v TO [{login}];")).await.unwrap();
            let mut reader =
                connection(&format!("{target};User Id={login};Password=Pbps!Test12345")).await;
            let physical = conn.query("SELECT CONVERT(int,CASE WHEN OBJECT_ID(N'dbo.v') IS NOT NULL THEN 1 ELSE 0 END) AS present, CONVERT(int,CASE WHEN OBJECT_ID(N'dbo.absent2541') IS NULL THEN 1 ELSE 0 END) AS absent;").await.unwrap();
            assert_eq!(physical[0].try_get::<i32>("present").unwrap(), Some(1));
            assert_eq!(physical[0].try_get::<i32>("absent").unwrap(), Some(1));
            eprintln!("owner2541: dbo.v present=1, dbo.absent2541 absent=1");
            let permission = reader.query("SELECT HAS_PERMS_BY_NAME(N'dbo.v',N'OBJECT',N'VIEW DEFINITION') AS view_definition, HAS_PERMS_BY_NAME(N'dbo.v',N'OBJECT',N'SELECT') AS data_read, HAS_PERMS_BY_NAME(N'dbo.absent2541',N'OBJECT',N'VIEW DEFINITION') AS absent_definition;").await.unwrap();
            eprintln!(
                "permission2541 before data grant: view_definition={:?}, data_read={:?}, absent_definition={:?}",
                permission[0].try_get::<i32>("view_definition").unwrap(),
                permission[0].try_get::<i32>("data_read").unwrap(),
                permission[0].try_get::<i32>("absent_definition").unwrap()
            );
            conn.execute(&format!("GRANT SELECT ON dbo.v TO [{login}];"))
                .await
                .unwrap();
            let visibility = reader.query("SELECT USER_NAME() AS reader, CONVERT(int,CASE WHEN OBJECT_DEFINITION(OBJECT_ID(N'dbo.v')) IS NULL THEN 1 ELSE 0 END) AS hidden, HAS_PERMS_BY_NAME(N'dbo.v',N'OBJECT',N'SELECT') AS readable;").await.unwrap();
            assert_eq!(
                visibility[0].try_get::<&str>("reader").unwrap(),
                Some(login.as_str())
            );
            assert_eq!(visibility[0].try_get::<i32>("hidden").unwrap(), Some(1));
            assert_eq!(visibility[0].try_get::<i32>("readable").unwrap(), Some(1));
            let names = reader.query("SELECT CONVERT(int,COUNT_BIG(*)) AS found FROM sys.objects o JOIN sys.schemas s ON s.schema_id=o.schema_id WHERE s.name=N'dbo' AND o.name=N'v';").await.unwrap();
            eprintln!(
                "catalog2541 by schema/name: {:?}",
                names[0].try_get::<i32>("found").unwrap()
            );
            let catalog_visibility = reader.query("SELECT CONVERT(int,CASE WHEN m.object_id IS NULL THEN 1 ELSE 0 END) AS missing, CONVERT(int,CASE WHEN m.definition IS NULL THEN 1 ELSE 0 END) AS hidden FROM sys.objects o LEFT JOIN sys.sql_modules m ON m.object_id=o.object_id WHERE o.object_id=OBJECT_ID(N'dbo.v');").await.unwrap();
            eprintln!(
                "catalog2541: rows={}, module_missing={:?}, definition_hidden={:?}",
                catalog_visibility.len(),
                catalog_visibility
                    .first()
                    .map(|row| row.try_get::<i32>("missing").unwrap()),
                catalog_visibility
                    .first()
                    .map(|row| row.try_get::<i32>("hidden").unwrap())
            );
            let hidden =
                capture::required_externals(&mut reader, &[&schema.desired], &capture_key()).await;
            if let Ok(inputs) = &hidden {
                eprintln!(
                    "capture2541: v_root={}, v_materialized={}",
                    inputs.roots().contains(&capture::object("dbo", "v")),
                    inputs
                        .captured()
                        .bound_objects(&capture::object("dbo", "v"))
                        .is_some()
                );
            }

            let error = match hidden {
                Ok(_) => panic!("an unreadable candidate must not disappear"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains(&login)
                    && error.contains("DENY VIEW DEFINITION")
                    && error.contains("hidden major_id=")
                    && error.contains("pbps principal only")
                    && error.contains("separate catalog-reader account"),
                "{error}"
            );
            // Restoring visibility makes the same requested domain readable.
            conn.execute(&format!(
                "REVOKE VIEW DEFINITION ON OBJECT::dbo.v FROM [{login}];"
            ))
            .await
            .unwrap();
            let recovered =
                capture::required_externals(&mut reader, &[&schema.desired], &capture_key()).await;
            assert!(
                recovered
                    .unwrap()
                    .roots()
                    .contains(&capture::object("dbo", "v"))
            );
            drop(reader);
            // reconstruct2374::both drops this database and its dep2374_ login.
            let missing = declared(ModuleKind::View, "dbo", "SELECT i FROM absent2541");
            let absent =
                capture::required_externals(&mut conn, &[&missing.desired], &capture_key())
                    .await
                    .unwrap();
            assert!(
                !absent
                    .roots()
                    .contains(&capture::object("dbo", "absent2541"))
            );
            let error = observe(
                target,
                scratch,
                &Declared {
                    base: schema.base,
                    desired: missing.desired,
                },
                None,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(error.contains("absent2541"), "{error}");
        })
    }
    reconstruct2374::both(case).await;
}

#[path = "chained2541_visibility_tests.rs"]
mod visibility;
