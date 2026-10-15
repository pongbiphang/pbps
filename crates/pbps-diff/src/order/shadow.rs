//! The effects and the verifier, run beside an order that exists (DEC-1954.6).

use pbps_dialect::Dialect;
use pbps_model::ModuleDeps;
use pbps_model::operation::{
    Effects, Operation, OperationId, Operations, OwnerError, Owners, Requirement, Snapshot, effects,
};

use super::edges::{Baseline, Because, Checks, Graph, graph};
use super::verify::{Violation, verify_with};

/// Everything the effects say about one ordered plan.
#[derive(Debug, Clone)]
pub struct Report {
    pub violations: Vec<Violation>,
    pub graph: Graph,
    /// Operations whose names the models could not own, so their effects are
    /// unknown: not a plan with nothing to say.
    pub unowned: Vec<(OperationId, OwnerError)>,
}

/// The effects of every operation, portable and the dialect's.
///
/// An operation the models cannot own has no effects here, and is listed so
/// that nothing reads it as independent.
#[derive(Debug, Clone)]
pub struct Shadowed {
    pub effects: Vec<Effects>,
    pub unowned: Vec<(OperationId, OwnerError)>,
}

pub fn analyse(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    dialect: &dyn Dialect,
) -> Shadowed {
    let mut unowned = Vec::new();
    // A foreign key the plan drops and adds again (a primary-key rebuild
    // restores the keys that point at it) is absent while the rows between
    // are written, so it does not order them. Their effects are read from a
    // baseline without it; the plan's lifetime of the key is not modelled
    // beyond that (a row before the drop or after the restore is not ordered
    // by it either, which is narrow, not wrong).
    let rebuilt = rebuilt_foreign_keys(operations, owners, before);
    let without = (!rebuilt.is_empty()).then(|| {
        let mut schema = before.schema.clone();
        for (table, name) in &rebuilt {
            if let Some(table) = schema.tables.get_mut(table) {
                table.foreign_keys.remove(name);
            }
        }
        schema
    });
    let effects = operations
        .iter()
        .map(|op: &Operation| {
            let before = match (&without, op.change()) {
                (
                    Some(schema),
                    pbps_model::Change::InsertRow { .. }
                    | pbps_model::Change::UpdateRow { .. }
                    | pbps_model::Change::DeleteRow { .. },
                ) => Snapshot {
                    schema,
                    ids: before.ids,
                },
                _ => before,
            };
            let portable = effects(op, owners, before, after);
            let engine = dialect.engine_effects(op, owners, before, after);
            match (portable, engine) {
                (Ok(p), Ok(e)) => p.merged(e),
                (Err(error), _) | (_, Err(error)) => {
                    unowned.push((op.id(), error));
                    Effects::default()
                }
            }
        })
        .collect();
    Shadowed { effects, unowned }
}

/// The baseline's foreign keys, by table and name, that the plan both drops
/// and adds.
fn rebuilt_foreign_keys(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
) -> std::collections::BTreeSet<(pbps_model::TableName, String)> {
    use pbps_model::Change;
    let mut dropped = std::collections::BTreeSet::new();
    let mut added = std::collections::BTreeSet::new();
    for op in operations.iter() {
        let (Change::DropForeignKey { table, name } | Change::AddForeignKey { table, name, .. }) =
            op.change()
        else {
            continue;
        };
        // Named by the baseline's spelling of the table.
        let Ok(uid) = owners.table(table, op.origin().spelling()) else {
            continue;
        };
        let Some(base) = before.ids.tables.get(&uid) else {
            continue;
        };
        let set = if matches!(op.change(), Change::DropForeignKey { .. }) {
            &mut dropped
        } else {
            &mut added
        };
        set.insert((base.clone(), name.clone()));
    }
    dropped.intersection(&added).cloned().collect()
}

