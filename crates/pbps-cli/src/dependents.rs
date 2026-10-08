//! What depends on a module a PostgreSQL plan drops, and where the plan
//! accounts for it (#314, ADR-0009 §4, DEC-314.1).
//!
//! Every module change on this engine is a `DROP` and a `CREATE` (ADR-0009 §3),
//! and the engine refuses the `DROP` while anything depends on the module: a
//! view over a view, a check constraint or a column default calling a
//! function, an index using one. `modules::dependents` has enumerated those
//! from `pg_depend` since ADR-0009 §4 was written, and nothing called it — so a
//! saved plan could omit an unchanged dependent and reach an engine-refused
//! `DROP` at apply time, which is the applyable-and-predictably-fails outcome
//! SPEC §7.5 exists to prevent.
//!
//! The planner's half is [`weave`]: every dependent the plan does not already
//! remove before the module's drop is removed there, and every dependent the
//! declarations keep is put back after the module's create, in the plan where
//! the approver sees it. The apply's half is [`unaccounted`], which only asks
//! whether the saved plan still removes everything that depends on what it
//! drops — a dependent created after planning is one the approver never saw,
//! and the plan is refused rather than extended.
//!
//! Pure over the dependents already read, so the ordering is testable without
//! an engine; the reads are `engine::module_dependents`'.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, ColumnRef, IdsFile, ModuleDeps, ModuleId, ModuleKind, PlannedChange, Schema,
    TableName,
};
use pbps_pg::modules::{Dependent, Holds, Part};

/// Every module this plan drops, rebuilt or not, in plan order: an
/// `AlterModule` (emitted as a drop and a create in one step) and a
/// `DropModule` (alone, or paired with a `CreateModule` of the same id).
///
/// A plain drop is here too, unlike `engine::rebuilt_modules`: a rebuild is
/// what `before_a_rebuild` is about, but a dependent refuses the `DROP`
/// whether or not a `CREATE` follows it.
// The complement is every change that does not drop a module.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn dropped_modules(changes: &ChangeSet) -> Vec<(ModuleId, ModuleKind)> {
    let mut out: Vec<(ModuleId, ModuleKind)> = Vec::new();
    for p in &changes.changes {
        let found = match &p.change {
            Change::AlterModule { id, module } => Some((id.clone(), module.kind)),
            Change::DropModule { id, kind } => Some((id.clone(), *kind)),
            _ => None,
        };
        if let Some(found) = found
            && !out.iter().any(|(id, _)| *id == found.0)
        {
            out.push(found);
        }
    }
    out
}

/// The declared modules among the dependents that this plan does not touch:
/// the ones the plan has to rebuild around the module under them, and has to
/// ask the differ to, so their grants and `PUBLIC` execute are restated with
/// them (`pbps_diff::diff_rebuilding`).
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn untouched_module_dependents(
    changes: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
) -> std::collections::BTreeSet<ModuleId> {
    let touched = |x: &ModuleId| {
        changes.changes.iter().any(|p| match &p.change {
            Change::AlterModule { id, .. }
            | Change::DropModule { id, .. }
            | Change::CreateModule { id, .. } => id == x,
            _ => false,
        })
    };
    // A root the plan drops and never creates again is gone for good. A
    // declared module still depending on it cannot be rebuilt around it: the
    // rebuild would create it against a module that no longer exists, and
    // the plan would fail at apply. Left out here, it reaches `weave`'s
    // refusal of a kept dependent of a module dropped for good, by name
    // (DEC-314.1, #947).
    let recreated = |root: &ModuleId| {
        changes.changes.iter().any(|p| match &p.change {
            Change::AlterModule { id, .. } | Change::CreateModule { id, .. } => id == root,
            _ => false,
        })
    };
    // Left out wherever else it is found: a view on a dropped function and
    // on a rebuilt one is still a view on the dropped function, and rebuilding
    // it for the second would hand `weave` a create to take as its
    // restoration from the first.
    //
    // Unless the plan cuts its path to the dropped root. `dependents` is
    // transitive, so a view `v2` over an edited `v1` over `f` is listed under
    // `f` even when the edit makes `v1` stop calling `f`. That `v2` is on `f`
    // only through `v1`, it is listed under `v1` too (a module the plan
    // drops), and it is rebuilt around `v1` as any view over an edited view
    // is (#1069 review). A module listed under both `f` and such a `v1` that
    // also calls `f` directly cannot be told apart without parsing; it is
    // rebuilt, and its `CREATE` fails at apply, which is the loud outcome,
    // where refusing would turn the valid plan away.
    let under = |root: &ModuleId, x: &ModuleId| {
        found.get(root).is_some_and(|deps| {
            deps.iter()
                .any(|d| matches!(&d.holds, Holds::Module(y) if y == x))
        })
    };
    let on_a_dropped_root: std::collections::BTreeSet<&ModuleId> = found
        .iter()
        .filter(|(root, _)| !recreated(root))
        .flat_map(|(root, deps)| {
            deps.iter().filter_map(move |d| match &d.holds {
                Holds::Module(x) => Some((root, x)),
                Holds::TablePart { .. } | Holds::Unrepresentable(_) => None,
            })
        })
        .filter(|(root, x)| {
            !found
                .get(*root)
                .into_iter()
                .flatten()
                .any(|m| match &m.holds {
                    Holds::Module(m) => m != *x && touched(m) && under(m, x),
                    Holds::TablePart { .. } | Holds::Unrepresentable(_) => false,
                })
        })
        .map(|(_, x)| x)
        .collect();
    found
        .values()
        .flatten()
        .filter_map(|d| match &d.holds {
            Holds::Module(x)
                if declared.modules.contains_key(x)
                    && !touched(x)
                    && !on_a_dropped_root.contains(x) =>
            {
                Some(x.clone())
            }
            Holds::Module(_) | Holds::TablePart { .. } | Holds::Unrepresentable(_) => None,
        })
        .collect()
}

/// Where a dropped module leaves the plan and where it comes back.
struct Span {
    drop_at: usize,
    create_at: Option<usize>,
}

#[allow(clippy::wildcard_enum_match_arm)]
fn span(changes: &[PlannedChange], root: &ModuleId) -> Option<Span> {
    let mut drop_at = None;
    let mut create_at = None;
    for (i, p) in changes.iter().enumerate() {
        match &p.change {
            Change::AlterModule { id, .. } if id == root => {
                drop_at = Some(i);
                create_at = Some(i);
            }
            Change::DropModule { id, .. } if id == root => drop_at = Some(i),
            Change::CreateModule { id, .. } if id == root => create_at = Some(i),
            _ => {}
        }
    }
    let drop_at = drop_at?;
    Some(Span {
        drop_at,
        create_at: create_at.filter(|c| *c >= drop_at),
    })
}

/// Whether `change` removes exactly this dependent.
#[allow(clippy::wildcard_enum_match_arm)]
fn removes(change: &Change, holds: &Holds) -> bool {
    match (holds, change) {
        (Holds::Module(x), Change::DropModule { id, .. }) => id == x,
        (Holds::TablePart { table, part }, change) => match (part, change) {
            (Part::Check(n), Change::DropCheck { table: t, name }) => t == table && name == n,
            (Part::Index(n), Change::DropIndex { table: t, name }) => t == table && name == n,
            (
                Part::Default(c),
                Change::AlterColumnDefault {
                    column, to: None, ..
                },
            ) => column.table == *table && column.name == *c,
            // A partition's default goes only when the partition takes
            // neither its own nor its parent's (#1588).
            (
                Part::Default(c),
                Change::SetPartitionDefault {
                    table: t,
                    column,
                    to: None,
                    fallback: None,
                    ..
                },
            ) => t == table && column == c,
            // A changed expression takes the old one's dependency away
            // (DEC-1168.1). If the new one calls the module again, its
            // `SET EXPRESSION` binds the old module and the drop is refused
            // inside the transaction: the loud outcome, where telling the two
            // apart would mean parsing the expression (DECISIONS 174).
            (Part::Generated(c), Change::AlterColumnExpression { column, .. }) => {
                column.table == *table && column.name == *c
            }
            _ => false,
        },
        _ => false,
    }
}

