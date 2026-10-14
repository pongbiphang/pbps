/// Every probe describes the rows at its statement, including a plan whose
/// later write would have hidden a failure if projected backwards.
#[tokio::test]
#[ignore = "needs a live engine"]
async fn tightening_reads_prior_writes_and_never_future_writes() {
    use pbps_model::{Cell, Change, ChangeSet, IdsFile, PlannedChange, Uid, UidKind, Value, plan_names::PlanNames};
    let mut db = TestDb::create("positional1775").await;
    let dialect = positional_dialect();
    let table = TableName::new(positional_schema(), "items");
    db.conn.execute(&format!("CREATE TABLE {table} (id int PRIMARY KEY, value int NULL); INSERT INTO {table} VALUES (1, NULL);")).await.unwrap();
    let column = Uid::derived(UidKind::Column, "value", 0);
    let tighten = Change::AlterColumnNullability {
        uid: column.clone(), column: table.column("value"), ty: ty("int"), to_nullable: false,
        through_external_partitions: false, collation: None,
    };
    let update = Change::UpdateRow {
        table: table.clone(), key_column: "id".into(), key: "1".into(),
        columns: [("value".into(), (Cell::Value(Value::Null), Cell::Value(Value::Int(1))))].into(),
        unchanged: Default::default(), types: [("value".into(), ty("int"))].into(),
        after_types: Default::default(), key_type: None,
    };
    let delete = Change::DeleteRow {
        table: table.clone(), key_column: "id".into(), key: "1".into(),
        cause: pbps_model::change::DeleteCause::Undeclared, row: Default::default(),
        dropped: Default::default(), types: Default::default(), after_types: Default::default(), key_type: None,
    };
    let ids = IdsFile {
        tables: [(Uid::derived(UidKind::Table, "items", 0), table.clone())].into(),
        columns: [(column, table.column("value")), (Uid::derived(UidKind::Column, "id", 0), table.column("id"))].into(),
        ..IdsFile::default()
    };
    let mut observed = Vec::new();
    for (label, changes, expected) in [
        ("no preceding repair", vec![tighten.clone()], 1),
        ("later update", vec![tighten.clone(), update.clone()], 1),
        ("earlier update", vec![update, tighten.clone()], 0),
        ("later delete", vec![tighten.clone(), delete.clone()], 1),
        ("earlier delete", vec![delete, tighten], 0),
    ] {
        let plan = ChangeSet { changes: changes.into_iter().map(PlannedChange::new).collect() };
        let names = PlanNames::of(&plan.changes, &ids).unwrap();
        let report = dialect.preflight(&names);
        assert!(report.unchecked.is_empty(), "{label}: {report:?}");
        let probe = report.probes.iter().find(|p| p.description.contains("NULL")).unwrap();
        let rows = db.conn.query(&probe.sql).await.unwrap();
        let count: i32 = rows[0].try_get_at(0).unwrap().unwrap();
        let frame = dialect.transaction_framing(&pbps_model::SessionZone::utc());
        db.conn.begin(&frame).await.unwrap();
        let mut accepted = true;
        'apply: for p in &plan.changes {
            for statement in dialect.emit(&p.change, p.strategy).unwrap() {
                if db.conn.execute(&statement.sql).await.is_err() {
                    accepted = false;
                    break 'apply;
                }
            }
        }
        db.conn.rollback(&frame).await.unwrap();
        observed.push((label, expected, count, accepted));
    }
    db.drop().await;
    for (label, expected, count, accepted) in observed {
        assert_eq!(count, expected, "{label}: probe agrees with statement position");
        assert_eq!(accepted, expected == 0, "{label}: engine agrees with the probe");
    }
}
