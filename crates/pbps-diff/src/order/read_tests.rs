//! Graph/replay agreement across every ordering of each three-step lifetime.
use super::*;
use pbps_model::operation::*;
use pbps_model::*;

const ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];
fn operations() -> Operations {
    Operations::of_plan(
        &(0..3)
            .map(|i| {
                PlannedChange::new(Change::DropModule {
                    id: ModuleId::Named(TableName::new("app", format!("dummy{i}"))),
                    kind: ModuleKind::View,
                })
            })
            .collect::<Vec<_>>(),
        ModuleEdits::InPlace,
    )
}
fn uid(kind: UidKind) -> Uid {
    Uid::derived(kind, "read boundary", 0)
}

fn object_key(kind: UidKind, seed: &str) -> ObjectKey {
    let ids = IdsFile::default();
    let owners = PlanOwners::new(
        pbps_model::plan_names::PlanNames::of(&[], &ids).unwrap(),
        &ids,
        &ids,
    );
    let uid = Uid::derived(kind, seed, 0);
    let name = TableName::new("app", "source");
    let change = if kind == UidKind::Column {
        Change::DropColumn {
            uid,
            column: name.column("k"),
        }
    } else {
        Change::DropTable {
            uid,
            name,
            detach_from: None,
        }
    };
    Operations::of_plan(&[PlannedChange::new(change)], ModuleEdits::InPlace).as_slice()[0]
        .key(&owners)
        .unwrap()
}

#[test]
fn tree_and_column_value_lifetimes_agree_with_independent_replay() {
    for key in [
        object_key(UidKind::Table, "read boundary"),
        object_key(UidKind::Column, "read boundary"),
    ] {
        for (incarnation, bound, base) in [
            (Incarnation::Base, false, true),
            (Incarnation::Base, true, true),
            (Incarnation::Final, false, true),
            (Incarnation::Final, true, true),
            (Incarnation::Final, false, false),
            (Incarnation::Final, true, false),
        ] {
            let ops = operations();
            let creates = Effects {
                creates: vec![
                    Resource::Object(key.clone(), Incarnation::Final),
                    Resource::Column(key.clone()),
                ],
                ..Default::default()
            };
            let removes = Effects {
                removes: vec![
                    Resource::Object(key.clone(), Incarnation::Base),
                    Resource::Column(key.clone()),
                ],
                ..Default::default()
            };
            let requirement = if bound {
                Requirement::Bound(
                    ObjectKey::Module(ModuleId::Named(TableName::new("app", "binding_surface"))),
                    BoundInput {
                        input: key.clone(),
                        incarnation,
                    },
                )
            } else {
                Requirement::Is(key.clone(), incarnation)
            };
            let reads = Effects {
                requires: vec![requirement, Requirement::Reads(key.clone())],
                ..Default::default()
            };
            let fx = if base {
                vec![removes, creates, reads]
            } else {
                vec![creates, reads, removes]
            };
            let baseline = Baseline::from_keys(base.then_some(key.clone()));
            let g = graph(
                ops.as_slice(),
                &fx,
                &baseline,
                &Default::default(),
                Checks::ALL,
            );
            for order in ORDERS {
                let at = |i| order.iter().position(|&j| i == j).unwrap();
                let edges_hold =
                    g.unmet.is_empty() && g.edges.iter().all(|e| at(e.before) < at(e.after));
                let reordered = reordered_operations(&ops, &order);
                let fx: Vec<_> = order.iter().map(|&i| fx[i].clone()).collect();
                let violations =
                    verify::verify(reordered.as_slice(), &fx, &baseline, &Default::default());
                // The existing creation/retirement contract fixes physical
                // generations; compare read replay within those lifetimes.
                let generation_holds = if base { at(0) < at(1) } else { at(0) < at(2) };
                let replay_holds = generation_holds
                    && !violations.iter().any(|v| {
                        matches!(
                            v,
                            verify::Violation::Unmet { .. }
                                | verify::Violation::StillPresent { .. }
                        )
                    });
                let expected = if !base {
                    at(0) < at(1) && at(1) < at(2)
                } else if incarnation == Incarnation::Base {
                    at(2) < at(0) && at(0) < at(1)
                } else {
                    at(0) < at(1) && at(1) < at(2)
                };
                assert_eq!(
                    edges_hold, expected,
                    "{key:?} {incarnation:?} bound={bound} base={base} {order:?}: {g:?}"
                );
                assert_eq!(
                    replay_holds, expected,
                    "{key:?} {incarnation:?} bound={bound} base={base} {order:?}: {violations:?}"
                );
            }
        }
    }
}

