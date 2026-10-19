use std::collections::BTreeMap;

use pbps_model::*;

const RAW: &str = "col\nline\r\u{1b}[31m\u{9b}\\\u{200b}名前";
const SHOWN: &str = r"col\nline\r\u{1b}[31m\u{9b}\\\u{200b}名前";

#[test]
fn column_names_preserve_raw_map_keys_and_escape_exactly_once() {
    for (raw, shown) in [(RAW, SHOWN), ("名前_ünï", "名前_ünï"), ("a\\b", r"a\\b")] {
        let name = ColumnName::from(raw);
        assert_eq!(name.raw(), raw);
        assert_eq!(name.shown().to_string(), shown);
        assert_eq!(
            serde_json::to_string(&name).unwrap(),
            serde_json::to_string(raw).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<ColumnName>(&serde_json::to_string(raw).unwrap()).unwrap(),
            name
        );
        let map: BTreeMap<ColumnName, _> = [(name.clone(), 1)].into();
        let ordered: indexmap::IndexMap<ColumnName, _> = [(name, 1)].into();
        assert_eq!(map.get(raw), Some(&1));
        assert_eq!(ordered.get(raw), Some(&1));
        if raw != shown {
            assert!(!map.contains_key(shown));
            assert!(!ordered.contains_key(shown));
        }
    }
}

#[test]
fn a_column_reference_serializes_the_raw_column_identity() {
    let table = TableName::new("app", "t");
    let column = table.column(RAW);
    let raw = format!("app.t.{RAW}");
    assert_eq!(column.raw().to_string(), raw);
    assert_eq!(column.shown().to_string(), format!("app.t.{SHOWN}"));
    assert_eq!(
        serde_json::to_string(&column).unwrap(),
        serde_json::to_string(&raw).unwrap()
    );
    assert_eq!(raw.parse::<ColumnRef>().unwrap(), column);
    assert_ne!(table.column(SHOWN), column);
}

#[test]
fn temporal_period_membership_and_row_projection_use_raw_column_names() {
    let end = ColumnName::from(format!("{RAW}end"));
    let period = SystemTime {
        start: RAW.into(),
        end: end.clone(),
        hidden: false,
        versioning: None,
    };
    assert!(period.is_period_column(RAW));
    assert!(!period.is_period_column(SHOWN));
    let table = Table {
        columns: [
            ("id".into(), Column::new("int".parse().unwrap())),
            (
                RAW.into(),
                Column::new("datetime2(7)".parse().unwrap()).not_null(),
            ),
            (
                end.clone(),
                Column::new("datetime2(7)".parse().unwrap()).not_null(),
            ),
            (SHOWN.into(), Column::new("int".parse().unwrap())),
        ]
        .into(),
        system_time: Some(period),
        ..Default::default()
    };
    let writable: Vec<_> = table
        .row_columns("id")
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(writable, [ColumnName::from(SHOWN)]);
    assert!(!writable.contains(&ColumnName::from(RAW)));
    assert!(!writable.contains(&end));
}

