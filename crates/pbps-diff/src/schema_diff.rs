//! Change planning: comparing two "state + identity" pairs to produce a change
//! set.
//!
//! # Why both sides carry an identity file
//!
//! Matching up "which column is which" cannot rely on names — a rename makes the
//! names disagree across the two sides. Nor can it rely on this revision's intent
//! annotations, because a deployment may be many versions behind: when prod sits
//! at v1 and the declarations have reached v5, that rename intent left the
//! working tree long ago.
//!
//! So each side brings its own identity file and matching happens by **uid**: the
//! base's ids say `c_x → customer_name`, the declared ids say `c_x → full_name`,
//! and one comparison gives the rename directly, with no need to walk a chain of
//! names version by version. (This is exactly the problem SPEC §4.2 sets out to
//! solve.)
//!
//! # Where the base comes from
//!
//! `base` may come from the previous version in source control (an offline
//! preview) or from querying the database itself (Phase 3's authoritative plan).
//! This layer does not care which; it does the comparison and nothing else.

use crate::identity::type_refs::{TypeIdentity, TypeResolver, TypeSide};
use pbps_model::ColumnName;
use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::ColumnChange;
use pbps_model::ConstraintChange;
use pbps_model::ExtensionChange;
use pbps_model::GrantChange;
use pbps_model::IndexChange;
use pbps_model::ModuleChange;
use pbps_model::PartitionChange;
use pbps_model::PolicyChange;
use pbps_model::RoleChange;
use pbps_model::RowChange;
use pbps_model::SearchChange;
use pbps_model::SearchObject;
use pbps_model::TableChange;
use pbps_model::TablePropertyChange;
use pbps_model::TypeChange;
use pbps_model::TypeValueChange;
use pbps_model::change::DeleteCause;
use pbps_model::data::cell;
use pbps_model::{
    Cell, Change, ChangeSet, ColumnRef, ColumnType, DataMode, DetachedKind, DetachedName,
    GrantTarget, Hints, IdsFile, Index, IndexMethod, IndexPart, Module, ModuleId, Permission,
    PlannedChange, PrimaryKey, PublicAccess, Renames, ReplicaIdentity, RoutineOrigin, Schema,
    Table, TableName, Uid, UniqueConstraint, Value, ValueSource,
};

mod columns;
mod common;
mod computed;
mod constraints;
mod dependents;
mod error;
mod exclusions;
mod modules;
mod numbering;
mod partitions;
mod restoration;
mod roles;
mod rows;
mod search;
mod sequences;
mod table_props;
mod types;

use columns::diff_columns;
use common::change_in_words;
use common::columns_of;
use common::defaults_differ;
use common::partition_columns_by_uid;
use common::renamed_columns;
use common::renames_of;
use common::tie_break_text;
use computed::diff_computed;
use computed::refuse_computed_dependencies;
use computed::refuse_temporal_changes;
use constraints::ParameterEdit;
pub use constraints::adopts;
use constraints::diff_constraints;
#[cfg(test)]
use constraints::diff_constraints_by;
use constraints::parameter_edit;
use dependents::clear_external_partitions_after_loosening;
use dependents::rebind_foreign_keys_to_a_new_occupant;
use dependents::recreate_referenced_foreign_keys;
use dependents::recreate_retyped_dependents;
use dependents::refuse_unjoinable_foreign_keys;
use dependents::split_tightenings_from_retypes;
pub use error::DiffError;
pub use exclusions::adopts_exclusion;
use exclusions::diff_exclusions;
use exclusions::exclusion_hazards;
pub use exclusions::holds_key_with_equality;
pub use modules::ModuleDelta;
use modules::diff_modules;
pub use modules::module_delta;
use modules::names_changing_hands;
use modules::trigger_changes_hands;
use numbering::identities_put_on;
use numbering::numbering_after_deletes;
use numbering::numbering_put_on;
use numbering::refuse_rows_referencing_late_inserts;
use numbering::sequence_release_depths;
use partitions::attach::Attached;
use partitions::attach::attached;
use partitions::attach::diff_partition_columns;
use partitions::attach::partition_default;
use partitions::detach::detached_names;
use partitions::detach::parent_shape_changes;
use partitions::refusals::parent_column;
use partitions::refusals::refuse_partition_changes;
pub use restoration::restoration_comparison;
use roles::diff_roles;
pub(crate) use roles::member_depth;
use roles::revoke_public_execution;
use rows::diff_data;
use rows::omitted_defaults;
use rows::row_table;
use rows::tables_with_inserts;
use rows::tables_with_row_changes;
use search::diff_search;
pub(crate) use search::released_search;
use search::search_depth;
use sequences::diff_sequences;
use table_props::ForeignKeyGraph;
use table_props::diff_filegroups;
use table_props::diff_replica_identity;
use table_props::diff_storage_parameters;
use table_props::unlink_cycles_around_persistence_switches;
pub use types::type_differences;

/// One side's complete input: a state plus its own identity mapping.
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
}

/// Both sides' identity files, indexed once per diff for the passes that look
/// up per table or per column (#1870).
struct Indexes<'a> {
    base: pbps_model::IdsIndex<'a>,
    declared: pbps_model::IdsIndex<'a>,
}

/// Compares the base against the declarations and produces a change set.
///
/// `description` is **not compared** yet: it only affects data-catalogue prose,
/// not structure, and writing it to an extended property belongs to Phase 5.
///
/// The hints are the **declared** side's: a strategy describes how to operate on
/// the table as it will be, and a table that no longer exists has nothing left
/// to operate on (ADR-0003).
pub fn diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    diff_rebuilding(base, declared, dialect, hints, &BTreeSet::new())
}

/// [`diff`] for the plan a resolver's scratch compiles: from the packages the
/// declarations adopt to the declarations. Nothing runs this order. Scratch
/// takes its changes and re-settles its own sequence over them, one part at a
/// time, with the text edges as guesses the engine corrects (DEC-1959.6,
/// DEC-2143.9), so the debug observer does not judge it as an order that runs,
/// as it does not judge the resolver's other intermediate plans. A pair of
/// modules whose texts name each other, one name being only an alias, has no
/// lexical order at all and would otherwise fail the observer here.
pub fn bootstrap(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    // The buildable form of both sides: a text search object no identity names is not built,
    // so the differ does not refuse a held one as having none and stop a rehearsal or a
    // resolver run before it starts (#2855, DEC-2855.1).
    let (base_schema, declared_schema) = (
        base.schema.buildable(base.ids),
        declared.schema.buildable(declared.ids),
    );
    rebuilding_by(
        Side {
            schema: &base_schema,
            ids: base.ids,
        },
        Side {
            schema: &declared_schema,
            ids: declared.ids,
        },
        dialect,
        hints,
        &BTreeSet::new(),
        // From nothing, no module stands to be rebuilt, so the only difference
        // from `diff` is that the plan is not observed.
        Rebinding::Evidence,
        Screen::Text,
        None,
    )
}

/// [`diff`], with these unchanged modules rebuilt as if the declarations had
/// edited them.
///
/// For the engine fact the declarations cannot see: on PostgreSQL a module a
/// rebuilt module's dependents include has to be dropped and created around
/// it (#314, DEC-314.1), and only a connected read of `pg_depend` knows which.
/// Rebuilt here, not synthesized by the caller, because a rebuild is more
/// than its two statements: the grants it takes with it and the `PUBLIC`
/// execute it gets back are written by the passes below, and a caller adding
/// the pair after those passes had run would restore neither. It is the same
/// place `rebound_modules` adds the rebuilds a dialect asks for (DECISIONS 422).
///
/// An id that the plan already changes, or that is not declared on both
/// sides, is ignored: there is nothing unchanged to rebuild.
pub fn diff_rebuilding(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    rebuilding_by(
        base,
        declared,
        dialect,
        hints,
        also,
        Rebinding::Candidates,
        Screen::Text,
        None,
    )
}

/// Who decides which unchanged modules a plan's arrivals rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebinding {
    /// ADR-0013's candidate test, through `Dialect::rebound_modules`: the
    /// conservative rule ordinary planning keeps (DECISIONS 422).
    Candidates,
    /// The resolver's binding evidence, which the caller passes as `also`.
    /// A candidate is a reason to resolve, not proof that a rebuild is
    /// needed (ADR-0016 §1, DEC-1515.1), so the differ adds none of its own.
    Evidence,
}

/// Who judges a standing SQL Server computed column against the plan's
/// changes to a column it reads or a function it calls (#1460).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The differ, by text, over-approximating (DEC-1174.1): an offline
    /// plan's only judge, and never applied (SPEC §7.3).
    Text,
    /// The catalog's expression edges, which a connected SQL Server plan
    /// reads and matches under the database's collation (DEC-1431.1). The
    /// text screen folds case, and refused a retype of `A` beside a computed
    /// column reading `a` in a case-sensitive database. A computed column the
    /// plan adds, new or again, has no edge for its new text and is still
    /// screened here (#1459).
    Catalog,
}

/// [`diff_rebuilding`], or [`diff_connected`] for `Screen::Catalog`, for a
/// connected plan, which also hands over `read_back`: what it read, before the
/// recorded declared texts are put in place of the engine's spelling. Only
/// the comparisons the engine makes by parse tree read it; every other
/// comparison stays on the recorded texts (#1642 review).
pub fn diff_read_back(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    screen: Screen,
    read_back: &Schema,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    rebuilding_by(
        base,
        declared,
        dialect,
        hints,
        also,
        Rebinding::Candidates,
        screen,
        Some(read_back),
    )
}

/// [`diff_rebuilding`] for a connected SQL Server plan, which judges its
/// standing computed columns by the catalog's edges (`Screen::Catalog`).
pub fn diff_connected(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    rebuilding_by(
        base,
        declared,
        dialect,
        hints,
        also,
        Rebinding::Candidates,
        Screen::Catalog,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rebuilding_by(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    let mut d = diff_partial_rebuilding(
        base, declared, dialect, hints, also, rebinding, screen, read_back,
    );
    // Only an extended input takes the general route, and only then can an
    // annotation edge discharge a refusal that a module-only plan keeps: a
    // module-only plan never builds the contract, which needs the identity of
    // every table of both states that an offline plan may not hold.
    let extended = hints.annotations.is_extended();
    let mut deferred = Vec::new();
    if extended {
        d.errors.retain(|error| {
            if matches!(error, DiffError::ComputedFunctionChanged { .. }) {
                deferred.push(error.clone());
                false
            } else {
                true
            }
        });
    }
    if d.errors.is_empty() {
        // The resolver's intermediate plan is not an order anything runs: its
        // final one is observed where it is made.
        #[cfg(any(test, debug_assertions))]
        if rebinding == Rebinding::Candidates {
            crate::order::shadow::observe_plan(base, declared, dialect, hints, &d.changes);
        }
        let plan = finish_diff(base, declared, dialect, d.changes);
        if !extended {
            return Ok(plan);
        }
        let mut plan = plan
            .with_annotations(
                pbps_model::operation::Snapshot {
                    schema: base.schema,
                    ids: base.ids,
                },
                pbps_model::operation::Snapshot {
                    schema: declared.schema,
                    ids: declared.ids,
                },
                &hints.annotations.recorded,
                &hints.annotations.declared,
                if dialect.rebuilds_modules() {
                    pbps_model::operation::ModuleEdits::Rebuilt
                } else {
                    pbps_model::operation::ModuleEdits::InPlace
                },
            )
            .map_err(|why| vec![DiffError::Annotations(why)])?;
        plan.defer(deferred);
        Ok(plan)
    } else {
        Err(d.errors)
    }
}

// The annotation entry point admits the same complete differ result, with
// class-order refusals checked against its final hard order before returning.
pub(crate) fn finish_diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: ChangeSet,
) -> crate::order::DiffPlan {
    let pending = crate::order::boundaries::meetings(base, declared, &changes);
    let mut plan = crate::order::propose(changes);
    plan.pending_boundaries(
        pending,
        if dialect.rebuilds_modules() {
            pbps_model::operation::ModuleEdits::Rebuilt
        } else {
            pbps_model::operation::ModuleEdits::InPlace
        },
    );
    crate::order::differ(plan)
}

/// Everything [`diff`] found, with the differences it could not express kept
/// *beside* the ones it could rather than replacing them.
///
/// # Why both, and why only `verify` wants them
///
/// The two callers ask different questions. `plan` asks "may this be applied",
/// and one difference the emitter has no statement for makes the answer no —
/// a partial plan is worse than none, so it takes the `Result`.
///
/// `verify` asks "what is different", and that is a report. Dropping the
/// expressible changes on the floor because *another* table had an altered
/// `IDENTITY` undercounted the drift, and sent the `on_drift` hook a payload
/// missing differences the differ had already phrased perfectly well.
pub fn diff_partial(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
) -> Diffed {
    diff_partial_rebuilding(
        base,
        declared,
        dialect,
        hints,
        &BTreeSet::new(),
        Rebinding::Candidates,
        Screen::Text,
        None,
    )
}

/// The DEFAULT partition a new range or list partition is made beside, under
/// the name the plan leaves it (#1547, DEC-1547.1, #1632): one the base holds
/// as the parent's DEFAULT and the declarations keep as that parent's. Its
/// rows in the new range or list are moved into the partition before it is attached, so the
/// plan is the same whether it holds any or not. A parent the plan creates
/// has none, and neither has a DEFAULT the plan drops, detaches or makes:
/// one leaving goes in class 6, before the partition is created, and a new
/// one starts empty. The name is the declared one because renames run in
/// class 1, before every create.
fn standing_default(
    base: Side<'_>,
    declared: Side<'_>,
    of: &pbps_model::PartitionOf,
) -> Option<TableName> {
    // A HASH parent has no DEFAULT (#1632).
    if !matches!(
        of.bound,
        pbps_model::PartitionBound::Range { .. } | pbps_model::PartitionBound::List { .. }
    ) {
        return None;
    }
    let parent = base.ids.tables.get(declared.ids.table_uid(&of.parent)?)?;
    let is_default_of = |side: Side<'_>, name: &TableName, parent: &TableName| {
        side.schema
            .tables
            .get(name)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|o| {
                &o.parent == parent && matches!(o.bound, pbps_model::PartitionBound::Default)
            })
    };
    declared.ids.tables.iter().find_map(|(uid, name)| {
        let was = base.ids.tables.get(uid)?;
        (is_default_of(declared, name, &of.parent) && is_default_of(base, was, parent))
            .then(|| name.clone())
    })
}

