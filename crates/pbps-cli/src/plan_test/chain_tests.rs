use super::chain::{ChainProblem, Hop, PinGroup, order};
use super::git::GitProblem;
use pbps_config::test_manifest::CommitId;
use std::collections::{BTreeMap, BTreeSet};

fn id(n: u8) -> CommitId {
    CommitId::parse(&format!("{n:040x}")).unwrap()
}

/// A history given as each commit's ancestors (itself included).
struct Graph(BTreeMap<CommitId, BTreeSet<CommitId>>);

impl Graph {
    /// `edges`: (commit, parent) pairs; the ancestors are the closure.
    fn new(edges: &[(u8, u8)]) -> Self {
        let mut all = BTreeMap::<u8, BTreeSet<u8>>::new();
        for &(c, p) in edges {
            all.entry(c).or_default().insert(c);
            all.entry(p).or_default().insert(p);
        }
        loop {
            let mut grew = false;
            for &(c, p) in edges {
                let up = all[&p].clone();
                let mine = all.get_mut(&c).unwrap();
                for a in up {
                    grew |= mine.insert(a);
                }
            }
            if !grew {
                break;
            }
        }
        Self(
            all.into_iter()
                .map(|(c, set)| (id(c), set.into_iter().map(id).collect()))
                .collect(),
        )
    }

    fn ancestor(&self, a: &CommitId, b: &CommitId) -> Result<bool, GitProblem> {
        Ok(self.0.get(b).is_some_and(|set| set.contains(a)))
    }
}

fn run(
    graph: &Graph,
    base: u8,
    pins: &[u8],
    tip: u8,
) -> Result<super::ReplayChain, Vec<ChainProblem>> {
    let pins: Vec<_> = pins.iter().copied().map(id).collect();
    order(
        &id(base),
        &pins,
        &id(tip),
        |a, b| graph.ancestor(a, b),
        |_, _| Ok(7),
    )
}

/// 1 <- 2 <- 3 <- 4 on one line; 5 branches from 2.
fn line() -> Graph {
    Graph::new(&[(2, 1), (3, 2), (4, 3), (5, 2)])
}

#[test]
fn pins_replay_oldest_first_and_equal_pins_form_one_group_in_manifest_order() {
    let chain = run(&line(), 3, &[2, 1, 2], 4).unwrap();
    assert_eq!(
        chain.groups,
        [
            PinGroup {
                commit: id(1),
                fixtures: vec![1]
            },
            PinGroup {
                commit: id(2),
                fixtures: vec![0, 2]
            },
            PinGroup {
                commit: id(3),
                fixtures: vec![]
            },
        ]
    );
    assert_eq!(
        chain.hops,
        [
            Hop {
                from: id(1),
                to: id(2),
                distance: 7
            },
            Hop {
                from: id(2),
                to: id(3),
                distance: 7
            },
        ]
    );
}

#[test]
fn no_fixtures_means_the_base_alone_and_no_hop() {
    let chain = run(&line(), 3, &[], 3).unwrap();
    assert_eq!(
        chain.groups,
        [PinGroup {
            commit: id(3),
            fixtures: vec![]
        }]
    );
    assert!(chain.hops.is_empty());
}

#[test]
fn a_fixture_pinned_at_the_base_is_loaded_at_the_base() {
    let chain = run(&line(), 3, &[3], 3).unwrap();
    assert_eq!(
        chain.groups,
        [PinGroup {
            commit: id(3),
            fixtures: vec![0]
        }]
    );
    assert!(chain.hops.is_empty(), "no hop to replay");
}

#[test]
fn pins_on_diverging_histories_are_refused_by_name() {
    // 3 and 5 both descend from 2 and neither contains the other.
    let found = run(&line(), 4, &[3, 5], 4).unwrap_err();
    assert!(
        found.contains(&ChainProblem::Incomparable { a: id(3), b: id(5) }),
        "{found:?}"
    );
    // The same pins as a single line are fine.
    assert!(run(&line(), 4, &[2, 3], 4).is_ok());
}

