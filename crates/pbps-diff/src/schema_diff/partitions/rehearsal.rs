//! The question a partition's own indexes put to a rehearsal, and the plan its
//! answer changes (#1671, DEC-1671.2, DEC-1671.3).
//!
//! Two plans ask: an attach, about the attached table's indexes, keys and
//! exclusions, and a parent's new index or exclusion, about the own ones of each
//! partition standing under it. Without an answer every one a parent's might
//! take is dropped (DEC-1671.1). That plan is where the question comes from, so
//! the two share one rule: what may be left to the engine is what the plan would
//! otherwise drop.

use super::super::common::renames_of;
use super::super::constraints::may_adopt;
use super::super::exclusions::may_adopt_exclusion;
use super::super::*;
use super::adoption::{referencing_key, standing};
use pbps_model::ObjectLocalName;
use pbps_model::adoption::{
    AdditionAnswer, AdditionRehearsal, AttachAnswer, AttachRehearsal, AttachedAnswer, IndexKind,
    PartitionOwns, RelationIndex, Remark, Skipped, addition, drops, reparameterizes, reshapes,
    unsettles,
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

/// One standing partition whose own indexes a rehearsal can be asked about
/// under its parent's new ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionQuestion {
    pub uid: Uid,
    pub parent_uid: Uid,
    pub table: TableName,
    pub parent: TableName,
    /// What the scratch runs on the parent, in plan order: the columns the plan
    /// adds to it, then each of its new indexes and exclusions that might take
    /// one of `declared` or `retiring`.
    pub changes: Vec<Change>,
    /// The partition's own the plan drops and adds back for want of an answer,
    /// each of which may be left standing.
    pub declared: BTreeSet<ObjectLocalName>,
    /// Those the plan drops because the declarations no longer hold them, each
    /// of which may be left to a parent's addition.
    pub retiring: BTreeSet<ObjectLocalName>,
    /// Every other one it drops of the partition's.
    pub dropped: BTreeSet<ObjectLocalName>,
}

impl AdditionQuestion {
    /// The parent's new indexes and exclusions, in plan order, each with what
    /// an index it takes is.
    pub fn additions(&self) -> impl Iterator<Item = (&ObjectLocalName, IndexKind)> {
        self.changes.iter().filter_map(|change| {
            let (_, name) = addition(change)?;
            let kind = if matches!(change, Change::Index(_)) {
                IndexKind::Plain
            } else {
                IndexKind::Exclusion
            };
            Some((name, kind))
        })
    }
}

/// What a rehearsal can be asked about one plan, and what it will not be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Questions {
    pub attaches: Vec<Question>,
    pub additions: Vec<AdditionQuestion>,
    /// Each attached table or standing partition with something an answer could
    /// save that is not asked about, and why.
    pub skipped: Vec<(Uid, Skipped)>,
}

impl Questions {
    pub fn is_empty(&self) -> bool {
        self.attaches.is_empty() && self.additions.is_empty() && self.skipped.is_empty()
    }
}

