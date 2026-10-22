use super::{Plan, Read, Staged};
use crate::deploy::{StagedRead, refuse_pinned_movement};
use pbps_model::{
    Change, ChangeSet, GrantTarget, IdsFile, Permission, PlannedChange, Role, Schema,
};

fn schema(roles: &[(&str, &[Permission])]) -> Schema {
    Schema {
        roles: roles
            .iter()
            .map(|(name, permissions)| {
                let grants = if permissions.is_empty() {
                    Default::default()
                } else {
                    [(
                        GrantTarget::Schema("dbo".into()),
                        permissions.iter().copied().collect(),
                    )]
                    .into()
                };
                (
                    pbps_model::RoleName::from(*name),
                    Role {
                        description: None,
                        grants,
                    },
                )
            })
            .collect(),
        ..Schema::default()
    }
}

fn changes(steps: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: steps.into_iter().map(PlannedChange::new).collect(),
    }
}

fn rename(uid: &str, from: &str, to: &str) -> Change {
    Change::RenameRole {
        uid: uid.parse().unwrap(),
        from: from.into(),
        to: to.into(),
    }
}

fn drop_role(uid: &str, name: &str) -> Change {
    Change::DropRole {
        uid: uid.parse().unwrap(),
        name: name.into(),
        members: vec![],
    }
}

fn ids(roles: &[(&str, &str)]) -> IdsFile {
    IdsFile {
        roles: roles
            .iter()
            .map(|(uid, name)| (uid.parse().unwrap(), pbps_model::RoleName::from(*name)))
            .collect(),
        ..IdsFile::default()
    }
}

fn compare(cs: &ChangeSet, ids: &IdsFile, before: &Schema, after: &Schema) -> Result<(), String> {
    refuse_pinned_movement(
        &pbps_mssql::Mssql,
        Read::transactional(Plan { changes: cs, ids }),
        before,
        after,
        "test",
    )
    .map_err(|e| format!("{e:#}"))
}

#[test]
fn a_role_handoff_compares_only_the_surviving_identitys_grants() {
    use Permission::{Insert, Select, Update};
    let cs = changes(vec![
        drop_role("r_aaaaaa", "a"),
        rename("r_bbbbbb", "b", "a"),
    ]);
    let ids = ids(&[("r_bbbbbb", "a")]);
    let before = schema(&[("a", &[Insert]), ("b", &[Select])]);
    compare(&cs, &ids, &before, &schema(&[("a", &[Select])])).unwrap();
    for held in [&[][..], &[Select, Update][..], &[Insert][..]] {
        let error = compare(&cs, &ids, &before, &schema(&[("a", held)])).unwrap_err();
        assert!(
            error.contains("role a does not hold on schema::dbo"),
            "{error}"
        );
    }
}

#[test]
fn a_replacement_role_starts_without_the_retired_holders_grants() {
    use Permission::{Insert, Select};
    let cs = changes(vec![
        drop_role("r_aaaaaa", "a"),
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "a".into(),
        },
        Change::Grant {
            role: "a".into(),
            target: GrantTarget::Schema("dbo".into()),
            permissions: [Select].into(),
        },
    ]);
    let ids = ids(&[("r_bbbbbb", "a")]);
    let before = schema(&[("a", &[Insert])]);
    compare(&cs, &ids, &before, &schema(&[("a", &[Select])])).unwrap();
    for held in [&[][..], &[Select, Insert][..]] {
        assert!(compare(&cs, &ids, &before, &schema(&[("a", held)])).is_err());
    }
}

#[test]
fn a_new_role_at_a_vacated_name_is_not_forwarded_through_the_old_rename() {
    use Permission::{Insert, Select};
    let cs = changes(vec![
        rename("r_aaaaaa", "a", "b"),
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "a".into(),
        },
        Change::Grant {
            role: "a".into(),
            target: GrantTarget::Schema("dbo".into()),
            permissions: [Insert].into(),
        },
    ]);
    let ids = ids(&[("r_aaaaaa", "b"), ("r_bbbbbb", "a")]);
    let before = schema(&[("a", &[Select])]);
    let after = schema(&[("a", &[Insert]), ("b", &[Select])]);
    compare(&cs, &ids, &before, &after).unwrap();
    for name in ["a", "b"] {
        let mut robbed = after.clone();
        robbed.roles.get_mut(name).unwrap().grants.clear();
        let error = compare(&cs, &ids, &before, &robbed).unwrap_err();
        assert!(
            error.contains(&format!("role {name} does not hold")),
            "{error}"
        );
    }
}

