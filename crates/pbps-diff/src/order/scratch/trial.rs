//! Order-owned readiness over immutable scratch groups (DEC-2465.1).
use super::{Constraint, Engine, Schedule};
use crate::order::Source;
use pbps_model::ChangeSet;
use std::{collections::BTreeSet, marker::PhantomData, ops::Range};

/// A real engine's opaque blueprint is the only production constructor.
/// This grants no PlanView and offers no caller-supplied permutation.
///
/// ```compile_fail
/// fn arbitrary<E: pbps_diff::order::scratch::Engine>(p: &pbps_diff::order::scratch::TrialProgram<E>) {
///     p.start(0..2, &[1, 0]).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge<E: pbps_diff::order::scratch::Engine>() {
///     let _ = pbps_diff::order::scratch::Progress::<E> { through: 1, brand: std::marker::PhantomData };
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TrialProgram<E: Engine> {
    hard: Vec<Vec<usize>>,
    why: Vec<super::EdgeWhy>,
    dropped: Vec<super::DroppedGuess>,
    sources: Vec<usize>,
    subjects: Vec<String>,
    brand: PhantomData<fn(E) -> E>,
}
impl<E: Engine> TrialProgram<E> {
    pub(super) fn project(
        edges: &[Constraint],
        sources: &[usize],
        names: &ChangeSet,
    ) -> Result<Self, String> {
        let mut schedule = Schedule::new(names.changes.len());
        let mut incoming = vec![Vec::new(); names.changes.len()];
        for edge in edges.iter().filter(|e| e.source != Source::Guess) {
            schedule.edge(edge.before, edge.after, Source::Lexical, edge.reason);
            if edge.before >= incoming.len() || edge.after >= incoming.len() {
                return Err("scratch hard edge names an absent operation".into());
            }
            incoming[edge.after].push(edge.before);
        }
        schedule
            .settle_raw(names.clone())
            .map_err(|e| e.describe(names))?;
        let mut ranks = vec![None; incoming.len()];
        for (rank, &source) in sources.iter().enumerate() {
            ranks[source] = Some(rank);
        }
        let mut hard = Vec::new();
        for &source in sources {
            let mut pending = incoming[source].clone();
            let mut seen = BTreeSet::new();
            let mut predecessors = BTreeSet::new();
            while let Some(before) = pending.pop() {
                if !seen.insert(before) {
                    continue;
                }
                if let Some(rank) = ranks[before] {
                    predecessors.insert(rank);
                } else {
                    pending.extend(&incoming[before]);
                }
            }
            hard.push(predecessors.into_iter().collect());
        }
        Ok(Self {
            hard,
            why: Vec::new(),
            dropped: Vec::new(),
            sources: Vec::new(),
            subjects: Vec::new(),
            brand: PhantomData,
        })
    }