#[test]
fn a_pin_newer_than_the_base_is_refused() {
    let found = run(&line(), 2, &[3], 4).unwrap_err();
    assert_eq!(
        found,
        [ChainProblem::AfterBase {
            pin: id(3),
            base: id(2)
        }]
    );
}

#[test]
fn a_base_the_candidate_does_not_contain_is_refused() {
    // 5 branched from 2; the base 3 is not behind it.
    let found = run(&line(), 3, &[], 5).unwrap_err();
    assert_eq!(
        found,
        [ChainProblem::BaseNotBehindCandidate {
            base: id(3),
            tip: id(5)
        }]
    );
}

#[test]
fn an_answer_that_could_not_be_had_is_never_read_as_not_an_ancestor() {
    let graph = line();
    let found = order(
        &id(3),
        &[id(1)],
        &id(4),
        |a, b| {
            if *a == id(1) {
                Err(GitProblem::Unreadable("object store damaged".into()))
            } else {
                graph.ancestor(a, b)
            }
        },
        |_, _| Ok(1),
    )
    .unwrap_err();
    assert!(
        found
            .iter()
            .all(|p| matches!(p, ChainProblem::Unknown { .. })),
        "{found:?}"
    );
    assert!(
        !found
            .iter()
            .any(|p| matches!(p, ChainProblem::Incomparable { .. }))
    );
}

#[test]
fn every_problem_is_returned_and_each_pair_is_asked_once() {
    let graph = line();
    let mut asked = Vec::new();
    let pins = [id(3), id(5)];
    let found = order(
        &id(4),
        &pins,
        &id(5),
        |a, b| {
            asked.push((a.clone(), b.clone()));
            graph.ancestor(a, b)
        },
        |_, _| Ok(1),
    )
    .unwrap_err();
    // Diverging pins and a base the candidate does not contain, together.
    assert!(
        found
            .iter()
            .any(|p| matches!(p, ChainProblem::Incomparable { .. }))
    );
    assert!(
        found
            .iter()
            .any(|p| matches!(p, ChainProblem::BaseNotBehindCandidate { .. }))
    );
    let unique: BTreeSet<_> = asked.iter().collect();
    assert_eq!(
        unique.len(),
        asked.len(),
        "a pair was asked twice: {asked:?}"
    );
}

/// A shallow clone cannot always walk a pair in the direction it is first
/// asked: it answers `Incomplete` there and proves the other direction. Which
/// direction comes first depends on how the two ids sort, so both ids are
/// tried in both roles.
#[test]
fn a_pair_one_direction_proves_is_ordered_whichever_way_the_ids_sort() {
    for (older, newer) in [(1, 2), (2, 1)] {
        let graph = Graph::new(&[(newer, older)]);
        let chain = order(
            &id(newer),
            &[id(older)],
            &id(newer),
            |a, b| {
                if graph.ancestor(a, b) == Ok(true) {
                    Ok(true)
                } else if graph.ancestor(b, a) == Ok(true) {
                    // The walk that would answer "no" is the one a shallow
                    // boundary refuses to vouch for.
                    Err(GitProblem::Incomplete)
                } else {
                    Ok(false)
                }
            },
            |_, _| Ok(1),
        )
        .unwrap_or_else(|problems| panic!("{older} before {newer}: {problems:?}"));
        let order: Vec<_> = chain.groups.iter().map(|g| g.commit.clone()).collect();
        assert_eq!(order, [id(older), id(newer)]);
    }
}

#[test]
fn a_pair_neither_direction_can_answer_is_unknown_once_per_direction_and_never_incomparable() {
    let found = order(
        &id(2),
        &[id(1)],
        &id(2),
        |a, b| {
            if (a, b) == (&id(1), &id(2)) || (a, b) == (&id(2), &id(1)) {
                Err(GitProblem::Incomplete)
            } else {
                Ok(true)
            }
        },
        |_, _| Ok(1),
    )
    .unwrap_err();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .all(|p| matches!(p, ChainProblem::Unknown { .. })),
        "{found:?}"
    );
}
