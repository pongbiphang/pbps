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
    pub symbolic: pbps_model::operation::symbolic::State,
    pub effects: Vec<Effects>,
    pub unowned: Vec<(OperationId, OwnerError)>,
}

/// Explicit operation-level analysis, including the membership contract.
/// Catalog-dependent facts remain obligations until evidence is supplied.
pub fn analyse(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    dialect: &(impl Dialect + ?Sized),
) -> Shadowed {
    analyse_with_membership(operations, owners, before, after, dialect, None, None)
}

/// Ordinary effects with connected identifier comparisons, without opting into
/// a membership contract. Temporary dependent writes can resemble column intent;
/// registered plan obligations or explicit membership analysis own that contract.
/// Missing comparisons remain unknown; a failed read is never an empty answer.
pub fn analyse_with_catalog(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    dialect: &(impl Dialect + ?Sized),
    catalog: Option<&pbps_dialect::slots::CatalogFacts>,
) -> Shadowed {
    analyse_scoped(
        operations,
        owners,
        before,
        after,
        dialect,
        catalog,
        None,
        false,
        &[],
    )
}

/// Analyse the same operations with stored membership binding evidence.
/// Missing evidence keeps individual facts unread; it is never permission to
/// synthesize an empty binding set (DEC-2296.4).
#[allow(clippy::too_many_arguments)]
pub fn analyse_with_membership(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    dialect: &(impl Dialect + ?Sized),
    catalog: Option<&pbps_dialect::slots::CatalogFacts>,
    stored: Option<&pbps_model::operation::MembershipCatalog>,
) -> Shadowed {
    analyse_scoped(
        operations,
        owners,
        before,
        after,
        dialect,
        catalog,
        stored,
        true,
        &[],
    )
}

