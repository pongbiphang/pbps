//! Check minimality separately from soundness: a safe chosen order does not
//! establish that an emitted edge is necessary in the surrounding graph.
use super::*;

#[derive(Debug)]
enum Justification {
    /// Removing this occurrence still leaves the same directed dependency.
    ImpliedByOtherEdges(Vec<usize>),
    /// Preserve the existing logical write/write policy for restorations of
    /// one physical cell, including a same-value restoration (DEC-2296.1).
    RestorationWriteOrder,
    /// Keep an intermediate restoration on the declared physical incarnation,
    /// rather than using its value equality to cross a birth/retirement.
    PhysicalIncarnationBoundary,
    /// A stationary member has no transfer goal in project. The declared
    /// restoration still promises its own final value (DEC-2296.1).
    StationaryRestorationGoal {
        child: Recorded,
        property: Property,
        value: Value,
    },
}
struct Observation {
    order: Vec<usize>,
    violations: Vec<usize>,
    end: Option<State>,
}
fn alternative_path(g: &Graph, edge: &Edge, n: usize) -> Option<Vec<usize>> {
    let mut pending = vec![vec![edge.before]];
    let mut seen = BTreeSet::new();
    while let Some(path) = pending.pop() {
        let at = *path.last().unwrap();
        if at == edge.after {
            return Some(path);
        }
        if !seen.insert(at) {
            continue;
        }
        for next in g.edges.iter().filter(|e| *e != edge && e.before == at) {
            let mut extended = path.clone();
            extended.push(next.after);
            pending.push(extended);
        }
        assert!(seen.len() <= n);
    }
    None
}
fn final_restoration(f: &Fixture, at: usize, clause: usize, target: &Restoration) -> bool {
    let current = (f.ops[at].id(), clause);
    f.fx.iter().enumerate().all(|(i, effect)| {
        effect.symbolic.iter().enumerate().all(|(j, c)| {
            !matches!(c, Clause::Restore(r) if r.child == target.child && r.property == target.property
                && (f.ops[i].id(), j) > current)
        })
    })
}
fn justification(
    f: &Fixture,
    g: &Graph,
    e: &Edge,
    violations: &[&Observation],
) -> Option<Justification> {
    if let Some(path) = alternative_path(g, e, f.ops.len()) {
        return Some(Justification::ImpliedByOtherEdges(path));
    }
    if let Because::Restores(key) = &e.because {
        for a in &f.fx[e.before].symbolic {
            for b in &f.fx[e.after].symbolic {
                if let (Clause::Restore(a), Clause::Restore(b)) = (a, b)
                    && a.key == *key
                    && b.key == *key
                    && a.child == b.child
                    && a.property == b.property
                    && f.ops[e.before].id() < f.ops[e.after].id()
                {
                    return Some(Justification::RestorationWriteOrder);
                }
            }
        }
    }
    if let Because::Restores(key) = &e.because {
        for a in &f.fx[e.before].symbolic {
            for (clause, b) in f.fx[e.after].symbolic.iter().enumerate() {
                if let (Clause::Recurse(w), Clause::Restore(r)) = (a, b)
                    && w.scope == Scope::MembersAtOperation
                    && w.key == *key
                    && r.key == *key
                    && let Some(member) = f.base.members.get(&r.child)
                    && member.parent == key.parent
                    && member.aliases.get(&key.property) == Some(&r.property)
                    && !moves_by_child(&f.fx).contains_key(&r.child)
                    && final_restoration(f, e.after, clause, r)
                    && let Ok(value) = r.value.known()
                    && violations.iter().any(|o| o.end.is_some())
                    && violations.iter().filter_map(|o| o.end.as_ref()).all(|end| {
                        end.cells
                            .get(&(r.child.clone(), r.property.clone()))
                            .map(|c| &c.value)
                            != Some(&r.value)
                    })
                {
                    return Some(Justification::StationaryRestorationGoal {
                        child: r.child.clone(),
                        property: r.property.clone(),
                        value: value.clone(),
                    });
                }
            }
        }
    }
    if let Because::Restores(key) = &e.because {
        for a in &f.fx[e.before].symbolic {
            for (clause, b) in f.fx[e.after].symbolic.iter().enumerate() {
                if let (Clause::Recurse(w), Clause::Restore(r)) = (a, b)
                    && w.key == *key
                    && r.key == *key
                    && w.scope == Scope::MembersAtOperation
                    && w.lifetime != Lifetime::Retain
                    && !final_restoration(f, e.after, clause, r)
                    && let Some(member) = f.base.members.get(&r.child)
                    && member.parent == key.parent
                    && member.aliases.get(&key.property) == Some(&r.property)
                    && !moves_by_child(&f.fx).contains_key(&r.child)
                {
                    return Some(Justification::PhysicalIncarnationBoundary);
                }
            }
        }
    }
    None
}
#[derive(Default)]
struct Necessity {
    cases: usize,
    edges: usize,
    isolated: usize,
    universal: usize,
    justified: Vec<String>,
    unnecessary: Vec<String>,
    emitted: Vec<String>,
}
impl Necessity {
    fn check(&mut self, label: String, f: &Fixture) {
        self.cases += 1;
        let g = f.graph();
        let edges: Vec<_> = g.edges.iter().collect();
        let orders: Vec<_> = permutations(f.ops.len())
            .into_iter()
            .map(|order| {
                let mut position = vec![0; order.len()];
                for (at, &i) in order.iter().enumerate() {
                    position[i] = at;
                }
                let violations: Vec<_> = edges
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| position[e.before] >= position[e.after])
                    .map(|(i, _)| i)
                    .collect();
                let ordered = f.reordered(&order);
                let end = project(&ordered.ops, &ordered.fx, &ordered.base)
                    .ok()
                    .map(|p| p.state(At::End).unwrap().clone());
                Observation {
                    order,
                    violations,
                    end,
                }
            })
            .collect();
        for (i, e) in edges
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.because, Because::Boundary(_) | Because::Restores(_)))
        {
            self.edges += 1;
            self.emitted.push(format!(
                "{label}: {} -> {} {:?}",
                e.before, e.after, e.because
            ));
            let isolated: Vec<_> = orders
                .iter()
                .filter(|o| o.violations.as_slice() == [i])
                .collect();
            if !isolated.is_empty() && isolated.iter().all(|o| o.end.is_none()) {
                self.isolated += 1;
                continue;
            }
            let violations: Vec<_> = orders
                .iter()
                .filter(|o| o.violations.contains(&i))
                .collect();
            if isolated.is_empty()
                && !violations.is_empty()
                && violations.iter().all(|o| o.end.is_none())
            {
                self.universal += 1;
                continue;
            }
            if let Some(why) = justification(f, &g, e, &violations) {
                // Read the path as a certificate rather than a diagnostic-only
                // exemption; every step must be another edge of this graph.
                if let Justification::ImpliedByOtherEdges(path) = &why {
                    assert!(path.windows(2).all(|p| {
                        g.edges
                            .iter()
                            .any(|other| other != *e && other.before == p[0] && other.after == p[1])
                    }));
                }
                if let Justification::StationaryRestorationGoal {
                    child,
                    property,
                    value,
                } = &why
                {
                    assert!(violations.iter().filter_map(|o| o.end.as_ref()).all(|end| {
                        end.cells
                            .get(&(child.clone(), property.clone()))
                            .map(|c| &c.value)
                            != Some(&Fact::Known(value.clone()))
                    }));
                }
                self.justified.push(format!(
                    "{label}: {} -> {} {:?}: {why:?}",
                    e.before, e.after, e.because
                ));
            } else {
                let witness = isolated
                    .iter()
                    .copied()
                    .find(|o| o.end.is_some())
                    .or_else(|| violations.iter().copied().find(|o| o.end.is_some()))
                    .unwrap();
                self.unnecessary.push(format!("{label}: {} -> {} {:?}, replayable violation {:?}\nbase={:?}\nfx={:?}\nedges={:?}", e.before, e.after, e.because, witness.order, f.base, f.fx, g.edges));
            }
        }
    }
}
#[test]
fn every_generated_symbolic_edge_has_a_replay_witness_or_a_named_reason() {
    let mut audit = Necessity::default();
    generated_contracts(|label, f| audit.check(label, f));
    let summary = format!(
        "cases={} edges={} isolated={} universal={} justified={} unnecessary={}",
        audit.cases,
        audit.edges,
        audit.isolated,
        audit.universal,
        audit.justified.len(),
        audit.unnecessary.len()
    );
    if let Ok(path) = std::env::var("PBPS_MEMBERSHIP_NECESSITY") {
        std::fs::write(
            &path,
            format!("{summary}\n{}", audit.unnecessary.join("\n\n")),
        )
        .unwrap();
        std::fs::write(format!("{path}.reasons"), audit.justified.join("\n")).unwrap();
        std::fs::write(format!("{path}.edges"), audit.emitted.join("\n")).unwrap();
    }
    eprintln!("{summary}");
    assert!(audit.edges > 100 && audit.isolated > 0 && audit.universal > 0);
    assert!(
        audit.unnecessary.is_empty(),
        "{}\n{}",
        summary,
        audit
            .unnecessary
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

fn attach_restore(scope: Scope, policy: WritePolicy) -> Fixture {
    let mut f = Fixture::new(3);
    f.write(0, Version::Base, value("1"), value("7"));
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.scope = scope;
        w.policy = policy;
    }
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    f.restore(2, value("9"), true);
    f
}
fn requires_restoration(f: &mut Fixture, restore: usize, write: usize) {
    use pbps_model::operation::{Requirement, Resource};
    let key = ObjectKey::Module("app.restored".parse().unwrap());
    f.fx[restore]
        .creates
        .push(Resource::Object(key.clone(), Incarnation::Final));
    f.fx[write].requires.push(Requirement::Exists(key));
}
fn agreement(f: &Fixture, ordinary: &[(usize, usize)]) {
    graph_and_replay_agree(f, &f.graph(), ordinary);
    let mut audit = Necessity::default();
    audit.check("directed".into(), f);
    assert!(audit.unnecessary.is_empty(), "{:?}", audit.unnecessary);
}
#[test]
fn parent_only_writes_do_not_block_an_earlier_child_restoration() {
    for scope in [Scope::ParentOnly, Scope::MembersAtOperation] {
        let mut f = attach_restore(scope, WritePolicy::Overwrite);
        requires_restoration(&mut f, 2, 0);
        agreement(&f, &[(2, 0)]);
        if scope == Scope::ParentOnly {
            let g = f.graph();
            let settled = Schedule::of_graph(&g, &f.ops).settle_raw(f.plan()).unwrap();
            assert_eq!(settled.sequence, [1, 2, 0]);
            settled.proof.validate(&settled.changes).unwrap();
            assert!(!g.edges.iter().any(|e| e.before == 0 && e.after == 2));
            let ordered = f.reordered(&settled.sequence);
            assert_eq!(
                project(&ordered.ops, &ordered.fx, &ordered.base)
                    .unwrap()
                    .state(At::End)
                    .unwrap()
                    .cells[&(f.a.clone(), f.member.clone())]
                    .value,
                Fact::Known(value("9"))
            );
        } else {
            assert!(f.refusal().contains("wait on one another"));
        }
    }
}
#[test]
fn unreachable_and_detached_members_do_not_wait_for_parent_writes() {
    let mut standalone = Fixture::new(2);
    standalone.write(0, Version::Base, value("1"), value("7"));
    standalone.restore(1, value("9"), false);
    requires_restoration(&mut standalone, 1, 0);
    agreement(&standalone, &[(1, 0)]);
    assert!(
        !standalone
            .graph()
            .edges
            .iter()
            .any(|e| e.before == 0 && e.after == 1)
    );
    let mut detached = Fixture::new(3);
    detached.attached_baseline(value("9"));
    detached.write(0, Version::Base, value("1"), value("7"));
    detached.movement(
        1,
        MoveKind::Detach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    detached.restore(2, value("9"), false);
    requires_restoration(&mut detached, 2, 0);
    agreement(&detached, &[(2, 0)]);
    assert_eq!(
        Schedule::of_graph(&detached.graph(), &detached.ops)
            .sequence()
            .unwrap(),
        [1, 2, 0]
    );
}
#[test]
fn only_the_last_restoration_waits_for_a_destructive_parent_suffix() {
    let mut f = Fixture::new(4);
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    f.restore(2, value("7"), true);
    f.restore(3, value("9"), true);
    requires_restoration(&mut f, 2, 0);
    agreement(&f, &[(2, 0)]);
    assert_eq!(
        Schedule::of_graph(&f.graph(), &f.ops).sequence().unwrap(),
        [1, 2, 0, 3]
    );
    assert!(!f.edges().contains(&(f.ops[0].id(), f.ops[2].id())));
    assert!(f.edges().contains(&(f.ops[0].id(), f.ops[3].id())));
}
#[test]
fn value_preserving_suffixes_can_follow_the_final_restoration() {
    let mut own = attach_restore(Scope::MembersAtOperation, WritePolicy::PreserveOwn);
    requires_restoration(&mut own, 2, 0);
    agreement(&own, &[(2, 0)]);
    let mut rebuilt = base(4);
    for at in 0..2 {
        let mut w = write(0, 0);
        if let Clause::Recurse(w) = &mut w {
            w.input = if at == 0 {
                Version::Base
            } else {
                Version::After(rebuilt.ops[0].id())
            };
            w.from = Fact::Known(if at == 0 { value("1") } else { value("7") });
            w.to = Fact::Known(if at == 0 { value("7") } else { value("1") });
        }
        rebuilt.fx[at].symbolic.push(w);
    }
    let mut t = transfer(0, 0, 0);
    t.goal = Fact::Known(inherited("1"));
    rebuilt.fx[2]
        .symbolic
        .push(movement(0, 1, MoveKind::Attach, vec![t]));
    let mut r = restoration(0, 1, 0);
    if let Clause::Restore(r) = &mut r {
        r.value = Fact::Known(inherited("1"));
        r.member_of = Some(tables()[0].clone());
    }
    rebuilt.fx[3].symbolic.push(r);
    requires_restoration(&mut rebuilt, 3, 0);
    agreement(&rebuilt, &[(3, 0)]);
    assert_eq!(
        Schedule::of_graph(&rebuilt.graph(), &rebuilt.ops)
            .sequence()
            .unwrap(),
        [2, 3, 0, 1]
    );
}
#[test]
fn restoration_edges_keep_physical_birth_preconditions() {
    let mut f = lifetime_contract(false, true);
    if let Clause::Restore(r) = &mut f.fx[1].symbolic[0] {
        r.value = Fact::Known(inherited("7"));
    }
    assert!(f.edges().contains(&(f.ops[0].id(), f.ops[1].id())));
    sound(&f);
    let reverse = f.reordered(&[1, 0]);
    assert!(
        project(&reverse.ops, &reverse.fx, &reverse.base)
            .unwrap_err()
            .detail
            .contains("physical creation/retirement")
    );
    let mut retirement = base(2);
    member(&mut retirement, 0, 1);
    let mut w = write(0, 0);
    let absent = Value {
        datum: Datum::Absent,
        ..inherited("1")
    };
    if let Clause::Recurse(w) = &mut w {
        w.lifetime = Lifetime::Retire;
        w.to = Fact::Known(absent.clone());
    }
    retirement.fx[0].symbolic.push(w);
    let mut r = restoration(0, 1, 0);
    if let Clause::Restore(r) = &mut r {
        r.value = Fact::Known(absent);
        r.member_of = Some(tables()[0].clone());
    }
    retirement.fx[1].symbolic.push(r);
    // Retirement accepts the restored absence and reaches the same final
    // value/origin in both orders; it needs no preference edge.
    assert!(retirement.graph().edges.is_empty());
    sound(&retirement);
}
#[test]
fn restoration_write_order_applies_only_to_the_same_physical_cell() {
    let mut f = base(2);
    member(&mut f, 0, 1);
    for at in 0..2 {
        let mut r = restoration(0, 1, 0);
        if let Clause::Restore(r) = &mut r {
            r.property = properties()[at].clone();
            r.member_of = Some(tables()[0].clone());
        }
        f.fx[at].symbolic.push(r);
    }
    assert!(f.graph().edges.is_empty());
    sound(&f);
    let e = Edge {
        before: 0,
        after: 1,
        source: Source::Effect,
        because: Because::Restores(Key {
            parent: tables()[0].clone(),
            property: Property::Address,
        }),
    };
    assert!(justification(&f, &f.graph(), &e, &[]).is_none());
    if let Clause::Restore(r) = &mut f.fx[1].symbolic[0] {
        r.property = Property::Address;
    }
    assert!(matches!(
        justification(&f, &f.graph(), &e, &[]),
        Some(Justification::RestorationWriteOrder)
    ));
    let reverse = Edge {
        before: 1,
        after: 0,
        ..e.clone()
    };
    assert!(justification(&f, &f.graph(), &reverse, &[]).is_none());
}
#[test]
fn a_stationary_restoration_reason_requires_a_changed_final_value() {
    let mut f = base(2);
    member(&mut f, 0, 1);
    f.fx[0].symbolic.push(write(0, 0));
    let mut r = restoration(0, 1, 0);
    if let Clause::Restore(r) = &mut r {
        r.value = Fact::Known(value("9"));
        r.member_of = Some(tables()[0].clone());
    }
    f.fx[1].symbolic.push(r);
    let mut audit = Necessity::default();
    audit.check("stationary-goal".into(), &f);
    assert!(audit.unnecessary.is_empty());
    assert_eq!(audit.justified.len(), 1);
    assert!(audit.justified[0].contains("StationaryRestorationGoal"));
    let forward = project(&f.ops, &f.fx, &f.base).unwrap();
    assert_eq!(
        forward.state(At::End).unwrap().cells[&(tables()[1].clone(), Property::Address)].value,
        Fact::Known(value("9"))
    );
    let reversed = f.reordered(&[1, 0]);
    let reverse = project(&reversed.ops, &reversed.fx, &reversed.base).unwrap();
    assert_eq!(
        reverse.state(At::End).unwrap().cells[&(tables()[1].clone(), Property::Address)].value,
        Fact::Known(inherited("7"))
    );
    // Equal values and parent-only writes cannot claim this exception.
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.scope = Scope::ParentOnly;
    }
    let e = Edge {
        before: 0,
        after: 1,
        source: Source::Effect,
        because: Because::Restores(Key {
            parent: tables()[0].clone(),
            property: Property::Address,
        }),
    };
    let ordered = f.reordered(&[1, 0]);
    let observations = [Observation {
        order: vec![1, 0],
        violations: vec![0],
        end: Some(
            project(&ordered.ops, &ordered.fx, &ordered.base)
                .unwrap()
                .state(At::End)
                .unwrap()
                .clone(),
        ),
    }];
    assert!(justification(&f, &f.graph(), &e, &[&observations[0]]).is_none());
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.scope = Scope::MembersAtOperation;
    }
    if let Clause::Restore(r) = &mut f.fx[1].symbolic[0] {
        r.value = Fact::Known(inherited("7"));
    }
    let ordered = f.reordered(&[1, 0]);
    let same_value = Observation {
        order: vec![1, 0],
        violations: vec![0],
        end: Some(
            project(&ordered.ops, &ordered.fx, &ordered.base)
                .unwrap()
                .state(At::End)
                .unwrap()
                .clone(),
        ),
    };
    assert!(justification(&f, &f.graph(), &e, &[&same_value]).is_none());
}

