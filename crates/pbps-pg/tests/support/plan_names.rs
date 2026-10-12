use super::*;

/// A column drop uses its current occupant's statistics; the later rename uses the
/// surviving occupant's, even though both statements mention the same name.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; run scripts/live-tests-pg.sh"]
async fn estimates_measure_each_occupant_before_and_after_name_reuse() {
    use pbps_model::{Change, IdsFile, PlannedChange, plan_names::PlanNames};
    use pbps_pg::estimate::{self, Rows};
    let mut db = TestDb::create("estimate_reuse1868").await;
    db.conn.execute("CREATE TABLE public.a(v int); INSERT INTO public.a VALUES(1),(2),(3); CREATE TABLE public.b(v int); INSERT INTO public.b VALUES(1),(2),(3),(4),(5),(6),(7); ANALYZE public.a; ANALYZE public.b;").await.unwrap();
    // The differ normally folds a column drop into its table drop. This
    // consistent typed chain exercises the consumer's statement boundary.
    let changes = [
        Change::DropColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            column: "public.a.v".parse().unwrap(),
        },
        Change::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "public.a".parse().unwrap(),
            detach_from: None,
        },
        Change::RenameTable {
            uid: "t_bbbbbb".parse().unwrap(),
            from: "public.b".parse().unwrap(),
            to: "public.a".parse().unwrap(),
            defaults: vec![],
        },
    ]
    .map(PlannedChange::new);
    let ids = IdsFile {
        tables: [("t_bbbbbb".parse().unwrap(), "public.a".parse().unwrap())].into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let mut estimates = estimate::planned_estimates(&names);
    assert_eq!(
        estimates.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        [0, 2]
    );
    for ((_, e), rows) in estimates.iter_mut().zip([3, 7]) {
        estimate::against(&mut db.conn, e).await.unwrap();
        assert_eq!(e.rows, Some(Rows::Estimated(rows)), "{}", e.about);
    }
    let pg = Postgres::new();
    db.conn.begin(pg.transaction_framing()).await.unwrap();
    let reports = pbps_pg::impact::drop_blockers(
        &mut db.conn,
        &pbps_model::ChangeSet {
            changes: changes.to_vec(),
        },
    )
    .await
    .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().all(|r| r.blocking.is_empty()));
    for p in &changes {
        for statement in pg.emit(&p.change, p.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    db.conn.commit(pg.transaction_framing()).await.unwrap();
    let rows = db
        .conn
        .query("SELECT COUNT(*) AS n FROM public.a")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(7));
    db.drop().await;
}

/// Ownership starts at DETACH, while the physical CHECK and column already
/// exist. Losing the first spelling would silently report a known scan cost.
#[tokio::test]
#[ignore = "needs a live PostgreSQL; run scripts/live-tests-pg.sh"]
async fn a_detached_columns_estimate_keeps_its_catalog_spelling_after_rename() {
    use pbps_model::{
        Change, Column, IdsFile, PlannedChange, Table, TableName, plan_names::PlanNames,
    };
    use pbps_pg::estimate::{self, Reads, Rows};
    let mut db = TestDb::create("estimate_detach1868").await;
    db.conn.execute("CREATE TABLE public.parent(k integer NOT NULL, v integer) PARTITION BY RANGE(k); CREATE TABLE public.child PARTITION OF public.parent DEFAULT; ALTER TABLE public.child ADD CONSTRAINT child_check CHECK(v > 0); INSERT INTO public.child VALUES(1,7); ANALYZE public.child").await.unwrap();
    let child: TableName = "public.child".parse().unwrap();
    let parent: TableName = "public.parent".parse().unwrap();
    let mut shape = Table::default();
    shape.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    shape
        .columns
        .insert("v".into(), Column::new("integer".parse().unwrap()));
    let changes = [
        Change::DetachPartition {
            uid: "t_bbbbbb".parse().unwrap(),
            table: child.clone(),
            parent: parent.clone(),
            names: vec![],
            shape: Box::new(shape),
        },
        Change::RenameColumn {
            uid: "c_dddddd".parse().unwrap(),
            table: child.clone(),
            from: "v".into(),
            to: "amount".into(),
            table_was: None,
        },
        Change::AlterColumnNullability {
            uid: "c_dddddd".parse().unwrap(),
            column: child.column("amount"),
            ty: "integer".parse().unwrap(),
            to_nullable: false,
            through_external_partitions: false,
            collation: None,
        },
    ]
    .map(PlannedChange::new);
    let ids = IdsFile {
        tables: [
            ("t_aaaaaa".parse().unwrap(), parent.clone()),
            ("t_bbbbbb".parse().unwrap(), child.clone()),
        ]
        .into(),
        columns: [
            ("c_aaaaaa".parse().unwrap(), parent.column("k")),
            ("c_bbbbbb".parse().unwrap(), parent.column("v")),
            ("c_cccccc".parse().unwrap(), child.column("k")),
            ("c_dddddd".parse().unwrap(), child.column("amount")),
        ]
        .into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let mut estimates = estimate::planned_estimates(&names);
    let (_, cost) = estimates.iter_mut().find(|(i, _)| *i == 2).unwrap();
    estimate::against(&mut db.conn, cost).await.unwrap();
    assert_eq!(cost.rows, Some(Rows::Estimated(1)));
    assert!(
        matches!(&cost.reads, Reads::Unknown(why) if why.contains("child_check")),
        "{cost:?}"
    );
    let pg = Postgres::new();
    db.conn.begin(pg.transaction_framing()).await.unwrap();
    for p in &changes {
        for statement in pg.emit(&p.change, p.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    db.conn.commit(pg.transaction_framing()).await.unwrap();
    let rows = db
        .conn
        .query("SELECT amount FROM public.child")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("amount").unwrap(), Some(7));
    assert!(
        db.conn
            .execute("INSERT INTO public.child VALUES(2,NULL)")
            .await
            .is_err()
    );
    db.drop().await;
}
