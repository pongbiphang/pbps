use super::*;
use crate::{Column, PartitionBound, PartitionBy, PartitionOf, PlannedChange, Table};

fn table(name: &str) -> TableName {
    TableName::new("app", name)
}
fn uid(text: &str) -> Uid {
    text.parse().unwrap()
}
fn plain() -> Table {
    let mut shape = Table::default();
    shape
        .columns
        .insert("k".into(), Column::new("integer".parse().unwrap()));
    shape
        .columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    shape
}
fn baseline(attached: bool) -> (Schema, IdsFile) {
    let mut parent = plain();
    parent.partition_by = Some(PartitionBy {
        external: false,
        columns: vec!["k".into()],
        strategy: Default::default(),
    });
    let child = if attached {
        Table {
            partition_of: Some(PartitionOf {
                parent: table("parent"),
                bound: PartitionBound::Default,
                columns: BTreeMap::new(),
            }),
            ..Default::default()
        }
    } else {
        plain()
    };
    let schema = Schema {
        tables: [(table("parent"), parent), (table("child"), child)].into(),
        ..Default::default()
    };
    let mut ids = IdsFile {
        tables: [
            (uid("t_aaaaaa"), table("parent")),
            (uid("t_bbbbbb"), table("child")),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), table("parent").column("k")),
            (uid("c_bbbbbb"), table("parent").column("n")),
        ]
        .into(),
        ..Default::default()
    };
    if !attached {
        ids.columns
            .insert(uid("c_cccccc"), table("child").column("k"));
        ids.columns
            .insert(uid("c_dddddd"), table("child").column("n"));
    }
    (schema, ids)
}
fn detach() -> Change {
    Change::DetachPartition {
        uid: uid("t_bbbbbb"),
        table: table("child"),
        parent: table("parent"),
        names: vec![],
        shape: Box::new(plain()),
    }
}
fn attach() -> Change {
    Change::AttachPartition {
        uid: uid("t_bbbbbb"),
        table: table("child"),
        parent: table("parent"),
        bound: PartitionBound::Default,
        key: Vec::new(),
        shape: Box::new(plain()),
    }
}
fn steps(changes: Vec<Change>) -> Vec<PlannedChange> {
    changes.into_iter().map(PlannedChange::new).collect()
}

#[test]
fn detach_preserves_the_catalog_column_through_rename_and_ends_it_only_on_drop() {
    let (before, old_ids) = baseline(true);
    let (_, mut after) = baseline(false);
    after.columns.remove(&uid("c_dddddd"));
    let changes = steps(vec![
        detach(),
        Change::RenameColumn {
            uid: uid("c_dddddd"),
            table: table("child"),
            from: "n".into(),
            to: "amount".into(),
            table_was: None,
        },
        Change::DropColumn {
            uid: uid("c_dddddd"),
            column: table("child").column("amount"),
        },
    ]);
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
    let stored = table("child").column("n");
    assert_eq!(
        sources
            .names()
            .stored_column(&table("child").column("amount"), At::Before(2)),
        None
    );
    assert_eq!(
        sources.source(&table("child").column("amount"), At::Before(2)),
        Source::Stored(stored.clone())
    );
    assert_eq!(
        sources.locate(&stored, At::Start),
        Located::Present(ColumnAt {
            table_uid: uid("t_bbbbbb"),
            address: stored.clone(),
            owner: Owner::Partition {
                parent: table("parent")
            },
        })
    );
    assert_eq!(
        sources.locate(&stored, At::Before(2)),
        Located::Present(ColumnAt {
            table_uid: uid("t_bbbbbb"),
            address: table("child").column("amount"),
            owner: Owner::Column(uid("c_dddddd")),
        })
    );
    assert_eq!(sources.locate(&stored, At::End), Located::Retired);
    assert_eq!(
        sources.source(&table("child").column("amount"), At::End),
        Source::Retired
    );
    assert_eq!(
        sources.source(&table("other").column("n"), At::Start),
        Source::Unavailable
    );
    assert_eq!(
        sources.source(&table("parent").column("n"), At::End),
        Source::Stored(table("parent").column("n"))
    );
}