/// Whether `change` creates exactly this dependent.
#[allow(clippy::wildcard_enum_match_arm)]
fn restores(change: &Change, holds: &Holds) -> bool {
    match (holds, change) {
        (Holds::Module(x), Change::CreateModule { id, .. }) => id == x,
        (Holds::TablePart { table, part }, change) => match (part, change) {
            (Part::Check(n), Change::AddCheck { table: t, name, .. }) => t == table && name == n,
            (Part::Index(n), Change::AddIndex { table: t, name, .. }) => t == table && name == n,
            (
                Part::Default(c),
                Change::AlterColumnDefault {
                    column,
                    from: None,
                    to: Some(_),
                    ..
                },
            ) => column.table == *table && column.name == *c,
            (
                Part::Default(c),
                Change::SetPartitionDefault {
                    table: t,
                    column,
                    from: None,
                    to,
                    fallback,
                    ..
                },
            ) => t == table && column == c && (to.is_some() || fallback.is_some()),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `change` takes the dependent away with something larger: its
/// table, or the column a default belongs to. The plan still needs the exact
/// removal before the module's drop, but a dependent it takes away this way
/// is one the declarations let go of, not one they fail to hold.
#[allow(clippy::wildcard_enum_match_arm)]
fn removes_with_its_owner(change: &Change, holds: &Holds) -> bool {
    match (holds, change) {
        (Holds::TablePart { table, .. }, Change::DropTable { name, .. }) => name == table,
        (
            Holds::TablePart {
                table,
                part: Part::Default(c) | Part::Generated(c),
            },
            Change::DropColumn { column, .. },
        ) => column.table == *table && column.name == *c,
        _ => false,
    }
}

/// Whether `change` edits this dependent in one step that drops and creates
/// it: a view's `AlterModule`, or a default changed from one expression to
/// another.
#[allow(clippy::wildcard_enum_match_arm)]
fn edits_in_place(change: &Change, holds: &Holds) -> bool {
    match (holds, change) {
        (Holds::Module(x), Change::AlterModule { id, .. }) => id == x,
        (
            Holds::TablePart {
                table,
                part: Part::Default(c),
            },
            Change::AlterColumnDefault {
                column,
                from: Some(_),
                to: Some(_),
                ..
            },
        ) => column.table == *table && column.name == *c,
        // A partition's own default replaced by another, or by its parent's.
        (
            Holds::TablePart {
                table,
                part: Part::Default(c),
            },
            Change::SetPartitionDefault {
                table: t,
                column,
                from: Some(_),
                to,
                fallback,
                ..
            },
        ) => t == table && column == c && (to.is_some() || fallback.is_some()),
        _ => false,
    }
}

/// Replaces one change that edits this dependent in place with its removal
/// and its restoration, side by side, so each half can be put on its own side
/// of the module: a view's `AlterModule` is one step that drops and creates,
/// and a default changed from one expression to another is one statement.
#[allow(clippy::wildcard_enum_match_arm)]
fn split_in_place_edit(changes: &mut Vec<PlannedChange>, holds: &Holds, dialect: &dyn Dialect) {
    let Some(i) = find(changes, holds, edits_in_place) else {
        return;
    };
    let (removal, restoration) = match changes[i].change.clone() {
        Change::AlterModule { id, module } => (
            Change::DropModule {
                id: id.clone(),
                kind: module.kind,
            },
            Change::CreateModule { id, module },
        ),
        Change::AlterColumnDefault {
            uid,
            column,
            from,
            to,
        } => (
            Change::AlterColumnDefault {
                uid: uid.clone(),
                column: column.clone(),
                from,
                to: None,
            },
            Change::AlterColumnDefault {
                uid,
                column,
                from: None,
                to,
            },
        ),
        Change::SetPartitionDefault {
            uid,
            table,
            parent,
            column,
            from,
            to,
            fallback,
        } => (
            Change::SetPartitionDefault {
                uid: uid.clone(),
                table: table.clone(),
                parent: parent.clone(),
                column: column.clone(),
                from,
                to: None,
                fallback: None,
            },
            Change::SetPartitionDefault {
                uid,
                table,
                parent,
                column,
                from: None,
                to,
                fallback,
            },
        ),
        _ => return,
    };
    changes.splice(
        i..=i,
        [planned(removal, dialect), planned(restoration, dialect)],
    );
}

fn planned(change: Change, dialect: &dyn Dialect) -> PlannedChange {
    let mut p = PlannedChange::new(change);
    p.risks = dialect.change_risks(&p.change);
    p
}

/// The dependent as the plan names it just before change `i`.
///
/// The catalog names it as the database does now, and a plan that renames its
/// table or column says the new name from the rename on: the differ sorts
/// renames after a `DropModule` and before an `AlterModule`, so one dependent
/// has two names in one plan, and each change must be matched, and each
/// synthesized one written, with the name that holds at its own position.
#[allow(clippy::wildcard_enum_match_arm)]
fn named_at(changes: &[PlannedChange], i: usize, holds: &Holds) -> Holds {
    let Holds::TablePart { table, part } = holds else {
        return holds.clone();
    };
    let (mut table, mut part) = (table.clone(), part.clone());
    for p in &changes[..i] {
        match &p.change {
            Change::RenameTable { from, to, .. } if *from == table => table = to.clone(),
            Change::RenameColumn {
                table: t, from, to, ..
            } if *t == table => {
                if let Part::Default(c) | Part::Generated(c) = &mut part
                    && c == from
                {
                    *c = to.clone();
                }
            }
            _ => {}
        }
    }
    Holds::TablePart { table, part }
}

/// Rewrites a table part's change to name its table and column as `holds`
/// does: the name that holds where the change is being moved to. A removal
/// the differ put after a rename, moved before the module's drop, may land
/// before that rename, where the table still has its old name.
#[allow(clippy::wildcard_enum_match_arm)]
fn respell(p: &mut PlannedChange, holds: &Holds, dialect: &dyn Dialect) {
    let Holds::TablePart { table, part } = holds else {
        return;
    };
    match (&mut p.change, part) {
        (
            Change::DropCheck { table: t, .. }
            | Change::AddCheck { table: t, .. }
            | Change::DropIndex { table: t, .. }
            | Change::AddIndex { table: t, .. },
            _,
        ) => *t = table.clone(),
        (Change::AlterColumnDefault { column, .. }, Part::Default(c)) => {
            *column = ColumnRef::new(table.clone(), c.clone());
        }
        _ => return,
    }
    p.risks = dialect.change_risks(&p.change);
}

/// The first change matching `test` against the dependent's name at that
/// change's own position.
fn find(
    changes: &[PlannedChange],
    holds: &Holds,
    test: fn(&Change, &Holds) -> bool,
) -> Option<usize> {
    (0..changes.len()).find(|&i| test(&changes[i].change, &named_at(changes, i, holds)))
}

/// The dependent under the name the declarations use: after every rename in
/// the plan.
fn as_declared(changes: &[PlannedChange], d: &Dependent) -> Dependent {
    Dependent {
        described: d.described.clone(),
        holds: named_at(changes, changes.len(), &d.holds),
    }
}

/// The partition `table` is, as declared.
fn partition_of<'a>(
    declared: &'a Schema,
    table: &TableName,
) -> Option<&'a pbps_model::PartitionOf> {
    declared.tables.get(table)?.partition_of.as_ref()
}

/// The declared default of the column a partition's `column` is, on its
/// parent: what the engine copies into the partition and what it takes back
/// without one of its own.
fn parent_default(declared: &Schema, table: &TableName, column: &str) -> Option<String> {
    let of = partition_of(declared, table)?;
    declared
        .tables
        .get(&of.parent)?
        .columns
        .get(column)?
        .default
        .clone()
}

/// The change that removes a dependent the plan does not remove yet.
fn removal_of(d: &Dependent, declared: &Schema, ids: &[&IdsFile]) -> Result<Change, String> {
    match &d.holds {
        Holds::Module(x) => {
            // Only a declared module reaches here: an undeclared one the plan
            // does not remove was refused above, and one it does remove is
            // moved rather than synthesized.
            let kind = declared
                .modules
                .get(x)
                .map(|m| m.kind)
                .ok_or_else(|| format!("{}: its kind is not declared", d.described))?;
            Ok(Change::DropModule {
                id: x.clone(),
                kind,
            })
        }
        Holds::TablePart { table, part } => Ok(match part {
            Part::Check(name) => Change::DropCheck {
                table: table.clone(),
                name: name.clone(),
            },
            Part::Index(name) => Change::DropIndex {
                table: table.clone(),
                name: name.clone(),
            },
            // A partition's, its own or its copy of its parent's: taken off
            // the partition alone (#1588).
            Part::Default(column) if let Some(of) = partition_of(declared, table) => {
                Change::SetPartitionDefault {
                    uid: ids
                        .iter()
                        .find_map(|ids| ids.table_uid(table))
                        .cloned()
                        .ok_or_else(|| format!("{}: no identity names `{table}`", d.described))?,
                    table: table.clone(),
                    parent: of.parent.clone(),
                    column: column.clone(),
                    from: of.columns.get(column).and_then(|own| own.default.clone()),
                    to: None,
                    fallback: None,
                }
            }
            Part::Default(column) => {
                let column = ColumnRef::new(table.clone(), column.clone());
                let uid = ids
                    .iter()
                    .find_map(|ids| ids.column_uid(&column))
                    .cloned()
                    .ok_or_else(|| {
                        format!("{}: no identity names column `{column}`", d.described)
                    })?;
                Change::AlterColumnDefault {
                    uid,
                    from: declared
                        .tables
                        .get(table)
                        .and_then(|t| t.columns.get(&column.name))
                        .and_then(|c| c.default.clone()),
                    column,
                    to: None,
                }
            }
            // No statement takes a generation expression off; only the
            // plan's own expression change can, and `weave` looks for that.
            Part::Generated(_) => {
                return Err(format!(
                    "{}: a generated column's expression cannot be taken off around the module",
                    d.described
                ));
            }
        }),
        Holds::Unrepresentable(why) => Err(format!("{} — {why}", d.described)),
    }
}

/// The change that puts a declared dependent back.
fn restoration_of(d: &Dependent, declared: &Schema, ids: &[&IdsFile]) -> Option<Change> {
    match &d.holds {
        Holds::Module(x) => declared.modules.get(x).map(|m| Change::CreateModule {
            id: x.clone(),
            module: Box::new(m.clone()),
        }),
        Holds::TablePart { table, part } => {
            let t = declared.tables.get(table)?;
            match part {
                Part::Check(name) => t.checks.get(name).map(|c| Change::AddCheck {
                    table: table.clone(),
                    name: name.clone(),
                    constraint: c.clone(),
                }),
                Part::Index(name) => t.indexes.get(name).map(|i| Change::AddIndex {
                    table: table.clone(),
                    name: name.clone(),
                    index: Box::new(i.clone()),
                    clustered: false,
                }),
                Part::Generated(_) => None,
                // Its own, or its parent's again (#1588).
                Part::Default(column) if t.partition_of.is_some() => {
                    let to = partition_of(declared, table)?
                        .columns
                        .get(column)
                        .and_then(|own| own.default.clone());
                    let fallback = parent_default(declared, table, column);
                    if to.is_none() && fallback.is_none() {
                        return None;
                    }
                    Some(Change::SetPartitionDefault {
                        uid: ids.iter().find_map(|ids| ids.table_uid(table))?.clone(),
                        table: table.clone(),
                        parent: partition_of(declared, table)?.parent.clone(),
                        column: column.clone(),
                        from: None,
                        to,
                        fallback,
                    })
                }
                Part::Default(column) => {
                    let to = t.columns.get(column)?.default.clone()?;
                    let column = ColumnRef::new(table.clone(), column.clone());
                    let uid = ids.iter().find_map(|ids| ids.column_uid(&column))?.clone();
                    Some(Change::AlterColumnDefault {
                        uid,
                        column,
                        from: None,
                        to: Some(to),
                    })
                }
            }
        }
        Holds::Unrepresentable(_) => None,
    }
}

/// A check a table this plan attaches holds as its parent's. The parent's
/// own, which the declarations hold and which depends on the same module, is
/// removed and restored around the rebuild, and both reach the partition as
/// they recurse, so this one is never put back. Measured on 16 and 18, once
/// attached it is the parent's inherited copy, which the engine refuses to
/// drop on its own and the parent's drop takes with it; only before the
/// attach is it the table's to remove (#1642 review).
fn its_parents_once_attached(changes: &[PlannedChange], holds: &Holds) -> bool {
    attached_at(changes, holds).is_some()
}

/// Whether a check of a table this plan attaches goes with its parent's
/// before `drop_at`: attached first, and the parent's check of its name
/// dropped after the attach and before `drop_at`.
fn taken_by_its_parent(changes: &[PlannedChange], holds: &Holds, drop_at: usize) -> bool {
    let (
        Some(attach),
        Holds::TablePart {
            part: Part::Check(name),
            ..
        },
    ) = (attached_at(changes, holds), holds)
    else {
        return false;
    };
    // Attached after the drop, the copy is still the table's own when the
    // module goes, and nothing of the parent's takes it.
    if attach >= drop_at {
        return false;
    }
    let Change::AttachPartition { parent, .. } = &changes[attach].change else {
        return false;
    };
    changes[attach..drop_at].iter().any(|p| {
        matches!(&p.change, Change::DropCheck { table, name: n } if table == parent && n == name)
    })
}

/// Where this plan attaches the table holding `holds`, a check its
/// partition declaration does not keep as its own. One it keeps stays the
/// table's across the attach, is no copy of the parent's, and is woven around
/// the rebuild as any declared check (#1642 review).
fn attached_at(changes: &[PlannedChange], holds: &Holds) -> Option<usize> {
    let Holds::TablePart {
        table,
        part: Part::Check(name),
    } = holds
    else {
        return None;
    };
    changes.iter().position(|p| {
        matches!(&p.change, Change::AttachPartition { table: t, shape, .. }
            if t == table && !shape.checks.contains_key(name))
    })
}

/// Puts every dependent of every module this plan drops on the right side of
/// that drop: removed before it, and — when the declarations keep it —
/// restored after the module's create. Returns how many changes it added.
///
/// A change the plan already has is moved rather than duplicated: a view the
/// declarations drop is dropped before the function under it, not after; a
/// view the declarations edit is split into its drop and its create, one on
/// each side. What the plan does not have is synthesized from the
/// declarations, which is the only place pbps can put an object back from.
///
/// Refused, with every name, when a dependent cannot be accounted for: one
/// the model cannot represent, one this project does not declare and the plan
/// does not remove, and one the declarations keep on a module this plan
/// drops for good (`DROP … CASCADE` is not offered, SPEC 14.3).
///
/// `found` is each dropped module's dependents in `modules::dependents`'
/// order, deepest first; the removals go in that order and the restorations
/// in its reverse, which is DECISIONS 311's topological order.
pub(crate) fn weave(
    cs: &mut ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
    ids: &[&IdsFile],
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let roots = dropped_modules(cs);
    let mut refused: Vec<String> = Vec::new();
    for (root, _) in &roots {
        let Some(deps) = found.get(root) else {
            continue;
        };
        let blocked: Vec<Dependent> = deps
            .iter()
            .filter(|d| {
                let accounted = find(&cs.changes, &d.holds, removes).is_some()
                    || find(&cs.changes, &d.holds, removes_with_its_owner).is_some();
                matches!(d.holds, Holds::Unrepresentable(_))
                    || (!as_declared(&cs.changes, d).managed(declared)
                        && !accounted
                        && !its_parents_once_attached(&cs.changes, &d.holds))
            })
            .cloned()
            .collect();
        if let Some(why) = pbps_pg::modules::unmanaged_refusal(root, &blocked, &Schema::default()) {
            refused.push(why);
        }
    }
    if !refused.is_empty() {
        return Err(refused.join("\n\n"));
    }

    let before = cs.changes.len();
    for (root, _) in &roots {
        let Some(deps) = found.get(root) else {
            continue;
        };
        for d in deps {
            // Placed by the drop, after this loop: see `after_its_release`.
            if is_generated(&d.holds) {
                continue;
            }
            split_in_place_edit(&mut cs.changes, &d.holds, dialect);
            let Some(at) = span(&cs.changes, root) else {
                continue;
            };
            if attached_at(&cs.changes, &d.holds).is_some_and(|i| i < at.drop_at) {
                continue;
            }
            // Removed before the module's drop: moved there if the plan
            // removes it later, synthesized there if it does not remove it.
            // Taken away with its table or column before the module goes:
            // already removed, and nothing to put back, since the
            // declarations no longer have its owner. A removal synthesized
            // here would name an object that is gone by then.
            if find(&cs.changes, &d.holds, removes_with_its_owner).is_some_and(|i| i < at.drop_at) {
                continue;
            }
            let removal = find(&cs.changes, &d.holds, removes);
            let removed_at = match removal {
                Some(i) if i < at.drop_at => i,
                Some(i) => {
                    let mut moved = cs.changes.remove(i);
                    respell(
                        &mut moved,
                        &named_at(&cs.changes, at.drop_at, &d.holds),
                        dialect,
                    );
                    cs.changes.insert(at.drop_at, moved);
                    at.drop_at
                }
                None => {
                    // Named as it is where it goes: before the module's drop,
                    // after any rename the plan makes ahead of that.
                    let here = Dependent {
                        described: d.described.clone(),
                        holds: named_at(&cs.changes, at.drop_at, &d.holds),
                    };
                    let change = removal_of(&here, declared, ids)?;
                    cs.changes.insert(at.drop_at, planned(change, dialect));
                    at.drop_at
                }
            };
            // Its owner dropped by this plan, even after the module: then
            // what the declarations hold under the same name is the part of a
            // new table or column, created with it, and not this dependent
            // kept. The old one is removed before the drop above; the new one
            // is its own creation's business.
            if find(&cs.changes, &d.holds, removes_with_its_owner).is_some() {
                continue;
            }
            let kept = as_declared(&cs.changes, d);
            if !kept.managed(declared) {
                continue;
            }
            // Kept by the declarations: back after the module's create, or
            // anywhere after its own removal when the module is gone for good.
            let Some(at) = span(&cs.changes, root) else {
                continue;
            };
            let after = at.create_at.unwrap_or(removed_at);
            let restoration = find(&cs.changes, &d.holds, restores);
            match restoration {
                Some(j) if j > after => {}
                Some(j) => {
                    let mut moved = cs.changes.remove(j);
                    // `after` moved down by one if the restoration was above it.
                    let after = if j < after { after - 1 } else { after };
                    respell(
                        &mut moved,
                        &named_at(&cs.changes, after + 1, &d.holds),
                        dialect,
                    );
                    cs.changes.insert(after + 1, moved);
                }
                None if at.create_at.is_some() => {
                    let change = restoration_of(&kept, declared, ids).ok_or_else(|| {
                        format!(
                            "{}: the declarations hold it, but not in a form pbps can restore",
                            d.described
                        )
                    })?;
                    cs.changes.insert(after + 1, planned(change, dialect));
                }
                None => {
                    return Err(format!(
                        "`{root}` is dropped by this plan, and {} depends on it and is kept by \
                         the declarations. Remove the dependency from its declaration, or keep \
                         `{root}`, and plan again.",
                        d.described
                    ));
                }
            }
        }
    }
    // A dependent module's drop can itself move after its own releases, so
    // repeated until nothing moves: each pass only moves a drop later, after
    // something it depends on, and the drops form no cycle, so the passes end.
    for _ in 0..=roots.len() {
        let mut moved = false;
        for (root, _) in &roots {
            moved |= after_its_release(cs, root, found.get(root).map_or(&[][..], Vec::as_slice));
        }
        if !moved {
            break;
        }
    }
    let added = cs.changes.len() - before;
    // A rebuild woven in here carries the index's declared parameters in its
    // `CREATE`, as the differ's own do (#1483 review).
    pbps_model::change::drop_parameter_changes_of_rebuilt_indexes(&mut cs.changes, |p| &p.change);
    Ok(added)
}

fn is_generated(holds: &Holds) -> bool {
    matches!(
        holds,
        Holds::TablePart {
            part: Part::Generated(_),
            ..
        }
    )
}

/// Moves a module's drop after what releases its generated-column dependents:
/// the expression change that stops calling it, or the drop of the column or
/// its table (DEC-1168.1).
///
/// The drop moves, not the release. A release keeps the place the differ gave
/// it, after the column additions, renames, retypes and relaxations its new
/// expression may need, which a release moved up to the module's drop would
/// run ahead of. What the drop leaves behind is a function whose own body
/// reads a column the plan drops or retypes before the release, which only
/// the engine can tell and refuses inside the transaction. A module rebuilt in
/// place is already after both.
///
/// A module that depends on this one, dropped by the plan, counts as well:
/// its own drop may have moved after its own releases, and this drop has to
/// stay behind it. Returns whether the drop moved.
fn after_its_release(cs: &mut ChangeSet, root: &ModuleId, deps: &[Dependent]) -> bool {
    let Some(at) = span(&cs.changes, root) else {
        return false;
    };
    let latest = deps
        .iter()
        .filter_map(|d| match &d.holds {
            Holds::TablePart {
                part: Part::Generated(_),
                ..
            } => find(&cs.changes, &d.holds, removes)
                .or_else(|| find(&cs.changes, &d.holds, removes_with_its_owner)),
            Holds::Module(_) => find(&cs.changes, &d.holds, removes),
            Holds::TablePart { .. } | Holds::Unrepresentable(_) => None,
        })
        .filter(|i| *i > at.drop_at)
        .max();
    let Some(i) = latest else {
        return false;
    };
    // Removing the drop shifts what it follows up by one, so inserting at its
    // old index puts the drop right after it.
    let drop = cs.changes.remove(at.drop_at);
    cs.changes.insert(i, drop);
    true
}

/// The columns whose default a row this plan writes takes: an insert that
/// omits the column (its `defaults`), and an update that sets it back to its
/// default (#1030). A default these rows need has to be in place before them.
/// Any other write, an update of another column or an insert that spells this
/// one, leaves the default free to move after a rebuilt function.
#[allow(clippy::wildcard_enum_match_arm)]
fn defaults_taken_by_rows(cs: &ChangeSet) -> std::collections::BTreeSet<ColumnRef> {
    let mut out = std::collections::BTreeSet::new();
    for p in &cs.changes {
        match &p.change {
            Change::InsertRow {
                table, defaults, ..
            } => {
                for column in defaults.keys() {
                    out.insert(ColumnRef::new(table.clone(), column.clone()));
                }
            }
            Change::UpdateRow { table, columns, .. } => {
                for (column, (_, after)) in columns {
                    if matches!(after, pbps_model::Cell::Default(_)) {
                        out.insert(ColumnRef::new(table.clone(), column.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Where the last function create is, when the plan creates or rebuilds a
/// function; `None` when it does neither, and nothing has to follow a create.
#[allow(clippy::wildcard_enum_match_arm)]
fn last_function_create(cs: &ChangeSet) -> Option<usize> {
    // A function new to the database binds what calls it exactly as a rebuilt
    // one does: a check, default or generation expression the differ put
    // ahead of its `CREATE FUNCTION` names a function not there yet, and the
    // engine refuses it (DEC-942.1).
    let created = cs.changes.iter().any(|p| {
        matches!(&p.change, Change::CreateModule { module, .. } if module.kind == ModuleKind::Function)
    });
    // A routine the plan drops and creates counts when either side is a
    // function. The created side counts because a procedure that becomes a
    // function is a `DropModule` of the procedure and a `CreateModule` of the
    // function under one id, and whatever calls the new function has to follow
    // its create (#1024). The dropped side counts too: a function that becomes
    // a procedure leaves nothing of its own to call, but the same revision may
    // create another function that a new check calls, and judging by the
    // created kind alone stopped moving that check (#1047).
    let rebuilt = dropped_modules(cs).into_iter().any(|(id, dropped)| {
        cs.changes.iter().any(|p| match &p.change {
            Change::AlterModule { id: x, module } | Change::CreateModule { id: x, module } => {
                *x == id && (dropped == ModuleKind::Function || module.kind == ModuleKind::Function)
            }
            _ => false,
        })
    });
    if !rebuilt && !created {
        return None;
    }
    cs.changes.iter().rposition(|p| match &p.change {
        Change::AlterModule { module, .. } | Change::CreateModule { module, .. } => {
            module.kind == ModuleKind::Function
        }
        _ => false,
    })
}

/// Every function this plan creates or rebuilds, by the bare name a call
/// spells, with the position of its create.
#[allow(clippy::wildcard_enum_match_arm)]
fn function_creates(cs: &ChangeSet) -> Vec<(String, usize)> {
    cs.changes
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.change {
            Change::AlterModule { id, module } | Change::CreateModule { id, module }
                if module.kind == ModuleKind::Function =>
            {
                id.referenced_name().map(|n| (n.name, i))
            }
            _ => None,
        })
        .collect()
}

/// Whether `text` may call one of `functions`: the function's bare name
/// occurs in it as code, by the engine's lexer, or in a literal's contents,
/// which an OID-alias type such as `regprocedure` resolves to the function
/// (`pbps_pg::generated::may_call`, DEC-1364.1). Which function a call binds
/// to is not known without parsing it (DECISIONS 174), so a name in another
/// schema, or a column, field or word of the same name, counts too. A false
/// yes costs a later position; a false no would put the call ahead of the
/// create the engine needs.
fn calls_one_of(text: &str, functions: &[(String, usize)]) -> bool {
    functions
        .iter()
        .any(|(name, _)| pbps_pg::generated::may_call(text, name))
}

/// The expression text an index holds: its filter and its expression keys.
fn index_text(index: &pbps_model::Index) -> String {
    let mut parts: Vec<&str> = index.filter.iter().map(String::as_str).collect();
    parts.extend(index.columns.iter().filter_map(|c| c.key.expression()));
    parts.join(" , ")
}

/// The expression an added column carries: its default or its generation
/// expression. The engine refuses both together (DEC-1168.1).
fn column_expression(column: &pbps_model::Column) -> Option<&str> {
    column
        .default
        .as_deref()
        .or_else(|| column.generated.as_ref().map(|g| g.expression.as_str()))
}

/// Splits a table this plan creates, ahead of a function it creates or
/// rebuilds, into the table and its parts that may call that function as
/// separate changes, so that [`after_the_rebuilds`] can move them after the
/// function (#1027, DEC-942.1, DEC-1364.1). Returns how many changes it added.
///
/// The differ writes a new table as one `CreateTable` carrying its checks,
/// indexes and column defaults, and emits them together. Moving that change
/// is not an option, since views and routines may read the table. So its
/// checks become `AddCheck`, its indexes that hold an expression (a filter
/// or an expression key, DEC-1169.2) `AddIndex`, and its column defaults
/// `AlterColumnDefault`, each right after the table, when their text names a
/// function the plan creates or rebuilds. The same rule as for an existing
/// table then places them. A part whose text names none stays in the table,
/// where nothing it calls is missing. An index over plain columns stays in the
/// table: it holds no expression. A default stays when a row the plan
/// writes takes it (an insert that omits the column, an update that sets it
/// back to its default, #1030), and when no ids file names the column, since
/// `AlterColumnDefault` needs its uid.
///
/// Split this way, the table's check is added to an empty table, and it asks
/// for `--allow constraint` as any added check does. The approver sees the
/// constraint as its own step, where before it was inside `CREATE TABLE`.
///
/// A generated column is not split out. Its expression may call the function
/// as a default's may, but the engine has no way to give an existing column
/// one, and taken out of the table as a column of its own it would change the
/// table's column order (DEC-1168.1, DEC-1364.1).
///
/// A partitioned table's default split out this way reaches every partition
/// when it is set, measured on 16 and 18, a new partition's own default
/// included: [`after_their_parents_defaults`] sets each own default again
/// after it (#1588).
// The complement is every change that writes no rows.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn split_new_tables(
    cs: &mut ChangeSet,
    ids: &[&IdsFile],
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let Some(last) = last_function_create(cs) else {
        return Ok(0);
    };
    let taken = defaults_taken_by_rows(cs);
    let functions = function_creates(cs);
    let calls = |text: &str| calls_one_of(text, &functions);
    let mut added = 0;
    for i in (0..last).rev() {
        let Change::CreateTable { name, table, .. } = &mut cs.changes[i].change else {
            continue;
        };
        let name = name.clone();
        let mut parts: Vec<Change> = Vec::new();
        let calling: Vec<String> = table
            .checks
            .iter()
            .filter(|(_, check)| calls(&check.expression))
            .map(|(n, _)| n.clone())
            .collect();
        for check in calling {
            if let Some(constraint) = table.checks.remove(&check) {
                parts.push(Change::AddCheck {
                    table: name.clone(),
                    name: check,
                    constraint,
                });
            }
        }
        let filtered: Vec<String> = table
            .indexes
            .iter()
            .filter(|(_, index)| index.holds_expression() && calls(&index_text(index)))
            .map(|(n, _)| n.clone())
            .collect();
        for index in filtered {
            if let Some(spec) = table.indexes.remove(&index) {
                // Never the clustered index, which cannot be filtered; asked
                // of the table anyway, so the split cannot say otherwise.
                let clustered = table.index_is_clustered(&index);
                parts.push(Change::AddIndex {
                    table: name.clone(),
                    name: index,
                    index: Box::new(spec),
                    clustered,
                });
            }
        }
        for (column, spec) in table.columns.iter_mut() {
            let column_ref = ColumnRef::new(name.clone(), column.clone());
            if taken.contains(&column_ref) {
                continue;
            }
            let Some(uid) = ids.iter().find_map(|ids| ids.column_uid(&column_ref)) else {
                continue;
            };
            if spec.default.as_deref().is_some_and(calls)
                && let Some(to) = spec.default.take()
            {
                parts.push(Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: column_ref,
                    from: None,
                    to: Some(to),
                });
            }
        }
        if parts.is_empty() {
            continue;
        }
        // The table's own change no longer carries what moved out of it, so
        // its risks are recomputed with the rest.
        let table_change = cs.changes[i].change.clone();
        cs.changes[i] = planned(table_change, dialect);
        added += parts.len();
        for (k, part) in parts.into_iter().enumerate() {
            cs.changes.insert(i + 1 + k, planned(part, dialect));
        }
    }
    Ok(added)
}

/// Sets each partition's own default again after its parent's (#1588,
/// DEC-1581.1). Returns how many changes it added.
///
/// A partitioned table's `ALTER COLUMN … SET DEFAULT` reaches every
/// partition and overwrites a partition's own default on that column, and its
/// `DROP DEFAULT` takes it away, measured on 16 and 18. A parent's default this
/// plan sets, put back around a rebuild by [`weave`] or split out of a new
/// table by [`split_new_tables`], is therefore followed by every declared
/// partition's own default on that column, whether or not it calls the
/// function: one the plan already sets ahead of the parent's is moved after
/// it, and one it does not set is added. A partition created after the
/// parent's default needs neither, since its `CREATE` sets its own.
///
/// `ONLY` on the parent would leave the partitions alone, but it would also
/// leave a new partition without the parent's copy, and the parent's own
/// change is the parent's to spell (#1546). A parent's own default change is
/// planned on the parent, and its partitions' own defaults set again after it
/// by the differ, `DROP DEFAULT` included (DEC-1687.1).
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn after_their_parents_defaults(
    cs: &mut ChangeSet,
    declared: &Schema,
    ids: &[&IdsFile],
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let mut added = 0;
    let mut i = 0;
    while i < cs.changes.len() {
        let Change::AlterColumnDefault {
            column,
            to: Some(_),
            ..
        } = &cs.changes[i].change
        else {
            i += 1;
            continue;
        };
        let column = column.clone();
        let own: Vec<(TableName, String)> = declared
            .tables
            .iter()
            .filter_map(|(name, t)| {
                let of = t
                    .partition_of
                    .as_ref()
                    .filter(|of| of.parent == column.table)?;
                let default = of.columns.get(&column.name)?.default.clone()?;
                Some((name.clone(), default))
            })
            .collect();
        for (partition, default) in own {
            let sets = |c: &Change| {
                matches!(c, Change::SetPartitionDefault { table, column: c, to: Some(_), .. }
                    if *table == partition && *c == column.name)
            };
            let later = cs.changes[i + 1..].iter().any(|p| {
                sets(&p.change)
                    || matches!(&p.change, Change::CreateTable { name, .. } if *name == partition)
            });
            if later {
                continue;
            }
            if let Some(k) = (0..i).rev().find(|&k| sets(&cs.changes[k].change)) {
                let moved = cs.changes.remove(k);
                i -= 1;
                cs.changes.insert(i + 1, moved);
            } else {
                let uid = ids
                    .iter()
                    .find_map(|ids| ids.table_uid(&partition))
                    .cloned()
                    .ok_or_else(|| format!("no identity names the partition `{partition}`"))?;
                let fallback = parent_default(declared, &partition, &column.name);
                cs.changes.insert(
                    i + 1,
                    planned(
                        Change::SetPartitionDefault {
                            uid,
                            table: partition,
                            parent: column.table.clone(),
                            column: column.name.clone(),
                            from: None,
                            to: Some(default),
                            fallback,
                        },
                        dialect,
                    ),
                );
                added += 1;
            }
        }
        i += 1;
    }
    Ok(added)
}

/// Takes the plan's `PUBLIC` decisions out, so that the passes below reorder
/// modules and what may need them without them (#687).
///
/// Each decision sorts directly after its routine's `CREATE` (DEC-687.1), in
/// the very stretch [`after_their_functions`] and [`weave`] reorder. There it
/// is one more change those passes must not overtake, and it chained a
/// routine's create to another routine's revoke: a cycle where none exists.
/// A decision depends only on its routine existing, so it is set aside and
/// put back by [`settle_public_execution`] once the order is final.
pub(crate) fn take_public_execution(cs: &mut ChangeSet) -> Vec<PlannedChange> {
    let (decisions, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut cs.changes)
        .into_iter()
        .partition(|p| matches!(p.change, Change::PublicExecution { .. }));
    cs.changes = rest;
    decisions
}

/// Puts each `PUBLIC` decision back directly after the last `CREATE` or
/// `ALTER` of its routine, which is where DEC-687.1 orders it. A decision
/// on a routine this plan does not create or rebuild goes ahead of the roles
/// and grants, where its class leaves it.
pub(crate) fn settle_public_execution(cs: &mut ChangeSet, decisions: Vec<PlannedChange>) {
    for decision in decisions {
        let Change::PublicExecution { routine, .. } = &decision.change else {
            cs.changes.push(decision);
            continue;
        };
        let id = ModuleId::Routine(routine.clone());
        let at = cs
            .changes
            .iter()
            .rposition(|p| {
                matches!(&p.change,
                    Change::CreateModule { id: x, .. } | Change::AlterModule { id: x, .. } if *x == id)
            })
            .map(|i| i + 1)
            .or_else(|| {
                cs.changes.iter().position(|p| {
                    matches!(
                        p.change,
                        Change::CreateRole { .. } | Change::Grant { .. } | Change::SetDataMode { .. }
                    )
                })
            })
            .unwrap_or(cs.changes.len());
        cs.changes.insert(at, decision);
    }
}

/// Moves what this plan adds that may call a function it creates or rebuilds
/// to after that function's create (#942, DEC-942.1, DEC-1364.1). Returns how
/// many changes moved, or why no order performs the plan.
///
/// [`weave`] reads the catalog, where an addition this plan makes does not
/// exist yet, so it never sees one. The differ puts a check or an index in
/// class 13 and a default in class 9, ahead of every module in class 14, which
/// everywhere else is right: a view needs its table's columns, and nothing a
/// module holds needs a constraint. With a function rebuilt, it is exactly
/// wrong: the check is created against the old function, and the rebuild's
/// `DROP FUNCTION` is refused because of it. With a function new to the
/// database, it names one not there yet, and the check is refused itself.
///
/// Which function an expression calls is not known without parsing it, and
/// the planner does not parse expressions (DECISIONS 174). It is read the way
/// `creation_order_with` reads a module's definition (DECISIONS 315): every
/// name the text could call, by the engine's lexer, is taken as a call
/// ([`calls_one_of`]). An addition whose text names a function the plan
/// creates or rebuilds goes after the last function the plan creates, in the
/// order it had. That is a check, an index with a filter or an expression key
/// (an index's other columns are names), a default being set, and a changed
/// generation expression. An addition whose text names none stays where the
/// differ put it. A unique index with no expression stays too, since a
/// foreign key in its class may rest on it.
///
/// One default does not move: a default a row this plan writes takes, an
/// insert that omits the column or an update that sets it back to its
/// default (#1030). Row writes come before the modules, and such a row would
/// take the old default instead of the declared one. A write that spells the
/// column, or touches only others, takes nothing from it. Leaving it in
/// place means a default that calls the rebuilt function still meets the
/// `DROP` there, and the apply fails and rolls back. That is loud, where
/// moving it would record rows the declarations did not ask for.
///
/// A column added with a default or a generation expression that names a
/// function the plan creates or rebuilds is placed by [`after_their_functions`].
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn after_the_rebuilds(
    cs: &mut ChangeSet,
    released: &BTreeSet<ColumnRef>,
    deps: &ModuleDeps,
) -> Result<usize, String> {
    let Some(last) = last_function_create(cs) else {
        return Ok(0);
    };
    let taken = defaults_taken_by_rows(cs);
    let functions = function_creates(cs);
    let calls = |text: &str| calls_one_of(text, &functions);
    let recomputed: BTreeSet<&ColumnRef> = cs.changes[..last]
        .iter()
        .filter_map(|p| match &p.change {
            Change::AlterColumnExpression { column, to, .. }
                if !released.contains(column) && calls(to) =>
            {
                Some(column)
            }
            _ => None,
        })
        .collect();
    let moves: Vec<bool> = cs.changes[..last]
        .iter()
        .map(|p| match &p.change {
            Change::AddCheck { constraint, .. } => calls(&constraint.expression),
            Change::AlterColumnDefault {
                column,
                to: Some(to),
                ..
            } => !taken.contains(column) && calls(to),
            // A partition's, its own or its parent's taken back (#1588). A
            // row written through its parent takes the parent's default, not
            // the partition's (DEC-1578.1), so none is taken ahead of it.
            Change::SetPartitionDefault { to, fallback, .. } => to
                .as_ref()
                .or(fallback.as_ref())
                .is_some_and(|text| calls(text)),
            // A generation expression binds the functions it calls exactly
            // as a default does (DEC-1168.1). No row writes a generated
            // column, so none is taken ahead of it.
            // One that releases a generated column from a module the plan
            // drops stays ahead of that drop, which `weave` put after it.
            Change::AlterColumnExpression { column, .. } => recomputed.contains(column),
            // What validates a recomputed column's values stays behind its new
            // expression, which the order kept among the moved preserves: a
            // `NOT NULL`, and a key over it on either side of a foreign key.
            // Left here, it would judge the values the old expression stored.
            Change::AlterColumnNullability {
                column,
                to_nullable: false,
                ..
            } => recomputed.contains(column),
            Change::AddIndex { table, index, .. } => {
                (index.holds_expression() && calls(&index_text(index)))
                    || index.column_keys().is_some_and(|keys| {
                        keys.iter().any(|k| recomputed.contains(&table.column(k)))
                    })
            }
            Change::AddUnique {
                table, constraint, ..
            } => constraint
                .columns
                .iter()
                .any(|c| recomputed.contains(&table.column(c))),
            Change::SetPrimaryKey {
                table,
                to: Some(pk),
                ..
            } => pk
                .columns
                .iter()
                .any(|c| recomputed.contains(&table.column(c))),
            Change::AddForeignKey {
                table, constraint, ..
            } => {
                constraint
                    .columns
                    .iter()
                    .any(|c| recomputed.contains(&table.column(c)))
                    || constraint
                        .references_columns
                        .iter()
                        .any(|c| recomputed.contains(&constraint.references_table.column(c)))
            }
            _ => false,
        })
        .collect();
    let mut moved = Vec::new();
    let mut kept = Vec::new();
    let tail = cs.changes.split_off(last + 1);
    for (i, p) in cs.changes.drain(..).enumerate() {
        if moves.get(i).copied().unwrap_or(false) {
            moved.push(p);
        } else {
            kept.push(p);
        }
    }
    let count = moved.len();
    kept.extend(moved);
    kept.extend(tail);
    cs.changes = kept;
    Ok(count + after_their_functions(cs, deps)?)
}

/// The literals in the plan's expressions that may name a relation this plan
/// brings into being only after them (#1576, DEC-1576.1). PostgreSQL resolves
/// `'app.ix'::regclass` when the expression is created, so a default, check,
/// index or generation expression set before that relation's create fails
/// the apply. The apply rolls back, but the plan was approved.
///
/// The planner does not reorder for it: the differ puts expressions ahead of
/// the indexes and views that are the usual such relations, and a move would
/// pass rows the plan writes and the checks that judge them. The approver
/// splits the change instead, which the message says how to do.
///
/// A literal is a candidate only when the whole of it reads as the
/// relation's name, as an OID-alias input reads one ([`relation_literal`]),
/// in the arrival's schema or, unqualified, in the expression's. A literal
/// that merely contains the name is none. Each candidate is then looked up
/// on the target ([`LaterName`]), so one that already resolves is not
/// refused. One that is the name and is not cast can still be refused; the
/// remedy then costs a second plan, where missing a reference costs a failed
/// apply.
///
/// "Later" is by statement, not only by change: a new table's own indexes
/// are created after its defaults and checks ([`Step`], #1592).
pub(crate) fn names_a_later_relation(cs: &ChangeSet, dialect: &dyn Dialect) -> Vec<LaterName> {
    let mut arrivals: Vec<((usize, Step), TableName)> = Vec::new();
    let mut generated: Vec<((usize, Step), TableName)> = Vec::new();
    for (i, p) in cs.changes.iter().enumerate() {
        for (step, r, named) in relations_brought(&p.change) {
            if named {
                arrivals.push(((i, step), r));
            } else {
                generated.push(((i, step), r));
            }
        }
    }
    // A generated name the plan also declares is the declared relation's:
    // the engine moves the key's index aside to a numbered name, so only
    // the declared one is read as arriving. Likewise only the first of two
    // keys generating one name holds it: two tables sharing their first 58
    // bytes cut to the same `_pkey` (#1640 review).
    generated.sort_by_key(|(at, _)| *at);
    let mut held: Vec<TableName> = Vec::new();
    generated.retain(|(_, g)| {
        let free = !arrivals.iter().any(|(_, r)| r == g) && !held.contains(g);
        held.push(g.clone());
        free
    });
    let generated: Vec<TableName> = generated
        .into_iter()
        .map(|(at, r)| {
            arrivals.push((at, r.clone()));
            r
        })
        .collect();
    let mut found = Vec::new();
    for (i, p) in cs.changes.iter().enumerate() {
        let Some(scope) = expression_schema(&p.change) else {
            continue;
        };
        // Where an unqualified name is looked up, in order: `pg_catalog`
        // ahead of the write path, the expression's own schema (`None` is a
        // path the scan cannot read).
        let searched = |path: Vec<String>| {
            let mut path: Vec<String> = path.into_iter().filter(|s| s != "pg_temp").collect();
            if !path.iter().any(|s| s == "pg_catalog") {
                path.insert(0, "pg_catalog".to_owned());
            }
            Some(path)
        };
        let written = searched(vec![scope.to_owned()]);
        // A routine's string body is analysed under its own `SET
        // search_path`, which the engine applies before the validator runs;
        // an atomic body and a view are parsed under the session's, the write
        // path. Measured on 16 and 18 (#1599 review).
        // Only a routine's body is a string the engine analyses: a view's
        // or a trigger's literals are data, read once (#1599 review).
        let header = if let Change::CreateModule { module, .. } | Change::AlterModule { module, .. } =
            &p.change
            && matches!(module.kind, ModuleKind::Function | ModuleKind::Procedure)
        {
            Some(routine_header(&module.definition, dialect))
        } else {
            None
        };
        let own = header.as_ref().map(|h| match &h.path {
            Some(Some(path)) => searched(path.clone()),
            Some(None) => written.clone(),
            None => None,
        });
        for (step, whose, text) in expressions_set(&p.change) {
            let here = (i, step);
            let outer = dialect.lexicon().string_literals(&text);
            // A routine's string body is itself a literal, and its own
            // literals are bound as it is created: under the
            // `check_function_bodies = on` the framing pins, a SQL body is
            // analysed then, as an atomic body and a view are. So they are
            // read one level in. A PL/pgSQL body binds them only when it
            // runs, and is read alike: a refusal there costs a second plan, a
            // missed one a failed apply (#1599 review).
            // A header the scan could not read to its end may hold its body
            // in any literal, so every one is read one level in.
            let inner: Vec<String> = match &header {
                Some(RoutineHeader {
                    body: Some(body), ..
                }) => dialect.lexicon().string_literals(body),
                Some(RoutineHeader { unsure: true, .. }) => outer
                    .iter()
                    .flat_map(|l| dialect.lexicon().string_literals(l))
                    .collect(),
                _ => Vec::new(),
            };
            // A string body's literals are only checked as it is created, so
            // they bind nothing: any schema on the path that holds the name
            // then satisfies them. Every other literal is bound where it is
            // first found (#1599 review).
            let literals = outer.into_iter().map(|l| (l, &written, false)).chain(
                inner
                    .into_iter()
                    .map(|l| (l, own.as_ref().unwrap_or(&written), true)),
            );
            for (literal, path, checked_only) in literals {
                let Some((schema, name)) = relation_literal(&literal) else {
                    continue;
                };
                let later = |s: &str| {
                    arrivals
                        .iter()
                        .find(|(at, r)| *at > here && r.name == name && r.schema == s)
                        .map(|(_, r)| r)
                };
                let reference = match (&schema, path) {
                    (Some(s), _) => later(s).map(|r| (r, Vec::new())),
                    // The path, in order, up to the first schema the name
                    // binds in: a relation the plan made before the
                    // expression binds it there, one made after is a
                    // reference, and the schemas ahead of it are the
                    // target's to answer for (see `LaterName`).
                    // Checked only: refused when no schema on the path holds
                    // the name by then, and one will later. The target is
                    // asked about every schema the plan does not fill later.
                    (None, Some(path)) if checked_only => {
                        let earlier = path.iter().any(|s| {
                            arrivals
                                .iter()
                                .any(|(at, r)| *at <= here && r.name == name && r.schema == *s)
                        });
                        let first = path.iter().find_map(|s| later(s));
                        first.filter(|_| !earlier).map(|r| {
                            // A schema whose later arrival is a generated
                            // name may hold it now as well (#1640 review).
                            let asked = path
                                .iter()
                                .filter(|s| later(s).is_none_or(|r| generated.contains(r)))
                                .cloned();
                            (r, asked.collect())
                        })
                    }
                    (None, Some(path)) => {
                        let mut hit = None;
                        for (k, s) in path.iter().enumerate() {
                            if arrivals
                                .iter()
                                .any(|(at, r)| *at <= here && r.name == name && r.schema == *s)
                            {
                                break;
                            }
                            if let Some(r) = later(s) {
                                hit = Some((r, path[..k].to_vec()));
                                break;
                            }
                        }
                        hit
                    }
                    // A path the scan cannot read: an arrival of that name
                    // in any schema may be the one, so it is read as one.
                    (None, None) => arrivals
                        .iter()
                        .find(|(at, r)| *at > here && r.name == name)
                        .map(|(_, r)| (r, vec!["pg_catalog".to_owned()])),
                };
                if let Some((relation, mut searched)) = reference {
                    // A generated name may be held in its own schema now,
                    // and then the key's index takes a numbered one and the
                    // literal names what holds it: the target says which.
                    if generated.contains(relation) && !searched.contains(&relation.schema) {
                        searched.push(relation.schema.clone());
                    }
                    found.push(LaterName {
                        what: format!("{whose} names {relation}, which the plan creates after it"),
                        searched,
                        name,
                    });
                }
            }
        }
    }
    found
}

/// A word of a routine's header.
struct HeaderWord {
    /// The offset it ends at, which is the definition's.
    end: usize,
    /// A quoted identifier decoded, the rest folded.
    text: String,
    quoted: bool,
}

impl HeaderWord {
    /// Whether this is the keyword `kw`, which a quoted identifier never is.
    fn is(&self, kw: &str) -> bool {
        !self.quoted && self.text == kw
    }
}

/// A routine header's depth-zero words, with the offsets they end at, which
/// are the definition's: `header` blanks byte for byte. A parameter or a
/// return column is at depth one. Where the header ends is
/// [`routine_header`]'s to say.
fn header_words(definition: &str, dialect: &dyn Dialect) -> Vec<HeaderWord> {
    let header = dialect.lexicon().header(definition);
    let word = dialect.lexicon().identifier_continues;
    let mut words: Vec<HeaderWord> = Vec::new();
    let mut depth = 0usize;
    let mut chars = header.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '"' => {
                let mut name = String::new();
                let mut end = header.len();
                while let Some((i, c)) = chars.next() {
                    if c == '"' {
                        if chars.peek().map(|(_, n)| *n) == Some('"') {
                            name.push('"');
                            chars.next();
                        } else {
                            end = i + 1;
                            break;
                        }
                    } else {
                        name.push(c);
                    }
                }
                if depth == 0 {
                    words.push(HeaderWord {
                        end,
                        text: name,
                        quoted: true,
                    });
                }
            }
            c if word(c) => {
                let mut end = at + c.len_utf8();
                while let Some((i, c)) = chars.peek().copied() {
                    if !word(c) {
                        break;
                    }
                    end = i + c.len_utf8();
                    chars.next();
                }
                if depth == 0 {
                    let w = dialect.fold_ident(&header[at..end]).into_owned();
                    words.push(HeaderWord {
                        end,
                        text: w,
                        quoted: false,
                    });
                }
            }
            _ => {}
        }
    }
    words
}

/// What [`names_a_later_relation`] reads from a routine's header.
struct RoutineHeader {
    /// Its own `SET search_path`: `Some(None)` when it sets none,
    /// `Some(Some(schemas))` in order, and `None` when the clause is not in a
    /// form the scan reads, which the caller reads as any schema (#1604).
    path: Option<Option<Vec<String>>>,
    /// Its string body, the literal after its `AS`: the only literal whose
    /// own literals the engine analyses as it creates the routine. An atomic
    /// body has none.
    body: Option<String>,
    /// Whether a clause could not be read, so that where the header ends,
    /// and so which literal is the body, is not known.
    unsure: bool,
}

/// A routine's header, read clause by clause from the start (#1604).
///
/// Two spellings of `SET search_path` are read: the one
/// `pg_get_functiondef` writes, and so `pull` declares, one single-quoted
/// literal per schema (`TO 'a', 'b'`); and plain or double-quoted
/// identifiers. Anything else, a dollar-quoted argument, an escape string,
/// `FROM CURRENT`, `"$user"`, a comment, is unread: it costs a second plan
/// where the body names a relation the plan makes later, never a failed
/// apply. Measured on 18: the last of two clauses is the one stored, and each
/// quoted argument is one schema, `TO 'a,b'` the schema `a,b`.
///
/// Each `SET` clause's value list is consumed to its end, so a schema or a
/// value named `set`, `begin` or `return` is an entry, never a clause or a
/// body. The body is the string after `AS`, which clauses may follow, or
/// starts at `RETURN` or `BEGIN ATOMIC`, which comes last.
fn routine_header(definition: &str, dialect: &dyn Dialect) -> RoutineHeader {
    let words = header_words(definition, dialect);
    let mut header = RoutineHeader {
        path: Some(None),
        body: None,
        unsure: false,
    };
    let mut from = 0;
    let mut k = 0;
    while let Some(w) = words.get(k) {
        k += 1;
        if w.end <= from {
            continue;
        }
        // Clauses may follow a string body, `SET search_path` among them,
        // and the engine applies them all; the body itself is blanked, so
        // the walk goes on past it (#1599 review).
        if w.is("as") {
            if header.body.is_none() {
                header.body = definition
                    .get(w.end..)
                    .and_then(|rest| dialect.lexicon().string_literals(rest).into_iter().next());
            }
            continue;
        }
        if w.is("return") || (w.is("begin") && words.get(k).is_some_and(|n| n.is("atomic"))) {
            break;
        }
        if !w.is("set") {
            continue;
        }
        let Some(name) = words.get(k) else {
            break;
        };
        k += 1;
        // A setting's name matches in any case, quoted or not.
        let search = name.text.eq_ignore_ascii_case("search_path");
        match setting_value(definition, name.end, dialect, search) {
            Some((values, end)) => {
                if search {
                    header.path = Some(Some(values));
                }
                from = end;
            }
            None => {
                if search {
                    header.path = None;
                }
                header.unsure = true;
                from = name.end;
            }
        }
    }
    header
}

/// The value list of a `SET` clause whose name ends at `after`, and the
/// offset it ends at; `None` when it is not in a form [`routine_header`]
/// reads. A `search_path` list holds schemas, folded as the engine folds
/// them; any other setting's entries are only stepped over.
fn setting_value(
    definition: &str,
    after: usize,
    dialect: &dyn Dialect,
    search: bool,
) -> Option<(Vec<String>, usize)> {
    // The engine's own lexis throughout (#1599 review): what continues an
    // identifier, how an unquoted one folds, and the white space between
    // tokens, so a name reads here as the engine reads it.
    let word = dialect.lexicon().identifier_continues;
    fn skip(s: &str) -> &str {
        s.trim_start_matches(is_input_space)
    }
    let rest = skip(definition.get(after..)?);
    let rest = if let Some(r) = rest.strip_prefix('=') {
        r
    } else if rest.get(..2).is_some_and(|w| w.eq_ignore_ascii_case("to"))
        && !rest[2..].starts_with(word)
    {
        &rest[2..]
    } else {
        return None;
    };
    let mut values = Vec::new();
    let mut rest = skip(rest);
    loop {
        // Each part, and the bytes it takes.
        let (value, len) = if let Some(r) = rest.strip_prefix('\'') {
            quoted_part(r, '\'').map(|(v, n)| (v, n + 1))?
        } else if let Some(r) = rest.strip_prefix('"') {
            quoted_part(r, '"').map(|(v, n)| (v, n + 1))?
        } else if !search {
            // Another setting's bare value (`64MB`, `-1`, `on`) runs to a
            // separator.
            let len = rest
                .find(|c: char| is_input_space(c) || c == ',')
                .unwrap_or(rest.len());
            if len == 0 {
                return None;
            }
            (rest[..len].to_owned(), len)
        } else {
            let len = rest.find(|c: char| !word(c)).unwrap_or(rest.len());
            let w = dialect.fold_ident(&rest[..len]).into_owned();
            // A word glued to anything but a separator is a form this scan
            // does not read: `E'…'`, `U&"…"`.
            let glued = rest[len..]
                .chars()
                .next()
                .is_some_and(|c| !is_input_space(c) && c != ',');
            // A plain identifier only: one starting with a digit or holding
            // a `$`, a dollar-quoted `$$other$$` among them, is a form this
            // scan does not read (#1604).
            let plain = rest[..len]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_' || !c.is_ascii())
                && !rest[..len].contains('$');
            if !plain || w == "from" || w == "default" || glued {
                return None;
            }
            (w, len)
        };
        // `$user` is a schema named for whichever role runs the body.
        if search && value == "$user" {
            return None;
        }
        values.push(value);
        rest = skip(&rest[len..]);
        match rest.strip_prefix(',') {
            Some(r) => rest = skip(r),
            // The list ends at the header's end or at the next clause's
            // keyword. Anything else, a comment among them, is a form this
            // scan does not read, never the end of a shorter list.
            None if rest.is_empty() || rest.starts_with(word) => {
                return Some((values, definition.len() - rest.len()));
            }
            None => return None,
        }
    }
}

/// The contents of a part quoted with `quote`, a doubled quote standing for
/// itself, and the byte length of the part after its opening quote; `None`
/// when it never closes.
fn quoted_part(after_open: &str, quote: char) -> Option<(String, usize)> {
    let mut value = String::new();
    let mut chars = after_open.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c != quote {
            value.push(c);
        } else if chars.peek().map(|(_, n)| *n) == Some(quote) {
            value.push(quote);
            chars.next();
        } else {
            return Some((value, at + 1));
        }
    }
    None
}

/// A literal [`names_a_later_relation`] reads as a relation the plan creates
/// later. An unqualified one may instead resolve to a relation of that name
/// in `pg_catalog`, which is searched ahead of the write path; the target
/// says whether it does, and only a name that resolves nowhere is refused
/// ([`later_relation_refusal`]).
///
/// The arrival's own schema is never asked. A relation of that name there
/// now is one the plan must remove before its create, or the create fails;
/// removed before the expression, the expression fails, and removed after
/// it, the removal does, because the expression depends on what it bound
/// (measured on 18: `cannot drop index app.ix_new because other objects
/// depend on it`). So finding it there exempts nothing (#1589 review).
///
/// Except under a name the engine generates, an unnamed key's index's: one
/// held now makes the engine number the index instead, and the literal names
/// what holds it. So that schema is asked too (#1619).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaterName {
    pub(crate) what: String,
    /// The schemas ahead of the arrival's that the lookup searches, in order.
    pub(crate) searched: Vec<String>,
    pub(crate) name: String,
}

/// The refusal for the names that resolve nowhere on the target now.
pub(crate) fn later_relation_refusal(names: &[LaterName]) -> Result<(), String> {
    if names.is_empty() {
        return Ok(());
    }
    let found: Vec<&str> = names.iter().map(|n| n.what.as_str()).collect();
    Err(format!(
        "an expression names a relation this plan creates later, and PostgreSQL resolves the \
         name when the expression is created:\n  {}\nDeploy the relation first: leave the \
         expression out of this revision, apply it, then add the expression in a second plan",
        found.join("\n  ")
    ))
}

/// Where in its change's statements a relation arrives or an expression is
/// set (#1592). Every change but a new table is one statement, whose relation
/// arrives after the statement's own expressions.
///
/// A new table follows the PostgreSQL emitter (`create_table`), measured on
/// 18:
/// - Its defaults and generation expressions see the table itself, but no
///   index of it, the inline primary key's included: that index is built at
///   the end of the `CREATE TABLE`, after they are resolved. A partition's
///   own defaults, set right after its `CREATE`, see the same.
/// - Its checks are added after its primary key and unique constraints.
/// - Its indexes are created last, one by one in name order, and each one's
///   expression and filter see only those before it.
type Step = usize;

/// The table and its column expressions.
const AT_CREATE: Step = 0;
/// The primary key's and unique constraints' indexes.
const KEYS: Step = 1;
/// The checks.
const CHECKS: Step = 2;

/// The step of a new table's `k`th index in name order: its expression is
/// set at the step, and the index arrives at the next.
fn index_step(k: usize) -> Step {
    CHECKS + 1 + 2 * k
}

/// The name PostgreSQL gives the index of `table`'s unnamed primary key,
/// where every server encoding gives the same one. Measured on 18:
/// `<table>_pkey`, with the table's part cut to fit the 63-byte limit, and a
/// numbered `_pkey1`, `_pkey2`… instead while that name is held by any
/// relation of the schema, a composite type's included (#1619).
///
/// The cut counts bytes of the database's encoding, as `regclass` input does
/// (#1627 review). So past the limit, the name is told only when the part kept
/// is ASCII; otherwise there is none, and a literal naming it can be missed,
/// the failed apply DEC-1576.1 accepts, but never refused wrongly.
fn generated_key_name(table: &str) -> Option<String> {
    const SUFFIX: &str = "_pkey";
    let room = pbps_pg::MAX_IDENT_BYTES - SUFFIX.len();
    let kept = if table.len() <= room {
        table
    } else if table.as_bytes()[..room].is_ascii() {
        &table[..room]
    } else {
        return None;
    };
    Some(format!("{kept}{SUFFIX}"))
}

/// The relations a change brings into the namespace, by the name they arrive
/// under, and whether that name is declared rather than generated by the
/// engine. An index's name is a relation's on PostgreSQL, as is a view's.
#[allow(clippy::wildcard_enum_match_arm)]
fn relations_brought(change: &Change) -> Vec<(Step, TableName, bool)> {
    let index = |table: &TableName, name: &str| TableName {
        schema: table.schema.clone(),
        name: name.to_owned(),
    };
    // A key's index: its name, or the one the engine generates (#1619).
    let key = |table: &TableName, pk: &pbps_model::PrimaryKey| match &pk.name {
        Some(n) => Some((index(table, n), true)),
        None => generated_key_name(&table.name).map(|n| (index(table, &n), false)),
    };
    let one = match change {
        Change::CreateTable { name, table, .. } => {
            let mut out = vec![(AT_CREATE, name.clone(), true)];
            out.extend(
                table
                    .indexes
                    .keys()
                    .enumerate()
                    .map(|(k, n)| (index_step(k) + 1, index(name, n), true)),
            );
            out.extend(table.unique.keys().map(|n| (KEYS, index(name, n), true)));
            out.extend(
                table
                    .primary_key
                    .as_ref()
                    .and_then(|pk| key(name, pk))
                    .map(|(r, named)| (KEYS, r, named)),
            );
            return out;
        }
        Change::RenameTable { to, .. } => vec![(to.clone(), true)],
        Change::AddIndex { table, name, .. } | Change::AddUnique { table, name, .. } => {
            vec![(index(table, name), true)]
        }
        Change::SetPrimaryKey {
            table,
            to: Some(pk),
            ..
        } => key(table, pk).into_iter().collect(),
        Change::CreateModule {
            id: ModuleId::Named(name),
            module,
        } if module.kind == ModuleKind::View => {
            vec![(name.clone(), true)]
        }
        _ => Vec::new(),
    };
    // A one-statement change's relation exists only once its statement has
    // run, after the statement's own text is resolved: measured on 18, an
    // index filter or expression naming the index, and a view naming the
    // view, each fail (#1617 review). Only a new table is seen by its own
    // defaults and checks, which `AT_CREATE` above keeps.
    one.into_iter()
        .map(|(r, named)| (AT_CREATE + 1, r, named))
        .collect()
}

/// The schema an expression a change creates is written in. Its write
/// `search_path` is that schema alone: the CLI's dialect configures no extras
/// (`dialect_for`), and `pg_catalog`, searched first, holds no relation a plan
/// creates. So an unqualified name can only reach an arrival there
/// (#1589 review).
#[allow(clippy::wildcard_enum_match_arm)]
fn expression_schema(change: &Change) -> Option<&str> {
    match change {
        Change::CreateTable { name: table, .. }
        | Change::AddColumn { table, .. }
        | Change::AddCheck { table, .. }
        | Change::AddIndex { table, .. } => Some(&table.schema),
        Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnExpression { column, .. } => Some(&column.table.schema),
        // Its own under the partition's schema, its parent's taken back
        // under the parent's, as each is written (#1607 review).
        Change::SetPartitionDefault {
            table, parent, to, ..
        } => Some(if to.is_some() {
            &table.schema
        } else {
            &parent.schema
        }),
        // A module's body is written in its own schema, and a literal in it
        // that the engine casts as it creates the module, an atomic body's or
        // a view's, is bound then (#1599 review).
        Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => Some(id.schema()),
        _ => None,
    }
}

/// The expressions a change creates, each with its [`Step`] and what holds
/// it.
#[allow(clippy::wildcard_enum_match_arm)]
fn expressions_set(change: &Change) -> Vec<(Step, String, String)> {
    let column = |table: &TableName, name: &str, c: &pbps_model::Column| {
        column_expression(c).map(|e| {
            (
                format!("{}'s default or expression", table.column(name)),
                e.to_owned(),
            )
        })
    };
    if let Change::CreateTable { name, table, .. } = change {
        let at = |step: Step| move |(whose, text): (String, String)| (step, whose, text);
        let mut out: Vec<(Step, String, String)> = table
            .columns
            .iter()
            .filter_map(|(n, c)| column(name, n, c))
            .map(at(AT_CREATE))
            .collect();
        // A partition's own defaults, as a column's (#1578).
        out.extend(
            table
                .partition_of
                .iter()
                .flat_map(|of| of.columns.iter())
                .filter_map(|(n, own)| {
                    own.default
                        .clone()
                        .map(|d| (format!("{}'s own default", name.column(n)), d))
                })
                .map(at(AT_CREATE)),
        );
        out.extend(
            table
                .checks
                .iter()
                .map(|(n, c)| (format!("check {n} on {name}"), c.expression.clone()))
                .map(at(CHECKS)),
        );
        // Counted over every index, so `k` matches `relations_brought`.
        out.extend(
            table
                .indexes
                .iter()
                .enumerate()
                .filter(|(_, (_, ix))| ix.holds_expression() || ix.filter.is_some())
                .map(|(k, (n, ix))| {
                    (
                        index_step(k),
                        format!("index {n} on {name}"),
                        index_text(ix),
                    )
                }),
        );
        return out;
    }
    let one: Vec<(String, String)> = match change {
        Change::AddColumn {
            table,
            name,
            column: c,
            ..
        } => column(table, name, c).into_iter().collect(),
        Change::AlterColumnDefault {
            column,
            to: Some(to),
            ..
        } => vec![(format!("{column}'s default"), to.clone())],
        Change::SetPartitionDefault {
            table,
            column,
            to,
            fallback,
            ..
        } => to
            .as_ref()
            .or(fallback.as_ref())
            .map(|text| (format!("{}'s default", table.column(column)), text.clone()))
            .into_iter()
            .collect(),
        Change::AlterColumnExpression { column, to, .. } => {
            vec![(format!("{column}'s generation expression"), to.clone())]
        }
        Change::AddCheck {
            table,
            name,
            constraint,
        } => vec![(
            format!("check {name} on {table}"),
            constraint.expression.clone(),
        )],
        Change::AddIndex {
            table, name, index, ..
        } => vec![(format!("index {name} on {table}"), index_text(index))],
        Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
            vec![(format!("{id}"), module.definition.clone())]
        }
        _ => Vec::new(),
    };
    one.into_iter()
        .map(|(whose, text)| (AT_CREATE, whose, text))
        .collect()
}

/// The white space `regclass` input skips: the scanner's six, vertical tab
/// among them, which `char::is_ascii_whitespace` leaves out. Measured on 18:
/// `E'pg_class\013'` is `pg_class`, and a no-break space is part of the name
/// (#1589 review).
fn is_input_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// A literal that is wholly a relation's name, as `regclass` input reads one:
/// its optional schema and its name. `None` for anything else.
///
/// Measured on 18 (#1589 review), as the input's identifier splitter reads it:
/// - an unquoted part runs to a dot or ASCII white space, so `'app.a-b'`
///   names `"a-b"`;
/// - only ASCII letters fold, so `'app.ixÄ'` names `"ixÄ"` and not `"ixä"`;
/// - white space, [`is_input_space`], is allowed around the dot;
/// - an input of ASCII digits alone is an OID, not a name: `'1259'` is
///   `pg_class`;
/// - `'-'` exactly is OID 0, while `' -'` and `'"-"'` are names
///   (#1589 review);
/// - each part, quoted or not, is cut silently to the identifier limit, in
///   the database's encoding: 64 `a`s name the 63-`a` relation (#1593). See
///   [`truncated`] for a part whose cut that encoding decides.
fn relation_literal(contents: &str) -> Option<(Option<String>, String)> {
    // `-` alone, exactly, is the input's spelling of no relation (OID 0).
    if contents == "-" || (!contents.is_empty() && contents.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let mut parts = Vec::new();
    let mut rest = contents.trim_matches(is_input_space);
    loop {
        let (part, after) = if let Some(quoted) = rest.strip_prefix('"') {
            let mut name = String::new();
            let mut chars = quoted.char_indices();
            let end = loop {
                match chars.next()? {
                    (at, '"') if quoted[at + 1..].starts_with('"') => {
                        name.push('"');
                        chars.next();
                    }
                    (at, '"') => break at + 1,
                    (_, ch) => name.push(ch),
                }
            };
            (name, &quoted[end..])
        } else {
            let end = rest
                .find(|c: char| c == '.' || is_input_space(c))
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            (rest[..end].to_ascii_lowercase(), &rest[end..])
        };
        if part.is_empty() {
            return None;
        }
        parts.push(truncated(part));
        let after = after.trim_start_matches(is_input_space);
        if after.is_empty() {
            break;
        }
        rest = after.strip_prefix('.')?.trim_start_matches(is_input_space);
    }
    match parts.len() {
        1 => Some((None, parts.pop()?)),
        // A catalog's name before the schema is the current database's.
        2 | 3 => {
            let name = parts.pop()?;
            Some((parts.pop(), name))
        }
        _ => None,
    }
}

/// `part` cut to PostgreSQL's identifier limit, as the identifier splitter
/// cuts it, wherever that cut is the same in every server encoding: when the
/// part's first 63 bytes are ASCII, which is one byte in each of them.
///
/// The splitter counts bytes of the database's encoding, which this scan
/// does not know (an offline plan has no target). Measured on 18, 62 `a`s
/// then `é` is 64 bytes in UTF-8 and is cut to the 62 `a`s there, but is 63
/// bytes in a LATIN1 or WIN1252 database and names a relation of that whole
/// name. So a
/// part with other characters inside the limit is kept whole. A cut by
/// UTF-8 would read that literal as a later 62-`a` index and refuse a
/// valid plan (#1627 review). Kept whole, it is longer than any declared
/// name and matches no arrival: at worst a missed reference, the failed
/// apply DEC-1576.1 accepts.
fn truncated(mut part: String) -> String {
    let limit = pbps_pg::MAX_IDENT_BYTES;
    if part.len() > limit && part.as_bytes()[..limit].is_ascii() {
        part.truncate(limit);
    }
    part
}

/// Places a column this plan adds whose default or generation expression
/// names a function the plan creates or rebuilds after that function's
/// create, and whatever may need the column after the column (DEC-1364.1).
/// A table the plan creates whose generated column does so is placed whole,
/// since that column cannot leave the table without moving in its column
/// order (DEC-1274.2). Returns how many it placed, or why no order performs
/// them.
///
/// The column cannot simply follow the last create, as a check does: a
/// module the plan creates may read it, and measured on 18.6 the engine
/// resolves a SQL body's columns at `CREATE FUNCTION`, `BEGIN ATOMIC` or not,
/// and expands a view's or an atomic body's `*` there. So the stretch from the
/// column to the last function create is ordered again, keeping the plan's
/// order wherever nothing says otherwise: a stable topological order in which
/// the column waits for the creates its text names, and every later change
/// that [`needs`] an earlier one keeps following it. Only what may not need
/// the column is let past it.
///
/// A column that a function it calls may read, directly or through what that
/// function needs, closes a cycle no order performs: the engine refuses
/// either create first. So does a row the plan writes into the column, since
/// rows are written before the modules, where a trigger the plan creates
/// cannot fire on them. Such a plan is refused, naming the column and the
/// functions, with the two-plan remedy.
#[allow(clippy::wildcard_enum_match_arm)]
fn after_their_functions(cs: &mut ChangeSet, deps: &ModuleDeps) -> Result<usize, String> {
    let functions = function_creates(cs);
    let Some(last) = functions.iter().map(|(_, at)| *at).max() else {
        return Ok(0);
    };
    // Each such column, with the creates after it that its text names.
    let mut waits: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, p) in cs.changes[..last].iter().enumerate() {
        // A new table's generated column stays in its CREATE TABLE, where
        // its place in the column order is (DEC-1364.1), so the whole table
        // waits instead. So does a new partition's own default (#1578),
        // which has no column of its own to split into an
        // `AlterColumnDefault`.
        let texts: Vec<&str> = match &p.change {
            Change::AddColumn { column, .. } => column_expression(column).into_iter().collect(),
            Change::CreateTable { table, .. } => table
                .columns
                .values()
                .filter_map(|c| c.generated.as_ref().map(|g| g.expression.as_str()))
                .chain(
                    table
                        .partition_of
                        .iter()
                        .flat_map(|of| of.columns.values())
                        .filter_map(|c| c.default.as_deref()),
                )
                .collect(),
            _ => Vec::new(),
        };
        let named: Vec<usize> = functions
            .iter()
            .filter(|(name, at)| {
                *at > i
                    && texts
                        .iter()
                        .any(|text| pbps_pg::generated::may_call(text, name))
            })
            .map(|(_, at)| *at)
            .collect();
        if !named.is_empty() {
            waits.insert(i, named);
        }
    }
    let Some(&start) = waits.keys().next() else {
        return Ok(0);
    };
    let window: Vec<&Change> = cs.changes[start..=last].iter().map(|p| &p.change).collect();
    let n = window.len();
    // What may be held back: the columns, and whatever may need one of them
    // or something already held back. Only these can change places. The rest
    // keep their order among themselves, so `needs` is asked only of pairs
    // with one side here: a schema of many modules is not lexed pair by pair.
    let mut held: BTreeSet<usize> = waits.keys().map(|at| at - start).collect();
    for i in 0..n {
        if !held.contains(&i) && held.range(..i).any(|&j| needs(window[i], window[j], deps)) {
            held.insert(i);
        }
    }
    let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut previous_free: Option<usize> = None;
    for i in 0..n {
        if held.contains(&i) {
            preds[i].extend((0..i).filter(|&j| needs(window[i], window[j], deps)));
        } else {
            preds[i].extend(
                held.range(..i)
                    .filter(|&&j| needs(window[i], window[j], deps)),
            );
            preds[i].extend(previous_free);
            previous_free = Some(i);
        }
    }
    // A column follows the creates its text names. One that may also need the
    // column keeps both edges: that is the cycle refused below.
    for (at, named) in &waits {
        preds[at - start].extend(named.iter().map(|create| create - start));
    }
    // Kahn's algorithm, always taking the earliest ready change, so that what
    // nothing holds back keeps its place.
    let mut done = vec![false; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    while let Some(i) = (0..n).find(|&i| !done[i] && preds[i].iter().all(|&j| done[j])) {
        done[i] = true;
        order.push(i);
    }
    if order.len() < n {
        return Err(cycle(cs, start, &waits, &done, &window, deps));
    }
    let mut taken: Vec<Option<PlannedChange>> = cs.changes.drain(start..=last).map(Some).collect();
    let placed: Vec<PlannedChange> = order
        .iter()
        .map(|&i| taken[i].take().expect("each change is ordered once"))
        .collect();
    cs.changes.splice(start..start, placed);
    Ok(waits.len())
}

/// Why [`after_their_functions`] found no order: the first column it could
/// not place, the functions it calls, and what may need the column.
#[allow(clippy::wildcard_enum_match_arm)]
fn cycle(
    cs: &ChangeSet,
    start: usize,
    waits: &BTreeMap<usize, Vec<usize>>,
    done: &[bool],
    window: &[&Change],
    deps: &ModuleDeps,
) -> String {
    let Some((&at, named)) = waits.iter().find(|(at, _)| !done[**at - start]) else {
        return "a column this plan adds cannot be ordered against the functions it calls; \
                this is a bug in pbps, please report it"
            .into();
    };
    let column = match &cs.changes[at].change {
        Change::AddColumn { table, name, .. } => format!("{table}.{name}"),
        Change::CreateTable { name, table, .. } => table
            .columns
            .iter()
            .find(|(_, c)| c.generated.is_some())
            .map_or_else(
                || name.to_string(),
                |(column, _)| format!("{name}.{column}"),
            ),
        other => format!("{other:?}"),
    };
    let module = |i: usize| cs.changes[i].change.module_id().map(|id| format!("`{id}`"));
    let functions: Vec<String> = named.iter().filter_map(|&i| module(i)).collect();
    let blockers: Vec<String> = (0..window.len())
        .filter(|&i| !done[i] && start + i != at && needs(window[i], window[at - start], deps))
        .filter_map(|i| match window[i] {
            Change::InsertRow { table, .. } | Change::UpdateRow { table, .. } => {
                Some(format!("a row this plan writes into `{table}`"))
            }
            _ => module(start + i),
        })
        .collect();
    let blockers = if blockers.is_empty() {
        "something those functions need".to_owned()
    } else {
        blockers.join(", ")
    };
    let (them, their) = if functions.len() == 1 {
        ("it", "its")
    } else {
        ("them", "their")
    };
    let functions = functions.join(", ");
    format!(
        "the column `{column}` this plan adds calls {functions}, which the plan creates or \
         rebuilds, so it has to be added after {them}. But {blockers} may need the column, so \
         has to come after it, and {them} cannot be created before that. No order performs \
         both. Split it into two plans: create {functions} in a plan of {their} own first, or \
         add the column without the call first and give it the call in a second plan"
    )
}

/// Whether `later` may need `earlier`, a change ahead of it in the plan, to
/// have run first: what [`after_their_functions`] may not reorder. Anything
/// not shown independent here is held in order.
///
/// - A column added: a module that may read it ([`reads_column`]), a row that
///   writes it or takes its default, and another change of its table that
///   names it. A module drop and a row delete never need a new column.
/// - A module created: a module that names it, in code or in a literal an
///   OID-alias type may read (`may_call`), is attached to it, shares
///   its name (overloads are ordered by `depends_on:` alone, DECISIONS 212) or
///   declares it in `depends_on:`.
/// - Any other change of a table: a module that names the table, and a change
///   of the same table or one referencing it.
/// - A row write: everything after it. A trigger the plan creates must not
///   fire on a row the plan writes before the modules.
#[allow(clippy::wildcard_enum_match_arm)]
fn needs(later: &Change, earlier: &Change, deps: &ModuleDeps) -> bool {
    match earlier {
        Change::AddColumn { table, name, .. } => match later {
            Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                reads_column(id, &module.definition, table, name)
            }
            Change::DropModule { .. } | Change::DeleteRow { .. } => false,
            Change::InsertRow {
                table: t,
                row,
                defaults,
                ..
            } => t == table && (row.get(name).is_some() || defaults.contains_key(name)),
            Change::UpdateRow {
                table: t,
                columns,
                unchanged,
                ..
            } => t == table && (columns.contains_key(name) || unchanged.contains_key(name)),
            other => names_column(other, table, name),
        },
        Change::CreateModule { id: before, .. } | Change::AlterModule { id: before, .. } => {
            match later {
                Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                    let named = before
                        .referenced_name()
                        .is_some_and(|n| pbps_pg::generated::may_call(&module.definition, &n.name));
                    let attached = id.attached_to().is_some()
                        && id.attached_to() == before.referenced_name().as_ref();
                    let sibling = id.referenced_name().is_some()
                        && id.referenced_name() == before.referenced_name();
                    let declared = deps.get(id).is_some_and(|d| d.contains(before));
                    named || attached || sibling || declared
                }
                _ => true,
            }
        }
        Change::InsertRow { .. } | Change::UpdateRow { .. } | Change::DeleteRow { .. } => true,
        // Two new tables are independent unless the later one names the
        // earlier: a table held back for its generation expression must not
        // drag every later new table behind it, or a function that reads one
        // of those closes a cycle no order has (#1303 review).
        Change::CreateTable { name: made, .. } if matches!(later, Change::CreateTable { .. }) => {
            matches!(later, Change::CreateTable { table, .. } if names_new_table(table, made))
        }
        other => match (later, other.table()) {
            (Change::CreateModule { id, module } | Change::AlterModule { id, module }, Some(t)) => {
                names_table(id, &module.definition, t)
            }
            (later, Some(t)) => touches(later, t),
            _ => true,
        },
    }
}