impl Shadowed {
    /// Verifies the order the operations are in.
    pub fn report(
        self,
        operations: &Operations,
        baseline: &Baseline,
        declared: &ModuleDeps,
    ) -> Report {
        let ops = operations.as_slice();
        let violations = verify_with(ops, &self.effects, baseline, declared, Checks::ALL);
        let g = graph(ops, &self.effects, baseline, declared, Checks::ALL);
        Report {
            violations,
            graph: g,
            unowned: self.unowned,
        }
    }
}

/// Runs the effects and the verifier on the order the differ produced, in
/// tests and debug builds only (DEC-1954.6).
///
/// With `PBPS_ORDER_SHADOW` set to a path it appends one line per finding to
/// that file and never panics: how the go/no-go report is measured. Without
/// it a violation panics, so a declared effect an order contradicts fails the
/// test that produced the order.
#[cfg(any(test, debug_assertions))]
pub(crate) fn observe_plan(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
    changes: &pbps_model::ChangeSet,
) {
    use pbps_model::operation::ModuleEdits;
    let modes = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let operations = Operations::of_plan(&changes.changes, modes);
    observe(
        base,
        desired,
        dialect,
        &hints.module_deps,
        changes,
        &operations,
        Stage::Differ,
    );
}

/// Which order a plan is observed at. The differ's and the schedule's are not
/// the order that runs on a connected SQL Server plan: `computed_order` moves
/// a function's change after the computed column that reads it from the
/// catalog's edges (DEC-1431.1), so the pair is recorded and not failed there.
#[cfg(any(test, debug_assertions))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Before the connected passes: a computed column and its function may
    /// still be in the wrong order.
    Differ,
    /// The order that runs: every pass has finished (S3, #1954).
    Final,
}

/// The verifier on the order a caller outside this crate holds, in tests and
/// debug builds only: `schedule`'s output (`Stage::Differ` on a connected
/// plan, whose last pass is still to come) and the order a connected plan
/// finally runs (`Stage::Final`, where nothing is exempt).
#[cfg(any(test, debug_assertions))]
pub fn observe_scheduled(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    declared: &ModuleDeps,
    changes: &pbps_model::ChangeSet,
    stage: Stage,
) {
    use pbps_model::operation::ModuleEdits;
    let modes = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let operations = Operations::of_plan(&changes.changes, modes);
    observe(
        base,
        desired,
        dialect,
        declared,
        changes,
        &operations,
        stage,
    );
}

/// [`observe_plan`] for operations an expansion made, in the order they run:
/// `logical` is the plan their names are in force in.
#[cfg(any(test, debug_assertions))]
pub(crate) fn observe(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    declared: &ModuleDeps,
    logical: &pbps_model::ChangeSet,
    operations: &Operations,
    stage: Stage,
) {
    use pbps_model::operation::PlanOwners;
    use pbps_model::plan_names::PlanNames;
    let operations = operations.clone();
    let names = match PlanNames::of(&logical.changes, desired.ids) {
        Ok(names) => names,
        Err(error) => {
            sink(&operations, &[format!("names refused: {error}")], false);
            return;
        }
    };
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let (before, after) = (
        Snapshot {
            schema: base.schema,
            ids: base.ids,
        },
        Snapshot {
            schema: desired.schema,
            ids: desired.ids,
        },
    );
    let baseline = Baseline::of(before, &owners);
    let shadowed = analyse(&operations, &owners, before, after, dialect);
    let report = shadowed.report(&operations, &baseline, declared);
    let mut lines: Vec<String> = report
        .unowned
        .iter()
        .map(|(id, error)| format!("unowned {id}: {error}"))
        .collect();
    let ops = operations.as_slice();
    let (violations, any) = judge(ops, &report, stage);
    lines.extend(violations);
    if std::env::var_os("PBPS_ORDER_SHADOW").is_some() {
        let kind = |i: usize| {
            format!("{:?}", ops[i].change())
                .split([' ', '{'])
                .next()
                .unwrap_or("")
                .to_owned()
        };
        if report.graph.has_cycle(ops.len()) {
            lines.push("cycle in the effect graph".to_owned());
        }
        for e in &report.graph.edges {
            if matches!(e.because, super::edges::Because::WriteWrite(_)) {
                lines.push(format!("ww {} {}", kind(e.before), kind(e.after)));
            }
        }
        lines.push(format!(
            "ambiguous {} unmet {}",
            report.graph.ambiguous.len(),
            report.graph.unmet.len()
        ));
    }
    sink(&operations, &lines, any);
}

