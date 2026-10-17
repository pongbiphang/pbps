//! Settling a plan's order from its constraints (#1776 slice S3, #1956).
//!
//! [`Schedule::settle`] is Kahn's algorithm over the edges a caller states,
//! with the order the steps arrive in as the tie-break and nothing more
//! (DEC-1956.2). It returns the plan in that order and the
//! [`OrderingProof`] of the edges it kept, or a refusal that names what
//! cannot be ordered: a cycle with its members and the reason of each edge,
//! an obligation nothing meets, or an order a caller asked to keep that the
//! edges would reverse (DEC-1956.3).
//!
//! Every edge carries one sealed [`OrderReason`] beside its internal
//! [`Source`]. An effect edge's reason is [`reason`]'s exhaustive match over
//! the effect behind it, not its source (DEC-1956.1).
//!
//! The resolver, `order_after_releases` and the SQL Server computed-column
//! release order through it in every build, the resolver over the effects and
//! the others over their own edges. In tests and debug builds the verifier settles every observed order
//! over the full effect graph and requires the order it was given back: that
//! is how "every edge is respected" is checked (DEC-1956.3).

use std::collections::BTreeSet;
use std::fmt;

use pbps_model::ChangeSet;
use pbps_model::operation::{ObjectKey, Operation, Resource};
use pbps_model::resolver::{OrderEdge, OrderReason, OrderingProof};

use super::edges::{Because, Edge, Graph, Source};

/// One edge of a schedule: `before` runs before `after`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Constraint {
    /// Positions in the order the schedule was built over.
    pub before: usize,
    pub after: usize,
    /// Where the edge came from: internal, never sealed.
    pub source: Source,
    /// Why, in the sealed vocabulary.
    pub reason: OrderReason,
}

/// An order a caller keeps without stating every edge behind it: `before`
/// stays ahead of `after`, and an order that would reverse them is refused
/// with `refusal`, never repaired by moving `after` too.
///
/// It exists for what a pass knows a moved step must not pass, while the
/// edges that would let the later step move with it are not yet stated: name
/// slots (#1957) and lexical edges (#1967). Kept here rather than in the pass,
/// so the refusal is the schedule's (DEC-1956.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keep {
    pub before: usize,
    pub after: usize,
    pub refusal: String,
}

/// A requirement no step of the plan meets and the baseline does not, which
/// its caller refuses by name with the remedy it states (DEC-1956.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    pub operation: usize,
    pub refusal: String,
}

/// Why a schedule has no order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An edge names a step the schedule does not have.
    Outside {
        before: usize,
        after: usize,
        steps: usize,
    },
    /// The plan handed to [`Schedule::settle`] is not the one the schedule
    /// was built over.
    Mismatch { steps: usize, changes: usize },
    /// Obligations nothing meets, in the order they were stated.
    Unmet(Vec<Obligation>),
    /// The edges would reverse an order the caller keeps.
    Kept(Keep),
    /// Steps that wait on one another.
    Cycle {
        /// One strongly connected component, ascending: the cycle named.
        members: Vec<usize>,
        /// Every edge between its members.
        edges: Vec<Constraint>,
        /// Every step left unordered, ascending: the cycle's members and
        /// what waits on them.
        waiting: Vec<usize>,
    },
}

