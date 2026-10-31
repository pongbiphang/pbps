//! The production mapping, through the same analysis and effect settlement.
//! Observations contain no executable fixture constructor.
use super::*;
use crate::Postgres;
use pbps_diff::order::{Baseline, Checks, Schedule, analyse, graph};
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    BoundDatum, Change, Column, IdsFile, PartitionBound, PartitionBy, PartitionColumn, PartitionOf,
    PlannedChange, Schema, Table, TableName, ValueSource,
};

struct Scene {
    before: Schema,
    after: Schema,
    base_ids: IdsFile,
    ids: IdsFile,
}
fn scene(rename: bool) -> Scene {
    let parent: TableName = "app.p".parse().unwrap();
    let child: TableName = "app.a".parse().unwrap();
    let mut p = Table {
        partition_by: Some(PartitionBy {
            strategy: Default::default(),
            columns: vec!["k".into()],
            external: false,
        }),
        ..Default::default()
    };
    p.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let mut d = Column::new("integer".parse().unwrap());
    d.value_source = ValueSource::Default("1".into());
    p.columns.insert("d".into(), d.clone());
    let mut a = Table::default();
    a.columns.insert("k".into(), p.columns["k"].clone());
    d.value_source = ValueSource::Default("9".into());
    a.columns.insert(if rename { "e" } else { "d" }.into(), d);
    let mut before = Schema::default();
    before.tables.insert(parent.clone(), p);
    before.tables.insert(child.clone(), a);
    let mut base_ids = IdsFile::default();
    for (uid, name) in [("t_aaaaaa", parent.clone()), ("t_bbbbbb", child.clone())] {
        base_ids.tables.insert(uid.parse().unwrap(), name);
    }
    for (uid, at) in [
        ("c_aaaaaa", parent.column("k")),
        ("c_bbbbbb", parent.column("d")),
        ("c_cccccc", child.column("k")),
        ("c_dddddd", child.column(if rename { "e" } else { "d" })),
    ] {
        base_ids.columns.insert(uid.parse().unwrap(), at);
    }
    let mut after = before.clone();
    if rename {
        let p = after.tables.get_mut(&parent).unwrap();
        let c = p.columns.shift_remove("d").unwrap();
        p.columns.insert("e".into(), c);
    } else {
        after
            .tables
            .get_mut(&parent)
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .value_source = ValueSource::Default("7".into());
    }
    after.tables.insert(
        child,
        Table {
            partition_of: Some(PartitionOf {
                parent: parent.clone(),
                bound: bound(),
                columns: [(
                    if rename { "e" } else { "d" }.into(),
                    PartitionColumn {
                        default: Some("9".into()),
                        not_null: false,
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        },
    );
    let mut ids = base_ids.clone();
    ids.columns
        .retain(|uid, _| uid.as_str() != "c_cccccc" && uid.as_str() != "c_dddddd");
    if rename {
        ids.columns
            .insert("c_bbbbbb".parse().unwrap(), parent.column("e"));
    }
    Scene {
        before,
        after,
        base_ids,
        ids,
    }
}
fn bound() -> PartitionBound {
    PartitionBound::Range {
        from: vec![BoundDatum::MinValue],
        to: vec![BoundDatum::MaxValue],
    }
}
fn attach(s: &Scene) -> Change {
    Change::AttachPartition {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "app.a".parse().unwrap(),
        parent: "app.p".parse().unwrap(),
        bound: bound(),
        key: Default::default(),
        shape: Box::new(s.after.tables[&"app.a".parse::<TableName>().unwrap()].clone()),
    }
}
fn read(s: &Scene, steps: Vec<Change>) -> (Vec<usize>, pbps_diff::order::Graph) {
    read_input(s, steps, None)
}
fn read_input(
    s: &Scene,
    steps: Vec<Change>,
    input: Option<&[usize]>,
) -> (Vec<usize>, pbps_diff::order::Graph) {
    let steps: Vec<_> = steps.into_iter().map(PlannedChange::new).collect();
    let ops = Operations::of_plan(&steps, ModuleEdits::Rebuilt);
    let ops = match input {
        Some(sequence) => pbps_diff::order::reordered_operations(&ops, sequence),
        None => ops,
    };
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
    assert!(fx.unowned.is_empty(), "{:?}", fx.unowned);
    let g = graph(
        ops.as_slice(),
        &fx.effects,
        &Baseline::of(before, &owners).with_symbolic(fx.symbolic.clone()),
        &Default::default(),
        Checks::ALL,
    );
    let sequence = Schedule::of_graph(&g, ops.as_slice())
        .sequence()
        .unwrap_or_else(|e| panic!("{e}; {:?}", g.symbolic_unmet));
    let ordered = pbps_diff::order::reordered_operations(&ops, &sequence);
    let effects = sequence
        .iter()
        .map(|&at| fx.effects[at].clone())
        .collect::<Vec<_>>();
    assert!(
        pbps_diff::order::membership::project(ordered.as_slice(), &effects, &fx.symbolic).is_ok()
    );
    let mut final_state = None;
    let mut all = vec![Vec::new()];
    for at in 0..ops.len() {
        all = all
            .into_iter()
            .flat_map(|prefix| {
                (0..=prefix.len()).map(move |position| {
                    let mut order = prefix.clone();
                    order.insert(position, at);
                    order
                })
            })
            .collect();
    }
    for order in all {
        let mut position = vec![0; ops.len()];
        for (at, &i) in order.iter().enumerate() {
            position[i] = at;
        }
        if !g
            .edges
            .iter()
            .all(|e| position[e.before] < position[e.after])
        {
            continue;
        }
        let ordered = pbps_diff::order::reordered_operations(&ops, &order);
        let effects = order
            .iter()
            .map(|&at| fx.effects[at].clone())
            .collect::<Vec<_>>();
        let replay =
            pbps_diff::order::membership::project(ordered.as_slice(), &effects, &fx.symbolic)
                .unwrap();
        let end = replay.state(pbps_model::plan_names::At::End).unwrap();
        if let Some(previous) = &final_state {
            assert_eq!(previous, end, "{order:?}");
        } else {
            final_state = Some(end.clone());
        }
    }
    (sequence, g)
}
#[test]
fn a_production_parent_default_meets_the_candidates_own_goal_at_its_cut() {
    let s = scene(false);
    let change = Change::AlterColumnDefault {
        uid: "c_bbbbbb".parse().unwrap(),
        column: "app.p.d".parse().unwrap(),
        from: Some("1".into()),
        to: Some("7".into()),
    };
    for reversed in [false, true] {
        let steps = if reversed {
            vec![attach(&s), change.clone()]
        } else {
            vec![change.clone(), attach(&s)]
        };
        let (order, g) = read(&s, steps);
        assert_eq!(order, if reversed { vec![1, 0] } else { vec![0, 1] });
        assert!(
            g.edges
                .iter()
                .any(|e| matches!(e.because, pbps_diff::order::Because::Boundary(_)))
        );
    }
}
#[test]
fn a_production_restoration_admits_both_parent_default_membership_cuts() {
    let s = scene(false);
    let (_, g) = read(
        &s,
        vec![
            attach(&s),
            Change::AlterColumnDefault {
                uid: "c_bbbbbb".parse().unwrap(),
                column: "app.p.d".parse().unwrap(),
                from: Some("1".into()),
                to: Some("7".into()),
            },
            Change::SetPartitionDefault {
                uid: "t_bbbbbb".parse().unwrap(),
                table: "app.a".parse().unwrap(),
                parent: "app.p".parse().unwrap(),
                column: "d".into(),
                from: Some("9".into()),
                to: Some("9".into()),
                fallback: None,
            },
        ],
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| (e.before == 0 && e.after == 1) || (e.before == 1 && e.after == 0)),
        "{:?}",
        g.edges
    );
}
#[test]
fn a_production_column_name_version_precedes_an_attach_using_that_name() {
    let s = scene(true);
    let change = Change::RenameColumn {
        uid: "c_bbbbbb".parse().unwrap(),
        table: "app.p".parse().unwrap(),
        from: "d".into(),
        to: "e".into(),
        table_was: None,
    };
    for reversed in [false, true] {
        let steps = if reversed {
            vec![attach(&s), change.clone()]
        } else {
            vec![change.clone(), attach(&s)]
        };
        let (order, _) = read(&s, steps);
        assert_eq!(order, if reversed { vec![1, 0] } else { vec![0, 1] });
    }
}
#[test]
fn an_unread_expression_binding_does_not_turn_into_an_empty_binding_set() {
    let mut s = scene(false);
    let p: TableName = "app.p".parse().unwrap();
    s.before
        .tables
        .get_mut(&p)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::Default("app.f(1)".into());
    let f = facts::of(
        &Postgres::new(),
        &Default::default(),
        Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        },
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        },
    );
    let ck = s
        .base_ids
        .columns
        .iter()
        .find(|(u, _)| u.as_str() == "c_bbbbbb")
        .unwrap()
        .1;
    let key = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    }
    .column_identity(ck)
    .unwrap();
    let parent = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    }
    .table_identity(&p)
    .unwrap();
    assert_eq!(
        f.base.cells[&(
            parent,
            pbps_model::operation::symbolic::Property::Column(
                key,
                pbps_model::operation::symbolic::Attribute::Default
            )
        )]
            .value,
        pbps_model::operation::symbolic::Fact::Unread
    );
}

