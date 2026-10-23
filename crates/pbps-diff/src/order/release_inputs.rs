//! Carry a delayed insert's consumers through the existing release settlement.

use std::collections::BTreeSet;

use pbps_dialect::Dialect;
use pbps_model::ModuleDeps;
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};

use super::{Baseline, Checks, Graph, PlanningPlan, Schedule, analyse, graph};

/// Add the effect closure of inserts held behind a released input's drop.
/// Catalog/lexical waits and unrelated Keeps remain the caller's proposals;
/// this constructs no execution certificate and adds no settlement stage.
/// Graph unmet requirements remain observations, as in DEC-1956.3.
pub fn carry_inserts<S>(
    plan: &PlanningPlan<S>,
    mut release: Schedule,
    inserts: &BTreeSet<usize>,
    before: crate::Side<'_>,
    after: crate::Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    declared: &ModuleDeps,
) -> Result<Schedule, String> {
    if inserts.is_empty() {
        return Ok(release);
    }
    let names = pbps_model::plan_names::PlanNames::of(&plan.changes, after.ids)
        .map_err(|error| error.to_string())?;
    let owners = PlanOwners::new(names, before.ids, after.ids);
    let operations = plan.identities.as_ref().map_or_else(
        || {
            Operations::expanded(
                plan.logical,
                plan.origins
                    .iter()
                    .cloned()
                    .zip(plan.changes.iter().cloned()),
                if dialect.rebuilds_modules() {
                    ModuleEdits::Rebuilt
                } else {
                    ModuleEdits::InPlace
                },
            )
        },
        |identities| identities.with_steps(&plan.changes),
    );
    let base = Snapshot {
        schema: before.schema,
        ids: before.ids,
    };
    let end = Snapshot {
        schema: after.schema,
        ids: after.ids,
    };
    let effects = analyse(&operations, &owners, base, end, dialect);
    if let Some((_, error)) = effects.unowned.first() {
        return Err(error.to_string());
    }
    let mut graph = graph(
        operations.as_slice(),
        &effects.effects,
        &Baseline::of(base, &owners),
        declared,
        Checks::ALL,
    );
    if let Some(at) = inserts.iter().find(|&&at| at >= operations.len()) {
        return Err(format!("released-input insert {at} is outside the plan"));
    }
    let moving = consumers(&graph, operations.len(), inserts);
    // Incoming prerequisites constrain participants too. Edges only between
    // unrelated operations belong to their own existing stages, not this one.
    graph
        .edges
        .retain(|edge| moving[edge.before] || moving[edge.after]);
    let dependencies = Schedule::of_graph(&graph, operations.as_slice());
    for edge in dependencies.constraints() {
        release.edge(edge.before, edge.after, edge.source, edge.reason);
    }
    Ok(release)
}

fn consumers(graph: &Graph, steps: usize, inserts: &BTreeSet<usize>) -> Vec<bool> {
    let mut moving = vec![false; steps];
    let mut outgoing = vec![Vec::new(); steps];
    for &at in inserts {
        moving[at] = true;
    }
    for edge in &graph.edges {
        outgoing[edge.before].push(edge.after);
    }
    let mut pending: Vec<_> = inserts.iter().copied().collect();
    while let Some(at) = pending.pop() {
        for &dependent in &outgoing[at] {
            if !moving[dependent] {
                moving[dependent] = true;
                pending.push(dependent);
            }
        }
    }
    moving
}