impl Refusal {
    /// The refusal with its steps named by their subjects in `changes`, the
    /// order the schedule was built over.
    pub fn describe(&self, changes: &ChangeSet) -> String {
        let name = |i: usize| {
            changes
                .changes
                .get(i)
                .map_or_else(|| format!("#{i}"), |p| format!("`{}`", p.change.subject()))
        };
        match self {
            Refusal::Outside {
                before,
                after,
                steps,
            } => format!(
                "an ordering edge {before} -> {after} names a step outside the {steps} the plan has"
            ),
            Refusal::Mismatch { steps, changes } => {
                format!("the schedule orders {steps} steps and the plan has {changes}")
            }
            Refusal::Unmet(obligations) => obligations
                .iter()
                .map(|o| o.refusal.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            Refusal::Kept(keep) => keep.refusal.clone(),
            Refusal::Cycle { members, edges, .. } => {
                let members: Vec<String> = members.iter().map(|&i| name(i)).collect();
                let edges: Vec<String> = edges
                    .iter()
                    .map(|e| {
                        format!(
                            "{} before {} ({}, {:?})",
                            name(e.before),
                            name(e.after),
                            reason_word(e.reason),
                            e.source
                        )
                    })
                    .collect();
                format!(
                    "these steps wait on one another: {}; because {}",
                    members.join(", "),
                    edges.join(", ")
                )
            }
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe(&ChangeSet::default()))
    }
}

fn reason_word(reason: OrderReason) -> &'static str {
    match reason {
        OrderReason::Structural => "structural",
        OrderReason::Binding => "binding",
        OrderReason::Identity => "identity",
        OrderReason::Data => "data",
        OrderReason::Authorization => "authorization",
        OrderReason::Restoration => "restoration",
    }
}

/// A settled plan.
#[derive(Debug, Clone)]
pub struct Settled {
    pub changes: ChangeSet,
    /// The edges that ordered it, over its own positions.
    pub proof: OrderingProof,
    /// For each position of `changes`, the position it came from.
    pub sequence: Vec<usize>,
}

/// Which of two ready steps goes first (DEC-1956.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Tie {
    /// The lowest position first: a step an edge holds back sinks to right
    /// after what it waits for, and nothing else moves. The one in use.
    #[default]
    Sink,
    /// Kahn's algorithm run from the end, the highest position last: what a
    /// step waits for is hoisted to right before it. Kept to show the choice
    /// is a choice: the tests settle both ways where the issue asks for it.
    #[cfg_attr(not(test), allow(dead_code))]
    Hoist,
}

/// The constraints over a plan's steps, and what settles them.
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    steps: usize,
    tie: Tie,
    constraints: BTreeSet<Constraint>,
    keeps: Vec<Keep>,
    obligations: Vec<Obligation>,
}

impl Schedule {
    /// A schedule of `steps` steps and no edge: it settles to their order.
    pub fn new(steps: usize) -> Self {
        Self {
            steps,
            ..Self::default()
        }
    }

    /// The effect graph's edges over `operations`, each with its reason
    /// (DEC-1956.1). Its unmet requirements are not obligations here: the
    /// verifier records them as `unprovided` and the plan stands
    /// (DEC-1954.6, DEC-1956.3).
    pub fn of_graph(graph: &Graph, operations: &[Operation]) -> Self {
        let mut schedule = Self::new(operations.len());
        for e in &graph.edges {
            schedule.edge(e.before, e.after, e.source, reason(e, operations));
        }
        schedule
    }

    /// `before` runs before `after`.
    pub fn edge(&mut self, before: usize, after: usize, source: Source, reason: OrderReason) {
        self.constraints.insert(Constraint {
            before,
            after,
            source,
            reason,
        });
    }

    /// See [`Keep`].
    pub fn keep(&mut self, before: usize, after: usize, refusal: String) {
        self.keeps.push(Keep {
            before,
            after,
            refusal,
        });
    }

    /// See [`Obligation`].
    pub fn obligation(&mut self, operation: usize, refusal: String) {
        self.obligations.push(Obligation { operation, refusal });
    }

    /// The other tie-break, for the tests that hold a property under both.
    #[cfg(test)]
    pub(crate) fn hoisting(mut self) -> Self {
        self.tie = Tie::Hoist;
        self
    }

    /// The orders kept so far, in the order stated.
    pub fn kept_orders(&self) -> &[Keep] {
        &self.keeps
    }

    pub fn constraints(&self) -> &BTreeSet<Constraint> {
        &self.constraints
    }

    /// The order of the steps, as the positions they came from.
    pub fn sequence(&self) -> Result<Vec<usize>, Refusal> {
        self.settled().map(|(sequence, _)| sequence)
    }

