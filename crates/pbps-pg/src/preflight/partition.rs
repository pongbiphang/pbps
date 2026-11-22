//! Probes the partition family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    partition: &PartitionChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
) -> Result<Vec<Probe>, DialectError> {
    match partition {
        PartitionChange::RebuildPartitionedTable(_) => {
            let (operation, table) = change.pending_rebuild().expect("rebuild variant");
            Err(DialectError::RebuildUnavailable {
                operation,
                table: table.clone(),
            })
        }
        PartitionChange::CopyTableRows(_) | PartitionChange::AnalyzeTableTree(_) => Ok(skip(
            change,
            "the complete rebuild is recertified under its source-tree lock",
            unchecked,
        )),
        // A partition's own NOT NULL reads the partition's rows, as a
        // column's reads its table's (#1581).
        PartitionChange::SetPartitionNotNull {
            table,
            column,
            not_null: true,
            ..
        } => {
            let column = table.column(column);
            // A column the parent adds in this plan is not in the stored
            // rows, or is there under its name as the column leaving it.
            if names.columns_added.contains_key(&column) {
                return Ok(skip(change, "the column is added by this plan", unchecked));
            }
            match names.column(&column) {
                Some(stored) => Ok(vec![count_probes::nulls(
                    &COUNTS,
                    &column,
                    &qualified(&stored.table)?,
                    &quote(stored.name.raw())?,
                )]),
                None => Ok(skip(
                    change,
                    "the column does not exist before apply",
                    unchecked,
                )),
            }
        }
        PartitionChange::SetPartitionDefault { .. }
        | PartitionChange::SetPartitionNotNull { .. }
        | PartitionChange::DetachPartition { .. }
        | PartitionChange::AttachPartition { .. } => Ok(Vec::new()),
    }
}
