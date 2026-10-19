#![cfg(test)]
//! The frozen pre-S7 role ordering, shared by unit and connected flow tests.
//! Kept out of every production build (DEC-1959.1).

use pbps_model::{Change, ChangeSet, PlannedChange};
use std::collections::BTreeMap;

#[allow(clippy::wildcard_enum_match_arm)]
fn member_depth(planned: &[PlannedChange]) -> BTreeMap<pbps_model::RoleName, usize> {
    let dropped: BTreeMap<&pbps_model::RoleName, &[pbps_model::RoleName]> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropRole { name, members, .. } => Some((name, members.as_slice())),
            _ => None,
        })
        .collect();
    let mut depth: BTreeMap<pbps_model::RoleName, usize> =
        dropped.keys().map(|n| ((*n).to_owned(), 0)).collect();
    for _ in 0..dropped.len() {
        let mut moved = false;
        for (holder, members) in &dropped {
            let above = depth[*holder];
            for member in *members {
                if let Some(d) = depth.get_mut(member)
                    && *d <= above
                {
                    *d = above + 1;
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
    depth
}

pub fn legacy_order(cs: &mut ChangeSet) {
    let depth = member_depth(&cs.changes);
    let slots: Vec<usize> = cs
        .changes
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.change, Change::DropRole { .. }))
        .map(|(i, _)| i)
        .collect();
    let mut drops: Vec<PlannedChange> = slots.iter().rev().map(|&i| cs.changes.remove(i)).collect();
    drops.reverse();
    // Stable: two drops at one depth keep the order the differ gave them.
    drops.sort_by_key(|p| {
        let Change::DropRole { name, .. } = &p.change else {
            return 0;
        };
        depth.get(name).copied().unwrap_or(0)
    });
    for (slot, drop) in slots.into_iter().zip(drops) {
        cs.changes.insert(slot, drop);
    }
}

/// Equal plan bytes must keep their checksum too, with all other fields fixed.
pub fn checksum(changes: &ChangeSet) -> String {
    pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "mssql",
        "2026-10-19T00:00:00Z",
        pbps_model::PlanBaseline {
            description: "role-order oracle".into(),
            checksum: "baseline".into(),
            database_collation: None,
            time_zone: None,
        },
        changes.clone(),
        Default::default(),
    )
    .checksum()
}