#[test]
fn role_grants_follow_repeated_moves_and_temporary_names() {
    use Permission::{Insert, Select};
    for (cs, final_ids, before, after) in [
        (
            changes(vec![
                rename("r_aaaaaa", "a", "tmp"),
                rename("r_aaaaaa", "tmp", "b"),
            ]),
            ids(&[("r_aaaaaa", "b")]),
            schema(&[("a", &[Select])]),
            schema(&[("b", &[Select])]),
        ),
        (
            changes(vec![
                rename("r_aaaaaa", "a", "tmp"),
                rename("r_bbbbbb", "b", "a"),
                rename("r_aaaaaa", "tmp", "b"),
            ]),
            ids(&[("r_aaaaaa", "b"), ("r_bbbbbb", "a")]),
            schema(&[("a", &[Select]), ("b", &[Insert])]),
            schema(&[("b", &[Select]), ("a", &[Insert])]),
        ),
    ] {
        compare(&cs, &final_ids, &before, &after).unwrap();
        let mut robbed = after.clone();
        robbed.roles.get_mut("b").unwrap().grants.clear();
        assert!(compare(&cs, &final_ids, &before, &robbed).is_err());
    }
}

#[test]
fn staged_role_reads_keep_grants_at_the_observed_boundary() {
    use Permission::{Insert, Select};
    let rename = changes(vec![rename("r_aaaaaa", "a", "b")]);
    let final_ids = ids(&[("r_aaaaaa", "b")]);
    let staged = Staged::new(Plan {
        changes: &rename,
        ids: &final_ids,
    })
    .unwrap();
    let before = schema(&[("a", &[Select])]);
    let after = schema(&[("b", &[Select])]);
    for baseline in [&before, &after] {
        for (held, accepted) in [
            (&[Select][..], true),
            (&[][..], false),
            (&[Select, Insert][..], false),
        ] {
            let result = refuse_pinned_movement(
                &pbps_mssql::Mssql,
                staged.read(StagedRead::Closing { total: 1 }),
                baseline,
                &schema(&[("b", held)]),
                "test",
            );
            assert_eq!(result.is_ok(), accepted, "{result:?}");
        }
    }
    // Membership removal precedes DROP ROLE; retiring the planned UID must
    // not hide permission movement while the role is still observed.
    let mut step = drop_role("r_aaaaaa", "a");
    if let Change::DropRole { members, .. } = &mut step {
        members.push("member".into());
    }
    let dropped = changes(vec![step]);
    let empty_ids = IdsFile::default();
    let staged = Staged::new(Plan {
        changes: &dropped,
        ids: &empty_ids,
    })
    .unwrap();
    for (held, accepted) in [
        (&[Select][..], true),
        (&[][..], false),
        (&[Select, Insert][..], false),
    ] {
        let result = refuse_pinned_movement(
            &pbps_mssql::Mssql,
            staged.read(StagedRead::Checkpoint {
                completed: 1,
                total: 2,
            }),
            &before,
            &schema(&[("a", held)]),
            "test",
        );
        assert_eq!(result.is_ok(), accepted, "{result:?}");
    }
}

#[test]
fn ambiguous_baseline_roles_do_not_overwrite_each_others_grant_comparison() {
    use Permission::{Insert, Select};
    let cs = changes(vec![rename("r_aaaaaa", "a", "b")]);
    let ids = ids(&[("r_aaaaaa", "b")]);
    let before = schema(&[("a", &[Select]), ("b", &[Insert])]);
    for held in [&[Select][..], &[Insert][..]] {
        assert!(compare(&cs, &ids, &before, &schema(&[("b", held)])).is_err());
    }
}

