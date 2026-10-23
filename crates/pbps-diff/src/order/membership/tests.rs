//! Synthetic operation/effect facts are deliberately non-executable. The
//! independent tiny interpreter and enumeration are test-only (DEC-2296.1).
use super::*;
use crate::order::{Baseline, Checks, Schedule, Violation, graph, verify};
use pbps_model::operation::{
    BoundInput, ColumnKey, Incarnation, ModuleEdits, ObjectKey, Operations, OwnerError, Owners,
    Spelling,
};
use pbps_model::plan_names::At;
use pbps_model::{Change, ChangeSet, ModuleDeps, PlannedChange};

struct NoLookup;
impl Owners for NoLookup {
    fn table(
        &self,
        _: &pbps_model::TableName,
        spelling: Spelling,
    ) -> Result<pbps_model::Uid, OwnerError> {
        Err(OwnerError::Unmanaged {
            name: "unused".into(),
            spelling,
        })
    }
    fn column(
        &self,
        _: &pbps_model::ColumnRef,
        spelling: Spelling,
    ) -> Result<pbps_model::Uid, OwnerError> {
        Err(OwnerError::Unmanaged {
            name: "unused".into(),
            spelling,
        })
    }
    fn role(
        &self,
        _: &pbps_model::RoleName,
        spelling: Spelling,
    ) -> Result<pbps_model::Uid, OwnerError> {
        Err(OwnerError::Unmanaged {
            name: "unused".into(),
            spelling,
        })
    }
}
fn recorded(uid: &str) -> Recorded {
    // Read identity from a real operation payload; do not add a constructor
    // or an executable fixture route for this contract.
    let change = Change::CreateTable {
        uid: uid.parse().unwrap(),
        name: "app.p".parse().unwrap(),
        table: Box::default(),
        beside_default: None,
        moves_at_most: None,
    };
    let plan = vec![PlannedChange::new(change)];
    let ops = Operations::of_plan(&plan, ModuleEdits::InPlace);
    let ObjectKey::Table(recorded) = ops.as_slice()[0].key(&NoLookup).unwrap() else {
        panic!()
    };
    recorded
}
fn column(uid: &str) -> ColumnKey {
    let changes = vec![PlannedChange::new(Change::AddColumn {
        uid: uid.parse().unwrap(),
        table: "app.p".parse().unwrap(),
        name: "d".into(),
        column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
    })];
    let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
    let ObjectKey::Column(key) = ops.as_slice()[0].key(&NoLookup).unwrap() else {
        panic!()
    };
    key
}
fn value(text: &str) -> Value {
    Value {
        datum: Datum::Definition(text.into()),
        bindings: BTreeSet::new(),
        ownership: Ownership::Own,
    }
}
fn inherited(text: &str) -> Value {
    Value {
        ownership: Ownership::Inherited,
        ..value(text)
    }
}
fn property(table: &Recorded, attr: Attribute) -> Property {
    Property::Column(
        ColumnKey::Partition {
            table: table.clone(),
            column: "d".into(),
        },
        attr,
    )
}
fn cell_value(table: &Recorded, prop: &Property, v: Value) -> Cell {
    let object = match prop {
        Property::Column(column, _) => ObjectKey::Column(column.clone()),
        Property::Part(part) => ObjectKey::Part {
            table: table.clone(),
            part: part.clone(),
        },
        Property::Address | Property::Rows => ObjectKey::Table(table.clone()),
    };
    Cell {
        value: Fact::Known(v),
        version: Version::Base,
        origin: Some(PhysicalOrigin {
            table: table.clone(),
            object,
            birth: Version::Base,
        }),
    }
}
#[derive(Clone)]
struct Fixture {
    ops: Vec<Operation>,
    fx: Vec<Effects>,
    base: State,
    p: Recorded,
    a: Recorded,
    parent: Property,
    local: Property,
    member: Property,
}
impl Fixture {
    fn new(n: usize) -> Self {
        let plan: Vec<_> = (0..n)
            .map(|i| {
                PlannedChange::new(Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: format!("step{i}").into(),
                })
            })
            .collect();
        let ops = Operations::of_plan(&plan, ModuleEdits::InPlace)
            .as_slice()
            .to_vec();
        let p = recorded("t_aaaaaa");
        let a = recorded("t_bbbbbb");
        let parent = Property::Column(column("c_aaaaaa"), Attribute::Default);
        let local = Property::Column(column("c_bbbbbb"), Attribute::Default);
        let member = property(&a, Attribute::Default);
        let mut base = State::default();
        base.tables.extend([p.clone(), a.clone()]);
        base.cells.insert(
            (p.clone(), parent.clone()),
            cell_value(&p, &parent, value("1")),
        );
        base.cells.insert(
            (a.clone(), local.clone()),
            cell_value(&a, &local, value("9")),
        );
        Self {
            ops,
            fx: vec![Effects::default(); n],
            base,
            p,
            a,
            parent,
            local,
            member,
        }
    }
    fn write(&mut self, at: usize, input: Version, from: Value, to: Value) {
        let key = self.key();
        self.fx[at].symbolic.push(Clause::Recurse(RecursiveWrite {
            lifetime: Lifetime::Retain,
            key,
            input,
            from: Fact::Known(from),
            to: Fact::Known(to),
            scope: Scope::MembersAtOperation,
            policy: WritePolicy::Overwrite,
        }));
    }
    fn key(&self) -> Key {
        Key {
            parent: self.p.clone(),
            property: self.parent.clone(),
        }
    }
    fn movement(
        &mut self,
        at: usize,
        kind: MoveKind,
        check: ShapeCheck,
        policy: TransferPolicy,
        goal: Value,
    ) {
        let (from, to) = if kind == MoveKind::Attach {
            (self.local.clone(), self.member.clone())
        } else {
            (self.member.clone(), self.local.clone())
        };
        self.fx[at].symbolic.push(Clause::Move(s::Move {
            parent: self.p.clone(),
            child: self.a.clone(),
            kind,
            consumes: (kind != MoveKind::Attach).then_some(Version::Base),
            boundary: Boundary::All(vec![Transfer {
                parent: self.parent.clone(),
                from,
                to,
                check,
                policy,
                goal: Fact::Known(goal),
            }]),
        }));
    }
    fn attached_baseline(&mut self, v: Value) {
        let mut c = self
            .base
            .cells
            .remove(&(self.a.clone(), self.local.clone()))
            .unwrap();
        c.value = Fact::Known(v);
        self.base
            .cells
            .insert((self.a.clone(), self.member.clone()), c);
        self.base.members.insert(
            self.a.clone(),
            Membership {
                parent: self.p.clone(),
                incarnation: Version::Base,
                aliases: BTreeMap::from([(self.parent.clone(), self.member.clone())]),
            },
        );
    }
    fn restore(&mut self, at: usize, v: Value, member: bool) {
        let key = self.key();
        self.fx[at].symbolic.push(Clause::Restore(Restoration {
            key,
            child: self.a.clone(),
            property: if member {
                self.member.clone()
            } else {
                self.local.clone()
            },
            value: Fact::Known(v),
            member_of: member.then(|| self.p.clone()),
        }));
    }
    fn graph(&self) -> Graph {
        graph(
            &self.ops,
            &self.fx,
            &Baseline::empty().with_symbolic(self.base.clone()),
            &ModuleDeps::new(),
            Checks::ALL,
        )
    }
    fn plan(&self) -> ChangeSet {
        ChangeSet {
            changes: self.ops.iter().map(|o| o.planned().clone()).collect(),
        }
    }
    fn reordered(&self, order: &[usize]) -> Self {
        Self {
            ops: order.iter().map(|&i| self.ops[i].clone()).collect(),
            fx: order.iter().map(|&i| self.fx[i].clone()).collect(),
            ..self.clone()
        }
    }
    fn admits(&self) -> bool {
        Schedule::of_graph(&self.graph(), &self.ops)
            .sequence()
            .is_ok()
    }
    fn refusal(&self) -> String {
        Schedule::of_graph(&self.graph(), &self.ops)
            .sequence()
            .unwrap_err()
            .describe(&self.plan())
    }
    fn edges(&self) -> BTreeSet<(OperationId, OperationId)> {
        self.graph()
            .edges
            .iter()
            .map(|e| (self.ops[e.before].id(), self.ops[e.after].id()))
            .collect()
    }
}
fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn visit(n: usize, used: &mut Vec<bool>, p: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if p.len() == n {
            out.push(p.clone());
            return;
        }
        for i in 0..n {
            if !used[i] {
                used[i] = true;
                p.push(i);
                visit(n, used, p, out);
                p.pop();
                used[i] = false;
            }
        }
    }
    let mut out = Vec::new();
    visit(n, &mut vec![false; n], &mut Vec::new(), &mut out);
    out
}
fn permits(graph: &Graph, order: &[usize]) -> bool {
    let mut position = vec![0; order.len()];
    for (at, &i) in order.iter().enumerate() {
        position[i] = at;
    }
    graph.symbolic_unmet.is_empty()
        && graph
            .edges
            .iter()
            .all(|e| position[e.before] < position[e.after])
}
fn sound(f: &Fixture) {
    let g = f.graph();
    assert!(g.symbolic_unmet.is_empty(), "{:?}", g.symbolic_unmet);
    for order in permutations(f.ops.len()) {
        if permits(&g, &order) {
            let ordered = f.reordered(&order);
            project(&ordered.ops, &ordered.fx, &ordered.base)
                .unwrap_or_else(|e| panic!("{order:?}: {e}"));
        }
    }
    let done = Schedule::of_graph(&g, &f.ops).settle_raw(f.plan()).unwrap();
    done.proof.validate(&done.changes).unwrap();
    let mut positions = vec![0; f.ops.len()];
    for (at, &before) in done.sequence.iter().enumerate() {
        positions[before] = at;
    }
    let expected: BTreeSet<_> = g
        .edges
        .iter()
        .map(|edge| {
            (
                positions[edge.before],
                positions[edge.after],
                crate::order::schedule::reason(edge, &f.ops),
            )
        })
        .collect();
    let actual: BTreeSet<_> = done
        .proof
        .edges()
        .iter()
        .map(|edge| (edge.before, edge.after, edge.reason))
        .collect();
    assert_eq!(actual, expected);
    for edge in &g.edges {
        if matches!(edge.because, Because::Restores(_)) {
            assert_eq!(
                crate::order::schedule::reason(edge, &f.ops),
                pbps_model::resolver::OrderReason::Restoration
            );
        }
    }
    let ordered = f.reordered(&done.sequence);
    assert!(
        verify(
            &ordered.ops,
            &ordered.fx,
            &Baseline::empty().with_symbolic(f.base.clone()),
            &ModuleDeps::new()
        )
        .is_empty()
    );
}

