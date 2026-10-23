//! `settle` on its own: the properties and negatives #1956 names, over edges
//! stated by hand. The effect graph's own cases are in `tests.rs`.

use pbps_model::resolver::OrderReason;
use pbps_model::{Change, ChangeSet, PlannedChange};

use super::edges::Source;
use super::schedule::{Refusal, Schedule};

/// Roles named by letter: their subjects are what the refusals print.
fn plan(names: &[&str]) -> ChangeSet {
    ChangeSet {
        changes: names
            .iter()
            .map(|name| {
                PlannedChange::new(Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: (*name).into(),
                })
            })
            .collect(),
    }
}

fn names(changes: &ChangeSet) -> Vec<String> {
    changes
        .changes
        .iter()
        .map(|p| p.change.subject().trim_start_matches("role ").to_owned())
        .collect()
}

fn schedule(n: usize, edges: &[(usize, usize, Source)]) -> Schedule {
    let mut s = Schedule::new(n);
    for &(before, after, source) in edges {
        s.edge(before, after, source, OrderReason::Structural);
    }
    s
}

#[test]
fn with_no_edges_the_order_given_is_the_order_settled() {
    for n in [0, 1, 5] {
        let letters: Vec<String> = (0..n).map(|i| format!("r{i}")).collect();
        let given: Vec<&str> = letters.iter().map(String::as_str).collect();
        let settled = Schedule::new(n).settle_raw(plan(&given)).unwrap();
        assert_eq!(names(&settled.changes), given);
        assert_eq!(settled.sequence, (0..n).collect::<Vec<_>>());
        assert!(settled.proof.edges().is_empty());
    }
}

/// DEC-1956.2: with the lowest position first a step held back sinks to
/// right after what it waits for; run from the end, what it waits for is
/// hoisted instead. The issue's own example.
#[test]
fn a_held_step_sinks_after_what_it_waits_for_and_nothing_else_moves() {
    let edge = [(2, 0, Source::Effect)];
    let sunk = schedule(3, &edge)
        .settle_raw(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&sunk.changes), ["b", "c", "a"]);
    let hoisted = schedule(3, &edge)
        .hoisting()
        .settle_raw(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&hoisted.changes), ["c", "a", "b"]);
    // The proof is over the settled positions: `c` at 1 before `a` at 2.
    assert_eq!(
        sunk.proof
            .edges()
            .iter()
            .map(|e| (e.before, e.after))
            .collect::<Vec<_>>(),
        [(1, 2)]
    );
    sunk.proof.validate(&sunk.changes).unwrap();
}

/// Negative from the issue: A, B, C with C -> A where A and B interact
/// (A -> B) is C, A, B under either tie-break, never B, C, A.
#[test]
fn a_step_that_interacts_with_a_held_one_moves_with_it() {
    let edges = [(2, 0, Source::Effect), (0, 1, Source::Effect)];
    for s in [schedule(3, &edges), schedule(3, &edges).hoisting()] {
        let settled = s.settle_raw(plan(&["a", "b", "c"])).unwrap();
        assert_eq!(names(&settled.changes), ["c", "a", "b"]);
        assert_ne!(names(&settled.changes), ["b", "c", "a"]);
    }
}

#[test]
fn a_cycle_is_refused_by_name_not_ordered_arbitrarily() {
    let mut s = Schedule::new(4);
    s.edge(1, 2, Source::Effect, OrderReason::Identity);
    s.edge(2, 1, Source::Catalog, OrderReason::Data);
    // What waits on the cycle is left unordered with it, not named in it.
    s.edge(2, 3, Source::Effect, OrderReason::Structural);
    let given = plan(&["a", "b", "c", "d"]);
    let refusal = s.settle_raw(given.clone()).unwrap_err();
    let Refusal::Cycle {
        members,
        edges,
        waiting,
        ..
    } = &refusal
    else {
        panic!("{refusal:?}")
    };
    assert_eq!(members, &[1, 2]);
    assert_eq!(waiting, &[1, 2, 3]);
    assert_eq!(edges.len(), 2, "{edges:?}");
    let text = refusal.describe(&given);
    assert!(
        text.contains("`role b`, `role c`")
            && text.contains("`role b` before `role c` (identity, Effect)")
            && text.contains("`role c` before `role b` (data, Catalog)"),
        "{text}"
    );
    // Negative: without the edge back it settles.
    let mut s = Schedule::new(4);
    s.edge(1, 2, Source::Effect, OrderReason::Identity);
    s.edge(2, 3, Source::Effect, OrderReason::Structural);
    assert!(s.settle_raw(given).is_ok());
}

