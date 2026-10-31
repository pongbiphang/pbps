//! Extended references stay library-only until the YAML contract is enabled.
use super::*;
use pbps_diff::order::{Schedule, Source, propose};
use pbps_model::object_ref::ObjectDeps;
use pbps_model::operation::Snapshot;
use pbps_model::{Change, ChangeSet, Module, ModuleKind, PlannedChange};

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn module_to_object_annotations_drive_installs_and_removals() {
    let mut db = TestDb::create("annotations2469").await;
    let engine = Postgres::new();
    db.conn.execute("BEGIN TRANSACTION").await.unwrap();
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    let empty = Schema::default();
    let empty_ids = IdsFile::default();
    let table: TableName = "app.t".parse().unwrap();
    let module = Module {
        kind: ModuleKind::View,
        definition: "SELECT x FROM app.t".into(),
        description: None,
        options: Default::default(),
    };
    let shape = Table {
        columns: [("x".into(), Column::new("integer".parse().unwrap()))]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let schema = Schema {
        tables: [(table.clone(), shape.clone())].into(),
        modules: [("app.v".parse().unwrap(), module.clone())].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
        columns: [("c_aaaaaa".parse().unwrap(), table.column("x"))].into(),
        ..Default::default()
    };
    let annotations = ObjectDeps(
        [(
            "module::app.v".parse().unwrap(),
            ["table::app.t".parse().unwrap()].into(),
        )]
        .into(),
    );
    let input = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateModule {
                id: "app.v".parse().unwrap(),
                module: Box::new(module),
            }),
            PlannedChange::new(Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: table.clone(),
                table: Box::new(shape),
                beside_default: None,
                moves_at_most: None,
            }),
        ],
    };
    let plan = propose(input)
        .with_annotations(
            Snapshot {
                schema: &empty,
                ids: &empty_ids,
            },
            Snapshot {
                schema: &schema,
                ids: &ids,
            },
            &ObjectDeps::default(),
            &annotations,
            pbps_model::operation::ModuleEdits::Rebuilt,
        )
        .unwrap();
    let mut schedule = Schedule::new(2);
    // A deliberately opposing textual guess must yield; a typed/catalog
    // contradiction is pinned separately by the library's negative tests.
    schedule.edge(
        0,
        1,
        Source::Inferred,
        pbps_model::resolver::OrderReason::Structural,
    );
    let installed = schedule.settle(plan).unwrap();
    assert_eq!(installed.sequence, [1, 0]);
    for step in &installed.changes.changes {
        for statement in engine.fixture_emit(&step.change, step.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    db.conn
        .execute("INSERT INTO app.t(x) VALUES(7)")
        .await
        .unwrap();
    let rows = db.conn.query("SELECT x FROM app.v").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(7));
    let removals = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: table,
                detach_from: None,
            }),
            PlannedChange::new(Change::DropModule {
                id: "app.v".parse().unwrap(),
                kind: ModuleKind::View,
            }),
        ],
    };
    let plan = propose(removals)
        .with_annotations(
            Snapshot {
                schema: &schema,
                ids: &ids,
            },
            Snapshot {
                schema: &empty,
                ids: &empty_ids,
            },
            &annotations,
            &ObjectDeps::default(),
            pbps_model::operation::ModuleEdits::Rebuilt,
        )
        .unwrap();
    let removed = Schedule::new(2).settle(plan).unwrap();
    assert_eq!(removed.sequence, [1, 0]);
    for step in &removed.changes.changes {
        for statement in engine.fixture_emit(&step.change, step.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    let rows = db.conn.query("SELECT CAST(COUNT(*) AS varchar(10)) AS remaining FROM information_schema.views WHERE table_schema='app' AND table_name='v'").await.unwrap();
    assert_eq!(rows[0].try_get::<&str>("remaining").unwrap(), Some("0"));
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}

#[path = "annotation_partitions.rs"]
mod partitions;

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn partition_default_annotations_follow_the_module_across_ancestor_depths() {
    for (depth, inherited) in [(1, false), (3, false), (1, true), (3, true)] {
        let mut db = TestDb::create(&format!("annotation_partition_{depth}_{inherited}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        let fallback = if inherited { "DEFAULT 1" } else { "" };
        db.conn
            .execute(&format!(
                "CREATE TABLE app.root(x integer {fallback}) PARTITION BY RANGE(x)"
            ))
            .await
            .unwrap();
        let mut parent = "app.root".to_owned();
        for at in 0..depth - 1 {
            let name = format!("app.middle_{at}");
            db.conn
                .execute(&format!(
                    "CREATE TABLE {name} PARTITION OF {parent} DEFAULT PARTITION BY RANGE(x)"
                ))
                .await
                .unwrap();
            parent = name;
        }
        let scene = partitions::Scene::new(depth);
        let scene = if inherited {
            scene.with_root_default()
        } else {
            scene
        };
        let scene = scene.with_existing_ancestors();
        let engine = Postgres::new();
        let plan = scene.plan(&engine).unwrap();
        let module = plan
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::CreateModule { .. }))
            .unwrap();
        let default = plan
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::SetPartitionDefault { .. }))
            .unwrap();
        assert!(module < default);
        let mut reconstruction = pbps_pg::resolver::reconstruct::Reconstruction::new(
            &engine,
            &scene.input().unwrap(),
            &scene.ids,
        )
        .unwrap();
        reconstruction
            .compile(&engine, &pbps_model::SessionZone::utc(), &mut db.conn)
            .await
            .unwrap();
        assert_eq!(
            reconstruction.observations().unwrap().len(),
            reconstruction.len()
        );
        db.conn
            .execute("INSERT INTO app.child DEFAULT VALUES")
            .await
            .unwrap();
        let rows = db.conn.query("SELECT x FROM app.child").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(7));
        let rows = db.conn.query("SELECT count(*)::text AS bindings FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_attrdef a ON d.classid='pg_catalog.pg_attrdef'::regclass AND d.objid=a.oid WHERE a.adrelid='app.child'::regclass AND d.refclassid='pg_catalog.pg_proc'::regclass AND d.refobjid='app.f()'::regprocedure").await.unwrap();
        assert_eq!(rows[0].try_get::<&str>("bindings").unwrap(), Some("1"));
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn rebuilt_provider_waits_for_annotated_consumer_removal() {
    let mut db = TestDb::create("annotation_provider_availability").await;
    let engine = Postgres::new();
    db.conn.execute("BEGIN").await.unwrap();
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    db.conn
        .execute("CREATE VIEW app.provider AS SELECT 1 AS x")
        .await
        .unwrap();
    db.conn
        .execute("CREATE VIEW app.consumer AS SELECT x FROM app.provider")
        .await
        .unwrap();
    let module = |definition: &str| Module {
        kind: ModuleKind::View,
        definition: definition.into(),
        description: None,
        options: Default::default(),
    };
    let before = Schema {
        modules: [
            ("app.provider".parse().unwrap(), module("SELECT 1 AS x")),
            (
                "app.consumer".parse().unwrap(),
                module("SELECT x FROM app.provider"),
            ),
        ]
        .into(),
        ..Default::default()
    };
    let mut after = before.clone();
    after.modules.remove(&"app.consumer".parse().unwrap());
    after
        .modules
        .insert("app.provider".parse().unwrap(), module("SELECT 2 AS x"));
    let ids = IdsFile::default();
    let base = ObjectDeps(
        [(
            "app.consumer".parse().unwrap(),
            ["app.provider".parse().unwrap()].into(),
        )]
        .into(),
    );
    let input = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::AlterModule {
                id: "app.provider".parse().unwrap(),
                module: Box::new(module("SELECT 2 AS x")),
            }),
            PlannedChange::new(Change::DropModule {
                id: "app.consumer".parse().unwrap(),
                kind: ModuleKind::View,
            }),
        ],
    };
    let annotated = propose(input)
        .with_annotations(
            Snapshot {
                schema: &before,
                ids: &ids,
            },
            Snapshot {
                schema: &after,
                ids: &ids,
            },
            &base,
            &ObjectDeps::default(),
            pbps_model::operation::ModuleEdits::Rebuilt,
        )
        .unwrap();
    let edges = annotated.annotation_edges().unwrap();
    assert!(edges.iter().any(|e| e.before == 1 && e.after == 0));
    let settled = Schedule::new(2).settle(annotated).unwrap();
    assert_eq!(settled.sequence, [1, 0]);
    let mut sql = Vec::new();
    for step in &settled.changes.changes {
        for statement in engine.fixture_emit(&step.change, step.strategy).unwrap() {
            sql.push(statement.sql.clone());
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    assert!(sql[0].contains("DROP VIEW") && sql[0].contains("consumer"));
    assert!(sql[1].contains("DROP VIEW") && sql[1].contains("provider"));
    assert!(sql[2].contains("CREATE VIEW") && sql[2].contains("provider"));
    let rows = db
        .conn
        .query("SELECT CAST(x AS text) AS x FROM app.provider")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<&str>("x").unwrap(), Some("2"));
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}