#[test]
fn a_production_add_column_uses_presence_instead_of_a_fixed_move_order() {
    for candidate_has_column in [false, true] {
        let mut s = scene(false);
        let p: TableName = "app.p".parse().unwrap();
        let a: TableName = "app.a".parse().unwrap();
        s.after
            .tables
            .get_mut(&p)
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .value_source = ValueSource::Default("1".into());
        let mut x = Column::new("integer".parse().unwrap());
        x.value_source = ValueSource::Default("3".into());
        s.after
            .tables
            .get_mut(&p)
            .unwrap()
            .columns
            .insert("x".into(), x.clone());
        s.ids
            .columns
            .insert("c_eeeeee".parse().unwrap(), p.column("x"));
        if candidate_has_column {
            s.before
                .tables
                .get_mut(&a)
                .unwrap()
                .columns
                .insert("x".into(), x.clone());
            s.base_ids
                .columns
                .insert("c_ffffff".parse().unwrap(), a.column("x"));
        }
        let change = Change::AddColumn {
            uid: "c_eeeeee".parse().unwrap(),
            table: p,
            name: "x".into(),
            column: Box::new(x),
        };
        for reverse in [false, true] {
            let steps = if reverse {
                vec![attach(&s), change.clone()]
            } else {
                vec![change.clone(), attach(&s)]
            };
            let (order, _) = read(&s, steps);
            let change_first = candidate_has_column != reverse;
            assert_eq!(order, if change_first { vec![0, 1] } else { vec![1, 0] });
        }
    }
}

