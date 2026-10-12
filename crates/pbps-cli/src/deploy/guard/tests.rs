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
