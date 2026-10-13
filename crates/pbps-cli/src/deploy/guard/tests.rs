use super::super::{Settled, refuse_unplanned_movement};
use pbps_model::{
    Change, ChangeSet, CheckConstraint, Column, PlannedChange, ReplicaIdentity, Schema, Table,
    TableName,
};

fn table() -> Table {
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    t
}

fn schema(t: Table) -> Schema {
    Schema {
        tables: [(TableName::new("app", "t"), t)].into(),
        ..Default::default()
    }
}

fn plan(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}

fn partition(default: Option<&str>, not_null: bool) -> Table {
    Table {
        partition_of: Some(pbps_model::PartitionOf {
            parent: TableName::new("app", "events"),
            bound: pbps_model::PartitionBound::Range {
                from: vec![pbps_model::BoundDatum::Value("2026-01-01".into())],
                to: vec![pbps_model::BoundDatum::Value("2026-02-01".into())],
            },
            columns: [(
                "v".into(),
                pbps_model::PartitionColumn {
                    default: default.map(str::to_owned),
                    not_null,
                },
            )]
            .into(),
        }),
        ..Table::default()
    }
}

#[test]
fn a_partition_keeps_untouched_default_text_at_every_read() {
    let name = TableName::new("app", "t");
    let changes = plan(vec![Change::SetPartitionNotNull {
        uid: "t_000001".parse().unwrap(),
        table: name,
        column: "v".into(),
        not_null: true,
    }]);
    let before = schema(partition(Some("'kept'::text"), false));
    let kept = schema(partition(Some("'kept'::text"), true));
    let rewritten = schema(partition(Some("'changed'::text"), true));
    for read in [Settled::SoFar, Settled::Whole, Settled::Closing] {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            &kept,
            "test",
            read,
        )
        .expect("the plan changes only NOT NULL");
        let error = refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            &rewritten,
            "test",
            read,
        )
        .expect_err("an unchanged default is held to its recorded text");
        assert!(
            format!("{error:#}").contains("own column defaults or NOT NULLs"),
            "{error:#}"
        );
    }
}

#[test]
fn a_created_partition_keeps_its_place_from_the_first_read() {
    let name = TableName::new("app", "t");
    let mut declared = partition(None, false);
    declared.partition_of.as_mut().unwrap().columns.clear();
    let changes = plan(vec![Change::CreateTable {
        uid: "t_000001".parse().unwrap(),
        name,
        table: Box::new(declared.clone()),
        beside_default: None,
        moves_at_most: None,
    }]);
    let before = Schema::default();
    let correct = schema(declared.clone());
    for read in [Settled::SoFar, Settled::Whole, Settled::Closing] {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            &correct,
            "test",
            read,
        )
        .expect("the approved canonical placement");
        for field in 0..4 {
            let mut changed = declared.clone();
            match field {
                0 => changed.partition_of.as_mut().unwrap().parent = TableName::new("app", "other"),
                1 => {
                    changed.partition_of.as_mut().unwrap().bound =
                        pbps_model::PartitionBound::Range {
                            from: vec![pbps_model::BoundDatum::MinValue],
                            to: vec![pbps_model::BoundDatum::MaxValue],
                        }
                }
                2 => changed.partition_of = None,
                3 => {
                    changed.partition_by = Some(pbps_model::PartitionBy {
                        columns: vec!["v".into()],
                        external: false,
                    })
                }
                _ => unreachable!(),
            }
            let error = refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &schema(changed),
                "test",
                read,
            )
            .expect_err("a different place cannot become the first checkpoint");
            assert!(
                format!("{error:#}").contains("partition parent, bound or key"),
                "field {field}: {error:#}"
            );
        }
    }
}

#[test]
fn a_partition_default_follows_the_last_parent_or_own_setting() {
    let name = TableName::new("app", "t");
    let parent = TableName::new("app", "events");
    let reading = |parent_default: &str, own_default: Option<&str>| {
        let mut schema = schema(partition(own_default, false));
        let mut parent_table = table();
        let mut value = Column::new("text".parse().unwrap());
        value.value_source = pbps_model::ValueSource::Default(parent_default.into());
        parent_table.columns.insert("v".into(), value);
        parent_table.partition_by = Some(pbps_model::PartitionBy {
            external: false,
            columns: vec!["id".into()],
        });
        schema.tables.insert(parent.clone(), parent_table);
        schema
    };
    let own = Change::SetPartitionDefault {
        uid: "t_000001".parse().unwrap(),
        table: name.clone(),
        parent: parent.clone(),
        column: "v".into(),
        from: Some("'kept'".into()),
        to: Some("'changed'".into()),
        fallback: None,
    };
    let inherited = Change::AlterColumnDefault {
        uid: "c_000001".parse().unwrap(),
        column: pbps_model::ColumnRef::new(parent.clone(), "v"),
        from: Some("'parent'".into()),
        to: Some("'new parent'".into()),
    };
    let before = reading("'parent'::text", Some("'kept'::text"));
    for own_is_last in [false, true] {
        let changes = plan(if own_is_last {
            vec![inherited.clone(), own.clone()]
        } else {
            vec![own.clone(), inherited.clone()]
        });
        let right = reading(
            "'new parent'::text",
            own_is_last.then_some("'changed'::text"),
        );
        let wrong = reading(
            "'new parent'::text",
            (!own_is_last).then_some("'changed'::text"),
        );
        for read in [Settled::Whole, Settled::Closing] {
            refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &right,
                "test",
                read,
            )
            .expect("the later statement determines whether the partition has its own default");
            let error = refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &wrong,
                "test",
                read,
            )
            .expect_err("the earlier statement cannot excuse the wrong final presence");
            assert!(
                format!("{error:#}").contains("own column defaults or NOT NULLs"),
                "{error:#}"
            );
        }
        for intermediate in [&before, &right, &wrong] {
            refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                intermediate,
                "test",
                Settled::SoFar,
            )
            .expect("planned default presence is unpromised between statements");
        }
    }
}

