//! The families of change that cannot occur on SQLite, or that need the table
//! rebuilt, answered by name (#2950). Each variant is listed: no wildcard arm.

use pbps_model::{
    ConstraintChange, ExtensionChange, GrantChange, PartitionChange, PolicyChange, RoleChange,
    RowChange, SearchChange, TablePropertyChange, TypeChange, TypeValueChange,
};

use super::{Sql, later, rebuild};
use crate::validate::{not_built, unsupported};

pub(super) fn table_property(change: &TablePropertyChange) -> Sql {
    Err(match change {
        TablePropertyChange::SetReplicaIdentity { .. } => unsupported("a replica identity"),
        // The differ plans comments only for an engine that holds them.
        TablePropertyChange::SetComment { .. } => {
            unsupported("comments on tables and columns: SQLite has no comment object")
        }
        TablePropertyChange::SetStorageParameters { .. } => unsupported("table storage parameters"),
        TablePropertyChange::SetTablePersistence { .. } => unsupported("unlogged tables"),
    })
}

pub(super) fn partition(change: &PartitionChange) -> Sql {
    match change {
        PartitionChange::RebuildPartitionedTable(_)
        | PartitionChange::CopyTableRows(_)
        | PartitionChange::AnalyzeTableTree(_)
        | PartitionChange::DetachPartition { .. }
        | PartitionChange::AttachPartition { .. }
        | PartitionChange::SetPartitionDefault { .. }
        | PartitionChange::SetPartitionNotNull { .. } => Err(unsupported("partitions")),
    }
}

pub(super) fn constraint(change: &ConstraintChange) -> Sql {
    Err(match change {
        ConstraintChange::SetPrimaryKey { .. } => rebuild("changing a table's primary key"),
        ConstraintChange::AddUnique { .. } | ConstraintChange::DropUnique { .. } => {
            rebuild("adding or dropping a unique constraint")
        }
        ConstraintChange::AddForeignKey { .. } => rebuild("adding a foreign key"),
        ConstraintChange::DropForeignKey { .. } => {
            later("dropping a foreign key (ALTER TABLE .. DROP CONSTRAINT)")
        }
        ConstraintChange::AddCheck { .. } | ConstraintChange::DropCheck { .. } => {
            later("adding or dropping a check constraint (ALTER TABLE .. ADD/DROP CONSTRAINT)")
        }
        ConstraintChange::SetExclusion { .. } => unsupported("exclusion constraints"),
    })
}

pub(super) fn policy(change: &PolicyChange) -> Sql {
    match change {
        PolicyChange::CreatePolicy { .. }
        | PolicyChange::DropPolicy { .. }
        | PolicyChange::SetRowSecurity { .. } => Err(unsupported("row security and policies")),
    }
}

pub(super) fn row(change: &RowChange) -> Sql {
    match change {
        RowChange::InsertRow { .. }
        | RowChange::UpdateRow { .. }
        | RowChange::DeleteRow { .. }
        | RowChange::SetDataMode { .. }
        | RowChange::SetEnsureKeys { .. } => Err(not_built("declared rows (reference data)")),
    }
}

const NO_USERS: &str =
    "roles and grants; SQLite has no users, and the file's permissions are the only control";

pub(super) fn permission(change: &GrantChange) -> Sql {
    match change {
        GrantChange::Grant { .. }
        | GrantChange::Revoke { .. }
        | GrantChange::PublicExecution { .. } => Err(unsupported(NO_USERS)),
    }
}

pub(super) fn type_value(change: &TypeValueChange) -> Sql {
    match change {
        TypeValueChange::AddEnumValue { .. }
        | TypeValueChange::RenameEnumValue { .. }
        | TypeValueChange::AlterDomain { .. }
        | TypeValueChange::EnumRebuild { .. } => Err(unsupported("declared enum and domain types")),
    }
}

pub(super) fn role(change: &RoleChange) -> Sql {
    match change {
        RoleChange::CreateRole { .. }
        | RoleChange::DropRole { .. }
        | RoleChange::RenameRole { .. } => Err(unsupported(NO_USERS)),
    }
}

pub(super) fn declared_type(change: &TypeChange) -> Sql {
    match change {
        TypeChange::CreateType { .. }
        | TypeChange::DropType { .. }
        | TypeChange::RenameType { .. } => Err(unsupported("declared enum and domain types")),
    }
}

/// The one family where this answer, and not validation, is the guard: the trait
/// has no validation hook for extensions, so a project can declare one.
pub(super) fn extension(change: &ExtensionChange) -> Sql {
    match change {
        ExtensionChange::CreateExtension { .. }
        | ExtensionChange::DropExtension { .. }
        | ExtensionChange::UpdateExtension { .. }
        | ExtensionChange::SetExtensionSchema { .. } => Err(unsupported(
            "extensions; SQLite has none, and loading one is not authorized",
        )),
    }
}

pub(super) fn search(change: &SearchChange) -> Sql {
    match change {
        SearchChange::CreateSearch { .. } | SearchChange::DropSearch { .. } => Err(unsupported(
            "declared text search configurations and dictionaries",
        )),
    }
}
