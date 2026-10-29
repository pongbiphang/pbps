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
