//! Expected closing prerequisites, derived only from approved typed changes.

use super::{InputManifest, ManifestError, ObjectIdentity, Surface};
use crate::{Change, ChangeSet, GrantTarget, ModuleId};
use std::collections::{BTreeMap, BTreeSet};

/// The adapter's logical inventory of the catalog records owned by one
/// managed surface. Internal dependents (for example a relation's row type)
/// belong here only when the engine proved their ownership.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectTransition {
    pub surface: Surface,
    pub before: BTreeSet<ObjectIdentity>,
    pub after: BTreeSet<ObjectIdentity>,
}

impl InputManifest {
    /// Preserve all untouched prerequisites and change exactly the catalog
    /// records that the approved plan changes. `compiled` supplies their
    /// measured desired properties, never SQL to run during apply.
    pub fn project(
        &self,
        changes: &ChangeSet,
        compiled: &Self,
        transitions: &[ObjectTransition],
    ) -> Result<Self, ManifestError> {
        if self.adapter() != compiled.adapter()
            || self.engine_major() != compiled.engine_major()
            || self.key_id() != compiled.key_id()
            || self.scope() != compiled.scope()
        {
            return Err(ManifestError::Invalid);
        }
        // Checking supplied entries alone accepts an empty inventory for a
        // DROP, leaving the closing manifest at the pre-DDL state (SPEC 9.3.2).
        for step in changes
            .changes
            .iter()
            .filter(|p| changes_catalog(&p.change))
        {
            if !transitions
                .iter()
                .any(|t| covers_change(&step.change, &t.surface))
            {
                return Err(ManifestError::Incomplete);
            }
        }
        let mut removed = BTreeSet::new();
        let mut installed = BTreeSet::new();
        let mut owners = BTreeSet::new();
        for transition in transitions {
            if (transition.before.is_empty() && transition.after.is_empty())
                || !owners.insert(&transition.surface)
                || !changes
                    .changes
                    .iter()
                    .any(|p| touches(&p.change, &transition.surface))
            {
                return Err(ManifestError::Invalid);
            }
            for (inventory, manifest, before) in [
                (&transition.before, self, true),
                (&transition.after, compiled, false),
            ] {
                for object in inventory {
                    let records = manifest.prerequisites();
                    let position = records
                        .binary_search_by(|p| p.object.cmp(object))
                        .map_err(|_| ManifestError::Incomplete)?;
                    let ownership = &records[position].ownership;
                    // Ownership permits aggregation, not unrelated mutations.
                    // The same typed scope that requires a record below must
                    // also authorize replacing it (SPEC 9.3.2).
                    let affected = changes.changes.iter().any(|step| {
                        touches(&step.change, &transition.surface)
                            && changed_owners(&step.change).is_some_and(
                                |(candidates, opening, closing)| {
                                    (if before { opening } else { closing })
                                        && candidates
                                            .iter()
                                            .any(|owner| ownership.permits(owner, changes))
                                },
                            )
                    });
                    if !ownership.permits(&transition.surface, changes) || !affected {
                        return Err(ManifestError::Invalid);
                    }
                }
            }
            for object in &transition.before {
                if !removed.insert(object.clone()) {
                    return Err(ManifestError::Invalid);
                }
            }
            for object in &transition.after {
                if !installed.insert(object.clone()) {
                    return Err(ManifestError::Invalid);
                }
            }
        }
        // An aggregate label permits children but proves no inventory. Every
        // catalog mutation must account for its independently captured owner
        // and all owned records, even when that owner is not created/dropped.
        for step in &changes.changes {
            if let Some((candidates, opening, closing)) = changed_owners(&step.change) {
                let complete = candidates.iter().any(|owner| {
                    [
                        (self, &removed, opening, true),
                        (compiled, &installed, closing, false),
                    ]
                    .into_iter()
                    .all(|(manifest, inventory, required, before)| {
                        if !required {
                            return true;
                        }
                        let records: Vec<_> = manifest
                            .prerequisites()
                            .iter()
                            .filter(|p| p.ownership.permits(owner, changes))
                            .collect();
                        // A grant or constraint may follow an approved CREATE,
                        // or precede a DROP. The typed plan must prove why the
                        // owner is absent at that snapshot, not the inventory.
                        if records.is_empty() && planned_absence(owner, changes, before) {
                            return true;
                        }
                        records
                            .iter()
                            .any(|p| p.ownership.matches_surface(owner, changes))
                            && records.iter().all(|p| inventory.contains(&p.object))
                    })
                });
                if !complete {
                    return Err(ManifestError::Incomplete);
                }
            }
        }
        let mut objects: BTreeMap<_, _> = self
            .prerequisites()
            .iter()
            .map(|p| (p.object.clone(), p.clone()))
            .collect();
        for object in &removed {
            if objects.remove(object).is_none() {
                return Err(ManifestError::Incomplete);
            }
        }
        let compiled_objects: BTreeMap<_, _> = compiled
            .prerequisites()
            .iter()
            .map(|p| (&p.object, p))
            .collect();
        for object in &installed {
            let p = compiled_objects
                .get(object)
                .ok_or(ManifestError::Incomplete)?;
            if objects.insert(object.clone(), (*p).clone()).is_some() {
                return Err(ManifestError::Invalid);
            }
        }
        let mut membership = self.membership().to_vec();
        for (result, desired) in membership.iter_mut().zip(compiled.membership()) {
            if result.predicate != desired.predicate {
                return Err(ManifestError::Incomplete);
            }
            result.members.retain(|id| !removed.contains(id));
            result
                .members
                .extend(desired.members.intersection(&installed).cloned());
        }
        let mut runtime_bound = self.runtime_bound().clone();
        runtime_bound.retain(|id| !removed.contains(id));
        runtime_bound.extend(compiled.runtime_bound().intersection(&installed).cloned());
        let mut lookups = self.identifications().to_vec();
        if lookups.len() != compiled.identifications().len() {
            return Err(ManifestError::Incomplete);
        }
        for (result, desired) in lookups.iter_mut().zip(compiled.identifications()) {
            if (&result.signature, &result.search_path, &result.kind)
                != (&desired.signature, &desired.search_path, &desired.kind)
            {
                return Err(ManifestError::Incomplete);
            }
            if result.resolved != desired.resolved {
                if result
                    .resolved
                    .as_ref()
                    .is_some_and(|id| !removed.contains(id))
                    || desired
                        .resolved
                        .as_ref()
                        .is_some_and(|id| !installed.contains(id))
                {
                    return Err(ManifestError::Invalid);
                }
                result.resolved = desired.resolved.clone();
            }
        }
        Self::new(
            self.adapter().into(),
            self.engine_major(),
            self.key_id().into(),
            self.scope().clone(),
            self.baseline().into(),
            self.session().into(),
            objects.into_values().collect(),
            membership,
            runtime_bound,
            lookups,
        )
    }
}

