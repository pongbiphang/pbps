//! The rule driver: expansions, and the loop that runs one (#1776 planning
//! contract §4, slice S6b, #1958).
//!
//! An [`Expansion`] names the obligations a plan leaves unmet and answers
//! each with a [`Rewrite`]: steps added at an anchor, a whole step split into
//! parts, and the edges that order them. [`expand`] applies one rewrite at a
//! time and orders the plan through [`Schedule::settle`] after each, so no
//! rule moves a step itself. After every rewrite it checks:
//!
//! - the transformation contract: every logical change of the plan it was
//!   given is still there, whole or as its parts, and an added step serves
//!   one of them;
//! - targeted progress: the obligation the rewrite named is no longer unmet.
//!   A rewrite may leave others of its own;
//! - the budget: [`FACTOR`] rewrites per operation the plan asks for
//!   (DEC-1958.1), exhausted only by a rule that does not converge.
//!
//! A rewrite that adds nothing ends the loop with a refusal naming what is
//! left; a plan with nothing unmet ends it as a success, once the final
//! order over [`Expansion::order`]'s edges still leaves nothing unmet. The added steps
//! carry their [`Provenance`] for the plan output, never sealed (DEC-1958.3).
//!
//! An expansion's return type has no whole step: it cannot turn a rename into
//! a drop and a create, or add a logical change of its own.

use std::fmt;

use pbps_model::operation::{Origin, Source as LogicalSource};
use pbps_model::resolver::OrderReason;
use pbps_model::{AddedRole, ChangeSet, PlannedChange, Provenance};

use super::edges::Source;
use super::schedule::Schedule;

/// Rewrites allowed per operation the plan asks for (DEC-1958.1).
pub const FACTOR: usize = 2;

/// A step a rewrite names: one the plan has now, by its position, or one the
/// same rewrite adds, by its index in [`Rewrite::Add::added`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Plan(usize),
    Added(usize),
}

/// Where an added step enters the plan before `settle` orders it: next to a
/// step the plan has now. Several anchored to one side of the same step keep
/// the order they were added in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Before(usize),
    After(usize),
}

/// A step an expansion adds, for the logical change of the step at `of`.
/// There is no whole variant (#1776).
#[derive(Debug, Clone)]
pub enum Added {
    Teardown(Addition),
    Restoration(Addition),
}

#[derive(Debug, Clone)]
pub struct Addition {
    /// The step, by its position now, whose logical change this one serves.
    pub of: usize,
    pub change: PlannedChange,
    pub anchor: Anchor,
    pub reason: String,
}

/// One edge of a rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub before: Step,
    pub after: Step,
    pub source: Source,
    pub reason: OrderReason,
}

/// What an expansion answers one obligation with.
#[derive(Debug, Clone)]
pub enum Rewrite {
    /// The whole step at `at` replaced, in place, by at least two parts (DEC-1959.2).
    Split {
        at: usize,
        parts: Box<[PlannedChange; 2]>,
        /// Extra parts of the same logical change, in declaration order.
        tail: Vec<PlannedChange>,
        reason: String,
    },
    /// Steps added, steps respelled for where the edges take them, and the
    /// edges.
    Add {
        added: Vec<Added>,
        /// A step the plan has, spelled again with the names in force where
        /// it is going. Its change keeps its kind.
        respelled: Vec<(usize, PlannedChange)>,
        edges: Vec<Order>,
    },
}

impl Rewrite {
    fn is_empty(&self) -> bool {
        match self {
            Rewrite::Split { .. } => false,
            Rewrite::Add {
                added,
                respelled,
                edges,
            } => added.is_empty() && respelled.is_empty() && edges.is_empty(),
        }
    }
}

/// A rule that adds the steps an obligation needs.
pub trait Expansion {
    type Obligation: Clone + PartialEq + fmt::Display;

    /// The rule's name, as the plan output reports it.
    fn rule(&self) -> &'static str;

    /// What the plan leaves unmet, in the order the rule answers it: derived
    /// from scratch, from the plan as it stands.
    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<Self::Obligation>, String>;

