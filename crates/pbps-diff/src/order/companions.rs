//! Existing companion placement and partition splitting. The ordering
//! boundary owns these writes; callers supply declarations and lexical facts.
use pbps_dialect::Dialect;
use pbps_model::{Change, ColumnRef, ModuleId, PlannedChange, Schema, TableName, ValueSource};
use std::collections::BTreeSet;

/// Removed companions retain their exact source mapping until restoration.
pub struct PublicDecisions(
    Vec<(
        PlannedChange,
        pbps_model::operation::Origin,
        Option<pbps_model::operation::Operation>,
    )>,
);

pub fn take_public_execution(cs: &mut super::PlanningPlan) -> PublicDecisions {
    if let Some(identities) = &cs.identities {
        cs.allocator.reserve(identities);
    }
    let tracked = cs.identities.is_some();
    let identities = cs
        .identities
        .take()
        .map(|ops| ops.iter().cloned().map(Some).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![None; cs.changes.len()]);
    assert_eq!(identities.len(), cs.changes.len());
    let (decisions, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut cs.payload.changes)
        .into_iter()
        .zip(std::mem::take(&mut cs.origins))
        .zip(identities)
        .map(|((p, origin), identity)| (p, origin, identity))
        .partition(|(p, _, _)| matches!(p.change, Change::PublicExecution { .. }));
    let mut retained = Vec::new();
    for (step, origin, identity) in rest {
        cs.payload.changes.push(step);
        cs.origins.push(origin);
        if tracked {
            retained.push(identity.expect("tracked step retains its event"));
        }
    }
    if tracked {
        cs.identities = Some(pbps_model::operation::Operations::from_operations(retained));
    }
    cs.invalidate();
    PublicDecisions(decisions)
}

pub fn settle_public_execution(cs: &mut super::PlanningPlan, decisions: PublicDecisions) {
    for (decision, origin, identity) in decisions.0 {
        let Change::PublicExecution { routine, .. } = &decision.change else {
            insert_identity(cs, cs.changes.len(), identity);
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
        insert_identity(cs, at, identity);
        cs.payload.changes.insert(at, decision);
        cs.origins.insert(at, origin);
    }
    cs.invalidate();
}

fn insert_identity(
    cs: &mut super::PlanningPlan,
    at: usize,
    identity: Option<pbps_model::operation::Operation>,
) {
    if let Some(operations) = &cs.identities {
        let mut retained = operations.iter().cloned().collect::<Vec<_>>();
        retained.insert(
            at,
            identity.expect("a held tracked step returns with its event"),
        );
        cs.identities = Some(pbps_model::operation::Operations::from_operations(retained));
    }
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
    let sets: Vec<_> = sets
        .into_iter()
        .rev()
        .map(|(at, change, origin)| (at, planned(change, dialect), origin))
        .collect();
    if let Some(identities) = &cs.identities {
        let prior = identities.with_steps(&cs.payload.changes);
        cs.allocator.reserve(&prior);
        let added = cs.allocator.allocate(
            sets.iter()
                .map(|(_, p, origin)| (origin.clone(), p.clone())),
            if cs.boundaries.is_empty() {
                pbps_model::operation::ModuleEdits::InPlace
            } else {
                cs.boundaries.module_edits()
            },
        );
        cs.identities = Some(prior);
        for ((after, _, _), identity) in sets.iter().zip(added.iter()) {
            insert_identity(cs, after + 1, Some(identity.clone()));
        }
    }
    for (after, part, origin) in sets {
        cs.payload.changes.insert(after + 1, part);
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

#[cfg(test)]
mod identity_tests {
    use super::*;
    use pbps_model::operation::{ModuleEdits, Operations, Origin, Source};
    use pbps_model::{ChangeSet, PublicAccess, RoutineOrigin};
    fn public(name: &str) -> PlannedChange {
        let ModuleId::Routine(routine) = format!("app.{name}()").parse().unwrap() else {
            panic!()
        };
        PlannedChange::new(Change::PublicExecution {
            routine,
            access: PublicAccess::Revoked,
            origin: RoutineOrigin::Rebuilt,
        })
    }
    #[test]
    fn held_public_events_reserve_ids_even_when_no_operations_remain_visible() {
        let rebinding = Source::Rebinding(pbps_model::resolver::Surface::Module(
            "app.g()".parse().unwrap(),
        ));
        let origins = vec![Origin::Whole(0), Origin::Restoration(rebinding.clone())];
        let mut plan = crate::order::propose(ChangeSet {
            changes: vec![public("f"), public("g")],
        });
        plan.origins = origins;
        let original = Operations::expanded(
            plan.logical,
            plan.origins
                .iter()
                .cloned()
                .zip(plan.changes.iter().cloned()),
            ModuleEdits::Rebuilt,
        );
        plan.identities = Some(original.clone());
        let held = take_public_execution(&mut plan);
        assert!(plan.changes.is_empty());
        assert!(plan.identities.as_ref().unwrap().is_empty());
        let settled = crate::order::Schedule::new(0).settle(plan).unwrap();
        let plan: crate::order::PlanningPlan = crate::order::plan::state(settled.changes);
        let mut plan =
            crate::order::writebacks::PlanningInput::new(vec![public("h")]).append_to(plan);
        let first = plan.identities.as_ref().unwrap().as_slice()[0].clone();
        let extra = plan.allocator.allocate(
            [(Origin::Restoration(rebinding), public("i"))],
            ModuleEdits::Rebuilt,
        );
        let added =
            Operations::from_operations(std::iter::once(first).chain(extra.iter().cloned()));
        plan.payload.changes = added.iter().map(|o| o.planned().clone()).collect();
        plan.origins = added.iter().map(|o| o.origin().clone()).collect();
        plan.identities = Some(added.clone());
        settle_public_execution(&mut plan, held);
        let done = plan.identities.as_ref().unwrap();
        let ids: BTreeSet<_> = done.iter().map(|o| o.id()).collect();
        assert_eq!(ids.len(), 4, "held and new events must never share an ID");
        for op in original.iter() {
            let retained = done
                .iter()
                .find(|candidate| candidate.id() == op.id())
                .unwrap();
            assert_eq!(retained.planned(), op.planned());
            assert_eq!(retained.logical_origin(), op.logical_origin());
        }
        assert!(added.as_slice()[0].id().source() > original.as_slice()[1].id().source());
        assert_eq!(
            added.as_slice()[1].id().source(),
            original.as_slice()[1].id().source()
        );
        assert!(added.as_slice()[1].id().part() > original.as_slice()[1].id().part());
        assert_eq!(done.len(), plan.changes.len());
    }
    #[test]
    fn untracked_public_companions_keep_the_existing_untracked_route() {
        let mut plan = crate::order::propose(ChangeSet {
            changes: vec![public("f")],
        });
        let expected = plan.changes().clone();
        let held = take_public_execution(&mut plan);
        assert!(plan.identities.is_none());
        settle_public_execution(&mut plan, held);
        assert_eq!(plan.changes(), &expected);
        assert!(plan.identities.is_none());
    }
}
