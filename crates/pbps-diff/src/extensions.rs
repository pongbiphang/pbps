use crate::{Side, schema_diff::DiffError};
use std::collections::BTreeSet;

/// The adopted packages a connected or scratch bootstrap must already hold.
///
/// This is not lifecycle permission: callers must verify these exact
/// prerequisites before compiling managed objects and before recording them.
/// Real target differences continue through `differences` (DEC-2447.1).
pub fn bootstrap_base(desired: Side<'_>) -> (pbps_model::Schema, pbps_model::IdsFile) {
    (
        pbps_model::Schema {
            extensions: desired
                .schema
                .extensions
                .iter()
                .filter(|(name, _)| desired.ids.extension_uid(name).is_some())
                .map(|(name, definition)| (name.clone(), definition.clone()))
                .collect(),
            ..Default::default()
        },
        pbps_model::IdsFile {
            extensions: desired
                .ids
                .extensions
                .iter()
                .filter(|(_, name)| desired.schema.extensions.contains_key(*name))
                .map(|(uid, name)| (uid.clone(), name.clone()))
                .collect(),
            ..Default::default()
        },
    )
}

/// The packages the declaration holds under a recorded uid that the base does
/// not have: each becomes a pinned `CREATE EXTENSION` (DEC-2447.6). Inventory
/// with no uid never creates (DEC-2447.2). `requires` is the target's package
/// catalog (control `requires` per declared package, absent offline); only the
/// prerequisites this plan also creates become edges, because one already
/// installed needs none and one neither installed nor created is refused with
/// the target facts before a plan exists.
pub fn creates(
    base: Side<'_>,
    desired: Side<'_>,
    dialect: &str,
    requires: &std::collections::BTreeMap<
        pbps_model::ObjectLocalName,
        BTreeSet<pbps_model::ObjectLocalName>,
    >,
) -> Vec<pbps_model::Change> {
    if dialect != "postgres" {
        return Vec::new();
    }
    let created: std::collections::BTreeMap<_, _> = desired
        .schema
        .extensions
        .iter()
        .filter(|(name, _)| !base.schema.extensions.contains_key(*name))
        .filter_map(|(name, definition)| {
            Some((name, (desired.ids.extension_uid(name)?, definition)))
        })
        .collect();
    created
        .iter()
        .map(
            |(name, (uid, definition))| pbps_model::Change::CreateExtension {
                uid: (*uid).clone(),
                name: (*name).clone(),
                definition: (*definition).clone(),
                requires: requires
                    .get(*name)
                    .into_iter()
                    .flatten()
                    .filter_map(|required| created.get(required))
                    .map(|(uid, _)| (*uid).clone())
                    .collect(),
            },
        )
        .collect()
}

/// The packages the base holds under a recorded uid that the declarations no
/// longer hold and a recorded intent retired (a tombstone for the uid).
fn retired<'a>(
    base: Side<'a>,
    desired: Side<'_>,
) -> impl Iterator<Item = &'a pbps_model::ObjectLocalName> {
    let tombstoned: BTreeSet<pbps_model::Uid> = desired.ids.tombstones.keys().cloned().collect();
    base.schema
        .extensions
        .keys()
        .filter(move |name| !desired.schema.extensions.contains_key(*name))
        .filter(move |name| {
            base.ids
                .extension_uid(name)
                .is_some_and(|uid| tombstoned.contains(uid))
        })
}

/// The packages a recorded intent retires, each as a `DROP EXTENSION RESTRICT`
/// (DEC-2447.8), judged against what the target says the drop would reach.
///
/// Every object the drop would delete or be blocked by must be one this plan
/// also removes, which the drop then waits for; any other is a refusal that
/// names it, because an `x` edge is a user's mark and never permission to
/// delete. A package another dropped package requires is dropped after it; one
/// a surviving package requires is refused. Offline there are no facts, so a
/// drop is planned only against a database.
pub fn drops(
    base: Side<'_>,
    desired: Side<'_>,
    dialect: &str,
    facts: &std::collections::BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionDropFacts>,
) -> (Vec<pbps_model::Change>, Vec<DiffError>) {
    if dialect != "postgres" {
        return (Vec::new(), Vec::new());
    }
    let dropped: std::collections::BTreeMap<_, _> = retired(base, desired)
        .filter_map(|name| {
            Some((
                name,
                (
                    base.ids.extension_uid(name)?,
                    base.schema.extensions.get(name)?,
                ),
            ))
        })
        .collect();
    let removed: Vec<&pbps_model::ModuleId> = base
        .schema
        .modules
        .keys()
        .filter(|id| !desired.schema.modules.contains_key(*id))
        .collect();
    let (mut changes, mut errors) = (Vec::new(), Vec::new());
    for (name, (uid, definition)) in &dropped {
        let Some(facts) = facts.get(*name) else {
            errors.push(DiffError::ExtensionDropNeedsDatabase {
                name: (*name).clone(),
            });
            continue;
        };
        let (mut requirers, mut releases, mut found) =
            (BTreeSet::new(), BTreeSet::new(), Vec::new());
        for member in &facts.outside {
            match member.module(removed.iter().copied()) {
                Some(id) => {
                    releases.insert(id.clone());
                }
                None => found.push(format!(
                    "`{member}` depends on the package, so the engine would delete it too, and this plan does not drop it"
                )),
            }
        }
        for member in &facts.blockers {
            if member.class == "extension"
                && let [required_by] = member.names.as_slice()
            {
                match dropped.iter().find(|(n, _)| n.raw() == required_by) {
                    Some((_, (other, _))) => {
                        requirers.insert((*other).clone());
                    }
                    None => found.push(format!(
                        "package `{}` requires it and stays",
                        pbps_model::escape::Escaped(required_by)
                    )),
                }
            } else if let Some(id) = member.module(removed.iter().copied()) {
                releases.insert(id.clone());
            } else {
                found.push(format!(
                    "`{member}` depends on the package and this plan does not remove it, so the engine would refuse"
                ));
            }
        }
        if !found.is_empty() {
            errors.push(DiffError::ExtensionDropRefused {
                name: (*name).clone(),
                found,
            });
            continue;
        }
        changes.push(pbps_model::Change::DropExtension {
            uid: (*uid).clone(),
            name: (*name).clone(),
            definition: (*definition).clone(),
            requirers,
            releases,
        });
    }
    (changes, errors)
}

