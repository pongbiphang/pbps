use super::*;

fn positional_dialect() -> impl pbps_dialect::Dialect {
    Postgres::new()
}
fn positional_schema() -> &'static str {
    "public"
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn an_attach_cannot_borrow_a_later_detachs_empty_range() {
    use pbps_model::{
        Change, ChangeSet, Column, IdsFile, PlannedChange, Uid, UidKind, plan_names::PlanNames,
    };
    let mut db = TestDb::create("late_detach1775").await;
    db.conn
        .execute(
            "CREATE TABLE public.parent (id int) PARTITION BY RANGE (id);
        CREATE TABLE public.standing PARTITION OF public.parent DEFAULT;
        INSERT INTO public.parent VALUES (1);
        CREATE TABLE public.child (id int);",
        )
        .await
        .unwrap();
    let parent = TableName::new("public", "parent");
    let child = TableName::new("public", "child");
    let standing = TableName::new("public", "standing");
    let child_id = Uid::derived(UidKind::Table, "child", 0);
    let standing_id = Uid::derived(UidKind::Table, "standing", 0);
    let shape = pbps_model::Table {
        columns: [("id".into(), Column::new(ty("int")))].into(),
        ..Default::default()
    };
    let attach = Change::AttachPartition {
        uid: child_id.clone(),
        table: child.clone(),
        parent: parent.clone(),
        bound: pbps_model::PartitionBound::Range {
            from: vec![pbps_model::BoundDatum::Value("0".into())],
            to: vec![pbps_model::BoundDatum::Value("10".into())],
        },
        shape: Box::new(shape.clone()),
    };
    let detach = Change::DetachPartition {
        uid: standing_id.clone(),
        table: standing.clone(),
        parent: parent.clone(),
        names: vec![],
        shape: Box::new(shape),
    };
    let ids = IdsFile {
        tables: [
            (Uid::derived(UidKind::Table, "parent", 0), parent),
            (child_id, child),
            (standing_id, standing),
        ]
        .into(),
        ..Default::default()
    };
    let dialect = Postgres::new();
    let mut observations = Vec::new();
    for (label, changes, expected) in [
        ("late detach", vec![attach.clone(), detach.clone()], 1),
        ("prior detach", vec![detach, attach], 0),
    ] {
        let plan = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        let names = PlanNames::of(&plan.changes, &ids).unwrap();
        let report = dialect.preflight(&names);
        let probe = report
            .probes
            .iter()
            .find(|p| p.description.contains("inside the range"))
            .unwrap();
        let count = counted(&mut db.conn, &probe.sql).await;
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
        observations.push((label, expected, count, accepted));
    }
    db.drop().await;
    for (label, expected, count, accepted) in observations {
        assert_eq!(
            count, expected,
            "{label}: a future detach cannot erase today's range occupancy"
        );
        assert_eq!(accepted, expected == 0, "{label}: the engine agrees");
    }
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../pbps-dialect/tests/support/positional_preflight.rs"
));

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn tightening_sees_an_attached_child_only_after_the_attach() {
    use pbps_model::{
        Change, ChangeSet, Column, IdsFile, PlannedChange, Uid, UidKind, plan_names::PlanNames,
    };
    let mut db = TestDb::create("late_attach1775").await;
    db.conn
        .execute(
            "CREATE TABLE public.parent (id int, value int) PARTITION BY RANGE (id);
        CREATE TABLE public.standing PARTITION OF public.parent DEFAULT;
        INSERT INTO public.parent VALUES (20, 1);
        CREATE TABLE public.child (id int, value int);
        INSERT INTO public.child VALUES (1, NULL);",
        )
        .await
        .unwrap();
    let parent = TableName::new("public", "parent");
    let child = TableName::new("public", "child");
    let column = Uid::derived(UidKind::Column, "parent.value", 0);
    let child_id = Uid::derived(UidKind::Table, "child", 0);
    let tighten = Change::AlterColumnNullability {
        uid: column.clone(),
        column: parent.column("value"),
        ty: ty("int"),
        to_nullable: false,
        through_external_partitions: false,
        collation: None,
    };
    let update = Change::UpdateRow {
        table: parent.clone(),
        key_column: "id".into(),
        key: "20".into(),
        columns: [(
            "value".into(),
            (
                pbps_model::Cell::Value(pbps_model::Value::Int(1)),
                pbps_model::Cell::Value(pbps_model::Value::Int(2)),
            ),
        )]
        .into(),
        unchanged: Default::default(),
        types: [("value".into(), ty("int"))].into(),
        after_types: Default::default(),
        key_type: None,
    };
    let shape = pbps_model::Table {
        columns: [
            ("id".into(), Column::new(ty("int"))),
            ("value".into(), Column::new(ty("int"))),
        ]
        .into(),
        ..Default::default()
    };
    let attach = Change::AttachPartition {
        uid: child_id.clone(),
        table: child.clone(),
        parent: parent.clone(),
        bound: pbps_model::PartitionBound::Range {
            from: vec![pbps_model::BoundDatum::Value("0".into())],
            to: vec![pbps_model::BoundDatum::Value("10".into())],
        },
        shape: Box::new(shape),
    };
    let ids = IdsFile {
        tables: [
            (Uid::derived(UidKind::Table, "parent", 0), parent.clone()),
            (child_id, child),
        ]
        .into(),
        columns: [(column, parent.column("value"))].into(),
        ..Default::default()
    };
    let dialect = Postgres::new();
    let mut observations = Vec::new();
    for (label, changes, expected) in [
        (
            "late attach",
            vec![update.clone(), tighten.clone(), attach.clone()],
            0,
        ),
        ("prior attach", vec![attach, update, tighten], 1),
    ] {
        let plan = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        let names = PlanNames::of(&plan.changes, &ids).unwrap();
        let report = dialect.preflight(&names);
        assert!(report.unchecked.is_empty(), "{label}: {report:?}");
        let probe = report
            .probes
            .iter()
            .find(|p| p.description.contains("existing NULLs"))
            .unwrap();
        let count = counted(&mut db.conn, &probe.sql).await;
        let frame = dialect.transaction_framing(&pbps_model::SessionZone::utc());
        db.conn.begin(&frame).await.unwrap();
        let mut target_accepted = true;
        for p in &plan.changes {
            for statement in dialect.emit(&p.change, p.strategy).unwrap() {
                let result = db.conn.execute(&statement.sql).await;
                if matches!(p.change, Change::AlterColumnNullability { .. }) {
                    target_accepted &= result.is_ok();
                } else {
                    result.unwrap();
                }
            }
            if matches!(p.change, Change::AlterColumnNullability { .. }) {
                break;
            }
        }
        db.conn.rollback(&frame).await.unwrap();
        observations.push((label, expected, count, target_accepted));
    }
    db.drop().await;
    for (label, expected, count, target_accepted) in observations {
        assert_eq!(
            count, expected,
            "{label}: only rows attached before this statement count"
        );
        assert_eq!(
            target_accepted,
            expected == 0,
            "{label}: the engine's tightening agrees"
        );
    }
}
