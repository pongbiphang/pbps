//! Native chained staging, on supplied and Docker scratch (#2376).
use super::*;
use pbps_model::{Column, ForeignKey, PrimaryKey, ReferentialAction, Table, TableName};
use pbps_mssql::resolver::capture;
use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;

const SEED: &str = "CREATE TABLE dbo.a(i int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.managed(i int CONSTRAINT managed_pk PRIMARY KEY,CONSTRAINT to_a FOREIGN KEY(i) REFERENCES dbo.a(i) ON DELETE CASCADE); CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY); ALTER TABLE dbo.x ADD CONSTRAINT x_managed FOREIGN KEY(i) REFERENCES dbo.managed(i) ON DELETE CASCADE;";
const EARLY: &str = "CREATE TABLE dbo.a(i int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY);";
const DEFERRED: &str = "ALTER TABLE dbo.x ADD CONSTRAINT x_managed FOREIGN KEY(i) REFERENCES dbo.managed(i) ON DELETE CASCADE;";

fn declared() -> Declared {
    let mut column = Column::new("int".parse().unwrap());
    column.nullable = false;
    let mut table = Table {
        columns: [("i".into(), column)].into(),
        primary_key: Some(PrimaryKey {
            name: Some("managed_pk".into()),
            columns: vec!["i".into()],
            storage_parameters: Default::default(),
        }),
        ..Default::default()
    };
    table.foreign_keys.insert(
        "to_a".into(),
        ForeignKey {
            columns: vec!["i".into()],
            references_table: TableName::new("dbo", "a"),
            references_columns: vec!["i".into()],
            on_delete: ReferentialAction::Cascade,
            on_update: ReferentialAction::NoAction,
        },
    );
    let schema = Schema {
        tables: [(TableName::new("dbo", "managed"), table)].into(),
        ..Default::default()
    };
    Declared {
        base: schema.clone(),
        desired: schema,
    }
}

async fn observe(
    target: &str,
    scratch: &str,
    declared: &Declared,
    file: Option<&str>,
    dependencies: &pbps_model::dependency::Dependencies,
) -> Result<
    (
        BTreeSet<capture::ObjectIdentity>,
        BTreeMap<pbps_model::dependency::DependencyName, pbps_model::resolver::DependencyPlacement>,
    ),
    Error,