// Exhaustive: adding a catalog mutation must explicitly name the owner and
// its endpoints. An ADD COLUMN may aggregate under Table without changing
// unrelated children, so the typed change determines the minimum owner.
fn changed_owners(change: &Change) -> Option<(Vec<Surface>, bool, bool)> {
    let (owner, opening, closing) = match change {
        Change::CreateTable { name, .. } => (Surface::Table(name.clone()), false, true),
        Change::DropTable { name, .. } => (Surface::Table(name.clone()), true, false),
        Change::RenameTable { from, .. } => (Surface::Table(from.clone()), true, true),
        Change::AddColumn { table, name, .. } => (Surface::Column(table.column(name)), false, true),
        Change::DropColumn { column, .. } => (Surface::Column(column.clone()), true, false),
        Change::RenameColumn { table, from, .. } => {
            (Surface::Column(table.column(from)), true, true)
        }
        Change::AlterColumnType { column, .. } | Change::AlterColumnNullability { column, .. } => {
            (Surface::Column(column.clone()), true, true)
        }
        Change::AlterColumnDefault {
            column, from, to, ..
        } => (
            Surface::Default(column.clone()),
            from.is_some(),
            to.is_some(),
        ),
        Change::SetPrimaryKey { table, .. }
        | Change::AddUnique { table, .. }
        | Change::DropUnique { table, .. }
        | Change::AddForeignKey { table, .. }
        | Change::DropForeignKey { table, .. } => (Surface::Table(table.clone()), true, true),
        Change::AddCheck { table, name, .. } => (
            Surface::Check {
                table: table.clone(),
                name: name.clone(),
            },
            false,
            true,
        ),
        Change::DropCheck { table, name } => (
            Surface::Check {
                table: table.clone(),
                name: name.clone(),
            },
            true,
            false,
        ),
        Change::AddIndex { table, name, .. } => (
            Surface::Index {
                table: table.clone(),
                name: name.clone(),
            },
            false,
            true,
        ),
        Change::DropIndex { table, name } => (
            Surface::Index {
                table: table.clone(),
                name: name.clone(),
            },
            true,
            false,
        ),
        Change::CreateModule { id, .. } => (Surface::Module(id.clone()), false, true),
        Change::AlterModule { id, .. } => (Surface::Module(id.clone()), true, true),
        Change::DropModule { id, .. } => (Surface::Module(id.clone()), true, false),
        Change::PublicExecution { routine, .. } => (
            Surface::Module(ModuleId::Routine(routine.clone())),
            true,
            true,
        ),
        Change::Grant { target, .. } | Change::Revoke { target, .. } => match target {
            GrantTarget::Schema(name) => (Surface::Namespace(name.clone()), true, true),
            GrantTarget::Routine(id) => {
                (Surface::Module(ModuleId::Routine(id.clone())), true, true)
            }
            // An object grant may address a table or a named module. One
            // independently owned target must account for both endpoints.
            GrantTarget::Object(name) => {
                return Some((
                    vec![
                        Surface::Table(name.clone()),
                        Surface::Module(ModuleId::Named(name.clone())),
                    ],
                    true,
                    true,
                ));
            }
        },
        Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::CreateRole { .. }
        | Change::RenameRole { .. }
        | Change::DropRole { .. } => return None,
    };
    Some((vec![owner], opening, closing))
}

