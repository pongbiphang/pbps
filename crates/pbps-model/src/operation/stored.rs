//! What the connected seam read of the baseline's stored columns for a
//! membership boundary. Pure data: the engine reads it, the planner carries
//! it, and nothing here executes or orders anything.

use crate::{ColumnRef, ModuleId};
use std::collections::{BTreeMap, BTreeSet};

/// The stored columns of the tables a boundary meets, by baseline address. A
/// column absent here was not read, which is not the same as read and empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MembershipCatalog {
    pub columns: BTreeMap<ColumnRef, StoredColumn>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredColumn {
    /// The stored DEFAULT as the engine prints it.
    pub default: Option<String>,
    /// The routines that default binds.
    pub routines: BTreeSet<ModuleId>,
    /// The columns owning the sequences that default binds.
    pub sequences: BTreeSet<ColumnRef>,
    /// Whether the default binds something the facts cannot represent.
    pub unrepresented: bool,
    /// Whether the column holds a NOT NULL of its own.
    pub local_not_null: bool,
}
