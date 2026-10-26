//! Fixture pins as one ancestry chain that ends at the base (ADR-0019 §4).
//!
//! Two pins on diverging histories have no order, and an arbitrary sort would
//! turn what one branch added into what the other dropped. So the relation is
//! asked of git for each pair, never assumed, and a pair neither of which
//! contains the other is refused by name.

use std::collections::BTreeSet;

use pbps_config::test_manifest::CommitId;

use super::git::GitProblem;

/// The fixtures pinned at one commit, as indices into the manifest's list, in
/// manifest order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinGroup {
    pub commit: CommitId,
    pub fixtures: Vec<usize>,
}

/// One replay jump between neighbouring pins, with its git revision distance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Hop {
    pub from: CommitId,
    pub to: CommitId,
    pub distance: u64,
}

/// Oldest pin first; the last group is the base, with no fixtures when none
/// is pinned there. A manifest without fixtures is just the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayChain {
    pub groups: Vec<PinGroup>,
    pub hops: Vec<Hop>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ChainProblem {
    #[error(
        "fixture pins {a} and {b} are on diverging histories, so neither can be replayed to the \
         other; re-pin one onto a common chain with `pbps test fixtures repin`"
    )]
    Incomparable { a: CommitId, b: CommitId },
    #[error(
        "fixture pin {pin} is a descendant of the base {base}; a pin cannot be newer than the \
         schema it is replayed to"
    )]
    AfterBase { pin: CommitId, base: CommitId },
    #[error("the base {base} is not an ancestor of the candidate commit {tip}")]
    BaseNotBehindCandidate { base: CommitId, tip: CommitId },
    #[error("whether {ancestor} is an ancestor of {descendant} is unknown: {problem}")]
    Unknown {
        ancestor: CommitId,
        descendant: CommitId,
        problem: GitProblem,
    },
}

/// Orders `pins` (one per fixture, manifest order) into a chain ending at
/// `base`, and checks that `base` is behind `tip`: the candidate commit, or
/// the captured HEAD for a working-tree candidate.
///
/// `ancestor(a, b)` says whether `a` is an ancestor of, or equal to, `b`, and
/// `distance(from, to)` counts `from..to`. A failed answer is carried as
/// [`ChainProblem::Unknown`]: a history that cannot be read is never read as
/// "not an ancestor". Every problem found is returned.
pub(crate) fn order(
    base: &CommitId,
    pins: &[CommitId],
    tip: &CommitId,
    mut ancestor: impl FnMut(&CommitId, &CommitId) -> Result<bool, GitProblem>,
    mut distance: impl FnMut(&CommitId, &CommitId) -> Result<u64, GitProblem>,
) -> Result<ReplayChain, Vec<ChainProblem>> {
    let mut problems = Vec::new();
    // Each ordered pair is asked of git once. A failed answer is reported once
    // however many times the pair comes up, and only if it settled nothing.
    let mut known =
        std::collections::BTreeMap::<(CommitId, CommitId), Result<bool, GitProblem>>::new();
    let mut ask = |a: &CommitId, b: &CommitId| -> Result<bool, GitProblem> {
        known
            .entry((a.clone(), b.clone()))
            .or_insert_with(|| ancestor(a, b))
            .clone()
    };
    let mut reported = BTreeSet::<(CommitId, CommitId)>::new();
    let mut unknown = |problems: &mut Vec<ChainProblem>,
                       a: &CommitId,
                       b: &CommitId,
                       answer: Result<bool, GitProblem>| {
        if let Err(problem) = answer
            && reported.insert((a.clone(), b.clone()))
        {
            problems.push(ChainProblem::Unknown {
                ancestor: a.clone(),
                descendant: b.clone(),
                problem,
            });
        }
    };
    match ask(base, tip) {
        Ok(false) => problems.push(ChainProblem::BaseNotBehindCandidate {
            base: base.clone(),
            tip: tip.clone(),
        }),
        Ok(true) => {}
        Err(problem) => unknown(&mut problems, base, tip, Err(problem)),
    }

    let distinct: Vec<&CommitId> = pins
        .iter()
        .chain(std::iter::once(base))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // How many of the others each commit contains; in a chain that is its
    // position.
    let mut behind = vec![0usize; distinct.len()];
    let mut comparable = true;
    for (i, a) in distinct.iter().enumerate() {
        for (j, b) in distinct.iter().enumerate().skip(i + 1) {
            // One direction proving ancestry settles the pair. A shallow
            // clone answers `Incomplete` for a pair it cannot walk in one
            // direction and yet walks in the other, so the first failure
            // must not hide the second question.
            let forward = ask(a, b);
            if forward == Ok(true) {
                behind[j] += 1;
                continue;
            }
            let reverse = ask(b, a);
            if reverse == Ok(true) {
                behind[i] += 1;
                continue;
            }
            comparable = false;
            if forward == Ok(false) && reverse == Ok(false) {
                problems.push(ChainProblem::Incomparable {
                    a: (*a).clone(),
                    b: (*b).clone(),
                });
            } else {
                unknown(&mut problems, a, b, forward);
                unknown(&mut problems, b, a, reverse);
            }
        }
    }
    if !comparable {
        return Err(problems);
    }
    let mut order: Vec<usize> = (0..distinct.len()).collect();
    order.sort_by_key(|&i| behind[i]);
    // The base is the newest, or a pin is newer than the schema it replays to.
    for &i in &order {
        if distinct[i] != base && behind[i] > behind[position(&distinct, base)] {
            problems.push(ChainProblem::AfterBase {
                pin: distinct[i].clone(),
                base: base.clone(),
            });
        }
    }

    let groups: Vec<PinGroup> = order
        .iter()
        .map(|&i| PinGroup {
            commit: distinct[i].clone(),
            fixtures: pins
                .iter()
                .enumerate()
                .filter(|(_, pin)| *pin == distinct[i])
                .map(|(index, _)| index)
                .collect(),
        })
        .collect();
    let mut hops = Vec::new();
    for pair in groups.windows(2) {
        match distance(&pair[0].commit, &pair[1].commit) {
            Ok(distance) => hops.push(Hop {
                from: pair[0].commit.clone(),
                to: pair[1].commit.clone(),
                distance,
            }),
            Err(problem) => problems.push(ChainProblem::Unknown {
                ancestor: pair[0].commit.clone(),
                descendant: pair[1].commit.clone(),
                problem,
            }),
        }
    }
    if problems.is_empty() {
        Ok(ReplayChain { groups, hops })
    } else {
        Err(problems)
    }
}

fn position(commits: &[&CommitId], wanted: &CommitId) -> usize {
    commits
        .iter()
        .position(|c| *c == wanted)
        .expect("the base is among the commits")
}
