//! Existing draft ordering and subsumption, owned by order (DEC-1959.5).
use pbps_model::{Change, IndexPart, PlannedChange, TableName};
use std::collections::BTreeSet;

pub(crate) fn sort_draft<K: Ord>(
    planned: &mut [PlannedChange],
    key: impl FnMut(&PlannedChange) -> K,
) {
    planned.sort_by_cached_key(key);
}

#[allow(clippy::wildcard_enum_match_arm)]
pub fn drop_parameter_changes_of_rebuilt_indexes<T>(
    changes: &mut Vec<T>,
    change: impl Fn(&T) -> &Change,
) {
    let rebuilt: BTreeSet<(TableName, IndexPart)> = changes
        .iter()
        .filter_map(|c| match change(c) {
            Change::AddIndex { table, name, .. } => {
                Some((table.clone(), IndexPart::Index(name.clone())))
            }
            Change::AddUnique { table, name, .. } => {
                Some((table.clone(), IndexPart::Unique(name.clone())))
            }
            Change::SetPrimaryKey {
                table, to: Some(_), ..
            } => Some((table.clone(), IndexPart::PrimaryKey)),
            _ => None,
        })
        .collect();
    changes.retain(|c| {
        !matches!(change(c), Change::SetIndexStorageParameters { table, target, .. }
            if rebuilt.contains(&(table.clone(), target.clone())))
    });
}

/// The existing subsumption normalization, coupled to planning origins.
pub fn normalize(plan: &mut super::PlanningPlan) {
    let mut parts: Vec<_> = std::mem::take(&mut plan.payload.changes)
        .into_iter()
        .zip(std::mem::take(&mut plan.origins))
        .collect();
    drop_parameter_changes_of_rebuilt_indexes(&mut parts, |(part, _)| &part.change);
    (plan.payload.changes, plan.origins) = parts.into_iter().unzip();
    plan.invalidate();
}