/// Each relation a rehearsal of these two sides could read, by the names the
/// base holds them under: a table they would attach with its parent, and a
/// standing partition with its parent where the plan could state one of the
/// parent's indexes or exclusions anew. What a connected read fetches ahead of
/// the first diff, so wider than [`questions`], which also reads the plan.
pub fn candidates(base: Side<'_>, declared: Side<'_>) -> BTreeSet<TableName> {
    let mut found = BTreeSet::new();
    // A change of type or module can have a pass state an unchanged index of the
    // parent's again, which the report then says was not rehearsed.
    let restating = base.schema.types != declared.schema.types
        || base.schema.modules != declared.schema.modules;
    for (uid, name) in &base.ids.tables {
        let Some(was) = base.schema.tables.get(name) else {
            continue;
        };
        let now = declared
            .ids
            .tables
            .get(uid)
            .and_then(|now| declared.schema.tables.get(now));
        let Some(now) = now else { continue };
        let Some(parent) = now
            .partition_of
            .as_ref()
            .and_then(|of| declared.ids.table_uid(&of.parent))
            .and_then(|uid| base.ids.tables.get(uid))
        else {
            continue;
        };
        let attached = was.partition_of.is_none() && was.partition_by.is_none();
        let grows = || {
            let new = now
                .partition_of
                .as_ref()
                .and_then(|of| declared.schema.tables.get(&of.parent));
            restating || base.schema.tables.get(parent) != new
        };
        if attached || (was.partition_of.is_some() && grows()) {
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
) -> Questions {
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
pub(super) fn taken(declared: &Schema, table: &TableName, name: &ObjectLocalName) -> bool {
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
) -> Questions {
    let mut found = Questions::default();
    if candidates(base, declared).is_empty() {
        return found;
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
    // Nothing is asked of a refused plan.
    if !plan.errors.is_empty() {
        return found;
    }
    let mut skipped = Vec::new();
    found.attaches = changes
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
            let goal = declared.schema.tables.get(parent)?;
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
            if owns.is_empty() {
                return None;
            }
            if let Some(why) = attach_skip(changes, theirs, goal) {
                skipped.push((uid.clone(), why));
                return None;
            }
            Some(Question {
                uid: uid.clone(),
                table: table.clone(),
                parent: parent.clone(),
                owns,
                dropped,
            })
        })
        .collect();
    found.skipped = skipped;
    additions(base, declared, dialect, changes, &mut found);
    found
}

/// Why no rehearsal answers for the attach of a table to a parent the base held
/// as `theirs`, whatever an engine would say. A parent this plan reshapes is not
/// yet the one the engine will attach to (DEC-2296.12), and a routine altered or
/// dropped is the planner's own reason too: what calls it is taken down around
/// that, an index left standing among it.
pub(super) fn attach_skip(
    changes: &[PlannedChange],
    theirs: &Table,
    declared: &Table,
) -> Option<Skipped> {
    if parent_shape_changes(theirs, declared) {
        Some(Skipped::ParentColumns)
    } else {
        changes
            .iter()
            .any(|p| unsettles(&p.change))
            .then_some(Skipped::Unsettles)
    }
}

/// As [`attach_skip`], for what a parent's new index takes of a standing
/// partition's, whose own `rebuilt` the plan drops to add back. Columns the plan
/// adds to the parent are no reason: the shells take them (DEC-1671.3). A
/// storage parameter changed on one of the partition's exclusions is one. On one
/// the plan rebuilds, no change of its own says so, since the rebuild carries
/// it; left standing, it would be altered ahead of the addition.
pub(super) fn addition_skip<'a>(
    changes: &[PlannedChange],
    parent: &TableName,
    partition: &TableName,
    (was, now): (&Table, &Table),
    mut rebuilt: impl Iterator<Item = &'a ObjectLocalName>,
) -> Option<Skipped> {
    let any = |asks: &dyn Fn(&Change) -> bool| changes.iter().any(|p| asks(&p.change));
    // Only an exclusion's: the differ refuses an own index's on a partition.
    let altered = |name: &ObjectLocalName| {
        was.exclude
            .get(name)
            .zip(now.exclude.get(name))
            .is_some_and(|(a, b)| a.storage_parameters != b.storage_parameters)
    };
    if any(&|c| reshapes(c, parent)) {
        Some(Skipped::ParentColumns)
    } else if any(&unsettles) {
        Some(Skipped::Unsettles)
    } else {
        (any(&|c| reparameterizes(c, partition)) || rebuilt.any(altered))
            .then_some(Skipped::OwnParameters)
    }
}

/// An index or exclusion `change` takes off the partition `table`, which holds
/// no key, and whether it is an exclusion.
fn dropped_from<'a>(change: &'a Change, table: &TableName) -> Option<(&'a ObjectLocalName, bool)> {
    let (of, name, exclusion) =
        if let Change::Index(IndexChange::DropIndex { table, name }) = change {
            (table, name, false)
        } else if let Change::Constraint(ConstraintChange::SetExclusion {
            table,
            name,
            to: None,
            ..
        }) = change
        {
            (table, name, true)
        } else {
            return None;
        };
    (of == table).then_some((name, exclusion))
}

