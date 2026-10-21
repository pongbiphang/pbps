//! Split callable parts out of CREATE without moving another logical change
//! (DEC-1959.2). The existing lexical schedule places them after expansion.

use std::collections::BTreeSet;

use pbps_dialect::Dialect;
use pbps_model::{Change, ColumnRef, IdsFile, ModuleKind, PlannedChange, ValueSource};

use super::expand::{Expansion, Order, Rewrite};

pub struct NewTables<'a, D: Dialect + ?Sized> {
    pub ids: &'a [&'a IdsFile],
    pub functions: &'a [(String, usize)],
    pub taken: &'a BTreeSet<ColumnRef>,
    pub dialect: &'a D,
}

impl<D: Dialect + ?Sized> NewTables<'_, D> {
    fn calls(&self, text: &str) -> bool {
        self.functions
            .iter()
            .any(|(name, _)| self.dialect.generation_may_call(text, name))
    }

    fn planned(&self, change: Change) -> PlannedChange {
        let mut planned = PlannedChange::new(change);
        planned.risks = self.dialect.change_risks(&planned.change);
        planned
    }

    fn parts(&self, original: &PlannedChange) -> Option<(PlannedChange, Vec<PlannedChange>)> {
        let Change::CreateTable { name, table, .. } = &original.change else {
            return None;
        };
        let mut bare = original.change.clone();
        let Change::CreateTable { table: rest, .. } = &mut bare else {
            unreachable!("a clone retains its kind");
        };
        let mut parts = Vec::new();
        for (name_in_table, constraint) in &table.checks {
            if self.calls(&constraint.expression) {
                rest.checks.remove(name_in_table);
                parts.push(self.planned(Change::AddCheck {
                    table: name.clone(),
                    name: name_in_table.clone(),
                    constraint: constraint.clone(),
                }));
            }
        }
        for (name_in_table, index) in &table.indexes {
            let mut texts: Vec<&str> = index.filter.iter().map(String::as_str).collect();
            texts.extend(index.columns.iter().filter_map(|c| c.key.expression()));
            // Joining preserves even the old scanner's treatment of fragments;
            // broadening that lexical rule would change which parts split.
            if index.holds_expression() && self.calls(&texts.join(" , ")) {
                rest.indexes.remove(name_in_table);
                parts.push(self.planned(Change::AddIndex {
                    table: name.clone(),
                    name: name_in_table.clone(),
                    index: Box::new(index.clone()),
                    clustered: table.index_is_clustered(name_in_table),
                }));
            }
        }
        for (column, spec) in &mut rest.columns {
            let column_ref = name.column(column);
            if self.taken.contains(&column_ref) {
                continue;
            }
            let Some(uid) = self.ids.iter().find_map(|ids| ids.column_uid(&column_ref)) else {
                continue;
            };
            if let ValueSource::Default(default) = &mut spec.value_source
                && self.calls(default)
            {
                let to = std::mem::take(default);
                spec.value_source = ValueSource::None;
                parts.push(self.planned(Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: column_ref,
                    from: None,
                    to: Some(to),
                }));
            }
        }
        if parts.is_empty() {
            return None;
        }
        // Ordinary split checks retain their approval; only the existing
        // generated-table path may derive empty-CREATE risks (DEC-1503.6).
        Some((self.planned(bare), parts))
    }
}

impl<D: Dialect + ?Sized> Expansion for NewTables<'_, D> {
    type Obligation = usize;

    fn rule(&self) -> &'static str {
        "new-table-expressions"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<usize>, String> {
        let last = plan.iter().rposition(|p| {
            matches!(&p.change, Change::CreateModule { module, .. } | Change::AlterModule { module, .. }
                if module.kind == ModuleKind::Function)
        });
        // Work from the end, as before: earlier positions keep naming their
        // original whole change after a later table grows extra parts.
        Ok((0..last.unwrap_or(0))
            .rev()
            .filter(|&at| self.parts(&plan[at]).is_some())
            .collect())
    }

    fn expand(&self, plan: &[PlannedChange], at: &usize) -> Result<Rewrite, String> {
        let (bare, parts) = self
            .parts(&plan[*at])
            .ok_or_else(|| format!("{}: step {at} has no callable table parts", self.rule()))?;
        let mut parts = parts.into_iter();
        let first = parts.next().expect("a split has a deferred part");
        Ok(Rewrite::Split {
            at: *at,
            parts: Box::new([bare, first]),
            tail: parts.collect(),
            reason: "a new table exists before its expressions call planned functions".into(),
        })
    }

    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        // No step changes position in this expansion. With no ordering edge,
        // settle's existing Sink policy keeps the anchored parts in place.
        Ok(Vec::new())
    }
}
