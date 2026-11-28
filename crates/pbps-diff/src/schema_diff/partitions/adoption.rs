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
//! (`refusals.rs`). So is one a rehearsal shows the engine leaves alone
//! (`rehearsal.rs`, DEC-1671.3).
//!
//! The order is not this module's: a drop sorts ahead of every addition, a
//! parent's addition first in its class, and `before_its_partitions_indexes`
//! keeps the parent's ahead of the partition's where a later pass moved one.

use super::super::constraints::may_adopt;
use super::super::exclusions::may_adopt_exclusion;
use super::super::*;
use super::rehearsal::{addition_skip, attach_skip, taken};
use pbps_model::ObjectLocalName;
use pbps_model::adoption::{Remark, adds};

/// An own index, key or exclusion the loose rule drops because one of the
/// parent's may take it (DEC-1671.1): what the pass that pushes the drop knows
/// of it, for the plan to say (DEC-1671.3).
pub(in super::super) struct Loose {
    drop: Change,
    table: TableName,
    /// None only for a primary key the base holds no name for.
    name: Option<ObjectLocalName>,
    /// The parent's that may take it.
    by: Vec<ObjectLocalName>,
    /// `by` names exclusion constraints.
    exclusions: bool,
    /// The parent's are ones this plan adds.
    new: bool,
    /// The declarations keep it, and the plan adds it again.
    again: bool,
}

impl Loose {
    /// For `drop` ahead of an attach. `declared` says whether the declarations
    /// keep what it drops as it stood, and is `None` where they do not hold it,
    /// as no partition holds a key. `None` is returned where they declare it
    /// otherwise: that drop is the redefinition's and not the parent's.
    pub(in super::super) fn attached(
        drop: &Change,
        table: &TableName,
        name: Option<&ObjectLocalName>,
        by: &[ObjectLocalName],
        exclusions: bool,
        declared: Option<bool>,
    ) -> Option<Self> {
        (declared != Some(false)).then(|| Loose {
            drop: drop.clone(),
            table: table.clone(),
            name: name.cloned(),
            by: by.to_vec(),
            exclusions,
            new: false,
            again: declared.is_some(),
        })
    }
}

/// Says on the plan's steps why each own the loose rule drops is dropped: on the
/// drop, and on the add after it. `base` is the side as the caller gave it, as
/// a rehearsal's question is asked of it.
///
/// A resolver is offered only where it could keep the own and none was asked
/// about the table, in a plan the question is asked of (DEC-1671.2). Under an
/// attach that is an own whose name the declarations give to nothing. Under a
/// parent's new index it is a declared one, which a resolver keeps by leaving it
/// beside the copy (DEC-1671.3).
pub(in super::super) fn say_why(
    base: Side<'_>,
    declared: &Schema,
    hints: &Hints,
    loose: &[Loose],
    planned: &mut [PlannedChange],
) {
    for own in loose {
        let attach = planned.iter().find_map(|p| {
            if let Change::Partition(PartitionChange::AttachPartition {
                uid, table, parent, ..
            }) = &p.change
                && *table == own.table
            {
                Some((uid, parent))
            } else {
                None
            }
        });
        let askable = attach.is_some_and(|(uid, parent)| {
            let skipped = match (base.schema.tables.get(parent), declared.tables.get(parent)) {
                (Some(was), Some(now)) => attach_skip(planned, was, now).is_some(),
                _ => true,
            };
            // A rehearsal could not leave out a key it has no name for.
            let unnamed = planned.iter().any(|p| {
                matches!(&p.change, Change::Constraint(ConstraintChange::SetPrimaryKey {
                    table, from: Some(key), to: None, ..
                }) if *table == own.table && key.name.is_none())
            });
            !skipped && !unnamed && !hints.attach_rehearsals.iter().any(|a| a.table() == uid)
        });
        let could_keep = if own.new {
            could_stand_beside(base, declared, hints, loose, &own.table, planned)
        } else {
            askable
                && own
                    .name
                    .as_ref()
                    .is_some_and(|name| !taken(declared, &own.table, name))
        };
        let remark = Remark::Loose {
            by: own.by.clone(),
            exclusions: own.exclusions,
            new: own.new,
            again: own.again,
            resolver_could_keep: could_keep,
        };
        for p in planned.iter_mut() {
            let added =
                own.again && own.name.is_some() && adds(&p.change, &own.table) == own.name.as_ref();
            if added || p.change == own.drop {
                p.remarks.push(remark.clone());
            }
        }
    }
}

/// Whether a resolver could leave a declared own of `partition`'s standing
/// beside its parent's new ones: none was asked about the partition, and a
/// rehearsal would be asked of this plan (DEC-1671.3).
fn could_stand_beside(
    base: Side<'_>,
    declared: &Schema,
    hints: &Hints,
    loose: &[Loose],
    partition: &TableName,
    planned: &[PlannedChange],
) -> bool {
    let (Some(uid), Some(was), Some(now)) = (
        base.ids.table_uid(partition),
        base.schema.tables.get(partition),
        declared.tables.get(partition),
    ) else {
        return false;
    };
    let rebuilt = loose
        .iter()
        .filter(|own| own.new && own.table == *partition)
        .filter_map(|own| own.name.as_ref());
    !hints.attach_rehearsals.iter().any(|a| a.table() == uid)
        && now.partition_of.as_ref().is_some_and(|of| {
            addition_skip(planned, &of.parent, partition, (was, now), rebuilt).is_none()
        })
}