#[test]
fn own_default_restoration_allows_both_orders_but_records_different_actual_targets() {
    let mut f = Fixture::new(3);
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    f.restore(2, value("9"), true);
    assert_eq!(
        f.edges(),
        BTreeSet::from([
            (f.ops[0].id(), f.ops[2].id()),
            (f.ops[1].id(), f.ops[2].id())
        ])
    );
    for input in permutations(3) {
        let input = f.reordered(&input);
        assert_eq!(input.edges(), f.edges());
        sound(&input);
    }
    // Independent integer/default interpreter: no cut compiler, projection,
    // WritePolicy or transfer helper is used to decide the oracle outcome.
    for order in permutations(3) {
        let (mut parent, mut child, mut member, mut valid) = (1, 9, false, true);
        let mut reached = false;
        for &step in &order {
            match step {
                0 => {
                    parent = 7;
                    if member {
                        child = parent;
                        reached = true;
                    }
                }
                1 => {
                    member = true;
                }
                2 => {
                    if !member {
                        valid = false;
                    }
                    child = 9;
                }
                _ => unreachable!(),
            }
        }
        valid &= parent == 7 && child == 9;
        assert_eq!(permits(&f.graph(), &order), valid, "{order:?}");
        if valid {
            let ordered = f.reordered(&order);
            let projection = project(&ordered.ops, &ordered.fx, &ordered.base).unwrap();
            assert_eq!(
                projection
                    .writes
                    .contains(&(f.ops[0].id(), f.a.clone(), f.member.clone())),
                reached
            );
            assert_eq!(
                projection.state(At::End).unwrap().cells[&(f.a.clone(), f.member.clone())].value,
                Fact::Known(value("9"))
            );
        }
    }
}

