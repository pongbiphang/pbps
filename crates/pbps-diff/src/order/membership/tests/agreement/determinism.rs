//! Compare complete replay states and independently declared endpoint goals.
//! Version and physical origin are part of equality; neither is normalized.
use super::*;

pub(super) fn aliased_contracts(mut visit: impl FnMut(String, &Fixture)) {
    // Distinct parent/property keys deliberately share one physical target.
    // Include same-value writes: equal definitions do not erase event identity.
    for first in 0..6 {
        for second in 0..6 {
            for same in [false, true] {
                for kind in [None, Some(MoveKind::Attach), Some(MoveKind::Detach)] {
                    let mut f = base(2 + usize::from(kind.is_some()));
                    if kind == Some(MoveKind::Detach) {
                        member(&mut f, 0, 1);
                    }
                    if let Some(kind) = kind {
                        f.fx[2]
                            .symbolic
                            .push(movement(0, 1, kind, vec![transfer(0, 0, 0)]));
                    }
                    for (at, key) in [first, second].into_iter().enumerate() {
                        f.fx[at].symbolic.push(Clause::Restore(Restoration {
                            key: Key {
                                parent: tables()[key / 2].clone(),
                                property: properties()[key % 2].clone(),
                            },
                            child: tables()[1].clone(),
                            property: Property::Address,
                            value: Fact::Known(value(if same || at == 0 { "1" } else { "7" })),
                            member_of: (kind == Some(MoveKind::Attach))
                                .then(|| tables()[0].clone()),
                        }));
                    }
                    for order in permutations(f.ops.len()) {
                        visit(
                            format!("aliases/restores/{first}/{second}/{same}/{kind:?}/{order:?}"),
                            &f.reordered(&order),
                        );
                    }
                }
            }
        }
    }
    // Parent keys, member aliases, transfer maps and restoration keys vary
    // independently; several logical keys can address each physical cell.
    for recursive_key in 0..6 {
        for restore_key in 0..6 {
            for target in 0..2 {
                for kind in [None, Some(MoveKind::Attach), Some(MoveKind::Detach)] {
                    let mut f = base(2 + usize::from(kind.is_some()));
                    if kind != Some(MoveKind::Attach) {
                        member(&mut f, 0, 1);
                        // Swap aliases as well as the identity mapping.
                        if target == 1 {
                            f.base.members.get_mut(&tables()[1]).unwrap().aliases =
                                BTreeMap::from([
                                    (Property::Address, Property::Rows),
                                    (Property::Rows, Property::Address),
                                ]);
                        }
                    }
                    f.fx[0]
                        .symbolic
                        .push(write(recursive_key / 2, recursive_key % 2));
                    let mut r = restoration(restore_key / 2, 1, restore_key % 2);
                    if let Clause::Restore(r) = &mut r {
                        r.property = properties()[target].clone();
                        r.member_of = (kind != Some(MoveKind::Detach)).then(|| tables()[0].clone());
                    }
                    f.fx[1].symbolic.push(r);
                    if let Some(kind) = kind {
                        let from = if kind == MoveKind::Detach {
                            1 - target
                        } else {
                            target
                        };
                        let mut t = transfer(restore_key % 2, from, target);
                        t.goal = Fact::Known(value("7"));
                        f.fx[2].symbolic.push(movement(0, 1, kind, vec![t]));
                    }
                    for order in permutations(f.ops.len()) {
                        visit(
                            format!(
                                "aliases/mixed/{recursive_key}/{restore_key}/{target}/{kind:?}/{order:?}"
                            ),
                            &f.reordered(&order),
                        );
                    }
                }
            }
        }
    }
    // Independent recursive keys can also alias a member's physical input,
    // or a move's parent/source/target. Keep both clauses even when they share
    // a key: explicit predecessors then supply the same-key event chain.
    for first in 0..6 {
        for second in 0..6 {
            for swapped in [false, true] {
                for action in 0..3 {
                    let mut f = base(3);
                    if action != 1 {
                        member(&mut f, 0, 1);
                        if swapped {
                            f.base.members.get_mut(&tables()[1]).unwrap().aliases =
                                BTreeMap::from([
                                    (Property::Address, Property::Rows),
                                    (Property::Rows, Property::Address),
                                ]);
                        }
                    }
                    for (at, key) in [first, second].into_iter().enumerate() {
                        let mut w = write(key / 2, key % 2);
                        if at == 1
                            && first == second
                            && let Clause::Recurse(w) = &mut w
                        {
                            w.input = Version::After(f.ops[0].id());
                            w.from = Fact::Known(value("7"));
                            w.to = Fact::Known(value("1"));
                        }
                        f.fx[at].symbolic.push(w);
                    }
                    if action == 0 {
                        let mut r = restoration(second / 2, 1, usize::from(swapped));
                        if let Clause::Restore(r) = &mut r {
                            r.member_of = Some(tables()[0].clone());
                        }
                        f.fx[2].symbolic.push(r);
                    } else {
                        let mut t = transfer(0, usize::from(swapped), 0);
                        t.goal = Fact::Known(value("1"));
                        f.fx[2].symbolic.push(movement(
                            0,
                            1,
                            if action == 1 {
                                MoveKind::Attach
                            } else {
                                MoveKind::Detach
                            },
                            vec![t],
                        ));
                    }
                    for order in permutations(3) {
                        visit(
                            format!(
                                "aliases/recursive/{first}/{second}/{swapped}/{action}/{order:?}"
                            ),
                            &f.reordered(&order),
                        );
                    }
                }
            }
        }
    }
}