/// The packages the declarations hold at a version other than the base's, in
/// the schema the base holds them in, each as an `ALTER EXTENSION ... UPDATE TO`
/// (DEC-2447.10). Offline there are no facts, so an update is planned only
/// against a database. The update waits for the packages this plan creates that
/// the target version requires, and for the modules this plan drops that depend
/// on a member: a superset of what the script will retire, which is known only
/// after the rehearsal, and dropping a consumer first is never wrong. A package
/// that also changes schema is refused in [`differences`].
pub fn updates(
    base: Side<'_>,
    desired: Side<'_>,
    dialect: &str,
    facts: &std::collections::BTreeMap<
        pbps_model::ObjectLocalName,
        pbps_model::ExtensionUpdateFacts,
    >,
) -> (Vec<pbps_model::Change>, Vec<DiffError>) {
    if dialect != "postgres" {
        return (Vec::new(), Vec::new());
    }
    let removed: Vec<&pbps_model::ModuleId> = base
        .schema
        .modules
        .keys()
        .filter(|id| !desired.schema.modules.contains_key(*id))
        .collect();
    let created: std::collections::BTreeMap<_, _> = desired
        .schema
        .extensions
        .keys()
        .filter(|name| !base.schema.extensions.contains_key(*name))
        .filter_map(|name| Some((name, desired.ids.extension_uid(name)?)))
        .collect();
    let (mut changes, mut errors) = (Vec::new(), Vec::new());
    for (name, to) in &desired.schema.extensions {
        let Some(from) = base.schema.extensions.get(name) else {
            continue;
        };
        let (Some(uid), Some(recorded)) = (
            base.ids.extension_uid(name),
            desired.ids.extension_uid(name),
        ) else {
            continue;
        };
        if uid != recorded || from.version == to.version {
            continue;
        }
        let Some(facts) = facts.get(name) else {
            errors.push(DiffError::ExtensionUpdateNeedsDatabase {
                name: name.clone(),
                from: from.version.clone(),
                to: to.version.clone(),
            });
            continue;
        };
        let carried = |members: &BTreeSet<pbps_model::ProviderMember>| {
            members
                .iter()
                .filter_map(|m| m.module(removed.iter().copied()).cloned())
                .collect::<Vec<_>>()
        };
        changes.push(pbps_model::Change::UpdateExtension {
            uid: uid.clone(),
            name: name.clone(),
            from: from.clone(),
            // Beside a move the update runs first, in the schema the package
            // holds now, and the move starts from where it leaves it
            // (DEC-2447.11).
            to: pbps_model::Extension {
                schema: from.schema.clone(),
                version: to.version.clone(),
            },
            requires: facts
                .requires
                .iter()
                .filter_map(|required| created.get(required).map(|uid| (*uid).clone()))
                .collect(),
            releases: carried(&facts.reach.outside)
                .into_iter()
                .chain(carried(&facts.reach.blockers))
                .collect(),
        });
    }
    (changes, errors)
}

/// The packages the declarations hold in another schema than the base, each as
/// an `ALTER EXTENSION ... SET SCHEMA` (DEC-2447.11). A move starts from the
/// version the package holds once this plan's update, if any, has run. It waits
/// for the modules this plan drops that hold the name a member would take in the
/// destination. Offline there are no facts, so a move is planned only against a
/// database.
pub fn moves(
    base: Side<'_>,
    desired: Side<'_>,
    dialect: &str,
    facts: &std::collections::BTreeMap<
        pbps_model::ObjectLocalName,
        std::collections::BTreeSet<pbps_model::ProviderMember>,
    >,
) -> (Vec<pbps_model::Change>, Vec<DiffError>) {
    if dialect != "postgres" {
        return (Vec::new(), Vec::new());
    }
    let removed: Vec<&pbps_model::ModuleId> = base
        .schema
        .modules
        .keys()
        .filter(|id| !desired.schema.modules.contains_key(*id))
        .collect();
    let (mut changes, mut errors) = (Vec::new(), Vec::new());
    for (name, to) in &desired.schema.extensions {
        let Some(from) = base.schema.extensions.get(name) else {
            continue;
        };
        let (Some(uid), Some(recorded)) = (
            base.ids.extension_uid(name),
            desired.ids.extension_uid(name),
        ) else {
            continue;
        };
        if uid != recorded || from.schema == to.schema {
            continue;
        }
        let Some(members) = facts.get(name) else {
            errors.push(DiffError::ExtensionMoveNeedsDatabase {
                name: name.clone(),
                from: from.schema.clone(),
                to: to.schema.clone(),
            });
            continue;
        };
        changes.push(pbps_model::Change::SetExtensionSchema {
            uid: uid.clone(),
            name: name.clone(),
            from: pbps_model::Extension {
                schema: from.schema.clone(),
                version: to.version.clone(),
            },
            to: to.clone(),
            releases: members
                .iter()
                .filter_map(|m| m.module(removed.iter().copied()).cloned())
                .collect(),
        });
    }
    (changes, errors)
}

/// The comparison schema with every column of a type a moved package provides
/// spelled in the schema the package moves to (DEC-2447.12).
///
/// A column keeps its type through `ALTER EXTENSION ... SET SCHEMA`: the type
/// keeps its OID and only its name changes (measured on 16.15 and 18.6). The
/// recorded schema still spells `ext.citext` and the declaration spells the new
/// schema, so compared as written the column would be retyped. Both sides name
/// one type, so the base is carried through the move, by the members the
/// connected read listed for the package (as the destination names them),
/// before the columns are compared. Only
/// a typed column reference is forwarded: a default, a CHECK or an index key
/// that spells the old schema is a declared text change and is planned as one.
pub fn forward_moved_types<'a>(
    comparison: std::borrow::Cow<'a, pbps_model::Schema>,
    base: Side<'_>,
    desired: Side<'_>,
    facts: &std::collections::BTreeMap<
        pbps_model::ObjectLocalName,
        std::collections::BTreeSet<pbps_model::ProviderMember>,
    >,
) -> std::borrow::Cow<'a, pbps_model::Schema> {
    use pbps_model::{ColumnType, TypeRef};
    let mut moved: std::collections::BTreeMap<TypeRef, TypeRef> = std::collections::BTreeMap::new();
    for (name, to) in &desired.schema.extensions {
        let (Some(from), Some(members)) = (base.schema.extensions.get(name), facts.get(name))
        else {
            continue;
        };
        let same = matches!(
            (base.ids.extension_uid(name), desired.ids.extension_uid(name)),
            (Some(a), Some(b)) if a == b
        );
        if !same || from.schema == to.schema {
            continue;
        }
        // The facts are the members as the destination names them. The engine
        // names a type as one qualified name (`moved.citext`, or `"My B".citext`),
        // and `relocated` read the other way is how it reads where the package
        // is now. The array type of a member is the column's own `[]` and has
        // no entry: a name with a suffix does not parse as one.
        for member in members.iter().filter(|m| m.class == "type") {
            if let [name] = member.names.as_slice()
                && let Ok(new) = name.parse::<TypeRef>()
                && new.schema() == to.schema
                && let [previous] = member.relocated(&to.schema, &from.schema).names.as_slice()
                && let Ok(old) = previous.parse::<TypeRef>()
            {
                moved.insert(old, new);
            }
        }
    }
    if moved.is_empty() {
        return comparison;
    }
    let forward = |ty: &mut ColumnType| {
        // An array is looked up by its element.
        if let Some(reference) = ty.element().type_ref()
            && let Some(new) = moved.get(reference)
        {
            let carried = ColumnType::reference(new.clone());
            *ty = if ty.is_array() {
                carried.into_array()
            } else {
                carried
            };
        }
    };
    let mut schema = comparison.into_owned();
    for table in schema.tables.values_mut() {
        table.columns.values_mut().for_each(|c| forward(&mut c.ty));
    }
    for ty in schema.types.values_mut() {
        if let pbps_model::ManagedType::Domain { base, .. } = ty {
            forward(base);
        }
    }
    std::borrow::Cow::Owned(schema)
}

