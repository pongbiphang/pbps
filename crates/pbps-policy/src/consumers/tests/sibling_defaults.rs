use super::*;

fn fixture() -> (Schema, IdsFile) {
    let table = Table {
        columns: [
            ("a".into(), Column::new(ColumnType::simple("integer"))),
            ("b".into(), {
                let mut b = Column::new(ColumnType::simple("integer")).not_null();
                b.value_source = ValueSource::Default("7".into());
                b
            }),
        ]
        .into(),
        ..Default::default()
    };
    let other: TableName = "app.other".parse().unwrap();
    (
        Schema {
            tables: [(name(), table.clone()), (other.clone(), table)].into(),
            ..Default::default()
        },
        IdsFile {
            tables: [(uid("t_aaaaaa"), name()), (uid("t_bbbbbb"), other.clone())].into(),
            columns: [
                (uid("c_aaaaaa"), name().column("a")),
                (uid("c_bbbbbb"), name().column("b")),
                (uid("c_cccccc"), other.column("a")),
                (uid("c_dddddd"), other.column("b")),
            ]
            .into(),
            ..Default::default()
        },
    )
}

#[test]
fn column_writers_keep_required_sibling_defaults_without_broadening_other_edits() {
    // Literal counts: the new protection applies only when the sibling needs
    // its last source. Direct column/table matches retain their old behavior.
    for (label, nullable, to, owner, entry, writes, fallback, count) in [
        (
            "required sibling",
            false,
            None,
            "app.t",
            "app.t.a",
            true,
            None,
            1,
        ),
        ("table writer", false, None, "app.t", "app.t", true, None, 1),
        (
            "nullable sibling",
            true,
            None,
            "app.t",
            "app.t.a",
            true,
            None,
            0,
        ),
        (
            "replacement",
            false,
            Some("8"),
            "app.t",
            "app.t.a",
            true,
            None,
            0,
        ),
        (
            "inherited source",
            false,
            None,
            "app.t",
            "app.t.a",
            true,
            Some("9"),
            0,
        ),
        (
            "unrelated table",
            false,
            None,
            "app.other",
            "app.t.a",
            true,
            None,
            0,
        ),
        ("reader", false, None, "app.t", "app.t.a", false, None, 0),
        (
            "direct nullable column",
            true,
            None,
            "app.t",
            "app.t.b",
            true,
            None,
            1,
        ),
        (
            "nullable table writer",
            true,
            None,
            "app.t",
            "app.t",
            true,
            None,
            1,
        ),
    ] {
        let (mut before, ids) = fixture();
        let owner: TableName = owner.parse().unwrap();
        let b = before
            .tables
            .get_mut(&owner)
            .unwrap()
            .columns
            .get_mut("b")
            .unwrap();
        b.nullable = nullable;
        if let Some(default) = fallback {
            b.ty = "app.d".parse().unwrap();
            before
                .types
                .insert("app.d".parse().unwrap(), domain(false, Some(default), None));
        }
        let mut after = before.clone();
        after
            .tables
            .get_mut(&owner)
            .unwrap()
            .columns
            .get_mut("b")
            .unwrap()
            .value_source = to.map_or(ValueSource::None, |v| ValueSource::Default(v.into()));
        let changed = owner.column("b");
        let found = findings(
            &before,
            &after,
            &ids,
            &ids,
            &consumers(entry, writes),
            vec![Change::AlterColumnDefault {
                uid: ids.column_uid(&changed).unwrap().clone(),
                column: changed,
                from: Some("7".into()),
                to: to.map(str::to_owned),
            }],
        );
        assert_eq!(found.len(), count, "{label}: {found:?}");
        for (_, f) in found {
            assert_eq!(f.id, "consumer.unproven");
            assert_eq!(f.severity, Severity::Error);
            assert!(
                f.message
                    .contains(&format!("consumers.billing-v41.writes: {entry}"))
            );
            assert!(f.message.contains("app.t.b"));
        }
    }
}