#[test]
fn attach_uses_the_recorded_ordinary_uid_before_its_scope_is_retired() {
    let (before, old_ids) = baseline(false);
    let (_, after) = baseline(true);
    let changes = steps(vec![attach()]);
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
    let stored = table("child").column("n");
    assert_eq!(
        sources.locate(&stored, At::Start),
        Located::Present(ColumnAt {
            table_uid: uid("t_bbbbbb"),
            address: stored.clone(),
            owner: Owner::Column(uid("c_dddddd")),
        })
    );
    assert_eq!(
        sources.locate(&stored, At::End),
        Located::Present(ColumnAt {
            table_uid: uid("t_bbbbbb"),
            address: stored.clone(),
            owner: Owner::Partition {
                parent: table("parent")
            },
        })
    );
    assert_eq!(
        sources.source(&stored, At::End),
        Source::Stored(stored.clone())
    );
    let missing = PhysicalSources::of(
        PlanNames::of(&changes, &after).unwrap(),
        &before,
        &IdsFile::default(),
    )
    .unwrap();
    assert_eq!(
        missing.locate(&stored, At::Start),
        Located::Present(ColumnAt {
            table_uid: uid("t_bbbbbb"),
            address: stored,
            owner: Owner::Unavailable
        })
    );
}

#[test]
fn attach_then_detach_keeps_the_source_but_does_not_resurrect_the_old_uid() {
    let (before, old_ids) = baseline(false);
    let (_, mut after) = baseline(false);
    after.columns.remove(&uid("c_cccccc"));
    after.columns.remove(&uid("c_dddddd"));
    after
        .columns
        .insert(uid("c_eeeeee"), table("child").column("k"));
    after
        .columns
        .insert(uid("c_ffffff"), table("child").column("n"));
    let changes = steps(vec![attach(), detach()]);
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
    let stored = table("child").column("n");
    assert_eq!(
        sources.source(&stored, At::End),
        Source::Stored(stored.clone())
    );
    assert_eq!(
        sources.locate(&stored, At::End),
        Located::Present(ColumnAt {
            table_uid: uid("t_bbbbbb"),
            address: stored,
            owner: Owner::Column(uid("c_ffffff"))
        })
    );
}

#[test]
fn a_reused_column_name_has_a_new_source_and_cannot_receive_the_old_dependent() {
    let (before, old_ids) = baseline(false);
    let mut after = old_ids.clone();
    after.columns.remove(&uid("c_dddddd"));
    after
        .columns
        .insert(uid("c_eeeeee"), table("child").column("n"));
    let changes = steps(vec![
        Change::DropColumn {
            uid: uid("c_dddddd"),
            column: table("child").column("n"),
        },
        Change::AddColumn {
            uid: uid("c_eeeeee"),
            table: table("child"),
            name: "n".into(),
            column: Box::new(Column::new("integer".parse().unwrap())),
        },
    ]);
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
    let stored = table("child").column("n");
    assert_eq!(
        sources.source(&stored, At::Start),
        Source::Stored(stored.clone())
    );
    assert_eq!(sources.source(&stored, At::After(0)), Source::Retired);
    assert_eq!(
        sources.source(&stored, At::End),
        Source::Created { step: 1 }
    );
    assert_eq!(sources.locate(&stored, At::End), Located::Retired);
    assert_eq!(
        sources.source(&table("parent").column("n"), At::End),
        Source::Stored(table("parent").column("n"))
    );
}

#[test]
fn a_parent_column_rename_reaches_only_the_children_attached_at_that_boundary() {
    let (before, old_ids) = baseline(true);
    let (_, mut after) = baseline(false);
    after
        .columns
        .insert(uid("c_bbbbbb"), table("parent").column("amount"));
    let changes = steps(vec![
        detach(),
        Change::RenameColumn {
            uid: uid("c_bbbbbb"),
            table: table("parent"),
            from: "n".into(),
            to: "amount".into(),
            table_was: None,
        },
    ]);
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
    assert_eq!(
        sources.source(&table("child").column("n"), At::End),
        Source::Stored(table("child").column("n"))
    );
    assert_eq!(
        sources.source(&table("child").column("amount"), At::End),
        Source::Unavailable
    );
    // Without the transition, the inherited physical column follows its
    // parent's rename, while its managed owner remains the partition.
    let (_, mut after) = baseline(true);
    after
        .columns
        .insert(uid("c_bbbbbb"), table("parent").column("amount"));
    let changes = vec![changes[1].clone()];
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
    assert_eq!(
        sources.source(&table("child").column("amount"), At::End),
        Source::Stored(table("child").column("n"))
    );
    assert_eq!(
        sources.source(&table("child").column("n"), At::End),
        Source::Retired
    );
}

