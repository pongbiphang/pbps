//! The constraints a set of effects implies.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::ModuleDeps;
use pbps_model::operation::{
    Effects, Incarnation, ObjectKey, Operation, OperationId, Recorded, Requirement, Resource,
    Snapshot, baseline_bindings, baseline_keys, holds_unknown,
};
use pbps_model::plan_names::At;

/// Where an edge came from. Internal and never sealed: the sealed vocabulary
/// is `OrderReason`, which S3 maps each of these to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// The effects of the two operations.
    Effect,
    /// A `depends_on:` annotation (SPEC §4).
    Declared,
    /// What a body or an expression names, read through a lexer (S6a).
    Lexical,
    /// What the engine's catalog says depends on what (connected plans).
    Catalog,
    /// What the resolver's evidence binds an object to (S7a).
    Binding,
    /// What the executing principal must hold when a step runs: a grant the
    /// resolver's binding DDL and rows need first. Not an effect, so the
    /// verifier cannot re-derive it, and an engine's resolver supplies it
    /// with its bindings (DEC-1968.1).
    Execution,
    /// An edge a bounded search proposed (S4).
    Search,
    /// A name matched in a text and nothing else: a guess the engine may not
    /// need. A schedule obeys it unless it closes a cycle, and then gives the
    /// order stated to the steps in that cycle ([`Schedule::prefer`],
    /// DEC-2389.4).
    ///
    /// [`Schedule::prefer`]: super::Schedule::prefer
    Guess,
}

/// Why an effect edge exists, so a report can say it and a reviewer can check
/// it against `docs/ORDERING.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Because {
    /// A symbolic membership/property version boundary (DEC-2296.1).
    Boundary(pbps_model::operation::symbolic::Key),
    /// An explicit own-value restoration after its scoped writers.
    Restores(pbps_model::operation::symbolic::Key),
    /// The earlier creates what the later requires.
    Creates(ObjectKey),
    /// The earlier rewrites what the later requires, or reads the new name of.
    Rewrites(ObjectKey),
    /// The earlier releases what would block the later rewrite.
    BeforeRewrite(ObjectKey),
    /// The later removes what the earlier requires.
    Removes(ObjectKey),
    /// The earlier removes what the later needs gone.
    Frees(ObjectKey),
    /// The earlier needs it gone, and the later brings it back.
    Precedes(ObjectKey),
    /// The earlier writes the values the later reads.
    Writes(ObjectKey),
    /// Two operations write the same resource: the final state depends on
    /// which runs last, so they keep their base order.
    WriteWrite(Resource),
    /// The plan's instance replaces the baseline's under one address, so the
    /// baseline's ends first (S1a: an object has at most these two).
    Replaces(ObjectKey),
    /// A `depends_on:` annotation.
    Declared,
    /// Created policies follow the build; RLS flags follow policies.
    RowSecurity,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    /// Positions in the slice the graph was built over.
    pub before: usize,
    pub after: usize,
    pub source: Source,
    pub because: Because,
}

/// A requirement neither the plan nor the baseline can satisfy. This is an
/// observational `unprovided` fact, not a settlement obligation (DEC-1954.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmet {
    pub operation: usize,
    pub requirement: Requirement,
}

/// A requirement the plan can satisfy on either side of a rebuild, so no edge
/// is drawn: the operation names the instance it means with `Is`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguous {
    pub operation: usize,
    pub key: ObjectKey,
}

#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub edges: BTreeSet<Edge>,
    pub unmet: Vec<Unmet>,
    pub ambiguous: Vec<Ambiguous>,
    pub symbolic_unmet: Vec<super::membership::Problem>,
}

impl Graph {
    /// Whether the edges contain a cycle: a pair of constraints no order
    /// satisfies, the invented cycle of DEC-1364.1 when effects are too wide.
    pub fn has_cycle(&self, n: usize) -> bool {
        let mut incoming = vec![0usize; n];
        let mut out: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        for e in &self.edges {
            if out[e.before].insert(e.after) {
                incoming[e.after] += 1;
            }
        }
        let mut ready: Vec<usize> = (0..n).filter(|&i| incoming[i] == 0).collect();
        let mut seen = 0;
        while let Some(i) = ready.pop() {
            seen += 1;
            for &j in &out[i] {
                incoming[j] -= 1;
                if incoming[j] == 0 {
                    ready.push(j);
                }
            }
        }
        seen != n
    }