#[test]
fn hostile_role_handoffs_keep_raw_grant_holders_distinct() {
    use Permission::{Insert, Select};
    let name = "role\nline\\\u{202e}名前";
    let shown = pbps_model::RoleName::from(name).shown().to_string();
    let cs = changes(vec![
        drop_role("r_aaaaaa", name),
        rename("r_bbbbbb", &shown, name),
    ]);
    let final_ids = ids(&[("r_bbbbbb", name)]);
    let before = schema(&[(name, &[Insert]), (&shown, &[Select])]);
    compare(&cs, &final_ids, &before, &schema(&[(name, &[Select])])).unwrap();
    assert!(compare(&cs, &final_ids, &before, &schema(&[(name, &[Insert])])).is_err());
    assert!(compare(&cs, &final_ids, &before, &schema(&[(&shown, &[Select])])).is_err());
}

#[test]
fn serial_usage_readback_tracks_the_union_and_refuses_missing_or_unplanned_access() {
    use pbps_model::{Column, OwnedSequence, Table, ValueSource};
    for target in ["dbo.t", "dbo.p1", "dbo.p2a"] {
        let mut before = Schema::default();
        let mut table = Table::default();
        let mut column = Column::new("integer".parse().unwrap());
        column.value_source = ValueSource::Sequence(OwnedSequence::serial());
        table.columns.insert("id".into(), column);
        before.tables.insert("dbo.t".parse().unwrap(), table);
        for (name, parent) in [("dbo.p1", "dbo.t"), ("dbo.p2a", "dbo.p1")] {
            before.tables.insert(
                name.parse().unwrap(),
                Table {
                    partition_of: Some(pbps_model::PartitionOf {
                        parent: parent.parse().unwrap(),
                        bound: pbps_model::PartitionBound::Default,
                        columns: Default::default(),
                    }),
                    ..Default::default()
                },
            );
        }
        before.roles.insert(
            "writer".into(),
            Role {
                grants: [(
                    target.parse().unwrap(),
                    [Permission::Insert, Permission::SerialSequences].into(),
                )]
                .into(),
                ..Default::default()
            },
        );
        let cs = changes(vec![Change::Revoke {
            role: "writer".into(),
            target: target.parse().unwrap(),
            permissions: [Permission::Insert].into(),
        }]);
        let mut ids = ids(&[("r_aaaaaa", "writer")]);
        ids.tables
            .insert("t_aaaaaa".parse().unwrap(), "dbo.t".parse().unwrap());
        ids.columns
            .insert("c_aaaaaa".parse().unwrap(), "dbo.t.id".parse().unwrap());
        for (uid, name) in [("t_bbbbbb", "dbo.p1"), ("t_cccccc", "dbo.p2a")] {
            ids.tables
                .insert(uid.parse().unwrap(), name.parse().unwrap());
        }
        let mut after = before.clone();
        after.roles.get_mut("writer").unwrap().grants = [(
            "sequence::dbo.t.id".parse().unwrap(),
            [Permission::Usage].into(),
        )]
        .into();
        compare(&cs, &ids, &before, &after).unwrap();
        let preserved = after.clone();
        after.roles.get_mut("writer").unwrap().grants.clear();
        assert!(
            compare(&cs, &ids, &before, &after)
                .unwrap_err()
                .contains("usage")
        );
        after = preserved.clone();
        after
            .roles
            .get_mut("writer")
            .unwrap()
            .grants
            .get_mut(&"sequence::dbo.t.id".parse().unwrap())
            .unwrap()
            .insert(Permission::Update);
        assert!(
            compare(&cs, &ids, &before, &after)
                .unwrap_err()
                .contains("update")
        );
        // Adding INSERT folds the same physical entry into the marker.
        let cs = changes(vec![Change::Grant {
            role: "writer".into(),
            target: target.parse().unwrap(),
            permissions: [Permission::Insert, Permission::SerialSequences].into(),
        }]);
        compare(&cs, &ids, &preserved, &before).unwrap();
    }
}