#[test]
fn a_missing_partition_baseline_is_unavailable_not_a_new_column() {
    let (_, old_ids) = baseline(true);
    let (_, after) = baseline(false);
    let changes = steps(vec![detach()]);
    let names = PlanNames::of(&changes, &after).unwrap();
    let sources = PhysicalSources::of(names, &Schema::default(), &old_ids).unwrap();
    assert_eq!(
        sources.source(&table("child").column("n"), At::End),
        Source::Unavailable
    );
    assert_eq!(
        sources.locate(&table("child").column("n"), At::End),
        Located::Unavailable
    );
}

#[test]
fn executable_table_handoffs_keep_sources_with_their_original_owners() {
    let (before, old_ids) = baseline(false);
    let rename = |id: &str, from: &str, to: &str| Change::RenameTable {
        uid: uid(id),
        from: table(from),
        to: table(to),
        defaults: vec![],
    };
    for swapped in [false, true] {
        let (changes, parent_end, child_end) = if swapped {
            (
                steps(vec![
                    rename("t_aaaaaa", "parent", "temporary"),
                    rename("t_bbbbbb", "child", "parent"),
                    rename("t_aaaaaa", "temporary", "child"),
                ]),
                "child",
                "parent",
            )
        } else {
            (
                steps(vec![
                    rename("t_bbbbbb", "child", "next"),
                    rename("t_aaaaaa", "parent", "child"),
                ]),
                "child",
                "next",
            )
        };
        let mut after = old_ids.clone();
        after.tables.insert(uid("t_aaaaaa"), table(parent_end));
        after.tables.insert(uid("t_bbbbbb"), table(child_end));
        for column in after.columns.values_mut() {
            column.table = if column.table == table("parent") {
                table(parent_end)
            } else {
                table(child_end)
            };
        }
        let names = PlanNames::of(&changes, &after).unwrap();
        let sources = PhysicalSources::of(names, &before, &old_ids).unwrap();
        for (original, current) in [("parent", parent_end), ("child", child_end)] {
            assert_eq!(
                sources.table_source(&table(current), At::End),
                Source::Stored(table(original))
            );
            assert_eq!(
                sources.locate_table(&table(original), At::End),
                Located::Present(table(current))
            );
            assert_eq!(
                sources.source(&table(current).column("n"), At::End),
                Source::Stored(table(original).column("n"))
            );
            let Located::Present(column) = sources.locate(&table(original).column("n"), At::End)
            else {
                panic!("original physical column")
            };
            assert_eq!(column.address, table(current).column("n"));
        }
        assert_eq!(
            sources.locate_table(&table("absent"), At::End),
            Located::Unavailable
        );
    }
}

#[test]
fn a_dropped_tables_dependents_never_follow_a_new_holder_of_its_name() {
    let (before, old_ids) = baseline(false);
    let mut after = old_ids.clone();
    after.tables.remove(&uid("t_aaaaaa"));
    after.tables.insert(uid("t_bbbbbb"), table("parent"));
    after
        .columns
        .retain(|_, column| column.table == table("child"));
    for column in after.columns.values_mut() {
        column.table = table("parent");
    }
    let changes = steps(vec![
        Change::DropTable {
            uid: uid("t_aaaaaa"),
            name: table("parent"),
            detach_from: None,
        },
        Change::RenameTable {
            uid: uid("t_bbbbbb"),
            from: table("child"),
            to: table("parent"),
            defaults: vec![],
        },
    ]);
    let sources =
        PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &old_ids).unwrap();
    assert_eq!(
        sources.locate_table(&table("parent"), At::End),
        Located::Retired
    );
    assert_eq!(
        sources.locate(&table("parent").column("n"), At::End),
        Located::Retired
    );
    assert_eq!(
        sources.source(&table("parent").column("n"), At::End),
        Source::Stored(table("child").column("n"))
    );
}