/// Cuts `text` to at most `max` bytes without splitting a character: a
/// module or table name may be non-ASCII, and `String::truncate` panics inside
/// one. A diagnostic must never fail the plan it describes.
#[cfg(any(test, debug_assertions))]
pub(crate) fn clip(text: &mut String, max: usize) {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// The verifier's findings as lines, and whether any of them fails the run.
#[cfg(any(test, debug_assertions))]
pub(crate) fn judge(ops: &[Operation], report: &Report, stage: Stage) -> (Vec<String>, bool) {
    let find = |id: OperationId| ops.iter().find(|o| o.id() == id);
    let name = |id: OperationId| {
        find(id).map_or_else(
            || id.to_string(),
            |o| {
                let mut text = format!("{id} {:?}", o.change());
                clip(&mut text, 160);
                text
            },
        )
    };
    // An obligation that nothing in the plan provides and the baseline lacks
    // cannot be met by any order: the input disagrees with itself (for example
    // an identity file naming a column the base declarations lack). It is
    // recorded, but only an order that could be wrong fails the run.
    let unprovided = |operation: OperationId, requirement: &Requirement| {
        report.graph.unmet.iter().any(|u| {
            ops.get(u.operation).is_some_and(|o| o.id() == operation)
                && &u.requirement == requirement
        })
    };
    // A function a computed column calls is moved by the connected pass
    // (`computed_order.rs`, DEC-1431.1) after the differ made this order, from
    // the catalog's edges; the differ's own order for that pair is not the one
    // that runs. Recorded as `connected`, and not failed, until the verifier
    // runs on the order that runs.
    let module_op = |id: OperationId| {
        stage == Stage::Differ
            && find(id).is_some_and(|o| {
                matches!(
                    o.change(),
                    pbps_model::Change::AlterModule { .. } | pbps_model::Change::DropModule { .. }
                )
            })
    };
    let computed = |key: &pbps_model::operation::ObjectKey| {
        matches!(
            key,
            pbps_model::operation::ObjectKey::Part {
                part: pbps_model::operation::PartKey::Computed(_),
                ..
            }
        )
    };
    let violations: Vec<String> = report
        .violations
        .iter()
        .map(|v| match v {
            Violation::Unmet {
                operation,
                requirement,
            } if unprovided(*operation, requirement) => {
                format!("unprovided {} needs {requirement:?}", name(*operation))
            }
            Violation::Unmet {
                operation,
                requirement,
            } => format!("unmet {} needs {requirement:?}", name(*operation)),
            Violation::StillPresent { operation, key }
                if unprovided(*operation, &Requirement::Absent(key.clone())) =>
            {
                format!("unprovided {} needs {key:?} gone", name(*operation))
            }
            Violation::StillPresent { operation, key }
                if module_op(*operation) && computed(key) =>
            {
                format!("connected {} needs {key:?} gone", name(*operation))
            }
            Violation::StillPresent { operation, key } => {
                format!("present {} needs {key:?} gone", name(*operation))
            }
            Violation::Edge {
                before,
                after,
                edge,
            } if (module_op(*before) || module_op(*after))
                && matches!(
                    &edge.because,
                    Because::Frees(key) | Because::Precedes(key) if computed(key)
                ) =>
            {
                format!(
                    "connected {} must precede {} because {:?}",
                    name(*before),
                    name(*after),
                    edge.because
                )
            }
            Violation::Edge {
                before,
                after,
                edge,
            } => format!(
                "edge {} must precede {} because {:?}",
                name(*before),
                name(*after),
                edge.because
            ),
            Violation::BrokenDeclared { before, after } => {
                format!("declared {} must precede {}", name(*before), name(*after))
            }
        })
        .collect();
    let any = violations
        .iter()
        .any(|v| !v.starts_with("unprovided ") && !v.starts_with("connected "));
    (violations, any)
}

/// Appends `text` to the report file, creating it.
#[cfg(any(test, debug_assertions))]
pub(crate) fn write_report(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

#[cfg(any(test, debug_assertions))]
fn sink(operations: &Operations, lines: &[String], fail: bool) {
    if lines.is_empty() {
        return;
    }
    let who = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    match std::env::var_os("PBPS_ORDER_SHADOW") {
        Some(path) => {
            // One write per call, so that threads do not interleave lines.
            let text: String = lines
                .iter()
                .map(|line| {
                    let line = line.replace(['\n', '\t'], " ");
                    format!("{who}\t{}\t{line}\n", operations.len())
                })
                .collect();
            // Lost evidence is not an empty report: the failure is shown, and
            // fails the run that was measuring.
            if let Err(error) = write_report(std::path::Path::new(&path), &text) {
                eprintln!(
                    "PBPS_ORDER_SHADOW: cannot write {}: {error}",
                    path.to_string_lossy()
                );
                panic!("the order report is incomplete: {error}");
            }
        }
        None if fail => {
            // The order itself, so the finding can be read without a rerun.
            let order: Vec<String> = operations
                .as_slice()
                .iter()
                .map(|op| {
                    let mut text = format!("{} {:?}", op.id(), op.change());
                    clip(&mut text, 330);
                    text
                })
                .collect();
            panic!(
                "the effects contradict this order:\n{}\nthe order:\n{}",
                lines.join("\n"),
                order.join("\n")
            )
        }
        None => {}
    }
}

/// Classifies the resolver graph's constraints against the effects' edges,
/// over the steps in the order the graph numbered them. Written to the
/// `PBPS_ORDER_SHADOW` file only; it never fails a test.
#[cfg(any(test, debug_assertions))]
pub(crate) fn observe_constraints(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
    logical: &pbps_model::ChangeSet,
    operations: &Operations,
    constraints: &std::collections::BTreeSet<pbps_model::resolver::OrderEdge>,
) {
    use pbps_model::operation::PlanOwners;
    use pbps_model::plan_names::PlanNames;
    if std::env::var_os("PBPS_ORDER_SHADOW").is_none() {
        return;
    }
    let Ok(names) = PlanNames::of(&logical.changes, desired.ids) else {
        return;
    };
    let owners = PlanOwners::new(names, base.ids, desired.ids);
    let before = Snapshot {
        schema: base.schema,
        ids: base.ids,
    };
    let after = Snapshot {
        schema: desired.schema,
        ids: desired.ids,
    };
    let baseline = Baseline::of(before, &owners);
    let shadowed = analyse(operations, &owners, before, after, dialect);
    let g = graph(
        operations.as_slice(),
        &shadowed.effects,
        &baseline,
        &hints.module_deps,
        Checks::ALL,
    );
    let (classified, added) =
        super::compare::classify(operations, constraints, &g, &shadowed.effects);
    let ops = operations.as_slice();
    let name = |i: usize| {
        let mut text = format!("{}:{:?}", ops[i].id(), ops[i].change());
        clip(&mut text, 330);
        text
    };
    let mut lines = Vec::new();
    for c in &classified {
        lines.push(format!(
            "cmp {:?} {:?} | {} -> {} | {}",
            c.class,
            c.edge.reason,
            name(c.edge.before),
            name(c.edge.after),
            c.why
        ));
    }
    for (a, b, because) in &added {
        lines.push(format!(
            "added {:?} | {} -> {}",
            because,
            name(*a),
            name(*b)
        ));
    }
    sink(operations, &lines, false);
}
