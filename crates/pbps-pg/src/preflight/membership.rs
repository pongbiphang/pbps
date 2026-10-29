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
        let mut sources = Vec::new();
        for table in tables {
            let uid = owner(names, &table, At::Before(step)).ok_or_else(|| {
                unavailable("a selected row contribution has no current table identity")
            })?;
            // Moving tables with declared row writes retain their existing
            // representation guard. Never guess a row after an unknown write.
            if names.changes()[..step].iter().any(|p|matches!(&p.change,Change::InsertRow{table:t,..}|Change::UpdateRow{table:t,..}|Change::DeleteRow{table:t,..} if owner(names,t,At::End).as_ref()==Some(&uid))){return Err(unavailable("the selected membership contribution has preceding row writes which cannot be evaluated before apply"));}
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
}
