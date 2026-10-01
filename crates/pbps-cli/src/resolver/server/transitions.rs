//! Complete typed ownership inventories for the final ordered plan.
//!
//! Catalog addresses come only from the two qualified captures. Recorded
//! UIDs distinguish a renamed old name from a new object reusing that name.

use super::Error;
use pbps_model::resolver::{ObjectOwnership, ObjectTransition, Surface};
use pbps_model::{Change, ChangeSet, GrantTarget, ModuleId, Schema};
use pbps_pg::resolver::capture::BindingRecord;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Affected {
    children: bool,
    opening: bool,
    closing: bool,
    /// A DROP can coexist with a CREATE of the same spelling under a new UID.
    /// Its opening endpoint is the dropped UID, never the desired one's.
    dropped_before: Option<Surface>,
}

fn contains(owner: &ObjectOwnership, surface: &Surface, children: bool) -> bool {
    let ObjectOwnership::Surface(actual) = owner else {
        return false;
    };
    actual == surface
        || children
            && match (surface, actual) {
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

fn inventory(
    records: &[BindingRecord],
    surface: &Surface,
    children: bool,
) -> BTreeSet<pbps_model::resolver::ObjectIdentity> {
    records
        .iter()
        .filter(|entry| contains(&entry.ownership, surface, children))
        .map(|entry| entry.object.clone())
        .collect()
}

fn object_grant(
    name: &pbps_model::TableName,
    opening: &[BindingRecord],
    compiled: &[BindingRecord],
) -> Result<Surface, Error> {
    let table = Surface::Table(name.clone());
    let view = Surface::Module(ModuleId::Named(name.clone()));
    let present = |surface: &Surface| {
        !inventory(opening, surface, false).is_empty()
            || !inventory(compiled, surface, false).is_empty()
    };
    match (present(&table), present(&view)) {
        (true, false) => Ok(table),
        (false, true) => Ok(view),
        _ => Err(Error::Binding(
            "an object grant has no unique qualified table or view owner".into(),
        )),
    }
}

fn final_column(
    uid: &pbps_model::Uid,
    old: &pbps_model::ColumnRef,
    desired: pbps_diff::Side<'_>,
) -> pbps_model::ColumnRef {
    desired
        .ids
        .columns
        .get(uid)
        .cloned()
        .unwrap_or_else(|| old.clone())
}

fn opening_endpoint(
    final_surface: &Surface,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
) -> Option<Surface> {
    match final_surface {
        Surface::Namespace(name) => Some(Surface::Namespace(name.clone())),
        Surface::Table(table) => {
            let uid = desired.ids.table_uid(table)?;
            base.ids.tables.get(uid).cloned().map(Surface::Table)
        }
        Surface::Column(column) | Surface::Default(column) => {
            let uid = desired.ids.column_uid(column)?;
            let prior = base.ids.columns.get(uid)?.clone();
            Some(if matches!(final_surface, Surface::Default(_)) {
                Surface::Default(prior)
            } else {
                Surface::Column(prior)
            })
        }
        Surface::Check { table, name } | Surface::Index { table, name } => {
            let uid = desired.ids.table_uid(table)?;
            let prior = base.ids.tables.get(uid)?.clone();
            Some(if matches!(final_surface, Surface::Check { .. }) {
                Surface::Check {
                    table: prior,
                    name: name.clone(),
                }
            } else {
                Surface::Index {
                    table: prior,
                    name: name.clone(),
                }
            })
        }
        Surface::Module(id) => Some(Surface::Module(id.clone())),
    }
}

fn declared(schema: &Schema, surface: &Surface) -> bool {
    match surface {
        Surface::Namespace(_) => true,
        Surface::Table(table) => schema.tables.contains_key(table),
        Surface::Column(column) => schema
            .tables
            .get(&column.table)
            .is_some_and(|table| table.columns.contains_key(&column.name)),
        Surface::Default(column) => schema
            .tables
            .get(&column.table)
            .and_then(|table| table.columns.get(&column.name))
            .is_some_and(|column| column.default.is_some() || column.generated.is_some()),
        Surface::Check { table, name } => schema
            .tables
            .get(table)
            .is_some_and(|table| table.checks.contains_key(name)),
        Surface::Index { table, name } => schema
            .tables
            .get(table)
            .is_some_and(|table| table.indexes.contains_key(name)),
        Surface::Module(id) => schema.modules.contains_key(id),
    }
}

fn specializes(child: &Surface, parent: &Surface) -> bool {
    match (parent, child) {
        (Surface::Table(table), Surface::Column(column) | Surface::Default(column)) => {
            table == &column.table
        }
        (
            Surface::Table(table),
            Surface::Check { table: owner, .. } | Surface::Index { table: owner, .. },
        ) => table == owner,
        (Surface::Column(column), Surface::Default(owner)) => column == owner,
        _ => false,
    }
}

/// Source coordinates are selected by change kind. CREATE and ADD already
/// name the final object; RENAME's `to` is final and its recorded UID locates
/// the opening spelling. DROP records an explicit opening endpoint so a
/// subsequent CREATE reusing the spelling cannot steal its inventory.
pub(super) fn derive(
    changes: &ChangeSet,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    opening: &[BindingRecord],
    compiled: &[BindingRecord],
) -> Result<Vec<ObjectTransition>, Error> {
    let mut affected: BTreeMap<Surface, Affected> = BTreeMap::new();
    for step in &changes.changes {
        let (surface, children, opening, closing, dropped) = match &step.change {
            Change::CreateTable { name, .. } => {
                (Surface::Table(name.clone()), true, false, true, None)
            }
            Change::DropTable { name, .. } => (
                Surface::Table(name.clone()),
                true,
                true,
                false,
                Some(Surface::Table(name.clone())),
            ),
            Change::RenameTable { to, .. } => (Surface::Table(to.clone()), true, true, true, None),
            Change::AddColumn { table, name, .. } => {
                (Surface::Column(table.column(name)), true, false, true, None)
            }
            Change::DropColumn { column, .. } => (
                Surface::Column(column.clone()),
                true,
                true,
                false,
                Some(Surface::Column(column.clone())),
            ),
            Change::RenameColumn { table, to, .. } => {
                (Surface::Column(table.column(to)), true, true, true, None)
            }
            Change::AlterColumnType { uid, column, .. } => (
                Surface::Column(final_column(uid, column, desired)),
                true,
                true,
                true,
                None,
            ),
            Change::AlterColumnNullability { uid, column, .. } => (
                Surface::Column(final_column(uid, column, desired)),
                false,
                true,
                true,
                None,
            ),
            Change::AlterColumnDefault {
                uid,
                column,
                from,
                to,
            } => {
                let surface = Surface::Default(column.clone());
                // A teardown may carry an old address while an ordinary
                // removal carries the final one. The recorded UID identifies
                // the same opening default in either case.
                let prior = if from.is_some() {
                    Some(Surface::Default(
                        base.ids.columns.get(uid).cloned().ok_or_else(|| {
                            Error::Binding(
                                "a default change lacks its recorded opening column UID".into(),
                            )
                        })?,
                    ))
                } else {
                    None
                };
                (surface, false, from.is_some(), to.is_some(), prior)
            }
            Change::AlterColumnExpression { uid, column, .. } => {
                let prior = base.ids.columns.get(uid).cloned().ok_or_else(|| {
                    Error::Binding(
                        "an expression change lacks its recorded opening column UID".into(),
                    )
                })?;
                if desired.ids.columns.get(uid) != Some(column) {
                    return Err(Error::Binding(
                        "an expression change disagrees with its recorded desired column UID".into(),
                    ));
                }
                // SET EXPRESSION replaces pg_attrdef, not the column (DEC-1168.1).
                // The recorded UID selects the opening inventory across renames.
                (
                    Surface::Default(column.clone()),
                    false,
                    true,
                    true,
                    Some(Surface::Default(prior)),
                )
            }
            Change::SetPrimaryKey { table, .. }
            | Change::AddUnique { table, .. }
            | Change::DropUnique { table, .. }
            | Change::AddForeignKey { table, .. }
            | Change::DropForeignKey { table, .. } => {
                (Surface::Table(table.clone()), false, true, true, None)
            }
            Change::AddCheck { table, name, .. } => (
                Surface::Check {
                    table: table.clone(),
                    name: name.clone(),
                },
                false,
                false,
                true,
                None,
            ),
            Change::DropCheck { table, name } => {
                let surface = Surface::Check {
                    table: table.clone(),
                    name: name.clone(),
                };
                (surface.clone(), false, true, false, Some(surface))
            }
            Change::AddIndex { table, name, .. } => (
                Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                },
                false,
                false,
                true,
                None,
            ),
            Change::DropIndex { table, name } => {
                let surface = Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                };
                (surface.clone(), false, true, false, Some(surface))
            }
            Change::CreateModule { id, .. } => {
                (Surface::Module(id.clone()), false, false, true, None)
            }
            Change::AlterModule { id, .. } => {
                (Surface::Module(id.clone()), false, true, true, None)
            }
            Change::DropModule { id, .. } => (
                Surface::Module(id.clone()),
                false,
                true,
                false,
                Some(Surface::Module(id.clone())),
            ),
            Change::PublicExecution { routine, .. } => (
                Surface::Module(ModuleId::Routine(routine.clone())),
                false,
                true,
                true,
                None,
            ),
            Change::Grant { target, .. } | Change::Revoke { target, .. } => (
                match target {
                    GrantTarget::Schema(name) => Surface::Namespace(name.clone()),
                    GrantTarget::Routine(id) => Surface::Module(ModuleId::Routine(id.clone())),
                    GrantTarget::Object(name) => object_grant(name, opening, compiled)?,
                },
                false,
                true,
                true,
                None,
            ),
            Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::CreateRole { .. }
            | Change::RenameRole { .. }
            | Change::DropRole { .. } => continue,
        };
        let entry = affected.entry(surface).or_default();
        entry.children |= children;
        entry.opening |= opening;
        entry.closing |= closing;
        if dropped.is_some() {
            entry.dropped_before = dropped;
        }
    }
    let (mut transitions, opening_endpoints): (Vec<ObjectTransition>, Vec<Option<Surface>>) =
        affected
            .into_iter()
            .map(|(surface, intent)| {
                let prior = intent
                    .dropped_before
                    .or_else(|| opening_endpoint(&surface, base, desired));
                let before = if intent.opening {
                    prior
                        .as_ref()
                        .filter(|prior| declared(base.schema, prior))
                        .map(|prior| inventory(opening, prior, intent.children))
                        .unwrap_or_default()
                } else {
                    BTreeSet::new()
                };
                let after = if intent.closing && declared(desired.schema, &surface) {
                    inventory(compiled, &surface, intent.children)
                } else {
                    BTreeSet::new()
                };
                (
                    ObjectTransition {
                        surface,
                        before,
                        after,
                    },
                    prior,
                )
            })
            .unzip();
    // Parent renames cover their complete child inventory, while a separately
    // changed child has its own exact typed owner. Give each catalog address
    // to that child once; projection still checks that every parent and child
    // mutation has a covering transition and that the union is complete.
    for parent in 0..transitions.len() {
        let mut child_before = BTreeSet::new();
        let mut child_after = BTreeSet::new();
        for (index, child) in transitions.iter().enumerate() {
            if index == parent {
                continue;
            }
            // A teardown keeps its opening spelling even when another UID
            // takes that name in the desired schema. Use the endpoint that
            // selected its opening inventory, never resolve it again by the
            // final spelling of the transition.
            if let (Some(child_opening), Some(parent_opening)) =
                (&opening_endpoints[index], &opening_endpoints[parent])
                && specializes(child_opening, parent_opening)
            {
                child_before.extend(child.before.iter().cloned());
            }
            if specializes(&child.surface, &transitions[parent].surface) {
                child_after.extend(child.after.iter().cloned());
            }
        }
        transitions[parent]
            .before
            .retain(|object| !child_before.contains(object));
        transitions[parent]
            .after
            .retain(|object| !child_after.contains(object));
    }
    Ok(transitions)
}