// Visible in the crate for the ordering benchmark, which times planning
// without the test-only observer that `diff` runs (#1965).
#[allow(clippy::too_many_arguments)]
pub(crate) fn diff_partial_rebuilding(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Diffed {
    let mut changes = Vec::new();
    // An approved rewrite must never degrade to an ordinary DROP/CREATE: what
    // each lineage record means depends on what this environment holds. PR4a
    // qualifies the rewrite but deliberately grants no execution (DEC-2298.12).
    let lineage = crate::rebuild::lineage_errors(base, declared, dialect);
    if !lineage.is_empty() {
        return Diffed {
            changes: ChangeSet::default(),
            errors: lineage,
        };
    }
    let rebuilds: Vec<_> = declared
        .ids
        .rebuilds
        .values()
        .filter_map(|r| declared.ids.tables.get(&r.replacement_root))
        .filter_map(|table| crate::rebuild::qualify(base, declared, table, dialect).ok())
        .map(|root| Change::Partition(PartitionChange::RebuildPartitionedTable(Box::new(root))))
        .map(|change| PlannedChange {
            risks: dialect.change_risks(&change),
            ..PlannedChange::new(change)
        })
        .collect();
    if !rebuilds.is_empty() {
        return Diffed {
            changes: ChangeSet { changes: rebuilds },
            errors: crate::extensions::differences(base, declared, dialect.name()),
        };
    }
    // The passes below read module-to-module edges; the other references are
    // the transient contract's (`rebuilding_by`).
    let module_deps = hints.annotations.modules(declared.schema);
    let mut errs: Vec<DiffError> =
        pbps_model::module::dependency_cycles(declared.schema, &module_deps)
            .into_iter()
            .map(|modules| DiffError::DependsOnCycle { modules })
            .collect();
    errs.extend(crate::extensions::differences(
        base,
        declared,
        dialect.name(),
    ));
    diff_sequences(base.schema, declared.schema, dialect, &mut errs);
    if !dialect.records_conversions() && !declared.ids.conversions.is_empty() {
        errs.push(DiffError::ConversionsUnsupported {
            count: declared.ids.conversions.len(),
        });
    }

    let (mut label_changes, label_errors) = crate::enum_labels::changes(base, declared, hints);
    errs.extend(label_errors);
    let comparison = crate::retained::comparison(base, declared, hints, &mut label_changes);
    let comparison =
        crate::extensions::forward_moved_types(comparison, base, declared, &hints.extension_moves);
    let comparison = crate::extensions::forward_moved_classes(
        comparison,
        base,
        declared,
        &hints.extension_moves,
    );
    let comparison = crate::extensions::forward_moved_operators(
        comparison,
        base,
        declared,
        &hints.extension_moves,
    );
    let base = Side {
        schema: &comparison,
        ids: base.ids,
    };
    // The domains rebuilt under their uid: for a changed base or collation, as
    // the replacement of the type they are built on follows, or for an array
    // that blocks an in-place check (DEC-1635.9). They get no alteration.
    let rebuilt_domains =
        crate::domain_rebuild::plan(base, declared, hints, &label_changes, dialect);
    errs.extend(rebuilt_domains.errors);
    changes.extend(label_changes);
    changes.extend(rebuilt_domains.changes);
    changes.extend(crate::extensions::creates(
        base,
        declared,
        dialect.name(),
        &hints.extension_requires,
    ));
    let (extension_drops, extension_drop_errors) =
        crate::extensions::drops(base, declared, dialect.name(), &hints.extension_drops);
    changes.extend(extension_drops);
    errs.extend(extension_drop_errors);
    let (extension_updates, extension_update_errors) =
        crate::extensions::updates(base, declared, dialect.name(), &hints.extension_updates);
    changes.extend(extension_updates);
    errs.extend(extension_update_errors);
    // After the updates: a move of a package that is also updated starts from
    // the version the update leaves, and the plan's own order keeps them so
    // (DEC-2447.11).
    let (extension_moves, extension_move_errors) =
        crate::extensions::moves(base, declared, dialect.name(), &hints.extension_moves);
    changes.extend(extension_moves);
    errs.extend(extension_move_errors);
    changes.extend(diff_search(base, declared, hints, &mut errs));

    let types = diff_types(
        base,
        declared,
        dialect,
        &rebuilt_domains.uids,
        &mut changes,
        &mut errs,
    );

    let (
        TableFacts {
            index,
            renames,
            standing,
        },
        changes_out,
        errs_out,
    ) = diff_tables(base, declared, dialect, types, read_back, changes, errs);
    changes = changes_out;
    errs = errs_out;
    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;

    split_tightenings_from_retypes(&mut changes);
    clear_external_partitions_after_loosening(declared.schema, &mut changes);
    crate::enum_labels::rebuild_defaults(base, declared, &mut changes);
    recreate_retyped_dependents(base, declared, &renames, dialect, &mut changes);
    recreate_referenced_foreign_keys(base, declared, &renames, &mut changes);
    rebind_foreign_keys_to_a_new_occupant(base, declared, &mut changes);
    unlink_cycles_around_persistence_switches(declared.schema, &mut changes);
    // After every pass that drops and re-adds an index: each re-add of the
    // identity's index loses the identity, and is followed by it again.
    let early_identity = diff_replica_identity(
        base.schema,
        declared.schema,
        declared_tables,
        base_tables,
        &mut changes,
    );

    diff_modules(
        base.schema,
        declared.schema,
        &names_changing_hands(base, declared),
        dialect,
        &mut changes,
    );
    refuse_computed_dependencies(
        base.schema,
        declared.schema,
        dialect,
        screen,
        &changes,
        &mut errs,
    );
    refuse_temporal_changes(base, declared, &changes, &mut errs);
    refuse_unjoinable_foreign_keys(declared.schema, dialect, &changes, &mut errs);
    refuse_rows_referencing_late_inserts(declared, &changes, &mut errs);
    refuse_partition_changes(base, declared, &index, hints, dialect, &changes, &mut errs);
    changes = rebind_modules(base, declared, dialect, hints, also, rebinding, changes);
    diff_roles(base, declared, dialect, &mut changes);
    revoke_public_execution(base.schema, dialect, hints, &mut changes);

    // The ordering and risk pass below runs whether or not there are errors:
    // it is pure computation over the changes already built, and a caller that
    // is going to *report* the partial set needs it sorted and classified
    // exactly as a plan would be.
    // After every pass that rebuilds an index: its `CREATE` carries the
    // declared parameters (#1483 review).
    // Every change is in: whichever pass produced a retype or a drop of a held
    // column is seen, not only the table loop's.
    for (table, held) in &standing {
        let stated = exclusion_hazards(
            table,
            held,
            declared.schema.tables.get(table),
            changes.iter().filter(|c| c.table() == Some(table)),
            dialect,
            &mut errs,
        );
        changes.extend(stated);
    }
    // Descriptions are comments only where the engine holds them; elsewhere they stay the
    // free text they were (#2729).
    if dialect.holds_descriptions() {
        let comments = crate::comments::changes(base, declared, &changes);
        changes.extend(comments);
    }
    crate::order::drop_parameter_changes_of_rebuilt_indexes(&mut changes, |c| c);
    let mut planned: Vec<PlannedChange> = changes.into_iter().map(PlannedChange::new).collect();
    for p in &mut planned {
        // Attached here, not looked up at emit time: the plan file is the
        // artifact the deployment gate reviews, and a hint resolved later
        // against a YAML file the deployment host may not have is a hint
        // nobody read (ADR-0003).
        if let Some(strategy) = p.change.table().and_then(|t| hints.strategies.get(t)) {
            p.strategy = *strategy;
        }
    }

    crate::row_defaults::catalog(&mut planned, base, declared, &hints.row_guard_defaults);

    let (facts, mut planned, errs) = sort_facts(
        planned,
        base,
        declared,
        dialect,
        module_deps,
        index,
        early_identity,
        errs,
    );
    // Cached: `sort_by_key` computed this key, `Debug` text and all, on both
    // sides of every comparison, a third of a 4,000-table plan's CPU
    // (#1908). The cache keeps equal keys in their original order, as the
    // stable sort did, so the order is the same.
    crate::order::normalization::sort_draft(&mut planned, |p| facts.key(p));
    finish_sort(planned, base, declared, dialect, renames, errs)
}

/// The types phase of `diff_partial_rebuilding`: refusals for types that no
/// identity file records, then drop, alter and rename of the recorded ones, the
/// creates, and the refusals for rename cycles and for retained identities
/// whose declaration is gone.
///
/// It pushes into the caller's lists instead of returning its own, because the
/// order of `errors` is part of the result and the passes that follow read
/// `changes` as the earlier phases left it. It returns the resolver the tables
/// phase reads.
fn diff_types<'a>(
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &(impl Dialect + ?Sized),
    rebuilt_domain_uids: &BTreeMap<Uid, crate::domain_rebuild::Why>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) -> TypeResolver<'a> {
    // Definitions match by recorded identity. A rename is never inferred
    // from equal definitions or a pair of disappearing/appearing names.
    let types = TypeResolver::new(base.ids, declared.ids);
    let unrecorded_base = Schema {
        types: base
            .schema
            .types
            .iter()
            .filter(|(name, _)| {
                matches!(
                    types.resolve(TypeSide::Base, name),
                    TypeIdentity::ExternalOrUnrecorded(_)
                ) && matches!(
                    types.resolve(TypeSide::Desired, name),
                    TypeIdentity::ExternalOrUnrecorded(_)
                )
            })
            .map(|(n, d)| (n.clone(), d.clone()))
            .collect(),
        ..Default::default()
    };
    errs.extend(type_differences(
        &unrecorded_base,
        declared.schema,
        dialect,
        false,
    ));
    let mut type_renames = BTreeMap::new();
    for (uid, old_name) in &base.ids.types {
        let Some(old) = base.schema.types.get(old_name) else {
            continue;
        };
        match declared
            .ids
            .types
            .get(uid)
            .and_then(|name| declared.schema.types.get(name).map(|ty| (name, ty)))
        {
            None => {
                let used = types.has_retained_dependent(base.schema, declared.schema, uid);
                if used {
                    errs.push(DiffError::TypeLifecycleUnsupported { name: old_name.clone(), reason: "a retained declaration still uses this type; drop or change its dependents first".into() });
                }
                changes.push(Change::Type(TypeChange::DropType {
                    uid: uid.clone(),
                    name: old_name.clone(),
                    definition: Box::new(old.clone()),
                }));
            }
            Some((name, new)) => {
                let mut comparable = old.clone();
                if let pbps_model::ManagedType::Domain { base: ty, .. } = &mut comparable {
                    types.remap(ty, TypeSide::Base, TypeSide::Desired);
                }
                let old_schema = Schema {
                    types: [(name.clone(), comparable)].into(),
                    ..Default::default()
                };
                let new_schema = Schema {
                    types: [(name.clone(), new.clone())].into(),
                    ..Default::default()
                };
                let mut refused = Vec::new();
                // An enum's labels and a domain's every attribute have their
                // own changes; only a change of kind is refused here
                // (DEC-1635.6, DEC-1635.8, DEC-1635.9).
                if !matches!(
                    (&old_schema.types[name], new),
                    (
                        pbps_model::ManagedType::Enum { .. },
                        pbps_model::ManagedType::Enum { .. }
                    ) | (
                        pbps_model::ManagedType::Domain { .. },
                        pbps_model::ManagedType::Domain { .. }
                    )
                ) {
                    refused = type_differences(&old_schema, &new_schema, dialect, true);
                }
                // A retained domain changes its checks, default and NOT NULL
                // in place: what it drops before the plan's rows, and what it
                // adds after them. A base or collation change was refused
                // above and plans nothing (DEC-1635.8).
                let comparable = &old_schema.types[name];
                let alters = refused.is_empty()
                    && !rebuilt_domain_uids.contains_key(uid)
                    && matches!(
                        (comparable, new),
                        (
                            pbps_model::ManagedType::Domain { .. },
                            pbps_model::ManagedType::Domain { .. }
                        )
                    );
                if alters && let Some(delta) = pbps_model::DomainDelta::of(comparable, new) {
                    for (half, wanted) in [
                        (pbps_model::DomainHalf::Loosen, delta.loosens()),
                        (pbps_model::DomainHalf::Tighten, delta.tightens()),
                    ] {
                        if wanted {
                            changes.push(Change::TypeValue(TypeValueChange::AlterDomain {
                                uid: uid.clone(),
                                name: name.clone(),
                                from: Box::new(comparable.clone()),
                                to: Box::new(new.clone()),
                                half,
                            }));
                        }
                    }
                }
                errs.extend(refused);
                if old_name != name {
                    type_renames.insert(old_name.clone(), name.clone());
                    changes.push(Change::Type(TypeChange::RenameType {
                        uid: uid.clone(),
                        from: old_name.clone(),
                        to: name.clone(),
                        // What stands when the rename runs: an enum's labels
                        // and a domain the plan alters are the old ones, the
                        // domain under the spellings of the types it is over
                        // that the plan leaves.
                        definition: Box::new(match (old, alters) {
                            (pbps_model::ManagedType::Enum { .. }, _) => old.clone(),
                            (pbps_model::ManagedType::Domain { .. }, true) => comparable.clone(),
                            (pbps_model::ManagedType::Domain { .. }, false) => new.clone(),
                        }),
                    }));
                }
            }
        }
    }
    for (uid, name) in &declared.ids.types {
        if base
            .ids
            .types
            .get(uid)
            .and_then(|old| base.schema.types.get(old))
            .is_none()
            && let Some(definition) = declared.schema.types.get(name)
        {
            changes.push(Change::Type(TypeChange::CreateType {
                uid: uid.clone(),
                name: name.clone(),
                definition: Box::new(definition.clone()),
            }));
        }
    }
    for start in type_renames.keys() {
        let mut seen = BTreeSet::new();
        let mut name = start;
        while let Some(next) = type_renames.get(name) {
            if !seen.insert(name) {
                errs.push(DiffError::TypeLifecycleUnsupported {
                    name: start.clone(),
                    reason: "type rename cycle (swap) is refused; use separate plans".into(),
                });
                break;
            }
            name = next;
        }
    }

    // A retained uid is still a managed claim after a manual drop and a
    // baseline. It cannot become an unheld identity in an empty apply.
    for (uid, name) in &declared.ids.types {
        if !declared.schema.types.contains_key(name)
            && types
                .name(TypeSide::Base, uid)
                .and_then(|old| base.schema.types.get(old))
                .is_none()
        {
            errs.push(DiffError::TypeLifecycleUnsupported {
                name: name.clone(),
                reason: "its identity remains recorded but its declaration is absent".into(),
            });
        }
    }
    types
}

/// What the tables phase leaves for the passes after it.
struct TableFacts<'a> {
    /// Both identity files, indexed: the partition refusals and the sort facts read it.
    index: Indexes<'a>,
    /// The table and column renames of this plan: the dependents of a retyped
    /// column, the foreign keys onto a renamed table and the rename order read them.
    renames: Renames,
    /// The base tables that hold an exclusion, as this plan's renames leave
    /// them: the exclusion hazards read them.
    standing: BTreeMap<TableName, Table>,
}

