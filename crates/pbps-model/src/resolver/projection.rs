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

// Only typed changes authorize transitions. The adapter identifies catalog
// records; it cannot use an unrelated plan as permission to change an input.
#[allow(clippy::wildcard_enum_match_arm)]
fn touches(c: &Change, surface: &Surface) -> bool {
    match surface {
        Surface::Namespace(name) => {
            grant_target(c).is_some_and(|g| matches!(g, GrantTarget::Schema(s) if s == name))
        }
        Surface::Table(t) => {
            (c.objects().any(|o| o == t)
                && !matches!(
                    c,
                    Change::InsertRow { .. }
                        | Change::UpdateRow { .. }
                        | Change::DeleteRow { .. }
                        | Change::SetDataMode { .. }
                        | Change::SetColumnDeprecated { .. }
                ))
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