/// A table's column types carried through the moves of the plan, where the
/// read-back shows them carried: a column that was a reference into a schema a
/// package leaves and is now the same name in the one it goes to is the same
/// column. Only a typed column reference is carried, an array by its element,
/// and only what `now` shows moved, so a type that stayed behind in the schema
/// stays as it is. Used by the apply's read-back comparison of a table the plan
/// does not touch, where the plan's own move is the one explanation for a type
/// changing schema (DEC-2447.12).
pub fn forward_schema_moves(
    table: &mut pbps_model::Table,
    now: &pbps_model::Table,
    moves: &[(&str, &str)],
) {
    use pbps_model::{ColumnType, TypeRef};
    if moves.is_empty() {
        return;
    }
    for (name, column) in &mut table.columns {
        let Some(read) = now.columns.get(name) else {
            continue;
        };
        let (was, is) = (column.ty.element(), read.ty.element());
        let (Some(was), Some(is)) = (was.type_ref(), is.type_ref()) else {
            continue;
        };
        let carried = was.name() == is.name()
            && moves
                .iter()
                .any(|(from, to)| *from == was.schema() && *to == is.schema());
        if carried && column.ty.is_array() == read.ty.is_array() {
            let reference = ColumnType::reference(
                TypeRef::new(is.schema(), is.name()).unwrap_or_else(|_| is.clone()),
            );
            column.ty = if read.ty.is_array() {
                reference.into_array()
            } else {
                reference
            };
        }
    }
}

/// How many packages of the plan each create waits behind, so that the differ's
/// own order has a prerequisite ahead of what requires it. It is the effects
/// graph that decides the order the plan keeps; this rank only keeps the
/// differ's draft from contradicting it, as a domain's rank does for the enum
/// under it (a cycle is the effects graph's refusal, and ranks the cycle as 0).
pub(crate) fn depths<'a>(
    changes: impl Iterator<Item = &'a pbps_model::Change>,
) -> std::collections::BTreeMap<pbps_model::Uid, isize> {
    let requires: std::collections::BTreeMap<_, _> = changes
        .filter_map(|c| {
            if let pbps_model::Change::CreateExtension { uid, requires, .. }
            | pbps_model::Change::UpdateExtension { uid, requires, .. } = c
            {
                Some((uid, requires))
            // A drop waits for the drops of the packages that require it: the
            // same shape, from the other side (DEC-2447.8).
            } else if let pbps_model::Change::DropExtension { uid, requirers, .. } = c {
                Some((uid, requirers))
            } else {
                None
            }
        })
        .collect();
    fn depth<'a>(
        uid: &'a pbps_model::Uid,
        requires: &std::collections::BTreeMap<&'a pbps_model::Uid, &'a BTreeSet<pbps_model::Uid>>,
        walking: &mut BTreeSet<&'a pbps_model::Uid>,
    ) -> isize {
        let Some(needs) = requires.get(uid) else {
            return 0;
        };
        if !walking.insert(uid) {
            return 0;
        }
        let deepest = needs
            .iter()
            .map(|need| 1 + depth(need, requires, walking))
            .max()
            .unwrap_or(0);
        walking.remove(uid);
        deepest
    }
    requires
        .keys()
        .map(|uid| ((*uid).clone(), depth(uid, &requires, &mut BTreeSet::new())))
        .collect()
}

/// Refusals share the drift channel, so no executable plan can lose an
/// extension difference merely because no Change can yet express it. A create
/// is not one: [`creates`] expresses it.
pub fn differences(base: Side<'_>, desired: Side<'_>, dialect: &str) -> Vec<DiffError> {
    let mut errors = Vec::new();
    let mut refuse = |name: &pbps_model::ObjectLocalName, action: String| {
        errors.push(DiffError::ExtensionLifecycleUnsupported {
            name: name.clone(),
            action,
        })
    };
    for name in base
        .schema
        .extensions
        .keys()
        .chain(desired.schema.extensions.keys())
        // A full catalog read is inventory, not an adoption. Match the managed
        // scope even when a caller passes an unprojected read (DEC-2447.2).
        .filter(|name| {
            base.ids.extension_uid(name).is_some() || desired.ids.extension_uid(name).is_some()
        })
        .collect::<BTreeSet<_>>()
    {
        if dialect != "postgres" {
            refuse(name, format!("PostgreSQL extensions on {}", dialect));
            continue;
        }
        match (
            base.schema.extensions.get(name),
            desired.schema.extensions.get(name),
        ) {
            (None, Some(_)) => {}
            // A drop is planned from a recorded intent: the declarations
            // simply no longer holding a package is not one (DEC-2447.8).
            (Some(_), None) => {
                let retired = base
                    .ids
                    .extension_uid(name)
                    .is_some_and(|uid| desired.ids.tombstones.contains_key(uid));
                if !retired {
                    refuse(
                        name,
                        "DROP EXTENSION without a recorded intent (`pbps drop-extension NAME --reason WHY`)"
                            .into(),
                    );
                }
            }
            (Some(_), Some(_)) => {
                // A version change is an update (`updates`) and a schema change a
                // move (`moves`), alone or together (DEC-2447.10, DEC-2447.11).
                if base.ids.extension_uid(name) != desired.ids.extension_uid(name) {
                    refuse(
                        name,
                        "replace the recorded extension identity; drop the package in one plan \
                         (`pbps drop-extension`) and declare the name again in the next"
                            .into(),
                    );
                }
            }
            (None, None) => unreachable!(),
        }
    }
    for (uid, name) in &base.ids.extensions {
        if let Some(to) = desired.ids.extensions.get(uid)
            && name != to
        {
            refuse(
                name,
                format!("rename to `{}`; extension names are immutable", to.shown()),
            );
        }
    }
    errors
}

