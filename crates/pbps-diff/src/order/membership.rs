//! Compile symbolic membership effects into the existing settlement, and
//! replay their concrete targets independently (DEC-2296.1).

use super::{Because, Edge, Graph, Source};
use pbps_model::operation::symbolic::{self as s, *};
use pbps_model::operation::{Effects, Operation, OperationId, Recorded};
use std::collections::{BTreeMap, BTreeSet};

pub const REMEDY: &str = "change the columns and the partitions in separate plans";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub operation: OperationId,
    pub detail: String,
}
impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}; {REMEDY}",
            self.operation,
            pbps_model::Text::from(self.detail.clone()).shown()
        )
    }
}
impl std::error::Error for Problem {}
fn problem(op: OperationId, detail: impl Into<String>) -> Problem {
    Problem {
        operation: op,
        detail: detail.into(),
    }
}
fn cell<'a>(state: &'a State, table: &Recorded, property: &Property) -> Result<&'a Cell, String> {
    if !state.tables.contains(table) {
        return Err(format!("unknown table UID {}", table.uid()));
    }
    state
        .cells
        .get(&(table.clone(), property.clone()))
        .ok_or_else(|| format!("missing property {property:?} of {}", table.uid()))
}
fn baseline_valid(state: &State) -> Result<(), String> {
    for (child, member) in &state.members {
        if !state.tables.contains(child) || !state.tables.contains(&member.parent) {
            return Err("membership baseline contains an unknown UID".into());
        }
        if child == &member.parent || state.members.contains_key(&member.parent) {
            return Err("nested or cyclic membership scope is not qualified".into());
        }
        if member.incarnation != Version::Base {
            return Err("baseline membership cannot have a current-plan birth".into());
        }
        let unique: BTreeSet<_> = member.aliases.values().collect();
        if unique.len() != member.aliases.len() {
            return Err("membership aliases collide".into());
        }
    }
    Ok(())
}
fn next_origin(
    previous: &Cell,
    value: &Value,
    lifetime: Lifetime,
    id: OperationId,
    table: &Recorded,
    property: &Property,
) -> Result<Option<PhysicalOrigin>, String> {
    match lifetime {
        Lifetime::Retain if previous.origin.is_some() => Ok(previous.origin.clone()),
        Lifetime::Retire if previous.origin.is_some() && value.datum == Datum::Absent => Ok(None),
        Lifetime::Create
            if previous.origin.is_none()
                && previous.value.known()?.datum == Datum::Absent
                && value.datum != Datum::Absent =>
        {
            let object = match property {
                Property::Column(key, _) => pbps_model::operation::ObjectKey::Column(key.clone()),
                Property::Part(part) => pbps_model::operation::ObjectKey::Part {
                    table: table.clone(),
                    part: part.clone(),
                },
                Property::Address | Property::Rows => {
                    pbps_model::operation::ObjectKey::Table(table.clone())
                }
            };
            Ok(Some(PhysicalOrigin {
                table: table.clone(),
                object,
                birth: Version::After(id),
            }))
        }
        Lifetime::Retain | Lifetime::Create | Lifetime::Retire => {
            Err("physical creation/retirement disagrees with the input lifetime".into())
        }
    }
}

fn transfers(m: &s::Move) -> Result<&[Transfer], String> {
    match &m.boundary {
        Boundary::All(ts) => Ok(ts),
        Boundary::Alternatives(_) => Err(format!(
            "non-conjunctive boundary for parent {} and child {}",
            m.parent.uid(),
            m.child.uid()
        )),
    }
}

#[derive(Clone, Copy)]
struct Position {
    at: usize,
    clause: usize,
    id: OperationId,
}
#[derive(Clone, Copy)]
struct Write<'a> {
    position: Position,
    value: &'a RecursiveWrite,
}
#[derive(Clone, Copy)]
struct Restore<'a> {
    position: Position,
    value: &'a Restoration,
}

fn moves_by_child(fx: &[Effects]) -> BTreeMap<Recorded, Vec<&s::Move>> {
    let mut moves: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for clause in fx.iter().flat_map(|effect| &effect.symbolic) {
        if let Clause::Move(m) = clause {
            moves.entry(m.child.clone()).or_default().push(m);
        }
    }
    moves
}

