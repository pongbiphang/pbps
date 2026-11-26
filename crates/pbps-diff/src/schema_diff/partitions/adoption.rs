//! A partitioned parent's new index or exclusion over standing partitions: each
//! partition's own that the engine might take as its clone is rebuilt around it
//! (#1671, DEC-1671.1).
//!
//! Building a parent's index or exclusion adopts a standing partition's own one
//! that matches it, which then is the parent's clone: it goes when the parent's
//! does, and no later read holds it as the partition's. Which one matches is the
//! engine's to say, from what its expressions bind to and the order the
//! partition's indexes were made in, so none of that is predicted. Every own
//! one the loose [`may_adopt`] or [`may_adopt_exclusion`] matches is dropped
//! ahead of the parent's and added again after it, as declared. The engine then
//! finds nothing to adopt, and what it adds later is the partition's own.
//!
//! An own unique index a foreign key references is the exception: rebuilt, the
//! key would be bound to the parent's clone. It is left as it stands, and where
//! the engine would adopt it the parent's addition is refused
//! (`refusals.rs`).
//!
//! The order is not this module's: a drop sorts ahead of every addition, a
//! parent's addition first in its class, and `before_its_partitions_indexes`
//! keeps the parent's ahead of the partition's where a later pass moved one.

use super::super::constraints::may_adopt;
use super::super::exclusions::may_adopt_exclusion;
use super::super::*;

/// A partition standing under a parent that gains a part, on both sides of the
/// plan.
struct Standing<'a, P> {
    name: &'a TableName,
    /// The partition as the base held it, spelled as this plan's renames leave
    /// it, which is how the parent's new part names its columns.
    was: Table,
    now: &'a Table,
    parents: &'a [P],
}

/// The partitions standing on both sides under a parent that is itself
/// partitioned and standing on both sides, for the parents in `added`. One the
/// plan creates, attaches or moves has no part the base held under this parent.
fn standing<'a, P>(
    base: Side<'_>,
    declared: Side<'a>,
    renames: &Renames,
    added: &'a BTreeMap<&TableName, Vec<P>>,
) -> Vec<Standing<'a, P>> {
    let partitioned = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_by.is_some() && t.partition_of.is_none())
    };
    // Each parent under the name the base held it by, which a rename moves.
    let parents: BTreeMap<&TableName, (&TableName, &[P])> = added
        .iter()
        .filter(|(name, _)| partitioned(declared.schema, name))
        .filter_map(|(&name, parts)| {
            let was = base.ids.tables.get(declared.ids.table_uid(name)?)?;
            partitioned(base.schema, was).then_some((name, (was, parts.as_slice())))
        })
        .collect();
    if parents.is_empty() {
        return Vec::new();
    }
    declared
        .ids
        .tables
        .iter()
        .filter_map(|(uid, name)| {
            let now = declared.schema.tables.get(name)?;
            let (parent, parts) = parents.get(&now.partition_of.as_ref()?.parent)?;
            let base_name = base.ids.tables.get(uid)?;
            let was = base.schema.tables.get(base_name)?;
            (was.partition_of.as_ref()?.parent == **parent).then(|| Standing {
                name,
                was: renames.apply(was, base_name).into_owned(),
                now,
                parents: parts,
            })
        })
        .collect()
}

/// A foreign key the declarations hold that may be bound to a partition's own
/// index: one that references the partition over the columns of a unique index
/// with no predicate and no expression, which is what the engine binds a key to.
/// A rebuild drops the key and adds it again, and the engine then binds it to
/// the parent's clone, the older index, which no read of the partition holds
/// (measured on 16.15 and 18.6, DEC-1671.1). Which of several such indexes a key
/// is bound to is not in the model, so each is taken as referenced.
pub(super) fn referencing_key<'a>(
    declared: &'a Schema,
    partition: &TableName,
    own: &Index,
) -> Option<(&'a TableName, &'a pbps_model::ObjectLocalName)> {
    if !own.unique || own.filter.is_some() {
        return None;
    }
    // In any order: a key names the index's columns as it likes.
    let columns: BTreeSet<ColumnName> = own.column_keys()?.into_iter().collect();
    declared.tables.iter().find_map(|(table, t)| {
        t.foreign_keys
            .iter()
            .find(|(_, key)| {
                key.references_table == *partition
                    && key
                        .references_columns
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        == columns
            })
            .map(|(name, _)| (table, name))
    })
}

