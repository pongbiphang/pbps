use super::*;

#[test]
fn real_collation_changes_are_unproven_for_readers_and_writers() {
    let mut before = schema();
    let column = before
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .get_mut("old")
        .unwrap();
    column.ty = ColumnType::simple("text");
    column.collation = Some(pbps_model::Collation::new("pg_catalog.\"C\""));
    let mut after = before.clone();
    let to = Some(pbps_model::Collation::new("pg_catalog.\"POSIX\""));
    after
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .get_mut("old")
        .unwrap()
        .collation = to.clone();
    let change = Change::AlterColumnType {
        uid: uid("c_aaaaaa"),
        column: name().column("old"),
        from: ColumnType::simple("text"),
        to: ColumnType::simple("text"),
        from_nullable: true,
        to_nullable: true,
        from_collation: before.tables[&name()].columns["old"].collation.clone(),
        to_collation: to,
        through_external_partitions: false,
        using: None,
    };
    for writes in [false, true] {
        let found = findings(
            &before,
            &after,
            &ids(),
            &ids(),
            &consumers("app.t.old", writes),
            vec![change.clone()],
        );
        assert_eq!(
            found.len(),
            1,
            "master collation result, writes={writes}: {found:?}"
        );
        assert_eq!(found[0].1.id, "consumer.unproven");
    }
}

fn column(s: &mut Schema) -> &mut Column {
    s.tables
        .get_mut(&name())
        .unwrap()
        .columns
        .get_mut("old")
        .unwrap()
}

#[test]
fn ordinary_edits_classify_the_effective_interface_with_literal_cells() {
    // (reader, writer): explicit changes can expose the same inherited value.
    for (label, nn, inherited, was_nullable, now_nullable, was, now, expected) in [
        ("tighten", false, None, true, false, None, None, [0, 1]),
        ("loosen", false, None, false, true, None, None, [1, 0]),
        (
            "redundant tighten",
            true,
            None,
            true,
            false,
            None,
            None,
            [0, 0],
        ),
        (
            "domain keeps NOT NULL",
            true,
            None,
            false,
            true,
            None,
            None,
            [0, 0],
        ),
        (
            "same DEFAULT exposed",
            false,
            Some("1"),
            true,
            true,
            Some("1"),
            None,
            [0, 0],
        ),
        (
            "different DEFAULT exposed",
            false,
            Some("1"),
            true,
            true,
            Some("2"),
            None,
            [0, 1],
        ),
        (
            "same DEFAULT restated",
            false,
            Some("1"),
            true,
            true,
            None,
            Some("1"),
            [0, 0],
        ),
        (
            "different DEFAULT restated",
            false,
            Some("1"),
            true,
            true,
            None,
            Some("2"),
            [0, 1],
        ),
    ] {
        let mut before = schema();
        before
            .types
            .insert("app.d".parse().unwrap(), domain(nn, inherited, None));
        column(&mut before).ty = "app.d".parse().unwrap();
        column(&mut before).nullable = was_nullable;
        column(&mut before).value_source =
            was.map_or(ValueSource::None, |v| ValueSource::Default(v.into()));
        let mut after = before.clone();
        column(&mut after).nullable = now_nullable;
        column(&mut after).value_source =
            now.map_or(ValueSource::None, |v| ValueSource::Default(v.into()));
        let mut changes = vec![Change::AlterColumnDefault {
            uid: uid("c_aaaaaa"),
            column: name().column("old"),
            from: was.map(str::to_owned),
            to: now.map(str::to_owned),
        }];
        if was_nullable != now_nullable {
            changes = vec![
                Change::AlterColumnNullability {
                    uid: uid("c_aaaaaa"),
                    column: name().column("old"),
                    ty: column(&mut after).ty.clone(),
                    to_nullable: now_nullable,
                    collation: None,
                    through_external_partitions: false,
                },
                Change::AlterColumnType {
                    uid: uid("c_aaaaaa"),
                    column: name().column("old"),
                    from: "app.d".parse().unwrap(),
                    to: "app.d".parse().unwrap(),
                    from_nullable: was_nullable,
                    to_nullable: now_nullable,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: None,
                },
            ];
        }
        for change in changes {
            for (writes, count) in [false, true].into_iter().zip(expected) {
                let found = findings(
                    &before,
                    &after,
                    &ids(),
                    &ids(),
                    &consumers("app.t.old", writes),
                    vec![change.clone()],
                );
                assert_eq!(found.len(), count, "{label}, writes={writes}: {found:?}");
                assert!(found.iter().all(|(_, f)| f.id == "consumer.unproven"));
            }
        }
    }
}

