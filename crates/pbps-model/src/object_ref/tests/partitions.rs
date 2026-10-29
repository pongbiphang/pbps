use super::*;
use crate::ValueSource;

fn tree() -> Schema {
    let by = || crate::PartitionBy {
        strategy: crate::PartitionStrategy::Range,
        columns: vec!["x".into()],
        external: false,
    };
    let mut root = Table {
        columns: ["x", "y", "z", "serial", "defaulted"]
            .into_iter()
            .map(|name| (name.into(), Column::new("integer".parse().unwrap())))
            .collect(),
        partition_by: Some(by()),
        ..Default::default()
    };
    root.columns["serial"].value_source = ValueSource::Sequence(crate::OwnedSequence::serial());
    root.columns["defaulted"].value_source = ValueSource::Default("1".into());
    let mut schema = Schema {
        tables: [("app.root".parse().unwrap(), root)].into(),
        modules: [("app.v".parse().unwrap(), module(ModuleKind::View))].into(),
        ..Default::default()
    };
    for (name, parent) in [
        ("app.a", "app.root"),
        ("app.b", "app.a"),
        ("app.leaf", "app.b"),
    ] {
        schema.tables.insert(
            name.parse().unwrap(),
            Table {
                partition_of: Some(crate::PartitionOf {
                    parent: parent.parse().unwrap(),
                    bound: crate::PartitionBound::Default,
                    columns: Default::default(),
                }),
                partition_by: (name != "app.leaf").then(by),
                ..Default::default()
            },
        );
    }
    for (table, column, default, not_null) in [
        ("app.a", "x", Some("11"), false),
        ("app.a", "y", None, true),
        ("app.b", "x", None, true),
        ("app.b", "y", Some("22"), false),
        ("app.leaf", "x", Some("33"), false),
    ] {
        schema
            .tables
            .get_mut(&table.parse().unwrap())
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .insert(
                column.into(),
                crate::PartitionColumn {
                    default: default.map(str::to_owned),
                    not_null,
                },
            );
    }
    schema
}

