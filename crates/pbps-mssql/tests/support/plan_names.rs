use super::*;

/// A consistent typed chain can add and then widen a column, although the
/// differ normally folds its final type into the addition. The catalog has
/// no instance of that column to ask about before this plan runs.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_new_columns_retype_has_no_stored_key_to_probe_but_a_missing_old_one_refuses() {
    use pbps_model::{Change, PlannedChange, plan_names::PlanNames};
    let mut db = TestDb::create("new_retype1868").await;
    db.conn
        .execute("CREATE TABLE dbo.t (id int);")
        .await
        .unwrap();
    let after = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), "dbo.t".parse().unwrap())].into(),
        columns: [("c_aaaaaa".parse().unwrap(), "dbo.t.added".parse().unwrap())].into(),
        ..Default::default()
    };
    let widen = PlannedChange::new(Change::AlterColumnType {
        uid: "c_aaaaaa".parse().unwrap(),
        column: "dbo.t.added".parse().unwrap(),
        from: ty("varchar(10)"),
        to: ty("varchar(20)"),
        from_nullable: true,
        to_nullable: true,
        from_collation: None,
        to_collation: None,
        through_external_partitions: false,
    });
    let existing = [widen.clone()];
    let names = PlanNames::of(&existing, &after).unwrap();
    let missing = pbps_mssql::impact::key_drop_blockers(&mut db.conn, &names)
        .await
        .unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("absent from the readable catalog"),
        "{missing}"
    );

    let changes = [
        PlannedChange::new(Change::AddColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: "dbo.t".parse().unwrap(),
            name: "added".into(),
            column: Box::new(Column::new(ty("varchar(10)"))),
        }),
        widen,
    ];
    let names = PlanNames::of(&changes, &after).unwrap();
    assert!(
        pbps_mssql::impact::key_drop_blockers(&mut db.conn, &names)
            .await
            .unwrap()
            .is_empty()
    );
    // The engine accepts exactly the typed steps whose pre-execution catalog
    // read used to refuse the not-yet-created column.
    db.conn.begin(Mssql.transaction_framing()).await.unwrap();
    for p in &changes {
        for statement in Mssql.emit(&p.change, p.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    db.conn.commit(Mssql.transaction_framing()).await.unwrap();
    let rows = db.conn.query("SELECT max_length FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.t') AND name=N'added';").await.unwrap();
    assert_eq!(rows[0].try_get::<i16>("max_length").unwrap(), Some(20));
    db.drop().await;
}

/// Statistics and clustered-index probes belong to the table at this step,
/// including the table dropped before another one takes its name.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn layout_estimates_measure_each_occupant_before_and_after_name_reuse() {
    use pbps_model::{Change, IdsFile, PlannedChange, plan_names::PlanNames};
    use pbps_mssql::estimate::{self, Reads, Rewrite};
    let mut db = TestDb::create("estimate_reuse1868").await;
    db.conn.execute("CREATE TABLE dbo.a(v int); CREATE CLUSTERED INDEX ix ON dbo.a(v); INSERT dbo.a VALUES(1),(2),(3); CREATE TABLE dbo.b(v int); INSERT dbo.b VALUES(1),(2),(3),(4),(5),(6),(7);").await.unwrap();
    let changes = [
        Change::DropIndex {
            table: "dbo.a".parse().unwrap(),
            name: "ix".into(),
        },
        Change::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "dbo.a".parse().unwrap(),
            detach_from: None,
        },
        Change::RenameTable {
            uid: "t_bbbbbb".parse().unwrap(),
            from: "dbo.b".parse().unwrap(),
            to: "dbo.a".parse().unwrap(),
            defaults: vec![],
        },
        Change::AddIndex {
            table: "dbo.a".parse().unwrap(),
            name: "ix".into(),
            clustered: true,
            index: Box::new(pbps_model::Index {
                columns: vec![pbps_model::IndexColumn::column("v")],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            }),
        },
    ]
    .map(PlannedChange::new);
    let ids = IdsFile {
        tables: [("t_bbbbbb".parse().unwrap(), "dbo.a".parse().unwrap())].into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let mut estimates = estimate::planned_estimates(&names);
    assert_eq!(
        estimates.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        [0, 3]
    );
    for ((_, e), rows) in estimates.iter_mut().zip([3, 7]) {
        estimate::against(&mut db.conn, e).await.unwrap();
        assert_eq!(e.rows, Some(rows), "{}", e.about);
        assert_eq!((&e.rewrite, &e.reads), (&Rewrite::Yes, &Reads::EveryRow));
    }
    db.conn.begin(Mssql.transaction_framing()).await.unwrap();
    for p in &changes {
        for statement in Mssql.emit(&p.change, p.strategy).unwrap() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
    }
    db.conn.commit(Mssql.transaction_framing()).await.unwrap();
    let rows = db
        .conn
        .query("SELECT COUNT_BIG(*) AS n FROM dbo.a")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(7));
    db.drop().await;
}
