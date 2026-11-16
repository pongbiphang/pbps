//! Probes the module family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(module: &ModuleChange) -> Result<Vec<Probe>, DialectError> {
    match module {
        // A module holds no rows. A definition the engine will not compile
        // fails inside the plan's transaction, where the rollback is total.
        ModuleChange::CreateModule { .. }
        | ModuleChange::AlterModule { .. }
        | ModuleChange::DropModule { .. } => Ok(Vec::new()),
    }
}