#[path = "annotation_meetings.rs"]
mod meetings;

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn an_annotated_partition_column_meeting_keeps_its_edge_and_values() {
    let mut db = TestDb::create("annotation_meeting2296").await;
    db.conn
        .execute(
            "BEGIN; CREATE SCHEMA app;
        CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$;
        CREATE TABLE app.p(k integer NOT NULL,d integer DEFAULT app.f(1)) PARTITION BY RANGE(k);
        CREATE TABLE app.a(k integer NOT NULL,d integer DEFAULT 9);
        INSERT INTO app.a(k) VALUES(1);",
        )
        .await
        .unwrap();
    let scene = meetings::scene();
    let version = db
        .conn
        .query("SELECT current_setting('server_version_num') AS v")
        .await
        .unwrap()[0]
        .try_get::<&str>("v")
        .unwrap()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let catalog = pbps_pg::modules::dependencies::references(
        &mut db.conn,
        Snapshot {
            schema: &scene.before,
            ids: &scene.base,
        },
        &scene.base.tables.keys().cloned().collect(),
    )
    .await
    .unwrap();
    let column = "app.p.d".parse().unwrap();
    assert_eq!(
        catalog.columns[&column].routines,
        scene.stored.columns[&column].routines
    );
    let plan = meetings::canonical(&scene, version, Some(&catalog)).unwrap();
    let engine = Postgres::new().with_server_version(version);
    for step in &plan.changes {
        for statement in engine.fixture_emit(&step.change, step.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    let rows = db
        .conn
        .query("SELECT d::text AS d, pg_typeof(d)::text AS ty FROM app.v")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get::<&str>("d").unwrap(), Some("9"));
    assert_eq!(rows[0].try_get::<&str>("ty").unwrap(), Some("bigint"));
    db.conn
        .execute("INSERT INTO app.a(k) VALUES(2)")
        .await
        .unwrap();
    let rows = db
        .conn
        .query("SELECT d::text AS d FROM app.a WHERE k=2")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<&str>("d").unwrap(), Some("9"));
    let rows = db.conn.query("SELECT inhparent::regclass::text AS parent FROM pg_inherits WHERE inhrelid='app.a'::regclass").await.unwrap();
    assert_eq!(rows[0].try_get::<&str>("parent").unwrap(), Some("app.p"));
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}
