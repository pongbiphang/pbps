//! Extended references stay library-only until the YAML contract is enabled.
use super::*;
use pbps_diff::order::{Schedule, Source, propose};
use pbps_model::object_ref::ObjectDeps;
use pbps_model::operation::Snapshot;
use pbps_model::{Change, ChangeSet, Module, ModuleKind, PlannedChange};

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn module_to_object_annotations_drive_installs_and_removals() {
    let mut db = TestDb::create("annotations2469").await;
    let engine = Mssql;
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
            pbps_model::operation::ModuleEdits::InPlace,
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
            pbps_model::operation::ModuleEdits::InPlace,
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

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn in_place_provider_keeps_availability_and_releases_old_dependencies() {
    let mut db = TestDb::create("annotation_provider_availability").await;
    let engine = Mssql;
    db.conn.execute("BEGIN TRANSACTION").await.unwrap();
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
            pbps_model::operation::ModuleEdits::InPlace,
        )
        .unwrap();
    let edges = annotated.annotation_edges().unwrap();
    assert!(edges.is_empty());
    let settled = Schedule::new(2).settle(annotated).unwrap();
    assert_eq!(settled.sequence, [0, 1]);
    let rows = db
        .conn
        .query("SELECT CAST(OBJECT_ID('app.provider') AS varchar(20)) AS id")
        .await
        .unwrap();
    let original = rows[0].try_get::<&str>("id").unwrap().unwrap().to_owned();
    let provider = &settled.changes.changes[0];
    let sql = engine
        .fixture_emit(&provider.change, provider.strategy)
        .unwrap();
    assert_eq!(sql.len(), 1);
    assert!(sql[0].sql.contains("CREATE OR ALTER VIEW"));
    db.conn.execute(&sql[0].sql).await.unwrap();
    let rows = db
        .conn
        .query("SELECT CAST(OBJECT_ID('app.provider') AS varchar(20)) AS id")
        .await
        .unwrap();
    assert_eq!(
        rows[0].try_get::<&str>("id").unwrap(),
        Some(original.as_str())
    );
    let rows = db
        .conn
        .query("SELECT CAST(x AS varchar(20)) AS x FROM app.consumer")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<&str>("x").unwrap(), Some("2"));
    let consumer = &settled.changes.changes[1];
    for statement in engine
        .fixture_emit(&consumer.change, consumer.strategy)
        .unwrap()
    {
        db.conn.execute(&statement.sql).await.unwrap();
    }
    // Editing an in-place consumer must still release its old provider first.
    let mut released = before.clone();
    released.modules.remove(&"app.provider".parse().unwrap());
    released
        .modules
        .insert("app.consumer".parse().unwrap(), module("SELECT 3 AS x"));
    db.conn
        .execute("CREATE VIEW app.consumer AS SELECT x FROM app.provider")
        .await
        .unwrap();
    let plan = propose(ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropModule {
                id: "app.provider".parse().unwrap(),
                kind: ModuleKind::View,
            }),
            PlannedChange::new(Change::AlterModule {
                id: "app.consumer".parse().unwrap(),
                module: Box::new(module("SELECT 3 AS x")),
            }),
        ],
    })
    .with_annotations(
        Snapshot {
            schema: &before,
            ids: &ids,
        },
        Snapshot {
            schema: &released,
            ids: &ids,
        },
        &base,
        &ObjectDeps::default(),
        pbps_model::operation::ModuleEdits::InPlace,
    )
    .unwrap();
    assert!(
        plan.annotation_edges()
            .unwrap()
            .iter()
            .any(|e| e.before == 1 && e.after == 0)
    );
    let settled = Schedule::new(2).settle(plan).unwrap();
    assert_eq!(settled.sequence, [1, 0]);
    for step in &settled.changes.changes {
        for statement in engine.fixture_emit(&step.change, step.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}
