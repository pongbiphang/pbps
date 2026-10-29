//! Reference class scenes below schema_diff; canonical assertions remain PR 7's gate.
use super::*;
use crate::engine_effects::scenes_test::{self as scenes, KINDS, Kind, Motion};

use scenes::steps;

#[test]
fn every_reference_class_scene_has_the_same_cut_or_named_endpoint_limit() {
    let mut accepted = 0;
    for kind in KINDS {
        for motion in [Motion::Attach, Motion::Detach, Motion::Drop] {
            for final_cut in [false, true] {
                for version in [160000, 180000] {
                    let s = scenes::build(kind, motion, final_cut);
                    let planned: Vec<_> = steps(&s, kind, motion)
                        .into_iter()
                        .map(PlannedChange::new)
                        .collect();
                    let ops = Operations::of_plan(&planned, ModuleEdits::Rebuilt);
                    let owners = PlanOwners::new(
                        PlanNames::of(&planned, &s.ids).unwrap(),
                        &s.base_ids,
                        &s.ids,
                    );
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
                        &Postgres::new().with_server_version(version),
                    );
                    assert!(fx.unowned.is_empty());
                    let graph = graph(
                        ops.as_slice(),
                        &fx.effects,
                        &Baseline::of(before, &owners).with_symbolic(fx.symbolic.clone()),
                        &Default::default(),
                        Checks::ALL,
                    );
                    let label = format!("{kind:?}/{motion:?}/final={final_cut}/PG{version}");
                    let result = Schedule::of_graph(&graph, ops.as_slice()).sequence();
                    let sequence = match result {
                        Ok(s) => s,
                        Err(e) => {
                            let conservative = motion == Motion::Attach
                                && final_cut
                                && ((matches!(kind, Kind::SetNotNull) && version == 160000)
                                    || matches!(kind, Kind::DropDefault));
                            assert!(
                                conservative
                                    && e.to_string()
                                        .contains("different physical endpoint identities"),
                                "{label}: {e}"
                            );
                            continue;
                        }
                    };
                    if motion != Motion::Drop
                        && matches!(
                            kind,
                            Kind::Add
                                | Kind::Generated
                                | Kind::Drop
                                | Kind::Rename
                                | Kind::Widen
                                | Kind::Narrow
                                | Kind::Collation
                        )
                    {
                        assert_eq!(
                            sequence,
                            if final_cut { vec![0, 1] } else { vec![1, 0] },
                            "{label}"
                        );
                    }
                    let mut end = None;
                    let mut orders = vec![Vec::new()];
                    for at in 0..ops.len() {
                        orders = orders
                            .into_iter()
                            .flat_map(|prefix| {
                                (0..=prefix.len()).map(move |i| {
                                    let mut order = prefix.clone();
                                    order.insert(i, at);
                                    order
                                })
                            })
                            .collect();
                    }
                    for order in orders {
                        if !graph.edges.iter().all(|e| {
                            order.iter().position(|i| *i == e.before)
                                < order.iter().position(|i| *i == e.after)
                        }) {
                            continue;
                        }
                        let reordered = pbps_diff::order::reordered_operations(&ops, &order);
                        let effects: Vec<_> =
                            order.iter().map(|i| fx.effects[*i].clone()).collect();
                        let replay = pbps_diff::order::membership::project(
                            reordered.as_slice(),
                            &effects,
                            &fx.symbolic,
                        )
                        .unwrap_or_else(|e| panic!("{label}: {e}"));
                        let state = replay.state(pbps_model::plan_names::At::End).unwrap();
                        if let Some(previous) = &end {
                            assert_eq!(previous, state, "{label}");
                        } else {
                            end = Some(state.clone());
                        }
                    }
                    assert!(end.is_some(), "{label}");
                    accepted += 1;
                }
            }
        }
    }
    assert_eq!(accepted, 153);
}

use scenes::{build, name, uid};
#[test]
fn an_absent_owned_sequence_has_no_source_and_is_never_inherited() {
    let s = build(Kind::Widen, Motion::Attach, false);
    let before = Snapshot {
        schema: &s.before,
        ids: &s.base_ids,
    };
    let after = Snapshot {
        schema: &s.after,
        ids: &s.ids,
    };
    let pg = Postgres::new().with_server_version(180000);
    let facts = facts::of(&pg, &Default::default(), before, after);
    for state in [&facts.base, &facts.goal] {
        for ((_, p), cell) in &state.cells {
            if matches!(
                p,
                pbps_model::operation::symbolic::Property::Column(
                    _,
                    pbps_model::operation::symbolic::Attribute::Numbering
                )
            ) {
                let value = cell.value.known().unwrap();
                assert_eq!(value.datum, pbps_model::operation::symbolic::Datum::Absent);
                assert_eq!(
                    value.ownership,
                    pbps_model::operation::symbolic::Ownership::Own
                );
                assert!(cell.origin.is_none());
            }
        }
    }
}
#[test]
fn a_missing_ordinary_uid_is_an_obligation_before_any_name_projection() {
    for missing_parent in [false, true] {
        let mut s = build(Kind::Widen, Motion::Attach, false);
        s.base_ids.columns.remove(&uid(if missing_parent {
            "c_bbbbbb"
        } else {
            "c_dddddd"
        }));
        let steps = vec![
            PlannedChange::new(Change::AlterColumnType {
                uid: uid("c_bbbbbb"),
                column: name("app.p").column("d"),
                from: "integer".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: None,
            }),
            PlannedChange::new(steps(&s, Kind::Widen, Motion::Attach).remove(1)),
        ];
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
        assert!(fx.effects[1].symbolic.iter().any(|c|matches!(c,pbps_model::operation::symbolic::Clause::Require(pbps_model::operation::symbolic::Fact::Unsupported{reason,..}) if reason.contains("ordinary column without a recorded UID"))),"{:?}",fx.effects[1].symbolic);
    }
}
