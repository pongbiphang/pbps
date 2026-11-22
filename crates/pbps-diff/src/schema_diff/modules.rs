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
