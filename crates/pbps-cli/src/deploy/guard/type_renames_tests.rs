use super::super::{Settled, refuse_unplanned_movement};
use pbps_model::{
    Change, ChangeSet, Column, Generated, Index, IndexColumn, IndexKey, ManagedType, Module,
    ModuleKind, PlannedChange, RowPolicy, Schema, Table, ValueSource,
};

fn observed(ty: &str) -> Schema {
    let cast = format!("'ok'::{ty}");
    let check = format!("v <> 'sad'::{ty}");
    let mut column = Column::new(ty.parse().unwrap());
    column.value_source = ValueSource::Default(cast.clone());
    let mut generated = Column::new(ty.parse().unwrap());
    generated.value_source = ValueSource::Generated(Generated {
        expression: cast.clone(),
        stored: true,
    });
    let table = Table {
        columns: [("v".into(), column), ("g".into(), generated)].into(),
        checks: [(
            "allowed".into(),
            pbps_model::CheckConstraint {
                expression: check.clone(),
            },
        )]
        .into(),
        indexes: [(
            "ix".into(),
            Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Expression(format!("v = {cast}")),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: Some(check.clone()),
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        )]
        .into(),
        row_policies: [(
            "p".into(),
            RowPolicy {
                using: Some(check.clone().into()),
                with_check: Some(format!("v = {cast}").into()),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    Schema {
        types: [
            (
                ty.parse().unwrap(),
                ManagedType::Enum {
                    values: vec!["sad".into(), "ok".into()],
                },
            ),
            (
                "app.kept".parse().unwrap(),
                ManagedType::Domain {
                    base: ty.parse().unwrap(),
                    not_null: false,
                    default: Some(cast.clone()),
                    checks: [("allowed".into(), format!("VALUE <> 'sad'::{ty}"))].into(),
                    collation: None,
                },
            ),
        ]
        .into(),
        tables: [
            ("app.t".parse().unwrap(), table),
            (
                "app.part".parse().unwrap(),
                Table {
                    partition_of: Some(pbps_model::PartitionOf {
                        parent: "app.t".parse().unwrap(),
                        bound: pbps_model::PartitionBound::Default,
                        columns: [(
                            "v".into(),
                            pbps_model::PartitionColumn {
                                default: Some(cast.clone()),
                                not_null: false,
                            },
                        )]
                        .into(),
                    }),
                    ..Default::default()
                },
            ),
        ]
        .into(),
        modules: [(
            "app.v".parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: format!("SELECT {cast} AS v"),
                options: Default::default(),
            },
        )]
        .into(),
        ..Default::default()
    }
}

#[test]
fn approved_type_moves_preserve_every_catalog_expression_and_refuse_real_edits() {
    let before = observed("app.mood");
    let pg = pbps_pg::Postgres::new();
    for to in ["app.feeling", "moved.mood", "moved.feeling"] {
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::RenameType {
                uid: "y_000001".parse().unwrap(),
                from: "app.mood".parse().unwrap(),
                to: to.parse().unwrap(),
                definition: Box::new(before.types[&"app.mood".parse().unwrap()].clone()),
            })],
        };
        let after = observed(to);
        let check = |after: &Schema| {
            refuse_unplanned_movement(&pg, &changes, &before, after, "test", Settled::Closing)
        };
        check(&after).unwrap();
        // Each measured text site must still notice an edit in the same window.
        for site in 0..11 {
            let mut drift = after.clone();
            let table = drift.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
            match site {
                0 => table.columns["v"].value_source = ValueSource::Default(format!("'sad'::{to}")),
                1 => {
                    let ValueSource::Generated(g) = &mut table.columns["g"].value_source else {
                        unreachable!()
                    };
                    g.expression = format!("'sad'::{to}");
                }
                2 => table.checks.get_mut("allowed").unwrap().expression = "v IS NOT NULL".into(),
                3 => table.indexes.get_mut("ix").unwrap().filter = Some("v IS NOT NULL".into()),
                4 => {
                    table.indexes.get_mut("ix").unwrap().columns[0].key =
                        IndexKey::Expression("v IS NULL".into())
                }
                5 => table.row_policies.get_mut("p").unwrap().using = Some("v IS NULL".into()),
                6 => table.row_policies.get_mut("p").unwrap().with_check = Some("v IS NULL".into()),
                7 => {
                    drift
                        .modules
                        .get_mut(&"app.v".parse().unwrap())
                        .unwrap()
                        .definition = format!("SELECT 'sad'::{to} AS v")
                }
                8 => {
                    let ManagedType::Domain { default, .. } =
                        drift.types.get_mut(&"app.kept".parse().unwrap()).unwrap()
                    else {
                        unreachable!()
                    };
                    *default = Some(format!("'sad'::{to}"));
                }
                9 => {
                    drift
                        .tables
                        .get_mut(&"app.part".parse().unwrap())
                        .unwrap()
                        .partition_of
                        .as_mut()
                        .unwrap()
                        .columns
                        .get_mut("v")
                        .unwrap()
                        .default = Some(format!("'sad'::{to}"))
                }
                10 => {
                    let ManagedType::Domain { checks, .. } =
                        drift.types.get_mut(&"app.kept".parse().unwrap()).unwrap()
                    else {
                        unreachable!()
                    };
                    checks.insert("allowed".into(), "VALUE IS NOT NULL".into());
                }
                _ => unreachable!(),
            }
            assert!(check(&drift).is_err(), "site {site}, {to}");
        }
    }
}

