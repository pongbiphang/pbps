//! What follows the sort: the rename and column order, the drop requirements, the risks and the label uses.

use super::super::*;

/// What `diff_partial_rebuilding` does after the sort: the rename order and the
/// last-column order, the drop requirements, the risks and the unsafe label uses.
/// It pushes into `errs` in place: the order of `errors` is part of the result.
pub(in super::super) fn finish_sort(
    mut planned: Vec<PlannedChange>,
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    renames: Renames,
    mut errs: Vec<DiffError>,
) -> Diffed {
    for cycle in crate::order::rename_order::order(&mut planned, base, &renames, dialect) {
        errs.push(DiffError::TableRenameCycle { renames: cycle });
    }
    for (table, column, block) in
        crate::order::last_column::order(&mut planned, base, declared, dialect)
    {
        errs.push(DiffError::LastColumnReplacementUnorderable {
            table,
            column,
            block,
        });
    }
    let mut changes = ChangeSet { changes: planned };
    if let Err(reason) = crate::drop_requirements::assign(&mut changes, base.ids, declared.ids)
        .and_then(|()| crate::drop_requirements::validate(&changes, base.ids, declared.ids))
    {
        errs.push(DiffError::DropRequirement { reason });
    }
    // Replacement risk belongs to both policy parts, including in partial
    // reports. Reuse the same UID/name pairing as final plan admission.
    let risks = crate::order::generated::risks(&changes, declared.ids, dialect);
    for (part, risks) in changes.changes.iter_mut().zip(risks) {
        part.risks = risks;
    }
    errs.extend(crate::enum_labels::unsafe_uses(&changes, declared, dialect));
    Diffed {
        changes,
        errors: errs,
    }
}
