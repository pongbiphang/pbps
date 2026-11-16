//! Emits the type_value family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(type_value: &TypeValueChange) -> Sql {
    match type_value {
        TypeValueChange::AddEnumValue { name, .. }
        | TypeValueChange::AlterDomain { name, .. }
        | TypeValueChange::RenameEnumValue { name, .. }
        | TypeValueChange::EnumRebuild { name, .. } => Err(DialectError::Unsupported {
            dialect: crate::types::DIALECT,
            feature: format!("declared type {}", name.shown()),
        }),
    }
}