    /// The rewrite that meets `obligation`, or a refusal of the plan.
    fn expand(
        &self,
        plan: &[PlannedChange],
        obligation: &Self::Obligation,
    ) -> Result<Rewrite, String>;

    /// Bind input address versions after this rewrite's names are selected.
    /// This grants no ordering or execution authority.
    fn bind_inputs(
        &self,
        operations: &pbps_model::operation::Operations,
    ) -> Result<pbps_model::operation::Operations, String> {
        Ok(operations.clone())
    }

    /// The edges the finished plan is settled over, from scratch.
    fn order(&self, plan: &[PlannedChange]) -> Result<Vec<Order>, String>;
}

/// A plan an expansion has finished.
#[derive(Debug, Clone)]
pub struct Expanded {
    pub changes: super::PlanningPlan,
    pub rewrites: usize,
}

/// Runs `expansion` over `plan` until nothing is unmet, with `factor`
/// rewrites per operation the plan asks for: [`FACTOR`] outside the tests.
pub fn expand<E: Expansion>(
    plan: super::PlanningPlan,
    expansion: &E,
    factor: usize,
) -> Result<Expanded, String> {
    let mut input = plan.clone();
    let logical: Vec<String> = plan.changes.iter().map(|p| p.change.subject()).collect();
    let mut state = State {
        origins: (0..plan.changes.len()).map(Origin::Whole).collect(),
        changes: plan.payload.changes,
        proof: input.proof.clone(),
    };
    // The operations the plan asks for: its own, and one per obligation it
    // states before any rewrite. Weave asks for a teardown or a restoration
    // per dependent, which a plan of one rebuilt function can have dozens of
    // (DEC-1958.1).
    let asked = state.changes.len() + expansion.unmet(&state.changes)?.len();
    let budget = factor * asked.max(1);
    let mut rewrites = 0;
    loop {
        let unmet = expansion.unmet(&state.changes)?;
        let Some(first) = unmet.first() else {
            break;
        };
        if rewrites == budget {
            return Err(format!(
                "{}: the rewrite budget of {budget} ({factor} per operation, {asked} asked for) \
                 is spent and {} remain unmet: {}",
                expansion.rule(),
                unmet.len(),
                list(&unmet)
            ));
        }
        let rewrite = expansion.expand(&state.changes, first)?;
        if rewrite.is_empty() {
            return Err(format!(
                "{}: nothing meets {}",
                expansion.rule(),
                list(&unmet)
            ));
        }
        state.apply(rewrite, expansion.rule(), &logical)?;
        rewrites += 1;
        if expansion.unmet(&state.changes)?.contains(first) {
            return Err(format!(
                "{}: a rewrite for {first} left it unmet",
                expansion.rule()
            ));
        }
    }
    let order = expansion.order(&state.changes)?;
    let n = state.changes.len();
    state.settle(&order, n)?;
    // The final order may move a step past one that met an obligation by its
    // position; a plan returned as expanded meets every one in the order it
    // is returned in.
    let left = expansion.unmet(&state.changes)?;
    if !left.is_empty() {
        return Err(format!(
            "{}: the final order leaves {} unmet: {}",
            expansion.rule(),
            left.len(),
            list(&left)
        ));
    }
    let retained: Vec<_> = if input.identities.is_none() {
        Vec::new()
    } else {
        state
            .origins
            .iter()
            .map(|origin| {
                if let Origin::Whole(at) = origin {
                    Some(*at)
                } else {
                    None
                }
            })
            .collect()
    };
    let mut changes = super::PlanningPlan::new(
        ChangeSet {
            changes: state.changes,
        },
        state
            .origins
            .into_iter()
            .map(|origin| match origin {
                Origin::Whole(at) => input.origins[at].clone(),
                Origin::SplitPart(at) => match input.origins[at].source() {
                    LogicalSource::Change(at) => Origin::SplitPart(at),
                    LogicalSource::Rebinding(_) => input.origins[at].clone(),
                },
                Origin::Teardown(LogicalSource::Change(at)) => {
                    Origin::Teardown(input.origins[at].source())
                }
                Origin::Restoration(LogicalSource::Change(at)) => {
                    Origin::Restoration(input.origins[at].source())
                }
                origin @ (Origin::Teardown(LogicalSource::Rebinding(_))
                | Origin::Restoration(LogicalSource::Rebinding(_))) => origin,
            })
            .collect(),
        input.logical,
        state.proof,
    );
    if let Some(identities) = &input.identities {
        input.allocator.reserve(identities);
        let added = input.allocator.allocate(
            retained
                .iter()
                .enumerate()
                .filter(|(_, prior)| prior.is_none())
                .map(|(at, _)| {
                    (
                        changes.origins[at].clone(),
                        changes.payload.changes[at].clone(),
                    )
                }),
            if input.boundaries.is_empty() {
                pbps_model::operation::ModuleEdits::InPlace
            } else {
                input.boundaries.module_edits()
            },
        );
        let mut added = added.iter().cloned();
        let operations =
            pbps_model::operation::Operations::from_operations(retained.iter().map(|prior| {
                prior.map_or_else(
                    || added.next().expect("every new step has a fresh identity"),
                    |at| identities.as_slice()[at].clone(),
                )
            }));
        changes.identities =
            Some(expansion.bind_inputs(&operations.with_steps(&changes.payload.changes))?);
    }
    changes.boundaries = input.boundaries;
    changes.allocator = input.allocator;
    changes.annotations = input.annotations;
    Ok(Expanded { changes, rewrites })
}