#[test]
fn production_merges_cover_release_values_missing_from_both_endpoints() {
    let s = scene(false);
    let (_, g) = read(
        &s,
        vec![
            Change::AlterColumnDefault {
                uid: "c_bbbbbb".parse().unwrap(),
                column: "app.p.d".parse().unwrap(),
                from: Some("1".into()),
                to: None,
            },
            Change::AlterColumnDefault {
                uid: "c_bbbbbb".parse().unwrap(),
                column: "app.p.d".parse().unwrap(),
                from: None,
                to: Some("7".into()),
            },
            attach(&s),
            Change::SetPartitionDefault {
                uid: "t_bbbbbb".parse().unwrap(),
                table: "app.a".parse().unwrap(),
                parent: "app.p".parse().unwrap(),
                column: "d".into(),
                from: Some("9".into()),
                to: Some("9".into()),
                fallback: None,
            },
        ],
    );
    assert!(g.symbolic_unmet.is_empty());
}

#[test]
fn a_parent_address_write_keeps_uid_membership_and_its_logical_name_history() {
    let mut s = scene(false);
    let old: TableName = "app.p".parse().unwrap();
    let new: TableName = "other.q".parse().unwrap();
    let mut parent = s.after.tables.remove(&old).unwrap();
    parent.columns.get_mut("d").unwrap().value_source = ValueSource::Default("1".into());
    s.after.tables.insert(new.clone(), parent);
    s.after
        .tables
        .get_mut(&"app.a".parse::<TableName>().unwrap())
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .parent = new.clone();
    s.ids
        .tables
        .insert("t_aaaaaa".parse().unwrap(), new.clone());
    for at in s.ids.columns.values_mut().filter(|at| at.table == old) {
        at.table = new.clone();
    }
    let logical = vec![
        Change::RenameTable {
            uid: "t_aaaaaa".parse().unwrap(),
            from: old,
            to: new.clone(),
            defaults: vec![],
        },
        Change::AttachPartition {
            uid: "t_bbbbbb".parse().unwrap(),
            table: "app.a".parse().unwrap(),
            parent: new,
            bound: bound(),
            key: vec![],
            shape: Box::new(s.after.tables[&"app.a".parse::<TableName>().unwrap()].clone()),
        },
    ];
    for input in [vec![0, 1], vec![1, 0]] {
        let (order, _) = read_input(&s, logical.clone(), Some(&input));
        assert_eq!(
            order,
            if input[0] == 0 {
                vec![0, 1]
            } else {
                vec![1, 0]
            }
        );
    }
}