    pub fn edges_from(&self, source: Source) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(move |e| e.source == source)
    }
}

/// The connected computed-column pass owns these release edges. An earlier
/// schedule cannot apply their lexical approximation before that pass reads
/// the catalog (DEC-1431.1, DEC-2228.1).
pub(crate) fn connected_module_release(edge: &Edge, operations: &[Operation]) -> bool {
    matches!(
        &edge.because,
        Because::Frees(ObjectKey::Part {
            part: pbps_model::operation::PartKey::Computed(_),
            ..
        }) | Because::Precedes(ObjectKey::Part {
            part: pbps_model::operation::PartKey::Computed(_),
            ..
        })
    ) && [edge.before, edge.after].into_iter().any(|at| {
        matches!(
            operations[at].change(),
            pbps_model::Change::AlterModule { .. } | pbps_model::Change::DropModule { .. }
        )
    })
}

/// What the baseline holds. Rows and permissions are not in it, and
/// [`Baseline::holds`] says "not known" for them rather than "no".
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    held: BTreeSet<ObjectKey>,
    bindings: BTreeMap<ObjectKey, Recorded>,
    pub(crate) symbolic: pbps_model::operation::symbolic::State,
}

impl Baseline {
    /// Descriptive facts only; supplying them cannot certify a draft.
    pub fn with_symbolic(mut self, state: pbps_model::operation::symbolic::State) -> Self {
        self.symbolic = state;
        self
    }
    pub fn of(before: Snapshot<'_>, owners: &dyn pbps_model::operation::Owners) -> Self {
        Self {
            held: baseline_keys(before, owners),
            bindings: baseline_bindings(before, owners)
                .into_iter()
                .map(|b| (b.surface, b.target))
                .collect(),
            symbolic: Default::default(),
        }
    }

    /// A baseline that holds nothing: only for the guard check that shows what
    /// ignoring it costs.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_keys(keys: impl IntoIterator<Item = ObjectKey>) -> Self {
        Self {
            held: keys.into_iter().collect(),
            bindings: BTreeMap::new(),
            symbolic: Default::default(),
        }
    }

    pub(crate) fn bound_target(&self, key: &ObjectKey) -> Option<&Recorded> {
        self.bindings.get(key)
    }

    /// `Some(true)` it holds the key, `Some(false)` it does not, `None` it
    /// cannot say.
    pub fn holds(&self, key: &ObjectKey) -> Option<bool> {
        if self.held.contains(key) {
            Some(true)
        } else if holds_unknown(key) || matches!(key, ObjectKey::Role(_)) {
            // A role is the cluster's, and its identity file need not list a
            // role the cluster holds (ADR-0010 §3).
            None
        } else {
            Some(false)
        }
    }
}

/// Which parts of the analysis are on. Every test runs with [`Checks::ALL`];
/// the guard checks turn one off and show its negative then passes wrongly
/// (acceptance of #1954).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checks {
    pub baseline: bool,
    pub incarnations: bool,
    pub write_write: bool,
}

impl Checks {
    pub const ALL: Checks = Checks {
        baseline: true,
        incarnations: true,
        write_write: true,
    };
}

/// A rename moves an address. A reader of the key is ordered against it by
/// the name the reader spells, which is evidence, and not by position
/// (DEC-1954.4).
fn renames(op: &Operation) -> bool {
    matches!(
        op.change(),
        pbps_model::Change::RenameTable { .. }
            | pbps_model::Change::RenameColumn { .. }
            | pbps_model::Change::RenameRole { .. }
    )
}

/// Whether `reader` runs before the rename `rename`, by the name it spells.
fn reads_before(reader: &Operation, rename: &Operation) -> bool {
    use pbps_model::operation::{Source as Origin, Spelling};
    let Origin::Change(j) = rename.source() else {
        return false;
    };
    if let Some(before) = reader.address_before(rename) {
        return before;
    }
    match reader.logical_origin().spelling() {
        Spelling::Base => true,
        Spelling::Final => false,
        Spelling::At(At::Start) => true,
        Spelling::At(At::End) => false,
        // The name in force before change `i`: the renames at `i` or later
        // have not happened.
        Spelling::At(At::Before(i)) => j >= i,
        Spelling::At(At::After(i)) => j > i,
        Spelling::Input(_) => unreachable!("logical origins do not carry address bindings"),
    }
}

