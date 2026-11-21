use super::*;
use pbps_dialect::MinimalDialect;
use pbps_model::ColumnChange;
use pbps_model::ManagedType;
use pbps_model::TypeChange;

#[test]
fn a_same_named_other_uid_cannot_supply_a_missing_declaration() {
    let name: pbps_model::TypeRef = "app.mood".parse().unwrap();
    let base = Schema {
        types: [(
            name.clone(),
            ManagedType::Enum {
                values: vec!["old".into()],
            },
        )]
        .into(),
        ..Default::default()
    };
    let old = IdsFile {
        types: [("y_000001".parse().unwrap(), name.clone())].into(),
        ..Default::default()
    };
    let new = IdsFile {
        types: [("y_000002".parse().unwrap(), name)].into(),
        ..Default::default()
    };
    let errors = diff(
        Side {
            schema: &base,
            ids: &old,
        },
        Side {
            schema: &Schema::default(),
            ids: &new,
        },
        &MinimalDialect,
        &Hints::default(),
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("its identity remains recorded")),
        "{errors:?}"
    );
}

#[test]
fn replacement_dependents_follow_their_own_identity_on_each_side() {
    for array in [false, true] {
        for domain in [false, true] {
            let mood: pbps_model::TypeRef = "app.mood".parse().unwrap();
            let old: Uid = "y_000001".parse().unwrap();
            let new: Uid = "y_000002".parse().unwrap();
            let own: Uid = if domain { "y_000003" } else { "t_000001" }
                .parse()
                .unwrap();
            let definition = ManagedType::Enum {
                values: vec!["ok".into()],
            };
            let mut base = Schema {
                types: [(mood.clone(), definition.clone())].into(),
                ..Default::default()
            };
            let mut desired = base.clone();
            let mut base_ids = IdsFile {
                types: [(old.clone(), mood.clone())].into(),
                ..Default::default()
            };
            let mut ids = IdsFile {
                types: [(new, mood)].into(),
                ..Default::default()
            };
            let ty: ColumnType = if array { "app.mood[]" } else { "app.mood" }
                .parse()
                .unwrap();
            if domain {
                desired.types.insert(
                    "app.choice".parse().unwrap(),
                    ManagedType::Domain {
                        base: ty,
                        not_null: false,
                        default: None,
                        checks: Default::default(),
                        collation: None,
                    },
                );
                ids.types.insert(own.clone(), "app.choice".parse().unwrap());
            } else {
                desired.tables.insert(
                    "app.t".parse().unwrap(),
                    Table {
                        columns: [("v".into(), pbps_model::Column::new(ty))].into(),
                        ..Default::default()
                    },
                );
                ids.tables.insert(own.clone(), "app.t".parse().unwrap());
                ids.columns.insert(
                    "c_000001".parse().unwrap(),
                    ColumnRef {
                        table: "app.t".parse().unwrap(),
                        name: "v".into(),
                    },
                );
            }
            let run = |base: &Schema, before: &IdsFile| {
                diff(
                    Side {
                        schema: base,
                        ids: before,
                    },
                    Side {
                        schema: &desired,
                        ids: &ids,
                    },
                    &MinimalDialect,
                    &Hints::default(),
                )
            };
            let plan = run(&base, &base_ids).unwrap();
            assert!(
                plan.changes
                    .iter()
                    .any(|s| matches!(&s.change, Change::Type(TypeChange::DropType { uid, .. }) if uid == &old))
            );
            if domain {
                base.types.insert(
                    "app.choice".parse().unwrap(),
                    desired.types[&"app.choice".parse().unwrap()].clone(),
                );
                base_ids.types.insert(own, "app.choice".parse().unwrap());
            } else {
                base.tables = desired.tables.clone();
                base_ids.tables = ids.tables.clone();
                base_ids.columns = ids.columns.clone();
            }
            let errors = run(&base, &base_ids).unwrap_err();
            assert!(
                errors.iter().any(|e| e.to_string().contains("app.mood")
                    && e.to_string().contains("retained declaration")),
                "{errors:?}"
            );
            // Reusing the dependent's name with a new own uid is a new declaration.
            if domain {
                base_ids.types.remove(&"y_000003".parse().unwrap());
            } else {
                base_ids.tables.clear();
                base_ids.columns.clear();
            }
            run(&base, &base_ids).unwrap();
        }
    }
}