#[test]
fn an_unmet_obligation_is_refused_by_name_with_its_remedy() {
    let mut s = Schedule::new(2);
    s.obligation(
        1,
        "`b` needs `x` gone, and nothing drops it: drop it first".into(),
    );
    let refusal = s.settle_raw(plan(&["a", "b"])).unwrap_err();
    assert!(matches!(&refusal, Refusal::Unmet(o) if o.len() == 1 && o[0].operation == 1));
    assert!(
        refusal
            .to_string()
            .contains("nothing drops it: drop it first"),
        "{refusal}"
    );
    // Negative: with none stated, the plan settles.
    assert!(Schedule::new(2).settle_raw(plan(&["a", "b"])).is_ok());
}

#[test]
fn an_edge_outside_the_plan_or_another_plan_is_refused() {
    let s = schedule(2, &[(5, 0, Source::Effect)]);
    assert!(matches!(
        s.settle_raw(plan(&["a", "b"])),
        Err(Refusal::Outside { before: 5, .. })
    ));
    assert!(matches!(
        Schedule::new(3).settle_raw(plan(&["a", "b"])),
        Err(Refusal::Mismatch {
            steps: 3,
            changes: 2
        })
    ));
}

/// DEC-1956.3, #2083: a cycle made only of `depends_on:` annotations is
/// planned today in the module sort's order, and keeps it. One with any
/// other edge in it is refused.
#[test]
fn a_cycle_of_annotations_alone_keeps_the_order_given_and_any_other_is_refused() {
    let declared = [
        (2, 1, Source::Declared),
        (1, 2, Source::Declared),
        (3, 0, Source::Effect),
    ];
    let settled = schedule(4, &declared)
        .settle_raw(plan(&["a", "b", "c", "d"]))
        .unwrap();
    assert_eq!(names(&settled.changes), ["b", "c", "d", "a"]);
    // The annotations it set aside are not in the proof.
    assert_eq!(settled.proof.edges().len(), 1);
    settled.proof.validate(&settled.changes).unwrap();

    // #1956 review: what reaches one member does not split them. `c`
    // waits for nothing and `a` for `c`; `a` and `b` keep their order.
    let reached = [
        (0, 1, Source::Declared),
        (1, 0, Source::Declared),
        (2, 0, Source::Effect),
    ];
    let settled = schedule(3, &reached)
        .settle_raw(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&settled.changes), ["c", "a", "b"]);
    let mut kept = schedule(3, &reached);
    kept.keep(0, 1, "`a` stays ahead of `b`".into());
    assert!(kept.settle_raw(plan(&["a", "b", "c"])).is_ok());

    // #2098 review: the component is one step for every edge that reaches
    // it. An edge into its later member holds its earlier one too.
    let into_later = [
        (0, 1, Source::Declared),
        (1, 0, Source::Declared),
        (2, 1, Source::Effect),
    ];
    let settled = schedule(3, &into_later)
        .settle_raw(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&settled.changes), ["c", "a", "b"]);
    settled.proof.validate(&settled.changes).unwrap();
    // The mirror: an edge out of its earlier member leaves from its later.
    let out_of_earlier = [
        (0, 2, Source::Declared),
        (2, 0, Source::Declared),
        (0, 1, Source::Effect),
    ];
    let settled = schedule(3, &out_of_earlier)
        .settle_raw(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&settled.changes), ["a", "c", "b"]);
    settled.proof.validate(&settled.changes).unwrap();
    // #2098 review: a step unrelated to every member keeps its place in the
    // order given, between them as on master; only what is ordered against
    // a member is held outside the span.
    let unrelated = [(0, 2, Source::Declared), (2, 0, Source::Declared)];
    let settled = schedule(3, &unrelated)
        .settle_raw(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&settled.changes), ["a", "b", "c"]);
    assert!(settled.proof.edges().is_empty());
    settled.proof.validate(&settled.changes).unwrap();

    let mixed = [
        (2, 1, Source::Declared),
        (1, 2, Source::Effect),
        (3, 0, Source::Effect),
    ];
    assert!(matches!(
        schedule(4, &mixed).settle_raw(plan(&["a", "b", "c", "d"])),
        Err(Refusal::Cycle { members, .. }) if members == [1, 2]
    ));
}

