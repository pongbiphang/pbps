//! Complete typed ownership inventories for the final ordered plan.
//!
//! Catalog addresses come only from the two qualified captures. Recorded
//! UIDs distinguish a renamed old name from a new object reusing that name.

use super::Error;
use pbps_model::resolver::{ObjectIdentity, ObjectOwnership, ObjectTransition, Surface};
use pbps_model::{Change, ChangeSet, Schema};
use pbps_pg::resolver::capture::BindingRecord;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Affected {
    children: bool,
    opening: bool,
    closing: bool,
    /// A DROP can coexist with a CREATE or a rename into the same spelling
    /// under another UID. Each dropped UID's opening endpoint is kept, beside
    /// the one the desired UID records; a later change never replaces them.
    dropped_before: BTreeSet<Surface>,
    /// A change other than a drop also addresses this spelling, so the UID
    /// the desired schema records there has an opening endpoint too.
    kept: bool,
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

/// Whether an identity names one of `set` anywhere in its signature.
fn mentions(object: &ObjectIdentity, set: &BTreeSet<ObjectIdentity>) -> bool {
    object
        .signature
        .iter()
        .any(|part| set.contains(part) || mentions(part, set))
}

/// `seed` and every record tied to it: those a dependency row ties to a
/// record already reached (a foreign key on another table that points at
/// the seed), then, to a fixed point, every record whose identity names one
/// (that key's RI triggers on either table and their dependency rows).
fn reach(records: &[BindingRecord], seed: &BTreeSet<ObjectIdentity>) -> BTreeSet<ObjectIdentity> {
    let mut found = seed.clone();
    loop {
        let size = found.len();
        let dependents: Vec<_> = records
            .iter()
            .filter(|entry| entry.object.class == "pg_depend")
            .filter(|entry| {
                entry
                    .object
                    .signature
                    .get(1)
                    .is_some_and(|referenced| found.contains(referenced))
            })
            .filter_map(|entry| entry.object.signature.first().cloned())
            .collect();
        found.extend(dependents);
        let named: Vec<_> = records
            .iter()
            .filter(|entry| mentions(&entry.object, &found))
            .map(|entry| entry.object.clone())
            .collect();
        found.extend(named);
        if found.len() == size {
            return found;
        }
    }
}

/// The table a table-family surface hangs off.
fn table_of(surface: &Surface) -> Option<&pbps_model::TableName> {
    match surface {
        Surface::Table(table) | Surface::Check { table, .. } | Surface::Index { table, .. } => {
            Some(table)
        }
        Surface::Column(column) | Surface::Default(column) => Some(&column.table),
        Surface::Namespace(_) | Surface::Module(_) => None,
    }
}

/// A touched table's tree and what a dependency ties to it, less what
/// another transition already inventories. Only records a table-family
/// surface owns ride; a view or routine tied to the table keeps its own
/// transition or its full fingerprint, and an unqualified record never
/// rides (DEC-1274.1, #1466).
fn sweep(
    records: &[BindingRecord],
    tree: &BTreeSet<ObjectIdentity>,
    taken: &BTreeSet<ObjectIdentity>,
) -> BTreeSet<ObjectIdentity> {
    let reached = reach(records, tree);
    records
        .iter()
        .filter(|entry| {
            reached.contains(&entry.object)
                && !taken.contains(&entry.object)
                && matches!(&entry.ownership, ObjectOwnership::Surface(owner)
                    if table_of(owner).is_some())
        })
        .map(|entry| entry.object.clone())
        .collect()
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
    let mut final_tables = base.ids.tables.clone();
    for step in &changes.changes {
        if let Change::CreateTable { uid, name, .. } = &step.change {
            final_tables.insert(uid.clone(), name.clone());
        } else if let Change::RenameTable { uid, to, .. } = &step.change {
            final_tables.insert(uid.clone(), to.clone());
        }
    }
    let mut tables = BTreeMap::new();
    for (uid, table) in &base.ids.tables {
        if tables.insert(table.clone(), uid.clone()).is_some() {
            return Err(Error::Binding(
                "duplicate recorded opening table identity".into(),
            ));
        }
    }
    for step in &changes.changes {
        let parent = if let Change::AddColumn { table, .. } | Change::RenameColumn { table, .. } =
            &step.change
        {
            Some(table)
        } else if let Change::DropColumn { column, .. } = &step.change {
            Some(&column.table)
        } else {
            None
        };
        if let Some(table) = parent {
            let uid = tables.get(table).ok_or_else(|| {
                Error::Binding("a column-vector change lacks its recorded table UID".into())
            })?;
            let final_table = final_tables.get(uid).ok_or_else(|| {
                Error::Binding(
                    "a parent inventory lacks its recorded closing table endpoint".into(),
                )
            })?;
            if desired
                .ids
                .tables
                .get(uid)
                .is_some_and(|recorded| recorded != final_table)
            {
                return Err(Error::Binding(
                    "a parent inventory disagrees with its closing table UID".into(),
                ));
            }
            let prior = base.ids.tables.get(uid).cloned().map(Surface::Table);
            let entry = affected
                .entry(Surface::Table(final_table.clone()))
                .or_default();
            // Keep the parent exact: adding it must not grant authority over
            // other columns, defaults, checks or independent indexes. They
            // still ride on the table as references, below (#1466).
            entry.opening = true;
            entry.closing = true;
            entry.dropped_before.extend(prior);
        }
        let (surface, children, opening, closing, dropped) = match &step.change {
            Change::CreateTable { uid, name, .. } => {
                if tables.values().any(|known| known == uid)
                    || tables.insert(name.clone(), uid.clone()).is_some()
                {
                    return Err(Error::Binding(
                        "a table creation reuses a live recorded identity".into(),
                    ));
                }
                let endpoint = final_tables.get(uid).ok_or_else(|| {
                    Error::Binding("a created table lacks its recorded closing endpoint".into())
                })?;
                (Surface::Table(endpoint.clone()), true, false, true, None)
            }
            Change::DropTable { uid, name, .. } => {
                if tables.remove(name).as_ref() != Some(uid) {
                    return Err(Error::Binding(
                        "a table drop disagrees with its recorded UID".into(),
                    ));
                }
                (
                    Surface::Table(name.clone()),
                    true,
                    true,
                    false,
                    Some(Surface::Table(
                        base.ids
                            .tables
                            .get(uid)
                            .cloned()
                            .unwrap_or_else(|| name.clone()),
                    )),
                )
            }
            Change::RenameTable { uid, from, to, .. } => {
                if tables.remove(from).as_ref() != Some(uid)
                    || tables.insert(to.clone(), uid.clone()).is_some()
                {
                    return Err(Error::Binding(
                        "a table rename disagrees with its recorded UID".into(),
                    ));
                }
                let endpoint = final_tables.get(uid).ok_or_else(|| {
                    Error::Binding("a renamed table lacks its recorded closing endpoint".into())
                })?;
                (
                    Surface::Table(endpoint.clone()),
                    true,
                    true,
                    true,
                    base.ids.tables.get(uid).cloned().map(Surface::Table),
                )
            }
            Change::AddColumn {
                uid, table, name, ..
            } => {
                let final_column = final_column(uid, &table.column(name), desired);
                (Surface::Column(final_column), true, false, true, None)
            }
            // A computed column is SQL Server's (#1174), and PostgreSQL
            // evidence has no transition for it; refuse rather than seal a
            // plan whose column no inventory covers.
            Change::AddComputedColumn { .. } | Change::DropComputedColumn { .. } => {
                return Err(Error::Binding(
                    "a computed column has no PostgreSQL resolver transition".into(),
                ));
            }
            Change::DropColumn { uid, column, .. } => (
                Surface::Column(column.clone()),
                true,
                true,
                false,
                Some(Surface::Column(
                    base.ids
                        .columns
                        .get(uid)
                        .cloned()
                        .unwrap_or_else(|| column.clone()),
                )),
            ),
            Change::RenameColumn { uid, table, to, .. } => {
                let final_column = final_column(uid, &table.column(to), desired);
                (
                    Surface::Column(final_column),
                    true,
                    true,
                    true,
                    base.ids.columns.get(uid).cloned().map(Surface::Column),
                )
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
                        "an expression change disagrees with its recorded desired column UID"
                            .into(),
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
            // Authorization changes leave every fingerprinted property as it
            // was (DEC-1274.1): the target stays an untouched input, and the
            // ordinary apply guard checks that a declared grant took.
            Change::PublicExecution { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::InsertRow { .. }
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
        entry.kept |= dropped.is_none();
        entry.dropped_before.extend(dropped);
    }
    let (mut transitions, opening_endpoints): (Vec<ObjectTransition>, Vec<BTreeSet<Surface>>) =
        affected
            .into_iter()
            .map(|(surface, intent)| {
                let mut priors = intent.dropped_before;
                if priors.is_empty() || intent.kept {
                    priors.extend(opening_endpoint(&surface, base, desired));
                }
                let before = if intent.opening {
                    priors
                        .iter()
                        .filter(|prior| declared(base.schema, prior))
                        .flat_map(|prior| inventory(opening, prior, intent.children))
                        .collect()
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
                        references: BTreeSet::new(),
                        surface,
                        before,
                        after,
                    },
                    priors,
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
            if opening_endpoints[index].iter().any(|child_opening| {
                opening_endpoints[parent]
                    .iter()
                    .any(|parent_opening| specializes(child_opening, parent_opening))
            }) {
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
    // One DDL statement reaches past its own surface: a retype rebuilds a
    // covering index, a default change flips the column's flag, a rename
    // rewrites another table's foreign key. The table is the unit of the
    // closing inventory (#1466): each touched table's whole tree, and what a
    // dependency ties to it, rides on one of its transitions as references.
    // The exact per-surface inventories above still carry the authority.
    let mut anchors: BTreeMap<pbps_model::TableName, usize> = BTreeMap::new();
    let mut openings: BTreeMap<pbps_model::TableName, BTreeSet<pbps_model::TableName>> =
        BTreeMap::new();
    for (index, transition) in transitions.iter().enumerate() {
        let Some(table) = table_of(&transition.surface) else {
            continue;
        };
        let anchor = anchors.entry(table.clone()).or_insert(index);
        if transition.surface == Surface::Table(table.clone()) {
            *anchor = index;
        }
        openings.entry(table.clone()).or_default().extend(
            opening_endpoints[index]
                .iter()
                .filter_map(table_of)
                .filter(|name| base.schema.tables.contains_key(*name))
                .cloned(),
        );
    }
    let mut taken_before: BTreeSet<_> = transitions
        .iter()
        .flat_map(|t| t.before.iter().cloned())
        .collect();
    let mut taken_after: BTreeSet<_> = transitions
        .iter()
        .flat_map(|t| t.after.iter().cloned())
        .collect();
    for (table, anchor) in anchors {
        let opening_tree: BTreeSet<_> = openings
            .get(&table)
            .into_iter()
            .flatten()
            .flat_map(|name| inventory(opening, &Surface::Table(name.clone()), true))
            .collect();
        let compiled_tree = if desired.schema.tables.contains_key(&table) {
            inventory(compiled, &Surface::Table(table.clone()), true)
        } else {
            BTreeSet::new()
        };
        let before = sweep(opening, &opening_tree, &taken_before);
        let after = sweep(compiled, &compiled_tree, &taken_after);
        taken_before.extend(before.iter().cloned());
        taken_after.extend(after.iter().cloned());
        let transition = &mut transitions[anchor];
        transition.references.extend(before.iter().cloned());
        transition.references.extend(after.iter().cloned());
        transition.before.extend(before);
        transition.after.extend(after);
    }
    Ok(transitions)
}
