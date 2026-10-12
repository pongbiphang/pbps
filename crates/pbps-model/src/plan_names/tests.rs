use super::*;
use crate::{Column, Table};

fn uid(kind: UidKind, name: &str) -> Uid {
    Uid::derived(kind, name, 0)
}
fn t(name: &str) -> TableName {
    TableName::new("app", name)
}
fn table(name: &str) -> Uid {
    uid(UidKind::Table, name)
}
fn column(name: &str) -> Uid {
    uid(UidKind::Column, name)
}
fn p(change: Change) -> PlannedChange {
    PlannedChange::new(change)
}
fn rename(uid: &Uid, from: TableName, to: TableName) -> PlannedChange {
    p(Change::RenameTable {
        uid: uid.clone(),
        from,
        to,
        defaults: vec![],
    })
}
fn ids(tables: &[(&Uid, TableName)], columns: &[(&Uid, ColumnRef)]) -> IdsFile {
    IdsFile {
        tables: tables
            .iter()
            .map(|(u, n)| ((*u).clone(), n.clone()))
            .collect(),
        columns: columns
            .iter()
            .map(|(u, n)| ((*u).clone(), n.clone()))
            .collect(),
        ..IdsFile::default()
    }
}
fn alter(uid: &Uid, address: ColumnRef) -> PlannedChange {
    p(Change::AlterColumnDefault {
        uid: uid.clone(),
        column: address,
        from: None,
        to: Some("7".into()),
    })
}