/// A partition standing under a parent that gains a part, on both sides of the
/// plan.
pub(super) struct Standing<'a, P> {
    pub name: &'a TableName,
    pub uid: &'a Uid,
    /// The name the base held the partition by.
    pub base_name: &'a TableName,
    pub parent: &'a TableName,
    /// The partition as the base held it, spelled as this plan's renames leave
    /// it, which is how the parent's new part names its columns.
    pub was: Table,
    pub now: &'a Table,
    pub parents: &'a [P],
}

/// The partitions standing on both sides under a parent that is itself
/// partitioned and standing on both sides, for the parents in `added`. One the
/// plan creates, attaches or moves has no part the base held under this parent.
pub(super) fn standing<'a, P>(
    base: Side<'a>,
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
            let parent = &now.partition_of.as_ref()?.parent;
            let (base_parent, parts) = parents.get(parent)?;
            let base_name = base.ids.tables.get(uid)?;
            let was = base.schema.tables.get(base_name)?;
            (was.partition_of.as_ref()?.parent == **base_parent).then(|| Standing {
                name,
                uid,
                base_name,
                parent,
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
    // In any order: a key names the index's columns as it likes. The counts
    // must agree too: the engine binds a key only to an index with as many
    // keys, and refuses a key that names a column twice, so `(a)` is never
    // bound to `(a, a)` (measured on 18.7, draft round 2).
    let keys = own.column_keys()?;
    let columns: BTreeSet<&ColumnName> = keys.iter().collect();
    declared.tables.iter().find_map(|(table, t)| {
        t.foreign_keys
            .iter()
            .find(|(_, key)| {
                key.references_table == *partition
                    && key.references_columns.len() == keys.len()
                    && key.references_columns.iter().collect::<BTreeSet<_>>() == columns
            })
            .map(|(name, _)| (table, name))
    })
}

/// Every standing partition's own index that a parent's new index might take,
/// dropped and added again as declared, and marked as dropped for that. One the plan already drops, to remove or
/// redefine it, is left as the plan has it, and one the declarations no longer
/// hold is among those. One a foreign key references is left standing
/// ([`referencing_key`]), and so is each of `beside`, which a rehearsal shows
/// none of them takes.
///
/// It runs ahead of `recreate_referenced_foreign_keys`, which reads these drops
/// as it reads any other of a unique index.
pub(in super::super) fn rebuild_adoptable_indexes(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    dialect: &(impl Dialect + ?Sized),
    beside: &BTreeSet<(TableName, ObjectLocalName)>,
    changes: &mut Vec<Change>,
    loose: &mut Vec<Loose>,
) {
    let mut added: BTreeMap<&TableName, Vec<(&ObjectLocalName, &Index)>> = BTreeMap::new();
    let mut dropped = BTreeSet::new();
    for change in changes.iter() {
        if let Change::Index(IndexChange::AddIndex {
            table, name, index, ..
        }) = change
        {
            added.entry(table).or_default().push((name, index));
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
            let by: Vec<_> = partition
                .parents
                .iter()
                .filter(|(_, p)| may_adopt(p, held, dialect))
                .map(|(parent, _)| (*parent).clone())
                .collect();
            if dropped.contains(&(partition.name, name))
                || by.is_empty()
                || referencing_key(declared.schema, partition.name, index).is_some()
                || beside.contains(&(partition.name.clone(), name.clone()))
            {
                continue;
            }
            let drop = Change::Index(IndexChange::DropIndex {
                table: partition.name.clone(),
                name: name.clone(),
            });
            loose.push(Loose {
                drop: drop.clone(),
                table: partition.name.clone(),
                name: Some(name.clone()),
                by,
                exclusions: false,
                new: true,
                again: true,
            });
            rebuilt.push(drop);
            rebuilt.push(Change::Index(IndexChange::AddIndex {
                table: partition.name.clone(),
                name: name.clone(),
                index: Box::new(index.clone()),
                clustered: partition.now.index_is_clustered(name.raw()),
                rehearsal: None,
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
    beside: &BTreeSet<(TableName, ObjectLocalName)>,
    changes: &mut Vec<Change>,
    loose: &mut Vec<Loose>,
) {
    let mut added: BTreeMap<&TableName, Vec<(&ObjectLocalName, &pbps_model::ExclusionConstraint)>> =
        BTreeMap::new();
    let mut dropped = BTreeSet::new();
    for change in changes.iter() {
        if let Change::Constraint(ConstraintChange::SetExclusion {
            table,
            name,
            from,
            to,
            ..
        }) = change
        {
            if from.is_some() {
                dropped.insert((table, name));
            } else if let Some(to) = to {
                added.entry(table).or_default().push((name, to));
            }
        }
    }
    let mut rebuilt = Vec::new();
    for partition in standing(base, declared, renames, &added) {
        for (name, held) in &partition.was.exclude {
            let Some(exclusion) = partition.now.exclude.get(name) else {
                continue;
            };
            let by: Vec<_> = partition
                .parents
                .iter()
                .filter(|(_, p)| may_adopt_exclusion(p, held, dialect))
                .map(|(parent, _)| (*parent).clone())
                .collect();
            if dropped.contains(&(partition.name, name))
                || by.is_empty()
                || beside.contains(&(partition.name.clone(), name.clone()))
            {
                continue;
            }
            let part = |from, to| {
                Change::Constraint(ConstraintChange::SetExclusion {
                    table: partition.name.clone(),
                    name: name.clone(),
                    from,
                    to,
                    rehearsal: None,
                })
            };
            let drop = part(Some(Box::new(held.clone())), None);
            loose.push(Loose {
                drop: drop.clone(),
                table: partition.name.clone(),
                name: Some(name.clone()),
                by,
                exclusions: true,
                new: true,
                again: true,
            });
            rebuilt.push(drop);
            rebuilt.push(part(None, Some(Box::new(exclusion.clone()))));
        }
    }
    changes.extend(rebuilt);
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests;
