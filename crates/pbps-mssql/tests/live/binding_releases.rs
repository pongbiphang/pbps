//! The binding-release stage must leave a separate computed rebuild for later.
use super::*;
use pbps_diff::{
    Side,
    order::{self, Schedule, Source},
};
use pbps_model::resolver::OrderReason;
use pbps_model::{Change, ChangeSet, ModuleId, PlannedChange};

#[path = "../support/binding_release_plan.rs"]
mod plan;

async fn execute(conn: &mut Conn, change: &Change) {
    for statement in Mssql.emit(change, Default::default()).unwrap() {
        conn.execute(&statement.sql).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB"]
async fn binding_release_and_an_independent_computed_rebuild_both_apply() {
    let mut db = TestDb::create("binding_computed2228").await;
    let (before, after, ids, changes) = plan::combined("z");
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    for (id, module) in &before.modules {
        execute(
            &mut db.conn,
            &Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            },
        )
        .await;
    }
    db.conn
        .execute("CREATE TABLE app.t (id int, c AS app.z(id)); INSERT INTO app.t (id) VALUES (2)")
        .await
        .unwrap();
    let z = ModuleId::Named(TableName::new("app", "z"));
    let sql = Mssql
        .emit(
            &Change::AlterModule {
                id: z.clone(),
                module: Box::new(after.modules[&z].clone()),
            },
            Default::default(),
        )
        .unwrap();
    let error = db.conn.execute(&sql[0].sql).await.unwrap_err().to_string();
    assert!(error.contains("3729"), "{error}");
    let edges = pbps_mssql::catalog::expression_edges(&mut db.conn, &[TableName::new("app", "t")])
        .await
        .unwrap();
    assert!(edges.iter().any(|e| e.from == TableName::new("app", "t")
        && e.from_column.as_ref().is_some_and(|c| c.raw() == "c")
        && e.to == TableName::new("app", "z")));
    let expected = ChangeSet {
        changes: [1, 0, 2, 3, 4].map(|i| changes.changes[i].clone()).to_vec(),
    };
    let shared = order::before_rewrites(
        changes,
        Side {
            schema: &before,
            ids: &ids,
        },
        Side {
            schema: &after,
            ids: &ids,
        },
        &Mssql,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(shared, expected);
    // Only now does the captured computed dependency order the independent
    // rebuild. It cannot constrain the earlier binding-release stage.
    let mut connected = Schedule::new(shared.changes.len());
    connected.edge(3, 2, Source::Catalog, OrderReason::Structural);
    connected.edge(2, 4, Source::Catalog, OrderReason::Structural);
    connected.keep(0, 1, "the caller releases its old binding first".into());
    let ready = connected.settle(shared).unwrap().changes;
    for PlannedChange { change, .. } in ready.changes {
        execute(&mut db.conn, &change).await;
    }
    db.conn.execute("IF app.f(2) <> 3 OR app.g(2) <> 3 OR (SELECT c FROM app.t) <> 4 THROW 51000, 'wrong combined result', 1; IF NOT EXISTS (SELECT 1 FROM sys.computed_columns WHERE object_id = OBJECT_ID(N'app.t') AND name = N'c' AND is_persisted = 1) THROW 51000, 'computed column was not rebuilt', 1;").await.unwrap();
    db.drop().await;
}
