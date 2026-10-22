//! The creation boundary shared by ordinary and resolver schedules.

use pbps_model::Change;

/// Policies follow the structural/module/data build; flags follow policies.
/// These are setup phases, not inferred bindings inside predicate text.
pub fn edges<'a>(changes: impl IntoIterator<Item = &'a Change>) -> Vec<(usize, usize)> {
    let changes: Vec<_> = changes.into_iter().collect();
    let policies: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| matches!(c, Change::CreatePolicy { .. }).then_some(i))
        .collect();
    let flags: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| matches!(c, Change::SetRowSecurity { .. }).then_some(i))
        .collect();
    if policies.is_empty() && flags.is_empty() {
        return Vec::new();
    }
    let suppliers: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            (!matches!(
                c,
                Change::CreatePolicy { .. }
                    | Change::SetRowSecurity { .. }
                    | Change::SetDataMode { .. }
                    | Change::SetEnsureKeys { .. }
            ) && (c.table().is_some()
                || matches!(
                    c,
                    Change::CreateModule { .. }
                        | Change::AlterModule { .. }
                        | Change::PublicExecution { .. }
                )))
            .then_some(i)
        })
        .collect();
    let mut out = Vec::new();
    for &later in policies.iter().chain(&flags) {
        out.extend(suppliers.iter().map(|&earlier| (earlier, later)));
    }
    for &flag in &flags {
        out.extend(policies.iter().map(|&policy| (policy, flag)));
    }
    out
}
