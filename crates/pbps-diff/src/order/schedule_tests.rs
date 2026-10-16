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
        let settled = Schedule::new(n).settle(plan(&given)).unwrap();
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
    let sunk = schedule(3, &edge).settle(plan(&["a", "b", "c"])).unwrap();
    assert_eq!(names(&sunk.changes), ["b", "c", "a"]);
    let hoisted = schedule(3, &edge)
        .hoisting()
        .settle(plan(&["a", "b", "c"]))
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
        let settled = s.settle(plan(&["a", "b", "c"])).unwrap();
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
    let refusal = s.settle(given.clone()).unwrap_err();
    let Refusal::Cycle {
        members,
        edges,
        waiting,
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
    assert!(s.settle(given).is_ok());
}

#[test]
fn an_unmet_obligation_is_refused_by_name_with_its_remedy() {
    let mut s = Schedule::new(2);
    s.obligation(
        1,
        "`b` needs `x` gone, and nothing drops it: drop it first".into(),
    );
    let refusal = s.settle(plan(&["a", "b"])).unwrap_err();
    assert!(matches!(&refusal, Refusal::Unmet(o) if o.len() == 1 && o[0].operation == 1));
    assert!(
        refusal
            .to_string()
            .contains("nothing drops it: drop it first"),
        "{refusal}"
    );
    // Negative: with none stated, the plan settles.
    assert!(Schedule::new(2).settle(plan(&["a", "b"])).is_ok());
}

#[test]
fn an_edge_outside_the_plan_or_another_plan_is_refused() {
    let s = schedule(2, &[(5, 0, Source::Effect)]);
    assert!(matches!(
        s.settle(plan(&["a", "b"])),
        Err(Refusal::Outside { before: 5, .. })
    ));
    assert!(matches!(
        Schedule::new(3).settle(plan(&["a", "b"])),
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
        .settle(plan(&["a", "b", "c", "d"]))
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
        .settle(plan(&["a", "b", "c"]))
        .unwrap();
    assert_eq!(names(&settled.changes), ["c", "a", "b"]);
    let mut kept = schedule(3, &reached);
    kept.keep(0, 1, "`a` stays ahead of `b`".into());
    assert!(kept.settle(plan(&["a", "b", "c"])).is_ok());

    let mixed = [
        (2, 1, Source::Declared),
        (1, 2, Source::Effect),
        (3, 0, Source::Effect),
    ];
    assert!(matches!(
        schedule(4, &mixed).settle(plan(&["a", "b", "c", "d"])),
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
    match s.settle(plan(&["a", "b", "c"])) {
        Err(Refusal::Kept(keep)) => assert_eq!(keep.refusal, "`a` may not pass `b`"),
        other => panic!("{other:?}"),
    }
    // Negative: what it keeps lies after where it lands.
    let mut s = schedule(3, &[(1, 0, Source::Catalog)]);
    s.keep(0, 2, "`a` may not pass `c`".into());
    let settled = s.settle(plan(&["a", "b", "c"])).unwrap();
    assert_eq!(names(&settled.changes), ["b", "a", "c"]);
    // Several reversed: the one whose earlier step lands first, then the
    // first stated, as the pass checked them. Settled: c, b, d, a.
    let mut s = schedule(4, &[(3, 0, Source::Catalog), (2, 1, Source::Catalog)]);
    s.keep(0, 2, "a's, stated first".into());
    s.keep(1, 2, "b's, landing first".into());
    s.keep(1, 2, "b's, stated later".into());
    match s.settle(plan(&["a", "b", "c", "d"])) {
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
    let first = schedule(6, &edges).settle(given.clone()).unwrap();
    let mut reversed = edges;
    reversed.reverse();
    let again = schedule(6, &reversed).settle(given).unwrap();
    assert_eq!(names(&first.changes), names(&again.changes));
    assert_eq!(first.proof, again.proof);

    let at = |from: usize| first.sequence.iter().position(|&i| i == from).unwrap();
    let carried: Vec<(usize, usize, Source)> = edges
        .iter()
        .map(|&(b, a, source)| (at(b), at(a), source))
        .collect();
    let settled_again = schedule(6, &carried).settle(first.changes.clone()).unwrap();
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
    match s.settle(changes) {
        Err(Refusal::Cycle { members, .. }) => assert_eq!(members.len(), n),
        other => panic!("{:?}", other.map(|s| s.sequence.len())),
    }
}
