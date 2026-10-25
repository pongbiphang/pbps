use super::*;
use pbps_model::{IdsFile, PlannedChange, UidKind};

#[test]
fn row_write_and_null_evidence_keys_keep_raw_column_names() {
    let column = pbps_model::ColumnName::from("cell\n\r\u{1b}[31m\u{9b}\\\u{200b}名前");
    let shown = pbps_model::ColumnName::from(column.shown().to_string());
    let row = RowKey::from("1");
    let mut writes = RowWrites::<&str>::default();
    writes.insert(&row, &column, Some("NULL"));
    assert!(writes.inserted[&row].contains_key(column.raw()));
    assert!(!writes.inserted[&row].contains_key(shown.raw()));
    assert_eq!(
        writes.nulls_of(&row, &|c| Some(c.clone()), |v| *v == "NULL"),
        [column.clone()].into()
    );
    writes.update(&row, &shown, None);
    assert!(writes.unprobeable.contains_key(shown.raw()));
    assert!(!writes.unprobeable.contains_key(column.raw()));
}

fn table(name: &str) -> TableName {
    TableName::new("app", name)
}

fn uid(kind: UidKind, name: &str) -> Uid {
    Uid::derived(kind, name, 0)
}

fn rename(id: &Uid, from: &str, to: &str) -> PlannedChange {
    PlannedChange::new(Change::RenameTable {
        uid: id.clone(),
        from: table(from),
        to: table(to),
        defaults: vec![],
    })
}

#[test]
fn an_empty_plan_has_one_valid_boundary_and_no_removals() {
    let ids = IdsFile::default();
    let names = PlanNames::of(&[], &ids).unwrap();
    let facts = PlanFacts::of(&names);
    assert_eq!(facts.before(0).events().count(), 0);
    assert_eq!(facts.end().table(&table("outside")), Some(table("outside")));
    assert!(!facts.end().created(&table("outside")));
    assert_eq!(
        facts.end().removed_from(&table("outside")),
        Removal::Keys(vec![])
    );
}

#[test]
#[should_panic(expected = "preflight boundary outside plan")]
fn a_boundary_past_the_plan_is_not_an_empty_view() {
    let ids = IdsFile::default();
    let names = PlanNames::of(&[], &ids).unwrap();
    PlanFacts::of(&names).before(1);
}

