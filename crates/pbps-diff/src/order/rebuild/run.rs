//! Non-executable Split/Add expansion over the prepared inventory (DEC-2298.8/.9).
use super::*;
use crate::order::{
    self, PlanningPlan, Source,
    expand::{self, Added, Addition, Anchor, Expansion, Order, Rewrite, Step},
};
use pbps_model::resolver::OrderReason;
use std::fmt;

#[derive(Debug)]
pub struct Expanded {
    pub changes: PlanningPlan,
    pub operations: Operations,
    pub target: ValidIds,
    pub addresses: Vec<IntermediateAddress>,
    pub rewrites: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Need {
    Split,
    Part(usize),
}
impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Split => f.write_str("split the recorded blocking rewrite"),
            Self::Part(i) => write!(f, "provide rebuild part {i}"),
        }
    }
}
fn added(purpose: Purpose) -> Option<bool> {
    match purpose {
        Purpose::Teardown => Some(true),
        Purpose::Restoration => Some(false),
        Purpose::Tree
        | Purpose::Copy
        | Purpose::Own
        | Purpose::Analyze
        | Purpose::Swap
        | Purpose::Retire => None,
    }
}
struct Rule<'a> {
    prepared: Prepared,
    input: &'a Input<'a>,
    dialect: &'a dyn Dialect,
    canonical: Operations,
}
impl Rule<'_> {
    fn select(&self, plan: &[PlannedChange]) -> Result<(Operations, Vec<usize>), String> {
        let mut seen = BTreeSet::new();
        let mut indices = vec![];
        for p in plan {
            let i = self
                .prepared
                .parts
                .iter()
                .position(|part| part.change.change == p.change)
                .ok_or("unrelated rebuild part")?;
            if !seen.insert(i)
                || p.risks != self.prepared.parts[i].change.risks
                || p.risks != self.dialect.change_risks(&p.change)
                || p.strategy != Strategy::default()
            {
                return Err("duplicate rebuild part or changed risk/mode".into());
            }
            indices.push(i);
        }
        Ok((
            Operations::from_operations(
                indices
                    .iter()
                    .map(|&i| self.canonical.as_slice()[i].clone()),
            )
            .with_steps(plan),
            indices,
        ))
    }
    fn effects(&self, ops: &Operations) -> Result<Vec<Effects>, String> {
        let owners = self.prepared.owners(self.input)?;
        let before = Snapshot {
            schema: self.input.before.schema,
            ids: self.input.before.ids,
        };
        let after = Snapshot {
            schema: self.input.desired.schema,
            ids: &self.prepared.target,
        };
        ops.iter()
            .map(|op| {
                Ok(effects(op, &owners, before, after)?
                    .merged(self.dialect.engine_effects(op, &owners, before, after)?))
            })
            .collect::<Result<Vec<_>, OwnerError>>()
            .map_err(|e| e.to_string())
    }
    fn slots(&self, indices: &[usize]) -> slots::Slots {
        let mut slots = self.prepared.slots.clone();
        slots.effects = indices.iter().map(|&i| slots.effects[i].clone()).collect();
        slots
    }
    fn key(&self, d: &Dependent, spelling: Spelling) -> Result<Option<ObjectKey>, String> {
        if spelling == Spelling::Final
            && matches!(&d.holds,Holds::TablePart {table,..} if !self.input.desired.schema.tables.contains_key(table))
        {
            return Ok(None);
        }
        let view_ids = crate::rebuild::logical_view(self.input.desired.ids, self.input.root);
        let sources = order::deployment::Sources {
            before: self.input.before,
            after: &view_ids,
        };
        let view = sources.project(&self.prepared.logical)?;
        let change = order::deployment::removal_of(
            d,
            self.input.before.schema,
            order::deployment::default_location(&view, &d.holds, pbps_model::plan_names::At::Start),
        )?;
        let origin = match spelling {
            Spelling::Base => Origin::Teardown(OriginSource::Change(0)),
            Spelling::Final => Origin::Restoration(OriginSource::Change(0)),
            Spelling::At(_) => return Err("catalog surface needs an endpoint".into()),
        };
        let prototype = Operations::expanded(
            1,
            [(origin, PlannedChange::new(change))],
            ModuleEdits::Rebuilt,
        );
        prototype.as_slice()[0]
            .key(&self.prepared.owners(self.input)?)
            .map(Some)
            .map_err(|e| e.to_string())
    }
    fn replay(&self, plan: &[PlannedChange]) -> Result<Operations, String> {
        let (ops, indices) = self.select(plan)?;
        let owners = self.prepared.owners(self.input)?;
        let before = Snapshot {
            schema: self.input.before.schema,
            ids: self.input.before.ids,
        };
        let violations = order::verify::verify(
            ops.as_slice(),
            &self.effects(&ops)?,
            &order::Baseline::of(before, &owners),
            &self.input.hints.module_deps,
        );
        let (names, skipped) = slots::verify(
            ops.as_slice(),
            &self.slots(&indices),
            self.dialect,
            Some(&self.prepared.names),
            &owners,
            before,
        );
        if !violations.is_empty() || !names.is_empty() || !skipped.is_empty() {
            return Err(format!(
                "rebuild replay refused: {violations:?} {names:?} {skipped:?}"
            ));
        }
        Ok(ops)
    }
}
impl Expansion for Rule<'_> {
    type Obligation = Need;
    fn rule(&self) -> &'static str {
        "partitioned-table-rebuild"
    }
    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<Need>, String> {
        let root = plan.len() == 1 && plan[0].change == self.prepared.logical[0].change;
        let indices = if root {
            if plan[0].risks != self.dialect.change_risks(&plan[0].change)
                || plan[0].strategy != Strategy::default()
            {
                return Err("changed rebuild root risk/mode".into());
            }
            vec![]
        } else {
            self.select(plan)?.1
        };
        let mut needs = if root { vec![Need::Split] } else { vec![] };
        for (i, p) in self.prepared.parts.iter().enumerate() {
            if !indices.contains(&i) {
                if !root && added(p.purpose).is_none() {
                    return Err("rebuild lost a logical split part".into());
                }
                if added(p.purpose).is_some() {
                    needs.push(Need::Part(i));
                }
            }
        }
        Ok(needs)
    }
    fn expand(&self, plan: &[PlannedChange], need: &Need) -> Result<Rewrite, String> {
        Ok(match need {
            Need::Split => {
                let mut parts = self
                    .prepared
                    .parts
                    .iter()
                    .filter(|p| added(p.purpose).is_none())
                    .map(|p| p.change.clone());
                Rewrite::Split {
                    at: 0,
                    parts: Box::new([
                        parts.next().ok_or("empty rebuild split")?,
                        parts.next().ok_or("single rebuild split")?,
                    ]),
                    tail: parts.collect(),
                    reason: need.to_string(),
                }
            }
            Need::Part(i) => {
                let p = &self.prepared.parts[*i];
                let a = Addition {
                    of: 0,
                    change: p.change.clone(),
                    anchor: Anchor::After(plan.len() - 1),
                    reason: format!("{}: {}", need, p.change.change.subject()),
                };
                Rewrite::Add {
                    added: vec![match added(p.purpose) {
                        Some(true) => Added::Teardown(a),
                        Some(false) => Added::Restoration(a),
                        None => return Err("split part cannot be an addition".into()),
                    }],
                    respelled: vec![],
                    edges: vec![],
                }
            }
        })
    }
    fn order(&self, plan: &[PlannedChange]) -> Result<Vec<Order>, String> {
        let (ops, indices) = self.select(plan)?;
        let owners = self.prepared.owners(self.input)?;
        let before = Snapshot {
            schema: self.input.before.schema,
            ids: self.input.before.ids,
        };
        let graph = order::graph(
            ops.as_slice(),
            &self.effects(&ops)?,
            &order::Baseline::of(before, &owners),
            &self.input.hints.module_deps,
            order::Checks::ALL,
        );
        if !graph.unmet.is_empty() || !graph.ambiguous.is_empty() {
            return Err(format!(
                "incomplete rebuild requirements: {:?} {:?}",
                graph.unmet, graph.ambiguous
            ));
        }
        let mut out: Vec<_> = graph
            .edges
            .iter()
            .map(|e| Order {
                before: Step::Plan(e.before),
                after: Step::Plan(e.after),
                source: e.source,
                reason: order::schedule::reason(e, ops.as_slice()),
            })
            .collect();
        out.extend(
            slots::edges(&self.slots(&indices), Some(&self.prepared.names), &[])
                .iter()
                .map(|e| Order {
                    before: Step::Plan(e.before),
                    after: Step::Plan(e.after),
                    source: Source::Effect,
                    reason: OrderReason::Identity,
                }),
        );
        let positions = |purpose| {
            indices
                .iter()
                .enumerate()
                .filter_map(|(at, &i)| (self.prepared.parts[i].purpose == purpose).then_some(at))
                .collect::<Vec<_>>()
        };
        let mut edge = |a, b, source| {
            if a != b {
                out.push(Order {
                    before: Step::Plan(a),
                    after: Step::Plan(b),
                    source,
                    reason: OrderReason::Restoration,
                });
            }
        };
        let copy = positions(Purpose::Copy)[0];
        let analyze = positions(Purpose::Analyze)[0];
        let own = positions(Purpose::Own);
        let swaps = positions(Purpose::Swap);
        let retire = positions(Purpose::Retire);
        edge(copy, analyze, Source::Effect);
        for &part in &own {
            edge(copy, part, Source::Effect);
            edge(part, analyze, Source::Effect);
        }
        for &parent in &own {
            if matches!(
                ops.as_slice()[parent].change(),
                Change::BuildPartitionedIndex(_)
            ) {
                for &leaf in &own {
                    if matches!(ops.as_slice()[leaf].change(), Change::AddIndex { .. }) {
                        edge(parent, leaf, Source::Effect);
                    }
                }
            }
        }
        for &swap in &swaps {
            edge(analyze, swap, Source::Effect);
            for release in positions(Purpose::Teardown) {
                edge(release, swap, Source::Effect);
            }
            for restore in positions(Purpose::Restoration) {
                edge(swap, restore, Source::Effect);
            }
        }
        for (i, &part) in indices.iter().enumerate() {
            if self.prepared.parts[part].purpose != Purpose::Retire {
                for &drop in &retire {
                    edge(i, drop, Source::Effect);
                }
            }
        }
        let old = self
            .input
            .before
            .ids
            .table_uid(&self.input.root.table)
            .unwrap();
        let parent = retire
            .iter()
            .copied()
            .find(|&i| matches!(ops.as_slice()[i].change(),Change::DropTable {uid,..} if uid==old))
            .ok_or("missing parent retirement")?;
        for &leaf in &retire {
            edge(leaf, parent, Source::Effect);
        }
        let mut bindings = BTreeMap::new();
        for (object, d) in &self.prepared.closure.surfaces {
            let old = self.key(d, Spelling::Base)?;
            let new = self.key(d, Spelling::Final)?;
            let locate = |key, teardown| {
                ops.iter().enumerate().find_map(|(at, op)| {
                    let purpose = self.prepared.parts[indices[at]].purpose;
                    ((if teardown {
                        purpose == Purpose::Teardown
                    } else {
                        purpose == Purpose::Own || purpose == Purpose::Restoration
                    }) && op.key(&owners).ok().as_ref() == key)
                        .then_some(at)
                })
            };
            bindings.insert(
                object,
                (locate(old.as_ref(), true), locate(new.as_ref(), false)),
            );
        }
        for (from, link) in &self.prepared.closure.edges {
            let a = &self.prepared.closure.owners[from];
            let b = &self.prepared.closure.owners[&link.dependent];
            if a != b
                && let (Some((drop_a, create_a)), Some((drop_b, create_b))) =
                    (bindings.get(a), bindings.get(b))
            {
                if let (Some(a), Some(b)) = (drop_a, drop_b) {
                    edge(*b, *a, Source::Catalog);
                }
                if let (Some(a), Some(b)) = (create_a, create_b) {
                    edge(*a, *b, Source::Catalog);
                }
            }
        }
        Ok(out)
    }
}
/// Consume the validated inventory into a settled draft, with independent replay.
/// It still cannot be lowered, sealed or admitted as an executable rebuild.
pub fn expand(
    input: &Input<'_>,
    prepared: Prepared,
    dialect: &dyn Dialect,
) -> Result<Expanded, String> {
    let run = || {
        if dialect.name() != "postgres" || input.mode != PlanMode::Transactional {
            return Err("rebuild requires PostgreSQL transactional mode".into());
        }
        let target =
            crate::rebuild::target_identities(input.before, input.desired, input.root, dialect)
                .map_err(|e| e.to_string())?;
        if target != prepared.target
            || prepared.logical.first().map(|p| &p.change)
                != Some(&Change::RebuildPartitionedTable(Box::new(
                    input.root.clone(),
                )))
        {
            return Err("prepared rebuild no longer matches its input".into());
        }
        let rule = Rule {
            canonical: prepared.operations(),
            prepared,
            input,
            dialect,
        };
        let mut logical = rule.prepared.logical.clone();
        logical[0].risks = dialect.change_risks(&logical[0].change);
        let mut done = expand::expand(
            order::propose(ChangeSet { changes: logical }),
            &rule,
            expand::FACTOR,
        )?;
        let operations = rule.replay(&done.changes.changes)?;
        done.changes.identities = Some(operations.clone());
        Ok(Expanded {
            changes: done.changes,
            operations,
            target: rule.prepared.target,
            addresses: rule.prepared.addresses,
            rewrites: done.rewrites,
        })
    };
    run().map_err(|why: String| format!("{}: {why}", input.root.table.shown()))
}

#[cfg(test)]
mod tests;
