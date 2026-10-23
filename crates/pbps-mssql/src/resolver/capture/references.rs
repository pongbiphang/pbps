//! One multipart-reference boundary, shared by target and staged capture.
//! #2280 can relax it for explicitly declared dependencies here.

use super::{Uncovered, read::Catalog};

pub(super) fn require_local(
    catalog: &Catalog,
    id: i64,
    identity: &super::ObjectIdentity,
) -> Result<(), Uncovered> {
    if catalog
        .dependencies
        .iter()
        .any(|d| d.referencing == id && (d.server.is_some() || d.database.is_some()))
        || catalog.qualified_synonyms.contains(&id)
    {
        return Err(Uncovered::object(
            identity,
            "a reference names an object in another database or server",
        ));
    }
    Ok(())
}