/// Whether the addition `change` might take what `own` holds under `name`, an
/// index for an index's and an exclusion for an exclusion's.
fn might_take(
    change: &Change,
    own: &Table,
    name: &ObjectLocalName,
    dialect: &(impl Dialect + ?Sized),
) -> bool {
    if let Change::Index(IndexChange::AddIndex { index, .. }) = change {
        own.indexes
            .get(name)
            .is_some_and(|own| may_adopt(index, own, dialect))
    } else if let Change::Constraint(ConstraintChange::SetExclusion { to: Some(to), .. }) = change {
        own.exclude
            .get(name)
            .is_some_and(|own| may_adopt_exclusion(to, own, dialect))
    } else {
        false
    }
}

/// The questions of each parent's new index or exclusion in `changes`, one for
/// each partition standing under it.
fn additions(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: &[PlannedChange],
    found: &mut Questions,
) {
    let mut added: BTreeMap<&TableName, Vec<&Change>> = BTreeMap::new();
    for p in changes {
        if let Some((table, _)) = addition(&p.change) {
            added.entry(table).or_default().push(&p.change);
        }
    }
    let renames = renames_of(base, declared);
    for partition in standing(base, declared, &renames, &added) {
        let takes = |change: &Change, name: &ObjectLocalName| {
            might_take(change, &partition.was, name, dialect)
        };
        let (mut kept, mut retiring, mut dropped) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for p in changes {
            let Some((name, exclusion)) = dropped_from(&p.change, partition.name) else {
                continue;
            };
            // The loose passes say which they dropped to add back.
            if p.remarks
                .iter()
                .any(|r| matches!(r, Remark::Loose { new: true, .. }))
            {
                kept.insert(name.clone());
                continue;
            }
            let held = if exclusion {
                partition.now.exclude.contains_key(name)
            } else {
                partition.now.indexes.contains_key(name)
            };
            // One a foreign key may be bound to would leave the key on the
            // parent's clone, as the base or the declarations have the key
            // (DEC-1671.3).
            let referenced = || {
                let own = base
                    .schema
                    .tables
                    .get(partition.base_name)
                    .and_then(|t| t.indexes.get(name));
                own.is_some_and(|own| {
                    referencing_key(base.schema, partition.base_name, own).is_some()
                }) || partition.was.indexes.get(name).is_some_and(|own| {
                    referencing_key(declared.schema, partition.name, own).is_some()
                })
            };
            if !held
                && added[partition.parent].iter().any(|c| takes(c, name))
                && !taken(declared.schema, partition.name, name)
                && !referenced()
            {
                retiring.insert(name.clone());
            } else {
                dropped.insert(name.clone());
            }
        }
        if kept.is_empty() && retiring.is_empty() {
            continue;
        }
        if let Some(why) = addition_skip(
            changes,
            partition.parent,
            partition.name,
            (&partition.was, partition.now),
            kept.iter(),
        ) {
            found.skipped.push((partition.uid.clone(), why));
            continue;
        }
        let Some(parent_uid) = declared.ids.table_uid(partition.parent) else {
            continue;
        };
        let run = changes
            .iter()
            .map(|p| &p.change)
            .filter(|change| {
                matches!(change, Change::Column(ColumnChange::AddColumn { table, .. })
                    if table == partition.parent)
                    || (addition(change).is_some_and(|(table, _)| table == partition.parent)
                        && kept.iter().chain(&retiring).any(|own| takes(change, own)))
            })
            .cloned()
            .collect();
        found.additions.push(AdditionQuestion {
            uid: partition.uid.clone(),
            parent_uid: parent_uid.clone(),
            table: partition.name.clone(),
            parent: partition.parent.clone(),
            changes: run,
            declared: kept,
            retiring,
            dropped,
        });
    }
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

/// Each index of a list under its name, where its kind can be read.
fn read(list: &[RelationIndex]) -> BTreeMap<&ObjectLocalName, IndexKind> {
    list.iter()
        .filter_map(|index| Some((&index.name, index.kind()?)))
        .collect()
}

/// The one thing said about the table, where only one is.
fn only<'a>(answers: &'a [AttachAnswer], table: &Uid) -> Option<&'a AttachAnswer> {
    let mut mine = answers.iter().filter(|a| a.table() == table);
    let answer = mine.next()?;
    mine.next().is_none().then_some(answer)
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
    let AttachAnswer::Attach(AttachedAnswer {
        parent: of,
        rehearsal,
        ..
    }) = only(answers, &question.uid)?
    else {
        return None;
    };
    let (theirs, mine_now) = (read(&rehearsal.parent), read(&rehearsal.table));
    let mine_held = held(table);
    let taken_by: BTreeSet<_> = rehearsal.kept.values().collect();
    (*of == question.parent
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

/// As [`accepted`], for a standing partition `was`: the same additions, the
/// partition's own as the base holds them, and nothing claimed but what the
/// question asks, each retiring own taken by one addition of its kind.
fn accepted_addition<'a>(
    question: &AdditionQuestion,
    was: &Table,
    answers: &'a [AttachAnswer],
) -> Option<&'a AdditionAnswer> {
    let AttachAnswer::Addition(answer) = only(answers, &question.uid)? else {
        return None;
    };
    let own = read(&answer.own);
    let additions: BTreeMap<_, _> = question.additions().collect();
    let takers: BTreeSet<_> = answer.adopted.values().collect();
    (answer.parent == question.parent_uid
        && answer
            .additions
            .iter()
            .eq(question.additions().map(|(name, _)| name))
        && own.len() == answer.own.len()
        && own == held(was)
        && takers.len() == answer.adopted.len()
        && answer.adopted.iter().all(|(name, by)| {
            question.retiring.contains(name) && own.get(name) == additions.get(by)
        })
        && answer
            .beside
            .iter()
            .all(|name| question.declared.contains(name)))
    .then_some(answer.as_ref())
}