#[test]
fn a_rebuilt_routines_binding_is_final_on_the_restored_parent_default() {
    use pbps_model::operation::{BoundInput, Incarnation, ObjectKey, symbolic::*};
    let mut s = scene(false);
    let loaded = pbps_load::load_module_str(std::path::Path::new("f.yml"),
        "function: app.f(integer)\ndefinition: |\n  (x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$\n").unwrap();
    let id = loaded.id;
    s.before.modules.insert(id.clone(), loaded.module.clone());
    let mut rebuilt = loaded.module;
    rebuilt.definition =
        "(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x*10 $$".into();
    s.after.modules.insert(id.clone(), rebuilt.clone());
    for (model, name, column, expression) in [
        (&mut s.before, "app.p", "d", "app.f(1)"),
        (&mut s.after, "app.p", "d", "app.f(1)"),
    ] {
        model
            .tables
            .get_mut(&name.parse::<TableName>().unwrap())
            .unwrap()
            .columns
            .get_mut(column)
            .unwrap()
            .value_source = ValueSource::Default(expression.into());
    }
    let mut catalog = pbps_model::operation::MembershipCatalog::default();
    catalog.columns.insert(
        "app.p.d".parse().unwrap(),
        pbps_model::operation::StoredColumn {
            default: Some("app.f(1)".into()),
            routines: [id.clone()].into(),
            ..Default::default()
        },
    );
    let pg = Postgres::new().with_server_version(180000);
    let steps = vec![
        PlannedChange::new(Change::AlterModule {
            id: id.clone(),
            module: Box::new(rebuilt),
        }),
        PlannedChange::new(Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            from: None,
            to: Some("app.f(1)".into()),
        }),
        PlannedChange::new(attach(&s)),
    ];
    let ops = Operations::of_plan(&steps, ModuleEdits::Rebuilt);
    let owners = PlanOwners::new(PlanNames::of(&steps, &s.ids).unwrap(), &s.base_ids, &s.ids);
    let fx = pbps_diff::order::analyse_with_membership(
        &ops,
        &owners,
        Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        },
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        },
        &pg,
        None,
        Some(&catalog),
    );
    assert!(fx.unowned.is_empty(), "{:?}", fx.unowned);
    let write = fx.effects[1]
        .symbolic
        .iter()
        .find_map(|c| match c {
            Clause::Recurse(w)
                if matches!(w.key.property, Property::Column(_, Attribute::Default)) =>
            {
                Some(w)
            }
            Clause::Recurse(_) | Clause::Move(_) | Clause::Restore(_) | Clause::Require(_) => None,
        })
        .unwrap();
    assert_eq!(
        write.to.known().unwrap().bindings,
        [BoundInput {
            input: ObjectKey::Module(id),
            incarnation: Incarnation::Final
        }]
        .into()
    );
}