#[test]
fn type_replacement_guards_follow_final_uids_and_refuse_the_old_definition() {
    let old = ManagedType::Enum {
        values: vec!["old".into()],
    };
    let new = ManagedType::Enum {
        values: vec!["new".into()],
    };
    let name: pbps_model::TypeRef = "app.mood".parse().unwrap();
    let before = Schema {
        types: [(name.clone(), old.clone())].into(),
        ..Default::default()
    };
    let after = Schema {
        types: [(name.clone(), new.clone())].into(),
        ..Default::default()
    };
    let create = Change::CreateType {
        uid: "y_000002".parse().unwrap(),
        name: name.clone(),
        definition: Box::new(new),
    };
    let drop = Change::DropType {
        uid: "y_000001".parse().unwrap(),
        name: name.clone(),
        definition: Box::new(old),
    };
    for parts in [
        vec![drop.clone(), create.clone()],
        vec![create.clone(), drop.clone()],
    ] {
        let changes = ChangeSet {
            changes: parts.into_iter().map(PlannedChange::new).collect(),
        };
        let check = |read| {
            refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                read,
                "test",
                Settled::Closing,
            )
        };
        check(&after).unwrap();
        assert!(check(&before).is_err());
    }
}

#[test]
fn replacement_warnings_name_both_uids_and_preserve_identity_advice() {
    use super::super::type_replacement_warnings;
    let name: pbps_model::TypeRef = "app.mood".parse().unwrap();
    let old: pbps_model::Uid = "y_000001".parse().unwrap();
    let new: pbps_model::Uid = "y_000002".parse().unwrap();
    let base = pbps_model::IdsFile {
        types: [(old.clone(), name.clone())].into(),
        ..Default::default()
    };
    let desired = pbps_model::IdsFile {
        types: [(new.clone(), name.clone())].into(),
        ..Default::default()
    };
    let definition = Box::new(ManagedType::Enum {
        values: vec!["ok".into()],
    });
    let drop = PlannedChange::new(Change::DropType {
        uid: old.clone(),
        name: name.clone(),
        definition: definition.clone(),
    });
    let create = PlannedChange::new(Change::CreateType {
        uid: new,
        name: name.clone(),
        definition: definition.clone(),
    });
    let warnings = type_replacement_warnings(
        &base,
        &desired,
        &ChangeSet {
            changes: vec![drop.clone(), create.clone()],
        },
        true,
    );
    assert_eq!(warnings.len(), 1);
    let message = &warnings[0].message;
    for text in [
        "app.mood",
        "y_000001",
        "y_000002",
        "different types",
        "history does not carry over",
        "original declaration file",
        "renamed_from",
        "change in place under the same uid",
        "slice 4a",
        "slice 4b",
    ] {
        assert!(message.contains(text), "{text}: {message}");
    }
    for changes in [
        vec![drop],
        vec![create],
        vec![],
        vec![PlannedChange::new(Change::RenameType {
            uid: old,
            from: name,
            to: "app.renamed".parse().unwrap(),
            definition,
        })],
    ] {
        assert!(
            type_replacement_warnings(&base, &desired, &ChangeSet { changes }, true).is_empty()
        );
    }
}
