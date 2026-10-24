//! The one entry point that finishes a plan's order after the differ sorted it
//! (#1955, DEC-1955.1).
//!
//! Every place that shows or runs a plan used to remember which reordering
//! passes to run. `plan --db` ran them all, bootstrap re-ran three, and the
//! offline preview ran none, so the preview of a new table whose default calls
//! a new function put the table first (#1598). [`schedule`] holds the list
//! once: a caller gives it the plan, what the declarations say, and whatever
//! it has read from a catalog, and gets back the plan in the order `plan --db`
//! would give it from the same facts.
//!
//! This module adapts CLI facts to `pbps_diff::order::deployment::schedule`,
//! which owns the pure passes and completed-plan authority (DEC-1959.5).
//! The former CLI implementation remains only as a frozen test oracle.
//!
//! This changes which call sites order a plan, not how a plan is ordered: the
//! passes run in `plan --db`'s order, and a caller that supplies the same
//! facts gets the order it always got.

#[cfg(test)]
use crate::certified_fixture::FixtureEmission as _;

use pbps_diff::order::PlanningPlan;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::fmt;

#[cfg(test)]
use pbps_dialect::Dialect;
#[cfg(test)]
use pbps_model::{Change, ChangeSet};
use pbps_model::{ModuleDeps, ModuleId, Schema};
use pbps_pg::modules::Dependent;

use super::Sources;
#[cfg(test)]
use super::{
    after_the_rebuilds, after_the_rows_that_leave, after_their_parents_defaults,
    before_its_partitions_indexes, dropped_modules, released, settle_public_execution,
    split_moving_defaults, split_new_tables, standing_partitions, take_public_execution, weave,
};

/// What the declarations and the baseline say. None of it needs a database, so
/// every caller has it, the offline preview included.
#[derive(Clone, Copy)]
pub(crate) struct Declarations<'a> {
    /// The declared schema.
    pub schema: &'a Schema,
    /// The baseline the plan departs from and the identities the plan ends in.
    pub sources: Sources<'a>,
    /// The `depends_on:` edges the identifier scan cannot see.
    pub module_deps: &'a ModuleDeps,
}

/// What a module drop's catalog dependents are, per dropped module, deepest
/// first (`modules::dependents`).
pub(crate) type ModuleDependents = BTreeMap<ModuleId, Vec<Dependent>>;

/// The members of each role the plan drops, by role name.
pub(crate) type RoleMembers = BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>;

/// What a caller has read from a catalog. A fact is `Some` once it was read,
/// even when what was read is empty; `None` means nobody asked. The two never
/// share a representation, so a pass cannot take a database that was never
/// consulted for one that holds nothing. `Facts::default()` is the offline
/// case: no catalog, so no fact.
#[derive(Clone, Copy, Default)]
pub(crate) struct Facts<'a> {
    module_dependents: Option<&'a ModuleDependents>,
    role_members: Option<&'a RoleMembers>,
    policy_dependents: Option<&'a crate::row_security::PolicyDependents>,
}

impl<'a> Facts<'a> {
    /// The dependents of every module the plan drops or rebuilds, as read.
    pub(crate) fn with_module_dependents(self, found: &'a ModuleDependents) -> Self {
        Self {
            module_dependents: Some(found),
            ..self
        }
    }

    /// Exact policies on each edited column, including successful empty reads.
    pub(crate) fn with_policy_dependents(
        self,
        policies: &'a crate::row_security::PolicyDependents,
    ) -> Self {
        Self {
            policy_dependents: Some(policies),
            ..self
        }
    }

    /// The members of every role the plan drops, as read. A dropped role the
    /// map does not name has none.
    pub(crate) fn with_role_members(self, members: &'a RoleMembers) -> Self {
        Self {
            role_members: Some(members),
            ..self
        }
    }
}

#[cfg(test)]
use pbps_diff::order::deployment::schedule::Skipped;

/// The plan in its final order, and what it took to get there.
#[cfg(test)]
#[derive(Debug)]
struct OracleScheduled {
    pub changes: PlanningPlan,
    /// Changes the passes added: a dependent put back around a module, a
    /// partition's own default set again.
    pub added: usize,
    /// Parts split out of a table the plan creates.
    pub split: usize,
    /// Additions moved after the function they call.
    pub moved: usize,
    /// Passes that needed a fact the caller did not have.
    pub skipped: Vec<Skipped>,
}

/// Why no order performs the plan, or why it could not be formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal(String);