#[test]
fn unrelated_membership_trees_do_not_enter_an_active_familys_facts() {
    let mut s = scene(false);
    let p: TableName = "other.p".parse().unwrap();
    let c: TableName = "other.c".parse().unwrap();
    for schema in [&mut s.before, &mut s.after] {
        schema.tables.insert(
            p.clone(),
            Table {
                partition_by: Some(PartitionBy {
                    strategy: Default::default(),
                    columns: vec!["k".into()],
                    external: false,
                }),
                ..Default::default()
            },
        );
        schema.tables.insert(
            c.clone(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: p.clone(),
                    bound: bound(),
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
    }
    for ids in [&mut s.base_ids, &mut s.ids] {
        ids.tables.insert("t_cccccc".parse().unwrap(), p.clone());
        ids.tables.insert("t_dddddd".parse().unwrap(), c.clone());
    }
    let steps: Vec<_> = vec![
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            from: Some("1".into()),
            to: Some("7".into()),
        },
        attach(&s),
    ]
    .into_iter()
    .map(PlannedChange::new)
    .collect();
    let ops = Operations::of_plan(&steps, ModuleEdits::Rebuilt);
    let owners = PlanOwners::new(PlanNames::of(&steps, &s.ids).unwrap(), &s.base_ids, &s.ids);
    let fx = analyse(
        &ops,
        &owners,
        Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        },
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        },
        &Postgres::new().with_server_version(180000),
    );
    assert_eq!(fx.symbolic.tables.len(), 2);
    assert!(fx.symbolic.members.is_empty());
    assert!(
        !fx.symbolic
            .tables
            .iter()
            .any(|t| t.uid().as_str() == "t_cccccc" || t.uid().as_str() == "t_dddddd")
    );
}

#[test]
fn a_defaultless_restoration_clears_own_values_without_inventing_a_parent_default() {
    let mut s = scene(false);
    s.after
        .tables
        .get_mut(&"app.p".parse::<TableName>().unwrap())
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::None;
    s.after
        .tables
        .get_mut(&"app.a".parse::<TableName>().unwrap())
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .clear();
    let steps = vec![
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            from: Some("1".into()),
            to: None,
        },
        attach(&s),
        Change::SetPartitionDefault {
            uid: "t_bbbbbb".parse().unwrap(),
            table: "app.a".parse().unwrap(),
            parent: "app.p".parse().unwrap(),
            column: "d".into(),
            from: Some("9".into()),
            to: None,
            fallback: None,
        },
    ];
    read(&s, steps);
}

#[test]
fn production_not_null_restorations_preserve_both_measured_attach_cuts() {
    for (old, new, candidate, own, restore) in [
        (true, false, true, true, false),
        (false, true, true, false, false),
        (false, true, false, false, false),
        (true, false, true, false, true),
    ] {
        let mut s = scene(false);
        let p: TableName = "app.p".parse().unwrap();
        let a: TableName = "app.a".parse().unwrap();
        s.before
            .tables
            .get_mut(&p)
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .nullable = !old;
        let goal = s
            .after
            .tables
            .get_mut(&p)
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap();
        goal.nullable = !new;
        goal.value_source = ValueSource::Default("1".into());
        s.before
            .tables
            .get_mut(&a)
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .nullable = !candidate;
        s.after
            .tables
            .get_mut(&a)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .not_null = own;
        let mut steps = vec![
            Change::AlterColumnNullability {
                uid: "c_bbbbbb".parse().unwrap(),
                column: p.column("d"),
                ty: "integer".parse().unwrap(),
                to_nullable: !new,
                collation: None,
                through_external_partitions: false,
            },
            attach(&s),
        ];
        if restore {
            steps.push(Change::SetPartitionNotNull {
                uid: "t_bbbbbb".parse().unwrap(),
                table: a,
                column: "d".into(),
                not_null: own,
            });
        }
        let (_, graph) = read(&s, steps.clone());
        if restore {
            assert!(
                !graph.edges.iter().any(|e| ((e.before == 0 && e.after == 1)
                    || (e.before == 1 && e.after == 0))
                    && matches!(e.because, pbps_diff::order::Because::Boundary(_))),
                "a terminal restoration admits both cuts: {:?}",
                graph.edges
            );
            assert!(graph.edges.iter().any(|e| e.before == 1 && e.after == 2));
        }
        let reversed: Vec<_> = (0..steps.len()).rev().collect();
        read_input(&s, steps, Some(&reversed));
    }
}

