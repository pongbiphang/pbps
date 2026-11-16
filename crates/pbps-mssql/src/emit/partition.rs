//! Emits the partition family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(change: &Change, partition: &PartitionChange) -> Sql {
    match partition {
        PartitionChange::RebuildPartitionedTable(_)
        | PartitionChange::CopyTableRows(_)
        | PartitionChange::AnalyzeTableTree(_) => {
            let (operation, table) = change.pending_rebuild().expect("explicit rebuild variant");
            Err(DialectError::RebuildUnavailable {
                operation,
                table: table.clone(),
            })
        }
        PartitionChange::DetachPartition { table, parent, .. }
        | PartitionChange::AttachPartition { table, parent, .. } => Err(DialectError::Invalid {
            dialect: crate::types::DIALECT,
            message: format!(
                "{table} is a partition of {parent}, which SQL Server's model does not hold",
                table = table.shown(),
                parent = parent.shown()
            ),
        }),
        // A partition is PostgreSQL's in this model (#1581).
        PartitionChange::SetPartitionDefault { table, .. }
        | PartitionChange::SetPartitionNotNull { table, .. } => Err(DialectError::Invalid {
            dialect: crate::types::DIALECT,
            message: format!(
                "{table} is a partition, which SQL Server's model does not hold",
                table = table.shown()
            ),
        }),
    }
}
