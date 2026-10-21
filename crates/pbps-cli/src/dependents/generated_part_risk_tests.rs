//! Every creation-risk recipient must precede the owner's writes (DEC-1503.4).
use pbps_dialect::Dialect;
use pbps_model::*;

fn fixture(kind: &str, writer: &str, late: bool) -> SavedPlan {
    let name = TableName::new("app", "a");
    let child = TableName::new("app", "child");
    let table_uid: Uid = "t_015030".parse().unwrap();
    let child_uid: Uid = "t_015034".parse().unwrap();
    let id_uid: Uid = "c_015031".parse().unwrap();
    let g_uid: Uid = "c_015032".parse().unwrap();
    let mut ids = IdsFile::default();
    ids.tables.insert(table_uid.clone(), name.clone());
    ids.columns.insert(id_uid, name.column("id"));
    ids.columns.insert(g_uid.clone(), name.column("g"));
    let mut table = Table::default();
    table.columns.insert(
        "id".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    if kind != "primary" {
        table.primary_key = Some(PrimaryKey {
            name: Some("a_pk".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
    }
    let create = PlannedChange::new(Change::CreateTable {
        uid: table_uid,
        name: name.clone(),
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    });
    let mut g = Column::new("integer".parse().unwrap());
    g.value_source = ValueSource::Generated(Generated {
        expression: "id * 2".into(),
        stored: true,
    });
    let installation = PlannedChange::new(Change::AddColumn {
        uid: g_uid,
        table: name.clone(),
        name: "g".into(),
        column: Box::new(g),
    });
    let part = match kind {
        "column" => {
            let uid: Uid = "c_015033".parse().unwrap();
            ids.columns.insert(uid.clone(), name.column("z"));
            Change::AddColumn {
                uid,
                table: name.clone(),
                name: "z".into(),
                column: Box::new(Column::new("integer".parse().unwrap()).not_null()),
            }
        }
        "check" => Change::AddCheck {
            table: name.clone(),
            name: "positive".into(),
            constraint: CheckConstraint {
                expression: "g > 0".into(),
            },
        },
        "primary" => Change::SetPrimaryKey {
            table: name.clone(),
            from: None,
            to: Some(PrimaryKey {
                name: Some("a_pk".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            }),
            nonclustered: false,
        },
        "unique" => Change::AddUnique {
            table: name.clone(),
            name: "a_uq".into(),
            constraint: UniqueConstraint {
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            },
            clustered: false,
        },
        "index" => Change::AddIndex {
            table: name.clone(),
            name: "a_ix".into(),
            index: Box::new(Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: true,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            }),
            clustered: false,
        },
        "foreign" => Change::AddForeignKey {
            table: name.clone(),
            name: "a_fk".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["id".into()],
                references_table: name.clone(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            }),
        },
        _ => panic!("unknown fixture part"),
    };
    let write = match writer {
        "insert" => Change::InsertRow {
            table: name.clone(),
            key_column: "id".into(),
            identity_key: false,
            key: "1".into(),
            row: if kind == "column" && !late {
                Row([("z".into(), Value::Int(3))].into())
            } else {
                Row::default()
            },
            defaults: Default::default(),
            types: Default::default(),
        },
        "update" => Change::UpdateRow {
            table: name.clone(),
            key_column: "id".into(),
            key: "1".into(),
            columns: Default::default(),
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        },
        "delete" => Change::DeleteRow {
            table: name.clone(),
            key_column: "id".into(),
            key: "1".into(),
            cause: pbps_model::change::DeleteCause::Undeclared,
            row: Default::default(),
            dropped: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        },
        "attach" => {
            ids.tables.insert(child_uid.clone(), child.clone());
            Change::AttachPartition {
                uid: child_uid,
                table: child,
                parent: name,
                bound: PartitionBound::Default,
                key: vec!["id".into()],
                shape: Box::default(),
            }
        }
        "detach" => {
            ids.tables.insert(child_uid.clone(), child.clone());
            Change::DetachPartition {
                uid: child_uid,
                table: child,
                parent: name,
                names: vec![],
                shape: Box::default(),
            }
        }
        "move" => {
            ids.tables.insert(child_uid.clone(), child.clone());
            let source = TableName::new("app", "old_default");
            ids.tables
                .insert("t_015035".parse().unwrap(), source.clone());
            Change::CreateTable {
                uid: child_uid,
                name: child,
                table: Box::new(Table {
                    partition_of: Some(PartitionOf {
                        parent: name,
                        bound: PartitionBound::Default,
                        columns: Default::default(),
                    }),
                    ..Default::default()
                }),
                beside_default: Some(source),
                moves_at_most: Some(1),
            }
        }
        _ => panic!("unknown fixture writer"),
    };
    let pg = pbps_pg::Postgres::new();
    let mut write = PlannedChange::new(write);
    write.risks = pg.change_risks(&write.change);
    let mut part = PlannedChange::new(part);
    part.risks.clear();
    let changes = if late {
        vec![create, installation, write, part]
    } else {
        vec![create, installation, part, write]
    };
    pbps_model::plan_names::PlanNames::of(&changes, &ids).unwrap();
    SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "2026-10-22T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "per-part empty-table context".into(),
            checksum: "a".repeat(64),
            database_collation: None,
        },
        ChangeSet { changes },
        ids,
    )
}

#[test]
fn every_creation_risk_recipient_requires_its_own_empty_table_position() {
    for version in [160_000, 180_000] {
        let pg = pbps_pg::Postgres::new().with_server_version(version);
        for kind in ["column", "check", "primary", "unique", "index", "foreign"] {
            for writer in ["insert", "update", "delete", "attach", "detach", "move"] {
                let mut plan: SavedPlan = serde_json::from_slice(
                    &serde_json::to_vec(&fixture(kind, writer, true)).unwrap(),
                )
                .unwrap();
                let error = crate::validate_saved_plan(&plan, &pg)
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("inconsistent risks"),
                    "{kind}, {writer}: {error}"
                );
                let risks = pbps_diff::order::generated::risks(&plan.changes, &plan.ids, &pg);
                assert!(
                    risks[1].is_empty(),
                    "the installation still precedes the write"
                );
                let intrinsic = pg.change_risks(&plan.changes.changes[3].change);
                assert!(!intrinsic.is_empty(), "{kind} must exercise an actual risk");
                assert_eq!(risks[3], intrinsic, "{kind}, {writer}");
                plan.changes.changes[3].risks = intrinsic;
                crate::validate_saved_plan(&plan, &pg).unwrap();
                let safe = fixture(kind, writer, false);
                crate::validate_saved_plan(&safe, &pg).unwrap();
                assert!(
                    pbps_diff::order::generated::risks(&safe.changes, &safe.ids, &pg)[2].is_empty()
                );
            }
        }
    }
}

