#![cfg(test)]
//! Frozen 557d4984 scratch function hold; no production ordering uses it.
use super::*;

pub(super) fn after_their_functions(
    steps: Vec<Step>,
    late: Vec<Late>,
    functions: &[(usize, String)],
    foreign: &BTreeSet<usize>,
) -> Result<Vec<Step>, ReconstructError> {
    let late: Vec<(Late, Vec<usize>)> = late
        .into_iter()
        .filter_map(|table| {
            let called: Vec<usize> = functions
                .iter()
                .filter(|(_, name)| {
                    table
                        .generated
                        .iter()
                        .any(|text| crate::generated::may_call(text, name))
                })
                .map(|(at, _)| *at)
                .collect();
            (!called.is_empty()).then_some((table, called))
        })
        .collect();
    if late.is_empty() {
        return Ok(steps);
    }
    // A literal counts: an OID-alias literal such as `'app.n'::regclass`
    // names the table to the engine as the step compiles (DEC-1364.1).
    let names = |at: usize, name: &str| {
        steps[at]
            .statements
            .iter()
            .any(|sql| crate::generated::may_call(sql, name))
    };
    let early = |at: usize| matches!(steps[at].phase, Phase::Tables | Phase::Keys);
    // A foreign key naming a late table waits for the end of the modules:
    // it names only tables, and every table exists by then.
    let keys: Vec<usize> = foreign
        .iter()
        .copied()
        .filter(|&at| late.iter().any(|(table, _)| names(at, &table.table.name)))
        .collect();
    let mut modules: Vec<usize> = (0..steps.len())
        .filter(|&at| steps[at].phase == Phase::Modules)
        .collect();
    // Steps already moved for an earlier table. A table that names one of
    // them, such as by an OID-alias literal in its generated column, follows
    // it as it follows its own functions.
    let mut delayed: BTreeSet<usize> = BTreeSet::new();
    for (table, called) in &late {
        let group: Vec<usize> = table
            .steps
            .clone()
            .filter(|&at| early(at) && !keys.contains(&at))
            .collect();
        let point = modules
            .iter()
            .rposition(|at| {
                called.contains(at)
                    || (delayed.contains(at)
                        && steps[*at]
                            .names
                            .iter()
                            .any(|(_, _, name)| group.iter().any(|&own| names(own, name))))
            })
            .map_or(0, |position| position + 1);
        delayed.extend(group.iter().copied());
        let mut held = BTreeSet::from([table.table.name.clone()]);
        let mut kept = Vec::new();
        let mut moved = Vec::new();
        for &at in &modules[..point] {
            if held.iter().any(|name| {
                if name == &table.table.name {
                    steps[at]
                        .statements
                        .iter()
                        .any(|sql| crate::generated::names_table(sql, &table.table))
                } else {
                    names(at, name)
                }
            }) {
                if called.contains(&at) {
                    return Err(ReconstructError::Cycle(format!(
                        "table {} ({})",
                        table.table.shown(),
                        steps[at].declaration
                    )));
                }
                held.extend(steps[at].names.iter().map(|(_, _, name)| name.clone()));
                moved.push(at);
            } else {
                kept.push(at);
            }
        }
        kept.extend(group);
        kept.extend(moved);
        kept.extend_from_slice(&modules[point..]);
        modules = kept;
    }
    modules.extend(keys);
    let placed: BTreeSet<usize> = modules.iter().copied().collect();
    let order: Vec<usize> = (0..steps.len())
        .filter(|at| !placed.contains(at) && early(*at))
        .chain(modules)
        .chain((0..steps.len()).filter(|&at| steps[at].phase == Phase::Expressions))
        .collect();
    let mut taken: Vec<Option<Step>> = steps.into_iter().map(Some).collect();
    Ok(order
        .into_iter()
        .map(|at| {
            let mut step = taken[at].take().expect("each step is placed once");
            if placed.contains(&at) {
                step.phase = Phase::Modules;
            }
            step
        })
        .collect())
}

// Frozen dd84f894 metadata permutation after raw lowering, retained only to
// compare every final Step field with the blueprint-before-lowering route.
pub(super) fn settle_steps(
    steps: Vec<Step>,
    schedule: &pbps_diff::order::Schedule,
    expanded: &pbps_model::ChangeSet,
) -> Result<Vec<Step>, ReconstructError> {
    let order_plan = pbps_model::ChangeSet {
        changes: steps
            .iter()
            .map(|s| pbps_model::PlannedChange::new(expanded.changes[s.operation].change.clone()))
            .collect(),
    };
    let settled = schedule
        .settle(pbps_diff::order::propose(order_plan.clone()))
        .map_err(|e| ReconstructError::Cycle(e.describe(&order_plan)))?;
    let mut taken: Vec<_> = steps.into_iter().map(Some).collect();
    Ok(settled
        .sequence
        .iter()
        .map(|&i| taken[i].take().expect("a settled order is a permutation"))
        .collect())
}
