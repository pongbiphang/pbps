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
//! The passes themselves are private to [`super`], so the compiler refuses a
//! call from any other module of this crate. Role drops are ordered inside
//! `pbps-diff::order` once their memberships are supplied (DEC-1959.1).
//!
//! This changes which call sites order a plan, not how a plan is ordered: the
//! passes run in `plan --db`'s order, and a caller that supplies the same
//! facts gets the order it always got.

use crate::order::PlanningPlan;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::order::catalog::Dependent;
use pbps_dialect::Dialect;
use pbps_model::{Change, ChangeSet, ModuleDeps, ModuleId, Schema};

use super::{
    Sources, after_the_rebuilds, after_the_rows_that_leave, after_their_parents_defaults,
    before_its_partitions_indexes, dropped_modules, released, settle_public_execution,
    split_moving_defaults, split_new_tables, standing_partitions, take_public_execution, weave,
};

/// What the declarations and the baseline say. None of it needs a database, so
/// every caller has it, the offline preview included.
#[derive(Clone, Copy)]
pub struct Declarations<'a> {
    /// The declared schema.
    pub schema: &'a Schema,
    /// The baseline the plan departs from and the identities the plan ends in.
    pub sources: Sources<'a>,
    /// The `depends_on:` edges the identifier scan cannot see.
    pub module_deps: &'a ModuleDeps,
}

/// What a module drop's catalog dependents are, per dropped module, deepest
/// first (`modules::dependents`).
pub type ModuleDependents = BTreeMap<ModuleId, Vec<Dependent>>;

/// The members of each role the plan drops, by role name.
pub type RoleMembers = BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>;

/// What a caller has read from a catalog. A fact is `Some` once it was read,
/// even when what was read is empty; `None` means nobody asked. The two never
/// share a representation, so a pass cannot take a database that was never
/// consulted for one that holds nothing. `Facts::default()` is the offline
/// case: no catalog, so no fact.
#[derive(Clone, Copy, Default)]
pub struct Facts<'a> {
    module_dependents: Option<&'a ModuleDependents>,
    role_members: Option<&'a RoleMembers>,
    policy_dependents: Option<&'a crate::order::row_security::PolicyDependents>,
    membership_catalog: Option<&'a pbps_model::operation::MembershipCatalog>,
}

impl<'a> Facts<'a> {
    /// The dependents of every module the plan drops or rebuilds, as read.
    pub fn with_module_dependents(self, found: &'a ModuleDependents) -> Self {
        Self {
            module_dependents: Some(found),
            ..self
        }
    }

    /// Exact policies on each edited column, including successful empty reads.
    pub fn with_policy_dependents(
        self,
        policies: &'a crate::order::row_security::PolicyDependents,
    ) -> Self {
        Self {
            policy_dependents: Some(policies),
            ..self
        }
    }

    /// The stored columns of the partition families whose membership and
    /// columns change in one plan (`boundary_catalog_tables`), read in the
    /// same call and transaction as the module dependents.
    pub fn with_membership_catalog(
        self,
        stored: &'a pbps_model::operation::MembershipCatalog,
    ) -> Self {
        Self {
            membership_catalog: Some(stored),
            ..self
        }
    }

    /// What `with_membership_catalog` carried, if anyone asked.
    pub fn membership_catalog(&self) -> Option<&'a pbps_model::operation::MembershipCatalog> {
        self.membership_catalog
    }

    /// The members of every role the plan drops, as read. A dropped role the
    /// map does not name has none.
    pub fn with_role_members(self, members: &'a RoleMembers) -> Self {
        Self {
            role_members: Some(members),
            ..self
        }
    }
}

/// A pass that did not run because the fact it needs was not supplied.
/// Reported, so that an order made without a catalog is not read as one made
/// with it. Its words are the preview's, the one caller that has no catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// `weave` needs the dependents of every module the plan drops or
    /// rebuilds. With it goes the set of columns it leaves released, which is
    /// read from what it found: where it does not run, no module drop waits
    /// for a release.
    Weave,
    /// Role-drop ordering needs the members of every role the plan drops.
    RoleDrops,
    /// Referenced edits and table drops wait for policy owners the plan drops.
    PolicyDependents,
}

impl Skipped {
    /// The stable id of the note a JSON report carries for it (SPEC §9.8:
    /// offline uncertainty is the same evidence in both reports).
    pub fn id(&self) -> &'static str {
        match self {
            Self::Weave => "plan.module-dependents-unread",
            Self::RoleDrops => "plan.role-members-unread",
            Self::PolicyDependents => "plan.policy-dependents-unread",
        }
    }
}