#[test]
fn abstract_property_keys_share_the_four_cut_outcomes_without_a_rank() {
    // This is the engine-neutral cut algebra across key families, not a
    // PostgreSQL policy table. Default/NOT NULL policies have separate pins.
    let mut properties: Vec<_> = [
        Attribute::Presence,
        Attribute::Name,
        Attribute::Type,
        Attribute::Collation,
        Attribute::Default,
        Attribute::Numbering,
        Attribute::NotNull,
    ]
    .into_iter()
    .map(|a| Property::Column(column("c_aaaaaa"), a))
    .collect();
    properties.extend([Property::Address, Property::Rows]);
    properties.extend(
        [
            pbps_model::operation::PartKey::Index("i".into()),
            pbps_model::operation::PartKey::PrimaryKey,
            pbps_model::operation::PartKey::Unique("u".into()),
            pbps_model::operation::PartKey::Check("c".into()),
            pbps_model::operation::PartKey::ForeignKey("fk".into()),
        ]
        .into_iter()
        .map(Property::Part),
    );
    for prop in properties {
        for kind in [MoveKind::Attach, MoveKind::Detach, MoveKind::Drop] {
            for final_shape in [false, true] {
                let mut f = Fixture::new(2);
                let old = f.parent.clone();
                let parent_cell = f.base.cells.remove(&(f.p.clone(), old)).unwrap();
                f.parent = prop.clone();
                f.base
                    .cells
                    .insert((f.p.clone(), f.parent.clone()), parent_cell);
                let child_text = if final_shape { "7" } else { "1" };
                if kind == MoveKind::Attach {
                    f.base
                        .cells
                        .get_mut(&(f.a.clone(), f.local.clone()))
                        .unwrap()
                        .value = Fact::Known(value(child_text));
                } else {
                    f.attached_baseline(value("1"));
                }
                f.write(0, Version::Base, value("1"), value("7"));
                let goal = if kind == MoveKind::Attach {
                    inherited("7")
                } else {
                    value(child_text)
                };
                f.movement(
                    1,
                    kind,
                    ShapeCheck::SameDefinition,
                    TransferPolicy::Inherit,
                    goal,
                );
                let expected = match kind {
                    MoveKind::Attach if final_shape => (0, 1),
                    MoveKind::Attach => (1, 0),
                    MoveKind::Detach if final_shape => (0, 1),
                    MoveKind::Detach => (1, 0),
                    MoveKind::Drop => {
                        assert!(f.edges().is_empty());
                        sound(&f);
                        continue;
                    }
                };
                assert_eq!(
                    f.edges(),
                    BTreeSet::from([(f.ops[expected.0].id(), f.ops[expected.1].id())]),
                    "{prop:?}/{kind:?}"
                );
                sound(&f);
                let reverse = f.reordered(&[1, 0]);
                assert_eq!(reverse.edges(), f.edges());
                sound(&reverse);
            }
        }
    }
    // Neither shape: a third definition is a named obligation, never Sink's guess.
    let mut f = Fixture::new(2);
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("7"),
    );
    assert!(f.refusal().contains("no boundary"));
    assert!(f.refusal().contains(REMEDY));
}

#[test]
fn same_text_with_new_binding_uses_the_explicit_release_restore_chain() {
    let mut f = Fixture::new(3);
    let mut old = value("f()");
    let mut new = old.clone();
    let key = ObjectKey::Module(pbps_model::ModuleId::Named("app.f".parse().unwrap()));
    old.bindings.insert(BoundInput {
        input: key.clone(),
        incarnation: Incarnation::Base,
    });
    new.bindings.insert(BoundInput {
        input: key,
        incarnation: Incarnation::Final,
    });
    let absent = Value {
        datum: Datum::Absent,
        ..value("")
    };
    f.base
        .cells
        .get_mut(&(f.p.clone(), f.parent.clone()))
        .unwrap()
        .value = Fact::Known(old.clone());
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(new.clone());
    f.write(0, Version::Base, old, absent.clone());
    f.write(1, Version::After(f.ops[0].id()), absent, new.clone());
    new.ownership = Ownership::Inherited;
    f.movement(
        2,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        new,
    );
    assert_eq!(
        f.edges(),
        BTreeSet::from([
            (f.ops[0].id(), f.ops[1].id()),
            (f.ops[1].id(), f.ops[2].id())
        ])
    );
    sound(&f);
    sound(&f.reordered(&[2, 1, 0]));
}

#[test]
fn equal_endpoint_values_cannot_hide_an_invalid_middle_cut() {
    let mut f = Fixture::new(3);
    f.attached_baseline(value("1"));
    let absent = Value {
        datum: Datum::Absent,
        ..value("")
    };
    f.write(0, Version::Base, value("1"), absent.clone());
    f.write(1, Version::After(f.ops[0].id()), absent, value("1"));
    f.movement(
        2,
        MoveKind::Detach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("1"),
    );
    assert!(f.refusal().contains("non-convex"));
    assert!(f.refusal().contains(REMEDY));
    let middle = f.reordered(&[0, 2, 1]);
    assert!(
        project(&middle.ops, &middle.fx, &middle.base)
            .unwrap_err()
            .detail
            .contains("final property")
    );
    for order in [[2, 0, 1], [0, 1, 2]] {
        let valid = f.reordered(&order);
        assert!(project(&valid.ops, &valid.fx, &valid.base).is_ok());
    }
}

