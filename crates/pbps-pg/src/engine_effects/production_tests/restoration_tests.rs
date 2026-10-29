//! Restoration intent cannot replace the statement's actual output.
use super::*;
use pbps_model::operation::{
    BoundInput, Incarnation, ObjectKey, Origin, Recorded, Source,
    symbolic::{Attribute, Clause, Facts, Property, Restoration},
};

fn inherited() -> Scene {
    let mut s = scene(false);
    s.after
        .tables
        .get_mut(&"app.a".parse::<TableName>().unwrap())
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .clear();
    s
}

fn default(to: Option<&str>, fallback: Option<&str>) -> Change {
    Change::SetPartitionDefault {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "app.a".parse().unwrap(),
        parent: "app.p".parse().unwrap(),
        column: "d".into(),
        from: Some("9".into()),
        to: to.map(str::to_owned),
        fallback: fallback.map(str::to_owned),
    }
}

fn inspect(
    s: &Scene,
    change: Change,
    origin: Origin,
    edit: impl FnOnce(&mut Facts, &(Recorded, Property)),
) -> Vec<Clause> {
    let steps = [PlannedChange::new(attach(s)), PlannedChange::new(change)];
    let ops = Operations::expanded(
        2,
        [
            (Origin::Whole(0), steps[0].clone()),
            (origin, steps[1].clone()),
        ],
        ModuleEdits::Rebuilt,
    );
    let owners = PlanOwners::new(PlanNames::of(&steps, &s.ids).unwrap(), &s.base_ids, &s.ids);
    let before = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let after = Snapshot {
        schema: &s.after,
        ids: &s.ids,
    };
    let pg = Postgres::new().with_server_version(180000);
    let mut f = facts::of(&pg, &Default::default(), before, after);
    let op = &ops.as_slice()[1];
    let (key, attribute) = match op.key(&owners).unwrap() {
        ObjectKey::Default(key) => (key, Attribute::Default),
        ObjectKey::Column(key) => (key, Attribute::NotNull),
        other @ (ObjectKey::Type(_)
        | ObjectKey::Table(_)
        | ObjectKey::Role(_)
        | ObjectKey::Module(_)
        | ObjectKey::NotNull(_)
        | ObjectKey::Part { .. }
        | ObjectKey::Row { .. }
        | ObjectKey::Rows { .. }
        | ObjectKey::ColumnOrder(_)
        | ObjectKey::Grant { .. }) => panic!("unexpected restoration key: {other:?}"),
    };
    let target = (
        after.table_identity(&"app.a".parse().unwrap()).unwrap(),
        Property::Column(key, attribute),
    );
    edit(&mut f, &target);
    operations::of(&pg, op, &owners, before, after, &f)
        .unwrap()
        .symbolic
}

fn restored(clauses: &[Clause]) -> &Restoration {
    let [Clause::Restore(restoration)] = clauses else {
        panic!("expected one restoration: {clauses:?}")
    };
    restoration
}

#[test]
fn a_partition_fallback_models_the_emitted_value_and_refuses_the_wrong_goal() {
    let s = inherited();
    let change = default(None, Some("3"));
    let clauses = inspect(&s, change.clone(), Origin::Whole(1), |_, _| {});
    assert_eq!(restored(&clauses).value, facts::definition("3"));

    let steps: Vec<_> = [
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            from: Some("1".into()),
            to: Some("7".into()),
        },
        attach(&s),
        change,
    ]
    .into_iter()
    .map(PlannedChange::new)
    .collect();
    let ops = Operations::of_plan(&steps, ModuleEdits::Rebuilt);
    let owners = PlanOwners::new(PlanNames::of(&steps, &s.ids).unwrap(), &s.base_ids, &s.ids);
    let before = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let after = Snapshot {
        schema: &s.after,
        ids: &s.ids,
    };
    let fx = analyse(
        &ops,
        &owners,
        before,
        after,
        &Postgres::new().with_server_version(180000),
    );
    let g = graph(
        ops.as_slice(),
        &fx.effects,
        &Baseline::of(before, &owners).with_symbolic(fx.symbolic),
        &Default::default(),
        Checks::ALL,
    );
    assert!(Schedule::of_graph(&g, ops.as_slice()).sequence().is_err());
}