// Only an approved lifecycle operation can explain a missing endpoint for a
// mutation. Parent creation/removal owns its children; unrelated DDL cannot
// excuse a missing prerequisite (SPEC 9.3.2).
#[allow(clippy::wildcard_enum_match_arm)]
fn planned_absence(owner: &Surface, changes: &ChangeSet, before: bool) -> bool {
    let ownership = super::ObjectOwnership::Surface(owner.clone());
    changes.changes.iter().any(|step| {
        let boundary = match &step.change {
            Change::CreateTable { name, .. } if before => Surface::Table(name.clone()),
            Change::DropTable { name, .. } if !before => Surface::Table(name.clone()),
            Change::AddColumn { table, name, .. } if before => Surface::Column(table.column(name)),
            Change::DropColumn { column, .. } if !before => Surface::Column(column.clone()),
            Change::CreateModule { id, .. } if before => Surface::Module(id.clone()),
            Change::DropModule { id, .. } if !before => Surface::Module(id.clone()),
            Change::AddCheck { table, name, .. } if before => Surface::Check {
                table: table.clone(),
                name: name.clone(),
            },
            Change::DropCheck { table, name } if !before => Surface::Check {
                table: table.clone(),
                name: name.clone(),
            },
            Change::AddIndex { table, name, .. } if before => Surface::Index {
                table: table.clone(),
                name: name.clone(),
            },
            Change::DropIndex { table, name } if !before => Surface::Index {
                table: table.clone(),
                name: name.clone(),
            },
            Change::AlterColumnDefault {
                column, from, to, ..
            } if (before && from.is_none() && to.is_some())
                || (!before && from.is_some() && to.is_none()) =>
            {
                Surface::Default(column.clone())
            }
            _ => return false,
        };
        ownership.permits(&boundary, changes)
    })
}

// A child record can be touched by its owner's DDL without accounting for
// the owner itself: a default-only transition leaves a dropped table behind
// (SPEC 9.3.2). Aggregate owner inventories remain allowed, but a child must
// never substitute for the table/column that the approved change creates or removes.
fn covers_change(c: &Change, surface: &Surface) -> bool {
    touches(c, surface)
        && match surface {
            Surface::Column(_) => {
                !matches!(c, Change::CreateTable { .. } | Change::DropTable { .. })
            }
            Surface::Default(_) => matches!(c, Change::AlterColumnDefault { .. }),
            Surface::Namespace(_)
            | Surface::Table(_)
            | Surface::Check { .. }
            | Surface::Index { .. }
            | Surface::Module(_) => true,
        }
}

// Only typed changes authorize transitions. The adapter identifies catalog
// records; it cannot use an unrelated plan as permission to change an input.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn touches(c: &Change, surface: &Surface) -> bool {
    if !changes_catalog(c) {
        return false;
    }
    match surface {
        Surface::Namespace(name) => {
            grant_target(c).is_some_and(|g| matches!(g, GrantTarget::Schema(s) if s == name))
        }
        Surface::Table(t) => {
            c.objects().any(|o| o == t)
                || grant_target(c).is_some_and(|g| matches!(g, GrantTarget::Object(o) if o == t))
        }
        Surface::Column(column) => {
            c.columns_redefined().iter().any(|(r, _)| r == column)
                || matches!(c, Change::CreateTable { name, .. } | Change::DropTable { name, .. } if name == &column.table)
        }
        Surface::Default(column) => {
            matches!(c, Change::AlterColumnDefault { column: r, .. } if r == column)
                || matches!(c, Change::CreateTable { name, .. } | Change::DropTable { name, .. } if name == &column.table)
                || matches!(c, Change::AddColumn { table, name, .. } if &table.column(name) == column)
        }
        Surface::Check { table, name } => {
            matches!(c, Change::AddCheck { table: t, name: n, .. } | Change::DropCheck { table: t, name: n } if t == table && n == name)
        }
        Surface::Index { table, name } => {
            matches!(c, Change::AddIndex { table: t, name: n, .. } | Change::DropIndex { table: t, name: n } if t == table && n == name)
        }
        Surface::Module(id) => {
            c.module_id() == Some(id)
                || matches!((c, id), (Change::PublicExecution { routine, .. }, ModuleId::Routine(r)) if routine == r)
                || grant_target(c).is_some_and(|g| match (g, id) {
                    (GrantTarget::Routine(a), ModuleId::Routine(b)) => a == b,
                    (GrantTarget::Object(a), ModuleId::Named(b)) => a == b,
                    _ => false,
                })
        }
    }
}

fn grant_target(c: &Change) -> Option<&GrantTarget> {
    if let Change::Grant { target, .. } | Change::Revoke { target, .. } = c {
        Some(target)
    } else {
        None
    }
}

// Role identity is sealed separately by AuthorizationCondition. Reference
// rows and pbps-only metadata cannot change catalog prerequisites.
fn changes_catalog(c: &Change) -> bool {
    !matches!(
        c,
        Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::CreateRole { .. }
            | Change::RenameRole { .. }
            | Change::DropRole { .. }
    )
}