/// Whether a new table's definition may name another new table `made`: a
/// column of its row type, a foreign key to it, or an expression whose text
/// may name it, read by the engine's lexer as a module's is. A string
/// literal counts too: an OID-alias literal such as `'app.a'::regclass` is
/// resolved as the default or check is installed (measured on 16 and 18),
/// as `may_call` reads a function's (DEC-1364.1).
fn names_new_table(table: &pbps_model::Table, made: &TableName) -> bool {
    let named = |text: &str| pbps_pg::generated::may_call(text, &made.name);
    let typed = |base: &str| base == made.name || base == made.to_string();
    table.columns.values().any(|column| {
        typed(&column.ty.base)
            || column.default.as_deref().is_some_and(named)
            || column
                .generated
                .as_ref()
                .is_some_and(|g| named(&g.expression))
    }) || table
        .foreign_keys
        .values()
        .any(|key| &key.references_table == made)
        || table.checks.values().any(|check| named(&check.expression))
        // A partition's own default, as a column's (#1578).
        || table
            .partition_of
            .iter()
            .flat_map(|of| of.columns.values())
            .any(|own| own.default.as_deref().is_some_and(named))
        // A partition names its parent, which its `CREATE … PARTITION OF`
        // needs standing: a held parent takes its partitions with it (#1586).
        || table
            .partition_of
            .as_ref()
            .is_some_and(|of| &of.parent == made)
        || table.indexes.values().any(|index| {
            index.filter.as_deref().is_some_and(named)
                || index.columns.iter().any(|column| {
                    matches!(&column.key, pbps_model::IndexKey::Expression(text) if named(text))
                })
        })
}

