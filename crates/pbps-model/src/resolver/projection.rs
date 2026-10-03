//! Expected closing prerequisites, derived only from approved typed changes.

use super::ownership::{at_endpoint, transition_at_endpoint};
use super::{InputManifest, ManifestError, ObjectIdentity, ObjectOwnership, Surface};
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
    /// Records of a touched table's tree, or tied to it by a dependency, that
    /// no narrower surface of this plan inventories: a sibling index a retype
    /// rebuilds, the column whose default flag a default change flips, a
    /// foreign key on another table that a rename rewrites. One DDL statement
    /// reaches past its own surface, so the table is the unit of the closing
    /// inventory (DEC-1274.1, #1466). Only a table-family transition a
    /// catalog change touches may list them, and only table-family records;
    /// the adapter proves each tie.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub references: BTreeSet<ObjectIdentity>,
}

impl InputManifest {
    /// The closing manifest of an approved plan: every prerequisite the plan
    /// does not change, unchanged, plus the candidate membership, runtime-bound
    /// set and signature lookups the plan's own objects add or remove.
    ///
    /// A record the plan changes is not predicted (DEC-1274.1). Its declared
    /// properties are the managed revalidation's to check at apply, and its
    /// bindings are the sealed surface resolutions'. Scratch cannot reproduce
    /// what the target keeps undeclared about it: generated names, column
    /// order, storage settings or a column's stored missing value. `compiled`
    /// proves the closing inventories and supplies the plan's own members.
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
        let (removed, installed) = self.check_transitions(changes, Some(compiled), transitions)?;
        for object in &installed {
            if compiled
                .prerequisites()
                .binary_search_by(|p| p.object.cmp(object))
                .is_err()
            {
                return Err(ManifestError::Incomplete);
            }
        }
        let mut membership = self.membership().to_vec();
        if membership.len() != compiled.membership().len() {
            return Err(ManifestError::Incomplete);
        }
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
            result.resolved = resolved_lookup(result, desired, &removed, &installed)?;
        }
        let source: BTreeMap<_, _> = compiled
            .prerequisites()
            .iter()
            .filter(|p| installed.contains(&p.object))
            .map(|p| (p.object.clone(), p.managed_closing()))
            .collect();
        self.closing(&removed, &source, membership, runtime_bound, lookups)
    }

    /// The reader's check that `after` is this manifest's closing manifest for
    /// the plan. Without the compiled capture it proves the opening-side
    /// authority and that only the plan's own records changed.
    pub fn closing_matches(
        &self,
        changes: &ChangeSet,
        after: &Self,
        transitions: &[ObjectTransition],
    ) -> Result<bool, ManifestError> {
        let (removed, installed) = self.check_transitions(changes, None, transitions)?;
        if after.membership().len() != self.membership().len()
            || after.identifications().len() != self.identifications().len()
        {
            return Ok(false);
        }
        let mut membership = Vec::new();
        for (opening, closing) in self.membership().iter().zip(after.membership()) {
            if opening.predicate != closing.predicate {
                return Ok(false);
            }
            let kept: BTreeSet<_> = opening.members.difference(&removed).cloned().collect();
            if !kept.is_subset(&closing.members)
                || !closing
                    .members
                    .difference(&kept)
                    .all(|id| installed.contains(id))
            {
                return Ok(false);
            }
            membership.push(closing.clone());
        }
        let kept: BTreeSet<_> = self.runtime_bound().difference(&removed).cloned().collect();
        if !kept.is_subset(after.runtime_bound())
            || !after
                .runtime_bound()
                .difference(&kept)
                .all(|id| installed.contains(id))
        {
            return Ok(false);
        }
        // A placeholder is only ever a record the plan installs; which ones
        // the closing manifest must keep follows from its references below.
        let mut source = BTreeMap::new();
        for p in after
            .prerequisites()
            .iter()
            .filter(|p| p.is_managed_closing())
        {
            // The reader holds no compiled records, but it does hold each
            // placeholder's ownership: the transition installing it must
            // authorize that owner, as sealing checked.
            let owned = transitions.iter().any(|t| {
                t.after.contains(&p.object)
                    && (authorized(changes, t, &p.ownership, false)
                        || carried_reference(changes, t, &p.object, &p.ownership))
            });
            if !installed.contains(&p.object) || !owned {
                return Ok(false);
            }
            source.insert(p.object.clone(), p.clone());
        }
        let mut lookups = Vec::new();
        for (opening, closing) in self.identifications().iter().zip(after.identifications()) {
            if (&opening.signature, &opening.search_path, &opening.kind)
                != (&closing.signature, &closing.search_path, &closing.kind)
            {
                return Ok(false);
            }
            let mut lookup = opening.clone();
            lookup.resolved = resolved_lookup(opening, closing, &removed, &installed)?;
            lookups.push(lookup);
        }
        Ok(&self.closing(
            &removed,
            &source,
            membership,
            after.runtime_bound().clone(),
            lookups,
        )? == after)
    }

    /// Untouched records as they are, and a placeholder for each installed
    /// record that carries bindings or that a kept record, member, runtime
    /// limitation or lookup names. Other installed records, such as engine-
    /// named keys or sequences, are the managed revalidation's alone.
    fn closing(
        &self,
        removed: &BTreeSet<ObjectIdentity>,
        installed: &BTreeMap<ObjectIdentity, super::Prerequisite>,
        membership: Vec<super::Membership>,
        runtime_bound: BTreeSet<ObjectIdentity>,
        lookups: Vec<super::RoutineLookup>,
    ) -> Result<Self, ManifestError> {
        let mut objects: BTreeMap<_, _> = self
            .prerequisites()
            .iter()
            .map(|p| (p.object.clone(), p.clone()))
            .collect();
        for object in removed {
            if objects.remove(object).is_none() {
                return Err(ManifestError::Incomplete);
            }
        }
        let mut pending: Vec<ObjectIdentity> = installed
            .values()
            .filter(|p| !p.bindings.is_empty())
            .map(|p| p.object.clone())
            .chain(
                objects
                    .values()
                    .flat_map(|p| p.bindings.iter().map(|b| b.target.clone())),
            )
            .chain(membership.iter().flat_map(|m| m.members.iter().cloned()))
            .chain(runtime_bound.iter().cloned())
            .chain(lookups.iter().filter_map(|l| l.resolved.clone()))
            .chain(self.scope().retained.iter().cloned())
            .collect();
        while let Some(object) = pending.pop() {
            if objects.contains_key(&object) {
                continue;
            }
            let Some(placeholder) = installed.get(&object) else {
                continue;
            };
            pending.extend(placeholder.bindings.iter().map(|b| b.target.clone()));
            objects.insert(object, placeholder.clone());
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

    /// Typed authority for every transition: each catalog change has one, and
    /// each inventoried record is owned by a surface the change affects. The
    /// closing side needs the compiled records' ownership, so a reader without
    /// them checks the opening side alone.
    fn check_transitions(
        &self,
        changes: &ChangeSet,
        compiled: Option<&Self>,
        transitions: &[ObjectTransition],
    ) -> Result<(BTreeSet<ObjectIdentity>, BTreeSet<ObjectIdentity>), ManifestError> {
        // Checking supplied entries alone accepts an empty inventory for a
        // DROP, leaving the closing manifest at the pre-DDL state (SPEC 9.3.2).
        for (index, step) in changes.changes.iter().enumerate() {
            if !changes_catalog(&step.change) {
                continue;
            }
            if !transitions.iter().any(|t| {
                covers_change(&step.change, &t.surface)
                    || vector_transition_matches(&step.change, changes, index, &t.surface)
            }) {
                return Err(ManifestError::Incomplete);
            }
        }
        let mut removed = BTreeSet::new();
        let mut installed = BTreeSet::new();
        let mut owners = BTreeSet::new();
        for transition in transitions {
            if (transition.before.is_empty() && transition.after.is_empty())
                || !transition
                    .references
                    .iter()
                    .all(|r| transition.before.contains(r) || transition.after.contains(r))
                || !owners.insert(&transition.surface)
                || !changes.changes.iter().enumerate().any(|(index, p)| {
                    touches(&p.change, &transition.surface)
                        || vector_transition_matches(&p.change, changes, index, &transition.surface)
                })
            {
                return Err(ManifestError::Invalid);
            }
            let sides = [
                Some((&transition.before, self, true)),
                compiled.map(|compiled| (&transition.after, compiled, false)),
            ];
            for (inventory, manifest, before) in sides.into_iter().flatten() {
                for object in inventory {
                    let records = manifest.prerequisites();
                    let position = records
                        .binary_search_by(|p| p.object.cmp(object))
                        .map_err(|_| ManifestError::Incomplete)?;
                    let ownership = &records[position].ownership;
                    let affected = authorized(changes, transition, ownership, before)
                        || carried_reference(changes, transition, object, ownership);
                    if !affected {
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
        // and its affected records. Containment alone is not a mutation: a
        // table grant preserves columns/defaults/checks/indexes (SPEC 9.3.2).
        for (index, step) in changes.changes.iter().enumerate() {
            if let Some((candidates, opening, closing)) = changed_owners(&step.change) {
                let complete = candidates.iter().any(|owner| {
                    [
                        Some((self, &removed, opening, true)),
                        compiled.map(|compiled| (compiled, &installed, closing, false)),
                    ]
                    .into_iter()
                    .flatten()
                    .all(|(manifest, inventory, required, before)| {
                        if !required {
                            return true;
                        }
                        let owner = owner.at_endpoint(changes, index, before);
                        let records: Vec<_> = manifest
                            .prerequisites()
                            .iter()
                            .filter(|p| owner.contains(&p.ownership))
                            .collect();
                        // A grant or constraint may follow an approved CREATE,
                        // or precede a DROP. The typed plan must prove why the
                        // owner is absent at that snapshot, not the inventory.
                        if records.is_empty() && planned_absence(owner.surface(), changes, before) {
                            return true;
                        }
                        records
                            .iter()
                            .any(|p| p.ownership.matches_surface(owner.surface()))
                            && records.iter().all(|p| inventory.contains(&p.object))
                    })
                });
                if !complete {
                    return Err(ManifestError::Incomplete);
                }
            }
            // A live column-vector change also changes its table's properties.
            // This is an independent obligation, never an alternative that can
            // excuse an omitted column/default inventory (SPEC 9.3.2).
            if let Some(parent) = column_parent(&step.change) {
                let owner = OwnerScope::Exact(parent);
                let sides = [
                    Some((self, &removed, true)),
                    compiled.map(|compiled| (compiled, &installed, false)),
                ];
                for (manifest, inventory, before) in sides.into_iter().flatten() {
                    let owner = owner.at_endpoint(changes, index, before);
                    let records: Vec<_> = manifest
                        .prerequisites()
                        .iter()
                        .filter(|p| owner.contains(&p.ownership))
                        .collect();
                    if records.is_empty() && planned_absence(owner.surface(), changes, before) {
                        continue;
                    }
                    if records.is_empty() || records.iter().any(|p| !inventory.contains(&p.object))
                    {
                        return Err(ManifestError::Incomplete);
                    }
                }
            }
        }
        // A record the plan installs cannot already be an input it keeps: a
        // CREATE over an existing, untouched object is not an approved change.
        if installed.iter().any(|object| {
            !removed.contains(object)
                && self
                    .prerequisites()
                    .binary_search_by(|p| p.object.cmp(object))
                    .is_ok()
        }) {
            return Err(ManifestError::Invalid);
        }
        Ok((removed, installed))
    }
}

/// Ownership permits aggregation, not unrelated mutations. The same typed
/// scope that requires a record must also authorize replacing it (SPEC 9.3.2).
fn authorized(
    changes: &ChangeSet,
    transition: &ObjectTransition,
    ownership: &ObjectOwnership,
    before: bool,
) -> bool {
    changes.changes.iter().enumerate().any(|(index, step)| {
        vector_inventory_permitted(
            &step.change,
            changes,
            index,
            before,
            &transition.surface,
            ownership,
        ) || touches(&step.change, &transition.surface)
            && ownership.permits(&transition_at_endpoint(
                &transition.surface,
                changes,
                index,
                before,
            ))
            && (changed_owners(&step.change).is_some_and(|(candidates, opening, closing)| {
                (if before { opening } else { closing })
                    && candidates.iter().any(|owner| {
                        owner
                            .at_endpoint(changes, index, before)
                            .contains(ownership)
                    })
            }) || column_parent(&step.change).is_some_and(|parent| {
                OwnerScope::Exact(parent)
                    .at_endpoint(changes, index, before)
                    .contains(ownership)
            }))
    })
}

/// A table-family surface: a table and what hangs off it.
fn table_family(surface: &Surface) -> bool {
    matches!(
        surface,
        Surface::Table(_)
            | Surface::Column(_)
            | Surface::Default(_)
            | Surface::Check { .. }
            | Surface::Index { .. }
    )
}

/// A record of a touched table's tree, or tied to it, may ride on that
/// table's transition (DEC-1274.1, #1466): the transition must list it as a
/// reference, be a table-family surface a catalog change touches, and the
/// record must be owned by a table-family surface. A routine, view,
/// namespace or unqualified record never rides.
fn carried_reference(
    changes: &ChangeSet,
    transition: &ObjectTransition,
    object: &ObjectIdentity,
    ownership: &ObjectOwnership,
) -> bool {
    transition.references.contains(object)
        && table_family(&transition.surface)
        && matches!(ownership, ObjectOwnership::Surface(owner) if table_family(owner))
        && changes.changes.iter().enumerate().any(|(index, step)| {
            changes_catalog(&step.change)
                && (touches(&step.change, &transition.surface)
                    || vector_transition_matches(&step.change, changes, index, &transition.surface))
        })
}

/// A signature lookup may change only between the plan's own records: what
/// it named before must be removed, and what it names after installed.
fn resolved_lookup(
    opening: &super::RoutineLookup,
    closing: &super::RoutineLookup,
    removed: &BTreeSet<ObjectIdentity>,
    installed: &BTreeSet<ObjectIdentity>,
) -> Result<Option<ObjectIdentity>, ManifestError> {
    if opening.resolved != closing.resolved
        && (opening
            .resolved
            .as_ref()
            .is_some_and(|id| !removed.contains(id))
            || closing
                .resolved
                .as_ref()
                .is_some_and(|id| !installed.contains(id)))
    {
        return Err(ManifestError::Invalid);
    }
    Ok(closing.resolved.clone())
}

// Internal records share their qualified surface's atomic properties. Child
// surfaces need separate mutation authority: a table ACL/constraint change
// does not replace its columns, defaults, checks or independently named indexes.
enum OwnerScope {
    Exact(Surface),
    WithChildren(Surface),
}

impl OwnerScope {
    fn surface(&self) -> &Surface {
        match self {
            Self::Exact(surface) | Self::WithChildren(surface) => surface,
        }
    }

    fn at_endpoint(&self, changes: &ChangeSet, step: usize, before: bool) -> Self {
        let surface = at_endpoint(self.surface(), changes, step, before);
        match self {
            Self::Exact(_) => Self::Exact(surface),
            Self::WithChildren(_) => Self::WithChildren(surface),
        }
    }

    fn contains(&self, ownership: &ObjectOwnership) -> bool {
        match self {
            Self::Exact(surface) => ownership.matches_surface(surface),
            Self::WithChildren(surface) => ownership.permits(surface),
        }
    }
}

fn column_parent(change: &Change) -> Option<Surface> {
    if let Change::AddColumn { table, .. } | Change::RenameColumn { table, .. } = change {
        Some(Surface::Table(table.clone()))
    } else if let Change::DropColumn { column, .. } = change {
        Some(Surface::Table(column.table.clone()))
    } else {
        None
    }
}

// A final transition label can follow several UID-preserving renames.
// Compare it with both endpoints of the specific typed vector operation,
// rather than treating another UID's reuse of a spelling as that operation.
fn vector_owner_matches(
    owner: &OwnerScope,
    changes: &ChangeSet,
    index: usize,
    surface: &Surface,
) -> bool {
    [true, false].into_iter().any(|before| {
        ObjectOwnership::Surface(owner.at_endpoint(changes, index, before).surface().clone())
            .permits(surface)
    })
}

fn vector_transition_matches(
    change: &Change,
    changes: &ChangeSet,
    index: usize,
    surface: &Surface,
) -> bool {
    let Some(parent) = column_parent(change) else {
        return false;
    };
    vector_owner_matches(&OwnerScope::Exact(parent), changes, index, surface)
        || changed_owners(change).is_some_and(|(owners, _, _)| {
            owners
                .iter()
                .any(|owner| vector_owner_matches(owner, changes, index, surface))
        })
}

fn vector_inventory_permitted(
    change: &Change,
    changes: &ChangeSet,
    index: usize,
    before: bool,
    surface: &Surface,
    ownership: &ObjectOwnership,
) -> bool {
    let Some(parent) = column_parent(change) else {
        return false;
    };
    let parent = OwnerScope::Exact(parent);
    (vector_owner_matches(&parent, changes, index, surface)
        && parent
            .at_endpoint(changes, index, before)
            .contains(ownership))
        || changed_owners(change).is_some_and(|(owners, opening, closing)| {
            (if before { opening } else { closing })
                && owners.iter().any(|owner| {
                    vector_owner_matches(owner, changes, index, surface)
                        && owner
                            .at_endpoint(changes, index, before)
                            .contains(ownership)
                })
        })
}

// Exhaustive: every catalog mutation names its owner, extent and endpoints.
// Parent lifecycles move/create/remove children. Type conversion can replace
// a column's default, but nullability and table ACL/constraint edits do not.
fn changed_owners(change: &Change) -> Option<(Vec<OwnerScope>, bool, bool)> {
    use OwnerScope::{Exact, WithChildren};
    let (owner, opening, closing) = match change {
        Change::CreateTable { name, .. } => {
            (WithChildren(Surface::Table(name.clone())), false, true)
        }
        Change::DropTable { name, .. } => (WithChildren(Surface::Table(name.clone())), true, false),
        Change::RenameTable { from, .. } => {
            (WithChildren(Surface::Table(from.clone())), true, true)
        }
        Change::AddColumn { table, name, .. } => (
            WithChildren(Surface::Column(table.column(name))),
            false,
            true,
        ),
        Change::DropColumn { column, .. } => {
            (WithChildren(Surface::Column(column.clone())), true, false)
        }
        // A computed column is a column surface in the namespace, and its
        // dependents (an index over it) go with it (#1174).
        Change::AddComputedColumn { table, name, .. } => (
            WithChildren(Surface::Column(table.column(name))),
            false,
            true,
        ),
        Change::DropComputedColumn { table, name, .. } => (
            WithChildren(Surface::Column(table.column(name))),
            true,
            false,
        ),
        Change::RenameColumn { table, from, .. } => (
            WithChildren(Surface::Column(table.column(from))),
            true,
            true,
        ),
        Change::AlterColumnType { column, .. } => {
            (WithChildren(Surface::Column(column.clone())), true, true)
        }
        Change::AlterColumnNullability { column, .. } => {
            (Exact(Surface::Column(column.clone())), true, true)
        }
        Change::AlterColumnDefault {
            column, from, to, ..
        } => (
            Exact(Surface::Default(column.clone())),
            from.is_some(),
            to.is_some(),
        ),
        // A generation expression is the column's `pg_attrdef` row, as a
        // default is, replaced in place (DEC-1168.1).
        Change::AlterColumnExpression { column, .. } => {
            (Exact(Surface::Default(column.clone())), true, true)
        }
        // A standalone index's parameters are its own catalog row, as its
        // creation is; a key's or a unique constraint's index is the table's
        // constraint surface, as their changes are (#1442, #1483 review).
        Change::SetIndexStorageParameters {
            table,
            target: crate::change::IndexPart::Index(name),
            ..
        } => (
            Exact(Surface::Index {
                table: table.clone(),
                name: name.clone(),
            }),
            true,
            true,
        ),
        Change::SetPrimaryKey { table, .. }
        | Change::SetIndexStorageParameters { table, .. }
        | Change::SetTablePersistence { table, .. }
        | Change::SetStorageParameters { table, .. }
        | Change::SetReplicaIdentity { table, .. }
        | Change::AddUnique { table, .. }
        | Change::DropUnique { table, .. }
        | Change::AddForeignKey { table, .. }
        | Change::DropForeignKey { table, .. } => {
            (Exact(Surface::Table(table.clone())), true, true)
        }
        Change::AddCheck { table, name, .. } => (
            Exact(Surface::Check {
                table: table.clone(),
                name: name.clone(),
            }),
            false,
            true,
        ),
        Change::DropCheck { table, name } => (
            Exact(Surface::Check {
                table: table.clone(),
                name: name.clone(),
            }),
            true,
            false,
        ),
        Change::AddIndex { table, name, .. } => (
            Exact(Surface::Index {
                table: table.clone(),
                name: name.clone(),
            }),
            false,
            true,
        ),
        Change::DropIndex { table, name } => (
            Exact(Surface::Index {
                table: table.clone(),
                name: name.clone(),
            }),
            true,
            false,
        ),
        Change::CreateModule { id, .. } => (Exact(Surface::Module(id.clone())), false, true),
        Change::AlterModule { id, .. } => (Exact(Surface::Module(id.clone())), true, true),
        Change::DropModule { id, .. } => (Exact(Surface::Module(id.clone())), true, false),
        Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::CreateRole { .. }
        | Change::RenameRole { .. }
        | Change::DropRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => return None,
    };
    Some((vec![owner], opening, closing))
}

// Only an approved lifecycle operation can explain a missing endpoint for a
// mutation. Parent creation/removal owns its children; unrelated DDL cannot
// excuse a missing prerequisite (SPEC 9.3.2).
#[allow(clippy::wildcard_enum_match_arm)]
fn planned_absence(owner: &Surface, changes: &ChangeSet, before: bool) -> bool {
    let ownership = super::ObjectOwnership::Surface(owner.clone());
    changes.changes.iter().enumerate().any(|(index, step)| {
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
        ownership.permits(&at_endpoint(&boundary, changes, index, before))
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
            Surface::Default(_) => matches!(
                c,
                Change::AlterColumnDefault { .. } | Change::AlterColumnExpression { .. }
            ),
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
                // A computed column's lifecycle, which is a part's change and
                // so outside `columns_redefined` (#1174).
                || matches!(c, Change::AddComputedColumn { table, name, .. }
                    | Change::DropComputedColumn { table, name, .. }
                    if &table.column(name) == column)
                || matches!(c, Change::CreateTable { name, .. } | Change::DropTable { name, .. } if name == &column.table)
        }
        Surface::Default(column) => {
            matches!(c, Change::AlterColumnDefault { column: r, .. } | Change::AlterColumnExpression { column: r, .. } if r == column)
                || matches!(c, Change::CreateTable { name, .. } | Change::DropTable { name, .. } if name == &column.table)
                || matches!(c, Change::AddColumn { table, name, .. } if &table.column(name) == column)
        }
        Surface::Check { table, name } => {
            matches!(c, Change::AddCheck { table: t, name: n, .. } | Change::DropCheck { table: t, name: n } if t == table && n == name)
        }
        Surface::Index { table, name } => {
            matches!(c, Change::AddIndex { table: t, name: n, .. } | Change::DropIndex { table: t, name: n } if t == table && n == name)
                || matches!(c, Change::SetIndexStorageParameters { table: t, target: crate::change::IndexPart::Index(n), .. } if t == table && n == name)
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
// rows and pbps-only metadata cannot change catalog prerequisites. Neither
// can a GRANT, REVOKE or PUBLIC execution change: ACLs and owners are not
// fingerprinted (DEC-1274.1), so its target stays an untouched input with its
// full fingerprint. The ordinary apply guard checks that a declared grant
// took (SPEC 7.6); an object grant does not change a binding.
fn changes_catalog(c: &Change) -> bool {
    !matches!(
        c,
        Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::CreateRole { .. }
            | Change::RenameRole { .. }
            | Change::DropRole { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexMethod, IndexPart, TableName};

    fn set(target: IndexPart) -> Change {
        Change::SetIndexStorageParameters {
            table: TableName::new("app", "t"),
            target,
            method: IndexMethod::Btree,
            set: [("fillfactor".to_owned(), "80".to_owned())].into(),
            reset: BTreeSet::new(),
        }
    }

    /// A standalone index's parameters are its own surface's, as its
    /// creation is, so a resolver-backed plan that changes only them
    /// projects; a key's or a unique constraint's are the table's
    /// constraint surface (#1442, #1483 review).
    #[test]
    fn an_index_parameter_change_owns_the_index_surface() {
        let index = Surface::Index {
            table: TableName::new("app", "t"),
            name: "ix".into(),
        };
        let owners = |c: &Change| changed_owners(c).expect("an owned change").0;
        assert!(matches!(
            owners(&set(IndexPart::Index("ix".into()))).as_slice(),
            [OwnerScope::Exact(s)] if *s == index
        ));
        assert!(touches(&set(IndexPart::Index("ix".into())), &index));
        assert!(matches!(
            owners(&set(IndexPart::PrimaryKey)).as_slice(),
            [OwnerScope::Exact(Surface::Table(_))]
        ));
        // Negative: another index's surface, and a key's change.
        assert!(!touches(&set(IndexPart::Index("other".into())), &index));
        assert!(!touches(&set(IndexPart::PrimaryKey), &index));
    }
}