impl Refusal {
    /// A refusal of the PostgreSQL module passes, worded as `plan --db` always
    /// worded it.
    #[cfg(test)]
    fn module_passes(why: impl fmt::Display) -> Self {
        Self(format!("module_dependents (PostgreSQL): {why}"))
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// Finishes the order of `changes`: every pass that reorders a plan after the
/// differ, in the order `plan --db` runs them. A pass that needs a fact `facts`
/// does not hold is skipped and named in [`Scheduled::skipped`]; it is never
/// run over an empty read.
///
/// The plan is taken and returned rather than edited, so a refusal leaves no
/// half-ordered plan behind.
pub(crate) fn schedule<S: pbps_diff::order::RouteState<D>, D: crate::emission::Engine>(
    cs: PlanningPlan<S>,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &D,
) -> Result<pbps_diff::order::deployment::schedule::Scheduled<S, D>, Refusal> {
    use pbps_diff::order::deployment as owned;
    let declarations = owned::schedule::Declarations {
        schema: declared.schema,
        sources: owned::Sources {
            before: declared.sources.before,
            after: declared.sources.after,
        },
        module_deps: declared.module_deps,
    };
    let mut read = owned::schedule::Facts::default();
    if let Some(found) = facts.module_dependents {
        read = read.with_module_dependents(found);
    }
    if let Some(found) = facts.role_members {
        read = read.with_role_members(found);
    }
    if let Some(found) = facts.policy_dependents {
        read = read.with_policy_dependents(found);
    }
    #[cfg(test)]
    let expected = oracle_schedule(
        pbps_diff::order::propose(cs.changes().clone()),
        declared,
        facts,
        dialect,
    );
    let actual = owned::schedule::schedule(cs, &declarations, &read, dialect)
        .map_err(|why| Refusal(why.to_string()));
    #[cfg(test)]
    match (&actual, &expected) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(actual.changes.changes(), expected.changes.changes());
            assert_eq!(
                (actual.added, actual.split, actual.moved),
                (expected.added, expected.split, expected.moved)
            );
            assert_eq!(
                actual
                    .skipped
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                expected
                    .skipped
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
        }
        (Err(actual), Err(expected)) => assert_eq!(actual, expected),
        _ => panic!("canonical route and frozen schedule disagree: {actual:?} / {expected:?}"),
    }
    actual
}

#[cfg(test)]
pub(crate) fn schedule_fixture<S: pbps_diff::order::RouteState<dyn Dialect>>(
    cs: PlanningPlan<S>,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &(dyn crate::certified_fixture::FixtureEmission + 'static),
) -> Result<pbps_diff::order::deployment::schedule::Scheduled<S, dyn Dialect>, Refusal> {
    use pbps_diff::order::deployment as owned;
    let declarations = owned::schedule::Declarations {
        schema: declared.schema,
        sources: owned::Sources {
            before: declared.sources.before,
            after: declared.sources.after,
        },
        module_deps: declared.module_deps,
    };
    let mut read = owned::schedule::Facts::default();
    if let Some(found) = facts.module_dependents {
        read = read.with_module_dependents(found);
    }
    if let Some(found) = facts.role_members {
        read = read.with_role_members(found);
    }
    if let Some(found) = facts.policy_dependents {
        read = read.with_policy_dependents(found);
    }
    #[cfg(test)]
    let expected = oracle_schedule(
        pbps_diff::order::propose(cs.changes().clone()),
        declared,
        facts,
        dialect,
    );
    let actual = owned::schedule::schedule_semantic(cs, &declarations, &read, dialect)
        .map_err(|why| Refusal(why.to_string()));
    #[cfg(test)]
    match (&actual, &expected) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(actual.changes.changes(), expected.changes.changes());
            assert_eq!(
                (actual.added, actual.split, actual.moved),
                (expected.added, expected.split, expected.moved)
            );
            assert_eq!(
                actual
                    .skipped
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                expected
                    .skipped
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
        }
        (Err(actual), Err(expected)) => assert_eq!(actual, expected),
        _ => panic!("canonical route and frozen schedule disagree: {actual:?} / {expected:?}"),
    }
    actual
}

#[cfg(test)]
fn oracle_schedule(
    mut cs: PlanningPlan,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &dyn crate::certified_fixture::FixtureEmission,
) -> Result<OracleScheduled, Refusal> {
    let (mut added, mut split, mut moved) = (0, 0, 0);
    let mut skipped = Vec::new();
    if dialect.name() == "postgres" {
        // Out of the way of the reordering passes, and back beside each
        // routine's final `CREATE` once they are done (#687).
        let decisions = take_public_execution(&mut cs);
        match facts.module_dependents {
            Some(found) => {
                added += weave(&mut cs, found, declared.schema, declared.sources, dialect)
                    .map_err(Refusal::module_passes)?;
            }
            // Nothing to weave without a module drop; with one, its
            // dependents are the catalog's to name.
            None if !dropped_modules(&cs).is_empty() => skipped.push(Skipped::Weave),
            None => {}
        }
        let ids = [declared.sources.after, declared.sources.before.ids];
        split += split_new_tables(&mut cs, &ids, declared.module_deps, dialect)
            .map_err(Refusal::module_passes)?;
        let releases = match facts.module_dependents {
            Some(found) => {
                let partitions = standing_partitions(declared.schema, &cs);
                let view = declared.sources.project(&cs.changes).map_err(Refusal)?;
                released(&cs, found, &partitions, &view)
            }
            // `weave` did not run, so no module drop was placed after the
            // change that releases a column: none is released.
            None => BTreeSet::new(),
        };
        // A create that moves rows out of a DEFAULT is not held with the rest
        // (#1763 review).
        split_moving_defaults(&mut cs, declared.schema, dialect);
        let mut handovers = pbps_diff::order::Schedule::new(cs.changes.len());
        if declared
            .sources
            .before
            .schema
            .modules
            .values()
            .any(|m| dialect.declares_a_constraint(m))
        {
            let slots = pbps_diff::order::slots::handovers(
                &cs.changes,
                declared.sources.before,
                pbps_diff::Side {
                    schema: declared.schema,
                    ids: declared.sources.after,
                },
                dialect,
                &Default::default(),
            )
            .map_err(Refusal)?;
            // Only the new constraint-trigger handovers join this existing
            // settle. Every other slot keeps its current ordering route;
            // without a handover the old pass is byte-identical (DEC-2100.1).
            for edge in slots.constraints() {
                if matches!(
                    &cs.changes[edge.before].change,
                    Change::AlterModule {
                        id: ModuleId::Trigger { .. },
                        ..
                    } | Change::DropModule {
                        id: ModuleId::Trigger { .. },
                        ..
                    }
                ) && matches!(
                    &cs.changes[edge.after].change,
                    Change::AddCheck { .. }
                        | Change::AddForeignKey { .. }
                        | Change::AddUnique { .. }
                        | Change::SetPrimaryKey { to: Some(_), .. }
                        | Change::CreateTable { .. }
                        | Change::AttachPartition { .. }
                ) {
                    handovers.edge(edge.before, edge.after, edge.source, edge.reason);
                }
            }
            keep_handover_dependents(&cs, declared, dialect, &mut handovers)?;
        }
        moved += after_the_rebuilds(
            &mut cs,
            &releases,
            declared.module_deps,
            declared.schema,
            handovers,
            dialect,
        )
        .map_err(Refusal::module_passes)?;
        // A partition's own default after its parent's, wherever the passes
        // above left that (#1588).
        added += after_their_parents_defaults(&mut cs, declared.schema, &ids, dialect)
            .map_err(Refusal::module_passes)?;
        // And a parent's new index ahead of the partition indexes it could
        // take (#1745 review).
        before_its_partitions_indexes(&mut cs, declared.schema, dialect)
            .map_err(Refusal::module_passes)?;
        // And a partition's drop after the rows that stop referencing it
        // (#1553).
        after_the_rows_that_leave(&mut cs, declared.schema).map_err(Refusal::module_passes)?;
        settle_public_execution(&mut cs, decisions);
    }
    // The differ sorted the role drops before it could know who holds whom;
    // once the members are in, a role holding another dropped role goes
    // first, or its `DROP MEMBER` names a principal already gone (DECISIONS
    // 127, 139). This pass moves only the drops, among their own places, so
    // it commutes with the rest.
    if cs
        .changes
        .iter()
        .any(|p| matches!(p.change, Change::DropRole { .. }))
    {
        match facts.role_members {
            Some(members) => {
                pbps_diff::order::writebacks::role_members(&mut cs, members);
                cs = pbps_diff::order::role_drops(cs).map_err(Refusal)?;
            }
            None => skipped.push(Skipped::RoleDrops),
        }
    }
    cs = pbps_diff::order::before_rewrites(
        cs,
        declared.sources.before,
        pbps_diff::Side {
            schema: declared.schema,
            ids: declared.sources.after,
        },
        dialect,
        declared.module_deps,
    )
    .map_err(Refusal)?;
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
        #[cfg(test)]
        let old = rls_oracle::settle(cs.changes().clone(), declared, facts, dialect);
        let done = (|| {
            let schedule = if releases.is_empty() {
                pbps_diff::order::Schedule::new(cs.changes.len())
            } else {
                // Preserve the existing combined graph and its refusal priority.
                policy_release_schedule(&cs, declared, facts, dialect)?
            };
            pbps_diff::order::row_security::settle(cs.clone(), schedule, &releases).map_err(Refusal)
        })();
        #[cfg(test)]
        rls_oracle::same(&done, &old, dialect);
        cs = done?;
    }
    // The complete typed plan proves which additions finish a newly
    // generated table, even if an older lowering added them separately.
    pbps_diff::order::writebacks::creation_risks(&mut cs, declared.sources.after, dialect);
    // The verifier runs on the order this entry point settles, in tests and
    // debug builds (#1954, DEC-1954.6). On a connected SQL Server plan the
    // computed-column pass still follows, so that pair is recorded and not
    // failed here; `deploy` checks the order that runs.
    // Debug assertions alone, not `cfg(test)`: `pbps-diff` is compiled
    // without its own `cfg(test)` here, so a release test build has no
    // observer to call.
    #[cfg(debug_assertions)]
    pbps_diff::order::shadow::observe_scheduled(
        declared.sources.before,
        pbps_diff::Side {
            schema: declared.schema,
            ids: declared.sources.after,
        },
        dialect,
        declared.module_deps,
        &cs,
        pbps_diff::order::shadow::Stage::Differ,
        None,
    );
    Ok(OracleScheduled {
        changes: cs,
        added,
        split,
        moved,
        skipped,
    })
}