/// Whether a module may read column `column` of `table`: its definition names
/// the column or the table. A module naming the table may take its whole
/// column shape without naming a column, in more spellings than a list could
/// hold: measured on 18.6, a view's or an atomic body's `*`, a `NATURAL` join,
/// and a positional `INSERT INTO t VALUES (…)`, atomic or not, are all bound
/// to the columns there when the module is created (DEC-1364.1).
fn reads_column(id: &ModuleId, definition: &str, table: &TableName, column: &str) -> bool {
    pbps_pg::generated::may_read(definition, column) || names_table(id, definition, table)
}

/// Whether a module may name `table`: its definition holds the table's bare
/// name, or it is a trigger on it.
fn names_table(id: &ModuleId, definition: &str, table: &TableName) -> bool {
    id.attached_to()
        .is_some_and(|on| on.schema == table.schema && on.name == table.name)
        || pbps_pg::generated::may_name(definition, &table.name)
}

/// Whether a change of a table may name the new column `column` of `table`.
/// A change of another table names it only as a foreign key's target, or as
/// a new partition's own entry on its parent's column.
#[allow(clippy::wildcard_enum_match_arm)]
fn names_column(change: &Change, table: &TableName, column: &str) -> bool {
    let named = |text: &str| pbps_pg::generated::may_read(text, column);
    let listed = |columns: &[String]| columns.iter().any(|c| c == column);
    if let Change::AddForeignKey { constraint, .. } = change
        && &constraint.references_table == table
        && listed(&constraint.references_columns)
    {
        return true;
    }
    // A partition created under the table sets its own default or NOT NULL,
    // and builds its own indexes and checks, on the parent's columns right
    // after its `CREATE`, so it needs each column one of them names (#1692
    // review).
    if let Change::CreateTable { table: t, .. } = change
        && let Some(of) = &t.partition_of
        && &of.parent == table
        && (of.columns.contains_key(column)
            || t.indexes.values().any(|index| {
                index.columns.iter().any(|c| c.key.column() == Some(column))
                    || listed(&index.include)
                    || named(&index_text(index))
            })
            || t.checks.values().any(|check| named(&check.expression)))
    {
        return true;
    }
    // A standing partition's own default, NOT NULL, index or check on the
    // parent's column needs it too (#1692 review). Only a default says its
    // parent; for the others this pass, which has no schema, cannot tell a
    // partition from another table, so a change of another table naming a
    // column of that name waits for it as well. That only holds it longer,
    // short of a cycle, which is refused with its two-plan remedy.
    let partition_own = match change {
        Change::SetPartitionDefault { parent, .. } => parent == table,
        Change::SetPartitionNotNull { .. } | Change::AddIndex { .. } | Change::AddCheck { .. } => {
            true
        }
        _ => false,
    };
    if change.table() != Some(table) && !partition_own {
        return change.table().is_none();
    }
    match change {
        Change::AddColumn { column: c, .. } => column_expression(c).is_some_and(named),
        Change::AlterColumnDefault { column: c, to, .. } => {
            c.name == column || to.as_deref().is_some_and(named)
        }
        Change::SetPartitionDefault {
            column: c,
            to,
            fallback,
            ..
        } => c == column || to.as_deref().or(fallback.as_deref()).is_some_and(named),
        Change::SetPartitionNotNull { column: c, .. } => c == column,
        Change::AlterColumnExpression { column: c, to, .. } => c.name == column || named(to),
        Change::AlterColumnType { column: c, .. }
        | Change::AlterColumnNullability { column: c, .. }
        | Change::SetColumnDeprecated { column: c, .. } => c.name == column,
        Change::AddCheck { constraint, .. } => named(&constraint.expression),
        Change::AddIndex { index, .. } => {
            index.columns.iter().any(|c| c.key.column() == Some(column))
                || listed(&index.include)
                || named(&index_text(index))
        }
        Change::AddUnique { constraint, .. } => listed(&constraint.columns),
        Change::SetPrimaryKey { to, .. } => to.as_ref().is_none_or(|pk| listed(&pk.columns)),
        Change::AddForeignKey { constraint, .. } => listed(&constraint.columns),
        _ => true,
    }
}

/// Whether a change acts on `table`, or on a table referencing it. A change
/// that names no table is held to anything.
#[allow(clippy::wildcard_enum_match_arm)]
fn touches(change: &Change, table: &TableName) -> bool {
    match change {
        Change::AddForeignKey {
            table: t,
            constraint,
            ..
        } => t == table || &constraint.references_table == table,
        Change::CreateTable { .. } => true,
        other => other.table().is_none_or(|t| t == table),
    }
}

/// Every dependent of a module this plan drops that the plan does not remove
/// before that drop, named: the apply's question, asked of the saved plan as
/// it stands. Empty when the plan accounts for everything — which is what
/// [`weave`] leaves it in.
/// The generated columns whose expression change releases them from a module
/// this plan drops, named as that change names them (DEC-1168.1). `weave`
/// puts the module's drop after each such change; this is how
/// [`after_the_rebuilds`] knows not to move it past that drop.
pub(crate) fn released(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
) -> BTreeSet<ColumnRef> {
    let mut out = BTreeSet::new();
    for (root, _) in dropped_modules(cs) {
        for d in found.get(&root).into_iter().flatten() {
            if !matches!(
                d.holds,
                Holds::TablePart {
                    part: Part::Generated(_),
                    ..
                }
            ) {
                continue;
            }
            if let Some(i) = find(&cs.changes, &d.holds, removes)
                && let Change::AlterColumnExpression { column, .. } = &cs.changes[i].change
            {
                out.insert(column.clone());
            }
        }
    }
    out
}