#[test]
fn a_detached_default_restores_the_transferred_property() {
    let mut s = scene(false);
    std::mem::swap(&mut s.before, &mut s.after);
    std::mem::swap(&mut s.base_ids, &mut s.ids);
    let p: TableName = "app.p".parse().unwrap();
    let a: TableName = "app.a".parse().unwrap();
    let old = s.before.tables[&p].columns["d"].clone();
    let goal = s
        .after
        .tables
        .get_mut(&p)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap();
    goal.value_source = old.value_source;
    goal.ty = "bigint".parse().unwrap();
    let steps = vec![
        Change::AlterColumnType {
            uid: "c_bbbbbb".parse().unwrap(),
            column: p.column("d"),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_collation: None,
            to_collation: None,
            from_nullable: true,
            to_nullable: true,
            through_external_partitions: false,
            using: None,
        },
        Change::DetachPartition {
            uid: "t_bbbbbb".parse().unwrap(),
            table: a.clone(),
            parent: p,
            shape: Box::new(s.after.tables[&a].clone()),
            names: Default::default(),
        },
        Change::AlterColumnDefault {
            uid: "c_dddddd".parse().unwrap(),
            column: a.column("d"),
            from: Some("9".into()),
            to: Some("9".into()),
        },
    ];
    let (_, graph) = read(&s, steps);
    assert!(graph.edges.iter().any(|e| e.before == 1 && e.after == 2));

    // The transfer also preserves actual bound identities, rather than
    // replacing an unread routine default with an empty dependency set.
    s.before
        .tables
        .get_mut(&a)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .default = Some("app.f(9)".into());
    s.after
        .tables
        .get_mut(&a)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::Default("app.f(9)".into());
    let routine = pbps_model::ModuleId::Routine("app.f(integer)".parse().unwrap());
    let mut catalog = pbps_model::operation::MembershipCatalog::default();
    catalog.columns.insert(
        a.column("d"),
        pbps_model::operation::StoredColumn {
            default: Some("app.f(9)".into()),
            routines: [routine.clone()].into(),
            ..Default::default()
        },
    );
    let pg = Postgres::new().with_server_version(180000);
    let before = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let after = Snapshot {
        schema: &s.after,
        ids: &s.ids,
    };
    let f = facts::of(&pg, &catalog, before, after);
    let property = pbps_model::operation::symbolic::Property::Column(
        after.column_identity(&a.column("d")).unwrap(),
        pbps_model::operation::symbolic::Attribute::Default,
    );
    let value = f.goal.cells[&(after.table_identity(&a).unwrap(), property)]
        .value
        .known()
        .unwrap();
    assert!(
        value
            .bindings
            .iter()
            .any(|b| b.input == pbps_model::operation::ObjectKey::Module(routine.clone()))
    );
}

mod class;
mod meeting_golden;
mod scenes_tests;