fn normalised(effects: &Effects, incarnations: bool) -> Effects {
    if incarnations {
        return effects.clone();
    }
    let merge = |r: &Resource| match r {
        Resource::Object(key, _) => Resource::Object(key.clone(), Incarnation::Final),
        Resource::Column(key) => Resource::Column(key.clone()),
    };
    Effects {
        symbolic: effects.symbolic.clone(),
        bindings: effects.bindings.clone(),
        creates: effects.creates.iter().map(merge).collect(),
        removes: effects
            .removes
            .iter()
            .map(|r| match r {
                Resource::Object(key, _) => Resource::Object(key.clone(), Incarnation::Base),
                Resource::Column(key) => Resource::Column(key.clone()),
            })
            .collect(),
        rewrites: effects.rewrites.iter().map(merge).collect(),
        requires: effects
            .requires
            .iter()
            .map(|r| match r {
                Requirement::Is(key, _) => Requirement::Exists(key.clone()),
                other @ (Requirement::Exists(_)
                | Requirement::Bound(..)
                | Requirement::Absent(_)
                | Requirement::ReleaseGap(_)
                | Requirement::NotBoundTo { .. }
                | Requirement::Reads(_)
                | Requirement::BeforeRewrite(_)
                | Requirement::Holds(_)) => other.clone(),
            })
            .collect(),
    }
}

#[derive(Default)]
struct Index {
    creators: BTreeMap<ObjectKey, Vec<usize>>,
    removers: BTreeMap<ObjectKey, Vec<usize>>,
    rewriters: BTreeMap<ObjectKey, Vec<usize>>,
    /// The writers of the values under a key, of whatever kind.
    valuers: BTreeMap<ObjectKey, Vec<usize>>,
    /// Every writer of a resource, for write-write.
    writers: BTreeMap<Resource, BTreeSet<usize>>,
}

impl Index {
    fn of(effects: &[Effects]) -> Self {
        let mut index = Index::default();
        for (i, fx) in effects.iter().enumerate() {
            // A step that writes a column's values also writes the rows of
            // the table it requires, for a reader of the whole table.
            let writes_a_column = fx
                .creates
                .iter()
                .chain(&fx.removes)
                .chain(&fx.rewrites)
                .any(|r| matches!(r, Resource::Column(ObjectKey::Column(_))));
            if writes_a_column {
                for requirement in &fx.requires {
                    if let Requirement::Exists(key @ ObjectKey::Table(_)) = requirement {
                        index.valuers.entry(key.clone()).or_default().push(i);
                    }
                }
            }
            for r in &fx.creates {
                match r {
                    Resource::Object(k, _) => index.creators.entry(k.clone()).or_default().push(i),
                    Resource::Column(k) => index.valuers.entry(k.clone()).or_default().push(i),
                }
                index.writers.entry(r.clone()).or_default().insert(i);
            }
            for r in &fx.removes {
                match r {
                    Resource::Object(k, _) => index.removers.entry(k.clone()).or_default().push(i),
                    Resource::Column(k) => index.valuers.entry(k.clone()).or_default().push(i),
                }
                index.writers.entry(r.clone()).or_default().insert(i);
            }
            for r in &fx.rewrites {
                match r {
                    Resource::Object(k, _) => index.rewriters.entry(k.clone()).or_default().push(i),
                    Resource::Column(k) => index.valuers.entry(k.clone()).or_default().push(i),
                }
                index.writers.entry(r.clone()).or_default().insert(i);
            }
        }
        index
    }
}

