//! Connected role-drop ordering, within the places those drops already hold
//! (DEC-1959.1). No expansion and no movement of another kind of change.

use std::collections::BTreeMap;

use pbps_model::resolver::OrderReason;
use pbps_model::{Change, ChangeSet};

use super::{Schedule, Source};

/// Orders drops whose memberships the caller has read and written into them.
/// The role-only projection lets `settle` reorder them without carrying any
/// non-role step across a drop, even when their slots are interleaved.
pub fn role_drops(mut changes: ChangeSet) -> Result<ChangeSet, String> {
    let slots: Vec<_> = changes
        .changes
        .iter()
        .enumerate()
        .filter_map(|(i, p)| matches!(p.change, Change::DropRole { .. }).then_some(i))
        .collect();
    let drops = ChangeSet {
        changes: slots.iter().map(|&i| changes.changes[i].clone()).collect(),
    };
    let schedule = constraints(&drops);
    let settled = schedule
        .settle(drops.clone())
        .map_err(|refusal| refusal.describe(&drops))?;
    for (slot, drop) in slots.into_iter().zip(settled.changes.changes) {
        changes.changes[slot] = drop;
    }
    Ok(changes)
}

fn constraints(drops: &ChangeSet) -> Schedule {
    let mut schedule = Schedule::new(drops.changes.len());
    let depth = crate::schema_diff::member_depth(&drops.changes);
    let mut by_name = BTreeMap::new();
    let mut levels = BTreeMap::<_, Vec<_>>::new();
    for (i, p) in drops.changes.iter().enumerate() {
        let Change::DropRole { name, .. } = &p.change else {
            unreachable!("the projection contains only role drops");
        };
        by_name.insert(name, i);
        levels.entry(depth[name]).or_default().push(i);
    }
    for (i, p) in drops.changes.iter().enumerate() {
        let Change::DropRole { members, .. } = &p.change else {
            unreachable!("the projection contains only role drops");
        };
        for member in members {
            if let Some(&j) = by_name.get(member) {
                schedule.edge(i, j, Source::Catalog, OrderReason::Authorization);
            }
        }
    }
    let mut previous = None;
    for level in levels.values() {
        // Holder edges alone may let one branch's child precede another
        // branch's root. The last peer of one layer precedes the first of
        // the next; peer chains make that a complete barrier with one edge.
        if let Some(above) = previous {
            schedule.edge(above, level[0], Source::Catalog, OrderReason::Authorization);
        }
        // State stable peer order as constraints, not a pre-sort of the
        // operations. These chains make same-level Keep entries redundant
        // and avoid edges between every pair of adjacent-layer roles.
        for pair in level.windows(2) {
            schedule.edge(
                pair[0],
                pair[1],
                Source::Catalog,
                OrderReason::Authorization,
            );
        }
        previous = level.last().copied();
    }
    schedule
}

#[cfg(test)]
#[path = "../../tests/support/role_order.rs"]
pub(crate) mod legacy;

#[cfg(test)]
mod tests;
