//! What every object a package provides needs before a plan relies on it: the
//! rules of E's type route that do not depend on what the object is (DEC-2447.12,
//! DEC-2447.13). This is the verbatim move out of `type_refs`; the next commit
//! generalises it.

use std::collections::BTreeMap;

use pbps_model::escape::Escaped;
use pbps_model::{Change, ChangeSet, Schema, TypePin, TypeRef};

use crate::type_refs::is_planned;
use crate::user_types::UserType;

/// The packages the project declares, by the schema each is declared in. A
/// package the target lacks is created by this plan or the plan is refused
/// elsewhere, and the sealed manifest then decides whether a type is its own
/// ([`provider_proof`]). The last schema is the desired one.
pub(crate) fn declared_packages<'a>(
    schemas: &[&'a Schema],
) -> BTreeMap<&'a str, Vec<(&'a str, &'a str)>> {
    let mut by_name: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for schema in schemas {
        for (name, definition) in &schema.extensions {
            by_name.insert(
                name.raw(),
                (definition.schema.as_str(), definition.version.as_str()),
            );
        }
    }
    let mut by_schema: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (name, (schema, version)) in by_name {
        by_schema.entry(schema).or_default().push((name, version));
    }
    by_schema
}

/// The proof after the rehearsal for each placeholder pin: the package the plan
/// creates, or moves and updates, must list that exact type among its sealed
/// members (class `type`, as the engine names it), so a type of the same
/// spelling from anywhere else does not stand in for the one the order assumed
/// (DEC-2447.12). A created package is held to its manifest, a changed one to
/// the manifest it has after the change.
pub fn provider_proof(
    pins: &[TypePin],
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    for pin in pins.iter().filter(|p| is_planned(p)) {
        let Some(provider) = &pin.provider else {
            return Err(format!(
                "the type pin for `{}.{}` names no package",
                Escaped(&pin.schema),
                Escaped(&pin.name)
            ));
        };
        // The engine names a type as one qualified name.
        let reference = TypeRef::new(&pin.schema, &pin.name).map_err(|e| e.to_string())?;
        let member = pbps_model::ProviderMember {
            class: "type".to_owned(),
            names: vec![reference.raw().to_string()],
            args: Vec::new(),
        };
        let manifest = created
            .iter()
            .find(|e| e.name.raw() == provider.extension)
            .map(|e| &e.manifest)
            .or_else(|| {
                changed
                    .iter()
                    .find(|e| e.name.raw() == provider.extension)
                    .map(|e| &e.after)
            })
            .ok_or_else(|| {
                format!(
                    "the type `{}.{}` is taken as a member of the extension `{}`, which this \
                     plan neither creates nor changes",
                    Escaped(&pin.schema),
                    Escaped(&pin.name),
                    Escaped(&provider.extension)
                )
            })?;
        if !manifest.members.contains(&member) {
            return Err(format!(
                "the type `{}.{}` is not a member of the extension `{}` after the plan's change, \
                 so a column cannot rely on this plan to bring it",
                Escaped(&pin.schema),
                Escaped(&pin.name),
                Escaped(&provider.extension)
            ));
        }
    }
    Ok(())
}

/// A provided type is usable only when the project manages its package at the
/// version the target holds: a package nobody opted in has no proof route
/// (DEC-2447.3, DEC-2447.12).
pub(crate) fn check_providers(schemas: &[&Schema], types: &[UserType]) -> Result<(), String> {
    for ty in types {
        let Some((extension, version)) = ty.provider() else {
            continue;
        };
        let declared = schemas.iter().find_map(|s| {
            s.extensions
                .iter()
                .find(|(name, _)| name.raw() == extension)
                .map(|(_, definition)| definition)
        });
        match declared {
            None => {
                return Err(format!(
                    "the type `{}` belongs to the extension `{}`, which this project does not \
                     manage; opt it in with `pbps pull --extension {}` and record it with \
                     `pbps baseline`, then plan again",
                    ty.reference().shown(),
                    Escaped(extension),
                    Escaped(extension),
                ));
            }
            Some(definition) if definition.version != version => {
                return Err(format!(
                    "the type `{}` belongs to the extension `{}` at version `{}`, and the \
                     project records version `{}`; bring them together before planning",
                    ty.reference().shown(),
                    Escaped(extension),
                    Escaped(version),
                    Escaped(&definition.version),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// A package is not updated in the plan that holds a column of one of its
/// types (DEC-2447.12): an update can change what the type does (measured:
/// `hstore` 1.4 to 1.8 adds its subscript handler), the type's pin covers that,
/// and a pin that moves inside the plan that approved it can never be checked.
/// The update goes in its own plan, which re-reads and re-pins the type.
pub(crate) fn refuse_update_beside_provided(
    types: &[UserType],
    changes: &ChangeSet,
) -> Result<(), String> {
    for planned in &changes.changes {
        let Change::UpdateExtension { name, .. } = &planned.change else {
            continue;
        };
        if let Some(ty) = types.iter().find(|t| {
            t.provider()
                .is_some_and(|(extension, _)| extension == name.raw())
        }) {
            return Err(format!(
                "this plan updates extension `{}` and also uses its type `{}`; an update can \
                 change what the type does, so update the package in a plan of its own first",
                name.shown(),
                ty.reference().shown()
            ));
        }
    }
    Ok(())
}

/// A pin of a type whose package this plan moves says where the type is once
/// the move has run, so that every later read of it can find it (DEC-2447.12).
pub(crate) fn note_moves(pins: &mut [TypePin], changes: &ChangeSet) {
    for pin in pins.iter_mut().filter(|p| !is_planned(p)) {
        let Some(provider) = &mut pin.provider else {
            continue;
        };
        provider.moved_to = changes.changes.iter().find_map(|planned| {
            let Change::SetExtensionSchema { name, from, to, .. } = &planned.change else {
                return None;
            };
            (name.raw() == provider.extension && from.schema == pin.schema)
                .then(|| to.schema.clone())
        });
    }
}

/// A pin read again is the pin that was sealed, except that a type its package's
/// move carried is in the schema the plan said it would be (DEC-2447.12).
pub(crate) fn same_pin(sealed: &TypePin, now: &TypePin) -> bool {
    // Before the move, the type is where it was and the pin is what was sealed,
    // less the note of where it will be.
    let mut before = sealed.clone();
    if let Some(provider) = &mut before.provider {
        provider.moved_to = None;
    }
    if before == *now {
        return true;
    }
    // After it, the type is in the schema the plan said and nothing else moved.
    let moved = sealed
        .provider
        .as_ref()
        .and_then(|p| p.moved_to.as_deref())
        .is_some_and(|schema| schema == now.schema);
    moved
        && TypePin {
            schema: now.schema.clone(),
            ..before
        } == *now
}
