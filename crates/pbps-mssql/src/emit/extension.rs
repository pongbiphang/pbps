//! Emits the extension family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(extension: &ExtensionChange) -> Sql {
    match extension {
        ExtensionChange::CreateExtension { name, .. }
        | ExtensionChange::DropExtension { name, .. }
        | ExtensionChange::UpdateExtension { name, .. }
        | ExtensionChange::SetExtensionSchema { name, .. } => Err(DialectError::Unsupported {
            dialect: crate::types::DIALECT,
            feature: format!("extension {}", name.shown()),
        }),
    }
}
