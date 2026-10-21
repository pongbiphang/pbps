#![cfg(test)]
//! Frozen dd84f894 parent-default restoration, before S7 PR3 (DEC-1959.4).
use super::*;

#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn after_their_parents_defaults(
    cs: &mut ChangeSet,
    declared: &Schema,
    ids: &[&IdsFile],
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<usize, String> {
    let mut added = 0;
    let mut i = 0;
    while i < cs.changes.len() {
        let Change::AlterColumnDefault {
            column,
            to: Some(_),
            ..
        } = &cs.changes[i].change
        else {
            i += 1;
            continue;
        };
        let column = column.clone();
        let own: Vec<(TableName, String)> = declared
            .tables
            .iter()
            .filter_map(|(name, t)| {
                let of = t
                    .partition_of
                    .as_ref()
                    .filter(|of| of.parent == column.table)?;
                let default = of.columns.get(&column.name)?.default.clone()?;
                Some((name.clone(), default))
            })
            .collect();
        for (partition, default) in own {
            let sets = |c: &Change| {
                matches!(c, Change::SetPartitionDefault { table, column: c, to: Some(_), .. }
                    if *table == partition && *c == column.name)
            };
            let later = cs.changes[i + 1..].iter().any(|p| {
                sets(&p.change)
                    || matches!(&p.change, Change::CreateTable { name, .. } if *name == partition)
            });
            if later {
                continue;
            }
            if let Some(k) = (0..i).rev().find(|&k| sets(&cs.changes[k].change)) {
                let moved = cs.changes.remove(k);
                i -= 1;
                cs.changes.insert(i + 1, moved);
            } else {
                let uid = ids
                    .iter()
                    .find_map(|ids| ids.table_uid(&partition))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "no identity names the partition `{partition}`",
                            partition = partition.shown()
                        )
                    })?;
                let fallback = parent_default(declared, &partition, column.name.raw());
                cs.changes.insert(
                    i + 1,
                    planned(
                        Change::SetPartitionDefault {
                            uid,
                            table: partition,
                            parent: column.table.clone(),
                            column: column.name.clone(),
                            from: None,
                            to: Some(default),
                            fallback,
                        },
                        dialect,
                    ),
                );
                added += 1;
            }
        }
        i += 1;
    }
    Ok(added)
}

pub(super) fn same_plan(
    actual: &ChangeSet,
    expected: &ChangeSet,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) {
    let mut comparable = actual.clone();
    for (now, old) in comparable.changes.iter_mut().zip(&expected.changes) {
        if let Some(origin) = &now.provenance
            && origin.rule == "partition-parent-defaults"
            && old.provenance.as_ref() != Some(origin)
        {
            assert_eq!(origin.role, pbps_model::AddedRole::Restoration);
            assert!(old.provenance.is_none(), "an existing origin must survive");
            let Change::SetPartitionDefault { parent, column, .. } = &now.change else {
                panic!("only an added partition restoration has this origin");
            };
            assert!(actual.changes.iter().any(|p| {
                matches!(&p.change, Change::AlterColumnDefault { column: c, to: Some(_), .. }
                    if c.table == *parent && c.name == *column && p.change.subject() == origin.source)
            }), "a restoration serves a real parent SET");
            now.provenance = None;
        }
    }
    super::split_oracle::same_plan(&comparable, expected, dialect);
}

use pbps_model::{Column, PartitionBound, PartitionColumn, PartitionOf, Table, Uid, UidKind};

fn partition(k: usize) -> TableName {
    match k {
        0 => TableName::new("app", "pa"),
        1 => TableName::new("arch", "pb"),
        2 => TableName::new("app", "inh"),
        3 => TableName::new("app", "other"),
        _ => unreachable!(),
    }
}