#[test]
fn forks_missing_facts_aliases_and_cross_property_disjunctions_refuse_with_a_remedy() {
    let mut fork = Fixture::new(2);
    fork.write(0, Version::Base, value("1"), value("7"));
    fork.write(1, Version::Base, value("1"), value("8"));
    assert!(fork.refusal().contains("fork"));
    let mut mismatch = Fixture::new(1);
    mismatch.write(0, Version::Base, value("wrong"), value("7"));
    assert!(mismatch.refusal().contains("input definition"));
    assert!(project(&mismatch.ops, &mismatch.fx, &mismatch.base).is_err());
    let mut missing = fork.clone();
    if let Clause::Recurse(w) = &mut missing.fx[0].symbolic[0] {
        w.input = Version::After(missing.ops[0].id());
    }
    assert!(!missing.admits());
    let mut unread = Fixture::new(1);
    unread.fx[0].symbolic.push(Clause::Require(Fact::Unread));
    assert!(unread.refusal().contains("not read"));
    let mut choice = Fixture::new(1);
    choice.movement(
        0,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    if let Clause::Move(m) = &mut choice.fx[0].symbolic[0] {
        let Boundary::All(first) = &m.boundary else {
            unreachable!()
        };
        let mut base_left = first[0].clone();
        base_left.goal = Fact::Known(value("base-default"));
        let mut base_right = base_left.clone();
        base_right.parent = Property::Address;
        base_right.goal = Fact::Known(value("base-address"));
        let mut final_left = base_left.clone();
        final_left.goal = Fact::Known(value("final-default"));
        let mut final_right = base_right.clone();
        final_right.goal = Fact::Known(value("final-address"));
        m.boundary = Boundary::Alternatives(vec![
            vec![base_left, base_right],
            vec![final_left, final_right],
        ]);
    }
    assert!(choice.refusal().contains("non-conjunctive"));
    let mut unknown = Fixture::new(1);
    unknown.movement(
        0,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    unknown.base.tables.remove(&unknown.a);
    assert!(unknown.refusal().contains("unknown UID"));
    let mut alias = Fixture::new(1);
    alias.attached_baseline(value("9"));
    alias.movement(
        0,
        MoveKind::Detach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    alias
        .base
        .members
        .get_mut(&alias.a)
        .unwrap()
        .aliases
        .clear();
    assert!(alias.refusal().contains("physical alias"));
    assert!(project(&alias.ops, &alias.fx, &alias.base).is_err());
    for f in [&fork, &missing, &unread, &choice, &unknown, &alias] {
        assert!(f.refusal().contains(REMEDY));
    }
}

#[test]
fn transferred_properties_keep_physical_birth_and_drop_retires_the_uid() {
    for kind in [MoveKind::Attach, MoveKind::Detach, MoveKind::Drop] {
        let mut f = Fixture::new(1);
        if kind != MoveKind::Attach {
            f.attached_baseline(value("9"));
        }
        let source = if kind == MoveKind::Attach {
            f.local.clone()
        } else {
            f.member.clone()
        };
        let origin = f.base.cells[&(f.a.clone(), source)].origin.clone();
        f.movement(
            0,
            kind,
            ShapeCheck::None,
            TransferPolicy::Preserve,
            value("9"),
        );
        let view = project(&f.ops, &f.fx, &f.base).unwrap();
        let end = view.state(At::End).unwrap();
        if kind == MoveKind::Drop {
            assert!(!end.tables.contains(&f.a));
            assert!(end.cells.keys().all(|(uid, _)| *uid != f.a));
        } else {
            let target = if kind == MoveKind::Attach {
                &f.member
            } else {
                &f.local
            };
            assert_eq!(end.cells[&(f.a.clone(), target.clone())].origin, origin);
        }
        assert!(view.state(At::After(4)).is_none());
    }
}

#[test]
fn final_replay_rejects_broken_order_and_pending_never_means_absent() {
    let mut f = Fixture::new(2);
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("7"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("7"),
    );
    let pending = Pending::new(&f.ops, &f.fx, &f.base);
    assert!(matches!(pending.lookup(&f.a, &f.local), Lookup::Pending));
    assert!(matches!(
        pending.lookup(&recorded("t_cccccc"), &f.local),
        Lookup::Unavailable(_)
    ));
    let reverse = f.reordered(&[1, 0]);
    let violations = verify(
        &reverse.ops,
        &reverse.fx,
        &Baseline::empty().with_symbolic(f.base.clone()),
        &ModuleDeps::new(),
    );
    assert!(
        violations
            .iter()
            .any(|v| matches!(v, Violation::Symbolic(_)))
    );
    assert!(
        violations
            .iter()
            .any(|v| matches!(v, Violation::Edge { .. }))
    );
    let mut clean = Fixture::new(1);
    clean.fx.clear();
    clean.ops.clear();
    assert!(matches!(
        Pending::new(&clean.ops, &clean.fx, &clean.base).lookup(&clean.a, &clean.local),
        Lookup::Known(_)
    ));
}

#[test]
fn measured_capability_and_row_refusals_are_explicit_facts_not_invented_edges() {
    for sqlstate in [
        "42804", "42P21", "42P16", "55000", "42601", "22003", "23502", "23505", "23514", "23503",
        "2BP01",
    ] {
        let mut f = Fixture::new(1);
        f.fx[0].symbolic.push(Clause::Require(Fact::Unsupported {
            sqlstate: Some(sqlstate.into()),
            reason: "measured engine condition".into(),
        }));
        let why = f.refusal();
        assert!(
            why.contains(sqlstate) && why.contains(REMEDY) && why.contains("role step0"),
            "{why}"
        );
        assert!(f.graph().edges.is_empty());
        assert!(project(&f.ops, &f.fx, &f.base).is_err());
        f.fx[0].symbolic = [Clause::Require(Fact::Known(()))].into();
        assert!(f.admits());
    }
}

#[test]
fn conflicting_cuts_are_a_named_cycle_with_the_separate_plans_remedy() {
    let mut f = Fixture::new(2);
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("7"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("7"),
    );
    let mut g = f.graph();
    g.edges.insert(Edge {
        before: 1,
        after: 0,
        source: Source::Binding,
        because: Because::Declared,
    });
    let why = Schedule::of_graph(&g, &f.ops)
        .sequence()
        .unwrap_err()
        .describe(&f.plan());
    assert!(
        why.contains("wait on one another") && why.contains(REMEDY),
        "{why}"
    );
}

#[test]
fn independent_row_and_part_writes_do_not_create_phantom_recursion() {
    let mut f = Fixture::new(2);
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.scope = Scope::ParentOnly;
    }
    assert!(f.edges().is_empty());
    sound(&f);
    for order in permutations(2) {
        let ordered = f.reordered(&order);
        let view = project(&ordered.ops, &ordered.fx, &ordered.base).unwrap();
        assert!(
            view.writes
                .iter()
                .all(|(id, table, _)| *id != f.ops[0].id() || *table == f.p)
        );
    }
}

#[test]
fn measured_local_not_null_rules_are_engine_policies_not_a_scheduling_preference() {
    let nn = |yes, ownership| Value {
        datum: Datum::NotNull(yes),
        ownership,
        ..value("")
    };
    for (policy, needs_detach_first) in [
        (WritePolicy::Overwrite, true),
        (WritePolicy::PreserveOwn, false),
    ] {
        let mut f = Fixture::new(2);
        f.base
            .cells
            .get_mut(&(f.p.clone(), f.parent.clone()))
            .unwrap()
            .value = Fact::Known(nn(true, Ownership::Own));
        f.attached_baseline(nn(true, Ownership::LocalNotNull));
        f.write(
            0,
            Version::Base,
            nn(true, Ownership::Own),
            nn(false, Ownership::Own),
        );
        if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
            w.policy = policy;
        }
        f.movement(
            1,
            MoveKind::Detach,
            ShapeCheck::NotNullAtLeast,
            TransferPolicy::Preserve,
            nn(true, Ownership::Own),
        );
        assert_eq!(
            f.edges().contains(&(f.ops[1].id(), f.ops[0].id())),
            needs_detach_first
        );
        sound(&f);
    }
    let mut f = Fixture::new(2);
    f.base
        .cells
        .get_mut(&(f.p.clone(), f.parent.clone()))
        .unwrap()
        .value = Fact::Known(nn(true, Ownership::Own));
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(nn(false, Ownership::Own));
    f.write(
        0,
        Version::Base,
        nn(true, Ownership::Own),
        nn(false, Ownership::Own),
    );
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::NotNullAtLeast,
        TransferPolicy::Preserve,
        nn(false, Ownership::Own),
    );
    assert_eq!(f.edges(), BTreeSet::from([(f.ops[0].id(), f.ops[1].id())]));
    sound(&f);
}

