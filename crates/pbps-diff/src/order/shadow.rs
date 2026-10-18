//! The effects and the verifier, run beside an order that exists (DEC-1954.6).

use pbps_dialect::Dialect;
use pbps_model::ModuleDeps;
#[cfg(any(test, debug_assertions))]
use pbps_model::operation::Requirement;
use pbps_model::operation::{
    Effects, Operation, OperationId, Operations, OwnerError, Owners, Snapshot, effects,
};

#[cfg(any(test, debug_assertions))]
use super::edges::Because;
use super::edges::{Baseline, Checks, Graph, graph};
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
    let effects = operations
        .iter()
        .map(|op: &Operation| {
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
        Stage::BeforeSchedule,
        None,
    );
}

/// Which order a plan is observed at. The differ's and the schedule's are not
/// the order that runs on a connected SQL Server plan: `computed_order` moves
/// a function's change after the computed column that reads it from the
/// catalog's edges (DEC-1431.1), so the pair is recorded and not failed there.
#[cfg(any(test, debug_assertions))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The class order, before the shared schedule releases constraint slots.
    BeforeSchedule,
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
    facts: Option<&pbps_dialect::slots::CatalogFacts>,
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
        facts,
    );
}

/// [`observe_plan`] for operations an expansion made, in the order they run:
/// `logical` is the plan their names are in force in.
#[cfg(any(test, debug_assertions))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn observe(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    declared: &ModuleDeps,
    logical: &pbps_model::ChangeSet,
    operations: &Operations,
    stage: Stage,
    facts: Option<&pbps_dialect::slots::CatalogFacts>,
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
    let (slot_lines, slot_fail) = judge_slots(ops, &owners, before, dialect, facts, stage);
    lines.extend(slot_lines);
    let any = any || slot_fail;
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
        stage != Stage::Final
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

/// The name slots' findings as lines, and whether any fails the run
/// (#1964, DEC-1964.2). A claim on a held name and a broken handover fail it;
/// a comparison no fact answers is recorded as skipped, with the engine and
/// whether the plan had the catalog's facts, and fails nothing.
#[cfg(any(test, debug_assertions))]
pub(crate) fn judge_slots(
    ops: &[Operation],
    owners: &dyn Owners,
    before: Snapshot<'_>,
    dialect: &dyn Dialect,
    facts: Option<&pbps_dialect::slots::CatalogFacts>,
    stage: Stage,
) -> (Vec<String>, bool) {
    use super::slots::{SlotViolation, analyse, verify};
    let slots = analyse(ops, owners, before, dialect);
    let (violations, skipped) = verify(ops, &slots, dialect, facts, owners, before);
    // The class order has not visited after_the_rebuilds yet. Record only
    // the companion-slot handovers that pass owns as pending here; its
    // output and the final order must satisfy them (DEC-2100.1).
    let pending: Vec<_> = if stage == Stage::BeforeSchedule {
        super::slots::edges(&slots, None, &[])
            .into_iter()
            .filter(|e| {
                matches!(
                    ops[e.before].change(),
                    pbps_model::Change::AlterModule {
                        id: pbps_model::ModuleId::Trigger { .. },
                        ..
                    } | pbps_model::Change::DropModule {
                        id: pbps_model::ModuleId::Trigger { .. },
                        ..
                    }
                ) && matches!(
                    ops[e.after].change(),
                    pbps_model::Change::AddCheck { .. }
                        | pbps_model::Change::AddForeignKey { .. }
                        | pbps_model::Change::AddUnique { .. }
                        | pbps_model::Change::SetPrimaryKey { to: Some(_), .. }
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    let is_pending = |violation: &SlotViolation| match violation {
        SlotViolation::Held {
            operation, slot, ..
        } => pending
            .iter()
            .any(|e| ops[e.after].id() == *operation && e.slot == *slot),
        SlotViolation::Edge {
            before,
            after,
            slot,
        } => pending.iter().any(|e| {
            ops[e.before].id() == *before && ops[e.after].id() == *after && e.slot == *slot
        }),
        SlotViolation::Contested { .. } | SlotViolation::IntermediateHeld { .. } => false,
    };
    let name = |id: OperationId| {
        ops.iter().find(|o| o.id() == id).map_or_else(
            || id.to_string(),
            |o| {
                let mut text = format!("{id} {:?}", o.change());
                clip(&mut text, 160);
                text
            },
        )
    };
    let mode = if facts.is_some() {
        "connected"
    } else {
        "offline"
    };
    let mut lines: Vec<String> = violations
        .iter()
        .map(|v| match v {
            SlotViolation::Held {
                operation,
                slot,
                holder,
            } => format!(
                "slot held {} claims {slot} held by {holder}",
                name(*operation)
            ),
            SlotViolation::Contested {
                operation,
                slot,
                holder,
            } => format!(
                "slot contested {} claims {slot} held by {holder}",
                name(*operation)
            ),
            SlotViolation::IntermediateHeld {
                operation,
                slot,
                holder,
            } => format!(
                "slot intermediate {} passes through {slot} held by {holder}",
                name(*operation)
            ),
            SlotViolation::Edge {
                before,
                after,
                slot,
            } => format!(
                "slot edge {} must precede {} because it frees {slot}",
                name(*before),
                name(*after)
            ),
        })
        .collect();
    for (line, violation) in lines.iter_mut().zip(&violations) {
        if is_pending(violation) {
            *line = format!("scheduled {line}");
        }
    }
    let fail = lines
        .iter()
        .any(|l| !l.starts_with("slot contested ") && !l.starts_with("scheduled "));
    for s in &skipped {
        lines.push(format!(
            "slot skipped {} {mode} {} claims {} beside {}",
            dialect.name(),
            s.operation,
            s.claimed,
            s.held
        ));
    }
    for (id, error) in &slots.unowned {
        lines.push(format!("slot unowned {id}: {error}"));
    }
    for id in &slots.unlowered {
        lines.push(format!("slot unlowered {} {id}", dialect.name()));
    }
    (lines, fail)
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
