//! The pure planner consumes identities observed on a real engine, then the
//! ordinary emitter executes its sealed sequence on a separate target.

use super::*;
use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect;
use pbps_model::resolver::{BoundSurface, Surface, SurfaceResolution};
use pbps_model::{Change, ChangeSet, Column, Hints, IdsFile, Module, ModuleKind, Schema, Table};
use std::collections::BTreeMap;

fn declared(body: &str) -> Schema {
    let mut schema = Schema::default();
    let mut table = Table::default();
    let mut column = Column::new("integer".parse().unwrap());
    column.default = Some("app.f()".into());
    table.columns.insert("id".into(), column);
    table.checks.insert(
        "positive".into(),
        pbps_model::CheckConstraint {
            expression: "id >= app.f()".into(),
        },
    );
    table.indexes.insert(
        "ix".into(),
        pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: Some("id >= app.f()".into()),
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    schema.tables.insert("app.t".parse().unwrap(), table);
    schema.modules.insert(
        "app.f()".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: format!("() {body}"),
        },
    );
    schema.modules.insert(
        "app.a()".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN app.f()".into(),
        },
    );
    schema
}

fn ids(schema: &Schema, base: &IdsFile) -> IdsFile {
    pbps_diff::resolve(
        schema,
        base,
        &[],
        &pbps_diff::Context {
            operator: "614-test".into(),
            today: "2026-09-28".into(),
        },
    )
    .unwrap()
    .ids
}

fn catalog_surface(id: &ObjectIdentity, table: &pbps_model::TableName) -> Option<Surface> {
    let table = table.clone();
    match id.class.as_str() {
        "pg_proc" if id.name == ["app", "f"] || id.name == ["app", "a"] => Some(Surface::Module(
            format!("app.{}()", id.name[1]).parse().unwrap(),
        )),
        "pg_attrdef"
            if id
                .signature
                .first()
                .is_some_and(|c| c.name == ["id"] || c.name == ["n"]) =>
        {
            Some(Surface::Default(table.column(&id.signature[0].name[0])))
        }
        "pg_constraint" if id.name == ["positive"] => Some(Surface::Check {
            table,
            name: "positive".into(),
        }),
        "pg_index"
            if id
                .signature
                .first()
                .is_some_and(|i| i.name == ["app", "ix"]) =>
        {
            Some(Surface::Index {
                table,
                name: "ix".into(),
            })
        }
        _ => None,
    }
}

fn observed(captured: &CapturedInputs) -> BTreeMap<Surface, BoundSurface> {
    let table = captured
        .inputs
        .keys()
        .find(|id| id.class == "pg_class" && (id.name == ["app", "t"] || id.name == ["app", "u"]))
        .map(|id| pbps_model::TableName::new(&id.name[0], &id.name[1]))
        .unwrap_or_else(|| "app.t".parse().unwrap());
    captured
        .inputs
        .iter()
        .filter_map(|(object, input)| {
            let surface = catalog_surface(object, &table)?;
            let mut bindings: Vec<_> = input
                .bindings
                .iter()
                .map(|b| pbps_model::resolver::Binding {
                    node: b.node.clone(),
                    path: b.path.clone(),
                    target: b.target.clone(),
                })
                .collect();
            bindings.sort();
            let managed_inputs = input
                .bindings
                .iter()
                .filter_map(|b| {
                    if b.target.class == "pg_class"
                        && (b.target.name == ["app", "t"] || b.target.name == ["app", "u"])
                    {
                        Some(Surface::Table(pbps_model::TableName::new(
                            &b.target.name[0],
                            &b.target.name[1],
                        )))
                    } else if b.target.class == "column"
                        && (b.target.name == ["id"] || b.target.name == ["n"])
                        && b.target.signature.first().is_some_and(|owner| {
                            owner.name == ["app", "t"] || owner.name == ["app", "u"]
                        })
                    {
                        Some(Surface::Column(table.column(&b.target.name[0])))
                    } else if b.target.class == "pg_proc" && b.target.name == ["app", "f"] {
                        Some(Surface::Module("app.f()".parse().unwrap()))
                    } else {
                        None
                    }
                })
                .collect();
            Some((
                surface,
                BoundSurface {
                    object: object.clone(),
                    bindings,
                    managed_inputs,
                },
            ))
        })
        .collect()
}