    /// `changes`, which the schedule was built over, in its order.
    pub fn settle(&self, changes: ChangeSet) -> Result<Settled, Refusal> {
        if changes.changes.len() != self.steps {
            return Err(Refusal::Mismatch {
                steps: self.steps,
                changes: changes.changes.len(),
            });
        }
        let (sequence, kept) = self.settled()?;
        let mut position = vec![0usize; self.steps];
        for (at, &from) in sequence.iter().enumerate() {
            position[from] = at;
        }
        let mut taken: Vec<Option<pbps_model::PlannedChange>> =
            changes.changes.into_iter().map(Some).collect();
        let ordered = ChangeSet {
            changes: sequence
                .iter()
                .map(|&i| taken[i].take().expect("a sequence is a permutation"))
                .collect(),
        };
        let edges: BTreeSet<OrderEdge> = kept
            .iter()
            .map(|c| OrderEdge {
                before: position[c.before],
                after: position[c.after],
                reason: c.reason,
            })
            .collect();
        // Every kept edge runs forward in the order Kahn's algorithm made and
        // names a step of it, which is all the proof validates.
        let proof = OrderingProof::new(&ordered, edges)
            .expect("a Kahn order runs every edge it kept forward");
        Ok(Settled {
            changes: ordered,
            proof,
            sequence,
        })
    }

    /// The order, and the edges it kept: every edge but those inside a
    /// component made only of `depends_on:` annotations, whose members keep
    /// the order given (DEC-1956.3, #2083).
    fn settled(&self) -> Result<(Vec<usize>, Vec<&Constraint>), Refusal> {
        let n = self.steps;
        if let Some(c) = self
            .constraints
            .iter()
            .find(|c| c.before >= n || c.after >= n || c.before == c.after)
        {
            return Err(Refusal::Outside {
                before: c.before,
                after: c.after,
                steps: n,
            });
        }
        if let Some(k) = self.keeps.iter().find(|k| k.before >= n || k.after >= n) {
            return Err(Refusal::Outside {
                before: k.before,
                after: k.after,
                steps: n,
            });
        }
        if !self.obligations.is_empty() {
            return Err(Refusal::Unmet(self.obligations.clone()));
        }
        let mut kept: Vec<&Constraint> = self.constraints.iter().collect();
        let mut emitted = kahn(n, &kept, self.tie);
        if emitted.len() < n {
            // A component made only of annotations is planned today, in the
            // order the module sort gave it; the policy is #2083's. Its
            // annotations are set aside, its members are chained in the order
            // given, and every edge that reaches it is redirected: an edge into
            // any member holds its first member, and an edge out of any member
            // leaves from its last. A step ordered against any member is so
            // held outside the span; a step unrelated to every member keeps
            // its place in the order given, which may fall between them, as
            // on master today (#1956 review). The chain and the redirected
            // edges order, but the proof holds the edges as stated, which they
            // imply.
            let mut chain: Vec<Constraint> = Vec::new();
            let mut declared_only: BTreeSet<(usize, usize)> = BTreeSet::new();
            // For each member of such a component: its first and last member.
            let mut ends: Vec<Option<(usize, usize)>> = vec![None; n];
            for members in components(n, &kept) {
                if members.len() < 2 {
                    continue;
                }
                let inside: BTreeSet<usize> = members.iter().copied().collect();
                let edges: Vec<&Constraint> = kept
                    .iter()
                    .copied()
                    .filter(|c| inside.contains(&c.before) && inside.contains(&c.after))
                    .collect();
                if edges.iter().all(|c| c.source == Source::Declared) {
                    declared_only.extend(edges.iter().map(|c| (c.before, c.after)));
                    chain.extend(members.windows(2).map(|pair| Constraint {
                        before: pair[0],
                        after: pair[1],
                        source: Source::Declared,
                        reason: OrderReason::Structural,
                    }));
                    let span = (members[0], members[members.len() - 1]);
                    for &m in &members {
                        ends[m] = Some(span);
                    }
                }
            }
            if !declared_only.is_empty() {
                kept.retain(|c| !declared_only.contains(&(c.before, c.after)));
                let redirected: Vec<Constraint> = kept
                    .iter()
                    .map(|c| Constraint {
                        before: ends[c.before].map_or(c.before, |(_, last)| last),
                        after: ends[c.after].map_or(c.after, |(first, _)| first),
                        ..(*c).clone()
                    })
                    .collect();
                let ordering: Vec<&Constraint> = redirected.iter().chain(&chain).collect();
                emitted = kahn(n, &ordering, self.tie);
            }
        }
        // A kept order the edges reversed, among the steps that were placed:
        // the first by where its earlier step landed, then by the order the
        // caller stated them. Checked before a cycle, so a refusal names the
        // first thing that went wrong in the order the plan would run.
        let mut position = vec![usize::MAX; n];
        for (at, &from) in emitted.iter().enumerate() {
            position[from] = at;
        }
        let reversed = self
            .keeps
            .iter()
            .enumerate()
            .filter(|(_, k)| {
                position[k.before] != usize::MAX
                    && position[k.after] != usize::MAX
                    && position[k.after] < position[k.before]
            })
            .min_by_key(|(stated, k)| (position[k.before], *stated));
        if let Some((_, keep)) = reversed {
            return Err(Refusal::Kept(keep.clone()));
        }
        if emitted.len() < n {
            let placed: BTreeSet<usize> = emitted.iter().copied().collect();
            let waiting: Vec<usize> = (0..n).filter(|i| !placed.contains(i)).collect();
            let left: Vec<&Constraint> = kept
                .iter()
                .copied()
                .filter(|c| !placed.contains(&c.before) && !placed.contains(&c.after))
                .collect();
            let members = components(n, &left)
                .into_iter()
                .filter(|members| members.len() > 1)
                .min()
                .unwrap_or_default();
            let inside: BTreeSet<usize> = members.iter().copied().collect();
            let edges = left
                .iter()
                .filter(|c| inside.contains(&c.before) && inside.contains(&c.after))
                .map(|c| (*c).clone())
                .collect();
            return Err(Refusal::Cycle {
                members,
                edges,
                waiting,
            });
        }
        Ok((emitted, kept))
    }
}