#[test]
fn a_partition_restoration_never_invents_a_goal_or_its_bindings() {
    use pbps_model::operation::symbolic::Clause;
    let s = scene(false);
    let mut steps = vec![
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            from: Some("1".into()),
            to: Some("7".into()),
        },
        attach(&s),
        Change::SetPartitionNotNull {
            uid: "t_bbbbbb".parse().unwrap(),
            table: "app.a".parse().unwrap(),
            column: "d".into(),
            not_null: true,
        },
    ];
    let inspect = |steps: Vec<Change>| {
        let steps: Vec<_> = steps.into_iter().map(PlannedChange::new).collect();
        let ops = Operations::of_plan(&steps, ModuleEdits::Rebuilt);
        let owners = PlanOwners::new(PlanNames::of(&steps, &s.ids).unwrap(), &s.base_ids, &s.ids);
        analyse(
            &ops,
            &owners,
            Snapshot {
                schema: &s.before,
                ids: &s.base_ids,
            },
            Snapshot {
                schema: &s.after,
                ids: &s.ids,
            },
            &Postgres::new().with_server_version(180000),
        )
        .effects
        .pop()
        .unwrap()
        .symbolic
    };
    assert!(inspect(steps.clone()).iter().any(|c| matches!(c, Clause::Require(Fact::Unsupported { reason, .. }) if reason.contains("disagrees"))));
    steps[2] = Change::SetPartitionDefault {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "app.a".parse().unwrap(),
        parent: "app.p".parse().unwrap(),
        column: "d".into(),
        from: Some("9".into()),
        to: Some("app.f(3)".into()),
        fallback: None,
    };
    assert!(
        inspect(steps)
            .iter()
            .any(|c| matches!(c, Clause::Restore(r) if r.value==Fact::Unread))
    );
}

#[test]
fn an_attaching_sources_default_chain_is_consumed_before_partition_restoration() {
    let s = scene(false);
    let steps = vec![
        Change::AlterColumnDefault {
            uid: "c_dddddd".parse().unwrap(),
            column: "app.a.d".parse().unwrap(),
            from: Some("9".into()),
            to: None,
        },
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: "app.p.d".parse().unwrap(),
            from: Some("1".into()),
            to: Some("7".into()),
        },
        attach(&s),
        Change::SetPartitionDefault {
            uid: "t_bbbbbb".parse().unwrap(),
            table: "app.a".parse().unwrap(),
            parent: "app.p".parse().unwrap(),
            column: "d".into(),
            from: None,
            to: Some("9".into()),
            fallback: None,
        },
    ];
    let (_, graph) = read(&s, steps);
    assert!(graph.edges.iter().any(|e| e.before == 0 && e.after == 2));
    assert!(graph.edges.iter().any(|e| e.before == 2 && e.after == 3));
}

#[test]
fn a_new_parent_keeps_its_existing_birth_and_attachment_contract() {
    let mut s = scene(false);
    let p: TableName = "app.p".parse().unwrap();
    s.before.tables.remove(&p);
    s.base_ids.tables.remove(&"t_aaaaaa".parse().unwrap());
    s.base_ids.columns.retain(|_, at| at.table != p);
    let mut birth = s.after.tables[&p].clone();
    let column = birth.columns.shift_remove("d").unwrap();
    let steps = vec![
        Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: p.clone(),
            table: Box::new(birth),
            beside_default: None,
            moves_at_most: None,
        },
        Change::AddColumn {
            uid: "c_bbbbbb".parse().unwrap(),
            table: p,
            name: "d".into(),
            column: Box::new(column),
        },
        attach(&s),
    ];
    let planned: Vec<_> = steps.into_iter().map(PlannedChange::new).collect();
    let ops = Operations::of_plan(&planned, ModuleEdits::Rebuilt);
    let owners = PlanOwners::new(
        PlanNames::of(&planned, &s.ids).unwrap(),
        &s.base_ids,
        &s.ids,
    );
    for pg in [
        Postgres::new(),
        Postgres::new().with_server_version(160000),
        Postgres::new().with_server_version(180000),
    ] {
        let fx = analyse(
            &ops,
            &owners,
            Snapshot {
                schema: &s.before,
                ids: &s.base_ids,
            },
            Snapshot {
                schema: &s.after,
                ids: &s.ids,
            },
            &pg,
        );
        assert!(fx.unowned.is_empty());
        assert!(fx.effects.iter().all(|e| e.symbolic.is_empty()));
        assert_eq!(fx.symbolic, Default::default());
        let graph = graph(
            ops.as_slice(),
            &fx.effects,
            &Baseline::of(
                Snapshot {
                    schema: &s.before,
                    ids: &s.base_ids,
                },
                &owners,
            ),
            &Default::default(),
            Checks::ALL,
        );
        assert_eq!(
            Schedule::of_graph(&graph, ops.as_slice())
                .sequence()
                .unwrap(),
            vec![0, 1, 2]
        );
    }
}