pub(in crate::order::membership::tests) fn declared_goals(f: &Fixture, end: &State) -> Vec<String> {
    let mut errors = Vec::new();
    let mut restores = BTreeMap::new();
    let inputs: BTreeSet<_> =
        f.fx.iter()
            .flat_map(|fx| &fx.symbolic)
            .filter_map(|c| {
                if let Clause::Recurse(w) = c {
                    Some((w.key.clone(), w.input))
                } else {
                    None
                }
            })
            .collect();
    for (at, fx) in f.fx.iter().enumerate() {
        for (clause, c) in fx.symbolic.iter().enumerate() {
            match c {
                Clause::Restore(r) => {
                    let entry = restores
                        .entry((r.child.clone(), r.property.clone()))
                        .or_insert(((f.ops[at].id(), clause), r));
                    if (f.ops[at].id(), clause) > entry.0 {
                        *entry = ((f.ops[at].id(), clause), r);
                    }
                }
                Clause::Recurse(w)
                    if !inputs.contains(&(w.key.clone(), Version::After(f.ops[at].id()))) =>
                {
                    if end
                        .cells
                        .get(&(w.key.parent.clone(), w.key.property.clone()))
                        .map(|c| (&c.value, c.version))
                        != Some((&w.to, Version::After(f.ops[at].id())))
                    {
                        errors.push(format!("recursive endpoint {:?}", w.key));
                    }
                }
                Clause::Move(m) if m.kind != MoveKind::Drop => {
                    if let Boundary::All(ts) = &m.boundary {
                        for t in ts {
                            if end
                                .cells
                                .get(&(m.child.clone(), t.to.clone()))
                                .map(|c| &c.value)
                                != Some(&t.goal)
                            {
                                errors.push(format!(
                                    "transfer endpoint {:?}",
                                    (m.child.clone(), t.to.clone())
                                ));
                            }
                        }
                    }
                }
                Clause::Recurse(_) | Clause::Move(_) | Clause::Require(_) => {}
            }
        }
    }
    for (target, ((id, _), r)) in restores {
        if end.cells.get(&target).map(|c| (&c.value, c.version))
            != Some((&r.value, Version::After(id)))
        {
            errors.push(format!("restoration endpoint {target:?}"));
        }
    }
    errors
}
#[derive(Default)]
pub(super) struct Determinism {
    cases: usize,
    accepted: usize,
    orders: usize,
    pub(super) differences: Vec<String>,
    pub(super) goals: Vec<String>,
}
impl Determinism {
    pub(super) fn check(&mut self, label: String, f: &Fixture) {
        self.cases += 1;
        let g = f.graph();
        if Schedule::of_graph(&g, &f.ops).sequence().is_err() {
            return;
        }
        self.accepted += 1;
        let mut ends = Vec::new();
        let mut bad_goals = Vec::new();
        for order in permutations(f.ops.len()) {
            if !permits(&g, &order) {
                continue;
            }
            self.orders += 1;
            let ordered = f.reordered(&order);
            let p = project(&ordered.ops, &ordered.fx, &ordered.base).expect("soundness");
            let end = p.state(At::End).unwrap().clone();
            let errors = declared_goals(f, &end);
            if !errors.is_empty() {
                bad_goals.push(format!("{order:?}: {errors:?}"));
            }
            if ends.iter().all(|(_, e)| e != &end) {
                ends.push((order, end));
            }
        }
        assert!(!ends.is_empty());
        if ends.len() != 1 {
            self.differences.push(format!(
                "{label}: {} distinct states\nends={ends:?}\nbase={:?}\nfx={:?}",
                ends.len(),
                f.base,
                f.fx
            ));
        }
        if !bad_goals.is_empty() {
            self.goals.push(format!(
                "{label}: {bad_goals:?}\nbase={:?}\nfx={:?}",
                f.base, f.fx
            ));
        }
    }
}
#[test]
fn every_permitted_membership_order_reaches_the_same_declared_final_state() {
    let mut audit = Determinism::default();
    generated_contracts(|label, f| audit.check(label, f));
    let summary = format!(
        "cases={} accepted={} orders={} nondeterministic={} wrong_goals={}",
        audit.cases,
        audit.accepted,
        audit.orders,
        audit.differences.len(),
        audit.goals.len()
    );
    if let Ok(path) = std::env::var("PBPS_MEMBERSHIP_DETERMINISM") {
        std::fs::write(
            &path,
            format!("{summary}\n{}", audit.differences.join("\n\n")),
        )
        .unwrap();
        std::fs::write(format!("{path}.goals"), audit.goals.join("\n\n")).unwrap();
    }
    eprintln!("{summary}");
    assert!(audit.accepted > 100 && audit.orders > audit.accepted);
    assert!(
        audit.differences.is_empty(),
        "{summary}\n{}",
        audit
            .differences
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    assert!(
        audit.goals.is_empty(),
        "{summary}\n{}",
        audit
            .goals
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

#[test]
fn different_keys_restoring_one_cell_share_the_declared_write_order() {
    use pbps_model::operation::{Requirement, Resource};
    for same_value in [false, true] {
        let mut f = base(2);
        f.fx[0].symbolic.push(restoration(0, 1, 0));
        let mut r = restoration(2, 1, 1);
        if let Clause::Restore(r) = &mut r {
            r.property = Property::Address;
            r.value = Fact::Known(value(if same_value { "7" } else { "9" }));
        }
        f.fx[1].symbolic.push(r);
        for order in permutations(2) {
            let ordered = f.reordered(&order);
            assert!(ordered.edges().contains(&(f.ops[0].id(), f.ops[1].id())));
            graph_and_replay_agree(&ordered, &ordered.graph(), &[]);
            let mut audit = Determinism::default();
            audit.check("different-restoration-keys".into(), &ordered);
            assert!(audit.differences.is_empty() && audit.goals.is_empty());
        }
        let reversed = f.reordered(&[1, 0]);
        let replay = project(&reversed.ops, &reversed.fx, &reversed.base).unwrap();
        // Both sequences replay, but only the declared last write is the goal.
        assert!(!declared_goals(&f, replay.state(At::End).unwrap()).is_empty());
        let input = ObjectKey::Module("app.restored".parse().unwrap());
        f.fx[1]
            .creates
            .push(Resource::Object(input.clone(), Incarnation::Final));
        f.fx[0].requires.push(Requirement::Exists(input));
        assert!(f.refusal().contains("wait on one another"));
        graph_and_replay_agree(&f, &f.graph(), &[(1, 0)]);
    }
    // A different physical property remains independent despite sharing a Key.
    let mut f = base(2);
    f.fx[0].symbolic.push(restoration(0, 1, 0));
    let mut r = restoration(0, 1, 0);
    if let Clause::Restore(r) = &mut r {
        r.property = Property::Rows;
    }
    f.fx[1].symbolic.push(r);
    assert!(f.edges().is_empty());
    let mut audit = Determinism::default();
    audit.check("independent-cells".into(), &f);
    assert!(audit.differences.is_empty() && audit.goals.is_empty());
}

#[test]
fn equal_transfer_values_do_not_choose_between_distinct_endpoint_events() {
    let mut f = base(2);
    let mut w = write(0, 0);
    if let Clause::Recurse(w) = &mut w {
        w.policy = WritePolicy::PreserveOwn;
    }
    f.fx[0].symbolic.push(w);
    f.fx[1]
        .symbolic
        .push(movement(0, 1, MoveKind::Attach, vec![transfer(0, 0, 0)]));
    let mut endpoints = Vec::new();
    for order in permutations(2) {
        let ordered = f.reordered(&order);
        assert!(
            ordered
                .refusal()
                .contains("different physical endpoint identities")
        );
        let p = project(&ordered.ops, &ordered.fx, &ordered.base).unwrap();
        endpoints.push(p.state(At::End).unwrap().clone());
    }
    let cell = (tables()[1].clone(), Property::Address);
    assert_eq!(
        endpoints[0].cells[&cell].value,
        endpoints[1].cells[&cell].value
    );
    assert_eq!(
        endpoints[0].cells[&cell].origin,
        endpoints[1].cells[&cell].origin
    );
    assert_ne!(
        endpoints[0].cells[&cell].version,
        endpoints[1].cells[&cell].version
    );
    // Parent-only writes cannot change the transferred cell's event identity.
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.scope = Scope::ParentOnly;
    }
    sound(&f);
    let mut audit = Determinism::default();
    audit.check("parent-only".into(), &f);
    assert!(audit.differences.is_empty() && audit.goals.is_empty());
    // A declared shape that admits only one cut needs no ambiguity refusal.
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.scope = Scope::MembersAtOperation;
    }
    if let Clause::Move(m) = &mut f.fx[1].symbolic[0] {
        let Boundary::All(ts) = &mut m.boundary else {
            unreachable!()
        };
        ts[0].check = ShapeCheck::SameDefinition;
    }
    sound(&f);
    assert!(f.edges().contains(&(f.ops[1].id(), f.ops[0].id())));
    let mut audit = Determinism::default();
    audit.check("single-cut".into(), &f);
    assert!(audit.differences.is_empty() && audit.goals.is_empty());
}