impl fmt::Display for Skipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Weave => {
                "What depends on a dropped or rebuilt module is removed before it and put back \
                 after; `pbps plan --db` lists it for the target, this preview cannot."
            }
            Self::PolicyDependents => {
                "A policy's table drop precedes changes to tables or columns it references; `pbps plan --db` \
                 reads those policy dependencies for the target, this preview cannot."
            }
            Self::RoleDrops => {
                "A dropped role's members are removed first; `pbps plan --db` lists them for \
                 the target, this preview cannot."
            }
        })
    }
}

/// The plan in its final order, and what it took to get there.
pub struct Scheduled<
    S: crate::order::RouteState<D> = crate::order::Draft,
    D: Dialect + ?Sized = dyn Dialect,
> {
    pub changes: PlanningPlan<S::Output>,
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

impl<S: crate::order::RouteState<D>, D: Dialect + ?Sized> fmt::Debug for Scheduled<S, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduled")
            .field("changes", &self.changes)
            .field("added", &self.added)
            .field("split", &self.split)
            .field("moved", &self.moved)
            .field("skipped", &self.skipped)
            .finish()
    }
}

/// Why no order performs the plan, or why it could not be formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(String);

impl Refusal {
    /// A refusal of the PostgreSQL module passes, worded as `plan --db` always
    /// worded it.
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
pub fn schedule<S: crate::order::RouteState<D>, D: Dialect>(
    cs: PlanningPlan<S>,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &D,
) -> Result<Scheduled<S, D>, Refusal> {
    scheduled::<S, D>(cs, declared, facts, dialect)
}

/// Semantic observation cannot produce a real engine's certificate.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn schedule_semantic<S: crate::order::RouteState<dyn Dialect>>(
    cs: PlanningPlan<S>,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &(dyn Dialect + 'static),
) -> Result<Scheduled<S, dyn Dialect>, Refusal> {
    scheduled::<S, dyn Dialect>(cs, declared, facts, dialect)
}

fn scheduled<S: crate::order::RouteState<D>, D: Dialect + ?Sized>(
    cs: PlanningPlan<S>,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &dyn Dialect,
) -> Result<Scheduled<S, D>, Refusal> {
    crate::order::plan::reject_pending_rebuild(cs.changes(), cs.boundaries.rebuilt())
        .map_err(Refusal)?;
    let mut cs: PlanningPlan = crate::order::plan::state(cs);
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
        let mut handovers = crate::order::Schedule::new(cs.changes.len());
        let type_change = |change: &Change| {
            matches!(
                change,
                Change::AddEnumValue { .. }
                    | Change::RenameEnumValue { .. }
                    | Change::EnumRebuild { .. }
                    | Change::CreateType { .. }
                    | Change::DropType { .. }
                    | Change::RenameType { .. }
            )
        };
        if cs.changes.iter().any(|p| type_change(&p.change))
            || declared
                .sources
                .before
                .schema
                .modules
                .values()
                .any(|m| dialect.declares_a_constraint(m))
        {
            let slots = crate::order::slots::handovers(
                &cs.changes,
                declared.sources.before,
                crate::Side {
                    schema: declared.schema,
                    ids: declared.sources.after,
                },
                dialect,
                &Default::default(),
            )
            .map_err(Refusal)?;
            // Declared type and constraint-trigger handovers join this
            // existing settlement. Other slots keep their established route
            // (DEC-1635.3, DEC-2100.1).
            for edge in slots.constraints() {
                if type_change(&cs.changes[edge.before].change)
                    || type_change(&cs.changes[edge.after].change)
                    || (matches!(
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
                    ))
                {
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
                crate::order::writebacks::role_members(&mut cs, members);
                cs = crate::order::role_drops(cs).map_err(Refusal)?;
            }
            None => skipped.push(Skipped::RoleDrops),
        }
    }
    cs = crate::order::before_rewrites(
        cs,
        declared.sources.before,
        crate::Side {
            schema: declared.schema,
            ids: declared.sources.after,
        },
        dialect,
        declared.module_deps,
    )
    .map_err(Refusal)?;
    let edges = crate::order::row_security::edges(cs.changes.iter().map(|p| &p.change));
    let releases = if dialect.name() == "postgres" {
        match facts.policy_dependents {
            Some(found) => {
                crate::order::row_security::policy_releases(&cs, declared.sources.after, found)
                    .map_err(Refusal::module_passes)?
            }
            None => {
                if cs
                    .changes
                    .iter()
                    .any(|p| crate::order::row_security::policy_target_uid(&p.change).is_some())
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
        let done = (|| {
            let schedule = if releases.is_empty() {
                crate::order::Schedule::new(cs.changes.len())
            } else {
                // Preserve the existing combined graph and its refusal priority.
                policy_release_schedule(&cs, declared, facts, dialect)?
            };
            crate::order::row_security::settle(cs.clone(), schedule, &releases).map_err(Refusal)
        })();
        cs = done?;
    }
    // The complete typed plan proves which additions finish a newly
    // generated table, even if an older lowering added them separately.
    crate::order::writebacks::creation_risks(&mut cs, declared.sources.after, dialect);
    refuse_type_slots(&cs, declared, dialect)?;
    // The verifier runs on the order this entry point settles, in tests and
    // debug builds (#1954, DEC-1954.6). On a connected SQL Server plan the
    // computed-column pass still follows, so that pair is recorded and not
    // failed here; `deploy` checks the order that runs.
    // Debug assertions alone, not `cfg(test)`: `pbps-diff` is compiled
    // without its own `cfg(test)` here, so a release test build has no
    // observer to call.
    #[cfg(debug_assertions)]
    crate::order::shadow::observe_scheduled(
        declared.sources.before,
        crate::Side {
            schema: declared.schema,
            ids: declared.sources.after,
        },
        dialect,
        declared.module_deps,
        &cs,
        crate::order::shadow::Stage::Differ,
        None,
    );
    cs.boundaries.ensure(&cs).map_err(Refusal)?;
    Ok(Scheduled {
        changes: crate::order::plan::state::<_, S::Output>(cs),
        added,
        split,
        moved,
        skipped,
    })
}

// Class order has not settled Types handovers yet (DEC-1635.3). Replay only
// after the shared scheduler has placed releases before their claims.
fn refuse_type_slots(
    cs: &ChangeSet,
    declared: &Declarations<'_>,
    dialect: &dyn Dialect,
) -> Result<(), Refusal> {
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};

    let mode = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let operations = Operations::of_plan(&cs.changes, mode);
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, declared.sources.after)
        .map_err(|error| Refusal(error.to_string()))?;
    let owners = PlanOwners::new(names, declared.sources.before.ids, declared.sources.after);
    let before = Snapshot {
        schema: declared.sources.before.schema,
        ids: declared.sources.before.ids,
    };
    let slots = crate::order::slots::analyse(operations.as_slice(), &owners, before, dialect);
    for violation in crate::order::slots::verify(
        operations.as_slice(),
        &slots,
        dialect,
        None,
        &owners,
        before,
    )
    .0
    {
        if let crate::order::slots::SlotViolation::Contested { slot, .. }
        | crate::order::slots::SlotViolation::IntermediateHeld { slot, .. } = violation
            && let pbps_dialect::slots::Namespace::Types(schema) = slot.namespace
            && let Ok(name) = pbps_model::TypeRef::new(&schema, &slot.name)
        {
            return Err(Refusal(
                crate::DiffError::TypeLifecycleUnsupported {
                    name,
                    reason: "pg_type name is already occupied by another declaration".into(),
                }
                .to_string(),
            ));
        }
    }
    Ok(())
}

/// A sinking claimer must take its dependents with it. Read those edges from
/// the effects rather than listing FK/key pairs: the same requirement can
/// belong to another operation, or itself have readers (DEC-2100.3).
fn keep_handover_dependents(
    cs: &ChangeSet,
    declared: &Declarations<'_>,
    dialect: &dyn Dialect,
    handovers: &mut crate::order::Schedule,
) -> Result<(), Refusal> {
    use crate::order::{Schedule, edges, shadow};
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

fn policy_release_schedule(
    cs: &PlanningPlan,
    declared: &Declarations<'_>,
    facts: &Facts<'_>,
    dialect: &dyn Dialect,
) -> Result<crate::order::Schedule, Refusal> {
    let mut schedule = crate::order::row_security::release_schedule(
        cs,
        declared.sources.before,
        crate::Side {
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
        crate::order::row_security::module_constraints(&mut schedule, edges).map_err(Refusal)?;
    }
    Ok(schedule)
}
