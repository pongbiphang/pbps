//! Generate declarations, then enumerate orders with the independent replay.
//! No generated fixture can be lowered or used as an executable plan.
use super::*;

struct Seed(u64);
impl Seed {
    fn pick(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}
fn tables() -> [Recorded; 3] {
    [
        recorded("t_aaaaaa"),
        recorded("t_bbbbbb"),
        recorded("t_cccccc"),
    ]
}
fn properties() -> [Property; 2] {
    [Property::Address, Property::Rows]
}
fn base(n: usize) -> Fixture {
    let mut f = Fixture::new(n);
    f.base = State::default();
    for table in tables() {
        f.base.tables.insert(table.clone());
        for property in properties() {
            f.base.cells.insert(
                (table.clone(), property.clone()),
                cell_value(&table, &property, value("1")),
            );
        }
    }
    f
}
fn movement(parent: usize, child: usize, kind: MoveKind, ts: Vec<Transfer>) -> Clause {
    Clause::Move(s::Move {
        parent: tables()[parent].clone(),
        child: tables()[child].clone(),
        kind,
        consumes: (kind != MoveKind::Attach).then_some(Version::Base),
        boundary: Boundary::All(ts),
    })
}
fn transfer(parent: usize, from: usize, to: usize) -> Transfer {
    Transfer {
        parent: properties()[parent].clone(),
        from: properties()[from].clone(),
        to: properties()[to].clone(),
        check: ShapeCheck::None,
        policy: TransferPolicy::Preserve,
        goal: Fact::Known(value("1")),
    }
}
fn member(f: &mut Fixture, parent: usize, child: usize) {
    f.base.members.insert(
        tables()[child].clone(),
        Membership {
            parent: tables()[parent].clone(),
            incarnation: Version::Base,
            aliases: properties().into_iter().map(|p| (p.clone(), p)).collect(),
        },
    );
}
#[derive(Default)]
struct Audit {
    cases: usize,
    accepted: usize,
    named: usize,
    cycles: usize,
    orders: usize,
    accepted_orders: usize,
    replayable: usize,
    disagreements: Vec<String>,
    outcomes: Vec<String>,
}
impl Audit {
    fn check(&mut self, label: String, f: &Fixture) {
        self.cases += 1;
        let g = f.graph();
        let sequence = Schedule::of_graph(&g, &f.ops).sequence();
        self.outcomes.push(format!(
            "{label}: {} {:?}",
            if sequence.is_ok() {
                "accepted"
            } else {
                "refused"
            },
            g.symbolic_unmet
                .iter()
                .map(|p| &p.detail)
                .collect::<Vec<_>>()
        ));
        let mut topological = 0;
        let mut replayable = 0;
        let mut bad = Vec::new();
        // Do not let obligations hide graph-permitted orders from the audit,
        // or an empty set of orders count as successful replay vacuously.
        for order in permutations(f.ops.len()) {
            let mut positions = vec![0; order.len()];
            for (at, &i) in order.iter().enumerate() {
                positions[i] = at;
            }
            let permits = g
                .edges
                .iter()
                .all(|e| positions[e.before] < positions[e.after]);
            let ordered = f.reordered(&order);
            let replay = project(&ordered.ops, &ordered.fx, &ordered.base);
            replayable += usize::from(replay.is_ok());
            if permits {
                topological += 1;
                if let Err(e) = replay {
                    bad.push(format!("{order:?}: {}", e.detail));
                }
            }
        }
        self.orders += topological;
        self.replayable += replayable;
        let error = if let Ok(order) = sequence {
            self.accepted += 1;
            self.accepted_orders += topological;
            let ordered = f.reordered(&order);
            assert!(g.symbolic_unmet.is_empty());
            if topological == 0
                || !bad.is_empty()
                || project(&ordered.ops, &ordered.fx, &ordered.base).is_err()
            {
                Some(format!(
                    "accepted, {topological} topological orders, {} unsafe: {bad:?}",
                    bad.len()
                ))
            } else {
                None
            }
        } else if !g.symbolic_unmet.is_empty() {
            self.named += 1;
            assert!(matches!(sequence, Err(crate::order::Refusal::Unmet(_))));
            // A qualification limit must identify the immutable operation and
            // state its reason and remedy, even if concrete replay is possible.
            for p in &g.symbolic_unmet {
                assert!(f.ops.iter().any(|op| op.id() == p.operation));
                assert!(!p.detail.is_empty());
                assert!(p.to_string().contains(REMEDY));
            }
            None
        } else {
            self.cycles += 1;
            (replayable > 0).then(|| format!("unnamed refusal, {replayable} replayable orders"))
        };
        if let Some(error) = error {
            self.disagreements.push(format!(
                "{label}: {error}\nbase={:?}\nfx={:?}\nedges={:?}",
                f.base, f.fx, g.edges
            ));
        }
    }
}

fn generated_contracts(mut visit: impl FnMut(String, &Fixture)) {
    // Exhaust all directed membership graphs of up to three moves. This
    // includes fresh chains/cycles, competing parents and repeated children.
    let endpoints = [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)];
    for n in 0..=3_u32 {
        for mut code in 0..6_usize.pow(n) {
            let label = format!("moves/{n}/{code}");
            let mut f = base(n as usize);
            for fx in &mut f.fx {
                let (p, c) = endpoints[code % 6];
                code /= 6;
                fx.symbolic
                    .push(movement(p, c, MoveKind::Attach, vec![transfer(0, 0, 0)]));
            }
            visit(label, &f);
        }
    }
    // Exhaust occupancy and all two-transfer maps over two slots. Targets
    // may overlap other sources (atomic swaps), or an untouched occupied slot.
    for kind in [MoveKind::Attach, MoveKind::Detach, MoveKind::Drop] {
        for occupied in 0..4 {
            for code in 0..73 {
                let mut f = base(1);
                if kind != MoveKind::Attach {
                    member(&mut f, 0, 1);
                }
                for (i, p) in properties().into_iter().enumerate() {
                    if occupied & (1 << i) == 0 {
                        f.base.cells.remove(&(tables()[1].clone(), p));
                    }
                }
                let mut ts = Vec::new();
                let mut left = code;
                let len = if code == 0 {
                    0
                } else if code <= 8 {
                    left -= 1;
                    1
                } else {
                    left -= 9;
                    2
                };
                for _ in 0..len {
                    let v = left % 8;
                    left /= 8;
                    ts.push(transfer(v / 4, (v / 2) % 2, v % 2));
                }
                f.fx[0].symbolic.push(movement(0, 1, kind, ts));
                visit(format!("slots/{kind:?}/{occupied}/{code}"), &f);
            }
        }
    }
    // Dense valid boundaries keep acceptance coverage meaningful: different
    // cuts, same-value rebuilds, several writes and post-move restorations.
    for kind in [MoveKind::Attach, MoveKind::Detach, MoveKind::Drop] {
        for scope in [Scope::ParentOnly, Scope::MembersAtOperation] {
            for policy in [
                WritePolicy::Overwrite,
                WritePolicy::PreserveOwn,
                WritePolicy::PreserveOwnership,
            ] {
                for transfer_policy in [TransferPolicy::Preserve, TransferPolicy::Inherit] {
                    for check in [ShapeCheck::None, ShapeCheck::SameDefinition] {
                        for restore_count in 0..=2 {
                            for goal in [value("1"), value("7"), inherited("1"), inherited("7")] {
                                let mut f = base(3 + restore_count);
                                if kind != MoveKind::Attach {
                                    member(&mut f, 0, 1);
                                }
                                for at in 0..2 {
                                    f.fx[at].symbolic.push(Clause::Recurse(RecursiveWrite {
                                        key: Key {
                                            parent: tables()[0].clone(),
                                            property: Property::Address,
                                        },
                                        lifetime: Lifetime::Retain,
                                        scope,
                                        policy,
                                        input: if at == 0 {
                                            Version::Base
                                        } else {
                                            Version::After(f.ops[0].id())
                                        },
                                        from: Fact::Known(if at == 0 {
                                            value("1")
                                        } else {
                                            value("7")
                                        }),
                                        to: Fact::Known(if at == 0 {
                                            value("7")
                                        } else {
                                            value("1")
                                        }),
                                    }));
                                }
                                let mut t = transfer(0, 0, 0);
                                t.policy = transfer_policy.clone();
                                t.check = check;
                                t.goal = Fact::Known(goal.clone());
                                f.fx[2].symbolic.push(movement(0, 1, kind, vec![t]));
                                for at in 3..f.ops.len() {
                                    f.fx[at].symbolic.push(Clause::Restore(Restoration {
                                        key: Key {
                                            parent: tables()[0].clone(),
                                            property: Property::Address,
                                        },
                                        child: tables()[1].clone(),
                                        property: Property::Address,
                                        value: Fact::Known(if at == 3 && restore_count == 2 {
                                            value("7")
                                        } else {
                                            goal.clone()
                                        }),
                                        member_of: (kind == MoveKind::Attach)
                                            .then(|| tables()[0].clone()),
                                    }));
                                }
                                visit(
                                    format!(
                                        "boundaries/{kind:?}/{scope:?}/{policy:?}/{transfer_policy:?}/{check:?}/{restore_count}/{goal:?}"
                                    ),
                                    &f,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    // Incompatible property cuts form a genuine cycle, rather than a named
    // qualification limit. Neither operation order can replay successfully.
    let mut cycle = base(2);
    for i in 0..2 {
        cycle.fx[0].symbolic.push(write(0, i));
    }
    cycle
        .base
        .cells
        .get_mut(&(tables()[1].clone(), Property::Rows))
        .unwrap()
        .value = Fact::Known(value("7"));
    let ts = (0..2)
        .map(|i| Transfer {
            policy: TransferPolicy::Inherit,
            check: ShapeCheck::SameDefinition,
            goal: Fact::Known(inherited("7")),
            ..transfer(i, i, i)
        })
        .collect();
    cycle.fx[1]
        .symbolic
        .push(movement(0, 1, MoveKind::Attach, ts));
    for input in permutations(2) {
        visit(format!("cycle/{input:?}"), &cycle.reordered(&input));
    }
    // Valid physical births/rebuilds need more than arbitrary random inputs:
    // a restoration through the wrong parent can corrupt the absent input.
    for rebuild in [false, true] {
        for matched in [false, true] {
            visit(
                format!("lifetime/{rebuild}/{matched}"),
                &lifetime_contract(rebuild, matched),
            );
        }
    }
    aliased_contracts(&mut visit);
    // Fixed-seed contracts combine baseline membership, up to three moves,
    // recursive writes, restorations and multiple clauses per operation.
    // Vary known cells, aliases, lifetime, scope, ownership and endpoint goals.
    let mut seed = Seed(0x2296_c0de_5eed_0003);
    for case in 0..12000 {
        let n = 1 + seed.pick(5);
        let mut f = base(n);
        match seed.pick(5) {
            1 => member(&mut f, 0, 1),
            2 => member(&mut f, 0, 2),
            3 => {
                member(&mut f, 0, 1);
                member(&mut f, 0, 2);
            }
            4 => member(&mut f, 1, 2),
            _ => {}
        }
        for c in f.base.cells.values_mut() {
            if seed.pick(12) == 0 {
                c.value = Fact::Known(inherited("1"));
            }
            if seed.pick(30) == 0 {
                c.value = Fact::Unread;
            }
            if seed.pick(20) == 0 {
                c.origin = None;
                c.value = Fact::Known(Value {
                    datum: Datum::Absent,
                    ..value("1")
                });
            }
        }
        if seed.pick(4) == 0 {
            f.base.cells.remove(&(
                tables()[seed.pick(3)].clone(),
                properties()[seed.pick(2)].clone(),
            ));
        }
        let mut moves = 0;
        let mut previous: BTreeMap<(usize, usize), (OperationId, Value)> = BTreeMap::new();
        for at in 0..n {
            let count = if seed.pick(5) == 0 { 2 } else { 1 };
            for _ in 0..count {
                let action = seed.pick(5);
                let p = seed.pick(3);
                let child = seed.pick(3);
                let prop = seed.pick(2);
                let clause = if action <= 1 && moves < 3 {
                    moves += 1;
                    let kind = match f.base.members.get(&tables()[child]) {
                        Some(_) if seed.pick(3) != 0 => {
                            if seed.pick(3) == 0 {
                                MoveKind::Drop
                            } else {
                                MoveKind::Detach
                            }
                        }
                        _ => MoveKind::Attach,
                    };
                    let mut ts = Vec::new();
                    for i in 0..seed.pick(3) {
                        let mut t =
                            transfer(i, if seed.pick(2) == 0 { i } else { prop }, seed.pick(2));
                        t.policy = if seed.pick(4) == 0 {
                            TransferPolicy::Inherit
                        } else {
                            TransferPolicy::Preserve
                        };
                        t.check = if seed.pick(4) == 0 {
                            ShapeCheck::SameDefinition
                        } else {
                            ShapeCheck::None
                        };
                        if t.policy == TransferPolicy::Inherit && kind == MoveKind::Attach {
                            t.goal = Fact::Known(inherited("1"));
                        }
                        ts.push(t);
                    }
                    movement(p, child, kind, ts)
                } else if action <= 3 {
                    let (input, from) = previous
                        .get(&(p, prop))
                        .cloned()
                        .map_or((Version::Base, value("1")), |(id, v)| {
                            (Version::After(id), v)
                        });
                    let to = if seed.pick(3) == 0 {
                        value("1")
                    } else {
                        value("7")
                    };
                    previous.insert((p, prop), (f.ops[at].id(), to.clone()));
                    Clause::Recurse(RecursiveWrite {
                        key: Key {
                            parent: tables()[p].clone(),
                            property: properties()[prop].clone(),
                        },
                        input,
                        from: Fact::Known(from),
                        to: Fact::Known(to),
                        lifetime: if seed.pick(12) == 0 {
                            Lifetime::Create
                        } else {
                            Lifetime::Retain
                        },
                        scope: if seed.pick(4) == 0 {
                            Scope::ParentOnly
                        } else {
                            Scope::MembersAtOperation
                        },
                        policy: if seed.pick(3) == 0 {
                            WritePolicy::PreserveOwn
                        } else {
                            WritePolicy::Overwrite
                        },
                    })
                } else {
                    Clause::Restore(Restoration {
                        key: Key {
                            parent: tables()[p].clone(),
                            property: properties()[prop].clone(),
                        },
                        child: tables()[child].clone(),
                        property: properties()[seed.pick(2)].clone(),
                        value: Fact::Known(if seed.pick(3) == 0 {
                            value("7")
                        } else {
                            value("1")
                        }),
                        member_of: if seed.pick(2) == 0 {
                            None
                        } else {
                            Some(tables()[p].clone())
                        },
                    })
                };
                f.fx[at].symbolic.push(clause);
            }
        }
        visit(format!("seed/{case}"), &f);
    }
}

#[test]
fn every_generated_membership_contract_agrees_with_all_its_topological_replays() {
    let mut audit = Audit::default();
    generated_contracts(|label, f| audit.check(label, f));
    let summary = format!(
        "cases={} accepted={} named={} cycles={} topological_orders={} accepted_orders={} replayable_orders={} disagreements={}",
        audit.cases,
        audit.accepted,
        audit.named,
        audit.cycles,
        audit.orders,
        audit.accepted_orders,
        audit.replayable,
        audit.disagreements.len()
    );
    if let Ok(path) = std::env::var("PBPS_MEMBERSHIP_AUDIT") {
        std::fs::write(
            &path,
            format!("{summary}\n{}", audit.disagreements.join("\n\n")),
        )
        .unwrap();
        std::fs::write(format!("{path}.outcomes"), audit.outcomes.join("\n")).unwrap();
    }
    eprintln!("{summary}");
    assert!(audit.accepted > 100 && audit.named > 100 && audit.cycles > 0);
    assert!(audit.accepted_orders > audit.accepted && audit.orders > audit.cases);
    assert!(
        audit.disagreements.is_empty(),
        "{}\n{}",
        summary,
        audit
            .disagreements
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

#[test]
fn declared_move_chains_and_cycles_are_named_obligations_before_replay() {
    for links in [vec![(0, 1), (1, 2)], vec![(0, 1), (1, 2), (2, 0)]] {
        let mut f = base(links.len());
        for (at, (p, c)) in links.into_iter().enumerate() {
            f.fx[at]
                .symbolic
                .push(movement(p, c, MoveKind::Attach, vec![transfer(0, 0, 0)]));
        }
        for input in permutations(f.ops.len()) {
            let input = f.reordered(&input);
            assert!(input.refusal().contains(
                "declared moves create an unqualified nested or cyclic membership scope"
            ));
            assert!(project(&input.ops, &input.fx, &input.base).is_err());
        }
    }
    // Independent children share a parent without becoming nested.
    let mut star = base(2);
    for (at, child) in [1, 2].into_iter().enumerate() {
        star.fx[at].symbolic.push(movement(
            0,
            child,
            MoveKind::Attach,
            vec![transfer(0, 0, 0)],
        ));
    }
    sound(&star);
}

#[test]
fn transfer_targets_exclude_untouched_cells_but_allow_atomic_source_swaps() {
    for kind in [MoveKind::Attach, MoveKind::Detach] {
        let mut f = base(1);
        if kind == MoveKind::Detach {
            member(&mut f, 0, 1);
        }
        f.fx[0]
            .symbolic
            .push(movement(0, 1, kind, vec![transfer(0, 0, 1)]));
        assert!(
            f.refusal()
                .contains("transfer target collides with an untouched physical slot")
        );
        assert!(
            project(&f.ops, &f.fx, &f.base)
                .unwrap_err()
                .detail
                .contains("transfer collides with another physical slot")
        );
        // The second source is removed before either target is installed.
        if let Clause::Move(m) = &mut f.fx[0].symbolic[0] {
            let Boundary::All(ts) = &mut m.boundary else {
                unreachable!()
            };
            ts.push(transfer(1, 1, 0));
        }
        sound(&f);
        // A vacant destination is also legal.
        f.base.cells.remove(&(tables()[1].clone(), Property::Rows));
        if let Clause::Move(m) = &mut f.fx[0].symbolic[0] {
            let Boundary::All(ts) = &mut m.boundary else {
                unreachable!()
            };
            ts.pop();
        }
        sound(&f);
    }
}

fn write(parent: usize, property: usize) -> Clause {
    Clause::Recurse(RecursiveWrite {
        key: Key {
            parent: tables()[parent].clone(),
            property: properties()[property].clone(),
        },
        input: Version::Base,
        from: Fact::Known(value("1")),
        to: Fact::Known(value("7")),
        lifetime: Lifetime::Retain,
        scope: Scope::MembersAtOperation,
        policy: WritePolicy::Overwrite,
    })
}
fn restoration(parent: usize, child: usize, property: usize) -> Clause {
    Clause::Restore(Restoration {
        key: Key {
            parent: tables()[parent].clone(),
            property: properties()[property].clone(),
        },
        child: tables()[child].clone(),
        property: properties()[property].clone(),
        value: Fact::Known(value("7")),
        member_of: None,
    })
}
#[test]
fn stationary_recursive_targets_require_known_aliases_sources_and_lifetimes() {
    for defect in ["alias", "cell", "unread", "lifetime"] {
        let mut f = base(1);
        member(&mut f, 0, 1);
        f.fx[0].symbolic.push(write(0, 0));
        sound(&f);
        let slot = (tables()[1].clone(), Property::Address);
        match defect {
            "alias" => {
                f.base
                    .members
                    .get_mut(&tables()[1])
                    .unwrap()
                    .aliases
                    .remove(&Property::Address);
            }
            "cell" => {
                f.base.cells.remove(&slot);
            }
            "unread" => {
                f.base.cells.get_mut(&slot).unwrap().value = Fact::Unread;
            }
            "lifetime" => {
                f.base.cells.get_mut(&slot).unwrap().origin = None;
            }
            _ => unreachable!(),
        }
        assert!(!f.admits(), "{defect}");
        assert!(project(&f.ops, &f.fx, &f.base).is_err(), "{defect}");
        // A parent-only write does not need the unavailable member property.
        if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
            w.scope = Scope::ParentOnly;
        }
        sound(&f);
    }
    // An attached member must carry aliases for the properties recursion uses.
    let mut f = base(2);
    f.fx[0].symbolic.push(write(0, 0));
    f.fx[1]
        .symbolic
        .push(movement(0, 1, MoveKind::Attach, vec![]));
    assert!(
        f.refusal()
            .contains("no declared membership transfer coverage")
    );
    assert!(project(&f.ops, &f.fx, &f.base).is_ok());
    let ordered = f.reordered(&[1, 0]);
    assert!(project(&ordered.ops, &ordered.fx, &ordered.base).is_err());
}

#[test]
fn overlapping_physical_inputs_need_an_explicit_shared_version_chain() {
    let mut cases = Vec::new();
    let mut f = base(2);
    f.fx[0].symbolic.push(restoration(0, 2, 1));
    f.fx[1].symbolic.push(write(2, 1));
    cases.push((f, "restoration overwrites a recursive input"));
    let mut f = base(2);
    member(&mut f, 0, 1);
    f.fx[0].symbolic.push(write(0, 0));
    f.fx[1].symbolic.push(write(1, 0));
    cases.push((f, "independent recursive chains alias"));
    let mut f = base(2);
    f.fx[0].symbolic.push(write(1, 0));
    f.fx[1]
        .symbolic
        .push(movement(0, 1, MoveKind::Attach, vec![transfer(0, 0, 0)]));
    cases.push((f, "recursive input shares a transferred or retired slot"));
    let mut f = base(2);
    f.fx[0].symbolic.push(restoration(2, 0, 0));
    let mut t = transfer(0, 0, 0);
    t.check = ShapeCheck::SameDefinition;
    f.fx[1]
        .symbolic
        .push(movement(0, 1, MoveKind::Attach, vec![t]));
    cases.push((f, "restoration overwrites a transfer parent input"));
    for (f, reason) in cases {
        assert!(f.refusal().contains(reason), "{reason}");
        assert!(permutations(f.ops.len()).iter().any(|order| {
            let ordered = f.reordered(order);
            project(&ordered.ops, &ordered.fx, &ordered.base).is_err()
        }));
        let mut audit = Audit::default();
        audit.check(reason.into(), &f);
        assert!(audit.disagreements.is_empty());
    }
}

fn lifetime_contract(rebuild: bool, matched: bool) -> Fixture {
    let mut f = base(if rebuild { 3 } else { 2 });
    member(&mut f, 0, 1);
    let absent = Value {
        datum: Datum::Absent,
        ..value("1")
    };
    if !rebuild {
        for table in &tables()[..2] {
            let c = f
                .base
                .cells
                .get_mut(&(table.clone(), Property::Address))
                .unwrap();
            c.value = Fact::Known(absent.clone());
            c.origin = None;
        }
    }
    let last_write = usize::from(rebuild);
    for at in 0..=last_write {
        f.fx[at].symbolic.push(Clause::Recurse(RecursiveWrite {
            key: Key {
                parent: tables()[0].clone(),
                property: Property::Address,
            },
            input: if at == 0 {
                Version::Base
            } else {
                Version::After(f.ops[0].id())
            },
            from: Fact::Known(if rebuild && at == 0 {
                value("1")
            } else {
                absent.clone()
            }),
            to: Fact::Known(if rebuild && at == 0 {
                absent.clone()
            } else {
                value("7")
            }),
            lifetime: if rebuild && at == 0 {
                Lifetime::Retire
            } else {
                Lifetime::Create
            },
            scope: Scope::MembersAtOperation,
            policy: WritePolicy::Overwrite,
        }));
    }
    let mut r = restoration(if matched { 0 } else { 2 }, 1, 0);
    if let Clause::Restore(r) = &mut r {
        r.member_of = Some(tables()[0].clone());
    }
    f.fx[last_write + 1].symbolic.push(r);
    f
}
#[test]
fn a_foreign_restoration_cannot_corrupt_a_recursive_members_physical_birth() {
    for rebuild in [false, true] {
        let f = lifetime_contract(rebuild, false);
        assert!(
            f.refusal()
                .contains("restoration key does not identify its recursive physical target")
        );
        let mut successes = 0;
        let mut failures = 0;
        for order in permutations(f.ops.len()) {
            let ordered = f.reordered(&order);
            if project(&ordered.ops, &ordered.fx, &ordered.base).is_ok() {
                successes += 1;
            } else {
                failures += 1;
            }
        }
        assert!(successes > 0 && failures > 0);
        // A matching key carries the write-before-restoration constraints.
        sound(&lifetime_contract(rebuild, true));
    }
}

mod necessity;

mod determinism;
pub(super) use determinism::declared_goals;
use determinism::{Determinism, aliased_contracts};
