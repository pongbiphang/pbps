//! Existing companion placement and partition splitting. The ordering
//! boundary owns these writes; callers supply declarations and lexical facts.
use pbps_dialect::Dialect;
use pbps_model::{Change, ColumnRef, ModuleId, PlannedChange, Schema, TableName, ValueSource};
use std::collections::BTreeSet;

/// Removed companions retain their exact source mapping until restoration.
pub struct PublicDecisions(Vec<(PlannedChange, pbps_model::operation::Origin)>);

pub fn take_public_execution(cs: &mut super::PlanningPlan) -> PublicDecisions {
    let (decisions, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut cs.payload.changes)
        .into_iter()
        .zip(std::mem::take(&mut cs.origins))
        .partition(|(p, _)| matches!(p.change, Change::PublicExecution { .. }));
    (cs.payload.changes, cs.origins) = rest.into_iter().unzip();
    cs.invalidate();
    PublicDecisions(decisions)
}

pub fn settle_public_execution(cs: &mut super::PlanningPlan, decisions: PublicDecisions) {
    for (decision, origin) in decisions.0 {
        let Change::PublicExecution { routine, .. } = &decision.change else {
            cs.payload.changes.push(decision);
            cs.origins.push(origin);
            continue;
        };
        let id = ModuleId::Routine(routine.clone());
        let at = cs
            .changes
            .iter()
            .rposition(|p| {
                matches!(&p.change,
                    Change::CreateModule { id: x, .. } | Change::AlterModule { id: x, .. } if *x == id)
            })
            .map(|i| i + 1)
            .or_else(|| {
                cs.changes.iter().position(|p| {
                    matches!(
                        p.change,
                        Change::CreateRole { .. } | Change::Grant { .. } | Change::SetDataMode { .. }
                        | Change::SetEnsureKeys { .. }
                    )
                })
            })
            .unwrap_or(cs.changes.len());
        cs.payload.changes.insert(at, decision);
        cs.origins.insert(at, origin);
    }
    cs.invalidate();
}

pub fn split_moving_defaults(
    cs: &mut super::PlanningPlan,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    functions: &[(String, usize)],
) {
    let called_after = |text: &str, at: usize| {
        functions
            .iter()
            .filter(|(f, created)| *created > at && dialect.generation_may_call(text, f))
            .map(|(_, created)| *created)
            .max()
    };
    let late: BTreeSet<ColumnRef> = cs
        .changes
        .iter()
        .enumerate()
        .filter_map(|(j, p)| {
            let Change::AddColumn {
                table,
                name,
                column,
                ..
            } = &p.change
            else {
                return None;
            };
            column_expression(column)
                .and_then(|text| called_after(text, j))
                .map(|_| table.column(name))
        })
        .collect();
    // Each split change, with the place it goes right after.
    let mut sets: Vec<(usize, Change, pbps_model::operation::Origin)> = Vec::new();
    for (i, p) in cs.payload.changes.iter_mut().enumerate() {
        let Change::CreateTable {
            uid,
            name,
            table,
            beside_default: Some(_),
            moves_at_most: _,
        } = &mut p.change
        else {
            continue;
        };
        let Some(of) = table.partition_of.as_mut() else {
            continue;
        };
        let parent = of.parent.clone();
        let before = sets.len();
        for (column, own) in &mut of.columns {
            let added_late = late.contains(&parent.column(column));
            let after = own
                .default
                .as_deref()
                .and_then(|text| called_after(text, i));
            if (after.is_some() || added_late)
                && let Some(to) = own.default.take()
            {
                sets.push((
                    after.unwrap_or(i),
                    Change::SetPartitionDefault {
                        uid: uid.clone(),
                        table: name.clone(),
                        parent: parent.clone(),
                        column: column.clone(),
                        from: None,
                        to: Some(to),
                        fallback: parent_default(declared, name, column.raw()),
                    },
                    pbps_model::operation::Origin::Restoration(cs.origins[i].source()),
                ));
            }
            if added_late && std::mem::take(&mut own.not_null) {
                sets.push((
                    i,
                    Change::SetPartitionNotNull {
                        uid: uid.clone(),
                        table: name.clone(),
                        column: column.clone(),
                        not_null: true,
                    },
                    pbps_model::operation::Origin::Restoration(cs.origins[i].source()),
                ));
            }
        }
        of.columns.retain(|_, c| c.default.is_some() || c.not_null);
        if sets.len() > before {
            p.risks = dialect.change_risks(&p.change);
        }
    }
    // Each right after its place, inserted from the last place back so that
    // the earlier places still hold; two after one place keep their order.
    // One split off for a column the parent adds names that column, so the
    // reorder holds it behind the column as it would have held the create.
    sets.sort_by_key(|(after, _, _)| *after);
    for (after, change, origin) in sets.into_iter().rev() {
        cs.payload
            .changes
            .insert(after + 1, planned(change, dialect));
        cs.origins.insert(after + 1, origin);
    }
    cs.invalidate();
}

fn column_expression(column: &pbps_model::Column) -> Option<&str> {
    match &column.value_source {
        ValueSource::Default(default) => Some(default),
        ValueSource::Generated(generated) => Some(&generated.expression),
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
    }
}

fn parent_default(declared: &Schema, table: &TableName, column: &str) -> Option<String> {
    let of = partition_of(declared, table)?;
    match &declared
        .tables
        .get(&of.parent)?
        .columns
        .get(column)?
        .value_source
    {
        ValueSource::Default(default) => Some(default.clone()),
        ValueSource::None
        | ValueSource::Identity(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => None,
    }
}

fn partition_of<'a>(
    declared: &'a Schema,
    table: &TableName,
) -> Option<&'a pbps_model::PartitionOf> {
    declared.tables.get(table)?.partition_of.as_ref()
}

fn planned(change: Change, dialect: &(impl Dialect + ?Sized)) -> PlannedChange {
    let mut part = PlannedChange::new(change);
    part.risks = dialect.change_risks(&part.change);
    part
}