#[test]
fn adoption_preserves_clone_lineage_and_existing_physical_origin() {
    let mut f = Fixture::new(2);
    let part = pbps_model::operation::PartKey::Index("parent_i".into());
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("1"));
    f.write(0, Version::Base, value("1"), value("1"));
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.policy = WritePolicy::PreserveOwnership;
    }
    let goal = Value {
        ownership: Ownership::Clone {
            parent: f.p.clone(),
            part: part.clone(),
        },
        ..value("1")
    };
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Adopt(part),
        goal,
    );
    assert!(f.edges().is_empty());
    sound(&f);
    for order in permutations(2) {
        let ordered = f.reordered(&order);
        let projected = project(&ordered.ops, &ordered.fx, &ordered.base).unwrap();
        assert_eq!(
            projected.state(At::End).unwrap().cells[&(f.a.clone(), f.member.clone())].origin,
            f.base.cells[&(f.a.clone(), f.local.clone())].origin
        );
    }
}

#[test]
fn several_children_receive_only_writes_inside_their_own_membership_interval() {
    let mut f = Fixture::new(5);
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    f.restore(2, value("9"), true);
    let b = recorded("t_cccccc");
    let before = property(&b, Attribute::Default);
    let after = property(&b, Attribute::Type);
    f.base.tables.insert(b.clone());
    f.base.cells.insert(
        (b.clone(), before.clone()),
        cell_value(&b, &before, value("11")),
    );
    let mut attach = f.fx[1].symbolic[0].clone();
    if let Clause::Move(m) = &mut attach {
        m.child = b.clone();
        if let Boundary::All(ts) = &mut m.boundary {
            ts[0].from = before;
            ts[0].to = after.clone();
            ts[0].goal = Fact::Known(value("11"));
        }
    }
    f.fx[3].symbolic.push(attach);
    let key = f.key();
    f.fx[4].symbolic.push(Clause::Restore(Restoration {
        key,
        child: b.clone(),
        property: after.clone(),
        value: Fact::Known(value("11")),
        member_of: Some(f.p.clone()),
    }));
    sound(&f);
    for order in permutations(5) {
        if permits(&f.graph(), &order) {
            let projected = f.reordered(&order);
            let view = project(&projected.ops, &projected.fx, &projected.base).unwrap();
            for (child, move_at, prop) in [(&f.a, 1, &f.member), (&b, 3, &after)] {
                let first = order.iter().position(|i| *i == move_at).unwrap();
                let c = order.iter().position(|i| *i == 0).unwrap();
                assert_eq!(
                    view.writes
                        .contains(&(f.ops[0].id(), child.clone(), prop.clone())),
                    first < c
                );
            }
        }
    }
}