#[test]
fn domain_value_contexts_keep_null_arrays_and_nested_elements_separate() {
    for (ty, inner_nn, was_nn, now_nn, nullable, expected) in [
        ("app.outer", false, false, true, true, [0, 1]),
        ("app.outer", false, true, false, true, [1, 0]),
        ("app.outer", true, true, false, true, [0, 0]),
        ("app.outer", true, false, true, true, [0, 0]),
        ("app.outer[]", false, false, true, true, [0, 1]),
        ("app.outer[]", false, true, false, false, [1, 0]),
        ("app.outer[]", true, true, false, false, [0, 0]),
        ("app.array", false, false, true, false, [0, 1]),
        ("app.array", false, true, false, false, [1, 0]),
        ("app.array", true, true, false, false, [0, 0]),
        ("app.nested", false, false, true, false, [0, 1]),
        ("app.nested", false, true, false, false, [1, 0]),
        ("app.nested", true, true, false, false, [0, 0]),
    ] {
        let mut before = schema();
        before
            .types
            .insert("app.inner".parse().unwrap(), domain(inner_nn, None, None));
        let mut outer = domain(was_nn, None, None);
        if let pbps_model::ManagedType::Domain { base, .. } = &mut outer {
            *base = "app.inner".parse().unwrap();
        }
        before
            .types
            .insert("app.outer".parse().unwrap(), outer.clone());
        for (name, base) in [("app.array", "app.outer[]"), ("app.nested", "app.array[]")] {
            let mut spec = domain(true, None, None);
            if let pbps_model::ManagedType::Domain { base: ty, .. } = &mut spec {
                *ty = base.parse().unwrap();
            }
            before.types.insert(name.parse().unwrap(), spec);
        }
        column(&mut before).ty = ty.parse().unwrap();
        column(&mut before).nullable = nullable;
        let mut after = before.clone();
        let mut to = outer.clone();
        if let pbps_model::ManagedType::Domain { not_null, .. } = &mut to {
            *not_null = now_nn;
        }
        after.types.insert("app.outer".parse().unwrap(), to.clone());
        let mut i = ids();
        i.types
            .insert(uid("y_aaaaaa"), "app.outer".parse().unwrap());
        for rebuild in [false, true] {
            let change = if rebuild {
                Change::EnumRebuild {
                    uid: uid("y_aaaaaa"),
                    name: "app.outer".parse().unwrap(),
                    tmp: "app.tmp".parse().unwrap(),
                    step: pbps_model::EnumRebuildStep::Create {
                        definition: Box::new(to.clone()),
                        removed: vec![],
                        ordinal: 0,
                        over: None,
                        why: Some("array element constraint".into()),
                    },
                }
            } else {
                Change::AlterDomain {
                    uid: uid("y_aaaaaa"),
                    name: "app.outer".parse().unwrap(),
                    from: Box::new(outer.clone()),
                    to: Box::new(to.clone()),
                    half: if now_nn {
                        pbps_model::DomainHalf::Tighten
                    } else {
                        pbps_model::DomainHalf::Loosen
                    },
                }
            };
            for (writes, count) in [false, true].into_iter().zip(expected) {
                let found = findings(
                    &before,
                    &after,
                    &i,
                    &i,
                    &consumers("app.t.old", writes),
                    vec![change.clone()],
                );
                assert_eq!(
                    found.len(),
                    count,
                    "{ty}, inner={inner_nn}, {was_nn}->{now_nn}, writes={writes}, rebuild={rebuild}: {found:?}"
                );
            }
        }
    }
}