/// [`differences`], plus a refusal for each package the declarations hold that
/// the base lacks: for the callers that install nothing, which check that the
/// packages they were handed stand exactly as declared (a bootstrap's
/// prerequisites, a replay target). A create is for [`creates`] alone, behind a
/// rehearsal and a sealed manifest (DEC-2447.6); here it would be a package
/// nobody is going to install.
pub fn prerequisite_differences(
    base: Side<'_>,
    desired: Side<'_>,
    dialect: &str,
) -> Vec<DiffError> {
    let mut errors = differences(base, desired, dialect);
    for name in retired(base, desired) {
        errors.push(DiffError::ExtensionLifecycleUnsupported {
            name: name.clone(),
            action: "DROP EXTENSION".into(),
        });
    }
    for name in desired.schema.extensions.keys() {
        let (old, new) = (
            base.schema.extensions.get(name),
            desired.schema.extensions.get(name),
        );
        if let (Some(old), Some(new)) = (old, new) {
            for (changed, action) in [
                (old.version != new.version, "ALTER EXTENSION UPDATE"),
                (old.schema != new.schema, "ALTER EXTENSION SET SCHEMA"),
            ] {
                if changed {
                    errors.push(DiffError::ExtensionLifecycleUnsupported {
                        name: name.clone(),
                        action: action.into(),
                    });
                }
            }
        }
    }
    for change in creates(base, desired, dialect, &Default::default()) {
        if let pbps_model::Change::CreateExtension { name, .. } = change {
            errors.push(DiffError::ExtensionLifecycleUnsupported {
                name,
                action: "CREATE EXTENSION".into(),
            });
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Extension, IdsFile, Schema};

    #[test]
    fn unowned_inventory_does_not_request_lifecycle_in_either_direction() {
        for name in ["plpgsql", "citext", "vendor_default"] {
            for namespace in ["pg_catalog", "packages"] {
                let schema = Schema {
                    extensions: [(
                        name.into(),
                        Extension {
                            schema: namespace.into(),
                            version: "exact".into(),
                        },
                    )]
                    .into(),
                    ..Default::default()
                };
                let empty = Schema::default();
                let no_ids = IdsFile::default();
                let unowned = Side {
                    schema: &schema,
                    ids: &no_ids,
                };
                let absent = Side {
                    schema: &empty,
                    ids: &no_ids,
                };
                assert!(differences(unowned, absent, "postgres").is_empty());
                assert!(differences(absent, unowned, "postgres").is_empty());
                assert!(bootstrap_base(unowned).0.extensions.is_empty());
                let ids = IdsFile {
                    extensions: [("e_000001".parse().unwrap(), name.into())].into(),
                    ..Default::default()
                };
                let owned = Side {
                    schema: &schema,
                    ids: &ids,
                };
                // A recorded identity for an absent package is a create, which
                // `differences` leaves to `creates`; it is not a refusal.
                assert!(differences(absent, owned, "postgres").is_empty());
                let planned = creates(absent, owned, "postgres", &Default::default());
                assert!(
                    matches!(
                        planned.as_slice(),
                        [pbps_model::Change::CreateExtension { name: n, .. }] if n.raw() == name
                    ),
                    "{planned:?}"
                );
                assert!(creates(owned, absent, "postgres", &Default::default()).is_empty());
                assert!(creates(unowned, absent, "postgres", &Default::default()).is_empty());
                for (before, after, action) in [
                    (owned, absent, "DROP EXTENSION"),
                    (unowned, owned, "identity"),
                ] {
                    let errors = differences(before, after, "postgres");
                    assert_eq!(errors.len(), 1);
                    assert!(errors[0].to_string().contains(action));
                }
            }
        }
    }

    #[test]
    fn scratch_prerequisites_preserve_packages_without_authorizing_target_creation() {
        let schema = Schema {
            extensions: [(
                "citext".into(),
                Extension {
                    schema: "ext".into(),
                    version: "1.4".into(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let ids = IdsFile {
            extensions: [("e_000001".parse().unwrap(), "citext".into())].into(),
            ..Default::default()
        };
        let desired = Side {
            schema: &schema,
            ids: &ids,
        };
        let (scratch, scratch_ids) = bootstrap_base(desired);
        assert_eq!(scratch.extensions, schema.extensions);
        assert_eq!(scratch_ids.extensions, ids.extensions);
        assert!(
            scratch.tables.is_empty()
                && scratch.types.is_empty()
                && scratch.roles.is_empty()
                && scratch.modules.is_empty()
        );
        assert!(
            differences(
                Side {
                    schema: &scratch,
                    ids: &scratch_ids
                },
                desired,
                "postgres"
            )
            .is_empty()
        );
        let empty = Schema::default();
        let nothing = Side {
            schema: &empty,
            ids: &IdsFile::default(),
        };
        assert!(differences(nothing, desired, "postgres").is_empty());
        assert_eq!(
            creates(nothing, desired, "postgres", &Default::default()).len(),
            1
        );
        // The same declarations plan nothing on another engine, which refuses.
        assert!(creates(nothing, desired, "mssql", &Default::default()).is_empty());
        assert!(
            differences(
                Side {
                    schema: &scratch,
                    ids: &scratch_ids
                },
                desired,
                "mssql"
            )[0]
            .to_string()
            .contains("PostgreSQL extensions")
        );
    }

    #[test]
    fn lifecycle_differences_are_named_and_unchanged_packages_have_no_drift() {
        let schema = Schema {
            extensions: [(
                "citext".into(),
                Extension {
                    schema: "ext".into(),
                    version: "1.4".into(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let ids = IdsFile {
            extensions: [("e_000001".parse().unwrap(), "citext".into())].into(),
            ..Default::default()
        };
        let side = Side {
            schema: &schema,
            ids: &ids,
        };
        assert!(differences(side, side, "postgres").is_empty());
        assert!(!differences(side, side, "mssql").is_empty());
        let found = differences(
            side,
            Side {
                schema: &Schema::default(),
                ids: &ids,
            },
            "postgres",
        );
        assert!(
            found
                .iter()
                .any(|e| e.to_string().contains("DROP EXTENSION")
                    && e.to_string().contains("citext")),
            "{found:?}"
        );
        // A version change is an update and a schema change a move, alone or
        // together: neither is a refusal here (both are planned from facts), but
        // the callers that install nothing still refuse each (DEC-2447.10,
        // DEC-2447.11).
        let at = |schema: &str, version: &str| Schema {
            extensions: [(
                "citext".into(),
                Extension {
                    schema: schema.into(),
                    version: version.into(),
                },
            )]
            .into(),
            ..Default::default()
        };
        for (changed, actions) in [
            (at("ext", "1.6"), vec!["UPDATE"]),
            (at("other", "1.4"), vec!["SET SCHEMA"]),
            (at("other", "1.6"), vec!["UPDATE", "SET SCHEMA"]),
        ] {
            let desired = Side {
                schema: &changed,
                ids: &ids,
            };
            assert!(differences(side, desired, "postgres").is_empty());
            let strict = prerequisite_differences(side, desired, "postgres");
            assert_eq!(strict.len(), actions.len(), "{strict:?}");
            for action in actions {
                assert!(
                    strict.iter().any(|e| e.to_string().contains(action)),
                    "{action}: {strict:?}"
                );
            }
        }
        let empty = Schema::default();
        let nothing = Side {
            schema: &empty,
            ids: &IdsFile::default(),
        };
        assert!(differences(nothing, side, "postgres").is_empty());
        let new_ids = IdsFile {
            extensions: [("e_000001".parse().unwrap(), "other".into())].into(),
            ..Default::default()
        };
        assert!(
            differences(
                side,
                Side {
                    schema: &schema,
                    ids: &new_ids
                },
                "postgres"
            )
            .iter()
            .any(|e| e.to_string().contains("immutable"))
        );
    }

    #[test]
    fn managed_scope_and_identity_allocation_do_not_infer_package_ownership() {
        let schema = Schema {
            extensions: [(
                "plpgsql".into(),
                Extension {
                    schema: "pg_catalog".into(),
                    version: "1.0".into(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let ctx = crate::Context {
            operator: "operator".into(),
            today: "2026-10-29".into(),
        };
        let resolved = crate::resolve(&schema, &IdsFile::default(), &[], &ctx).unwrap();
        assert_eq!(resolved.ids.extensions.len(), 1);
        assert_eq!(
            crate::resolve(&schema, &resolved.ids, &[], &ctx)
                .unwrap()
                .ids,
            resolved.ids
        );
        assert!(
            crate::scope(
                &schema,
                &IdsFile::default(),
                &Default::default(),
                &Default::default()
            )
            .schema
            .extensions
            .is_empty()
        );
        assert_eq!(
            crate::scope(
                &schema,
                &resolved.ids,
                &Default::default(),
                &Default::default()
            )
            .schema,
            schema
        );
        assert_eq!(
            crate::observed_ids(&schema, &resolved.ids).extensions,
            resolved.ids.extensions
        );
    }

    /// A target holding the packages `held` (each recorded) and the routines
    /// `app.<name>()` of `dropped_modules`, against declarations that keep every
    /// held package but those `retired`, whose identities tombstone them.
    struct World {
        base: Schema,
        base_ids: IdsFile,
        desired: Schema,
        desired_ids: IdsFile,
    }

    fn uid(name: &str) -> pbps_model::Uid {
        pbps_model::Uid::derived(pbps_model::UidKind::Extension, name, 0)
    }

    fn routine(name: &str) -> pbps_model::ModuleId {
        format!("app.{name}()").parse().unwrap()
    }

    fn module() -> pbps_model::Module {
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: "() RETURNS int LANGUAGE sql RETURN 1".into(),
            options: Default::default(),
        }
    }

    fn world(held: &[&str], retired: &[&str], dropped_modules: &[&str]) -> World {
        let extension = |_: &&str| Extension {
            schema: "ext".into(),
            version: "1.0".into(),
        };
        let mut base = Schema::default();
        let mut desired = Schema::default();
        let mut base_ids = IdsFile::default();
        let mut desired_ids = IdsFile::default();
        for name in held {
            base.extensions.insert((*name).into(), extension(name));
            base_ids.extensions.insert(uid(name), (*name).into());
            if retired.contains(name) {
                desired_ids.tombstones.insert(
                    uid(name),
                    pbps_model::Tombstone {
                        require: None,
                        was: (*name).to_owned(),
                        dropped_at: "2026-11-02".into(),
                        reason: "retired".into(),
                        operator: "test".into(),
                    },
                );
            } else {
                desired.extensions.insert((*name).into(), extension(name));
                desired_ids.extensions.insert(uid(name), (*name).into());
            }
        }
        for name in dropped_modules {
            base.modules.insert(routine(name), module());
        }
        World {
            base,
            base_ids,
            desired,
            desired_ids,
        }
    }

    impl World {
        fn drops(
            &self,
            facts: &std::collections::BTreeMap<
                pbps_model::ObjectLocalName,
                pbps_model::ExtensionDropFacts,
            >,
        ) -> (Vec<pbps_model::Change>, Vec<DiffError>) {
            drops(
                Side {
                    schema: &self.base,
                    ids: &self.base_ids,
                },
                Side {
                    schema: &self.desired,
                    ids: &self.desired_ids,
                },
                "postgres",
                facts,
            )
        }
    }

    fn member(class: &str, names: &[&str]) -> pbps_model::ProviderMember {
        pbps_model::ProviderMember {
            class: class.into(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            args: Vec::new(),
        }
    }

    fn facts(
        name: &str,
        outside: &[pbps_model::ProviderMember],
        blockers: &[pbps_model::ProviderMember],
    ) -> std::collections::BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionDropFacts>
    {
        [(
            name.into(),
            pbps_model::ExtensionDropFacts {
                outside: outside.iter().cloned().collect(),
                blockers: blockers.iter().cloned().collect(),
            },
        )]
        .into()
    }

    /// A recorded package a tombstone retires is one `DROP EXTENSION` carrying
    /// its definition as the base held it; nothing else is a drop (DEC-2447.8).
    #[test]
    fn only_a_package_a_recorded_intent_retired_is_dropped() {
        let w = world(&["citext", "hstore"], &["citext"], &[]);
        let (changes, errors) = w.drops(&facts("citext", &[], &[]));
        assert!(errors.is_empty(), "{errors:?}");
        let [
            pbps_model::Change::DropExtension {
                uid: u,
                name,
                definition,
                requirers,
                releases,
            },
        ] = changes.as_slice()
        else {
            panic!("{changes:?}")
        };
        assert_eq!(
            (u, name.raw(), definition.version.as_str()),
            (&uid("citext"), "citext", "1.0")
        );
        assert!(requirers.is_empty() && releases.is_empty());
        // Negative: the package that stays, and one removed with no tombstone,
        // are not drops, and the latter is `differences`' refusal.
        let mut untombstoned = world(&["citext"], &["citext"], &[]);
        untombstoned.desired_ids.tombstones.clear();
        let (changes, errors) = untombstoned.drops(&facts("citext", &[], &[]));
        assert!(changes.is_empty() && errors.is_empty());
        let refused = differences(
            Side {
                schema: &untombstoned.base,
                ids: &untombstoned.base_ids,
            },
            Side {
                schema: &untombstoned.desired,
                ids: &untombstoned.desired_ids,
            },
            "postgres",
        );
        assert!(
            refused[0].to_string().contains("without a recorded intent"),
            "{refused:?}"
        );
        let tombstoned = differences(
            Side {
                schema: &w.base,
                ids: &w.base_ids,
            },
            Side {
                schema: &w.desired,
                ids: &w.desired_ids,
            },
            "postgres",
        );
        assert!(tombstoned.is_empty(), "{tombstoned:?}");
        // The strict variant for bootstrap callers refuses a drop whatever.
        let strict = prerequisite_differences(
            Side {
                schema: &w.base,
                ids: &w.base_ids,
            },
            Side {
                schema: &w.desired,
                ids: &w.desired_ids,
            },
            "postgres",
        );
        assert!(
            strict[0].to_string().contains("DROP EXTENSION"),
            "{strict:?}"
        );
    }

    /// With no facts from a database a drop is refused by name, and on another
    /// dialect there is no drop at all.
    #[test]
    fn a_drop_needs_the_targets_facts() {
        let w = world(&["citext"], &["citext"], &[]);
        let (changes, errors) = w.drops(&Default::default());
        assert!(changes.is_empty());
        assert!(errors[0].to_string().contains("plan --db"), "{errors:?}");
        let (changes, errors) = drops(
            Side {
                schema: &w.base,
                ids: &w.base_ids,
            },
            Side {
                schema: &w.desired,
                ids: &w.desired_ids,
            },
            "mssql",
            &facts("citext", &[], &[]),
        );
        assert!(changes.is_empty() && errors.is_empty());
    }

    /// What the engine would delete with the package is carried only where this
    /// plan drops the module, and refused by name otherwise; a module is
    /// recognised by its class and name, however the engine spells its types.
    #[test]
    fn what_the_drop_reaches_is_carried_only_when_the_plan_removes_it() {
        let w = world(&["citext"], &["citext"], &["marked"]);
        let marked = pbps_model::ProviderMember {
            class: "function".into(),
            names: vec!["app".into(), "marked".into()],
            args: Vec::new(),
        };
        let (changes, errors) = w.drops(&facts("citext", std::slice::from_ref(&marked), &[]));
        assert!(errors.is_empty(), "{errors:?}");
        let [pbps_model::Change::DropExtension { releases, .. }] = changes.as_slice() else {
            panic!("{changes:?}")
        };
        assert_eq!(releases.iter().collect::<Vec<_>>(), [&routine("marked")]);
        // Negative: the same function when the plan keeps it, an index, and a
        // default value: refused, each by name, all in one message.
        let kept = world(&["citext"], &["citext"], &[]);
        let (changes, errors) = kept.drops(&facts(
            "citext",
            &[marked.clone(), member("index", &["app", "x_idx"])],
            &[member("default value", &["app", "uses", "n"])],
        ));
        assert!(changes.is_empty());
        let text = errors[0].to_string();
        for named in [
            "function app.marked",
            "index app.x_idx",
            "default value app.uses.n",
        ] {
            assert!(text.contains(named), "{named}: {text}");
        }
        // A view over a member is the rule the engine names, and is carried
        // when the plan drops that view.
        let mut with_view = world(&["citext"], &["citext"], &[]);
        with_view.base.modules.insert(
            "app.over".parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            },
        );
        let rule = member("rule", &["app", "over", "_RETURN"]);
        let (changes, errors) = with_view.drops(&facts("citext", &[], &[rule]));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            matches!(changes.as_slice(), [pbps_model::Change::DropExtension { releases, .. }] if releases.len() == 1)
        );
    }

    /// A package another package requires goes after the requirer when both
    /// are dropped, and is refused when the requirer stays.
    #[test]
    fn a_required_package_is_dropped_after_its_requirer_or_not_at_all() {
        let required_by = |n: &str| member("extension", &[n]);
        let both = world(&["aaa_dep", "zzz_base"], &["aaa_dep", "zzz_base"], &[]);
        let mut all = facts("zzz_base", &[], &[required_by("aaa_dep")]);
        all.extend(facts("aaa_dep", &[], &[]));
        let (changes, errors) = both.drops(&all);
        assert!(errors.is_empty(), "{errors:?}");
        let by_name = |wanted: &str| {
            changes.iter().find_map(|c| {
                if let pbps_model::Change::DropExtension {
                    name, requirers, ..
                } = c
                    && name.raw() == wanted
                {
                    Some(requirers.clone())
                } else {
                    None
                }
            })
        };
        assert_eq!(by_name("zzz_base").unwrap(), [uid("aaa_dep")].into());
        assert!(by_name("aaa_dep").unwrap().is_empty());
        // Negative: the requirer stays.
        let alone = world(&["aaa_dep", "zzz_base"], &["zzz_base"], &[]);
        let (changes, errors) = alone.drops(&facts("zzz_base", &[], &[required_by("aaa_dep")]));
        assert!(changes.is_empty());
        assert!(
            errors[0]
                .to_string()
                .contains("package `aaa_dep` requires it and stays"),
            "{errors:?}"
        );
        // The draft rank keeps the requirer ahead: depth counts the other way.
        let ranks = depths(changes_of(&both, &all).iter());
        assert!(
            ranks[&uid("zzz_base")] > ranks[&uid("aaa_dep")],
            "{ranks:?}"
        );
    }

    fn changes_of(
        w: &World,
        f: &std::collections::BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionDropFacts>,
    ) -> Vec<pbps_model::Change> {
        w.drops(f).0
    }

    fn ctx() -> crate::Context {
        crate::Context {
            operator: "test".into(),
            today: "2026-11-02".into(),
        }
    }

    fn drop_intent(name: &str) -> pbps_model::Intent {
        pbps_model::Intent::DropExtension {
            name: name.into(),
            reason: "retired for good".parse().unwrap(),
        }
    }

    /// A recorded intent turns the uid of a package the declarations dropped
    /// into a tombstone naming the reason and the operator, and nothing else
    /// does: with no intent the uid is forgotten as before, and the baseline
    /// check says a reason is missing (DEC-2447.8).
    #[test]
    fn a_drop_intent_becomes_a_tombstone_and_a_removal_without_one_is_a_blocker() {
        let w = world(&["citext"], &[], &[]);
        let declared = Schema::default();
        let resolved =
            crate::resolve(&declared, &w.base_ids, &[drop_intent("citext")], &ctx()).unwrap();
        let tombstone = &resolved.ids.tombstones[&uid("citext")];
        assert_eq!(
            (
                tombstone.was.as_str(),
                tombstone.reason.as_str(),
                tombstone.operator.as_str()
            ),
            ("citext", "retired for good", "test")
        );
        assert!(resolved.ids.extensions.is_empty());
        assert_eq!(
            resolved.dropped_extensions,
            [(uid("citext"), "citext".into())]
        );
        // No intent: forgotten, no tombstone, and the baseline names the removal.
        let forgotten = crate::resolve(&declared, &w.base_ids, &[], &ctx()).unwrap();
        assert!(forgotten.ids.extensions.is_empty() && forgotten.ids.tombstones.is_empty());
        let blockers = crate::identity::resolve_with_baseline(
            &declared,
            &w.base_ids,
            &[],
            0,
            Side {
                schema: &w.base,
                ids: &w.base_ids,
            },
            &ctx(),
        )
        .unwrap_err();
        assert!(
            matches!(blockers.as_slice(), [crate::identity::Blocker::DropExtensionNeedsReason { name }] if name.raw() == "citext"),
            "{blockers:?}"
        );
        // With the intent the same call succeeds.
        let ok = crate::identity::resolve_with_baseline(
            &declared,
            &w.base_ids,
            &[drop_intent("citext")],
            0,
            Side {
                schema: &w.base,
                ids: &w.base_ids,
            },
            &ctx(),
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    /// An intent that names a package still declared, or one that was never
    /// there, matches nothing and is named as a probable typo; one already
    /// recorded is absorbed, so a rerun is not refused.
    #[test]
    fn a_drop_intent_that_matches_nothing_is_a_typo_and_a_recorded_one_is_absorbed() {
        let w = world(&["citext", "hstore"], &[], &[]);
        let mut declared = Schema::default();
        declared.extensions.insert(
            "hstore".into(),
            Extension {
                schema: "ext".into(),
                version: "1.0".into(),
            },
        );
        // `hstore` is still declared; `nosuch` was never recorded.
        for named in ["hstore", "nosuch"] {
            let blockers =
                crate::resolve(&declared, &w.base_ids, &[drop_intent(named)], &ctx()).unwrap_err();
            assert!(
                blockers
                    .iter()
                    .any(|b| matches!(b, crate::identity::Blocker::UnusedIntent { .. })),
                "{named}: {blockers:?}"
            );
        }
        // Recorded once, the same intent on the next run is absorbed.
        let first =
            crate::resolve(&declared, &w.base_ids, &[drop_intent("citext")], &ctx()).unwrap();
        let again = crate::resolve(&declared, &first.ids, &[drop_intent("citext")], &ctx());
        assert!(again.is_ok(), "{again:?}");
        // ... unless the name is declared again: that is a new incarnation.
        declared.extensions.insert(
            "citext".into(),
            Extension {
                schema: "ext".into(),
                version: "1.0".into(),
            },
        );
        let reused =
            crate::resolve(&declared, &first.ids, &[drop_intent("citext")], &ctx()).unwrap_err();
        assert!(
            reused
                .iter()
                .any(|b| matches!(b, crate::identity::Blocker::UnusedIntent { .. }))
        );
    }
    /// A version change under one recorded identity is an update that waits for
    /// the packages of the plan its target version requires and for the modules
    /// the plan drops that use a member; what it does not drop is no refusal,
    /// because a kept dependent is the ordinary case, and anything that is not a
    /// version change under one identity is no update at all.
    #[test]
    fn a_version_change_is_an_update_that_waits_for_what_it_must_and_nothing_else_is() {
        let at = |schema: &str, version: &str| Extension {
            schema: schema.into(),
            version: version.into(),
        };
        let mut base = Schema {
            extensions: [("citext".into(), at("ext", "1.4"))].into(),
            ..Default::default()
        };
        for dropped in ["app.marked()", "app.xmarked()"] {
            base.modules.insert(
                dropped.parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "() RETURNS int LANGUAGE sql RETURN 1".into(),
                    options: Default::default(),
                },
            );
        }
        let desired = Schema {
            extensions: [
                ("citext".into(), at("ext", "1.6")),
                ("cube".into(), at("ext", "1.5")),
            ]
            .into(),
            ..Default::default()
        };
        let ids = |names: &[&str]| IdsFile {
            extensions: names.iter().map(|n| (uid(n), (*n).into())).collect(),
            ..Default::default()
        };
        let (base_ids, desired_ids) = (ids(&["citext"]), ids(&["citext", "cube"]));
        let member = |class: &str, names: &[&str]| pbps_model::ProviderMember {
            class: class.into(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            args: Vec::new(),
        };
        let facts: std::collections::BTreeMap<_, _> = [(
            "citext".into(),
            pbps_model::ExtensionUpdateFacts {
                requires: ["cube".into(), "installed".into()].into(),
                reach: pbps_model::ExtensionDropFacts {
                    outside: [member("function", &["app", "xmarked"])].into(),
                    blockers: [
                        member("function", &["app", "marked"]),
                        member("default value", &["app", "kept", "n"]),
                    ]
                    .into(),
                },
            },
        )]
        .into();
        type Facts = std::collections::BTreeMap<
            pbps_model::ObjectLocalName,
            pbps_model::ExtensionUpdateFacts,
        >;
        fn run(
            base: &Schema,
            base_ids: &IdsFile,
            desired: &Schema,
            desired_ids: &IdsFile,
            facts: &Facts,
        ) -> (Vec<pbps_model::Change>, Vec<DiffError>) {
            updates(
                Side {
                    schema: base,
                    ids: base_ids,
                },
                Side {
                    schema: desired,
                    ids: desired_ids,
                },
                "postgres",
                facts,
            )
        }
        let (changes, errors) = run(&base, &base_ids, &desired, &desired_ids, &facts);
        assert!(errors.is_empty(), "{errors:?}");
        let [
            pbps_model::Change::UpdateExtension {
                from,
                to,
                requires,
                releases,
                ..
            },
        ] = changes.as_slice()
        else {
            panic!("{changes:?}")
        };
        assert_eq!((from.version.as_str(), to.version.as_str()), ("1.4", "1.6"));
        // The package this plan creates, not the one already installed; the
        // module this plan drops, not the default value it keeps.
        assert_eq!(requires.iter().collect::<Vec<_>>(), [&uid("cube")]);
        assert_eq!(
            releases.iter().collect::<Vec<_>>(),
            [&routine("marked"), &routine("xmarked")]
        );

        // Negatives, each producing no update: no facts (offline) is named; the
        // same version; another identity; another dialect.
        let (none, errors) = run(
            &base,
            &base_ids,
            &desired,
            &desired_ids,
            &Default::default(),
        );
        assert!(none.is_empty());
        let text = errors[0].to_string();
        assert!(
            text.contains("from `1.4` to `1.6`") && text.contains("only against a database"),
            "{text}"
        );
        let same = Schema {
            extensions: [("citext".into(), at("ext", "1.4"))].into(),
            ..Default::default()
        };
        assert!(
            run(&base, &base_ids, &same, &desired_ids, &facts)
                .0
                .is_empty()
        );
        // Beside a new schema the update still runs where the package is; the
        // move is `moves`' and starts from the version the update leaves.
        let moved = Schema {
            extensions: [("citext".into(), at("other", "1.6"))].into(),
            ..Default::default()
        };
        let (beside, _) = run(&base, &base_ids, &moved, &desired_ids, &facts);
        let [pbps_model::Change::UpdateExtension { to, .. }] = beside.as_slice() else {
            panic!("{beside:?}")
        };
        assert_eq!((to.schema.as_str(), to.version.as_str()), ("ext", "1.6"));
        assert!(
            run(&base, &base_ids, &desired, &ids(&["other"]), &facts)
                .0
                .is_empty()
        );
        // The same name under another recorded identity is a replacement, not
        // an update.
        let replaced = IdsFile {
            extensions: [(uid("elsewhere"), "citext".into())].into(),
            ..Default::default()
        };
        assert!(
            run(&base, &base_ids, &desired, &replaced, &facts)
                .0
                .is_empty()
        );
        assert!(
            run(
                &base,
                &IdsFile::default(),
                &desired,
                &IdsFile::default(),
                &facts
            )
            .0
            .is_empty()
        );
        let (none, errors) = updates(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &desired,
                ids: &desired_ids,
            },
            "mssql",
            &facts,
        );
        assert!(none.is_empty() && errors.is_empty());
        // The package's own prerequisite ranks it behind the create.
        let rank = depths(
            changes.iter().chain(
                creates(
                    Side {
                        schema: &base,
                        ids: &base_ids,
                    },
                    Side {
                        schema: &desired,
                        ids: &desired_ids,
                    },
                    "postgres",
                    &[("cube".into(), Default::default())].into(),
                )
                .iter(),
            ),
        );
        assert_eq!(rank[&uid("citext")], 1);
        assert_eq!(rank[&uid("cube")], 0);
    }

    /// The read-back of a table the plan does not touch is carried through the
    /// plan's moves only where it shows the type moved: a scalar and an array
    /// follow, a type that stayed in the schema does not, a name that differs
    /// does not, and with no move nothing does (DEC-2447.12).
    #[test]
    fn a_read_back_column_is_carried_only_where_it_shows_the_type_moved() {
        use pbps_model::{Column, ColumnType, Table, TypeRef};
        let reference = |name: &str| ColumnType::reference(name.parse::<TypeRef>().unwrap());
        let table = |columns: &[(&str, ColumnType)]| {
            let mut table = Table::default();
            for (name, ty) in columns {
                table
                    .columns
                    .insert((*name).into(), Column::new(ty.clone()));
            }
            table
        };
        let before = table(&[
            ("c", reference("ext.citext")),
            ("ca", reference("ext.citext").into_array()),
            ("h", reference("ext.hstore")),
            ("renamed", reference("ext.citext")),
            ("shape", reference("ext.citext")),
            ("t", "text".parse().unwrap()),
        ]);
        let after = table(&[
            ("c", reference("moved.citext")),
            ("ca", reference("moved.citext").into_array()),
            ("h", reference("ext.hstore")),
            ("renamed", reference("moved.other")),
            ("shape", reference("moved.citext").into_array()),
            ("t", "text".parse().unwrap()),
        ]);
        let mut carried = before.clone();
        forward_schema_moves(&mut carried, &after, &[("ext", "moved")]);
        let ty = |t: &Table, c: &str| t.columns[c].ty.raw().to_string();
        assert_eq!(ty(&carried, "c"), "moved.citext");
        assert_eq!(ty(&carried, "ca"), "moved.citext[]");
        // Negative: it stayed, another name, scalar became an array, not a reference.
        assert_eq!(ty(&carried, "h"), "ext.hstore");
        assert_eq!(ty(&carried, "renamed"), "ext.citext");
        assert_eq!(ty(&carried, "shape"), "ext.citext");
        assert_eq!(ty(&carried, "t"), "text");
        // Negative: no move, or a move of other schemas, carries nothing.
        let mut same = before.clone();
        forward_schema_moves(&mut same, &after, &[]);
        assert_eq!(same, before);
        forward_schema_moves(&mut same, &after, &[("a", "b")]);
        assert_eq!(same, before);
    }

    /// A moved package's columns are carried to its new schema before they are
    /// compared, so that the move does not retype them (DEC-2447.12): a scalar
    /// and an array of a member type follow it, a type that is not a member and
    /// a type in another schema do not, and a package that does not move, has no
    /// facts or is another identity forwards nothing.
    #[test]
    fn a_moved_packages_columns_are_carried_and_nothing_else_is() {
        use pbps_model::{Column, ColumnType, Table, TypeRef};
        let at = |schema: &str| Extension {
            schema: schema.into(),
            version: "1.6".into(),
        };
        let column = |ty: ColumnType| Column::new(ty);
        let reference = |name: &str| ColumnType::reference(name.parse::<TypeRef>().unwrap());
        let mut table = Table::default();
        for (name, ty) in [
            ("c", reference("ext.citext")),
            ("ca", reference("ext.citext").into_array()),
            ("other", reference("ext.other")),
            ("elsewhere", reference("app.citext")),
            ("t", "text".parse().unwrap()),
        ] {
            table.columns.insert(name.into(), column(ty));
        }
        let name = pbps_model::TableName::new("app", "wants");
        let base = Schema {
            extensions: [("citext".into(), at("ext"))].into(),
            tables: [(name.clone(), table)].into(),
            ..Default::default()
        };
        let desired = Schema {
            extensions: [("citext".into(), at("moved"))].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            extensions: [(uid("citext"), "citext".into())].into(),
            ..Default::default()
        };
        let member = |name: &str| pbps_model::ProviderMember {
            class: "type".into(),
            names: vec![name.to_owned()],
            args: Vec::new(),
        };
        let facts: std::collections::BTreeMap<_, _> = [(
            "citext".into(),
            [member("moved.citext"), member("moved.citext[]")].into(),
        )]
        .into();
        let run = |base: &Schema, desired: &Schema, ids_desired: &IdsFile, facts: &_| {
            forward_moved_types(
                std::borrow::Cow::Borrowed(base),
                Side {
                    schema: base,
                    ids: &ids,
                },
                Side {
                    schema: desired,
                    ids: ids_desired,
                },
                facts,
            )
            .into_owned()
        };
        let carried = run(&base, &desired, &ids, &facts);
        let ty = |schema: &Schema, column: &str| {
            schema.tables[&name].columns[column].ty.raw().to_string()
        };
        assert_eq!(ty(&carried, "c"), "moved.citext");
        assert_eq!(ty(&carried, "ca"), "moved.citext[]");
        // Negative: not a member, another schema, not a reference.
        assert_eq!(ty(&carried, "other"), "ext.other");
        assert_eq!(ty(&carried, "elsewhere"), "app.citext");
        assert_eq!(ty(&carried, "t"), "text");
        // Negative: the package does not move, has no facts, or is another one.
        let stays = Schema {
            extensions: [("citext".into(), at("ext"))].into(),
            ..Default::default()
        };
        assert_eq!(run(&base, &stays, &ids, &facts), base);
        assert_eq!(run(&base, &desired, &ids, &Default::default()), base);
        let other = IdsFile {
            extensions: [(uid("hstore"), "citext".into())].into(),
            ..Default::default()
        };
        assert_eq!(run(&base, &desired, &other, &facts), base);
    }

    /// A schema change under one recorded identity is a move that starts from the
    /// version this plan's update leaves, waits for the modules the plan drops
    /// that hold a name a member would take in the destination, and nothing else
    /// is one: no facts is named, the same schema, another identity and another
    /// dialect plan nothing (DEC-2447.11).
    #[test]
    fn a_schema_change_is_a_move_that_waits_for_the_names_it_takes_and_nothing_else_is() {
        let at = |schema: &str, version: &str| Extension {
            schema: schema.into(),
            version: version.into(),
        };
        let mut base = Schema {
            extensions: [("citext".into(), at("ext", "1.4"))].into(),
            ..Default::default()
        };
        base.modules.insert(routine("taken"), module());
        base.modules.insert(routine("kept"), module());
        let desired = Schema {
            extensions: [("citext".into(), at("app", "1.4"))].into(),
            modules: [(routine("kept"), module())].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            extensions: [(uid("citext"), "citext".into())].into(),
            ..Default::default()
        };
        let member = |names: &[&str]| pbps_model::ProviderMember {
            class: "function".into(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            args: Vec::new(),
        };
        // `app.taken()` is dropped by the plan, `app.kept()` stays, and
        // `app.free()` is nothing the plan manages.
        let facts: std::collections::BTreeMap<_, _> = [(
            "citext".into(),
            [
                member(&["app", "taken"]),
                member(&["app", "kept"]),
                member(&["app", "free"]),
            ]
            .into(),
        )]
        .into();
        let run = |base: &Schema, desired: &Schema, facts: &_, dialect: &str| {
            moves(
                Side {
                    schema: base,
                    ids: &ids,
                },
                Side {
                    schema: desired,
                    ids: &ids,
                },
                dialect,
                facts,
            )
        };
        let (changes, errors) = run(&base, &desired, &facts, "postgres");
        assert!(errors.is_empty(), "{errors:?}");
        let [
            pbps_model::Change::SetExtensionSchema {
                from, to, releases, ..
            },
        ] = changes.as_slice()
        else {
            panic!("{changes:?}")
        };
        assert_eq!((from.schema.as_str(), to.schema.as_str()), ("ext", "app"));
        assert_eq!(from.version, to.version);
        assert_eq!(releases.iter().collect::<Vec<_>>(), [&routine("taken")]);

        // Beside a version change the move starts from the version the update
        // leaves, so the two statements chain.
        let newer = Schema {
            extensions: [("citext".into(), at("app", "1.6"))].into(),
            ..desired.clone()
        };
        let (beside, _) = run(&base, &newer, &facts, "postgres");
        let [pbps_model::Change::SetExtensionSchema { from, to, .. }] = beside.as_slice() else {
            panic!("a move beside an update: {beside:?}")
        };
        assert_eq!((from.version.as_str(), to.version.as_str()), ("1.6", "1.6"));
        assert_eq!(from.schema, "ext");

        // Negatives: offline is named, the same schema, another identity and
        // another dialect plan nothing.
        let (none, errors) = run(&base, &desired, &Default::default(), "postgres");
        assert!(none.is_empty());
        let text = errors[0].to_string();
        assert!(
            text.contains("from `ext` to `app`") && text.contains("only against a database"),
            "{text}"
        );
        let same = Schema {
            extensions: [("citext".into(), at("ext", "1.4"))].into(),
            ..desired.clone()
        };
        assert!(run(&base, &same, &facts, "postgres").0.is_empty());
        let other = IdsFile {
            extensions: [(uid("elsewhere"), "citext".into())].into(),
            ..Default::default()
        };
        let (none, errors) = moves(
            Side {
                schema: &base,
                ids: &ids,
            },
            Side {
                schema: &desired,
                ids: &other,
            },
            "postgres",
            &facts,
        );
        assert!(none.is_empty() && errors.is_empty());
        let (none, errors) = run(&base, &desired, &facts, "mssql");
        assert!(none.is_empty() && errors.is_empty());
    }
}
