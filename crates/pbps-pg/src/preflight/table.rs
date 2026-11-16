//! Probes the table family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(table: &TableChange) -> Result<Vec<Probe>, DialectError> {
    match table {
        // Everything else either cannot fail on the data or cannot be counted.
        TableChange::CreateTable { .. }
        | TableChange::DropTable { .. }
        | TableChange::RenameTable { .. } => Ok(Vec::new()),
    }
}