/// The tables phase of `diff_partial_rebuilding`: the tables only the base
/// holds are dropped, the tables only the declarations hold are created, and
/// each table both sides hold is renamed, compared and attached or detached.
///
/// It takes the lists of `changes` and `errs` built so far and hands them back,
/// not lists of its own to append afterwards: the order of both is part of the
/// result, and the loop over the surviving tables reads `changes` as `&changes`
/// and lends both as `&mut changes` and `&mut errs`, which a `&mut Vec`
/// parameter would not allow. It takes the resolver by value, as no pass after
/// it reads it. It returns the facts the passes after it read.
fn diff_tables<'a>(
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &(impl Dialect + ?Sized),
    types: TypeResolver<'_>,
    read_back: Option<&Schema>,
    mut changes: Vec<Change>,
    mut errs: Vec<DiffError>,
) -> (TableFacts<'a>, Vec<Change>, Vec<DiffError>) {
    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;
    let index = Indexes {
        base: base.ids.index(),
        declared: declared.ids.index(),
    };

    // One UID-derived address/template map, shared by every moving boundary.
    // Pending is a pure comparison; diff_columns still runs once in its usual place.
    let parent_shapes: BTreeMap<_, _> = base_tables
        .iter()
        .filter_map(|(uid, old_name)| {
            if !dialect.partition_meetings() {
                return None;
            }
            let new_name = declared_tables.get(uid)?;
            let old = base.schema.tables.get(old_name)?;
            let new = declared.schema.tables.get(new_name)?;
            let pending = old.partition_by.is_some()
                && old.partition_of.is_none()
                && new.partition_by.is_some()
                && new.partition_of.is_none()
                && (old_name != new_name || parent_shape_changes(old, new));
            Some((uid, (old_name, new_name, new, pending)))
        })
        .collect();
    let final_parent = |name: &TableName| {
        base.ids
            .table_uid(name)
            .and_then(|uid| parent_shapes.get(uid))
            .map_or_else(|| name.clone(), |(_, name, _, _)| (*name).clone())
    };

    // Table present only in the base: dropped. Its foreign keys are dropped
    // first as separate changes — two dropped tables that reference each other
    // would otherwise fail or succeed depending on which DROP TABLE runs first.
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            if let Some(t) = base.schema.tables.get(name) {
                for fk_name in t.foreign_keys.keys() {
                    changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                        table: name.clone(),
                        name: fk_name.clone(),
                    }));
                }
            }
            changes.push(Change::Table(TableChange::DropTable {
                uid: uid.clone(),
                name: name.clone(),
                // A partition is detached from its parent before it goes
                // (#1171, DEC-1171.1).
                detach_from: base
                    .schema
                    .tables
                    .get(name)
                    .and_then(|t| t.partition_of.as_ref())
                    .map(|of| final_parent(&of.parent)),
            }));
        }
    }

    // Table present only in the declarations: created. Foreign keys are split
    // out of the CREATE into their own changes, because they sort after every
    // CreateTable — a new table's FK may reference another new table, and
    // within one ordering class the order of creation is not meaningful.
    for (uid, name) in declared_tables {
        if !base_tables.contains_key(uid)
            && let Some(t) = declared.schema.tables.get(name)
        {
            let mut table = t.clone();
            for (fk_name, fk) in std::mem::take(&mut table.foreign_keys) {
                changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: fk_name,
                    constraint: Box::new(fk),
                }));
            }
            // Rows are split out of the CREATE for the same reason the
            // foreign keys above are: they are separate statements in a
            // separate ordering class, and a `CreateTable` carrying rows would
            // give the emitter a second place to produce DML.
            match (&table.data, table.primary_key.as_ref()) {
                (Some(data), Some(pk)) if pk.columns.len() == 1 => {
                    let identity_key = table.columns.get(&pk.columns[0]).is_some_and(|c| match &c
                        .value_source
                    {
                        ValueSource::Identity(_) => true,
                        // A `serial` key is refused by its dialect (#1631).
                        ValueSource::None
                        | ValueSource::Default(_)
                        | ValueSource::Sequence(_)
                        | ValueSource::Generated(_) => false,
                    });
                    for (key, row) in &data.rows {
                        let (defaults, types) = omitted_defaults(&table, pk.columns[0].raw(), row);
                        changes.push(Change::Row(RowChange::InsertRow {
                            table: name.clone(),
                            key_column: pk.columns[0].clone(),
                            identity_key,
                            key: key.clone(),
                            row: row.clone(),
                            defaults,
                            types,
                        }));
                    }
                }
                (Some(_), _) => errs.push(DiffError::DataWithoutKey {
                    table: name.clone(),
                }),
                (None, _) => {}
            }
            let beside_default = table
                .partition_of
                .as_ref()
                .and_then(|of| standing_default(base, declared, of));
            // What a human allowed it to take (#1788): read from the
            // declared side, where `pbps move-rows` writes it.
            let moves_at_most = beside_default
                .as_ref()
                .and_then(|_| declared.ids.moves.get(uid))
                .map(|m| m.max_rows);
            for (policy_name, policy) in &table.row_policies {
                changes.push(Change::Policy(PolicyChange::CreatePolicy {
                    table: name.clone(),
                    name: policy_name.clone(),
                    policy: policy.clone(),
                    origin: pbps_model::RowSecurityOrigin::Created,
                }));
            }
            if !table.row_security.is_default() {
                changes.push(Change::Policy(PolicyChange::SetRowSecurity {
                    uid: uid.clone(),
                    table: name.clone(),
                    from: Default::default(),
                    to: table.row_security,
                    origin: pbps_model::RowSecurityOrigin::Created,
                }));
            }
            // An exclusion is added by its own change, after the table's rows
            // (DEC-2448.3): the creation payload never carries one, so no
            // emitter can create the table and lose it.
            let exclusions = std::mem::take(&mut table.exclude);
            changes.push(Change::Table(TableChange::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(table),
                beside_default,
                moves_at_most,
            }));
            for (exclusion, constraint) in exclusions {
                changes.push(Change::Constraint(ConstraintChange::SetExclusion {
                    table: name.clone(),
                    name: exclusion,
                    from: None,
                    to: Some(Box::new(constraint)),
                }));
            }
        }
    }

    // Table present on both sides: possibly renamed, and its contents must be
    // compared.
    let renames = renames_of(base, declared);
    let mut standing: BTreeMap<TableName, Table> = BTreeMap::new();
    for (name, table) in &declared.schema.tables {
        if table.unlogged {
            continue;
        }
        for (key, fk) in &table.foreign_keys {
            if declared
                .schema
                .tables
                .get(&fk.references_table)
                .is_some_and(|t| t.unlogged)
            {
                errs.push(DiffError::PermanentReferencesUnlogged {
                    table: name.clone(),
                    key: key.clone(),
                    target: fk.references_table.clone(),
                });
            }
            // Through a partitioned table, which the engine lets pass: the
            // key reaches each of its partitions (#1580).
            for (partition, _) in declared.schema.tables.iter().filter(|(_, t)| {
                t.unlogged
                    && t.partition_of
                        .as_ref()
                        .is_some_and(|of| of.parent == fk.references_table)
            }) {
                errs.push(DiffError::PermanentReferencesUnloggedPartition {
                    table: name.clone(),
                    key: key.clone(),
                    target: fk.references_table.clone(),
                    partition: partition.clone(),
                });
            }
        }
    }
    let ctx = TableCtx {
        base,
        declared,
        dialect,
        types: &types,
        index: &index,
        read_back,
        renames: &renames,
        parent_shapes: &parent_shapes,
        final_parent: &final_parent,
        base_tables,
    };
    for (uid, declared_name) in declared_tables {
        diff_surviving_table(
            &ctx,
            uid,
            declared_name,
            &mut standing,
            &mut changes,
            &mut errs,
        );
    }
    (
        TableFacts {
            index,
            renames,
            standing,
        },
        changes,
        errs,
    )
}

/// The read-only facts the loop over the tables both sides hold shares across
/// its iterations; `diff_surviving_table` reads them.
struct TableCtx<'a, D: Dialect + ?Sized> {
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &'a D,
    types: &'a TypeResolver<'a>,
    index: &'a Indexes<'a>,
    read_back: Option<&'a Schema>,
    renames: &'a Renames,
    parent_shapes: &'a BTreeMap<&'a Uid, (&'a TableName, &'a TableName, &'a Table, bool)>,
    final_parent: &'a dyn Fn(&TableName) -> TableName,
    base_tables: &'a BTreeMap<Uid, TableName>,
}