/// The contract is activated by the plan's registered membership obligations.
/// Ordinary dependent expansion already owns its order and reads no membership
/// bindings; its temporary column writes do not register a new contract.
#[allow(clippy::too_many_arguments)]
pub(super) fn analyse_planning<S>(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    dialect: &(impl Dialect + ?Sized),
    catalog: Option<&pbps_dialect::slots::CatalogFacts>,
    stored: Option<&pbps_model::operation::MembershipCatalog>,
    plan: &super::PlanningPlan<S>,
    annotations: &[super::Constraint],
) -> Shadowed {
    analyse_scoped(
        operations,
        owners,
        before,
        after,
        dialect,
        catalog,
        stored,
        !plan.boundaries.is_empty(),
        annotations,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn analyse_scoped(
    operations: &Operations,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    dialect: &(impl Dialect + ?Sized),
    catalog: Option<&pbps_dialect::slots::CatalogFacts>,
    stored: Option<&pbps_model::operation::MembershipCatalog>,
    membership: bool,
    annotations: &[super::Constraint],
) -> Shadowed {
    use pbps_model::operation::{End, Lineage, MembershipCatalog};
    let lineage = Lineage::of(before, after);
    let payload = pbps_model::ChangeSet {
        changes: operations.iter().map(|op| op.planned().clone()).collect(),
    };
    let meetings = super::boundaries::meetings(
        crate::Side {
            schema: before.schema,
            ids: before.ids,
        },
        crate::Side {
            schema: after.schema,
            ids: after.ids,
        },
        &payload,
    );
    let mut facts = if !membership || meetings.is_empty() {
        Default::default()
    } else {
        dialect.symbolic_facts(
            before,
            after,
            stored.unwrap_or(&MembershipCatalog::default()),
        )
    };
    // New parents use the existing birth/attachment contract, not a recursive
    // write against a baseline property version that never existed.
    facts.parents.retain(|p| {
        meetings.iter().any(|m| m.parent == *p.uid())
            && lineage.family(p).is_some_and(|f| f.present(End::Base))
    });
    let family: std::collections::BTreeSet<_> = lineage
        .families()
        .filter(|(p, _)| facts.parents.contains(p))
        .flat_map(|(p, f)| std::iter::once(p).chain(f.children()))
        .cloned()
        .collect();
    for state in [&mut facts.base, &mut facts.goal] {
        state.tables.retain(|t| family.contains(t));
        state.cells.retain(|(t, _), _| family.contains(t));
        state.members.retain(|c, _| family.contains(c));
    }
    if !facts.parents.is_empty() {
        // Only explicit module births change a retained routine binding.
        #[allow(clippy::wildcard_enum_match_arm)]
        let born: std::collections::BTreeSet<_> = operations
            .iter()
            .filter_map(|op| match op.change() {
                pbps_model::Change::CreateModule { id, .. } => {
                    Some(pbps_model::operation::ObjectKey::Module(id.clone()))
                }
                pbps_model::Change::AlterModule { id, .. } if dialect.rebuilds_modules() => {
                    Some(pbps_model::operation::ObjectKey::Module(id.clone()))
                }
                _ => None,
            })
            .collect();
        for cell in facts.goal.cells.values_mut() {
            if let pbps_model::operation::symbolic::Fact::Known(value) = &mut cell.value {
                value.bindings = value
                    .bindings
                    .iter()
                    .cloned()
                    .map(|mut binding| {
                        if born.contains(&binding.input) {
                            binding.incarnation = pbps_model::operation::Incarnation::Final;
                        }
                        binding
                    })
                    .collect();
            }
        }
    }
    let mut unowned = Vec::new();
    let mut effects: Vec<_> = operations
        .iter()
        .map(|op: &Operation| {
            let portable = effects(op, owners, before, after);
            let engine = dialect
                .engine_effects(op, owners, before, after, catalog)
                .and_then(|e| {
                    if !membership {
                        return Ok(e);
                    }
                    dialect
                        .symbolic_effects(op, owners, before, after, &facts, &lineage)
                        .map(|s| e.merged(s))
                });
            match (portable, engine) {
                (Ok(p), Ok(e)) => p.merged(e),
                (Err(error), _) | (_, Err(error)) => {
                    unowned.push((op.id(), error));
                    Effects::default()
                }
            }
        })
        .collect();
    if effects.iter().any(|e| !e.symbolic.is_empty()) {
        super::boundaries::bind(
            operations,
            &mut effects,
            &Baseline::of(before, owners).with_symbolic(facts.base.clone()),
            annotations,
        );
        super::boundaries::transfers(&mut effects, &facts.base, dialect);
    }
    Shadowed {
        effects,
        unowned,
        symbolic: facts.base,
    }
}

impl Shadowed {
    /// Verifies the order the operations are in.
    pub fn report(
        self,
        operations: &Operations,
        baseline: &Baseline,
        declared: &ModuleDeps,
    ) -> Report {
        let baseline = baseline.clone().with_symbolic(self.symbolic.clone());
        let ops = operations.as_slice();
        let violations = verify_with(ops, &self.effects, &baseline, declared, Checks::ALL);
        let g = graph(ops, &self.effects, &baseline, declared, Checks::ALL);
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
    dialect: &(impl Dialect + ?Sized),
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
    /// The class order, before the shared schedule releases constraint slots
    /// and baseline bindings.
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
    dialect: &(impl Dialect + ?Sized),
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
    dialect: &(impl Dialect + ?Sized),
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
    let shadowed = analyse_with_catalog(&operations, &owners, before, after, dialect, facts);
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
                && (&u.requirement == requirement
                    || matches!((&u.requirement, requirement),
                        (Requirement::NotBoundTo { surface, .. }, Requirement::Absent(key)) if surface == key))
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
    let pending_release = if stage == Stage::Final {
        vec![false; ops.len()]
    } else {
        super::release_inputs::pending_consumers(&report.graph, ops)
    };
    let violations: Vec<String> = report
        .violations
        .iter()
        .map(|v| match v {
            Violation::Symbolic(problem) => format!("symbolic {problem}"),
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
            } if stage != Stage::Final && super::edges::connected_module_release(edge, ops) => {
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
            } if pending_release[edge.before] || pending_release[edge.after] => {
                format!(
                    "connected {} must precede {} because {:?}",
                    name(*before),
                    name(*after),
                    edge.because
                )
            }
            // The shared schedule owns this edge. Before it runs, record
            // it rather than rejecting the class order it will repair.
            Violation::Edge {
                before,
                after,
                edge,
            } if stage == Stage::BeforeSchedule
                && matches!(edge.because, Because::BeforeRewrite(_)) =>
            {
                format!(
                    "scheduled {} must precede {} because {:?}",
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
    let any = violations.iter().any(|v| {
        !v.starts_with("unprovided ")
            && !v.starts_with("connected ")
            && !v.starts_with("scheduled ")
    });
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
    dialect: &(impl Dialect + ?Sized),
    facts: Option<&pbps_dialect::slots::CatalogFacts>,
    stage: Stage,
) -> (Vec<String>, bool) {
    use super::slots::{SlotViolation, analyse, verify};
    let slots = analyse(ops, owners, before, dialect);
    let (violations, skipped) = verify(ops, &slots, dialect, facts, owners, before);
    // The class order has not visited after_the_rebuilds yet. Type and
    // constraint-trigger handovers join that existing slot settlement;
    // every subsequent replay must satisfy them (DEC-1635.3, DEC-2100.1).
    let pending: Vec<_> = if stage == Stage::BeforeSchedule {
        super::slots::edges(&slots, None, &[])
            .into_iter()
            .filter(|e| {
                matches!(e.slot.namespace, pbps_dialect::slots::Namespace::Types(_))
                    || (matches!(
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
                            | pbps_model::Change::CreateTable { .. }
                            | pbps_model::Change::AttachPartition { .. }
                    ))
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
        SlotViolation::MissingInput { .. } | SlotViolation::Contested { .. } => false,
        // Even a true collision belongs to the scheduled refusal, so the
        // differ can return its logical input without a debug-only panic.
        SlotViolation::IntermediateHeld { slot, .. } => {
            stage == Stage::BeforeSchedule
                && matches!(slot.namespace, pbps_dialect::slots::Namespace::Types(_))
        }
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
            SlotViolation::MissingInput {
                operation,
                slot,
                holder,
            } => format!("slot input {} needs {holder:?} at {slot}", name(*operation)),
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
