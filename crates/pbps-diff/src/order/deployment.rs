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

use crate::order::PlanningPlan;
pub mod schedule;

use pbps_model::ValueSource;
use pbps_model::part_effects::{DefaultChange, Effect, Origin, Part as DependencyPart, PartEffect};
use pbps_model::physical_sources::{ColumnAt, Located, Owner, PhysicalSources};
use pbps_model::plan_names::{At, PlanNames};

/// Each rebuild of the projection borrows the current immutable order. A
/// splice must release that view; the next lookup then sees the new boundary.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub before: crate::Side<'a>,
    pub after: &'a IdsFile,
}

impl Sources<'_> {
    pub fn project<'p>(
        &'p self,
        changes: &'p [PlannedChange],
    ) -> Result<PhysicalSources<'p>, String> {
        let names = PlanNames::of(changes, self.after).map_err(|e| e.to_string())?;
        PhysicalSources::of(names, self.before.schema, self.before.ids).map_err(|e| e.to_string())
    }
}

// The regression counts actual classifier calls through the dependent walk.
// A cache constructed but then ignored must not appear to save work.
#[inline]
fn part_effects(change: &Change) -> Vec<PartEffect<'_>> {
    change.part_effects()
}

// A cache borrows the same immutable slice as the physical-source view.
// Every splice releases both; the next view projects the new steps (DEC-1981.1).
fn projected_parts(changes: &[PlannedChange]) -> Vec<Vec<PartEffect<'_>>> {
    changes.iter().map(|p| part_effects(&p.change)).collect()
}

fn boundary(changes: &[PlannedChange], i: usize) -> At {
    if i == changes.len() {
        At::End
    } else {
        At::Before(i)
    }
}

use std::collections::{BTreeMap, BTreeSet};

use super::catalog::{Dependent, Holds, Part};
use crate::order::expand::{self, Added, Addition, Anchor, Expansion, Order, Rewrite, expand};
use crate::order::{Schedule, Source};
use pbps_dialect::Dialect;
use pbps_model::resolver::OrderReason;
use pbps_model::{
    Change, ChangeSet, ColumnRef, IdsFile, ModuleDeps, ModuleId, ModuleKind, Partitions,
    PlannedChange, Schema, TableName,
};