    /// A remedy must not close a known dependency cycle, even when one of its
    /// legs is only inferred. An uncertain guess may suppress advice, never
    /// justify a hard-cycle remedy.
    pub fn can_precede(&self, before: usize, after: usize) -> bool {
        let mut pending = vec![before];
        let mut seen = BTreeSet::new();
        while let Some(at) = pending.pop() {
            if at == after {
                return false;
            }
            if seen.insert(at) {
                pending.extend(&self.hard[at]);
                if let Some(&operation) = self.sources.get(at) {
                    pending.extend(
                        self.why
                            .iter()
                            .filter(|why| why.edge.after == operation)
                            .filter_map(|why| {
                                self.sources
                                    .iter()
                                    .position(|&source| source == why.edge.before)
                            }),
                    );
                }
            }
        }
        true
    }
    pub(super) fn set_trace(
        &mut self,
        why: Vec<super::EdgeWhy>,
        dropped: Vec<super::DroppedGuess>,
        sources: Vec<usize>,
        names: &ChangeSet,
    ) {
        self.why = why;
        self.dropped = dropped;
        self.sources = sources;
        self.subjects = names
            .changes
            .iter()
            .map(|p| pbps_model::escape::Escaped(&p.change.subject()).to_string())
            .collect();
    }
    pub fn explanation(&self, at: usize) -> Vec<String> {
        let operation = self.sources[at];
        let mut lines = vec![format!(
            "initial rank {}; attempts require every hard predecessor",
            at + 1
        )];
        for why in self.why.iter().filter(|w| w.edge.after == operation) {
            lines.push(format!(
                "{:?} prerequisite from {}",
                why.rule, self.subjects[why.edge.before]
            ));
        }
        for dropped in self
            .dropped
            .iter()
            .filter(|d| d.edge.after == operation || d.edge.before == operation)
        {
            lines.push(format!(
                "guess {} → {} set aside ({} stage); cycle component: {}",
                self.subjects[dropped.edge.before],
                self.subjects[dropped.edge.after],
                if dropped.backward_only {
                    "backward-first"
                } else {
                    "remaining-guesses"
                },
                dropped
                    .members
                    .iter()
                    .map(|&m| self.subjects[m].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }
    /// A range is a blueprint interval; earlier committed ranges are facts,
    /// never permission to draw a provider across a chained-fill boundary.
    pub fn begin(&self) -> Progress<E> {
        Progress {
            through: 0,
            brand: PhantomData,
        }
    }
    pub fn start(
        &self,
        range: Range<usize>,
        committed: &Progress<E>,
    ) -> Result<Trials<'_, E>, String> {
        if range.start != committed.through
            || range.start > range.end
            || range.end > self.hard.len()
        {
            return Err("scratch trial ranges must follow the complete committed prefix".into());
        }
        let n = range.len();
        let budget = n
            .checked_add(1)
            .and_then(|v| n.checked_mul(v))
            .map(|v| v / 2)
            .ok_or("scratch trial attempt budget overflow")?;
        let mut done = vec![false; self.hard.len()];
        done[..range.start].fill(true);
        Ok(Trials {
            program: self,
            range,
            done,
            last: vec![None; self.hard.len()],
            syntax: vec![false; self.hard.len()],
            successes: Vec::with_capacity(n),
            attempts: 0,
            budget,
        })
    }
}

/// Opaque readiness receipt; the executor retains it only after COMMIT.
#[derive(Debug, Clone)]
pub struct Progress<E: Engine> {
    through: usize,
    brand: PhantomData<fn(E) -> E>,
}

/// A range-local cursor. Failed attempts wait for a success generation;
/// ambiguous syntax waits until ordinary candidates are exhausted.
pub struct Trials<'p, E: Engine> {
    program: &'p TrialProgram<E>,
    range: Range<usize>,
    done: Vec<bool>,
    last: Vec<Option<usize>>,
    syntax: Vec<bool>,
    successes: Vec<usize>,
    attempts: usize,
    budget: usize,
}
impl<'p, E: Engine> Trials<'p, E> {
    pub fn next_ready(&mut self) -> Option<Attempt<'_, 'p, E>> {
        if self.attempts == self.budget {
            return None;
        }
        for syntax in [false, true] {
            let candidate = self.range.clone().find(|&at| {
                !self.done[at]
                    && self.syntax[at] == syntax
                    && self.last[at] != Some(self.successes.len())
                    && self.program.hard[at]
                        .iter()
                        .all(|&before| self.done[before])
            });
            if let Some(position) = candidate {
                // Issuing a token consumes this generation even if dropped.
                self.last[position] = Some(self.successes.len());
                self.attempts += 1;
                return Some(Attempt {
                    trials: self,
                    position,
                });
            }
        }
        None
    }
    pub fn finish(self) -> Option<Progress<E>> {
        self.pending().next().is_none().then_some(Progress {
            through: self.range.end,
            brand: PhantomData,
        })
    }
    pub fn pending(&self) -> impl Iterator<Item = usize> + '_ {
        self.range.clone().filter(|&at| !self.done[at])
    }
    pub fn blocked_by(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        self.program.hard[at]
            .iter()
            .copied()
            .filter(|&before| !self.done[before])
    }
}

/// Not constructible or cloneable by a caller. Only the cursor selects a
/// group; reporting its outcome cannot select another group or erase an edge.
pub struct Attempt<'a, 'p, E: Engine> {
    trials: &'a mut Trials<'p, E>,
    position: usize,
}
impl<E: Engine> Attempt<'_, '_, E> {
    pub fn position(&self) -> usize {
        self.position
    }
    pub fn overtaken(&self) -> Vec<String> {
        let operation = self.trials.program.sources[self.position];
        self.trials
            .program
            .why
            .iter()
            .filter(|w| w.edge.after == operation && w.edge.source == Source::Guess)
            .filter(|w| !self.trials.program.dropped.iter().any(|d| d.edge == w.edge))
            .filter(|w| {
                self.trials
                    .program
                    .sources
                    .iter()
                    .position(|&s| s == w.edge.before)
                    .is_some_and(|rank| !self.trials.done[rank])
            })
            .map(|w| self.trials.program.subjects[w.edge.before].clone())
            .collect()
    }
    pub fn is_recovery(&self) -> bool {
        self.trials.syntax[self.position]
    }
    pub fn park_syntax(self) {
        self.trials.syntax[self.position] = true;
    }
    pub fn succeeded(self) {
        self.trials.done[self.position] = true;
        self.trials.successes.push(self.position);
    }
}
