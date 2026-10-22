//! Complete identities for older dependency tests that only stated payloads.
//! New scope-transition tests supply their real before and after sides directly.

use super::*;

use crate::test_plan_ids as plan_ids;

#[allow(clippy::wildcard_enum_match_arm)]
fn inputs(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
    supplied: &[&IdsFile],
    partitions: &Partitions,
) -> (Schema, IdsFile, IdsFile) {
    let mut before = declared.clone();
    for p in cs.changes.iter().rev() {
        match &p.change {
            Change::RenameTable { from, to, .. } => {
                if let Some(shape) = before.tables.remove(to) {
                    before.tables.insert(from.clone(), shape);
                }
                for shape in before.tables.values_mut() {
                    if let Some(partition) = &mut shape.partition_of
                        && partition.parent == *to
                    {
                        partition.parent = from.clone();
                    }
                }
            }
            Change::RenameColumn {
                table, from, to, ..
            } => {
                if let Some(shape) = before.tables.get_mut(table)
                    && let Some(column) = shape.columns.shift_remove(to)
                {
                    shape.columns.insert(from.clone(), column);
                }
            }
            Change::DetachPartition { table, parent, .. } => {
                before.tables.entry(table.clone()).or_default().partition_of =
                    Some(pbps_model::PartitionOf {
                        parent: parent.clone(),
                        bound: pbps_model::PartitionBound::Default,
                        columns: Default::default(),
                    });
            }
            Change::AttachPartition { table, shape, .. } => {
                before.tables.insert(table.clone(), shape.as_ref().clone());
            }
            _ => {}
        }
    }
    for d in found.values().flatten() {
        if let Holds::TablePart { table, part } = &d.holds {
            before.tables.entry(table.clone()).or_default();
            if let Part::Default(column) | Part::Generated(column) = part {
                let parent = before.tables[table]
                    .partition_of
                    .as_ref()
                    .map(|p| &p.parent)
                    .or_else(|| partitions.parent(table))
                    .cloned();
                let owner = parent.as_ref().unwrap_or(table);
                before
                    .tables
                    .entry(owner.clone())
                    .or_default()
                    .columns
                    .entry(column.clone())
                    .or_insert_with(|| pbps_model::Column::new("integer".parse().unwrap()));
                if let Some(parent) = parent {
                    before
                        .tables
                        .get_mut(table)
                        .unwrap()
                        .partition_of
                        .get_or_insert(pbps_model::PartitionOf {
                            parent,
                            bound: pbps_model::PartitionBound::Default,
                            columns: Default::default(),
                        });
                }
            }
        }
    }
    let mut after = plan_ids::ids(cs, &before);
    // Supplied untouched UIDs remain authoritative. Explicit changes already
    // own their identities, so they are never overwritten by partial fixtures.
    for ids in supplied {
        for (uid, column) in &ids.columns {
            let explicit = cs
                .changes
                .iter()
                .any(|p| p.change.columns().contains(column));
            if !explicit && after.column_uid(column).is_some() {
                after.columns.retain(|_, address| address != column);
                after.columns.insert(uid.clone(), column.clone());
            }
        }
    }
    // Ordinary start UIDs are reconstructed by the production index itself;
    // baseline-only ownership is needed by the explicit transition fixtures.
    (before, IdsFile::default(), after)
}

pub(super) fn weave(
    cs: &mut ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    declared: &Schema,
    ids: &[&IdsFile],
    dialect: &dyn crate::certified_fixture::FixtureEmission,
) -> Result<usize, String> {
    let (before, before_ids, after) = inputs(cs, found, declared, ids, &Partitions::default());
    super::test_adapters::weave(
        cs,
        found,
        declared,
        Sources {
            before: pbps_diff::Side {
                schema: &before,
                ids: &before_ids,
            },
            after: &after,
        },
        dialect,
    )
}

pub(super) fn unaccounted(
    cs: &ChangeSet,
    found: &BTreeMap<ModuleId, Vec<Dependent>>,
    partitions: &Partitions,
) -> Vec<String> {
    let (before, before_ids, after) = inputs(cs, found, &Schema::default(), &[], partitions);
    let sources = Sources {
        before: pbps_diff::Side {
            schema: &before,
            ids: &before_ids,
        },
        after: &after,
    };
    let view = sources
        .project(&cs.changes)
        .expect("coherent dependency fixture identities");
    super::unaccounted(cs, found, partitions, &view)
}