/// One table both sides hold: renamed, then detached, attached or compared
/// with its declaration. It pushes into the caller's lists in place, as the
/// loop it came from did: the order of both is part of the result, and the
/// attach and detach arms stop early where the table needs no more.
#[allow(clippy::needless_borrow)] // The body moved as it was; its borrows were of owned locals.
fn diff_surviving_table<D: Dialect + ?Sized>(
    ctx: &TableCtx<'_, D>,
    uid: &Uid,
    declared_name: &TableName,
    standing: &mut BTreeMap<TableName, Table>,
    mut changes: &mut Vec<Change>,
    mut errs: &mut Vec<DiffError>,
) {
    let TableCtx {
        base,
        declared,
        dialect,
        types,
        index,
        read_back,
        renames,
        parent_shapes,
        final_parent,
        base_tables,
    } = *ctx;
    let Some(base_name) = base_tables.get(uid) else {
        return;
    };
    if base_name != declared_name {
        let defaults = base
            .schema
            .tables
            .get(base_name)
            .map(|t| {
                t.columns
                    .iter()
                    .filter(|(_, c)| match &c.value_source {
                        ValueSource::Default(_) => true,
                        ValueSource::None
                        | ValueSource::Identity(_)
                        | ValueSource::Sequence(_)
                        | ValueSource::Generated(_) => false,
                    })
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        changes.push(Change::Table(TableChange::RenameTable {
            uid: uid.clone(),
            from: base_name.clone(),
            to: declared_name.clone(),
            defaults,
        }));
    }
    let (Some(base_table), Some(declared_table)) = (
        base.schema.tables.get(base_name),
        declared.schema.tables.get(declared_name),
    ) else {
        return;
    };
    // Only a retained identity needs explicit policy teardown. Dropping
    // the owner removes its policies, while a replacement UID is created
    // through the ordinary new-table path (DEC-1173.2).
    for name in base_table
        .row_policies
        .keys()
        .chain(declared_table.row_policies.keys())
        .collect::<BTreeSet<_>>()
    {
        let before = base_table.row_policies.get(name);
        let after = declared_table.row_policies.get(name);
        if before == after {
            continue;
        }
        if let Some(policy) = before {
            changes.push(Change::Policy(PolicyChange::DropPolicy {
                table: base_name.clone(),
                name: name.clone(),
                policy: policy.clone(),
            }));
        }
        if let Some(policy) = after {
            changes.push(Change::Policy(PolicyChange::CreatePolicy {
                table: declared_name.clone(),
                name: name.clone(),
                policy: policy.clone(),
                origin: pbps_model::RowSecurityOrigin::Standing,
            }));
        }
    }
    if base_table.row_security != declared_table.row_security {
        changes.push(Change::Policy(PolicyChange::SetRowSecurity {
            uid: uid.clone(),
            table: declared_name.clone(),
            from: base_table.row_security,
            to: declared_table.row_security,
            origin: pbps_model::RowSecurityOrigin::Standing,
        }));
    }
    // A partition declared as an ordinary table is detached, and that is
    // all this plan does to it (#1544, DEC-1544.1): its base holds no
    // columns of its own, so a column-by-column diff would read every one
    // as new.
    if let Some(of) = &base_table.partition_of
        && declared_table.partition_of.is_none()
        && declared_table.partition_by.is_none()
    {
        match base
            .schema
            .tables
            .get(&of.parent)
            .ok_or_else(|| {
                vec![format!(
                    "its parent {} is not in the base",
                    of.parent.shown()
                )]
            })
            // The parent as this plan leaves it: a foreign key to a
            // table renamed in the same plan is declared under the new
            // name, as `diff_constraints` reads it below. And the
            // partition as well, whose own foreign keys are matched by
            // name and definition (#1632).
            .and_then(|parent| {
                detached_names(
                    &renames.apply(parent, &of.parent),
                    &renames.apply(base_table, base_name),
                    declared_table,
                    dialect,
                    base.ids
                        .table_uid(&of.parent)
                        .and_then(|uid| parent_shapes.get(uid))
                        .is_some_and(|(_, _, _, pending)| *pending),
                )
            }) {
            Ok(names) => changes.push(Change::Partition(PartitionChange::DetachPartition {
                uid: uid.clone(),
                table: declared_name.clone(),
                parent: final_parent(&of.parent),
                names,
                shape: Box::new(declared_table.clone()),
            })),
            Err(what) => errs.push(DiffError::DetachedShape {
                table: declared_name.clone(),
                parent: of.parent.clone(),
                what,
            }),
        }
        return;
    }

    // An ordinary table declared as a partition is attached, and keeps
    // its rows (#1545, DEC-1545.1). Its columns become its parent's, so
    // they are not compared; what it holds of its own once attached is
    // compared with the declaration as a standing partition's is, and
    // brought to it by the same changes after the attach.
    if base_table.partition_of.is_none()
        && let Some(of) = &declared_table.partition_of
    {
        match attached(
            base,
            base_name,
            declared_name,
            base_table,
            declared_table,
            of,
            dialect,
            read_back,
            declared
                .ids
                .table_uid(&of.parent)
                .and_then(|uid| parent_shapes.get(uid))
                .map(|(old, _, goal, pending)| (*old, *pending, *goal)),
        ) {
            Ok(Attached {
                table,
                defaultless,
                displaced,
                released,
            }) => {
                changes.extend(released);
                // Gone before the attach, so the engine adopts the one
                // index left for each of its parent's or builds it; one
                // the declaration keeps as its own is added after.
                for name in displaced {
                    changes.push(Change::Index(IndexChange::DropIndex {
                        table: declared_name.clone(),
                        name,
                    }));
                }
                changes.push(Change::Partition(PartitionChange::AttachPartition {
                    uid: uid.clone(),
                    table: declared_name.clone(),
                    parent: of.parent.clone(),
                    bound: of.bound.clone(),
                    key: dialect.attach_scan_key(
                        uid,
                        &declared
                            .schema
                            .tables
                            .get(&of.parent)
                            .and_then(|parent| {
                                let by = parent.partition_by.as_ref()?;
                                by.columns
                                    .iter()
                                    .map(|c| Some((c.clone(), parent.columns.get(c)?.ty.clone())))
                                    .collect::<Option<Vec<_>>>()
                            })
                            .unwrap_or_default(),
                        &declared.schema.types,
                    ),
                    shape: Box::new(declared_table.clone()),
                }));
                // Compared as this plan leaves them: an own foreign key
                // to a table renamed in the same plan is declared under
                // the new name (#1632). Matched to the parent's above on
                // the base side, where the engine matches by relation.
                diff_constraints(
                    declared_name,
                    &renames.apply(&table, base_name),
                    declared_table,
                    &mut changes,
                    &mut errs,
                );
                // Its own exclusions, those the parent's did not take (DEC-2448.7),
                // as the base held them: a retirement is asked of the table's
                // original definition, as for a standing table.
                let retired = |key: &pbps_model::ObjectLocalName| {
                    base_table
                        .exclude
                        .get(key)
                        .is_some_and(|held| declared.ids.exclusion_retired(uid, key, held))
                };
                diff_exclusions(
                    declared_name,
                    &renames.apply(&table, base_name),
                    declared_table,
                    &retired,
                    &mut changes,
                    &mut errs,
                );
                diff_storage_parameters(uid, declared_name, &table, declared_table, &mut changes);
                if table.unlogged != declared_table.unlogged {
                    changes.push(Change::TableProperty(
                        TablePropertyChange::SetTablePersistence {
                            uid: uid.clone(),
                            table: declared_name.clone(),
                            unlogged: declared_table.unlogged,
                        },
                    ));
                }
                diff_partition_columns(
                    uid,
                    declared_name,
                    &table,
                    declared_table,
                    base,
                    declared,
                    &index,
                    dialect,
                    &mut changes,
                    &mut errs,
                );
                // A column without a default where its parent's has one:
                // the attach gives it none, which no declaration can say
                // and the reader does not hold, so it takes its parent's
                // unless it declares its own, which the comparison above
                // sets (measured on 16 and 18).
                let parent = declared.schema.tables.get(&of.parent);
                for column in defaultless {
                    let own = of.columns.get(&column).and_then(|c| c.default.as_ref());
                    let fallback = partition_default(
                        dialect,
                        &of.parent,
                        &column,
                        parent.and_then(|p| p.columns.get(&column)),
                        base.schema
                            .tables
                            .get(&of.parent)
                            .and_then(|p| p.columns.get(&column)),
                        &mut errs,
                    );
                    if let (None, Some(fallback)) = (own, fallback) {
                        changes.push(Change::Partition(PartitionChange::SetPartitionDefault {
                            uid: uid.clone(),
                            table: declared_name.clone(),
                            parent: of.parent.clone(),
                            column,
                            from: None,
                            to: None,
                            fallback: Some(fallback),
                        }));
                    }
                }
            }
            Err(what) => errs.push(DiffError::AttachedShape {
                table: declared_name.clone(),
                parent: of.parent.clone(),
                what,
            }),
        }
        return;
    }

    diff_columns(
        &index,
        &types,
        base_name,
        declared_name,
        base_table,
        declared_table,
        dialect,
        &declared.ids.conversions,
        &mut changes,
        &mut errs,
    );
    // Asked of the whole change list below, not of this loop's own: an enum
    // or domain rebuild retypes a column before this loop runs.
    if !base_table.exclude.is_empty() {
        standing.insert(
            declared_name.clone(),
            renames.apply(base_table, base_name).into_owned(),
        );
    }
    diff_filegroups(declared_name, base_table, declared_table, &mut errs);
    diff_constraints(
        declared_name,
        &renames.apply(base_table, base_name),
        declared_table,
        &mut changes,
        &mut errs,
    );
    // A retirement holds the definition as the baseline wrote it, and the
    // table compared below has had this plan's column renames applied, so
    // authority is asked of the original: a plan that renames a key column
    // and drops the constraint is a valid one, and no rerun of `pbps
    // drop-exclusion` could record the renamed text.
    let retired = |key: &pbps_model::ObjectLocalName| {
        base_table
            .exclude
            .get(key)
            .is_some_and(|held| declared.ids.exclusion_retired(uid, key, held))
    };
    diff_exclusions(
        declared_name,
        &renames.apply(base_table, base_name),
        declared_table,
        &retired,
        &mut changes,
        &mut errs,
    );
    diff_computed(declared_name, base_table, declared_table, &mut changes);
    diff_storage_parameters(uid, declared_name, base_table, declared_table, &mut changes);
    if base_table.unlogged != declared_table.unlogged {
        changes.push(Change::TableProperty(
            TablePropertyChange::SetTablePersistence {
                uid: uid.clone(),
                table: declared_name.clone(),
                unlogged: declared_table.unlogged,
            },
        ));
    }
    diff_partition_columns(
        uid,
        declared_name,
        base_table,
        declared_table,
        base,
        declared,
        &index,
        dialect,
        &mut changes,
        &mut errs,
    );
    // Rows are compared by column *name* on each side, and a rename in
    // this same plan means the two sides know one column by two names.
    // The uid is what says they are the same column.
    let base_cols = columns_of(&index.base, base_name);
    let base_name_of: BTreeMap<ColumnName, ColumnName> = columns_of(&index.declared, declared_name)
        .iter()
        .filter_map(|(uid, d)| base_cols.get(uid).map(|b| (d.name.clone(), b.name.clone())))
        .collect();
    // The observed rows as this plan's label renames leave them: the
    // engine renames a label in the rows that hold it (DEC-2346.2).
    let forwarded = crate::enum_labels::forwarded_rows(
        base_table,
        (&TypeResolver::new(base.ids, declared.ids), TypeSide::Base),
        &crate::enum_labels::label_renames(&*changes),
    );
    diff_data(
        declared_name,
        forwarded.as_ref().unwrap_or(base_table),
        declared_table,
        &base_name_of,
        &mut changes,
        &mut errs,
    );
}

/// The module rebinding phase of `diff_partial_rebuilding`: the modules the
/// dialect, the caller or an enum's dependents ask to have rebuilt around the
/// plan's changes, and the modules a schema-bound teardown takes down.
///
/// It takes the list of `changes` built so far and hands it back rebuilt, not a
/// list of its own to append afterwards: it rewrites entries already in the
/// list, and `diff_roles`, the exclusion hazards and the sort read the list as
/// this phase leaves it. Moving the list in and out keeps its order.
fn rebind_modules(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    mut changes: Vec<Change>,
) -> Vec<Change> {
    // A module declaration can stay byte-for-byte identical while a new
    // overload or shadow changes what it should bind to. Ask the dialect
    // before sorting, rather than appending unreviewed SQL at apply time.
    let arriving: Vec<ModuleId> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Module(ModuleChange::CreateModule { id, .. }) => Some(id.clone()),
            Change::Table(TableChange::CreateTable { name, .. }) => {
                Some(ModuleId::Named(name.clone()))
            }
            Change::Table(TableChange::RenameTable { to, .. }) => Some(ModuleId::Named(to.clone())),
            Change::Table(TableChange::DropTable { .. })
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(ModuleChange::AlterModule { .. })
            | Change::Module(ModuleChange::DropModule { .. })
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    let changed: BTreeSet<ModuleId> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Module(ModuleChange::CreateModule { id, .. })
            | Change::Module(ModuleChange::AlterModule { id, .. })
            | Change::Module(ModuleChange::DropModule { id, .. }) => Some(id.clone()),
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    // One set, so a module both the dialect and the caller ask for is rebuilt
    // once.
    let candidates = match rebinding {
        Rebinding::Candidates => dialect.rebound_modules(declared.schema, &arriving, &changed),
        Rebinding::Evidence => BTreeSet::new(),
    };
    let rebound: BTreeSet<ModuleId> = candidates
        .into_iter()
        .chain(also.iter().cloned())
        .chain(hints.enum_dependents.iter().cloned())
        .collect();
    for id in rebound {
        if !changed.contains(&id)
            && base.schema.modules.contains_key(&id)
            && let Some(module) = declared.schema.modules.get(&id)
        {
            changes.push(Change::Module(ModuleChange::AlterModule {
                id,
                module: Box::new(module.clone()),
            }));
        }
    }
    // A module over a rebuilt enum's column cannot be restated in place after
    // the retype: it comes down in class 0 and goes back in class 14, around
    // the retypes and the swap (DEC-1635.7).
    if !hints.enum_dependents.is_empty() {
        let mut split = Vec::new();
        for change in std::mem::take(&mut changes) {
            if let Change::Module(ModuleChange::AlterModule { id, module }) = &change
                && hints.enum_dependents.contains(id)
            {
                split.push(Change::Module(ModuleChange::DropModule {
                    id: id.clone(),
                    kind: module.kind,
                }));
                split.push(Change::Module(ModuleChange::CreateModule {
                    id: id.clone(),
                    module: module.clone(),
                }));
            } else {
                split.push(change);
            }
        }
        changes = split;
    }
    // A module the engine binds to what this plan changes cannot stand beside
    // the change, whether the declarations edited it or not (SQL Server's
    // SCHEMABINDING, DEC-2329.1). It goes as a drop and comes back as a
    // create: the classes put the first before the table changes and the
    // second after them, as they do for any module. Before `diff_roles`, so
    // the grants a rebuilt module loses are written again.
    for id in dialect.schema_bound_teardown(base.schema, declared.schema, &changes) {
        let (Some(before), Some(module)) = (
            base.schema.modules.get(&id),
            declared.schema.modules.get(&id),
        ) else {
            continue;
        };
        if changes.iter().any(|c| {
            matches!(c, Change::Module(ModuleChange::DropModule { id: other, .. }) | Change::Module(ModuleChange::CreateModule { id: other, .. }) if *other == id)
        }) {
            continue;
        }
        changes.retain(|c| !matches!(c, Change::Module(ModuleChange::AlterModule { id: other, .. }) if *other == id));
        changes.push(Change::Module(ModuleChange::DropModule {
            id: id.clone(),
            kind: before.kind,
        }));
        changes.push(Change::Module(ModuleChange::CreateModule {
            id,
            module: Box::new(module.clone()),
        }));
    }
    changes
}

/// The creation and the drop rank of each module among the modules, by their
/// dependencies: `dependency_rank` reads them for the module changes.
#[allow(clippy::needless_borrow)] // The block moved as it was; its borrows were of an owned local.
fn module_ranks(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    module_deps: &pbps_model::module::ModuleDeps,
) -> (BTreeMap<ModuleId, usize>, BTreeMap<ModuleId, usize>) {
    // Modules are ordered among themselves by dependency: a view over a view
    // has to be created second, and dropped first (ADR-0002).
    // Lexed by the dialect: where a literal ends is the engine's rule, and an
    // edge read out of a literal the engine had not closed put a view before
    // the one it selects from (DECISIONS 315).
    let lex = |definition: &str| dialect.code_only(definition);
    let rank = |from: &str, to: &str| dialect.bare_name_rank(from, to);
    let lexis = pbps_model::module::Lexis {
        code_only: &lex,
        quoted_identifiers: dialect.lexicon().quoted_identifiers,
        continues_ident: dialect.lexicon().identifier_continues,
        reserved: dialect.lexicon().reserved,
        bare_rank: &rank,
    };
    let create_rank = rank_of(&pbps_model::module::creation_order_with(
        &declared.schema.modules,
        &module_deps,
        &lexis,
    ));
    let drop_rank = rank_of(&pbps_model::module::creation_order_with(
        &base.schema.modules,
        &module_deps,
        &lexis,
    ));
    (create_rank, drop_rank)
}

/// The parents this plan attaches partitions to, and the rank of the tables that
/// receive rows or attaches: `late_attach_ranks` reads both, `dependency_rank`
/// the rank.
fn attach_parents_and_data_rank(
    planned: &[PlannedChange],
    declared: Side<'_>,
) -> (BTreeMap<TableName, TableName>, BTreeMap<TableName, usize>) {
    // Rows follow the foreign keys among the tables that declare them. The
    // declared side is the right one to read: a row being inserted is going
    // into the schema as it will be, not as it was.
    //
    // An attach brings rows into its parent and validates the parent's
    // foreign keys over them, so the parents this plan attaches to are
    // ordered with the tables that receive rows: a parent after every table
    // it references, whether rows or another attach fill that one (#1642
    // review).
    let attach_parents: BTreeMap<TableName, TableName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Partition(PartitionChange::AttachPartition { table, parent, .. }) =
                &p.change
            {
                Some((table.clone(), parent.clone()))
            } else {
                None
            }
        })
        .collect();
    let data_rank = rank_of_tables(&if attach_parents.is_empty() {
        pbps_model::data::insertion_order(declared.schema)
    } else {
        pbps_model::data::supply_order(
            declared.schema,
            declared
                .schema
                .tables
                .iter()
                .filter(|(_, t)| t.data.is_some())
                .map(|(n, _)| n.clone())
                .chain(attach_parents.values().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    });
    (attach_parents, data_rank)
}

/// The tables a `RenameColumn` of this plan claims a column name on:
/// `frees_a_renamed_column` reads it to find the drop that frees a name.
fn column_name_claims(planned: &[PlannedChange]) -> BTreeSet<TableName> {
    // The tables a `RenameColumn` in this plan claims a column name on. Both
    // this and the drops below carry `declared_table_name` (`diff_columns`),
    // so the two spellings meet.
    //
    // The *table*, and not the name, because the name is a question this
    // cannot answer. On SQL Server what makes two spellings one column name is
    // the database's collation, which a plan computed offline does not have
    // (SPEC 7.3) — and `Mssql::fold_ident` returns the identity for exactly
    // that reason. **Measured** on the pinned image, each in a database of the
    // named collation:
    //
    //     SQL_Latin1_General_CP1_CI_AS   note  vs Note  ->  one name, Msg 15335
    //     SQL_Latin1_General_CP1_CI_AI   café  vs cafe  ->  one name, Msg 15335
    //     Latin1_General_CS_AS           note  vs Note  ->  two names, both kept
    //
    // Case, then accents; width and kana sensitivity are two more flags on the
    // same collation name, and a binary collation is another answer again.
    // Folding the name here would be guessing at which of those the target
    // database chose, and each guess that comes up short refuses a valid plan
    // at the engine — the defect this is here to fix, one collation further
    // out. So the question asked is the one that is true under every
    // collation: a rename can only collide with a column of its own table.
    //
    // The cost is that a drop of a column nothing claims moves too, and
    // nothing between its old class and its new one can notice: a `Revoke`
    // names no column, and a column this plan drops is never a rename's
    // source, since the two come from different uids and no two baseline
    // columns share a name.
    let tables_claiming_a_column_name: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Column(ColumnChange::RenameColumn { table, .. }) => Some(table.clone()),
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(ColumnChange::AddColumn { .. })
            | Change::Column(ColumnChange::DropColumn { .. })
            | Change::Column(ColumnChange::AlterColumnType { .. })
            | Change::Column(ColumnChange::AlterColumnNullability { .. })
            | Change::Column(ColumnChange::AlterColumnDefault { .. })
            | Change::Column(ColumnChange::AlterColumnExpression { .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { .. })
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Column(ColumnChange::AddComputedColumn { .. })
            | Change::Column(ColumnChange::DropComputedColumn { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    tables_claiming_a_column_name
}

/// How many renames each `RenameColumn` waits on, which ranks it within its class.
/// It pushes the refusals for cycles of renames into `errs` in place: the
/// order of `errors` is part of the result.
fn rename_chain_depths(
    planned: &[PlannedChange],
    errs: &mut Vec<DiffError>,
) -> BTreeMap<Rename, usize> {
    // A column rename into a name another rename of the same table vacates
    // runs after it: skipped revisions that rename `b` to `c` and then `a` to
    // `b` leave a chain the uids would otherwise order, and `a -> b` while `b`
    // still stands is refused by both engines (DEC-541.1). Its depth is how many
    // links it waits on; the chain's far end, whose target is free, goes
    // first. A cycle, two columns trading names, has no order an engine takes
    // without a temporary name, so the plan says so instead of emitting one.
    let vacates: BTreeMap<(TableName, ColumnName), ColumnName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::RenameColumn {
                table, from, to, ..
            }) = &p.change
            {
                Some(((table.clone(), from.clone()), to.clone()))
            } else {
                None
            }
        })
        .collect();
    let mut cycles: BTreeSet<(TableName, BTreeSet<ColumnName>)> = BTreeSet::new();
    for ((table, from), to) in &vacates {
        let mut path = vec![from.clone()];
        let mut next = to.clone();
        while let Some(after) = vacates.get(&(table.clone(), next.clone())) {
            // Only the links that close the loop: a chain can lead into one.
            if let Some(start) = path.iter().position(|seen| *seen == next) {
                cycles.insert((table.clone(), path[start..].iter().cloned().collect()));
                break;
            }
            path.push(next.clone());
            next = after.clone();
        }
    }
    // What each rename waits on: the rename vacating its target. Under a
    // case-insensitive collation `a -> B` also waits on `b -> c`, since the
    // engine reads `B` as held by `b` (DEC-981.3). Such a link is added after
    // every exact one and only where it closes no cycle: on a case-sensitive
    // database `a -> B` beside `b -> A` is a valid pair, not a swap, and a
    // folded link must not hide an exact one it would loop through.
    let mut waits: BTreeMap<&Rename, BTreeSet<&Rename>> = vacates
        .iter()
        .map(|(rename, to)| {
            let exact = vacates.get_key_value(&(rename.0.clone(), to.clone()));
            (rename, exact.map(|(k, _)| k).into_iter().collect())
        })
        .collect();
    for (rename, to) in &vacates {
        let folded = crate::order::rename_order::case_folded(to.raw());
        for vacating in vacates.keys() {
            if vacating != rename
                && vacating.0 == rename.0
                && vacating.1 != *to
                && crate::order::rename_order::case_folded(vacating.1.raw()) == folded
                && !reaches(&waits, vacating, rename)
            {
                waits
                    .get_mut(rename)
                    .expect("every rename has an entry")
                    .insert(vacating);
            }
        }
    }
    // Its depth is the longest run of renames it waits on; the chain's far
    // end, whose target is free, goes first. A rename on an exact cycle is
    // refused below, so its depth is only cut short, never relied on.
    let mut memo = BTreeMap::new();
    let chain_depth: BTreeMap<Rename, usize> = vacates
        .keys()
        .map(|rename| {
            (
                rename.clone(),
                depth(&waits, rename, &mut BTreeSet::new(), &mut memo),
            )
        })
        .collect();
    for (table, columns) in cycles {
        errs.push(DiffError::ColumnRenameCycle { table, columns });
    }
    chain_depth
}

type Rename = (TableName, ColumnName);

fn reaches<'a>(
    waits: &BTreeMap<&'a Rename, BTreeSet<&'a Rename>>,
    from: &'a Rename,
    to: &Rename,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(at) = stack.pop() {
        if at == to {
            return true;
        }
        if seen.insert(at) {
            stack.extend(waits.get(at).into_iter().flatten().copied());
        }
    }
    false
}

fn depth<'a>(
    waits: &BTreeMap<&'a Rename, BTreeSet<&'a Rename>>,
    at: &'a Rename,
    open: &mut BTreeSet<&'a Rename>,
    memo: &mut BTreeMap<&'a Rename, usize>,
) -> usize {
    if let Some(&d) = memo.get(at) {
        return d;
    }
    if !open.insert(at) {
        return 0;
    }
    let d = waits
        .get(at)
        .into_iter()
        .flatten()
        .map(|&next| 1 + depth(waits, next, open, memo))
        .max()
        .unwrap_or(0);
    open.remove(at);
    memo.insert(at, d);
    d
}

/// The uids of the base's generated columns, whose drops `sort_class` puts first
/// in their class.
fn generated_columns_in_base(base: Side<'_>, index: &Indexes<'_>) -> BTreeSet<pbps_model::Uid> {
    // The base's generated columns, whose drops go first in their class: an
    // input column cannot be dropped while a generated column still reads it
    // (DEC-1168.1). By uid, not by name: a `DropColumn` names the column
    // under the declared table name, which a rename in the same plan makes
    // differ from the base's.
    let generated_in_base: BTreeSet<pbps_model::Uid> = base
        .schema
        .tables
        .iter()
        .flat_map(|(name, table)| {
            table
                .columns
                .iter()
                .filter(|(_, c)| match &c.value_source {
                    ValueSource::Generated(_) => true,
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_) => false,
                })
                .filter_map(|(column, _)| index.base.column_uid(&name.column(column)).cloned())
        })
        .collect();
    generated_in_base
}

/// The tables this plan writes rows into or deletes rows from: `late_attach` and
/// `sort_class` read it.
fn row_change_tables(planned: &[PlannedChange]) -> BTreeSet<TableName> {
    let row_tables: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| row_table(&p.change).cloned())
        .collect();
    row_tables
}