fn fixture(salt: u32) -> (Schema, IdsFile) {
    let parent = TableName::new("app", "ev");
    let mut table = Table::default();
    for name in ["k", "v", "other"] {
        let mut column = Column::new("integer".parse().unwrap());
        if name == "v" {
            column.value_source = ValueSource::Default("app.f(1)".into());
        }
        table.columns.insert(name.into(), column);
    }
    table.partition_by = Some(pbps_model::PartitionBy {
        external: false,
        columns: vec!["k".into()],
        strategy: Default::default(),
    });
    let mut schema = Schema {
        tables: [(parent.clone(), table)].into(),
        ..Default::default()
    };
    for k in 0..4 {
        let own = match k {
            0 => Some(("v", "app.f(2)")),
            1 => Some(("v", "f(3)")),
            2 => None,
            3 => Some(("other", "7")),
            _ => unreachable!(),
        };
        schema.tables.insert(
            partition(k),
            Table {
                partition_of: Some(PartitionOf {
                    parent: parent.clone(),
                    bound: PartitionBound::Range {
                        from: vec![pbps_model::BoundDatum::Value((k * 10).to_string())],
                        to: vec![pbps_model::BoundDatum::Value((k * 10 + 10).to_string())],
                    },
                    columns: own
                        .map(|(name, default)| {
                            (
                                name.into(),
                                PartitionColumn {
                                    default: Some(default.into()),
                                    not_null: false,
                                },
                            )
                        })
                        .into_iter()
                        .collect(),
                }),
                ..Default::default()
            },
        );
    }
    let mut ids = super::generated_tests::identities(&schema);
    ids.tables = ids
        .tables
        .into_values()
        .map(|name| {
            (
                Uid::derived(UidKind::Table, &name.raw().to_string(), salt),
                name,
            )
        })
        .collect();
    ids.columns = ids
        .columns
        .into_values()
        .map(|name| {
            (
                Uid::derived(UidKind::Column, &name.raw().to_string(), salt),
                name,
            )
        })
        .collect();
    (schema, ids)
}

fn parent(ids: &IdsFile, column: &str, to: Option<&str>) -> Change {
    let column = TableName::new("app", "ev").column(column);
    Change::AlterColumnDefault {
        uid: ids.column_uid(&column).unwrap().clone(),
        column,
        from: Some("app.f(1)".into()),
        to: to.map(str::to_owned),
    }
}

fn restore(ids: &IdsFile, k: usize) -> Change {
    Change::SetPartitionDefault {
        uid: ids.table_uid(&partition(k)).unwrap().clone(),
        table: partition(k),
        parent: TableName::new("app", "ev"),
        column: "v".into(),
        from: Some("9".into()),
        to: Some(if k == 0 { "app.f(2)" } else { "f(3)" }.into()),
        fallback: Some("app.f(1)".into()),
    }
}

fn create(schema: &Schema, ids: &IdsFile, k: usize) -> Change {
    Change::CreateTable {
        uid: ids.table_uid(&partition(k)).unwrap().clone(),
        name: partition(k),
        table: Box::new(schema.tables[&partition(k)].clone()),
        beside_default: None,
        moves_at_most: None,
    }
}

fn free(name: &str, salt: u32) -> Change {
    Change::CreateRole {
        uid: Uid::derived(UidKind::Role, name, salt),
        name: name.into(),
    }
}