#[test]
fn a_detach_preserves_the_roots_usage_and_an_attach_checks_its_new_marker() {
    use pbps_model::{
        Column, OwnedSequence, PartitionBound, PartitionColumn, PartitionOf, Table, ValueSource,
    };
    let mut before = Schema::default();
    let mut serial = Column::new("integer".parse().unwrap());
    serial.value_source = ValueSource::Sequence(OwnedSequence::serial());
    before.tables.insert(
        "dbo.t".parse().unwrap(),
        Table {
            columns: [("id".into(), serial)].into(),
            ..Default::default()
        },
    );
    before.tables.insert(
        "dbo.p1".parse().unwrap(),
        Table {
            partition_of: Some(PartitionOf {
                parent: "dbo.t".parse().unwrap(),
                bound: PartitionBound::Default,
                columns: [(
                    "id".into(),
                    PartitionColumn {
                        default: Some("0".into()),
                        not_null: false,
                    },
                )]
                .into(),
            }),
            ..Default::default()
        },
    );
    before.roles.insert(
        "writer".into(),
        Role {
            grants: [(
                "dbo.p1".parse().unwrap(),
                [Permission::Insert, Permission::SerialSequences].into(),
            )]
            .into(),
            ..Default::default()
        },
    );
    let mut ids = ids(&[("r_aaaaaa", "writer")]);
    ids.tables = [
        ("t_aaaaaa".parse().unwrap(), "dbo.t".parse().unwrap()),
        ("t_bbbbbb".parse().unwrap(), "dbo.p1".parse().unwrap()),
    ]
    .into();
    ids.columns = [
        ("c_aaaaaa".parse().unwrap(), "dbo.t.id".parse().unwrap()),
        ("c_bbbbbb".parse().unwrap(), "dbo.p1.id".parse().unwrap()),
    ]
    .into();
    let mut detached = before.clone();
    let mut plain = Column::new("integer".parse().unwrap());
    plain.value_source = ValueSource::Default("0".into());
    detached.tables.insert(
        "dbo.p1".parse().unwrap(),
        Table {
            columns: [("id".into(), plain)].into(),
            ..Default::default()
        },
    );
    detached.roles.get_mut("writer").unwrap().grants = [
        ("dbo.p1".parse().unwrap(), [Permission::Insert].into()),
        (
            "sequence::dbo.t.id".parse().unwrap(),
            [Permission::Usage].into(),
        ),
    ]
    .into();
    let cs = changes(vec![Change::DetachPartition {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "dbo.p1".parse().unwrap(),
        parent: "dbo.t".parse().unwrap(),
        names: vec![],
        shape: Box::new(detached.tables[&"dbo.p1".parse().unwrap()].clone()),
    }]);
    compare(&cs, &ids, &before, &detached).unwrap();
    let mut missing = detached.clone();
    missing
        .roles
        .get_mut("writer")
        .unwrap()
        .grants
        .remove(&"sequence::dbo.t.id".parse().unwrap());
    assert!(compare(&cs, &ids, &before, &missing).is_err());
    let cs = changes(vec![
        Change::AttachPartition {
            uid: "t_bbbbbb".parse().unwrap(),
            table: "dbo.p1".parse().unwrap(),
            parent: "dbo.t".parse().unwrap(),
            bound: PartitionBound::Default,
            key: vec![],
            shape: Box::new(before.tables[&"dbo.p1".parse().unwrap()].clone()),
        },
        Change::Grant {
            role: "writer".into(),
            target: "dbo.p1".parse().unwrap(),
            permissions: [Permission::SerialSequences].into(),
        },
    ]);
    ids.columns.remove(&"c_bbbbbb".parse().unwrap());
    compare(&cs, &ids, &missing, &before).unwrap();
    let mut ungranted = before.clone();
    ungranted
        .roles
        .get_mut("writer")
        .unwrap()
        .grants
        .get_mut(&"dbo.p1".parse().unwrap())
        .unwrap()
        .remove(&Permission::SerialSequences);
    assert!(compare(&cs, &ids, &missing, &ungranted).is_err());
}