/// What the accepted answers about standing partitions change, held for the
/// passes that come after.
#[derive(Default)]
pub(in super::super) struct Answered {
    /// The declared own the plan leaves standing, so no pass drops them.
    pub beside: BTreeSet<(TableName, ObjectLocalName)>,
    additions: Vec<(AdditionQuestion, AdditionAnswer)>,
}

/// Leaves to each attach what an accepted answer says the engine takes: the
/// drop of that index, key or exclusion goes, and the attach records the
/// answer. A parent's addition's retiring own go the same way; what it leaves
/// beside is returned for the passes that would rebuild it. With no answer,
/// `changes` is left as it was.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn leave_to_engine(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
    changes: &mut Vec<Change>,
) -> Answered {
    let mut answered = Answered::default();
    if hints.attach_rehearsals.is_empty() {
        return answered;
    }
    let questions = asked(
        base, declared, dialect, hints, also, rebinding, screen, read_back,
    );
    let leave = |table: &TableName, kept: Vec<&RelationIndex>, changes: &mut Vec<Change>| {
        changes.retain(|change| !kept.iter().any(|index| drops(change, table, index)));
    };
    for question in &questions.attaches {
        let (Some(parent), Some(table)) = (
            base.schema.tables.get(&question.parent),
            base.schema.tables.get(&question.table),
        ) else {
            continue;
        };
        let Some(answer) = accepted(question, parent, table, &hints.attach_rehearsals) else {
            continue;
        };
        let kept = answer
            .table
            .iter()
            .filter(|index| answer.kept.contains_key(&index.name))
            .collect();
        leave(&question.table, kept, changes);
        for change in changes.iter_mut() {
            if let Change::Partition(PartitionChange::AttachPartition { uid, rehearsal, .. }) =
                change
                && *uid == question.uid
            {
                *rehearsal = Some(Box::new(answer.clone()));
            }
        }
    }
    for question in questions.additions {
        let Some(was) = base.schema.tables.get(&question.table) else {
            continue;
        };
        let Some(answer) = accepted_addition(&question, was, &hints.attach_rehearsals) else {
            continue;
        };
        let kept = answer
            .own
            .iter()
            .filter(|index| answer.adopted.contains_key(&index.name))
            .collect();
        leave(&question.table, kept, changes);
        answered.beside.extend(
            answer
                .beside
                .iter()
                .map(|name| (question.table.clone(), name.clone())),
        );
        answered.additions.push((question, answer.clone()));
    }
    answered
}