/// Every module this plan drops, rebuilt or not, in plan order: an
/// `AlterModule` (emitted as a drop and a create in one step) and a
/// `DropModule` (alone, or paired with a `CreateModule` of the same id).
///
/// A plain drop is here too, unlike `engine::rebuilt_modules`: a rebuild is
/// what `before_a_rebuild` is about, but a dependent refuses the `DROP`
/// whether or not a `CREATE` follows it.
// The complement is every change that does not drop a module.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn dropped_modules(changes: &ChangeSet) -> Vec<(ModuleId, ModuleKind)> {
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
/// them (`crate::diff_rebuilding`).
#[allow(clippy::wildcard_enum_match_arm)]
pub fn untouched_module_dependents(
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

// These callers ask different questions of the same part address. Parent
// reach belongs to removal above, never to exact restoration (DEC-1699.1).
#[allow(clippy::wildcard_enum_match_arm)]
fn exact_dependent(part: &DependencyPart<'_>, holds: &Holds) -> bool {
    match (part, holds) {
        (DependencyPart::Module { id, .. }, Holds::Module(module)) => *id == module,
        (
            DependencyPart::Named {
                table,
                kind: pbps_model::Part::Check,
                name,
            },
            Holds::TablePart {
                table: target,
                part: Part::Check(target_name),
            },
        )
        | (
            DependencyPart::Named {
                table,
                kind: pbps_model::Part::Index,
                name,
            },
            Holds::TablePart {
                table: target,
                part: Part::Index(target_name),
            },
        ) => *table == target && *name == target_name.raw(),
        (
            DependencyPart::Named {
                table,
                kind: pbps_model::Part::Policy,
                name,
            },
            Holds::TablePart {
                table: target,
                part: Part::Policy(target_name),
            },
        ) => *table == target && *name == target_name,
        (
            DependencyPart::Default { column, .. },
            Holds::TablePart {
                table,
                part: Part::Default(name),
            },
        )
        | (
            DependencyPart::Generated(column),
            Holds::TablePart {
                table,
                part: Part::Generated(name),
            },
        ) => column.table == *table && column.name == *name,
        _ => false,
    }
}

/// Whether `change` removes exactly this dependent.
#[allow(clippy::wildcard_enum_match_arm)]
fn removes(parts: &[PartEffect<'_>], holds: &Holds, partitions: &Partitions) -> bool {
    parts.iter().any(|fact| {
        if fact.effect != Effect::Removed
            && !matches!(
                (&fact.part, fact.effect),
                (DependencyPart::Generated(_), Effect::Rewritten)
            )
        {
            return false;
        }
        match (&fact.part, holds) {
            (
                DependencyPart::Default { column, change },
                Holds::TablePart {
                    table,
                    part: Part::Default(name),
                },
            ) => {
                column.name == *name
                    && match change {
                        DefaultChange::Partition { .. } => column.table == *table,
                        DefaultChange::Column { .. } | DefaultChange::Declaration => {
                            reaches(&column.table, table, partitions)
                        }
                    }
            }
            (
                DependencyPart::Generated(column),
                Holds::TablePart {
                    table,
                    part: Part::Generated(name),
                },
            ) => column.name == *name && reaches(&column.table, table, partitions),
            _ => exact_dependent(&fact.part, holds),
        }
    })
}

/// Whether `change` creates exactly this dependent.
///
/// Exact, unlike [`removes`]: a parent's `SET DEFAULT` gives each partition
/// the parent's default, and a partition's own comes back only through its
/// own change, which has to be the one placed after the module (#1699).
#[allow(clippy::wildcard_enum_match_arm)]
fn restores(parts: &[PartEffect<'_>], holds: &Holds, _: &Partitions) -> bool {
    parts.iter().any(|fact| {
        fact.origin == Origin::Direct
            && fact.effect == Effect::Created
            && !matches!(fact.part, DependencyPart::Generated(_))
            && exact_dependent(&fact.part, holds)
    })
}

/// Whether `change` takes the dependent away with something larger: its
/// table, or the column a default belongs to. The plan still needs the exact
/// removal before the module's drop, but a dependent it takes away this way
/// is one the declarations let go of, not one they fail to hold.
#[allow(clippy::wildcard_enum_match_arm)]
fn removes_with_its_owner(
    parts: &[PartEffect<'_>],
    holds: &Holds,
    partitions: &Partitions,
) -> bool {
    parts
        .iter()
        .any(|fact| match (&fact.part, fact.effect, holds) {
            (DependencyPart::Table(name), Effect::Removed, Holds::TablePart { table, .. }) => {
                *name == table
            }
            (
                DependencyPart::Column(column),
                Effect::Removed,
                Holds::TablePart {
                    table,
                    part: Part::Default(name) | Part::Generated(name),
                },
            ) => column.name == *name && reaches(&column.table, table, partitions),
            _ => false,
        })
}

/// The partitions of `schema` that stand through the whole plan: every one
/// but those it attaches, detaches or drops. The declarations at plan time
/// and the baseline read at apply then give the same relation, and a
/// parent's change is taken to reach only a partition it reaches wherever it
/// runs (DEC-1699.1).
#[allow(clippy::wildcard_enum_match_arm)]
pub fn standing_partitions(schema: &Schema, cs: &ChangeSet) -> Partitions {
    let moving: Vec<&TableName> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::AttachPartition { table, .. } | Change::DetachPartition { table, .. } => {
                Some(table)
            }
            Change::DropTable {
                name,
                detach_from: Some(_),
                ..
            } => Some(name),
            _ => None,
        })
        .collect();
    Partitions::of(schema).without(moving)
}

/// Whether a change to a column of `changed` reaches a part of `table`: the
/// table itself, or the parent of a partition, whose column changes the
/// engine recurses into every partition. A partition's part names the
/// partition, and the parent's change names the parent alone (DEC-1699.1).
fn reaches(changed: &TableName, table: &TableName, partitions: &Partitions) -> bool {
    changed == table || partitions.parent(table) == Some(changed)
}

/// Whether `change` edits this dependent in one step that drops and creates
/// it: a view's `AlterModule`, or a default changed from one expression to
/// another.
#[allow(clippy::wildcard_enum_match_arm)]
fn edits_in_place(parts: &[PartEffect<'_>], holds: &Holds, _: &Partitions) -> bool {
    parts.iter().any(|fact| {
        fact.effect == Effect::Rewritten
            && matches!(
                fact.part,
                DependencyPart::Module { .. } | DependencyPart::Default { .. }
            )
            && exact_dependent(&fact.part, holds)
    })
}

/// The removal and the restoration one change that edits a dependent in place
/// is made of, so each half can be put on its own side of the module: a
/// view's `AlterModule` is one step that drops and creates, and a default
/// changed from one expression to another is one statement.
#[allow(clippy::wildcard_enum_match_arm)]
fn in_place_parts(change: &Change) -> Option<(Change, Change)> {
    Some(match change.clone() {
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
        _ => return None,
    })
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
fn named_at(sources: &PhysicalSources<'_>, at: At, holds: &Holds) -> Option<Holds> {
    let Holds::TablePart { table, part } = holds else {
        return Some(holds.clone());
    };
    let (table, part) = match part {
        Part::Default(column) | Part::Generated(column) => {
            let Located::Present(here) = sources.locate(&table.column(column), at) else {
                return None;
            };
            let part = match part {
                Part::Default(_) => Part::Default(here.address.name),
                Part::Generated(_) => Part::Generated(here.address.name),
                Part::Check(_) | Part::Index(_) | Part::Policy(_) => unreachable!(),
            };
            (here.address.table, part)
        }
        Part::Check(_) | Part::Index(_) | Part::Policy(_) => {
            let Located::Present(table) = sources.locate_table(table, at) else {
                return None;
            };
            (table, part.clone())
        }
    };
    Some(Holds::TablePart { table, part })
}

/// Checks and indexes have no column owner to select. Their payload stays
/// declared while their table follows the physical source at insertion.
#[allow(clippy::wildcard_enum_match_arm)]
fn respell(p: &mut PlannedChange, holds: &Holds, dialect: &dyn Dialect) {
    let Holds::TablePart { table, .. } = holds else {
        return;
    };
    match &mut p.change {
        Change::DropCheck { table: t, .. }
        | Change::AddCheck { table: t, .. }
        | Change::DropIndex { table: t, .. }
        | Change::AddIndex { table: t, .. } => *t = table.clone(),
        _ => return,
    }
    p.risks = dialect.change_risks(&p.change);
}

#[allow(clippy::wildcard_enum_match_arm)]
fn respell_at(
    p: &mut PlannedChange,
    original: &Holds,
    sources: &PhysicalSources<'_>,
    at: At,
    dialect: &dyn Dialect,
) -> Result<(), String> {
    let here = named_at(sources, at, original).ok_or_else(|| {
        "the dependent's physical source is unavailable at its insertion boundary".to_owned()
    })?;
    let Holds::TablePart {
        table,
        part: Part::Default(column),
    } = original
    else {
        respell(p, &here, dialect);
        return Ok(());
    };
    let Located::Present(located) = sources.locate(&table.column(column), at) else {
        return Err("the default's physical source is unavailable".into());
    };
    let (from, to, fallback, origin) = match &p.change {
        Change::AlterColumnDefault { uid, from, to, .. } => {
            // A standing parent's change still recurses into its child. It
            // keeps its identity, but uses its name at the new boundary.
            if let Owner::Partition { parent } = &located.owner
                && let Some((table_uid, name)) = sources.names().column_at(uid, at)
                && let Some(table) = sources.names().table_at(table_uid, at)
                && table == parent
            {
                p.change = Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: table.column(name),
                    from: from.clone(),
                    to: to.clone(),
                };
                p.risks = dialect.change_risks(&p.change);
                return Ok(());
            }
            (from.clone(), to.clone(), None, None)
        }
        Change::SetPartitionDefault {
            from,
            to,
            fallback,
            parent,
            ..
        } => (
            from.clone(),
            to.clone(),
            fallback.clone(),
            Some(parent.clone()),
        ),
        _ => return Ok(()),
    };
    p.change = default_change(Some(located), from, to, fallback, origin)?;
    p.risks = dialect.change_risks(&p.change);
    Ok(())
}

/// Synthesis and movement share the boundary's typed ownership. A desired
/// name cannot identify an earlier holder, especially across attach/detach.
fn default_change(
    located: Option<ColumnAt>,
    from: Option<String>,
    to: Option<String>,
    fallback: Option<String>,
    origin: Option<TableName>,
) -> Result<Change, String> {
    let located =
        located.ok_or("the default's physical source is unavailable at its insertion boundary")?;
    let (column_uid, parent) = match located.owner {
        Owner::Column(uid) => (Some(uid), None),
        Owner::Partition { parent } => (None, Some(parent)),
        Owner::Unavailable => {
            return Err(
                "the default's managed owner is unavailable at its insertion boundary".into(),
            );
        }
    };
    // Ownership and expression origin are different facts. An inherited
    // default needs the parent's write path even before ATTACH; the existing
    // table-keyed change carries that origin without inventing a column UID
    // or losing it in an ordinary column's `to` expression (DEC-1875.1).
    let inherited = to.is_none() && fallback.is_some();
    if !inherited && let Some(uid) = column_uid {
        Ok(Change::AlterColumnDefault {
            uid,
            column: located.address,
            from,
            to,
        })
    } else {
        Ok(Change::SetPartitionDefault {
            uid: located.table_uid,
            table: located.address.table,
            parent: origin
                .or(parent)
                .ok_or("the default's expression origin is unavailable")?,
            column: located.address.name,
            from,
            to,
            fallback,
        })
    }
}

/// The first change matching `test` against the dependent's name at that
/// change's own position.
fn find(
    sources: &PhysicalSources<'_>,
    parts: &[Vec<PartEffect<'_>>],
    holds: &Holds,
    partitions: &Partitions,
    test: fn(&[PartEffect<'_>], &Holds, &Partitions) -> bool,
) -> Option<usize> {
    sources
        .names()
        .changes()
        .iter()
        .enumerate()
        .find_map(|(i, _)| {
            let partitions = partitions.at(sources.names(), At::End, At::Before(i));
            named_at(sources, At::Before(i), holds)
                .filter(|here| test(&parts[i], here, &partitions))
                .map(|_| i)
        })
}

/// A retired source is never the declarations' next holder of the same name.
fn as_declared(sources: &PhysicalSources<'_>, d: &Dependent) -> Option<Dependent> {
    Some(Dependent {
        described: d.described.clone(),
        holds: named_at(sources, At::End, &d.holds)?,
    })
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
    match &declared
        .tables
        .get(&of.parent)?
        .columns
        .get(column)?
        .value_source
    {
        ValueSource::Default(default) => Some(default.clone()),
        ValueSource::None
        | ValueSource::Identity(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => None,
    }
}

/// The change that removes a dependent the plan does not remove yet.
fn removal_of(
    d: &Dependent,
    declared: &Schema,
    located: Option<ColumnAt>,
) -> Result<Change, String> {
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
                let from = if let Some(of) = partition_of(declared, table) {
                    of.columns.get(column).and_then(|own| own.default.clone())
                } else {
                    declared
                        .tables
                        .get(table)
                        .and_then(|t| t.columns.get(column))
                        .and_then(|c| match &c.value_source {
                            ValueSource::Default(default) => Some(default.clone()),
                            ValueSource::None
                            | ValueSource::Identity(_)
                            | ValueSource::Sequence(_)
                            | ValueSource::Generated(_) => None,
                        })
                };
                default_change(located, from, None, None, None)?
            }
            // No statement takes a generation expression off; only the
            // plan's own expression change can, and `weave` looks for that.
            Part::Policy(_) => {
                return Err(format!(
                    "{}: explicitly remove or replace the dependent policy in the declarations",
                    d.described
                ));
            }
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
fn restoration_of(
    d: &Dependent,
    declared: &Schema,
    located: Option<ColumnAt>,
) -> Result<Change, String> {
    let unsupported = || {
        format!(
            "{}: the declarations hold it, but not in a form pbps can restore",
            d.described
        )
    };
    match &d.holds {
        Holds::Module(x) => {
            let module = declared.modules.get(x).ok_or_else(unsupported)?;
            Ok(Change::CreateModule {
                id: x.clone(),
                module: Box::new(module.clone()),
            })
        }
        Holds::TablePart { table, part } => {
            let t = declared.tables.get(table).ok_or_else(unsupported)?;
            Ok(match part {
                Part::Check(name) => Change::AddCheck {
                    table: table.clone(),
                    name: name.clone(),
                    constraint: t.checks.get(name).ok_or_else(unsupported)?.clone(),
                },
                Part::Index(name) => Change::AddIndex {
                    table: table.clone(),
                    name: name.clone(),
                    index: Box::new(t.indexes.get(name).ok_or_else(unsupported)?.clone()),
                    clustered: false,
                },
                Part::Policy(_) | Part::Generated(_) => return Err(unsupported()),
                Part::Default(column) => {
                    let (to, fallback, origin) = if let Some(of) = &t.partition_of {
                        (
                            of.columns.get(column).and_then(|own| own.default.clone()),
                            parent_default(declared, table, column.raw()),
                            Some(of.parent.clone()),
                        )
                    } else {
                        let to = match &t.columns.get(column).ok_or_else(unsupported)?.value_source
                        {
                            ValueSource::Default(default) => Some(default.clone()),
                            ValueSource::None
                            | ValueSource::Identity(_)
                            | ValueSource::Sequence(_)
                            | ValueSource::Generated(_) => None,
                        };
                        (to, None, None)
                    };
                    if to.is_none() && fallback.is_none() {
                        return Err(unsupported());
                    }
                    default_change(located, None, to, fallback, origin)?
                }
            })
        }
        Holds::Unrepresentable(_) => Err(unsupported()),
    }
}

/// Only defaults need a column owner when synthesizing a table part. A
/// missing source remains an error in `default_change`, never a new holder.
fn default_location(sources: &PhysicalSources<'_>, original: &Holds, at: At) -> Option<ColumnAt> {
    let Holds::TablePart {
        table,
        part: Part::Default(column),
    } = original
    else {
        return None;
    };
    match sources.locate(&table.column(column), at) {
        Located::Present(here) => Some(here),
        Located::Retired | Located::Unavailable => None,
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
///
/// The refusals come first. The steps are added by the rule driver, one
/// obligation at a time, with [`Weave`] as its expansion: nothing here moves
/// a change itself, `settle` does, over the edges each rewrite states
/// (DEC-1958.2).
pub fn weave(
    cs: &mut PlanningPlan,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
    sources: Sources<'_>,
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let roots = dropped_modules(cs);
    let mut refused: Vec<String> = Vec::new();
    let partitions = standing_partitions(declared, cs);
    let view = sources.project(&cs.changes)?;
    let parts = projected_parts(&cs.changes);
    for (root, _) in &roots {
        let Some(deps) = found.get(root) else {
            continue;
        };
        let blocked: Vec<Dependent> = deps
            .iter()
            .filter(|d| {
                let accounted = find(&view, &parts, &d.holds, &partitions, removes).is_some()
                    || find(&view, &parts, &d.holds, &partitions, removes_with_its_owner).is_some();
                matches!(d.holds, Holds::Unrepresentable(_))
                    || (!as_declared(&view, d).is_some_and(|d| d.managed(declared))
                        && !accounted
                        && !its_parents_once_attached(&cs.changes, &d.holds))
            })
            .cloned()
            .collect();
        if let Some(why) = super::catalog::unmanaged_refusal(root, &blocked, &Schema::default()) {
            refused.push(why);
        }
    }
    // A partition's copy of its parent's check or index is the parent's to
    // the dependents query (#1745 review). The parent's drop, placed before
    // the module's, takes every copy while the partitions are attached, and
    // its add after the module's create puts them back. A detach between the
    // two finds no copy to give the declared name, and would leave the
    // detached table without it. Two plans, as for the parent's own changes
    // (DEC-1687.1, DEC-1689.1). A table attached is its own until the attach
    // and its parent's after (`its_parents_once_attached`), and one dropped
    // goes with its copy.
    for (root, _) in &roots {
        let Some(deps) = found.get(root) else {
            continue;
        };
        for d in deps {
            let Holds::TablePart {
                table,
                part: Part::Check(_) | Part::Index(_),
            } = &d.holds
            else {
                continue;
            };
            let moving: Vec<String> = cs
                .changes
                .iter()
                .filter_map(|p| {
                    if let Change::DetachPartition {
                        table: t, parent, ..
                    } = &p.change
                        && parent == table
                    {
                        Some(t.shown().to_string())
                    } else {
                        None
                    }
                })
                .collect();
            if !moving.is_empty() {
                refused.push(format!(
                    "`{root}` is rebuilt while this plan detaches {} from {table}, whose {} \
                     calls it; rebuild it and detach in separate plans",
                    moving.join(", "),
                    d.described,
                    table = table.shown(),
                    root = root.shown()
                ));
            }
        }
    }
    if !refused.is_empty() {
        return Err(refused.join("\n\n"));
    }

    let before = cs.changes.len();
    let weave = Weave {
        roots: roots.iter().map(|(root, _)| root.clone()).collect(),
        found,
        declared,
        sources,
        dialect,
        partitions,
    };
    let given = crate::order::take(cs);

    let done = expand(given, &weave, expand::FACTOR);

    *cs = done?.changes;
    let added = cs.changes.len() - before;
    // A rebuild woven in here carries the index's declared parameters in its
    // `CREATE`, as the differ's own do (#1483 review). The parameter change
    // it subsumes is removed after the driver, which cannot remove a logical
    // change (DEC-1958.4).
    crate::order::normalize(cs);
    Ok(added)
}

fn module_order(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
    sources: Sources<'_>,
    dialect: &dyn Dialect,
) -> Result<Vec<Order>, String> {
    Weave {
        roots: dropped_modules(cs)
            .into_iter()
            .map(|(root, _)| root)
            .collect(),
        found,
        declared,
        sources,
        dialect,
        partitions: standing_partitions(declared, cs),
    }
    .order(&cs.changes)
}

/// [`weave`] as an expansion (#1958): for each dependent of each module the
/// plan drops, the steps that put it on the right side of the drop, one
/// obligation at a time in `found`'s order.
struct Weave<'a> {
    /// The modules the plan dropped before any rewrite: a split leaves a
    /// dependent view's drop behind, which is not a root of its own.
    roots: Vec<ModuleId>,
    found: &'a BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &'a Schema,
    sources: Sources<'a>,
    dialect: &'a dyn Dialect,
    partitions: Partitions,
}

/// What a dependent of a dropped module still needs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    /// One change edits it in place, which cannot be on both sides.
    Split { root: ModuleId, k: usize },
    /// Gone before the module's drop.
    Removed { root: ModuleId, k: usize },
    /// Back after the module's create.
    Restored { root: ModuleId, k: usize },
}

/// A need, with the dependent named.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Named {
    need: Need,
    described: String,
}

impl std::fmt::Display for Named {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.need {
            Need::Split { root, .. } => write!(
                f,
                "{} is edited in one step and has to be split around `{root}`",
                self.described,
                root = root.shown()
            ),
            Need::Removed { root, .. } => {
                write!(
                    f,
                    "{} has to be gone before `{root}` is dropped",
                    self.described,
                    root = root.shown()
                )
            }
            Need::Restored { root, .. } => write!(
                f,
                "{} has to come back after `{root}` is created",
                self.described,
                root = root.shown()
            ),
        }
    }
}

impl Weave<'_> {
    /// The dependents of `root`, in the order they are answered: a
    /// partitioned parent's index last, so that it is restored first. Each
    /// restoration is placed right after the module's create, ahead of the
    /// ones placed before it. Built after a partition's own index of its
    /// shape, it would take that as its clone (DEC-1688.1, #1745 review).
    /// Nothing depends on an index, so their drops may go in any order.
    fn ordered(&self, root: &ModuleId) -> Vec<(usize, &Dependent)> {
        let Some(deps) = self.found.get(root) else {
            return Vec::new();
        };
        let parents_index = |d: &Dependent| {
            matches!(&d.holds, Holds::TablePart { table, part: Part::Index(_) }
                if !self.partitions.of_parent(table).is_empty())
        };
        deps.iter()
            .enumerate()
            .filter(|(_, d)| !parents_index(d))
            .chain(deps.iter().enumerate().filter(|(_, d)| parents_index(d)))
            .collect()
    }

    fn dependent(&self, root: &ModuleId, k: usize) -> Result<&Dependent, String> {
        self.found
            .get(root)
            .and_then(|deps| deps.get(k))
            .ok_or_else(|| format!("`{root}` has no dependent {k}", root = root.shown()))
    }

    /// The first thing `d` still needs around `root`, as the plan stands.
    fn need(
        &self,
        changes: &[PlannedChange],
        root: &ModuleId,
        k: usize,
        d: &Dependent,
    ) -> Result<Option<Need>, String> {
        let view = self.sources.project(changes)?;
        let parts = projected_parts(changes);
        let partitions = &self.partitions;
        if find(&view, &parts, &d.holds, partitions, edits_in_place)
            .is_some_and(|i| in_place_parts(&changes[i].change).is_some())
        {
            return Ok(Some(Need::Split {
                root: root.clone(),
                k,
            }));
        }
        let Some(at) = span(changes, root) else {
            return Ok(None);
        };
        if attached_at(changes, &d.holds).is_some_and(|i| i < at.drop_at) {
            return Ok(None);
        }
        // Taken away with its table or column before the module goes:
        // already removed, and nothing to put back, since the declarations
        // no longer have its owner.
        if find(&view, &parts, &d.holds, partitions, removes_with_its_owner)
            .is_some_and(|i| i < at.drop_at)
        {
            return Ok(None);
        }
        // A policy explicitly removed, or removed with its table, needs
        // ordering, never an automatic rebuild. Final catalog edges place the
        // release before the routine even when the differ put it later.
        if matches!(
            d.holds,
            Holds::TablePart {
                part: Part::Policy(_),
                ..
            }
        ) && (find(&view, &parts, &d.holds, partitions, removes_with_its_owner).is_some()
            || find(&view, &parts, &d.holds, partitions, removes).is_some())
        {
            return Ok(None);
        }
        let removed_at = match find(&view, &parts, &d.holds, partitions, removes) {
            Some(i) if i < at.drop_at => i,
            Some(_) | None => {
                return Ok(Some(Need::Removed {
                    root: root.clone(),
                    k,
                }));
            }
        };
        // Its owner dropped by this plan, even after the module: then what
        // the declarations hold under the same name is the part of a new
        // table or column, created with it, and not this dependent kept.
        if find(&view, &parts, &d.holds, partitions, removes_with_its_owner).is_some() {
            return Ok(None);
        }
        let Some(kept) = as_declared(&view, d) else {
            return Ok(None);
        };
        if !kept.managed(self.declared) {
            return Ok(None);
        }
        let after = at.create_at.unwrap_or(removed_at);
        match find(&view, &parts, &d.holds, partitions, restores) {
            Some(j) if j > after => Ok(None),
            Some(_) | None => Ok(Some(Need::Restored {
                root: root.clone(),
                k,
            })),
        }
    }
}

impl Expansion for Weave<'_> {
    type Obligation = Named;

    fn rule(&self) -> &'static str {
        "weave"
    }

    fn unmet(&self, changes: &[PlannedChange]) -> Result<Vec<Named>, String> {
        let mut out = Vec::new();
        for root in &self.roots {
            for (k, d) in self.ordered(root) {
                // Placed by the drop's edges: see `order`.
                if is_generated(&d.holds) {
                    continue;
                }
                if let Some(need) = self.need(changes, root, k, d)? {
                    out.push(Named {
                        need,
                        described: d.described.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    fn expand(&self, changes: &[PlannedChange], named: &Named) -> Result<Rewrite, String> {
        let partitions = &self.partitions;
        let dialect = self.dialect;
        let view = self.sources.project(changes)?;
        let parts = projected_parts(changes);
        match &named.need {
            Need::Split { root, k } => {
                let d = self.dependent(root, *k)?;
                let at = find(&view, &parts, &d.holds, partitions, edits_in_place)
                    .ok_or_else(|| format!("{named}: nothing edits it in place"))?;
                let (removal, restoration) = in_place_parts(&changes[at].change)
                    .ok_or_else(|| format!("{named}: its edit does not split"))?;
                Ok(Rewrite::Split {
                    at,
                    parts: Box::new([planned(removal, dialect), planned(restoration, dialect)]),
                    tail: Vec::new(),
                    // Raw provenance is shared JSON and escaped by report::changes until #2084 step 5.
                    reason: format!("{} depends on `{root}`", d.described, root = root.raw()),
                })
            }
            Need::Removed { root, k } => {
                let d = self.dependent(root, *k)?;
                let at = span(changes, root).ok_or_else(|| format!("{named}: no drop"))?;
                // Raw provenance is shared JSON and escaped by report::changes until #2084 step 5.
                let reason = format!(
                    "{} depends on `{root}`, which this plan drops",
                    d.described,
                    root = root.raw()
                );
                match find(&view, &parts, &d.holds, partitions, removes) {
                    // Moved to the module's drop, ahead of everything from
                    // the drop to where the plan had it, and written with the
                    // names in force there (DEC-1958.2).
                    Some(i) => {
                        let mut moved = changes[i].clone();
                        respell_at(&mut moved, &d.holds, &view, At::Before(at.drop_at), dialect)?;
                        Ok(Rewrite::Add {
                            added: Vec::new(),
                            respelled: vec![(i, moved)],
                            edges: (at.drop_at..i)
                                .map(|c| catalog(expand::Step::Plan(i), expand::Step::Plan(c)))
                                .collect(),
                        })
                    }
                    None => {
                        // Named as it is where it goes: before the module's
                        // drop, after any rename the plan makes ahead of that.
                        let here = Dependent {
                            described: d.described.clone(),
                            holds: named_at(&view, At::Before(at.drop_at), &d.holds).ok_or_else(
                                || format!("{}: physical source is unavailable", d.described),
                            )?,
                        };
                        let final_name = as_declared(&view, d).unwrap_or_else(|| here.clone());
                        let change = removal_of(
                            &final_name,
                            self.declared,
                            default_location(&view, &d.holds, At::Before(at.drop_at)),
                        )?;
                        let mut added = planned(change, dialect);
                        respell(&mut added, &here.holds, dialect);
                        Ok(Rewrite::Add {
                            added: vec![Added::Teardown(Addition {
                                of: at.drop_at,
                                change: added,
                                anchor: Anchor::Before(at.drop_at),
                                reason,
                            })],
                            respelled: Vec::new(),
                            edges: vec![catalog(
                                expand::Step::Added(0),
                                expand::Step::Plan(at.drop_at),
                            )],
                        })
                    }
                }
            }
            Need::Restored { root, k } => {
                let d = self.dependent(root, *k)?;
                let at = span(changes, root).ok_or_else(|| format!("{named}: no drop"))?;
                let removed_at = find(&view, &parts, &d.holds, partitions, removes)
                    .ok_or_else(|| format!("{named}: it is not removed"))?;
                let after = at.create_at.unwrap_or(removed_at);
                let kept = as_declared(&view, d)
                    .ok_or_else(|| format!("{named}: the declarations do not keep it"))?;
                match find(&view, &parts, &d.holds, partitions, restores) {
                    // Moved to right after the module's create, behind
                    // everything from where the plan had it to the create.
                    Some(j) => {
                        let mut moved = changes[j].clone();
                        respell_at(
                            &mut moved,
                            &d.holds,
                            &view,
                            boundary(changes, after + 1),
                            dialect,
                        )?;
                        Ok(Rewrite::Add {
                            added: Vec::new(),
                            respelled: vec![(j, moved)],
                            edges: (j + 1..=after)
                                .map(|c| restoring(expand::Step::Plan(c), expand::Step::Plan(j)))
                                .collect(),
                        })
                    }
                    None if at.create_at.is_some() => {
                        let here_at = boundary(changes, after + 1);
                        let here = named_at(&view, here_at, &d.holds).ok_or_else(|| {
                            format!("{}: physical source is unavailable", d.described)
                        })?;
                        let change = restoration_of(
                            &kept,
                            self.declared,
                            default_location(&view, &d.holds, here_at),
                        )?;
                        let mut added = planned(change, dialect);
                        respell(&mut added, &here, dialect);
                        Ok(Rewrite::Add {
                            added: vec![Added::Restoration(Addition {
                                of: after,
                                change: added,
                                anchor: Anchor::After(after),
                                // Raw provenance is shared JSON and escaped by report::changes until #2084 step 5.
                                reason: format!(
                                    "{} depends on `{root}`, which this plan rebuilds",
                                    d.described,
                                    root = root.raw()
                                ),
                            })],
                            respelled: Vec::new(),
                            edges: vec![restoring(
                                expand::Step::Plan(after),
                                expand::Step::Added(0),
                            )],
                        })
                    }
                    None => Err(format!(
                        "`{root}` is dropped by this plan, and {} depends on it and is kept by \
                         the declarations. Remove the dependency from its declaration, or keep \
                         `{root}`, and plan again.",
                        d.described,
                        root = root.shown()
                    )),
                }
            }
        }
    }

    /// A module's drop after what releases its generated-column dependents:
    /// the expression change that stops calling it, or the drop of the column
    /// or its table (DEC-1168.1), and after the drop of a module that depends
    /// on it, which may itself follow its own releases. An edge from each, so
    /// `settle` sinks the drop to right after the last (DEC-1958.2), with the
    /// removals and restorations the rewrites placed held around it.
    ///
    /// The drop moves, not the release. A release keeps the place the differ
    /// gave it, after the column additions, renames, retypes and relaxations
    /// its new expression may need, which a release moved up to the module's
    /// drop would run ahead of. What the drop leaves behind is a function
    /// whose own body reads a column the plan drops or retypes before the
    /// release, which only the engine can tell and refuses inside the
    /// transaction. A module rebuilt in place is already after both.
    fn order(&self, changes: &[PlannedChange]) -> Result<Vec<Order>, String> {
        let view = self.sources.project(changes)?;
        let parts = projected_parts(changes);
        let partitions = &self.partitions;
        let mut edges = Vec::new();
        for root in &self.roots {
            let Some(at) = span(changes, root) else {
                continue;
            };
            // A held routine drop must take its paired create with it.
            if let Some(create) = at.create_at.filter(|i| *i != at.drop_at) {
                edges.push(catalog(
                    expand::Step::Plan(at.drop_at),
                    expand::Step::Plan(create),
                ));
            }
            for d in self.found.get(root).map_or(&[][..], Vec::as_slice) {
                let release = match &d.holds {
                    Holds::TablePart {
                        part: Part::Generated(_),
                        ..
                    } => find(&view, &parts, &d.holds, partitions, removes).or_else(|| {
                        find(&view, &parts, &d.holds, partitions, removes_with_its_owner)
                    }),
                    Holds::Module(_) => find(&view, &parts, &d.holds, partitions, removes),
                    Holds::TablePart {
                        part: Part::Policy(_),
                        ..
                    } => find(&view, &parts, &d.holds, partitions, removes).or_else(|| {
                        find(&view, &parts, &d.holds, partitions, removes_with_its_owner)
                    }),
                    Holds::TablePart { .. } | Holds::Unrepresentable(_) => None,
                };
                if let Some(i) = release.filter(|i| *i != at.drop_at) {
                    edges.push(catalog(
                        expand::Step::Plan(i),
                        expand::Step::Plan(at.drop_at),
                    ));
                }
                if is_generated(&d.holds) {
                    continue;
                }
                // What the rewrites established, re-derived, so a drop that
                // sinks after a release takes its restorations with it and
                // never passes a removal (#1958 review). Only edges the order
                // already runs forward: they hold it, never reverse it.
                if let Some(i) =
                    find(&view, &parts, &d.holds, partitions, removes).filter(|i| *i < at.drop_at)
                {
                    edges.push(catalog(
                        expand::Step::Plan(i),
                        expand::Step::Plan(at.drop_at),
                    ));
                }
                if let Some(create) = at.create_at
                    && let Some(j) =
                        find(&view, &parts, &d.holds, partitions, restores).filter(|j| *j > create)
                {
                    edges.push(restoring(expand::Step::Plan(create), expand::Step::Plan(j)));
                }
            }
        }
        Ok(edges)
    }
}

/// What the catalog says depends on what, with the order it sets.
fn catalog(before: expand::Step, after: expand::Step) -> Order {
    Order {
        before,
        after,
        source: Source::Catalog,
        reason: OrderReason::Structural,
    }
}

/// A restoration after what it is restored around.
fn restoring(before: expand::Step, after: expand::Step) -> Order {
    Order {
        before,
        after,
        source: Source::Catalog,
        reason: OrderReason::Restoration,
    }
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
fn calls_one_of(text: &str, functions: &[(String, usize)], dialect: &dyn Dialect) -> bool {
    functions
        .iter()
        .any(|(name, _)| dialect.generation_may_call(text, name))
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
    match &column.value_source {
        ValueSource::Default(default) => Some(default),
        ValueSource::Generated(generated) => Some(&generated.expression),
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => None,
    }
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
/// A new generated table's typed column parts are expanded first, retaining
/// declaration order (DEC-1503.1). Its parts finish CREATE with creation risks
/// only before the first row write in the final order (DEC-1503.4).
/// Other added checks keep the existing
/// constraint approval, where the approver sees a separately added step.
/// Unsplittable generations retain their whole-table hold when the connected
/// capability cannot install an expression on the complete column shape.
///
/// A partitioned table's default split out this way reaches every partition
/// when it is set, measured on 16 and 18, a new partition's own default
/// included: [`after_their_parents_defaults`] sets each own default again
/// after it (#1588).
// The complement is every change that writes no rows.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn split_new_tables(
    cs: &mut PlanningPlan,
    ids: &[&IdsFile],
    deps: &ModuleDeps,
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    expand_new_tables(cs, ids, deps, dialect)
}

fn expand_new_tables(
    cs: &mut PlanningPlan,
    ids: &[&IdsFile],
    deps: &ModuleDeps,
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let empty = IdsFile::default();
    let generated =
        crate::order::generated::expand(cs, ids.first().copied().unwrap_or(&empty), deps, dialect)?;
    if last_function_create(cs).is_none() {
        return Ok(0);
    }
    let taken = defaults_taken_by_rows(cs);
    let functions = function_creates(cs);
    let rule = crate::order::new_tables::NewTables {
        ids,
        functions: &functions,
        taken: &taken,
        dialect,
    };
    let before = cs.changes.len();
    *cs = expand(crate::order::take(cs), &rule, expand::FACTOR)?.changes;
    Ok(generated + cs.changes.len() - before)
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
/// A parent's new index ahead of every partition index it could take as its
/// clone, wherever the passes before left them (DEC-1688.1, #1745 review,
/// DEC-1967.2). The differ ranks the parent's first and the weave restores it
/// first, but a partition's own index that calls a rebuilt function can still
/// come ahead of the parent's restoration.
///
/// An edge from the parent's index to each partition index it could adopt
/// (`adopts`), and none to one it could not, which `settle` orders: the
/// partition's index sinks to right after the parent's. What rests on it
/// between them sinks with it, by the same edges [`rests_on`] reads: a
/// foreign key on a unique one, a literal naming it, a replica identity on
/// it. What the parent's index needs runs before it already. A partition's
/// index the parent's rests on is a cycle no order satisfies, refused by name,
/// as is one that would carry a change that could create a table, a column
/// or a module: what rests on that is not read.
pub fn before_its_partitions_indexes(
    cs: &mut PlanningPlan,
    declared: &Schema,
    dialect: &dyn Dialect,
) -> Result<(), String> {
    let adoptable_by = |parent: &TableName, index: &pbps_model::Index, c: &Change| {
        let Change::AddIndex {
            table, index: own, ..
        } = c
        else {
            return false;
        };
        declared
            .tables
            .get(table)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|of| of.parent == *parent)
            && crate::schema_diff::adopts(index, own)
    };
    // Changes a partition's index may carry past the parent's: none creates
    // a table, a column or a module.
    let crossable = |c: &Change| {
        matches!(
            c,
            Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::SetIndexStorageParameters { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::DropPolicy { .. }
                | Change::CreatePolicy { .. }
                | Change::SetRowSecurity { .. }
                | Change::SetReplicaIdentity { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. }
        )
    };
    let mut schedule = Schedule::new(cs.changes.len());
    // The stretch a partition's index may sink through: from the first one
    // ahead of a parent's that could take it, to the last such parent's.
    let mut stretch: Option<(usize, usize)> = None;
    let mut adoptions: Vec<(usize, usize)> = Vec::new();
    for (k, p) in cs.changes.iter().enumerate() {
        let Change::AddIndex {
            table: parent,
            index,
            ..
        } = &p.change
        else {
            continue;
        };
        for (j, q) in cs.changes.iter().enumerate() {
            if adoptable_by(parent, index, &q.change) {
                schedule.edge(k, j, Source::Effect, OrderReason::Structural);
                adoptions.push((k, j));
                if j < k {
                    stretch = Some(stretch.map_or((j, k), |(lo, hi)| (lo.min(j), hi.max(k))));
                }
            }
        }
    }
    let Some((lo, hi)) = stretch else {
        return Ok(());
    };
    for later in lo..=hi {
        for earlier in lo..later {
            if rests_on(
                &cs.changes[later].change,
                &cs.changes[earlier].change,
                dialect,
            ) {
                schedule.edge(earlier, later, Source::Effect, OrderReason::Structural);
            }
        }
    }
    // A change that could create a table, a column or a module is not
    // carried: `rests_on` does not read what rests on it in turn, as a caller
    // on its callee or an index on its table. An edge keeps it ahead of the
    // parent's index, so carrying it is a cycle, refused as the hoist refused
    // passing it.
    let mut held: Vec<(usize, usize)> = Vec::new();
    for &(k, j) in adoptions.iter().filter(|(k, j)| j < k) {
        for c in j + 1..k {
            if !crossable(&cs.changes[c].change) {
                schedule.edge(c, k, Source::Effect, OrderReason::Structural);
                held.push((k, c));
            }
        }
    }
    let settled = match schedule.settle(cs.clone()) {
        Ok(settled) => settled,
        Err(crate::order::Refusal::Cycle { members, .. }) => {
            // `k` is a parent's `AddIndex`, as every adoption's first end is.
            let named = |i: usize| {
                if let Change::AddIndex { table, name, .. } = &cs.changes[i].change {
                    (name.shown().to_string(), table.shown().to_string())
                } else {
                    (cs.changes[i].change.subject(), String::new())
                }
            };
            let refused = |k: usize, why: String| {
                let (name, parent) = named(k);
                format!(
                    "the index `{name}` on {parent} is built after a partition's own index it \
                     could take as its clone, and cannot be moved ahead of it: {why}; make the \
                     partition's index and this change in separate plans"
                )
            };
            // A partition's index carrying a change it may not.
            if let Some(&(k, c)) = held
                .iter()
                .find(|(k, c)| members.contains(k) && members.contains(c))
            {
                return Err(refused(
                    k,
                    format!(
                        "it would pass a change to {}",
                        cs.changes[c].change.subject()
                    ),
                ));
            }
            // The parent's index rests on a partition's it could take.
            return Err(adoptions
                .iter()
                .find(|(k, j)| members.contains(k) && members.contains(j))
                .map_or_else(
                    || {
                        "the partitioned parents' new indexes and their partitions' admit no \
                         order in which each parent's is built ahead of the partition indexes \
                         it could take; make them in separate plans"
                            .to_owned()
                    },
                    |&(k, j)| {
                        refused(
                            k,
                            format!(
                                "it rests on {}, which it could take",
                                cs.changes[j].change.subject()
                            ),
                        )
                    },
                ));
        }
        Err(refusal) => return Err(refusal.describe(cs)),
    };
    *cs = settled.changes;
    Ok(())
}

/// A partition's drop or detach after the row changes that may stop
/// referencing it (#1553, DEC-1553.1).
///
/// The engine refuses to detach a partition while a row references one of
/// its rows through a foreign key to its parent, and the drop detaches it
/// first. The differ puts both in class 6, ahead of the row updates and
/// deletes of classes 11 and 12, so a revision that deletes or repoints the
/// referencing rows and drops the partition was refused: by the pre-flight,
/// and by the engine in that order too. An edge from each update or delete
/// of a table with a foreign key to the parent puts the drop after them, and
/// `settle` sinks it there and moves nothing else.
///
/// Which rows stop referencing the partition is their content, not the
/// change's kind, so every update and delete of such a table is waited for:
/// one that leaves its reference in place is counted by the pre-flight
/// either way.
///
/// Only where the drop passes nothing that may need it done first
/// ([`passable_by_a_leaving_partition`]). A partition created or attached
/// over its range, a name it frees taken again, a row written into its
/// parent: any of them in the stretch keeps the class order, and the
/// pre-flight refuses the referencing rows by name, as before.
pub fn after_the_rows_that_leave(cs: &mut PlanningPlan, declared: &Schema) -> Result<(), String> {
    let referencing = |parent: &TableName| -> BTreeSet<&TableName> {
        declared
            .tables
            .iter()
            .filter(|(_, t)| {
                t.foreign_keys
                    .values()
                    .any(|key| key.references_table == *parent)
            })
            .map(|(name, _)| name)
            .collect()
    };
    let mut schedule = Schedule::new(cs.changes.len());
    let mut held = false;
    for (d, p) in cs.changes.iter().enumerate() {
        let (partition, parent) = if let Change::DropTable {
            name,
            detach_from: Some(parent),
            ..
        }
        | Change::DetachPartition {
            table: name,
            parent,
            ..
        } = &p.change
        {
            (name, parent)
        } else {
            continue;
        };
        let from = referencing(parent);
        let leaving: Vec<usize> = (d + 1..cs.changes.len())
            .filter(|&k| {
                matches!(
                    &cs.changes[k].change,
                    Change::UpdateRow { table, .. } | Change::DeleteRow { table, .. }
                        if from.contains(table)
                )
            })
            .collect();
        let Some(&last) = leaving.last() else {
            continue;
        };
        if !cs.changes[d + 1..last]
            .iter()
            .all(|q| passable_by_a_leaving_partition(&q.change, partition, parent))
        {
            continue;
        }
        for k in leaving {
            schedule.edge(k, d, Source::Effect, OrderReason::Data);
        }
        held = true;
    }
    if !held {
        return Ok(());
    }
    let settled = schedule
        .settle(cs.clone())
        .map_err(|refusal| refusal.describe(cs))?;
    *cs = settled.changes;
    Ok(())
}

/// Whether a partition's drop or detach may be carried past `c`
/// ([`after_the_rows_that_leave`]): a change that neither needs it done nor
/// is undone by it. A row deleted anywhere, a row written or a column changed
/// in a table other than its parent and itself, and another table's drop. A
/// row written into the parent may land in its range, a create or an attach
/// may take its range or a name it frees, and a numbering source names a
/// sequence: none of them is passed.
#[allow(clippy::wildcard_enum_match_arm)]
fn passable_by_a_leaving_partition(c: &Change, partition: &TableName, parent: &TableName) -> bool {
    let other = |table: &TableName| table != partition && table != parent;
    match c {
        Change::DeleteRow { .. } => true,
        Change::InsertRow { table, .. }
        | Change::UpdateRow { table, .. }
        | Change::AddColumn { table, .. }
        | Change::SetStorageParameters { table, .. } => other(table),
        Change::DropTable { name, .. } => other(name),
        Change::AlterColumnType { column, .. }
        | Change::AlterColumnNullability { column, .. }
        | Change::AlterColumnDefault { column, .. }
        | Change::AlterColumnExpression { column, .. }
        | Change::SetColumnDeprecated { column, .. } => other(&column.table),
        _ => false,
    }
}

/// Whether `later` must stay after `earlier`, among the changes
/// [`before_its_partitions_indexes`] may move: a literal in its text that
/// may name a relation `earlier` brings ([`relation_literal`]), a foreign
/// key on a key or unique index `earlier` adds to the table it references,
/// a replica identity on an index `earlier` adds, or a change on its own
/// table taking a name a drop there may free. Read wide, not exact: a
/// dependency imagined moves a change that could have stayed, one missed
/// breaks the plan. These are the edges a partition's index that sinks
/// carries what rests on it with (DEC-1967.2).
fn rests_on(later: &Change, earlier: &Change, dialect: &dyn Dialect) -> bool {
    let brought = relations_brought(earlier);
    if !brought.is_empty() {
        let names: Vec<String> = expressions_set(later)
            .into_iter()
            .flat_map(|(_, _, text)| dialect.lexicon().string_literals(&text))
            .filter_map(|l| relation_literal(&l).map(|(_, name)| name))
            .collect();
        if names.iter().any(|name| {
            brought.iter().any(|(_, b)| match b {
                Brought::Named(r) => r.name == *name,
                // The engine chooses the name: any may be it.
                Brought::Key(_) => true,
            })
        }) {
            return true;
        }
    }
    if let Change::AddForeignKey { constraint, .. } = later
        && let Change::AddUnique { table, .. }
        | Change::SetPrimaryKey { table, .. }
        | Change::AddIndex { table, .. } = earlier
        && *table == constraint.references_table
    {
        return true;
    }
    if let Change::SetReplicaIdentity {
        table,
        to: Some(pbps_model::ReplicaIdentity::Index(index)),
        ..
    } = later
        && let Change::AddIndex {
            table: on, name, ..
        } = earlier
        && on == table
        && name == index
    {
        return true;
    }
    fn takes_a_name(c: &Change) -> Option<&TableName> {
        if let Change::AddIndex { table, .. }
        | Change::AddUnique { table, .. }
        | Change::SetPrimaryKey { table, .. }
        | Change::AddCheck { table, .. }
        | Change::AddForeignKey { table, .. } = c
        {
            Some(table)
        } else {
            None
        }
    }
    fn frees_a_name(c: &Change) -> Option<&TableName> {
        if let Change::DropIndex { table, .. }
        | Change::DropUnique { table, .. }
        | Change::SetPrimaryKey { table, .. }
        | Change::DropCheck { table, .. }
        | Change::DropForeignKey { table, .. } = c
        {
            Some(table)
        } else {
            None
        }
    }
    takes_a_name(later).is_some_and(|t| frees_a_name(earlier) == Some(t))
}

pub fn after_their_parents_defaults(
    cs: &mut PlanningPlan,
    declared: &Schema,
    ids: &[&IdsFile],
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let before = cs.changes.len();
    let rule = crate::order::parent_defaults::ParentDefaults {
        initial: &cs.changes,
        declared,
        ids,
        dialect,
    };
    crate::order::expand::expand(cs.clone(), &rule, crate::order::expand::FACTOR).map(|expanded| {
        *cs = expanded.changes;
        cs.changes.len() - before
    })
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
fn take_public_execution(cs: &mut PlanningPlan) -> crate::order::PublicDecisions {
    crate::order::take_public_execution(cs)
}

/// Puts each `PUBLIC` decision back directly after the last `CREATE` or
/// `ALTER` of its routine, which is where DEC-687.1 orders it. A decision
/// on a routine this plan does not create or rebuild goes ahead of the roles
/// and grants, where its class leaves it.
fn settle_public_execution(cs: &mut PlanningPlan, decisions: crate::order::PublicDecisions) {
    crate::order::settle_public_execution(cs, decisions)
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
/// creates or rebuilds waits for the last function the plan creates by a
/// `Lexical` edge, and `settle` runs it right after that create, in the order
/// it had (DEC-1967.1). That is a check, an index with a filter or an expression key
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
pub fn after_the_rebuilds(
    cs: &mut PlanningPlan,
    released: &BTreeSet<ColumnRef>,
    deps: &ModuleDeps,
    declared: &pbps_model::Schema,
    mut schedule: Schedule,
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let has_handovers = !schedule.constraints().is_empty();
    // A partition delayed past its trigger's release must take its readers
    // with it. Preserve the same structural/lexical needs the existing
    // function ordering uses across each affected stretch. In particular,
    // a row still precedes the module changes, so carrying that row is a
    // named cycle rather than silently running it under a new trigger.
    let arrivals: Vec<_> = schedule
        .constraints()
        .iter()
        .filter_map(|edge| {
            (edge.after < edge.before
                && matches!(
                    cs.changes[edge.after].change,
                    Change::CreateTable { .. } | Change::AttachPartition { .. }
                ))
            .then_some((edge.after, edge.before))
        })
        .collect();
    let partitions = Partitions::of(declared);
    for (first, last) in arrivals {
        for later in first + 1..=last {
            for earlier in first..later {
                let parent = match &cs.changes[earlier].change {
                    Change::CreateTable { table, .. } => {
                        table.partition_of.as_ref().map(|of| &of.parent)
                    }
                    Change::AttachPartition { parent, .. } => Some(parent),
                    _ => None,
                };
                // Rows written through the parent may route into the new
                // partition even though they do not spell its name.
                let parent_rows = match &cs.changes[later].change {
                    Change::InsertRow { table, .. }
                    | Change::UpdateRow { table, .. }
                    | Change::DeleteRow { table, .. } => Some(table) == parent,
                    _ => false,
                };
                if parent_rows
                    || needs(
                        &cs.changes[later].change,
                        &cs.changes[earlier].change,
                        deps,
                        &partitions,
                        dialect,
                    )
                {
                    schedule.edge(earlier, later, Source::Effect, OrderReason::Structural);
                }
            }
        }
    }
    let last = last_function_create(cs);
    if last.is_none() && !has_handovers {
        return Ok(0);
    }
    let last = last.unwrap_or(0);
    let taken = defaults_taken_by_rows(cs);
    let functions = function_creates(cs);
    let calls = |text: &str| calls_one_of(text, &functions, dialect);
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
    let mut moves: Vec<bool> = cs.changes[..last]
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
    // New-table creation parts carry their own function and data edges.
    // The older last-function barrier must not move them independently.
    for (p, moves) in cs.changes[..last].iter().zip(&mut moves) {
        if crate::order::generated::is_part(p) {
            *moves = false;
        }
    }
    // Each addition that moves waits for the last function create, a
    // `Lexical` edge (#1967, DEC-1967.1): its text names a function the plan
    // creates or rebuilds, or it validates a column such text recomputes.
    // `settle` puts them right after that create, in the order they had,
    // ahead of what followed it. The barrier is the last create, not the
    // functions each names: a function may call another the plan creates
    // after it, a moved addition may rest on another, and a validator follows
    // the recomputation it judges only through the order they keep.
    for (i, _) in moves.iter().enumerate().filter(|(_, m)| **m) {
        schedule.edge(last, i, Source::Lexical, OrderReason::Structural);
    }
    let count = moves.iter().filter(|m| **m).count();
    let settled = schedule.settle(cs.clone()).map_err(|refusal| {
        if has_handovers {
            refusal.describe(cs)
        } else {
            refusal.to_string()
        }
    })?;
    let count = if has_handovers {
        crate::order::schedule::moved(&settled.sequence)
    } else {
        count
    };
    *cs = settled.changes;
    let held = after_their_functions(cs, deps, &Partitions::of(declared), dialect)?;
    crate::order::generated::settle(cs, declared, deps, dialect)?;
    Ok(count + held)
}

/// A create that moves rows out of a DEFAULT partition (#1547) is never held
/// behind a function, as [`after_their_functions`] holds a create whose own
/// default calls one the plan creates later: what must follow the move — a
/// tightening of the DEFAULT, a key into the tree, a trigger on it, a row
/// write — would be let past it, and each was found one at a time (#1763
/// review). Its own default is set instead by a `SetPartitionDefault` after
/// the last function the text may call. The move inserts every column of
/// the rows it takes, so it never evaluates the default; only rows inserted
/// after the plan do, and by then it is set.
///
/// Nor is it held with a column the parent adds behind a function (the
/// column's own default or expression calls it): its own default and NOT
/// NULL on that column are split off into changes that follow the column.
/// An own index or check naming the column cannot be, and
/// [`after_their_functions`] refuses that plan by name.
///
/// Run before [`after_the_rebuilds`], which holds what calls a function. The
/// create and what is split off it carry the dialect's risks recomputed: a
/// NOT NULL the create no longer sets is a risk of the change that does.
fn split_moving_defaults(cs: &mut PlanningPlan, declared: &Schema, dialect: &dyn Dialect) {
    let functions = function_creates(cs);
    crate::order::split_moving_defaults(cs, declared, dialect, &functions);
}

/// Places a column this plan adds whose default or generation expression
/// names a function the plan creates or rebuilds after that function's
/// create, and whatever may need the column after the column (DEC-1364.1).
/// New tables normally use creation parts (DEC-1503.1). Only a suffix
/// partition key, a generation reading a later column, or a called function
/// naming the table or its types reaches this hold
/// without a known PostgreSQL 17+ expression-replacement capability; those
/// tables keep DEC-1274.2's whole-table order. Returns how many it placed,
/// or why no order performs them.
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
pub fn after_their_functions(
    cs: &mut PlanningPlan,
    deps: &ModuleDeps,
    partitions: &Partitions,
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    function_edges(cs, deps, partitions, dialect)
}

#[allow(clippy::wildcard_enum_match_arm)]
fn function_edges(
    cs: &mut PlanningPlan,
    deps: &ModuleDeps,
    partitions: &Partitions,
    dialect: &dyn Dialect,
) -> Result<usize, String> {
    let functions = function_creates(cs);
    let Some(last) = functions.iter().map(|(_, at)| *at).max() else {
        return Ok(0);
    };
    // Each such column, with the creates after it that its text names.
    let mut waits: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, p) in cs.changes[..last].iter().enumerate() {
        // These columns belong to a new table's expansion, whose effects
        // and explicit edges permit its functions to precede the seed rows.
        if crate::order::generated::is_part(p) {
            continue;
        }
        // Unsplittable shapes keep their generation in CREATE
        // when expression replacement is unavailable (DEC-1503.1), so the
        // whole table waits. So does a new partition's own default (#1578),
        // which has no column of its own to split into an
        // `AlterColumnDefault`; one beside a DEFAULT, whose create moves
        // rows, had its default split off already (`split_moving_defaults`).
        let texts: Vec<&str> = match &p.change {
            Change::AddColumn { column, .. } => column_expression(column).into_iter().collect(),
            Change::CreateTable { table, .. } => table
                .columns
                .values()
                .filter_map(|c| match &c.value_source {
                    ValueSource::Generated(generated) => Some(generated.expression.as_str()),
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_) => None,
                })
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
                        .any(|text| dialect.generation_may_call(text, name))
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
        if !held.contains(&i)
            && held
                .range(..i)
                .any(|&j| needs(window[i], window[j], deps, partitions, dialect))
        {
            held.insert(i);
        }
    }
    // A create that moves rows is never held (`split_moving_defaults`):
    // what must follow the move would be let past it. One still held names a
    // column the parent adds behind a function in an own index or check,
    // which cannot be split off.
    if let Some(Change::CreateTable {
        name,
        beside_default: Some(default),
        ..
    }) = held.iter().map(|&i| window[i]).find(|c| {
        matches!(
            c,
            Change::CreateTable {
                beside_default: Some(_),
                ..
            }
        )
    }) {
        return Err(format!(
            "{name} is made beside the DEFAULT partition {default} and takes its rows, but an \
             index or check of its own names a column this plan adds after a function the \
             column calls, so the move would have to wait for that function and let past it \
             what must follow the move. Add the column in one plan and the partition in the next.",
            name = name.shown(),
            default = default.shown()
        ));
    }
    let mut schedule = crate::order::Schedule::new(cs.changes.len());
    let mut previous_free = None;
    for i in 0..n {
        let earlier: Vec<_> = if held.contains(&i) {
            (0..i).collect()
        } else {
            held.range(..i).copied().collect()
        };
        for j in earlier {
            if needs(window[i], window[j], deps, partitions, dialect) {
                let declared = window[i]
                    .module_id()
                    .zip(window[j].module_id())
                    .is_some_and(|(later, earlier)| {
                        deps.get(later).is_some_and(|d| d.contains(earlier))
                    });
                schedule.edge(
                    start + j,
                    start + i,
                    if declared {
                        Source::Declared
                    } else {
                        Source::Lexical
                    },
                    OrderReason::Structural,
                );
            }
        }
        if !held.contains(&i) {
            // Keep the free chain as edges. Keep is a refusal guard, not an
            // ordering constraint, and would not preserve this base order.
            if let Some(before) = previous_free {
                schedule.edge(before, start + i, Source::Lexical, OrderReason::Structural);
            }
            previous_free = Some(start + i);
        }
    }
    for (at, named) in &waits {
        for &create in named {
            schedule.edge(create, *at, Source::Lexical, OrderReason::Structural);
        }
    }

    // All edges stay inside the old window. Sink emits the unchanged prefix
    // first and the suffix last; only held steps can pass the free chain.
    let settled = schedule.settle(cs.clone()).map_err(|refusal| {
        if let crate::order::Refusal::Cycle { waiting, .. } = &refusal {
            let done: Vec<bool> = (start..=last).map(|at| !waiting.contains(&at)).collect();
            cycle(cs, start, &waits, &done, &window, deps, partitions, dialect)
        } else {
            refusal.describe(cs)
        }
    })?;
    *cs = settled.changes;
    Ok(waits.len())
}

/// Why [`after_their_functions`] found no order: the first column it could
/// not place, the functions it calls, and what may need the column.
#[allow(clippy::wildcard_enum_match_arm)]
// Keep the original diagnostic inputs together with the engine lexical question.
#[allow(clippy::too_many_arguments)]
fn cycle(
    cs: &ChangeSet,
    start: usize,
    waits: &BTreeMap<usize, Vec<usize>>,
    done: &[bool],
    window: &[&Change],
    deps: &ModuleDeps,
    partitions: &Partitions,
    dialect: &dyn Dialect,
) -> String {
    let Some((&at, named)) = waits.iter().find(|(at, _)| !done[**at - start]) else {
        return "a column this plan adds cannot be ordered against the functions it calls; \
                this is a bug in pbps, please report it"
            .into();
    };
    let column = match &cs.changes[at].change {
        Change::AddColumn { table, name, .. } => {
            format!("{table}.{name}", table = table.shown(), name = name.shown(),)
        }
        Change::CreateTable { name, table, .. } => table
            .columns
            .iter()
            .find(|(_, c)| match c.value_source {
                ValueSource::Generated(_) => true,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => false,
            })
            .map_or_else(
                || name.shown().to_string(),
                |(column, _)| {
                    format!(
                        "{name}.{column}",
                        name = name.shown(),
                        column = column.shown(),
                    )
                },
            ),
        other => format!("{other:?}"),
    };
    let module = |i: usize| {
        cs.changes[i]
            .change
            .module_id()
            .map(|id| format!("`{id}`", id = id.shown()))
    };
    let functions: Vec<String> = named.iter().filter_map(|&i| module(i)).collect();
    let blockers: Vec<String> = (0..window.len())
        .filter(|&i| {
            !done[i]
                && start + i != at
                && needs(window[i], window[at - start], deps, partitions, dialect)
        })
        .filter_map(|i| match window[i] {
            Change::InsertRow { table, .. } | Change::UpdateRow { table, .. } => Some(format!(
                "a row this plan writes into `{table}`",
                table = table.shown()
            )),
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
///   names it. A module drop and a row delete never need a new column. A
///   partitioned table's column is each partition's too, so a module or a
///   row of one of its partitions counts as one of the table's.
/// - A module created: a module that names it, in code or in a literal an
///   OID-alias type may read (`may_call`), is attached to it, shares
///   its name (overloads are ordered by `depends_on:` alone, DECISIONS 212) or
///   declares it in `depends_on:`.
/// - Any other change of a table: a module that names the table, and a change
///   of the same table or one referencing it.
/// - A row write: everything after it. A trigger the plan creates must not
///   fire on a row the plan writes before the modules.
#[allow(clippy::wildcard_enum_match_arm)]
fn needs(
    later: &Change,
    earlier: &Change,
    deps: &ModuleDeps,
    partitions: &Partitions,
    dialect: &dyn Dialect,
) -> bool {
    match earlier {
        Change::AddColumn { table, name, .. } => match later {
            Change::CreateModule { id, module } | Change::AlterModule { id, module } => partitions
                .holding(table)
                .any(|t| reads_column(id, &module.definition, t, name.raw(), dialect)),
            Change::DropModule { .. } | Change::DeleteRow { .. } => false,
            Change::InsertRow {
                table: t,
                row,
                defaults,
                ..
            } => {
                partitions.holding(table).any(|p| p == t)
                    && (row.get(name.raw()).is_some() || defaults.contains_key(name))
            }
            Change::UpdateRow {
                table: t,
                columns,
                unchanged,
                ..
            } => {
                partitions.holding(table).any(|p| p == t)
                    && (columns.contains_key(name) || unchanged.contains_key(name))
            }
            other => names_column(other, table, name.raw(), partitions, dialect),
        },
        Change::CreateModule { id: before, .. } | Change::AlterModule { id: before, .. } => {
            match later {
                Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                    let named = before
                        .referenced_name()
                        .is_some_and(|n| dialect.generation_may_call(&module.definition, &n.name));
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
            matches!(later, Change::CreateTable { table, .. } if names_new_table(table, made, dialect))
        }
        other => match (later, other.table()) {
            (Change::CreateModule { id, module } | Change::AlterModule { id, module }, Some(t)) => {
                names_table(id, &module.definition, t, dialect)
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
fn names_new_table(table: &pbps_model::Table, made: &TableName, dialect: &dyn Dialect) -> bool {
    let named = |text: &str| dialect.generation_names_table(text, made);
    let typed = |base: &str| base == made.name || base == made.raw().to_string();
    table.columns.values().any(|column| {
        typed(&column.ty.base)
            || match &column.value_source {
                ValueSource::Default(default) => named(default),
                ValueSource::Generated(generated) => named(&generated.expression),
                ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => false,
            }
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
fn reads_column(
    id: &ModuleId,
    definition: &str,
    table: &TableName,
    column: &str,
    dialect: &dyn Dialect,
) -> bool {
    dialect.may_name(definition, column) || names_table(id, definition, table, dialect)
}

/// Whether a module may name `table`: its definition holds the table's bare
/// name, or it is a trigger on it.
fn names_table(id: &ModuleId, definition: &str, table: &TableName, dialect: &dyn Dialect) -> bool {
    id.attached_to()
        .is_some_and(|on| on.schema == table.schema && on.name == table.name)
        || dialect.generation_names_table(definition, table)
}

/// Whether a change of a table may name the new column `column` of `table`.
/// A change of another table names it only as a foreign key's target, or as
/// a new partition's own entry on its parent's column.
#[allow(clippy::wildcard_enum_match_arm)]
fn names_column(
    change: &Change,
    table: &TableName,
    column: &str,
    partitions: &Partitions,
    dialect: &dyn Dialect,
) -> bool {
    let named = |text: &str| dialect.may_name(text, column);
    let listed = |columns: &[pbps_model::ColumnName]| columns.iter().any(|c| c.raw() == column);
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
                index
                    .columns
                    .iter()
                    .any(|c| c.key.column().is_some_and(|c| c.raw() == column))
                    || listed(&index.include)
                    || named(&index_text(index))
            })
            || t.checks.values().any(|check| named(&check.expression)))
    {
        return true;
    }
    // A standing partition's own default, NOT NULL, index or check on the
    // parent's column needs it too (#1692 review). A default says its parent;
    // the others are told by the declared partitions, so a change of a table
    // that is no partition of this one never waits for its column.
    let partition_own = match change {
        Change::SetPartitionDefault { parent, .. } => parent == table,
        Change::SetPartitionNotNull { .. } | Change::AddIndex { .. } | Change::AddCheck { .. } => {
            change
                .table()
                .is_some_and(|t| partitions.holding(table).skip(1).any(|p| p == t))
        }
        _ => false,
    };
    if change.table() != Some(table) && !partition_own {
        return change.table().is_none();
    }
    match change {
        Change::AddColumn { column: c, .. } => column_expression(c).is_some_and(named),
        Change::AlterColumnDefault { column: c, to, .. } => {
            c.name == column.into() || to.as_deref().is_some_and(named)
        }
        Change::SetPartitionDefault {
            column: c,
            to,
            fallback,
            ..
        } => c.raw() == column || to.as_deref().or(fallback.as_deref()).is_some_and(named),
        Change::SetPartitionNotNull { column: c, .. } => c.raw() == column,
        Change::AlterColumnExpression { column: c, to, .. } => c.name == column.into() || named(to),
        Change::AlterColumnType { column: c, .. }
        | Change::AlterColumnNullability { column: c, .. }
        | Change::SetColumnDeprecated { column: c, .. } => c.name == column.into(),
        Change::AddCheck { constraint, .. } => named(&constraint.expression),
        Change::AddIndex { index, .. } => {
            index
                .columns
                .iter()
                .any(|c| c.key.column().is_some_and(|c| c.raw() == column))
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
    // A shared type has its own identity. Treating its table-less change as
    // touching every table reverses a type-to-table slot handover.
    if matches!(
        change,
        Change::CreateType { .. } | Change::DropType { .. } | Change::RenameType { .. }
    ) {
        return false;
    }
    if crate::order::generated::needs_complete(change, table) {
        return true;
    }
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
fn released(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    partitions: &Partitions,
    view: &PhysicalSources<'_>,
) -> BTreeSet<ColumnRef> {
    let parts = projected_parts(view.names().changes());
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
            if let Some(i) = find(view, &parts, &d.holds, partitions, removes)
                && let Change::AlterColumnExpression { column, .. } = &cs.changes[i].change
            {
                out.insert(column.clone());
            }
        }
    }
    out
}

///
/// `partitions` is the relation of the schema the plan was made over: a
/// parent's column change releases its partitions' parts (DEC-1699.1).
pub fn unaccounted(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    partitions: &Partitions,
    view: &PhysicalSources<'_>,
) -> Vec<String> {
    let parts = projected_parts(view.names().changes());
    let mut out = Vec::new();
    for (root, _) in dropped_modules(cs) {
        let (Some(deps), Some(at)) = (found.get(&root), span(&cs.changes, &root)) else {
            continue;
        };
        for d in deps {
            let removed_first = find(view, &parts, &d.holds, partitions, removes)
                .or_else(|| find(view, &parts, &d.holds, partitions, removes_with_its_owner))
                .is_some_and(|i| i < at.drop_at)
                || taken_by_its_parent(&cs.changes, &d.holds, at.drop_at);
            if !removed_first {
                out.push(format!(
                    "{} depends on `{root}`",
                    d.described,
                    root = root.shown()
                ));
            }
        }
    }
    out
}

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

/// What a change brings into the relation namespace ([`relations_brought`]).
pub enum Brought {
    /// A relation under a name the plan declares.
    Named(TableName),
    /// The index of this table's unnamed primary key, whose name the engine
    /// chooses (`key_arrivals`).
    Key(TableName),
}

/// The relations a change brings into the namespace: by the name they
/// arrive under, or, for an unnamed key's index, by its table, the engine
/// choosing the name ([`Brought`]). An index's name is a relation's on
/// PostgreSQL, as is a view's.
#[allow(clippy::wildcard_enum_match_arm)]
fn relations_brought(change: &Change) -> Vec<(Step, Brought)> {
    let index = |table: &TableName, name: &str| {
        Brought::Named(TableName {
            schema: table.schema.clone(),
            name: name.to_owned(),
        })
    };
    // A key's index: its name, or the one the engine generates (#1619).
    let key = |table: &TableName, pk: &pbps_model::PrimaryKey| match &pk.name {
        Some(n) => index(table, n.raw()),
        None => Brought::Key(table.clone()),
    };
    let one = match change {
        Change::CreateTable { name, table, .. } => {
            let mut out = vec![(AT_CREATE, Brought::Named(name.clone()))];
            out.extend(
                table
                    .indexes
                    .keys()
                    .enumerate()
                    .map(|(k, n)| (index_step(k) + 1, index(name, n.raw()))),
            );
            out.extend(table.unique.keys().map(|n| (KEYS, index(name, n.raw()))));
            out.extend(table.primary_key.as_ref().map(|pk| (KEYS, key(name, pk))));
            return out;
        }
        Change::RenameTable { to, .. } => vec![Brought::Named(to.clone())],
        Change::AddIndex { table, name, .. } | Change::AddUnique { table, name, .. } => {
            vec![index(table, name.raw())]
        }
        Change::SetPrimaryKey {
            table,
            to: Some(pk),
            ..
        } => vec![key(table, pk)],
        Change::CreateModule {
            id: ModuleId::Named(name),
            module,
        } if module.kind == ModuleKind::View => {
            vec![Brought::Named(name.clone())]
        }
        _ => Vec::new(),
    };
    // A one-statement change's relation exists only once its statement has
    // run, after the statement's own text is resolved: measured on 18, an
    // index filter or expression naming the index, and a view naming the
    // view, each fail (#1617 review). Only a new table is seen by its own
    // defaults and checks, which `AT_CREATE` above keeps.
    one.into_iter().map(|b| (AT_CREATE + 1, b)).collect()
}

/// The expressions a change creates, each with its [`Step`] and what holds
/// it.
#[allow(clippy::wildcard_enum_match_arm)]
fn expressions_set(change: &Change) -> Vec<(Step, String, String)> {
    let column = |table: &TableName, name: &str, c: &pbps_model::Column| {
        column_expression(c).map(|e| {
            (
                format!("{}'s default or expression", table.column(name).shown()),
                e.to_owned(),
            )
        })
    };
    if let Change::CreateTable { name, table, .. } = change {
        let at = |step: Step| move |(whose, text): (String, String)| (step, whose, text);
        let mut out: Vec<(Step, String, String)> = table
            .columns
            .iter()
            .filter_map(|(n, c)| column(name, n.raw(), c))
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
                        .map(|d| (format!("{}'s own default", name.column(n).shown()), d))
                })
                .map(at(AT_CREATE)),
        );
        out.extend(
            table
                .checks
                .iter()
                .map(|(n, c)| {
                    (
                        format!("check {n} on {name}", name = name.shown(), n = n.shown()),
                        c.expression.clone(),
                    )
                })
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
                        format!("index {n} on {name}", name = name.shown(), n = n.shown()),
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
        } => column(table, name.raw(), c).into_iter().collect(),
        Change::AlterColumnDefault {
            column,
            to: Some(to),
            ..
        } => vec![(
            format!("{column}'s default", column = column.shown()),
            to.clone(),
        )],
        Change::SetPartitionDefault {
            table,
            column,
            to,
            fallback,
            ..
        } => to
            .as_ref()
            .or(fallback.as_ref())
            .map(|text| {
                (
                    format!("{}'s default", table.column(column).shown()),
                    text.clone(),
                )
            })
            .into_iter()
            .collect(),
        Change::AlterColumnExpression { column, to, .. } => {
            vec![(
                format!("{column}'s generation expression", column = column.shown()),
                to.clone(),
            )]
        }
        Change::AddCheck {
            table,
            name,
            constraint,
        } => vec![(
            format!(
                "check {name} on {table}",
                table = table.shown(),
                name = name.shown()
            ),
            constraint.expression.clone(),
        )],
        Change::AddIndex {
            table, name, index, ..
        } => vec![(
            format!(
                "index {name} on {table}",
                table = table.shown(),
                name = name.shown()
            ),
            index_text(index),
        )],
        Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
            vec![(format!("{id}", id = id.shown()), module.definition.clone())]
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
    let limit = 63; // PostgreSQL identifier bytes, unchanged from the engine adapter.
    if part.len() > limit && part.as_bytes()[..limit].is_ascii() {
        part.truncate(limit);
    }
    part
}
