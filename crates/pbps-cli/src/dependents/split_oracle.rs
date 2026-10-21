#![cfg(test)]
//! Frozen 160d16f5 table split, before its S7 conversion (DEC-1959.2).
use super::*;

#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn split_new_tables(
    cs: &mut ChangeSet,
    ids: &[&IdsFile],
    deps: &ModuleDeps,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<usize, String> {
    let empty = IdsFile::default();
    let generated = pbps_diff::order::generated::legacy_expand(
        cs,
        ids.first().copied().unwrap_or(&empty),
        deps,
        dialect,
    )?;
    let Some(last) = last_function_create(cs) else {
        return Ok(0);
    };
    let taken = defaults_taken_by_rows(cs);
    let functions = function_creates(cs);
    let calls = |text: &str| calls_one_of(text, &functions);
    let mut added = generated;
    for i in (0..last).rev() {
        let Change::CreateTable { name, table, .. } = &mut cs.changes[i].change else {
            continue;
        };
        let name = name.clone();
        let mut parts: Vec<Change> = Vec::new();
        let calling: Vec<String> = table
            .checks
            .iter()
            .filter(|(_, check)| calls(&check.expression))
            .map(|(n, _)| n.clone())
            .collect();
        for check in calling {
            if let Some(constraint) = table.checks.remove(&check) {
                parts.push(Change::AddCheck {
                    table: name.clone(),
                    name: check,
                    constraint,
                });
            }
        }
        let filtered: Vec<String> = table
            .indexes
            .iter()
            .filter(|(_, index)| index.holds_expression() && calls(&index_text(index)))
            .map(|(n, _)| n.clone())
            .collect();
        for index in filtered {
            if let Some(spec) = table.indexes.remove(&index) {
                // Never the clustered index, which cannot be filtered; asked
                // of the table anyway, so the split cannot say otherwise.
                let clustered = table.index_is_clustered(&index);
                parts.push(Change::AddIndex {
                    table: name.clone(),
                    name: index,
                    index: Box::new(spec),
                    clustered,
                });
            }
        }
        for (column, spec) in table.columns.iter_mut() {
            let column_ref = ColumnRef::new(name.clone(), column.clone());
            if taken.contains(&column_ref) {
                continue;
            }
            let Some(uid) = ids.iter().find_map(|ids| ids.column_uid(&column_ref)) else {
                continue;
            };
            match &mut spec.value_source {
                ValueSource::Default(default) if calls(default) => {
                    let to = std::mem::take(default);
                    spec.value_source = ValueSource::None;
                    parts.push(Change::AlterColumnDefault {
                        uid: uid.clone(),
                        column: column_ref,
                        from: None,
                        to: Some(to),
                    });
                }
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_)
                | ValueSource::Generated(_) => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        // The table's own change no longer carries what moved out of it, so
        // its risks are recomputed with the rest.
        let table_change = cs.changes[i].change.clone();
        let provenance = cs.changes[i].provenance.clone();
        cs.changes[i] = planned(table_change, dialect);
        cs.changes[i].provenance = provenance;
        added += parts.len();
        for (k, part) in parts.into_iter().enumerate() {
            cs.changes.insert(i + 1 + k, planned(part, dialect));
        }
    }
    Ok(added)
}

pub(super) fn same_plan(
    actual: &ChangeSet,
    expected: &ChangeSet,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) {
    assert_eq!(actual, expected);
    assert_eq!(
        serde_json::to_vec(actual).unwrap(),
        serde_json::to_vec(expected).unwrap()
    );
    let sql = |plan: &ChangeSet| {
        plan.changes
            .iter()
            .map(|p| {
                dialect
                    .emit(&p.change, p.strategy)
                    .map(|statements| statements.into_iter().map(|s| s.sql).collect::<Vec<_>>())
                    .map_err(|e| e.to_string())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(sql(actual), sql(expected));
    let saved = |changes: &ChangeSet| {
        pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Database,
            "postgres",
            "2026-10-22T00:00:00Z",
            pbps_model::PlanBaseline {
                description: "S7 split oracle".into(),
                checksum: "baseline".into(),
                database_collation: None,
                time_zone: None,
            },
            changes.clone(),
            Default::default(),
        )
    };
    assert_eq!(saved(actual).checksum(), saved(expected).checksum());
    // Only this rule's new diagnostic origins may differ. In particular the
    // generated-new-table marker must survive unchanged for its own schedule.
    for (now, old) in actual.changes.iter().zip(&expected.changes) {
        if now
            .provenance
            .as_ref()
            .is_some_and(|p| p.rule == "new-table-expressions")
        {
            assert!(!pbps_diff::order::generated::is_part(old));
            assert_eq!(
                now.provenance.as_ref().unwrap().role,
                pbps_model::AddedRole::SplitPart
            );
        } else {
            assert_eq!(now.provenance, old.provenance);
        }
    }
}

pub(super) fn same_expansion(
    actual: &Result<expand::Expanded, String>,
    expected: &Result<expansion_oracle::Expanded, String>,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) {
    match (actual, expected) {
        (Ok(now), Ok(old)) => {
            assert_eq!(now.rewrites, old.rewrites);
            same_plan(&now.changes, &old.changes, dialect);
        }
        (Err(now), Err(old)) => assert_eq!(now, old),
        _ => panic!("the frozen driver and the current driver disagree: {actual:?} / {expected:?}"),
    }
}

fn schema() -> Schema {
    use pbps_model::*;
    let mut table = Table::default();
    table.columns.insert(
        "id".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let mut value = Column::new("integer".parse().unwrap());
    value.value_source = ValueSource::Default("app.f(1)".into());
    table.columns.insert("v".into(), value);
    for (name, expression) in [
        ("ck_call", "app.f(id) > 0"),
        ("ck_comment", "id <> 0 /* app.f(id) */"),
    ] {
        table.checks.insert(
            name.into(),
            CheckConstraint {
                expression: expression.into(),
            },
        );
    }
    for (name, key, filter) in [
        ("ix_plain", IndexKey::Column("id".into()), None),
        (
            "ix_filter",
            IndexKey::Column("id".into()),
            Some("app.f(id) > 0".into()),
        ),
        (
            "ix_expression",
            IndexKey::Expression("app.f(id)".into()),
            None,
        ),
    ] {
        table.indexes.insert(
            name.into(),
            Index {
                columns: vec![IndexColumn {
                    key,
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
    }
    let mut other = table.clone();
    other.indexes = other
        .indexes
        .into_iter()
        .map(|(name, index)| (format!("{name}_b"), index))
        .collect();
    Schema {
        tables: [
            (TableName::new("app", "a"), table),
            (TableName::new("app", "b"), other),
        ]
        .into(),
        modules: [(
            "app.f(integer)".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2".into(),
                description: None,
            },
        )]
        .into(),
        ..Default::default()
    }
}

#[test]
fn split_parts_match_the_frozen_plan_sql_and_checksum_for_32_uid_seeds() {
    use pbps_model::*;
    for salt in 0..32 {
        for version in [None, Some(160_000), Some(170_000), Some(180_000)] {
            let mut schema = schema();
            let mut ids = super::generated_tests::identities(&schema);
            let dialect = version.map_or_else(pbps_pg::Postgres::new, |v| {
                pbps_pg::Postgres::new().with_server_version(v)
            });
            // Different UIDs must not become an accidental ordering key.
            ids.columns = ids
                .columns
                .into_values()
                .map(|name| {
                    (
                        Uid::derived(
                            UidKind::Column,
                            &(name.raw().to_string() + "-" + &salt.to_string()),
                            0,
                        ),
                        name,
                    )
                })
                .collect();
            let seeded = ids.clone();
            for variant in 0..4 {
                ids = seeded.clone();
                let mut cs = pbps_diff::diff(
                    pbps_diff::Side {
                        schema: &Schema::default(),
                        ids: &IdsFile::default(),
                    },
                    pbps_diff::Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &dialect,
                    &Hints::default(),
                )
                .unwrap()
                .changes()
                .clone();
                match variant {
                    0 => {}
                    // No UID means a callable default stays in CREATE.
                    1 => ids.columns.clear(),
                    // An in-place alteration without a rebuild has no old
                    // split obligation, even though its kind is Function.
                    2 => {
                        for p in &mut cs.changes {
                            if let Change::CreateModule { id, module } = &p.change {
                                p.change = Change::AlterModule {
                                    id: id.clone(),
                                    module: module.clone(),
                                };
                            }
                        }
                    }
                    3 => cs.changes.reverse(),
                    _ => unreachable!(),
                }
                let mut expected = cs.clone();
                let count =
                    split_new_tables(&mut expected, &[&ids], &ModuleDeps::new(), &dialect).unwrap();
                let actual = super::test_adapters::split_new_tables(
                    &mut cs,
                    &[&ids],
                    &ModuleDeps::new(),
                    &dialect,
                )
                .unwrap();
                assert_eq!(actual, count);
                same_plan(&cs, &expected, &dialect);
                let once = cs.clone();
                assert_eq!(
                    super::test_adapters::split_new_tables(
                        &mut cs,
                        &[&ids],
                        &ModuleDeps::new(),
                        &dialect
                    ),
                    Ok(0)
                );
                same_plan(&cs, &once, &dialect);
                if variant == 0 {
                    assert_eq!(count, 8);
                    let origins: Vec<_> = cs
                        .changes
                        .iter()
                        .filter_map(|p| p.provenance.as_ref())
                        .collect();
                    assert_eq!(origins.len(), 10);
                    assert!(origins.iter().all(
                        |p| p.role == AddedRole::SplitPart && p.rule == "new-table-expressions"
                    ));
                }
            }
            schema.modules.clear();
            let mut no_functions = ChangeSet {
                changes: schema
                    .tables
                    .into_iter()
                    .map(|(name, table)| {
                        PlannedChange::new(Change::CreateTable {
                            uid: Uid::derived(UidKind::Table, &name.raw().to_string(), salt),
                            name,
                            table: Box::new(table),
                            beside_default: None,
                            moves_at_most: None,
                        })
                    })
                    .collect(),
            };
            let given = no_functions.clone();
            assert_eq!(
                super::test_adapters::split_new_tables(
                    &mut no_functions,
                    &[&ids],
                    &ModuleDeps::new(),
                    &dialect
                ),
                Ok(0)
            );
            same_plan(&no_functions, &given, &dialect);
        }
    }
}

#[tokio::test]
#[ignore = "requires the pinned PostgreSQL servers"]
async fn expanded_table_checks_indexes_and_defaults_execute_on_both_servers() {
    use pbps_db::{Conn, Driver};
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(variable).expect("both PostgreSQL endpoints are required");
        let database = format!("pbps_1959_{:016x}", rand::random::<u64>());
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
            .await
            .unwrap();
        let result: anyhow::Result<()> = async {
            let mut conn =
                Conn::connect(Driver::Postgres, &format!("{server} dbname={database}")).await?;
            conn.execute("CREATE SCHEMA app").await?;
            let dialect = crate::engine::planning_dialect(&mut conn).await?;
            let schema = schema();
            let ids = super::generated_tests::identities(&schema);
            let cs = super::generated_tests::ordinary(&schema, &ids, dialect.as_ref())
                .map_err(anyhow::Error::msg)?;
            assert_eq!(
                cs.changes
                    .iter()
                    .filter_map(|p| p.provenance.as_ref())
                    .filter(|p| p.rule == "new-table-expressions")
                    .count(),
                10
            );
            conn.execute("BEGIN").await?;
            for p in &cs.changes {
                for statement in dialect.emit(&p.change, p.strategy)? {
                    conn.execute(&statement.sql).await?;
                }
            }
            for table in ["a", "b"] {
                conn.execute(&format!("INSERT INTO app.{table} (id) VALUES (3)"))
                    .await?;
                let rows = conn
                    .query(&format!("SELECT v FROM app.{table} WHERE id=3"))
                    .await?;
                assert_eq!(rows[0].try_get::<i32>("v")?, Some(2));
                let indexes = conn.query(&format!("SELECT count(*)::integer AS n FROM pg_catalog.pg_indexes WHERE schemaname='app' AND tablename='{table}'")).await?;
                assert_eq!(indexes[0].try_get::<i32>("n")?, Some(3));
                conn.execute("SAVEPOINT bad_insert").await?;
                assert!(
                    conn.execute(&format!("INSERT INTO app.{table} (id) VALUES (-1)"))
                        .await
                        .is_err()
                );
                conn.execute("ROLLBACK TO SAVEPOINT bad_insert").await?;
                let count = conn
                    .query(&format!("SELECT count(*)::integer AS n FROM app.{table}"))
                    .await?;
                assert_eq!(count[0].try_get::<i32>("n")?, Some(1));
            }
            conn.execute("COMMIT").await?;
            Ok(())
        }
        .await;
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        result.unwrap();
    }
}