#[test]
fn slot_read_edges_and_holder_replay_agree_at_all_three_address_boundaries() {
    use pbps_dialect::slots::{Namespace, SlotKey};
    use pbps_model::plan_names::PlanNames;
    let dialect = super::slot_tests::Slotted { exact: true };
    let slot = |name: &str| SlotKey {
        namespace: Namespace::Relations("app".into()),
        name: name.into(),
    };
    let key = object_key(UidKind::Table, "read boundary");
    let ops = operations();
    for boundary in ["source", "temporary", "backup"] {
        let mut ids = IdsFile::default();
        let mut schema = Schema::default();
        if boundary != "temporary" {
            ids.tables
                .insert(uid(UidKind::Table), TableName::new("app", "source"));
            schema
                .tables
                .insert(TableName::new("app", "source"), Table::default());
        }
        let owners = PlanOwners::new(PlanNames::of(&[], &ids).unwrap(), &ids, &ids);
        let before = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        let mut fx = vec![slots::SlotEffects::default(); 3];
        let (from, to, read) = if boundary == "temporary" {
            ("temporary", "final", "temporary")
        } else {
            (
                "source",
                "backup",
                if boundary == "source" {
                    "source"
                } else {
                    "backup"
                },
            )
        };
        if boundary == "temporary" {
            fx[0].claims.push((slot(from), key.clone()));
        }
        fx[1].reads.push((slot(from), key.clone()));
        fx[1].releases.push((slot(from), key.clone()));
        fx[1].claims.push((slot(to), key.clone()));
        fx[2].reads.push((slot(read), key.clone()));
        let slots = slots::Slots {
            effects: fx,
            ..Default::default()
        };
        let edges = slots::edges(&slots, None, &[]);
        for order in ORDERS {
            let at = |i| order.iter().position(|&j| i == j).unwrap();
            let edges_hold = edges.iter().all(|e| at(e.before) < at(e.after));
            let ordered = reordered_operations(&ops, &order);
            let slots = slots::Slots {
                effects: order.iter().map(|&i| slots.effects[i].clone()).collect(),
                ..Default::default()
            };
            let (violations, skipped) =
                slots::verify(ordered.as_slice(), &slots, &dialect, None, &owners, before);
            assert!(skipped.is_empty());
            let replay_holds = !violations
                .iter()
                .any(|v| !matches!(v, slots::SlotViolation::Edge { .. }));
            assert_eq!(
                edges_hold, replay_holds,
                "{boundary} {order:?}: {violations:?}"
            );
        }
        let mut unknown = slots.clone();
        unknown.effects[2].reads[0].0.name = "SOURCE".into();
        let collated = super::slot_tests::Slotted { exact: false };
        assert!(
            slots::verify(ops.as_slice(), &unknown, &collated, None, &owners, before)
                .0
                .iter()
                .any(|v| matches!(v, slots::SlotViolation::MissingInput { .. }))
        );
        // A same-spelled slot held by a different UID cannot satisfy the read.
        let mut wrong = slots.clone();
        wrong.effects[2].reads[0].1 = object_key(UidKind::Table, "other");
        assert!(
            slots::verify(ops.as_slice(), &wrong, &dialect, None, &owners, before)
                .0
                .iter()
                .any(|v| matches!(v, slots::SlotViolation::MissingInput { .. }))
        );
    }
}

#[test]
fn retired_tree_and_column_values_cannot_be_read_while_the_definition_still_stands() {
    for key in [
        object_key(UidKind::Table, "read boundary"),
        object_key(UidKind::Column, "read boundary"),
    ] {
        let ops = operations();
        let fx = vec![
            Effects {
                removes: vec![Resource::Column(key.clone())],
                ..Default::default()
            },
            Effects {
                requires: vec![
                    Requirement::Is(key.clone(), Incarnation::Base),
                    Requirement::Reads(key.clone()),
                ],
                ..Default::default()
            },
            Effects::default(),
        ];
        let base = Baseline::from_keys([key]);
        let g = graph(ops.as_slice(), &fx, &base, &Default::default(), Checks::ALL);
        assert!(g.edges.iter().any(|e| e.before == 1 && e.after == 0));
        let violations = verify::verify(ops.as_slice(), &fx, &base, &Default::default());
        assert!(violations.iter().any(|v| matches!(
            v,
            verify::Violation::Unmet {
                requirement: Requirement::Reads(_),
                ..
            }
        )));
    }
}