#[test]
fn an_intermediate_restoration_cannot_claim_the_final_value_certificate() {
    let mut f = Fixture::new(3);
    f.attached_baseline(value("9"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.restore(1, value("7"), true);
    f.restore(2, value("9"), true);
    let order = vec![1, 0, 2];
    let ordered = f.reordered(&order);
    let end = project(&ordered.ops, &ordered.fx, &ordered.base)
        .unwrap()
        .state(At::End)
        .unwrap()
        .clone();
    let observation = Observation {
        order,
        violations: vec![0],
        end: Some(end),
    };
    let e = Edge {
        before: 0,
        after: 1,
        source: Source::Effect,
        because: Because::Restores(f.key()),
    };
    assert!(justification(&f, &f.graph(), &e, &[&observation]).is_none());
    assert!(f.edges().contains(&(f.ops[0].id(), f.ops[2].id())));
    assert!(!f.edges().contains(&(f.ops[0].id(), f.ops[1].id())));
}

#[test]
fn intermediate_restorations_keep_the_physical_incarnation_boundary() {
    for rebuild in [false, true] {
        let original = lifetime_contract(rebuild, true);
        let mut f = Fixture::new(original.ops.len() + 1);
        f.base = original.base.clone();
        f.fx[..original.fx.len()].clone_from_slice(&original.fx);
        let last = original.fx.len() - 1;
        f.fx[last + 1] = f.fx[last].clone();
        if let Clause::Restore(r) = &mut f.fx[last].symbolic[0] {
            r.value = Fact::Known(value("9"));
        }
        sound(&f);
        let write = last - 1;
        assert!(f.edges().contains(&(f.ops[write].id(), f.ops[last].id())));
        let mut audit = Necessity::default();
        audit.check("physical-incarnation".into(), &f);
        assert!(audit.unnecessary.is_empty(), "{:?}", audit.unnecessary);
        if rebuild {
            // A restoration before retirement can replay, but it acts on the
            // old physical origin. The declared intermediate restoration is
            // kept on the recreated incarnation, just as before this fix.
            let before = f.reordered(&[last, 0, 1, last + 1]);
            let view = project(&before.ops, &before.fx, &before.base).unwrap();
            assert_eq!(
                view.state(At::Before(0)).unwrap().cells[&(tables()[1].clone(), Property::Address)]
                    .origin
                    .as_ref()
                    .unwrap()
                    .birth,
                Version::Base
            );
            let view = project(&f.ops, &f.fx, &f.base).unwrap();
            assert_eq!(
                view.state(At::Before(last)).unwrap().cells
                    [&(tables()[1].clone(), Property::Address)]
                    .origin
                    .as_ref()
                    .unwrap()
                    .birth,
                Version::After(f.ops[write].id())
            );
            assert!(
                audit
                    .justified
                    .iter()
                    .any(|r| r.contains("PhysicalIncarnationBoundary"))
            );
        }
    }
}

#[test]
fn a_nonconvex_restoration_domain_is_named_instead_of_choosing_an_incarnation() {
    let mut f = lifetime_contract(true, true);
    for fx in &mut f.fx[..2] {
        if let Clause::Recurse(w) = &mut fx.symbolic[0] {
            w.policy = WritePolicy::PreserveOwnership;
        }
    }
    assert!(f.refusal().contains("non-convex restoration boundary"));
    for (order, accepted) in [
        (vec![2, 0, 1], true),
        (vec![0, 2, 1], false),
        (vec![0, 1, 2], true),
    ] {
        let ordered = f.reordered(&order);
        assert_eq!(
            project(&ordered.ops, &ordered.fx, &ordered.base).is_ok(),
            accepted,
            "{order:?}"
        );
    }
    let mut audit = Audit::default();
    audit.check("nonconvex-restoration".into(), &f);
    assert!(audit.disagreements.is_empty());
}
