//! Run-scoped names for the roles a plan names (#2379).
//!
//! A cluster's roles are shared by every database on it, so a rehearsal on a
//! shared scratch cluster can neither create the target's role names (two runs
//! would collide, and the roles would outlive the run) nor grant to roles the
//! scratch lacks. Each role a plan names is therefore mapped to a role created
//! for the run, whose name carries the run's token. The rehearsal then lowers
//! the same model with the mapped names, so the grants, the policies and the
//! comparison all run as written; nothing is rewritten in text.
//!
//! This is the engine-neutral half. Which roles an engine keeps under their own
//! name, how it creates and drops the run's roles, and what the names look like
//! are the engine's, and are given to [`RoleMapping::generate`].

use std::collections::{BTreeMap, BTreeSet};

use crate::change::{Change, ChangeSet};
use crate::schema::{PolicyRoles, RowPolicy, Schema};
use crate::{IdsFile, RoleName};

/// Each role a plan names, and the role the run creates for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleMapping {
    to_run: BTreeMap<RoleName, RoleName>,
    /// The mapped names in the order they were numbered.
    created: Vec<RoleName>,
}

impl RoleMapping {
    /// Maps every name in `names` that `keeps` does not keep under its own name
    /// to `<label>_<n>_<token>`, `n` counting the mapped names in order, so the
    /// same names always map the same way within a run.
    pub fn generate<'a>(
        names: impl IntoIterator<Item = &'a RoleName>,
        label: &str,
        token: &str,
        keeps: impl Fn(&RoleName) -> bool,
    ) -> Self {
        let names: BTreeSet<_> = names.into_iter().collect();
        let mut to_run = BTreeMap::new();
        let mut created = Vec::new();
        for name in names.into_iter().filter(|name| !keeps(name)) {
            let mapped: RoleName = format!("{label}_{}_{token}", created.len()).into();
            created.push(mapped.clone());
            to_run.insert(name.clone(), mapped);
        }
        Self { to_run, created }
    }

    /// Every role under its own name. A rehearsal on a cluster that is its
    /// own, or whose roles are the target's, needs no other names; `created`
    /// is then the names the run has to make, which may be none.
    pub fn unchanged(created: impl IntoIterator<Item = RoleName>) -> Self {
        Self {
            to_run: BTreeMap::new(),
            created: created.into_iter().collect(),
        }
    }

    /// The names the run creates, in the order they are numbered.
    pub fn created(&self) -> &[RoleName] {
        &self.created
    }

    pub fn is_empty(&self) -> bool {
        self.to_run.is_empty()
    }

    /// The run's name for `role`, or `role` itself where it is kept or unknown.
    pub fn name(&self, role: &RoleName) -> RoleName {
        self.to_run.get(role).unwrap_or(role).clone()
    }

    fn roles(&self, roles: &BTreeSet<RoleName>) -> BTreeSet<RoleName> {
        roles.iter().map(|role| self.name(role)).collect()
    }

    fn policy(&self, policy: &mut RowPolicy) {
        if let PolicyRoles::Roles(roles) = &policy.to {
            policy.to = PolicyRoles::Roles(self.roles(roles));
        }
    }

    /// `schema` with every role it names under the run's name.
    pub fn schema(&self, schema: &Schema) -> Schema {
        let mut mapped = schema.clone();
        mapped.roles = std::mem::take(&mut mapped.roles)
            .into_iter()
            .map(|(name, role)| (self.name(&name), role))
            .collect();
        for table in mapped.tables.values_mut() {
            for policy in table.row_policies.values_mut() {
                self.policy(policy);
            }
        }
        mapped
    }

    /// `ids` with the same names, so identity still finds the role it names.
    pub fn ids(&self, ids: &IdsFile) -> IdsFile {
        let mut mapped = ids.clone();
        for name in mapped.roles.values_mut() {
            *name = self.name(name);
        }
        mapped
    }

    /// `changes` with every role any change names under the run's name.
    pub fn changes(&self, changes: &ChangeSet) -> ChangeSet {
        let mut mapped = changes.clone();
        for planned in &mut mapped.changes {
            self.change(&mut planned.change);
        }
        mapped
    }

    /// Exhaustive over [`Change`], so a change added later that names a role
    /// has to be classified here as it is for [`Change::named_roles`].
    fn change(&self, change: &mut Change) {
        match change {
            Change::CreateRole { name, .. } => *name = self.name(name),
            Change::DropRole { name, members, .. } => {
                *name = self.name(name);
                for member in members {
                    *member = self.name(member);
                }
            }
            Change::Grant { role, .. } | Change::Revoke { role, .. } => {
                *role = self.name(role);
            }
            Change::RenameRole { from, to, .. } => {
                *from = self.name(from);
                *to = self.name(to);
            }
            Change::CreatePolicy { policy, .. } | Change::DropPolicy { policy, .. } => {
                self.policy(policy);
            }
            Change::RebuildPartitionedTable(_)
            | Change::BuildPartitionedIndex(_)
            | Change::CopyTableRows(_)
            | Change::AnalyzeTableTree(_)
            | Change::PublicExecution { .. }
            | Change::AddEnumValue { .. }
            | Change::RenameEnumValue { .. }
            | Change::EnumRebuild { .. }
            | Change::CreateType { .. }
            | Change::DropType { .. }
            | Change::RenameType { .. }
            | Change::CreateTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterDomain { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::AlterColumnNumbering { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. } => {}
        }
    }
}

impl Change {
    /// Every role this change names: those [`Change::roles`] reaches, the
    /// members a dropped role loses, and the roles a row-security policy it
    /// creates or drops applies to, which that accessor leaves out because
    /// they are a part of something else. [`RoleMapping::change`] is the
    /// exhaustive match a change added later has to be classified in.
    pub fn named_roles(&self) -> BTreeSet<RoleName> {
        let mut roles: BTreeSet<_> = self.roles().cloned().collect();
        if let Change::CreatePolicy { policy, .. } | Change::DropPolicy { policy, .. } = self
            && let PolicyRoles::Roles(named) = &policy.to
        {
            roles.extend(named.iter().cloned());
        }
        if let Change::DropRole { members, .. } = self {
            roles.extend(members.iter().cloned());
        }
        roles
    }
}

impl Schema {
    /// Every role the schema names: the roles it declares and the roles its
    /// row-security policies apply to.
    pub fn named_roles(&self) -> BTreeSet<RoleName> {
        let mut roles: BTreeSet<_> = self.roles.keys().cloned().collect();
        for table in self.tables.values() {
            for policy in table.row_policies.values() {
                if let PolicyRoles::Roles(named) = &policy.to {
                    roles.extend(named.iter().cloned());
                }
            }
        }
        roles
    }
}

#[cfg(test)]
mod tests;
