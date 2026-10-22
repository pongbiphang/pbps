use pbps_model::operation::{
    self, ModuleEdits, ObjectKey, Operations, PartKey, PlanOwners, Snapshot,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::*;

const RAW: &str = "part\nline\\slash\u{200b}";
const SHOWN: &str = r"part\nline\\slash\u{200b}";

fn local(kind: &str, text: &str) -> ObjectLocalName {
    format!("{kind}{text}").into()
}
fn shape() -> Table {
    let mut table = Table {
        columns: [("id".into(), Column::new("int".parse().unwrap()).not_null())].into(),
        primary_key: Some(PrimaryKey {
            name: Some(local("pk", RAW)),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        }),
        ..Default::default()
    };
    for text in [RAW, SHOWN] {
        table.unique.insert(
            local("uq", text),
            UniqueConstraint {
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            },
        );
        table.foreign_keys.insert(
            local("fk", text),
            ForeignKey {
                columns: vec!["id".into()],
                references_table: TableName::new("app", "parent"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        table.checks.insert(
            local("ck", text),
            CheckConstraint {
                expression: format!("id > {}", if text == RAW { 0 } else { 1 }),
            },
        );
        table.indexes.insert(
            local("ix", text),
            Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Expression("id + 1".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: Some("id > 0".into()),
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
    }
    table
}

#[test]
fn local_names_keep_raw_identity_and_string_wire_schema_and_debug() {
    let name = ObjectLocalName::from(RAW);
    assert_eq!(name.raw(), RAW);
    assert_eq!(name.shown().to_string(), SHOWN);
    assert_eq!(name.clone().into_raw(), RAW);
    assert_eq!(format!("{name:?}"), "\"part\\nline\\\\slash\\u{200b}\"");
    assert_eq!(
        serde_json::to_string(&name).unwrap(),
        serde_json::to_string(RAW).unwrap()
    );
    assert_eq!(
        serde_json::from_str::<ObjectLocalName>(&serde_json::to_string(RAW).unwrap()).unwrap(),
        name
    );
    assert_eq!(
        schemars::schema_for!(ObjectLocalName),
        schemars::schema_for!(String)
    );
    let map: std::collections::BTreeMap<ObjectLocalName, u8> =
        [(name.clone(), 1), (SHOWN.into(), 2)].into();
    assert_eq!(map.get(RAW), Some(&1));
    assert_eq!(map.get(SHOWN), Some(&2));
    assert_eq!(map.get("absent"), None);
    assert_ne!(name, ObjectLocalName::from(SHOWN));
}

#[test]
fn replica_identity_projections_escape_only_the_name_and_preserve_punctuation() {
    for (value, raw, shown) in [
        (
            ReplicaIdentity::Unique(RAW.into()),
            format!("{{unique: {RAW}}}"),
            format!("{{unique: {SHOWN}}}"),
        ),
        (
            ReplicaIdentity::Index(RAW.into()),
            format!("{{index: {RAW}}}"),
            format!("{{index: {SHOWN}}}"),
        ),
        (ReplicaIdentity::Full, "full".into(), "full".into()),
        (ReplicaIdentity::Nothing, "nothing".into(), "nothing".into()),
        (
            ReplicaIdentity::PrimaryKey,
            "primary_key".into(),
            "primary_key".into(),
        ),
    ] {
        assert_eq!(value.raw().to_string(), raw);
        assert_eq!(value.shown().to_string(), shown);
    }
    let mut table = shape();
    table.clustered = Some(Clustered::Unique(local("uq", RAW)));
    assert!(table.unique_is_clustered(&format!("uq{RAW}")));
    assert!(!table.unique_is_clustered(&format!("uq{SHOWN}")));
    table.clustered = Some(Clustered::Index(local("ix", RAW)));
    assert!(table.index_is_clustered(&format!("ix{RAW}")));
    assert!(!table.index_is_clustered(&format!("ix{SHOWN}")));
    assert_ne!(
        PartKey::from(&IndexPart::Index(RAW.into())),
        PartKey::from(&IndexPart::Unique(RAW.into()))
    );
}

#[test]
fn declared_keys_and_bindings_remove_only_the_raw_check_and_index() {
    let name = TableName::new("app", "t");
    let schema = Schema {
        tables: [(name.clone(), shape())].into(),
        ..Default::default()
    };
    let mut declared = Declared::from_schema(&schema);
    for text in [RAW, SHOWN] {
        declared
            .bindings
            .checks
            .entry(name.clone())
            .or_default()
            .insert(local("ck", text), Binding::default());
        declared
            .bindings
            .filters
            .entry(name.clone())
            .or_default()
            .insert(local("ix", text), Binding::default());
    }
    assert_eq!(declared.expressions.checks[&name].len(), 2);
    assert_eq!(declared.expressions.filters[&name].len(), 2);
    assert_eq!(declared.expressions.keys[&name].len(), 2);
    declared.advance(&ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropCheck {
                table: name.clone(),
                name: local("ck", RAW),
            }),
            PlannedChange::new(Change::DropIndex {
                table: name.clone(),
                name: local("ix", RAW),
            }),
        ],
    });
    for (keys, kind) in [
        (&declared.expressions.checks[&name], "ck"),
        (&declared.expressions.filters[&name], "ix"),
    ] {
        assert_eq!(keys.len(), 1);
        assert!(keys.contains_key(format!("{kind}{SHOWN}").as_str()));
        assert!(!keys.contains_key(format!("{kind}{RAW}").as_str()));
    }
    assert_eq!(
        declared.expressions.keys[&name].keys().collect::<Vec<_>>(),
        vec![&local("ix", SHOWN)]
    );
    assert_eq!(
        declared.bindings.checks[&name].keys().collect::<Vec<_>>(),
        vec![&local("ck", SHOWN)]
    );
    assert_eq!(
        declared.bindings.filters[&name].keys().collect::<Vec<_>>(),
        vec![&local("ix", SHOWN)]
    );
}

#[test]
fn whole_and_individual_operation_slots_claim_and_release_raw_local_names() {
    let table_name = TableName::new("app", "t");
    let uid: Uid = "t_aaaaaa".parse().unwrap();
    let ids = IdsFile {
        tables: [(uid.clone(), table_name.clone())].into(),
        ..Default::default()
    };
    let table = shape();
    let base = Schema {
        tables: [(table_name.clone(), table.clone())].into(),
        ..Default::default()
    };
    let mut changes = vec![PlannedChange::new(Change::CreateTable {
        uid,
        name: table_name.clone(),
        table: Box::new(table.clone()),
        beside_default: None,
        moves_at_most: None,
    })];
    for change in [
        Change::AddUnique {
            table: table_name.clone(),
            name: local("uq", RAW),
            constraint: table.unique[&local("uq", RAW)].clone(),
            clustered: false,
        },
        Change::DropUnique {
            table: table_name.clone(),
            name: local("uq", RAW),
        },
        Change::AddForeignKey {
            table: table_name.clone(),
            name: local("fk", RAW),
            constraint: Box::new(table.foreign_keys[&local("fk", RAW)].clone()),
        },
        Change::DropForeignKey {
            table: table_name.clone(),
            name: local("fk", RAW),
        },
        Change::AddCheck {
            table: table_name.clone(),
            name: local("ck", RAW),
            constraint: table.checks[&local("ck", RAW)].clone(),
        },
        Change::DropCheck {
            table: table_name.clone(),
            name: local("ck", RAW),
        },
        Change::AddIndex {
            table: table_name.clone(),
            name: local("ix", RAW),
            index: Box::new(table.indexes[&local("ix", RAW)].clone()),
            clustered: false,
        },
        Change::DropIndex {
            table: table_name.clone(),
            name: local("ix", RAW),
        },
    ] {
        changes.push(PlannedChange::new(change));
    }
    let empty = IdsFile::default();
    let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &empty, &ids);
    let operations = Operations::of_plan(&changes, ModuleEdits::InPlace);
    let whole = operation::names(
        &operations.as_slice()[0],
        &owners,
        Snapshot {
            schema: &base,
            ids: &ids,
        },
    )
    .unwrap();
    for (kind, prefix) in [
        (operation::NameKind::Key, "pk"),
        (operation::NameKind::Key, "uq"),
        (operation::NameKind::Constraint, "fk"),
        (operation::NameKind::Constraint, "ck"),
        (operation::NameKind::Index, "ix"),
    ] {
        assert!(
            whole
                .claims
                .iter()
                .any(|n| n.address.kind == kind && n.address.name == format!("{prefix}{RAW}")),
            "{whole:?}"
        );
    }
    for op in operations.iter().skip(1) {
        let part = op.change().constraints().unwrap();
        let raw = part.name.unwrap();
        assert!(raw.ends_with(RAW));
        let ObjectKey::Part { part: key, .. } = op.key(&owners).unwrap() else {
            panic!("part key");
        };
        let key_name = match key {
            PartKey::Unique(n) | PartKey::ForeignKey(n) | PartKey::Check(n) | PartKey::Index(n) => {
                n
            }
            other @ (PartKey::PrimaryKey
            | PartKey::Computed(_)
            | PartKey::Policy(_)
            | PartKey::RowSecurity) => panic!("{other:?}"),
        };
        assert_eq!(key_name.raw(), raw);
        let names = operation::names(
            op,
            &owners,
            Snapshot {
                schema: &base,
                ids: &ids,
            },
        )
        .unwrap();
        let addresses = names
            .claims
            .iter()
            .chain(&names.releases)
            .map(|n| n.address.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(addresses, vec![raw]);
        let effects = op.change().part_effects();
        let pbps_model::part_effects::Part::Named { name, .. } = &effects[0].part else {
            panic!("named effect");
        };
        assert_eq!(*name, raw);
    }
}

#[test]
fn collation_refusals_render_the_foreign_key_name_once() {
    let parent = TableName::new("app", "parent");
    let child = TableName::new("app", "t");
    let column = |collation| {
        let mut c = Column::new("int".parse().unwrap());
        c.collation = Some(Collation::new(collation));
        c
    };
    let mut schema = Schema {
        tables: [
            (
                parent.clone(),
                Table {
                    columns: [("id".into(), column("right"))].into(),
                    ..Default::default()
                },
            ),
            (
                child.clone(),
                Table {
                    columns: [("id".into(), column("left"))].into(),
                    foreign_keys: [(
                        RAW.into(),
                        ForeignKey {
                            columns: vec!["id".into()],
                            references_table: parent,
                            references_columns: vec!["id".into()],
                            on_delete: Default::default(),
                            on_update: Default::default(),
                        },
                    )]
                    .into(),
                    ..Default::default()
                },
            ),
        ]
        .into(),
        ..Default::default()
    };
    assert_eq!(
        schema.foreign_key_collation_problems(true),
        vec![format!(
            "app.t: foreign key `{SHOWN}` joins `id` (`left`) to app.parent.`id` (`right`); both sides of a foreign key must declare the same collation"
        )]
    );
    schema
        .tables
        .get_mut(&child)
        .unwrap()
        .columns
        .get_mut("id")
        .unwrap()
        .collation = Some(Collation::new("right"));
    assert!(schema.foreign_key_collation_problems(true).is_empty());
}

#[test]
fn detach_binding_mappings_rename_only_the_raw_check_and_index() {
    let parent = TableName::new("app", "parent");
    let child = TableName::new("app", "child");
    let mut declared = Declared::from_schema(&Schema {
        tables: [(parent.clone(), shape())].into(),
        ..Default::default()
    });
    for text in [RAW, SHOWN] {
        declared
            .bindings
            .checks
            .entry(parent.clone())
            .or_default()
            .insert(local("ck", text), Binding::default());
        declared
            .bindings
            .filters
            .entry(parent.clone())
            .or_default()
            .insert(local("ix", text), Binding::default());
    }
    let mut detached = shape();
    let check = detached.checks.remove(&local("ck", RAW)).unwrap();
    detached.checks.insert(local("new_ck", RAW), check);
    let index = detached.indexes.remove(&local("ix", RAW)).unwrap();
    detached.indexes.insert(local("new_ix", RAW), index);
    declared.advance(&ChangeSet {
        changes: vec![PlannedChange::new(Change::DetachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: child.clone(),
            parent: parent.clone(),
            names: vec![
                DetachedName {
                    kind: DetachedKind::Check,
                    parent: local("ck", RAW),
                    name: Some(local("new_ck", RAW)),
                },
                DetachedName {
                    kind: DetachedKind::Index,
                    parent: local("ix", RAW),
                    name: Some(local("new_ix", RAW)),
                },
            ],
            shape: Box::new(detached),
        })],
    });
    for (map, old, new) in [
        (&declared.bindings.checks, "ck", "new_ck"),
        (&declared.bindings.filters, "ix", "new_ix"),
    ] {
        assert_eq!(map[&child].len(), 2);
        assert!(map[&child].contains_key(&local(new, RAW)));
        assert!(map[&child].contains_key(&local(old, SHOWN)));
        assert!(!map[&child].contains_key(&local(new, SHOWN)));
        assert!(!map[&child].contains_key(&local(old, RAW)));
        assert!(map[&parent].contains_key(&local(old, RAW)));
    }
}