#[test]
fn a_removal_keeps_its_original_holder_when_another_table_takes_the_name() {
    let a = uid(UidKind::Table, "a");
    let b = uid(UidKind::Table, "b");
    let changes = [
        PlannedChange::new(Change::DropForeignKey {
            table: table("a"),
            name: "fk".into(),
        }),
        PlannedChange::new(Change::DropTable {
            uid: a,
            name: table("a"),
            detach_from: None,
        }),
        rename(&b, "b", "a"),
    ];
    let ids = IdsFile {
        tables: [(b, table("a"))].into(),
        ..IdsFile::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let facts = PlanFacts::of(&names);
    assert_eq!(
        facts.before(0).removed_from(&table("a")),
        Removal::Keys(vec![])
    );
    assert_eq!(
        facts.before(1).removed_from(&table("a")),
        Removal::Keys(vec!["fk".into()])
    );
    assert_eq!(facts.before(1).table(&table("a")), Some(table("a")));
    assert_eq!(facts.before(2).table(&table("a")), None);
    assert!(!facts.before(2).created(&table("a")));
    assert_eq!(facts.end().table(&table("a")), Some(table("b")));
    assert_eq!(facts.end().removed_from(&table("a")), Removal::Table);
    assert_eq!(facts.end().removed_from(&table("b")), Removal::Keys(vec![]));
    assert_eq!(facts.before(1).events().count(), 1);
    assert_eq!(facts.before(2).events().count(), 2);
}

#[test]
fn a_temporary_name_and_column_chain_are_read_at_the_requested_boundary() {
    let a = uid(UidKind::Table, "a");
    let b = uid(UidKind::Table, "b");
    let c = uid(UidKind::Column, "c");
    let changes = [
        rename(&a, "a", "tmp"),
        rename(&b, "b", "a"),
        rename(&a, "tmp", "b"),
        PlannedChange::new(Change::RenameColumn {
            uid: c.clone(),
            table: table("b"),
            from: "c".into(),
            to: "d".into(),
            table_was: Some(table("a")),
        }),
        PlannedChange::new(Change::RenameColumn {
            uid: c.clone(),
            table: table("b"),
            from: "d".into(),
            to: "e".into(),
            table_was: Some(table("a")),
        }),
    ];
    let ids = IdsFile {
        tables: [(a, table("b")), (b, table("a"))].into(),
        columns: [(c, table("b").column("e"))].into(),
        ..IdsFile::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let facts = PlanFacts::of(&names);
    assert_eq!(facts.before(1).table(&table("tmp")), Some(table("a")));
    assert_eq!(facts.before(2).table(&table("a")), Some(table("b")));
    assert_eq!(
        facts.before(3).column(&table("b").column("c")),
        Some(table("a").column("c"))
    );
    assert_eq!(facts.before(3).renamed_columns(&table("b")).count(), 0);
    assert_eq!(
        facts.before(4).column(&table("b").column("d")),
        Some(table("a").column("c"))
    );
    assert_eq!(
        facts.end().column(&table("b").column("e")),
        Some(table("a").column("c"))
    );
    assert_eq!(facts.end().table(&table("tmp")), None);
}

#[test]
fn a_created_table_has_no_stored_relation_even_after_its_create() {
    let id = uid(UidKind::Table, "new");
    let changes = [PlannedChange::new(Change::CreateTable {
        uid: id.clone(),
        name: table("new"),
        table: Box::default(),
        beside_default: None,
        moves_at_most: None,
    })];
    let ids = IdsFile {
        tables: [(id, table("new"))].into(),
        ..IdsFile::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let facts = PlanFacts::of(&names);
    assert_eq!(facts.before(0).table(&table("new")), None);
    assert!(!facts.before(0).created(&table("new")));
    assert_eq!(facts.before(1).table(&table("new")), None);
    assert!(facts.before(1).created(&table("new")));
    assert_eq!(facts.before(0).events().count(), 0);
    assert!(matches!(
        facts.end().events().next(),
        Some((0, Event::CreateTable { .. }))
    ));
}

#[test]
fn omitted_nulls_and_engine_assignments_are_different_from_defaults() {
    let row = Row([("explicit".into(), Value::Null)].into());
    let defaults = [("default".into(), "NULL".into())].into();
    let types = [("omitted".into(), "integer".parse().unwrap())].into();
    assert_eq!(
        WrittenCell::inserted(
            &row,
            &defaults,
            &types,
            &pbps_model::ColumnName::from("explicit")
        ),
        WrittenCell::Value(&Value::Null)
    );
    // The engine's literal reader, not this walk, decides whether NULL is
    // evaluable. In particular, an arbitrary default is not an omitted cell.
    assert_eq!(
        WrittenCell::inserted(
            &row,
            &defaults,
            &types,
            &pbps_model::ColumnName::from("default")
        ),
        WrittenCell::Default("NULL")
    );
    assert_eq!(
        WrittenCell::inserted(
            &row,
            &defaults,
            &types,
            &pbps_model::ColumnName::from("omitted")
        ),
        WrittenCell::OmittedNull
    );
    assert_eq!(
        WrittenCell::inserted(
            &row,
            &defaults,
            &types,
            &pbps_model::ColumnName::from("identity")
        ),
        WrittenCell::EngineAssigned
    );
}

#[test]
fn deleting_and_updating_one_key_excludes_it_once_and_keeps_unknowns_visible() {
    let mut rows = RowWrites::<&str>::default();
    let deleted = RowKey("deleted".into());
    let changed = RowKey("changed".into());
    let unrelated = RowKey("unrelated".into());
    rows.deleted.insert(deleted.clone());
    rows.update(&deleted, &pbps_model::ColumnName::from("fk"), Some("old"));
    rows.update(&changed, &pbps_model::ColumnName::from("fk"), None);
    rows.update(
        &unrelated,
        &pbps_model::ColumnName::from("description"),
        Some("text"),
    );
    assert_eq!(rows.gone_keys(&["fk".into()]), [&deleted, &changed].into());
    assert_eq!(rows.gone_keys(&["untouched".into()]), [&deleted].into());
    assert_eq!(rows.unprobeable["fk"], [changed.clone()].into());
    assert_eq!(rows.updated[&changed]["fk"], None);
    assert!(rows.has_writes());
    let mut only_deletes = RowWrites::<&str>::default();
    only_deletes.deleted.insert(deleted);
    assert!(!only_deletes.has_writes());
}

#[test]
fn a_deleted_row_cannot_arrive_again_from_its_earlier_writes() {
    let mut rows = RowWrites::<&str>::default();
    let gone = RowKey("gone".into());
    let kept = RowKey("kept".into());
    for row in [&gone, &kept] {
        rows.insert(row, &pbps_model::ColumnName::from("known"), Some("1"));
        rows.update(row, &pbps_model::ColumnName::from("unknown"), None);
        rows.typed.insert(row.clone(), ["known".into()].into());
        rows.held_null
            .insert(row.clone(), ["sibling".into()].into());
    }
    rows.insert(&gone, &pbps_model::ColumnName::from("gone_only"), None);
    rows.delete(&pbps_model::ColumnName::from("id"), &gone);
    assert_eq!(rows.key_column, "id".into());
    assert_eq!(rows.deleted, [gone.clone()].into());
    assert_eq!(rows.inserted.keys().collect::<Vec<_>>(), [&kept]);
    assert_eq!(rows.updated.keys().collect::<Vec<_>>(), [&kept]);
    assert_eq!(rows.typed.keys().collect::<Vec<_>>(), [&kept]);
    assert_eq!(rows.held_null.keys().collect::<Vec<_>>(), [&kept]);
    assert_eq!(
        rows.unprobeable,
        [("unknown".into(), [kept.clone()].into())].into()
    );
    assert!(rows.has_writes());
    rows.delete(&pbps_model::ColumnName::from("id"), &kept);
    assert!(!rows.has_writes());
    assert!(rows.unprobeable.is_empty());
    assert_eq!(rows.gone_keys(&[]), [&gone, &kept].into());
}

#[test]
fn null_evidence_stays_with_its_row_and_uses_the_callers_interpretation() {
    let mut rows = RowWrites::<&str>::default();
    let first = RowKey("1".into());
    let second = RowKey("2".into());
    rows.insert(&first, &pbps_model::ColumnName::from("a"), Some("NULL"));
    rows.update(
        &first,
        &pbps_model::ColumnName::from("b"),
        Some("engine-null"),
    );
    rows.held_null
        .insert(first.clone(), ["held".into(), "gone".into()].into());
    rows.insert(&second, &pbps_model::ColumnName::from("unknown"), None);
    let stored = |name: &pbps_model::ColumnName| {
        (name.raw() != "gone")
            .then(|| pbps_model::ColumnName::from(format!("stored_{}", name.shown())))
    };
    assert_eq!(
        rows.nulls_of(&first, &stored, |v| *v == "NULL" || *v == "engine-null"),
        ["stored_a".into(), "stored_b".into(), "stored_held".into()].into()
    );
    assert!(rows.nulls_of(&second, &stored, |_| true).is_empty());
    assert_eq!(rows.unprobeable["unknown"], [second.clone()].into());
    assert!(!rows.inserted.contains_key(&second));
}

#[test]
fn a_backfill_and_a_later_retype_remain_distinct_prefix_facts() {
    let column = uid(UidKind::Column, "added");
    let changes = [
        PlannedChange::new(Change::AddColumn {
            uid: column.clone(),
            table: table("a"),
            name: "added".into(),
            column: Box::new(Column::new("integer".parse().unwrap())),
        }),
        PlannedChange::new(Change::AlterColumnType {
            uid: column.clone(),
            column: table("a").column("added"),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        }),
    ];
    let ids = IdsFile {
        tables: [(uid(UidKind::Table, "a"), table("a"))].into(),
        columns: [(column, table("a").column("added"))].into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let facts = PlanFacts::of(&names);
    assert_eq!(facts.before(0).events().count(), 0);
    assert!(matches!(
        facts.before(1).events().collect::<Vec<_>>().as_slice(),
        [(0, Event::AddColumn { .. })]
    ));
    let end = facts.end().events().collect::<Vec<_>>();
    assert!(matches!(
        end.as_slice(),
        [
            (0, Event::AddColumn { .. }),
            (1, Event::AlterColumnType { .. })
        ]
    ));
    if let Event::AddColumn { column, .. } = end[0].1 {
        assert_eq!(column.ty, "integer".parse().unwrap());
    }
}

#[test]
fn an_enum_rebuilds_retype_step_is_an_event_of_its_column_and_no_other_step_is() {
    let step = |step| {
        PlannedChange::new(Change::EnumRebuild {
            uid: "y_000001".parse().unwrap(),
            name: "app.e".parse().unwrap(),
            tmp: "app.tmp".parse().unwrap(),
            step,
        })
    };
    let definition = Box::new(pbps_model::ManagedType::Enum { values: vec![] });
    let changes = [
        step(pbps_model::EnumRebuildStep::Create {
            definition: definition.clone(),
            removed: vec![],
            ordinal: 0,
        }),
        step(pbps_model::EnumRebuildStep::Retype {
            column_uid: uid(UidKind::Column, "m"),
            column: table("a").column("m"),
            to: ColumnType::reference("app.e".parse().unwrap()),
            array: false,
            removed: vec![],
        }),
        step(pbps_model::EnumRebuildStep::Swap {
            definition,
            comment: None,
        }),
    ];
    let ids = IdsFile {
        tables: [(uid(UidKind::Table, "a"), table("a"))].into(),
        columns: [(uid(UidKind::Column, "m"), table("a").column("m"))].into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let facts = PlanFacts::of(&names);
    let end = facts.end().events().collect::<Vec<_>>();
    let [(1, Event::RebuildEnumColumn { column })] = end.as_slice() else {
        panic!("{end:?}");
    };
    assert_eq!(**column, table("a").column("m"));
    assert_eq!(facts.before(1).events().count(), 0);
}

#[test]
fn row_projection_preserves_unknown_assignments_and_excludes_future_deletes() {
    let key = RowKey("1".into());
    let changes = [
        PlannedChange::new(Change::UpdateRow {
            table: table("a"),
            key_column: "id".into(),
            key: key.clone(),
            columns: [(
                "fk".into(),
                (
                    Cell::Value(Value::Int(1)),
                    Cell::Default("next_value()".into()),
                ),
            )]
            .into(),
            unchanged: [("sibling".into(), Cell::Value(Value::Null))].into(),
            types: [("fk".into(), "integer".parse().unwrap())].into(),
            after_types: [("fk".into(), "bigint".parse().unwrap())].into(),
            key_type: None,
        }),
        PlannedChange::new(Change::DeleteRow {
            table: table("a"),
            key_column: "id".into(),
            key: key.clone(),
            cause: pbps_model::change::DeleteCause::Undeclared,
            row: Default::default(),
            dropped: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        }),
    ];
    let ids = IdsFile {
        tables: [(uid(UidKind::Table, "a"), table("a"))].into(),
        ..Default::default()
    };
    let names = PlanNames::of(&changes, &ids).unwrap();
    let facts = PlanFacts::of(&names);
    let render = |assignment: Assignment<'_>, types: &mut Vec<ColumnType>| {
        types.push(assignment.ty.unwrap().clone());
        None::<String>
    };
    let null = |cell: &Cell| matches!(cell, Cell::Value(Value::Null));
    assert!(facts.before(0).writes(render, null).is_empty());
    let prefix = facts.before(1).writes(render, null);
    let rows = &prefix[&table("a")];
    assert!(rows.deleted.is_empty());
    assert_eq!(rows.updated[&key]["fk"], None);
    assert_eq!(rows.unprobeable["fk"], [key.clone()].into());
    assert_eq!(rows.held_null[&key], ["sibling".into()].into());
    assert_eq!(rows.metadata, vec!["bigint".parse().unwrap()]);
    let whole = facts.end().writes(render, null);
    assert_eq!(whole[&table("a")].deleted, [key.clone()].into());
    assert_eq!(whole[&table("a")].gone_keys(&["fk".into()]), [&key].into());
    assert!(!whole[&table("a")].has_writes());
    assert!(whole[&table("a")].unprobeable.is_empty());
    assert!(whole[&table("a")].held_null.is_empty());
}