fn ids(schema: &Schema) -> IdsFile {
    IdsFile {
        tables: schema
            .tables
            .keys()
            .map(|name| {
                (
                    Uid::derived(crate::UidKind::Table, &name.raw().to_string(), 0),
                    name.clone(),
                )
            })
            .collect(),
        columns: schema
            .tables
            .iter()
            .filter(|(_, table)| table.partition_of.is_none())
            .flat_map(|(name, table)| {
                table.columns.keys().map(move |column| {
                    let column = name.column(column);
                    (
                        Uid::derived(crate::UidKind::Column, &column.raw().to_string(), 0),
                        column,
                    )
                })
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn multi_level_validation_merges_middle_defaults_and_not_null_without_owning_sequences() {
    let schema = tree();
    let registry = Registry::new(&schema);
    for target in [
        "column::app.leaf.x",
        "default::app.leaf.x",
        "not_null::app.leaf.x",
        "column::app.leaf.y",
        "default::app.leaf.y",
        "not_null::app.leaf.y",
        "column::app.leaf.z",
        "default::app.leaf.defaulted",
        "column::app.leaf.serial",
    ] {
        assert!(
            registry
                .validate(&dependencies(&[("app.v", &[target])]), None)
                .is_empty(),
            "{target}"
        );
        assert_eq!(
            registry.declaration(&reference(target)).unwrap(),
            &Declaration::Table("app.leaf".parse().unwrap())
        );
    }
    for target in [
        "default::app.leaf.z",
        "not_null::app.leaf.z",
        "sequence::app.leaf.serial",
    ] {
        assert!(
            matches!(
                registry
                    .validate(&dependencies(&[("app.v", &[target])]), None)
                    .as_slice(),
                [Problem::MissingPart(_)]
            ),
            "{target}"
        );
    }
    // The first middle level's x is still nullable; its child's NOT NULL cannot
    // propagate upwards. A false override on the leaf cannot relax that child.
    assert!(matches!(
        registry.declaration(&reference("not_null::app.a.x")),
        Err(Problem::MissingPart(_))
    ));
    for name in ["app.root", "app.a", "app.b", "app.leaf"] {
        assert_eq!(
            crate::role::serial_grant_columns(&name.parse().unwrap(), &schema.tables),
            ["app.root.serial".parse().unwrap()].into()
        );
    }
}

#[test]
fn multi_level_binding_uses_each_descendants_table_uid_and_the_roots_column_uid() {
    let schema = tree();
    let ids = ids(&schema);
    let owners = PlanOwners::new(
        crate::plan_names::PlanNames::of(&[], &ids).unwrap(),
        &ids,
        &ids,
    );
    let registry = Registry::new(&schema);
    let root_column = ids.column_uid(&"app.root.x".parse().unwrap()).unwrap();
    for table in ["app.a", "app.b", "app.leaf"] {
        let target = format!("column::{table}.x");
        assert!(
            ids.column_uid(&format!("{table}.x").parse().unwrap())
                .is_none()
        );
        for spelling in [Spelling::Base, Spelling::Final] {
            let bound = registry
                .bind(&reference(&target), &owners, spelling)
                .unwrap();
            assert_eq!(
                bound.root,
                BoundRoot::InheritedColumn {
                    table: ids.table_uid(&table.parse().unwrap()).unwrap().clone(),
                    column: root_column.clone(),
                }
            );
        }
    }
    let deps = dependencies(&[(
        "app.v",
        &[
            "column::app.leaf.x",
            "default::app.leaf.x",
            "not_null::app.leaf.x",
            "default::app.leaf.y",
        ],
    )]);
    assert!(
        registry
            .validate(&deps, Some((&owners, Spelling::Final)))
            .is_empty()
    );
    for target in ["default::app.leaf.x", "not_null::app.leaf.x"] {
        assert_eq!(
            registry
                .bind(&reference(target), &owners, Spelling::Final)
                .unwrap()
                .root,
            BoundRoot::InheritedColumn {
                table: ids.table_uid(&"app.leaf".parse().unwrap()).unwrap().clone(),
                column: root_column.clone()
            }
        );
    }
    // Known table identity never substitutes for a missing root column identity.
    let mut missing = ids.clone();
    missing.columns.remove(root_column);
    let owners = PlanOwners::new(
        crate::plan_names::PlanNames::of(&[], &missing).unwrap(),
        &missing,
        &missing,
    );
    assert!(matches!(
        registry.bind(&reference("column::app.leaf.x"), &owners, Spelling::Final),
        Err(Problem::Unrecorded { .. })
    ));
}

#[test]
fn multi_level_missing_ancestors_and_cycles_supply_no_partial_column_facts() {
    let good = tree();
    let ids = ids(&good);
    let owners = PlanOwners::new(
        crate::plan_names::PlanNames::of(&[], &ids).unwrap(),
        &ids,
        &ids,
    );
    for parent in ["app.missing", "app.leaf", "app.a"] {
        let mut schema = good.clone();
        schema
            .tables
            .get_mut(&"app.a".parse().unwrap())
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .parent = parent.parse().unwrap();
        let registry = Registry::new(&schema);
        assert!(
            crate::partitions::ancestor_chain(&"app.leaf".parse().unwrap(), &schema.tables)
                .is_none()
        );
        assert!(
            crate::role::serial_grant_columns(&"app.leaf".parse().unwrap(), &schema.tables)
                .is_empty()
        );
        for target in [
            "column::app.leaf.x",
            "default::app.leaf.x",
            "not_null::app.leaf.x",
        ] {
            assert!(
                matches!(
                    registry
                        .validate(&dependencies(&[("app.v", &[target])]), None)
                        .as_slice(),
                    [Problem::UnknownTarget(_)]
                ),
                "{parent}: {target}"
            );
            assert!(matches!(
                registry.bind(&reference(target), &owners, Spelling::Final),
                Err(Problem::UnknownTarget(_))
            ));
        }
    }
    let mut missing_root = good;
    missing_root.tables.remove(&"app.root".parse().unwrap());
    assert!(
        Registry::new(&missing_root)
            .declaration(&reference("column::app.leaf.x"))
            .is_err()
    );
}

#[test]
fn multi_level_overrides_cannot_create_columns_absent_from_the_root() {
    let mut schema = tree();
    schema
        .tables
        .get_mut(&"app.b".parse().unwrap())
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .insert(
            "absent".into(),
            crate::PartitionColumn {
                default: Some("7".into()),
                not_null: true,
            },
        );
    let registry = Registry::new(&schema);
    for target in [
        "column::app.leaf.absent",
        "default::app.leaf.absent",
        "not_null::app.leaf.absent",
    ] {
        assert!(matches!(
            registry.declaration(&reference(target)),
            Err(Problem::UnknownTarget(_))
        ));
    }
    assert!(
        registry
            .declaration(&reference("column::app.leaf.x"))
            .is_ok()
    );
}