/// Records on each parent's addition what it takes, and what is left beside it,
/// of each partition an answer was accepted for. After every pass that states an
/// addition.
pub(in super::super) fn record_additions(
    base: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    answered: &Answered,
    changes: &mut [Change],
) {
    for change in changes {
        let Some((parent, name)) = addition(change).map(|(t, n)| (t.clone(), n.clone())) else {
            continue;
        };
        let partitions: Vec<PartitionOwns> = answered
            .additions
            .iter()
            .filter(|(question, _)| question.parent == parent)
            .filter_map(|(question, answer)| {
                let was = base.schema.tables.get(&question.table)?;
                let adopted: BTreeSet<_> = answer
                    .adopted
                    .iter()
                    .filter(|(_, by)| **by == name)
                    .map(|(own, _)| own.clone())
                    .collect();
                let beside: BTreeSet<_> = answer
                    .beside
                    .iter()
                    .filter(|own| might_take(change, was, own, dialect))
                    .cloned()
                    .collect();
                (!adopted.is_empty() || !beside.is_empty()).then(|| PartitionOwns {
                    table: question.table.clone(),
                    own: answer.own.clone(),
                    adopted,
                    beside,
                })
            })
            .collect();
        if partitions.is_empty() {
            continue;
        }
        let record = Some(Box::new(AdditionRehearsal { partitions }));
        if let Change::Index(IndexChange::AddIndex { rehearsal, .. })
        | Change::Constraint(ConstraintChange::SetExclusion { rehearsal, .. }) = change
        {
            *rehearsal = record;
        }
    }
}

/// Says, under the attach or under the parent's first new index or exclusion,
/// why a rehearsal that was asked for could not answer: one line for the
/// partitions of a step that share a reason (DEC-1671.3).
pub(in super::super) fn say_skipped(
    declared: Side<'_>,
    hints: &Hints,
    planned: &mut [PlannedChange],
) {
    let mut said: BTreeMap<(usize, Skipped), Vec<TableName>> = BTreeMap::new();
    for answer in &hints.attach_rehearsals {
        let AttachAnswer::Skipped { table: uid, why } = answer else {
            continue;
        };
        let Some(table) = declared.ids.tables.get(uid) else {
            continue;
        };
        let of = declared
            .schema
            .tables
            .get(table)
            .and_then(|t| t.partition_of.as_ref());
        // The attach's own step names its table.
        let attach = planned.iter().position(|p| {
            matches!(&p.change, Change::Partition(PartitionChange::AttachPartition {
                uid: attached, ..
            }) if attached == uid)
        });
        let addition = || {
            planned.iter().position(|p| {
                addition(&p.change)
                    .zip(of)
                    .is_some_and(|((t, _), of)| *t == of.parent)
            })
        };
        let Some(at) = attach.or_else(addition) else {
            continue;
        };
        said.entry((at, *why))
            .or_default()
            .extend(attach.is_none().then(|| table.clone()));
    }
    for ((at, why), mut partitions) in said {
        partitions.sort();
        planned[at]
            .remarks
            .push(Remark::NotRehearsed { partitions, why });
    }
}

#[cfg(test)]
mod addition_tests;
#[cfg(test)]
mod tests;