/// Before the connected release settlement, its row consumers may still
/// precede an expression rewrite. Final replay keeps every check enabled.
#[cfg(any(test, debug_assertions))]
pub(super) fn pending_consumers(
    graph: &Graph,
    operations: &[pbps_model::operation::Operation],
) -> Vec<bool> {
    let mut releases = std::collections::BTreeMap::new();
    let mut drops = std::collections::BTreeMap::new();
    for (at, operation) in operations.iter().enumerate() {
        if let pbps_model::Change::AlterColumnExpression { column, .. } = operation.change() {
            releases.insert(column.table.clone(), at);
        }
        if let pbps_model::Change::DropColumn { column, .. } = operation.change() {
            drops.entry(column.table.clone()).or_insert(at);
        }
    }
    // Catalog-only trigger facts can require the drop even when portable
    // effects conservatively omit Absent(NotNull). Until that catalog read,
    // defer only potential crossings; Final always checks the actual result.
    let inserts = operations
        .iter()
        .enumerate()
        .filter_map(|(at, operation)| {
            let pbps_model::Change::InsertRow { table, .. } = operation.change() else {
                return None;
            };
            (drops.get(table).is_some_and(|&drop| drop < at)
                && releases.get(table).is_some_and(|&release| at < release))
            .then_some(at)
        })
        .collect();
    consumers(graph, operations.len(), &inserts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Source, propose};
    use pbps_model::resolver::OrderReason;
    use pbps_model::{
        Change, ChangeSet, Column, ForeignKey, IdsFile, PlannedChange, PrimaryKey,
        ReferentialAction, Row, RowKey, Schema, Table, TableName, Uid, UidKind, Value,
    };

    fn table(name: &str) -> TableName {
        TableName::new("app", name)
    }
    fn column(name: &str) -> pbps_model::ColumnRef {
        table("p").column(name)
    }
    fn uid(name: &str, kind: UidKind) -> Uid {
        Uid::derived(kind, name, 0)
    }
    fn row(name: &str, key: i64, cells: &[(&str, i64)]) -> Change {
        Change::InsertRow {
            table: table(name),
            key_column: "id".into(),
            identity_key: false,
            key: RowKey::from(key.to_string().as_str()),
            row: cells
                .iter()
                .map(|(name, value)| ((*name).into(), Value::Int(*value)))
                .collect::<Row>(),
            defaults: Default::default(),
            types: Default::default(),
        }
    }
    fn shape(columns: &[&str]) -> Table {
        Table {
            columns: columns
                .iter()
                .map(|name| ((*name).into(), Column::new("integer".parse().unwrap())))
                .collect(),
            primary_key: Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            }),
            ..Default::default()
        }
    }
    fn carried(consumer: Change) {
        let mut before = Schema::default();
        before
            .tables
            .insert(table("p"), shape(&["id", "a", "b", "g"]));
        let mut child = shape(&["id", "pid"]);
        child.foreign_keys.insert(
            "fk".into(),
            ForeignKey {
                columns: vec!["pid".into()],
                references_table: table("p"),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::default(),
                on_update: ReferentialAction::default(),
            },
        );
        before.tables.insert(table("c"), child);
        before
            .tables
            .get_mut(&table("p"))
            .unwrap()
            .columns
            .get_mut("g")
            .unwrap()
            .value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
            expression: "a * 2".into(),
            stored: true,
        });
        before
            .tables
            .get_mut(&table("p"))
            .unwrap()
            .columns
            .get_mut("a")
            .unwrap()
            .nullable = false;
        let mut after = before.clone();
        after
            .tables
            .get_mut(&table("p"))
            .unwrap()
            .columns
            .get_mut("g")
            .unwrap()
            .value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
            expression: "b * 2".into(),
            stored: true,
        });
        after
            .tables
            .get_mut(&table("p"))
            .unwrap()
            .columns
            .shift_remove("a");
        if let Change::AddUnique {
            name, constraint, ..
        } = &consumer
        {
            after
                .tables
                .get_mut(&table("p"))
                .unwrap()
                .unique
                .insert(name.clone(), constraint.clone());
        }
        let mut base_ids = IdsFile::default();
        for (name, shape) in &before.tables {
            base_ids
                .tables
                .insert(uid(&name.raw().to_string(), UidKind::Table), name.clone());
            for col in shape.columns.keys() {
                let address = name.column(col).raw().to_string();
                base_ids
                    .columns
                    .insert(uid(&address, UidKind::Column), name.column(col));
            }
        }
        let mut ids = base_ids.clone();
        let a = uid("app.p.a", UidKind::Column);
        ids.columns.remove(&a);
        let plan = propose(ChangeSet {
            changes: vec![
                Change::DropColumn {
                    uid: a,
                    column: column("a"),
                },
                row("p", 1, &[("b", 3)]),
                consumer,
                Change::AlterColumnExpression {
                    uid: uid("app.p.g", UidKind::Column),
                    column: column("g"),
                    from: "a * 2".into(),
                    to: "b * 2".into(),
                },
            ]
            .into_iter()
            .map(PlannedChange::new)
            .collect(),
        });
        let mut schedule = Schedule::new(4);
        schedule.edge(3, 0, Source::Catalog, OrderReason::Structural);
        schedule.edge(0, 1, Source::Catalog, OrderReason::Structural);
        if matches!(&plan.changes[2].change, Change::AddUnique { .. }) {
            let operations = Operations::of_plan(&plan.changes, ModuleEdits::InPlace);
            let names = pbps_model::plan_names::PlanNames::of(&plan.changes, &ids).unwrap();
            let owners = PlanOwners::new(names, &base_ids, &ids);
            let base = Snapshot {
                schema: &before,
                ids: &base_ids,
            };
            let end = Snapshot {
                schema: &after,
                ids: &ids,
            };
            let report = analyse(
                &operations,
                &owners,
                base,
                end,
                &pbps_dialect::MinimalDialect,
            )
            .report(
                &operations,
                &Baseline::of(base, &owners),
                &ModuleDeps::new(),
            );
            let (lines, fails) = super::super::shadow::judge(
                operations.as_slice(),
                &report,
                super::super::shadow::Stage::Differ,
            );
            assert!(!fails, "{lines:?}");
            assert!(lines.iter().any(|line| line.starts_with("connected ")));
            assert!(
                super::super::shadow::judge(
                    operations.as_slice(),
                    &report,
                    super::super::shadow::Stage::Final
                )
                .1
            );
            let wrong = super::super::reordered_operations(&operations, &[2, 3, 0, 1]);
            let wrong_report = analyse(&wrong, &owners, base, end, &pbps_dialect::MinimalDialect)
                .report(&wrong, &Baseline::of(base, &owners), &ModuleDeps::new());
            assert!(
                super::super::shadow::judge(
                    wrong.as_slice(),
                    &wrong_report,
                    super::super::shadow::Stage::Differ
                )
                .1
            );
            let mut catalog_only = report.clone();
            catalog_only.graph.edges.retain(|edge| {
                !matches!(
                    edge.because,
                    super::super::Because::Frees(pbps_model::operation::ObjectKey::NotNull(_))
                )
            });
            // A standing declaration can hide the required-input effect
            // while an earlier trigger rebuild makes the catalog require it.
            assert!(
                !super::super::shadow::judge(
                    operations.as_slice(),
                    &catalog_only,
                    super::super::shadow::Stage::Differ
                )
                .1
            );
        }
        let schedule = carry_inserts(
            &plan,
            schedule,
            &BTreeSet::from([1]),
            crate::Side {
                schema: &before,
                ids: &base_ids,
            },
            crate::Side {
                schema: &after,
                ids: &ids,
            },
            &pbps_dialect::MinimalDialect,
            &ModuleDeps::new(),
        )
        .unwrap();
        let settled = schedule.settle(plan).unwrap();
        assert_eq!(settled.sequence, vec![3, 0, 1, 2]);
        assert_eq!(
            settled.changes.origins(),
            &[
                pbps_model::operation::Origin::Whole(3),
                pbps_model::operation::Origin::Whole(0),
                pbps_model::operation::Origin::Whole(1),
                pbps_model::operation::Origin::Whole(2)
            ]
        );
        assert!(
            settled
                .proof
                .edges()
                .iter()
                .any(|edge| edge.reason == OrderReason::Data)
        );
    }

    #[test]
    fn a_child_row_follows_an_insert_delayed_by_its_released_input() {
        carried(row("c", 1, &[("pid", 1)]));
    }
    #[test]
    fn a_later_same_table_row_follows_an_insert_delayed_by_its_released_input() {
        carried(row("p", 2, &[("b", 4)]));
    }
    #[test]
    fn a_key_over_rows_follows_an_insert_delayed_by_its_released_input() {
        carried(Change::AddUnique {
            table: table("p"),
            name: "uq".into(),
            constraint: pbps_model::UniqueConstraint {
                columns: vec!["b".into()],
                storage_parameters: Default::default(),
            },
            clustered: false,
        });
    }
    #[test]
    fn no_carried_insert_keeps_the_existing_schedule_without_reading_facts() {
        let plan = propose(ChangeSet::default());
        let empty = Schema::default();
        let ids = IdsFile::default();
        let side = crate::Side {
            schema: &empty,
            ids: &ids,
        };
        assert!(
            carry_inserts(
                &plan,
                Schedule::new(0),
                &BTreeSet::new(),
                side,
                side,
                &pbps_dialect::MinimalDialect,
                &ModuleDeps::new()
            )
            .unwrap()
            .constraints()
            .is_empty()
        );
    }
}
