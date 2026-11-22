//! Views, routines and triggers: what changes hands and what changes.

use super::*;

/// Modules are matched by **name**, never by uid: they carry no data, so they
/// carry no identity (ADR-0002).
///
/// Two of the three comparisons are ordinary. The third is the interesting one:
/// a module whose *kind* or trigger table changed is not an alteration at all —
/// `CREATE OR ALTER` cannot turn a view into a procedure, or move a trigger to
/// another table — so it is emitted as a drop followed by a create.
/// Table names held by one table in the baseline and another after this plan:
/// a table dropped, or renamed away, and another renamed or created into its
/// name, across skipped revisions (DEC-1118.1). Anything keyed by such a name
/// compares equal across the two tables while belonging to different ones.
pub(super) fn names_changing_hands(base: Side<'_>, declared: Side<'_>) -> BTreeSet<TableName> {
    let before: BTreeMap<&TableName, &Uid> = base
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    declared
        .ids
        .tables
        .iter()
        .filter(|(uid, name)| before.get(name).is_some_and(|was| was != uid))
        .map(|(_, name)| name.clone())
        .collect()
}

// A trigger's table name can stay equal while its owner UID changes. Both
// the differ and restoration exemption must recognize that boundary (DEC-1118.1).
pub(super) fn trigger_changes_hands(id: &ModuleId, changing_hands: &BTreeSet<TableName>) -> bool {
    matches!(id, ModuleId::Trigger { on, .. }
        if changing_hands.contains(&TableName::new(on.schema.clone(), on.name.clone())))
}

/// The logical module change, shared with plan-time consumers (SPEC §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleDelta {
    Unchanged,
    Alter,
    Rebuild,
}

/// Descriptions are declaration metadata. Kind changes cannot use ALTER;
/// options and dialect-normalized definitions determine whether to restate it.
pub fn module_delta(
    before: &Module,
    after: &Module,
    dialect: &(impl Dialect + ?Sized),
) -> ModuleDelta {
    // A trigger moved to another table needs no case of its own any
    // more: its table is part of its identity, so the move is a key
    // that is gone and a key that is new, and the two loops here
    // already spell that as a drop and a create (ADR-0009 §1). What is
    // left is the kind, which the key does not hold: `CREATE OR ALTER`
    // cannot turn a view into a procedure.
    if before.kind != after.kind {
        ModuleDelta::Rebuild
    } else if dialect.normalize_definition(&before.definition)
        // The definition is compared after the dialect's lightweight
        // normalization, never by understanding it (SPEC §8.2). What
        // survives that is re-stated in full, which is idempotent and
        // keeps the permissions granted on the object.
        != dialect.normalize_definition(&after.definition)
        // The option is part of what the module is: the engine
        // keeps it, refuses changes to what it reads while it
        // holds, and `CREATE OR ALTER` sets it either way.
        || before.options != after.options
    {
        ModuleDelta::Alter
    } else {
        ModuleDelta::Unchanged
    }
}

pub(super) fn diff_modules(
    base: &Schema,
    declared: &Schema,
    changing_hands: &BTreeSet<TableName>,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    for (id, module) in &declared.modules {
        match base.modules.get(id) {
            None => changes.push(Change::Module(ModuleChange::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            })),
            // A trigger on a name that passes to another table: one id for
            // two triggers, the doomed or departed table's and the one this
            // plan declares on the new occupant. The first goes before the
            // table changes and the second is created after them, exactly as
            // for a trigger that changes kind (DEC-1118.1).
            Some(before) if trigger_changes_hands(id, changing_hands) => {
                changes.push(Change::Module(ModuleChange::DropModule {
                    id: id.clone(),
                    kind: before.kind,
                }));
                changes.push(Change::Module(ModuleChange::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                }));
            }
            Some(before) => match module_delta(before, module, dialect) {
                ModuleDelta::Unchanged => {}
                ModuleDelta::Alter => changes.push(Change::Module(ModuleChange::AlterModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                })),
                ModuleDelta::Rebuild => {
                    changes.push(Change::Module(ModuleChange::DropModule {
                        id: id.clone(),
                        kind: before.kind,
                    }));
                    changes.push(Change::Module(ModuleChange::CreateModule {
                        id: id.clone(),
                        module: Box::new(module.clone()),
                    }));
                }
            },
        }
    }

    for (id, module) in &base.modules {
        if !declared.modules.contains_key(id) {
            changes.push(Change::Module(ModuleChange::DropModule {
                id: id.clone(),
                kind: module.kind,
            }));
        }
    }
}

/// The module rebinding phase of `diff_partial_rebuilding`: the modules the
/// dialect, the caller or an enum's dependents ask to have rebuilt around the
/// plan's changes, and the modules a schema-bound teardown takes down.
///
/// It takes the list of `changes` built so far and hands it back rebuilt, not a
/// list of its own to append afterwards: it rewrites entries already in the
/// list, and `diff_roles`, the exclusion hazards and the sort read the list as
/// this phase leaves it. Moving the list in and out keeps its order.
pub(super) fn rebind_modules(
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
pub(super) fn module_ranks(
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
