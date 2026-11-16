//! Probes the partition family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(
    change: &Change,
    partition: &PartitionChange,
) -> Result<Vec<Probe>, DialectError> {
    match partition {
        PartitionChange::RebuildPartitionedTable(_)
        | PartitionChange::CopyTableRows(_)
        | PartitionChange::AnalyzeTableTree(_) => {
            let (operation, table) = change.pending_rebuild().expect("rebuild variant");
            Err(DialectError::RebuildUnavailable {
                operation,
                table: table.clone(),
            })
        }
        PartitionChange::DetachPartition { .. }
        | PartitionChange::AttachPartition { .. }
        | PartitionChange::SetPartitionDefault { .. }
        | PartitionChange::SetPartitionNotNull { .. } => Ok(Vec::new()),
    }
}
