//! Ownership facts are independent of the proposed transition inventory.

use super::Surface;
use crate::{Change, ChangeSet};

/// Catalog capture alone does not prove which managed surface may change a
/// record. A qualified adapter maps engine-proved ownership into this typed
/// fact; references to an object are not ownership of it (SPEC 9.3.2).
/// Unqualified records remain valid read prerequisites, never write authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "surface",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum ObjectOwnership {
    Unqualified,
    Surface(Surface),
}

impl ObjectOwnership {
    pub(super) fn permits(&self, surface: &Surface, changes: &ChangeSet) -> bool {
        let Self::Surface(owner) = self else {
            return false;
        };
        let owner = relocated(owner, changes);
        let surface = relocated(surface, changes);
        owner == surface
            || match (&surface, &owner) {
                (Surface::Table(table), Surface::Column(column) | Surface::Default(column)) => {
                    table == &column.table
                }
                (
                    Surface::Table(table),
                    Surface::Check { table: child, .. } | Surface::Index { table: child, .. },
                ) => table == child,
                (Surface::Column(column), Surface::Default(child)) => column == child,
                _ => false,
            }
    }
}

// Both inventories may live under the old or new spelling. Only the explicit
// identity decisions in the approved plan can equate these typed owners;
// catalog-class/name parsing remains the adapter's responsibility.
#[allow(clippy::wildcard_enum_match_arm)]
fn relocated(surface: &Surface, changes: &ChangeSet) -> Surface {
    let mut result = surface.clone();
    for planned in &changes.changes {
        match &planned.change {
            Change::RenameTable { from, to, .. } => {
                let table = match &mut result {
                    Surface::Table(table)
                    | Surface::Check { table, .. }
                    | Surface::Index { table, .. } => Some(table),
                    Surface::Column(column) | Surface::Default(column) => Some(&mut column.table),
                    Surface::Namespace(_) | Surface::Module(_) => None,
                };
                if let Some(table) = table
                    && table == from
                {
                    *table = to.clone();
                }
            }
            Change::RenameColumn {
                table,
                from,
                to,
                table_was,
                ..
            } => {
                if let Surface::Column(column) | Surface::Default(column) = &mut result
                    && (&column.table == table || table_was.as_ref() == Some(&column.table))
                    && &column.name == from
                {
                    column.name = to.clone();
                }
            }
            _ => {}
        }
    }
    result
}
