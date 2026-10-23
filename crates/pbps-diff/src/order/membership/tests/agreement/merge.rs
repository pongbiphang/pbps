//! General merge tables: the read parent version determines transfer output.
use super::*;

fn policy(a: Value, b: Value, output_a: Value, output_b: Value) -> TransferPolicy {
    TransferPolicy::Merge(Merge {
        cases: vec![
            MergeCase {
                parent: a,
                candidate: value("9"),
                result: Fact::Known(output_a),
            },
            MergeCase {
                parent: b,
                candidate: value("9"),
                result: Fact::Known(output_b),
            },
        ],
    })
}
fn fixture(
    scope: Scope,
    policy: WritePolicy,
    transfer_policy: TransferPolicy,
    goal: Value,
    restore: bool,
) -> Fixture {
    let mut f = base(2 + usize::from(restore));
    f.base
        .cells
        .get_mut(&(tables()[1].clone(), Property::Address))
        .unwrap()
        .value = Fact::Known(value("9"));
    let mut w = write(0, 0);
    if let Clause::Recurse(w) = &mut w {
        w.scope = scope;
        w.policy = policy;
    }
    f.fx[0].symbolic.push(w);
    f.fx[1].symbolic.push(movement(
        0,
        1,
        MoveKind::Attach,
        vec![Transfer {
            policy: transfer_policy,
            goal: Fact::Known(goal.clone()),
            ..transfer(0, 0, 0)
        }],
    ));
    if restore {
        let mut r = restoration(0, 1, 0);
        if let Clause::Restore(r) = &mut r {
            r.value = Fact::Known(goal);
            r.member_of = Some(tables()[0].clone());
        }
        f.fx[2].symbolic.push(r);
    }
    f
}
pub(super) fn generated_contracts(mut visit: impl FnMut(String, &Fixture)) {
    not_null_contracts(&mut visit);
    for scope in [Scope::ParentOnly, Scope::MembersAtOperation] {
        for write_policy in [
            WritePolicy::Overwrite,
            WritePolicy::PreserveOwn,
            WritePolicy::PreserveOwnership,
        ] {
            for left in [value("9"), inherited("1"), inherited("7")] {
                for right in [value("9"), inherited("1"), inherited("7")] {
                    for goal in [value("9"), inherited("1"), inherited("7"), value("neither")] {
                        for restore in [false, true] {
                            let p = policy(value("1"), value("7"), left.clone(), right.clone());
                            let f = fixture(scope, write_policy, p, goal.clone(), restore);
                            for order in permutations(f.ops.len()) {
                                visit(
                                    format!(
                                        "merge/{scope:?}/{write_policy:?}/{left:?}/{right:?}/{goal:?}/{restore}/{order:?}"
                                    ),
                                    &f.reordered(&order),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
fn nn(value: bool, ownership: Ownership) -> Value {
    Value {
        datum: Datum::NotNull(value),
        bindings: BTreeSet::new(),
        ownership,
    }
}
fn not_null_contracts(mut visit: impl FnMut(String, &Fixture)) {
    for reverse in [false, true] {
        for scope in [Scope::ParentOnly, Scope::MembersAtOperation] {
            for wp in [
                WritePolicy::Overwrite,
                WritePolicy::PreserveOwn,
                WritePolicy::StrengthenNotNull,
            ] {
                for candidate in [nn(false, Ownership::Own), nn(true, Ownership::LocalNotNull)] {
                    let rows = [false, true]
                        .into_iter()
                        .map(|p| MergeCase {
                            parent: nn(p, Ownership::Own),
                            candidate: candidate.clone(),
                            result: Fact::Known(nn(
                                matches!(candidate.datum, Datum::NotNull(true)),
                                if !p && candidate.datum == Datum::NotNull(true) {
                                    Ownership::LocalNotNull
                                } else {
                                    Ownership::Inherited
                                },
                            )),
                        })
                        .collect();
                    let policy = TransferPolicy::Merge(Merge { cases: rows });
                    for goal in [
                        nn(false, Ownership::Inherited),
                        nn(true, Ownership::Inherited),
                        nn(true, Ownership::LocalNotNull),
                        nn(false, Ownership::Own),
                    ] {
                        for restore in [false, true] {
                            let mut f = base(2 + usize::from(restore));
                            f.base
                                .cells
                                .get_mut(&(tables()[0].clone(), Property::Address))
                                .unwrap()
                                .value = Fact::Known(nn(reverse, Ownership::Own));
                            f.base
                                .cells
                                .get_mut(&(tables()[1].clone(), Property::Address))
                                .unwrap()
                                .value = Fact::Known(candidate.clone());
                            let mut w = write(0, 0);
                            if let Clause::Recurse(w) = &mut w {
                                w.from = Fact::Known(nn(reverse, Ownership::Own));
                                w.to = Fact::Known(nn(!reverse, Ownership::Own));
                                w.scope = scope;
                                w.policy = wp;
                            }
                            f.fx[0].symbolic.push(w);
                            f.fx[1].symbolic.push(movement(
                                0,
                                1,
                                MoveKind::Attach,
                                vec![Transfer {
                                    check: ShapeCheck::NotNullAtLeast,
                                    policy: policy.clone(),
                                    goal: Fact::Known(goal.clone()),
                                    ..transfer(0, 0, 0)
                                }],
                            ));
                            if restore {
                                let mut r = restoration(0, 1, 0);
                                if let Clause::Restore(r) = &mut r {
                                    r.value = Fact::Known(goal.clone());
                                    r.member_of = Some(tables()[0].clone());
                                }
                                f.fx[2].symbolic.push(r);
                            }
                            for order in permutations(f.ops.len()) {
                                visit(
                                    format!(
                                        "merge-nn/{reverse}/{scope:?}/{wp:?}/{candidate:?}/{goal:?}/{restore}/{order:?}"
                                    ),
                                    &f.reordered(&order),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn a_parent_version_read_places_a_merge_on_only_the_goal_admissible_cut() {
    for (goal, expected) in [(value("before"), (1, 0)), (value("after"), (0, 1))] {
        let f = fixture(
            Scope::ParentOnly,
            WritePolicy::Overwrite,
            policy(value("1"), value("7"), value("before"), value("after")),
            goal,
            false,
        );
        assert!(
            f.edges()
                .contains(&(f.ops[expected.0].id(), f.ops[expected.1].id()))
        );
        graph_and_replay_agree(&f, &f.graph(), &[]);
        for order in permutations(2) {
            sound(&f.reordered(&order));
        }
    }
    let f = fixture(
        Scope::ParentOnly,
        WritePolicy::Overwrite,
        policy(value("1"), value("7"), value("same"), value("same")),
        value("same"),
        false,
    );
    assert!(f.edges().is_empty());
    sound(&f);
    let mut a = Determinism::default();
    a.check("both-cuts".into(), &f);
    assert!(a.differences.is_empty() && a.goals.is_empty());
    let f = fixture(
        Scope::ParentOnly,
        WritePolicy::Overwrite,
        policy(value("1"), value("7"), value("before"), value("after")),
        value("neither"),
        false,
    );
    assert!(f.refusal().contains("no boundary"));
    assert!(f.refusal().contains(REMEDY));
}
#[test]
fn unavailable_or_overlapping_merge_reads_are_named_instead_of_guessed() {
    for defect in ["missing", "overlap", "unread"] {
        let mut p = policy(value("1"), value("7"), value("9"), value("9"));
        let TransferPolicy::Merge(ref mut merge) = p else {
            unreachable!()
        };
        match defect {
            "missing" => {
                merge.cases.pop();
            }
            "overlap" => {
                merge.cases.push(merge.cases[0].clone());
            }
            "unread" => {
                merge.cases[0].result = Fact::Unread;
            }
            _ => unreachable!(),
        }
        let f = fixture(
            Scope::ParentOnly,
            WritePolicy::Overwrite,
            p,
            value("9"),
            false,
        );
        assert!(!f.admits());
        assert!(f.refusal().contains(REMEDY));
    }
}