#[test]
fn a_parent_drop_keeps_the_other_own_entries_across_a_table_rename() {
    let child = TableName::new("app", "t");
    let parent = TableName::new("app", "events");
    let mut before = schema(partition(Some("'kept'::text"), false));
    before
        .tables
        .get_mut(&child)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .insert(
            "w".into(),
            pbps_model::PartitionColumn {
                default: None,
                not_null: true,
            },
        );
    let mut parent_table = Table::default();
    for (name, ty) in [("ts", "date"), ("v", "text"), ("w", "text")] {
        parent_table
            .columns
            .insert(name.into(), Column::new(ty.parse().unwrap()));
    }
    parent_table.partition_by = Some(pbps_model::PartitionBy {
        external: false,
        columns: vec!["ts".into()],
    });
    before.tables.insert(parent.clone(), parent_table);
    for renamed in [false, true] {
        let target = if renamed {
            TableName::new("app", "history")
        } else {
            parent.clone()
        };
        let mut changes = Vec::new();
        if renamed {
            changes.push(Change::RenameTable {
                uid: "t_000010".parse().unwrap(),
                from: parent.clone(),
                to: target.clone(),
                defaults: Vec::new(),
            });
        }
        changes.push(Change::DropColumn {
            uid: "c_000001".parse().unwrap(),
            column: pbps_model::ColumnRef::new(target.clone(), "v"),
        });
        let changes = plan(changes);
        let mut after = before.clone();
        let mut parent_table = after.tables.remove(&parent).unwrap();
        parent_table.columns.shift_remove("v");
        after.tables.insert(target.clone(), parent_table);
        let of = after
            .tables
            .get_mut(&child)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap();
        of.parent = target;
        of.columns.remove("v");
        for read in [Settled::Whole, Settled::Closing] {
            refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &after,
                "test",
                read,
            )
            .expect("the drop removes only its own entry, under either parent name");
            for kept_dropped in [false, true] {
                let mut wrong = after.clone();
                let columns = &mut wrong
                    .tables
                    .get_mut(&child)
                    .unwrap()
                    .partition_of
                    .as_mut()
                    .unwrap()
                    .columns;
                if kept_dropped {
                    columns.insert(
                        "v".into(),
                        pbps_model::PartitionColumn {
                            default: Some("'kept'::text".into()),
                            not_null: false,
                        },
                    );
                } else {
                    columns.clear();
                }
                let error = refuse_unplanned_movement(
                    &pbps_pg::Postgres::new(),
                    &changes,
                    &before,
                    &wrong,
                    "test",
                    read,
                )
                .expect_err("the removed and untouched own entries answer independently");
                assert!(
                    format!("{error:#}").contains("own column defaults or NOT NULLs"),
                    "{error:#}"
                );
            }
        }
    }
}

