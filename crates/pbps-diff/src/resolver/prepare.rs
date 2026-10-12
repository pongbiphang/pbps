use super::Error;
use pbps_dialect::Dialect;
use pbps_model::resolver::{Surface, SurfaceResolution};
use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, Schema, ValueSource};
use std::collections::BTreeSet;

pub(super) fn surfaces(schema: &Schema) -> BTreeSet<Surface> {
    let mut result = BTreeSet::new();
    for (table, definition) in &schema.tables {
        // Generated expressions use the same binding surface as defaults;
        // omitting them rejects qualified evidence (DEC-1168.1). A
        // partition's own default is one too (DEC-1578.1).
        for column in definition.expression_columns() {
            result.insert(Surface::Default(table.column(column)));
        }
        for name in definition.checks.keys() {
            result.insert(Surface::Check {
                table: table.clone(),
                name: name.clone(),
            });
        }
        for (name, index) in &definition.indexes {
            if index.holds_expression() {
                result.insert(Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                });
            }
        }
    }
    result.extend(schema.modules.keys().cloned().map(Surface::Module));
    result
}

pub(super) fn coverage(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    observations: &[SurfaceResolution],
) -> Result<(), Error> {
    let after = surfaces(desired.schema);
    let opening = surfaces(base.schema);
    let before: BTreeSet<_> = opening
        .iter()
        .map(|surface| removal_spelling(surface, base, desired, &opening, &after))
        .collect();
    let expected: BTreeSet<_> = before.union(&after).cloned().collect();
    let actual: BTreeSet<_> = observations.iter().map(|o| o.surface.clone()).collect();
    if let Some(surface) = expected.symmetric_difference(&actual).next() {
        return Err(Error::Coverage(surface.clone()));
    }
    if actual.len() != observations.len() {
        return Err(Error::Coverage(observations[0].surface.clone()));
    }
    for o in observations {
        if o.current.is_some() != before.contains(&o.surface)
            || o.desired.is_some() != after.contains(&o.surface)
        {
            return Err(Error::Coverage(o.surface.clone()));
        }
    }
    Ok(())
}

/// The spelling under which a base surface is covered. One the plan keeps is
/// its own; one the plan removes is named as its removal names it once the
/// plan's renames have run, through the recorded identities, unless another
/// surface already holds that spelling: one the plan keeps (a rename that
/// keeps it), or another base surface, such as a dropped table's whose name
/// the renamed table takes. A removal named there is covered by that surface,
/// and two removals are never collapsed onto one spelling.
pub(super) fn removal_spelling(
    surface: &Surface,
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    before: &BTreeSet<Surface>,
    after: &BTreeSet<Surface>,
) -> Surface {
    if after.contains(surface) {
        return surface.clone();
    }
    let named = forward(surface, base.ids, desired.ids);
    if after.contains(&named) || (&named != surface && before.contains(&named)) {
        surface.clone()
    } else {
        named
    }
}

/// Replacing an input requires tearing down existing dependents even when
/// recreating them will bind the same logical name. Propagate that requirement
/// through managed surfaces before invoking the ordinary module restore pass.
pub(super) fn rebuilds(
    ordinary: &ChangeSet,
    observations: &[SurfaceResolution],
    rebuilds_modules: bool,
    before_ids: &IdsFile,
    after_ids: &IdsFile,
) -> BTreeSet<Surface> {
    let desired_for = |o: &SurfaceResolution| {
        let surface = forward(&o.surface, before_ids, after_ids);
        observations
            .iter()
            .find(|d| d.surface == surface)
            .and_then(|d| d.desired.as_ref())
    };
    let mut result: BTreeSet<_> = observations.iter().filter(|o| {
        matches!((&o.current, desired_for(o)), (Some(a), Some(b)) if a.bindings != b.bindings)
    }).map(|o| forward(&o.surface, before_ids, after_ids)).collect();
    loop {
        let before = result.len();
        for o in observations {
            if let (Some(current), Some(_)) = (&o.current, desired_for(o))
                && current.managed_inputs.iter().any(|input| {
                    result.contains(&forward(input, before_ids, after_ids))
                        || ordinary.changes.iter().any(|p| {
                            invalidates(&p.change, input)
                                || (rebuilds_modules && matches!((&p.change, input),
                                    (Change::AlterModule { id, .. }, Surface::Module(m)) if id == m))
                        })
                }) {
                result.insert(forward(&o.surface, before_ids, after_ids));
            }
        }
        if result.len() == before {
            return result;
        }
    }
}