/// What a pass keeps is never reversed: refused by name where the edges
/// would, and silent where they do not.
#[test]
fn a_kept_order_the_edges_would_reverse_is_refused_not_repaired() {
    // `a` waits for `c`, and must stay ahead of `b`.
    let mut s = schedule(3, &[(2, 0, Source::Catalog)]);
    s.keep(0, 1, "`a` may not pass `b`".into());
    match s.settle_raw(plan(&["a", "b", "c"])) {
        Err(Refusal::Kept(keep)) => assert_eq!(keep.refusal, "`a` may not pass `b`"),
        other => panic!("{other:?}"),
    }
    // Negative: what it keeps lies after where it lands.
    let mut s = schedule(3, &[(1, 0, Source::Catalog)]);
    s.keep(0, 2, "`a` may not pass `c`".into());
    let settled = s.settle_raw(plan(&["a", "b", "c"])).unwrap();
    assert_eq!(names(&settled.changes), ["b", "a", "c"]);
    // Several reversed: the one whose earlier step lands first, then the
    // first stated, as the pass checked them. Settled: c, b, d, a.
    let mut s = schedule(4, &[(3, 0, Source::Catalog), (2, 1, Source::Catalog)]);
    s.keep(0, 2, "a's, stated first".into());
    s.keep(1, 2, "b's, landing first".into());
    s.keep(1, 2, "b's, stated later".into());
    match s.settle_raw(plan(&["a", "b", "c", "d"])) {
        Err(Refusal::Kept(keep)) => assert_eq!(keep.refusal, "b's, landing first"),
        other => panic!("{other:?}"),
    }
}

/// Whole-plan idempotency and determinism: the settled plan, settled again
/// under the same edges carried to its positions, is unchanged; and the
/// order the edges were stated in changes nothing.
#[test]
fn settling_is_idempotent_and_deterministic() {
    let edges = [
        (4, 0, Source::Effect),
        (3, 1, Source::Catalog),
        (0, 2, Source::Lexical),
        (5, 3, Source::Effect),
    ];
    let given = plan(&["a", "b", "c", "d", "e", "f"]);
    let first = schedule(6, &edges).settle_raw(given.clone()).unwrap();
    let mut reversed = edges;
    reversed.reverse();
    let again = schedule(6, &reversed).settle_raw(given).unwrap();
    assert_eq!(names(&first.changes), names(&again.changes));
    assert_eq!(first.proof, again.proof);

    let at = |from: usize| first.sequence.iter().position(|&i| i == from).unwrap();
    let carried: Vec<(usize, usize, Source)> = edges
        .iter()
        .map(|&(b, a, source)| (at(b), at(a), source))
        .collect();
    let settled_again = schedule(6, &carried)
        .settle_raw(first.changes.clone())
        .unwrap();
    assert_eq!(settled_again.sequence, (0..6).collect::<Vec<_>>());
    assert_eq!(names(&settled_again.changes), names(&first.changes));
    assert_eq!(settled_again.proof, first.proof);
}

#[test]
fn a_pass_counts_the_steps_settle_held_back() {
    use super::schedule::moved;
    assert_eq!(moved(&[0, 1, 2]), 0);
    // `a` held behind `c`.
    assert_eq!(moved(&[1, 2, 0]), 1);
    // `a` behind `d`, then `b` behind `a`.
    assert_eq!(moved(&[2, 3, 0, 1]), 2);
    assert_eq!(moved(&[]), 0);
}

/// A long chain settles without recursing: the component search is
/// iterative.
#[test]
fn a_long_cycle_is_named_without_overflowing_the_stack() {
    let n = 200_000;
    let mut s = Schedule::new(n);
    for i in 0..n {
        s.edge(i, (i + 1) % n, Source::Effect, OrderReason::Structural);
    }
    let changes = ChangeSet {
        changes: (0..n)
            .map(|_| {
                PlannedChange::new(Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "r".into(),
                })
            })
            .collect(),
    };
    match s.settle_raw(changes) {
        Err(Refusal::Cycle { members, .. }) => assert_eq!(members.len(), n),
        other => panic!("{:?}", other.map(|s| s.sequence.len())),
    }
}

#[test]
fn ordinary_unprovided_requirements_stay_observational_beside_symbolic_obligations() {
    use pbps_model::operation::{ModuleEdits, ObjectKey, Operations, Requirement};
    let given = plan(&["a"]);
    let operations = Operations::of_plan(&given.changes, ModuleEdits::InPlace);
    let missing = Requirement::Exists(ObjectKey::Module(pbps_model::ModuleId::Named(
        "app.missing".parse().unwrap(),
    )));
    let mut graph = super::Graph {
        unmet: vec![super::Unmet {
            operation: 0,
            requirement: missing,
        }],
        ..Default::default()
    };
    let settled = Schedule::of_graph(&graph, operations.as_slice())
        .settle_raw(given.clone())
        .unwrap();
    assert_eq!(settled.changes, given);
    assert!(settled.proof.edges().is_empty());
    // Even a mixed graph refuses only its explicit symbolic obligation.
    graph.symbolic_unmet.push(super::membership::Problem {
        operation: operations.as_slice()[0].id(),
        detail: "unread membership fact".into(),
    });
    let refusal = Schedule::of_graph(&graph, operations.as_slice())
        .sequence()
        .unwrap_err();
    let Refusal::Unmet(obligations) = &refusal else {
        panic!("{refusal:?}")
    };
    assert_eq!(obligations.len(), 1);
    assert!(obligations[0].refusal.contains("unread membership fact"));
    assert!(!obligations[0].refusal.contains("app.missing"));
}