/// A sinking claimer must take its dependents with it. Read those edges from
/// the effects rather than listing FK/key pairs: the same requirement can
/// belong to another operation, or itself have readers (DEC-2100.3).
#[cfg(test)]
fn keep_handover_dependents(
    cs: &ChangeSet,
    declared: &Declarations<'_>,
    dialect: &dyn crate::certified_fixture::FixtureEmission,
    handovers: &mut pbps_diff::order::Schedule,
) -> Result<(), Refusal> {
    use pbps_diff::order::{Schedule, edges, shadow};
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};

    let mut pending: BTreeSet<_> = handovers
        .constraints()
        .iter()
        .filter(|edge| edge.after < edge.before)
        .map(|edge| edge.after)
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let operations = Operations::of_plan(&cs.changes, ModuleEdits::Rebuilt);
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, declared.sources.after)
        .map_err(|error| Refusal(error.to_string()))?;
    let owners = PlanOwners::new(names, declared.sources.before.ids, declared.sources.after);
    let before = Snapshot {
        schema: declared.sources.before.schema,
        ids: declared.sources.before.ids,
    };
    let after = Snapshot {
        schema: declared.schema,
        ids: declared.sources.after,
    };
    let effects = shadow::analyse(&operations, &owners, before, after, dialect);
    if let Some((_, error)) = effects.unowned.first() {
        return Err(Refusal(error.to_string()));
    }
    let graph = edges::graph(
        operations.as_slice(),
        &effects.effects,
        &edges::Baseline::of(before, &owners),
        declared.module_deps,
        edges::Checks::ALL,
    );
    let dependencies = Schedule::of_graph(&graph, operations.as_slice());
    let mut seen = BTreeSet::new();
    while let Some(before) = pending.pop_first() {
        if !seen.insert(before) {
            continue;
        }
        for edge in dependencies
            .constraints()
            .iter()
            .filter(|e| e.before == before)
        {
            handovers.edge(edge.before, edge.after, edge.source, edge.reason);
            pending.insert(edge.after);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../pbps-diff/tests/support/role_order.rs"]
mod role_order_oracle;

/// Existing structural constraints remain true when catalog policy releases
/// require a different teardown order. This graph is used only for that case.
#[cfg(test)]
fn policy_release_schedule(
    cs: &PlanningPlan,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &dyn crate::certified_fixture::FixtureEmission,
) -> Result<pbps_diff::order::Schedule, Refusal> {
    let mut schedule = pbps_diff::order::row_security::release_schedule(
        cs,
        declared.sources.before,
        pbps_diff::Side {
            schema: declared.schema,
            ids: declared.sources.after,
        },
        declared.module_deps,
        dialect,
    )
    .map_err(Refusal)?;
    if let Some(found) = facts.module_dependents {
        let edges = super::module_order(cs, found, declared.schema, declared.sources, dialect)
            .map_err(Refusal::module_passes)?;
        pbps_diff::order::row_security::module_constraints(&mut schedule, edges)
            .map_err(Refusal)?;
    }
    Ok(schedule)
}

#[cfg(test)]
#[path = "rls_oracle.rs"]
mod rls_oracle;

#[cfg(test)]
mod tests {
    use super::schedule_fixture as schedule;
    use super::*;
    use pbps_model::{
        Column, ColumnRef, Generated, IdsFile, Module, ModuleKind, PlannedChange, Table, TableName,
        Uid, UidKind, ValueSource,
    };
    use pbps_pg::modules::Holds;

    fn pg() -> pbps_pg::Postgres {
        pbps_pg::Postgres::new()
    }

    fn id(s: &str) -> ModuleId {
        s.parse().unwrap()
    }

    fn function(definition: &str) -> Module {
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: definition.into(),
        }
    }

    fn column(source: ValueSource) -> Column {
        let mut column = Column::new("integer".parse().unwrap());
        column.value_source = source;
        column
    }

    /// Two new functions and three new tables: one whose default calls the
    /// first, one whose generated column calls the second, and one that calls
    /// nothing.
    fn declarations() -> (Schema, IdsFile) {
        let mut schema = Schema::default();
        schema.modules.insert(
            id("app.zz()"),
            function("() RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT 1 $$"),
        );
        schema.modules.insert(
            id("app.zz_double(integer)"),
            function("(i integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT i * 2 $$"),
        );
        let mut ids = IdsFile::default();
        for (name, extra) in [
            (
                "dflt",
                Some(("note", ValueSource::Default("app.zz()".into()))),
            ),
            (
                "gen",
                Some((
                    "g",
                    ValueSource::Generated(Generated {
                        expression: "app.zz_double(id)".into(),
                        stored: true,
                    }),
                )),
            ),
            ("a_plain", None),
        ] {
            let table = TableName::new("app", name);
            let mut shape = Table::default();
            shape.columns.insert("id".into(), column(ValueSource::None));
            ids.columns.insert(
                Uid::derived(UidKind::Column, &format!("app.{name}.id"), 0),
                ColumnRef::new(table.clone(), "id"),
            );
            if let Some((extra, source)) = extra {
                shape.columns.insert(extra.into(), column(source));
                ids.columns.insert(
                    Uid::derived(UidKind::Column, &format!("app.{name}.{extra}"), 0),
                    ColumnRef::new(table.clone(), extra),
                );
            }
            ids.tables.insert(
                Uid::derived(UidKind::Table, &format!("app.{name}"), 0),
                table.clone(),
            );
            schema.tables.insert(table, shape);
        }
        (schema, ids)
    }

    fn differ_order(schema: &Schema, ids: &IdsFile) -> pbps_diff::order::DiffPlan {
        pbps_diff::diff(
            pbps_diff::Side {
                schema: &Schema::default(),
                ids: &IdsFile::default(),
            },
            pbps_diff::Side { schema, ids },
            &pg(),
            &pbps_model::Hints::default(),
        )
        .unwrap()
    }

    fn scheduled<S: pbps_diff::order::RouteState<dyn Dialect>>(
        cs: PlanningPlan<S>,
        schema: &Schema,
        ids: &IdsFile,
        facts: &Facts<'_>,
    ) -> Result<pbps_diff::order::deployment::schedule::Scheduled<S, dyn Dialect>, Refusal> {
        schedule(
            cs,
            &Declarations {
                schema,
                sources: Sources {
                    before: pbps_diff::Side {
                        schema: &Schema::default(),
                        ids: &IdsFile::default(),
                    },
                    after: ids,
                },
                module_deps: &ModuleDeps::new(),
            },
            facts,
            &pg(),
        )
    }

    #[test]
    fn policy_owner_drops_preserve_column_name_handovers_and_distinguish_unread_facts() {
        let input = TableName::new("app", "input");
        let renamed = TableName::new("app", "renamed");
        let owner = TableName::new("app", "owner");
        let input_uid = Uid::derived(UidKind::Table, "input", 0);
        let owner_uid = Uid::derived(UidKind::Table, "owner", 0);
        let replacement_uid = Uid::derived(UidKind::Table, "replacement", 0);
        let target_uid = Uid::derived(UidKind::Column, "target", 0);
        let mut before = Schema::default();
        before.tables.insert(
            input.clone(),
            Table {
                columns: [("target".into(), column(ValueSource::None))].into(),
                ..Default::default()
            },
        );
        before.tables.insert(
            owner.clone(),
            Table {
                row_policies: [(
                    "cross_table".into(),
                    pbps_model::RowPolicy {
                        using: Some("EXISTS(SELECT FROM app.input WHERE target > 0)".into()),
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            },
        );
        let mut ids = IdsFile::default();
        ids.tables.insert(input_uid.clone(), input.clone());
        ids.tables.insert(owner_uid.clone(), owner.clone());
        ids.columns
            .insert(target_uid.clone(), input.column("target"));
        for retype in [false, true] {
            let edit = if retype {
                Change::AlterColumnType {
                    uid: target_uid.clone(),
                    column: input.column("target"),
                    from: "integer".parse().unwrap(),
                    to: "bigint".parse().unwrap(),
                    through_external_partitions: false,
                    from_nullable: true,
                    to_nullable: true,
                    from_collation: None,
                    to_collation: None,
                    using: None,
                }
            } else {
                Change::DropColumn {
                    uid: target_uid.clone(),
                    column: input.column("target"),
                }
            };
            let changes = ChangeSet {
                changes: vec![
                    edit,
                    Change::RenameTable {
                        uid: input_uid.clone(),
                        from: input.clone(),
                        to: renamed.clone(),
                        defaults: vec![],
                    },
                    Change::DropTable {
                        uid: owner_uid.clone(),
                        name: owner.clone(),
                        detach_from: None,
                    },
                    Change::CreateTable {
                        uid: replacement_uid.clone(),
                        name: input.clone(),
                        table: Box::default(),
                        beside_default: None,
                        moves_at_most: None,
                    },
                ]
                .into_iter()
                .map(PlannedChange::new)
                .collect(),
            };
            let mut after = Schema::default();
            let mut shape = before.tables[&input].clone();
            if retype {
                shape.columns["target"].ty = "bigint".parse().unwrap();
            } else {
                shape.columns.shift_remove("target");
            }
            after.tables.insert(renamed.clone(), shape);
            after.tables.insert(input.clone(), Table::default());
            let mut final_ids = ids.clone();
            final_ids.tables.remove(&owner_uid);
            final_ids.tables.insert(input_uid.clone(), renamed.clone());
            final_ids
                .tables
                .insert(replacement_uid.clone(), input.clone());
            if retype {
                final_ids
                    .columns
                    .insert(target_uid.clone(), renamed.column("target"));
            } else {
                final_ids.columns.remove(&target_uid);
            }
            let declared = Declarations {
                schema: &after,
                sources: Sources {
                    before: pbps_diff::Side {
                        schema: &before,
                        ids: &ids,
                    },
                    after: &final_ids,
                },
                module_deps: &ModuleDeps::new(),
            };
            let offline = schedule(
                pbps_diff::order::propose(changes.clone()),
                &declared,
                &Facts::default(),
                &pg(),
            )
            .unwrap();
            assert_eq!(offline.skipped, [Skipped::PolicyDependents]);
            assert_eq!(offline.skipped[0].id(), "plan.policy-dependents-unread");
            assert_eq!(offline.changes, changes);
            assert!(
                schedule(
                    pbps_diff::order::propose(changes.clone()),
                    &declared,
                    &Facts::default().with_policy_dependents(&BTreeMap::new()),
                    &pg()
                )
                .unwrap_err()
                .to_string()
                .contains("were not read")
            );
            let empty = [
                (target_uid.clone(), vec![]),
                (input_uid.clone(), vec![]),
                (owner_uid.clone(), vec![]),
            ]
            .into();
            let no_policy = schedule(
                pbps_diff::order::propose(changes.clone()),
                &declared,
                &Facts::default().with_policy_dependents(&empty),
                &pg(),
            )
            .unwrap();
            assert!(no_policy.skipped.is_empty());
            assert_eq!(no_policy.changes, changes);
            let found = [
                (
                    target_uid.clone(),
                    vec![pbps_pg::modules::PolicyDependent {
                        table: owner.clone(),
                        name: "cross_table".into(),
                    }],
                ),
                // The same external policy also changes its deparse when
                // the referenced table is renamed, so its owner releases
                // both catalog bindings before either edit.
                (
                    input_uid.clone(),
                    vec![pbps_pg::modules::PolicyDependent {
                        table: owner.clone(),
                        name: "cross_table".into(),
                    }],
                ),
                (owner_uid.clone(), vec![]),
            ]
            .into();
            let facts = Facts::default().with_policy_dependents(&found);
            let ordered = schedule(
                pbps_diff::order::propose(changes.clone()),
                &declared,
                &facts,
                &pg(),
            )
            .unwrap();
            assert_eq!(
                ordered
                    .changes
                    .changes
                    .iter()
                    .map(|p| &p.change)
                    .collect::<Vec<_>>(),
                [2, 0, 1, 3].map(|i| &changes.changes[i].change)
            );
            pbps_diff::order::row_security::check_releases(&ordered.changes, &final_ids, &found)
                .unwrap();
            assert!(
                pbps_diff::order::row_security::check_releases(&changes, &final_ids, &found)
                    .unwrap_err()
                    .to_string()
                    .contains("must precede")
            );
        }
    }

    #[test]
    fn whole_table_policy_chains_are_ordered_and_cycles_are_named() {
        let tables: Vec<_> = ["a", "m", "z"]
            .map(|name| TableName::new("app", name))
            .into();
        let uids: Vec<_> = ["a", "m", "z"]
            .map(|name| Uid::derived(UidKind::Table, name, 0))
            .into();
        let before = Schema {
            tables: tables
                .iter()
                .map(|t| (t.clone(), Table::default()))
                .collect(),
            ..Default::default()
        };
        let ids = IdsFile {
            tables: uids.iter().cloned().zip(tables.iter().cloned()).collect(),
            ..Default::default()
        };
        let changes = ChangeSet {
            changes: uids
                .iter()
                .zip(&tables)
                .map(|(uid, table)| {
                    PlannedChange::new(Change::DropTable {
                        uid: uid.clone(),
                        name: table.clone(),
                        detach_from: None,
                    })
                })
                .collect(),
        };
        let after = Schema::default();
        let final_ids = IdsFile::default();
        let deps = ModuleDeps::new();
        let declared = Declarations {
            schema: &after,
            sources: Sources {
                before: pbps_diff::Side {
                    schema: &before,
                    ids: &ids,
                },
                after: &final_ids,
            },
            module_deps: &deps,
        };
        let policy = |owner: usize, name: &str| pbps_pg::modules::PolicyDependent {
            table: tables[owner].clone(),
            name: name.into(),
        };
        let mut found: crate::row_security::PolicyDependents = [
            (uids[0].clone(), vec![policy(2, "reads_a")]),
            (uids[1].clone(), vec![policy(1, "own_predicate")]),
            (uids[2].clone(), vec![policy(1, "reads_z")]),
        ]
        .into();
        let ordered = schedule(
            pbps_diff::order::propose(changes.clone()),
            &declared,
            &Facts::default().with_policy_dependents(&found),
            &pg(),
        )
        .unwrap();
        assert_eq!(
            ordered
                .changes
                .changes
                .iter()
                .map(|p| p.change.table().unwrap())
                .collect::<Vec<_>>(),
            [&tables[1], &tables[2], &tables[0]]
        );
        pbps_diff::order::row_security::check_releases(&ordered.changes, &final_ids, &found)
            .unwrap();
        assert!(
            pbps_diff::order::row_security::check_releases(&changes, &final_ids, &found).is_err()
        );
        let mut retained = changes.clone();
        retained.changes.remove(1);
        let error = schedule(
            pbps_diff::order::propose(retained),
            &declared,
            &Facts::default().with_policy_dependents(&found),
            &pg(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("reads_z") && error.contains("explicitly remove or replace"),
            "{error}"
        );
        found
            .get_mut(&uids[1])
            .unwrap()
            .push(policy(0, "closes_cycle"));
        let error = schedule(
            pbps_diff::order::propose(changes),
            &declared,
            &Facts::default().with_policy_dependents(&found),
            &pg(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("wait on one another") && error.contains("closes_cycle"),
            "{error}"
        );
    }

    #[test]
    fn row_security_setup_follows_modules_after_scheduling() {
        let (mut schema, ids) = declarations();
        let table = schema
            .tables
            .get_mut(&TableName::new("app", "a_plain"))
            .unwrap();
        table.row_security.enabled = true;
        table.row_policies.insert(
            "p".into(),
            pbps_model::RowPolicy {
                using: Some("app.zz() > 0".into()),
                ..Default::default()
            },
        );
        let mut changes = differ_order(&schema, &ids).changes().clone();
        // The module passes can move suppliers after the initial class sort.
        // Start with that adverse order to pin the final scheduling boundary.
        changes.changes.sort_by_key(|p| {
            matches!(
                p.change,
                Change::CreateModule { .. } | Change::PublicExecution { .. }
            )
        });
        let result = scheduled(
            pbps_diff::order::propose(changes),
            &schema,
            &ids,
            &Facts::default(),
        )
        .unwrap()
        .changes;
        let policy = result
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::CreatePolicy { .. }))
            .unwrap();
        let flags = result
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::SetRowSecurity { .. }))
            .unwrap();
        for (at, step) in result.changes.iter().enumerate() {
            if matches!(
                step.change,
                Change::CreateModule { .. } | Change::PublicExecution { .. }
            ) {
                assert!(at < policy);
            }
        }
        assert!(policy < flags);
        let again = scheduled(result.clone(), &schema, &ids, &Facts::default()).unwrap();
        assert_eq!(again.changes, result);
    }

    // A test rendering: what each change is, in the plan's order.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn names(cs: &ChangeSet) -> Vec<String> {
        cs.changes
            .iter()
            .map(|p| match &p.change {
                Change::CreateTable { name, .. } => format!("table {name}", name = name.shown()),
                Change::CreateModule { id, .. } => format!("create {id}", id = id.shown()),
                Change::DropModule { id, .. } => format!("drop {id}", id = id.shown()),
                Change::AlterColumnDefault { column, .. } => {
                    format!("default {}.{}", column.table.shown(), column.name.shown())
                }
                Change::DropRole { name, .. } => format!(
                    "drop role {name}",
                    // Preserve the original spelling in this SQL or comparison fixture.
                    name = name.raw(),
                ),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn position(order: &[String], what: &str) -> usize {
        order
            .iter()
            .position(|n| n == what)
            .unwrap_or_else(|| panic!("{what} is not in {order:?}"))
    }

    #[test]
    fn a_deferred_key_keeps_its_dependents_without_carrying_unrelated_foreign_keys() {
        use pbps_model::{
            CheckConstraint, ForeignKey, PrimaryKey, ReplicaIdentity, UniqueConstraint,
        };

        for drop_trigger in [false, true] {
            for kind in ["unique", "primary", "check"] {
                let mut before = Schema::default();
                let mut ids = IdsFile::default();
                let t = TableName::new("app", "t");
                let c = TableName::new("app", "c");
                let r = TableName::new("app", "r");
                for table in [&t, &c, &r] {
                    let mut shape = Table::default();
                    let mut id = column(ValueSource::None);
                    id.nullable = false;
                    shape.columns.insert("id".into(), id);
                    before.tables.insert(table.clone(), shape);
                    ids.tables.insert(
                        Uid::derived(UidKind::Table, &table.shown().to_string(), 0),
                        table.clone(),
                    );
                    ids.columns.insert(
                        Uid::derived(UidKind::Column, &format!("{}.id", table.shown()), 0),
                        table.column("id"),
                    );
                }
                let key = |name: &str| PrimaryKey {
                    name: Some(name.into()),
                    columns: vec!["id".into()],
                    storage_parameters: BTreeMap::new(),
                };
                before.tables.get_mut(&r).unwrap().primary_key = Some(key("pk_r"));
                let trigger = ModuleId::Trigger {
                    on: t.clone(),
                    name: "audit".into(),
                };
                before.modules.insert(
                    id("app.f()"),
                    function("() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"),
                );
                before.modules.insert(
                    trigger.clone(),
                    Module {
                        kind: ModuleKind::Trigger,
                        description: None,
                        definition:
                            "CONSTRAINT AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()"
                                .into(),
                    },
                );
                let mut after = before.clone();
                if drop_trigger {
                    after.modules.remove(&trigger);
                } else {
                    after.modules.get_mut(&trigger).unwrap().definition =
                        "AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()".into();
                }
                let target = after.tables.get_mut(&t).unwrap();
                match kind {
                    "unique" => {
                        target.unique.insert(
                            "audit".into(),
                            UniqueConstraint {
                                columns: vec!["id".into()],
                                storage_parameters: BTreeMap::new(),
                            },
                        );
                        target.replica_identity = Some(ReplicaIdentity::Unique("audit".into()));
                    }
                    "primary" => {
                        target.primary_key = Some(key("audit"));
                        target.replica_identity = Some(ReplicaIdentity::PrimaryKey);
                    }
                    _ => {
                        target.checks.insert(
                            "audit".into(),
                            CheckConstraint {
                                expression: "id >= 0".into(),
                            },
                        );
                    }
                }
                let fk = |target| ForeignKey {
                    columns: vec!["id".into()],
                    references_table: target,
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                };
                let child = after.tables.get_mut(&c).unwrap();
                child.foreign_keys.insert(
                    "dependent".into(),
                    fk(if kind == "check" {
                        r.clone()
                    } else {
                        t.clone()
                    }),
                );
                child.foreign_keys.insert("unrelated".into(), fk(r.clone()));
                let base = pbps_diff::Side {
                    schema: &before,
                    ids: &ids,
                };
                let desired = pbps_diff::Side {
                    schema: &after,
                    ids: &ids,
                };
                let raw = pbps_diff::diff(base, desired, &pg(), &Default::default()).unwrap();
                let result = schedule(
                    raw.clone(),
                    &Declarations {
                        schema: &after,
                        sources: Sources {
                            before: base,
                            after: &ids,
                        },
                        module_deps: &ModuleDeps::new(),
                    },
                    &Facts::default(),
                    &pg(),
                )
                .unwrap();
                let changes = &result.changes.changes;
                let at = |predicate: &dyn Fn(&Change) -> bool| {
                    changes.iter().position(|p| predicate(&p.change)).unwrap()
                };
                let release = at(&|change| {
                    matches!(
                        change,
                        Change::AlterModule { .. } | Change::DropModule { .. }
                    )
                });
                let claim = at(&|change| {
                    matches!(
                        change,
                        Change::AddUnique { .. }
                            | Change::SetPrimaryKey { .. }
                            | Change::AddCheck { .. }
                    )
                });
                let dependent = at(
                    &|change| matches!(change, Change::AddForeignKey { name, .. } if name.raw() == "dependent"),
                );
                let unrelated = at(
                    &|change| matches!(change, Change::AddForeignKey { name, .. } if name.raw() == "unrelated"),
                );
                assert!(release < claim, "{kind}, drop={drop_trigger}: {changes:?}");
                if kind != "check" {
                    let identity =
                        at(&|change| matches!(change, Change::SetReplicaIdentity { .. }));
                    assert!(claim < identity, "{kind}: {changes:?}");
                }
                if drop_trigger {
                    // A drop already precedes the additions; importing its
                    // dependents must not perturb that satisfied handover.
                    assert_eq!(result.changes, raw);
                } else {
                    assert!(unrelated < release, "{kind}: {changes:?}");
                    assert_eq!(claim < dependent, kind != "check", "{kind}: {changes:?}");
                }
            }
        }
    }

    /// #1598: the differ puts a table (class 7) ahead of every function
    /// (class 14), so with no pass behind it a table calling a function is
    /// created first. Offline the schedule runs the passes that read no
    /// catalog, and what calls a function follows it.
    #[test]
    fn offline_generated_table_names_precede_functions_and_its_columns_follow_them() {
        let (schema, ids) = declarations();
        let differ = differ_order(&schema, &ids);
        let order = names(&differ);
        assert!(
            position(&order, "table app.gen") < position(&order, "create app.zz_double(integer)")
        );

        let offline = scheduled(differ.clone(), &schema, &ids, &Facts::default()).unwrap();
        let order = names(&offline.changes);
        // Names exist before the function; only the generation waits.
        let function = position(&order, "create app.zz_double(integer)");
        assert!(position(&order, "table app.gen") < function);
        assert!(
            offline
                .changes
                .changes
                .iter()
                .enumerate()
                .any(|(i, p)| i > function
                    && matches!(&p.change, Change::AddColumn { table, name, .. }
                if table.name == "gen" && name.raw() == "g"))
        );
        // A default is split out of its table and set after the function.
        assert!(position(&order, "table app.dflt") < position(&order, "create app.zz()"));
        assert!(position(&order, "create app.zz()") < position(&order, "default app.dflt.note"));
        // Negative: tables that stay in place keep the differ's order among
        // themselves, and a table that calls nothing stays ahead of the
        // function.
        let before = names(&differ);
        assert_eq!(
            position(&before, "table app.a_plain") < position(&before, "table app.dflt"),
            position(&order, "table app.a_plain") < position(&order, "table app.dflt"),
        );
        assert!(position(&order, "table app.a_plain") < position(&order, "create app.zz()"));
        assert_eq!(offline.split, 2);
        assert_eq!(offline.skipped, []);
    }

    /// The offline order is the catalog-free order of `plan --db`: the same
    /// declarations with a catalog that held no dependents are given the same
    /// plan, so the preview shows what `plan --db` would do when it has
    /// nothing to add.
    #[test]
    fn the_offline_order_equals_the_order_of_a_catalog_that_found_nothing() {
        let (schema, ids) = declarations();
        let differ = differ_order(&schema, &ids);
        let found = ModuleDependents::new();
        let offline = scheduled(differ.clone(), &schema, &ids, &Facts::default()).unwrap();
        let connected = scheduled(
            differ,
            &schema,
            &ids,
            &Facts::default().with_module_dependents(&found),
        )
        .unwrap();
        assert_eq!(offline.changes, connected.changes);
        assert_eq!(
            (offline.added, offline.split, offline.moved),
            (connected.added, connected.split, connected.moved)
        );
    }

    /// The passes as the call sites ran them before the schedule held the list
    /// (#1955): bootstrap's three, and `plan --db`'s all of them over a
    /// catalog that found nothing. Kept as the reference the schedule must
    /// reproduce, so that moving the list changed which call sites order a
    /// plan and not how a plan is ordered.
    mod before_the_schedule {
        use super::*;
        use crate::dependents::{
            after_their_parents_defaults, before_its_partitions_indexes, settle_public_execution,
            split_moving_defaults, split_new_tables, take_public_execution,
        };

        fn after_the_rebuilds(
            cs: &mut PlanningPlan,
            released: &BTreeSet<super::ColumnRef>,
            deps: &ModuleDeps,
            schema: &Schema,
            dialect: &dyn crate::certified_fixture::FixtureEmission,
        ) -> Result<usize, String> {
            let order = pbps_diff::order::Schedule::new(cs.changes.len());
            crate::dependents::after_the_rebuilds(cs, released, deps, schema, order, dialect)
        }

        pub(super) fn bootstrap(mut cs: PlanningPlan, schema: &Schema, ids: &IdsFile) -> ChangeSet {
            let decisions = take_public_execution(&mut cs);
            split_new_tables(&mut cs, &[ids], &ModuleDeps::new(), &pg()).unwrap();
            after_the_rebuilds(&mut cs, &BTreeSet::new(), &ModuleDeps::new(), schema, &pg())
                .unwrap();
            after_their_parents_defaults(&mut cs, schema, &[ids], &pg()).unwrap();
            settle_public_execution(&mut cs, decisions);
            cs.changes().clone()
        }

        pub(super) fn connected(mut cs: PlanningPlan, schema: &Schema, ids: &IdsFile) -> ChangeSet {
            let decisions = take_public_execution(&mut cs);
            let both = [ids, &IdsFile::default()];
            split_new_tables(&mut cs, &both, &ModuleDeps::new(), &pg()).unwrap();
            split_moving_defaults(&mut cs, schema, &pg());
            after_the_rebuilds(&mut cs, &BTreeSet::new(), &ModuleDeps::new(), schema, &pg())
                .unwrap();
            after_their_parents_defaults(&mut cs, schema, &both, &pg()).unwrap();
            before_its_partitions_indexes(&mut cs, schema, &pg()).unwrap();
            settle_public_execution(&mut cs, decisions);
            cs.changes().clone()
        }
    }

    /// Bootstrap and `plan --db` order a plan from the same declarations as
    /// they did before the schedule held the passes. Bootstrap now also runs
    /// the two passes it never ran, which act only on a create beside a
    /// standing DEFAULT partition and on a parent's index built after a
    /// partition's own; these declarations have neither.
    #[test]
    fn the_schedule_orders_as_bootstrap_and_plan_db_did() {
        let (schema, ids) = declarations();
        let differ = differ_order(&schema, &ids);
        let moved = before_the_schedule::bootstrap(
            pbps_diff::order::propose(differ.changes().clone()),
            &schema,
            &ids,
        );
        assert_ne!(moved, differ, "the reference moves something");
        let now = scheduled(differ.clone(), &schema, &ids, &Facts::default()).unwrap();
        assert_eq!(now.changes, moved);
        let nothing = ModuleDependents::new();
        let connected = scheduled(
            differ.clone(),
            &schema,
            &ids,
            &Facts::default().with_module_dependents(&nothing),
        )
        .unwrap();
        assert_eq!(
            connected.changes,
            before_the_schedule::connected(
                pbps_diff::order::propose(differ.changes().clone()),
                &schema,
                &ids
            )
        );
    }

    /// The schedule is stable: an order it made is not changed by making it
    /// again, so a caller that schedules twice (a re-plan) cannot move a
    /// change the first time left in place.
    #[test]
    fn scheduling_a_scheduled_plan_changes_nothing() {
        let (schema, ids) = declarations();
        let once = scheduled(
            differ_order(&schema, &ids),
            &schema,
            &ids,
            &Facts::default(),
        )
        .unwrap()
        .changes;
        let twice = scheduled(once.clone(), &schema, &ids, &Facts::default()).unwrap();
        assert_eq!(once, twice.changes);
    }

    /// A pass that needs the catalog's answer is skipped offline and named,
    /// not run over an empty read. The same plan with a catalog that found a
    /// dependent the declarations do not manage is refused by `weave`; with
    /// nothing found, it passes; with nothing asked, it is skipped. Three
    /// outcomes for three states of the fact, none of them the other's.
    #[test]
    fn a_pass_that_needs_a_catalog_fact_is_skipped_offline_and_never_run_over_an_empty_read() {
        let schema = Schema::default();
        let ids = IdsFile::default();
        let plan = || ChangeSet {
            changes: vec![PlannedChange::new(Change::DropModule {
                id: id("app.f(integer)"),
                kind: ModuleKind::Function,
            })],
        };
        let stranger = ModuleDependents::from([(
            id("app.f(integer)"),
            vec![Dependent {
                described: "view app.v".into(),
                holds: Holds::Module(id("app.v")),
            }],
        )]);
        let nothing = ModuleDependents::from([(id("app.f(integer)"), Vec::new())]);

        let offline = scheduled(
            pbps_diff::order::propose(plan()),
            &schema,
            &ids,
            &Facts::default(),
        )
        .unwrap();
        assert_eq!(offline.skipped, [Skipped::Weave]);
        assert_eq!(offline.changes, plan());

        let empty = scheduled(
            pbps_diff::order::propose(plan()),
            &schema,
            &ids,
            &Facts::default().with_module_dependents(&nothing),
        )
        .unwrap();
        assert_eq!(empty.skipped, []);

        let refused = scheduled(
            pbps_diff::order::propose(plan()),
            &schema,
            &ids,
            &Facts::default().with_module_dependents(&stranger),
        )
        .unwrap_err();
        assert!(
            refused
                .to_string()
                .starts_with("module_dependents (PostgreSQL): "),
            "{refused}"
        );
        assert!(refused.to_string().contains("app.v"), "{refused}");
    }

    /// A plan that drops or rebuilds no module has nothing for `weave` to
    /// place, so nothing is skipped and nothing is reported.
    #[test]
    fn a_plan_with_no_module_drop_skips_nothing_offline() {
        let (schema, ids) = declarations();
        let offline = scheduled(
            differ_order(&schema, &ids),
            &schema,
            &ids,
            &Facts::default(),
        )
        .unwrap();
        assert_eq!(offline.skipped, []);
    }

    /// A role drop's order depends on who holds whom, which only the catalog
    /// says. Offline the pass is skipped and named, and the drops keep the
    /// differ's order; with the members read it runs, writes them into the
    /// drops and orders the holder first.
    #[test]
    fn role_drops_are_ordered_by_membership_only_once_the_members_are_read() {
        let schema = Schema::default();
        let ids = IdsFile::default();
        let drop = |name: &str| {
            PlannedChange::new(Change::DropRole {
                // Effect analysis reads the identity history too: distinct
                // roles cannot share one synthetic identity.
                uid: Uid::derived(UidKind::Role, name, 0),
                name: name.to_owned().into(),
                members: Vec::new(),
            })
        };
        let plan = || ChangeSet {
            changes: vec![drop("member"), drop("holder")],
        };
        let sql_server = pbps_mssql::Mssql;
        let run = |facts: &Facts<'_>| {
            schedule(
                pbps_diff::order::propose(plan()),
                &Declarations {
                    schema: &schema,
                    sources: Sources {
                        before: pbps_diff::Side {
                            schema: &schema,
                            ids: &ids,
                        },
                        after: &ids,
                    },
                    module_deps: &ModuleDeps::new(),
                },
                facts,
                &sql_server,
            )
            .unwrap()
        };

        let offline = run(&Facts::default());
        assert_eq!(offline.skipped, [Skipped::RoleDrops]);
        assert_eq!(
            names(&offline.changes),
            ["drop role member", "drop role holder"]
        );

        let members = RoleMembers::from([("holder".into(), vec!["member".into()])]);
        let connected = run(&Facts::default().with_role_members(&members));
        assert_eq!(connected.skipped, []);
        assert_eq!(
            names(&connected.changes),
            ["drop role holder", "drop role member"]
        );
        // The members are written into the plan by name (ADR-0005).
        assert!(connected.changes.changes.iter().any(|p| matches!(
            &p.change,
            Change::DropRole { name, members, .. } if name.raw() == "holder" && members == &[pbps_model::RoleName::from("member")]
        )));
        let mut expected = plan();
        for p in &mut expected.changes {
            if let Change::DropRole {
                name, members: of, ..
            } = &mut p.change
            {
                *of = members.get(name).cloned().unwrap_or_default();
            }
        }
        super::role_order_oracle::legacy_order(&mut expected);
        assert_eq!(connected.changes, expected);
        assert_eq!(
            serde_json::to_vec(&connected.changes.changes()).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            super::role_order_oracle::checksum(&connected.changes),
            super::role_order_oracle::checksum(&expected)
        );
        let sql = |plan: &ChangeSet| {
            plan.changes
                .iter()
                .flat_map(|p| sql_server.fixture_emit(&p.change, p.strategy).unwrap())
                .map(|s| s.sql)
                .collect::<Vec<_>>()
        };
        assert_eq!(sql(&connected.changes), sql(&expected));

        // Read and empty: a role the read does not name holds nobody.
        let none = RoleMembers::new();
        let nobody = run(&Facts::default().with_role_members(&none));
        assert_eq!(nobody.skipped, []);
        assert_eq!(
            names(&nobody.changes),
            ["drop role member", "drop role holder"]
        );
        assert_eq!(offline.changes, plan());
        assert_eq!(nobody.changes, plan());

        // Unread also preserves any membership payload already supplied;
        // treating it as an empty read would silently erase that payload.
        let mut supplied = plan();
        if let Change::DropRole { members, .. } = &mut supplied.changes[1].change {
            members.push("member".into());
        }
        let unread = schedule(
            pbps_diff::order::propose(supplied.clone()),
            &Declarations {
                schema: &schema,
                sources: Sources {
                    before: pbps_diff::Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    after: &ids,
                },
                module_deps: &ModuleDeps::new(),
            },
            &Facts::default(),
            &sql_server,
        )
        .unwrap();
        assert_eq!(unread.changes, supplied);
        assert_eq!(unread.skipped, [Skipped::RoleDrops]);
    }

    #[test]
    fn role_drop_scripts_and_checksums_match_the_legacy_sort_for_interleaved_branches() {
        for salt in 0..32 {
            let drop = |name: &str, members: &[&str]| {
                PlannedChange::new(Change::DropRole {
                    uid: Uid::derived(UidKind::Role, name, salt),
                    name: name.into(),
                    members: members.iter().map(|m| (*m).into()).collect(),
                })
            };
            let other = |name: &str| {
                PlannedChange::new(Change::DropTable {
                    uid: Uid::derived(UidKind::Table, name, salt),
                    name: TableName::new("dbo", name),
                    detach_from: None,
                })
            };
            for reverse in [false, true] {
                let mut input = ChangeSet {
                    changes: vec![
                        drop("leaf", &["some_user"]),
                        other("first_table"),
                        drop("branch", &["leaf"]),
                        drop("other_root", &[]),
                        other("second_table"),
                        drop("root", &["branch"]),
                    ],
                };
                if reverse {
                    input.changes.reverse();
                }
                let mut expected = input.clone();
                super::role_order_oracle::legacy_order(&mut expected);
                let actual =
                    pbps_diff::order::role_drops(pbps_diff::order::propose(input)).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(
                    serde_json::to_vec(&actual.changes()).unwrap(),
                    serde_json::to_vec(&expected).unwrap()
                );
                assert_eq!(
                    super::role_order_oracle::checksum(&actual),
                    super::role_order_oracle::checksum(&expected)
                );
                let sql = |changes: &ChangeSet| {
                    changes
                        .changes
                        .iter()
                        .flat_map(|p| {
                            pbps_mssql::Mssql
                                .fixture_emit(&p.change, p.strategy)
                                .unwrap()
                        })
                        .map(|s| s.sql)
                        .collect::<Vec<_>>()
                };
                assert_eq!(sql(&actual), sql(&expected));
            }
        }
    }

    /// The passes that reorder a PostgreSQL plan are not SQL Server's: a plan
    /// for it comes back as it went in, whatever it holds.
    #[test]
    fn a_sql_server_plan_gets_no_postgresql_pass() {
        let (schema, ids) = declarations();
        let differ = differ_order(&schema, &ids);
        let sql_server = pbps_mssql::Mssql;
        let out = schedule(
            differ.clone(),
            &Declarations {
                schema: &schema,
                sources: Sources {
                    before: pbps_diff::Side {
                        schema: &Schema::default(),
                        ids: &IdsFile::default(),
                    },
                    after: &ids,
                },
                module_deps: &ModuleDeps::new(),
            },
            &Facts::default(),
            &sql_server,
        )
        .unwrap();
        assert_eq!(out.changes, differ);
        assert_eq!(out.skipped, []);
    }
}
