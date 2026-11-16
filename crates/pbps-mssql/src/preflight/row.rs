//! Probes the row family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    row: &RowChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
) -> Result<Vec<Probe>, DialectError> {
    match row {
        // Inserting and updating a declared row need no probe: the row's whole
        // content is in the plan, and anything the engine refuses about it
        // rolls the plan back.
        RowChange::InsertRow { .. }
        | RowChange::UpdateRow { .. }
        | RowChange::SetDataMode { .. }
        | RowChange::SetEnsureKeys { .. } => Ok(Vec::new()),
        RowChange::DeleteRow {
            table,
            key_column,
            key,
            ..
        } => match names.view().column(&table.column(key_column)) {
            Some(stored) => {
                let mut probes = vec![delete_probe(table, key, &stored, names)?];
                // And, per table this plan writes to a default the probe
                // cannot evaluate, the refusal where a foreign key to this
                // table spans such a column (124). After the count, so a
                // reader meets the rows first.
                for (child, moved) in &names.moved {
                    if let Some(p) = unprobeable_probe(table, key, &stored, child, moved, names)? {
                        probes.push(p);
                    }
                }
                Ok(probes)
            }
            None => Ok(skip(
                change,
                "the deleted row key does not exist before apply",
                unchecked,
            )),
        },
    }
}