/// Kahn's algorithm, the lowest position first among the steps that are
/// ready: a step waits where an edge holds it and otherwise keeps the order
/// given (DEC-1956.2). Returns the steps it could place, in order; fewer than
/// `n` when the rest wait on one another.
fn kahn(n: usize, constraints: &[&Constraint], tie: Tie) -> Vec<usize> {
    let mut incoming = vec![0usize; n];
    let mut outgoing: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for c in constraints {
        let (from, to) = match tie {
            Tie::Sink => (c.before, c.after),
            Tie::Hoist => (c.after, c.before),
        };
        if outgoing[from].insert(to) {
            incoming[to] += 1;
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|&i| incoming[i] == 0).collect();
    let mut out = Vec::with_capacity(n);
    loop {
        let next = match tie {
            Tie::Sink => ready.pop_first(),
            Tie::Hoist => ready.pop_last(),
        };
        let Some(i) = next else { break };
        out.push(i);
        for &after in &outgoing[i] {
            incoming[after] -= 1;
            if incoming[after] == 0 {
                ready.insert(after);
            }
        }
    }
    if tie == Tie::Hoist {
        out.reverse();
    }
    out
}

/// The strongly connected components of the edges, each ascending, in
/// ascending order of their least member (Tarjan's algorithm, iterative, so a
/// long chain cannot overflow the stack).
fn components(n: usize, constraints: &[&Constraint]) -> Vec<Vec<usize>> {
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    for c in constraints {
        outgoing[c.before].push(c.after);
    }
    for next in &mut outgoing {
        next.sort_unstable();
        next.dedup();
    }
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut found: Vec<Vec<usize>> = Vec::new();
    let mut counter = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // (vertex, next child to visit)
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = counter;
        low[root] = counter;
        counter += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut child)) = work.last_mut() {
            if let Some(&w) = outgoing[v].get(*child) {
                *child += 1;
                if index[w] == usize::MAX {
                    index[w] = counter;
                    low[w] = counter;
                    counter += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut members = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                members.sort_unstable();
                found.push(members);
            }
        }
    }
    found.sort();
    found
}

