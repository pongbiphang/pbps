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
pub fn finish<S, T>(
    before: &super::PlanningPlan<T>,
    mut after: super::PlanningPlan<S>,
    describe: impl Fn(&pbps_model::Change) -> String,
) -> Result<super::PlanningPlan<S>, String> {
    let edges = remap(before, &after, before.proof().edges(), describe)?;
    if let Some(identities) = &before.identities {
        // Reuse the same equal-change ordinal matching as the proof remap.
        // Scope tails and woven additions keep their actual planning sources.
        let mut matched = vec![None; after.payload.changes.len()];
        for (position, planned) in before.payload.changes.iter().enumerate() {
            let found = after
                .payload
                .changes
                .iter()
                .enumerate()
                .position(|(i, now)| {
                    matched[i].is_none() && same_change(&planned.change, &now.change)
                })
                .expect("the proof remap already matched every operation");
            matched[found] = Some(position);
        }
        let extras = identities.additional(
            matched
                .iter()
                .enumerate()
                .filter(|(_, from)| from.is_none())
                .map(|(i, _)| (after.origins[i].clone(), after.payload.changes[i].clone())),
            pbps_model::operation::ModuleEdits::InPlace,
        );
        let mut extra = extras.iter();
        let operations =
            pbps_model::operation::Operations::from_operations(matched.into_iter().map(|from| {
                from.map_or_else(
                    || {
                        extra
                            .next()
                            .expect("every unmatched operation has a fresh identity")
                            .clone()
                    },
                    |from| identities.as_slice()[from].clone(),
                )
            }))
            .with_steps(&after.payload.changes);
        after.origins = operations
            .iter()
            .map(|operation| operation.origin().clone())
            .collect();
        after.identities = Some(operations);
    }
    after.proof = pbps_model::resolver::OrderingProof::new(&after, edges)
        .map_err(|error| format!("the final order: {error}"))?;
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::operation::{ModuleEdits, Operations, Origin, Source};
    use pbps_model::{Change, ChangeSet, PlannedChange};
    #[test]
    fn scope_tails_and_woven_additions_keep_resolver_ids_without_collisions() {
        let column = pbps_model::TableName::new("app", "t").column("n");
        let default = |value: &str| {
            PlannedChange::new(Change::AlterColumnDefault {
                uid: "c_aaaaaa".parse().unwrap(),
                column: column.clone(),
                from: None,
                to: Some(value.into()),
            })
        };
        let origin = Origin::Restoration(Source::Rebinding(
            pbps_model::resolver::Surface::Default(column.clone()),
        ));
        let mut before = crate::order::propose(ChangeSet {
            changes: vec![default("1")],
        });
        before.logical = 0;
        before.origins = vec![origin.clone()];
        before.identities = Some(Operations::expanded(
            0,
            [(origin.clone(), default("1"))],
            ModuleEdits::Rebuilt,
        ));
        let before = crate::order::plan::canonical(before, &pbps_dialect::MinimalDialect);
        let scope = PlannedChange::new(Change::SetDataMode {
            table: column.table.clone(),
            from: None,
            to: Some(pbps_model::data::DataMode::Exact),
        });
        let mut after = crate::order::writebacks::ScopeInput::new(vec![scope])
            .unwrap()
            .append_to(before.clone(), &pbps_dialect::MinimalDialect);
        after.payload.changes.insert(0, default("2"));
        after.origins.insert(0, origin.clone());
        after.invalidate();
        let after = finish(&before, after, |change| change.subject()).unwrap();
        let operations = after.identities.as_ref().unwrap();
        let ids: Vec<_> = operations
            .iter()
            .map(|operation| operation.id().to_string())
            .collect();
        assert_eq!(ids, ["0.1", "0.0", "1.0"]);
        assert_eq!(
            operations
                .iter()
                .map(|op| op.id())
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        assert_eq!(
            operations.as_slice()[1].id(),
            before.identities.as_ref().unwrap().as_slice()[0].id()
        );
        assert_eq!(operations.as_slice()[2].origin(), &Origin::Whole(0));
        assert_eq!(
            operations.iter().map(|op| op.planned()).collect::<Vec<_>>(),
            after.payload.changes.iter().collect::<Vec<_>>()
        );
    }
}
