#![cfg(test)]
//! Frozen master 34b1b1db RLS settlement, before moving it into order.
use super::*;

pub(super) fn settle(
    mut cs: ChangeSet,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &dyn crate::certified_fixture::FixtureEmission,
) -> Result<ChangeSet, Refusal> {
    let mut skipped = Vec::new();
    let edges = pbps_diff::order::row_security::edges(cs.changes.iter().map(|p| &p.change));
    let releases = if dialect.name() == "postgres" {
        match facts.policy_dependents {
            Some(found) => crate::row_security::policy_releases(&cs, declared.sources.after, found)
                .map_err(Refusal::module_passes)?,
            None => {
                if cs
                    .changes
                    .iter()
                    .any(|p| crate::row_security::policy_target_uid(&p.change).is_some())
                {
                    skipped.push(Skipped::PolicyDependents);
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    if !edges.is_empty() || !releases.is_empty() {
        let mut schedule = if releases.is_empty() {
            pbps_diff::order::Schedule::new(cs.changes.len())
        } else {
            // Catalog policy edges can move a column release past a routine
            // drop or a name handover. Settle them with the existing effect,
            // name-slot and module-catalog constraints, not a positional move
            // that invents dependencies between unrelated table drops.
            policy_release_schedule(&cs, declared, facts, dialect)?
        };
        for release in &releases {
            schedule.edge(
                release.before,
                release.after,
                pbps_diff::order::Source::Catalog,
                pbps_model::resolver::OrderReason::Structural,
            );
        }
        for (before, after) in edges {
            schedule.edge(
                before,
                after,
                pbps_diff::order::Source::Effect,
                pbps_model::resolver::OrderReason::Structural,
            );
        }
        cs = schedule
            .settle(pbps_diff::order::propose(cs.clone()))
            .map_err(|why| {
                Refusal(format!(
                    "{}{}",
                    why.describe(&cs),
                    if releases.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\n{}",
                            releases
                                .iter()
                                .map(|r| r.refusal.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    }
                ))
            })?
            .into_planning()
            .changes()
            .clone();
    }
    Ok(cs)
}

fn policy_release_schedule(
    cs: &ChangeSet,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &dyn crate::certified_fixture::FixtureEmission,
) -> Result<pbps_diff::order::Schedule, Refusal> {
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
    let operations = Operations::of_plan(&cs.changes, ModuleEdits::Rebuilt);
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, declared.sources.after)
        .map_err(|why| Refusal(why.to_string()))?;
    let owners = PlanOwners::new(names, declared.sources.before.ids, declared.sources.after);
    let before = Snapshot {
        schema: declared.sources.before.schema,
        ids: declared.sources.before.ids,
    };
    let after = Snapshot {
        schema: declared.schema,
        ids: declared.sources.after,
    };
    let effects = pbps_diff::order::analyse(&operations, &owners, before, after, dialect);
    if let Some((id, why)) = effects.unowned.first() {
        return Err(Refusal(format!(
            "policy release operation {id} has unread effects: {why}"
        )));
    }
    let graph = pbps_diff::order::graph(
        operations.as_slice(),
        &effects.effects,
        &pbps_diff::order::Baseline::of(before, &owners),
        declared.module_deps,
        pbps_diff::order::Checks::ALL,
    );
    let mut schedule = pbps_diff::order::Schedule::of_graph(&graph, operations.as_slice());
    for edge in
        pbps_diff::order::generated::constraints(cs, declared.schema, declared.module_deps, dialect)
    {
        schedule.edge(edge.before, edge.after, edge.source, edge.reason);
    }
    let slots = pbps_diff::order::slots::analyse(operations.as_slice(), &owners, before, dialect);
    if !slots.unowned.is_empty() || !slots.unlowered.is_empty() {
        return Err(Refusal(
            "policy release has unread name-slot effects".into(),
        ));
    }
    pbps_diff::order::slots::schedule(
        &mut schedule,
        &pbps_diff::order::slots::edges(&slots, None, &[]),
    );
    if let Some(found) = facts.module_dependents {
        for edge in
            super::super::module_order(cs, found, declared.schema, declared.sources, dialect)
                .map_err(Refusal::module_passes)?
        {
            let (
                pbps_diff::order::expand::Step::Plan(before),
                pbps_diff::order::expand::Step::Plan(after),
            ) = (edge.before, edge.after)
            else {
                return Err(Refusal(
                    "module catalog edge names an unexpanded operation".into(),
                ));
            };
            schedule.edge(before, after, edge.source, edge.reason);
        }
    }
    Ok(schedule)
}

pub(super) fn same(
    actual: &Result<PlanningPlan, Refusal>,
    expected: &Result<ChangeSet, Refusal>,
    dialect: &dyn crate::certified_fixture::FixtureEmission,
) {
    match (actual, expected) {
        (Ok(now), Ok(old)) => {
            now.proof().validate(now.changes()).unwrap();
            assert_eq!(now.origins().len(), now.changes().changes.len());
            super::super::split_oracle::same_plan(now.changes(), old, dialect);
        }
        (Err(now), Err(old)) => assert_eq!(now, old),
        _ => panic!("the frozen RLS settlement and order disagree: {actual:?} / {expected:?}"),
    }
}
