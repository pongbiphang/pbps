//! The effects and the verifier, run beside an order that exists (DEC-1954.6).

use pbps_dialect::Dialect;
use pbps_model::ModuleDeps;
use pbps_model::operation::{
    Effects, Operation, OperationId, Operations, OwnerError, Owners, Requirement, Snapshot, effects,
};

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
    observe(base, desired, dialect, hints, changes, &operations);
}

/// [`observe_plan`] for operations an expansion made, in the order they run:
/// `logical` is the plan their names are in force in.
#[cfg(any(test, debug_assertions))]
pub(crate) fn observe(
    base: crate::Side<'_>,
    desired: crate::Side<'_>,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
    logical: &pbps_model::ChangeSet,
    operations: &Operations,
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
    let report = shadowed.report(&operations, &baseline, &hints.module_deps);
    let mut lines: Vec<String> = report
        .unowned
        .iter()
        .map(|(id, error)| format!("unowned {id}: {error}"))
        .collect();
    let ops = operations.as_slice();
    let (violations, any) = judge(ops, &report);
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
pub(crate) fn judge(ops: &[Operation], report: &Report) -> (Vec<String>, bool) {
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
            Violation::StillPresent { operation, key } => {
                format!("present {} needs {key:?} gone", name(*operation))
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
    let any = violations.iter().any(|v| !v.starts_with("unprovided "));
    (violations, any)
}

#[cfg(any(test, debug_assertions))]
fn sink(operations: &Operations, lines: &[String], fail: bool) {
    use std::io::Write;
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
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = file.write_all(text.as_bytes());
            }
        }
        None if fail => panic!("the effects contradict this order:\n{}", lines.join("\n")),
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
        clip(&mut text, 110);
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
