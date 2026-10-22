//! Restore a partition's own default after its parent's recursive SET
//! (DEC-1959.4). Expansion supplies missing steps; Schedule owns their order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pbps_dialect::Dialect;
use pbps_model::resolver::OrderReason;
use pbps_model::{Change, ColumnRef, IdsFile, PlannedChange, Schema, TableName, ValueSource};

use super::Source;
use super::expand::{Added, Addition, Anchor, Expansion, Order, Rewrite, Step};

pub struct ParentDefaults<'a, D: Dialect + ?Sized> {
    /// Input to this expansion, so additions are recognized without making
    /// unsealed diagnostic provenance an input to ordering.
    pub initial: &'a [PlannedChange],
    pub declared: &'a Schema,
    pub ids: &'a [&'a IdsFile],
    pub dialect: &'a D,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restoration {
    column: ColumnRef,
    partition: TableName,
}

impl fmt::Display for Restoration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "partition {} column {} needs its own default after {}",
            self.partition.shown(),
            self.column.name.shown(),
            self.column.table.shown()
        )
    }
}

fn parent_set(change: &Change) -> Option<&ColumnRef> {
    if let Change::AlterColumnDefault {
        column,
        to: Some(_),
        ..
    } = change
    {
        Some(column)
    } else {
        None
    }
}

fn sets(change: &Change, partition: &TableName, column: &ColumnRef) -> bool {
    matches!(change, Change::SetPartitionDefault { table, column: name, to: Some(_), .. }
        if table == partition && *name == column.name)
}

fn created_later(plan: &[PlannedChange], at: usize, partition: &TableName) -> bool {
    plan[at + 1..]
        .iter()
        .any(|p| matches!(&p.change, Change::CreateTable { name, .. } if name == partition))
}

fn order(before: Step, after: Step, source: Source) -> Order {
    Order {
        before,
        after,
        source,
        reason: OrderReason::Restoration,
    }
}

impl<D: Dialect + ?Sized> ParentDefaults<'_, D> {
    fn own(&self, column: &ColumnRef) -> Vec<(TableName, String)> {
        self.declared
            .tables
            .iter()
            .filter_map(|(name, table)| {
                let of = table
                    .partition_of
                    .as_ref()
                    .filter(|of| of.parent == column.table)?;
                let default = of.columns.get(&column.name)?.default.clone()?;
                Some((name.clone(), default))
            })
            .collect()
    }

    fn missing(&self, plan: &[PlannedChange]) -> Vec<(usize, Restoration, String)> {
        let mut seen = BTreeSet::new();
        let mut missing = Vec::new();
        for (at, p) in plan.iter().enumerate() {
            let Some(column) = parent_set(&p.change) else {
                continue;
            };
            for (partition, default) in self.own(column) {
                // A later CREATE sets its own override. Any existing SET is
                // reused, wherever it is; ordering is not a missing operation.
                if created_later(plan, at, &partition)
                    || plan.iter().any(|p| sets(&p.change, &partition, column))
                    || !seen.insert((column.clone(), partition.clone()))
                {
                    continue;
                }
                missing.push((
                    at,
                    Restoration {
                        column: column.clone(),
                        partition,
                    },
                    default,
                ));
            }
        }
        missing
    }

    fn fallback(&self, column: &ColumnRef) -> Option<String> {
        match &self
            .declared
            .tables
            .get(&column.table)?
            .columns
            .get(&column.name)?
            .value_source
        {
            ValueSource::Default(default) => Some(default.clone()),
            ValueSource::None
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => None,
        }
    }
}

impl<D: Dialect + ?Sized> Expansion for ParentDefaults<'_, D> {
    type Obligation = Restoration;

    fn rule(&self) -> &'static str {
        "partition-parent-defaults"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<Restoration>, String> {
        Ok(self
            .missing(plan)
            .into_iter()
            .map(|(_, need, _)| need)
            .collect())
    }

    fn expand(&self, plan: &[PlannedChange], need: &Restoration) -> Result<Rewrite, String> {
        let (at, _, default) = self
            .missing(plan)
            .into_iter()
            .find(|(_, own, _)| own == need)
            .ok_or_else(|| format!("{need}: no missing restoration"))?;
        let uid = self
            .ids
            .iter()
            .find_map(|ids| ids.table_uid(&need.partition))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no identity names the partition `{partition}`",
                    partition = need.partition.shown()
                )
            })?;
        let change = Change::SetPartitionDefault {
            uid,
            table: need.partition.clone(),
            parent: need.column.table.clone(),
            column: need.column.name.clone(),
            from: None,
            to: Some(default),
            fallback: self.fallback(&need.column),
        };
        let mut planned = PlannedChange::new(change);
        planned.risks = self.dialect.change_risks(&planned.change);
        Ok(Rewrite::Add {
            added: vec![Added::Restoration(Addition {
                of: at,
                change: planned,
                anchor: Anchor::After(at),
                reason: "the parent's SET DEFAULT overwrites the partition's own default".into(),
            })],
            respelled: Vec::new(),
            edges: vec![order(Step::Plan(at), Step::Added(0), Source::Effect)],
        })
    }

    fn order(&self, plan: &[PlannedChange]) -> Result<Vec<Order>, String> {
        // Parent sets keep their base order. One restoration can therefore
        // serve them all by following the final SET on that column.
        let mut parents = BTreeMap::new();
        for (at, p) in plan.iter().enumerate() {
            if let Some(column) = parent_set(&p.change) {
                parents.insert(column, at);
            }
        }
        let mut edges = Vec::new();
        for (column, at) in parents {
            let mut restored = Vec::new();
            for (partition, _) in self.own(column) {
                if created_later(plan, at, &partition) {
                    continue;
                }
                let Some(own) = plan
                    .iter()
                    .rposition(|p| sets(&p.change, &partition, column))
                else {
                    continue;
                };
                edges.push(order(Step::Plan(at), Step::Plan(own), Source::Effect));
                // New anchored restorations join the ones sunk from earlier
                // positions. A later original SET keeps its original place.
                if own < at
                    || !self
                        .initial
                        .iter()
                        .any(|p| sets(&p.change, &partition, column))
                {
                    restored.push(own);
                }
            }
            // The old insertion after one parent reversed declaration order.
            // This explicit compatibility chain exists only for identical
            // bytes, as the same-depth chain does in DEC-1959.1. It is not a
            // rank, a tie-break or a local permutation of operations.
            for pair in restored.iter().rev().collect::<Vec<_>>().windows(2) {
                edges.push(order(
                    Step::Plan(*pair[0]),
                    Step::Plan(*pair[1]),
                    Source::Lexical,
                ));
            }
        }
        Ok(edges)
    }
}