/// Declared transfer coverage is a prerequisite, independently of the order
/// chosen. An unrelated move cannot qualify a restoration's new ownership.
fn restoration_target(
    r: &Restoration,
    moves: &BTreeMap<Recorded, Vec<&s::Move>>,
    base: &State,
) -> Result<(), String> {
    if !base.tables.contains(&r.key.parent) || !base.tables.contains(&r.child) {
        return Err("restoration endpoint has an unknown UID".into());
    }
    if let Some(moves) = moves.get(&r.child) {
        if moves.iter().any(|m| {
            m.parent == r.key.parent
                && m.kind != MoveKind::Drop
                && r.member_of.as_ref() == (m.kind == MoveKind::Attach).then_some(&m.parent)
                && transfers(m).is_ok_and(|ts| {
                    ts.iter()
                        .any(|t| t.parent == r.key.property && t.to == r.property)
                })
        }) {
            return Ok(());
        }
        return Err("restoration target has no matching ownership/property transfer".into());
    }
    if base.members.get(&r.child).map(|m| &m.parent) != r.member_of.as_ref()
        || cell(base, &r.child, &r.property).is_err()
    {
        return Err("restoration target ownership/property is unavailable".into());
    }
    Ok(())
}

/// Called only with symbolic clauses: ordinary graphs retain their exact
/// edges and obligations. This does not choose or try an operation order.
pub(crate) fn extend(graph: &mut Graph, ops: &[Operation], fx: &[Effects], base: &State) {
    if fx.iter().all(|f| f.symbolic.is_empty()) {
        return;
    }
    if let Err(why) = baseline_valid(base) {
        graph.symbolic_unmet.push(problem(ops[0].id(), why));
        return;
    }
    let positions: BTreeMap<_, _> = ops.iter().enumerate().map(|(i, o)| (o.id(), i)).collect();
    let mut writes: BTreeMap<Key, Vec<Write<'_>>> = BTreeMap::new();
    let mut moves = Vec::new();
    let mut restores: BTreeMap<(Key, Recorded), Vec<Restore<'_>>> = BTreeMap::new();
    for (at, (op, effect)) in ops.iter().zip(fx).enumerate() {
        for (clause, value) in effect.symbolic.iter().enumerate() {
            let position = Position {
                at,
                clause,
                id: op.id(),
            };
            match value {
                Clause::Recurse(w) => writes
                    .entry(w.key.clone())
                    .or_default()
                    .push(Write { position, value: w }),
                Clause::Move(m) => moves.push((position, m)),
                Clause::Restore(r) => restores
                    .entry((r.key.clone(), r.child.clone()))
                    .or_default()
                    .push(Restore { position, value: r }),
                Clause::Require(fact) => {
                    if let Err(why) = fact.known() {
                        graph.symbolic_unmet.push(problem(op.id(), why));
                    }
                }
            }
        }
    }
    if positions.len() != ops.len() {
        graph
            .symbolic_unmet
            .push(problem(ops[0].id(), "duplicate operation identity"));
    }
    let mut chains = BTreeMap::new();
    for (key, candidates) in writes {
        match chain(&key, &candidates, base) {
            Ok(ordered) => {
                for pair in ordered.windows(2) {
                    edge(graph, pair[0].position, pair[1].position, &key);
                }
                chains.insert(key, ordered);
            }
            Err(why) => graph
                .symbolic_unmet
                .push(problem(candidates[0].position.id, why)),
        }
    }
    for ((key, _), list) in &mut restores {
        // Existing write/write policy uses immutable operation identities.
        list.sort_by_key(|r| (r.position.id, r.position.clause));
        for pair in list.windows(2) {
            restore_edge(graph, pair[0].position, pair[1].position, key);
        }
        for r in list.iter() {
            if let Err(why) = r.value.value.known() {
                graph.symbolic_unmet.push(problem(r.position.id, why));
            }
            if let Some(chain) = chains.get(key) {
                for w in chain {
                    restore_edge(graph, w.position, r.position, key);
                }
            }
        }
    }
    let declared_moves = moves_by_child(fx);
    for rs in restores.values() {
        for r in rs {
            if let Err(why) = restoration_target(r.value, &declared_moves, base) {
                graph.symbolic_unmet.push(problem(r.position.id, why));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for (position, m) in moves {
        let id = position.id;
        let checked = (|| -> Result<(), String> {
            if !seen.insert(m.child.clone()) {
                return Err(
                    "multiple membership moves of one UID require an explicit transfer chain"
                        .into(),
                );
            }
            if !base.tables.contains(&m.child) || !base.tables.contains(&m.parent) {
                return Err("a membership endpoint has an unknown UID".into());
            }
            if m.parent == m.child
                || base.members.contains_key(&m.parent)
                || (m.kind == MoveKind::Attach
                    && base.members.values().any(|member| member.parent == m.child))
            {
                return Err("nested or cyclic membership scope is not qualified".into());
            }
            match (m.kind, base.members.get(&m.child), m.consumes) {
                (MoveKind::Attach, None, None) => {}
                (MoveKind::Detach | MoveKind::Drop, Some(held), Some(version))
                    if held.parent == m.parent && held.incarnation == version => {}
                _ => return Err("membership input does not match the recorded endpoint".into()),
            }
            let mut source_slots = BTreeSet::new();
            let mut target_slots = BTreeSet::new();
            let mut parent_slots = BTreeSet::new();
            for t in transfers(m)? {
                if !source_slots.insert(t.from.clone())
                    || !target_slots.insert(t.to.clone())
                    || !parent_slots.insert(t.parent.clone())
                {
                    return Err("a transfer has conflicting physical aliases".into());
                }
                if m.kind != MoveKind::Attach
                    && base
                        .members
                        .get(&m.child)
                        .and_then(|member| member.aliases.get(&t.parent))
                        != Some(&t.from)
                {
                    return Err("transfer does not consume the recorded physical alias".into());
                }
                let key = Key {
                    parent: m.parent.clone(),
                    property: t.parent.clone(),
                };
                let list = chains.get(&key).map_or(&[][..], Vec::as_slice);
                let rs = restores
                    .get(&(key.clone(), m.child.clone()))
                    .map_or(&[][..], Vec::as_slice);
                for r in rs {
                    let member = if m.kind == MoveKind::Attach {
                        Some(&m.parent)
                    } else {
                        None
                    };
                    if m.kind == MoveKind::Drop || r.value.member_of.as_ref() != member {
                        return Err("restoration requires the wrong membership form".into());
                    }
                    if r.value.property != t.to {
                        return Err("restoration addresses a different transferred property".into());
                    }
                    restore_edge(graph, position, r.position, &key);
                }
                let mut admitted = Vec::new();
                for cut in 0..=list.len() {
                    admitted.push(admits(m, t, list, rs, base, cut)?);
                }
                let lo = admitted.iter().position(|yes| *yes).ok_or_else(|| {
                    format!("no boundary satisfies {key:?} for child {}", m.child.uid())
                })?;
                let hi = admitted.iter().rposition(|yes| *yes).unwrap();
                if admitted[lo..=hi].iter().any(|yes| !yes) {
                    return Err(format!(
                        "non-convex boundary for {key:?} and child {}",
                        m.child.uid()
                    ));
                }
                if lo > 0 {
                    edge(graph, list[lo - 1].position, position, &key);
                }
                if hi < list.len() {
                    edge(graph, position, list[hi].position, &key);
                }
            }
            Ok(())
        })();
        if let Err(why) = checked {
            graph.symbolic_unmet.push(problem(id, why));
        }
    }
    // Every symbolic issue must refer to this immutable operation collection.
    for issue in &graph.symbolic_unmet {
        assert!(positions.contains_key(&issue.operation));
    }
}
fn restore_edge(graph: &mut Graph, before: Position, after: Position, key: &Key) {
    dependency(graph, before, after, Because::Restores(key.clone()));
}
fn edge(graph: &mut Graph, before: Position, after: Position, key: &Key) {
    dependency(graph, before, after, Because::Boundary(key.clone()));
}
fn dependency(graph: &mut Graph, before: Position, after: Position, because: Because) {
    if before.at == after.at {
        // Operations are indivisible. A self-edge cannot repair clauses in
        // the opposite order; replay executes the original clause sequence.
        if before.clause >= after.clause {
            graph.symbolic_unmet.push(problem(
                before.id,
                format!(
                    "symbolic clause {} must precede clause {} for {because:?}",
                    before.clause, after.clause
                ),
            ));
        }
    } else {
        graph.edges.insert(Edge {
            before: before.at,
            after: after.at,
            source: Source::Effect,
            because,
        });
    }
}
fn chain<'a>(key: &Key, list: &[Write<'a>], base: &State) -> Result<Vec<Write<'a>>, String> {
    let first = cell(base, &key.parent, &key.property)?;
    first.value.known()?;
    let mut version = first.version;
    let mut value = first.value.known()?;
    let mut physical = first.clone();
    let mut left = list.to_vec();
    let mut out = Vec::new();
    while !left.is_empty() {
        let next: Vec<_> = left
            .iter()
            .enumerate()
            .filter(|(_, w)| w.value.input == version)
            .map(|(i, _)| i)
            .collect();
        if next.len() != 1 {
            return Err(format!(
                "fork or missing predecessor of {key:?} at {version:?}"
            ));
        }
        let w = left.remove(next[0]);
        if w.value.from.known()? != value {
            return Err(format!(
                "input definition/binding of {key:?} disagrees with {version:?}"
            ));
        }
        value = w.value.to.known()?;
        physical.origin = next_origin(
            &physical,
            value,
            w.value.lifetime,
            w.position.id,
            &key.parent,
            &key.property,
        )?;
        physical.value = Fact::Known(value.clone());
        version = Version::After(w.position.id);
        out.push(w);
    }
    Ok(out)
}
fn admits(
    m: &s::Move,
    t: &Transfer,
    chain: &[Write<'_>],
    rs: &[Restore<'_>],
    base: &State,
    cut: usize,
) -> Result<bool, String> {
    let mut parent = cell(base, &m.parent, &t.parent)?.value.known()?.clone();
    let mut physical = cell(base, &m.child, &t.from)?.clone();
    let mut child = physical.value.known()?.clone();
    for w in &chain[..cut] {
        parent = w.value.to.known()?.clone();
        if m.kind != MoveKind::Attach && w.value.scope == Scope::MembersAtOperation {
            let next = w.value.policy.apply(&child, &parent);
            let Ok(origin) = next_origin(
                &physical,
                &next,
                w.value.lifetime,
                w.position.id,
                &m.child,
                &t.from,
            ) else {
                return Ok(false);
            };
            physical.origin = origin;
            physical.value = Fact::Known(next.clone());
            child = next;
        }
    }
    if m.kind == MoveKind::Drop {
        return Ok(true);
    }
    if m.kind == MoveKind::Attach {
        if !t.check.accepts(&parent, &child) {
            return Ok(false);
        }
        child = t.policy.attach(&m.parent, &parent, &child);
        for w in &chain[cut..] {
            if w.value.scope == Scope::MembersAtOperation {
                let next = w.value.policy.apply(&child, w.value.to.known()?);
                let Ok(origin) = next_origin(
                    &physical,
                    &next,
                    w.value.lifetime,
                    w.position.id,
                    &m.child,
                    &t.to,
                ) else {
                    return Ok(false);
                };
                physical.origin = origin;
                physical.value = Fact::Known(next.clone());
                child = next;
            }
        }
    } else {
        child.ownership = Ownership::Own;
    }
    if let Some(last) = rs.last() {
        child = last.value.value.known()?.clone();
    }
    Ok(child == *t.goal.known()?)
}

/// A projection is tied to immutable operations/effects. It is descriptive,
/// never a PlanView and never a route for certifying an unordered payload.
/// ```compile_fail
/// fn executable(p: &pbps_diff::order::membership::Projection<'_>) {
///     let _: &dyn pbps_diff::order::PlanView<pbps_dialect::MinimalDialect> = p;
/// }
/// ```
/// A projection cannot outlive a mutation of its operations:
/// ```compile_fail
/// use pbps_diff::order::membership::project;
/// use pbps_model::operation::{Effects, Operation, symbolic::State};
/// fn reorder(ops: &mut [Operation], fx: &[Effects], base: &State) {
///     let view = project(ops, fx, base).unwrap();
///     ops.swap(0, 1);
///     let _ = view.state(pbps_model::plan_names::At::End);
/// }
/// ```
#[derive(Debug)]
pub struct Projection<'a> {
    operations: &'a [Operation],
    effects: &'a [Effects],
    states: Vec<State>,
    writes: Vec<(OperationId, Recorded, Property)>,
}
impl Projection<'_> {
    pub fn state(&self, at: pbps_model::plan_names::At) -> Option<&State> {
        use pbps_model::plan_names::At;
        let index = match at {
            At::Start => 0,
            At::End => self.operations.len(),
            At::Before(i) if i < self.operations.len() => i,
            At::Before(_) => return None,
            At::After(i) => i.checked_add(1)?,
        };
        self.states.get(index)
    }
    pub fn effects(&self) -> &[Effects] {
        self.effects
    }
    pub fn writes(&self) -> &[(OperationId, Recorded, Property)] {
        &self.writes
    }
}