/// The sealed reason of an effect edge (DEC-1956.1): an exhaustive match, so
/// an edge with no reason does not compile. The effect behind the edge
/// decides, not its source; a source decides only where it is the whole
/// story (evidence, an annotation, a search's proposal).
pub fn reason(edge: &Edge, operations: &[Operation]) -> OrderReason {
    match edge.source {
        Source::Binding => OrderReason::Binding,
        Source::Execution => OrderReason::Authorization,
        Source::Declared => OrderReason::Structural,
        // A proposed edge orders a slot's handover (#1957).
        Source::Search => OrderReason::Identity,
        Source::Effect | Source::Lexical | Source::Catalog => match &edge.because {
            Because::Creates(key)
            | Because::Removes(key)
            | Because::Frees(key)
            | Because::Precedes(key) => of_key(key),
            // A rename moves the address its reader spells.
            Because::Rewrites(key) => {
                let renames = |i: usize| {
                    operations.get(i).is_some_and(|op| {
                        matches!(
                            op.change(),
                            pbps_model::Change::RenameTable { .. }
                                | pbps_model::Change::RenameColumn { .. }
                                | pbps_model::Change::RenameRole { .. }
                        )
                    })
                };
                if renames(edge.before) || renames(edge.after) {
                    OrderReason::Identity
                } else {
                    of_key(key)
                }
            }
            Because::Writes(_) => OrderReason::Data,
            // Two writes of one object are a question of which instance
            // stands; two writes of values or rows, of which values do.
            Because::WriteWrite(Resource::Object(
                ObjectKey::Row { .. } | ObjectKey::RowByUnique { .. },
                _,
            ))
            | Because::WriteWrite(Resource::Column(_)) => OrderReason::Data,
            Because::WriteWrite(Resource::Object(
                ObjectKey::Table(_)
                | ObjectKey::Column(_)
                | ObjectKey::Role(_)
                | ObjectKey::Default(_)
                | ObjectKey::Module(_)
                | ObjectKey::NotNull(_)
                | ObjectKey::Part { .. }
                | ObjectKey::Grant { .. },
                _,
            )) => OrderReason::Identity,
            // One address passes from the baseline's instance to the plan's.
            Because::Replaces(_) => OrderReason::Identity,
            Because::Declared => OrderReason::Structural,
        },
    }
}

/// What depending on an object is, by the kind of object.
fn of_key(key: &ObjectKey) -> OrderReason {
    match key {
        ObjectKey::Row { .. } | ObjectKey::RowByUnique { .. } => OrderReason::Data,
        ObjectKey::Grant { .. } | ObjectKey::Role(_) => OrderReason::Authorization,
        ObjectKey::Table(_)
        | ObjectKey::Column(_)
        | ObjectKey::Default(_)
        | ObjectKey::Module(_)
        | ObjectKey::NotNull(_)
        | ObjectKey::Part { .. } => OrderReason::Structural,
    }
}

/// The settle check the verifier runs (DEC-1956.3): the effect graph, settled
/// over the order given, must give it back. Where it would not, each edge the
/// order runs the other way is named, as the verifier named them before
/// `settle` existed; annotations are the replay's (`BrokenDeclared`).
pub(crate) fn unsettled(graph: &Graph, operations: &[Operation]) -> Vec<Edge> {
    let schedule = Schedule::of_graph(graph, operations);
    let identity = |sequence: &[usize]| sequence.iter().enumerate().all(|(at, &i)| at == i);
    match schedule.sequence() {
        Ok(sequence) if identity(&sequence) => Vec::new(),
        Ok(_) | Err(_) => graph
            .edges
            .iter()
            .filter(|e| e.source != Source::Declared && e.before >= e.after)
            .cloned()
            .collect(),
    }
}

/// Which steps a settled order moved: a step any later one now precedes.
/// What a pass that used to move steps one by one reports as its count.
pub fn moved(sequence: &[usize]) -> usize {
    let mut furthest: Option<usize> = None;
    let mut count = 0;
    for &i in sequence {
        if furthest.is_some_and(|f| f > i) {
            count += 1;
        }
        furthest = Some(furthest.map_or(i, |f| f.max(i)));
    }
    count
}