#[test]
fn touching_a_table_keeps_each_scalar_at_every_read() {
    let name = TableName::new("app", "t");
    let check = CheckConstraint {
        expression: "id > 0".into(),
    };
    let changes = plan(vec![Change::AddCheck {
        table: name.clone(),
        name: "ck".into(),
        constraint: check.clone(),
    }]);
    let mut after = table();
    after.checks.insert("ck".into(), check);
    for field in 0..5 {
        let mut before = table();
        let mut held = after.clone();
        // These period columns already exist, so adding period metadata is
        // the only observed movement in the temporal case.
        if field == 3 {
            for t in [&mut before, &mut held] {
                for column in ["vf", "vt"] {
                    t.columns.insert(
                        column.into(),
                        Column::new("datetime2".parse().unwrap()).not_null(),
                    );
                }
            }
        }
        let mut changed = held.clone();
        let message = match field {
            0 => {
                changed.replica_identity = Some(ReplicaIdentity::Full);
                "replica identity changed"
            }
            1 => {
                changed
                    .storage_parameters
                    .insert("fillfactor".into(), "80".into());
                "storage parameters"
            }
            2 => {
                changed.unlogged = true;
                "is unlogged"
            }
            3 => {
                changed.system_time = Some(pbps_model::SystemTime {
                    start: "vf".into(),
                    end: "vt".into(),
                    hidden: false,
                    versioning: None,
                });
                "period or system versioning"
            }
            4 => {
                changed.partition_by = Some(pbps_model::PartitionBy {
                    columns: vec!["id".into()],
                    external: false,
                });
                "partition parent, bound or key"
            }
            _ => unreachable!(),
        };
        let pg = pbps_pg::Postgres::new();
        let dialect: &dyn pbps_dialect::Dialect = if field == 3 { &pbps_mssql::Mssql } else { &pg };
        for read in [Settled::SoFar, Settled::Whole, Settled::Closing] {
            refuse_unplanned_movement(
                dialect,
                &changes,
                &schema(before.clone()),
                &schema(held.clone()),
                "test",
                read,
            )
            .expect("the plan leaves every scalar alone");
            let error = refuse_unplanned_movement(
                dialect,
                &changes,
                &schema(before.clone()),
                &schema(changed.clone()),
                "test",
                read,
            )
            .expect_err(message);
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
    }
}

#[test]
fn planned_table_scalars_keep_their_existing_checkpoint_policies() {
    let name = TableName::new("app", "t");
    let uid = "t_000000".parse::<pbps_model::Uid>().unwrap();
    let settings = [
        Change::SetReplicaIdentity {
            uid: uid.clone(),
            table: name.clone(),
            to: Some(ReplicaIdentity::Full),
        },
        Change::SetStorageParameters {
            uid: uid.clone(),
            table: name.clone(),
            set: [("fillfactor".into(), "80".into())].into(),
            reset: Default::default(),
        },
        Change::SetTablePersistence {
            uid,
            table: name,
            unlogged: true,
        },
    ];
    for (field, change) in settings.into_iter().enumerate() {
        let mut planned = table();
        let mut third = table();
        match field {
            0 => {
                planned.replica_identity = Some(ReplicaIdentity::Full);
                third.replica_identity = Some(ReplicaIdentity::Nothing);
            }
            1 => {
                planned
                    .storage_parameters
                    .insert("fillfactor".into(), "80".into());
                third
                    .storage_parameters
                    .insert("fillfactor".into(), "90".into());
            }
            2 => planned.unlogged = true,
            _ => unreachable!(),
        }
        let changes = plan(vec![change]);
        for read in [Settled::SoFar, Settled::Whole, Settled::Closing] {
            for (observed, allowed_mid, allowed_end) in [
                (table(), true, false),
                (planned.clone(), true, true),
                (third.clone(), field != 1, false),
            ] {
                let result = refuse_unplanned_movement(
                    &pbps_pg::Postgres::new(),
                    &changes,
                    &schema(table()),
                    &schema(observed),
                    "test",
                    read,
                );
                let allowed = if read == Settled::SoFar {
                    allowed_mid
                } else {
                    allowed_end
                };
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "field {field}, whole {}: {result:?}",
                    read.whole()
                );
            }
        }
    }
}

#[test]
fn a_planned_default_replica_identity_is_not_an_absent_setting() {
    let changes = plan(vec![Change::SetReplicaIdentity {
        uid: "t_000000".parse().unwrap(),
        table: TableName::new("app", "t"),
        to: None,
    }]);
    let mut before = table();
    before.replica_identity = Some(ReplicaIdentity::Full);
    for read in [Settled::SoFar, Settled::Whole, Settled::Closing] {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &schema(before.clone()),
            &schema(table()),
            "test",
            read,
        )
        .expect("the declared DEFAULT identity");
        let result = refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &schema(before.clone()),
            &schema(before.clone()),
            "test",
            read,
        );
        assert_eq!(result.is_ok(), read == Settled::SoFar, "{result:?}");
    }
}

#[test]
fn scalar_settings_follow_their_uid_when_a_table_name_changes_hands() {
    let a = TableName::new("app", "a");
    let b = TableName::new("app", "b");
    let c = TableName::new("app", "c");
    let ua = "t_000000".parse::<pbps_model::Uid>().unwrap();
    let ub = "t_000001".parse::<pbps_model::Uid>().unwrap();
    let changes = plan(vec![
        Change::SetReplicaIdentity {
            uid: ua.clone(),
            table: a.clone(),
            to: Some(ReplicaIdentity::Full),
        },
        Change::SetReplicaIdentity {
            uid: ub.clone(),
            table: b.clone(),
            to: Some(ReplicaIdentity::Nothing),
        },
        Change::RenameTable {
            uid: ub.clone(),
            from: b.clone(),
            to: c.clone(),
            defaults: Vec::new(),
        },
        Change::RenameTable {
            uid: ua.clone(),
            from: a.clone(),
            to: b.clone(),
            defaults: Vec::new(),
        },
        Change::SetStorageParameters {
            uid: ua.clone(),
            table: b.clone(),
            set: [("fillfactor".into(), "80".into())].into(),
            reset: Default::default(),
        },
        Change::SetStorageParameters {
            uid: ub.clone(),
            table: c.clone(),
            set: [("fillfactor".into(), "90".into())].into(),
            reset: Default::default(),
        },
        Change::SetTablePersistence {
            uid: ua,
            table: b.clone(),
            unlogged: true,
        },
        Change::SetTablePersistence {
            uid: ub,
            table: c.clone(),
            unlogged: false,
        },
    ]);
    let before = Schema {
        tables: [(a, table()), (b.clone(), table())].into(),
        ..Default::default()
    };
    let mut ta = table();
    ta.replica_identity = Some(ReplicaIdentity::Full);
    ta.storage_parameters
        .insert("fillfactor".into(), "80".into());
    ta.unlogged = true;
    let mut tb = table();
    tb.replica_identity = Some(ReplicaIdentity::Nothing);
    tb.storage_parameters
        .insert("fillfactor".into(), "90".into());
    let after = Schema {
        tables: [(b.clone(), ta), (c.clone(), tb)].into(),
        ..Default::default()
    };
    for read in [Settled::Whole, Settled::Closing] {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            &after,
            "test",
            read,
        )
        .expect("each holder receives its own settings");
        for field in 0..3 {
            let mut wrong = after.clone();
            let other = wrong.tables[&c].clone();
            let t = wrong.tables.get_mut(&b).unwrap();
            let message = match field {
                0 => {
                    t.replica_identity = other.replica_identity;
                    "replica identity"
                }
                1 => {
                    t.storage_parameters = other.storage_parameters;
                    "storage parameters"
                }
                2 => {
                    t.unlogged = other.unlogged;
                    "is logged"
                }
                _ => unreachable!(),
            };
            let error = refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &wrong,
                "test",
                read,
            )
            .expect_err("another holder's setting");
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
    }
}