#[test]
fn physical_sources_agrees_with_symbolic_scope_entry_and_exit() {
    use pbps_model::physical_sources::{Located, Owner, PhysicalSources, Source as PhysicalSource};
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{Column, IdsFile, PartitionBound, PartitionOf, Schema, Table, TableName};
    let parent: TableName = "app.p".parse().unwrap();
    let child: TableName = "app.a".parse().unwrap();
    let p_uid = "t_aaaaaa".parse().unwrap();
    let a_uid = "t_bbbbbb".parse().unwrap();
    let mut base = Schema::default();
    let table = Table {
        columns: [("d".into(), Column::new("int".parse().unwrap()))].into(),
        ..Default::default()
    };
    base.tables.insert(parent.clone(), table.clone());
    base.tables.insert(child.clone(), table.clone());
    let mut ids = IdsFile::default();
    ids.tables.insert(p_uid, parent.clone());
    ids.tables.insert(a_uid, child.clone());
    ids.columns
        .insert("c_aaaaaa".parse().unwrap(), parent.column("d"));
    ids.columns
        .insert("c_bbbbbb".parse().unwrap(), child.column("d"));
    let mut final_ids = ids.clone();
    final_ids.columns.remove(&"c_bbbbbb".parse().unwrap());
    let bound = PartitionBound::Default;
    let of = PartitionOf {
        parent: parent.clone(),
        bound: bound.clone(),
        columns: Default::default(),
    };
    let shape = Table {
        partition_of: Some(of),
        ..Default::default()
    };
    let change = PlannedChange::new(Change::AttachPartition {
        uid: "t_bbbbbb".parse().unwrap(),
        table: child.clone(),
        parent: parent.clone(),
        bound,
        key: vec![],
        shape: Box::new(shape),
    });
    let changes = vec![change];
    let names = PlanNames::of(&changes, &final_ids).unwrap();
    let view = PhysicalSources::of(names, &base, &ids).unwrap();
    assert_eq!(
        view.source(&child.column("d"), At::End),
        PhysicalSource::Stored(child.column("d"))
    );
    assert!(
        matches!(view.locate(&child.column("d"),At::End),Located::Present(found) if found.owner==Owner::Partition {parent:parent.clone()})
    );
    let mut detached_base = base.clone();
    detached_base.tables.insert(
        child.clone(),
        Table {
            partition_of: Some(PartitionOf {
                parent: parent.clone(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        },
    );
    let mut detached_ids = final_ids.clone();
    detached_ids
        .columns
        .insert("c_cccccc".parse().unwrap(), child.column("d"));
    let detach = vec![PlannedChange::new(Change::DetachPartition {
        uid: "t_bbbbbb".parse().unwrap(),
        table: child.clone(),
        parent: parent.clone(),
        names: vec![],
        shape: Box::new(table.clone()),
    })];
    let detached_names = PlanNames::of(&detach, &detached_ids).unwrap();
    let detached = PhysicalSources::of(detached_names, &detached_base, &final_ids).unwrap();
    assert_eq!(
        detached.source(&child.column("d"), At::End),
        PhysicalSource::Stored(child.column("d"))
    );
    assert!(
        matches!(detached.locate(&child.column("d"), At::End), Located::Present(found) if found.owner == Owner::Column("c_cccccc".parse().unwrap()))
    );
    let mut f = Fixture::new(1);
    f.movement(
        0,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    let projected = project(&f.ops, &f.fx, &f.base).unwrap();
    assert_eq!(projected.state(At::End).unwrap().members[&f.a].parent, f.p);
}

#[test]
fn creation_then_detach_keeps_the_new_birth_when_the_parent_retires_its_copy() {
    let mut f = Fixture::new(3);
    let absent = Value {
        datum: Datum::Absent,
        ..value("")
    };
    let p = f
        .base
        .cells
        .get_mut(&(f.p.clone(), f.parent.clone()))
        .unwrap();
    p.value = Fact::Known(absent.clone());
    p.origin = None;
    f.attached_baseline(absent.clone());
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.member.clone()))
        .unwrap()
        .origin = None;
    f.write(0, Version::Base, absent.clone(), value("new"));
    if let Clause::Recurse(w) = &mut f.fx[0].symbolic[0] {
        w.lifetime = Lifetime::Create;
    }
    f.movement(
        1,
        MoveKind::Detach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("new"),
    );
    f.write(2, Version::After(f.ops[0].id()), value("new"), absent);
    if let Clause::Recurse(w) = &mut f.fx[2].symbolic[0] {
        w.lifetime = Lifetime::Retire;
    }
    sound(&f);
    assert_eq!(
        f.edges(),
        BTreeSet::from([
            (f.ops[0].id(), f.ops[1].id()),
            (f.ops[1].id(), f.ops[2].id()),
            (f.ops[0].id(), f.ops[2].id())
        ])
    );
    let view = project(&f.ops, &f.fx, &f.base).unwrap();
    let state = view.state(At::End).unwrap();
    assert!(
        state.cells[&(f.p.clone(), f.parent.clone())]
            .origin
            .is_none()
    );
    assert_eq!(
        state.cells[&(f.a.clone(), f.local.clone())]
            .origin
            .as_ref()
            .unwrap()
            .birth,
        Version::After(f.ops[0].id())
    );
    let mut invalid = f.clone();
    if let Clause::Recurse(w) = &mut invalid.fx[0].symbolic[0] {
        w.lifetime = Lifetime::Retain;
    }
    assert!(!invalid.admits());
    assert!(project(&invalid.ops, &invalid.fx, &invalid.base).is_err());
}

#[test]
fn malformed_membership_epochs_and_unavailable_restoration_targets_are_named_obligations() {
    let mut f = Fixture::new(1);
    f.attached_baseline(value("9"));
    f.movement(
        0,
        MoveKind::Detach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    f.base.members.get_mut(&f.a).unwrap().incarnation = Version::After(f.ops[0].id());
    if let Clause::Move(m) = &mut f.fx[0].symbolic[0] {
        m.consumes = Some(Version::After(f.ops[0].id()));
    }
    assert!(f.refusal().contains("birth"));
    assert!(project(&f.ops, &f.fx, &f.base).is_err());
    f.base.members.get_mut(&f.a).unwrap().incarnation = Version::Base;
    if let Clause::Move(m) = &mut f.fx[0].symbolic[0] {
        m.consumes = Some(Version::Base);
    }
    f.base.members.get_mut(&f.a).unwrap().parent = f.a.clone();
    assert!(f.refusal().contains("cyclic"));
    assert!(project(&f.ops, &f.fx, &f.base).is_err());
    let mut r = Fixture::new(1);
    r.restore(0, value("9"), true);
    assert!(r.refusal().contains("unavailable"));
    assert!(r.refusal().contains(REMEDY));
    assert!(project(&r.ops, &r.fx, &r.base).is_err());
}

#[test]
fn pending_is_scoped_to_changed_properties_and_does_not_erase_unread_facts() {
    let mut f = Fixture::new(1);
    let unrelated = property(&f.a, Attribute::Collation);
    f.base.cells.insert(
        (f.a.clone(), unrelated.clone()),
        cell_value(&f.a, &unrelated, value("C")),
    );
    f.write(0, Version::Base, value("1"), value("7"));
    let pending = Pending::new(&f.ops, &f.fx, &f.base);
    assert!(matches!(pending.lookup(&f.a, &unrelated), Lookup::Known(_)));
    f.base
        .cells
        .get_mut(&(f.a.clone(), unrelated.clone()))
        .unwrap()
        .value = Fact::Unread;
    assert!(matches!(
        Pending::new(&f.ops, &f.fx, &f.base).lookup(&f.a, &unrelated),
        Lookup::Pending
    ));
    f.base
        .cells
        .get_mut(&(f.a.clone(), unrelated.clone()))
        .unwrap()
        .value = Fact::Unsupported {
        sqlstate: Some("55000".into()),
        reason: "unqualified identity".into(),
    };
    assert!(
        matches!(Pending::new(&f.ops,&f.fx,&f.base).lookup(&f.a,&unrelated),Lookup::Unavailable(why) if why.contains("55000"))
    );
}

#[test]
fn rename_and_default_restorations_share_one_graph_for_the_1896_shape() {
    // The contract half of #1896. Module lifetimes remain the existing
    // concrete binding effects' responsibility; no synthetic plan is emitted.
    let mut f = Fixture::new(5);
    let name = Property::Column(column("c_aaaaaa"), Attribute::Name);
    let local_name = Property::Column(column("c_bbbbbb"), Attribute::Name);
    let member_name = Property::Column(column("c_aaaaaa"), Attribute::Name);
    f.base.cells.insert(
        (f.p.clone(), name.clone()),
        cell_value(&f.p, &name, value("d")),
    );
    f.base.cells.insert(
        (f.a.clone(), local_name.clone()),
        cell_value(&f.a, &local_name, value("e")),
    );
    f.member = Property::Column(column("c_aaaaaa"), Attribute::Default);
    f.write(0, Version::Base, value("1"), value("7"));
    f.fx[1].symbolic.push(Clause::Recurse(RecursiveWrite {
        key: Key {
            parent: f.p.clone(),
            property: name.clone(),
        },
        input: Version::Base,
        from: Fact::Known(value("d")),
        to: Fact::Known(value("e")),
        scope: Scope::MembersAtOperation,
        policy: WritePolicy::Overwrite,
        lifetime: Lifetime::Retain,
    }));
    f.movement(
        2,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    if let Clause::Move(m) = &mut f.fx[2].symbolic[0]
        && let Boundary::All(ts) = &mut m.boundary
    {
        ts.push(Transfer {
            parent: name.clone(),
            from: local_name,
            to: member_name,
            check: ShapeCheck::SameDefinition,
            policy: TransferPolicy::Inherit,
            goal: Fact::Known(inherited("e")),
        });
    }
    f.restore(3, value("9"), true);
    let b = recorded("t_cccccc");
    let own = Property::Column(column("c_aaaaaa"), Attribute::Default);
    f.base.tables.insert(b.clone());
    f.base
        .cells
        .insert((b.clone(), own.clone()), cell_value(&b, &own, value("2")));
    f.base.cells.insert(
        (b.clone(), name.clone()),
        cell_value(&b, &name, inherited("d")),
    );
    f.base.members.insert(
        b.clone(),
        Membership {
            parent: f.p.clone(),
            incarnation: Version::Base,
            aliases: BTreeMap::from([(f.parent.clone(), own.clone()), (name.clone(), name)]),
        },
    );
    let key = f.key();
    f.fx[4].symbolic.push(Clause::Restore(Restoration {
        key,
        child: b,
        property: own,
        value: Fact::Known(value("2")),
        member_of: Some(f.p.clone()),
    }));
    assert!(f.edges().contains(&(f.ops[1].id(), f.ops[2].id())));
    sound(&f);
}

#[test]
fn recursion_never_touches_a_member_of_another_parent() {
    let mut f = Fixture::new(1);
    f.attached_baseline(value("9"));
    let other = recorded("t_cccccc");
    let other_property = Property::Column(column("c_cccccc"), Attribute::Default);
    f.base.tables.insert(other.clone());
    f.base.cells.insert(
        (other.clone(), other_property.clone()),
        cell_value(&other, &other_property, value("4")),
    );
    let member = f.base.members.get_mut(&f.a).unwrap();
    member.parent = other;
    member.aliases = BTreeMap::from([(other_property, f.member.clone())]);
    f.write(0, Version::Base, value("1"), value("7"));
    sound(&f);
    let view = project(&f.ops, &f.fx, &f.base).unwrap();
    assert!(view.writes.iter().all(|(_, table, _)| *table == f.p));
    assert_eq!(
        view.state(At::End).unwrap().cells[&(f.a.clone(), f.member.clone())].value,
        Fact::Known(value("9"))
    );
}

/// Compare every represented order, not only Sink's choice. Concrete replay
/// does not interpret ordinary graph edges, so those remain explicit premises.
fn graph_and_replay_agree(f: &Fixture, g: &Graph, ordinary: &[(usize, usize)]) {
    let mut any = false;
    for order in permutations(f.ops.len()) {
        let ordered = f.reordered(&order);
        let mut positions = vec![0; order.len()];
        for (at, &i) in order.iter().enumerate() {
            positions[i] = at;
        }
        let expected = ordinary.iter().all(|&(a, b)| positions[a] < positions[b])
            && project(&ordered.ops, &ordered.fx, &ordered.base).is_ok();
        assert_eq!(permits(g, &order), expected, "order {order:?}");
        any |= expected;
    }
    let settled = Schedule::of_graph(g, &f.ops).sequence();
    assert_eq!(settled.is_ok(), any, "{settled:?}");
    if let Ok(order) = settled {
        let ordered = f.reordered(&order);
        project(&ordered.ops, &ordered.fx, &ordered.base).unwrap();
    }
}

#[test]
fn intra_operation_boundaries_follow_clause_order_in_compiler_and_replay() {
    for kind in [MoveKind::Attach, MoveKind::Detach, MoveKind::Drop] {
        for write_first_required in [false, true] {
            for write_first in [false, true] {
                let mut f = Fixture::new(1);
                if kind == MoveKind::Attach {
                    f.base
                        .cells
                        .get_mut(&(f.a.clone(), f.local.clone()))
                        .unwrap()
                        .value = Fact::Known(value(if write_first_required { "7" } else { "1" }));
                } else {
                    f.attached_baseline(value("1"));
                }
                f.write(0, Version::Base, value("1"), value("7"));
                f.movement(
                    0,
                    kind,
                    ShapeCheck::SameDefinition,
                    TransferPolicy::Inherit,
                    if kind == MoveKind::Attach {
                        inherited("7")
                    } else {
                        value(if write_first_required { "7" } else { "1" })
                    },
                );
                if !write_first {
                    f.fx[0].symbolic.reverse();
                }
                let expected = kind == MoveKind::Drop || write_first == write_first_required;
                assert_eq!(
                    f.admits(),
                    expected,
                    "{kind:?}, {write_first_required}, {write_first}"
                );
                assert_eq!(project(&f.ops, &f.fx, &f.base).is_ok(), expected);
                if !expected {
                    assert!(f.refusal().contains("symbolic clause"));
                }
                graph_and_replay_agree(&f, &f.graph(), &[]);
            }
        }
    }
    // Restoration and membership dependencies use the same intra-operation
    // check: their edges cannot silently disappear either.
    let mut f = Fixture::new(1);
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        0,
        MoveKind::Attach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    f.restore(0, value("9"), true);
    let clauses = f.fx[0].symbolic.clone();
    for order in permutations(3) {
        f.fx[0].symbolic = order.iter().map(|&i| clauses[i].clone()).collect();
        let expected = order[2] == 2;
        assert_eq!(f.admits(), expected, "clauses {order:?}");
        graph_and_replay_agree(&f, &f.graph(), &[]);
    }
}

#[test]
fn a_cycle_uses_only_its_own_symbolic_edges_for_the_remedy() {
    let mut f = Fixture::new(4);
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("7"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("7"),
    );
    for because in [Because::Boundary(f.key()), Because::Restores(f.key())] {
        let mut g = f.graph();
        g.edges.clear();
        g.edges.insert(Edge {
            before: 0,
            after: 1,
            source: Source::Effect,
            because,
        });
        let ordinary = [
            Edge {
                before: 2,
                after: 3,
                source: Source::Effect,
                because: Because::Declared,
            },
            Edge {
                before: 3,
                after: 2,
                source: Source::Declared,
                because: Because::Declared,
            },
        ];
        g.edges.extend(ordinary.clone());
        let why = Schedule::of_graph(&g, &f.ops).sequence().unwrap_err();
        let crate::order::Refusal::Cycle {
            members, remedy, ..
        } = &why
        else {
            panic!("{why:?}")
        };
        assert_eq!(members, &[2, 3]);
        assert_eq!(remedy, &None);
        assert!(!why.describe(&f.plan()).contains(REMEDY));
        // Replay accepts the symbolic component; only the independent
        // ordinary cycle rules out every complete order.
        project(&f.ops, &f.fx, &f.base).unwrap();
        graph_and_replay_agree(&f, &g, &[(2, 3), (3, 2)]);
        g.edges.remove(&ordinary[1]);
        graph_and_replay_agree(&f, &g, &[(2, 3)]);
        // A reported cycle that actually contains the symbolic edge does
        // retain its remedy; the ordinary component is now acyclic.
        let mut reversed = ordinary[0].clone();
        reversed.before = 1;
        reversed.after = 0;
        g.edges.insert(reversed.clone());
        let why = Schedule::of_graph(&g, &f.ops).sequence().unwrap_err();
        let crate::order::Refusal::Cycle {
            members, remedy, ..
        } = &why
        else {
            panic!("{why:?}")
        };
        assert_eq!(members, &[0, 1]);
        assert_eq!(remedy.as_deref(), Some(REMEDY));
        graph_and_replay_agree(&f, &g, &[(2, 3), (1, 0)]);
    }
}

#[test]
fn a_restoration_requires_its_own_property_transfer_in_compiler_and_replay() {
    let mut f = Fixture::new(2);
    f.attached_baseline(value("9"));
    f.movement(
        1,
        MoveKind::Detach,
        ShapeCheck::None,
        TransferPolicy::Preserve,
        value("9"),
    );
    let other_parent = Property::Column(column("c_aaaaaa"), Attribute::Type);
    let other_child = property(&f.a, Attribute::Type);
    f.base.cells.insert(
        (f.p.clone(), other_parent.clone()),
        cell_value(&f.p, &other_parent, value("int")),
    );
    f.base.cells.insert(
        (f.a.clone(), other_child.clone()),
        cell_value(&f.a, &other_child, value("int")),
    );
    f.base
        .members
        .get_mut(&f.a)
        .unwrap()
        .aliases
        .insert(other_parent.clone(), other_child.clone());
    f.fx[0].symbolic.push(Clause::Restore(Restoration {
        key: Key {
            parent: f.p.clone(),
            property: other_parent.clone(),
        },
        child: f.a.clone(),
        property: other_child.clone(),
        value: Fact::Known(value("bigint")),
        member_of: None,
    }));
    // The detach carries only the default, not the restoration's type.
    // Neither input permutation may treat that unrelated transfer as proof
    // that the standalone type restoration is available.
    for order in permutations(2) {
        let shuffled = f.reordered(&order);
        assert!(
            shuffled
                .refusal()
                .contains("no matching ownership/property transfer")
        );
        assert!(project(&shuffled.ops, &shuffled.fx, &shuffled.base).is_err());
        graph_and_replay_agree(&shuffled, &shuffled.graph(), &[]);
    }
    // Supplying the actual transfer admits detach -> restoration, and only
    // that order; the property keeps its physical origin through the detach.
    if let Clause::Move(m) = &mut f.fx[1].symbolic[0] {
        let Boundary::All(ts) = &mut m.boundary else {
            unreachable!()
        };
        ts.push(Transfer {
            parent: other_parent,
            from: other_child.clone(),
            to: other_child.clone(),
            check: ShapeCheck::None,
            policy: TransferPolicy::Preserve,
            goal: Fact::Known(value("bigint")),
        });
    }
    for order in permutations(2) {
        let shuffled = f.reordered(&order);
        graph_and_replay_agree(&shuffled, &shuffled.graph(), &[]);
        assert!(shuffled.edges().contains(&(f.ops[1].id(), f.ops[0].id())));
        let done = shuffled.reordered(
            &Schedule::of_graph(&shuffled.graph(), &shuffled.ops)
                .sequence()
                .unwrap(),
        );
        let view = project(&done.ops, &done.fx, &done.base).unwrap();
        assert_eq!(
            view.state(At::End).unwrap().cells[&(f.a.clone(), other_child.clone())].origin,
            f.base.cells[&(f.a.clone(), other_child.clone())].origin
        );
    }
    // A matching property on a different parent does not qualify it either.
    let other = recorded("t_cccccc");
    f.base.tables.insert(other.clone());
    if let Clause::Restore(r) = &mut f.fx[0].symbolic[0] {
        r.key.parent = other;
    }
    assert!(
        f.refusal()
            .contains("no matching ownership/property transfer")
    );
    graph_and_replay_agree(&f, &f.graph(), &[]);
}

#[test]
fn projection_before_requires_an_existing_operation_even_on_an_empty_plan() {
    for n in [0, 1, 3] {
        let mut f = Fixture::new(n);
        if n > 0 {
            f.write(0, Version::Base, value("1"), value("7"));
        }
        let view = project(&f.ops, &f.fx, &f.base).unwrap();
        assert_eq!(view.state(At::Start), Some(&f.base));
        assert!(view.state(At::End).is_some());
        for i in 0..n {
            assert_eq!(
                view.state(At::Before(i)),
                view.state(if i == 0 { At::Start } else { At::After(i - 1) })
            );
        }
        for i in [n, n + 1, usize::MAX] {
            assert!(
                view.state(At::Before(i)).is_none(),
                "Before({i}) with n={n}"
            );
            assert!(view.state(At::After(i)).is_none(), "After({i}) with n={n}");
        }
        if n > 0 {
            assert_eq!(view.state(At::After(n - 1)), view.state(At::End));
        }
    }
}

#[test]
fn ordinary_unprovided_requirements_do_not_block_symbolic_boundaries() {
    let mut f = Fixture::new(2);
    f.base
        .cells
        .get_mut(&(f.a.clone(), f.local.clone()))
        .unwrap()
        .value = Fact::Known(value("7"));
    f.write(0, Version::Base, value("1"), value("7"));
    f.movement(
        1,
        MoveKind::Attach,
        ShapeCheck::SameDefinition,
        TransferPolicy::Inherit,
        inherited("7"),
    );
    f.fx[0]
        .requires
        .push(pbps_model::operation::Requirement::Exists(
            ObjectKey::Module("app.unmanaged_view".parse().unwrap()),
        ));
    let graph = f.graph();
    assert_eq!(graph.unmet.len(), 1);
    assert!(graph.symbolic_unmet.is_empty());
    assert_eq!(f.edges(), BTreeSet::from([(f.ops[0].id(), f.ops[1].id())]));
    graph_and_replay_agree(&f, &graph, &[]);
}

mod agreement;
