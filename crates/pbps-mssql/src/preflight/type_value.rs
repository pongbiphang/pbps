//! Probes the type_value family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(type_value: &TypeValueChange) -> Result<Vec<Probe>, DialectError> {
    match type_value {
        TypeValueChange::AddEnumValue { .. }
        | TypeValueChange::AlterDomain { .. }
        | TypeValueChange::RenameEnumValue { .. }
        | TypeValueChange::EnumRebuild { .. } => Ok(Vec::new()),
    }
}
