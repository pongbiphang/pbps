//! The question an attach's own indexes put to a rehearsal, and the plan its
//! answer changes (#1671, DEC-1671.2).
//!
//! Without an answer every own index a parent's might take is dropped ahead of
//! the attach (DEC-1671.1). That plan is where the question comes from, so the
//! two share one rule: what may be left to the attach is what the plan would
//! otherwise drop.

use super::super::constraints::may_adopt;
use super::super::exclusions::may_adopt_exclusion;
use super::super::*;
use pbps_model::ObjectLocalName;
use pbps_model::adoption::{
    AttachAnswer, AttachRehearsal, IndexKind, RelationIndex, drops, unsettles,
};

/// One attach whose own indexes a rehearsal can be asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub uid: Uid,
    pub table: TableName,
    pub parent: TableName,
    /// The indexes, keys and exclusions the plan drops for want of an answer,
    /// each by the name of the index it stands on, and each of which may be
    /// left to the attach.
    pub owns: BTreeSet<ObjectLocalName>,
    /// Every other one it drops of the table's, whatever the engine says: the
    /// declarations give its name to something, or no index or constraint of
    /// the parent's might take it. A rehearsal leaves these out, as they are
    /// gone by the time of the attach.
    pub dropped: BTreeSet<ObjectLocalName>,
}

/// Each table these two sides would attach, with its parent, by the names the
/// base holds them under: what a connected read fetches ahead of the first
/// diff. Wider than [`questions`], which also reads the plan.
pub fn candidates(base: Side<'_>, declared: Side<'_>) -> BTreeSet<TableName> {
    let mut found = BTreeSet::new();
    for (uid, name) in &base.ids.tables {
        let ordinary = base
            .schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_of.is_none() && t.partition_by.is_none());
        let parent = declared
            .ids
            .tables
            .get(uid)
            .and_then(|now| declared.schema.tables.get(now)?.partition_of.as_ref())
            .and_then(|of| base.ids.tables.get(declared.ids.table_uid(&of.parent)?));
        if let (true, Some(parent)) = (ordinary, parent) {
            found.extend([name.clone(), parent.clone()]);
        }
    }
    found
}

/// What a rehearsal can be asked about the plan from `base` to `declared`.
pub fn questions(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    read_back: &Schema,
) -> Vec<Question> {
    asked(
        base,
        declared,
        dialect,
        hints,
        &BTreeSet::new(),
        Rebinding::Candidates,
        Screen::Text,
        Some(read_back),
    )
}

/// Whether the declarations give `name` to a relation of `table`'s schema, or
/// to anything of `table`'s own. An index left standing under it would be in
/// the way of what the drop made room for, so the answer would decide which
/// plans can be applied and not only what they cost.
fn taken(declared: &Schema, table: &TableName, name: &ObjectLocalName) -> bool {
    let relation = TableName::new(&table.schema, name.raw());
    declared.tables.contains_key(&relation)
        || declared.sequences.contains_key(&relation)
        || declared.modules.contains_key(&ModuleId::Named(relation))
        || declared.tables.iter().any(|(other, t)| {
            other.schema == table.schema
                && (t.indexes.contains_key(name)
                    || t.unique.contains_key(name)
                    || t.exclude.contains_key(name)
                    || t.primary_key
                        .as_ref()
                        .is_some_and(|key| key.name.as_ref() == Some(name)))
        })
        || declared
            .tables
            .get(table)
            .is_some_and(|t| t.checks.contains_key(name) || t.foreign_keys.contains_key(name))
}

