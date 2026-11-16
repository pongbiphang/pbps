//! Emits the search family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(search: &SearchChange) -> Sql {
    match search {
        SearchChange::CreateSearch {
            uid,
            name,
            definition,
            ..
        } => crate::search::create(uid, name, definition),
        SearchChange::DropSearch { uid, name, .. } => crate::search::drop(uid, name),
    }
}