#[test]
fn serial_sequence_grants_disappear_with_the_column_uid_across_renames() {
    use pbps_diff::{Side, diff};
    use pbps_model::{Column, Hints, OwnedSequence, Table, TableName, ValueSource};

    for rename_table in [false, true] {
        for rename_column in [false, true] {
            for drop_column in [true, false] {
                let from = TableName::new("app", "original");
                let to = if rename_table {
                    TableName::new("app", "renamed")
                } else {
                    from.clone()
                };
                let mut table = Table::default();
                for name in ["id", "other"] {
                    let mut sequence = OwnedSequence::serial();
                    sequence.name = Some(format!("serial_{name}"));
                    let mut column = Column::new("integer".parse().unwrap()).not_null();
                    column.value_source = ValueSource::Sequence(sequence);
                    table.columns.insert(name.into(), column);
                }
                let before = Schema {
                    tables: [(from.clone(), table)].into(),
                    roles: [(
                        "reader".into(),
                        Role {
                            grants: [
                                (
                                    GrantTarget::Schema("app".into()),
                                    [Permission::Usage].into(),
                                ),
                                (
                                    GrantTarget::Sequence(from.column("id")),
                                    [Permission::Usage].into(),
                                ),
                                (
                                    GrantTarget::Sequence(from.column("other")),
                                    [Permission::Select].into(),
                                ),
                            ]
                            .into(),
                            ..Default::default()
                        },
                    )]
                    .into(),
                    ..Default::default()
                };
                let before_ids = IdsFile {
                    tables: [("t_aaaaaa".parse().unwrap(), from.clone())].into(),
                    columns: [
                        ("c_aaaaaa".parse().unwrap(), from.column("id")),
                        ("c_bbbbbb".parse().unwrap(), from.column("other")),
                    ]
                    .into(),
                    roles: [("r_aaaaaa".parse().unwrap(), "reader".into())].into(),
                    ..Default::default()
                };
                let mut after = before.clone();
                let mut after_ids = before_ids.clone();
                let mut table = after.tables.remove(&from).unwrap();
                let mut column = table.columns.shift_remove("id").unwrap();
                if drop_column {
                    after_ids.columns.remove(&"c_aaaaaa".parse().unwrap());
                } else {
                    column.value_source = ValueSource::None;
                    let name = if rename_column { "renamed_id" } else { "id" };
                    table.columns.insert(name.into(), column);
                    after_ids
                        .columns
                        .insert("c_aaaaaa".parse().unwrap(), to.column(name));
                }
                // A dropped name can pass to another serial column. Its
                // sequence and grant survive under that different UID.
                let survivor = if drop_column && rename_column {
                    "id"
                } else {
                    "other"
                };
                let column = table.columns.shift_remove("other").unwrap();
                table.columns.insert(survivor.into(), column);
                after.tables.insert(to.clone(), table);
                after_ids
                    .tables
                    .insert("t_aaaaaa".parse().unwrap(), to.clone());
                after_ids
                    .columns
                    .insert("c_bbbbbb".parse().unwrap(), to.column(survivor));
                let target = GrantTarget::Sequence(to.column(survivor));
                after.roles.get_mut("reader").unwrap().grants = [
                    (
                        GrantTarget::Schema("app".into()),
                        [Permission::Usage].into(),
                    ),
                    (target.clone(), [Permission::Select].into()),
                ]
                .into();
                let pg = pbps_pg::Postgres::new();
                let cs = diff(
                    Side {
                        schema: &before,
                        ids: &before_ids,
                    },
                    Side {
                        schema: &after,
                        ids: &after_ids,
                    },
                    &pg,
                    &Hints::default(),
                )
                .unwrap();
                if drop_column {
                    assert!(
                        cs.changes.iter().all(|p| p.change.grant().is_none()),
                        "{cs:?}"
                    );
                    assert!(cs.changes.iter().any(|p| matches!(&p.change,
                        Change::DropColumn { uid, column } if *uid == "c_aaaaaa".parse().unwrap() && column == &to.column("id"))), "{cs:?}");
                }
                let check = |after: &Schema| {
                    refuse_pinned_movement(
                        &pg,
                        Read::transactional(Plan {
                            changes: &cs,
                            ids: &after_ids,
                        }),
                        &before,
                        after,
                        "test",
                    )
                };
                let result = check(&after);
                assert!(
                    result.is_ok(),
                    "table rename={rename_table}, column rename={rename_column}, drop={drop_column}: {result:?}"
                );
                let mut missing = after.clone();
                missing
                    .roles
                    .get_mut("reader")
                    .unwrap()
                    .grants
                    .remove(&target);
                let error = check(&missing).unwrap_err().to_string();
                assert!(error.contains("role reader"), "{error}");
                let mut widened = after.clone();
                widened
                    .roles
                    .get_mut("reader")
                    .unwrap()
                    .grants
                    .get_mut(&target)
                    .unwrap()
                    .insert(Permission::Update);
                let error = check(&widened).unwrap_err().to_string();
                assert!(error.contains("role reader"), "{error}");
            }
        }
    }
}