#[test]
fn column_requirements_follow_one_uid_through_intermediate_spellings() {
    use pbps_model::{IdsFile, ValueSource};
    let name = TableName::new("app", "t");
    let uid: pbps_model::Uid = "c_000001".parse().unwrap();
    let rename = |from: &str, to: &str| Change::RenameColumn {
        uid: uid.clone(),
        table: name.clone(),
        from: from.into(),
        to: to.into(),
        table_was: None,
    };
    let default = |column: &str, from: Option<&str>, to: Option<&str>| Change::AlterColumnDefault {
        uid: uid.clone(),
        column: name.column(column),
        from: from.map(str::to_owned),
        to: to.map(str::to_owned),
    };
    let changes = plan(vec![
        default("id", Some("1"), None),
        rename("id", "temporary"),
        rename("temporary", "final"),
        default("final", None, Some("2")),
    ]);
    let mut ids = IdsFile::default();
    ids.tables.insert("t_000001".parse().unwrap(), name.clone());
    ids.columns.insert(uid, name.column("final"));
    let mut before = schema(table());
    before
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .get_mut("id")
        .unwrap()
        .value_source = ValueSource::Default("(1)".into());
    let mut after = before.clone();
    let t = after.tables.get_mut(&name).unwrap();
    let mut column = t.columns.shift_remove("id").unwrap();
    column.value_source = ValueSource::Default("(2)".into());
    t.columns.insert("final".into(), column);
    let check = |after: &Schema| {
        super::super::refuse_pinned_movement(
            &pbps_pg::Postgres::new(),
            super::Plan {
                changes: &changes,
                ids: &ids,
            },
            &before,
            after,
            "test",
            Settled::Closing,
        )
    };
    check(&after).expect("the last default and final spelling belong to the same UID");
    for impostor in ["id", "temporary"] {
        let mut wrong = after.clone();
        wrong
            .tables
            .get_mut(&name)
            .unwrap()
            .columns
            .insert(impostor.into(), Column::new("integer".parse().unwrap()));
        assert!(check(&wrong).is_err(), "an impostor occupies {impostor}");
    }
    let mut wrong = after.clone();
    wrong
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .get_mut("final")
        .unwrap()
        .nullable = false;
    let error = check(&wrong).expect_err("a planned default does not own nullability");
    assert!(format!("{error:#}").contains("nullability"));
    wrong
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .get_mut("final")
        .unwrap()
        .nullable = true;
    wrong
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .get_mut("final")
        .unwrap()
        .value_source = ValueSource::None;
    let error = check(&wrong).expect_err("the final default must be present");
    assert!(format!("{error:#}").contains("does not have the default"));
}

#[test]
fn a_column_drop_before_its_parent_rename_reaches_only_its_own_partition_entry() {
    let parent = TableName::new("app", "events");
    let renamed = TableName::new("app", "history");
    let child = TableName::new("app", "t");
    let mut before = schema(partition(Some("'kept'::text"), false));
    let mut parent_table = table();
    parent_table
        .columns
        .insert("v".into(), Column::new("text".parse().unwrap()));
    before.tables.insert(parent.clone(), parent_table);
    let changes = plan(vec![
        Change::DropColumn {
            uid: "c_000001".parse().unwrap(),
            column: parent.column("v"),
        },
        Change::RenameTable {
            uid: "t_000001".parse().unwrap(),
            from: parent.clone(),
            to: renamed.clone(),
            defaults: Vec::new(),
        },
    ]);
    let ids = super::fixture_ids(&changes, &before);
    let mut after = before.clone();
    let mut parent_table = after.tables.remove(&parent).unwrap();
    parent_table.columns.shift_remove("v");
    after.tables.insert(renamed.clone(), parent_table);
    let of = after
        .tables
        .get_mut(&child)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap();
    of.parent = renamed;
    of.columns.remove("v");
    let check = |after: &Schema| {
        super::super::refuse_pinned_movement(
            &pbps_pg::Postgres::new(),
            super::Plan {
                changes: &changes,
                ids: &ids,
            },
            &before,
            after,
            "test",
            Settled::Closing,
        )
    };
    check(&after).expect("the parent's UID carries the drop through its later rename");
    let mut wrong = after.clone();
    wrong
        .tables
        .get_mut(&child)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .insert(
            "v".into(),
            pbps_model::PartitionColumn {
                default: Some("'kept'::text".into()),
                not_null: false,
            },
        );
    let error = check(&wrong).expect_err("the dropped column's local override cannot remain");
    assert!(format!("{error:#}").contains("own column defaults or NOT NULLs"));
}

