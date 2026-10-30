//! The unchanged reference canonical oracle, sharing the original scene builder.
use super::*;
use crate::engine_effects::scenes_test::{KINDS, Kind, Motion, build, name, uid};
use pbps_dialect::PlanPayload;
use pbps_diff::{
    Side,
    order::deployment::{
        Sources,
        schedule::{Declarations, Facts, schedule},
    },
};
#[test]
fn column_membership_classes_finish_or_name_the_retained_endpoint_limit() {
    let mut cases = 0;
    let mut failures = Vec::new();
    for kind in KINDS {
        for motion in [Motion::Attach, Motion::Detach, Motion::Drop] {
            for cut_final in [false, true] {
                for version in [160000, 180000] {
                    // A changed DEFAULT can be inherited only when its writer saw A.
                    // ATTACH preserves an already equal candidate as an own/equal
                    // recording; both cuts are represented by the endpoint goal.
                    let s = build(kind, motion, cut_final);
                    let before = Side {
                        schema: &s.before,
                        ids: &s.base_ids,
                    };
                    let after = Side {
                        schema: &s.after,
                        ids: &s.ids,
                    };
                    let pg = Postgres::new().with_server_version(version);
                    let label = format!("{kind:?}/{motion:?}/final={cut_final}/PG{version}");
                    let plan = pbps_diff::diff(before, after, &pg, &Default::default())
                        .unwrap_or_else(|e| panic!("{label}: {e:?}"));
                    let policies = s
                        .base_ids
                        .tables
                        .keys()
                        .chain(s.base_ids.columns.keys())
                        .cloned()
                        .map(|u| (u, Vec::new()))
                        .collect();
                    let done = schedule(
                        plan,
                        &Declarations {
                            schema: &s.after,
                            sources: Sources {
                                before,
                                after: &s.ids,
                            },
                            module_deps: &Default::default(),
                        },
                        &Facts::default()
                            .with_module_dependents(&Default::default())
                            .with_policy_dependents(&policies)
                            .with_membership_catalog(&Default::default()),
                        &pg,
                    );
                    let done = match done {
                        Ok(done) => done,
                        Err(e) => {
                            let conservative = motion == Motion::Attach
                                && cut_final
                                && ((matches!(kind, Kind::SetNotNull) && version == 160000)
                                    || matches!(kind, Kind::DropDefault));
                            if conservative
                                && e.to_string()
                                    .contains("different physical endpoint identities")
                            {
                                continue;
                            }
                            failures.push(format!("{label}: {e}"));
                            continue;
                        }
                    };
                    let notes = done
                        .changes
                        .changes
                        .iter()
                        .flat_map(|p| &p.findings)
                        .filter(|f| f.id == "plan.partition-alter-attach-lock")
                        .count();
                    assert_eq!(
                        notes,
                        usize::from(
                            motion == Motion::Attach
                                && !matches!(kind, Kind::ParentName | Kind::ParentSchema)
                        ),
                        "{label}"
                    );
                    let ordered = done
                        .changes
                        .ordered(pbps_model::PlanMode::Transactional, &pg);
                    assert!(!ordered.changes().is_empty(), "{label}");
                    let operations = ordered.operations();
                    let owners = PlanOwners::new(
                        PlanNames::of(&ordered.changes().changes, &s.ids).unwrap(),
                        &s.base_ids,
                        &s.ids,
                    );
                    let fx = analyse(
                        &operations,
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
                    assert!(fx.unowned.is_empty(), "{label}: {:?}", fx.unowned);
                    let result = pbps_diff::order::membership::project(
                        operations.as_slice(),
                        &fx.effects,
                        &fx.symbolic,
                    );
                    assert!(result.is_ok(), "{label}: {result:?}");
                    let g = graph(
                        operations.as_slice(),
                        &fx.effects,
                        &Baseline::of(
                            Snapshot {
                                schema: &s.before,
                                ids: &s.base_ids,
                            },
                            &owners,
                        )
                        .with_symbolic(fx.symbolic.clone()),
                        &Default::default(),
                        Checks::ALL,
                    );
                    assert_permitted_orders(&operations, &fx.effects, &fx.symbolic, &g, &label);

                    cases += 1;
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}; {cases} accepted",
        failures.join("\n")
    );
    assert_eq!(cases, 153);
}

#[test]
fn physical_column_lists_are_obligations_at_attach_and_detach() {
    for motion in [Motion::Attach, Motion::Detach] {
        let mut s = build(Kind::Widen, motion, false);
        let a = name("app.a");
        if motion == Motion::Attach {
            s.before
                .tables
                .get_mut(&a)
                .unwrap()
                .columns
                .swap_indices(0, 1);
        } else {
            s.after
                .tables
                .get_mut(&a)
                .unwrap()
                .columns
                .insert("extra".into(), Column::new("integer".parse().unwrap()));
            s.ids.columns.insert(uid("c_ffffff"), a.column("extra"));
        }
        let pg = Postgres::new().with_server_version(180000);
        let before = Side {
            schema: &s.before,
            ids: &s.base_ids,
        };
        let after = Side {
            schema: &s.after,
            ids: &s.ids,
        };
        let plan = pbps_diff::diff(before, after, &pg, &Default::default()).unwrap();
        let policies = s
            .base_ids
            .tables
            .keys()
            .chain(s.base_ids.columns.keys())
            .cloned()
            .map(|u| (u, Vec::new()))
            .collect();
        let error = schedule(
            plan,
            &Declarations {
                schema: &s.after,
                sources: Sources {
                    before,
                    after: &s.ids,
                },
                module_deps: &Default::default(),
            },
            &Facts::default()
                .with_policy_dependents(&policies)
                .with_membership_catalog(&Default::default()),
            &pg,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("physical column order") || error.contains("physical order"),
            "{motion:?}: {error}"
        );
        assert!(
            error.contains(pbps_diff::order::membership::REMEDY),
            "{error}"
        );
    }
}
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
    let facts = super::super::facts::of(&pg, &Default::default(), before, after);
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
            PlannedChange::new(
                crate::engine_effects::scenes_test::steps(&s, Kind::Widen, Motion::Attach)
                    .pop()
                    .unwrap(),
            ),
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

pub(super) fn assert_permitted_orders(
    operations: &Operations,
    effects: &[pbps_model::operation::Effects],
    symbolic: &pbps_model::operation::symbolic::State,
    g: &pbps_diff::order::Graph,
    label: &str,
) {
    let mut orders = vec![Vec::new()];
    for at in 0..operations.len() {
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
    let mut end = None;
    for order in orders {
        let mut positions = vec![0; order.len()];
        for (at, &i) in order.iter().enumerate() {
            positions[i] = at;
        }
        if !g
            .edges
            .iter()
            .all(|e| positions[e.before] < positions[e.after])
        {
            continue;
        }
        let ops = pbps_diff::order::reordered_operations(operations, &order);
        let effects: Vec<_> = order.iter().map(|&i| effects[i].clone()).collect();
        let replay = pbps_diff::order::membership::project(ops.as_slice(), &effects, symbolic)
            .unwrap_or_else(|e| panic!("{label}/{order:?}: {e}"));
        let state = replay.state(pbps_model::plan_names::At::End).unwrap();
        if let Some(previous) = &end {
            assert_eq!(previous, state, "{label}/{order:?}");
        } else {
            end = Some(state.clone());
        }
    }

    assert!(end.is_some(), "{label}: no permitted replay");
}

mod cuts;

use super::policy_support::{fixture, rows};
use pbps_model::operation::symbolic::{Attribute, Property};
use serde::Deserialize;
#[derive(Deserialize)]
struct Policy {
    change: Change,
    pg16: String,
    pg18: Option<String>,
    sequence_input: bool,
    parent_uid: Option<String>,
}
#[test]
fn the_policy_data_covers_statement_kinds_on_pg16_and_pg18() {
    let cases: Vec<Policy> = serde_json::from_str(include_str!("class/policies.json")).unwrap();
    let mut count = 0;
    assert_eq!(cases.len(), 18);
    for Policy {
        change,
        pg16,
        pg18,
        sequence_input,
        parent_uid,
    } in cases
    {
        assert_eq!(
            change.parent_column_uid().map(|u| u.as_str()),
            parent_uid.as_deref()
        );
        for (version, expected) in [
            (160000, pg16.as_str()),
            (180000, pg18.as_deref().unwrap_or(&pg16)),
        ] {
            let actual = fixture(&change, Some(version), |f| {
                if sequence_input {
                    for ((_, p), c) in &mut f.base.cells {
                        if matches!(p, Property::Column(_, Attribute::Numbering)) {
                            c.value = facts::definition("owned sequence");
                        }
                    }
                }
            });
            assert_eq!(rows(&actual), expected, "{version}: {change:?}");
            count += 1;
        }
    }
    assert_eq!(count, 36);
}