/// Only recorded identities move a surface between names. A namespace/name
/// resemblance is never a rename decision. Reversing the maps gives teardown
/// its pre-rename address while creation retains the approved final address.
pub(super) fn forward(surface: &Surface, from: &IdsFile, to: &IdsFile) -> Surface {
    let column = |c: &pbps_model::ColumnRef| {
        from.column_uid(c)
            .and_then(|uid| to.columns.get(uid))
            .cloned()
            .unwrap_or_else(|| c.clone())
    };
    match surface {
        Surface::Table(t) => Surface::Table(from.resolved_in(t, to)),
        Surface::Column(c) => Surface::Column(column(c)),
        Surface::Default(c) => Surface::Default(column(c)),
        Surface::Check { table, name } => Surface::Check {
            table: from.resolved_in(table, to),
            name: name.clone(),
        },
        Surface::Index { table, name } => Surface::Index {
            table: from.resolved_in(table, to),
            name: name.clone(),
        },
        Surface::Namespace(_) | Surface::Module(_) => surface.clone(),
    }
}

pub(super) fn invalidates(change: &Change, surface: &Surface) -> bool {
    match surface {
        Surface::Namespace(_) => false,
        Surface::Module(id) => matches!(change, Change::DropModule { id: gone, .. } if gone == id),
        Surface::Table(table) => {
            matches!(change, Change::DropTable { name, .. } if name == table)
                || matches!(change, Change::RenameTable { from, .. } if from == table)
        }
        // Definition comparisons also name declaration-only metadata. It emits
        // no DDL, so dropping a bound view/routine would invent a dependency
        // failure on a plan that needs no catalog change (SPEC §7.6).
        Surface::Column(column) => change
            .columns_redefined()
            .iter()
            .any(|(r, field)| r == column && *field != pbps_model::ColumnField::Deprecated),
        // A rewritten generation expression replaces its `pg_attrdef` as a
        // changed default does (DEC-1168.1).
        Surface::Default(column) => {
            matches!(change, Change::AlterColumnDefault { column: r, from: Some(_), .. } if r == column)
                || matches!(change, Change::AlterColumnExpression { column: r, .. } if r == column)
                // A partition's: every one of these sets, replaces or drops
                // the partition's own `pg_attrdef`, an inherited copy of its
                // parent's included (#1607 review).
                || matches!(change, Change::SetPartitionDefault { table, column: c, .. }
                    if *table == column.table && *c == column.name)
        }
        Surface::Check { table, name } => {
            matches!(change, Change::DropCheck { table: t, name: n } if t == table && n == name)
        }
        Surface::Index { table, name } => {
            matches!(change, Change::DropIndex { table: t, name: n } if t == table && n == name)
        }
    }
}