#[test]
fn ordinary_analysis_does_not_invent_membership_catalog_reads() {
    use pbps_model::operation::symbolic::{Clause, Fact};
    let mut s = scene(false);
    let parent: TableName = "app.p".parse().unwrap();
    let child: TableName = "app.a".parse().unwrap();
    for schema in [&mut s.before, &mut s.after] {
        schema
            .tables
            .get_mut(&parent)
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .value_source = ValueSource::Default("app.f(1)".into());
    }
    s.before
        .tables
        .get_mut(&child)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::Default("app.f(1)".into());
    s.after
        .tables
        .get_mut(&child)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .clear();
    let steps = vec![
        Change::AlterColumnDefault {
            uid: "c_dddddd".parse().unwrap(),
            column: child.column("d"),
            from: None,
            to: None,
        },
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: parent.column("d"),
            from: Some("app.f(1)".into()),
            to: None,
        },
        Change::AlterColumnDefault {
            uid: "c_bbbbbb".parse().unwrap(),
            column: parent.column("d"),
            from: None,
            to: Some("app.f(1)".into()),
        },
        Change::SetPartitionDefault {
            uid: "t_bbbbbb".parse().unwrap(),
            table: child,
            parent: parent.clone(),
            column: "d".into(),
            from: None,
            to: None,
            fallback: Some("app.f(1)".into()),
        },
        attach(&s),
    ];
    let planned: Vec<_> = steps.into_iter().map(PlannedChange::new).collect();
    let ops = Operations::of_plan(&planned, ModuleEdits::Rebuilt);
    let owners = PlanOwners::new(
        PlanNames::of(&planned, &s.ids).unwrap(),
        &s.base_ids,
        &s.ids,
    );
    let inspect = |after: &Schema, stored| {
        pbps_diff::order::analyse_with_membership(
            &ops,
            &owners,
            Snapshot {
                schema: &s.before,
                ids: &s.base_ids,
            },
            Snapshot {
                schema: after,
                ids: &s.ids,
            },
            &Postgres::new().with_server_version(180000),
            None,
            stored,
        )
    };
    let ordinary = pbps_diff::order::analyse_with_catalog(
        &ops,
        &owners,
        Snapshot {
            schema: &s.before,
            ids: &s.base_ids,
        },
        Snapshot {
            schema: &s.after,
            ids: &s.ids,
        },
        &Postgres::new().with_server_version(180000),
        None,
    );
    assert!(ordinary.unowned.is_empty());
    assert!(ordinary.effects.iter().all(|e| e.symbolic.is_empty()));
    assert_eq!(ordinary.symbolic, Default::default());
    // An explicit successful but empty read does not prove expression bindings.
    let empty_read = Default::default();
    let observed = inspect(&s.after, Some(&empty_read));
    assert!(
        observed
            .effects
            .iter()
            .flat_map(|e| &e.symbolic)
            .any(|c| matches!(c, Clause::Recurse(w) if w.from == Fact::Unread))
    );
    // An explicit contract with no read still demands its missing facts.
    s.after
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .get_mut("d")
        .unwrap()
        .value_source = ValueSource::Default("app.f(2)".into());
    let changed = inspect(&s.after, None);
    assert!(
        changed
            .effects
            .iter()
            .flat_map(|e| &e.symbolic)
            .any(|c| matches!(c, Clause::Recurse(w) if w.from == Fact::Unread))
    );
}

mod restoration_tests;
