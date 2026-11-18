//! Emits the search family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(search: &SearchChange) -> Sql {
    match search {
        SearchChange::CreateSearch { name, .. }
        | SearchChange::AlterSearchMapping { name, .. }
        | SearchChange::AlterSearchDictionary { name, .. }
        | SearchChange::DropSearch { name, .. } => Err(DialectError::Unsupported {
            dialect: crate::types::DIALECT,
            feature: format!("{}", name.shown()),
        }),
    }
}