#[allow(clippy::wildcard_enum_match_arm, clippy::too_many_arguments)]
pub(super) fn changes(
    ordinary: &ChangeSet,
    base: &Schema,
    desired: &Schema,
    ids: &IdsFile,
    before_ids: &IdsFile,
    rebuilds: &BTreeSet<Surface>,
    hints: &pbps_model::Hints,
    dialect: &dyn Dialect,
) -> Result<(ChangeSet, usize), Error> {
    let mut changes = Vec::new();
    for planned in &ordinary.changes {
        let expanded = match &planned.change {
            Change::CreateTable {
                uid,
                name,
                table,
                beside_default,
                moves_at_most,
            } => {
                let mut bare = (**table).clone();
                let checks = std::mem::take(&mut bare.checks);
                let indexes = std::mem::take(&mut bare.indexes);
                // An identity on one of those indexes waits for it: the
                // `CREATE` would set it before the index exists (#1444).
                let identity = matches!(
                    bare.replica_identity,
                    Some(pbps_model::ReplicaIdentity::Index(_))
                )
                .then(|| bare.replica_identity.take())
                .flatten();
                let mut defaults = Vec::new();
                for (column, spec) in &mut bare.columns {
                    let default = match &mut spec.value_source {
                        ValueSource::Default(default) => std::mem::take(default),
                        ValueSource::None
                        | ValueSource::Identity(_)
                        | ValueSource::Generated(_) => continue,
                    };
                    spec.value_source = ValueSource::None;
                    let reference = name.column(column);
                    defaults.push(Change::AlterColumnDefault {
                        uid: ids
                            .column_uid(&reference)
                            .ok_or_else(|| Error::Definition(Surface::Default(reference.clone())))?
                            .clone(),
                        column: reference,
                        from: None,
                        to: Some(default),
                    });
                }
                let mut parts = vec![Change::CreateTable {
                    uid: uid.clone(),
                    name: name.clone(),
                    table: Box::new(bare),
                    beside_default: beside_default.clone(),
                    moves_at_most: *moves_at_most,
                }];
                parts.extend(defaults);
                parts.extend(
                    checks
                        .into_iter()
                        .map(|(check, constraint)| Change::AddCheck {
                            table: name.clone(),
                            name: check,
                            constraint,
                        }),
                );
                parts.extend(indexes.into_iter().map(|(index, spec)| Change::AddIndex {
                    table: name.clone(),
                    clustered: table.index_is_clustered(&index),
                    name: index,
                    index: Box::new(spec),
                }));
                if identity.is_some() {
                    parts.push(Change::SetReplicaIdentity {
                        uid: uid.clone(),
                        table: name.clone(),
                        to: identity,
                    });
                }
                parts
            }
            Change::AlterModule { id, module } if dialect.rebuilds_modules() => vec![
                Change::DropModule {
                    id: id.clone(),
                    kind: base
                        .modules
                        .get(id)
                        .ok_or_else(|| Error::Definition(Surface::Module(id.clone())))?
                        .kind,
                },
                Change::CreateModule {
                    id: id.clone(),
                    module: module.clone(),
                },
            ],
            // ADD COLUMN DEFAULT fills existing rows. Splitting it would
            // change data, so its default must instead wait for its inputs.
            Change::AlterColumnDefault {
                uid,
                column,
                from: Some(from),
                to: Some(to),
            } => vec![
                Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: column.clone(),
                    from: Some(from.clone()),
                    to: None,
                },
                Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: column.clone(),
                    from: None,
                    to: Some(to.clone()),
                },
            ],
            _ => vec![planned.change.clone()],
        };
        for change in expanded {
            let mut p = planned.clone();
            if matches!(planned.change, Change::CreateTable { .. })
                && matches!(change, Change::AddIndex { .. })
            {
                // Ordinary CREATE TABLE builds indexes on its new empty table
                // without online/concurrent DDL. Splitting must keep that
                // transactional behavior (SPEC 9.3.2).
                p.strategy = Default::default();
            }
            p.risks = dialect.change_risks(&change);
            p.change = change;
            changes.push(p);
        }
    }
    // The rebuilds follow the ordinary plan. They spell a teardown by its
    // base name and a restoration by its final one, wherever they sit, so
    // the graph reads their tables by kind, not by position (DEC-1498.1).
    let mut added: Vec<PlannedChange> = Vec::new();
    for surface in rebuilds {
        let previous = forward(surface, ids, before_ids);
        let pair = match surface {
            Surface::Default(column)
                if desired
                    .tables
                    .get(&column.table)
                    .is_some_and(|t| t.partition_of.is_some())
                    || base
                        .tables
                        .get(&ids.resolved_in(&column.table, before_ids))
                        .is_some_and(|t| t.partition_of.is_some()) =>
            {
                match partition_default(surface, column, base, desired, ids, before_ids)? {
                    Some(pair) => pair,
                    None => continue,
                }
            }
            Surface::Default(column) => {
                let Surface::Default(before_column) = &previous else {
                    unreachable!("surface kind is preserved")
                };
                let generated = |schema: &Schema, at: &pbps_model::ColumnRef| {
                    schema
                        .tables
                        .get(&at.table)
                        .and_then(|t| t.columns.get(&at.name))
                        .and_then(|c| match &c.value_source {
                            ValueSource::Generated(value) => Some(value.clone()),
                            ValueSource::None
                            | ValueSource::Identity(_)
                            | ValueSource::Default(_) => None,
                        })
                };
                // A generation expression is rewritten in place with its own
                // text, keeping the column and its place in the column order,
                // so the engine binds it anew (DEC-1168.1). A server with no
                // in-place form refuses that change by name.
                if let (Some(old), Some(new)) =
                    (generated(base, before_column), generated(desired, column))
                {
                    let uid = ids
                        .column_uid(column)
                        .ok_or_else(|| Error::Definition(surface.clone()))?
                        .clone();
                    let rewrite = Change::AlterColumnExpression {
                        uid,
                        column: column.clone(),
                        from: old.expression,
                        to: new.expression,
                    };
                    if !changes.iter().chain(&added).any(|p| p.change == rewrite) {
                        let mut planned = PlannedChange::new(rewrite);
                        planned.risks = dialect.change_risks(&planned.change);
                        planned.strategy = hints
                            .strategies
                            .get(&column.table)
                            .copied()
                            .unwrap_or_default();
                        added.push(planned);
                    }
                    continue;
                }
                let uid = ids
                    .column_uid(column)
                    .ok_or_else(|| Error::Definition(surface.clone()))?
                    .clone();
                // A column the plan replaces is created with its default by
                // the ordinary plan, which binds it anew: there is no default
                // that stands through the plan to rebuild (#1610 review).
                if !before_ids.columns.contains_key(&uid) {
                    continue;
                }
                let old = base
                    .tables
                    .get(&before_column.table)
                    .and_then(|t| t.columns.get(&before_column.name))
                    .and_then(default_of);
                let new = desired
                    .tables
                    .get(&column.table)
                    .and_then(|t| t.columns.get(&column.name))
                    .and_then(default_of);
                let Some((old, new)) = both_defaults(surface, old, new)? else {
                    continue;
                };
                vec![
                    Change::AlterColumnDefault {
                        uid: uid.clone(),
                        column: before_column.clone(),
                        from: Some(old),
                        to: None,
                    },
                    Change::AlterColumnDefault {
                        uid,
                        column: column.clone(),
                        from: None,
                        to: Some(new),
                    },
                ]
            }
            Surface::Check { table, name } => {
                let Surface::Check {
                    table: before_table,
                    ..
                } = &previous
                else {
                    unreachable!("surface kind is preserved")
                };
                let constraint = desired
                    .tables
                    .get(table)
                    .and_then(|t| t.checks.get(name))
                    .ok_or_else(|| Error::Definition(surface.clone()))?
                    .clone();
                vec![
                    Change::DropCheck {
                        table: before_table.clone(),
                        name: name.clone(),
                    },
                    Change::AddCheck {
                        table: table.clone(),
                        name: name.clone(),
                        constraint,
                    },
                ]
            }
            Surface::Index { table, name } => {
                let Surface::Index {
                    table: before_table,
                    ..
                } = &previous
                else {
                    unreachable!("surface kind is preserved")
                };
                let definition = desired
                    .tables
                    .get(table)
                    .ok_or_else(|| Error::Definition(surface.clone()))?;
                let index = definition
                    .indexes
                    .get(name)
                    .ok_or_else(|| Error::Definition(surface.clone()))?
                    .clone();
                vec![
                    Change::DropIndex {
                        table: before_table.clone(),
                        name: name.clone(),
                    },
                    Change::AddIndex {
                        table: table.clone(),
                        name: name.clone(),
                        index: Box::new(index),
                        clustered: definition.index_is_clustered(name),
                    },
                ]
            }
            Surface::Module(_) => continue, // Already handled with all restore obligations by diff_rebuilding.
            Surface::Namespace(_) | Surface::Table(_) | Surface::Column(_) => {
                return Err(Error::Unsupported(surface.clone()));
            }
        };
        if previous != *surface {
            // The ordinary differ names a changed expression after the rename.
            // A binding rebuild needs its old input gone before that rename;
            // replace that teardown instead of retaining both spellings.
            changes.retain(|p| !tears_down(&p.change, surface));
        }
        for change in pair {
            if !changes.iter().chain(&added).any(|p| p.change == change) {
                let mut planned = PlannedChange::new(change);
                planned.risks = dialect.change_risks(&planned.change);
                if let Some(table) = planned.change.table() {
                    // Strategy is declared at the final owner name, including
                    // a teardown that must execute before its owner's rename.
                    let owner = match surface {
                        Surface::Default(c) => &c.table,
                        Surface::Check { table, .. } | Surface::Index { table, .. } => table,
                        _ => table,
                    };
                    planned.strategy = hints.strategies.get(owner).copied().unwrap_or_default();
                }
                added.push(planned);
            }
        }
    }
    // A parent's default the rebuild sets again is recursed by the engine
    // into every partition, over a partition's own default (DEC-1687.1). The
    // differ sets a partition's own again after a parent's default it
    // changes; a rebuild changes no text, so a partition's own that nothing
    // else in the plan sets is set again here, after the parent's (#1610
    // review).
    let mut again = Vec::new();
    for planned in &added {
        let Change::AlterColumnDefault {
            column,
            to: Some(fallback),
            ..
        } = &planned.change
        else {
            continue;
        };
        for (partition, table) in &desired.tables {
            let Some(own) = table
                .partition_of
                .as_ref()
                .filter(|of| of.parent == column.table)
                .and_then(|of| of.columns.get(&column.name))
                .and_then(|own| own.default.clone())
            else {
                continue;
            };
            let set = |c: &Change| {
                matches!(c, Change::SetPartitionDefault { table, column: name, to: Some(_), .. }
                    if table == partition && *name == column.name)
            };
            if changes.iter().chain(&added).any(|p| set(&p.change)) {
                continue;
            }
            let uid = ids
                .table_uid(partition)
                .ok_or_else(|| Error::Definition(Surface::Default(partition.column(&column.name))))?
                .clone();
            let mut planned = PlannedChange::new(Change::SetPartitionDefault {
                uid,
                table: partition.clone(),
                parent: column.table.clone(),
                column: column.name.clone(),
                from: Some(own.clone()),
                to: Some(own),
                fallback: Some(fallback.clone()),
            });
            planned.risks = dialect.change_risks(&planned.change);
            planned.strategy = hints.strategies.get(partition).copied().unwrap_or_default();
            again.push(planned);
        }
    }
    added.extend(again);
    let ordinary = changes.len();
    changes.extend(added);
    Ok((ChangeSet { changes }, ordinary))
}

