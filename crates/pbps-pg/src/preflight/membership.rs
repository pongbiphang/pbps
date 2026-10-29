//! Scalar validation reads the rows at the certified operation boundary,
//! using stored physical sources (DEC-2296.10). No SQL or ordering lives here.
use super::*;
use pbps_model::{
    Uid,
    operation::Snapshot,
    physical_sources::{PhysicalSources, Source},
    plan_names::{At, Holder},
};

fn unavailable(why: &str) -> DialectError {
    DialectError::Invalid {
        dialect: "postgres",
        message: why.into(),
    }
}
fn owner(names: &PlanNames<'_>, name: &TableName, at: At) -> Option<Uid> {
    match names.holder(name, at) {
        Holder::Tracked(u) => Some(u.clone()),
        Holder::Unmanaged | Holder::Vacant => None,
    }
}
/// None keeps the old probe byte-for-byte for a plan without this meeting.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn sources(
    names: &PlanNames<'_>,
    step: usize,
    before: Snapshot<'_>,
) -> Option<Result<Vec<ColumnRef>, DialectError>> {
    let change = &names.changes()[step].change;
    let column = match change {
        Change::AlterColumnType { column, .. }
        | Change::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        } => column,
        _ => return None,
    };
    let parent = owner(names, &column.table, At::Before(step))?;
    let mut moving = false;
    for (at, p) in names.changes().iter().enumerate() {
        let found = match &p.change {
            Change::AttachPartition { parent, .. } => owner(names, parent, At::Before(at)),
            Change::DetachPartition { uid, .. }
            | Change::DropTable {
                uid,
                detach_from: Some(_),
                ..
            } => before
                .ids
                .tables
                .get(uid)
                .and_then(|n| before.schema.tables.get(n))
                .and_then(|t| t.partition_of.as_ref())
                .and_then(|p| before.ids.table_uid(&p.parent))
                .cloned(),
            _ => None,
        };
        moving |= found.as_ref() == Some(&parent);
    }
    if !moving {
        return None;
    }
    Some((|| {
        let physical = PhysicalSources::of(names.clone(), before.schema, before.ids)
            .map_err(|e| unavailable(&e.to_string()))?;
        let pbps_model::physical_sources::Located::Present(members) =
            physical.members_at(&column.table, At::Before(step))
        else {
            return Err(unavailable("the selected membership set is unavailable"));
        };
        let mut tables = vec![column.table.clone()];
        tables.extend(members);
        unchanged(names, step, before, &tables.iter().collect::<Vec<_>>())?;
        let mut sources = Vec::new();
        for table in tables {
            owner(names, &table, At::Before(step)).ok_or_else(|| {
                unavailable("a selected row contribution has no current table identity")
            })?;
            match physical.source(&table.column(&column.name), At::Before(step)) {
                Source::Stored(source) => sources.push(source),
                Source::Created { .. } => {
                    return Err(unavailable(
                        "the selected membership input is created by this plan and cannot be read before apply",
                    ));
                }
                Source::Retired | Source::Unavailable => {
                    return Err(unavailable(
                        "the selected membership input has no stored physical source",
                    ));
                }
            }
        }
        Ok(sources)
    })())
}
/// Physical continuity proves addresses, not the values after a rewrite.
/// Share this conservative rule with range scans; metadata probes stay separate.
/// The ordinary rows_after projection cannot respell recorded USING expressions
/// either (DEC-2296.11), so no second conversion projection is introduced here.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn unchanged(
    names: &PlanNames<'_>,
    step: usize,
    before: Snapshot<'_>,
    tables: &[&TableName],
) -> Result<(), DialectError> {
    // Rebuild phases deliberately have no ordinary PlanFacts row events.
    // Their later recertification cannot supply evidence to an earlier probe.
    if names.changes()[..step]
        .iter()
        .any(|p| p.change.pending_rebuild().is_some())
    {
        return Err(unavailable(
            "a preceding rebuild phase has values which cannot be evaluated before apply",
        ));
    }
    let facts = PlanFacts::of(names);
    let writers = facts
        .before(step)
        .events()
        .filter_map(|(at, event)| {
            let table = match event {
                Event::AlterColumnType { column, .. }
                | Event::AlterColumnExpression { column }
                | Event::RebuildEnumColumn { column }
                | Event::DropColumn { column, .. } => &column.table,
                Event::AddColumn { table, .. }
                | Event::AddComputedColumn { table, .. }
                | Event::DropComputedColumn { table, .. }
                | Event::InsertRow { table, .. }
                | Event::UpdateRow { table, .. }
                | Event::DeleteRow { table, .. } => table,
                Event::CreateTable { .. } => match &names.changes()[at].change {
                    Change::CreateTable {
                        beside_default: Some(default),
                        ..
                    } => default,
                    _ => return None,
                },
                Event::DropTable { .. }
                | Event::RenameColumn { .. }
                | Event::AddForeignKey { .. }
                | Event::DropForeignKey { .. }
                | Event::AttachPartition { .. } => return None,
            };
            Some((at, table))
        })
        .collect::<Vec<_>>();
    if writers.is_empty() {
        return Ok(());
    }
    let reason = || {
        unavailable(
            "the selected contribution has preceding row writes or value changes which cannot be evaluated before apply",
        )
    };
    let physical = PhysicalSources::of(names.clone(), before.schema, before.ids)
        .map_err(|e| unavailable(&e.to_string()))?;
    let mut reads = BTreeSet::new();
    for table in tables {
        reads.insert(owner(names, table, At::Before(step)).ok_or_else(reason)?);
        let pbps_model::physical_sources::Located::Present(members) =
            physical.members_at(table, At::Before(step))
        else {
            return Err(reason());
        };
        for member in members {
            reads.insert(owner(names, &member, At::Before(step)).ok_or_else(reason)?);
        }
    }
    for (at, table) in writers {
        let uid = owner(names, table, At::Before(at)).ok_or_else(reason)?;
        if reads.contains(&uid) {
            return Err(reason());
        }
        let pbps_model::physical_sources::Located::Present(members) =
            physical.members_at(table, At::Before(at))
        else {
            return Err(reason());
        };
        for member in members {
            if reads.contains(&owner(names, &member, At::Before(at)).ok_or_else(reason)?) {
                return Err(reason());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(
        attaching: bool,
        move_first: bool,
    ) -> (
        pbps_model::Schema,
        pbps_model::IdsFile,
        pbps_model::IdsFile,
        Vec<pbps_model::PlannedChange>,
    ) {
        use pbps_model::{
            BoundDatum, Column, PartitionBound, PartitionBy, PartitionOf, PlannedChange, Table,
        };
        let p: TableName = "app.p".parse().unwrap();
        let a: TableName = "app.a".parse().unwrap();
        let b: TableName = "app.b".parse().unwrap();
        let mut parent = Table {
            partition_by: Some(PartitionBy {
                strategy: Default::default(),
                columns: vec!["k".into()],
                external: false,
            }),
            ..Default::default()
        };
        for c in ["k", "d"] {
            parent
                .columns
                .insert(c.into(), Column::new("bigint".parse().unwrap()));
        }
        let member = || Table {
            partition_of: Some(PartitionOf {
                parent: p.clone(),
                bound: PartitionBound::Range {
                    from: vec![BoundDatum::MinValue],
                    to: vec![BoundDatum::MaxValue],
                },
                columns: Default::default(),
            }),
            ..Default::default()
        };
        let mut schema = pbps_model::Schema::default();
        schema.tables.insert(p.clone(), parent.clone());
        schema.tables.insert(b.clone(), member());
        let mut ordinary = parent;
        ordinary.partition_by = None;
        schema
            .tables
            .insert(a.clone(), if attaching { ordinary } else { member() });
        let mut ids = pbps_model::IdsFile::default();
        for (id, n) in [
            ("t_aaaaaa", p.clone()),
            ("t_bbbbbb", a.clone()),
            ("t_cccccc", b),
        ] {
            ids.tables.insert(id.parse().unwrap(), n);
        }
        for (id, n) in [("c_aaaaaa", p.column("k")), ("c_bbbbbb", p.column("d"))] {
            ids.columns.insert(id.parse().unwrap(), n);
        }
        let mut after = ids.clone();
        let ordinary_ids = [("c_cccccc", a.column("k")), ("c_dddddd", a.column("d"))];
        for (id, n) in ordinary_ids {
            if attaching {
                ids.columns.insert(id.parse().unwrap(), n);
            } else {
                after.columns.insert(id.parse().unwrap(), n);
            }
        }
        let c = Change::AlterColumnType {
            uid: "c_bbbbbb".parse().unwrap(),
            column: p.column("d"),
            from: "bigint".parse().unwrap(),
            to: "smallint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        };
        let m = if attaching {
            Change::AttachPartition {
                uid: "t_bbbbbb".parse().unwrap(),
                table: a,
                parent: p.clone(),
                bound: PartitionBound::Default,
                key: vec![],
                shape: Box::new(member()),
            }
        } else {
            Change::DetachPartition {
                uid: "t_bbbbbb".parse().unwrap(),
                table: a,
                parent: p.clone(),
                names: vec![],
                shape: Box::new({
                    let mut t = schema.tables[&p].clone();
                    t.partition_by = None;
                    t
                }),
            }
        };
        let steps = if move_first { vec![m, c] } else { vec![c, m] }
            .into_iter()
            .map(PlannedChange::new)
            .collect();
        (schema, ids, after, steps)
    }
    // Exercise the public preflight entry, including its single RLS wrapper.
    fn scalar(
        names: &PlanNames<'_>,
        step: usize,
        before: Snapshot<'_>,
    ) -> Option<Result<Vec<Probe>, DialectError>> {
        let _selected = sources(names, step, before)?;
        let report = crate::Postgres::new().preflight_with_sources(
            names,
            &pbps_model::Partitions::of(before.schema).renamed(names),
            before,
        );
        Some(if report.unchecked.is_empty() {
            Ok(report
                .probes
                .into_iter()
                .filter(|p| {
                    p.description.contains("cannot become")
                        || p.description.contains("existing NULLs")
                })
                .collect())
        } else {
            Err(unavailable(&report.unchecked[0].reason))
        })
    }
    #[test]
    fn scalar_probes_select_only_the_members_present_at_their_operation() {
        for attaching in [false, true] {
            for move_first in [false, true] {
                let (schema, ids, after, steps) = fixture(attaching, move_first);
                let names = PlanNames::of(&steps, &after).unwrap();
                let probes = scalar(
                    &names,
                    usize::from(move_first),
                    Snapshot {
                        schema: &schema,
                        ids: &ids,
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(probes.len(), 1);
                let sql = &probes[0].sql;
                assert!(
                    sql.contains("ONLY \"app\".\"p\"") && sql.contains("ONLY \"app\".\"b\""),
                    "{sql}"
                );
                assert_eq!(
                    sql.contains("ONLY \"app\".\"a\""),
                    attaching == move_first,
                    "{attaching}/{move_first}: {sql}"
                );
                if attaching == move_first {
                    assert!(
                        sql.contains("row_security_active(E'\"app\".\"a\"'::regclass)"),
                        "{sql}"
                    );
                }
            }
        }
    }
    #[test]
    fn an_unavailable_physical_contribution_does_not_become_an_empty_probe() {
        let (schema, mut ids, after, steps) = fixture(true, true);
        ids.tables.remove(&"t_bbbbbb".parse().unwrap());
        let names = PlanNames::of(&steps, &after).unwrap();
        assert!(
            scalar(
                &names,
                1,
                Snapshot {
                    schema: &schema,
                    ids: &ids
                }
            )
            .unwrap()
            .is_err()
        );
    }
    #[test]
    fn empty_membership_is_known_and_ordinary_probe_bytes_are_preserved() {
        let (mut schema, mut ids, after, steps) = fixture(true, false);
        schema.tables.remove(&"app.b".parse().unwrap());
        ids.tables.remove(&"t_cccccc".parse().unwrap());
        let names = PlanNames::of(&steps, &after).unwrap();
        let before = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        let selected = sources(&names, 0, before).unwrap().unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].table, "app.p".parse().unwrap());
        let ordinary = &steps[..1];
        let names = PlanNames::of(ordinary, &after).unwrap();
        let pg = crate::Postgres::new();
        let partitions = pbps_model::Partitions::of(&schema).renamed(&names);
        assert!(sources(&names, 0, before).is_none());
        let old = pg.preflight_with(&names, &partitions);
        let new = pg.preflight_with_sources(&names, &partitions, before);
        assert_eq!(format!("{old:?}"), format!("{new:?}"));
    }
    #[test]
    fn selected_inputs_with_row_writes_or_using_are_unchecked() {
        for using in [false, true] {
            let (schema, ids, after, mut steps) = fixture(true, true);
            if using {
                let Change::AlterColumnType { using, .. } = &mut steps[1].change else {
                    panic!()
                };
                *using = Some("d::smallint".into());
            } else {
                steps.insert(
                    1,
                    pbps_model::PlannedChange::new(Change::DeleteRow {
                        table: "app.p".parse().unwrap(),
                        key_column: "k".into(),
                        key: "1".into(),
                        row: Default::default(),
                        key_type: None,
                        cause: pbps_model::change::DeleteCause::Undeclared,
                        dropped: Default::default(),
                        types: Default::default(),
                        after_types: Default::default(),
                    }),
                );
            }
            let names = PlanNames::of(&steps, &after).unwrap();
            let report = crate::Postgres::new().preflight_with_sources(
                &names,
                &pbps_model::Partitions::of(&schema).renamed(&names),
                Snapshot {
                    schema: &schema,
                    ids: &ids,
                },
            );
            assert!(
                report.unchecked.iter().any(|u| u.reason.contains(if using {
                    "conversion expression"
                } else {
                    "preceding row writes"
                })),
                "{report:?}"
            );
            assert!(
                !report
                    .probes
                    .iter()
                    .any(|p| p.description.contains("cannot become"))
            );
        }
    }
    #[test]
    fn prior_source_value_rewrites_are_unchecked_and_later_ones_are_not_borrowed() {
        let (schema, ids, after, steps) = fixture(true, true);
        let a: TableName = "app.a".parse().unwrap();
        let rewrites = [
            Change::AlterColumnType {
                uid: "c_dddddd".parse().unwrap(),
                column: a.column("d"),
                from: "integer".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: Some("coalesce(d,0)".into()),
            },
            Change::AlterColumnExpression {
                uid: "c_dddddd".parse().unwrap(),
                column: a.column("d"),
                from: "k".into(),
                to: "k+1".into(),
            },
            Change::AddColumn {
                uid: "c_eeeeee".parse().unwrap(),
                table: a.clone(),
                name: "x".into(),
                column: Box::new(pbps_model::Column {
                    value_source: ValueSource::Default("1".into()),
                    ..pbps_model::Column::new("integer".parse().unwrap())
                }),
            },
        ];
        for change in rewrites {
            for earlier in [false, true] {
                let rewrite = pbps_model::PlannedChange::new(change.clone());
                let input = if earlier {
                    vec![rewrite, steps[0].clone(), steps[1].clone()]
                } else {
                    vec![steps[1].clone(), rewrite, steps[0].clone()]
                };
                let names = PlanNames::of(&input, &after).unwrap();
                let result = sources(
                    &names,
                    if earlier { 2 } else { 0 },
                    Snapshot {
                        schema: &schema,
                        ids: &ids,
                    },
                )
                .unwrap();
                assert_eq!(result.is_err(), earlier, "{change:?}: {result:?}");
            }
        }
    }
    #[test]
    fn unrelated_rewrites_and_default_metadata_do_not_hide_known_sources() {
        let (mut schema, mut ids, mut after, mut steps) = fixture(true, true);
        let u: TableName = "app.unrelated".parse().unwrap();
        schema
            .tables
            .insert(u.clone(), pbps_model::Table::default());
        ids.tables.insert("t_dddddd".parse().unwrap(), u.clone());
        after.tables.insert("t_dddddd".parse().unwrap(), u.clone());
        ids.columns
            .insert("c_ffffff".parse().unwrap(), u.column("d"));
        after
            .columns
            .insert("c_ffffff".parse().unwrap(), u.column("d"));
        for table in [u, "app.a".parse().unwrap()] {
            let change = if table.name == "unrelated" {
                Change::AlterColumnExpression {
                    uid: "c_ffffff".parse().unwrap(),
                    column: table.column("d"),
                    from: "1".into(),
                    to: "2".into(),
                }
            } else {
                Change::AlterColumnDefault {
                    uid: "c_dddddd".parse().unwrap(),
                    column: table.column("d"),
                    from: None,
                    to: Some("1".into()),
                }
            };
            steps.insert(0, pbps_model::PlannedChange::new(change));
        }
        let names = PlanNames::of(&steps, &after).unwrap();
        assert!(
            sources(
                &names,
                3,
                Snapshot {
                    schema: &schema,
                    ids: &ids
                }
            )
            .unwrap()
            .is_ok()
        );
    }
    #[test]
    fn rebuild_copy_prefixes_never_supply_stored_value_evidence() {
        let (schema, ids, _, _) = fixture(true, true);
        let p: TableName = "app.p".parse().unwrap();
        let a: TableName = "app.a".parse().unwrap();
        let copy = Change::CopyTableRows(Box::new(pbps_model::CopyTableRows {
            source: pbps_model::RebuildRelation {
                uid: "t_aaaaaa".parse().unwrap(),
                name: p.clone(),
            },
            target: pbps_model::RebuildRelation {
                uid: "t_bbbbbb".parse().unwrap(),
                name: a.clone(),
            },
            columns: ["k", "d"]
                .into_iter()
                .map(|c| pbps_model::CopyColumn {
                    source: Some(pbps_model::CopySource {
                        column: p.column(c),
                        ty: "bigint".parse().unwrap(),
                        collation: None,
                    }),
                    target: a.column(c),
                    cast: "bigint".parse().unwrap(),
                    collation: None,
                    stages: vec![],
                })
                .collect(),
            source_partitions: vec![pbps_model::RebuildRelation {
                uid: "t_cccccc".parse().unwrap(),
                name: "app.b".parse().unwrap(),
            }],
            target_partitions: vec![],
            max_rows: 32,
        }));
        let input = [pbps_model::PlannedChange::new(copy)];
        let names = PlanNames::of(&input, &ids).unwrap();
        let before = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        assert!(unchanged(&names, 0, before, &[&a]).is_ok());
        assert!(
            unchanged(&names, 1, before, &[&a])
                .unwrap_err()
                .to_string()
                .contains("preceding rebuild phase")
        );
    }
}
