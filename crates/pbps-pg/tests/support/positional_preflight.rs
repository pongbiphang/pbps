use super::*;

fn positional_dialect() -> impl crate::certified_fixture::FixtureEmission {
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
        key: Vec::new(),
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
            for statement in dialect.fixture_emit(&p.change, p.strategy).unwrap() {
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
        key: Vec::new(),
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
            for statement in dialect.fixture_emit(&p.change, p.strategy).unwrap() {
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

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_new_key_validates_writes_before_its_own_step_and_never_after_it() {
    use pbps_model::{
        Cell, Change, ChangeSet, ForeignKey, IdsFile, PlannedChange, ReferentialAction, Uid,
        UidKind, Value, plan_names::PlanNames,
    };
    let mut db = TestDb::create("key_position1775").await;
    db.conn
        .execute(
            "CREATE TABLE public.parent (id int PRIMARY KEY);
         CREATE TABLE public.child (id int PRIMARY KEY, parent int);
         INSERT INTO public.parent VALUES (1), (2);
         INSERT INTO public.child VALUES (10, 1);",
        )
        .await
        .unwrap();
    let parent = TableName::new("public", "parent");
    let child = TableName::new("public", "child");
    let delete = Change::DeleteRow {
        table: parent.clone(),
        key_column: "id".into(),
        key: "1".into(),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: Default::default(),
        dropped: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: None,
    };
    let update = Change::UpdateRow {
        table: child.clone(),
        key_column: "id".into(),
        key: "10".into(),
        columns: [(
            "parent".into(),
            (Cell::Value(Value::Int(1)), Cell::Value(Value::Int(2))),
        )]
        .into(),
        unchanged: Default::default(),
        types: [("parent".into(), ty("int"))].into(),
        after_types: Default::default(),
        key_type: None,
    };
    let add = Change::AddForeignKey {
        table: child.clone(),
        name: "child_parent_fkey".into(),
        constraint: Box::new(ForeignKey {
            columns: vec!["parent".into()],
            references_table: parent.clone(),
            references_columns: vec!["id".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        }),
    };
    let ids = IdsFile {
        tables: [
            (Uid::derived(UidKind::Table, "parent", 0), parent.clone()),
            (Uid::derived(UidKind::Table, "child", 0), child.clone()),
        ]
        .into(),
        columns: [
            (
                Uid::derived(UidKind::Column, "parent.id", 0),
                parent.column("id"),
            ),
            (
                Uid::derived(UidKind::Column, "child.id", 0),
                child.column("id"),
            ),
            (
                Uid::derived(UidKind::Column, "child.parent", 0),
                child.column("parent"),
            ),
        ]
        .into(),
        ..Default::default()
    };
    let drop_key = Change::DropForeignKey {
        table: child.clone(),
        name: "child_parent_fkey".into(),
    };
    let mut other_key = add.clone();
    if let Change::AddForeignKey { name, .. } = &mut other_key {
        *name = "other_parent_fkey".into();
    }
    let dialect = Postgres::new();
    let mut observations = Vec::new();
    for (label, changes, expected) in [
        ("no repair", vec![delete.clone(), add.clone()], Some(1)),
        (
            "repair between delete and key",
            vec![delete.clone(), update.clone(), add.clone()],
            Some(0),
        ),
        (
            "repair after key",
            vec![delete.clone(), add.clone(), update.clone()],
            Some(1),
        ),
        (
            "key before delete",
            vec![add.clone(), delete.clone()],
            Some(1),
        ),
        (
            "repair before delete",
            vec![add.clone(), update.clone(), delete.clone()],
            Some(0),
        ),
        (
            "repair after delete",
            vec![add.clone(), delete.clone(), update],
            Some(1),
        ),
        (
            "key already dropped",
            vec![add.clone(), drop_key.clone(), delete.clone()],
            None,
        ),
        (
            "future drop",
            vec![add.clone(), delete.clone(), drop_key.clone()],
            Some(1),
        ),
        (
            "key rebuilt",
            vec![add.clone(), drop_key, add.clone(), delete.clone()],
            Some(1),
        ),
        (
            "another key dropped",
            vec![
                add,
                other_key,
                Change::DropForeignKey {
                    table: child.clone(),
                    name: "other_parent_fkey".into(),
                },
                delete,
            ],
            Some(1),
        ),
    ] {
        let plan = ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        };
        let names = PlanNames::of(&plan.changes, &ids).unwrap();
        let report = dialect.preflight(&names);
        let probes: Vec<_> = report
            .probes
            .iter()
            .filter(|p| p.description.contains("on a column it also adds"))
            .collect();
        let count = match probes.first() {
            Some(probe) => Some(counted(&mut db.conn, &probe.sql).await),
            None => None,
        };
        let frame = dialect.transaction_framing(&pbps_model::SessionZone::utc());
        db.conn.begin(&frame).await.unwrap();
        let mut accepted = true;
        'apply: for p in &plan.changes {
            for statement in dialect.fixture_emit(&p.change, p.strategy).unwrap() {
                if db.conn.execute(&statement.sql).await.is_err() {
                    accepted = false;
                    break 'apply;
                }
            }
        }
        db.conn.rollback(&frame).await.unwrap();
        observations.push((label, expected, count, accepted, probes.len()));
    }
    db.drop().await;
    for (label, expected, count, accepted, _) in &observations {
        eprintln!("{label}: expected={expected:?}, count={count:?}, engine_accepts={accepted}");
    }
    for (label, expected, count, accepted, probe_count) in observations {
        assert_eq!(
            probe_count,
            usize::from(expected.is_some()),
            "{label}: only the active key contributes"
        );
        assert_eq!(count, expected, "{label}: the key owns its row projection");
        assert_eq!(
            accepted,
            expected.unwrap_or(0) == 0,
            "{label}: the engine agrees"
        );
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_planned_key_counts_only_references_that_survive_the_current_delete() {
    check_deleted_self_reference(false).await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
async fn membership_null_probes_read_each_selected_stored_source_once() {
    use pbps_model::{
        Change, IdsFile, PlannedChange, Uid, operation::Snapshot, plan_names::PlanNames,
    };
    let mut db = TestDb::create("membership2296f").await;
    db.conn
        .execute(
            "CREATE TABLE public.p(k integer, d bigint) PARTITION BY RANGE(k);
        CREATE TABLE public.b PARTITION OF public.p FOR VALUES FROM (10) TO (20);
        INSERT INTO public.b VALUES (11,NULL),(12,NULL);
        CREATE TABLE public.a(k integer, d bigint);
        INSERT INTO public.a VALUES (1,NULL);",
        )
        .await
        .unwrap();
    let schema = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .unwrap()
        .schema;
    let p: TableName = "public.p".parse().unwrap();
    let a: TableName = "public.a".parse().unwrap();
    let b: TableName = "public.b".parse().unwrap();
    let uid = |s: &str| s.parse::<Uid>().unwrap();
    let ids = IdsFile {
        tables: [
            (uid("t_aaaaaa"), p.clone()),
            (uid("t_bbbbbb"), a.clone()),
            (uid("t_cccccc"), b),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), p.column("k")),
            (uid("c_bbbbbb"), p.column("d")),
            (uid("c_cccccc"), a.column("k")),
            (uid("c_dddddd"), a.column("d")),
        ]
        .into(),
        ..Default::default()
    };
    let mut after = ids.clone();
    after.columns.retain(|_, c| c.table != a);
    let attach = Change::AttachPartition {
        uid: uid("t_bbbbbb"),
        table: a,
        parent: p.clone(),
        bound: pbps_model::PartitionBound::Range {
            from: vec![pbps_model::BoundDatum::Value("0".into())],
            to: vec![pbps_model::BoundDatum::Value("10".into())],
        },
        key: vec![],
        shape: Box::default(),
    };
    let tighten = Change::AlterColumnNullability {
        uid: uid("c_bbbbbb"),
        column: p.column("d"),
        ty: ty("bigint"),
        to_nullable: false,
        through_external_partitions: false,
        collation: None,
    };
    let pg = Postgres::new();
    let frame = pg.transaction_framing(&pbps_model::SessionZone::utc());
    for clean_standing in [false, true] {
        if clean_standing {
            db.conn.execute("UPDATE public.b SET d = 1").await.unwrap();
        }
        for first in [false, true] {
            let changes: Vec<_> = if first {
                vec![attach.clone(), tighten.clone()]
            } else {
                vec![tighten.clone(), attach.clone()]
            }
            .into_iter()
            .map(PlannedChange::new)
            .collect();
            let names = PlanNames::of(&changes, &after).unwrap();
            let report = pg.preflight_with_sources(
                &names,
                &pbps_model::Partitions::of(&schema).renamed(&names),
                Snapshot {
                    schema: &schema,
                    ids: &ids,
                },
            );
            assert!(report.unchecked.is_empty(), "{report:?}");
            let probe = report
                .probes
                .iter()
                .find(|p| p.description.contains("existing NULLs"))
                .unwrap();
            assert_eq!(probe.sql.matches("query_to_xml").count(), 1);
            assert_eq!(
                probe.sql.matches("row_security_active").count(),
                2 + usize::from(first)
            );
            let expected = if clean_standing { 0 } else { 2 } + i64::from(first);
            assert_eq!(counted(&mut db.conn, &probe.sql).await, expected);
            db.conn.begin(&frame).await.unwrap();
            let mut accepted = false;
            for planned in &changes {
                for statement in pg.fixture_emit(&planned.change, planned.strategy).unwrap() {
                    let result = db.conn.execute(&statement.sql).await;
                    if matches!(planned.change, Change::AlterColumnNullability { .. }) {
                        accepted = result.is_ok();
                    } else {
                        result.unwrap();
                    }
                }
                if matches!(planned.change, Change::AlterColumnNullability { .. }) {
                    break;
                }
            }
            db.conn.rollback(&frame).await.unwrap();
            assert_eq!(accepted, expected == 0);
        }
    }
    // RLS on any one selected relation suppresses the whole answer, including
    // planning of its policy expression behind the one shared wrapper.
    db.conn
        .execute(
            "CREATE ROLE w2296f_reader;
        GRANT USAGE ON SCHEMA public TO w2296f_reader;
        GRANT SELECT ON ALL TABLES IN SCHEMA public TO w2296f_reader;
        ALTER TABLE public.a ENABLE ROW LEVEL SECURITY;
        CREATE POLICY hide ON public.a USING (false);",
        )
        .await
        .unwrap();
    let changes = [PlannedChange::new(attach), PlannedChange::new(tighten)];
    let names = PlanNames::of(&changes, &after).unwrap();
    let report = pg.preflight_with_sources(
        &names,
        &pbps_model::Partitions::of(&schema).renamed(&names),
        Snapshot {
            schema: &schema,
            ids: &ids,
        },
    );
    let probe = report
        .probes
        .iter()
        .find(|p| p.description.contains("existing NULLs"))
        .unwrap();
    db.conn.execute("SET ROLE w2296f_reader").await.unwrap();
    let rows = db.conn.query(&probe.sql).await.unwrap();
    assert_eq!(rows[0].try_get_at::<i32>(0).unwrap(), None);
    db.conn
        .execute("RESET ROLE; DROP OWNED BY w2296f_reader; DROP ROLE w2296f_reader")
        .await
        .unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
async fn converted_membership_values_are_unchecked_and_engine_constraints_still_decide() {
    use pbps_model::{
        Change, IdsFile, PlannedChange, Uid, operation::Snapshot, plan_names::PlanNames,
    };
    let mut db = TestDb::create("rewritten_membership2296f").await;
    let uid = |s: &str| s.parse::<Uid>().unwrap();
    let p: TableName = "public.p".parse().unwrap();
    let a: TableName = "public.a".parse().unwrap();
    let pg = Postgres::new();
    let frame = pg.transaction_framing(&pbps_model::SessionZone::utc());
    for (label, using, key, expected) in [
        ("fills NULLs", Some("coalesce(d,0)"), false, true),
        ("preserves NULLs", Some("d::bigint"), false, false),
        (
            "unevaluable future values",
            Some("coalesce(d,(random()*0)::integer)"),
            false,
            true,
        ),
        ("range key fills NULLs", Some("coalesce(k,1)"), true, true),
        ("unchanged NULLs", None, false, false),
    ] {
        let child_types = if key {
            "k integer, d bigint"
        } else if using.is_some() {
            "k bigint, d integer"
        } else {
            "k bigint, d bigint"
        };
        db.conn.execute(&format!("CREATE TABLE public.p(k bigint, d bigint) PARTITION BY RANGE(k); CREATE TABLE public.a({child_types}); INSERT INTO public.a VALUES {};", if key { "(NULL,1)" } else { "(1,NULL)" })).await.unwrap();
        let schema = pbps_pg::catalog::introspect(&mut db.conn)
            .await
            .unwrap()
            .schema;
        let ids = IdsFile {
            tables: [(uid("t_aaaaaa"), p.clone()), (uid("t_bbbbbb"), a.clone())].into(),
            columns: [
                (uid("c_aaaaaa"), p.column("k")),
                (uid("c_bbbbbb"), p.column("d")),
                (uid("c_cccccc"), a.column("k")),
                (uid("c_dddddd"), a.column("d")),
            ]
            .into(),
            ..Default::default()
        };
        let mut after = ids.clone();
        after.columns.retain(|_, c| c.table != a);
        let mut changes = Vec::new();
        if let Some(using) = using {
            changes.push(Change::AlterColumnType {
                uid: uid(if key { "c_cccccc" } else { "c_dddddd" }),
                column: a.column(if key { "k" } else { "d" }),
                from: ty("integer"),
                to: ty("bigint"),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: Some(using.into()),
            });
        }
        changes.push(Change::AttachPartition {
            uid: uid("t_bbbbbb"),
            table: a.clone(),
            parent: p.clone(),
            bound: pbps_model::PartitionBound::Range {
                from: vec![pbps_model::BoundDatum::Value("0".into())],
                to: vec![pbps_model::BoundDatum::Value("10".into())],
            },
            key: vec![],
            shape: Box::default(),
        });
        changes.push(Change::AlterColumnNullability {
            uid: uid("c_bbbbbb"),
            column: p.column("d"),
            ty: ty("bigint"),
            to_nullable: false,
            through_external_partitions: false,
            collation: None,
        });
        let changes: Vec<_> = changes.into_iter().map(PlannedChange::new).collect();
        let names = PlanNames::of(&changes, &after).unwrap();
        let report = pg.preflight_with_sources(
            &names,
            &pbps_model::Partitions::of(&schema).renamed(&names),
            Snapshot {
                schema: &schema,
                ids: &ids,
            },
        );
        let mut refused = false;
        for probe in &report.probes {
            let rows = db
                .conn
                .query(&probe.sql)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}: {}", probe.sql));
            refused |= rows[0].try_get_at::<i32>(0).unwrap().is_some_and(|n| n > 0);
        }
        if using.is_some() {
            assert!(!refused, "{label}: {report:?}");
            assert!(
                report
                    .unchecked
                    .iter()
                    .any(|u| u.description.contains("public.p.d")
                        && u.reason.contains("value changes")),
                "{label}: {report:?}"
            );
            assert!(
                !report
                    .probes
                    .iter()
                    .any(|p| p.description.contains("existing NULLs")),
                "{label}: {report:?}"
            );
            if key {
                assert!(
                    !report
                        .probes
                        .iter()
                        .any(|p| p.description.contains("outside the range")),
                    "{report:?}"
                );
            }
            assert!(
                report
                    .probes
                    .iter()
                    .any(|p| p.description.contains("grant")),
                "metadata probes remain: {report:?}"
            );
        } else {
            assert!(refused, "{label}: stored NULLs are still refused");
        }
        db.conn.begin(&frame).await.unwrap();
        let mut accepted = true;
        'apply: for change in &changes {
            for sql in pg.fixture_emit(&change.change, change.strategy).unwrap() {
                if db.conn.execute(&sql.sql).await.is_err() {
                    accepted = false;
                    break 'apply;
                }
            }
        }
        db.conn.rollback(&frame).await.unwrap();
        assert_eq!(accepted, expected, "{label}");
        db.conn
            .execute("DROP TABLE public.a; DROP TABLE public.p;")
            .await
            .unwrap();
    }
    db.drop().await;
}
