#![cfg(test)]
use super::*;

pub(super) fn weave(
    cs: &mut ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
    sources: Sources<'_>,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<usize, String> {
    crate::test_planning(cs, |plan| {
        super::weave(plan, found, declared, sources, dialect)
    })
}
pub(super) fn split_new_tables(
    cs: &mut ChangeSet,
    ids: &[&IdsFile],
    deps: &ModuleDeps,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<usize, String> {
    crate::test_planning(cs, |plan| super::split_new_tables(plan, ids, deps, dialect))
}
pub(super) fn before_its_partitions_indexes(
    cs: &mut ChangeSet,
    declared: &Schema,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<(), String> {
    crate::test_planning(cs, |plan| {
        super::before_its_partitions_indexes(plan, declared, dialect)
    })
}
pub(super) fn after_the_rows_that_leave(
    cs: &mut ChangeSet,
    declared: &Schema,
) -> Result<(), String> {
    crate::test_planning(cs, |plan| super::after_the_rows_that_leave(plan, declared))
}
pub(super) fn after_their_parents_defaults(
    cs: &mut ChangeSet,
    declared: &Schema,
    ids: &[&IdsFile],
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<usize, String> {
    crate::test_planning(cs, |plan| {
        super::after_their_parents_defaults(plan, declared, ids, dialect)
    })
}
pub(super) fn after_the_rebuilds(
    cs: &mut ChangeSet,
    released: &BTreeSet<ColumnRef>,
    deps: &ModuleDeps,
    declared: &pbps_model::Schema,
    schedule: Schedule,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) -> Result<usize, String> {
    crate::test_planning(cs, |plan| {
        super::after_the_rebuilds(plan, released, deps, declared, schedule, dialect)
    })
}
pub(super) fn split_moving_defaults(
    cs: &mut ChangeSet,
    declared: &Schema,
    dialect: &dyn Dialect<Plan = dyn pbps_diff::order::PlanView>,
) {
    crate::test_planning(cs, |plan| {
        super::split_moving_defaults(plan, declared, dialect)
    })
}
pub(super) fn after_their_functions(
    cs: &mut ChangeSet,
    deps: &ModuleDeps,
    partitions: &Partitions,
) -> Result<usize, String> {
    crate::test_planning(cs, |plan| {
        super::after_their_functions(plan, deps, partitions)
    })
}

pub(super) fn take_public_execution(cs: &mut ChangeSet) -> Vec<PlannedChange> {
    let observed = cs
        .changes
        .iter()
        .filter(|p| matches!(p.change, Change::PublicExecution { .. }))
        .cloned()
        .collect();
    crate::test_planning(cs, |plan| {
        super::take_public_execution(plan);
    });
    observed
}
pub(super) fn settle_public_execution(cs: &mut ChangeSet, decisions: Vec<PlannedChange>) {
    // Reconstitute logical fixture input after its explicit raw draft edits;
    // no opaque token from a different candidate is reused.
    let mut input = cs.clone();
    input.changes.extend(decisions);
    let mut plan = pbps_diff::order::propose(input);
    let decisions = super::take_public_execution(&mut plan);
    super::settle_public_execution(&mut plan, decisions);
    *cs = plan.changes().clone();
}