/// The columns this plan tightens or retypes: `sequence_release_ranks` reads them.
fn columns_held_back(planned: &[PlannedChange]) -> BTreeSet<ColumnRef> {
    // A column this plan tightens or retypes: a conversion to an identity
    // waits for both, since the identity is made `NOT NULL`-only and in the
    // column's type at the time (#1930 review).
    let held_back: BTreeSet<ColumnRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AlterColumnNullability {
                column,
                to_nullable: false,
                ..
            })
            | Change::Column(ColumnChange::AlterColumnType { column, .. }) = &p.change
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect();
    held_back
}

/// The identity columns whose sequence bounds are retuned before their retype:
/// `sort_class` puts their numbering change first in its class.
fn columns_whose_bounds_go_first(
    planned: &[PlannedChange],
    dialect: &(impl Dialect + ?Sized),
) -> BTreeSet<ColumnRef> {
    // A kept identity on a column this plan narrows: its sequence is
    // retuned before the retype (#2013 review, DEC-1921.1). The engine moves
    // an identity's sequence with its column, and measured on 16 and 18 it
    // refuses the retype while a bound set for the wider type stands. An
    // identity's column is an integer, so a retype that is not safe narrows
    // its range, and bounds declared for the narrower type fit the wider one
    // too; a widening keeps the retype first, where the old bounds fit.
    let bounds_first: BTreeSet<ColumnRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AlterColumnType {
                column, from, to, ..
            }) = &p.change
                && p.change.value_change().map_or_else(
                    || dialect.type_change_risk(from, to),
                    |values| dialect.value_change(&values, "").risk,
                ) != pbps_dialect::TypeChangeRisk::Safe
            {
                Some(column)
            } else {
                None
            }
        })
        .filter(|column| {
            planned.iter().any(|p| {
                matches!(
                    &p.change,
                    Change::Column(ColumnChange::AlterColumnNumbering {
                        column: c,
                        from: ValueSource::Identity(_),
                        to: ValueSource::Identity(_),
                        ..
                    }) if c == *column
                )
            })
        })
        .cloned()
        .collect();
    bounds_first
}

/// How deep each owned-sequence release sits in its chain of renames, which
/// `sort_class` ranks it by. It pushes the refusals for cycles into `errs` in
/// place: the order of `errors` is part of the result.
#[allow(clippy::needless_borrow)] // The block moved as it was; its borrows were of an owned local.
fn sequence_release_ranks(
    planned: &[PlannedChange],
    held_back: &BTreeSet<ColumnRef>,
    errs: &mut Vec<DiffError>,
) -> BTreeMap<ColumnRef, usize> {
    let (early_release, sequence_cycles) =
        sequence_release_depths(planned.iter().map(|p| &p.change), &held_back);
    for renames in sequence_cycles {
        errs.push(DiffError::SequenceRenameCycle { renames });
    }
    early_release
}

/// The attached partitions that wait among the row changes, with the rank of
/// their parent: `sort_class` and the sort key read it.
fn late_attach_ranks(
    declared: Side<'_>,
    attach_parents: &BTreeMap<TableName, TableName>,
    row_tables: &BTreeSet<TableName>,
    data_rank: &BTreeMap<TableName, usize>,
) -> BTreeMap<TableName, isize> {
    // An attach validates its parent's foreign keys over the rows it brings,
    // as `ADD FOREIGN KEY` does, so one whose parent references a table this
    // plan writes rows into, or attaches a table to, waits for them: among
    // the row changes, at its parent's rank, after every table the parent
    // references and before the rows of a table that references the parent.
    // Its own changes follow it there. Any other attach stays in class 7
    // (#1642 review).
    let late_attach: BTreeMap<TableName, isize> = attach_parents
        .iter()
        .filter(|(_, parent)| {
            declared.schema.tables.get(*parent).is_some_and(|t| {
                t.foreign_keys.values().any(|fk| {
                    fk.references_table != **parent
                        && (row_tables.contains(&fk.references_table)
                            || attach_parents.values().any(|p| *p == fk.references_table))
                })
            })
        })
        .map(|(table, parent)| {
            (
                table.clone(),
                data_rank.get(parent).map_or(0, |r| *r as isize),
            )
        })
        .collect();
    late_attach
}

/// The foreign-key graph a persistence switch is ordered by in `sort_class`, none
/// when the plan has no switch, and the depth of its deepest table.
fn persistence_fk_graph<'a>(
    planned: &[PlannedChange],
    declared: Side<'a>,
) -> (Option<ForeignKeyGraph<'a>>, usize) {
    // How deep each declared table sits in the foreign-key graph: 0 for one
    // that references no other, else one more than the deepest it does.
    // Switched to logged, the referenced table goes first, so by depth;
    // switched to unlogged, the referencing one, so by depth reversed: the
    // engine refuses either the other way round (measured on 16 and 18,
    // #1443). Self-references do not count, nor the keys inside a cycle,
    // which are dropped around the switches (#1488 review).
    // Only a persistence switch reads it, so a plan without one does not
    // build it (#1792).
    let fk_graph = planned
        .iter()
        .any(|p| {
            matches!(
                p.change,
                Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
            )
        })
        .then(|| ForeignKeyGraph::of(declared.schema));
    let deepest = fk_graph.as_ref().map_or(0, ForeignKeyGraph::deepest);
    (fk_graph, deepest)
}

/// The columns of a parent whose default, numbering source or NULL-ness this plan
/// changes: `follows_its_parent` reads it.
fn parent_columns_moved(planned: &[PlannedChange]) -> BTreeSet<(TableName, ColumnName)> {
    // A parent's column default, numbering source or NULL-ness this plan
    // changes, which the engine recurses into every partition: its own on that
    // column is set after it, or the parent's would overwrite it (#1687).
    let parent_moved: BTreeSet<(TableName, ColumnName)> = planned
        .iter()
        .filter_map(|p| {
            // A retype carries its NOT NULL change with it (#1692 review).
            if let Change::Column(ColumnChange::AlterColumnDefault { column, .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { column, .. })
            | Change::Column(ColumnChange::AlterColumnNullability { column, .. }) = &p.change
            {
                Some((column.table.clone(), column.name.clone()))
            } else if let Change::Column(ColumnChange::AlterColumnType {
                column,
                from_nullable,
                to_nullable,
                ..
            }) = &p.change
                && from_nullable != to_nullable
            {
                Some((column.table.clone(), column.name.clone()))
            } else {
                None
            }
        })
        .collect();
    parent_moved
}

/// The partitioned parents whose columns this plan changes: `sort_class` creates
/// their partitions after those changes.
fn parents_with_column_changes(
    planned: &[PlannedChange],
    declared: Side<'_>,
) -> BTreeSet<TableName> {
    // The parents whose columns this plan changes (#1687).
    let parent_columns_change: BTreeSet<TableName> = planned
        .iter()
        .filter(|p| parent_column(&p.change).is_some())
        .filter_map(|p| p.change.table())
        .filter(|t| {
            declared
                .schema
                .tables
                .get(*t)
                .is_some_and(|t| t.partition_by.is_some())
        })
        .cloned()
        .collect();
    parent_columns_change
}

/// The DEFAULT partitions a create of this plan moves rows out of: `sort_class`
/// orders their NOT NULL after those creates.
fn defaults_moved_out_of(planned: &[PlannedChange]) -> BTreeSet<TableName> {
    // The DEFAULT partitions a create in this plan moves rows out of (#1547).
    let moved_out_of: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Table(TableChange::CreateTable {
                beside_default: Some(default),
                ..
            }) = &p.change
            {
                Some(default.clone())
            } else {
                None
            }
        })
        .collect();
    moved_out_of
}

/// How deep each type sits in its chain of base domains, in the declarations and
/// in the base: `dependency_rank` reads them. It pushes the refusal for a cycle
/// into `errs` in place: the order of `errors` is part of the result.
fn domain_type_ranks(
    base: Side<'_>,
    declared: Side<'_>,
    errs: &mut Vec<DiffError>,
) -> (
    BTreeMap<pbps_model::TypeRef, isize>,
    BTreeMap<pbps_model::TypeRef, isize>,
) {
    let mut type_rank = BTreeMap::new();
    let mut drop_type_rank = BTreeMap::new();
    for (schema, ranks) in [
        (declared.schema, &mut type_rank),
        (base.schema, &mut drop_type_rank),
    ] {
        for name in schema.types.keys() {
            let mut at = name;
            let mut seen = BTreeSet::new();
            while let Some(pbps_model::ManagedType::Domain { base, .. }) = schema.types.get(at) {
                if !seen.insert(at.clone()) {
                    errs.push(DiffError::TypeLifecycleUnsupported {
                        name: name.clone(),
                        reason: "domain base dependency cycle".into(),
                    });
                    break;
                }
                let Some(next) = base.type_ref() else {
                    break;
                };
                at = next;
            }
            ranks.insert(name.clone(), seen.len() as isize);
        }
    }
    (type_rank, drop_type_rank)
}

/// The rank behind the last package change of the plan: the search ranks start
/// from it, and the sort key adds it to the rank of a created domain.
fn package_floor_of(
    planned: &[PlannedChange],
    extension_rank: &BTreeMap<pbps_model::Uid, isize>,
) -> isize {
    // A domain this plan creates may be built on a type a package of this plan
    // provides, created or moved here (the effects say so: its base needs the
    // package's change). The draft is what orders two changes of one class, and
    // the effects only check it, so the creation ranks behind the last package
    // change instead of tying with it, where the subject decided (#2696 ready
    // round, DEC-2447.12).
    let package_floor: isize = planned
        .iter()
        .filter_map(|p| {
            if let Change::Extension(ExtensionChange::CreateExtension { uid, .. })
            | Change::Extension(ExtensionChange::UpdateExtension { uid, .. }) = &p.change
            {
                Some(extension_rank.get(uid).copied().unwrap_or(0) + 1)
            } else if let Change::Extension(ExtensionChange::SetExtensionSchema { uid, .. }) =
                &p.change
            {
                Some(extension_rank.get(uid).copied().unwrap_or(0) + 2)
            } else {
                None
            }
        })
        .max()
        .unwrap_or(0);
    package_floor
}

fn built_domain(c: &Change) -> Option<&pbps_model::ManagedType> {
    let definition = if let Change::Type(TypeChange::CreateType { definition, .. })
    | Change::TypeValue(TypeValueChange::EnumRebuild {
        step: pbps_model::EnumRebuildStep::Create { definition, .. },
        ..
    }) = c
    {
        definition.as_ref()
    } else {
        return None;
    };
    matches!(definition, pbps_model::ManagedType::Domain { .. }).then_some(definition)
}

/// The search objects this plan creates, and those it alters in place: the search
/// ranks and the domain floor read them.
fn searches_in_the_plan(
    planned: &[PlannedChange],
) -> (
    BTreeSet<&pbps_model::SearchRef>,
    BTreeSet<&pbps_model::SearchRef>,
) {
    // A dictionary is created before the configuration that maps it, and the configuration
    // dropped before the dictionary (DEC-2728.2): the depth of the references among the
    // objects the plan holds on each side. An object created here that maps a dictionary a
    // package may provide starts behind the packages of this plan (the effects:
    // `package_input`), and what maps it sits one rank behind that, so the floor is composed
    // into the depth, not added to the sort key afterwards.
    let created_search: BTreeSet<&pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(SearchChange::CreateSearch { name, .. }) = &p.change {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // One altered in place maps what it now maps, so it waits for the packages as a create does.
    let altered_search: BTreeSet<&pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(
                SearchChange::AlterSearchMapping { name, .. }
                | SearchChange::AlterSearchDictionary { name, .. },
            ) = &p.change
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    (created_search, altered_search)
}

/// The depth of each search object among the references on each side, for the
/// declarations and for the base: the sort key ranks the search changes by them.
fn search_ranks(
    base: Side<'_>,
    declared: Side<'_>,
    package_floor: isize,
    created_search: &BTreeSet<&pbps_model::SearchRef>,
    altered_search: &BTreeSet<&pbps_model::SearchRef>,
) -> (
    BTreeMap<pbps_model::SearchRef, isize>,
    BTreeMap<pbps_model::SearchRef, isize>,
) {
    let search_depths = |schema: &Schema, floor: isize| -> BTreeMap<pbps_model::SearchRef, isize> {
        let start = |name: &pbps_model::SearchRef, object: &pbps_model::SearchObject| {
            if floor > 0
                && (created_search.contains(name) || altered_search.contains(name))
                && object.dictionaries().iter().any(|d| {
                    d.name.schema() != "pg_catalog" && declared.ids.search_uid(d).is_none()
                })
            {
                floor
            } else {
                0
            }
        };
        schema
            .search
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    search_depth(schema, name, &start, &mut BTreeSet::new()),
                )
            })
            .collect()
    };
    let (search_rank, drop_search_rank) = (
        search_depths(declared.schema, package_floor),
        search_depths(base.schema, 0),
    );
    (search_rank, drop_search_rank)
}

/// The rank the created domains that name a created search object sort behind, 0
/// for a plan whose domains name none: the sort key adds it to their rank.
#[allow(clippy::let_and_return)] // The block moved as it was; it ends in the binding it returns.
fn domain_floor_behind_searches(
    planned: &[PlannedChange],
    dialect: &(impl Dialect + ?Sized),
    created_search: &BTreeSet<&pbps_model::SearchRef>,
    search_rank: &BTreeMap<pbps_model::SearchRef, isize>,
) -> isize {
    // A domain's check or default may name a configuration this plan creates by a literal
    // (`'app.docs'::regconfig`), which the engine resolves when the domain is created: the
    // same literal scan the scratch order uses (`may_call`). The two share a class, so the
    // domains rank behind every created search object, all of them together, which keeps the
    // order among them (a chain of domains is ranked by its bases). A plan whose domains name
    // none leaves them where they were.
    let names_a_search = planned
        .iter()
        .filter_map(|p| built_domain(&p.change))
        .any(|domain| {
            let pbps_model::ManagedType::Domain {
                default, checks, ..
            } = domain
            else {
                return false;
            };
            created_search.iter().any(|created| {
                default
                    .iter()
                    .chain(checks.values())
                    .any(|text| dialect.generation_may_call(text, created.name.name()))
            })
        });
    let domain_floor = if names_a_search {
        1 + created_search
            .iter()
            .filter_map(|created| search_rank.get(*created))
            .max()
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    domain_floor
}

/// What the sort of `diff_partial_rebuilding` reads, built once by `sort_facts` before
/// the sort takes the plan mutably. Every field is owned: a fact that borrows
/// `planned` is only an input to another fact and cannot be kept here.
struct SortFacts<'a> {
    /// The tables a `RenameColumn` of this plan claims a column name on: `frees_a_renamed_column` reads it.
    tables_claiming_a_column_name: BTreeSet<TableName>,
    /// The uids of the base's generated columns: `class` puts their drops first in their class.
    generated_in_base: BTreeSet<pbps_model::Uid>,
    /// The tables this plan writes rows into or deletes rows from: `class` orders the tightening and the numbering of their columns after the rows.
    row_tables: BTreeSet<TableName>,
    /// The columns this plan puts an identity on: `class` leaves the tightening of one ahead of the inserts of a table that deletes no rows.
    identified: BTreeSet<ColumnRef>,
    /// The tables this plan inserts rows into: `class` reads it with `late`.
    inserted: BTreeSet<TableName>,
    /// The tables a plan puts a numbering source on and both deletes and inserts rows of: `class` orders their inserts after the source.
    late: BTreeSet<TableName>,
    /// The columns this plan puts a numbering source on: `class` orders them after the deletes.
    installed: BTreeSet<ColumnRef>,
    /// The identity columns whose sequence bounds are retuned before their retype: `class` puts their numbering change first.
    bounds_first: BTreeSet<ColumnRef>,
    /// How deep each owned-sequence release sits in its chain of renames: `class` ranks it by that.
    early_release: BTreeMap<ColumnRef, usize>,
    /// The attached partitions that wait among the row changes, with the rank of their parent: `class` and `key` read it.
    late_attach: BTreeMap<TableName, isize>,
    /// The foreign-key graph a persistence switch is ordered by, none without a switch: `class` reads it.
    fk_graph: Option<ForeignKeyGraph<'a>>,
    /// The depth of the deepest table of `fk_graph`: `class` reads it for a switch to unlogged.
    deepest: usize,
    /// The columns of a parent whose default, numbering source or NULL-ness this plan changes: `follows_its_parent` reads it.
    parent_moved: BTreeSet<(TableName, ColumnName)>,
    /// The partitioned parents whose columns this plan changes: `class` creates their partitions after those changes.
    parent_columns_change: BTreeSet<TableName>,
    /// The DEFAULT partitions a create of this plan moves rows out of: `class` orders their NOT NULL after those creates.
    moved_out_of: BTreeSet<TableName>,
    /// The replica identities whose target stands before the plan: `class` sets them first.
    early_identity: BTreeSet<(Uid, Option<ReplicaIdentity>)>,
    /// How many renames each `RenameColumn` waits on: `class` ranks it by that.
    chain_depth: BTreeMap<Rename, usize>,
    /// The declarations: `follows_its_parent`, `class` and `key` read the partitioning and the column order of their tables.
    declared: Side<'a>,
    /// The creation rank of each module among the modules: `key` ranks the module creates by it.
    create_rank: BTreeMap<ModuleId, usize>,
    /// The drop rank of each module among the modules: `key` ranks the module drops by it.
    drop_rank: BTreeMap<ModuleId, usize>,
    /// The rank of the tables that receive rows or attaches: `key` ranks their changes by it.
    data_rank: BTreeMap<TableName, usize>,
    /// How deep each role sits among the roles dropped together: `key` drops a parent before its members.
    role_rank: BTreeMap<pbps_model::RoleName, usize>,
    /// How deep each type sits in its chain of base domains, in the declarations: `key` ranks the type changes by it.
    type_rank: BTreeMap<pbps_model::TypeRef, isize>,
    /// How deep each type sits in its chain of base domains, in the base: `key` ranks the type drops by it.
    drop_type_rank: BTreeMap<pbps_model::TypeRef, isize>,
    /// The depth of each package change among the packages it requires: `key` ranks the extension changes by it.
    extension_rank: BTreeMap<pbps_model::Uid, isize>,
    /// The depth of each search object among the references in the declarations: `key` ranks the search changes by it.
    search_rank: BTreeMap<pbps_model::SearchRef, isize>,
    /// The depth of each search object among the references in the base: `key` ranks the search drops by it.
    drop_search_rank: BTreeMap<pbps_model::SearchRef, isize>,
    /// The rank behind the last package change of the plan: `key` adds it to the rank of a created domain.
    package_floor: isize,
    /// The rank the created domains that name a created search object sort behind: `key` adds it to their rank.
    domain_floor: isize,
}

