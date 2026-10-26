//! One removal inventory for checkout ancestry and recorded drop intent.
//! Projection reads existing identities and explicit renames; it creates no UID
//! and does not infer that a disappeared name became an appeared one.
use pbps_model::{ColumnRef, IdsFile, Intent, RoleName, Schema, TableName, TypeRef, Uid};

/// A baseline object removed from management. SQL Server computed columns
/// have no UID (DEC-1174.1): ancestry covers them, but tombstones do not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    Table { uid: Uid, name: TableName },
    Column { uid: Uid, name: ColumnRef },
    ComputedColumn { name: ColumnRef },
    Role { uid: Uid, name: RoleName },
    Type { uid: Uid, name: TypeRef },
}

impl std::fmt::Display for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table { name, .. } => write!(f, "table {}", name.shown()),
            Self::Column { name, .. } | Self::ComputedColumn { name } => {
                write!(f, "column {}", name.shown())
            }
            Self::Role { name, .. } => write!(f, "role {}", name.shown()),
            Self::Type { name, .. } => write!(f, "type {}", name.shown()),
        }
    }
}

// Match the resolver's disappeared-to-appeared eligibility, so a retained
// annotation cannot move a new object that has reused the source name.
fn project_pending_renames<N: Clone + Ord>(
    names: &mut std::collections::BTreeMap<pbps_model::Uid, N>,
    declared: std::collections::BTreeSet<N>,
    intents: impl Iterator<Item = (N, N)>,
) {
    let known: std::collections::BTreeSet<_> = names.values().cloned().collect();
    let pending: std::collections::BTreeMap<_, _> = intents
        .filter(|(from, to)| {
            !declared.contains(from) && declared.contains(to) && !known.contains(to)
        })
        .collect();
    for name in names.values_mut() {
        if let Some(to) = pending.get(name) {
            *name = to.clone();
        }
    }
}