fn resolution(before: &CapturedInputs, after: &CapturedInputs) -> Vec<SurfaceResolution> {
    let before = observed(before);
    let after = observed(after);
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|s| SurfaceResolution {
            surface: s.clone(),
            current: before.get(s).cloned(),
            desired: after.get(s).cloned(),
        })
        .collect()
}

async fn execute(conn: &mut Conn, changes: &ChangeSet) -> Result<(), pbps_db::DbError> {
    for p in &changes.changes {
        for statement in crate::Postgres::default()
            .emit(&p.change, p.strategy)
            .unwrap()
        {
            conn.execute(&statement.sql).await?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn resolver_orders_cross_kind_binding_changes() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base_connection = std::env::var(variable).unwrap();
        let token = crate::catalog::probe_token().replace('-', "_");
        let target = format!("pbps_order614_t_{token}");
        let scratch = format!("pbps_order614_s_{token}");
        let mut admin = Conn::connect(Driver::Postgres, &base_connection)
            .await
            .unwrap();
        for db in [&target, &scratch] {
            admin
                .execute(&format!("CREATE DATABASE {db}"))
                .await
                .unwrap();
        }
        let worker = format!("pbps_order614_r_{token}");
        admin
            .execute(&format!("CREATE ROLE {worker} NOLOGIN"))
            .await
            .unwrap();
        let worker_for_run = worker.clone();
        let target_connection = format!("{base_connection} dbname={target}");
        let scratch_connection = format!("{base_connection} dbname={scratch}");
        let result = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(async move {
                    exercise(target_connection, scratch_connection, worker_for_run).await
                })
                .await
            })
            .await;
        for db in [&target, &scratch] {
            admin
                .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
                .await
                .unwrap();
        }
        admin.execute(&format!("DROP ROLE {worker}")).await.unwrap();
        result.expect("cross-kind fixture failed after cleanup");
    }
}