pub(crate) fn unaccounted(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (root, _) in dropped_modules(cs) {
        let (Some(deps), Some(at)) = (found.get(&root), span(&cs.changes, &root)) else {
            continue;
        };
        for d in deps {
            let removed_first = find(&cs.changes, &d.holds, removes)
                .or_else(|| find(&cs.changes, &d.holds, removes_with_its_owner))
                .is_some_and(|i| i < at.drop_at)
                || taken_by_its_parent(&cs.changes, &d.holds, at.drop_at);
            if !removed_first {
                out.push(format!("{} depends on `{root}`", d.described));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{CheckConstraint, Column, Module, Table, TableName, Uid, UidKind};

    fn pg() -> Box<dyn Dialect> {
        Box::new(pbps_pg::Postgres::new())
    }

    fn id(s: &str) -> ModuleId {
        s.parse().unwrap()
    }

    fn module(kind: ModuleKind, definition: &str) -> Module {
        Module {
            kind,
            description: None,
            definition: definition.into(),
        }
    }

    fn view(name: &str) -> Dependent {
        Dependent {
            described: format!("view {name}"),
            holds: Holds::Module(id(name)),
        }
    }

    fn part(part: Part, described: &str) -> Dependent {
        Dependent {
            described: described.into(),
            holds: Holds::TablePart {
                table: TableName::new("app", "t"),
                part,
            },
        }
    }

    /// A function `app.f(integer)`, a table whose check and default call it,
    /// and three views, each over the one before.
    fn declared() -> (Schema, IdsFile) {
        let mut s = Schema::default();
        s.modules.insert(
            id("app.f(integer)"),
            module(
                ModuleKind::Function,
                "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$",
            ),
        );
        for (name, over) in [
            ("app.v0", "app.t"),
            ("app.v1", "app.v0"),
            ("app.v2", "app.v1"),
        ] {
            s.modules.insert(
                id(name),
                module(ModuleKind::View, &format!("SELECT id FROM {over}")),
            );
        }
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        let mut n = Column::new("integer".parse().unwrap());
        n.default = Some("app.f(1)".into());
        t.columns.insert("n".into(), n);
        t.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "app.f(id) >= 0".into(),
            },
        );
        s.tables.insert(TableName::new("app", "t"), t);
        let mut ids = IdsFile::default();
        ids.columns.insert(
            Uid::derived(UidKind::Column, "app.t.n", 0),
            ColumnRef::new(TableName::new("app", "t"), "n"),
        );
        (s, ids)
    }

    /// `after_the_rebuilds` with no `depends_on:` edges, for an order it
    /// finds.
    fn rebuilds(cs: &mut ChangeSet, released: &BTreeSet<ColumnRef>) -> usize {
        after_the_rebuilds(cs, released, &ModuleDeps::new()).unwrap()
    }

    fn plan(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }

    // A test rendering: every other change prints as itself.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn rendered(cs: &ChangeSet) -> Vec<String> {
        cs.changes
            .iter()
            .map(|p| match &p.change {
                Change::DropModule { id, .. } => format!("drop {id}"),
                Change::CreateModule { id, .. } => format!("create {id}"),
                Change::AlterModule { id, .. } => format!("alter {id}"),
                Change::DropCheck { name, .. } => format!("drop check {name}"),
                Change::AddCheck { name, .. } => format!("add check {name}"),
                Change::AlterColumnDefault { column, to, .. } => {
                    format!("default {} -> {to:?}", column.name)
                }
                Change::SetPartitionDefault {
                    table,
                    column,
                    to,
                    fallback,
                    ..
                } => format!(
                    "default {}.{column} -> {to:?} else {fallback:?}",
                    table.name
                ),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// #687: the `PUBLIC` decisions are set aside while the passes reorder
    /// and come back directly after their routine's last `CREATE` or
    /// `ALTER`. A decision on a routine the plan does not touch goes ahead of
    /// the roles and grants instead.
    #[test]
    #[allow(clippy::wildcard_enum_match_arm)]
    fn public_decisions_return_beside_their_routines_final_create() {
        let decision = |routine: &str| Change::PublicExecution {
            routine: routine.parse().unwrap(),
            access: pbps_model::PublicAccess::Revoked,
            origin: pbps_model::RoutineOrigin::Created,
        };
        let f = module(
            ModuleKind::Function,
            "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$",
        );
        let create = |name: &str| Change::CreateModule {
            id: id(name),
            module: Box::new(f.clone()),
        };
        let role = Change::CreateRole {
            uid: Uid::generate(UidKind::Role),
            name: "reader".into(),
        };
        let mut cs = plan(vec![
            create("app.f(integer)"),
            decision("app.f(integer)"),
            create("app.g(integer)"),
            decision("app.g(integer)"),
            decision("app.h(integer)"),
            role.clone(),
        ]);
        let decisions = take_public_execution(&mut cs);
        assert_eq!(decisions.len(), 3);
        assert!(
            cs.changes
                .iter()
                .all(|p| !matches!(p.change, Change::PublicExecution { .. }))
        );
        // A pass moved `g` ahead of `f` and rebuilt `f` once more after it.
        cs.changes.swap(0, 1);
        cs.changes.insert(
            2,
            PlannedChange::new(Change::AlterModule {
                id: id("app.f(integer)"),
                module: Box::new(f.clone()),
            }),
        );
        settle_public_execution(&mut cs, decisions);
        let order: Vec<String> = cs
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::PublicExecution { routine, .. } => format!("public {routine}"),
                Change::CreateRole { name, .. } => format!("role {name}"),
                other => rendered(&ChangeSet {
                    changes: vec![PlannedChange::new(other.clone())],
                })
                .remove(0),
            })
            .collect();
        assert_eq!(
            order,
            [
                "create app.g(integer)",
                "public app.g(integer)",
                "create app.f(integer)",
                "alter app.f(integer)",
                "public app.f(integer)",
                "public app.h(integer)",
                "role reader",
            ]
        );
    }

    fn alter(s: &Schema, name: &str) -> Change {
        Change::AlterModule {
            id: id(name),
            module: Box::new(s.modules[&id(name)].clone()),
        }
    }

    // A test rendering: every other change prints as `rendered` prints it.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn names(cs: &ChangeSet) -> Vec<String> {
        cs.changes
            .iter()
            .map(|p| match &p.change {
                Change::AddIndex { name, .. } => format!("add index {name}"),
                Change::InsertRow { .. } => "insert".to_owned(),
                _ => rendered(&ChangeSet {
                    changes: vec![p.clone()],
                })
                .remove(0),
            })
            .collect()
    }

    fn add_check(name: &str) -> Change {
        check_of(name, "app.f(id) >= 0")
    }

    fn check_of(name: &str, expression: &str) -> Change {
        Change::AddCheck {
            table: TableName::new("app", "t"),
            name: name.into(),
            constraint: CheckConstraint {
                expression: expression.into(),
            },
        }
    }

    fn add_index(name: &str, filter: Option<&str>) -> Change {
        Change::AddIndex {
            table: TableName::new("app", "t"),
            name: name.into(),
            index: Box::new(pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: filter.is_none(),
                filter: filter.map(Into::into),
                method: Default::default(),
                storage_parameters: Default::default(),
            }),
            clustered: false,
        }
    }

    fn set_default(table: &str) -> Change {
        default_of(table, "app.f(1)")
    }

    fn default_of(table: &str, to: &str) -> Change {
        Change::AlterColumnDefault {
            uid: Uid::derived(UidKind::Column, &format!("app.{table}.n"), 0),
            column: ColumnRef::new(TableName::new("app", table), "n"),
            from: None,
            to: Some(to.into()),
        }
    }

    /// #942: what the plan itself adds, in the differ's order ahead of the
    /// modules, goes after the last function the plan creates when it
    /// rebuilds one. Relative order is kept; an unfiltered unique index stays.
    #[test]
    fn additions_that_can_call_a_rebuilt_function_follow_its_create() {
        let (s, _) = declared();
        let mut cs = plan(vec![
            set_default("t"),
            add_check("ck_a"),
            add_index("ix_unique", None),
            add_index("ix_filtered", Some("app.f(id) > 0")),
            add_check("ck_b"),
            alter(&s, "app.f(integer)"),
            alter(&s, "app.v0"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 4);
        assert_eq!(
            names(&cs),
            [
                "add index ix_unique",
                "alter app.f(integer)",
                "default n -> Some(\"app.f(1)\")",
                "add check ck_a",
                "add index ix_filtered",
                "add check ck_b",
                "alter app.v0",
            ]
        );
    }

    /// A generation expression binds the functions it calls, as a default
    /// does: a changed expression follows the rebuilt function's create, and
    /// so does a column added with one, since the function does not read the
    /// column (DEC-1168.1, DEC-1364.1). A column added without one stays.
    #[test]
    fn generation_expressions_follow_a_rebuilt_functions_create() {
        let (s, _) = declared();
        let t = TableName::new("app", "t");
        let mut generated = pbps_model::Column::new("integer".parse().unwrap());
        generated.generated = Some(pbps_model::Generated {
            expression: "app.f(id)".into(),
            stored: true,
        });
        let add = |name: &str, column: pbps_model::Column| Change::AddColumn {
            uid: "c_a1b2c3".parse().unwrap(),
            table: t.clone(),
            name: name.into(),
            column: Box::new(column),
        };
        let tighten = |column: &str| Change::AlterColumnNullability {
            uid: "c_m3n4p5".parse().unwrap(),
            column: t.column(column),
            ty: "integer".parse().unwrap(),
            to_nullable: false,
            collation: None,
        };
        let unique = |column: &str| Change::AddUnique {
            table: t.clone(),
            name: format!("uq_{column}"),
            constraint: pbps_model::UniqueConstraint {
                columns: vec![column.to_owned()],
                storage_parameters: Default::default(),
            },
            clustered: false,
        };
        let mut cs = plan(vec![
            Change::AlterColumnExpression {
                uid: "c_d4e5f6".parse().unwrap(),
                column: t.column("n"),
                from: "id * 2".into(),
                to: "app.f(id)".into(),
            },
            // `n`'s new NOT NULL and key follow its expression; `other`'s stay.
            tighten("n"),
            tighten("other"),
            unique("n"),
            unique("other"),
            add("g", generated),
            add("plain", pbps_model::Column::new("integer".parse().unwrap())),
            alter(&s, "app.f(integer)"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 4);
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let rebuilt = at(&|c| matches!(c, Change::AlterModule { .. }));
        let expression = at(&|c| matches!(c, Change::AlterColumnExpression { .. }));
        let generated_add = at(&|c| matches!(c, Change::AddColumn { name, .. } if name == "g"));
        let plain_add = at(&|c| matches!(c, Change::AddColumn { name, .. } if name == "plain"));
        assert!(rebuilt < expression, "{:?}", names(&cs));
        assert!(
            rebuilt < generated_add && plain_add < rebuilt,
            "{:?}",
            names(&cs)
        );
        let tightened = |name: &str| {
            at(
                &|c| matches!(c, Change::AlterColumnNullability { column, .. } if column.name == name),
            )
        };
        assert!(expression < tightened("n"), "{:?}", names(&cs));
        assert!(tightened("other") < rebuilt, "{:?}", names(&cs));
        let keyed = |name: &str| {
            at(&|c| matches!(c, Change::AddUnique { name: n, .. } if n == &format!("uq_{name}")))
        };
        assert!(expression < keyed("n"), "{:?}", names(&cs));
        assert!(keyed("other") < rebuilt, "{:?}", names(&cs));
    }

    /// An expression change that releases a generated column from a module
    /// this plan drops keeps its place, which `weave` put the drop after, even
    /// with a function created later: moved past the creates, it would follow
    /// the drop the engine refuses without it (DEC-1168.1). One that releases
    /// nothing still follows the rebuilt function.
    #[test]
    fn a_releasing_expression_change_stays_ahead_of_the_drop() {
        let (s, _) = declared();
        let t = TableName::new("app", "t");
        let recompute = |column: &str, to: &str| Change::AlterColumnExpression {
            uid: "c_d4e5f6".parse().unwrap(),
            column: t.column(column),
            from: "app.f(id)".into(),
            to: to.into(),
        };
        let mut cs = plan(vec![
            recompute("n", "id * 2"),
            recompute("other", "app.f(id) * 2"),
            alter(&s, "app.f(integer)"),
        ]);
        let released = BTreeSet::from([t.column("n")]);
        assert_eq!(rebuilds(&mut cs, &released), 1);
        let at = |name: &str| {
            cs.changes
                .iter()
                .position(|p| matches!(&p.change, Change::AlterColumnExpression { column, .. } if column.name == name))
                .unwrap()
        };
        let rebuilt = cs
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::AlterModule { .. }))
            .unwrap();
        assert!(
            at("n") < rebuilt && rebuilt < at("other"),
            "{:?}",
            names(&cs)
        );
    }

    /// #1024: a procedure that becomes a function is dropped as a procedure
    /// and created as a function, and what calls the new function follows its
    /// create. #1047: the reverse still counts, for another function the same
    /// revision creates.
    #[test]
    fn a_routine_changing_kind_to_or_from_a_function_counts_as_a_function_rebuild() {
        let routine = id("app.f(integer)");
        let as_kind = |kind| {
            Box::new(module(
                kind,
                "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$",
            ))
        };
        let mut cs = plan(vec![
            Change::DropModule {
                id: routine.clone(),
                kind: ModuleKind::Procedure,
            },
            add_check("ck"),
            Change::CreateModule {
                id: routine.clone(),
                module: as_kind(ModuleKind::Function),
            },
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert_eq!(
            names(&cs),
            [
                "drop app.f(integer)",
                "create app.f(integer)",
                "add check ck"
            ]
        );

        // #1047: a function that becomes a procedure, beside a new function
        // `g` the check calls. The check follows `g`'s create.
        let mut cs = plan(vec![
            Change::DropModule {
                id: routine.clone(),
                kind: ModuleKind::Function,
            },
            check_of("ck", "app.g(id) >= 0"),
            Change::CreateModule {
                id: routine.clone(),
                module: as_kind(ModuleKind::Procedure),
            },
            Change::CreateModule {
                id: id("app.g(integer)"),
                module: as_kind(ModuleKind::Function),
            },
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert_eq!(
            names(&cs),
            [
                "drop app.f(integer)",
                "create app.f(integer)",
                "create app.g(integer)",
                "add check ck"
            ]
        );

        // Negative: a procedure rebuilt as a procedure is no function
        // rebuild, and a check beside it stays where the differ put it.
        let mut cs = plan(vec![
            Change::DropModule {
                id: routine.clone(),
                kind: ModuleKind::Procedure,
            },
            add_check("ck"),
            Change::CreateModule {
                id: routine,
                module: as_kind(ModuleKind::Procedure),
            },
        ]);
        let before = names(&cs);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 0);
        assert_eq!(names(&cs), before);
    }

    /// #1027: a table the plan creates ahead of a rebuilt function is split
    /// into the table and its expression-bearing parts, which then follow the
    /// function. An unfiltered index stays in the table; a table whose rows
    /// the plan writes keeps its default; no rebuild splits nothing.
    #[test]
    fn a_new_tables_expressions_are_split_out_to_follow_a_rebuilt_function() {
        let (s, _) = declared();
        let n = TableName::new("app", "n");
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        let mut v = Column::new("integer".parse().unwrap());
        v.default = Some("app.f(1)".into());
        t.columns.insert("v".into(), v);
        t.checks.insert(
            "ck_n".into(),
            CheckConstraint {
                expression: "app.f(id) >= 0".into(),
            },
        );
        for (name, filter) in [("ix_f", Some("app.f(id) > 0")), ("ix_u", None)] {
            t.indexes.insert(
                name.into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("id".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: Vec::new(),
                    unique: filter.is_none(),
                    filter: filter.map(Into::into),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
        }
        let mut ids = IdsFile::default();
        ids.columns.insert(
            Uid::derived(UidKind::Column, "app.n.v", 0),
            ColumnRef::new(n.clone(), "v"),
        );
        let create = Change::CreateTable {
            uid: Uid::derived(UidKind::Table, "app.n", 0),
            name: n.clone(),
            table: Box::new(t.clone()),
        };
        #[allow(clippy::wildcard_enum_match_arm)]
        let table_of = |cs: &ChangeSet| match &cs.changes[0].change {
            Change::CreateTable { table, .. } => (**table).clone(),
            other => panic!("{other:?}"),
        };

        let mut cs = plan(vec![create.clone(), alter(&s, "app.f(integer)")]);
        assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), Ok(3));
        let left = table_of(&cs);
        assert!(left.checks.is_empty());
        assert_eq!(left.indexes.keys().collect::<Vec<_>>(), ["ix_u"]);
        assert_eq!(left.columns["v"].default, None);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 3);
        assert_eq!(
            names(&cs)[1..],
            [
                "alter app.f(integer)",
                "add check ck_n",
                "add index ix_f",
                "default v -> Some(\"app.f(1)\")",
            ]
        );

        // As a partitioned parent with a new partition declaring its own
        // default on `v`: the parent's, set after the function, overwrites
        // it, so the partition's own is set again right after (#1588). One
        // overriding another column needs nothing.
        let mut parent = t.clone();
        parent.partition_by = Some(pbps_model::PartitionBy {
            columns: vec!["id".into()],
        });
        let p1 = TableName::new("app", "n_1");
        let partition = |column: &str| Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: n.clone(),
                bound: pbps_model::PartitionBound::Default,
                columns: [(
                    column.to_owned(),
                    pbps_model::PartitionColumn {
                        default: Some("7".into()),
                        not_null: false,
                    },
                )]
                .into(),
            }),
            ..Table::default()
        };
        let mut ids = ids.clone();
        ids.tables
            .insert(Uid::derived(UidKind::Table, "app.n_1", 0), p1.clone());
        for (column, again) in [("v", true), ("id", false)] {
            let declared = Schema {
                tables: [(n.clone(), parent.clone()), (p1.clone(), partition(column))].into(),
                ..Schema::default()
            };
            let mut cs = plan(vec![
                Change::CreateTable {
                    uid: Uid::derived(UidKind::Table, "app.n", 0),
                    name: n.clone(),
                    table: Box::new(parent.clone()),
                },
                Change::CreateTable {
                    uid: Uid::derived(UidKind::Table, "app.n_1", 0),
                    name: p1.clone(),
                    table: Box::new(partition(column)),
                },
                alter(&s, "app.f(integer)"),
            ]);
            assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), Ok(3));
            rebuilds(&mut cs, &BTreeSet::new());
            assert_eq!(
                after_their_parents_defaults(&mut cs, &declared, &[&ids], pg().as_ref()),
                Ok(usize::from(again)),
                "{column}"
            );
            let parents = cs.changes.iter().position(|p| {
                matches!(&p.change, Change::AlterColumnDefault { column, .. } if column.table == n)
            });
            let own = cs.changes.iter().position(|p| {
                matches!(&p.change, Change::SetPartitionDefault { table, column: c, to: Some(to), .. }
                    if *table == p1 && c == "v" && to == "7")
            });
            if again {
                assert_eq!(own, parents.map(|i| i + 1), "{:?}", cs.changes);
            } else {
                assert_eq!(own, None, "{:?}", cs.changes);
            }
        }

        // A row that omits `v` takes its default: the default stays for it.
        let insert = Change::InsertRow {
            table: n.clone(),
            key_column: "id".into(),
            identity_key: false,
            key: pbps_model::RowKey::from("1"),
            row: pbps_model::Row::default(),
            defaults: BTreeMap::from([("v".to_owned(), "app.f(1)".to_owned())]),
            types: BTreeMap::new(),
        };
        let mut cs = plan(vec![create.clone(), insert, alter(&s, "app.f(integer)")]);
        assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), Ok(2));
        assert_eq!(
            table_of(&cs).columns["v"].default.as_deref(),
            Some("app.f(1)")
        );

        // No function rebuilt: the table is left as the differ wrote it.
        let mut cs = plan(vec![create]);
        assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), Ok(0));
        assert_eq!(table_of(&cs), t);
    }

    /// A function new to the database, with nothing rebuilt, is a boundary
    /// too: a check, a default and a generation expression calling it follow
    /// its `CREATE FUNCTION` (DEC-942.1). A column added with a generation
    /// expression that calls nothing the plan creates stays ahead of it, where
    /// the function may read the column (DEC-1364.1). A new procedure is no
    /// boundary: nothing calls one from an expression.
    #[test]
    fn what_may_call_a_new_function_follows_its_create() {
        let new_routine = |kind| Change::CreateModule {
            id: id("app.h(integer)"),
            module: Box::new(module(
                kind,
                "(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$",
            )),
        };
        let recompute = Change::AlterColumnExpression {
            uid: "c_d4e5f6".parse().unwrap(),
            column: TableName::new("app", "t").column("g"),
            from: "id * 2".into(),
            to: "app.h(id)".into(),
        };
        let mut generated = pbps_model::Column::new("integer".parse().unwrap());
        generated.generated = Some(pbps_model::Generated {
            expression: "id * 2".into(),
            stored: true,
        });
        let mut cs = plan(vec![
            check_of("ck", "app.h(id) >= 0"),
            default_of("t", "app.h(1)"),
            recompute.clone(),
            Change::AddColumn {
                uid: "c_a1b2c3".parse().unwrap(),
                table: TableName::new("app", "t"),
                name: "read".into(),
                column: Box::new(generated),
            },
            new_routine(ModuleKind::Function),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 3);
        assert!(
            matches!(cs.changes[0].change, Change::AddColumn { .. })
                && matches!(cs.changes[1].change, Change::CreateModule { .. }),
            "{:?}",
            names(&cs)
        );
        let mut cs = plan(vec![
            check_of("ck", "app.h(id) >= 0"),
            recompute,
            new_routine(ModuleKind::Procedure),
        ]);
        let before = names(&cs);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 0);
        assert_eq!(names(&cs), before);
    }

    fn routine(name: &str, body: &str) -> Change {
        Change::CreateModule {
            id: id(name),
            module: Box::new(module(
                ModuleKind::Function,
                &format!("() RETURNS integer LANGUAGE sql IMMUTABLE AS $$ {body} $$"),
            )),
        }
    }

    fn new_view(name: &str, definition: &str) -> Change {
        Change::CreateModule {
            id: id(name),
            module: Box::new(module(ModuleKind::View, definition)),
        }
    }

    /// A column `app.t.<name>` added with a generation expression, or with
    /// a default when `generated` is false.
    fn add_column(name: &str, expression: &str, generated: bool) -> Change {
        let mut column = Column::new("integer".parse().unwrap());
        if generated {
            column.generated = Some(pbps_model::Generated {
                expression: expression.into(),
                stored: true,
            });
        } else {
            column.default = Some(expression.into());
        }
        Change::AddColumn {
            uid: Uid::derived(UidKind::Column, &format!("app.t.{name}"), 0),
            table: TableName::new("app", "t"),
            name: name.into(),
            column: Box::new(column),
        }
    }

    /// DEC-1364.1: an addition whose text names no function the plan creates
    /// or rebuilds keeps the differ's place, beside a rebuilt function; and a
    /// new table keeps such parts inside its `CREATE TABLE`.
    #[test]
    fn an_addition_naming_no_created_function_keeps_its_place() {
        let (s, _) = declared();
        let mut cs = plan(vec![
            check_of("ck", "id > 0"),
            default_of("t", "0"),
            add_index("ix_filtered", Some("id > 0")),
            Change::AlterColumnExpression {
                uid: "c_d4e5f6".parse().unwrap(),
                column: TableName::new("app", "t").column("g"),
                from: "id * 2".into(),
                to: "id * 3".into(),
            },
            add_column("h", "id * 2", true),
            // A longer name is no call, in code or in a literal.
            check_of("ck_longer", "note <> 'app.ff(1)' AND app.ff(id) > 0"),
            alter(&s, "app.f(integer)"),
        ]);
        let before = names(&cs);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 0);
        assert_eq!(names(&cs), before);

        let n = TableName::new("app", "n");
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        t.checks.insert(
            "ck_calls".into(),
            CheckConstraint {
                expression: "app.f(id) >= 0".into(),
            },
        );
        t.checks.insert(
            "ck_plain".into(),
            CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        let mut cs = plan(vec![
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, "app.n", 0),
                name: n,
                table: Box::new(t),
            },
            alter(&s, "app.f(integer)"),
        ]);
        assert_eq!(split_new_tables(&mut cs, &[], pg().as_ref()), Ok(1));
        #[allow(clippy::wildcard_enum_match_arm)]
        match &cs.changes[0].change {
            Change::CreateTable { table, .. } => {
                assert_eq!(table.checks.keys().collect::<Vec<_>>(), ["ck_plain"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(names(&cs)[1], "add check ck_calls");
    }

    /// DEC-1364.1: a column added with an expression calling a new function
    /// follows that function's create, and a module that reads the column
    /// follows the column, though the differ put it ahead of the function. A
    /// column calling nothing new, and a module reading it, keep their
    /// places.
    #[test]
    fn a_column_calling_a_new_function_follows_it_and_its_readers_follow_the_column() {
        for generated in [true, false] {
            let mut cs = plan(vec![
                add_column("g", "app.f(1)", generated),
                add_column("plain", "id * 2", true),
                routine("app.a_reader()", "SELECT g FROM app.t LIMIT 1"),
                routine("app.f(integer)", "SELECT 1"),
                routine("app.z()", "SELECT plain FROM app.t LIMIT 1"),
            ]);
            assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
            let at =
                |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
            let column =
                |name: &str| at(&|c| matches!(c, Change::AddColumn { name: n, .. } if n == name));
            let created = |name: &str| {
                at(&|c| matches!(c, Change::CreateModule { id: i, .. } if *i == id(name)))
            };
            let order = names(&cs);
            assert!(created("app.f(integer)") < column("g"), "{order:?}");
            assert!(column("g") < created("app.a_reader()"), "{order:?}");
            assert_eq!(column("plain"), 0, "{order:?}");
            assert!(created("app.f(integer)") < created("app.z()"), "{order:?}");
        }
    }

    /// A new table whose generated column calls a new function cannot have
    /// the column split out without changing its column order, so the whole
    /// table follows the function, and a module naming the table follows the
    /// table. A new table generating from nothing new does not wait.
    #[test]
    fn a_new_table_generating_from_a_new_function_follows_it_whole() {
        let table = |name: &str, expression: &str| {
            let mut t = Table::default();
            t.columns.insert(
                "id".into(),
                Column::new("integer".parse().unwrap()).not_null(),
            );
            let mut g = Column::new("integer".parse().unwrap());
            g.generated = Some(pbps_model::Generated {
                expression: expression.into(),
                stored: true,
            });
            t.columns.insert("g".into(), g);
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, name, 0),
                name: name.parse().unwrap(),
                table: Box::new(t),
            }
        };
        let mut cs = plan(vec![
            table("app.n", "app.f(id)"),
            table("app.plain", "id * 2"),
            routine("app.a_reader()", "SELECT g FROM app.n LIMIT 1"),
            routine("app.f(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let created =
            |name: &str| at(&|c| matches!(c, Change::CreateModule { id: i, .. } if *i == id(name)));
        let table_at = |name: &str| {
            at(&|c| matches!(c, Change::CreateTable { name: n, .. } if n.to_string() == name))
        };
        let order = names(&cs);
        assert!(created("app.f(integer)") < table_at("app.n"), "{order:?}");
        assert!(table_at("app.n") < created("app.a_reader()"), "{order:?}");
        #[allow(clippy::wildcard_enum_match_arm)]
        match &cs.changes[table_at("app.n")].change {
            Change::CreateTable { table, .. } => {
                assert_eq!(table.columns.keys().collect::<Vec<_>>(), ["id", "g"]);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A new partition's own default calling a new function holds the whole
    /// partition after that function, as a generated column holds its table
    /// (#1578, DEC-1364.1): the partition has no column of its own to split
    /// the default into. A partition whose own default calls nothing new, and
    /// its parent, keep their places.
    #[test]
    fn a_new_partitions_own_default_calling_a_new_function_follows_it_whole() {
        let partition = |name: &str, default: &str| {
            let t = Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: [(
                        "v".to_owned(),
                        pbps_model::PartitionColumn {
                            default: Some(default.to_owned()),
                            not_null: false,
                        },
                    )]
                    .into(),
                }),
                ..Table::default()
            };
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, name, 0),
                name: name.parse().unwrap(),
                table: Box::new(t),
            }
        };
        let mut parent = Table::default();
        parent
            .columns
            .insert("v".into(), Column::new("integer".parse().unwrap()));
        parent.partition_by = Some(pbps_model::PartitionBy {
            columns: vec!["v".into()],
        });
        let mut cs = plan(vec![
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, "app.ev", 0),
                name: "app.ev".parse().unwrap(),
                table: Box::new(parent),
            },
            partition("app.ev_new", "app.f(1)"),
            partition("app.ev_plain", "7"),
            routine("app.f(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let table_at = |name: &str| {
            at(&|c| matches!(c, Change::CreateTable { name: n, .. } if n.to_string() == name))
        };
        let function =
            at(&|c| matches!(c, Change::CreateModule { id: i, .. } if *i == id("app.f(integer)")));
        let order = names(&cs);
        assert!(function < table_at("app.ev_new"), "{order:?}");
        assert!(table_at("app.ev") < function, "{order:?}");
        assert!(table_at("app.ev_plain") < function, "{order:?}");
    }

    /// A new partitioned parent held behind the function its generated column
    /// calls takes its new partitions with it, since `CREATE … PARTITION OF`
    /// needs the parent standing (#1586): the order is function, parent,
    /// partition. A partition of a parent that is not held keeps its place.
    #[test]
    fn a_held_new_parent_takes_its_new_partitions_with_it() {
        let parent = |name: &str, generated: Option<&str>| {
            let mut t = Table::default();
            t.columns
                .insert("k".into(), Column::new("integer".parse().unwrap()));
            if let Some(expression) = generated {
                let mut g = Column::new("integer".parse().unwrap());
                g.generated = Some(pbps_model::Generated {
                    expression: expression.into(),
                    stored: true,
                });
                t.columns.insert("g".into(), g);
            }
            t.partition_by = Some(pbps_model::PartitionBy {
                columns: vec!["k".into()],
            });
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, name, 0),
                name: name.parse().unwrap(),
                table: Box::new(t),
            }
        };
        let partition = |name: &str, of: &str| Change::CreateTable {
            uid: Uid::derived(UidKind::Table, name, 0),
            name: name.parse().unwrap(),
            table: Box::new(Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: of.parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Table::default()
            }),
        };
        let mut cs = plan(vec![
            parent("app.ev", Some("app.f(k)")),
            partition("app.ev_1", "app.ev"),
            parent("app.other", None),
            partition("app.other_1", "app.other"),
            routine("app.f(integer)", "SELECT 1"),
        ]);
        rebuilds(&mut cs, &BTreeSet::new());
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let table_at = |name: &str| {
            at(&|c| matches!(c, Change::CreateTable { name: n, .. } if n.to_string() == name))
        };
        let function =
            at(&|c| matches!(c, Change::CreateModule { id: i, .. } if *i == id("app.f(integer)")));
        let order = names(&cs);
        assert!(function < table_at("app.ev"), "{order:?}");
        assert!(table_at("app.ev") < table_at("app.ev_1"), "{order:?}");
        // Negative: the other tree calls nothing and stays ahead.
        assert!(table_at("app.other_1") < function, "{order:?}");
        assert!(table_at("app.other") < table_at("app.other_1"), "{order:?}");
    }

    /// A column a standing parent adds, held behind the function its default
    /// calls, takes with it a new partition that sets its own default on that
    /// column (#1692 review): the partition's `ALTER COLUMN` needs the column.
    /// A new partition with its own entries on other columns only is free.
    #[test]
    fn a_held_parent_column_takes_the_new_partitions_that_set_their_own_on_it() {
        let partition = |name: &str, column: &str| Change::CreateTable {
            uid: Uid::derived(UidKind::Table, name, 0),
            name: name.parse().unwrap(),
            table: Box::new(Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: [(
                        column.to_owned(),
                        pbps_model::PartitionColumn {
                            default: Some("5".to_owned()),
                            not_null: false,
                        },
                    )]
                    .into(),
                }),
                ..Table::default()
            }),
        };
        let index_on = |column: &str| pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(column.into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        // A new partition whose own index is on the column, with no own
        // default or NOT NULL on it, needs the column all the same.
        let indexed = |name: &str, index: &str, column: &str| {
            let Change::CreateTable {
                uid,
                name,
                mut table,
            } = partition(name, "v")
            else {
                unreachable!("a partition is created");
            };
            table.indexes.insert(index.into(), index_on(column));
            Change::CreateTable { uid, name, table }
        };
        // A standing partition's own changes on the column, and a default
        // on another parent's column of the same name.
        let standing_default = |table: &str, parent: &str| Change::SetPartitionDefault {
            uid: Uid::derived(UidKind::Table, table, 0),
            table: table.parse().unwrap(),
            parent: parent.parse().unwrap(),
            column: "extra".into(),
            from: None,
            to: Some("6".into()),
            fallback: None,
        };
        let mut extra = Column::new("integer".parse().unwrap());
        extra.default = Some("app.f()".into());
        let mut cs = plan(vec![
            Change::AddColumn {
                uid: "c_a1b2c3".parse().unwrap(),
                table: TableName::new("app", "ev"),
                name: "extra".into(),
                column: Box::new(extra),
            },
            partition("app.ev_new", "extra"),
            partition("app.ev_plain", "v"),
            indexed("app.ev_ix", "ev_ix_extra", "extra"),
            indexed("app.ev_ix_v", "ev_ix_v", "v"),
            standing_default("app.ev_1", "app.ev"),
            standing_default("app.x_1", "app.x"),
            // Each on a partition of its own, so that none follows another
            // change of its table rather than the column.
            Change::SetPartitionNotNull {
                uid: Uid::derived(UidKind::Table, "app.ev_2", 0),
                table: "app.ev_2".parse().unwrap(),
                column: "extra".into(),
                not_null: true,
            },
            Change::AddIndex {
                table: "app.ev_3".parse().unwrap(),
                name: "ev_3_extra".into(),
                index: Box::new(index_on("extra")),
                clustered: false,
            },
            routine("app.f()", "SELECT 1"),
        ]);
        rebuilds(&mut cs, &BTreeSet::new());
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let table_at = |name: &str| {
            at(&|c| matches!(c, Change::CreateTable { name: n, .. } if n.to_string() == name))
        };
        let function =
            at(&|c| matches!(c, Change::CreateModule { id: i, .. } if *i == id("app.f()")));
        let added = at(&|c| matches!(c, Change::AddColumn { .. }));
        let order = names(&cs);
        assert!(function < added, "{order:?}");
        assert!(added < table_at("app.ev_new"), "{order:?}");
        assert!(added < table_at("app.ev_ix"), "{order:?}");
        let default_at = |table: &str| {
            at(
                &|c| matches!(c, Change::SetPartitionDefault { table: t, .. } if t.to_string() == table),
            )
        };
        assert!(added < default_at("app.ev_1"), "{order:?}");
        assert!(
            added < at(&|c| matches!(c, Change::SetPartitionNotNull { .. })),
            "{order:?}"
        );
        assert!(
            added < at(&|c| matches!(c, Change::AddIndex { .. })),
            "{order:?}"
        );
        // Negative: another parent's partition default on its own `extra`.
        assert!(default_at("app.x_1") < function, "{order:?}");
        // Negative: a partition with nothing of its own on `extra` stays ahead.
        assert!(table_at("app.ev_plain") < function, "{order:?}");
        assert!(table_at("app.ev_ix_v") < function, "{order:?}");
    }

    /// A new table held back for its generation expression does not drag an
    /// unrelated new table behind it: when the function it calls reads that
    /// other table, the order other table, function, generated table exists
    /// and is found. A new table that names the held one still follows it.
    #[test]
    fn a_held_new_table_leaves_unrelated_new_tables_free() {
        let new_table = |name: &str, columns: &[(&str, Option<&str>)], key: Option<&str>| {
            let mut t = Table::default();
            for (column, expression) in columns {
                let mut c = Column::new("integer".parse().unwrap());
                c.generated = expression.map(|expression| pbps_model::Generated {
                    expression: expression.into(),
                    stored: true,
                });
                t.columns.insert((*column).into(), c);
            }
            if let Some(target) = key {
                t.foreign_keys.insert(
                    "fk".into(),
                    pbps_model::ForeignKey {
                        columns: vec!["id".into()],
                        references_table: target.parse().unwrap(),
                        references_columns: vec!["id".into()],
                        on_delete: Default::default(),
                        on_update: Default::default(),
                    },
                );
            }
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, name, 0),
                name: name.parse().unwrap(),
                table: Box::new(t),
            }
        };
        // A later new table naming app.n only inside an OID-alias literal.
        let literal_table = || {
            let mut t = Table::default();
            let mut r = Column::new("regclass".parse().unwrap());
            r.default = Some("'app.n'::regclass".into());
            t.columns.insert("r".into(), r);
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, "app.literal", 0),
                name: "app.literal".parse().unwrap(),
                table: Box::new(t),
            }
        };
        // And a new partition naming it only in its own default (#1578).
        let literal_partition = || Change::CreateTable {
            uid: Uid::derived(UidKind::Table, "app.part", 0),
            name: "app.part".parse().unwrap(),
            table: Box::new(Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.other".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: [(
                        "id".to_owned(),
                        pbps_model::PartitionColumn {
                            default: Some("'app.n'::regclass::oid::integer".into()),
                            not_null: false,
                        },
                    )]
                    .into(),
                }),
                ..Table::default()
            }),
        };
        let mut cs = plan(vec![
            new_table("app.n", &[("id", None), ("g", Some("app.f(id)"))], None),
            new_table("app.other", &[("id", None)], None),
            new_table("app.child", &[("id", None)], Some("app.n")),
            literal_table(),
            literal_partition(),
            routine("app.f(integer)", "SELECT count(*)::integer FROM app.other"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let table_at = |name: &str| {
            at(&|c| matches!(c, Change::CreateTable { name: n, .. } if n.to_string() == name))
        };
        let created = at(&|c| matches!(c, Change::CreateModule { .. }));
        let order = names(&cs);
        assert!(table_at("app.other") < created, "{order:?}");
        assert!(created < table_at("app.n"), "{order:?}");
        assert!(table_at("app.n") < table_at("app.child"), "{order:?}");
        assert!(table_at("app.n") < table_at("app.literal"), "{order:?}");
        assert!(table_at("app.n") < table_at("app.part"), "{order:?}");

        // Two tables held behind different functions keep their own order:
        // app.b waits for the earlier function but names app.a, which waits
        // for the later one.
        let mut cs = plan(vec![
            new_table("app.a", &[("id", None), ("g", Some("app.f2(id)"))], None),
            new_table(
                "app.b",
                &[
                    ("id", None),
                    ("g", Some("app.f1(id)")),
                    ("r", Some("'app.a'::regclass::oid::integer")),
                ],
                None,
            ),
            routine("app.f1(integer)", "SELECT 1"),
            routine("app.f2(integer)", "SELECT 2"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 2);
        let order = names(&cs);
        let position = |name: &str| {
            cs.changes
                .iter()
                .position(|p| {
                    matches!(&p.change, Change::CreateTable { name: n, .. } if n.to_string() == name)
                        || matches!(&p.change, Change::CreateModule { id, .. } if id.to_string() == name)
                })
                .unwrap_or_else(|| panic!("{name} missing from {order:?}"))
        };
        assert!(position("app.f2(integer)") < position("app.a"), "{order:?}");
        assert!(position("app.a") < position("app.b"), "{order:?}");

        // An underscored name is not read as app.a's array type: its spelling
        // depends on what the target already holds (#1503), and guessing it
        // would close a cycle no order has. app.other names a declared
        // app.__a and is read by the function app.a calls.
        let mut other = Table::default();
        let mut r = Column::new("integer".parse().unwrap());
        r.default = Some("'app.__a'::regtype::oid::integer".into());
        other.columns.insert("r".into(), r);
        let mut cs = plan(vec![
            new_table("app.a", &[("id", None), ("g", Some("app.f(id)"))], None),
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, "app.other", 0),
                name: "app.other".parse().unwrap(),
                table: Box::new(other),
            },
            routine("app.f(integer)", "SELECT count(*)::integer FROM app.other"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        let order = names(&cs);
        let position = |name: &str| {
            cs.changes
                .iter()
                .position(|p| {
                    matches!(&p.change, Change::CreateTable { name: n, .. } if n.to_string() == name)
                        || matches!(&p.change, Change::CreateModule { id, .. } if id.to_string() == name)
                })
                .unwrap_or_else(|| panic!("{name} missing from {order:?}"))
        };
        assert!(
            position("app.other") < position("app.f(integer)"),
            "{order:?}"
        );
        assert!(position("app.f(integer)") < position("app.a"), "{order:?}");
    }

    /// DEC-1364.1: a literal an OID-alias type reads names the function to
    /// the engine, so a check naming a new function only that way follows its
    /// create, and so does a column whose default does.
    #[test]
    fn a_function_named_only_in_an_oid_alias_literal_is_followed() {
        let mut cs = plan(vec![
            add_column("r", "('app.f(integer)'::regprocedure)::text", false),
            check_of("ck_regproc", "'app.f'::regproc IS NOT NULL"),
            routine("app.f(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 2);
        let order = names(&cs);
        assert!(order[0].starts_with("create app.f"), "{order:?}");
        assert!(order[1].starts_with("AddColumn"), "{order:?}");
        assert_eq!(order[2], "add check ck_regproc", "{order:?}");

        // A quoted name is read whole inside the literal.
        let mut cs = plan(vec![
            check_of(
                "ck_quoted",
                "'app.\"my func\"(integer)'::regprocedure IS NOT NULL",
            ),
            routine("app.my func(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert_eq!(names(&cs)[1], "add check ck_quoted", "{:?}", names(&cs));

        // A reserved word is a name, bare, to the OID-alias input.
        let mut cs = plan(vec![
            check_of("ck_reserved", "'select(integer)'::regprocedure IS NOT NULL"),
            routine("public.select(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert_eq!(names(&cs)[1], "add check ck_reserved", "{:?}", names(&cs));
    }

    /// A function's name in a comment, or as a Unicode identifier's escape
    /// character, is no call: a column whose expression only holds it there
    /// stays ahead of the function, which reads the column, and the plan is
    /// not refused as a cycle (DEC-1364.1).
    #[test]
    fn a_function_named_only_in_a_comment_is_no_call() {
        for (column, function) in [
            (add_column("g", "0 /* app.f(1) */", false), "app.f(integer)"),
            (
                add_column("g", "U&\"_0061\" UESCAPE '_' * 2", true),
                "app._()",
            ),
        ] {
            let mut cs = plan(vec![
                column,
                routine(function, "SELECT g FROM app.t LIMIT 1"),
            ]);
            let before = names(&cs);
            assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 0);
            assert_eq!(names(&cs), before);
        }
    }

    /// DEC-1364.1: a module naming the table may take its whole column shape
    /// without naming the column, by `*`, a spelled list beside it, or a
    /// positional insert, so it follows a column that moves. A view over
    /// another table keeps its place.
    #[test]
    fn a_module_naming_the_table_follows_a_moved_column() {
        let mut cs = plan(vec![
            add_column("g", "app.f(1)", true),
            new_view("app.a_all", "SELECT * FROM app.t"),
            new_view("app.a_other", "SELECT * FROM app.u"),
            new_view("app.a_spelled", "SELECT id FROM app.t"),
            routine(
                "app.a_writer()",
                "INSERT INTO app.t VALUES (1, 2, 3) RETURNING 1",
            ),
            routine("app.f(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert_eq!(
            names(&cs),
            [
                "create app.a_other",
                "create app.f(integer)",
                &names(&plan(vec![add_column("g", "app.f(1)", true)]))[0],
                "create app.a_all",
                "create app.a_spelled",
                "create app.a_writer()",
            ]
        );
    }

    /// A module naming another only in a literal, as `regprocedure` reads it
    /// in a `BEGIN ATOMIC` body, still follows it when that one moves after a
    /// column (DEC-1364.1).
    #[test]
    fn a_module_naming_a_moved_module_in_a_literal_follows_it() {
        let mut cs = plan(vec![
            add_column("g", "app.z(1)", true),
            routine("app.a_reader()", "SELECT g FROM app.t LIMIT 1"),
            Change::CreateModule {
                id: id("app.b_user()"),
                module: Box::new(module(
                    ModuleKind::Function,
                    "() RETURNS text LANGUAGE sql \
                     BEGIN ATOMIC SELECT 'app.a_reader()'::regprocedure::text; END",
                )),
            },
            routine("app.z(integer)", "SELECT 1"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        let at =
            |f: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| f(&p.change)).unwrap();
        let created =
            |name: &str| at(&|c| matches!(c, Change::CreateModule { id: i, .. } if *i == id(name)));
        let column = at(&|c| matches!(c, Change::AddColumn { .. }));
        let order = names(&cs);
        assert!(created("app.z(integer)") < column, "{order:?}");
        assert!(column < created("app.a_reader()"), "{order:?}");
        assert!(
            created("app.a_reader()") < created("app.b_user()"),
            "{order:?}"
        );
    }

    /// A `depends_on:` edge holds a module behind what it declares, though its
    /// text names nothing: moved after the column with its reader, it is
    /// moved too. Without the edge it keeps its place.
    #[test]
    fn a_declared_dependency_holds_a_module_behind_a_moved_reader() {
        let changes = || {
            plan(vec![
                add_column("g", "app.f(1)", true),
                routine("app.a_reader()", "SELECT g FROM app.t LIMIT 1"),
                routine("app.b_user()", "SELECT 1"),
                routine("app.f(integer)", "SELECT 1"),
            ])
        };
        let at = |cs: &ChangeSet, name: &str| {
            cs.changes
                .iter()
                .position(
                    |p| matches!(&p.change, Change::CreateModule { id: i, .. } if *i == id(name)),
                )
                .unwrap()
        };
        let deps = ModuleDeps::from([(id("app.b_user()"), BTreeSet::from([id("app.a_reader()")]))]);
        let mut cs = changes();
        assert_eq!(after_the_rebuilds(&mut cs, &BTreeSet::new(), &deps), Ok(1));
        assert!(
            at(&cs, "app.a_reader()") < at(&cs, "app.b_user()"),
            "{:?}",
            names(&cs)
        );

        let mut cs = changes();
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert!(
            at(&cs, "app.b_user()") < at(&cs, "app.f(integer)"),
            "{:?}",
            names(&cs)
        );
        assert!(
            at(&cs, "app.f(integer)") < at(&cs, "app.a_reader()"),
            "{:?}",
            names(&cs)
        );
    }

    /// DEC-1364.1: a function a new column calls, and that reads the column,
    /// is a cycle no order performs. The plan is refused by name with the
    /// two-plan remedy, directly or through a function it calls, and left as
    /// it was. A function that only names the column's table is held to the
    /// same, since naming the table may take its whole column shape.
    #[test]
    fn a_column_calling_a_function_that_reads_it_is_refused_by_name() {
        for changes in [
            vec![
                add_column("g", "app.f(1)", true),
                routine("app.f(integer)", "SELECT g FROM app.t LIMIT 1"),
            ],
            vec![
                add_column("g", "app.f(1)", false),
                routine("app.a_helper()", "SELECT max(g) FROM app.t"),
                Change::CreateModule {
                    id: id("app.f(integer)"),
                    module: Box::new(module(
                        ModuleKind::Function,
                        "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT app.a_helper() $$",
                    )),
                },
            ],
            vec![
                add_column("g", "app.f(1)", false),
                routine("app.f(integer)", "SELECT max(id) FROM app.t"),
            ],
        ] {
            let mut cs = plan(changes);
            let before = names(&cs);
            let why =
                after_the_rebuilds(&mut cs, &BTreeSet::new(), &ModuleDeps::new()).unwrap_err();
            assert!(why.contains("`app.t.g`"), "{why}");
            assert!(why.contains("`app.f(integer)`"), "{why}");
            assert!(why.contains("two plans"), "{why}");
            assert_eq!(names(&cs), before);
        }
    }

    /// Rows are written before the modules, so a row that writes the column,
    /// or takes its default, cannot follow a function the column calls: the
    /// plan is refused by name. A row that touches neither keeps its place,
    /// and so does every row beside a generated column.
    #[test]
    fn a_row_writing_a_column_that_calls_a_new_function_is_refused() {
        let insert = |row: &[&str], defaults: &[&str]| Change::InsertRow {
            table: TableName::new("app", "t"),
            key_column: "id".into(),
            identity_key: false,
            key: pbps_model::RowKey::from("1"),
            row: pbps_model::Row(
                row.iter()
                    .map(|c| ((*c).to_owned(), pbps_model::Value::Null))
                    .collect(),
            ),
            defaults: defaults
                .iter()
                .map(|c| ((*c).to_owned(), "app.f(1)".to_owned()))
                .collect(),
            types: BTreeMap::new(),
        };
        let ordered = |row: Change| {
            let mut cs = plan(vec![
                add_column("g", "app.f(1)", false),
                row,
                routine("app.f(integer)", "SELECT 1"),
            ]);
            after_the_rebuilds(&mut cs, &BTreeSet::new(), &ModuleDeps::new())
        };
        let why = ordered(insert(&["g"], &[])).unwrap_err();
        assert!(why.contains("a row this plan writes into `app.t`"), "{why}");
        assert!(
            ordered(insert(&[], &["g"])).is_err(),
            "a row taking its default"
        );
        assert_eq!(
            ordered(insert(&["id"], &[])),
            Ok(1),
            "a row of other columns"
        );
    }

    /// Negatives: no function rebuilt (a view rebuilt, or nothing), and a
    /// default a row the plan inserts takes, which must be in place before
    /// the insert that fills it.
    #[test]
    fn nothing_moves_without_a_rebuilt_function_or_ahead_of_its_rows() {
        let (s, _) = declared();
        for changes in [
            vec![add_check("ck"), alter(&s, "app.v0")],
            vec![add_check("ck")],
        ] {
            let mut cs = plan(changes);
            let before = names(&cs);
            assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 0);
            assert_eq!(names(&cs), before);
        }
        let mut cs = plan(vec![
            set_default("u"),
            Change::InsertRow {
                table: TableName::new("app", "u"),
                key_column: "id".into(),
                identity_key: false,
                key: pbps_model::RowKey::from("1"),
                row: pbps_model::Row::default(),
                defaults: BTreeMap::from([("n".to_owned(), "app.f(1)".to_owned())]),
                types: BTreeMap::new(),
            },
            add_check("ck"),
            alter(&s, "app.f(integer)"),
        ]);
        assert_eq!(rebuilds(&mut cs, &BTreeSet::new()), 1);
        assert_eq!(
            names(&cs),
            [
                "default n -> Some(\"app.f(1)\")",
                "insert",
                "alter app.f(integer)",
                "add check ck",
            ]
        );
    }

    /// #1030: only a row that takes the default holds it back. An insert
    /// that spells the column and an update of another column take nothing
    /// from it, and it moves; an update that sets the column back to its
    /// default takes it, and it stays.
    #[test]
    fn a_default_is_held_back_only_by_a_row_that_takes_it() {
        let (s, _) = declared();
        let insert = |defaults: &[&str]| Change::InsertRow {
            table: TableName::new("app", "u"),
            key_column: "id".into(),
            identity_key: false,
            key: pbps_model::RowKey::from("1"),
            row: pbps_model::Row::default(),
            defaults: defaults
                .iter()
                .map(|c| ((*c).to_owned(), "app.f(1)".to_owned()))
                .collect(),
            types: BTreeMap::new(),
        };
        let update = |column: &str, after: pbps_model::Cell| Change::UpdateRow {
            table: TableName::new("app", "u"),
            key_column: "id".into(),
            key: pbps_model::RowKey::from("1"),
            columns: BTreeMap::from([(
                column.to_owned(),
                (pbps_model::Cell::Value(pbps_model::Value::Null), after),
            )]),
            unchanged: BTreeMap::new(),
            types: BTreeMap::new(),
            after_types: BTreeMap::new(),
            key_type: None,
        };
        let moved = |write: Change| {
            let mut cs = plan(vec![set_default("u"), write, alter(&s, "app.f(integer)")]);
            rebuilds(&mut cs, &BTreeSet::new()) == 1
        };
        assert!(moved(insert(&[])), "an insert that spells `n`");
        assert!(
            moved(insert(&["other"])),
            "an insert defaulting another column"
        );
        assert!(
            moved(update("other", pbps_model::Cell::Default("0".into()))),
            "an update of another column"
        );
        assert!(
            moved(update(
                "n",
                pbps_model::Cell::Value(pbps_model::Value::Null)
            )),
            "an update that spells `n`"
        );
        assert!(!moved(insert(&["n"])), "an insert that omits `n`");
        assert!(
            !moved(update("n", pbps_model::Cell::Default("app.f(1)".into()))),
            "an update that sets `n` back to its default"
        );
    }

    /// An expression index the weave rebuilds around a function takes its
    /// declared parameters in its `CREATE`, so a change to them the plan
    /// carried is dropped: run beside the rebuild it would alter an index the
    /// plan has dropped, and a staged checkpoint would refuse the plan (#1483
    /// review).
    #[test]
    fn a_woven_index_rebuild_takes_over_its_parameter_change() {
        let (mut s, ids) = declared();
        let t = TableName::new("app", "t");
        s.tables.get_mut(&t).unwrap().indexes.insert(
            "ix_f".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Expression("app.f(id)".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: [("fillfactor".to_owned(), "80".to_owned())].into(),
            },
        );
        let parameters = Change::SetIndexStorageParameters {
            table: t.clone(),
            target: pbps_model::IndexPart::Index("ix_f".into()),
            method: Default::default(),
            set: [("fillfactor".to_owned(), "80".to_owned())].into(),
            reset: Default::default(),
        };
        let mut cs = plan(vec![parameters, alter(&s, "app.f(integer)")]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![part(
                Part::Index("ix_f".into()),
                "index ix_f on table app.t",
            )],
        )]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        assert!(
            !cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::SetIndexStorageParameters { .. })),
            "{:?}",
            rendered(&cs)
        );
        let added = cs.changes.iter().find_map(|p| {
            if let Change::AddIndex { index, .. } = &p.change {
                Some(index.storage_parameters.clone())
            } else {
                None
            }
        });
        assert_eq!(
            added,
            Some([("fillfactor".to_owned(), "80".to_owned())].into()),
            "{:?}",
            rendered(&cs)
        );
    }

    /// The case the issue names: a function edit, and a check and a default
    /// the diff never mentioned. Removed before the rebuild, restored after.
    #[test]
    fn a_table_attached_while_its_checks_function_is_rebuilt_takes_its_parents_back() {
        let (mut s, ids) = declared();
        let check = || CheckConstraint {
            expression: "app.f(id) >= 0".into(),
        };
        let mut parent = Table {
            columns: s.tables[&TableName::new("app", "t")].columns.clone(),
            ..Table::default()
        };
        parent.checks.insert("ck".into(), check());
        parent.partition_by = Some(pbps_model::PartitionBy {
            columns: vec!["id".into()],
        });
        s.tables.insert(TableName::new("app", "p"), parent);
        let bound = pbps_model::PartitionBound::Range {
            from: vec![pbps_model::BoundDatum::Value("0".into())],
            to: vec![pbps_model::BoundDatum::Value("10".into())],
        };
        // Declared as the partition it becomes: `ck` is its parent's, and
        // `own`, which calls the same function, its own.
        let partition = Table {
            checks: [("own".to_owned(), check())].into(),
            partition_of: Some(pbps_model::PartitionOf {
                parent: TableName::new("app", "p"),
                bound: bound.clone(),
                columns: Default::default(),
            }),
            ..Table::default()
        };
        s.tables
            .insert(TableName::new("app", "t"), partition.clone());
        let mut cs = plan(vec![
            Change::AttachPartition {
                uid: "t_cccccc".parse().unwrap(),
                table: TableName::new("app", "t"),
                parent: TableName::new("app", "p"),
                bound,
                shape: Box::new(partition),
            },
            alter(&s, "app.f(integer)"),
        ]);
        let on = |table: &str, name: &str| Dependent {
            described: format!("constraint {name} on table {table}"),
            holds: Holds::TablePart {
                table: table.parse().unwrap(),
                part: Part::Check(name.into()),
            },
        };
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![on("app.t", "own"), on("app.t", "ck"), on("app.p", "ck")],
        )]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).expect("woven");
        let touched: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| {
                if let Change::DropCheck { table, name } = &p.change {
                    Some(format!("drop {table} {name}"))
                } else if let Change::AddCheck { table, name, .. } = &p.change {
                    Some(format!("add {table} {name}"))
                } else if let Change::AttachPartition { .. } = &p.change {
                    Some("attach".to_owned())
                } else if let Change::AlterModule { .. } = &p.change {
                    Some("alter".to_owned())
                } else {
                    None
                }
            })
            .collect();
        // Attached before the rebuild: the parent's drop takes the copy, and
        // its own goes and comes back as any declared check.
        assert_eq!(
            touched,
            [
                "attach",
                "drop app.t own",
                "drop app.p ck",
                "alter",
                "add app.p ck",
                "add app.t own"
            ]
        );
        assert!(unaccounted(&cs, &found).is_empty());
        let woven = cs.changes.len();
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).expect("woven again");
        assert_eq!(cs.changes.len(), woven, "a fixed point");
        // Negative: attached after the module's drop, a copy nothing removes
        // is reported for the apply to refuse, not taken for the parent's.
        let late = plan(vec![
            alter(&s, "app.f(integer)"),
            cs.changes
                .iter()
                .find(|p| matches!(p.change, Change::AttachPartition { .. }))
                .unwrap()
                .change
                .clone(),
        ]);
        assert_eq!(
            unaccounted(&late, &found),
            [
                "constraint own on table app.t depends on `app.f(integer)`",
                "constraint ck on table app.t depends on `app.f(integer)`",
                "constraint ck on table app.p depends on `app.f(integer)`"
            ]
        );
        // Negative: without the attach, the same undeclared check is refused.
        let mut alone = plan(vec![alter(&s, "app.f(integer)")]);
        weave(&mut alone, &found, &s, &[&ids], pg().as_ref()).expect_err("not declared");
    }

    #[test]
    fn a_rebuilt_functions_check_and_default_go_before_it_and_come_back_after() {
        let (s, ids) = declared();
        let mut cs = plan(vec![alter(&s, "app.f(integer)")]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![
                part(Part::Check("ck".into()), "constraint ck on table app.t"),
                part(
                    Part::Default("n".into()),
                    "default value for column n of table app.t",
                ),
            ],
        )]);
        let added = weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        assert_eq!(added, 4);
        assert_eq!(
            rendered(&cs),
            [
                "drop check ck",
                "default n -> None",
                "alter app.f(integer)",
                "default n -> Some(\"app.f(1)\")",
                "add check ck",
            ]
        );
        assert!(unaccounted(&cs, &found).is_empty());
        // Every change carries the dialect's own risks: saved-plan
        // verification recomputes them and would refuse any other answer.
        for p in &cs.changes {
            assert_eq!(p.risks, pg().change_risks(&p.change), "{:?}", p.change);
        }
    }

    /// A chain of views over a rebuilt view: dropped deepest first, created
    /// back in reverse (DECISIONS 311).
    #[test]
    fn views_over_a_rebuilt_view_are_dropped_deepest_first_and_created_in_reverse() {
        let (s, ids) = declared();
        let mut cs = plan(vec![alter(&s, "app.v0")]);
        let found = BTreeMap::from([(id("app.v0"), vec![view("app.v2"), view("app.v1")])]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        assert_eq!(
            rendered(&cs),
            [
                "drop app.v2",
                "drop app.v1",
                "alter app.v0",
                "create app.v1",
                "create app.v2"
            ]
        );
    }

    /// What the plan already does to a dependent is moved, not duplicated: a
    /// view the declarations drop goes before the function under it, and a
    /// view they edit is split into its drop and its create.
    #[test]
    fn a_dependent_the_plan_already_changes_is_moved_or_split_not_duplicated() {
        let (mut s, ids) = declared();
        s.modules.remove(&id("app.v2"));
        let mut cs = plan(vec![
            alter(&s, "app.v0"),
            alter(&s, "app.v1"),
            Change::DropModule {
                id: id("app.v2"),
                kind: ModuleKind::View,
            },
        ]);
        let found = BTreeMap::from([(id("app.v0"), vec![view("app.v2"), view("app.v1")])]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        assert_eq!(
            rendered(&cs),
            [
                "drop app.v2",
                "drop app.v1",
                "alter app.v0",
                "create app.v1"
            ]
        );
    }

    /// Weaving a woven plan changes nothing, which is what lets the apply ask
    /// the same question of the saved plan.
    #[test]
    fn a_woven_plan_is_a_fixed_point() {
        let (s, ids) = declared();
        let mut cs = plan(vec![alter(&s, "app.f(integer)"), alter(&s, "app.v0")]);
        let found = BTreeMap::from([
            (
                id("app.f(integer)"),
                vec![part(
                    Part::Check("ck".into()),
                    "constraint ck on table app.t",
                )],
            ),
            (id("app.v0"), vec![view("app.v2"), view("app.v1")]),
        ]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        let once = rendered(&cs);
        assert_eq!(
            weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap(),
            0
        );
        assert_eq!(rendered(&cs), once);
        assert!(unaccounted(&cs, &found).is_empty());
    }

    /// The refusals, each with its name: an object this project does not
    /// declare, one the model cannot represent, and a declared dependent of a
    /// module the plan drops for good.
    #[test]
    fn a_dependent_the_plan_cannot_account_for_refuses_it_by_name() {
        let (s, ids) = declared();
        let rebuild = || plan(vec![alter(&s, "app.v0")]);

        let found = BTreeMap::from([(id("app.v0"), vec![view("public.late")])]);
        let e = weave(&mut rebuild(), &found, &s, &[&ids], pg().as_ref()).unwrap_err();
        assert!(
            e.contains("view public.late") && e.contains("does not declare it"),
            "{e}"
        );

        let odd = Dependent {
            described: "index ix on table app.t".into(),
            holds: Holds::Unrepresentable("an expression index".into()),
        };
        let found = BTreeMap::from([(id("app.v0"), vec![odd])]);
        let e = weave(&mut rebuild(), &found, &s, &[&ids], pg().as_ref()).unwrap_err();
        assert!(e.contains("index ix on table app.t"), "{e}");

        // A declared partition's default on a column neither it nor its
        // parent declares a default for: the declarations do not hold it
        // (#1588), as one on a table they do not declare.
        let mut with_partition = s.clone();
        with_partition.tables.insert(
            TableName::new("app", "t_1"),
            Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: TableName::new("app", "t"),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Table::default()
            },
        );
        let default_on = |table: &str| Dependent {
            described: format!("default value for column v of table app.{table}"),
            holds: Holds::TablePart {
                table: TableName::new("app", table),
                part: Part::Default("v".into()),
            },
        };
        let found = BTreeMap::from([(id("app.v0"), vec![default_on("t_1")])]);
        let e = weave(
            &mut rebuild(),
            &found,
            &with_partition,
            &[&ids],
            pg().as_ref(),
        )
        .unwrap_err();
        assert!(
            e.contains("column v of table app.t_1") && e.contains("does not declare it"),
            "{e}"
        );
        let found = BTreeMap::from([(id("app.v0"), vec![default_on("other")])]);
        let e = weave(
            &mut rebuild(),
            &found,
            &with_partition,
            &[&ids],
            pg().as_ref(),
        )
        .unwrap_err();
        assert!(e.contains("does not declare it"), "{e}");

        let mut gone = plan(vec![Change::DropModule {
            id: id("app.f(integer)"),
            kind: ModuleKind::Function,
        }]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![part(
                Part::Check("ck".into()),
                "constraint ck on table app.t",
            )],
        )]);
        let e = weave(&mut gone, &found, &s, &[&ids], pg().as_ref()).unwrap_err();
        assert!(
            e.contains("constraint ck") && e.contains("kept by the declarations"),
            "{e}"
        );
    }

    /// A function rebuilt under a partitioned table's default (#1588). Every
    /// partition holds a default row on the column, the parent's copy or its
    /// own, and each that calls the function is taken off before its drop and
    /// put back after its create: the copy as its parent's, the own as its
    /// own. The parent's, set again, reaches every partition, so each
    /// partition's own default follows it, one that calls nothing of the
    /// function's included.
    #[test]
    fn a_partitions_default_is_released_and_set_again_after_its_parents() {
        let (mut s, mut ids) = declared();
        let t = TableName::new("app", "t");
        s.tables.get_mut(&t).unwrap().partition_by = Some(pbps_model::PartitionBy {
            columns: vec!["id".into()],
        });
        let partition = |own: Option<&str>| Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: t.clone(),
                bound: pbps_model::PartitionBound::Default,
                columns: own
                    .map(|default| {
                        (
                            "n".to_owned(),
                            pbps_model::PartitionColumn {
                                default: Some(default.to_owned()),
                                not_null: false,
                            },
                        )
                    })
                    .into_iter()
                    .collect(),
            }),
            ..Table::default()
        };
        for (name, own) in [("t_1", None), ("t_2", Some("app.f(2)")), ("t_3", Some("5"))] {
            let name = TableName::new("app", name);
            s.tables.insert(name.clone(), partition(own));
            ids.tables
                .insert(Uid::derived(UidKind::Table, &name.to_string(), 0), name);
        }
        let default_on = |table: &str| Dependent {
            described: format!("default value for column n of table app.{table}"),
            holds: Holds::TablePart {
                table: TableName::new("app", table),
                part: Part::Default("n".into()),
            },
        };
        // The engine's order: each row that calls the function, `t_3`'s not.
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![default_on("t_2"), default_on("t"), default_on("t_1")],
        )]);
        let mut cs = plan(vec![alter(&s, "app.f(integer)")]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        assert_eq!(
            after_their_parents_defaults(&mut cs, &s, &[&ids], pg().as_ref()),
            Ok(1)
        );
        let names = rendered(&cs);
        let at = |name: &str| {
            names
                .iter()
                .position(|n| n == name)
                .unwrap_or_else(|| panic!("{name} in {names:?}"))
        };
        // One `ALTER` here, a drop and a create when emitted (ADR-0009 §3).
        let rebuilt = at("alter app.f(integer)");
        for released in [
            "default n -> None",
            "default t_1.n -> None else None",
            "default t_2.n -> None else None",
        ] {
            assert!(at(released) < rebuilt, "{names:?}");
        }
        // The copy back as its parent's, which the parent's own sets too.
        assert!(
            at("default t_1.n -> None else Some(\"app.f(1)\")") > rebuilt,
            "{names:?}"
        );
        let parents = at("default n -> Some(\"app.f(1)\")");
        assert!(parents > rebuilt, "{names:?}");
        // Its own, after the parent's: the one that calls the function, and
        // the one that does not, which the parent's overwrote.
        assert_eq!(
            names[parents + 1..],
            [
                "default t_3.n -> Some(\"5\") else Some(\"app.f(1)\")",
                "default t_2.n -> Some(\"app.f(2)\") else Some(\"app.f(1)\")",
            ],
            "{names:?}"
        );

        // Negative: without a parent's default set, no partition's is set
        // again.
        let mut own_only = plan(vec![alter(&s, "app.f(integer)")]);
        let found = BTreeMap::from([(id("app.f(integer)"), vec![default_on("t_2")])]);
        weave(&mut own_only, &found, &s, &[&ids], pg().as_ref()).unwrap();
        assert_eq!(
            after_their_parents_defaults(&mut own_only, &s, &[&ids], pg().as_ref()),
            Ok(0)
        );
    }

    /// The apply's question, of a plan that does not remove a dependent before
    /// the drop: it is named, and nothing is added.
    #[test]
    fn a_saved_plan_missing_a_removal_names_the_dependent() {
        let (s, _) = declared();
        let cs = plan(vec![alter(&s, "app.v0")]);
        let found = BTreeMap::from([(id("app.v0"), vec![view("public.late")])]);
        let left = unaccounted(&cs, &found);
        assert_eq!(left, ["view public.late depends on `app.v0`"]);
        // And a module with no dependents needs nothing.
        let none = BTreeMap::from([(id("app.v0"), Vec::new())]);
        assert!(unaccounted(&cs, &none).is_empty());
    }

    /// A plan that renames the table and the column while rebuilding the
    /// function their check and default call. The catalog names them as the
    /// database does now; the declarations and every change after the renames
    /// name them as the plan leaves them. Matched and written in the name that
    /// holds at each position, the dependents are still the declared ones.
    #[test]
    // Every other change is simply not one this test reads a name from.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn a_dependent_is_named_through_the_plans_own_renames() {
        let (mut s, mut ids) = declared();
        let mut t = s.tables.remove(&TableName::new("app", "t")).unwrap();
        let n = t.columns.shift_remove("n").unwrap();
        t.columns.insert("m".into(), n);
        s.tables.insert(TableName::new("app", "u"), t);
        let uid = ids.columns.keys().next().unwrap().clone();
        ids.columns
            .insert(uid.clone(), ColumnRef::new(TableName::new("app", "u"), "m"));
        let mut cs = plan(vec![
            Change::RenameTable {
                uid: Uid::derived(UidKind::Table, "app.t", 0),
                from: TableName::new("app", "t"),
                to: TableName::new("app", "u"),
                defaults: Vec::new(),
            },
            Change::RenameColumn {
                uid,
                table: TableName::new("app", "u"),
                from: "n".into(),
                to: "m".into(),
                table_was: None,
            },
            alter(&s, "app.f(integer)"),
        ]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![
                part(Part::Check("ck".into()), "constraint ck on table app.t"),
                part(
                    Part::Default("n".into()),
                    "default value for column n of table app.t",
                ),
            ],
        )]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        let tables: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::DropCheck { table, .. } | Change::AddCheck { table, .. } => {
                    Some(table.to_string())
                }
                Change::AlterColumnDefault { column, .. } => Some(column.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            tables,
            ["app.u", "app.u.m", "app.u.m", "app.u"],
            "{:?}",
            rendered(&cs)
        );
        assert!(unaccounted(&cs, &found).is_empty());
        assert_eq!(
            weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap(),
            0
        );
    }

    /// A plan that drops the table owning a dependent before the module goes
    /// (`DropTable` sorts before `AlterModule`) has already removed it: no
    /// removal is synthesized for an object that is gone by then, and none is
    /// restored.
    #[test]
    fn a_dependent_whose_table_the_plan_drops_first_is_already_accounted_for() {
        let (mut s, ids) = declared();
        s.tables.remove(&TableName::new("app", "t"));
        let mut cs = plan(vec![
            Change::DropTable {
                uid: Uid::derived(UidKind::Table, "app.t", 0),
                name: TableName::new("app", "t"),
                detach_from: None,
            },
            alter(&s, "app.f(integer)"),
        ]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![part(
                Part::Check("ck".into()),
                "constraint ck on table app.t",
            )],
        )]);
        assert_eq!(
            weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap(),
            0
        );
        assert_eq!(cs.changes.len(), 2, "{:?}", rendered(&cs));
        assert!(unaccounted(&cs, &found).is_empty());
    }

    /// The declared views a rebuild meets and the plan does not touch are the
    /// ones handed back to the differ to rebuild; one the plan already edits,
    /// an undeclared one and a table part are not.
    #[test]
    fn only_untouched_declared_modules_are_handed_back_to_the_differ() {
        let (s, _) = declared();
        let cs = plan(vec![alter(&s, "app.v0"), alter(&s, "app.v1")]);
        let found = BTreeMap::from([(
            id("app.v0"),
            vec![
                view("app.v2"),
                view("app.v1"),
                view("public.late"),
                part(Part::Check("ck".into()), "constraint ck on table app.t"),
            ],
        )]);
        let back: Vec<String> = untouched_module_dependents(&cs, &found, &s)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(back, ["app.v2"]);
    }

    /// #947: a declared view still calling a function the plan drops for
    /// good is not handed back to be rebuilt around it (the rebuild would
    /// create the view against nothing); `weave` refuses the plan and names
    /// it. Control: the same view beside a function the plan rebuilds is
    /// handed back as before.
    #[test]
    fn a_kept_dependent_of_a_function_dropped_for_good_is_refused_not_rebuilt() {
        let (s, ids) = declared();
        let found = BTreeMap::from([(id("app.f(integer)"), vec![view("app.v0")])]);
        let drop = || Change::DropModule {
            id: id("app.f(integer)"),
            kind: ModuleKind::Function,
        };

        let mut cs = plan(vec![drop()]);
        assert!(untouched_module_dependents(&cs, &found, &s).is_empty());
        let refused = weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap_err();
        assert!(refused.contains("view app.v0"), "{refused}");
        assert!(refused.contains("app.f(integer)"), "{refused}");

        let rebuilt = plan(vec![alter(&s, "app.f(integer)")]);
        let back: Vec<String> = untouched_module_dependents(&rebuilt, &found, &s)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(back, ["app.v0"]);

        // The same view also on a module the plan rebuilds: it is still on
        // the dropped function, and not handed back through the other root
        // (#1069 review). `weave` refuses it.
        let both = BTreeMap::from([
            (id("app.f(integer)"), vec![view("app.v0")]),
            (id("app.v1"), vec![view("app.v0")]),
        ]);
        let mut cs = plan(vec![drop(), alter(&s, "app.v1")]);
        assert!(untouched_module_dependents(&cs, &both, &s).is_empty());
        let refused = weave(&mut cs, &both, &s, &[&ids], pg().as_ref()).unwrap_err();
        assert!(refused.contains("view app.v0"), "{refused}");
    }

    /// A view over an edited view is rebuilt around the edit even when the
    /// edit stops the first view calling a function the plan drops for good:
    /// its path to the function goes through the edited view, which the plan
    /// cuts (#1069 review). Refusing it would turn a valid plan away.
    #[test]
    fn a_view_whose_path_to_a_dropped_function_the_plan_cuts_is_rebuilt() {
        let (s, ids) = declared();
        // Transitive and deepest first, as `modules::dependents` answers.
        let found = BTreeMap::from([
            (id("app.f(integer)"), vec![view("app.v2"), view("app.v1")]),
            (id("app.v1"), vec![view("app.v2")]),
        ]);
        let drop = Change::DropModule {
            id: id("app.f(integer)"),
            kind: ModuleKind::Function,
        };
        let cs = plan(vec![drop.clone(), alter(&s, "app.v1")]);
        let back: Vec<String> = untouched_module_dependents(&cs, &found, &s)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(back, ["app.v2"]);
        // What `rediff` returns with `v2` rebuilt, woven without refusal.
        let mut cs = plan(vec![drop, alter(&s, "app.v1"), alter(&s, "app.v2")]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).expect("a valid plan");
    }

    /// A plan that drops a function and replaces the table its check lives on
    /// (a new uid under the same name) does not keep the old check: the
    /// declared `ck` belongs to the new table and comes with its creation.
    /// The old one is removed before the function goes; nothing is restored,
    /// and the plan is not refused.
    #[test]
    fn a_dependent_whose_owner_the_plan_replaces_is_not_kept() {
        let (s, ids) = declared();
        let mut cs = plan(vec![
            Change::DropModule {
                id: id("app.f(integer)"),
                kind: ModuleKind::Function,
            },
            Change::DropTable {
                uid: Uid::derived(UidKind::Table, "app.t", 0),
                name: TableName::new("app", "t"),
                detach_from: None,
            },
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, "app.t", 1),
                name: TableName::new("app", "t"),
                table: Box::new(s.tables[&TableName::new("app", "t")].clone()),
            },
        ]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![part(
                Part::Check("ck".into()),
                "constraint ck on table app.t",
            )],
        )]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        let shape = rendered(&cs);
        assert_eq!(shape[0], "drop check ck", "{shape:?}");
        assert_eq!(shape[1], "drop app.f(integer)", "{shape:?}");
        assert!(!shape.iter().any(|c| c == "add check ck"), "{shape:?}");
        assert!(unaccounted(&cs, &found).is_empty());
    }

    /// A plan that drops a function for good, renames the table, and edits
    /// the check so it no longer calls the function: the differ puts the
    /// check's removal after the rename, and moving it before the function's
    /// drop puts it before the rename too, where the table has its old name.
    /// It is written in that name, so the plan is not refused and its SQL
    /// names a table that exists.
    #[test]
    // A test's catch-all: any other change first is the failure it reports.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn a_removal_moved_before_a_rename_is_written_in_the_old_name() {
        let (mut s, ids) = declared();
        let mut t = s.tables.remove(&TableName::new("app", "t")).unwrap();
        t.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "id >= 0".into(),
            },
        );
        s.tables.insert(TableName::new("app", "u"), t.clone());
        s.modules.remove(&id("app.f(integer)"));
        let mut cs = plan(vec![
            Change::DropModule {
                id: id("app.f(integer)"),
                kind: ModuleKind::Function,
            },
            Change::RenameTable {
                uid: Uid::derived(UidKind::Table, "app.t", 0),
                from: TableName::new("app", "t"),
                to: TableName::new("app", "u"),
                defaults: Vec::new(),
            },
            Change::DropCheck {
                table: TableName::new("app", "u"),
                name: "ck".into(),
            },
            Change::AddCheck {
                table: TableName::new("app", "u"),
                name: "ck".into(),
                constraint: t.checks["ck"].clone(),
            },
        ]);
        let found = BTreeMap::from([(
            id("app.f(integer)"),
            vec![part(
                Part::Check("ck".into()),
                "constraint ck on table app.t",
            )],
        )]);
        weave(&mut cs, &found, &s, &[&ids], pg().as_ref()).unwrap();
        match &cs.changes[0].change {
            Change::DropCheck { table, name } => {
                assert_eq!(table.to_string(), "app.t");
                assert_eq!(name, "ck");
            }
            other => panic!("expected the moved removal first, got {other:?}"),
        }
        assert!(unaccounted(&cs, &found).is_empty());
    }

    /// The check with nothing already on the target: every candidate stands.
    fn refusal_of(cs: &ChangeSet, dialect: &dyn Dialect) -> Result<(), String> {
        later_relation_refusal(&names_a_later_relation(cs, dialect))
    }

    /// A candidate says where the target is asked: an unqualified name in
    /// `pg_catalog`, searched first; a qualified one nowhere, since its
    /// schema is the arrival's (#1589).
    #[test]
    fn a_later_name_is_looked_up_where_the_write_path_searches() {
        let searched = |default: &str| -> Vec<Vec<String>> {
            names_a_later_relation(
                &plan(vec![default_of("t", default), add_index("ix_new", None)]),
                &*pg(),
            )
            .into_iter()
            .map(|n| n.searched)
            .collect()
        };
        assert_eq!(searched("('ix_new'::regclass)::text"), [["pg_catalog"]]);
        assert_eq!(
            searched("('app.ix_new'::regclass)::text"),
            [Vec::<String>::new()]
        );
        assert!(searched("('other.ix_new'::regclass)::text").is_empty());
    }

    /// A routine's string body is analysed under its own `SET search_path`,
    /// an atomic body under the write path (measured on 16 and 18, #1599
    /// review). An unqualified name in the string body is looked up along
    /// the routine's path, up to the first schema it binds in: ahead of that
    /// schema the target answers, and a relation the plan makes earlier on
    /// the path binds it. A path the scan cannot read takes any schema.
    #[test]
    fn a_routine_body_reads_a_name_along_its_own_search_path() {
        let with_path = |name: &str, path: &str, body: &str| Change::CreateModule {
            id: id(name),
            module: Box::new(module(
                ModuleKind::Function,
                &format!("() RETURNS integer LANGUAGE sql {path} AS $$ {body} $$"),
            )),
        };
        let index_in = |schema: &str| {
            let mut ix = add_index("ix_new", None);
            if let Change::AddIndex { table, .. } = &mut ix {
                *table = TableName::new(schema, "u");
            }
            ix
        };
        let reads = "SELECT ('ix_new'::regclass)::oid::integer";
        let searched = |changes: Vec<Change>| -> Vec<(String, Vec<String>)> {
            names_a_later_relation(&plan(changes), &*pg())
                .into_iter()
                .map(|n| (n.what, n.searched))
                .collect()
        };
        // In `app`, reading through `other`: an index made in `app` later
        // is not the one it names; one made in `other` later is.
        let own = "SET search_path = other, pg_temp";
        assert!(searched(vec![with_path("app.f()", own, reads), index_in("app")]).is_empty());
        let found = searched(vec![with_path("app.f()", own, reads), index_in("other")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].0.contains("names other.ix_new"), "{found:?}");
        assert_eq!(found[0].1, ["pg_catalog"]);
        // In `other`, reading through `app`: the routine's schema is not
        // where the name is looked up (the adversarial case).
        let back = "SET search_path TO 'app', 'pg_temp'";
        let found = searched(vec![with_path("other.f()", back, reads), index_in("app")]);
        assert_eq!(found.len(), 1, "{found:?}");
        // Ahead of the schema it binds in, the target answers; a relation
        // the plan made earlier on the path binds it there.
        let two = "SET search_path = other, app";
        let found = searched(vec![with_path("app.f()", two, reads), index_in("app")]);
        assert_eq!(found[0].1, ["pg_catalog", "other"], "{found:?}");
        assert!(
            searched(vec![
                index_in("other"),
                with_path("app.f()", two, reads),
                index_in("app")
            ])
            .is_empty()
        );
        // A string body's literal is only checked as it is created: an
        // earlier schema on its path holding the name satisfies it though a
        // later relation of that name arrives first on the path, and the
        // target is asked about every schema the plan does not fill later.
        let first_app = "SET search_path = app, other, pg_temp";
        assert!(
            searched(vec![
                index_in("other"),
                with_path("app.f()", first_app, reads),
                index_in("app")
            ])
            .is_empty()
        );
        let found = searched(vec![
            with_path("app.f()", first_app, reads),
            index_in("app"),
        ]);
        assert_eq!(found[0].1, ["pg_catalog", "other"], "{found:?}");
        // Path entries named like a body's start are entries, and the body
        // after them is still read.
        for entries in ["begin, app", "return, app"] {
            let path = format!("SET search_path = {entries}");
            assert_eq!(
                searched(vec![with_path("app.f()", &path, reads), index_in("app")]).len(),
                1,
                "{entries}"
            );
        }
        // An unread clause hides where the header ends: every literal is
        // then read one level in, in any schema.
        let hidden = Change::CreateModule {
            id: id("app.h()"),
            module: Box::new(module(
                ModuleKind::Function,
                &format!(
                    "() RETURNS integer LANGUAGE sql SET search_path = $$x$$, return AS $$ {reads} $$"
                ),
            )),
        };
        assert_eq!(searched(vec![hidden, index_in("third")]).len(), 1);
        // An atomic body is parsed under the write path, whatever the
        // routine sets.
        let atomic = Change::CreateModule {
            id: id("app.g()"),
            module: Box::new(module(
                ModuleKind::Function,
                "() RETURNS integer LANGUAGE sql SET search_path = other \
                 RETURN ('ix_new'::regclass)::oid::integer",
            )),
        };
        assert_eq!(searched(vec![atomic, index_in("app")]).len(), 1);
        // A path the scan cannot read takes any schema.
        let current = "SET search_path FROM CURRENT";
        assert_eq!(
            searched(vec![
                with_path("app.f()", current, reads),
                index_in("third")
            ])
            .len(),
            1
        );
        // Negative: no path of its own is the write path.
        assert!(searched(vec![with_path("app.f()", "", reads), index_in("other")]).is_empty());
        // Negative: a literal holding quotes is data in a view and in an
        // atomic body; only a string body is read one level in.
        let data = |kind: ModuleKind, definition: &str| Change::CreateModule {
            id: id("app.d()"),
            module: Box::new(module(kind, definition)),
        };
        for (kind, definition) in [
            (ModuleKind::View, "SELECT '''app.ix_new'''::text AS s"),
            (
                ModuleKind::Function,
                "() RETURNS text LANGUAGE sql RETURN '''app.ix_new'''::text",
            ),
        ] {
            assert!(
                searched(vec![data(kind, definition), index_in("app")]).is_empty(),
                "{definition}"
            );
        }
        // The same quotes inside a string body are the literal it analyses.
        assert_eq!(
            searched(vec![
                data(
                    ModuleKind::Function,
                    "() RETURNS integer LANGUAGE sql AS 'SELECT (''app.ix_new''::regclass)::oid::integer'"
                ),
                index_in("app")
            ])
            .len(),
            1
        );
    }

    /// The routine's own path as its header spells it, in either form the
    /// engine accepts and `pg_get_functiondef` writes; a body's `SET` and an
    /// atomic body's statements are no clause, and a form the scan does not
    /// read says so rather than reading as no path.
    #[test]
    fn a_routine_path_is_read_from_the_header_alone() {
        let path = |definition: &str| routine_header(definition, &*pg()).path;
        let some = |schemas: &[&str]| Some(Some(schemas.iter().map(|s| (*s).to_owned()).collect()));
        assert_eq!(
            path("() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$"),
            Some(None)
        );
        assert_eq!(
            path(
                "() RETURNS integer LANGUAGE sql SET search_path = other, pg_temp AS $$ SELECT 1 $$"
            ),
            some(&["other", "pg_temp"])
        );
        assert_eq!(
            path(
                "() RETURNS integer LANGUAGE sql\n SET search_path TO 'other', 'pg_temp'\nAS $$ SELECT 1 $$"
            ),
            some(&["other", "pg_temp"])
        );
        // Each quoted argument is one schema, commas and all (measured on
        // 18); the last of two clauses is the one stored.
        assert_eq!(
            path("() RETURNS integer LANGUAGE sql SET search_path TO 'a,b' AS $$ SELECT 1 $$"),
            some(&["a,b"])
        );
        assert_eq!(
            path(
                "() RETURNS integer LANGUAGE sql SET search_path = app SET search_path = other AS $$ SELECT 1 $$"
            ),
            some(&["other"])
        );
        // A quoted setting name, and a parameter named like a body's start.
        assert_eq!(
            path("() RETURNS integer LANGUAGE sql SET \"search_path\" = other AS $$ SELECT 1 $$"),
            some(&["other"])
        );
        assert_eq!(
            path(
                "(begin integer) RETURNS TABLE (return integer) LANGUAGE sql SET search_path = other AS $$ SELECT 1 $$"
            ),
            some(&["other"])
        );
        assert_eq!(
            path(
                "() RETURNS integer LANGUAGE sql SET search_path = \"Other\", Third AS $$ SELECT 1 $$"
            ),
            some(&["Other", "third"])
        );
        // The engine's lexis (#1599 review): a setting's name in any case,
        // quoted or not; an unquoted non-ASCII letter kept as written, since
        // only ASCII folds; and a no-break space part of the name, since only
        // the scanner's six characters separate.
        assert_eq!(
            path("() RETURNS integer LANGUAGE sql SET \"SEARCH_PATH\" = other AS $$ SELECT 1 $$"),
            some(&["other"])
        );
        assert_eq!(
            path("() RETURNS integer LANGUAGE sql SET search_path = ÄPP AS $$ SELECT 1 $$"),
            some(&["Äpp"])
        );
        assert_eq!(
            path("() RETURNS integer LANGUAGE sql SET search_path = app\u{a0} AS $$ SELECT 1 $$"),
            some(&["app\u{a0}"])
        );
        // #1604: clauses are read forward, each value list to its end, so
        // schemas named `set` and `search_path` are list entries wherever
        // they stand, and a second clause after a quoted list still wins.
        for (clauses, schemas) in [
            (
                "SET search_path = other, set, search_path, app",
                &["other", "set", "search_path", "app"][..],
            ),
            (
                "SET search_path = set, search_path, other, pg_temp",
                &["set", "search_path", "other", "pg_temp"][..],
            ),
            (
                "SET search_path TO 'app', 'pg_temp' SET search_path TO 'other', 'pg_temp'",
                &["other", "pg_temp"][..],
            ),
        ] {
            assert_eq!(
                path(&format!(
                    "() RETURNS integer LANGUAGE sql {clauses} AS $$ SELECT 1 $$"
                )),
                some(schemas),
                "{clauses}"
            );
        }
        // Another setting's list is stepped over, quoted or bare.
        assert_eq!(
            path(
                "() RETURNS integer LANGUAGE sql SET work_mem = '64MB' SET statement_timeout = 0 SET search_path = other AS $$ SELECT 1 $$"
            ),
            some(&["other"])
        );
        // A clause after the string body still applies, and the body is
        // still the string after `AS`.
        let late = "() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql SET search_path TO 'other', 'pg_temp'";
        assert_eq!(path(late), some(&["other", "pg_temp"]));
        assert_eq!(
            routine_header(late, &*pg()).body.as_deref(),
            Some(" SELECT 1 ")
        );
        // Not a clause: in the string body, or in an atomic body.
        assert_eq!(
            path("() RETURNS void LANGUAGE plpgsql AS $$ BEGIN SET search_path = x; END $$"),
            Some(None)
        );
        assert_eq!(
            path(
                "(\"begin\" integer) RETURNS void LANGUAGE sql SET search_path = x BEGIN ATOMIC UPDATE app.t SET search_path = 1; END"
            ),
            some(&["x"])
        );
        assert_eq!(
            path("() RETURNS void LANGUAGE sql BEGIN ATOMIC UPDATE app.t SET search_path = 1; END"),
            Some(None)
        );
        // Forms the scan does not read.
        for unread in [
            "SET search_path FROM CURRENT",
            "SET search_path TO DEFAULT",
            "SET search_path = \"$user\", public",
            "SET search_path = E'app'",
            "SET search_path /* c */ = app",
            // #1604: a dollar-quoted argument, and an unquoted `$user`.
            "SET search_path = $$other$$, app",
            "SET search_path = $user, app",
            // A comment inside the list: unread, never a shorter list.
            "SET search_path TO 'app' /* note */, 'other'",
        ] {
            assert_eq!(
                path(&format!(
                    "() RETURNS integer LANGUAGE sql {unread} AS $$ SELECT 1 $$"
                )),
                None,
                "{unread}"
            );
        }
    }

    /// #1576: PostgreSQL resolves `'app.ix'::regclass` when the expression is
    /// created, so a default set before the index it names fails the apply.
    /// The plan is refused with the two-plan remedy; set after the index, or
    /// naming anything the plan does not create later, it stands.
    #[test]
    fn an_expression_naming_a_relation_the_plan_creates_later_is_refused() {
        let named = "('app.ix_new'::regclass)::text";
        let refused = refusal_of(
            &plan(vec![default_of("t", named), add_index("ix_new", None)]),
            &*pg(),
        )
        .unwrap_err();
        assert!(
            refused.contains("app.t.n's default names app.ix_new"),
            "{refused}"
        );
        assert!(refused.contains("second plan"), "{refused}");
        // A check, and an index filter, are created the same way.
        let check = Change::AddCheck {
            table: TableName::new("app", "t"),
            name: "ck".into(),
            constraint: CheckConstraint {
                expression: "'app.ix_new'::regclass IS NOT NULL".into(),
            },
        };
        assert!(refusal_of(&plan(vec![check, add_index("ix_new", None)]), &*pg()).is_err());
        // Negative: the index first, a name the plan does not create, a
        // literal that only contains the name, another schema, and a name in
        // a comment.
        for (default, created) in [
            (named, "ix_other"),
            ("'ix_new and more'", "ix_new"),
            ("('other.ix_new'::regclass)::text", "ix_new"),
            ("0 /* 'app.ix_new' */", "ix_new"),
            // An OID, and a non-ASCII letter the input does not fold.
            ("to_regclass('1259')::text", "1259"),
            ("to_regclass('app.ixÄ')::text", "ixä"),
        ] {
            assert_eq!(
                refusal_of(
                    &plan(vec![default_of("t", default), add_index(created, None)]),
                    &*pg()
                ),
                Ok(()),
                "{default} / {created}"
            );
        }
        assert_eq!(
            refusal_of(
                &plan(vec![add_index("ix_new", None), default_of("t", named)]),
                &*pg()
            ),
            Ok(())
        );
        // An unqualified name is read in the expression's own schema, the
        // whole write path: an index of that name elsewhere is no reference.
        let unqualified = "('ix_new'::regclass)::text";
        assert!(
            refusal_of(
                &plan(vec![
                    default_of("t", unqualified),
                    add_index("ix_new", None)
                ]),
                &*pg()
            )
            .is_err()
        );
        let mut elsewhere = add_index("ix_new", None);
        if let Change::AddIndex { table, .. } = &mut elsewhere {
            *table = TableName::new("other", "u");
        }
        assert_eq!(
            refusal_of(&plan(vec![default_of("t", unqualified), elsewhere]), &*pg()),
            Ok(())
        );
        // The same non-ASCII letter the plan creates is a reference.
        assert!(
            refusal_of(
                &plan(vec![
                    default_of("t", "to_regclass('app.ixÄ')::text"),
                    add_index("ixÄ", None)
                ]),
                &*pg()
            )
            .is_err()
        ); // A module's body, cast as the engine creates it, and a partition's
        // own default are read the same way (#1599 review); a module created
        // after the index stands.
        let reader = routine("app.f()", "SELECT ('app.ix_new'::regclass)::oid::integer");
        let refused = refusal_of(
            &plan(vec![reader.clone(), add_index("ix_new", None)]),
            &*pg(),
        )
        .unwrap_err();
        assert!(refused.contains("app.f() names app.ix_new"), "{refused}");
        assert_eq!(
            refusal_of(&plan(vec![add_index("ix_new", None), reader]), &*pg()),
            Ok(())
        );
        let atomic = Change::CreateModule {
            id: id("app.g()"),
            module: Box::new(module(
                ModuleKind::Function,
                "() RETURNS integer LANGUAGE sql RETURN ('app.ix_new'::regclass)::oid::integer",
            )),
        };
        assert!(refusal_of(&plan(vec![atomic, add_index("ix_new", None)]), &*pg()).is_err());
        let partition = Change::CreateTable {
            uid: Uid::derived(UidKind::Table, "app.p", 0),
            name: "app.p".parse().unwrap(),
            table: Box::new(Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: [(
                        "n".to_owned(),
                        pbps_model::PartitionColumn {
                            default: Some(named.to_owned()),
                            not_null: false,
                        },
                    )]
                    .into(),
                }),
                ..Table::default()
            }),
        };
        let refused =
            refusal_of(&plan(vec![partition, add_index("ix_new", None)]), &*pg()).unwrap_err();
        assert!(refused.contains("app.p.n's own default"), "{refused}");
    }

    /// A new table `app.n` with an `id` column, a `label` column defaulting
    /// to `default`, a check `ck` and the named indexes with their filters.
    fn new_table(
        default: Option<&str>,
        check: Option<&str>,
        indexes: &[(&str, Option<&str>)],
    ) -> Table {
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        let mut label = Column::new("text".parse().unwrap());
        label.default = default.map(Into::into);
        t.columns.insert("label".into(), label);
        if let Some(check) = check {
            t.checks.insert(
                "ck".into(),
                CheckConstraint {
                    expression: check.into(),
                },
            );
        }
        for (name, filter) in indexes {
            if let Change::AddIndex { index, .. } = add_index(name, *filter) {
                t.indexes.insert((*name).into(), *index);
            }
        }
        t
    }

    fn create(table: Table) -> Change {
        Change::CreateTable {
            uid: Uid::derived(UidKind::Table, "app.n", 0),
            name: TableName::new("app", "n"),
            table: Box::new(table),
        }
    }

    /// #1592: a new table's own indexes are created after its defaults and
    /// checks, its key's index included (measured on 18), and in name order.
    /// So one of its expressions naming one of them is refused, though both
    /// are one change; what is created before the expression is not.
    #[test]
    fn a_new_tables_own_indexes_arrive_after_its_expressions() {
        let names = |what: &str| format!("('app.{what}'::regclass)::text");
        // Its default, naming its index, its unique constraint's and its
        // named primary key's.
        for own in ["ix_new", "uq_n", "pk_n"] {
            let mut t = new_table(Some(&names(own)), None, &[("ix_new", None)]);
            t.unique.insert(
                "uq_n".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                },
            );
            t.primary_key = Some(pbps_model::PrimaryKey {
                name: Some("pk_n".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            let refused = refusal_of(&plan(vec![create(t)]), &*pg()).unwrap_err();
            assert!(
                refused.contains(&format!(
                    "app.n.label's default or expression names app.{own}"
                )),
                "{refused}"
            );
        }
        // Its check, naming its index.
        let check = format!("{} <> ''", names("ix_new"));
        assert!(
            refusal_of(
                &plan(vec![create(new_table(
                    None,
                    Some(&check),
                    &[("ix_new", None)]
                ))]),
                &*pg()
            )
            .is_err()
        );
        // An index filter naming a sibling created after it, in name order.
        let filter = format!("{} IS NOT NULL", names("ix_b"));
        assert!(
            refusal_of(
                &plan(vec![create(new_table(
                    None,
                    None,
                    &[("ix_a", Some(&filter)), ("ix_b", None)]
                ))]),
                &*pg()
            )
            .is_err()
        );
        // Negative: the table itself from its default, its key's index from
        // its check (added after the key), a sibling created before the
        // index naming it, and a relation the plan does not create, such as
        // an existing table's index.
        let mut keyed = new_table(None, Some(&format!("{} <> ''", names("pk_n"))), &[]);
        keyed.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("pk_n".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        let before = format!("{} IS NOT NULL", names("ix_a"));
        for cs in [
            plan(vec![create(new_table(Some(&names("n")), None, &[]))]),
            plan(vec![create(keyed)]),
            plan(vec![create(new_table(
                None,
                None,
                &[("ix_a", None), ("ix_b", Some(&before))],
            ))]),
            plan(vec![create(new_table(
                Some(&names("ix_elsewhere")),
                None,
                &[("ix_new", None)],
            ))]),
        ] {
            assert_eq!(refusal_of(&cs, &*pg()), Ok(()), "{cs:?}");
        }
    }

    /// #1619: an unnamed key's index arrives under the name the engine
    /// generates, `<table>_pkey` cut to fit 63 bytes (measured on 18). A new
    /// table's default naming it is refused, as is an earlier default naming
    /// the index a key set on an existing table brings. The target is asked
    /// about the name in its own schema, where a relation holding it now
    /// makes the engine number the key's index instead.
    #[test]
    fn an_unnamed_keys_generated_index_name_arrives_with_the_key() {
        let names = |what: &str| format!("('app.{what}'::regclass)::text");
        let unnamed = || pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        };
        let keyed = |default: &str| {
            let mut t = new_table(Some(default), None, &[]);
            t.primary_key = Some(unnamed());
            t
        };
        let later = names_a_later_relation(&plan(vec![create(keyed(&names("n_pkey")))]), &*pg());
        assert_eq!(later.len(), 1, "{later:?}");
        assert!(later[0].what.contains("names app.n_pkey"), "{later:?}");
        assert_eq!(later[0].searched, ["app"]);
        // A key set on an existing table, after a default naming its index.
        let set = Change::SetPrimaryKey {
            table: TableName::new("app", "t"),
            from: None,
            to: Some(unnamed()),
            nonclustered: false,
        };
        let later =
            names_a_later_relation(&plan(vec![default_of("t", &names("t_pkey")), set]), &*pg());
        assert_eq!(later.len(), 1, "{later:?}");
        assert_eq!(later[0].searched, ["app"]);
        // The table's part is cut to fit, when it is ASCII up to the cut.
        assert_eq!(generated_key_name("n").as_deref(), Some("n_pkey"));
        assert_eq!(
            generated_key_name(&"t".repeat(63)),
            Some(format!("{}_pkey", "t".repeat(58)))
        );
        assert_eq!(
            generated_key_name(&"u".repeat(58)),
            Some(format!("{}_pkey", "u".repeat(58)))
        );
        // Negative: past the cut with other characters inside it, the name
        // depends on the database's encoding and is not told.
        assert_eq!(generated_key_name(&"ä".repeat(31)), None);
        // Negative: a named key's index is its name, not the generated one;
        // a name the plan also declares is the declared relation's, and the
        // key's index is numbered; another table's key is another name.
        let mut named = keyed(&names("n_pkey"));
        named.primary_key.as_mut().unwrap().name = Some("pk_n".into());
        for cs in [
            plan(vec![create(named)]),
            plan(vec![create(keyed(&names("m_pkey")))]),
        ] {
            assert_eq!(refusal_of(&cs, &*pg()), Ok(()), "{cs:?}");
        }
        // A string body checked against its own path, where two schemas get
        // the generated name later: either may hold it now, so the target is
        // asked about both (#1640 review).
        let body = Change::CreateModule {
            id: id("app.f()"),
            module: Box::new(module(
                ModuleKind::Function,
                "() RETURNS integer LANGUAGE sql SET search_path = app, other \
                 AS $$ SELECT ('n_pkey'::regclass)::oid::integer $$",
            )),
        };
        let key_on = |schema: &str| Change::SetPrimaryKey {
            table: TableName::new(schema, "n"),
            from: None,
            to: Some(unnamed()),
            nonclustered: false,
        };
        let later =
            names_a_later_relation(&plan(vec![body, key_on("app"), key_on("other")]), &*pg());
        assert_eq!(later.len(), 1, "{later:?}");
        assert_eq!(later[0].searched, ["pg_catalog", "app", "other"]);
        // Two tables cutting to one generated name: the first key holds it,
        // so the first table's check naming it binds that index, and the
        // second key's index is numbered.
        let long = |last: char| format!("{}{last}", "t".repeat(62));
        let pair = |table: &str, check: Option<&str>| {
            let mut t = new_table(None, check, &[]);
            t.primary_key = Some(unnamed());
            Change::CreateTable {
                uid: Uid::derived(UidKind::Table, &format!("app.{table}"), 0),
                name: TableName::new("app", table),
                table: Box::new(t),
            }
        };
        let cut = format!("{}_pkey", "t".repeat(58));
        let check = format!("{} <> ''", names(&cut));
        assert_eq!(
            refusal_of(
                &plan(vec![pair(&long('a'), Some(&check)), pair(&long('b'), None)]),
                &*pg()
            ),
            Ok(())
        );
        // Held by an index the plan declares: that index is the arrival,
        // and its name is its own, so the target is not asked.
        let mut held = new_table(Some(&names("n_pkey")), None, &[("n_pkey", None)]);
        held.primary_key = Some(unnamed());
        let later = names_a_later_relation(&plan(vec![create(held)]), &*pg());
        assert_eq!(later.len(), 1, "{later:?}");
        assert!(later[0].searched.is_empty(), "{later:?}");
    }

    /// A one-statement change's relation arrives after its own text: an
    /// index whose filter or expression names the index, and a view naming
    /// the view, are refused (#1617 review). Another relation that already
    /// arrived is not, and a new table naming itself is not (see
    /// `a_new_tables_own_indexes_arrive_after_its_expressions`).
    #[test]
    fn a_change_naming_the_relation_it_creates_is_refused() {
        let own = "('app.ix_self'::regclass)::oid > 0";
        assert!(refusal_of(&plan(vec![add_index("ix_self", Some(own))]), &*pg()).is_err());
        let mut expression = add_index("ix_self", None);
        if let Change::AddIndex { index, .. } = &mut expression {
            index.columns[0].key = pbps_model::IndexKey::Expression(format!("({own})::int"));
        }
        assert!(refusal_of(&plan(vec![expression]), &*pg()).is_err());
        assert_eq!(
            refusal_of(
                &plan(vec![
                    add_index("ix_other", None),
                    add_index("ix_self", Some("('app.ix_other'::regclass)::oid > 0")),
                ]),
                &*pg()
            ),
            Ok(())
        );
        let view = |name: &str, names: &str| Change::CreateModule {
            id: id(name),
            module: Box::new(module(
                ModuleKind::View,
                &format!("SELECT ('{names}'::regclass)::text AS x"),
            )),
        };
        assert!(refusal_of(&plan(vec![view("app.v", "app.v")]), &*pg()).is_err());
        assert_eq!(
            refusal_of(
                &plan(vec![view("app.w", "app.w0"), view("app.v", "app.w")]),
                &*pg()
            ),
            Ok(())
        );
    }

    /// A literal is a relation's name as `regclass` input reads it: unquoted
    /// parts fold to lower case, quoted ones are verbatim, and a catalog
    /// before the schema is allowed. Anything more is no name.
    #[test]
    fn a_relation_literal_is_read_as_regclass_input_reads_it() {
        let some =
            |schema: Option<&str>, name: &str| Some((schema.map(Into::into), name.to_owned()));
        assert_eq!(relation_literal("ix"), some(None, "ix"));
        assert_eq!(relation_literal(" App . IX "), some(Some("app"), "ix"));
        assert_eq!(
            relation_literal("\"App\".\"My \"\"ix\""),
            some(Some("App"), "My \"ix")
        );
        assert_eq!(relation_literal("db.app.ix"), some(Some("app"), "ix"));
        // Measured on 18 (#1589 review): an unquoted part runs to a dot or
        // white space, only ASCII letters fold, and digits alone are an OID.
        assert_eq!(relation_literal("app.a-b"), some(Some("app"), "a-b"));
        assert_eq!(relation_literal("app.IXÄ"), some(Some("app"), "ixÄ"));
        assert_eq!(relation_literal("\"1259\""), some(None, "1259"));
        // `-` exactly is OID 0; spaced or quoted, it is a name.
        assert_eq!(relation_literal(" -"), some(None, "-"));
        // A vertical tab is white space to the input; a no-break space is not.
        assert_eq!(relation_literal("pg_class\x0b"), some(None, "pg_class"));
        assert_eq!(relation_literal("app\x0b.\x0bix"), some(Some("app"), "ix"));
        assert_eq!(relation_literal("ix\u{a0}"), some(None, "ix\u{a0}"));
        assert_eq!(relation_literal("\"-\""), some(None, "-"));
        // Each part is cut to 63 bytes on a character boundary, quoted or
        // not, after folding (#1593, measured on 18).
        let a = |n: usize| "a".repeat(n);
        assert_eq!(
            relation_literal(&format!("app.{}", a(64))),
            some(Some("app"), &a(63))
        );
        assert_eq!(
            relation_literal(&format!("app.\"{}\"", a(64))),
            some(Some("app"), &a(63))
        );
        assert_eq!(
            relation_literal(&format!("app.{}", "A".repeat(64))),
            some(Some("app"), &a(63))
        );
        // Past the limit with other characters inside it, the cut depends
        // on the database's encoding, so the part is kept whole; past it with
        // ASCII up to the limit, it does not.
        let long = format!("{}éb", a(62));
        assert_eq!(
            relation_literal(&format!("app.\"{long}\"")),
            some(Some("app"), &long)
        );
        assert_eq!(
            relation_literal(&format!("app.\"{}é\"", a(63))),
            some(Some("app"), &a(63))
        );
        assert_eq!(
            relation_literal(&format!("{}.ix", "s".repeat(70))),
            some(Some(&"s".repeat(63)), "ix")
        );
        // Exactly 63 is kept whole.
        assert_eq!(relation_literal(&a(63)), some(None, &a(63)));
        for not_a_name in ["", "a b", "app.", ".ix", "a.b.c.d", "\"open", "1259", "-"] {
            assert_eq!(relation_literal(not_a_name), None, "{not_a_name:?}");
        }
    }
}