> {
    let ids = pbps_diff::resolve(
        &declared.desired,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "chained2376".into(),
            today: "2026-10-28".into(),
        },
    )
    .unwrap()
    .ids;
    let empty = (Schema::default(), IdsFile::default());
    let bootstrap = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty.0,
            ids: &empty.1,
        },
        pbps_diff::Side {
            schema: &declared.desired,
            ids: &ids,
        },
        &pbps_mssql::Mssql,
        &Hints::default(),
    )
    .unwrap();
    let request = super::super::super::ScopeRequest {
        schemas: vec!["dbo".into()],
        ..Default::default()
    };
    let binding = BindingRequest {
        bootstrap: &bootstrap,
        base: &declared.base,
        desired: &declared.desired,
        dependencies,
    };
    let key = capture_key();
    let mut source = connection(target).await;
    let identity = sql::cluster_identity(&mut source).await.unwrap();
    let (catalog, authorization) = crate::resolver::scope::read_target(
        &mut source,
        Driver::Mssql,
        &request.schemas,
        &[],
        &request.schemas,
        &[],
    )
    .await
    .unwrap();
    let mut admin = connection(scratch).await;
    let tokens = super::super::Tokens::generate();
    let provenance = provenance();
    let (provisioning, mut state) = super::super::engine::scratch(
        Driver::Mssql,
        &mut source,
        &mut admin,
        &tokens,
        &identity,
        scratch,
        &request,
        &Standard::Mssql,
        file,
        &provenance,
    )
    .await
    .map_err(|e| Error::Binding(e.to_string()))?;
    let result = async {
        let mut prepared = state
            .prepare(
                provisioning,
                &mut admin,
                &mut source,
                &catalog,
                &authorization,
                &request.schemas,
                &binding,
                &[],
                Some(&key),
            )
            .await?;
        let mut reconstruction = state
            .reconstruction(
                &mut source,
                &binding,
                &ids,
                &Default::default(),
                &mut prepared,
            )
            .await?;
        let mut staged = super::super::engine::staged(Driver::Mssql, &prepared)?;
        reconstruction
            .compile(
                &mut prepared,
                &mut admin,
                &mut source,
                &[],
                None,
                provenance.outside_writes,
                &mut staged,
            )
            .await?;
        let placements = prepared.dependency_placements();
        let super::super::engine::Prepared::Mssql(prepared) = prepared else {
            unreachable!()
        };
        let current =
            capture::capture_declared(&mut source, prepared.staged.scope(), dependencies, &key)
                .await
                .map_err(|e| Error::Binding(format!("target recheck {e:?}")))?;
        let changed = prepared.staged.changed_since(&current);
        assert!(changed.is_empty(), "{changed:?}");
        let roots = prepared.staged.compared().clone();
        Ok((roots, placements))
    }
    .await;
    state
        .release(&mut admin)
        .await
        .map_err(|e| Error::Binding(format!("cleanup {e:?}")))?;
    result
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_native_chain_is_compared_and_merged_for_renderer_and_deferred_file() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target).await.execute(SEED).await.unwrap();
            let schema = declared();
            for file in [None, Some(format!("{EARLY}\n-- pbps:deferred\n{DEFERRED}"))] {
                let (roots, _) = observe(
                    target,
                    scratch,
                    &schema,
                    file.as_deref(),
                    &Default::default(),
                )
                .await
                .unwrap();
                assert!(
                    roots.contains(&capture::object("dbo", "x_managed")),
                    "{roots:?}"
                );
                assert!(roots.contains(&capture::object("dbo", "a")));
                assert!(!roots.contains(&capture::object("dbo", "managed")));
                let outcome =
                    declared_refusal(target, scratch, &schema, file.as_deref(), &provenance())
                        .await
                        .to_string();
                assert!(
                    outcome.contains("planning from evidence") && outcome.contains("#2143"),
                    "{outcome}"
                );
            }
            connection(target)
                .await
                .execute("CREATE SCHEMA ext2376 AUTHORIZATION dbo;")
                .await
                .unwrap();
            connection(target)
                .await
                .execute(
                    "CREATE VIEW ext2376.chain_view2376 AS SELECT NULLIF(i,0) AS i FROM dbo.managed;",
                )
                .await
                .unwrap();
            connection(target)
                .await
                .execute("CREATE VIEW dbo.consumer2376 AS SELECT i FROM ext2376.chain_view2376;")
                .await
                .unwrap();
            let mut consumed = declared();
            consumed.desired.modules.insert(
                "dbo.consumer2376".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::View,
                    definition: "SELECT i FROM ext2376.chain_view2376".into(),
                    description: None,
                    options: Default::default(),
                },
            );
            consumed.base = consumed.desired.clone();
            // The unrelated suffix cannot hold the renderer behind its consumer.
            for file in [None, Some(format!("{EARLY}\n-- pbps:deferred\nSELECT 1;"))] {
                let (roots, _) = observe(
                    target,
                    scratch,
                    &consumed,
                    file.as_deref(),
                    &Default::default(),
                )
                .await
                .unwrap();
                assert!(
                    roots.contains(&capture::object("ext2376", "chain_view2376")),
                    "{roots:?}"
                );
                assert!(
                    roots.contains(&capture::object("dbo", "x_managed")),
                    "{roots:?}"
                );
            }
            connection(target)
                .await
                .execute(
                    "CREATE VIEW ext2376.relay_view2376 AS SELECT i FROM ext2376.chain_view2376;",
                )
                .await
                .unwrap();
            connection(target)
                .await
                .execute("ALTER VIEW dbo.consumer2376 AS SELECT i FROM ext2376.relay_view2376;")
                .await
                .unwrap();
            consumed
                .desired
                .modules
                .get_mut(&"dbo.consumer2376".parse().unwrap())
                .unwrap()
                .definition = "SELECT i FROM ext2376.relay_view2376".into();
            consumed.base = consumed.desired.clone();
            // Both renderer ancestors must precede the indirect managed
            // consumer even though the first has no direct managed reader.
            for file in [None, Some(format!("{EARLY}\n-- pbps:deferred\nSELECT 1;"))] {
                let (roots, _) = observe(
                    target,
                    scratch,
                    &consumed,
                    file.as_deref(),
                    &Default::default(),
                )
                .await
                .unwrap();
                for view in ["chain_view2376", "relay_view2376"] {
                    assert!(
                        roots.contains(&capture::object("ext2376", view)),
                        "{roots:?}"
                    );
                }
            }
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chain_missing_managed_shape_template_or_matching_shape_refuses_by_name() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target).await.execute(SEED).await.unwrap();
            let mut dropped = declared();
            dropped.desired = Schema::default();
            let error = observe(target, scratch, &dropped, None, &Default::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("x_managed") && error.contains("which reconstruction never creates"),
                "{error}"
            );
            let mut alias = declared();
            let table = alias
                .desired
                .tables
                .remove(&TableName::new("dbo", "managed"))
                .unwrap();
            alias
                .desired
                .tables
                .insert(TableName::new("dbo", "MANAGED"), table);
            // DEC-2268.1 compares stored spelling, even under a CI collation.
            let error = observe(target, scratch, &alias, None, &Default::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("x_managed") && error.contains("which reconstruction never creates"),
                "{error}"
            );
            let wrong = format!(
                "{EARLY}\n-- pbps:deferred\n{}",
                DEFERRED.replace("ON DELETE CASCADE", "ON DELETE NO ACTION")
            );
            let error = observe(
                target,
                scratch,
                &declared(),
                Some(&wrong),
                &Default::default(),
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("x_managed") && error.contains("foreign_key"),
                "{error}"
            );
            connection(target).await.execute("CREATE VIEW dbo.unrenderable2376 WITH SCHEMABINDING AS SELECT i FROM dbo.managed;").await.unwrap();
            connection(target)
                .await
                .execute("CREATE VIEW dbo.consumer2376 AS SELECT i FROM dbo.unrenderable2376;")
                .await
                .unwrap();
            let mut missing = declared();
            missing.desired.modules.insert(
                "dbo.consumer2376".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::View,
                    definition: "SELECT i FROM dbo.unrenderable2376".into(),
                    description: None,
                    options: Default::default(),
                },
            );
            missing.base = missing.desired.clone();
            let error = observe(target, scratch, &missing, None, &Default::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("unrenderable2376") && error.contains("template"),
                "{error}"
            );
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_deferred_duplicate_marker_and_failed_statement_refuse_and_cleanup() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            connection(target).await.execute(SEED).await.unwrap();
            for (file, expected) in [
                (
                    format!("{EARLY}\n-- pbps:deferred\n-- pbps:deferred\n{DEFERRED}"),
                    "second deferred marker",
                ),
                (
                    format!(
                        "{EARLY}\n-- pbps:deferred\nCREATE TABLE dbo.partial2376(i int);\nGO\nTHROW 50000, 'expected deferred failure', 1;"
                    ),
                    "deferred externals statement 2",
                ),
            ] {
                let error = observe(
                    target,
                    scratch,
                    &declared(),
                    Some(&file),
                    &Default::default(),
                )
                .await
                .unwrap_err()
                .to_string();
                assert!(error.contains(expected), "{error}");
            }
            observe(target, scratch, &declared(), None, &Default::default())
                .await
                .unwrap();
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_dependency_placement_records_real_catalog_and_declared_rehearsals() {
    use pbps_model::dependency::DependencyKind;
    use pbps_model::resolver::{DependencyPlacement, DependencyShape};
    for unreadable in [false, true] {
        dependencies2323::fixture(DependencyKind::Table, unreadable, move |case| async move {
            let dependencies =
                dependencies2323::declarations(&case.other_name, DependencyKind::Table);
            let name = dependencies.keys().next().unwrap().clone();
            let mut schema = Schema::default();
            schema.modules.insert(
                "dbo.managed2323".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::View,
                    description: None,
                    options: Default::default(),
                    definition: format!("SELECT id,name FROM [{}].dbo.dep2323", case.other_name),
                },
            );
            let declared = Declared {
                base: schema.clone(),
                desired: schema,
            };
            let (same_name, same_url) = create_database(&case.target_admin).await;
            let bound = observe(&case.target, &same_url, &declared, None, &dependencies).await;
            drop_database(&case.target_admin, &same_name).await;
            assert_eq!(bound.unwrap().1[&name], DependencyPlacement::Bound {});
            let expected = DependencyPlacement::Companion {
                shape: if unreadable {
                    DependencyShape::Declared
                } else {
                    DependencyShape::Compared
                },
            };
            let (_, placements) = observe(
                &case.target,
                &case.scratch_server,
                &declared,
                None,
                &dependencies,
            )
            .await
            .unwrap();
            assert_eq!(placements[&name], expected);
            let identity = sql::cluster_identity(&mut connection(&case.target).await)
                .await
                .unwrap();
            let image = server("PBPS_TEST_RESOLVER_MSSQL_IMAGE");
            super::super::container::with_container(
                Driver::Mssql,
                &case.target,
                &identity,
                &image,
                pbps_config::resolver::PullPolicy::Never,
                async |url: &str| {
                    let (_, placements) =
                        observe(&case.target, url, &declared, None, &dependencies)
                            .await
                            .unwrap();
                    assert_eq!(placements[&name], expected);
                    Ok(())
                },
            )
            .await
            .unwrap();
        })
        .await;
    }
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_unnamed_chained_roots_keep_the_producers_key_in_both_file_modes() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let seed = SEED.replace("CONSTRAINT x_managed ", "");
            connection(target)
                .await
                .execute(&format!("{seed} ALTER TABLE dbo.x ADD CHECK(i>=0);"))
                .await
                .unwrap();
            let deferred = DEFERRED.replace("CONSTRAINT x_managed ", "");
            let file =
                format!("{EARLY}\n-- pbps:deferred\n{deferred} ALTER TABLE dbo.x ADD CHECK(i>=0);");
            for supplied in [None, Some(file.as_str())] {
                let (roots, _) =
                    observe(target, scratch, &declared(), supplied, &Default::default())
                        .await
                        .unwrap();
                assert!(
                    roots.iter().any(|id| id.class.starts_with("pbps-unnamed-")
                        && id.signature.first() == Some(&capture::object("dbo", "x"))),
                    "{roots:?}"
                );
                if supplied.is_some() {
                    assert!(
                        roots.iter().any(|id| id.class == "pbps-unnamed-CHECK"),
                        "{roots:?}"
                    );
                }
            }
        })
    }
    reconstruct2374::both(case).await;
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_chained_views_wait_only_for_the_foreign_keys_actual_key() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            use pbps_model::{
                ComputedColumn, Index, IndexColumn, IndexKey, Module, ModuleKind, UniqueConstraint,
            };
            connection(target)
                .await
                .execute("CREATE SCHEMA ext2376 AUTHORIZATION dbo;")
                .await
                .unwrap();
            for kind in ["index", "unique", "primary", "unnamed"] {
                let function = "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM ext2376.base_ids AS v JOIN ext2376.a_peer AS w ON w.i=v.i WHERE v.i=@x); END";
                // The lexically earlier peer is independent except for UNIQUE,
                // where its real dependency must still keep it after base_ids.
                let peer = if kind == "unique" {
                    "CREATE VIEW ext2376.a_peer AS SELECT i FROM ext2376.base_ids;"
                } else {
                    "CREATE VIEW ext2376.a_peer AS SELECT NULLIF(i,0) AS i FROM dbo.managed;"
                };
                let key_sql = match kind {
                    "index" => "CREATE UNIQUE INDEX required_d ON dbo.managed(d);",
                    "unique" => "ALTER TABLE dbo.managed ADD CONSTRAINT required_d UNIQUE(d);",
                    "primary" => {
                        "ALTER TABLE dbo.managed ADD CONSTRAINT required_d PRIMARY KEY(d);"
                    }
                    "unnamed" => "ALTER TABLE dbo.managed ADD PRIMARY KEY(d);",
                    _ => unreachable!(),
                };
                // This is a real legal creation order. The view reads only i;
                // the key over later d is needed by x_required, not the view.
                for statement in [
                    "CREATE TABLE dbo.a(i int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.managed(i int NOT NULL,CONSTRAINT to_a FOREIGN KEY(i) REFERENCES dbo.a(i) ON DELETE CASCADE);".to_owned(),
                    "CREATE VIEW ext2376.base_ids AS SELECT NULLIF(i,0) AS i FROM dbo.managed;".to_owned(),
                    peer.to_owned(),
                    format!("CREATE FUNCTION dbo.f {function}"),
                    format!("ALTER TABLE dbo.managed ADD c AS dbo.f(i); ALTER TABLE dbo.managed ADD d AS (i + 1) PERSISTED NOT NULL; {key_sql} CREATE INDEX unrelated_d ON dbo.managed(d); CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY,CONSTRAINT x_required FOREIGN KEY(i) REFERENCES dbo.managed(d) ON DELETE CASCADE);"),
                ] {
                    connection(target).await.execute(&statement).await.unwrap();
                }
                let mut schema = declared();
                let table = schema
                    .desired
                    .tables
                    .get_mut(&TableName::new("dbo", "managed"))
                    .unwrap();
                table.primary_key = None;
                table.computed.insert(
                    "c".into(),
                    ComputedColumn {
                        expression: "dbo.f(i)".into(),
                        persisted: false,
                        not_null: false,
                    },
                );
                table.computed.insert(
                    "d".into(),
                    ComputedColumn {
                        expression: "i + 1".into(),
                        persisted: true,
                        not_null: true,
                    },
                );
                let index = |unique| Index {
                    columns: vec![IndexColumn {
                        key: IndexKey::Column("d".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                };
                table.indexes.insert("unrelated_d".into(), index(false));
                match kind {
                    "index" => {
                        table.indexes.insert("required_d".into(), index(true));
                    }
                    "unique" => {
                        table.unique.insert(
                            "required_d".into(),
                            UniqueConstraint {
                                columns: vec!["d".into()],
                                storage_parameters: Default::default(),
                            },
                        );
                    }
                    "primary" | "unnamed" => {
                        table.primary_key = Some(PrimaryKey {
                            name: (kind == "primary").then(|| "required_d".into()),
                            columns: vec!["d".into()],
                            storage_parameters: Default::default(),
                        });
                    }
                    _ => unreachable!(),
                }
                schema.desired.modules.insert(
                    "dbo.f()".parse().unwrap(),
                    Module {
                        kind: ModuleKind::Function,
                        definition: function.into(),
                        description: None,
                        options: Default::default(),
                    },
                );
                schema.base = schema.desired.clone();
                // This pins the native scratch route. Ordinary diff still
                // refuses function-backed computed creation before that route.
                let ids = pbps_diff::resolve(
                    &schema.desired,
                    &IdsFile::default(),
                    &[],
                    &pbps_diff::Context {
                        operator: "chained-key-regression".into(),
                        today: "2026-10-29".into(),
                    },
                )
                .unwrap()
                .ids;
                let table_name = TableName::new("dbo", "managed");
                let function_id: pbps_model::ModuleId = "dbo.f()".parse().unwrap();
                let plan = pbps_diff::order::propose(pbps_model::ChangeSet {
                    changes: vec![
                        pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "managed", 0),
                            name: table_name.clone(),
                            table: Box::new(schema.desired.tables[&table_name].clone()),
                            beside_default: None,
                            moves_at_most: None,
                        }),
                        pbps_model::PlannedChange::new(pbps_model::Change::CreateModule {
                            id: function_id.clone(),
                            module: Box::new(schema.desired.modules[&function_id].clone()),
                        }),
                    ],
                });
                let inputs = capture::required_externals(
                    &mut connection(target).await,
                    &[&schema.desired],
                    &capture_key(),
                )
                .await
                .unwrap();
                let external_prerequisites = inputs.external_prerequisites();
                let reconstruction =
                    pbps_mssql::resolver::reconstruct::Reconstruction::with_chained(
                        &pbps_mssql::Mssql,
                        &plan,
                        &ids,
                        &Default::default(),
                        Some(&inputs),
                        false,
                    )
                    .unwrap_or_else(|e| panic!("{kind}: {e}"));
                let roots: BTreeSet<_> = (0..reconstruction.len())
                    .filter_map(|at| match reconstruction.auxiliary(at) {
                        Some(pbps_mssql::resolver::reconstruct::AuxiliaryStep::Rendered(id)) => {
                            Some(id.clone())
                        }
                        _ => None,
                    })
                    .collect();
                let (scratch_name, scratch_url) = create_database(scratch).await;
                let result = async {
                    let mut setup = connection(&scratch_url).await;
                    setup
                        .execute("CREATE SCHEMA ext2376 AUTHORIZATION dbo;")
                        .await
                        .map_err(|e| e.to_string())?;
                    setup.execute(EARLY).await.map_err(|e| e.to_string())?;
                    for at in 0..reconstruction.len() {
                        if reconstruction.auxiliary(at).is_some() {
                            reconstruction.compile_external(&mut setup, at).await
                        } else {
                            reconstruction.compile_steps(&mut setup, at..at + 1).await
                        }
                        .map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(())
                }
                .await;
                drop_database(scratch, &scratch_name).await;
                result.unwrap_or_else(|e| panic!("{kind}: {e}"));
                assert!(
                    roots.contains(&capture::object("ext2376", "base_ids")),
                    "{kind}: {roots:?}"
                );
                assert!(
                    roots.contains(&capture::object("dbo", "x_required")),
                    "{kind}: {roots:?}"
                );
                let base = capture::object("ext2376", "base_ids");
                let peer = capture::object("ext2376", "a_peer");
                assert!(roots.contains(&peer), "{kind}: {roots:?}");
                assert!(external_prerequisites[&base].is_empty(), "{kind}");
                assert_eq!(
                    external_prerequisites[&peer],
                    if kind == "unique" {
                        BTreeSet::from([base])
                    } else {
                        BTreeSet::new()
                    },
                    "{kind}"
                );
                connection(target).await.execute("DROP TABLE dbo.x; DROP TABLE dbo.managed; DROP FUNCTION dbo.f; DROP VIEW ext2376.a_peer; DROP VIEW ext2376.base_ids; DROP TABLE dbo.a;").await.unwrap();
            }
        })
    }
    reconstruct2374::both(case).await;
}