fn list<T: fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

struct State {
    changes: Vec<PlannedChange>,
    /// Beside each step, what it is to the plan's logical changes.
    origins: Vec<Origin>,
    proof: pbps_model::resolver::OrderingProof,
}

impl State {
    fn apply(
        &mut self,
        rewrite: Rewrite,
        rule: &'static str,
        logical: &[String],
    ) -> Result<(), String> {
        let n = self.changes.len();
        match rewrite {
            Rewrite::Split {
                at,
                parts,
                tail,
                reason,
            } => {
                let Some(Origin::Whole(i)) = self.origins.get(at).cloned() else {
                    return Err(format!("{rule}: only a whole step splits, not step {at}"));
                };
                let count = parts.len() + tail.len();
                let parts = (*parts).into_iter().chain(tail).map(|mut p| {
                    p.provenance = Some(Provenance {
                        rule,
                        role: AddedRole::SplitPart,
                        source: logical[i].clone(),
                        reason: reason.clone(),
                    });
                    p
                });
                self.changes.splice(at..=at, parts);
                self.origins
                    .splice(at..=at, std::iter::repeat_n(Origin::SplitPart(i), count));
            }
            Rewrite::Add {
                added,
                respelled,
                edges,
            } => {
                for (at, change) in respelled {
                    let Some(old) = self.changes.get_mut(at) else {
                        return Err(format!("{rule}: respells step {at} of {n}"));
                    };
                    if std::mem::discriminant(&old.change) != std::mem::discriminant(&change.change)
                    {
                        return Err(format!(
                            "{rule}: respelling `{}` changed what kind of step it is",
                            pbps_model::escape::Escaped(&old.change.subject())
                        ));
                    }
                    *old = change;
                }
                // Where each step goes: the plan's in order, an added one
                // beside its anchor.
                let mut placed: Vec<Option<(PlannedChange, Origin)>> = Vec::new();
                let mut at_plan = vec![0usize; n];
                let mut at_added = vec![0usize; added.len()];
                let mut before: Vec<Vec<usize>> = vec![Vec::new(); n];
                let mut after: Vec<Vec<usize>> = vec![Vec::new(); n];
                let mut additions = Vec::with_capacity(added.len());
                for (k, a) in added.into_iter().enumerate() {
                    let (role, a) = match a {
                        Added::Teardown(a) => (AddedRole::Teardown, a),
                        Added::Restoration(a) => (AddedRole::Restoration, a),
                    };
                    let source = match self.origins.get(a.of) {
                        Some(o) => o.source(),
                        None => {
                            return Err(format!(
                                "{rule}: an added step serves step {} of {n}",
                                a.of
                            ));
                        }
                    };
                    let LogicalSource::Change(i) = source else {
                        return Err(format!("{rule}: an added step serves no logical change"));
                    };
                    let (side, anchor) = match a.anchor {
                        Anchor::Before(j) => (&mut before, j),
                        Anchor::After(j) => (&mut after, j),
                    };
                    let Some(slot) = side.get_mut(anchor) else {
                        return Err(format!("{rule}: an anchor at step {anchor} of {n}"));
                    };
                    slot.push(k);
                    let origin = if role == AddedRole::Teardown {
                        Origin::Teardown(LogicalSource::Change(i))
                    } else {
                        Origin::Restoration(LogicalSource::Change(i))
                    };
                    let mut change = a.change;
                    change.provenance = Some(Provenance {
                        rule,
                        role,
                        source: logical[i].clone(),
                        reason: a.reason,
                    });
                    additions.push(Some((change, origin)));
                }
                let mut plan: Vec<Option<(PlannedChange, Origin)>> =
                    std::mem::take(&mut self.changes)
                        .into_iter()
                        .zip(std::mem::take(&mut self.origins))
                        .map(Some)
                        .collect();
                for j in 0..n {
                    for &k in &before[j] {
                        at_added[k] = placed.len();
                        placed.push(additions[k].take());
                    }
                    at_plan[j] = placed.len();
                    placed.push(plan[j].take());
                    for &k in &after[j] {
                        at_added[k] = placed.len();
                        placed.push(additions[k].take());
                    }
                }
                let (changes, origins): (Vec<_>, Vec<_>) = placed
                    .into_iter()
                    .map(|p| p.expect("each step placed once"))
                    .unzip();
                self.changes = changes;
                self.origins = origins;
                let position = |s: Step| match s {
                    Step::Plan(j) => at_plan.get(j).copied(),
                    Step::Added(k) => at_added.get(k).copied(),
                };
                let mut mapped = Vec::with_capacity(edges.len());
                for e in edges {
                    let (Some(before), Some(after)) = (position(e.before), position(e.after))
                    else {
                        return Err(format!(
                            "{rule}: an edge names a step the rewrite does not have"
                        ));
                    };
                    mapped.push(Order {
                        before: Step::Plan(before),
                        after: Step::Plan(after),
                        source: e.source,
                        reason: e.reason,
                    });
                }
                let n = self.changes.len();
                self.settle(&mapped, n)?;
            }
        }
        self.contract(rule, logical.len())
    }