impl SortFacts<'_> {
    /// Whether `c` is the drop that frees a column name a `RenameColumn` of this plan claims:
    /// `class` asks it of every change.
    // Whether this drop can be what frees a name a `RenameColumn` above is
    // waiting for. A column drop and nothing else: no other change gives up a
    // column name.
    fn frees_a_renamed_column(&self, c: &Change) -> bool {
        match c {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => false,
            Change::Column(ColumnChange::DropColumn { column, .. }) => {
                self.tables_claiming_a_column_name.contains(&column.table)
            }
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(ColumnChange::AddColumn { .. })
            | Change::Column(ColumnChange::RenameColumn { .. })
            | Change::Column(ColumnChange::AlterColumnType { .. })
            | Change::Column(ColumnChange::AlterColumnNullability { .. })
            | Change::Column(ColumnChange::AlterColumnDefault { .. })
            | Change::Column(ColumnChange::AlterColumnExpression { .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { .. })
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Column(ColumnChange::AddComputedColumn { .. })
            | Change::Column(ColumnChange::DropComputedColumn { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => false,
        }
    }

    /// Whether `c` is a setting on a partition that waits among the row changes: `class` asks it.
    fn follows_late_attach(&self, c: &Change) -> bool {
        if let Change::Partition(PartitionChange::SetPartitionDefault { table, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { table, .. })
        | Change::TableProperty(TablePropertyChange::SetTablePersistence { table, .. })
        | Change::TableProperty(TablePropertyChange::SetStorageParameters { table, .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { table, .. }) = c
        {
            self.late_attach.contains_key(table)
        } else {
            false
        }
    }

    /// Whether `c` sets a partition's default or NOT NULL on a column its parent's change moves:
    /// `class` asks it.
    fn follows_its_parent(&self, c: &Change) -> bool {
        let Self {
            declared,
            parent_moved,
            ..
        } = self;
        let (Change::Partition(PartitionChange::SetPartitionDefault { table, column, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { table, column, .. })) = c
        else {
            return false;
        };
        declared
            .schema
            .tables
            .get(table)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|of| parent_moved.contains(&(of.parent.clone(), column.clone())))
    }

    /// The ordering class of `c` and a rank inside it: the first two parts of the sort key.
    // The class, and a rank inside it, so a change can sit between two
    // classes without a new ordinal shifting every one below it — the cost
    // `order_key`'s own doc names.
    fn class(&self, c: &Change) -> (u8, usize) {
        let Self {
            generated_in_base,
            row_tables,
            identified,
            inserted,
            late,
            installed,
            bounds_first,
            early_release,
            late_attach,
            fk_graph,
            deepest,
            parent_columns_change,
            moved_out_of,
            early_identity,
            chain_depth,
            declared,
            ..
        } = self;
        if matches!(
            c,
            Change::Type(TypeChange::CreateType { .. })
                | Change::Search(SearchChange::CreateSearch { .. })
                | Change::Search(SearchChange::AlterSearchMapping { .. })
                | Change::Search(SearchChange::AlterSearchDictionary { .. })
                | Change::TypeValue(TypeValueChange::AddEnumValue { .. })
                | Change::TypeValue(TypeValueChange::RenameEnumValue { .. })
                | Change::TypeValue(TypeValueChange::EnumRebuild {
                    step: pbps_model::EnumRebuildStep::Create { .. },
                    ..
                })
        ) {
            return (7, 0);
        }
        // The columns move and the type is swapped among the type-changing
        // alterations of class 9, ranked with a retype (DEC-1635.7).
        if matches!(
            c,
            Change::TypeValue(TypeValueChange::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Retype { .. }
                    | pbps_model::EnumRebuildStep::Swap { .. },
                ..
            })
        ) {
            return (COLUMN_ALTERATIONS, 1);
        }

        // After every module and its PUBLIC decision, then policies before
        // either flag. Keep the established class ordinals (DEC-1172.1).
        if matches!(c, Change::Policy(PolicyChange::CreatePolicy { .. })) {
            return (14, 2);
        }
        if matches!(c, Change::Policy(PolicyChange::SetRowSecurity { .. })) {
            return (14, 3);
        }
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && bounds_first.contains(column)
        {
            return (COLUMN_ALTERATIONS, 0);
        }
        if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = c
            && late_attach.contains_key(table)
        {
            return (ROW_DELETIONS - 1, 1);
        }
        if self.follows_late_attach(c) {
            return (ROW_DELETIONS - 1, 2);
        }
        // A DEFAULT partition's own NOT NULL, tightened in a plan that moves
        // rows out of it, comes after every create that moves them, whatever
        // class the create lands in (7, or (9, 5) under a parent whose columns
        // change): the rows the move takes may be the ones holding NULL there,
        // and the engine scans what is left. Its own checks and uniques are of
        // class 13, after every create already (#1763 review).
        if let Change::Partition(PartitionChange::SetPartitionNotNull {
            table,
            not_null: true,
            ..
        }) = c
            && moved_out_of.contains(table)
        {
            return (COLUMN_ALTERATIONS, 6);
        }
        if self.follows_its_parent(c) {
            return (COLUMN_ALTERATIONS, 4);
        }
        if let Change::TableProperty(TablePropertyChange::SetTablePersistence {
            table,
            unlogged,
            ..
        }) = c
        {
            let depth = fk_graph.as_ref().map_or(0, |g| g.depth(table));
            return (
                COLUMN_ALTERATIONS,
                10 + if *unlogged { deepest - depth } else { depth },
            );
        }
        // A replica identity whose target stands before the plan is set
        // first, under the table's old name: before the drops of class 2 can
        // take its old index, which would leave the table identifying no row
        // (DEC-1444.1).
        if let Change::TableProperty(TablePropertyChange::SetReplicaIdentity { uid, to, .. }) = c
            && early_identity.contains(&(uid.clone(), to.clone()))
        {
            return (0, 1);
        }
        // A tightening of a table whose rows this plan writes or deletes runs
        // after them, at the end of the deletes' class: the rows may be what
        // fills or removes its NULLs, and no row change needs the column NOT
        // NULL first. Still before the additions of class 13, a primary key
        // among them, which SQL Server refuses over a nullable column (#1367).
        //
        // An identity this plan puts on follows its tightening there, since
        // the engine refuses one on a nullable column: after the deletes,
        // which may remove the rows holding NULL. Any source put on follows
        // them too, since it continues past the values left, and those a
        // delete removes may be the greatest. The table's own inserts follow
        // the source, since a declared row leaves the numbered column out to
        // take its number. Where the plan inserts rows into the table and
        // deletes none, the source, and an identity's tightening, stay in
        // class 9 ahead of the inserts, where a referencing table's rows can
        // still follow them (#1930 review).
        let early =
            |column: &ColumnRef| inserted.contains(&column.table) && !late.contains(&column.table);
        if let Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        }) = c
            && row_tables.contains(&column.table)
            && !(identified.contains(column) && early(column))
        {
            return (ROW_DELETIONS, 2);
        }
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && installed.contains(column)
            && row_tables.contains(&column.table)
            && !early(column)
        {
            return (ROW_DELETIONS, 3);
        }
        if let Change::Row(RowChange::InsertRow { table, .. }) = c
            && late.contains(table)
        {
            return (ROW_DELETIONS, 4);
        }
        // An owned sequence given up, or renamed, frees its name before the
        // tables and columns this plan creates, one of which may take it:
        // after the drops of class 6, which free names too, and before
        // `CreateTable` (#1930 review). None needs anything class 9 does
        // first: giving one up drops a default and a sequence, a rename keeps
        // the sequence, and a conversion to an identity needs the column
        // `NOT NULL`, which a column this plan does not tighten already is.
        // An identity put on a column retyped after it moves with it.
        //
        // Among them, a rename to a name another of them frees follows it,
        // down the chain (#1930 review).
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && let Some(depth) = early_release.get(column)
        {
            return (6, 1 + depth);
        }
        // A generated column is dropped before the ordinary columns of its
        // class, and added after every column change of the class beyond:
        // the additions, and the in-place alterations of class 9. Measured,
        // the engine refuses to drop a column a generated column reads,
        // refuses a generation expression over a column that is not there yet,
        // and refuses to retype one a generated column reads. A generated
        // column never reads another, and nothing in class 9 needs one that is
        // new, so one layer each way is the whole order (DEC-1168.1).
        let drops_generated = matches!(c, Change::Column(ColumnChange::DropColumn { uid, .. }) if generated_in_base.contains(uid));
        if self.frees_a_renamed_column(c) {
            // The layer holds here too: every drop of a table that renames a
            // column comes to this class, the generated one and its input
            // alike, so the generated one keeps its place ahead.
            if drops_generated {
                return (2, 2);
            }
            // Between the constraint and index drops of class 2 — a column a
            // check or an index names cannot be dropped while they stand —
            // and the `RenameColumn` of class 3 that is waiting for its name.
            //
            // Measured on the pinned images, this is the difference between a
            // reviewed plan and one the engine refuses: SQL Server answers
            // `sp_rename` with `Msg 15335, The new name 'note' is already in
            // use as a COLUMN name and would cause a duplicate that is not
            // permitted`, PostgreSQL with `column "note" of relation "s"
            // already exists` (DECISIONS 474, issue #398).
            //
            // Only the drop that frees a claimed name moves. A rename into a
            // name nothing here gives up is refused by `resolve` as an
            // occupied target and never reaches this sort, which is the
            // property `a_single_revision_cannot_rename_into_an_occupied_baseline_name`
            // holds.
            return (2, 3);
        }
        if drops_generated {
            return (order_key(c), 0);
        }
        if let Change::Column(ColumnChange::AddColumn { column, .. }) = c
            && (match &column.value_source {
                ValueSource::Generated(_) => true,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => false,
            })
        {
            return (COLUMN_ALTERATIONS, 2);
        }
        // A computed column comes down after every index and constraint drop
        // of class 2, the ones that free a renamed column's name (2, 3)
        // included, and goes up after every alteration of class 9: it reads
        // the columns, in their final type, and nothing in either class reads
        // it (#1174).
        if matches!(c, Change::Column(ColumnChange::DropComputedColumn { .. })) {
            return (2, 4);
        }
        // A package's own objects are what a column, a domain or a module of
        // the plan may use (DEC-2447.12), and its create, update and move share
        // the type class with them, so they go first inside it: nothing else
        // there is ordered against them, and a tie would be decided by a random
        // uid.
        if matches!(
            c,
            Change::Extension(ExtensionChange::CreateExtension { .. })
                | Change::Extension(ExtensionChange::UpdateExtension { .. })
                | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        ) {
            return (order_key(c), 0);
        }
        if matches!(c, Change::Column(ColumnChange::AddComputedColumn { .. })) {
            return (COLUMN_ALTERATIONS, 3);
        }
        // A detach comes after every table drop of its class: it claims the
        // declared names of what it keeps, and a dropped table's index or
        // key may be what holds one now. A drop claims no name, so nothing
        // waits the other way (#1544).
        if matches!(
            c,
            Change::Partition(PartitionChange::DetachPartition { .. })
        ) {
            return (order_key(c), 2);
        }
        // A partition is created after every parent, whose columns, keys and
        // indexes the engine gives it as it is made (#1170). Under a parent
        // whose columns this plan changes, after those changes too: its own
        // default may name a column the parent adds, and a parent's default
        // set or dropped afterwards would overwrite its own (#1692 review).
        if let Change::Table(TableChange::CreateTable { table, .. }) = c
            && let Some(of) = &table.partition_of
        {
            if parent_columns_change.contains(&of.parent) {
                return (COLUMN_ALTERATIONS, 5);
            }
            return (order_key(c), 2);
        }
        // A partitioned parent's index first in its class, before any
        // partition's own: built after one, it would take it as its clone
        // (#1737 review, DEC-1688.1). First rather than the partition's own
        // last, which would put it after a replica identity naming it. A
        // parent's index reads only its columns, all of earlier classes.
        if let Change::Index(IndexChange::AddIndex { table, .. }) = c
            && declared
                .schema
                .tables
                .get(table)
                .is_some_and(|t| t.partition_by.is_some())
        {
            return (order_key(c), 0);
        }
        if let Change::Column(ColumnChange::RenameColumn { table, from, .. }) = c {
            let depth = chain_depth
                .get(&(table.clone(), from.clone()))
                .copied()
                .unwrap_or(0);
            (order_key(c), 1 + depth)
        } else {
            (order_key(c), 1)
        }
    }

    /// The sort key of `p`: its class, a rank among the dependencies of its kind, then the
    /// tiebreakers. `sort_draft` calls it once per change.
    // The tiebreaker within an ordering class is the table name, then the
    // change's rendering. Debug output alone would sort by uid, which is random
    // at mint time — the plan would be correct but differently ordered per
    // project, and a reviewer diffing two plan.sql files would see noise.
    //
    // `subject()` does not speak for one side consistently: `RenameTable` and
    // `RenameRole` answer with the name the object is losing, everything else
    // with the name it will have. Two changes to one object are therefore
    // sorted against two different spellings of it, and the alphabet — not the
    // dependency — decides which wins. Anything with a real order between them
    // belongs in ordering classes or explicit dependency edges; this
    // tiebreaker cannot express it.
    fn key(&self, p: &PlannedChange) -> (u8, usize, isize, String, usize, String) {
        let (class, within) = self.class(&p.change);
        (
            class,
            within,
            if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = &p.change
                && let Some(rank) = self.late_attach.get(table)
            {
                *rank
            } else if let Change::Extension(ExtensionChange::CreateExtension { uid, .. })
            | Change::Extension(ExtensionChange::DropExtension { uid, .. })
            | Change::Extension(ExtensionChange::UpdateExtension { uid, .. }) = &p.change
            {
                // Behind the packages of this plan it requires, or that require
                // it for a drop (DEC-2447.6, DEC-2447.8).
                self.extension_rank.get(uid).copied().unwrap_or(0)
            } else if let Change::Search(
                SearchChange::CreateSearch { name, .. }
                | SearchChange::AlterSearchMapping { name, .. }
                | SearchChange::AlterSearchDictionary { name, .. },
            ) = &p.change
            {
                // Behind the packages and the dictionaries of this plan it maps.
                self.search_rank.get(name).copied().unwrap_or(0)
            } else if let Change::Search(SearchChange::DropSearch { name, .. }) = &p.change {
                -self.drop_search_rank.get(name).copied().unwrap_or(0)
            } else if let Change::Extension(ExtensionChange::SetExtensionSchema { uid, .. }) =
                &p.change
            {
                // One rank behind the package's own update, whose result the
                // move starts from: the two rewrite one package, and the draft
                // is what orders two writes (DEC-2447.11).
                self.extension_rank.get(uid).copied().unwrap_or(0) + 1
            } else {
                dependency_rank(
                    &p.change,
                    &self.create_rank,
                    &self.drop_rank,
                    &self.data_rank,
                    &self.role_rank,
                    &self.type_rank,
                    &self.drop_type_rank,
                ) + if built_domain(&p.change).is_some() {
                    self.package_floor.max(self.domain_floor)
                } else {
                    0
                }
            },
            rebuild_subject(&p.change),
            // Columns added to one table in the order they are declared:
            // the engine appends each, so this is the table's column order,
            // which a pull writes and a positional `INSERT` binds to. Left
            // to the rendering below, the column's random uid decided it
            // (#2080).
            if let Change::Column(ColumnChange::AddColumn { table, name, .. }) = &p.change {
                self.declared
                    .schema
                    .tables
                    .get(table)
                    .and_then(|t| t.columns.get_index_of(name))
                    .unwrap_or(0)
            } else {
                0
            },
            tie_break_text(&p.change),
        )
    }
}

/// The facts the sort reads, computed in the order the passes ran before the split:
/// the three that push into `errs` are `rename_chain_depths`, `sequence_release_ranks`
/// and `domain_type_ranks`, in that order. The plan and the list go in by value and
/// come back, because the sort and the finish go on with them.
#[allow(clippy::too_many_arguments)] // Each fact reads one of these; the parent function is allowed the same.
fn sort_facts<'a>(
    planned: Vec<PlannedChange>,
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &(impl Dialect + ?Sized),
    module_deps: pbps_model::module::ModuleDeps,
    index: Indexes<'a>,
    early_identity: BTreeSet<(Uid, Option<ReplicaIdentity>)>,
    mut errs: Vec<DiffError>,
) -> (SortFacts<'a>, Vec<PlannedChange>, Vec<DiffError>) {
    let (create_rank, drop_rank) = module_ranks(base, declared, dialect, &module_deps);
    let (attach_parents, data_rank) = attach_parents_and_data_rank(&planned, declared);
    // Roles that are dropped together are ordered parent before member.
    let role_rank = member_depth(&planned);
    let tables_claiming_a_column_name = column_name_claims(&planned);
    let chain_depth = rename_chain_depths(&planned, &mut errs);
    let generated_in_base = generated_columns_in_base(base, &index);
    let row_tables = row_change_tables(&planned);
    let identified = identities_put_on(planned.iter().map(|p| &p.change));
    let inserted = tables_with_inserts(planned.iter().map(|p| &p.change));
    let late = numbering_after_deletes(planned.iter().map(|p| &p.change));
    let installed = numbering_put_on(planned.iter().map(|p| &p.change));
    let held_back = columns_held_back(&planned);
    let bounds_first = columns_whose_bounds_go_first(&planned, dialect);
    let early_release = sequence_release_ranks(&planned, &held_back, &mut errs);
    let late_attach = late_attach_ranks(declared, &attach_parents, &row_tables, &data_rank);
    // A function a computed column calls is dropped after the column, by a
    // connected plan's pass over the catalog's own edges
    // (`order_computed_by_edges`, DEC-1431.1). An offline plan is never
    // applied, and a text scan here took one function for another (#1174
    // review), so the differ moves nothing for it.
    let (fk_graph, deepest) = persistence_fk_graph(&planned, declared);
    let parent_moved = parent_columns_moved(&planned);
    let parent_columns_change = parents_with_column_changes(&planned, declared);
    let moved_out_of = defaults_moved_out_of(&planned);
    let (type_rank, drop_type_rank) = domain_type_ranks(base, declared, &mut errs);
    let extension_rank = crate::extensions::depths(planned.iter().map(|p| &p.change));
    let package_floor = package_floor_of(&planned, &extension_rank);
    let (created_search, altered_search) = searches_in_the_plan(&planned);
    let (search_rank, drop_search_rank) = search_ranks(
        base,
        declared,
        package_floor,
        &created_search,
        &altered_search,
    );
    let domain_floor =
        domain_floor_behind_searches(&planned, dialect, &created_search, &search_rank);
    (
        SortFacts {
            tables_claiming_a_column_name,
            generated_in_base,
            row_tables,
            identified,
            inserted,
            late,
            installed,
            bounds_first,
            early_release,
            late_attach,
            fk_graph,
            deepest,
            parent_moved,
            parent_columns_change,
            moved_out_of,
            early_identity,
            chain_depth,
            declared,
            create_rank,
            drop_rank,
            data_rank,
            role_rank,
            type_rank,
            drop_type_rank,
            extension_rank,
            search_rank,
            drop_search_rank,
            package_floor,
            domain_floor,
        },
        planned,
        errs,
    )
}