#[test]
fn renamed_column_lists_and_partition_keys_use_the_raw_address() {
    let table_name = TableName::new("app", "t");
    let before = ColumnName::from(RAW);
    let after = ColumnName::from(format!("{RAW}new"));
    let mut renames = Renames::default();
    renames.rename_column(table_name.column(&before), &after);
    let table = Table {
        primary_key: Some(PrimaryKey {
            name: None,
            columns: vec![before.clone()],
            storage_parameters: Default::default(),
        }),
        unique: [(
            "uq".into(),
            UniqueConstraint {
                columns: vec![before.clone()],
                storage_parameters: Default::default(),
            },
        )]
        .into(),
        indexes: [(
            "ix".into(),
            Index {
                columns: vec![IndexColumn::column(&before)],
                include: vec![before.clone()],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        )]
        .into(),
        foreign_keys: [(
            "fk".into(),
            ForeignKey {
                columns: vec![before.clone()],
                references_table: table_name.clone(),
                references_columns: vec![before.clone()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        )]
        .into(),
        partition_by: Some(PartitionBy {
            columns: vec![before.clone()],
            strategy: Default::default(),
            external: false,
        }),
        ..Default::default()
    };
    let moved = renames.apply(&table, &table_name);
    assert_eq!(
        moved.primary_key.as_ref().unwrap().columns,
        std::slice::from_ref(&after)
    );
    assert_eq!(moved.unique["uq"].columns, std::slice::from_ref(&after));
    assert_eq!(moved.indexes["ix"].columns[0].key.column(), Some(&after));
    assert_eq!(moved.indexes["ix"].include, std::slice::from_ref(&after));
    assert_eq!(
        moved.foreign_keys["fk"].columns,
        std::slice::from_ref(&after)
    );
    assert_eq!(
        moved.foreign_keys["fk"].references_columns,
        std::slice::from_ref(&after)
    );
    assert_eq!(
        moved.partition_by.as_ref().unwrap().columns,
        std::slice::from_ref(&after)
    );
    let partition = Table {
        partition_of: Some(PartitionOf {
            parent: table_name.clone(),
            bound: PartitionBound::Default,
            columns: [(before.clone(), PartitionColumn::default())].into(),
        }),
        ..Default::default()
    };
    let moved = renames.apply(&partition, &TableName::new("app", "part"));
    assert!(
        moved
            .partition_of
            .as_ref()
            .unwrap()
            .columns
            .contains_key(after.raw())
    );
    assert!(
        !moved
            .partition_of
            .as_ref()
            .unwrap()
            .columns
            .contains_key(after.shown().to_string().as_str())
    );
    // A different table and the literal display spelling are distinct identities.
    assert_eq!(
        renames.column(&TableName::new("app", "other"), &before),
        before
    );
    assert_eq!(
        renames.column(&table_name, &ColumnName::from(SHOWN)).raw(),
        SHOWN
    );
}

#[test]
fn declared_expression_and_binding_keys_follow_raw_column_renames() {
    let name = TableName::new("app", "t");
    let old = ColumnName::from(RAW);
    let new = ColumnName::from(format!("{RAW}new"));
    let mut column = Column::new("int".parse().unwrap());
    column.value_source = ValueSource::Default("1".into());
    let schema = Schema {
        tables: [(
            name.clone(),
            Table {
                columns: [(old.clone(), column)].into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let mut declared = Declared::from_schema(&schema);
    declared
        .bindings
        .defaults
        .insert(name.clone(), [(old.clone(), Binding::default())].into());
    declared.advance(&ChangeSet {
        changes: vec![PlannedChange::new(Change::RenameColumn {
            uid: "c_aaaaaa".parse().unwrap(),
            table: name.clone(),
            from: old.clone(),
            to: new.clone(),
            table_was: None,
        })],
    });
    assert_eq!(
        declared.expressions.defaults[&name]
            .get(new.raw())
            .map(String::as_str),
        Some("1")
    );
    assert!(declared.bindings.defaults[&name].contains_key(new.raw()));
    assert!(!declared.expressions.defaults[&name].contains_key(new.shown().to_string().as_str()));
    assert!(!declared.bindings.defaults[&name].contains_key(old.raw()));
}

#[test]
fn column_history_physical_sources_and_name_slots_keep_raw_addresses() {
    use pbps_model::operation::{
        ModuleEdits, Operations, PlanOwners, Snapshot, names as operation_names,
    };
    use pbps_model::physical_sources::{Located, PhysicalSources, Source};
    use pbps_model::plan_names::{At, Holder, PlanNames};

    let table = TableName::new("app", "t");
    let owner: Uid = "t_aaaaaa".parse().unwrap();
    let uid: Uid = "c_aaaaaa".parse().unwrap();
    let next = ColumnName::from(format!("{RAW}next"));
    let computed = ColumnName::from(format!("{RAW}computed"));
    let before = Schema {
        tables: [(
            table.clone(),
            Table {
                columns: [(RAW.into(), Column::new("int".parse().unwrap()))].into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let base = IdsFile {
        tables: [(owner.clone(), table.clone())].into(),
        columns: [(uid.clone(), table.column(RAW))].into(),
        ..Default::default()
    };
    let mut final_ids = base.clone();
    final_ids.columns.insert(uid.clone(), table.column(&next));
    let changes = vec![
        PlannedChange::new(Change::RenameColumn {
            uid: uid.clone(),
            table: table.clone(),
            from: RAW.into(),
            to: next.clone(),
            table_was: None,
        }),
        PlannedChange::new(Change::AddComputedColumn {
            table: table.clone(),
            name: computed.clone(),
            computed: ComputedColumn {
                expression: "1".into(),
                persisted: false,
                not_null: false,
            },
        }),
    ];
    let names = PlanNames::of(&changes, &final_ids).unwrap();
    assert_eq!(
        names.column_at(&uid, At::Start),
        Some((&owner, &ColumnName::from(RAW)))
    );
    assert_eq!(names.column_at(&uid, At::End), Some((&owner, &next)));
    assert!(matches!(
        names.column_holder(&table.column(RAW), At::Start),
        Holder::Tracked(_)
    ));
    assert!(matches!(
        names.column_holder(&table.column(SHOWN), At::Start),
        Holder::Unmanaged
    ));
    let physical = PhysicalSources::of(names, &before, &base).unwrap();
    assert_eq!(
        physical.source(&table.column(&next), At::End),
        Source::Stored(table.column(RAW))
    );
    assert!(
        matches!(physical.locate(&table.column(RAW), At::End), Located::Present(at) if at.address == table.column(&next))
    );
    assert!(matches!(
        physical.source(&table.column(next.shown().to_string()), At::End),
        Source::Unavailable
    ));

    let owners = PlanOwners::new(
        PlanNames::of(&changes, &final_ids).unwrap(),
        &base,
        &final_ids,
    );
    let operations = Operations::of_plan(&changes, ModuleEdits::InPlace);
    let snapshot = Snapshot {
        schema: &before,
        ids: &base,
    };
    let renamed = operation_names(&operations.as_slice()[0], &owners, snapshot).unwrap();
    assert_eq!(renamed.releases[0].address.name, RAW);
    assert_eq!(renamed.claims[0].address.name, next.raw());
    let added = operation_names(&operations.as_slice()[1], &owners, snapshot).unwrap();
    assert_eq!(added.claims[0].address.name, computed.raw());
    assert_ne!(added.claims[0].address.name, computed.shown().to_string());
}
