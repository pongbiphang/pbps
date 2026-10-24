//! One multipart-reference boundary, shared by target and staged capture.
//! Only explicitly declared tables and views cross it (DEC-2323.1).

use super::{Uncovered, read::Catalog};

pub(super) fn declared(
    catalog: &Catalog,
    dependency: &super::read::Dependency,
) -> Option<super::ObjectIdentity> {
    if dependency.server.is_some() || dependency.class != 1 || dependency.caller_dependent {
        return None;
    }
    let database = dependency.database.as_ref()?;
    let schema = dependency.schema.as_ref()?;
    catalog.declarations.keys().find_map(|name| {
        (name.database() == database
            && name.object().schema == *schema
            && name.object().name == dependency.entity)
            .then(|| super::ObjectIdentity {
                class: super::class::OBJECT.into(),
                name: vec![database.clone(), schema.clone(), dependency.entity.clone()],
                signature: Vec::new(),
            })
    })
}

pub(super) fn require_local(
    catalog: &Catalog,
    id: i64,
    identity: &super::ObjectIdentity,
) -> Result<(), Uncovered> {
    if let Some(dependency) = catalog.dependencies.iter().find(|d| {
        d.referencing == id
            && (d.server.is_some() || d.database.is_some())
            && declared(catalog, d).is_none()
    }) {
        let mut named = identity.clone();
        named.name = dependency
            .server
            .iter()
            .cloned()
            .chain(dependency.database.iter().cloned())
            .chain(dependency.schema.iter().cloned())
            .chain(std::iter::once(dependency.entity.clone()))
            .collect();
        return Err(Uncovered::object(
            &named,
            "a reference names an object in another database or server",
        ));
    }
    if catalog.qualified_synonyms.contains(&id) {
        return Err(Uncovered::object(
            identity,
            "a reference names an object in another database or server",
        ));
    }
    Ok(())
}
