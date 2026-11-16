//! Probes the table_property family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(table_property: &TablePropertyChange) -> Result<Vec<Probe>, DialectError> {
    match table_property {
        TablePropertyChange::SetComment { .. }
        | TablePropertyChange::SetTablePersistence { .. }
        | TablePropertyChange::SetStorageParameters { .. }
        | TablePropertyChange::SetReplicaIdentity { .. } => Ok(Vec::new()),
    }
}
