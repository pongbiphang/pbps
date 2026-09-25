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

use std::collections::BTreeMap;

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, ColumnRef, IdsFile, ModuleId, ModuleKind, PlannedChange, Schema,
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
    found
        .values()
        .flatten()
        .filter_map(|d| match &d.holds {
            Holds::Module(x) if declared.modules.contains_key(x) && !touched(x) => Some(x.clone()),
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
                part: Part::Default(c),
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
                if let Part::Default(c) = &mut part
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
                }),
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
    Ok(cs.changes.len() - before)
}

/// Moves what this plan adds that may call a function it rebuilds to after
/// that function's create (#942, DEC-942.1). Returns how many changes moved.
///
/// [`weave`] reads the catalog, where an addition this plan makes does not
/// exist yet, so it never sees one. The differ puts a check or an index in
/// class 13 and a default in class 9, ahead of every module in class 14, which
/// everywhere else is right: a view needs its table's columns, and nothing a
/// module holds needs a constraint. With a function rebuilt, it is exactly
/// wrong: the check is created against the old function, and the rebuild's
/// `DROP FUNCTION` is refused because of it.
///
/// Which function an expression calls is not known without parsing it, and
/// the planner does not parse expressions (DECISIONS 174). So the rule is
/// positional: when the plan rebuilds a function, every addition that can
/// carry an expression goes after the last function the plan creates, in the
/// order it had. That is a check, an index with a filter (an index's columns
/// are names, so its filter is the only place a call can be), and a default
/// being set. A unique index with no filter stays where it is, since a
/// foreign key in its class may rest on it and holds no expression anyway.
///
/// One default does not move: a default on a table whose rows this plan
/// writes. Row writes come before the modules, and a row the plan inserts
/// would take the old default instead of the declared one. Leaving it in
/// place means a default that calls the rebuilt function still meets the
/// `DROP` there, and the apply fails and rolls back. That is loud, where
/// moving it would record rows the declarations did not ask for. A column
/// added with a default calling the function is the same case: the column
/// has to exist before the modules that may read it.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn after_the_rebuilds(cs: &mut ChangeSet) -> usize {
    let rebuilt = dropped_modules(cs).into_iter().any(|(id, kind)| {
        kind == ModuleKind::Function
            && cs.changes.iter().any(|p| match &p.change {
                Change::AlterModule { id: x, .. } | Change::CreateModule { id: x, .. } => *x == id,
                _ => false,
            })
    });
    if !rebuilt {
        return 0;
    }
    let Some(last) = cs.changes.iter().rposition(|p| match &p.change {
        Change::AlterModule { module, .. } | Change::CreateModule { module, .. } => {
            module.kind == ModuleKind::Function
        }
        _ => false,
    }) else {
        return 0;
    };
    let writes_rows = |table: &pbps_model::TableName| {
        cs.changes.iter().any(|p| match &p.change {
            Change::InsertRow { table: t, .. } | Change::UpdateRow { table: t, .. } => t == table,
            _ => false,
        })
    };
    let moves: Vec<bool> = cs.changes[..last]
        .iter()
        .map(|p| match &p.change {
            Change::AddCheck { .. } => true,
            Change::AddIndex { index, .. } => index.filter.is_some(),
            Change::AlterColumnDefault {
                column,
                to: Some(_),
                ..
            } => !writes_rows(&column.table),
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
    count
}

/// Every dependent of a module this plan drops that the plan does not remove
/// before that drop, named: the apply's question, asked of the saved plan as
/// it stands. Empty when the plan accounts for everything — which is what
/// [`weave`] leaves it in.
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
        Change::AddCheck {
            table: TableName::new("app", "t"),
            name: name.into(),
            constraint: CheckConstraint {
                expression: "app.f(id) >= 0".into(),
            },
        }
    }

    fn add_index(name: &str, filter: Option<&str>) -> Change {
        Change::AddIndex {
            table: TableName::new("app", "t"),
            name: name.into(),
            index: Box::new(pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    name: "id".into(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: filter.is_none(),
                filter: filter.map(Into::into),
            }),
        }
    }

    fn set_default(table: &str) -> Change {
        Change::AlterColumnDefault {
            uid: Uid::derived(UidKind::Column, &format!("app.{table}.n"), 0),
            column: ColumnRef::new(TableName::new("app", table), "n"),
            from: None,
            to: Some("app.f(1)".into()),
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
        assert_eq!(after_the_rebuilds(&mut cs), 4);
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

    /// Negatives: no function rebuilt (a view rebuilt, or nothing), and a
    /// default on a table whose rows the plan writes, which must be in place
    /// before the insert that fills it.
    #[test]
    fn nothing_moves_without_a_rebuilt_function_or_ahead_of_its_rows() {
        let (s, _) = declared();
        for changes in [
            vec![add_check("ck"), alter(&s, "app.v0")],
            vec![add_check("ck")],
        ] {
            let mut cs = plan(changes);
            let before = names(&cs);
            assert_eq!(after_the_rebuilds(&mut cs), 0);
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
                defaults: BTreeMap::new(),
                types: BTreeMap::new(),
            },
            add_check("ck"),
            alter(&s, "app.f(integer)"),
        ]);
        assert_eq!(after_the_rebuilds(&mut cs), 1);
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
