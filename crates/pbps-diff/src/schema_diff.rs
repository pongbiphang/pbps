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
mod sort;
mod table_props;
mod tables;
mod types;

use columns::diff_columns;
use common::Indexes;
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
pub use constraints::may_adopt;
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
pub use exclusions::may_adopt_exclusion;
pub use modules::ModuleDelta;
use modules::diff_modules;
pub use modules::module_delta;
use modules::module_ranks;
use modules::names_changing_hands;
use modules::rebind_modules;
use modules::trigger_changes_hands;
use numbering::identities_put_on;
use numbering::numbering_after_deletes;
use numbering::numbering_put_on;
use numbering::refuse_rows_referencing_late_inserts;
use numbering::sequence_release_depths;
use partitions::adoption::rebuild_adoptable_exclusions;
use partitions::adoption::rebuild_adoptable_indexes;
use partitions::attach::Attached;
use partitions::attach::attached;
use partitions::attach::diff_partition_columns;
use partitions::attach::partition_default;
use partitions::detach::detached_names;
use partitions::detach::parent_shape_changes;
use partitions::refusals::parent_column;
use partitions::refusals::refuse_partition_changes;
use partitions::rehearsal::leave_to_attach;
pub use partitions::rehearsal::{Question, candidates, questions};
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
use sort::facts::sort_facts;
use sort::finish::finish_sort;
use sort::keys::COLUMN_ALTERATIONS;
use sort::keys::ROW_DELETIONS;
use sort::keys::dependency_rank;
use sort::keys::order_key;
use sort::keys::rank_of;
use sort::keys::rank_of_tables;
use sort::keys::rebuild_subject;
use table_props::ForeignKeyGraph;
use table_props::diff_filegroups;
use table_props::diff_replica_identity;
use table_props::diff_storage_parameters;
use table_props::unlink_cycles_around_persistence_switches;
use tables::TableFacts;
use tables::diff_tables;
use types::diff_types;
pub use types::type_differences;

/// One side's complete input: a state plus its own identity mapping.
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
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
            errors: crate::extensions::differences(
                base,
                declared,
                crate::extensions::Packages::of(dialect),
            ),
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
        crate::extensions::Packages::of(dialect),
    ));
    diff_sequences(base.schema, declared.schema, dialect, &mut errs);
    if !dialect.records_conversions() && !declared.ids.conversions.is_empty() {
        errs.push(DiffError::ConversionsUnsupported {
            count: declared.ids.conversions.len(),
        });
    }

    let (mut label_changes, label_errors) = crate::enum_labels::changes(base, declared, hints);
    errs.extend(label_errors);
    // The sides as the caller gave them, which an attach's question is asked of.
    let given = base;
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
        crate::extensions::Packages::of(dialect),
        &hints.extension_requires,
    ));
    let (extension_drops, extension_drop_errors) = crate::extensions::drops(
        base,
        declared,
        crate::extensions::Packages::of(dialect),
        &hints.extension_drops,
    );
    changes.extend(extension_drops);
    errs.extend(extension_drop_errors);
    let (extension_updates, extension_update_errors) = crate::extensions::updates(
        base,
        declared,
        crate::extensions::Packages::of(dialect),
        &hints.extension_updates,
    );
    changes.extend(extension_updates);
    errs.extend(extension_update_errors);
    // After the updates: a move of a package that is also updated starts from
    // the version the update leaves, and the plan's own order keeps them so
    // (DEC-2447.11).
    let (extension_moves, extension_move_errors) = crate::extensions::moves(
        base,
        declared,
        crate::extensions::Packages::of(dialect),
        &hints.extension_moves,
    );
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
    // Before every pass that reads the index drops: one a rehearsal leaves to
    // its attach is not dropped (DEC-1671.2).
    leave_to_attach(
        given,
        declared,
        dialect,
        hints,
        also,
        rebinding,
        screen,
        read_back,
        &mut changes,
    );
    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;

    split_tightenings_from_retypes(&mut changes);
    clear_external_partitions_after_loosening(declared.schema, &mut changes);
    crate::enum_labels::rebuild_defaults(base, declared, &mut changes);
    recreate_retyped_dependents(base, declared, &renames, dialect, &mut changes);
    // After every pass that adds a parent's index, and before the foreign keys
    // over the unique ones it drops are recreated.
    rebuild_adoptable_indexes(base, declared, &renames, dialect, &mut changes);
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
    // After the hazards, whose restatement of a parent's exclusion can take a
    // partition's own as any addition can.
    rebuild_adoptable_exclusions(base, declared, &renames, dialect, &mut changes);
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