#[test]
fn a_rename_keeps_every_observed_column_field_across_the_move_and_at_close() {
    use pbps_model::{Generated, Identity, OwnedSequence, ValueSource};
    let base = Column::new("integer".parse().unwrap());
    let mut cases = Vec::new();
    for field in [
        "type",
        "nullability",
        "collation",
        "deprecation",
        "description",
    ] {
        let mut changed = base.clone();
        match field {
            "type" => changed.ty = "bigint".parse().unwrap(),
            "nullability" => changed.nullable = !changed.nullable,
            "collation" => {
                changed.collation = Some(pbps_model::Collation::new("Latin1_General_100_CI_AS"))
            }
            "deprecation" => changed.deprecated = Some("another declaration".into()),
            "description" => changed.description = Some("another writer".into()),
            _ => unreachable!(),
        }
        cases.push((field, base.clone(), changed));
    }
    let generated = |text: &str, stored| {
        ValueSource::Generated(Generated {
            expression: text.into(),
            stored,
        })
    };
    for (field, before, after) in [
        (
            "default",
            ValueSource::Default("(1)".into()),
            ValueSource::Default("(2)".into()),
        ),
        (
            "generation expression",
            generated("id + 1", true),
            generated("id + 2", true),
        ),
        (
            "generation",
            generated("id + 1", true),
            generated("id + 1", false),
        ),
        (
            "identity",
            ValueSource::Identity(Identity {
                seed: 1,
                increment: 1,
            }),
            ValueSource::Identity(Identity {
                seed: 2,
                increment: 1,
            }),
        ),
        (
            "sequence",
            ValueSource::Sequence(OwnedSequence {
                start: 1,
                increment: 1,
                name: Some("kept_seq".into()),
            }),
            ValueSource::Sequence(OwnedSequence {
                start: 1,
                increment: 2,
                name: Some("kept_seq".into()),
            }),
        ),
    ] {
        let (mut was, mut now) = (base.clone(), base.clone());
        was.value_source = before;
        now.value_source = after;
        cases.push((field, was, now));
    }
    let name = TableName::new("app", "t");
    let changes = plan(vec![Change::RenameColumn {
        uid: "c_000001".parse().unwrap(),
        table: name.clone(),
        from: "id".into(),
        to: "renamed".into(),
        table_was: None,
    }]);
    let mut ids = pbps_model::IdsFile::default();
    ids.tables.insert("t_000001".parse().unwrap(), name.clone());
    ids.columns
        .insert("c_000001".parse().unwrap(), name.column("renamed"));
    let observed = |name: &str, column: Column| {
        schema(Table {
            columns: [(name.into(), column)].into(),
            ..Default::default()
        })
    };
    for (field, was, now) in cases {
        for (before_name, settled) in [("id", Settled::Whole), ("renamed", Settled::Closing)] {
            let before = observed(before_name, was.clone());
            let check = |column| {
                super::super::refuse_pinned_movement(
                    &pbps_pg::Postgres::new(),
                    super::Plan {
                        changes: &changes,
                        ids: &ids,
                    },
                    &before,
                    &observed("renamed", column),
                    "test",
                    settled,
                )
            };
            check(was.clone()).expect("a rename preserves the entire held column");
            let error = check(now.clone()).expect_err(field);
            assert!(
                format!("{error:#}").contains(&format!("different {field}")),
                "{field}: {error:#}"
            );
        }
    }
}

#[test]
fn column_diagnostics_keep_the_observation_order() {
    let name = TableName::new("app", "t");
    let before = schema(Table {
        columns: ["z", "a"]
            .into_iter()
            .map(|name| (name.into(), Column::new("integer".parse().unwrap())))
            .collect(),
        ..Default::default()
    });
    let changes = plan(vec![Change::SetTablePersistence {
        uid: "t_000001".parse().unwrap(),
        table: name.clone(),
        unlogged: true,
    }]);
    let mut after = before.clone();
    let t = after.tables.get_mut(&name).unwrap();
    t.unlogged = true;
    for column in t.columns.values_mut() {
        column.ty = "bigint".parse().unwrap();
    }
    let error = refuse_unplanned_movement(
        &pbps_pg::Postgres::new(),
        &changes,
        &before,
        &after,
        "test",
        Settled::Closing,
    )
    .expect_err("both untouched types changed");
    let text = format!("{error:#}");
    let columns: Vec<_> = text
        .lines()
        .filter(|line| line.contains("different type"))
        .map(|line| line.split('`').nth(1).unwrap())
        .collect();
    assert_eq!(columns, ["z", "a", "z", "a"], "{text}");
    let mut kept = before.clone();
    kept.tables.get_mut(&name).unwrap().unlogged = true;
    refuse_unplanned_movement(
        &pbps_pg::Postgres::new(),
        &changes,
        &before,
        &kept,
        "test",
        Settled::Closing,
    )
    .expect("the plan changes only persistence");
}