#[test]
fn a_column_finds_its_untouched_owner_in_the_final_mapping() {
    let (owner, c) = (table("a"), column("c"));
    let changes = vec![alter(&c, t("a").column("c"))];
    let after = ids(&[(&owner, t("a"))], &[(&c, t("a").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    for at in [At::Start, At::Before(0), At::After(0), At::End] {
        assert_eq!(names.column_at(&c, at), Some((&owner, "c")));
        assert_eq!(names.holder(&t("a"), at), Holder::Tracked(&owner));
    }
    assert_eq!(names.holder(&t("unmanaged"), At::Start), Holder::Unmanaged);
    assert_eq!(names.column_at(&column("missing"), At::End), None);
}

#[test]
fn a_dropped_name_and_its_next_holder_have_separate_histories() {
    let (a, b) = (table("a"), table("b"));
    let changes = vec![
        p(Change::DropTable {
            uid: a.clone(),
            name: t("a"),
            detach_from: None,
        }),
        rename(&b, t("b"), t("a")),
    ];
    let after = ids(&[(&b, t("a"))], &[]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.holder(&t("a"), At::Before(0)), Holder::Tracked(&a));
    assert_eq!(names.stored(&t("a"), At::Before(0)), Some(t("a")));
    assert_eq!(names.holder(&t("a"), At::After(0)), Holder::Vacant);
    assert_eq!(names.stored(&t("a"), At::After(0)), None);
    assert_eq!(names.holder(&t("a"), At::After(1)), Holder::Tracked(&b));
    assert_eq!(names.stored(&t("a"), At::End), Some(t("b")));
    assert_eq!(names.table_at(&a, At::End), None);
}

#[test]
fn a_source_named_change_in_a_chain_keeps_its_own_original_table() {
    let (a, b, c) = (table("a"), table("b"), column("c"));
    let changes = vec![
        alter(&c, t("b").column("c")),
        rename(&b, t("b"), t("c")),
        rename(&a, t("a"), t("b")),
    ];
    let after = ids(&[(&a, t("b")), (&b, t("c"))], &[(&c, t("c").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.stored(&t("b"), At::Before(0)), Some(t("b")));
    assert_eq!(names.stored(&t("b"), At::End), Some(t("a")));
    assert_eq!(names.column_at(&c, At::Start), Some((&b, "c")));
}

#[test]
fn a_swap_uses_the_temporary_holder_without_chasing_the_other_identity() {
    let (a, b) = (table("a"), table("b"));
    let changes = vec![
        rename(&a, t("a"), t("tmp")),
        rename(&b, t("b"), t("a")),
        rename(&a, t("tmp"), t("b")),
    ];
    let after = ids(&[(&a, t("b")), (&b, t("a"))], &[]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.holder(&t("tmp"), At::Start), Holder::Vacant);
    assert_eq!(names.holder(&t("tmp"), At::After(0)), Holder::Tracked(&a));
    assert_eq!(names.stored(&t("b"), At::End), Some(t("a")));
    assert_eq!(names.stored(&t("a"), At::End), Some(t("b")));
    assert_eq!(names.holder(&t("tmp"), At::End), Holder::Vacant);
}

#[test]
fn a_schema_move_leaves_a_vacancy_not_an_unmanaged_or_stale_holder() {
    let a = table("a");
    let moved = TableName::new("other", "a");
    let changes = vec![rename(&a, t("a"), moved.clone())];
    let after = ids(&[(&a, moved.clone())], &[]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.holder(&t("a"), At::End), Holder::Vacant);
    assert_eq!(names.holder(&t("unknown"), At::End), Holder::Unmanaged);
    assert_eq!(names.stored(&t("a"), At::End), None);
    assert_eq!(names.stored(&moved, At::End), Some(t("a")));
}

#[test]
fn column_handoffs_follow_the_owning_identity_through_its_table_rename() {
    let (a, c, d) = (table("a"), column("c"), column("d"));
    let changes = vec![
        rename(&a, t("a"), t("b")),
        p(Change::DropColumn {
            uid: c.clone(),
            column: t("b").column("c"),
        }),
        p(Change::RenameColumn {
            uid: d.clone(),
            table: t("b"),
            from: "d".into(),
            to: "c".into(),
            table_was: Some(t("a")),
        }),
    ];
    let after = ids(&[(&a, t("b"))], &[(&d, t("b").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(
        names.stored_column(&t("b").column("c"), At::Before(1)),
        Some(t("a").column("c"))
    );
    assert_eq!(
        names.column_holder(&t("b").column("c"), At::After(1)),
        Holder::Vacant
    );
    assert_eq!(
        names.stored_column(&t("b").column("c"), At::End),
        Some(t("a").column("d"))
    );
    assert_eq!(names.column_at(&c, At::End), None);
}

#[test]
fn roles_use_the_same_handoff_rules_in_a_separate_namespace() {
    let (a, b) = (uid(UidKind::Role, "a"), uid(UidKind::Role, "b"));
    let changes = vec![
        p(Change::DropRole {
            uid: a.clone(),
            name: "a".into(),
            members: vec![],
        }),
        p(Change::RenameRole {
            uid: b.clone(),
            from: "b".into(),
            to: "a".into(),
        }),
    ];
    let after = IdsFile {
        roles: [(b.clone(), "a".into())].into(),
        ..IdsFile::default()
    };
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.role_holder("a", At::Before(0)), Holder::Tracked(&a));
    assert_eq!(names.role_holder("a", At::After(0)), Holder::Vacant);
    assert_eq!(names.role_holder("a", At::End), Holder::Tracked(&b));
    assert_eq!(names.role_at(&b, At::Start), Some("b"));
    assert_eq!(names.holder(&t("a"), At::End), Holder::Unmanaged);
}

#[test]
fn a_created_tables_inline_column_has_no_stored_address() {
    let (a, c) = (table("a"), column("c"));
    let mut shape = Table::default();
    shape
        .columns
        .insert("c".into(), Column::new("int".parse().unwrap()));
    let changes = vec![p(Change::CreateTable {
        uid: a.clone(),
        name: t("a"),
        table: Box::new(shape),
        beside_default: None,
        moves_at_most: None,
    })];
    let after = ids(&[(&a, t("a"))], &[(&c, t("a").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.column_at(&c, At::Start), None);
    assert_eq!(names.column_at(&c, At::End), Some((&a, "c")));
    assert_eq!(names.stored(&t("a"), At::End), None);
    assert_eq!(names.stored_column(&t("a").column("c"), At::End), None);
}

#[test]
fn dropping_a_table_ends_the_lifetime_of_its_known_columns() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![
        alter(&c, t("a").column("c")),
        p(Change::DropTable {
            uid: a.clone(),
            name: t("a"),
            detach_from: None,
        }),
    ];
    let after = IdsFile::default();
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.column_at(&c, At::Start), Some((&a, "c")));
    assert_eq!(names.column_at(&c, At::After(0)), Some((&a, "c")));
    assert_eq!(names.column_at(&c, At::End), None);
}

#[test]
fn an_unmanaged_owner_does_not_get_a_synthesized_uid() {
    let changes = vec![alter(&column("c"), t("unknown").column("c"))];
    assert!(matches!(
        PlanNames::of(&changes, &IdsFile::default()),
        Err(PlanNamesError::Owner { .. })
    ));
}

#[test]
fn an_inconsistent_final_mapping_is_refused() {
    let a = table("a");
    let changes = vec![rename(&a, t("a"), t("b"))];
    assert!(matches!(
        PlanNames::of(&changes, &ids(&[(&a, t("c"))], &[])),
        Err(PlanNamesError::End { namespace: "table" })
    ));
}

#[test]
fn a_rename_cannot_read_a_source_its_uid_has_already_left() {
    let a = table("a");
    let changes = vec![rename(&a, t("a"), t("b")), rename(&a, t("a"), t("c"))];
    assert!(matches!(
        PlanNames::of(&changes, &ids(&[(&a, t("c"))], &[])),
        Err(PlanNamesError::Source { step: 1, .. })
    ));
}

#[test]
fn two_uids_cannot_hold_one_address_even_at_an_intermediate_step() {
    let (a, b) = (table("a"), table("b"));
    let changes = vec![rename(&a, t("a"), t("b")), rename(&b, t("b"), t("c"))];
    assert!(matches!(
        PlanNames::of(&changes, &ids(&[(&a, t("b")), (&b, t("c"))], &[])),
        Err(PlanNamesError::Collision { boundary: 1, .. })
    ));
}

#[test]
fn generated_valid_moves_are_injective_at_every_boundary() {
    let owners: Vec<_> = (0..4).map(|i| table(&format!("t{i}"))).collect();
    for seed in 0..32usize {
        let mut held: BTreeMap<_, _> = owners
            .iter()
            .enumerate()
            .map(|(i, u)| (u.clone(), t(&format!("slot{i}"))))
            .collect();
        let mut snapshots = vec![held.clone()];
        let mut changes = Vec::new();
        let mut random = seed + 1;
        for _ in 0..24 {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let owner = &owners[random % owners.len()];
            let free = (0..5)
                .map(|i| t(&format!("slot{i}")))
                .find(|n| !held.values().any(|v| v == n))
                .unwrap();
            changes.push(rename(owner, held[owner].clone(), free.clone()));
            held.insert(owner.clone(), free);
            snapshots.push(held.clone());
        }
        let after = IdsFile {
            tables: held,
            ..IdsFile::default()
        };
        let names = PlanNames::of(&changes, &after).unwrap();
        for (boundary, expected) in snapshots.iter().enumerate() {
            let at = if boundary == 0 {
                At::Start
            } else {
                At::After(boundary - 1)
            };
            let actual: BTreeMap<_, _> = owners
                .iter()
                .map(|u| (u.clone(), names.table_at(u, at).unwrap().clone()))
                .collect();
            assert_eq!(&actual, expected);
            let unique: std::collections::BTreeSet<_> = actual.values().collect();
            assert_eq!(unique.len(), owners.len());
            for (u, n) in expected {
                assert_eq!(names.holder(n, at), Holder::Tracked(u));
            }
        }
    }
}

#[test]
#[should_panic(expected = "change position outside indexed plan")]
fn an_invalid_position_is_not_reported_as_an_absent_object() {
    let after = IdsFile::default();
    let names = PlanNames::of(&[], &after).unwrap();
    names.holder(&t("a"), At::Before(0));
}

#[test]
fn an_added_column_is_absent_until_its_own_change() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![p(Change::AddColumn {
        uid: c.clone(),
        table: t("a"),
        name: "c".into(),
        column: Box::new(Column::new("int".parse().unwrap())),
    })];
    let after = ids(&[(&a, t("a"))], &[(&c, t("a").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(
        names.column_holder(&t("a").column("c"), At::Start),
        Holder::Vacant
    );
    assert_eq!(names.column_at(&c, At::After(0)), Some((&a, "c")));
    assert_eq!(names.stored_column(&t("a").column("c"), At::End), None);
}

#[test]
fn an_added_column_is_not_mistaken_for_an_inline_created_column() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![
        p(Change::CreateTable {
            uid: a.clone(),
            name: t("a"),
            table: Box::new(Table::default()),
            beside_default: None,
            moves_at_most: None,
        }),
        p(Change::AddColumn {
            uid: c.clone(),
            table: t("a"),
            name: "c".into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        }),
    ];
    let after = ids(&[(&a, t("a"))], &[(&c, t("a").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.column_at(&c, At::After(0)), None);
    assert_eq!(names.column_at(&c, At::End), Some((&a, "c")));
}

#[test]
fn final_ids_do_not_invent_a_column_missing_from_the_create_shape() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![p(Change::CreateTable {
        uid: a.clone(),
        name: t("a"),
        table: Box::new(Table::default()),
        beside_default: None,
        moves_at_most: None,
    })];
    let after = ids(&[(&a, t("a"))], &[(&c, t("a").column("c"))]);
    assert!(matches!(
        PlanNames::of(&changes, &after),
        Err(PlanNamesError::End {
            namespace: "column"
        })
    ));
}

#[test]
fn an_observation_cannot_invent_a_column_missing_from_the_create_shape() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![
        p(Change::CreateTable {
            uid: a.clone(),
            name: t("a"),
            table: Box::new(Table::default()),
            beside_default: None,
            moves_at_most: None,
        }),
        alter(&c, t("a").column("c")),
    ];
    let after = ids(&[(&a, t("a"))], &[(&c, t("a").column("c"))]);
    assert!(matches!(
        PlanNames::of(&changes, &after),
        Err(PlanNamesError::Source {
            namespace: "column",
            step: 1,
            ..
        })
    ));
}

#[test]
fn dropping_a_column_before_its_table_does_not_drop_the_column_twice() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![
        p(Change::DropColumn {
            uid: c.clone(),
            column: t("a").column("c"),
        }),
        p(Change::DropTable {
            uid: a.clone(),
            name: t("a"),
            detach_from: None,
        }),
    ];
    let after = IdsFile::default();
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.column_at(&c, At::Start), Some((&a, "c")));
    assert_eq!(names.column_at(&c, At::After(0)), None);
    assert_eq!(names.column_at(&c, At::End), None);
}

#[test]
fn columns_cannot_trade_their_owning_table_uid() {
    let (a, b, c) = (table("a"), table("b"), column("c"));
    let changes = vec![alter(&c, t("a").column("c"))];
    let after = ids(&[(&a, t("a")), (&b, t("b"))], &[(&c, t("b").column("c"))]);
    assert!(matches!(
        PlanNames::of(&changes, &after),
        Err(PlanNamesError::ColumnOwner { .. })
    ));
}

#[test]
fn a_column_rename_cannot_take_an_untouched_columns_name() {
    let (a, c, d) = (table("a"), column("c"), column("d"));
    let changes = vec![
        p(Change::RenameColumn {
            uid: c.clone(),
            table: t("a"),
            from: "c".into(),
            to: "d".into(),
            table_was: None,
        }),
        p(Change::RenameColumn {
            uid: d.clone(),
            table: t("a"),
            from: "d".into(),
            to: "e".into(),
            table_was: None,
        }),
    ];
    let after = ids(
        &[(&a, t("a"))],
        &[(&c, t("a").column("d")), (&d, t("a").column("e"))],
    );
    assert!(matches!(
        PlanNames::of(&changes, &after),
        Err(PlanNamesError::Collision {
            namespace: "column",
            boundary: 1,
            ..
        })
    ));
}

#[test]
fn the_uid_kind_is_checked_even_for_retired_identities() {
    let changes = vec![p(Change::DropTable {
        uid: column("wrong"),
        name: t("a"),
        detach_from: None,
    })];
    assert!(matches!(
        PlanNames::of(&changes, &IdsFile::default()),
        Err(PlanNamesError::Kind {
            namespace: "table",
            ..
        })
    ));
}

#[test]
fn a_detach_introduces_owned_column_identities_without_creating_its_table() {
    let (a, c) = (table("a"), column("c"));
    let mut shape = Table::default();
    shape
        .columns
        .insert("c".into(), Column::new("int".parse().unwrap()));
    let changes = vec![p(Change::DetachPartition {
        uid: a.clone(),
        table: t("a"),
        parent: t("parent"),
        names: vec![],
        shape: Box::new(shape),
    })];
    let after = ids(&[(&a, t("a"))], &[(&c, t("a").column("c"))]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.table_at(&a, At::Start), Some(&t("a")));
    assert_eq!(names.column_at(&c, At::Start), None);
    assert_eq!(names.column_at(&c, At::End), Some((&a, "c")));
}

#[test]
fn an_attach_retires_known_column_identities_without_dropping_its_table() {
    let (a, c) = (table("a"), column("c"));
    let changes = vec![
        alter(&c, t("a").column("c")),
        p(Change::AttachPartition {
            uid: a.clone(),
            table: t("a"),
            parent: t("parent"),
            bound: crate::PartitionBound::Default,
            shape: Box::new(Table::default()),
        }),
    ];
    let after = ids(&[(&a, t("a"))], &[]);
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.column_at(&c, At::Start), Some((&a, "c")));
    assert_eq!(names.column_at(&c, At::End), None);
    assert_eq!(names.table_at(&a, At::End), Some(&t("a")));
}

#[test]
fn a_final_detachs_column_identity_does_not_leak_into_an_earlier_ownership_scope() {
    let (owner, fresh) = (table("leaf"), column("fresh"));
    let mut shape = Table::default();
    shape
        .columns
        .insert("c".into(), Column::new("int".parse().unwrap()));
    let detach = || {
        p(Change::DetachPartition {
            uid: owner.clone(),
            table: t("leaf"),
            parent: t("parent"),
            names: vec![],
            shape: Box::new(shape.clone()),
        })
    };
    let attach = || {
        p(Change::AttachPartition {
            uid: owner.clone(),
            table: t("leaf"),
            parent: t("parent"),
            bound: crate::PartitionBound::Default,
            shape: Box::new(shape.clone()),
        })
    };
    let after = ids(&[(&owner, t("leaf"))], &[(&fresh, t("leaf").column("c"))]);
    for changes in [vec![attach(), detach()], vec![detach(), attach(), detach()]] {
        let names = PlanNames::of(&changes, &after).unwrap();
        assert_eq!(names.column_at(&fresh, At::Start), None);
        for at in 0..changes.len() - 1 {
            assert_eq!(names.column_at(&fresh, At::After(at)), None);
        }
        assert_eq!(names.column_at(&fresh, At::End), Some((&owner, "c")));
        assert_eq!(names.stored_column(&t("leaf").column("c"), At::End), None);
    }
}

#[test]
fn a_first_column_address_does_not_extend_its_managed_lifetime() {
    let (owner, fresh) = (table("leaf"), column("fresh"));
    let mut shape = Table::default();
    shape
        .columns
        .insert("v".into(), Column::new("int".parse().unwrap()));
    let changes = vec![
        p(Change::DetachPartition {
            uid: owner.clone(),
            table: t("leaf"),
            parent: t("parent"),
            names: vec![],
            shape: Box::new(shape),
        }),
        p(Change::RenameColumn {
            uid: fresh.clone(),
            table: t("leaf"),
            from: "v".into(),
            to: "amount".into(),
            table_was: None,
        }),
    ];
    let after = ids(
        &[(&owner, t("leaf"))],
        &[(&fresh, t("leaf").column("amount"))],
    );
    let names = PlanNames::of(&changes, &after).unwrap();
    assert_eq!(names.column_first(&fresh), Some((&owner, "v")));
    assert_eq!(names.column_first(&column("unknown")), None);
    assert_eq!(names.column_at(&fresh, At::Start), None);
    assert_eq!(names.column_at(&fresh, At::End), Some((&owner, "amount")));
    assert_eq!(
        names.stored_column(&t("leaf").column("amount"), At::End),
        None
    );
}
