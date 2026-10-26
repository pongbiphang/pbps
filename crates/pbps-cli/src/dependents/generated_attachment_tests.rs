//! Attachments consume complete shapes and can introduce standing rows.

use super::*;
use pbps_model::{Column, Module, PartitionBound, PartitionBy, PartitionOf, RiskClass, Table};

fn fixture(placeholder: bool, generated: bool) -> (ChangeSet, Schema, IdsFile, Schema, IdsFile) {
    let parent = TableName::new("app", "a");
    let child = TableName::new("app", "child");
    let mut table = Table::default();
    for name in ["id", "g", "z"] {
        table
            .columns
            .insert(name.into(), Column::new("integer".parse().unwrap()));
    }
    table.columns["g"].value_source = ValueSource::Generated(Generated {
        expression: if !generated {
            "id * 2"
        } else if placeholder {
            "app.f(z)"
        } else {
            "app.f(id)"
        }
        .into(),
        stored: true,
    });
    let mut standing = table.clone();
    standing.columns["g"].value_source = ValueSource::Generated(Generated {
        expression: "id * 3".into(),
        stored: true,
    });
    let before = Schema {
        tables: [(child.clone(), standing)].into(),
        ..Default::default()
    };
    let mut before_ids = identities(&before);
    table.partition_by = Some(PartitionBy {
        columns: vec!["id".into()],
        strategy: Default::default(),
        external: false,
    });
    let of = PartitionOf {
        parent: parent.clone(),
        bound: PartitionBound::Default,
        columns: Default::default(),
    };
    let mut schema = Schema {
        tables: [
            (parent.clone(), table.clone()),
            (
                child.clone(),
                Table {
                    partition_of: Some(of),
                    ..Default::default()
                },
            ),
        ]
        .into(),
        ..Default::default()
    };
    if generated {
        schema.modules.insert(
            "app.f(integer)".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2".into(),
                options: Default::default(),
            },
        );
    }
    let ids = identities(&schema);
    before_ids.tables = [(ids.table_uid(&child).unwrap().clone(), child.clone())].into();
    let cs = ChangeSet {
        changes: [
            Change::CreateTable {
                uid: ids.table_uid(&parent).unwrap().clone(),
                name: parent,
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            },
            Change::AttachPartition {
                uid: ids.table_uid(&child).unwrap().clone(),
                table: child,
                parent: TableName::new("app", "a"),
                bound: PartitionBound::Default,
                key: vec!["id".into()],
                shape: Box::new(schema.tables[&TableName::new("app", "child")].clone()),
            },
        ]
        .into_iter()
        .chain(
            schema
                .modules
                .iter()
                .map(|(id, module)| Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                }),
        )
        .chain(generated.then(|| Change::PublicExecution {
            routine: "app.f(integer)".parse().unwrap(),
            access: pbps_model::PublicAccess::Kept,
            origin: pbps_model::RoutineOrigin::Created,
        }))
        .map(PlannedChange::new)
        .collect(),
    };
    (cs, schema, ids, before, before_ids)
}