#[test]
fn a_retired_columns_promise_cannot_answer_for_the_next_holder_of_its_name() {
    use pbps_model::ValueSource;
    let name = TableName::new("app", "t");
    let mut t = Table::default();
    for (column, value) in [("a", "(1)"), ("b", "(2)")] {
        let mut spec = Column::new("integer".parse().unwrap());
        spec.value_source = ValueSource::Default(value.into());
        t.columns.insert(column.into(), spec);
    }
    let before = schema(t);
    let changes = plan(vec![
        Change::AlterColumnDefault {
            uid: "c_000001".parse().unwrap(),
            column: name.column("a"),
            from: Some("1".into()),
            to: None,
        },
        Change::DropColumn {
            uid: "c_000001".parse().unwrap(),
            column: name.column("a"),
        },
        Change::RenameColumn {
            uid: "c_000002".parse().unwrap(),
            table: name.clone(),
            from: "b".into(),
            to: "a".into(),
            table_was: None,
        },
    ]);
    let mut ids = pbps_model::IdsFile::default();
    ids.tables.insert("t_000001".parse().unwrap(), name.clone());
    ids.columns
        .insert("c_000002".parse().unwrap(), name.column("a"));
    let mut after = before.clone();
    let t = after.tables.get_mut(&name).unwrap();
    t.columns.shift_remove("a");
    let survivor = t.columns.shift_remove("b").unwrap();
    t.columns.insert("a".into(), survivor);
    let check = |after: &Schema| {
        super::super::refuse_pinned_movement(
            &pbps_pg::Postgres::new(),
            super::Plan {
                changes: &changes,
                ids: &ids,
            },
            &before,
            after,
            "test",
            Settled::Closing,
        )
    };
    check(&after).expect("the dropped UID's absent default is no promise about its successor");
    after
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .get_mut("a")
        .unwrap()
        .value_source = ValueSource::None;
    let error = check(&after).expect_err("the surviving UID keeps its own default text");
    assert!(format!("{error:#}").contains("different default"));
}

fn parent_with_distinct_own_columns() -> Schema {
    use pbps_model::{PartitionColumn, ValueSource};
    let parent = TableName::new("app", "events");
    let mut p = table();
    p.partition_by = Some(pbps_model::PartitionBy {
        columns: vec!["id".into()],
        external: false,
    });
    for (name, value) in [("a", "1"), ("b", "2")] {
        let mut c = Column::new("integer".parse().unwrap());
        c.value_source = ValueSource::Default(value.into());
        p.columns.insert(name.into(), c);
    }
    let mut c = partition(None, false);
    let of = c.partition_of.as_mut().unwrap();
    of.bound = pbps_model::PartitionBound::Range {
        from: vec![pbps_model::BoundDatum::Value("0".into())],
        to: vec![pbps_model::BoundDatum::Value("10".into())],
    };
    of.columns = [
        (
            "a".into(),
            PartitionColumn {
                default: Some("4".into()),
                not_null: false,
            },
        ),
        (
            "b".into(),
            PartitionColumn {
                default: Some("3".into()),
                not_null: true,
            },
        ),
    ]
    .into();
    let mut before = schema(c);
    before.tables.insert(parent.clone(), p);
    before
}

