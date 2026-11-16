//! Emits the type family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(r#type: &TypeChange) -> Sql {
    match r#type {
        TypeChange::CreateType { name, .. }
        | TypeChange::DropType { name, .. }
        | TypeChange::RenameType { from: name, .. } => Err(DialectError::Unsupported {
            dialect: crate::types::DIALECT,
            feature: format!("declared type {}", name.shown()),
        }),
    }
}