#[test]
fn a_partition_default_reuses_only_an_agreeing_normalized_goal() {
    let s = inherited();
    for origin in [Origin::Whole(1), Origin::Restoration(Source::Change(1))] {
        let clauses = inspect(&s, default(None, Some(" 7 ")), origin, |_, _| {});
        let value = restored(&clauses).value.known().unwrap();
        assert_eq!(value.datum, Datum::Definition("7".into()));
        assert_eq!(value.ownership, Ownership::Inherited);
        assert!(value.bindings.is_empty());
    }
    // An explicit own payload stays local even when its definition happens to
    // equal the inherited goal. Text equality cannot erase its ownership.
    let clauses = inspect(
        &s,
        default(Some("7"), Some("3")),
        Origin::Whole(1),
        |_, _| {},
    );
    assert_eq!(restored(&clauses).value, facts::definition("7"));
    // The emitter prefers the child's explicit value over its fallback.
    let clauses = inspect(
        &scene(false),
        default(Some("9"), Some("3")),
        Origin::Whole(1),
        |_, _| {},
    );
    assert_eq!(restored(&clauses).value, facts::definition("9"));

    let clauses = inspect(
        &s,
        default(None, Some("3")),
        Origin::Teardown(Source::Change(1)),
        |f, target| {
            f.goal.cells.remove(target);
        },
    );
    assert_eq!(restored(&clauses).value, facts::definition("3"));
    let clauses = inspect(&s, default(None, None), Origin::Whole(1), |_, _| {});
    assert_eq!(
        restored(&clauses).value,
        facts::definition("no column default")
    );
}

#[test]
fn a_partition_default_never_promotes_unread_goals_or_borrows_other_bindings() {
    let s = inherited();
    for payload in ["3", "7"] {
        for missing in [false, true] {
            let clauses = inspect(
                &s,
                default(None, Some(payload)),
                Origin::Whole(1),
                |f, key| {
                    if missing {
                        f.goal.cells.remove(key);
                    } else {
                        f.goal.cells.get_mut(key).unwrap().value = Fact::Unread;
                    }
                },
            );
            assert_eq!(restored(&clauses).value, Fact::Unread);
        }
    }
    // Even a known goal cannot prove what an unbound expression will write.
    let clauses = inspect(
        &s,
        default(None, Some("app.f(3)")),
        Origin::Whole(1),
        |_, _| {},
    );
    assert_eq!(restored(&clauses).value, Fact::Unread);

    // Matching text alone must not import a binding from the goal.
    let clauses = inspect(&s, default(None, Some("7")), Origin::Whole(1), |f, key| {
        let Fact::Known(value) = &mut f.goal.cells.get_mut(key).unwrap().value else {
            panic!()
        };
        value.bindings.insert(BoundInput {
            input: ObjectKey::Table(key.0.clone()),
            incarnation: Incarnation::Base,
        });
    });
    assert_eq!(restored(&clauses).value, facts::definition("7"));

    // The former to == own.default shortcut must validate the actual goal too.
    let clauses = inspect(
        &scene(false),
        default(Some("9"), None),
        Origin::Whole(1),
        |f, key| {
            f.goal.cells.get_mut(key).unwrap().value = facts::definition("3");
        },
    );
    assert_eq!(restored(&clauses).value, facts::definition("9"));
}

#[test]
fn a_partition_not_null_restoration_checks_its_output_before_reusing_the_goal() {
    let mut s = scene(false);
    s.after
        .tables
        .get_mut(&"app.a".parse::<TableName>().unwrap())
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .not_null = true;
    let change = Change::SetPartitionNotNull {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "app.a".parse().unwrap(),
        column: "d".into(),
        not_null: true,
    };
    let clauses = inspect(&s, change.clone(), Origin::Whole(1), |f, key| {
        f.goal.cells.get_mut(key).unwrap().value =
            facts::value(Datum::NotNull(false), Ownership::Inherited);
    });
    assert_eq!(
        restored(&clauses).value,
        facts::value(Datum::NotNull(true), Ownership::Own)
    );
    let clauses = inspect(&s, change.clone(), Origin::Whole(1), |f, key| {
        f.goal.cells.get_mut(key).unwrap().value = Fact::Unread;
    });
    assert_eq!(restored(&clauses).value, Fact::Unread);
    let clauses = inspect(&s, change, Origin::Whole(1), |f, key| {
        f.goal.cells.remove(key);
    });
    assert!(clauses.iter().any(|c| matches!(c, Clause::Require(_))));
}