#[test]
fn partition_defaults_distinguish_direct_contracts_from_siblings() {
    for (label, local, nullable, parent_default, entry, count) in [
        (
            "nullable local sibling of leaf writer",
            true,
            true,
            None,
            "app.leaf.a",
            0,
        ),
        (
            "parent loss affects leaf writer",
            false,
            false,
            Some("7"),
            "app.leaf.a",
            1,
        ),
        (
            "nullable parent sibling",
            false,
            true,
            Some("7"),
            "app.leaf.a",
            0,
        ),
        (
            "local loss affects parent writer",
            true,
            false,
            None,
            "app.t.a",
            1,
        ),
        (
            "local loss affects leaf writer",
            true,
            false,
            None,
            "app.leaf.a",
            1,
        ),
        (
            "parent writer keeps inherited source",
            true,
            false,
            Some("7"),
            "app.t.a",
            0,
        ),
        (
            "leaf writer keeps inherited source",
            true,
            false,
            Some("7"),
            "app.leaf.a",
            0,
        ),
        (
            "nullable local sibling of parent writer",
            true,
            true,
            None,
            "app.t.a",
            0,
        ),
        ("direct nullable column", true, true, None, "app.leaf.b", 1),
        (
            "table writer covers nullable column",
            true,
            true,
            None,
            "app.leaf",
            1,
        ),
        (
            "direct changed fallback",
            true,
            false,
            Some("7"),
            "app.leaf.b",
            1,
        ),
        (
            "table writer covers changed fallback",
            true,
            false,
            Some("7"),
            "app.leaf",
            1,
        ),
        ("unrelated writer", true, false, None, "app.other.a", 0),
    ] {
        let (mut before, mut ids) = fixture();
        let b = before
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .get_mut("b")
            .unwrap();
        b.nullable = nullable;
        b.value_source =
            parent_default.map_or(ValueSource::None, |v| ValueSource::Default(v.into()));
        before.tables.get_mut(&name()).unwrap().partition_by = Some(pbps_model::PartitionBy {
            strategy: Default::default(),
            columns: vec!["a".into()],
            external: false,
        });
        let leaf: TableName = "app.leaf".parse().unwrap();
        let mut of = pbps_model::PartitionOf {
            parent: name(),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        };
        if local {
            of.columns.entry("b".into()).or_default().default = Some("8".into());
        }
        before.tables.insert(
            leaf.clone(),
            Table {
                partition_of: Some(of),
                ..Default::default()
            },
        );
        ids.tables.insert(uid("t_cccccc"), leaf.clone());
        let mut after = before.clone();
        let change = if local {
            after
                .tables
                .get_mut(&leaf)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .clear();
            Change::SetPartitionDefault {
                uid: uid("t_cccccc"),
                table: leaf,
                parent: name(),
                column: "b".into(),
                from: Some("8".into()),
                to: None,
                fallback: parent_default.map(str::to_owned),
            }
        } else {
            after
                .tables
                .get_mut(&name())
                .unwrap()
                .columns
                .get_mut("b")
                .unwrap()
                .value_source = ValueSource::None;
            Change::AlterColumnDefault {
                uid: uid("c_bbbbbb"),
                column: name().column("b"),
                from: Some("7".into()),
                to: None,
            }
        };
        let found = findings(
            &before,
            &after,
            &ids,
            &ids,
            &consumers(entry, true),
            vec![change],
        );
        assert_eq!(found.len(), count, "{label}: {found:?}");
        assert!(found.iter().all(|(_, f)| f.id == "consumer.unproven"));
    }
}

#[test]
fn domain_default_losses_share_the_sibling_rule_in_alter_and_rebuild_paths() {
    for rebuild in [false, true] {
        for (label, not_null, explicit, array, entry, count) in [
            ("required sibling", true, None, false, "app.t.a", 1),
            ("nullable sibling", false, None, false, "app.t.a", 0),
            ("column override", true, Some("9"), false, "app.t.a", 0),
            ("element DEFAULT", true, None, true, "app.t.a", 0),
            ("unrelated table", true, None, false, "app.other.a", 0),
        ] {
            let (mut before, mut ids) = fixture();
            let from = domain(not_null, Some("7"), None);
            let mut to = domain(not_null, None, None);
            if rebuild && let pbps_model::ManagedType::Domain { base, .. } = &mut to {
                *base = ColumnType::simple("bigint");
            }
            let ty = "app.d".parse().unwrap();
            before.types.insert(ty, from.clone());
            let mut outer = domain(false, None, None);
            if let pbps_model::ManagedType::Domain { base, .. } = &mut outer {
                *base = "app.d".parse().unwrap();
            }
            before.types.insert("app.outer".parse().unwrap(), outer);
            let b = before
                .tables
                .get_mut(&name())
                .unwrap()
                .columns
                .get_mut("b")
                .unwrap();
            b.ty = if array { "app.outer[]" } else { "app.outer" }
                .parse()
                .unwrap();
            b.nullable = true;
            b.value_source = explicit.map_or(ValueSource::None, |v| ValueSource::Default(v.into()));
            ids.types.insert(uid("y_aaaaaa"), "app.d".parse().unwrap());
            ids.types
                .insert(uid("y_bbbbbb"), "app.outer".parse().unwrap());
            let mut after = before.clone();
            after.types.insert("app.d".parse().unwrap(), to.clone());
            let change = if rebuild {
                Change::EnumRebuild {
                    uid: uid("y_aaaaaa"),
                    name: "app.d".parse().unwrap(),
                    tmp: "app.tmp".parse().unwrap(),
                    step: pbps_model::EnumRebuildStep::Create {
                        definition: Box::new(to),
                        removed: vec![],
                        ordinal: 0,
                        over: None,
                        why: Some("domain base type changes".into()),
                    },
                }
            } else {
                Change::AlterDomain {
                    uid: uid("y_aaaaaa"),
                    name: "app.d".parse().unwrap(),
                    from: Box::new(from),
                    to: Box::new(to),
                    half: pbps_model::DomainHalf::Loosen,
                }
            };
            let found = findings(
                &before,
                &after,
                &ids,
                &ids,
                &consumers(entry, true),
                vec![change],
            );
            assert_eq!(found.len(), count, "{label}, rebuild={rebuild}: {found:?}");
            assert!(found.iter().all(|(_, f)| f.id == "consumer.unproven"));
        }
    }
}