/// This is a projection of existing names and explicit rename annotations, not
/// identity resolution: it allocates no uid and makes no rename decision.
pub fn inventory(
    baseline: crate::Side<'_>,
    local: &IdsFile,
    schema: &Schema,
    intents: &[Intent],
) -> Vec<Removal> {
    let recorded = baseline.ids;
    let recorded_schema = baseline.schema;
    let mut names = local.clone();
    project_pending_renames(
        &mut names.tables,
        schema.tables.keys().cloned().collect(),
        intents.iter().filter_map(|i| {
            let Intent::RenameTable { from, to } = i else {
                return None;
            };
            Some((from.clone(), to.clone()))
        }),
    );
    let local_index = local.index();
    for column in names.columns.values_mut() {
        if let Some(uid) = local_index.table_uid(&column.table)
            && let Some(to) = names.tables.get(uid)
        {
            column.table = to.clone();
        }
    }
    project_pending_renames(
        &mut names.columns,
        schema
            .tables
            .iter()
            .flat_map(|(name, table)| table.columns.keys().map(|column| name.column(column)))
            .collect(),
        intents.iter().filter_map(|i| {
            let Intent::RenameColumn { table, from, to } = i else {
                return None;
            };
            Some((table.column(from), table.column(to)))
        }),
    );
    project_pending_renames(
        &mut names.roles,
        schema.roles.keys().cloned().collect(),
        intents.iter().filter_map(|i| {
            let Intent::RenameRole { from, to } = i else {
                return None;
            };
            Some((from.clone(), to.clone()))
        }),
    );
    project_pending_renames(
        &mut names.types,
        schema.types.keys().cloned().collect(),
        intents.iter().filter_map(|i| {
            let Intent::RenameType { from, to } = i else {
                return None;
            };
            Some((from.clone(), to.clone()))
        }),
    );
    let mut removed_tables = std::collections::BTreeSet::new();
    let mut removed = Vec::new();
    for (uid, name) in &recorded.tables {
        if !recorded_schema.tables.contains_key(name) {
            continue;
        }
        if !names
            .tables
            .get(uid)
            .is_some_and(|name| schema.tables.contains_key(name))
        {
            removed_tables.insert(name);
            removed.push(Removal::Table {
                uid: uid.clone(),
                name: name.clone(),
            });
        }
    }
    let declared_tables: std::collections::BTreeMap<_, _> = recorded
        .tables
        .iter()
        .filter_map(|(uid, old)| {
            let name = names.tables.get(uid)?;
            Some((old, (name, schema.tables.get(name)?)))
        })
        .collect();
    for (uid, column) in &recorded.columns {
        let attached = declared_tables
            .get(&column.table)
            .is_some_and(|(_, table)| table.partition_of.is_some());
        if !attached
            && recorded_schema
                .tables
                .get(&column.table)
                .is_some_and(|table| table.columns.contains_key(&column.name))
            && !removed_tables.contains(&column.table)
            && !names.columns.get(uid).is_some_and(|column| {
                schema
                    .tables
                    .get(&column.table)
                    .is_some_and(|table| table.columns.contains_key(&column.name))
            })
        {
            let table = declared_tables
                .get(&column.table)
                .map_or(&column.table, |(name, _)| *name);
            removed.push(Removal::Column {
                uid: uid.clone(),
                name: table.column(column.name.clone()),
            });
        }
    }
    // Computed columns have no uid; their names are their existing identity.
    for (uid, old) in &recorded.tables {
        if removed_tables.contains(old) {
            continue;
        }
        if let Some(table) = recorded_schema.tables.get(old) {
            for column in table.computed.keys() {
                if !names
                    .tables
                    .get(uid)
                    .and_then(|name| schema.tables.get(name))
                    .is_some_and(|table| {
                        table.computed.contains_key(column) || table.columns.contains_key(column)
                    })
                {
                    let name = names.tables.get(uid).unwrap_or(old);
                    removed.push(Removal::ComputedColumn {
                        name: name.column(column.clone()),
                    });
                }
            }
        }
    }
    // PostgreSQL preserves the principal, but revoking its grants and removing
    // it from management can still undo another branch's deployed application.
    for (uid, role) in &recorded.roles {
        if recorded_schema.roles.contains_key(role)
            && !names
                .roles
                .get(uid)
                .is_some_and(|name| schema.roles.contains_key(name))
        {
            removed.push(Removal::Role {
                uid: uid.clone(),
                name: role.clone(),
            });
        }
    }
    for (uid, name) in &recorded.types {
        if recorded_schema.types.contains_key(name)
            && !names
                .types
                .get(uid)
                .is_some_and(|name| schema.types.contains_key(name))
        {
            removed.push(Removal::Type {
                uid: uid.clone(),
                name: name.clone(),
            });
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ManagedType, UidKind};

    fn fixture() -> (IdsFile, Schema) {
        let mut ids = IdsFile::default();
        let mut schema = Schema::default();
        let name: TypeRef = "app.mood".parse().unwrap();
        ids.types.insert(Uid::generate(UidKind::Type), name.clone());
        schema.types.insert(
            name,
            ManagedType::Enum {
                values: vec!["one".into()],
            },
        );
        (ids, schema)
    }

    #[test]
    fn managed_type_removals_keep_the_recorded_uid_and_ignore_stale_names() {
        let (ids, schema) = fixture();
        let baseline = crate::Side {
            schema: &schema,
            ids: &ids,
        };
        let (uid, name) = ids.types.first_key_value().unwrap();
        let expected = vec![Removal::Type {
            uid: uid.clone(),
            name: name.clone(),
        }];
        assert_eq!(
            inventory(baseline, &IdsFile::default(), &Schema::default(), &[]),
            expected
        );
        assert_eq!(expected[0].to_string(), "type app.mood");
        assert!(inventory(baseline, &ids, &schema, &[]).is_empty());
        let (replacement, _) = fixture();
        assert_eq!(inventory(baseline, &replacement, &schema, &[]), expected);
        let stale = crate::Side {
            schema: &Schema::default(),
            ids: &ids,
        };
        assert!(inventory(stale, &IdsFile::default(), &Schema::default(), &[]).is_empty());
    }

    #[test]
    fn pending_type_renames_preserve_uids_and_reused_sources_remain_visible() {
        let (mut ids, mut schema) = fixture();
        let from: TypeRef = "app.mood".parse().unwrap();
        let to: TypeRef = "app.feeling".parse().unwrap();
        let intent = Intent::RenameType {
            from: from.clone(),
            to: to.clone(),
        };
        let mut desired = schema.clone();
        let definition = desired.types.remove(&from).unwrap();
        desired.types.insert(to.clone(), definition.clone());
        assert!(
            inventory(
                crate::Side {
                    schema: &schema,
                    ids: &ids
                },
                &ids,
                &desired,
                std::slice::from_ref(&intent)
            )
            .is_empty()
        );
        *ids.types.values_mut().next().unwrap() = to.clone();
        schema = desired.clone();
        assert!(
            inventory(
                crate::Side {
                    schema: &schema,
                    ids: &ids
                },
                &ids,
                &desired,
                std::slice::from_ref(&intent)
            )
            .is_empty()
        );
        let reused = Uid::generate(UidKind::Type);
        ids.types.insert(reused.clone(), from.clone());
        schema.types.insert(from.clone(), definition);
        assert_eq!(
            inventory(
                crate::Side {
                    schema: &schema,
                    ids: &ids
                },
                &ids,
                &desired,
                &[intent]
            ),
            vec![Removal::Type {
                uid: reused,
                name: from
            }]
        );
    }

    #[test]
    fn computed_removals_are_inventoried_without_requiring_uid_tombstones() {
        let mut schema = Schema::default();
        let table: TableName = "dbo.t".parse().unwrap();
        let mut definition = pbps_model::Table::default();
        definition.columns.insert(
            "id".into(),
            pbps_model::Column::new("bigint".parse().unwrap()),
        );
        definition.computed.insert(
            "total".into(),
            pbps_model::ComputedColumn {
                expression: "id + 1".into(),
                persisted: false,
                not_null: false,
            },
        );
        schema.tables.insert(table.clone(), definition);
        let context = crate::Context {
            operator: "operator".into(),
            today: "2026-10-26".into(),
        };
        let ids = super::super::resolve(&schema, &IdsFile::default(), &[], &context)
            .unwrap()
            .ids;
        let mut desired = schema.clone();
        desired.tables.get_mut(&table).unwrap().computed.clear();
        let baseline = crate::Side {
            schema: &schema,
            ids: &ids,
        };
        assert_eq!(
            inventory(baseline, &ids, &desired, &[]),
            vec![Removal::ComputedColumn {
                name: table.column("total")
            }]
        );
        let resolved =
            super::super::resolve_with_baseline(&desired, &ids, &[], 0, baseline, &context)
                .unwrap();
        assert_eq!(resolved.ids, ids);
        assert!(resolved.ids.tombstones.is_empty());
    }
}
