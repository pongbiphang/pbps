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
            if let pbps_model::Change::CreateExtension { uid, requires, .. } = c {
                Some((uid, requires))
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
            (Some(_), None) => refuse(name, "DROP EXTENSION".into()),
            (Some(old), Some(new)) => {
                if old.version != new.version {
                    refuse(
                        name,
                        format!(
                            "ALTER EXTENSION UPDATE from `{}` to `{}`",
                            pbps_model::escape::Escaped(&old.version),
                            pbps_model::escape::Escaped(&new.version)
                        ),
                    );
                }
                if old.schema != new.schema {
                    refuse(
                        name,
                        format!(
                            "ALTER EXTENSION SET SCHEMA from `{}` to `{}`",
                            pbps_model::escape::Escaped(&old.schema),
                            pbps_model::escape::Escaped(&new.schema)
                        ),
                    );
                }
                if base.ids.extension_uid(name) != desired.ids.extension_uid(name) {
                    refuse(name, "replace the recorded extension identity".into());
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
        for (changed, action) in [
            (Schema::default(), "DROP EXTENSION"),
            (
                Schema {
                    extensions: [(
                        "citext".into(),
                        Extension {
                            schema: "ext".into(),
                            version: "1.6".into(),
                        },
                    )]
                    .into(),
                    ..Default::default()
                },
                "UPDATE",
            ),
            (
                Schema {
                    extensions: [(
                        "citext".into(),
                        Extension {
                            schema: "other".into(),
                            version: "1.4".into(),
                        },
                    )]
                    .into(),
                    ..Default::default()
                },
                "SET SCHEMA",
            ),
        ] {
            let found = differences(
                side,
                Side {
                    schema: &changed,
                    ids: &ids,
                },
                "postgres",
            );
            assert!(
                found
                    .iter()
                    .any(|e| e.to_string().contains(action) && e.to_string().contains("citext")),
                "{found:?}"
            );
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
}