#[test]
fn creating_a_partition_does_not_give_it_its_parents_stored_source() {
    let (mut before, mut old_ids) = baseline(true);
    before.tables.remove(&table("child"));
    old_ids.tables.remove(&uid("t_bbbbbb"));
    let (after_schema, after) = baseline(true);
    let changes = steps(vec![Change::CreateTable {
        uid: uid("t_bbbbbb"),
        name: table("child"),
        table: Box::new(after_schema.tables[&table("child")].clone()),
        beside_default: None,
        moves_at_most: None,
    }]);
    let sources =
        PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &old_ids).unwrap();
    assert_eq!(
        sources.table_source(&table("child"), At::End),
        Source::Created { step: 0 }
    );
    assert_eq!(
        sources.source(&table("child").column("n"), At::End),
        Source::Created { step: 0 }
    );
    assert_eq!(
        sources.locate(&table("child").column("n"), At::End),
        Located::Unavailable
    );
    assert_eq!(
        sources.source(&table("parent").column("n"), At::End),
        Source::Stored(table("parent").column("n"))
    );
}

#[test]
fn conflicting_recorded_identity_is_refused_instead_of_rebinding_a_source() {
    let (before, mut old_ids) = baseline(false);
    let (_, after) = baseline(false);
    old_ids.columns.remove(&uid("c_dddddd"));
    old_ids
        .columns
        .insert(uid("c_eeeeee"), table("child").column("n"));
    let changes = steps(vec![]);
    let error = PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &old_ids)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("baseline identity for app.child.n disagrees")
    );
}

#[test]
fn physical_order_survives_rename_and_appends_new_member_columns() {
    let (before, mut old_ids) = baseline(true);
    old_ids
        .columns
        .insert(uid("c_aaaaaa"), table("parent").column("n"));
    old_ids
        .columns
        .insert(uid("c_bbbbbb"), table("parent").column("k"));
    let mut after = old_ids.clone();
    after
        .columns
        .insert(uid("c_aaaaaa"), table("parent").column("amount"));
    after
        .columns
        .insert(uid("c_eeeeee"), table("parent").column("z"));
    let changes = steps(vec![
        Change::RenameColumn {
            uid: uid("c_aaaaaa"),
            table: table("parent"),
            from: "n".into(),
            to: "amount".into(),
            table_was: None,
        },
        Change::AddColumn {
            uid: uid("c_eeeeee"),
            table: table("parent"),
            name: "z".into(),
            column: Box::new(Column::new("integer".parse().unwrap())),
        },
    ]);
    let view =
        PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &old_ids).unwrap();
    for target in ["parent", "child"] {
        assert_eq!(
            view.columns_at(&table(target), At::Start),
            Located::Present(vec!["k".into(), "n".into()])
        );
        assert_eq!(
            view.columns_at(&table(target), At::After(0)),
            Located::Present(vec!["k".into(), "amount".into()])
        );
        assert_eq!(
            view.columns_at(&table(target), At::End),
            Located::Present(vec!["k".into(), "amount".into(), "z".into()])
        );
        assert_eq!(
            view.columns_at(&table(target), At::Before(changes.len())),
            Located::Unavailable
        );
    }
    assert_eq!(
        view.columns_at(&table("unmanaged"), At::Start),
        Located::Unavailable
    );
}
#[test]
fn reusing_a_dropped_column_name_does_not_reuse_its_old_position() {
    let (mut before, mut old_ids) = baseline(false);
    before
        .tables
        .get_mut(&table("parent"))
        .unwrap()
        .columns
        .insert("z".into(), Column::new("integer".parse().unwrap()));
    old_ids
        .columns
        .insert(uid("c_eeeeee"), table("parent").column("z"));
    let mut after = old_ids.clone();
    after.columns.remove(&uid("c_bbbbbb"));
    after
        .columns
        .insert(uid("c_ffffff"), table("parent").column("n"));
    let changes = steps(vec![
        Change::DropColumn {
            uid: uid("c_bbbbbb"),
            column: table("parent").column("n"),
        },
        Change::AddColumn {
            uid: uid("c_ffffff"),
            table: table("parent"),
            name: "n".into(),
            column: Box::new(Column::new("integer".parse().unwrap())),
        },
    ]);
    let view =
        PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &old_ids).unwrap();
    assert_eq!(
        view.columns_at(&table("parent"), At::Start),
        Located::Present(vec!["k".into(), "n".into(), "z".into()])
    );
    assert_eq!(
        view.columns_at(&table("parent"), At::End),
        Located::Present(vec!["k".into(), "z".into(), "n".into()])
    );
}