#[test]
fn resolver_creation_risks_follow_the_settled_parts_and_seal_that_order() {
    use pbps_model::resolver::{BoundSurface, ObjectIdentity, Surface, SurfaceResolution};
    let schema = super::cases::shape("rows");
    let ids = super::identities(&schema);
    let name = TableName::new("app", "a");
    let function: ModuleId = "app.f(integer)".parse().unwrap();
    let mut surfaces: Vec<_> = schema
        .modules
        .keys()
        .cloned()
        .map(Surface::Module)
        .collect();
    surfaces.extend([
        Surface::Default(name.column("g")),
        Surface::Check {
            table: name.clone(),
            name: "a_ck".into(),
        },
    ]);
    let resolution: Vec<_> = surfaces
        .into_iter()
        .map(|surface| {
            let inputs = match &surface {
                Surface::Default(_) => vec![
                    Surface::Module(function.clone()),
                    Surface::Column(name.column("id")),
                ],
                Surface::Check { .. } => vec![
                    Surface::Module(function.clone()),
                    Surface::Column(name.column("g")),
                ],
                Surface::Module(id) if id.name() == "v" => vec![Surface::Table(name.clone())],
                Surface::Module(_)
                | Surface::Namespace(_)
                | Surface::Table(_)
                | Surface::Column(_)
                | Surface::Index { .. } => vec![],
            };
            SurfaceResolution {
                surface,
                current: None,
                desired: Some(BoundSurface {
                    object: ObjectIdentity {
                        class: "fixture".into(),
                        name: vec!["observed".into()],
                        signature: vec![],
                    },
                    bindings: vec![],
                    managed_inputs: inputs.into_iter().collect(),
                }),
            }
        })
        .collect();
    for version in [160_000, 180_000] {
        let pg = pbps_pg::Postgres::new().with_server_version(version);
        let planned = pbps_diff::resolver::plan_operations(
            pbps_diff::Side {
                schema: &Schema::default(),
                ids: &IdsFile::default(),
            },
            pbps_diff::Side {
                schema: &schema,
                ids: &ids,
            },
            &Hints::default(),
            &resolution,
            &pg,
        )
        .unwrap();
        let cs = &planned.ordered.changes;
        let row = cs
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::InsertRow { .. }))
            .unwrap();
        let check = cs
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::AddCheck { .. }))
            .unwrap();
        assert!(row < check);
        assert!(cs.changes[check].risks.contains(&RiskClass::Constraint));
        let expected = pbps_diff::order::generated::risks(cs, &ids, &pg);
        assert_eq!(
            cs.changes
                .iter()
                .map(|p| p.risks.clone())
                .collect::<Vec<_>>(),
            expected
        );
        planned.ordered.proof.validate(cs).unwrap();
        for (operation, part) in planned.operations.iter().zip(&cs.changes) {
            assert_eq!(operation.planned(), part);
        }
        let ordinary = super::ordinary(&schema, &ids, &pg).unwrap();
        assert!(
            ordinary
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::AddCheck { .. })
                    && p.risks.contains(&RiskClass::Constraint))
        );
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn creation_parts_after_rows_keep_their_risks_on_both_servers() {
    use pbps_db::{Conn, Driver};
    for endpoint in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(endpoint).unwrap();
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let database = format!("pbps_r4_{:016x}", rand::random::<u64>());
        admin
            .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
            .await
            .unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
            .await
            .unwrap();
        let pg = crate::engine::planning_dialect(&mut conn).await.unwrap();
        for kind in ["column", "check"] {
            for late in [false, true] {
                conn.execute("BEGIN; CREATE SCHEMA app").await.unwrap();
                let mut plan = fixture(kind, "insert", late);
                if late {
                    assert!(
                        crate::validate_saved_plan(&plan, pg.as_ref())
                            .unwrap_err()
                            .to_string()
                            .contains("inconsistent risks")
                    );
                    plan.changes.changes[3].risks =
                        pg.change_risks(&plan.changes.changes[3].change);
                }
                crate::validate_saved_plan(&plan, pg.as_ref()).unwrap();
                let result: anyhow::Result<()> = async {
                    for part in &plan.changes.changes {
                        for sql in pg.legacy_emit(&part.change, part.strategy)? {
                            conn.execute(&sql.sql).await?;
                        }
                    }
                    Ok(())
                }
                .await;
                if late && kind == "column" {
                    let error = result.expect_err("NOT NULL addition judges the existing row");
                    assert_eq!(
                        error
                            .downcast_ref::<pbps_db::DbError>()
                            .unwrap()
                            .server_error_code()
                            .as_deref(),
                        Some("23502")
                    );
                } else {
                    result.unwrap();
                    let rows = conn.query("SELECT g FROM app.a").await.unwrap();
                    assert_eq!(rows[0].try_get::<i32>("g").unwrap(), Some(2));
                }
                conn.execute("ROLLBACK").await.unwrap();
            }
        }
        drop(conn);
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
    }
}
