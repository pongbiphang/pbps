//! The operation-level verifier: a replay of operations over symbolic
//! objects, from the baseline.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::ModuleDeps;
use pbps_model::operation::{
    Effects, Incarnation, ObjectKey, Operation, OperationId, Recorded, Requirement, Resource,
};

use super::edges::{Baseline, Checks, Edge, Source, graph};

/// Why the replay refused an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// Symbolic recursion/transfer failed independent concrete replay.
    Symbolic(super::membership::Problem),
    /// A requirement no earlier operation and no baseline instance satisfies
    /// at the step.
    Unmet {
        operation: OperationId,
        requirement: Requirement,
    },
    /// A dependency that has to be gone is still there.
    StillPresent {
        operation: OperationId,
        key: ObjectKey,
    },
    /// An edge the effects imply runs the other way: settled over the
    /// effect graph, the order would not come back as given (DEC-1956.3).
    Edge {
        before: OperationId,
        after: OperationId,
        edge: Edge,
    },
    /// A `depends_on:` annotation runs the other way.
    BrokenDeclared {
        before: OperationId,
        after: OperationId,
    },
}

#[derive(Debug, Clone, Default)]
struct Standing {
    base: bool,
    plan: bool,
    base_target: Option<Recorded>,
    plan_target: Option<Recorded>,
}

/// Replays `operations` in the order given and refuses it where a
/// requirement is not satisfied at its step, an `Absent` dependency is still
/// present, or a `depends_on:` edge is broken; and settles the effect graph
/// over the order, refusing it where `settle` would not give it back.
///
/// The replay is independent of whatever produced the order: it reads only
/// the effects, the baseline and the annotations (DEC-1954.6). The edges are
/// no longer checked one by one: an order `settle` makes respects them by
/// construction, and one a pass made is held to what `settle` would make of
/// it (DEC-1956.3).
pub fn verify(
    operations: &[Operation],
    effects: &[Effects],
    baseline: &Baseline,
    declared: &ModuleDeps,
) -> Vec<Violation> {
    verify_with(operations, effects, baseline, declared, Checks::ALL)
}