#[test]
fn new_columns_use_effective_nullability_and_only_whole_value_supply() {
    for (ty, nn, inherited, explicit, expected) in [
        ("app.d", true, None, None, 1),
        ("app.d", false, None, None, 0),
        ("app.d", true, Some("1"), None, 0),
        ("app.d", true, None, Some("1"), 0),
        ("app.derived", true, None, None, 1),
        ("app.derived", true, Some("1"), None, 0),
        ("app.d[]", true, Some("1"), None, 0),
    ] {
        let mut before = domain_chain_schema(inherited);
        before
            .types
            .insert("app.d".parse().unwrap(), domain(nn, inherited, None));
        let mut derived = domain(false, None, None);
        if let pbps_model::ManagedType::Domain { base, .. } = &mut derived {
            *base = "app.d".parse().unwrap();
        }
        before.types.insert("app.derived".parse().unwrap(), derived);
        let mut after = before.clone();
        let mut new = ids();
        let mut spec = Column::new(ty.parse().unwrap());
        spec.value_source = explicit.map_or(ValueSource::None, |v| ValueSource::Default(v.into()));
        after
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .insert("added".into(), spec.clone());
        new.columns.insert(uid("c_bbbbbb"), name().column("added"));
        let change = Change::AddColumn {
            uid: uid("c_bbbbbb"),
            table: name(),
            name: "added".into(),
            column: Box::new(spec),
        };
        for (writes, count) in [(false, 0), (true, expected)] {
            let found = findings(
                &before,
                &after,
                &ids(),
                &new,
                &consumers("app.t", writes),
                vec![change.clone()],
            );
            assert_eq!(
                found.len(),
                count,
                "{ty}/{nn}/{inherited:?}/{explicit:?}, writes={writes}: {found:?}"
            );
        }
        let mut empty = before.clone();
        empty.tables.clear();
        let created = Change::CreateTable {
            uid: uid("t_aaaaaa"),
            name: name(),
            table: Box::new(after.tables[&name()].clone()),
            beside_default: None,
            moves_at_most: None,
        };
        assert!(
            findings(
                &empty,
                &after,
                &IdsFile::default(),
                &new,
                &consumers("app.t", true),
                vec![created, change.clone()]
            )
            .is_empty()
        );
        let mut unrelated_before = before.clone();
        let mut unrelated_after = after.clone();
        let mut old = ids();
        let unrelated: TableName = "app.other".parse().unwrap();
        unrelated_before
            .tables
            .insert(unrelated.clone(), Table::default());
        unrelated_after
            .tables
            .insert(unrelated.clone(), Table::default());
        old.tables.insert(uid("t_cccccc"), unrelated.clone());
        new.tables.insert(uid("t_cccccc"), unrelated);
        assert!(
            findings(
                &unrelated_before,
                &unrelated_after,
                &old,
                &new,
                &consumers("app.other", true),
                vec![change]
            )
            .is_empty()
        );
    }
}

#[test]
fn partition_local_overrides_compare_before_and_after_on_the_owning_uid() {
    let (mut before, i, leaf) = partitioned();
    column(&mut before).nullable = false;
    let mut after = before.clone();
    column(&mut after).nullable = true;
    after
        .tables
        .get_mut(&leaf)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .entry("old".into())
        .or_default()
        .not_null = true;
    let local = Change::SetPartitionNotNull {
        uid: uid("t_bbbbbb"),
        table: leaf.clone(),
        column: "old".into(),
        not_null: true,
    };
    let parent = Change::AlterColumnNullability {
        uid: uid("c_aaaaaa"),
        column: name().column("old"),
        ty: ColumnType::simple("integer"),
        to_nullable: true,
        collation: None,
        through_external_partitions: false,
    };
    for (entry, writes, count) in [
        ("app.leaf.old", false, 0),
        ("app.leaf.old", true, 0),
        ("app.t.old", false, 1),
    ] {
        assert_eq!(
            findings(
                &before,
                &after,
                &i,
                &i,
                &consumers(entry, writes),
                vec![parent.clone(), local.clone()]
            )
            .len(),
            count
        );
    }
    column(&mut before).nullable = true;
    assert_eq!(
        findings(
            &before,
            &after,
            &i,
            &i,
            &consumers("app.leaf.old", true),
            vec![local]
        )
        .len(),
        1
    );
    // The DEFAULT path also sees the effective inherited value and retains the
    // differ callback for physical teardown/restoration of existing defaults.
    column(&mut before).value_source = ValueSource::Default("1".into());
    column(&mut after).value_source = ValueSource::Default("1".into());
    before
        .tables
        .get_mut(&leaf)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .entry("old".into())
        .or_default()
        .default = Some("1".into());
    let edit = Change::SetPartitionDefault {
        uid: uid("t_bbbbbb"),
        table: leaf.clone(),
        parent: name(),
        column: "old".into(),
        from: Some("1".into()),
        to: None,
        fallback: Some("1".into()),
    };
    assert!(
        findings(
            &before,
            &after,
            &i,
            &i,
            &consumers("app.leaf.old", true),
            vec![edit.clone()]
        )
        .is_empty()
    );
    column(&mut after).value_source = ValueSource::Default("2".into());
    let mut edit = edit;
    if let Change::SetPartitionDefault { fallback, .. } = &mut edit {
        *fallback = Some("2".into());
    }
    assert_eq!(
        findings(
            &before,
            &after,
            &i,
            &i,
            &consumers("app.leaf.old", true),
            vec![edit.clone()]
        )
        .len(),
        1
    );
    assert!(
        findings_with_restoration(
            &before,
            &after,
            &i,
            &i,
            &consumers("app.leaf.old", true),
            vec![edit],
            true
        )
        .is_empty()
    );
}

