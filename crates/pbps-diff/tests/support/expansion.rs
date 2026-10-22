#![cfg(test)]
//! Frozen 160d16f5 expansion driver. The only syntax adaptation accepts the
//! new, empty tail; existing rules must never send it any additional parts.

use super::order::Schedule;
use super::order::expand::{Added, Anchor, Expansion, Order, Rewrite, Step};
use pbps_model::operation::{Origin, Source as LogicalSource};
use pbps_model::{AddedRole, ChangeSet, PlannedChange, Provenance};
use std::fmt;

#[derive(Debug)]
pub struct Expanded {
    pub changes: ChangeSet,
    pub rewrites: usize,
}

pub fn expand<E: Expansion>(
    plan: ChangeSet,
    expansion: &E,
    factor: usize,
) -> Result<Expanded, String> {
    let logical: Vec<String> = plan.changes.iter().map(|p| p.change.subject()).collect();
    let mut state = State {
        origins: (0..plan.changes.len()).map(Origin::Whole).collect(),
        changes: plan.changes,
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
        if match &rewrite {
            Rewrite::Split { .. } => false,
            Rewrite::Add {
                added,
                respelled,
                edges,
            } => added.is_empty() && respelled.is_empty() && edges.is_empty(),
        } {
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
    Ok(Expanded {
        changes: ChangeSet {
            changes: state.changes,
        },
        rewrites,
    })
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
                assert!(tail.is_empty(), "the old driver only knows two parts");
                let Some(Origin::Whole(i)) = self.origins.get(at).cloned() else {
                    return Err(format!("{rule}: only a whole step splits, not step {at}"));
                };
                let parts = (*parts).map(|mut p| {
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
                    .splice(at..=at, [Origin::SplitPart(i), Origin::SplitPart(i)]);
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
                            old.change.subject()
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
            .settle(super::order::propose(changes.clone()))
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
        self.changes = settled.changes.changes.clone();
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
