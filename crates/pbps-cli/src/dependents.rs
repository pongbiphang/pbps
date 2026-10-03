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
                    || (!as_declared(&cs.changes, d).managed(declared) && !accounted)
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
// The complement is every change that writes no rows.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn split_new_tables(
    cs: &mut ChangeSet,
    ids: &[&IdsFile],
    dialect: &dyn Dialect,
) -> usize {
    let Some(last) = last_function_create(cs) else {
        return 0;
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
    added
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

/// Places a column this plan adds whose default or generation expression
/// names a function the plan creates or rebuilds after that function's
/// create, and whatever may need the column after the column (DEC-1364.1).
/// Returns how many such columns it placed, or why no order performs them.
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
fn after_their_functions(cs: &mut ChangeSet, deps: &ModuleDeps) -> Result<usize, String> {
    let functions = function_creates(cs);
    let Some(last) = functions.iter().map(|(_, at)| *at).max() else {
        return Ok(0);
    };
    // Each such column, with the creates after it that its text names.
    let mut waits: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, p) in cs.changes[..last].iter().enumerate() {
        if let Change::AddColumn { column, .. } = &p.change
            && let Some(text) = column_expression(column)
        {
            let named: Vec<usize> = functions
                .iter()
                .filter(|(name, at)| *at > i && pbps_pg::generated::may_call(text, name))
                .map(|(_, at)| *at)
                .collect();
            if !named.is_empty() {
                waits.insert(i, named);
            }
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
        other => match (later, other.table()) {
            (Change::CreateModule { id, module } | Change::AlterModule { id, module }, Some(t)) => {
                names_table(id, &module.definition, t)
            }
            (later, Some(t)) => touches(later, t),
            _ => true,
        },
    }
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
/// A change of another table names it only as a foreign key's target.
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
    if change.table() != Some(table) {
        return change.table().is_none();
    }
    match change {
        Change::AddColumn { column: c, .. } => column_expression(c).is_some_and(named),
        Change::AlterColumnDefault { column: c, to, .. } => {
            c.name == column || to.as_deref().is_some_and(named)
        }
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
                .is_some_and(|i| i < at.drop_at);
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
                other => format!("{other:?}"),
            })
            .collect()
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
        assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), 3);
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
        assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), 2);
        assert_eq!(
            table_of(&cs).columns["v"].default.as_deref(),
            Some("app.f(1)")
        );

        // No function rebuilt: the table is left as the differ wrote it.
        let mut cs = plan(vec![create]);
        assert_eq!(split_new_tables(&mut cs, &[&ids], pg().as_ref()), 0);
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
        assert_eq!(split_new_tables(&mut cs, &[], pg().as_ref()), 1);
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
}
