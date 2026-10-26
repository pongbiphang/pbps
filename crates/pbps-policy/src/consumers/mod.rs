//! Declared consumers, bound before the plan rules run (DEC-2398.1).

mod binding;
mod changes;

use crate::{Context, Policies, evaluate::Live};
pub use binding::Bindings;
use pbps_model::{ChangeSet, Finding, IdsFile, Module, ModuleId, Schema, Uid};
use std::collections::{BTreeMap, BTreeSet};

pub type Consumers = BTreeMap<String, Consumer>;

#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Consumer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<String>,
}

/// Already-read planning inputs. No policy reads git, a file or a database.
#[derive(Clone, Copy)]
pub struct Side<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
}

impl Side<'_> {
    // One effective definition for binding and the supplied-new-column rule.
    // Partition-local defaults and NOT NULL refine the parent's column.
    pub(super) fn column(
        &self,
        at: &pbps_model::ColumnRef,
    ) -> Option<(pbps_model::ColumnRef, pbps_model::Column)> {
        let source = pbps_model::Partitions::of(self.schema).column(at);
        let mut column = self
            .schema
            .tables
            .get(&source.table)?
            .columns
            .get(&source.name)?
            .clone();
        if let Some(own) = self
            .schema
            .tables
            .get(&at.table)?
            .partition_of
            .as_ref()
            .and_then(|of| of.columns.get(&at.name))
        {
            if let Some(default) = &own.default {
                column.value_source = pbps_model::ValueSource::Default(default.clone());
            }
            column.nullable &= !own.not_null;
        }
        Some((source, column))
    }
    pub(super) fn table(&self, uid: Option<&pbps_model::Uid>) -> Option<&pbps_model::Table> {
        self.schema.tables.get(self.ids.tables.get(uid?)?)
    }
}

/// Structural checks are never narrowed by `validate --since` or suppressed.
pub fn validate(
    consumers: &Consumers,
    schema: &Schema,
    spell: &dyn Fn(&ModuleId) -> ModuleId,
) -> Vec<String> {
    binding::entries(
        consumers,
        &[Side {
            schema,
            ids: &IdsFile::default(),
        }],
        spell,
        false,
    )
    .1
}

impl Bindings {
    /// Both sides participate: a removed declaration still has a consumer.
    pub fn resolve(
        consumers: &Consumers,
        sides: &[Side<'_>; 2],
        spell: &dyn Fn(&ModuleId) -> ModuleId,
    ) -> Result<Self, Vec<String>> {
        let (entries, errors) = binding::entries(consumers, sides, spell, true);
        if errors.is_empty() {
            Ok(Self(entries))
        } else {
            Err(errors)
        }
    }
}

// Column writes are table writes. Cache each written UID's whole partition
// tree once, retaining its seed so unrelated entries do not share suppressions.
fn written_tables(bound: &Bindings, sides: &[Side<'_>]) -> BTreeMap<Uid, BTreeSet<Uid>> {
    let mut links: BTreeMap<Uid, BTreeSet<Uid>> = BTreeMap::new();
    for side in sides {
        for (name, table) in &side.schema.tables {
            if let Some(of) = &table.partition_of
                && let (Some(child), Some(parent)) =
                    (side.ids.table_uid(name), side.ids.table_uid(&of.parent))
            {
                links
                    .entry(child.clone())
                    .or_default()
                    .insert(parent.clone());
                links
                    .entry(parent.clone())
                    .or_default()
                    .insert(child.clone());
            }
        }
    }
    let mut written: BTreeMap<Uid, BTreeSet<Uid>> = BTreeMap::new();
    for entry in &bound.0 {
        if entry.writes
            && let Some(table) = entry.table()
        {
            written
                .entry(table.clone())
                .or_default()
                .insert(table.clone());
        }
    }
    for tables in written.values_mut() {
        let mut pending: Vec<_> = tables.iter().cloned().collect();
        while let Some(table) = pending.pop() {
            for neighbor in links.get(&table).into_iter().flatten() {
                if tables.insert(neighbor.clone()) {
                    pending.push(neighbor.clone());
                }
            }
        }
    }
    written
}

pub fn plan(
    cs: &ChangeSet,
    bound: &Bindings,
    sides: &[Side<'_>; 2],
    policies: &Policies,
    ctx: &Context,
    unchanged: &dyn Fn(&Module, &Module) -> bool,
) -> Vec<(usize, Finding)> {
    let mut out = Vec::new();
    let written = written_tables(bound, sides);
    let mut tables: BTreeMap<_, _> = sides
        .first()
        .into_iter()
        .flat_map(|s| s.ids.tables.iter())
        .map(|(uid, name)| (name.clone(), uid.clone()))
        .collect();
    for (at, p) in cs.changes.iter().enumerate() {
        // Follow execution addresses, so reuse of a vacated name is a new table.
        if let pbps_model::Change::CreateTable { uid, name, .. } = &p.change {
            tables.insert(name.clone(), uid.clone());
        } else if let pbps_model::Change::RenameTable { uid, from, to, .. } = &p.change {
            tables.remove(from);
            tables.insert(to.clone(), uid.clone());
        }
        let table = if let pbps_model::Change::RenameTable { uid, .. } = &p.change {
            Some(uid)
        } else {
            p.change.table().and_then(|t| tables.get(t)).or_else(|| {
                p.change
                    .table()
                    .and_then(|t| sides.last()?.ids.table_uid(t))
            })
        };
        for entry in &bound.0 {
            // A table created by this plan has no pre-existing writer contract.
            if entry.writes && entry.table().is_some_and(|uid| {
                sides[0].table(Some(uid)).is_none() && cs.changes.iter().any(|p|
                    matches!(&p.change, pbps_model::Change::CreateTable { uid: created, .. } if created == uid))
            }) {
                continue;
            }
            let Some(class) = changes::classify(
                p,
                entry,
                table,
                entry.table().and_then(|uid| written.get(uid)),
                cs,
                sides,
                unchanged,
            ) else {
                continue;
            };
            let Some(live) = Live::of(policies, class.id(), ctx) else {
                continue;
            };
            let conclusion = match class {
                changes::Class::Breaking => "breaks",
                changes::Class::Unproven => "cannot be proven compatible with",
            };
            let message = format!(
                "Consumer `{}`: {} {} its `{}` contract on `{}`. Keep the old interface alongside the new one, migrate the consumer, then contract in a later revision. Retire `consumers.{}.{}: {}` when the consumer no longer needs it.",
                pbps_model::escape::Escaped(&entry.consumer),
                entry.change_text(&p.change),
                conclusion,
                entry.access(),
                pbps_model::escape::Escaped(&entry.text),
                pbps_model::escape::Escaped(&entry.consumer),
                entry.access(),
                pbps_model::escape::Escaped(&entry.text),
            );
            out.extend(live.finding(Some(&entry.subject), message).map(|f| (at, f)));
        }
        if let pbps_model::Change::DropTable { name, .. } = &p.change {
            tables.remove(name);
        }
    }
    out
}

#[cfg(test)]
mod tests;