/// A column's default, where its value comes from one: not an identity's
/// or a generation expression's, which are not defaults to rebuild.
fn default_of(column: &pbps_model::Column) -> Option<String> {
    match &column.value_source {
        ValueSource::Default(value) => Some(value.clone()),
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_) => None,
    }
}

/// Both sides' defaults of a rebuilt surface, or `None` when only one side
/// has one: the ordinary plan adds or removes that default itself, through
/// the change, or a column, table or partition it replaces, attaches or
/// detaches, so there is no default bound on both sides to rebuild. A
/// surface with no default on either side is a definition the rebuild cannot
/// spell, and is refused by name rather than skipped (#1610).
fn both_defaults(
    surface: &Surface,
    old: Option<String>,
    new: Option<String>,
) -> Result<Option<(String, String)>, Error> {
    match (old, new) {
        (Some(old), Some(new)) => Ok(Some((old, new))),
        (Some(_), None) | (None, Some(_)) => Ok(None),
        (None, None) => Err(Error::Definition(surface.clone())),
    }
}

/// The release and restoration of a partition's own default, the pair the
/// ordinary path builds for a module rebuild (DEC-1581.1). A partition
/// declares no column, so its default lives in `partition_of.columns`
/// (#1578) and the column is keyed by the partition's table UID; the name it
/// had before the plan is its parent column's, found by that column's UID
/// (#1610). A table the plan attaches or detaches keeps its default across
/// the change (#1578), as a column's default on one side and the
/// partition's own on the other: each half of the pair is spelled in the
/// form its side holds. `None` when only one side has a default, as
/// [`both_defaults`] says, or when the plan replaces the partition.
fn partition_default(
    surface: &Surface,
    column: &pbps_model::ColumnRef,
    base: &Schema,
    desired: &Schema,
    ids: &IdsFile,
    before_ids: &IdsFile,
) -> Result<Option<Vec<Change>>, Error> {
    let definition = || Error::Definition(surface.clone());
    let uid = ids.table_uid(&column.table).ok_or_else(definition)?.clone();
    // A partition the plan replaces is created with its own default by the
    // ordinary plan, which binds it anew: there is no default that stands
    // through the plan to rebuild (#1610 review).
    let Some(before_table) = before_ids.tables.get(&uid).cloned() else {
        return Ok(None);
    };
    let of = desired
        .tables
        .get(&column.table)
        .and_then(|t| t.partition_of.as_ref());
    let before_of = base
        .tables
        .get(&before_table)
        .and_then(|t| t.partition_of.as_ref());
    // A partition declares no column: its column's name before the plan is
    // its parent column's. A table the plan attaches or detaches has the
    // column as its own, under the name it keeps.
    let before_name = of
        .filter(|_| before_of.is_some())
        .and_then(|of| ids.column_uid(&of.parent.column(&column.name)))
        .and_then(|parent| before_ids.columns.get(parent))
        .map_or_else(|| column.name.clone(), |c| c.name.clone());
    // An attach keeps the table's column default as the partition's own
    // (#1578), as a detach keeps the override as the column's: each side's
    // default is read in the form that side holds it.
    let old = match before_of {
        Some(of) => of
            .columns
            .get(&before_name)
            .and_then(|own| own.default.clone()),
        None => base
            .tables
            .get(&before_table)
            .and_then(|t| t.columns.get(&before_name))
            .and_then(default_of),
    };
    // A detach keeps the override as the table's column default (#1578).
    let new = match of {
        Some(of) => of
            .columns
            .get(&column.name)
            .and_then(|own| own.default.clone()),
        None => desired
            .tables
            .get(&column.table)
            .and_then(|t| t.columns.get(&column.name))
            .and_then(default_of),
    };
    let Some((old, new)) = both_defaults(surface, old, new)? else {
        return Ok(None);
    };
    let release = match before_of {
        Some(before_of) => Change::SetPartitionDefault {
            uid: uid.clone(),
            table: before_table,
            parent: before_of.parent.clone(),
            column: before_name,
            from: Some(old),
            to: None,
            fallback: None,
        },
        None => {
            let before_column = before_table.column(&before_name);
            Change::AlterColumnDefault {
                uid: before_ids
                    .column_uid(&before_column)
                    .ok_or_else(definition)?
                    .clone(),
                column: before_column,
                from: Some(old),
                to: None,
            }
        }
    };
    let restore = match of {
        Some(of) => Change::SetPartitionDefault {
            uid,
            table: column.table.clone(),
            parent: of.parent.clone(),
            column: column.name.clone(),
            from: None,
            to: Some(new),
            fallback: desired
                .tables
                .get(&of.parent)
                .and_then(|parent| parent.columns.get(&column.name))
                .and_then(default_of),
        },
        None => Change::AlterColumnDefault {
            uid: ids.column_uid(column).ok_or_else(definition)?.clone(),
            column: column.clone(),
            from: None,
            to: Some(new),
        },
    };
    Ok(Some(vec![release, restore]))
}

fn tears_down(change: &Change, surface: &Surface) -> bool {
    matches!((change, surface),
        (Change::AlterColumnDefault { column: a, to: None, .. }, Surface::Default(b)) if a == b)
        || matches!((change, surface),
        (Change::SetPartitionDefault { table, column, to: None, fallback: None, .. }, Surface::Default(b))
            if *table == b.table && *column == b.name)
        || matches!((change, surface),
        (Change::DropCheck { table: a, name: n }, Surface::Check { table: b, name: m })
        | (Change::DropIndex { table: a, name: n }, Surface::Index { table: b, name: m }) if a == b && n == m)
}