/// What `diff_partial_rebuilding` does after the sort: the rename order and the
/// last-column order, the drop requirements, the risks and the unsafe label uses.
/// It pushes into `errs` in place: the order of `errors` is part of the result.
fn finish_sort(
    mut planned: Vec<PlannedChange>,
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    renames: Renames,
    mut errs: Vec<DiffError>,
) -> Diffed {
    for cycle in crate::order::rename_order::order(&mut planned, base, &renames, dialect) {
        errs.push(DiffError::TableRenameCycle { renames: cycle });
    }
    for (table, column, block) in
        crate::order::last_column::order(&mut planned, base, declared, dialect)
    {
        errs.push(DiffError::LastColumnReplacementUnorderable {
            table,
            column,
            block,
        });
    }
    let mut changes = ChangeSet { changes: planned };
    if let Err(reason) = crate::drop_requirements::assign(&mut changes, base.ids, declared.ids)
        .and_then(|()| crate::drop_requirements::validate(&changes, base.ids, declared.ids))
    {
        errs.push(DiffError::DropRequirement { reason });
    }
    // Replacement risk belongs to both policy parts, including in partial
    // reports. Reuse the same UID/name pairing as final plan admission.
    let risks = crate::order::generated::risks(&changes, declared.ids, dialect);
    for (part, risks) in changes.changes.iter_mut().zip(risks) {
        part.risks = risks;
    }
    errs.extend(crate::enum_labels::unsafe_uses(&changes, declared, dialect));
    Diffed {
        changes,
        errors: errs,
    }
}

/// What one comparison produced: the changes the differ could express, and the
/// differences it could not.
///
/// Not a `Result`, deliberately — the two are not alternatives. A comparison
/// can find both at once, and the type that says so is what stops a caller
/// discarding one to obtain the other.
#[derive(Debug, Default)]
pub struct Diffed {
    pub changes: ChangeSet,
    pub errors: Vec<DiffError>,
}