/// Independent concrete replay: no chain, cut or predicted child set from
/// the graph compiler is consulted. Failure never retries another order.
pub fn project<'a>(
    ops: &'a [Operation],
    fx: &'a [Effects],
    base: &State,
) -> Result<Projection<'a>, Problem> {
    assert_eq!(ops.len(), fx.len());
    if let Some(op) = ops.first() {
        baseline_valid(base).map_err(|why| problem(op.id(), why))?;
    }
    let declared_moves = moves_by_child(fx);
    for (op, effect) in ops.iter().zip(fx) {
        for clause in &effect.symbolic {
            if let Clause::Restore(r) = clause {
                restoration_target(r, &declared_moves, base)
                    .map_err(|why| problem(op.id(), why))?;
            }
        }
    }
    let mut states = vec![base.clone()];
    let mut writes = Vec::new();
    let mut goals = Vec::new();
    for (op, effect) in ops.iter().zip(fx) {
        let mut next = states.last().unwrap().clone();
        for clause in &effect.symbolic {
            apply(&mut next, op.id(), clause, &mut writes, &mut goals)
                .map_err(|why| problem(op.id(), why))?;
        }
        states.push(next);
    }
    for (id, table, property, goal) in goals {
        let actual = cell(states.last().unwrap(), &table, &property).and_then(|c| c.value.known());
        if actual != Ok(&goal) {
            return Err(problem(
                id,
                format!(
                    "final property {property:?} of {} does not satisfy the declared goal",
                    table.uid()
                ),
            ));
        }
    }
    Ok(Projection {
        operations: ops,
        effects: fx,
        states,
        writes,
    })
}
type Goal = (OperationId, Recorded, Property, Value);
fn apply(
    state: &mut State,
    id: OperationId,
    clause: &Clause,
    writes: &mut Vec<(OperationId, Recorded, Property)>,
    goals: &mut Vec<Goal>,
) -> Result<(), String> {
    match clause {
        Clause::Require(f) => {
            f.known()?;
        }
        Clause::Recurse(w) => {
            let held = cell(state, &w.key.parent, &w.key.property)?;
            if held.version != w.input || held.value.known()? != w.from.known()? {
                return Err(format!(
                    "{}: recursive input version/definition mismatch",
                    w.key.parent.uid()
                ));
            }
            let value = w.to.known()?.clone();
            let mut updated = held.clone();
            updated.origin =
                next_origin(held, &value, w.lifetime, id, &w.key.parent, &w.key.property)?;
            updated.value = Fact::Known(value.clone());
            updated.version = Version::After(id);
            state
                .cells
                .insert((w.key.parent.clone(), w.key.property.clone()), updated);
            writes.push((id, w.key.parent.clone(), w.key.property.clone()));
            let targets: Vec<_> = state
                .members
                .iter()
                .filter(|(_, m)| w.scope == Scope::MembersAtOperation && m.parent == w.key.parent)
                .map(|(child, m)| {
                    m.aliases
                        .get(&w.key.property)
                        .cloned()
                        .map(|property| (child.clone(), property))
                })
                .collect::<Option<Vec<_>>>()
                .ok_or("membership property alias is unavailable")?;
            for (child, property) in targets {
                let previous = cell(state, &child, &property)?;
                let mut updated = previous.clone();
                let next = w.policy.apply(previous.value.known()?, &value);
                updated.origin = next_origin(previous, &next, w.lifetime, id, &child, &property)?;
                updated.value = Fact::Known(next);
                updated.version = Version::After(id);
                state
                    .cells
                    .insert((child.clone(), property.clone()), updated);
                writes.push((id, child, property));
            }
        }
        Clause::Move(m) => {
            if !state.tables.contains(&m.parent) || !state.tables.contains(&m.child) {
                return Err("membership endpoint has an unknown UID".into());
            }
            if m.parent == m.child
                || state.members.contains_key(&m.parent)
                || (m.kind == MoveKind::Attach
                    && state
                        .members
                        .values()
                        .any(|member| member.parent == m.child))
            {
                return Err("nested or cyclic membership scope is not qualified".into());
            }
            match (m.kind, state.members.get(&m.child), m.consumes) {
                (MoveKind::Attach, None, None) => {}
                (MoveKind::Detach | MoveKind::Drop, Some(held), Some(version))
                    if held.parent == m.parent && held.incarnation == version => {}
                _ => return Err("membership incarnation/owner mismatch".into()),
            }
            let ts = transfers(m)?;
            if m.kind == MoveKind::Drop {
                state.members.remove(&m.child);
                state.tables.remove(&m.child);
                state.cells.retain(|(table, _), _| *table != m.child);
            } else {
                let mut pending = Vec::new();
                let mut source_slots = BTreeSet::new();
                let mut target_slots = BTreeSet::new();
                for t in ts {
                    if !source_slots.insert(t.from.clone()) || !target_slots.insert(t.to.clone()) {
                        return Err("transfer aliases collide".into());
                    }
                    if m.kind == MoveKind::Detach
                        && state
                            .members
                            .get(&m.child)
                            .and_then(|member| member.aliases.get(&t.parent))
                            != Some(&t.from)
                    {
                        return Err("transfer does not consume the current physical alias".into());
                    }
                    let p = cell(state, &m.parent, &t.parent)?.value.known()?;
                    let c = cell(state, &m.child, &t.from)?;
                    let mut next = c.clone();
                    let value = if m.kind == MoveKind::Attach {
                        if !t.check.accepts(p, c.value.known()?) {
                            return Err(format!("attach shape differs at {:?}", t.parent));
                        }
                        t.policy.attach(&m.parent, p, c.value.known()?)
                    } else {
                        Value {
                            ownership: Ownership::Own,
                            ..c.value.known()?.clone()
                        }
                    };
                    next.value = Fact::Known(value);
                    next.version = Version::After(id);
                    pending.push((t.from.clone(), t.to.clone(), next, t.goal.known()?.clone()));
                }
                for (from, _, _, _) in &pending {
                    state.cells.remove(&(m.child.clone(), from.clone()));
                }
                for (_, to, next, goal) in pending {
                    if state
                        .cells
                        .insert((m.child.clone(), to.clone()), next)
                        .is_some()
                    {
                        return Err("transfer collides with another physical slot".into());
                    }
                    goals.push((id, m.child.clone(), to, goal));
                }
                if m.kind == MoveKind::Attach {
                    state.members.insert(
                        m.child.clone(),
                        Membership {
                            parent: m.parent.clone(),
                            incarnation: Version::After(id),
                            aliases: ts
                                .iter()
                                .map(|t| (t.parent.clone(), t.to.clone()))
                                .collect(),
                        },
                    );
                } else {
                    state.members.remove(&m.child);
                }
            }
        }
        Clause::Restore(r) => {
            if state.members.get(&r.child).map(|m| &m.parent) != r.member_of.as_ref() {
                return Err("restoration uses the wrong ownership form".into());
            }
            let mut next = cell(state, &r.child, &r.property)?.clone();
            next.value = Fact::Known(r.value.known()?.clone());
            next.version = Version::After(id);
            state
                .cells
                .insert((r.child.clone(), r.property.clone()), next);
            writes.push((id, r.child.clone(), r.property.clone()));
        }
    }
    Ok(())
}