async fn exercise(target_connection: String, scratch_connection: String, worker: String) {
    let mut target = Conn::connect(Driver::Postgres, &target_connection)
        .await
        .unwrap();
    let mut scratch = Conn::connect(Driver::Postgres, &scratch_connection)
        .await
        .unwrap();
    target.execute("CREATE SCHEMA app").await.unwrap();
    new_table_online_indexes_are_transactional(&mut target).await;
    scratch.execute("CREATE SCHEMA app; CREATE TABLE app.t(id integer NOT NULL); CREATE FUNCTION app.f() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN (SELECT count(*)::integer FROM app.t); CREATE FUNCTION app.a() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN app.f(); ALTER TABLE app.t ALTER COLUMN id SET DEFAULT app.f(); ALTER TABLE app.t ADD CONSTRAINT positive CHECK(id >= app.f()); CREATE INDEX ix ON app.t(id) WHERE id >= app.f()").await.unwrap();
    let scope = CaptureScope {
        retained: BTreeSet::new(),
        candidates: [
            CandidateClass::Routine,
            CandidateClass::Relation,
            CandidateClass::Type,
        ]
        .into_iter()
        .map(|class| CandidateSet {
            class,
            namespace: Some("app".into()),
            name: None,
        })
        .collect(),
    };
    let current = capture(&mut target, &scope).await.unwrap();
    let compiled = capture(&mut scratch, &scope).await.unwrap();
    let desired = declared(
        "RETURNS integer LANGUAGE SQL IMMUTABLE RETURN (SELECT count(*)::integer FROM app.t)",
    );
    let empty = Schema::default();
    let empty_ids = IdsFile::default();
    let wanted_ids = ids(&desired, &empty_ids);
    let base = pbps_diff::Side {
        schema: &empty,
        ids: &empty_ids,
    };
    let wanted = pbps_diff::Side {
        schema: &desired,
        ids: &wanted_ids,
    };
    let ordinary =
        pbps_diff::diff(base, wanted, &crate::Postgres::default(), &Hints::default()).unwrap();
    // The old CREATE TABLE embeds a call to the as-yet absent routine.
    target.execute("BEGIN").await.unwrap();
    assert!(execute(&mut target, &ordinary).await.is_err());
    target.execute("ROLLBACK").await.unwrap();
    let observations = resolution(&current, &compiled);
    assert_eq!(
        observations.len(),
        5,
        "routine headers/default/check/index are all observed"
    );
    let ordered = pbps_diff::resolver::plan(
        base,
        wanted,
        &Hints::default(),
        &observations,
        &crate::Postgres::default(),
    )
    .unwrap();
    ordered.proof.validate(&ordered.changes).unwrap();
    // The complete SavedPlan serializer carries the final sequence unchanged;
    // model evidence tests separately exercise the resolved envelope and seal.
    let artifact = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "postgres",
        "fixture",
        pbps_model::PlanBaseline {
            description: "empty fixture".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        ordered.changes.clone(),
        wanted_ids.clone(),
    );
    let restored: pbps_model::SavedPlan =
        serde_json::from_str(&serde_json::to_string(&artifact).unwrap()).unwrap();
    assert_eq!(restored.checksum(), artifact.checksum());
    let roundtrip = restored.changes;
    assert_eq!(roundtrip, ordered.changes);
    ordered.proof.validate(&roundtrip).unwrap();
    target.execute("BEGIN").await.unwrap();
    execute(&mut target, &roundtrip).await.unwrap();
    target.execute("COMMIT").await.unwrap();
    let now = capture(&mut target, &scope).await.unwrap();
    let actual = observed(&now);
    for o in &observations {
        assert_eq!(
            actual[&o.surface].bindings,
            o.desired.as_ref().unwrap().bindings
        );
    }
    // Replacing an input retains the same logical name. Dependents must still
    // be dropped before it, and restored only after the new function exists.
    scratch.execute("ALTER TABLE app.t ALTER COLUMN id DROP DEFAULT; ALTER TABLE app.t DROP CONSTRAINT positive; DROP INDEX app.ix; DROP FUNCTION app.a(); DROP FUNCTION app.f(); CREATE FUNCTION app.f() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 8; CREATE FUNCTION app.a() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN app.f(); ALTER TABLE app.t ALTER COLUMN id SET DEFAULT app.f(); ALTER TABLE app.t ADD CONSTRAINT positive CHECK(id >= app.f()); CREATE INDEX ix ON app.t(id) WHERE id >= app.f()").await.unwrap();
    let replacement = declared("RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 8");
    let next = capture(&mut scratch, &scope).await.unwrap();
    let observations = resolution(&now, &next);
    let replaced = pbps_diff::resolver::plan(
        wanted,
        pbps_diff::Side {
            schema: &replacement,
            ids: &wanted_ids,
        },
        &Hints::default(),
        &observations,
        &crate::Postgres::default(),
    )
    .unwrap();
    let dropping = replaced
        .changes
        .changes
        .iter()
        .position(|p| matches!(&p.change, Change::DropModule { id, .. } if id == &"app.f()".parse().unwrap()))
        .unwrap();
    assert!(
        replaced.changes.changes[..dropping]
            .iter()
            .any(|p| matches!(p.change, Change::DropCheck { .. }))
    );
    target.execute("BEGIN").await.unwrap();
    execute(&mut target, &replaced.changes).await.unwrap();
    target.execute("COMMIT").await.unwrap();
    target
        .execute("INSERT INTO app.t DEFAULT VALUES")
        .await
        .unwrap();
    assert_eq!(
        target.query("SELECT id FROM app.t").await.unwrap()[0]
            .try_get::<i32>("id")
            .unwrap(),
        Some(8)
    );
    // A recorded UID, not a guessed rename, connects the current and desired
    // expression owners. Their teardown must name the table before its rename.
    target.execute("TRUNCATE app.t").await.unwrap();
    scratch.execute("ALTER TABLE app.t RENAME TO u; ALTER TABLE app.u RENAME COLUMN id TO n; ALTER TABLE app.u ALTER COLUMN n DROP DEFAULT; ALTER TABLE app.u DROP CONSTRAINT positive; DROP INDEX app.ix; DROP FUNCTION app.a(); DROP FUNCTION app.f(); CREATE FUNCTION app.f() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 9; CREATE FUNCTION app.a() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN app.f(); ALTER TABLE app.u ALTER COLUMN n SET DEFAULT app.f(); ALTER TABLE app.u ADD CONSTRAINT positive CHECK(n >= app.f()); CREATE INDEX ix ON app.u(n) WHERE n >= app.f()").await.unwrap();
    let mut renamed = declared("RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 9");
    let t: pbps_model::TableName = "app.t".parse().unwrap();
    let u: pbps_model::TableName = "app.u".parse().unwrap();
    let mut table = renamed.tables.remove(&t).unwrap();
    let column = table.columns.shift_remove("id").unwrap();
    table.columns.insert("n".into(), column);
    table.checks.get_mut("positive").unwrap().expression = "n >= app.f()".into();
    let index = table.indexes.get_mut("ix").unwrap();
    index.columns[0].key = pbps_model::IndexKey::Column("n".into());
    index.filter = Some("n >= app.f()".into());
    renamed.tables.insert(u.clone(), table);
    let mut renamed_ids = wanted_ids.clone();
    renamed_ids.rename_table(&t, &u);
    let column_uid = wanted_ids.column_uid(&t.column("id")).unwrap();
    renamed_ids.columns.get_mut(column_uid).unwrap().name = "n".into();
    let before_rename = capture(&mut target, &scope).await.unwrap();
    let after_rename = capture(&mut scratch, &scope).await.unwrap();
    let rename = pbps_diff::resolver::plan(
        pbps_diff::Side {
            schema: &replacement,
            ids: &wanted_ids,
        },
        pbps_diff::Side {
            schema: &renamed,
            ids: &renamed_ids,
        },
        &Hints::default(),
        &resolution(&before_rename, &after_rename),
        &crate::Postgres::default(),
    )
    .unwrap();
    assert_eq!(renamed_ids.table_uid(&u), wanted_ids.table_uid(&t));
    target.execute("BEGIN").await.unwrap();
    execute(&mut target, &rename.changes)
        .await
        .expect("a UID-backed rename and routine replacement must execute together");
    target
        .execute("INSERT INTO app.u DEFAULT VALUES")
        .await
        .unwrap();
    assert_eq!(
        target.query("SELECT n AS id FROM app.u").await.unwrap()[0]
            .try_get::<i32>("id")
            .unwrap(),
        Some(9)
    );
    target.execute("ROLLBACK").await.unwrap();
    scratch
        .execute("ALTER TABLE app.u RENAME COLUMN n TO id; ALTER TABLE app.u RENAME TO t")
        .await
        .unwrap();

    // A SECURITY DEFINER trigger runs as a distinct role. Rebuilding f removes
    // that role's grant; even a superuser's INSERT then needs it restored first.
    // The runtime-bound trigger is an execution fixture, not claimed binding
    // coverage: the resolver does not pretend it analyzed its PL/pgSQL body.
    target.execute(&format!("TRUNCATE app.t; ALTER TABLE app.t ALTER COLUMN id DROP DEFAULT; ALTER TABLE app.t DROP CONSTRAINT positive; DROP INDEX app.ix; ALTER TABLE app.t ADD PRIMARY KEY(id); GRANT USAGE ON SCHEMA app TO {worker}; GRANT EXECUTE ON FUNCTION app.f() TO {worker}; CREATE FUNCTION app.guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER AS 'BEGIN PERFORM app.f(); RETURN NEW; END'; ALTER FUNCTION app.guard() OWNER TO {worker}; CREATE TRIGGER check_execution BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.guard()" )).await.unwrap();
    let mut data_base = replacement.clone();
    let table = data_base.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
    table.columns.get_mut("id").unwrap().default = None;
    table.checks.clear();
    table.indexes.clear();
    table.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    table.data = Some(pbps_model::TableData {
        mode: pbps_model::DataMode::Exact,
        rows: BTreeMap::new(),
    });
    data_base.roles.insert(
        worker.clone(),
        pbps_model::Role {
            description: None,
            grants: BTreeMap::from([(
                "app.f()".parse().unwrap(),
                BTreeSet::from([pbps_model::Permission::Execute]),
            )]),
        },
    );
    let data_ids = ids(&data_base, &wanted_ids);
    let mut data_desired = data_base.clone();
    data_desired
        .modules
        .get_mut(&"app.f()".parse().unwrap())
        .unwrap()
        .definition = "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 9".into();
    data_desired
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .data
        .as_mut()
        .unwrap()
        .rows
        .insert("9".into(), pbps_model::Row::default());
    scratch.execute("ALTER TABLE app.t ALTER COLUMN id DROP DEFAULT; ALTER TABLE app.t DROP CONSTRAINT positive; DROP INDEX app.ix; DROP FUNCTION app.a(); DROP FUNCTION app.f(); CREATE FUNCTION app.f() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 9; CREATE FUNCTION app.a() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN app.f()").await.unwrap();
    let before_data = capture(&mut target, &scope).await.unwrap();
    let after_data = capture(&mut scratch, &scope).await.unwrap();
    let data = pbps_diff::resolver::plan(
        pbps_diff::Side {
            schema: &data_base,
            ids: &data_ids,
        },
        pbps_diff::Side {
            schema: &data_desired,
            ids: &data_ids,
        },
        &Hints::default(),
        &resolution(&before_data, &after_data),
        &crate::Postgres::default(),
    )
    .unwrap();
    let insert = data
        .changes
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::InsertRow { .. }))
        .unwrap();
    let grant = data
        .changes
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::Grant { .. }))
        .unwrap();
    assert!(
        grant < insert,
        "the approved grant must precede reference data"
    );
    let mut missing_grant = data.changes.clone();
    missing_grant.changes.remove(grant);
    target.execute("BEGIN").await.unwrap();
    let error = execute(&mut target, &missing_grant)
        .await
        .unwrap_err()
        .to_string();
    target.execute("ROLLBACK").await.unwrap();
    assert!(
        error.contains("permission denied for function f"),
        "{error}"
    );
    target.execute("BEGIN").await.unwrap();
    execute(&mut target, &data.changes).await.unwrap();
    target.execute("COMMIT").await.unwrap();
    assert_eq!(
        target.query("SELECT id FROM app.t").await.unwrap()[0]
            .try_get::<i32>("id")
            .unwrap(),
        Some(9)
    );
}