/// [`questions`] of the plan these settings make, which the differ asks of its
/// own.
#[allow(clippy::too_many_arguments)]
fn asked(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Vec<Question> {
    if candidates(base, declared).is_empty() {
        return Vec::new();
    }
    let unanswered = Hints {
        attach_rehearsals: Vec::new(),
        ..hints.clone()
    };
    let plan = diff_partial_rebuilding(
        base,
        declared,
        dialect,
        &unanswered,
        also,
        rebinding,
        screen,
        read_back,
    );
    let changes = &plan.changes.changes;
    // Nothing is asked of a plan admission would refuse a record in. A routine
    // altered or dropped is also the planner's own reason: what calls it is
    // taken down around that, an index left standing among it.
    if !plan.errors.is_empty() || changes.iter().any(|p| unsettles(&p.change)) {
        return Vec::new();
    }
    changes
        .iter()
        .filter_map(|p| {
            let Change::Partition(PartitionChange::AttachPartition {
                uid, table, parent, ..
            }) = &p.change
            else {
                return None;
            };
            let (was, theirs) = (
                base.schema.tables.get(table)?,
                base.schema.tables.get(parent)?,
            );
            // A parent this plan reshapes is not yet the one the engine will
            // attach to (DEC-2296.12).
            if parent_shape_changes(theirs, declared.schema.tables.get(parent)?) {
                return None;
            }
            let (mut owns, mut dropped) = (BTreeSet::new(), BTreeSet::new());
            for planned in changes {
                // What the change takes off the table, and whether a parent's
                // might take it: one the plan drops only because the
                // declaration does not hold it is no question for an engine.
                let (name, might_take) =
                    if let Change::Index(IndexChange::DropIndex { table: of, name }) =
                        &planned.change
                        && of == table
                    {
                        let might_take = was.indexes.get(name).is_some_and(|own| {
                            theirs
                                .indexes
                                .values()
                                .any(|their| may_adopt(their, own, dialect))
                        });
                        (name, might_take)
                    } else if let Change::Constraint(ConstraintChange::SetExclusion {
                        table: of,
                        name,
                        to: None,
                        ..
                    }) = &planned.change
                        && of == table
                    {
                        // Dropped for a retirement the identities record, or for a
                        // declaration that holds it, which keeps it out of `owns`.
                        let might_take = was.exclude.get(name).is_some_and(|own| {
                            theirs
                                .exclude
                                .values()
                                .any(|their| may_adopt_exclusion(their, own, dialect))
                        });
                        (name, might_take)
                    // An attach drops a key or a unique constraint only where an
                    // index of the parent's might take it.
                    } else if let Change::Constraint(ConstraintChange::DropUnique {
                        table: of,
                        name,
                    }) = &planned.change
                        && of == table
                    {
                        (name, true)
                    } else if let Change::Constraint(ConstraintChange::SetPrimaryKey {
                        table: of,
                        from: Some(key),
                        to: None,
                        ..
                    }) = &planned.change
                        && of == table
                    {
                        // A rehearsal could not leave out a key it has no name
                        // for. A read of the database names each.
                        (key.name.as_ref()?, true)
                    } else {
                        continue;
                    };
                if might_take && !taken(declared.schema, table, name) {
                    owns.insert(name.clone());
                } else {
                    dropped.insert(name.clone());
                }
            }
            (!owns.is_empty()).then(|| Question {
                uid: uid.clone(),
                table: table.clone(),
                parent: parent.clone(),
                owns,
                dropped,
            })
        })
        .collect()
}

/// The kind of index the model holds under each name of `table`'s. A key it
/// holds no name for is under none.
fn held(table: &Table) -> BTreeMap<&ObjectLocalName, IndexKind> {
    let key = table.primary_key.as_ref().and_then(|key| key.name.as_ref());
    table
        .indexes
        .keys()
        .map(|name| (name, IndexKind::Plain))
        .chain(table.unique.keys().map(|name| (name, IndexKind::Unique)))
        .chain(
            table
                .exclude
                .keys()
                .map(|name| (name, IndexKind::Exclusion)),
        )
        .chain(key.map(|name| (name, IndexKind::PrimaryKey)))
        .collect()
}

/// The one answer for `question` that agrees with it. Anything else is no
/// answer, so one made for another state of the two relations, or for another
/// attach, is never read as this one.
fn accepted<'a>(
    question: &Question,
    parent: &Table,
    table: &Table,
    answers: &'a [AttachAnswer],
) -> Option<&'a AttachRehearsal> {
    let mut mine = answers.iter().filter(|a| a.uid == question.uid);
    let answer = mine.next()?;
    let rehearsal = &answer.rehearsal;
    let read = |list: &'a [RelationIndex]| {
        list.iter()
            .filter_map(|index| Some((&index.name, index.kind()?)))
            .collect::<BTreeMap<_, _>>()
    };
    let (theirs, mine_now) = (read(&rehearsal.parent), read(&rehearsal.table));
    let mine_held = held(table);
    let taken_by: BTreeSet<_> = rehearsal.kept.values().collect();
    (mine.next().is_none()
        && answer.parent == question.parent
        && theirs.len() == rehearsal.parent.len()
        && theirs == held(parent)
        && question
            .owns
            .iter()
            .all(|own| mine_now.contains_key(own) && mine_now.get(own) == mine_held.get(own))
        && !rehearsal.kept.is_empty()
        && taken_by.len() == rehearsal.kept.len()
        && rehearsal
            .kept
            .iter()
            .all(|(own, by)| question.owns.contains(own) && rehearsal.pairs(own, by)))
    .then_some(rehearsal)
}

/// Leaves to each attach what an accepted answer says the engine takes: the
/// drop of that index, key or exclusion goes, and the attach records the
/// answer. With none, `changes` is left as it was.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn leave_to_attach(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
    changes: &mut Vec<Change>,
) {
    if hints.attach_rehearsals.is_empty() {
        return;
    }
    for question in asked(
        base, declared, dialect, hints, also, rebinding, screen, read_back,
    ) {
        let (Some(parent), Some(table)) = (
            base.schema.tables.get(&question.parent),
            base.schema.tables.get(&question.table),
        ) else {
            continue;
        };
        let Some(answer) = accepted(&question, parent, table, &hints.attach_rehearsals) else {
            continue;
        };
        let kept: Vec<_> = answer
            .table
            .iter()
            .filter(|index| answer.kept.contains_key(&index.name))
            .collect();
        changes.retain(|change| {
            !kept
                .iter()
                .any(|index| drops(change, &question.table, index))
        });
        for change in changes.iter_mut() {
            if let Change::Partition(PartitionChange::AttachPartition { uid, rehearsal, .. }) =
                change
                && *uid == question.uid
            {
                *rehearsal = Some(Box::new(answer.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests;
