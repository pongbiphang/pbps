//! Existing resolver publication compatibility, inside the ordering owner.
use std::collections::BTreeSet;

/// The resolver's edges, from its own order to `after`, the order the
/// caller's passes left. A change is matched by what it does, in order
/// among equal ones ([`same_change`]); a pass may add changes and their
/// findings, but not remove one the resolver ordered or put one before
/// another the resolver ordered after it.
pub fn remap(
    before: &pbps_model::ChangeSet,
    after: &pbps_model::ChangeSet,
    edges: &BTreeSet<pbps_model::resolver::OrderEdge>,
    describe: impl Fn(&pbps_model::Change) -> String,
) -> Result<BTreeSet<pbps_model::resolver::OrderEdge>, String> {
    let mut taken = vec![false; after.changes.len()];
    let mut at = Vec::with_capacity(before.changes.len());
    for planned in &before.changes {
        let Some(found) = after
            .changes
            .iter()
            .enumerate()
            .position(|(i, p)| !taken[i] && same_change(&planned.change, &p.change))
        else {
            return Err(format!(
                "a planning pass removed a change the resolver ordered: {}",
                describe(&planned.change)
            ));
        };
        taken[found] = true;
        at.push(found);
    }
    edges
        .iter()
        .map(|edge| {
            let (before_at, after_at) = (at[edge.before], at[edge.after]);
            if before_at < after_at {
                Ok(pbps_model::resolver::OrderEdge {
                    before: before_at,
                    after: after_at,
                    reason: edge.reason,
                })
            } else {
                Err(format!(
                    "a planning pass put `{}` before `{}`, which the resolver orders after it",
                    describe(&after.changes[after_at].change),
                    describe(&after.changes[before_at].change)
                ))
            }
        })
        .collect()
}

/// Whether `then`, a change the resolver ordered, is `now` after the
/// connected passes. What a change does is all of it, except what the
/// connected plan reads from the target and writes into it: a dropped
/// role's members, which the resolver leaves empty (#1516 review). Every
/// other field is the change's own, and a pass that rewrote one made
/// another change.
fn same_change(then: &pbps_model::Change, now: &pbps_model::Change) -> bool {
    use pbps_model::Change::DropRole;
    if let (
        DropRole { uid, name, members },
        DropRole {
            uid: now_uid,
            name: now_name,
            ..
        },
    ) = (then, now)
    {
        return members.is_empty() && uid == now_uid && name == now_name;
    }
    then == now
}

/// Replace the final intermediate's proof with the resolver's existing
/// ordinal remap. This is the old publication check, not a constraint union
/// or a new ordering stage; the caller retains its run-bound recheck.
pub fn finish(
    before: &super::PlanningPlan,
    mut after: super::PlanningPlan,
    describe: impl Fn(&pbps_model::Change) -> String,
) -> Result<super::PlanningPlan, String> {
    let edges = remap(before, &after, before.proof().edges(), describe)?;
    after.proof = pbps_model::resolver::OrderingProof::new(&after, edges)
        .map_err(|error| format!("the final order: {error}"))?;
    Ok(after)
}