#[test]
fn retired_parent_columns_remove_their_original_own_entries() {
    let parent = TableName::new("app", "events");
    let child = TableName::new("app", "t");
    let mut before = parent_with_distinct_own_columns();
    before
        .tables
        .get_mut(&child)
        .unwrap()
        .partition_of
        .as_mut()
        .unwrap()
        .columns
        .get_mut("a")
        .unwrap()
        .not_null = true;
    for (spellings, move_parent) in [
        (vec!["renamed"], false),
        (vec!["intermediate", "renamed"], false),
        (vec!["intermediate", "renamed"], true),
    ] {
        let mut from = "a";
        let mut changes = Vec::new();
        for to in spellings {
            changes.push(Change::RenameColumn {
                uid: "c_000001".parse().unwrap(),
                table_was: None,
                table: parent.clone(),
                from: from.into(),
                to: to.into(),
            });
            from = to;
        }
        let final_parent = if move_parent {
            let moved = TableName::new("app", "history");
            changes.push(Change::RenameTable {
                uid: "t_000002".parse().unwrap(),
                from: parent.clone(),
                to: moved.clone(),
                defaults: Vec::new(),
            });
            moved
        } else {
            parent.clone()
        };
        changes.push(Change::DropColumn {
            uid: "c_000001".parse().unwrap(),
            column: final_parent.column(from),
        });
        let changes = plan(changes);
        let mut after = before.clone();
        let mut parent_table = after.tables.remove(&parent).unwrap();
        parent_table.columns.shift_remove("a");
        after.tables.insert(final_parent.clone(), parent_table);
        let own = after
            .tables
            .get_mut(&child)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap();
        own.parent = final_parent;
        let retired = own.columns.remove("a").unwrap();
        let check = |now: &Schema| {
            refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                now,
                "test",
                Settled::Whole,
            )
        };
        check(&after).expect("dropping the renamed UID removes its original own entry");
        for spelling in ["a", from] {
            let mut wrong = after.clone();
            wrong
                .tables
                .get_mut(&child)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .insert(spelling.into(), retired.clone());
            let error =
                check(&wrong).expect_err("the retired UID leaves no own entry under either name");
            assert!(format!("{error:#}").contains("own column defaults or NOT NULLs"));
        }
        for field in ["default", "not_null"] {
            let mut wrong = after.clone();
            let kept = wrong
                .tables
                .get_mut(&child)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .get_mut("b")
                .unwrap();
            match field {
                "default" => kept.default = None,
                "not_null" => kept.not_null = false,
                _ => unreachable!(),
            }
            let error =
                check(&wrong).expect_err("retirement does not excuse another UID's own fields");
            assert!(format!("{error:#}").contains("own column defaults or NOT NULLs"));
        }
    }
}

#[test]
fn retired_parent_fields_do_not_change_the_next_columns_own_entries() {
    let parent = TableName::new("app", "events");
    let child = TableName::new("app", "t");
    let before = parent_with_distinct_own_columns();
    let fields = [
        Change::AlterColumnDefault {
            uid: "c_000001".parse().unwrap(),
            column: parent.column("a"),
            from: Some("1".into()),
            to: None,
        },
        Change::AlterColumnNullability {
            uid: "c_000001".parse().unwrap(),
            column: parent.column("a"),
            ty: "integer".parse().unwrap(),
            to_nullable: false,
            collation: None,
            through_external_partitions: false,
        },
    ];
    for field in fields {
        let changes = plan(vec![
            field,
            Change::DropColumn {
                uid: "c_000001".parse().unwrap(),
                column: parent.column("a"),
            },
            Change::RenameColumn {
                uid: "c_000002".parse().unwrap(),
                table_was: None,
                table: parent.clone(),
                from: "b".into(),
                to: "a".into(),
            },
        ]);
        let mut after = before.clone();
        let p = after.tables.get_mut(&parent).unwrap();
        p.columns.shift_remove("a");
        let b = p.columns.shift_remove("b").unwrap();
        p.columns.insert("a".into(), b);
        let own = &mut after
            .tables
            .get_mut(&child)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns;
        own.remove("a");
        let b = own.remove("b").unwrap();
        own.insert("a".into(), b);
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            &after,
            "test",
            Settled::Whole,
        )
        .expect("the retired column cannot erase the surviving partition override");
        for field in ["default", "not_null"] {
            let mut wrong = after.clone();
            let own = wrong
                .tables
                .get_mut(&child)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .get_mut("a")
                .unwrap();
            match field {
                "default" => own.default = None,
                "not_null" => own.not_null = false,
                _ => unreachable!(),
            }
            let error = refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &wrong,
                "test",
                Settled::Whole,
            )
            .expect_err("the retired column owns neither field of its replacement");
            assert!(
                format!("{error:#}").contains("own column defaults or NOT NULLs"),
                "{field}: {error:#}"
            );
        }
    }
}

