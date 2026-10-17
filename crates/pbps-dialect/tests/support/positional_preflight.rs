/// Every probe describes the rows at its statement, including a plan whose
/// later write would have hidden a failure if projected backwards.
#[tokio::test]
#[ignore = "needs a live engine"]
async fn tightening_reads_prior_writes_and_never_future_writes() {
    use pbps_model::{Cell, Change, ChangeSet, IdsFile, PlannedChange, Uid, UidKind, Value, plan_names::PlanNames};
    let mut db = TestDb::create("positional1775").await;
    let dialect = positional_dialect();
    let table = TableName::new(positional_schema(), "items");
    db.conn.execute(&format!("CREATE TABLE {table} (id int PRIMARY KEY, value int NULL); INSERT INTO {table} VALUES (1, NULL);", table = table.raw())).await.unwrap();
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

#[tokio::test]
#[ignore = "needs a live engine"]
async fn a_catalog_key_counts_only_references_that_survive_the_current_delete() {
    check_deleted_self_reference(true).await;
}

async fn check_deleted_self_reference(catalog_key: bool) {
    use pbps_model::{
        Cell, Change, ChangeSet, ForeignKey, IdsFile, PlannedChange, ReferentialAction, Uid,
        UidKind, Value, plan_names::PlanNames,
    };
    let mut db = TestDb::create(if catalog_key { "catalog_self1977" } else { "planned_self1977" }).await;
    let table = TableName::new(positional_schema(), "nodes");
    db.conn
        .execute(
            &format!("CREATE TABLE {table} (id int PRIMARY KEY, parent int);
        INSERT INTO {table} VALUES (1, 1), (2, 2);", table = table.raw(),
            ),
        )
        .await
        .unwrap();
    let ids = IdsFile {
        tables: [(Uid::derived(UidKind::Table, "nodes", 0), table.clone())].into(),
        columns: ["id", "parent"]
            .into_iter()
            .map(|column| {
                (
                    Uid::derived(UidKind::Column, column, 0),
                    table.column(column),
                )
            })
            .collect(),
        ..Default::default()
    };
    let add = Change::AddForeignKey {
        table: table.clone(),
        name: "nodes_parent_fkey".into(),
        constraint: Box::new(ForeignKey {
            columns: vec!["parent".into()],
            references_table: table.clone(),
            references_columns: vec!["id".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        }),
    };
    let update = Change::UpdateRow {
        table: table.clone(),
        key_column: "id".into(),
        key: "1".into(),
        columns: [(
            "parent".into(),
            (Cell::Value(Value::Int(2)), Cell::Value(Value::Int(1))),
        )]
        .into(),
        unchanged: Default::default(),
        types: [("parent".into(), ty("int"))].into(),
        after_types: Default::default(),
        key_type: None,
    };
    let delete = Change::DeleteRow {
        table: table.clone(),
        key_column: "id".into(),
        key: "1".into(),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: Default::default(),
        dropped: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: None,
    };
    let dialect = positional_dialect();
    let frame = dialect.transaction_framing(&pbps_model::SessionZone::utc());
    let mut observations = Vec::new();
    if catalog_key {
        db.conn
            .execute(
                &format!("ALTER TABLE {table} ADD CONSTRAINT nodes_parent_fkey
            FOREIGN KEY (parent) REFERENCES {table} (id);", table = table.raw(),
                ),
            )
            .await
            .unwrap();
    }
    for rewritten in [false, true] {
        for other_reference in [false, true] {
            db.conn
                .execute(&format!("DELETE FROM {table} WHERE id = 3;", table = table.raw()))
                .await
                .unwrap();
            db.conn
                .execute(&format!("UPDATE {table} SET parent = {} WHERE id = 1;", if rewritten { 2 } else { 1 }, table = table.raw()))
                .await
                .unwrap();
            if other_reference {
                db.conn
                    .execute(&format!("INSERT INTO {table} VALUES (3, 1);", table = table.raw()))
                    .await
                    .unwrap();
            }
            let mut changes = Vec::new();
            if !catalog_key {
                changes.push(add.clone());
            }
            if rewritten {
                changes.push(update.clone());
            }
            changes.push(delete.clone());
            let plan = ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            };
            let names = PlanNames::of(&plan.changes, &ids).unwrap();
            let report = dialect.preflight(&names);
            assert!(report.unchecked.is_empty());
            let description = if catalog_key {
                "rows in other tables that still reference"
            } else {
                "on a column it also adds"
            };
            let probe = report
                .probes
                .iter()
                .find(|p| p.description.contains(description))
                .unwrap();
            let rows = db.conn.query(&probe.sql).await.unwrap();
            let count: i32 = rows[0].try_get_at(0).unwrap().unwrap();
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
            observations.push((catalog_key, rewritten, other_reference, count, accepted));
        }
    }
    db.drop().await;
    for observation in &observations {
        eprintln!("self-reference {observation:?}");
    }
    for (catalog_key, rewritten, other_reference, count, accepted) in observations {
        let label =
            format!("catalog={catalog_key}, rewritten={rewritten}, other={other_reference}");
        assert_eq!(
            count,
            i32::from(other_reference),
            "{label}: only a surviving referrer counts"
        );
        assert_eq!(accepted, !other_reference, "{label}: the engine agrees");
    }
}