pub(crate) fn verify_with(
    operations: &[Operation],
    effects: &[Effects],
    baseline: &Baseline,
    declared: &ModuleDeps,
    checks: Checks,
) -> Vec<Violation> {
    assert_eq!(operations.len(), effects.len());
    let mut out = Vec::new();
    if effects.iter().any(|fx| !fx.symbolic.is_empty())
        && let Err(problem) = super::membership::project(operations, effects, &baseline.symbolic)
    {
        out.push(Violation::Symbolic(problem));
    }
    // A key some operation ends or produces is replayed; any other stands as
    // the baseline has it, in both senses.
    let touched: BTreeSet<&ObjectKey> = effects
        .iter()
        .flat_map(|fx| fx.creates.iter().chain(&fx.removes))
        .filter_map(|r| match r {
            Resource::Object(key, _) => Some(key),
            Resource::Column(_) => None,
        })
        .collect();
    // Index lifetimes once: retypes may ask about many different defaults.
    let mut releases: BTreeMap<&ObjectKey, Vec<usize>> = BTreeMap::new();
    let mut restorations: BTreeMap<&ObjectKey, Vec<usize>> = BTreeMap::new();
    for (i, fx) in effects.iter().enumerate() {
        for resource in &fx.removes {
            if let Resource::Object(key, _) = resource {
                releases.entry(key).or_default().push(i);
            }
        }
        for resource in &fx.creates {
            if let Resource::Object(key, _) = resource {
                restorations.entry(key).or_default().push(i);
            }
        }
    }
    let held = |key: &ObjectKey| -> Option<bool> {
        if checks.baseline {
            baseline.holds(key)
        } else {
            Some(false)
        }
    };
    // Lazily: a key's standing is the baseline's until an operation moves it.
    let mut replay: BTreeMap<ObjectKey, Standing> = BTreeMap::new();
    let start = |key: &ObjectKey| -> Standing {
        let base = held(key);
        let plan = !touched.contains(key) && base == Some(true);
        Standing {
            base: base == Some(true),
            plan,
            base_target: baseline.bound_target(key).cloned(),
            plan_target: plan.then(|| baseline.bound_target(key).cloned()).flatten(),
        }
    };
    let mut values: BTreeMap<ObjectKey, (bool, bool)> = BTreeMap::new();
    for (operation_index, (operation, fx)) in operations.iter().zip(effects).enumerate() {
        for requirement in &fx.requires {
            let key = requirement.key();
            let now = replay
                .entry(key.clone())
                .or_insert_with(|| start(key))
                .clone();
            let unknown = held(key).is_none();
            let present = |s: &Standing| s.base || s.plan;
            let ok = match requirement {
                Requirement::Exists(_) => present(&now) || unknown,
                Requirement::Is(_, Incarnation::Base) => {
                    if checks.incarnations {
                        now.base || unknown
                    } else {
                        present(&now) || unknown
                    }
                }
                Requirement::Is(_, Incarnation::Final) => {
                    if checks.incarnations {
                        now.plan || unknown
                    } else {
                        present(&now) || unknown
                    }
                }
                Requirement::Bound(_, bound) => {
                    let input = replay
                        .entry(bound.input.clone())
                        .or_insert_with(|| start(&bound.input));
                    let input = input.clone();
                    match bound.incarnation {
                        Incarnation::Base if checks.incarnations => input.base,
                        Incarnation::Final if checks.incarnations => input.plan,
                        Incarnation::Base | Incarnation::Final => present(&input),
                    }
                }
                // Ordering requirements are checked below against the whole graph.
                Requirement::Holds(_) => true, // Independently checked by slot replay.
                Requirement::Reads(_) => {
                    let state = values.entry(key.clone()).or_insert_with(|| {
                        let live = held(key) != Some(false);
                        (live, live)
                    });
                    match checks
                        .incarnations
                        .then(|| fx.read_incarnation(key))
                        .flatten()
                    {
                        Some(Incarnation::Base) => state.0,
                        Some(Incarnation::Final) => state.1,
                        None => true,
                    }
                }
                Requirement::BeforeRewrite(_) => true,
                Requirement::NotBoundTo { target, .. } => {
                    let still_bound = unknown
                        || (now.base && now.base_target.as_ref().is_none_or(|t| t == target))
                        || (now.plan && now.plan_target.as_ref().is_none_or(|t| t == target));
                    if still_bound {
                        out.push(Violation::StillPresent {
                            operation: operation.id(),
                            key: key.clone(),
                        });
                    }
                    true
                }
                Requirement::ReleaseGap(_) => {
                    // Derive activation from the operations' effects, independently
                    // of the graph: a combined replacement has no gap to check.
                    let gap = releases.get(key).is_some_and(|from| {
                        restorations.get(key).is_some_and(|to| {
                            from.iter().any(|&a| {
                                a != operation_index
                                    && to.iter().any(|&c| c != a && c != operation_index)
                            })
                        })
                    });
                    if gap && present(&now) {
                        out.push(Violation::StillPresent {
                            operation: operation.id(),
                            key: key.clone(),
                        });
                    }
                    true
                }
                Requirement::Absent(_) => {
                    if present(&now) {
                        out.push(Violation::StillPresent {
                            operation: operation.id(),
                            key: key.clone(),
                        });
                    }
                    true
                }
            };
            if !ok {
                out.push(Violation::Unmet {
                    operation: operation.id(),
                    requirement: requirement.clone(),
                });
            }
        }
        for resource in &fx.removes {
            if let Resource::Column(key) = resource {
                values.insert(key.clone(), (false, false));
            }
            if let Resource::Object(key, _) = resource {
                let s = replay.entry(key.clone()).or_insert_with(|| start(key));
                if checks.incarnations {
                    // A removal ends the instance that stands: the
                    // baseline's while it does, and otherwise the one the
                    // plan made, which a later `Is(Final)` must not then
                    // find (#1998).
                    if s.base {
                        s.base = false;
                    } else {
                        s.plan = false;
                    }
                } else {
                    s.base = false;
                    s.plan = false;
                }
            }
        }
        for resource in &fx.creates {
            if let Resource::Column(key) = resource {
                let live = held(key) != Some(false);
                values.entry(key.clone()).or_insert((live, live)).1 = true;
            }
        }
        for resource in &fx.rewrites {
            if let Resource::Column(key) = resource {
                let now = replay.entry(key.clone()).or_insert_with(|| start(key));
                if now.base || now.plan || held(key).is_none() {
                    values.entry(key.clone()).or_insert((now.base, now.plan)).1 = true;
                }
            }
        }
        for resource in &fx.creates {
            if let Resource::Object(key, _) = resource {
                let s = replay.entry(key.clone()).or_insert_with(|| start(key));
                s.plan = true;
                s.plan_target = fx.bound_target(key, Incarnation::Final).cloned();
            }
        }
    }
    // The edges the effects imply: settled over the order given, they must
    // give it back (DEC-1956.3). Where they would not, each edge the order
    // runs the other way is named. A `depends_on:` edge run the other way is
    // the replay's own finding, whatever settle makes of it (#2083).
    let g = graph(operations, effects, baseline, declared, checks);
    let unsettled: BTreeSet<Edge> = super::schedule::unsettled(&g, operations)
        .into_iter()
        .collect();
    for e in &g.edges {
        let (before, after) = (operations[e.before].id(), operations[e.after].id());
        if e.source == Source::Declared && e.before >= e.after {
            out.push(Violation::BrokenDeclared { before, after });
        } else if unsettled.contains(e) {
            out.push(Violation::Edge {
                before,
                after,
                edge: e.clone(),
            });
        }
    }
    out
}