#[test]
fn retained_columns_follow_type_uids_instead_of_a_rename_chains_names() {
    let enumeration = ManagedType::Enum {
        values: vec!["ok".into()],
    };
    let mut base = Schema {
        types: [
            ("app.a".parse().unwrap(), enumeration.clone()),
            ("app.b".parse().unwrap(), enumeration.clone()),
        ]
        .into(),
        tables: [(
            "app.t".parse().unwrap(),
            Table {
                columns: [(
                    "v".into(),
                    pbps_model::Column::new("app.b[]".parse().unwrap()),
                )]
                .into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let mut desired = base.clone();
    desired.types.remove(&"app.a".parse().unwrap());
    desired.types.insert("app.c".parse().unwrap(), enumeration);
    desired
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["v"]
        .ty = "app.c[]".parse().unwrap();
    let ids = |a: &str, b: &str| IdsFile {
        types: [
            ("y_000001".parse().unwrap(), a.parse().unwrap()),
            ("y_000002".parse().unwrap(), b.parse().unwrap()),
        ]
        .into(),
        tables: [("t_000001".parse().unwrap(), "app.t".parse().unwrap())].into(),
        columns: [(
            "c_000001".parse().unwrap(),
            ColumnRef {
                table: "app.t".parse().unwrap(),
                name: "v".into(),
            },
        )]
        .into(),
        ..Default::default()
    };
    let before = ids("app.a", "app.b");
    let after = ids("app.b", "app.c");
    let run = |base: &Schema, desired: &Schema| {
        diff(
            Side {
                schema: base,
                ids: &before,
            },
            Side {
                schema: desired,
                ids: &after,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap()
    };
    let plan = run(&base, &desired);
    assert!(!plan.changes.iter().any(|s| matches!(
        s.change,
        Change::Column(ColumnChange::AlterColumnType { .. })
    )));
    // Changing the binding to the other uid must still emit a retype.
    base.tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["v"]
        .ty = "app.a[]".parse().unwrap();
    assert!(run(&base, &desired).changes.iter().any(|s| matches!(
        s.change,
        Change::Column(ColumnChange::AlterColumnType { .. })
    )));
}

#[test]
fn a_domain_base_follows_its_uid_once_through_a_rename_chain() {
    let e = ManagedType::Enum {
        values: vec!["ok".into()],
    };
    let domain = |name: &str| ManagedType::Domain {
        base: name.parse().unwrap(),
        not_null: false,
        default: None,
        checks: Default::default(),
        collation: None,
    };
    for array in ["", "[]"] {
        let base = Schema {
            types: [
                ("app.a".parse().unwrap(), e.clone()),
                ("app.b".parse().unwrap(), e.clone()),
                ("app.d".parse().unwrap(), domain(&format!("app.b{array}"))),
            ]
            .into(),
            ..Default::default()
        };
        let desired = Schema {
            types: [
                ("app.b".parse().unwrap(), e.clone()),
                ("app.c".parse().unwrap(), e.clone()),
                ("app.d".parse().unwrap(), domain(&format!("app.c{array}"))),
            ]
            .into(),
            ..Default::default()
        };
        let ids = |a: &str, b: &str| IdsFile {
            types: [
                ("y_000001".parse().unwrap(), a.parse().unwrap()),
                ("y_000002".parse().unwrap(), b.parse().unwrap()),
                ("y_000003".parse().unwrap(), "app.d".parse().unwrap()),
            ]
            .into(),
            ..Default::default()
        };
        diff(
            Side {
                schema: &base,
                ids: &ids("app.a", "app.b"),
            },
            Side {
                schema: &desired,
                ids: &ids("app.b", "app.c"),
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        let mut wrong = desired.clone();
        wrong
            .types
            .insert("app.d".parse().unwrap(), domain(&format!("app.b{array}")));
        assert!(
            diff(
                Side {
                    schema: &base,
                    ids: &ids("app.a", "app.b")
                },
                Side {
                    schema: &wrong,
                    ids: &ids("app.b", "app.c")
                },
                &MinimalDialect,
                &Hints::default()
            )
            .is_err()
        );
    }
}