#[test]
fn own_field_requirements_follow_the_column_through_a_name_swap() {
    use pbps_model::ValueSource;
    let parent = TableName::new("app", "events");
    let child = TableName::new("app", "t");
    let before = parent_with_distinct_own_columns();
    let fields = [
        (
            Change::AlterColumnDefault {
                uid: "c_000002".parse().unwrap(),
                column: parent.column("b"),
                from: Some("2".into()),
                to: None,
            },
            None,
            true,
            None,
            true,
        ),
        (
            Change::SetPartitionDefault {
                uid: "t_000001".parse().unwrap(),
                table: child.clone(),
                parent: parent.clone(),
                column: "b".into(),
                from: Some("3".into()),
                to: None,
                fallback: Some("2".into()),
            },
            Some("2"),
            true,
            None,
            true,
        ),
        (
            Change::AlterColumnNullability {
                uid: "c_000002".parse().unwrap(),
                column: parent.column("b"),
                ty: "integer".parse().unwrap(),
                to_nullable: false,
                collation: None,
                through_external_partitions: false,
            },
            Some("2"),
            false,
            Some("3"),
            false,
        ),
        (
            Change::SetPartitionNotNull {
                uid: "t_000001".parse().unwrap(),
                table: child.clone(),
                column: "b".into(),
                not_null: false,
            },
            Some("2"),
            true,
            Some("3"),
            false,
        ),
    ];
    let rename = |uid: &str, from: &str, to: &str| Change::RenameColumn {
        uid: uid.parse().unwrap(),
        table: parent.clone(),
        from: from.into(),
        to: to.into(),
        table_was: None,
    };
    for (field, parent_default, parent_nullable, own_default, own_not_null) in fields {
        let changes = plan(vec![
            field,
            rename("c_000001", "a", "temporary"),
            rename("c_000002", "b", "a"),
            rename("c_000001", "temporary", "b"),
        ]);
        let mut after = before.clone();
        let p = after.tables.get_mut(&parent).unwrap();
        let a = p.columns.shift_remove("a").unwrap();
        let mut b = p.columns.shift_remove("b").unwrap();
        b.value_source =
            parent_default.map_or(ValueSource::None, |v: &str| ValueSource::Default(v.into()));
        b.nullable = parent_nullable;
        p.columns.insert("a".into(), b);
        p.columns.insert("b".into(), a);
        let own = &mut after
            .tables
            .get_mut(&child)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns;
        let a = own.remove("a").unwrap();
        let mut b = own.remove("b").unwrap();
        b.default = own_default.map(str::to_owned);
        b.not_null = own_not_null;
        own.insert("a".into(), b);
        own.insert("b".into(), a);
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            &after,
            "test",
            Settled::Whole,
        )
        .expect("the field setting follows b's UID to a");
        for field in ["default", "not_null"] {
            let mut wrong = after.clone();
            let untouched = wrong
                .tables
                .get_mut(&child)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .get_mut("b")
                .unwrap();
            match field {
                "default" => untouched.default = None,
                "not_null" => untouched.not_null = true,
                _ => unreachable!(),
            }
            let error = refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &before,
                &wrong,
                "test",
                Settled::Whole,
            )
            .expect_err("the field setting cannot move a's UID, now called b");
            assert!(
                format!("{error:#}").contains("own column defaults or NOT NULLs"),
                "{field}: {error:#}"
            );
        }
    }
}

#[test]
fn a_same_named_new_column_does_not_inherit_the_retired_definition() {
    use pbps_model::{Generated, IdsFile, ValueSource};
    let name = TableName::new("app", "t");
    let mut replacement = Column::new("bigint".parse().unwrap());
    replacement.nullable = false;
    replacement.value_source = ValueSource::Default("1".into());
    for generated in [true, false] {
        for move_table in [false, true] {
            let mut old = Column::new("integer".parse().unwrap());
            if generated {
                old.value_source = ValueSource::Generated(Generated {
                    expression: "id + 1".into(),
                    stored: true,
                });
            }
            let mut t = table();
            t.columns.insert("value".into(), old.clone());
            let before = schema(t);
            let mut changes = vec![
                Change::DropColumn {
                    uid: "c_000001".parse().unwrap(),
                    column: name.column("value"),
                },
                Change::AddColumn {
                    uid: "c_000002".parse().unwrap(),
                    table: name.clone(),
                    name: "value".into(),
                    column: Box::new(replacement.clone()),
                },
            ];
            let final_name = if move_table {
                let to = TableName::new("app", "moved");
                changes.push(Change::RenameTable {
                    uid: "t_000001".parse().unwrap(),
                    from: name.clone(),
                    to: to.clone(),
                    defaults: Vec::new(),
                });
                to
            } else {
                name.clone()
            };
            let changes = plan(changes);
            let mut ids = IdsFile::default();
            ids.tables
                .insert("t_000001".parse().unwrap(), final_name.clone());
            ids.columns
                .insert("c_000002".parse().unwrap(), final_name.column("value"));
            ids.columns
                .insert("c_000003".parse().unwrap(), final_name.column("id"));
            let mut after = Schema::default();
            let mut t = table();
            t.columns.insert("value".into(), replacement.clone());
            after.tables.insert(final_name.clone(), t);
            let check = |after: &Schema| {
                super::super::refuse_pinned_movement(
                    &pbps_pg::Postgres::new(),
                    super::Plan {
                        changes: &changes,
                        ids: &ids,
                    },
                    &before,
                    after,
                    "test",
                    Settled::Whole,
                )
            };
            check(&after).expect("the new UID answers to its declaration, not the retired column");
            for field in [
                "type",
                "nullability",
                "default",
                "generation",
                "presence",
                "unrelated",
            ] {
                let mut wrong = after.clone();
                let t = wrong.tables.get_mut(&final_name).unwrap();
                match field {
                    "type" => t.columns.get_mut("value").unwrap().ty = "text".parse().unwrap(),
                    "nullability" => t.columns.get_mut("value").unwrap().nullable = true,
                    "default" => {
                        t.columns.get_mut("value").unwrap().value_source = ValueSource::None
                    }
                    "generation" => {
                        t.columns.insert("value".into(), old.clone());
                    }
                    "presence" => {
                        t.columns.shift_remove("value");
                    }
                    "unrelated" => t.columns.get_mut("id").unwrap().ty = "bigint".parse().unwrap(),
                    _ => unreachable!(),
                }
                let error = check(&wrong)
                    .expect_err("replacement holds its promise and keeps unrelated fields");
                let column = if field == "unrelated" { "id" } else { "value" };
                assert!(format!("{error:#}").contains(&format!("column `{column}`")));
            }
        }
    }
}