fn plan(
    changes: Vec<Change>,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> ChangeSet {
    ChangeSet {
        changes: changes
            .into_iter()
            .map(|change| super::planned(change, dialect))
            .collect(),
    }
}

#[test]
fn parent_default_restorations_match_old_bytes_for_32_uid_seeds() {
    for salt in 0..32 {
        let (schema, ids) = fixture(salt);
        for version in [None, Some(160_000), Some(170_000), Some(180_000)] {
            let pg = version.map_or_else(pbps_pg::Postgres::new, |version| {
                pbps_pg::Postgres::new().with_server_version(version)
            });
            for a in 0..5 {
                for b in 0..5 {
                    for repeated in [false, true] {
                        // Missing, earlier, later, later CREATE, or between
                        // two parent SETs. New and existing restores mix.
                        let placements = [a, b];
                        let mut changes = vec![free("prefix", salt)];
                        for k in if salt % 2 == 0 { [0, 1] } else { [1, 0] } {
                            if placements[k] == 1 {
                                if salt % 3 == 0 {
                                    changes.push(restore(&ids, k));
                                }
                                changes.push(restore(&ids, k));
                            }
                        }
                        changes.push(parent(&ids, "v", Some("app.f(1)")));
                        changes.push(free("between", salt));
                        for (k, placement) in placements.iter().enumerate() {
                            if *placement == 4 {
                                changes.push(restore(&ids, k));
                            }
                        }
                        if repeated {
                            changes.push(parent(&ids, "v", Some("app.f(1)")));
                            changes.push(free("after_parent", salt));
                        }
                        for (k, placement) in placements.iter().enumerate() {
                            if *placement == 2 {
                                changes.push(restore(&ids, k));
                            }
                            if *placement == 3 {
                                changes.push(create(&schema, &ids, k));
                            }
                        }
                        changes.push(free("suffix", salt));
                        let mut cs = plan(changes, &pg);
                        // Metadata on reused operations must survive too.
                        for p in &mut cs.changes {
                            if matches!(p.change, Change::SetPartitionDefault { .. }) {
                                p.provenance = Some(pbps_model::Provenance {
                                    rule: if salt % 2 == 0 {
                                        "existing-restoration"
                                    } else {
                                        "partition-parent-defaults"
                                    },
                                    role: pbps_model::AddedRole::Restoration,
                                    source: "an earlier expansion".into(),
                                    reason: "retain the original payload".into(),
                                });
                            }
                        }
                        let expected_added =
                            placements.iter().filter(|&&position| position == 0).count();
                        assert_eq!(
                            super::after_their_parents_defaults(&mut cs, &schema, &[&ids], &pg),
                            Ok(expected_added),
                            "{salt}/{version:?}/{placements:?}/{repeated}"
                        );
                        let once = cs.clone();
                        assert_eq!(
                            super::after_their_parents_defaults(&mut cs, &schema, &[&ids], &pg),
                            Ok(0)
                        );
                        super::split_oracle::same_plan(&cs, &once, &pg);
                    }
                }
            }
        }
    }
}

#[test]
fn restoration_compatibility_is_an_explicit_constraint_consumed_by_settle() {
    let (schema, ids) = fixture(0);
    let pg = pbps_pg::Postgres::new();
    let cs = plan(
        vec![
            restore(&ids, 0),
            restore(&ids, 1),
            parent(&ids, "v", Some("app.f(1)")),
        ],
        &pg,
    );
    let rule = pbps_diff::order::parent_defaults::ParentDefaults {
        initial: &cs.changes,
        declared: &schema,
        ids: &[&ids],
        dialect: &pg,
    };
    let edges = rule.order(&cs.changes).unwrap();
    let mut schedule = Schedule::new(cs.changes.len());
    for edge in edges {
        let (expand::Step::Plan(before), expand::Step::Plan(after)) = (edge.before, edge.after)
        else {
            panic!("final constraints name existing steps");
        };
        schedule.edge(before, after, edge.source, edge.reason);
    }
    assert!(schedule.constraints().iter().any(|e| e.before == 1
        && e.after == 0
        && e.source == Source::Lexical
        && e.reason == OrderReason::Restoration));
    assert_eq!(schedule.settle(cs).unwrap().sequence, [2, 1, 0]);
}

#[test]
fn parent_default_negatives_add_nothing_and_missing_identity_is_refused_by_name() {
    let (schema, ids) = fixture(17);
    let pg = pbps_pg::Postgres::new();
    for changes in [
        vec![free("no_parent", 17)],
        vec![parent(&ids, "other", None)],
        vec![parent(&ids, "k", Some("1"))],
        vec![
            parent(&ids, "v", Some("app.f(1)")),
            create(&schema, &ids, 0),
            create(&schema, &ids, 1),
        ],
    ] {
        let mut cs = plan(changes, &pg);
        let original = cs.clone();
        assert_eq!(
            super::after_their_parents_defaults(&mut cs, &schema, &[&ids], &pg),
            Ok(0)
        );
        super::split_oracle::same_plan(&cs, &original, &pg);
    }
    for inherited in [false, true] {
        let mut declarations = schema.clone();
        for k in 0..2 {
            let own = &mut declarations
                .tables
                .get_mut(&partition(k))
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns;
            let mut override_ = own.remove("v").unwrap();
            if inherited {
                override_.default = None;
                own.insert("v".into(), override_);
            } else {
                own.insert("other".into(), override_);
            }
        }
        let mut cs = plan(vec![parent(&ids, "v", Some("app.f(1)"))], &pg);
        let original = cs.clone();
        assert_eq!(
            super::after_their_parents_defaults(&mut cs, &declarations, &[&ids], &pg),
            Ok(0)
        );
        super::split_oracle::same_plan(&cs, &original, &pg);
    }
    let mut no_identity = ids.clone();
    no_identity.tables.retain(|_, name| name != &partition(0));
    let mut cs = plan(vec![parent(&ids, "v", Some("app.f(1)"))], &pg);
    assert_eq!(
        super::after_their_parents_defaults(&mut cs, &schema, &[&no_identity], &pg),
        Err("no identity names the partition `app.pa`".into())
    );
}
