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
    pub(super) fn matches_surface(&self, surface: &Surface) -> bool {
        matches!(self, Self::Surface(owner) if owner == surface)
    }

    pub(super) fn permits(&self, surface: &Surface) -> bool {
        let Self::Surface(owner) = self else {
            return false;
        };
        owner == surface
            || match (surface, owner) {
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

/// Resolve a statement-time owner to one snapshot, never rewrite a closing
/// record as though its name belonged to the opening snapshot. Once a rename
/// identifies a UID, another object's reuse of that spelling cannot move it.
pub(super) fn at_endpoint(
    surface: &Surface,
    changes: &ChangeSet,
    step: usize,
    before: bool,
) -> Surface {
    let mut result = surface.clone();
    let mut table_uid = None;
    let mut column_uid = None;
    if before {
        for planned in changes.changes[..step].iter().rev() {
            relocate(
                &mut result,
                &planned.change,
                false,
                &mut table_uid,
                &mut column_uid,
            );
        }
    } else {
        for planned in &changes.changes[step..] {
            relocate(
                &mut result,
                &planned.change,
                true,
                &mut table_uid,
                &mut column_uid,
            );
        }
    }
    result
}

/// A transition may be labelled with either spelling of this exact rename.
/// Normalize that label at this step before resolving it to the snapshot;
/// a reused name in a different UID's rename is not an interchangeable label.
pub(super) fn transition_at_endpoint(
    surface: &Surface,
    changes: &ChangeSet,
    step: usize,
    before: bool,
) -> Surface {
    let mut opening = surface.clone();
    relocate(
        &mut opening,
        &changes.changes[step].change,
        false,
        &mut None,
        &mut None,
    );
    at_endpoint(&opening, changes, step, before)
}

#[allow(clippy::wildcard_enum_match_arm)]
fn relocate(
    surface: &mut Surface,
    change: &Change,
    forward: bool,
    table_uid: &mut Option<crate::Uid>,
    column_uid: &mut Option<crate::Uid>,
) {
    match change {
        Change::RenameTable { uid, from, to, .. } => {
            let table = match surface {
                Surface::Table(table)
                | Surface::Check { table, .. }
                | Surface::Index { table, .. } => Some(table),
                Surface::Column(column) | Surface::Default(column) => Some(&mut column.table),
                Surface::Namespace(_) | Surface::Module(_) => None,
            };
            let (from, to) = if forward { (from, to) } else { (to, from) };
            if let Some(table) = table
                && table == from
                && table_uid.as_ref().is_none_or(|selected| selected == uid)
            {
                *table_uid = Some(uid.clone());
                *table = to.clone();
            }
        }
        Change::RenameColumn {
            uid,
            table,
            from,
            to,
            ..
        } => {
            let (from, to) = if forward { (from, to) } else { (to, from) };
            if let Surface::Column(column) | Surface::Default(column) = surface
                && &column.table == table
                && &column.name == from
                && column_uid.as_ref().is_none_or(|selected| selected == uid)
            {
                *column_uid = Some(uid.clone());
                column.name = to.clone();
            }
        }
        _ => {}
    }
}