// A declaration may request online work for an existing table. Ordinary table
// creation ignores that request because its indexes belong to an empty table;
// resolver extraction must preserve the same executable transaction boundary.
async fn new_table_online_indexes_are_transactional(target: &mut Conn) {
    let empty = Schema::default();
    let before_ids = IdsFile::default();
    let name: pbps_model::TableName = "app.online_new".parse().unwrap();
    let mut table = Table::default();
    table
        .columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    table.indexes.insert(
        "online_new_ix".into(),
        pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column("n".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    let mut desired = Schema::default();
    desired.tables.insert(name.clone(), table);
    let after_ids = ids(&desired, &before_ids);
    let mut hints = Hints::default();
    hints
        .strategies
        .insert(name, pbps_model::Strategy { online: true });
    let before = pbps_diff::Side {
        schema: &empty,
        ids: &before_ids,
    };
    let after = pbps_diff::Side {
        schema: &desired,
        ids: &after_ids,
    };
    let dialect = crate::Postgres::default();
    let ordinary = pbps_diff::diff(before, after, &dialect, &hints).unwrap();
    target.execute("BEGIN").await.unwrap();
    execute(target, &ordinary).await.unwrap();
    target.execute("ROLLBACK").await.unwrap();
    let ordered = pbps_diff::resolver::plan(before, after, &hints, &[], &dialect).unwrap();
    let restored: ChangeSet =
        serde_json::from_str(&serde_json::to_string(&ordered.changes).unwrap()).unwrap();
    ordered.proof.validate(&restored).unwrap();
    target.execute("BEGIN").await.unwrap();
    execute(target, &restored)
        .await
        .expect("a new table's extracted online-declared index must execute transactionally");
    for step in &restored.changes {
        assert!(
            dialect
                .emit(&step.change, step.strategy)
                .unwrap()
                .iter()
                .all(|s| s.transactional)
        );
    }
    target.execute("ROLLBACK").await.unwrap();
}

/// A default removal spelled by its column's final address runs after the
/// recorded table and column renames that give the column that address
/// (#1292), on every supported engine. A table-only rename and a kept
/// default are the controls; each ends with the declared column and default.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_final_address_default_removal_executes_after_the_column_rename() {
    let t: pbps_model::TableName = "app.t".parse().unwrap();
    let u: pbps_model::TableName = "app.u".parse().unwrap();
    let schema = |table: &pbps_model::TableName, name: &str, default: Option<&str>| {
        let mut column = Column::new("integer".parse().unwrap());
        column.default = default.map(Into::into);
        let mut declared = Table::default();
        declared.columns.insert(name.into(), column);
        let mut schema = Schema::default();
        schema.tables.insert(table.clone(), declared);
        schema
    };
    let base = schema(&t, "id", Some("1"));
    let base_ids = ids(&base, &IdsFile::default());
    let attrdef = || BoundSurface {
        object: ObjectIdentity {
            class: "pg_attrdef".into(),
            name: vec!["app".into(), "d".into()],
            signature: vec![],
        },
        bindings: vec![],
        managed_inputs: BTreeSet::new(),
    };
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base_connection = std::env::var(variable).unwrap();
        for (name, default) in [("n", None), ("id", None), ("n", Some("1"))] {
            let desired = schema(&u, name, default);
            let mut desired_ids = base_ids.clone();
            desired_ids.rename_table(&t, &u);
            let uid = base_ids.column_uid(&t.column("id")).unwrap();
            desired_ids.columns.get_mut(uid).unwrap().name = name.into();
            let observations = if default.is_some() {
                vec![
                    SurfaceResolution {
                        surface: Surface::Default(t.column("id")),
                        current: Some(attrdef()),
                        desired: None,
                    },
                    SurfaceResolution {
                        surface: Surface::Default(u.column(name)),
                        current: None,
                        desired: Some(attrdef()),
                    },
                ]
            } else {
                vec![SurfaceResolution {
                    surface: Surface::Default(u.column(name)),
                    current: Some(attrdef()),
                    desired: None,
                }]
            };
            let ordered = pbps_diff::resolver::plan(
                pbps_diff::Side {
                    schema: &base,
                    ids: &base_ids,
                },
                pbps_diff::Side {
                    schema: &desired,
                    ids: &desired_ids,
                },
                &Hints::default(),
                &observations,
                &crate::Postgres::default(),
            )
            .unwrap();
            ordered.proof.validate(&ordered.changes).unwrap();
            let token = crate::catalog::probe_token().replace('-', "_");
            let database = format!("pbps_order1292_{token}");
            let mut admin = Conn::connect(Driver::Postgres, &base_connection)
                .await
                .unwrap();
            admin
                .execute(&format!("CREATE DATABASE {database}"))
                .await
                .unwrap();
            let mut target = Conn::connect(
                Driver::Postgres,
                &format!("{base_connection} dbname={database}"),
            )
            .await
            .unwrap();
            target
                .execute("CREATE SCHEMA app; CREATE TABLE app.t (id integer DEFAULT 1)")
                .await
                .unwrap();
            target.execute("BEGIN").await.unwrap();
            let applied = execute(&mut target, &ordered.changes).await;
            target.execute("COMMIT").await.unwrap();
            let rows = target
                .query(
                    "SELECT c.relname::text AS t, a.attname::text AS n, \
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS d \
                     FROM pg_catalog.pg_attribute a \
                     JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                     JOIN pg_catalog.pg_namespace s ON s.oid = c.relnamespace \
                     LEFT JOIN pg_catalog.pg_attrdef d \
                       ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                     WHERE s.nspname = 'app' AND a.attnum > 0 AND NOT a.attisdropped",
                )
                .await;
            drop(target);
            admin
                .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
                .await
                .unwrap();
            applied.unwrap_or_else(|error| {
                panic!(
                    "{variable} {name} {default:?}: {error}: {:?}",
                    ordered.changes
                )
            });
            let rows = rows.unwrap();
            assert_eq!(rows.len(), 1, "{variable} {name} {default:?}");
            let text = |field: &str| rows[0].try_get::<&str>(field).unwrap().map(str::to_owned);
            assert_eq!(text("t").as_deref(), Some("u"));
            assert_eq!(text("n").as_deref(), Some(name));
            assert_eq!(text("d").as_deref(), default, "{variable} {name}");
        }
    }
}
