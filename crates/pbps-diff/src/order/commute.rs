//! Two orders the effects allow, for the commutation suite (#1965, S1b-iii).
//!
//! The verifier checks an order against the declared effects, so it cannot
//! catch an effect nobody declared: a scheduler and a verifier reading the
//! same wrong effects agree on the same wrong plan. The suite closes that
//! from the other side. It takes a pair the effects call independent, runs
//! the plan in two orders that differ in that pair alone as far as the
//! effects can tell, each on its own database, and compares the results.
//!
//! This module only finds the pair and the second order. Running them is the
//! engine's, in each engine crate's live tests. Nothing here changes a plan.

use std::collections::BTreeSet;

use pbps_dialect::Dialect;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{ChangeSet, ModuleDeps};

use super::edges::{Baseline, Graph};
use super::shadow::{Report, analyse};

/// The effects of a plan's operations, in an order of them.
#[derive(Debug, Clone)]
pub struct Read {
    pub operations: Operations,
    pub report: Report,
}

/// Reads the effects of `changes`, one operation per change, in the order
/// `sequence` gives as positions into the plan (`None` for the plan's own).
///
/// Names stay those of the plan as the differ ordered it: a step's spelling
/// is fixed by its place in that order (DEC-1954.4), and an order the effects
/// allow never moves a step across a rename of what it names.
pub fn read(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    declared: &ModuleDeps,
    changes: &ChangeSet,
    sequence: Option<&[usize]>,
) -> Result<Read, String> {
    let modes = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let logical = Operations::of_plan(&changes.changes, modes);
    let operations = match sequence {
        Some(sequence) => crate::order::reordered_operations(&logical, sequence),
        None => logical,
    };
    let names = PlanNames::of(&changes.changes, desired.ids).map_err(|e| e.to_string())?;
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let before = Snapshot {
        schema: base.schema,
        ids: base.ids,
    };
    let after = Snapshot {
        schema: desired.schema,
        ids: desired.ids,
    };
    let baseline = Baseline::of(before, &owners);
    let report = analyse(&operations, &owners, before, after, dialect).report(
        &operations,
        &baseline,
        declared,
    );
    Ok(Read { operations, report })
}

/// Whether a path of edges joins `a` and `b`, in either direction: the
/// effects order them, transitively.
pub fn joined(graph: &Graph, n: usize, a: usize, b: usize) -> bool {
    let reach = |from: usize, to: usize| reaches(graph, n, from, Direction::Forward).contains(&to);
    reach(a, b) || reach(b, a)
}

/// The order `0..n`, changed so that `later` runs before `earlier` and every
/// edge still holds; `None` when the effects order the two.
///
/// Only what has to move moves. What stands between the two and leads to
/// `later` goes before it, in its own order; the rest stays after `earlier`.
/// Nothing between them can both follow `earlier` and lead to `later`: that
/// would be a path joining the two. So the edges among the moved steps hold,
/// and the order differs from `0..n` in this pair and not in what the
/// effects order.
///
/// # Panics
///
/// When `earlier` is not before `later`, or either is not below `n`.
pub fn swapped(graph: &Graph, n: usize, earlier: usize, later: usize) -> Option<Vec<usize>> {
    assert!(
        earlier < later && later < n,
        "{earlier} before {later} of {n}"
    );
    if joined(graph, n, earlier, later) {
        return None;
    }
    let leads = reaches(graph, n, later, Direction::Backward);
    let between = earlier + 1..later;
    let mut order: Vec<usize> = (0..earlier).collect();
    order.extend(between.clone().filter(|i| leads.contains(i)));
    order.extend([later, earlier]);
    order.extend(between.filter(|i| !leads.contains(i)));
    order.extend(later + 1..n);
    Some(order)
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

/// Every position a path of edges reaches from `from` (`from` excluded),
/// along the edges or against them.
fn reaches(graph: &Graph, n: usize, from: usize, direction: Direction) -> BTreeSet<usize> {
    let mut next: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in &graph.edges {
        match direction {
            Direction::Forward => next[e.before].push(e.after),
            Direction::Backward => next[e.after].push(e.before),
        }
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(i) = stack.pop() {
        for &j in &next[i] {
            if seen.insert(j) {
                stack.push(j);
            }
        }
    }
    seen.remove(&from);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::edges::{Because, Edge, Source};

    fn graph(edges: &[(usize, usize)]) -> Graph {
        let mut g = Graph::default();
        for &(before, after) in edges {
            g.edges.insert(Edge {
                before,
                after,
                source: Source::Declared,
                because: Because::Declared,
            });
        }
        g
    }

    /// Whether `order` keeps every edge of `g`.
    fn keeps(g: &Graph, order: &[usize]) -> bool {
        let at = |i: usize| order.iter().position(|&x| x == i).unwrap();
        g.edges.iter().all(|e| at(e.before) < at(e.after))
    }

    #[test]
    fn a_pair_no_edge_joins_is_swapped_and_nothing_else_moves() {
        let g = graph(&[(0, 3)]);
        assert_eq!(swapped(&g, 4, 1, 2), Some(vec![0, 2, 1, 3]));
    }

    #[test]
    fn what_leads_to_the_later_step_moves_before_it_and_the_rest_stays_after() {
        // 2 leads to 3; 1 follows 0 and nothing ties it to 3.
        let g = graph(&[(2, 3), (0, 1)]);
        let order = swapped(&g, 5, 0, 3).unwrap();
        assert_eq!(order, vec![2, 3, 0, 1, 4]);
        assert!(keeps(&g, &order));
    }

    /// A path through a third step orders the pair as surely as an edge.
    #[test]
    fn a_path_through_another_step_joins_the_pair_and_refuses_the_swap() {
        let g = graph(&[(0, 1), (1, 2)]);
        assert!(joined(&g, 3, 0, 2));
        assert!(joined(&g, 3, 2, 0));
        assert_eq!(swapped(&g, 3, 0, 2), None);
    }

    #[test]
    fn a_direct_edge_refuses_the_swap() {
        let g = graph(&[(0, 1)]);
        assert_eq!(swapped(&g, 2, 0, 1), None);
    }

    #[test]
    fn steps_that_share_nothing_are_not_joined() {
        let g = graph(&[(0, 1), (2, 3)]);
        assert!(!joined(&g, 4, 0, 3));
        let order = swapped(&g, 4, 1, 2).unwrap();
        assert_eq!(order, vec![0, 2, 1, 3]);
        assert!(keeps(&g, &order));
    }

    /// The order a swap produces keeps every edge, whatever the shape: the
    /// claim the suite rests on.
    #[test]
    fn every_swap_of_an_unjoined_pair_keeps_every_edge() {
        // A fixed pseudo-random sample of forward edges over 9 steps.
        let mut state = 0x2545_f491_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let n = 9;
            let edges: Vec<(usize, usize)> = (0..10)
                .map(|_| {
                    let a = (next() % n as u64) as usize;
                    let b = (next() % n as u64) as usize;
                    (a.min(b), a.max(b))
                })
                .filter(|(a, b)| a != b)
                .collect();
            let g = graph(&edges);
            for a in 0..n {
                for b in a + 1..n {
                    match swapped(&g, n, a, b) {
                        Some(order) => {
                            assert!(keeps(&g, &order), "{edges:?} {a} {b} {order:?}");
                            let at = |i| order.iter().position(|&x| x == i).unwrap();
                            assert!(at(b) < at(a));
                        }
                        None => assert!(joined(&g, n, a, b)),
                    }
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "before")]
    fn a_pair_out_of_order_is_a_caller_error() {
        let _ = swapped(&graph(&[]), 3, 2, 1);
    }
}