/// Every standing partition's own index that a parent's new index might take,
/// dropped and added again as declared. One the plan already drops, to remove or
/// redefine it, is left as the plan has it, and one the declarations no longer
/// hold is among those. One a foreign key references is left standing
/// ([`referencing_key`]).
///
/// It runs ahead of `recreate_referenced_foreign_keys`, which reads these drops
/// as it reads any other of a unique index.
pub(in super::super) fn rebuild_adoptable_indexes(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    let mut added: BTreeMap<&TableName, Vec<&Index>> = BTreeMap::new();
    let mut dropped = BTreeSet::new();
    for change in changes.iter() {
        if let Change::Index(IndexChange::AddIndex { table, index, .. }) = change {
            added.entry(table).or_default().push(index);
        } else if let Change::Index(IndexChange::DropIndex { table, name }) = change {
            dropped.insert((table, name));
        }
    }
    let mut rebuilt = Vec::new();
    for partition in standing(base, declared, renames, &added) {
        for (name, held) in &partition.was.indexes {
            let Some(index) = partition.now.indexes.get(name) else {
                continue;
            };
            if dropped.contains(&(partition.name, name))
                || !partition
                    .parents
                    .iter()
                    .any(|p| may_adopt(p, held, dialect))
                || referencing_key(declared.schema, partition.name, index).is_some()
            {
                continue;
            }
            rebuilt.push(Change::Index(IndexChange::DropIndex {
                table: partition.name.clone(),
                name: name.clone(),
            }));
            rebuilt.push(Change::Index(IndexChange::AddIndex {
                table: partition.name.clone(),
                name: name.clone(),
                index: Box::new(index.clone()),
                clustered: partition.now.index_is_clustered(name.raw()),
            }));
        }
    }
    changes.extend(rebuilt);
}

/// As [`rebuild_adoptable_indexes`], for exclusions: the drop and the add of one
/// kept definition, as `diff_exclusions` states a replacement, so no retirement
/// is asked. One the declarations no longer hold is not touched, since its drop
/// is the retirement gate's to allow or refuse.
///
/// It runs after `exclusion_hazards`, whose restatement of a parent's exclusion
/// around a column change is an addition like any other.
pub(in super::super) fn rebuild_adoptable_exclusions(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    let mut added: BTreeMap<&TableName, Vec<&pbps_model::ExclusionConstraint>> = BTreeMap::new();
    let mut dropped = BTreeSet::new();
    for change in changes.iter() {
        if let Change::Constraint(ConstraintChange::SetExclusion {
            table,
            name,
            from,
            to,
        }) = change
        {
            if from.is_some() {
                dropped.insert((table, name));
            } else if let Some(to) = to {
                added.entry(table).or_default().push(to);
            }
        }
    }
    let mut rebuilt = Vec::new();
    for partition in standing(base, declared, renames, &added) {
        for (name, held) in &partition.was.exclude {
            let Some(exclusion) = partition.now.exclude.get(name) else {
                continue;
            };
            if dropped.contains(&(partition.name, name))
                || !partition
                    .parents
                    .iter()
                    .any(|p| may_adopt_exclusion(p, held, dialect))
            {
                continue;
            }
            for (from, to) in [
                (Some(Box::new(held.clone())), None),
                (None, Some(Box::new(exclusion.clone()))),
            ] {
                rebuilt.push(Change::Constraint(ConstraintChange::SetExclusion {
                    table: partition.name.clone(),
                    name: name.clone(),
                    from,
                    to,
                }));
            }
        }
    }
    changes.extend(rebuilt);
}