/// Position in the dependency order, by table name.
fn rank_of_tables(order: &[TableName]) -> BTreeMap<TableName, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// Position in the dependency order, by identity.
fn rank_of(order: &[ModuleId]) -> BTreeMap<ModuleId, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// Where a change sorts *within* its ordering class.
///
/// Modules, reference rows and foreign keys each have a dependency order among
/// their class; everything else is zero and keeps the tiebreakers that were
/// already there. In every family the removing direction is the reverse of the
/// creating one, so a dependent goes before the thing it depends on.
///
/// Modules and rows carry a computed rank because their dependencies form a
/// graph. A foreign key's is a two-level layering — it depends on candidate
/// keys, and nothing depends on it — so a constant says it exactly.
fn dependency_rank(
    change: &Change,
    create_rank: &BTreeMap<ModuleId, usize>,
    drop_rank: &BTreeMap<ModuleId, usize>,
    data_rank: &BTreeMap<TableName, usize>,
    role_rank: &BTreeMap<pbps_model::RoleName, usize>,
    type_rank: &BTreeMap<pbps_model::TypeRef, isize>,
    drop_type_rank: &BTreeMap<pbps_model::TypeRef, isize>,
) -> isize {
    match change {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
            unreachable!("rebuild operations are expanded by order, never ranked by the differ")
        }
        Change::Module(ModuleChange::CreateModule { id, .. }) | Change::Module(ModuleChange::AlterModule { id, .. }) => {
            create_rank.get(id).map_or(0, |r| *r as isize)
        }
        Change::Module(ModuleChange::DropModule { id, .. }) => -(drop_rank.get(id).map_or(0, |r| *r as isize)),
        // Module ranks can be arbitrarily deep. A fixed -1 would leave a
        // policy behind a dropped routine it calls (DEC-1173.2).
        Change::Policy(PolicyChange::DropPolicy { .. }) => -(drop_rank.len() as isize) - 1,
        Change::TypeValue(TypeValueChange::AddEnumValue { ordinal, .. }) | Change::TypeValue(TypeValueChange::RenameEnumValue { ordinal, .. }) => {
            *ordinal as isize
        }
        // After every label change of class 7, which it may name.
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Loosen,
            from,
            to,
            ..
        }) if domain_sets_default(from, to) => isize::MAX,
        Change::TypeValue(TypeValueChange::AlterDomain { .. }) => 0,
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { ordinal, .. },
            ..
        }) => *ordinal as isize,
        // With a retype: after the defaults taken off for it (-2) and before
        // the ones written for the type it ends with (0). The swap follows
        // every column of its type because they share a subject and rank and
        // the rendering that breaks the tie names `Retype` before `Swap`
        // (pinned by `the_whole_plan_drops_and_restores_what_names_a_moved_column_and_orders_the_steps`).
        Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => -1,
        Change::Type(TypeChange::CreateType { name, .. }) | Change::Type(TypeChange::RenameType { to: name, .. }) => {
            *type_rank.get(name).unwrap_or(&0)
        }
        Change::Type(TypeChange::DropType { name, .. }) => -drop_type_rank.get(name).copied().unwrap_or(0),
        Change::Table(TableChange::CreateTable { .. })
        // Ranked by their depth where the sort key is built (`search_rank`).
        | Change::Search(SearchChange::CreateSearch { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Search(SearchChange::DropSearch { .. })
        | Change::Extension(ExtensionChange::CreateExtension { .. })
        | Change::Extension(ExtensionChange::DropExtension { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        | Change::Table(TableChange::DropTable { .. })
        | Change::Partition(PartitionChange::DetachPartition { .. })
        | Change::Partition(PartitionChange::AttachPartition { .. })
        | Change::Table(TableChange::RenameTable { .. })
        | Change::Column(ColumnChange::AddColumn { .. })
        | Change::Column(ColumnChange::DropColumn { .. })
        | Change::Column(ColumnChange::RenameColumn { .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { .. })
        | Change::TableProperty(TablePropertyChange::SetComment { .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { .. })
        | Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
        | Change::Policy(PolicyChange::CreatePolicy { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::TableProperty(TablePropertyChange::SetStorageParameters { .. })
        | Change::Constraint(ConstraintChange::DropUnique { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { .. })
        | Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::Constraint(ConstraintChange::DropCheck { .. })
        | Change::Index(IndexChange::DropIndex { .. })
        | Change::Column(ColumnChange::AddComputedColumn { .. })
        | Change::Column(ColumnChange::DropComputedColumn { .. })
        | Change::Row(RowChange::SetDataMode { .. })
        | Change::Row(RowChange::SetEnsureKeys { .. }) => 0,
        // The clustered index goes in ahead of the rest of the addition
        // class. Building it rebuilds every nonclustered index already on the
        // table, whose row locators become its key, so an index added before
        // it is built twice (#1178). Correctness needs nothing here: the old
        // clustered index is gone by now, since every drop is in class 2.
        //
        // A key with no layout to speak of — every PostgreSQL key, whose
        // `nonclustered` is always false — moves ahead of its siblings too,
        // which orders nothing that depends on it.
        Change::Constraint(ConstraintChange::SetPrimaryKey {
            to: Some(_),
            nonclustered: false,
            ..
        })
        | Change::Constraint(ConstraintChange::AddUnique {
            clustered: true, ..
        })
        | Change::Index(IndexChange::AddIndex {
            clustered: true, ..
        }) => -1,
        Change::Constraint(ConstraintChange::SetPrimaryKey { .. }) | Change::Constraint(ConstraintChange::AddUnique { .. }) | Change::Index(IndexChange::AddIndex { .. }) => 0,
        // After every addition of its class, the index it names among them,
        // and the foreign keys too: nothing in the class reads it.
        Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => 2,
        // A foreign key needs the key it references to exist, so it goes last
        // in the addition class and first in the drop class. Measured on the
        // pinned image, both halves are real: added before its key, the engine
        // refuses with 1776, "There are no primary or candidate keys in the
        // referenced table ... that match the referencing column list"; and
        // the key cannot be dropped while the foreign key stands (3727 for a
        // `UNIQUE` constraint, 3723 for a unique index).
        //
        // A constant, not a rank computed from the referenced columns, because
        // the dependency here is a total layering rather than a graph: every
        // supplier of a candidate key — `SetPrimaryKey`, `AddUnique` and
        // `AddIndex`, since a foreign key may reference a plain unique index —
        // is above every foreign key, and no foreign key supplies a key to
        // another. Matching suppliers by name would add a way to fail (a
        // supplier not recognised, and the ordering silently back) and buy
        // nothing. Everything else in both classes keeps rank 0 and the
        // tiebreaker it had.
        Change::Constraint(ConstraintChange::AddForeignKey { .. }) => 1,
        Change::Constraint(ConstraintChange::DropForeignKey { .. }) => -1,
        // A column's new type goes in before a default written for it. They
        // share class 9 and the tiebreaker below is the change's rendering,
        // which sorts `AlterColumnDefault` ahead of `AlterColumnType` by the
        // alphabet — so a default that only fits the new type was set against
        // the old one. Measured, PostgreSQL refuses `SET DEFAULT 'abc'` on an
        // `integer` column with "invalid input syntax for type integer", and
        // the plan that widened the column to `text` in the next statement
        // rolled back.
        //
        // A constant, like the foreign key's above and for the same reason:
        // the dependency is a layering, not a graph. A type change never needs
        // a default that is already there, and the nullability travels inside
        // the type change rather than beside it.
        Change::Column(ColumnChange::AlterColumnType { .. }) => -1,
        // And the old default goes before the type, which makes the three
        // phases a column can need one rank each: drop the default the old
        // type gave meaning to, change the type, install the default written
        // for the new one. Measured, SQL Server refuses the middle statement
        // while the first one's constraint stands (5074 with 4922 behind it),
        // and `diff_columns` splits a *replaced* default into these two halves
        // when the type moves, so both ranks have something to order.
        Change::Column(ColumnChange::AlterColumnDefault { to: None, .. }) => -2,
        Change::Column(ColumnChange::AlterColumnDefault { to: Some(_), .. }) => 0,
        // After any type change, as a new default is: the expression is
        // recomputed under the column's final type (DEC-1168.1).
        Change::Column(ColumnChange::AlterColumnExpression { .. }) => 0,
        // A numbering source (#1889, DEC-1889.1). One given up, and an
        // identity turned into a sequence, go first: ahead of the relaxation
        // its column may also take, since measured on 16 and 18 `DROP NOT
        // NULL` on an identity column is refused ("is an identity column"),
        // and ahead of the plain default it may give way to, which is set at
        // 0 and which giving up a sequence's default would drop (#1930
        // review). Anything else goes last: an identity
        // is added only to a column already `NOT NULL` ("must be declared
        // NOT NULL before identity can be added"), so after the tightening,
        // and a sequence is moved past the column's values in the type the
        // column ends with, so after the retype.
        Change::Column(ColumnChange::AlterColumnNumbering {
            from: ValueSource::Identity(_) | ValueSource::Sequence(_),
            to: ValueSource::None,
            ..
        })
        | Change::Column(ColumnChange::AlterColumnNumbering {
            from: ValueSource::Identity(_),
            to: ValueSource::Sequence(_),
            ..
        }) => -3,
        Change::Column(ColumnChange::AlterColumnNumbering { .. }) => 2,
        // A recomputation is checked against the column's nullability as it
        // stands, so a relaxation goes before one and a tightening after it.
        // Measured on 17.11, `SET EXPRESSION` that yields a NULL is refused
        // under `NOT NULL` ("contains null values") and accepted once
        // `DROP NOT NULL` has run. Nothing else in the class reads nullability.
        Change::Column(ColumnChange::AlterColumnNullability {
            to_nullable: true, ..
        }) => -1,
        Change::Column(ColumnChange::AlterColumnNullability {
            to_nullable: false, ..
        }) => 1,
        // A partition's own, alike: its column's type is its parent's, which
        // no change here moves (#1581).
        Change::Partition(PartitionChange::SetPartitionDefault { .. }) => 0,
        Change::Partition(PartitionChange::SetPartitionNotNull {
            not_null: false, ..
        }) => -1,
        Change::Partition(PartitionChange::SetPartitionNotNull { not_null: true, .. }) => 1,
        // Rows follow the foreign keys between their tables: a referenced
        // table's rows go in first, and out last.
        Change::Row(RowChange::InsertRow { table, .. }) | Change::Row(RowChange::UpdateRow { table, .. }) => {
            data_rank.get(table).map_or(0, |r| *r as isize)
        }
        Change::Row(RowChange::DeleteRow { table, .. }) => -(data_rank.get(table).map_or(0, |r| *r as isize)),
        // Two roles dropped together are ordered by membership: see
        // [`member_depth`].
        Change::Role(RoleChange::DropRole { name, .. }) => role_rank.get(name).map_or(0, |r| *r as isize),
        // The rest depend on nothing among themselves; a grant's target is
        // ordered by the class of the change, not by rank.
        Change::Role(RoleChange::CreateRole { .. })
        | Change::Role(RoleChange::RenameRole { .. })
        | Change::Permission(GrantChange::Grant { .. })
        | Change::Permission(GrantChange::Revoke { .. }) => 0,
        // The rank of the routine it settles, so it sorts beside that
        // routine's `CREATE` in class 14 (see `order_key`, #687).
        Change::Permission(GrantChange::PublicExecution { routine, .. }) => create_rank
            .get(&ModuleId::Routine(routine.clone()))
            .map_or(0, |r| *r as isize),
    }
}

/// The order of application.
///
/// Ordinarily the table rename comes first, so later steps use current names.
/// The specific freeing-drop graph may order some drops around it and supplies
/// their execution addresses (DECISIONS 496). Remaining constraint and index drops come next: they must precede both the
/// column renames they would otherwise block and the column drops they
/// reference. Adding them back must follow adding columns.
/// Inserting a class shifts every class below it, and these ordinals are
/// quoted in prose that uses them to justify behaviour: DECISIONS 140, 146,
/// 151, 174 and 237, `docs/PITFALLS.md`, `preflight.rs` and `deploy.rs`. A new
/// class means renumbering those in the same commit — a stale ordinal there
/// reads as a statement about the code and is not checked against it.
/// The class of the changes that alter an existing column in place: its
/// type, nullability, default or generation expression. Named because a
/// generated column's addition is placed after it (DEC-1168.1).
const COLUMN_ALTERATIONS: u8 = 9;

/// The class of row deletions, the last row changes. Named because a
/// tightening of a table with row changes is placed at its end (#1367).
const ROW_DELETIONS: u8 = 12;

/// Whether going from `from` to `to` sets a domain's default, as against
/// dropping or keeping it.
fn domain_sets_default(from: &pbps_model::ManagedType, to: &pbps_model::ManagedType) -> bool {
    pbps_model::DomainDelta::of(from, to).is_some_and(|d| matches!(d.default, Some(Some(_))))
}

/// The subject a change is sorted by within its class and rank. A rebuilt
/// type's steps lead with their phase, so that every retype of every rebuilt
/// type comes before the first swap, and a swap before the swap of the type
/// it is built on: the old base cannot be dropped while a domain over it
/// stands (DEC-1635.9). Every other change sorts by its own subject.
fn rebuild_subject(change: &Change) -> String {
    use pbps_model::EnumRebuildStep as Step;
    let Change::TypeValue(TypeValueChange::EnumRebuild { step, .. }) = change else {
        return change.subject();
    };
    match step {
        Step::Retype { .. } => format!("0 {}", change.subject()),
        // Fixed width, reversed: a deeper swap sorts first at any depth.
        Step::Swap { depth, .. } => format!("1 {:020} {}", usize::MAX - *depth, change.subject()),
        Step::Create { .. } => change.subject(),
    }
}

fn order_key(c: &Change) -> u8 {
    match c {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
            unreachable!("rebuild operations are expanded by order, never ranked by the differ")
        }
        Change::TypeValue(TypeValueChange::AddEnumValue { .. })
        | Change::TypeValue(TypeValueChange::RenameEnumValue { .. })
        | Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { .. },
            ..
        })
        // A package's own objects are what a later type, module or table of
        // the plan may use, so it goes with the type creates; the order among
        // packages is the `requires` edge, not this number (DEC-2447.6).
        | Change::Type(TypeChange::CreateType { .. })
        // Created before the tables, defaults, indexes and checks that name them by a
        // string literal, and a dictionary before the configuration that maps it
        // (DEC-2728.2). One altered in place goes first of its dependents too (DEC-2728.3).
        | Change::Search(SearchChange::CreateSearch { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Extension(ExtensionChange::CreateExtension { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. }) => 7,
        // A domain's checks and NOT NULL are dropped with a table's, before
        // the plan's rows, and added with them after the rows (DEC-1635.8).
        // A default the half sets can name a label the plan renames or adds,
        // so that half waits for the type changes (class 7); it still
        // precedes every column and row.
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Loosen,
            from,
            to,
            ..
        }) => {
            if domain_sets_default(from, to) {
                7
            } else {
                2
            }
        }
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Tighten,
            ..
        }) => 13,
        Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => COLUMN_ALTERATIONS,
        // After every module this plan drops and the packages it requires
        // are gone: late, like the type drops (DEC-2447.8).
        Change::Type(TypeChange::DropType { .. }) | Change::Search(SearchChange::DropSearch { .. }) | Change::Extension(ExtensionChange::DropExtension { .. }) => 18,
        Change::Type(TypeChange::RenameType { .. }) => 1,
        // A policy releases its bindings before module and column teardown.
        Change::Policy(PolicyChange::DropPolicy { .. }) => 0,
        // Modules go first and last, and both ends are load-bearing. A
        // SCHEMABINDING view blocks a rename of the column it binds, so every
        // module that is going has to go before the table changes; and a view
        // can only be created once the columns it selects exist.
        Change::Module(ModuleChange::DropModule { .. }) => 0,
        // A role drop needs nothing else gone first, and a plan that also
        // recreates the name wants the old one out of the way early.
        Change::Role(RoleChange::DropRole { .. }) => 0,
        // A table rename, and a role rename that depends on nothing here.
        Change::Table(TableChange::RenameTable { .. }) | Change::Role(RoleChange::RenameRole { .. }) => 1,
        // Before the column renames, and after the table rename that gives
        // these drops the name they use.
        //
        // The engine refuses `sp_rename` on a column a check constraint or a
        // filtered index's predicate names — 15336 for the check, 5074 with
        // 4922 behind it for the index, measured — so for those two the drop
        // is what makes the rename possible, and running it after was a valid,
        // reviewed plan the engine would not perform. The re-add stays in the
        // constraint class far below, where the new spelling can be written.
        //
        // The whole group moves, not the two kinds that need it. Measured, a
        // rename is never blocked by a constraint that does not name the
        // column, so moving the rest costs nothing — and the alternative is
        // asking which columns a check's expression names, which this tool
        // deliberately never parses (DECISIONS 174). `DropForeignKey`'s rank
        // travels with them and still puts it ahead of the key it references.
        //
        // A primary key that is *only* dropped is one of these drops and
        // travels with them. It was left in the addition class below because
        // one variant carries both directions, and a key that is still there
        // blocks the column changes every class from here down can make:
        // measured, `DROP NOT NULL` on a key column is `42P16` on PostgreSQL
        // and 5074 with 4922 behind it on SQL Server, and dropping the column
        // outright is the same 5074. So a declaration that gives up a key and
        // relaxes its column produced a plan neither engine would perform.
        //
        // Only `to: None`, and that is not a partial answer: a key being
        // *replaced* is emitted as two changes, its drop and its add (see
        // `diff_constraints`), so the drop arrives here and the add stays
        // below, where the columns its new shape may name have been added.
        //
        // `dependency_rank` already keeps a foreign-key drop ahead of it
        // inside this class, which is the order the engine requires and the
        // reason that rank exists.
        Change::Index(IndexChange::DropIndex { .. })
        | Change::Constraint(ConstraintChange::DropUnique { .. })
        | Change::Constraint(ConstraintChange::DropForeignKey { .. })
        | Change::Constraint(ConstraintChange::DropCheck { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { to: None, .. })
        | Change::Constraint(ConstraintChange::SetPrimaryKey { to: None, .. }) => 2,
        // After the drops above (`sort_class` places it at the class's end),
        // since an index or check over it blocks it (4922), and before every
        // rename, drop or retype of a column its expression reads, which it
        // blocks in turn (15336, 4922; measured on 17.0, #1174).
        Change::Column(ColumnChange::DropComputedColumn { .. }) => 2,
        // A class of its own, after the table renames: `sp_rename` on a column
        // names the table, and `resolve_columns` iterates the *declared*
        // schema, so a `RenameColumn` always carries the post-rename table.
        //
        // Not sharing class 1 with `RenameTable`, which is where it was. The
        // two are not peers — one depends on the other — and sharing a class
        // left `subject()` to separate them. `RenameTable` answers with its
        // `from`, the old name, while the column rename carries the new one,
        // so the two were sorted against two different names for one table and
        // the alphabet decided: `dbo.customers` -> `dbo.clients` put the
        // column rename first, against a table that did not exist yet.
        Change::Column(ColumnChange::RenameColumn { .. }) => 3,
        // After the renames, so a revoke names the role and the object as
        // they now are. It shared a class with the drops above until they
        // moved ahead of the renames; it has the opposite need, so it stayed
        // and took a class of its own. A revoke on an object this plan drops
        // is never emitted (see `diff_roles`).
        Change::Permission(GrantChange::Revoke { .. }) => 4,
        Change::Column(ColumnChange::DropColumn { .. }) => 5,
        // A detach frees its range, as a drop does, before a partition is
        // created over it in class 7 (#1544).
        Change::Table(TableChange::DropTable { .. }) | Change::Partition(PartitionChange::DetachPartition { .. }) => 6,
        // An attach takes its range as a created partition does, and once
        // a detach or a drop has freed it (#1545).
        Change::Table(TableChange::CreateTable { .. }) | Change::Partition(PartitionChange::AttachPartition { .. }) => 7,
        Change::Column(ColumnChange::AddColumn { .. }) => 8,
        Change::Column(ColumnChange::AlterColumnType { .. })
        | Change::Column(ColumnChange::AlterColumnNullability { .. })
        | Change::Column(ColumnChange::AlterColumnDefault { .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { .. })
        | Change::Column(ColumnChange::AlterColumnExpression { .. })
        | Change::Partition(PartitionChange::SetPartitionDefault { .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { .. }) => COLUMN_ALTERATIONS,
        // Once every column its expression reads is there in its final type
        // (`sort_class` places it after the class's alterations), and before
        // the indexes and checks of class 13 that may be over it (#1174).
        Change::Column(ColumnChange::AddComputedColumn { .. }) => COLUMN_ALTERATIONS,
        Change::Column(ColumnChange::SetColumnDeprecated { .. }) => 10,
        // A comment is metadata too, on the object as the plan leaves it: after its
        // create (7, 8), alterations (9) and every rename (1), under its final name
        // (DEC-2729.1, measured: a drop and a create lose a comment, a rename keeps it).
        Change::TableProperty(TablePropertyChange::SetComment { .. }) => 10,
        // Metadata too: no statement reads a storage parameter, and it runs
        // under the table's final name (DEC-1441.1).
        Change::TableProperty(TablePropertyChange::SetStorageParameters { .. }) => 10,
        // After the foreign-key drops of class 2 and before the adds of 13:
        // the engine checks the keys that stand when it switches (#1443).
        Change::TableProperty(TablePropertyChange::SetTablePersistence { .. }) => COLUMN_ALTERATIONS,
        // In place too, and nothing reads one (DEC-1442.1).
        Change::Index(IndexChange::SetIndexStorageParameters { .. }) => 10,
        // Rows arrive once every column they name exists and has its final
        // type, and before the constraints below: ADR-0004's "create table ->
        // insert rows -> add the foreign key that references them".
        Change::Row(RowChange::InsertRow { .. }) | Change::Row(RowChange::UpdateRow { .. }) => 11,
        // Rows leave after every insert and update, and after the foreign keys
        // that could block them are gone. No single order satisfies every
        // shape — a delete-then-insert on a table with a UNIQUE elsewhere
        // wants the delete first — but this is the order whose failure is
        // *loud*: the engine refuses inside the transaction and the plan rolls
        // back. Deleting first fails silently: a child row that moves its
        // foreign key to another parent in this same plan is still pointing
        // at the old one when the old one goes, and `ON DELETE CASCADE` takes
        // the child with it, after which the update touches zero rows and
        // nothing says so.
        Change::Row(RowChange::DeleteRow { .. }) => ROW_DELETIONS,
        Change::Constraint(ConstraintChange::SetPrimaryKey { .. })
        | Change::Constraint(ConstraintChange::AddUnique { .. })
        | Change::Constraint(ConstraintChange::AddForeignKey { .. })
        | Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { to: Some(_), .. })
        | Change::Index(IndexChange::AddIndex { .. }) => 13,
        // Late, it follows the index it names (DEC-1444.1); `sort_class`
        // moves the rest to the front.
        Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => 13,
        Change::Policy(PolicyChange::CreatePolicy { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::Module(ModuleChange::CreateModule { .. })
        | Change::Module(ModuleChange::AlterModule { .. }) => 14,
        // The `PUBLIC` decision for a routine travels with the routine: class
        // 14 at that routine's own create rank (`dependency_rank`), and its
        // subject is the routine's, so the sort puts it immediately after its
        // `CREATE` or `ALTER` with nothing between them. In a transactional
        // apply nothing outside sees the gap either way; in the autocommit
        // script `--sql` renders (DECISIONS 259) a routine created at 14 and
        // revoked at 16 held the engine's default `EXECUTE` for `PUBLIC` for
        // the rest of the modules and every role create (#687). A class of its
        // own after the modules would still leave a later routine's `CREATE`
        // in between, and would renumber every ordinal below (DEC-687.1).
        Change::Permission(GrantChange::PublicExecution { .. }) => 14,
        // A grant names an object, so it comes after every object exists —
        // and after the role does.
        Change::Role(RoleChange::CreateRole { .. }) => 15,
        Change::Permission(GrantChange::Grant { .. }) => 16,
        // Emits nothing; it exists so the recorded state matches the file. Last
        // keeps it out of the way of everything that does emit.
        Change::Row(RowChange::SetDataMode { .. }) | Change::Row(RowChange::SetEnsureKeys { .. }) => 17,
    }
}
#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests;

#[cfg(test)]
mod managed_type_tests;

#[cfg(test)]
mod type_replacement_tests;

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod meeting_shape_tests;

/// Text search objects through the whole differ: creation and drop order by depth, the
/// recorded reason a drop needs, and the dictionary a mapping must be able to name
/// (DEC-2728.2).
#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod search_tests;

#[cfg(test)]
mod split_pin_tests;
