use super::*;

#[path = "../../../pbps-model/tests/support/plan_ids.rs"]
mod plan_ids;

/// Older dependency-graph fixtures state ordinary typed changes without a
/// saved plan. Supply their explicit UIDs and otherwise untouched table IDs;
/// scope-transition fixtures below pass their real baseline and ids directly.
pub(super) async fn reports(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
) -> Result<Vec<pbps_db::impact::DropReport>, pbps_db::impact::ImpactError> {
    let baseline = pbps_model::Schema {
        tables: changes
            .changes
            .iter()
            .filter_map(|p| p.change.table())
            .map(|name| (name.clone(), pbps_model::Table::default()))
            .collect(),
        ..Default::default()
    };
    let ids = plan_ids::ids(changes, &baseline);
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
    let sources = pbps_model::physical_sources::PhysicalSources::of(
        names,
        &baseline,
        &pbps_model::IdsFile::default(),
    )
    .unwrap();
    pbps_pg::impact::drop_blockers(conn, &sources).await
}

/// DETACH starts managed column identity without creating the column. Its
/// subsequent DROP must still release the FK that exists in today's catalog.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; run scripts/live-tests-pg.sh"]
async fn a_detached_column_removes_its_existing_foreign_key_before_the_referenced_table() {
    use pbps_model::{
        Change, ChangeSet, Column, IdsFile, PlannedChange, Table, TableName,
        plan_names::{At, PlanNames},
    };
    let mut db = TestDb::create("physical_detach1875").await;
    db.conn
        .execute(
            "CREATE TABLE public.referenced(id integer PRIMARY KEY);
         CREATE TABLE public.parent(k integer NOT NULL, n integer) PARTITION BY RANGE(k);
         CREATE TABLE public.child PARTITION OF public.parent DEFAULT;",
        )
        .await
        .unwrap();
    let baseline = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .unwrap()
        .schema;
    assert!(
        baseline
            .tables
            .contains_key(&"public.child".parse().unwrap())
    );
    // This FK is a catalog dependency beside the recorded tree. Its own
    // partition-local declaration is not held by the catalog model; capture
    // the real column/partition baseline before adding that dependency.
    db.conn
        .execute(
            "ALTER TABLE public.child ADD CONSTRAINT child_fk FOREIGN KEY(n)
             REFERENCES public.referenced(id);
         INSERT INTO public.referenced VALUES(7);
         INSERT INTO public.child VALUES(1,7);",
        )
        .await
        .unwrap();
    let pg = Postgres::new();
    let framing = pg.transaction_framing(&pbps_model::SessionZone::utc());
    db.conn.begin(&framing).await.unwrap();
    let parent: TableName = "public.parent".parse().unwrap();
    let child: TableName = "public.child".parse().unwrap();
    let mut shape = Table::default();
    shape.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    shape
        .columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    let changes = ChangeSet {
        changes: vec![
            Change::DetachPartition {
                uid: "t_bbbbbb".parse().unwrap(),
                table: child.clone(),
                parent: parent.clone(),
                names: vec![],
                shape: Box::new(shape),
            },
            Change::DropColumn {
                uid: "c_dddddd".parse().unwrap(),
                column: child.column("n"),
            },
            Change::DropTable {
                uid: "t_cccccc".parse().unwrap(),
                name: "public.referenced".parse().unwrap(),
                detach_from: None,
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    let ids = IdsFile {
        tables: [
            ("t_aaaaaa".parse().unwrap(), parent.clone()),
            ("t_bbbbbb".parse().unwrap(), child.clone()),
        ]
        .into(),
        columns: [
            ("c_aaaaaa".parse().unwrap(), parent.column("k")),
            ("c_bbbbbb".parse().unwrap(), parent.column("n")),
            ("c_cccccc".parse().unwrap(), child.column("k")),
        ]
        .into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes.changes, &ids).unwrap();
    assert_eq!(
        names.stored_column(&child.column("n"), At::Before(1)),
        None,
        "managed scope must not be stretched back across detach"
    );
    let mut old_ids = ids.clone();
    old_ids.columns.retain(|_, column| column.table != child);
    old_ids.tables.insert(
        "t_cccccc".parse().unwrap(),
        "public.referenced".parse().unwrap(),
    );
    old_ids.columns.insert(
        "c_eeeeee".parse().unwrap(),
        "public.referenced.id".parse().unwrap(),
    );
    let sources =
        pbps_model::physical_sources::PhysicalSources::of(names, &baseline, &old_ids).unwrap();
    let alone = ChangeSet {
        changes: vec![changes.changes[2].clone()],
    };
    let mut alone_ids = old_ids.clone();
    alone_ids.tables.remove(&"t_cccccc".parse().unwrap());
    alone_ids.columns.remove(&"c_eeeeee".parse().unwrap());
    let alone_names = PlanNames::of(&alone.changes, &alone_ids).unwrap();
    let alone_sources =
        pbps_model::physical_sources::PhysicalSources::of(alone_names, &baseline, &old_ids)
            .unwrap();
    let refused = pbps_pg::impact::drop_blockers(&mut db.conn, &alone_sources)
        .await
        .unwrap();
    assert!(
        refused[0]
            .blocking
            .iter()
            .any(|why| why.contains("child_fk")),
        "{refused:?}"
    );
    let reports = pbps_pg::impact::drop_blockers(&mut db.conn, &sources)
        .await
        .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(
        reports.iter().all(|report| report.blocking.is_empty()),
        "{reports:?}"
    );
    for p in &changes.changes {
        for statement in pg.legacy_emit(&p.change, p.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    assert_eq!(number(&mut db.conn, "SELECT k FROM public.child").await, 1);
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT count(*)::integer FROM pg_catalog.pg_attribute
         WHERE attrelid='public.child'::regclass AND attnum>0 AND NOT attisdropped"
        )
        .await,
        1
    );
    db.conn.rollback(&framing).await.unwrap();
    db.drop().await;
}
