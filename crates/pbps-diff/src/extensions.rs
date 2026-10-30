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

/// Refusals share the drift channel, so no executable plan can lose an
/// extension difference merely because no Change can yet express it.
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
            (None, Some(_)) => refuse(name, "CREATE EXTENSION".into()),
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
                for (before, after, action) in [
                    (absent, owned, "CREATE EXTENSION"),
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
        assert!(
            differences(
                Side {
                    schema: &empty,
                    ids: &IdsFile::default()
                },
                desired,
                "postgres"
            )[0]
            .to_string()
            .contains("CREATE EXTENSION")
        );
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
        assert!(
            differences(
                Side {
                    schema: &empty,
                    ids: &IdsFile::default()
                },
                side,
                "postgres"
            )[0]
            .to_string()
            .contains("CREATE EXTENSION")
        );
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
