//! SQL-free auxiliary nodes in scratch's one settlement (DEC-2376.1).
use super::Prepared;
use crate::order::{Refusal, Schedule, Source};
use pbps_model::escape::Escaped;
use pbps_model::resolver::OrderReason;

/// A stable identity in the input blueprint, including operations that the
/// engine skips. An auxiliary identity is its position in the supplied list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Operation(usize),
    Auxiliary(usize),
}

#[derive(Debug, Clone)]
pub struct Auxiliary<M> {
    pub name: String,
    pub after: Vec<Node>,
    pub before: Vec<Node>,
    /// Logical facts only. SQL is lowered by the engine after settlement.
    pub metadata: M,
}

/// A managed reference addresses the settled parts / lowered groups. It is
/// not an operation identity for an auxiliary node. Its interleaved position
/// supplies hard predecessors and successors to managed trial compilation;
/// auxiliary execution is never retried by that trial by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<M> {
    Managed { part: usize },
    Auxiliary { metadata: M },
}

pub(super) fn settle<M: Clone>(
    prepared: &Prepared,
    at: &[Option<usize>],
    auxiliary: &[Auxiliary<M>],
    dropped: &mut Vec<super::DroppedGuess>,
) -> Result<(Vec<usize>, Vec<Step<M>>), String> {
    let count = auxiliary.len();
    let source = &prepared.changes;
    // Ready auxiliary nodes precede ready managed work. Thus a node sinks
    // exactly after its last prerequisite, without a second placement pass.
    let mut schedule = Schedule::new(count + source.changes.len());
    for edge in &prepared.edges {
        schedule.edge(
            edge.before + count,
            edge.after + count,
            edge.source,
            edge.reason,
        );
    }
    let position = |node: Node| match node {
        Node::Operation(i) if i < source.changes.len() => Some(count + i),
        Node::Auxiliary(i) if i < count => Some(i),
        Node::Operation(_) | Node::Auxiliary(_) => None,
    };
    for (i, node) in auxiliary.iter().enumerate() {
        for (other, after) in node
            .after
            .iter()
            .map(|n| (n, true))
            .chain(node.before.iter().map(|n| (n, false)))
        {
            let other = position(*other).ok_or_else(|| {
                format!(
                    "auxiliary `{}` names a missing prerequisite or consumer {other:?}",
                    Escaped(&node.name)
                )
            })?;
            let (before, after) = if after { (other, i) } else { (i, other) };
            schedule.edge(before, after, Source::Execution, OrderReason::Structural);
        }
    }
    let name = |i: usize| {
        if i < count {
            format!("`{}`", Escaped(&auxiliary[i].name))
        } else {
            format!("`{}`", Escaped(&source.changes[i - count].change.subject()))
        }
    };
    let sequence = if count == 0 {
        // The original managed route must retain the scheduler's provenance
        // in prepared operation coordinates (DEC-2465.3).
        let names = pbps_model::ChangeSet {
            changes: source
                .changes
                .iter()
                .map(|p| pbps_model::PlannedChange::new(p.change.clone()))
                .collect(),
        };
        schedule
            .settle_scratch(names, dropped)
            .map(|settled| settled.sequence)
    } else {
        schedule.sequence()
    }
    .map_err(|error| {
        if count == 0 {
            let names = pbps_model::ChangeSet {
                changes: source
                    .changes
                    .iter()
                    .map(|p| pbps_model::PlannedChange::new(p.change.clone()))
                    .collect(),
            };
            return error.describe(&names);
        }
        match &error {
            Refusal::Cycle { members, .. } => format!(
                "these scratch steps wait on one another: {}",
                members
                    .iter()
                    .map(|&i| name(i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Refusal::Outside { .. }
            | Refusal::Mismatch { .. }
            | Refusal::Unmet(_)
            | Refusal::Kept(_) => error.to_string(),
        }
    })?;
    let mut managed = Vec::new();
    let mut steps = Vec::new();
    let mut part = 0;
    for i in sequence {
        if i < count {
            steps.push(Step::Auxiliary {
                metadata: auxiliary[i].metadata.clone(),
            });
        } else {
            let operation = i - count;
            managed.push(operation);
            if at[operation].is_some() {
                steps.push(Step::Managed { part });
                part += 1;
            }
        }
    }
    Ok((managed, steps))
}