/// Frozen ordinary adapter from master 0582bcb4224d9adb737de0c4df11f8fac6b15d0e.
/// Ordinary reason arms and the settling algorithm are unchanged; the oracle
/// deliberately imports only edges, not observational graph diagnostics.
fn master_of_graph(
    graph: &super::Graph,
    operations: &[pbps_model::operation::Operation],
) -> Schedule {
    let mut schedule = Schedule::new(operations.len());
    for e in &graph.edges {
        schedule.edge(
            e.before,
            e.after,
            e.source,
            super::schedule::reason(e, operations),
        );
    }
    schedule
}

#[test]
fn ordinary_graphs_keep_masters_orders_proofs_and_refusals_exhaustively() {
    use super::{Because, Edge, Graph, Unmet};
    use pbps_model::operation::{
        BoundInput, Incarnation, ModuleEdits, ObjectKey, Operations, Requirement,
    };
    // Every absent/ordinary-source/declared edge, including self-edges, on
    // up to three steps, repeated for every source. This includes DAGs,
    // mixed and declared-only cycles, disconnected and empty graphs.
    // Rotate reasons and all ordinary requirement forms beside them.
    let sources = [
        Source::Effect,
        Source::Declared,
        Source::Lexical,
        Source::Catalog,
        Source::Binding,
        Source::Execution,
        Source::Search,
    ];
    let key = ObjectKey::Module(pbps_model::ModuleId::Named(
        "app.unmanaged_view".parse().unwrap(),
    ));
    let requirements = [
        Requirement::Exists(key.clone()),
        Requirement::Is(key.clone(), Incarnation::Final),
        Requirement::Absent(key.clone()),
        Requirement::Reads(key.clone()),
        Requirement::BeforeRewrite(key.clone()),
        Requirement::Bound(
            key.clone(),
            BoundInput {
                input: key.clone(),
                incarnation: Incarnation::Base,
            },
        ),
    ];
    for source in sources {
        for n in 0..=3usize {
            let owned: Vec<_> = (0..n).map(|i| format!("r{i}")).collect();
            let given = plan(&owned.iter().map(String::as_str).collect::<Vec<_>>());
            let operations = Operations::of_plan(&given.changes, ModuleEdits::InPlace);
            for seed in 0..3usize.pow((n * n) as u32) {
                let mut encoded = seed;
                let mut graph = Graph::default();
                for before in 0..n {
                    for after in 0..n {
                        let edge = encoded % 3;
                        encoded /= 3;
                        if edge != 0 {
                            graph.edges.insert(Edge {
                                before,
                                after,
                                source: if edge == 1 { source } else { Source::Declared },
                                because: match seed % 3 {
                                    0 => Because::Declared,
                                    1 => Because::Creates(key.clone()),
                                    _ => Because::BeforeRewrite(key.clone()),
                                },
                            });
                        }
                    }
                }
                graph.unmet.push(Unmet {
                    operation: n.saturating_sub(1),
                    requirement: requirements[seed % requirements.len()].clone(),
                });
                graph.ambiguous.push(super::edges::Ambiguous {
                    operation: 0,
                    key: key.clone(),
                });
                let actual = Schedule::of_graph(&graph, operations.as_slice());
                let oracle = master_of_graph(&graph, operations.as_slice());
                assert_eq!(
                    actual.constraints(),
                    oracle.constraints(),
                    "n={n}, seed={seed}"
                );
                match (
                    actual.settle_raw(given.clone()),
                    oracle.settle_raw(given.clone()),
                ) {
                    (Ok(a), Ok(b)) => {
                        assert_eq!(a.sequence, b.sequence);
                        assert_eq!(a.changes, b.changes);
                        assert_eq!(a.proof, b.proof);
                    }
                    (Err(a), Err(b)) => {
                        assert_eq!(a, b);
                        assert_eq!(a.describe(&given), b.describe(&given));
                    }
                    (a, b) => panic!("ordinary graph changed at n={n}, seed={seed}: {a:?} / {b:?}"),
                }
            }
        }
    }
}