#[test]
fn excluded_families_are_unavailable_instead_of_an_empty_physical_shape() {
    let (mut before, mut ids) = baseline(false);
    let child = before.tables.get_mut(&table("child")).unwrap();
    let column = child.columns.shift_remove("n").unwrap();
    child.columns.insert("amount".into(), column);
    ids.columns
        .insert(uid("c_dddddd"), table("child").column("amount"));
    let changes = steps(vec![
        attach(),
        Change::RenameColumn {
            uid: uid("c_bbbbbb"),
            table: table("parent"),
            from: "n".into(),
            to: "amount".into(),
            table_was: None,
        },
    ]);
    let mut after = ids.clone();
    after
        .columns
        .insert(uid("c_bbbbbb"), table("parent").column("amount"));
    after.columns.remove(&uid("c_cccccc"));
    after.columns.remove(&uid("c_dddddd"));
    assert!(PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &ids).is_err());
    let partial = PhysicalSources::of_excluding(
        PlanNames::of(&changes, &after).unwrap(),
        &before,
        &ids,
        [uid("t_aaaaaa"), uid("t_bbbbbb")].into(),
    )
    .unwrap();
    assert_eq!(
        partial.columns_at(&table("child"), At::End),
        Located::Unavailable
    );
    assert_eq!(
        partial.locate_table(&table("child"), At::End),
        Located::Unavailable
    );
    assert_eq!(
        partial.source(&table("child").column("n"), At::Start),
        Source::Unavailable
    );
    assert_eq!(
        partial.locate(&table("child").column("n"), At::Start),
        Located::Unavailable
    );
}

#[test]
fn membership_uses_the_requested_boundary_and_never_an_unknown_subset() {
    for attached in [false, true] {
        let (before, ids) = baseline(attached);
        let (_, after) = baseline(!attached);
        let changes = steps(vec![if attached { detach() } else { attach() }]);
        let sources =
            PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &ids).unwrap();
        let before_members = if attached {
            vec![table("child")]
        } else {
            Vec::new()
        };
        let after_members = if attached {
            Vec::new()
        } else {
            vec![table("child")]
        };
        assert_eq!(
            sources.members_at(&table("parent"), At::Before(0)),
            Located::Present(before_members)
        );
        assert_eq!(
            sources.members_at(&table("parent"), At::After(0)),
            Located::Present(after_members.clone())
        );
        assert_eq!(
            sources.members_at(&table("parent"), At::End),
            Located::Present(after_members)
        );
        assert_eq!(
            sources.members_at(&table("parent"), At::Before(1)),
            Located::Unavailable
        );
    }
    let (before, mut ids) = baseline(true);
    let after = ids.clone();
    ids.tables.remove(&uid("t_bbbbbb"));
    let changes = Vec::new();
    let sources =
        PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &ids).unwrap();
    assert_eq!(
        sources.members_at(&table("parent"), At::End),
        Located::Unavailable
    );
}

#[test]
fn an_unread_shape_is_not_a_known_empty_column_list() {
    let (mut before, mut ids) = baseline(false);
    before
        .tables
        .get_mut(&table("parent"))
        .unwrap()
        .columns
        .clear();
    ids.columns.retain(|_, at| at.table != table("parent"));
    let steps = Vec::new();
    let known = PhysicalSources::of(PlanNames::of(&steps, &ids).unwrap(), &before, &ids).unwrap();
    assert_eq!(
        known.columns_at(&table("parent"), At::Start),
        Located::Present(Vec::new())
    );
    assert_eq!(
        known.members_at(&table("parent"), At::Start),
        Located::Present(Vec::new())
    );
    let unread = PhysicalSources::of(
        PlanNames::of(&steps, &ids).unwrap(),
        &Schema::default(),
        &ids,
    )
    .unwrap();
    assert_eq!(
        unread.columns_at(&table("parent"), At::Start),
        Located::Unavailable
    );
    assert_eq!(
        unread.members_at(&table("parent"), At::Start),
        Located::Unavailable
    );
    let mut after = ids.clone();
    after.tables.insert(uid("t_cccccc"), table("empty"));
    let changes = self::steps(vec![Change::CreateTable {
        uid: uid("t_cccccc"),
        name: table("empty"),
        table: Box::default(),
        beside_default: None,
        moves_at_most: None,
    }]);
    let created =
        PhysicalSources::of(PlanNames::of(&changes, &after).unwrap(), &before, &ids).unwrap();
    assert_eq!(
        created.columns_at(&table("empty"), At::End),
        Located::Present(Vec::new())
    );
}