/// The graph of `effects`, over `operations` in the order given.
///
/// The order given is only an index: no direction comes from it. The one
/// exception is a pair of writes to the same resource, whose direction is the
/// operations' ids, which a plan's logical order assigned (S1a).
pub fn graph(
    operations: &[Operation],
    effects: &[Effects],
    baseline: &Baseline,
    declared: &ModuleDeps,
    checks: Checks,
) -> Graph {
    assert_eq!(operations.len(), effects.len());
    let effects: Vec<Effects> = effects
        .iter()
        .map(|fx| normalised(fx, checks.incarnations))
        .collect();
    let index = Index::of(&effects);
    let mut edges: BTreeSet<Edge> = BTreeSet::new();
    let mut unmet: Vec<Unmet> = Vec::new();
    let mut ambiguous: Vec<Ambiguous> = Vec::new();
    let held = |key: &ObjectKey| -> Option<bool> {
        if checks.baseline {
            baseline.holds(key)
        } else {
            Some(false)
        }
    };
    let mut edge = |before: usize, after: usize, source: Source, because: Because| {
        if before != after {
            out_edge(&mut edges, before, after, source, because);
        }
    };
    let none: Vec<usize> = Vec::new();
    for (b, fx) in effects.iter().enumerate() {
        for requirement in &fx.requires {
            let key = requirement.key();
            let creators = index.creators.get(key).unwrap_or(&none);
            // Once the first remover has run the object is gone, and any
            // other remover of it is ordered after that one as a second write
            // of the same resource.
            let removers: Vec<usize> = index
                .removers
                .get(key)
                .map(|all| {
                    all.iter()
                        .copied()
                        .min_by_key(|&i| operations[i].id())
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
            let removers = &removers;
            let rewriters = index.rewriters.get(key).unwrap_or(&none);
            let own_rewrite = rewriters.contains(&b);
            let rewrite_edges = |edge: &mut dyn FnMut(usize, usize, Source, Because)| {
                if own_rewrite {
                    // Two writes of one thing are ordered by their ids.
                    return;
                }
                for &a in rewriters {
                    if renames(&operations[a]) {
                        if fx
                            .requires
                            .iter()
                            .any(|r| matches!(r, Requirement::Holds(n) if &n.holder == key))
                        {
                            continue; // Shared slots own explicit input addresses.
                        }
                        if reads_before(&operations[b], &operations[a]) {
                            edge(b, a, Source::Effect, Because::Rewrites(key.clone()));
                        } else {
                            edge(a, b, Source::Effect, Because::Rewrites(key.clone()));
                        }
                    } else {
                        edge(a, b, Source::Effect, Because::Rewrites(key.clone()));
                    }
                }
            };
            match requirement {
                Requirement::Exists(_) => {
                    rewrite_edges(&mut edge);
                    let in_baseline = held(key) == Some(true);
                    match (
                        creators.iter().any(|&a| a != b),
                        removers.iter().any(|&a| a != b),
                    ) {
                        (true, true) => ambiguous.push(Ambiguous {
                            operation: b,
                            key: key.clone(),
                        }),
                        (true, false) => {
                            if !in_baseline {
                                for &a in creators {
                                    edge(a, b, Source::Effect, Because::Creates(key.clone()));
                                }
                            }
                        }
                        (false, true) => {
                            for &a in removers {
                                edge(b, a, Source::Effect, Because::Removes(key.clone()));
                            }
                        }
                        (false, false) => {}
                    }
                    if held(key) == Some(false) && creators.is_empty() {
                        unmet.push(Unmet {
                            operation: b,
                            requirement: requirement.clone(),
                        });
                    }
                }
                Requirement::Is(_, Incarnation::Final) => {
                    rewrite_edges(&mut edge);
                    for &a in creators {
                        edge(a, b, Source::Effect, Because::Creates(key.clone()));
                    }
                    if creators.is_empty() && held(key) == Some(false) {
                        unmet.push(Unmet {
                            operation: b,
                            requirement: requirement.clone(),
                        });
                    }
                }
                Requirement::Is(_, Incarnation::Base) => {
                    // A rename of what it reads moves the address it spells,
                    // so it is ordered by that name as any reader is
                    // (DEC-1954.4, #1998). Another rewrite is not: the
                    // baseline's instance is read whatever the plan does to
                    // it in place.
                    if !own_rewrite
                        && !fx
                            .requires
                            .iter()
                            .any(|r| matches!(r, Requirement::Holds(n) if &n.holder == key))
                    {
                        for &a in rewriters.iter().filter(|&&a| renames(&operations[a])) {
                            if reads_before(&operations[b], &operations[a]) {
                                edge(b, a, Source::Effect, Because::Rewrites(key.clone()));
                            } else {
                                edge(a, b, Source::Effect, Because::Rewrites(key.clone()));
                            }
                        }
                    }
                    for &a in removers {
                        edge(b, a, Source::Effect, Because::Removes(key.clone()));
                    }
                    if held(key) == Some(false) {
                        unmet.push(Unmet {
                            operation: b,
                            requirement: requirement.clone(),
                        });
                    }
                }
                Requirement::Bound(_, bound) => {
                    let input = &bound.input;
                    let (creators, removers) = (
                        index.creators.get(input).unwrap_or(&none),
                        index.removers.get(input).unwrap_or(&none),
                    );
                    match bound.incarnation {
                        Incarnation::Final => {
                            for &a in creators {
                                edge(a, b, Source::Binding, Because::Creates(input.clone()));
                            }
                        }
                        Incarnation::Base => {
                            for &a in removers {
                                edge(b, a, Source::Binding, Because::Removes(input.clone()));
                            }
                        }
                    }
                }
                Requirement::NotBoundTo { target, .. } => {
                    let relevant = |i: usize, incarnation| {
                        effects[i]
                            .bound_target(key, incarnation)
                            .is_none_or(|bound| bound == target)
                    };
                    let removers: Vec<_> = index
                        .removers
                        .get(key)
                        .unwrap_or(&none)
                        .iter()
                        .copied()
                        .filter(|&i| relevant(i, Incarnation::Base))
                        .min_by_key(|&i| operations[i].id())
                        .into_iter()
                        .collect();
                    for &a in &removers {
                        edge(a, b, Source::Effect, Because::Frees(key.clone()));
                    }
                    for &a in creators
                        .iter()
                        .filter(|&&i| relevant(i, Incarnation::Final))
                    {
                        edge(b, a, Source::Effect, Because::Precedes(key.clone()));
                    }
                    if removers.iter().all(|&a| a == b)
                        && held(key) != Some(false)
                        && baseline
                            .bound_target(key)
                            .is_none_or(|bound| bound == target)
                    {
                        unmet.push(Unmet {
                            operation: b,
                            requirement: requirement.clone(),
                        });
                    }
                }
                Requirement::ReleaseGap(_) => {
                    for &a in removers {
                        for &c in creators {
                            // A single in-place edit has no released state.
                            if a != c && a != b && c != b {
                                edge(a, b, Source::Effect, Because::Frees(key.clone()));
                                edge(b, c, Source::Effect, Because::Precedes(key.clone()));
                            }
                        }
                    }
                }
                Requirement::Absent(_) => {
                    for &a in removers {
                        edge(a, b, Source::Effect, Because::Frees(key.clone()));
                    }
                    for &a in creators {
                        edge(b, a, Source::Effect, Because::Precedes(key.clone()));
                    }
                    if removers.iter().all(|&a| a == b) && held(key) == Some(true) {
                        unmet.push(Unmet {
                            operation: b,
                            requirement: requirement.clone(),
                        });
                    }
                }
                Requirement::BeforeRewrite(_) => {
                    for &a in rewriters {
                        edge(b, a, Source::Effect, Because::BeforeRewrite(key.clone()));
                    }
                }
                Requirement::Holds(_) => {} // Edges and replay use the shared slot description.
                Requirement::Reads(_) => {
                    for &a in index.valuers.get(key).unwrap_or(&none) {
                        let ends_values =
                            effects[a].removes.contains(&Resource::Column(key.clone()));
                        let incarnation = fx.read_incarnation(key);
                        // A removal ends values; only a later incarnation waits
                        // for it. Base and bound-current readers precede it.
                        let later_creation = creators
                            .iter()
                            .any(|&c| operations[c].id() > operations[a].id());
                        let later_values = held(key) == Some(true)
                            && !creators.is_empty()
                            && (effects[a].creates.contains(&Resource::Column(key.clone()))
                                || effects[a].read_incarnation(key) == Some(Incarnation::Final));
                        if (ends_values
                            && (incarnation == Some(Incarnation::Base)
                                || (incarnation == Some(Incarnation::Final) && !later_creation)))
                            || (incarnation == Some(Incarnation::Base) && later_values)
                        {
                            edge(b, a, Source::Effect, Because::Removes(key.clone()));
                        } else {
                            edge(a, b, Source::Effect, Because::Writes(key.clone()));
                        }
                    }
                }
            }
        }
    }
    for (key, makers) in &index.creators {
        let Some(enders) = index.removers.get(key) else {
            continue;
        };
        // Only the baseline's incarnation is ended before a new one is made.
        // A key the baseline does not hold is ended by an operation that ends
        // what a plan step made, so that step comes first (a detach makes a
        // table's own default, which a later change of it then ends).
        if checks.baseline && held(key) == Some(false) {
            // Each ending ends the incarnations the plan's own order made
            // before it; one made after it is a later incarnation (a default
            // dropped and set again around a module change).
            for &ended in enders {
                for &made in makers {
                    if operations[made].id() < operations[ended].id() {
                        edge(made, ended, Source::Effect, Because::Creates(key.clone()));
                    }
                }
            }
            continue;
        }
        if let Some(&first) = enders.iter().min_by_key(|&&i| operations[i].id()) {
            for &made in makers {
                edge(first, made, Source::Effect, Because::Replaces(key.clone()));
            }
        }
    }
    if checks.write_write {
        for (resource, writers) in &index.writers {
            if writers.len() < 2 {
                continue;
            }
            // The ids a plan's logical order assigned, which is the base order.
            let mut ordered: Vec<usize> = writers.iter().copied().collect();
            ordered.sort_by_key(|&i| operations[i].id());
            for pair in ordered.windows(2) {
                edge(
                    pair[0],
                    pair[1],
                    Source::Effect,
                    Because::WriteWrite(resource.clone()),
                );
            }
        }
    }
    for (a, b) in super::row_security::edges(operations.iter().map(Operation::change)) {
        edge(a, b, Source::Effect, Because::RowSecurity);
    }
    declared_edges(operations, declared, &mut |a, b| {
        edge(a, b, Source::Declared, Because::Declared)
    });
    let mut result = Graph {
        edges,
        unmet,
        ambiguous,
        symbolic_unmet: Vec::new(),
    };
    super::membership::extend(&mut result, operations, &effects, &baseline.symbolic);
    result
}

fn out_edge(
    edges: &mut BTreeSet<Edge>,
    before: usize,
    after: usize,
    source: Source,
    because: Because,
) {
    edges.insert(Edge {
        before,
        after,
        source,
        because,
    });
}

/// `depends_on:` makes a module's definition follow the inputs it names, and
/// its drop precede theirs (SPEC §4).
pub fn declared_edges(
    operations: &[Operation],
    declared: &ModuleDeps,
    edge: &mut dyn FnMut(usize, usize),
) {
    use pbps_model::Change;
    // Indexed once: a pair scan over every operation per annotation is cubic
    // in the module operations of a plan.
    let mut by_module: std::collections::BTreeMap<&pbps_model::ModuleId, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, op) in operations.iter().enumerate() {
        if let Some(id) = op.change().module_id() {
            by_module.entry(id).or_default().push(i);
        }
    }
    let none: Vec<usize> = Vec::new();
    let defines =
        |c: &Change| matches!(c, Change::CreateModule { .. } | Change::AlterModule { .. });
    for (dependent, inputs) in declared {
        let dependents = by_module.get(dependent).unwrap_or(&none);
        for input in inputs {
            for &i in by_module.get(input).unwrap_or(&none) {
                for &j in dependents {
                    let (a, b) = (&operations[i], &operations[j]);
                    if defines(a.change()) && defines(b.change()) {
                        edge(i, j);
                    }
                    if matches!(a.change(), Change::DropModule { .. })
                        && matches!(b.change(), Change::DropModule { .. })
                    {
                        edge(j, i);
                    }
                }
            }
        }
    }
}

/// The ids of the operations, for a report.
pub fn ids(operations: &[Operation]) -> Vec<OperationId> {
    operations.iter().map(Operation::id).collect()
}

#[cfg(test)]
#[path = "edges_address_tests.rs"]
mod address_tests;
