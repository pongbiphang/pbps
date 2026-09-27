use super::Error;
use pbps_dialect::Dialect;
use pbps_model::resolver::{Surface, SurfaceResolution};
use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, Schema};
use std::collections::BTreeSet;

fn surfaces(schema: &Schema) -> BTreeSet<Surface> {
    let mut result = BTreeSet::new();
    for (table, definition) in &schema.tables {
        for (column, spec) in &definition.columns {
            if spec.default.is_some() {
                result.insert(Surface::Default(table.column(column)));
            }
        }
        for name in definition.checks.keys() {
            result.insert(Surface::Check {
                table: table.clone(),
                name: name.clone(),
            });
        }
        for (name, index) in &definition.indexes {
            if index.filter.is_some() {
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
    base: &Schema,
    desired: &Schema,
    observations: &[SurfaceResolution],
) -> Result<(), Error> {
    let before = surfaces(base);
    let after = surfaces(desired);
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
        Surface::Column(column) => change.columns_redefined().iter().any(|(r, _)| r == column),
        Surface::Default(column) => {
            matches!(change, Change::AlterColumnDefault { column: r, from: Some(_), .. } if r == column)
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
) -> Result<ChangeSet, Error> {
    let mut changes = Vec::new();
    for planned in &ordinary.changes {
        let expanded = match &planned.change {
            Change::CreateTable { uid, name, table } => {
                let mut bare = (**table).clone();
                let checks = std::mem::take(&mut bare.checks);
                let indexes = std::mem::take(&mut bare.indexes);
                let mut defaults = Vec::new();
                for (column, spec) in &mut bare.columns {
                    if let Some(default) = spec.default.take() {
                        let reference = name.column(column);
                        defaults.push(Change::AlterColumnDefault {
                            uid: ids
                                .column_uid(&reference)
                                .ok_or_else(|| {
                                    Error::Definition(Surface::Default(reference.clone()))
                                })?
                                .clone(),
                            column: reference,
                            from: None,
                            to: Some(default),
                        });
                    }
                }
                let mut parts = vec![Change::CreateTable {
                    uid: uid.clone(),
                    name: name.clone(),
                    table: Box::new(bare),
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
                    name: index,
                    index: Box::new(spec),
                }));
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
            p.risks = dialect.change_risks(&change);
            p.change = change;
            changes.push(p);
        }
    }
    for surface in rebuilds {
        let previous = forward(surface, ids, before_ids);
        let pair = match surface {
            Surface::Default(column) => {
                let Surface::Default(before_column) = &previous else {
                    unreachable!("surface kind is preserved")
                };
                let Some(old) = base
                    .tables
                    .get(&before_column.table)
                    .and_then(|t| t.columns.get(&before_column.name))
                    .and_then(|c| c.default.clone())
                else {
                    continue;
                };
                let new = desired
                    .tables
                    .get(&column.table)
                    .and_then(|t| t.columns.get(&column.name))
                    .and_then(|c| c.default.clone())
                    .ok_or_else(|| Error::Definition(surface.clone()))?;
                let uid = ids
                    .column_uid(column)
                    .ok_or_else(|| Error::Definition(surface.clone()))?
                    .clone();
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
                let index = desired
                    .tables
                    .get(table)
                    .and_then(|t| t.indexes.get(name))
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
            if !changes.iter().any(|p| p.change == change) {
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
                changes.push(planned);
            }
        }
    }
    Ok(ChangeSet { changes })
}

fn tears_down(change: &Change, surface: &Surface) -> bool {
    matches!((change, surface),
        (Change::AlterColumnDefault { column: a, to: None, .. }, Surface::Default(b)) if a == b)
        || matches!((change, surface),
        (Change::DropCheck { table: a, name: n }, Surface::Check { table: b, name: m })
        | (Change::DropIndex { table: a, name: n }, Surface::Index { table: b, name: m }) if a == b && n == m)
}
