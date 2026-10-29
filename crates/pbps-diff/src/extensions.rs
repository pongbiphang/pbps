use crate::{Side, schema_diff::DiffError};
use std::collections::BTreeSet;

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