    /// Orders the steps over `edges`, all of them [`Step::Plan`].
    fn settle(&mut self, edges: &[Order], n: usize) -> Result<(), String> {
        let mut schedule = Schedule::new(n);
        for e in edges {
            let (Step::Plan(before), Step::Plan(after)) = (e.before, e.after) else {
                return Err("an edge to settle names an added step".to_owned());
            };
            schedule.edge(before, after, e.source, e.reason);
        }
        let changes = ChangeSet {
            changes: std::mem::take(&mut self.changes),
        };
        let settled = schedule
            .settle_raw(changes.clone())
            .map_err(|r| r.describe(&changes))?;
        let mut origins: Vec<Option<Origin>> = std::mem::take(&mut self.origins)
            .into_iter()
            .map(Some)
            .collect();
        self.origins = settled
            .sequence
            .iter()
            .map(|&i| origins[i].take().expect("a sequence is a permutation"))
            .collect();
        self.changes = settled.changes.changes;
        self.proof = settled.proof;
        Ok(())
    }

    /// Every logical change is still there, whole or as its parts.
    fn contract(&self, rule: &str, logical: usize) -> Result<(), String> {
        let mut kept = vec![false; logical];
        for o in &self.origins {
            if let Origin::Whole(i) | Origin::SplitPart(i) = o
                && let Some(k) = kept.get_mut(*i)
            {
                *k = true;
            }
        }
        match kept.iter().position(|k| !k) {
            Some(i) => Err(format!("{rule}: the plan's change {i} is no longer in it")),
            None => Ok(()),
        }
    }
}