#[test]
fn inherited_column_addresses_follow_the_source_uid_across_a_rename() {
    for (was_not_null, expected) in [(false, 1), (true, 0)] {
        let (mut before, old, leaf) = partitioned();
        before
            .tables
            .get_mut(&leaf)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .entry("old".into())
            .or_default()
            .not_null = was_not_null;
        let mut after = before.clone();
        let parent = after.tables.get_mut(&name()).unwrap();
        let column = parent.columns.shift_remove("old").unwrap();
        parent.columns.insert("renamed".into(), column);
        let overrides = &mut after
            .tables
            .get_mut(&leaf)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns;
        overrides.remove("old");
        overrides.entry("renamed".into()).or_default().not_null = true;
        let mut new = old.clone();
        new.columns
            .insert(uid("c_aaaaaa"), name().column("renamed"));
        let change = Change::SetPartitionNotNull {
            uid: uid("t_bbbbbb"),
            table: leaf,
            column: "old".into(),
            not_null: true,
        };
        assert_eq!(
            findings(
                &before,
                &after,
                &old,
                &new,
                &consumers("app.leaf.old", true),
                vec![change]
            )
            .len(),
            expected
        );
    }
}

#[test]
fn ordinary_column_uids_do_not_follow_a_reused_name() {
    for (changed_uid, changed_name, expected) in [("c_bbbbbb", "x", 0), ("c_aaaaaa", "z", 1)] {
        let mut before = schema();
        let mut required = Column::new(ColumnType::simple("integer"));
        required.nullable = false;
        before.tables.get_mut(&name()).unwrap().columns = [
            ("x".into(), required.clone()),
            ("y".into(), required.clone()),
        ]
        .into();
        let mut old = ids();
        old.columns = [
            (uid("c_aaaaaa"), name().column("x")),
            (uid("c_bbbbbb"), name().column("y")),
        ]
        .into();
        let mut after = before.clone();
        after.tables.get_mut(&name()).unwrap().columns =
            [("z".into(), required.clone()), ("x".into(), required)].into();
        after
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .get_mut(changed_name)
            .unwrap()
            .nullable = true;
        let mut new = old.clone();
        new.columns.insert(uid("c_aaaaaa"), name().column("z"));
        new.columns.insert(uid("c_bbbbbb"), name().column("x"));
        let changes = ChangeSet {
            changes: vec![
                Change::RenameColumn {
                    uid: uid("c_aaaaaa"),
                    table: name(),
                    from: "x".into(),
                    to: "z".into(),
                    table_was: None,
                },
                Change::RenameColumn {
                    uid: uid("c_bbbbbb"),
                    table: name(),
                    from: "y".into(),
                    to: "x".into(),
                    table_was: None,
                },
                Change::AlterColumnNullability {
                    uid: uid(changed_uid),
                    column: name().column(changed_name),
                    ty: ColumnType::simple("integer"),
                    to_nullable: true,
                    collation: None,
                    through_external_partitions: false,
                },
            ]
            .into_iter()
            .map(PlannedChange::new)
            .collect(),
        };
        let sides = [
            Side {
                schema: &before,
                ids: &old,
            },
            Side {
                schema: &after,
                ids: &new,
            },
        ];
        let bound = Bindings::resolve(&consumers("app.t.z", false), &sides, &Clone::clone).unwrap();
        let mut policies = Policies::default();
        policies.suppress.push(crate::Suppression {
            rule: "consumer.breaking".into(),
            on: Some("app.t.z".into()),
            reason: "intentional rename reviewed".into(),
            until: Some("2099-01-01".into()),
        });
        let found = plan(&changes, &bound, &sides, &policies, &context(), &unchanged);
        assert_eq!(
            found.len(),
            expected,
            "{changed_uid}/{changed_name}: {found:?}"
        );
        assert!(
            found
                .iter()
                .all(|(_, finding)| finding.id == "consumer.unproven")
        );
    }
}