fn scheduled(
    placeholder: bool,
    generated: bool,
    pg: &(dyn crate::certified_fixture::FixtureEmission + 'static),
) -> (ChangeSet, IdsFile) {
    let (cs, schema, ids, before, before_ids) = fixture(placeholder, generated);
    let scheduled = super::super::schedule::schedule_fixture(
        pbps_diff::order::propose(cs),
        &super::super::schedule::Declarations {
            schema: &schema,
            sources: Sources {
                before: pbps_diff::Side {
                    schema: &before,
                    ids: &before_ids,
                },
                after: &ids,
            },
            module_deps: &ModuleDeps::new(),
        },
        &super::super::schedule::Facts::default(),
        pg,
    )
    .unwrap();
    (scheduled.changes.changes().clone(), ids)
}

#[test]
fn attachments_follow_complete_columns_and_unsplit_attachments_keep_their_order() {
    for placeholder in [false, true] {
        for pg in [
            pbps_pg::Postgres::new(),
            pbps_pg::Postgres::new().with_server_version(160_000),
            pbps_pg::Postgres::new().with_server_version(180_000),
        ] {
            let (cs, _) = scheduled(placeholder, true, &pg);
            let attach = cs
                .changes
                .iter()
                .position(|p| matches!(p.change, Change::AttachPartition { .. }))
                .unwrap();
            for (at, part) in cs.changes.iter().enumerate() {
                if matches!(
                    part.change,
                    Change::CreateTable { .. }
                        | Change::AddColumn { .. }
                        | Change::AlterColumnExpression { .. }
                ) {
                    assert!(at < attach, "{cs:?}");
                }
            }
            assert!(
                !cs.changes
                    .iter()
                    .any(|p| p.risks.contains(&RiskClass::Narrowing))
            );
        }
    }
    let (input, schema, ids, _, _) = fixture(false, false);
    let mut output = input.clone();
    let pg = pbps_pg::Postgres::new();
    crate::test_planning(&mut output, |plan| {
        pbps_diff::order::generated::expand(plan, &ids, &ModuleDeps::new(), &pg)
    })
    .unwrap();
    crate::test_planning(&mut output, |plan| {
        pbps_diff::order::generated::settle(plan, &schema, &ModuleDeps::new(), &pg)
    })
    .unwrap();
    assert_eq!(
        input, output,
        "an attachment into an unsplit table is unchanged"
    );
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn attached_generated_partitions_use_complete_shapes_and_preserve_recomputation_risks() {
    use pbps_db::{Conn, Driver};
    use pbps_model::{PlanBaseline, PlanOrigin, SavedPlan};
    for endpoint in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(endpoint).unwrap();
        for offline in [false, true] {
            for placeholder in [false, true] {
                let database = format!("pbps_attach1503_{:016x}", rand::random::<u64>());
                let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
                admin
                    .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
                    .await
                    .unwrap();
                let result: anyhow::Result<()> = async {
                    let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}")).await?;
                    conn.execute("CREATE SCHEMA app; CREATE TABLE app.child (id integer, g integer GENERATED ALWAYS AS (id * 3) STORED, z integer); INSERT INTO app.child(id,z) VALUES (2,5)").await?;
                    let pg: pbps_pg::Postgres = if offline { pbps_pg::Postgres::new() }
                        else { crate::engine::planning_dialect(&mut conn).await? };
                    let (cs, ids) = scheduled(placeholder, true, &pg);
                    for part in &cs.changes {
                        for sql in pg.fixture_emit(&part.change, part.strategy)? { conn.execute(&sql.sql).await?; }
                    }
                    let rows = conn.query("SELECT g FROM app.a").await?;
                    assert_eq!(rows[0].try_get::<i32>("g")?, Some(6), "installation before attachment leaves standing values alone");
                    // Re-run the reviewer's saved-plan order against the same
                    // standing child, before the real expression is installed.
                    conn.execute("ALTER TABLE app.a DETACH PARTITION app.child; DROP TABLE app.a; DROP FUNCTION app.f(integer)").await?;
                    let (mut reordered, ids) = (cs, ids);
                    let installation = reordered.changes.iter().position(|p| matches!(p.change, Change::AlterColumnExpression { .. }));
                    if let Some(install) = installation {
                        let attach = reordered.changes.iter().position(|p| matches!(p.change, Change::AttachPartition { .. })).unwrap();
                        let attachment = reordered.changes.remove(attach);
                        reordered.changes.insert(install, attachment);
                        let mut saved = SavedPlan::new(PlanOrigin::Database, "postgres", "2026-10-21T00:00:00Z", PlanBaseline {
                            time_zone: None, description: "attachment before expression".into(), checksum: "a".repeat(64), database_collation: None,
                        }, reordered, ids);
                        saved = serde_json::from_slice(&serde_json::to_vec(&saved)? )?;
                        let why = crate::validate_saved_plan(&saved, &pg).unwrap_err().to_string();
                        assert!(why.contains("inconsistent risks"), "{why}");
                        let risks = pbps_diff::order::generated::risks(&saved.changes, &saved.ids, &pg);
                        assert!(risks[install + 1].contains(&RiskClass::Narrowing));
                        for (part, risks) in saved.changes.changes.iter_mut().zip(risks) { part.risks = risks; }
                        crate::validate_saved_plan(&saved, &pg)?;
                        for part in &saved.changes.changes {
                            for sql in pg.fixture_emit(&part.change, part.strategy)? { conn.execute(&sql.sql).await?; }
                        }
                        let rows = conn.query("SELECT g FROM app.a").await?;
                        assert_eq!(rows[0].try_get::<i32>("g")?, Some(10), "SET EXPRESSION recomputes attached standing rows");
                    } else {
                        // Offline and 16 keep the whole-table path. They do
                        // not manufacture an empty-table expression part.
                        assert!(reordered.changes.iter().all(|p| !matches!(p.change, Change::AddColumn { .. })) || !placeholder);
                        assert!(!placeholder || !reordered.changes.iter().any(pbps_diff::order::generated::is_part));
                    }
                    Ok(())
                }.await;
                admin
                    .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                    .await
                    .unwrap();
                result.unwrap_or_else(|e| {
                    panic!("{endpoint}, offline={offline}, placeholder={placeholder}: {e:#}")
                });
            }
        }
    }
}