/// Early consumers can retain an explicit pending lookup without constructing
/// a concrete child union. There is no conversion to an executable plan.
/// ```compile_fail
/// fn certify(p: pbps_diff::order::membership::Pending<'_>) {
///     let _: &dyn pbps_diff::order::PlanView<pbps_dialect::MinimalDialect> = &p;
/// }
/// ```
#[derive(Debug)]
pub enum Lookup<'a> {
    Known(&'a Cell),
    Pending,
    Unavailable(String),
}
/// Retaining a pending view also prevents replacement of its effects:
/// ```compile_fail
/// use pbps_diff::order::membership::Pending;
/// use pbps_model::operation::{Effects, Operation, symbolic::State};
/// fn replace(ops: &[Operation], fx: &mut [Effects], base: &State) {
///     let view = Pending::new(ops, fx, base);
///     fx.swap(0, 1);
///     let _ = view.project();
/// }
/// ```
#[derive(Debug)]
pub struct Pending<'a> {
    operations: &'a [Operation],
    effects: &'a [Effects],
    base: &'a State,
    affected: BTreeSet<(Recorded, Property)>,
}
impl<'a> Pending<'a> {
    pub fn new(operations: &'a [Operation], effects: &'a [Effects], base: &'a State) -> Self {
        assert_eq!(operations.len(), effects.len());
        let mut members: BTreeMap<&Recorded, Vec<(&Recorded, &Membership)>> = BTreeMap::new();
        for (child, membership) in &base.members {
            members
                .entry(&membership.parent)
                .or_default()
                .push((child, membership));
        }
        let mut affected = BTreeSet::new();
        for clause in effects.iter().flat_map(|fx| &fx.symbolic) {
            match clause {
                Clause::Recurse(w) => {
                    affected.insert((w.key.parent.clone(), w.key.property.clone()));
                    if w.scope == Scope::MembersAtOperation {
                        for (child, member) in members.get(&w.key.parent).into_iter().flatten() {
                            if let Some(property) = member.aliases.get(&w.key.property) {
                                affected.insert(((*child).clone(), property.clone()));
                            }
                        }
                    }
                }
                Clause::Move(m) => match &m.boundary {
                    Boundary::All(ts) => {
                        for t in ts {
                            affected.insert((m.child.clone(), t.from.clone()));
                            affected.insert((m.child.clone(), t.to.clone()));
                        }
                        if m.kind == MoveKind::Drop {
                            affected.extend(
                                base.cells
                                    .keys()
                                    .filter(|(table, _)| *table == m.child)
                                    .cloned(),
                            );
                        }
                    }
                    Boundary::Alternatives(ts) => {
                        for t in ts.iter().flatten() {
                            affected.insert((m.child.clone(), t.from.clone()));
                            affected.insert((m.child.clone(), t.to.clone()));
                        }
                    }
                },
                Clause::Restore(r) => {
                    affected.insert((r.child.clone(), r.property.clone()));
                }
                Clause::Require(_) => {}
            }
        }
        Self {
            operations,
            effects,
            base,
            affected,
        }
    }
    pub fn lookup(&self, table: &Recorded, property: &Property) -> Lookup<'_> {
        if !self.base.tables.contains(table) {
            return Lookup::Unavailable("unknown table UID".into());
        }
        if self.affected.contains(&(table.clone(), property.clone())) {
            return Lookup::Pending;
        }
        match cell(self.base, table, property) {
            Ok(cell) => match &cell.value {
                Fact::Known(_) => Lookup::Known(cell),
                Fact::Unread => Lookup::Pending,
                Fact::Unsupported { .. } => Lookup::Unavailable(cell.value.known().unwrap_err()),
            },
            Err(why) => Lookup::Unavailable(why),
        }
    }
    pub fn project(&self) -> Result<Projection<'_>, Problem> {
        project(self.operations, self.effects, self.base)
    }
}

#[cfg(test)]
mod tests;
