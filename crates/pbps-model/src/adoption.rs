//! What a resolver's rehearsal answered about the indexes an attach, or a
//! parent's new index or exclusion, leaves to the engine, and what apply holds
//! the database to because of it (#1671, DEC-1671.2, DEC-1671.3).
//!
//! Which of a table's indexes the engine takes as the clone of one of its
//! parent's depends on what their expressions bind to and on the order they
//! were made in. The model holds neither. A rehearsal asks an engine, and the
//! plan then rests on both facts staying as they were read: the lists here are
//! that record.

use crate::{
    Change, ChangeSet, ColumnChange, ConstraintChange, ExtensionChange, IndexChange, ModuleChange,
    ModuleId, ObjectLocalName, PartitionChange, SavedPlan, TableChange, TableName, Text,
    TypeChange, TypeValueChange, Uid,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One index of a relation as its catalog spells it. A list of them is in the
/// order the engine looks through for an index to take: the order they were
/// made in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelationIndex {
    pub name: ObjectLocalName,
    /// The `CREATE INDEX` statement the engine prints for it.
    pub index: Text,
    /// The definition of the key, unique or exclusion constraint standing on
    /// it, where one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<Text>,
}

/// What stands on an index. An attach pairs one of the table's with one of the
/// parent's of the same kind only (DEC-1671.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// No constraint.
    Plain,
    PrimaryKey,
    Unique,
    Exclusion,
}

impl RelationIndex {
    /// Read from the constraint's definition as the engine prints it. `None` is
    /// a definition that is none of these, which pairs with nothing.
    pub fn kind(&self) -> Option<IndexKind> {
        let Some(constraint) = &self.constraint else {
            return Some(IndexKind::Plain);
        };
        [
            ("PRIMARY KEY", IndexKind::PrimaryKey),
            ("UNIQUE", IndexKind::Unique),
            ("EXCLUDE", IndexKind::Exclusion),
        ]
        .into_iter()
        .find_map(|(word, kind)| constraint.raw().starts_with(word).then_some(kind))
    }
}

/// An index as a database holds it now. An invalid one is never taken by an
/// attach, and its definition does not show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldIndex {
    pub index: RelationIndex,
    pub valid: bool,
    /// A parent's clone, which no list of a relation's own indexes holds.
    pub inherited: bool,
}

/// A relation as a rehearsal and apply read it: enough to make an empty copy of
/// it, and its indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// Each column as `CREATE TABLE` spells it: name, type, collation and
    /// `NOT NULL`.
    pub columns: Vec<String>,
    /// The partition key, for a partitioned table.
    pub partition_key: Option<String>,
    pub indexes: Vec<HeldIndex>,
}

/// The relations a read was asked for. `None` is one the catalog does not hold,
/// which is not one that has no index.
pub type Relations = BTreeMap<TableName, Option<Relation>>;

/// What the plan records on an attach that leaves an index to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachRehearsal {
    /// The parent's indexes as planning read them.
    pub parent: Vec<RelationIndex>,
    /// The table's, before any of the plan's drops.
    pub table: Vec<RelationIndex>,
    /// Each of the table's own indexes the plan leaves standing, a key's or an
    /// exclusion's by its name, and the parent's index the rehearsal's engine
    /// gave it to.
    pub kept: BTreeMap<ObjectLocalName, ObjectLocalName>,
}

impl AttachRehearsal {
    /// Whether both lists hold these two names and the indexes are of one kind.
    /// A constraint's index under a plain index of the parent's would leave the
    /// constraint the table's own, and PostgreSQL 16 also gives a key to a
    /// unique constraint, so nothing else may be left to an attach.
    pub fn pairs(&self, own: &ObjectLocalName, parents: &ObjectLocalName) -> bool {
        let kind = |list: &[RelationIndex], name: &ObjectLocalName| {
            list.iter()
                .find(|index| index.name == *name)
                .and_then(RelationIndex::kind)
        };
        kind(&self.table, own).is_some_and(|own| kind(&self.parent, parents) == Some(own))
    }
}

/// An attach rehearsal's answer on its way to the differ, which checks it
/// against the question it would ask before using it. One that keeps nothing
/// says the engine takes none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedAnswer {
    pub uid: Uid,
    pub parent: TableName,
    pub rehearsal: AttachRehearsal,
}

/// What one standing partition's own indexes are under a parent's addition, as
/// the plan records it on that addition (DEC-1671.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PartitionOwns {
    pub table: TableName,
    /// The partition's own indexes as planning read them, a parent's clones
    /// left out.
    pub own: Vec<RelationIndex>,
    /// Each own the declarations no longer hold that this addition takes, so
    /// the plan does not drop it: it is the addition's copy.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub adopted: BTreeSet<ObjectLocalName>,
    /// Each declared own this addition does not take, so the plan neither drops
    /// it nor adds it again: it stands beside the copy.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub beside: BTreeSet<ObjectLocalName>,
}

/// What the plan records on a parent's index or exclusion that leaves a
/// partition's own to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdditionRehearsal {
    pub partitions: Vec<PartitionOwns>,
}

/// A rehearsal's answer about one standing partition, on its way to the differ,
/// which checks it against the question it would ask before using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionAnswer {
    pub parent: Uid,
    pub uid: Uid,
    /// The parent's new indexes and exclusions the rehearsal ran, in order.
    pub additions: Vec<ObjectLocalName>,
    /// The partition's own indexes as planning read them.
    pub own: Vec<RelationIndex>,
    /// Each retiring own an addition took, and that addition.
    pub adopted: BTreeMap<ObjectLocalName, ObjectLocalName>,
    /// Each declared own no addition took.
    pub beside: BTreeSet<ObjectLocalName>,
}

/// Why a rehearsal that had something it could save gave no answer. The plan is
/// then the one made without a resolver, and the report says which of these it
/// was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skipped {
    /// A column of the parent's is retyped, renamed, dropped or otherwise
    /// altered, so a definition read now is not the one the engine will compare.
    ParentColumns,
    /// [`unsettles`].
    Unsettles,
    InvalidIndex,
    /// A storage parameter of one of the partition's own indexes changes, ahead
    /// of the parent's addition, which prints its definition otherwise.
    OwnParameters,
    /// The scratch could not run it.
    Scratch,
}

impl Skipped {
    /// The reason in the report's words.
    pub fn why(self) -> &'static str {
        match self {
            Skipped::ParentColumns => "this plan also changes the parent's columns",
            Skipped::Unsettles => {
                "this plan also renames, rebuilds or moves what an index definition is read \
                 through, or alters a function"
            }
            Skipped::InvalidIndex => "one of the indexes it would read is invalid",
            Skipped::OwnParameters => {
                "this plan also changes the storage parameters of one of the partition's indexes"
            }
            Skipped::Scratch => "the resolver's scratch could not run it",
        }
    }
}

/// What a resolver made of one table's own indexes, an attached one's or a
/// standing partition's, on its way to the differ. With no resolver there is
/// none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachAnswer {
    Attach(AttachedAnswer),
    Addition(Box<AdditionAnswer>),
    Skipped { table: Uid, why: Skipped },
}

impl AttachAnswer {
    /// The attached table's identity, or the partition's.
    pub fn table(&self) -> &Uid {
        match self {
            AttachAnswer::Attach(answer) => &answer.uid,
            AttachAnswer::Addition(answer) => &answer.uid,
            AttachAnswer::Skipped { table, .. } => table,
        }
    }
}

/// Each partition the addition `change` takes an own of. It changes them with no
/// change of the plan on them (DEC-1671.3).
pub fn adopts(change: &Change) -> impl Iterator<Item = &PartitionOwns> {
    record(change)
        .into_iter()
        .flat_map(|(_, rehearsal)| &rehearsal.partitions)
        .filter(|of| !of.adopted.is_empty())
}

/// What the report says of a change beyond what it does. Never sealed, as a
/// change's provenance is not: a plan read back from its file has none to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remark {
    /// An index, key or exclusion the plan drops because one of these of the
    /// parent's may take it as its copy (DEC-1671.1), on the drop and on the
    /// add after it.
    Loose {
        by: Vec<ObjectLocalName>,
        /// `by` names exclusion constraints, not indexes.
        exclusions: bool,
        /// The parent's are ones this plan adds.
        new: bool,
        /// The declarations keep it, so the plan adds it again.
        again: bool,
        /// No resolver was asked, and one could keep it.
        resolver_could_keep: bool,
    },
    /// A rehearsal that had something it could save gave no answer. `partitions`
    /// is empty on an attach, whose step already names the table.
    NotRehearsed {
        partitions: Vec<TableName>,
        why: Skipped,
    },
}

/// Whether `change` takes the index `index` off `relation`: the forms a plan
/// removes an index in ahead of an attach.
pub fn drops(change: &Change, relation: &TableName, index: &RelationIndex) -> bool {
    if let Change::Index(IndexChange::DropIndex { table, name })
    | Change::Constraint(ConstraintChange::DropUnique { table, name })
    | Change::Constraint(ConstraintChange::SetExclusion {
        table,
        name,
        from: Some(_),
        to: None,
        ..
    }) = change
    {
        table == relation && *name == index.name
    } else if let Change::Constraint(ConstraintChange::SetPrimaryKey {
        table,
        from: Some(key),
        to: None,
        ..
    }) = change
    {
        // A key the model holds no name for is the relation's one primary key.
        table == relation
            && key.name.as_ref().map_or_else(
                || index.kind() == Some(IndexKind::PrimaryKey),
                |name| *name == index.name,
            )
    } else {
        false
    }
}

/// The index or exclusion `change` adds to `relation`, by its name.
pub fn adds<'a>(change: &'a Change, relation: &TableName) -> Option<&'a ObjectLocalName> {
    let (table, name) = addition(change)?;
    (table == relation).then_some(name)
}

/// A new index or exclusion, with the table it is added to: the changes that can
/// record what a partition's own indexes are under it.
pub fn addition(change: &Change) -> Option<(&TableName, &ObjectLocalName)> {
    if let Change::Index(IndexChange::AddIndex { table, name, .. })
    | Change::Constraint(ConstraintChange::SetExclusion {
        table,
        name,
        from: None,
        to: Some(_),
        ..
    }) = change
    {
        Some((table, name))
    } else {
        None
    }
}

/// What the addition `change` records, where it records anything.
pub fn record(change: &Change) -> Option<(&TableName, &AdditionRehearsal)> {
    if let Change::Index(IndexChange::AddIndex {
        table,
        rehearsal: Some(rehearsal),
        ..
    })
    | Change::Constraint(ConstraintChange::SetExclusion {
        table,
        rehearsal: Some(rehearsal),
        ..
    }) = change
    {
        Some((table, rehearsal))
    } else {
        None
    }
}

/// What a standing partition's own indexes should be just before the change at
/// `at`: the recorded list less what earlier changes drop and what earlier
/// recorded additions take, then the names earlier changes add, which follow in
/// order with definitions the engine will print.
pub fn standing_own(
    changes: &ChangeSet,
    at: usize,
    partition: &TableName,
    recorded: &[RelationIndex],
) -> (Vec<RelationIndex>, Vec<ObjectLocalName>) {
    let ahead = &changes.changes[..at.min(changes.changes.len())];
    let adopted = |name: &ObjectLocalName| {
        ahead.iter().any(|p| {
            record(&p.change).is_some_and(|(_, r)| {
                r.partitions
                    .iter()
                    .any(|of| of.table == *partition && of.adopted.contains(name))
            })
        })
    };
    let kept = standing(changes, at, partition, recorded)
        .into_iter()
        .filter(|index| !adopted(&index.name))
        .collect();
    let added = ahead
        .iter()
        .filter_map(|p| adds(&p.change, partition).cloned())
        .collect();
    (kept, added)
}

/// [`difference`] where `added` follow `expected` by name alone.
pub fn difference_after(
    expected: &[RelationIndex],
    added: &[ObjectLocalName],
    found: &[HeldIndex],
) -> Option<String> {
    let mut known = expected.to_vec();
    for (place, name) in added.iter().enumerate() {
        // What stands there under that name, whatever it prints. One that does
        // not is a difference the comparison names.
        let held = found
            .get(expected.len() + place)
            .filter(|held| held.index.name == *name);
        known.push(held.map_or_else(
            || RelationIndex {
                name: name.clone(),
                index: Text::default(),
                constraint: None,
            },
            |held| held.index.clone(),
        ));
    }
    difference(&known, found)
}

/// What `relation` should hold once the changes ahead of the one at `at` have
/// run: the recorded list less what they drop. Nothing adds an index to either
/// side of an attach ahead of it, so an addition shows as a difference.
pub fn standing(
    changes: &ChangeSet,
    at: usize,
    relation: &TableName,
    recorded: &[RelationIndex],
) -> Vec<RelationIndex> {
    let ahead = &changes.changes[..at.min(changes.changes.len())];
    recorded
        .iter()
        .filter(|index| !ahead.iter().any(|p| drops(&p.change, relation, index)))
        .cloned()
        .collect()
}

/// The first way `found` is not `expected`, in words that name the index, or
/// `None` where they are the same indexes, defined alike, in the same order and
/// all valid.
pub fn difference(expected: &[RelationIndex], found: &[HeldIndex]) -> Option<String> {
    if let Some(held) = found.iter().find(|held| !held.valid) {
        return Some(format!(
            "its index `{name}` is invalid",
            name = held.index.name.shown()
        ));
    }
    let definition = |index: &RelationIndex| {
        index
            .constraint
            .as_ref()
            .unwrap_or(&index.index)
            .shown()
            .to_string()
    };
    (0..expected.len().max(found.len())).find_map(|at| {
        let (was, now) = (expected.get(at), found.get(at).map(|held| &held.index));
        if was == now {
            return None;
        }
        if let Some(was) = was
            && !found.iter().any(|held| held.index.name == was.name)
        {
            return Some(format!(
                "it no longer has the index `{name}`",
                name = was.name.shown()
            ));
        }
        if let Some(now) = now
            && !expected.iter().any(|index| index.name == now.name)
        {
            return Some(format!(
                "it has the index `{name}`, which the plan did not see there",
                name = now.name.shown()
            ));
        }
        let (was, now) = (was?, now?);
        Some(if was.name == now.name {
            format!(
                "its index `{name}` is defined otherwise: the plan saw `{}`, and it is `{}` now",
                definition(was),
                definition(now),
                name = was.name.shown()
            )
        } else {
            format!(
                "its indexes were made in another order: `{now}` is where the plan saw `{was}`, \
                 and the engine takes the first that fits",
                now = now.name.shown(),
                was = was.name.shown()
            )
        })
    })
}

/// A change that can alter what an index definition prints, or what it depends
/// on, between planning's read and the attach: a rename of a table, a column, a
/// type or an enum label, an enum rebuilt, an extension moved or updated, or a routine
/// altered or dropped. A recorded list would then differ from the database only
/// because the plan ran, so a plan that holds one records no rehearsal: the
/// planning half asks nothing for it, and admission refuses one that does
/// (DEC-1671.2).
pub fn unsettles(change: &Change) -> bool {
    matches!(
        change,
        Change::Table(TableChange::RenameTable { .. })
            | Change::Column(ColumnChange::RenameColumn { .. })
            | Change::Type(TypeChange::RenameType { .. })
            | Change::TypeValue(
                TypeValueChange::RenameEnumValue { .. } | TypeValueChange::EnumRebuild { .. }
            )
            | Change::Extension(
                ExtensionChange::SetExtensionSchema { .. }
                    | ExtensionChange::UpdateExtension { .. }
            )
            | Change::Module(
                ModuleChange::AlterModule {
                    id: ModuleId::Routine(_),
                    ..
                } | ModuleChange::DropModule {
                    id: ModuleId::Routine(_),
                    ..
                }
            )
    )
}

/// A change to a column of `parent`'s other than an addition, which reprints or
/// rebinds a definition read before it. A column the plan adds is in the shells a
/// rehearsal makes (DEC-1671.3).
pub fn reshapes(change: &Change, parent: &TableName) -> bool {
    matches!(change, Change::Column(column)
        if !matches!(column, ColumnChange::AddColumn { .. }
            | ColumnChange::SetColumnDeprecated { .. }))
        && change.table() == Some(parent)
}

/// A storage parameter change of one of `relation`'s indexes, which prints its
/// definition otherwise and runs before a parent's addition.
pub fn reparameterizes(change: &Change, relation: &TableName) -> bool {
    matches!(change, Change::Index(IndexChange::SetIndexStorageParameters { table, .. })
        if table == relation)
}

/// Admission of what a saved plan records. The record is what apply compares the
/// database with and what a reviewer is told stays, so one that does not agree
/// with its own plan is refused.
pub fn validate(plan: &SavedPlan) -> Result<(), String> {
    validate_additions(plan)?;
    for planned in &plan.changes.changes {
        let Change::Partition(PartitionChange::AttachPartition {
            table,
            shape,
            rehearsal: Some(rehearsal),
            ..
        }) = &planned.change
        else {
            continue;
        };
        let distinct = |list: &[RelationIndex]| {
            list.iter().map(|i| &i.name).collect::<BTreeSet<_>>().len() == list.len()
        };
        // A name the partition declares as its own is never left to be the
        // copy of the parent's.
        let declared = |name: &ObjectLocalName| {
            shape.indexes.contains_key(name)
                || shape.unique.contains_key(name)
                || shape.exclude.contains_key(name)
                || shape
                    .primary_key
                    .as_ref()
                    .is_some_and(|key| key.name.as_ref() == Some(name))
                // A check or foreign key shares the constraint namespace, so the
                // planner never keeps an own under one's name (`taken`).
                || shape.checks.contains_key(name)
                || shape.foreign_keys.contains_key(name)
        };
        let taken: BTreeSet<_> = rehearsal.kept.values().collect();
        let holds = plan.dialect == "postgres"
            && !rehearsal.kept.is_empty()
            && !plan.changes.changes.iter().any(|p| unsettles(&p.change))
            && distinct(&rehearsal.parent)
            && distinct(&rehearsal.table)
            && taken.len() == rehearsal.kept.len()
            && rehearsal.kept.iter().all(|(own, theirs)| {
                rehearsal.pairs(own, theirs)
                    && !declared(own)
                    && !rehearsal.table.iter().any(|index| {
                        index.name == *own
                            && plan
                                .changes
                                .changes
                                .iter()
                                .any(|p| drops(&p.change, table, index))
                    })
            });
        if !holds {
            return Err(format!(
                "invalid attach rehearsal on {table}",
                table = table.shown()
            ));
        }
    }
    Ok(())
}

/// A parent's record: its partitions' lists, and what its addition takes or
/// leaves of them, agreeing with each other and with the plan's other changes.
fn validate_additions(plan: &SavedPlan) -> Result<(), String> {
    let changes = &plan.changes.changes;
    // Across records: every list of one partition is the same list, and an own
    // one addition takes is taken by no other and left beside none.
    let mut lists: BTreeMap<&TableName, &[RelationIndex]> = BTreeMap::new();
    let mut taken: BTreeSet<(&TableName, &ObjectLocalName)> = BTreeSet::new();
    let mut left: BTreeSet<(&TableName, &ObjectLocalName)> = BTreeSet::new();
    for planned in changes {
        let Some((parent, rehearsal)) = record(&planned.change) else {
            continue;
        };
        // A drop of an exclusion holds the field too, and is never recorded.
        if addition(&planned.change).is_none() {
            return Err("a rehearsal is recorded on a change that adds nothing".to_owned());
        }
        let refused = || {
            Err(format!(
                "invalid parent rehearsal on {parent}",
                parent = parent.shown()
            ))
        };
        let kind = if matches!(planned.change, Change::Index(_)) {
            IndexKind::Plain
        } else {
            IndexKind::Exclusion
        };
        let tables: BTreeSet<_> = rehearsal.partitions.iter().map(|of| &of.table).collect();
        let holds = plan.dialect == "postgres"
            && !rehearsal.partitions.is_empty()
            && tables.len() == rehearsal.partitions.len()
            && !tables.contains(parent)
            && !changes
                .iter()
                .any(|p| unsettles(&p.change) || reshapes(&p.change, parent));
        if !holds {
            return refused();
        }
        for of in &rehearsal.partitions {
            let named: BTreeSet<_> = of.own.iter().map(|i| &i.name).collect();
            let claimed = || of.adopted.iter().chain(&of.beside);
            let agrees = named.len() == of.own.len()
                && claimed().next().is_some()
                && claimed().all(|name| {
                    of.own
                        .iter()
                        .any(|i| i.name == *name && i.kind() == Some(kind))
                })
                && lists
                    .insert(&of.table, &of.own)
                    .is_none_or(|before| before == of.own.as_slice())
                && of.adopted.iter().all(|name| {
                    taken.insert((&of.table, name)) && !left.contains(&(&of.table, name))
                })
                && of.beside.iter().all(|name| {
                    left.insert((&of.table, name));
                    !taken.contains(&(&of.table, name))
                })
                && !changes.iter().any(|p| {
                    claimed().any(|name| {
                        adds(&p.change, &of.table) == Some(name)
                            || of
                                .own
                                .iter()
                                .any(|i| i.name == *name && drops(&p.change, &of.table, i))
                    }) || reparameterizes(&p.change, &of.table)
                });
            if !agrees {
                return refused();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(name: &str) -> RelationIndex {
        RelationIndex {
            name: name.into(),
            index: format!("CREATE INDEX {name} ON app.c USING btree (n)").into(),
            constraint: None,
        }
    }

    fn held(list: &[RelationIndex]) -> Vec<HeldIndex> {
        list.iter()
            .map(|index| HeldIndex {
                index: index.clone(),
                valid: true,
                inherited: false,
            })
            .collect()
    }

    fn keyed(name: &str, constraint: &str) -> RelationIndex {
        RelationIndex {
            constraint: Some(constraint.into()),
            ..index(name)
        }
    }

    #[test]
    fn the_same_indexes_in_the_same_order_are_no_difference() {
        let list = [index("a"), index("b")];
        assert_eq!(difference(&list, &held(&list)), None);
        assert_eq!(difference(&[], &[]), None);
    }

    #[test]
    fn every_other_list_is_a_difference_that_names_the_index() {
        let list = [index("a"), index("b")];
        let says = |found: &[HeldIndex]| difference(&list, found).expect("a difference");
        assert!(says(&held(&[index("b"), index("a")])).contains("made in another order: `b`"));
        let mut redefined = index("a");
        redefined.index = "CREATE INDEX a ON app.c USING btree (m)".into();
        let found = says(&held(&[redefined, index("b")]));
        assert!(found.contains("`a` is defined otherwise"), "{found}");
        assert!(found.contains("(m)"), "{found}");
        let found = says(&held(&[keyed("a", "UNIQUE (n)"), index("b")]));
        assert!(found.contains("it is `UNIQUE (n)` now"), "{found}");
        assert!(says(&held(&[index("a")])).contains("no longer has the index `b`"));
        assert!(
            says(&held(&[index("a"), index("b"), index("x")]))
                .contains("has the index `x`, which the plan did not see")
        );
        assert!(says(&held(&[index("a"), index("x")])).contains("no longer has the index `b`"));
        let mut invalid = held(&list);
        invalid[1].valid = false;
        assert!(says(&invalid).contains("`b` is invalid"));
    }

    #[test]
    fn a_definition_from_the_database_cannot_start_a_line_of_its_own() {
        let mut found = index("a");
        found.index = "CREATE INDEX a\nforged".into();
        let said = difference(&[index("a")], &held(&[found])).unwrap();
        assert!(!said.contains('\n'), "{said}");
    }

    #[test]
    fn what_stands_before_an_attach_is_the_record_less_the_earlier_drops_of_that_relation() {
        let table = TableName::new("app", "c");
        let other = TableName::new("app", "p");
        let record = [
            index("a"),
            index("b"),
            keyed("u", "UNIQUE (n)"),
            keyed("x", "EXCLUDE USING gist (r WITH &&)"),
            keyed("pk", "PRIMARY KEY (k)"),
        ];
        let planned = |change| crate::PlannedChange::new(change);
        let key = |name: Option<&str>| crate::schema::PrimaryKey {
            name: name.map(Into::into),
            columns: vec!["k".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        };
        for name in [Some("pk"), None] {
            let changes = ChangeSet {
                changes: vec![
                    planned(Change::Index(IndexChange::DropIndex {
                        table: table.clone(),
                        name: "a".into(),
                    })),
                    // Another relation's drop of the same name takes nothing.
                    planned(Change::Index(IndexChange::DropIndex {
                        table: other.clone(),
                        name: "b".into(),
                    })),
                    planned(Change::Constraint(ConstraintChange::DropUnique {
                        table: table.clone(),
                        name: "u".into(),
                    })),
                    planned(Change::Constraint(ConstraintChange::SetPrimaryKey {
                        table: table.clone(),
                        from: Some(key(name)),
                        to: None,
                        nonclustered: false,
                    })),
                    // A drop after the position has not run yet.
                    planned(Change::Index(IndexChange::DropIndex {
                        table: table.clone(),
                        name: "x".into(),
                    })),
                ],
            };
            let left: Vec<_> = standing(&changes, 4, &table, &record)
                .into_iter()
                .map(|i| i.name)
                .collect();
            assert_eq!(left, ["b".into(), "x".into()], "{name:?}");
            assert_eq!(standing(&changes, 0, &table, &record), record);
        }
    }

    /// A plan that drops `c_a` and attaches `app.c` over `c_z`.
    fn recorded() -> SavedPlan {
        let table = TableName::new("app", "c");
        let attach = Change::Partition(PartitionChange::AttachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: table.clone(),
            parent: TableName::new("app", "p"),
            bound: crate::PartitionBound::Default,
            key: Vec::new(),
            shape: Box::default(),
            rehearsal: Some(Box::new(AttachRehearsal {
                parent: vec![index("p_n"), keyed("p_u", "UNIQUE (n)")],
                table: vec![index("c_z"), index("c_a"), keyed("c_u", "UNIQUE (n)")],
                kept: [("c_z".into(), "p_n".into())].into(),
            })),
        });
        let drop = Change::Index(IndexChange::DropIndex {
            table,
            name: "c_a".into(),
        });
        SavedPlan::new(
            crate::PlanOrigin::Database,
            "postgres",
            "2026-11-26T00:00:00Z",
            crate::PlanBaseline {
                time_zone: None,
                description: "test".into(),
                checksum: String::new(),
                database_collation: None,
            },
            ChangeSet {
                changes: [drop, attach]
                    .into_iter()
                    .map(crate::PlannedChange::new)
                    .collect(),
            },
            Default::default(),
        )
    }

    fn edited(edit: impl FnOnce(&mut AttachRehearsal, &mut crate::Table)) -> SavedPlan {
        let mut plan = recorded();
        let Change::Partition(PartitionChange::AttachPartition {
            rehearsal: Some(rehearsal),
            shape,
            ..
        }) = &mut plan.changes.changes[1].change
        else {
            panic!("the attach")
        };
        edit(rehearsal, shape);
        plan
    }

    #[test]
    fn a_record_that_agrees_with_its_plan_is_admitted_and_survives_the_file() {
        let plan = recorded();
        assert_eq!(validate(&plan), Ok(()));
        let written = serde_json::to_string(&plan).unwrap();
        let read: SavedPlan = serde_json::from_str(&written).unwrap();
        assert_eq!(read.changes, plan.changes);
        // An attach without one writes no field.
        let mut bare = plan;
        let Change::Partition(PartitionChange::AttachPartition { rehearsal, .. }) =
            &mut bare.changes.changes[1].change
        else {
            panic!("the attach")
        };
        *rehearsal = None;
        assert!(
            !serde_json::to_string(&bare)
                .unwrap()
                .contains("rehearsal\":{")
        );
        assert_eq!(validate(&bare), Ok(()));
    }

    #[test]
    fn a_record_that_does_not_agree_with_its_plan_is_refused() {
        let keep = |own: &str, by: &str| [(own.into(), by.into())].into();
        let mut cases = vec![
            ("nothing kept", edited(|r, _| r.kept.clear())),
            (
                "an index the plan drops",
                edited(|r, _| r.kept = keep("c_a", "p_n")),
            ),
            (
                "an unknown index",
                edited(|r, _| r.kept = keep("c_x", "p_n")),
            ),
            (
                "a constraint's index",
                edited(|r, _| r.kept = keep("c_u", "p_n")),
            ),
            (
                "an unknown taker",
                edited(|r, _| r.kept = keep("c_z", "p_x")),
            ),
            (
                "a taker that is a constraint's",
                edited(|r, _| r.kept = keep("c_z", "p_u")),
            ),
            (
                "one taker for two",
                edited(|r, _| {
                    r.table.push(index("c_b"));
                    r.kept.insert("c_b".into(), "p_n".into());
                }),
            ),
            (
                "a name twice on the table",
                edited(|r, _| r.table.push(index("c_z"))),
            ),
            (
                "a name twice on the parent",
                edited(|r, _| r.parent.push(index("p_n"))),
            ),
            (
                "an index the declaration holds",
                edited(|_, shape| {
                    shape.indexes.insert(
                        "c_z".into(),
                        crate::Index {
                            columns: vec![crate::IndexColumn::column("n")],
                            include: Vec::new(),
                            unique: false,
                            filter: None,
                            method: Default::default(),
                            storage_parameters: Default::default(),
                            filegroup: None,
                        },
                    );
                }),
            ),
        ];
        let mut elsewhere = recorded();
        elsewhere.dialect = "mssql".into();
        cases.push(("another engine", elsewhere));
        // A plan that renames a column, even of another table, can change what
        // a recorded definition prints before the attach (draft round 1).
        let mut renaming = recorded();
        renaming.changes.changes.insert(
            0,
            crate::PlannedChange::new(Change::Column(ColumnChange::RenameColumn {
                uid: crate::Uid::generate(crate::UidKind::Column),
                table: "app.other".parse().unwrap(),
                from: "a".into(),
                to: "b".into(),
                table_was: None,
            })),
        );
        cases.push(("a plan that renames a column", renaming));
        // An extension update can reprint an operator class an index names
        // (draft round 2).
        let mut updating = recorded();
        let package = |version: &str| crate::Extension {
            schema: "public".into(),
            version: version.into(),
        };
        updating.changes.changes.insert(
            0,
            crate::PlannedChange::new(Change::Extension(ExtensionChange::UpdateExtension {
                uid: crate::Uid::generate(crate::UidKind::Extension),
                name: "btree_gist".into(),
                from: package("1.6"),
                to: package("1.7"),
                requires: Default::default(),
                releases: Default::default(),
            })),
        );
        cases.push(("a plan that updates an extension", updating));
        for (why, plan) in cases {
            assert_eq!(
                validate(&plan),
                Err("invalid attach rehearsal on app.c".to_owned()),
                "{why}"
            );
        }
    }

    #[test]
    fn a_kind_is_read_from_the_constraint_the_engine_prints() {
        assert_eq!(index("a").kind(), Some(IndexKind::Plain));
        for (constraint, kind) in [
            ("PRIMARY KEY (k)", Some(IndexKind::PrimaryKey)),
            ("UNIQUE NULLS NOT DISTINCT (k)", Some(IndexKind::Unique)),
            ("EXCLUDE USING gist (r WITH &&)", Some(IndexKind::Exclusion)),
            ("CHECK (k > 0)", None),
            ("", None),
        ] {
            assert_eq!(keyed("a", constraint).kind(), kind, "{constraint}");
        }
    }

    /// [`recorded`] keeping a key, a unique constraint and an exclusion, each
    /// under the parent's of its kind.
    fn constrained(edit: impl FnOnce(&mut AttachRehearsal, &mut crate::Table)) -> SavedPlan {
        edited(|record, shape| {
            let each = |prefix: &str| {
                vec![
                    keyed(&format!("{prefix}_pk"), "PRIMARY KEY (k)"),
                    keyed(&format!("{prefix}_u"), "UNIQUE (n)"),
                    keyed(&format!("{prefix}_x"), "EXCLUDE USING btree (n WITH =)"),
                    index(&format!("{prefix}_n")),
                ]
            };
            record.parent = each("p");
            record.table = each("c");
            record.kept = ["pk", "u", "x"]
                .map(|of| (format!("c_{of}").into(), format!("p_{of}").into()))
                .into();
            edit(record, shape);
        })
    }

    #[test]
    fn a_kept_constraint_is_admitted_only_under_one_of_its_kind_that_the_plan_leaves() {
        assert_eq!(validate(&constrained(|_, _| {})), Ok(()));
        let one = |own: &str, by: &str| [(own.into(), by.into())].into();
        let mut cases = Vec::new();
        for (own, by) in [
            ("c_pk", "p_u"),
            ("c_u", "p_pk"),
            ("c_x", "p_u"),
            ("c_u", "p_x"),
            ("c_u", "p_n"),
            ("c_x", "p_n"),
            ("c_n", "p_u"),
        ] {
            cases.push((
                format!("{own} under {by}"),
                constrained(|r, _| r.kept = one(own, by)),
            ));
        }
        cases.push((
            "a constraint of no known kind on both sides".to_owned(),
            constrained(|r, _| {
                r.parent[1] = keyed("p_u", "CHECK (n > 0)");
                r.table[1] = keyed("c_u", "CHECK (n > 0)");
            }),
        ));
        cases.push((
            "an exclusion the declaration holds".to_owned(),
            constrained(|_, shape| {
                shape.exclude.insert(
                    "c_x".into(),
                    crate::ExclusionConstraint {
                        method: Default::default(),
                        keys: Vec::new(),
                        include: Vec::new(),
                        filter: None,
                        storage_parameters: Default::default(),
                        timing: crate::ExclusionTiming::Immediate,
                    },
                );
            }),
        ));
        // A check the declaration gives the kept unique constraint's name
        // (draft review round 2).
        cases.push((
            "a check the declaration names as the kept constraint".to_owned(),
            constrained(|_, shape| {
                shape.checks.insert(
                    "c_u".into(),
                    crate::CheckConstraint {
                        expression: "n > 0".into(),
                    },
                );
            }),
        ));
        let table = TableName::new("app", "c");
        let key = |name: Option<&str>| crate::schema::PrimaryKey {
            name: name.map(Into::into),
            columns: vec!["k".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        };
        let primary = |name| {
            Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: table.clone(),
                from: Some(key(name)),
                to: None,
                nonclustered: false,
            })
        };
        for (why, change) in [
            ("a kept key dropped by name", primary(Some("c_pk"))),
            ("a kept key dropped without one", primary(None)),
            (
                "a kept unique constraint dropped",
                Change::Constraint(ConstraintChange::DropUnique {
                    table: table.clone(),
                    name: "c_u".into(),
                }),
            ),
            (
                "a kept exclusion dropped",
                Change::Constraint(ConstraintChange::SetExclusion {
                    table: table.clone(),
                    name: "c_x".into(),
                    from: Some(Box::new(crate::ExclusionConstraint {
                        method: Default::default(),
                        keys: Vec::new(),
                        include: Vec::new(),
                        filter: None,
                        storage_parameters: Default::default(),
                        timing: crate::ExclusionTiming::Immediate,
                    })),
                    to: None,
                    rehearsal: None,
                }),
            ),
        ] {
            let mut plan = constrained(|_, _| {});
            plan.changes
                .changes
                .insert(0, crate::PlannedChange::new(change));
            cases.push((why.to_owned(), plan));
        }
        for (why, plan) in cases {
            assert_eq!(
                validate(&plan),
                Err("invalid attach rehearsal on app.c".to_owned()),
                "{why}"
            );
        }
    }

    fn table(name: &str) -> TableName {
        TableName::new("app", name)
    }

    fn names(list: &[&str]) -> BTreeSet<ObjectLocalName> {
        list.iter().map(|n| (*n).into()).collect()
    }

    fn owns(adopted: &[&str], beside: &[&str]) -> PartitionOwns {
        PartitionOwns {
            table: table("c"),
            own: vec![
                index("c_old"),
                index("c_a"),
                keyed("c_x", "EXCLUDE USING btree (n WITH =)"),
            ],
            adopted: names(adopted),
            beside: names(beside),
        }
    }

    fn add_index(name: &str, partitions: Option<Vec<PartitionOwns>>) -> Change {
        Change::Index(IndexChange::AddIndex {
            table: table("p"),
            name: name.into(),
            index: Box::new(crate::Index {
                columns: vec![crate::IndexColumn::column("n")],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            }),
            clustered: false,
            rehearsal: partitions.map(|partitions| Box::new(AdditionRehearsal { partitions })),
        })
    }

    /// An index added to `relation`, with no record.
    fn added_to(relation: &str, name: &str) -> Change {
        let mut change = add_index(name, None);
        if let Change::Index(IndexChange::AddIndex { table: of, .. }) = &mut change {
            *of = table(relation);
        }
        change
    }

    fn drop_of(relation: &str, name: &str) -> Change {
        Change::Index(IndexChange::DropIndex {
            table: table(relation),
            name: name.into(),
        })
    }

    /// A plan adding `p_n` over `app.c`, taking `c_old` and leaving `c_a`
    /// beside it.
    fn added() -> Vec<Change> {
        vec![add_index("p_n", Some(vec![owns(&["c_old"], &["c_a"])]))]
    }

    fn plan_of(changes: Vec<Change>) -> SavedPlan {
        let mut plan = recorded();
        plan.changes.changes = changes.into_iter().map(crate::PlannedChange::new).collect();
        plan
    }

    #[test]
    fn a_parents_record_that_agrees_with_its_plan_is_admitted_and_survives_the_file() {
        // A column added to the parent, and a drop of an own the record leaves
        // alone, change nothing it rests on.
        let column = Change::Column(ColumnChange::AddColumn {
            uid: crate::Uid::generate(crate::UidKind::Column),
            table: table("p"),
            name: "extra".into(),
            column: Box::new(crate::Column::new("int".parse().unwrap())),
        });
        let mut changes = added();
        changes.insert(0, column);
        changes.insert(0, drop_of("c", "c_x"));
        // A second addition leaving the same own beside it, over the same list.
        changes.push(add_index("p_m", Some(vec![owns(&[], &["c_a"])])));
        let plan = plan_of(changes);
        assert_eq!(validate(&plan), Ok(()));
        let written = serde_json::to_string(&plan).unwrap();
        let read: SavedPlan = serde_json::from_str(&written).unwrap();
        assert_eq!(read.changes, plan.changes);
        let bare = serde_json::to_string(&plan_of(vec![add_index("p_n", None)])).unwrap();
        assert!(!bare.contains("rehearsal"), "{bare}");
    }

    #[test]
    fn an_addition_changes_each_partition_it_takes_an_own_of_and_no_other() {
        let elsewhere = PartitionOwns {
            table: table("d"),
            ..owns(&[], &["c_a"])
        };
        let change = add_index("p_n", Some(vec![elsewhere, owns(&["c_old"], &[])]));
        let taken: Vec<_> = adopts(&change).map(|of| &of.table).collect();
        assert_eq!(taken, [&table("c")]);
        assert_eq!(adopts(&add_index("p_n", None)).count(), 0);
    }

    #[test]
    fn a_parents_record_that_does_not_agree_with_its_plan_is_refused() {
        let with = |partitions: Vec<PartitionOwns>| vec![add_index("p_n", Some(partitions))];
        let one = |edit: &dyn Fn(&mut PartitionOwns)| {
            let mut of = owns(&["c_old"], &["c_a"]);
            edit(&mut of);
            with(vec![of])
        };
        let and = |change: Change| {
            let mut changes = added();
            changes.insert(0, change);
            changes
        };
        let second = |of: PartitionOwns| {
            let mut changes = added();
            changes.push(add_index("p_m", Some(vec![of])));
            changes
        };
        let rename = Change::Column(ColumnChange::RenameColumn {
            uid: crate::Uid::generate(crate::UidKind::Column),
            table: table("other"),
            from: "a".into(),
            to: "b".into(),
            table_was: None,
        });
        let retype = Change::Column(ColumnChange::AlterColumnNullability {
            uid: crate::Uid::generate(crate::UidKind::Column),
            column: "app.p.n".parse().unwrap(),
            ty: "int".parse().unwrap(),
            to_nullable: true,
            collation: None,
            through_external_partitions: false,
        });
        let parameters = Change::Index(IndexChange::SetIndexStorageParameters {
            table: table("c"),
            target: crate::IndexPart::Index("c_x".into()),
            method: crate::IndexMethod::Btree,
            set: [("fillfactor".to_owned(), "70".to_owned())].into(),
            reset: Default::default(),
        });
        let cases = [
            ("no partition", with(Vec::new())),
            (
                "a partition twice",
                with(vec![owns(&["c_old"], &[]), owns(&[], &["c_a"])]),
            ),
            (
                "the parent as its own partition",
                one(&|of| of.table = table("p")),
            ),
            (
                "nothing decided",
                one(&|of| {
                    of.adopted.clear();
                    of.beside.clear();
                }),
            ),
            ("a name both ways", one(&|of| of.beside = names(&["c_old"]))),
            (
                "a name outside the list",
                one(&|of| of.beside = names(&["c_z"])),
            ),
            (
                "an own of another kind",
                one(&|of| of.beside = names(&["c_x"])),
            ),
            (
                "a name twice in the list",
                one(&|of| of.own.push(index("c_a"))),
            ),
            ("an own the plan drops", and(drop_of("c", "c_a"))),
            ("an adopted own the plan drops", and(drop_of("c", "c_old"))),
            ("an own the plan adds", and(added_to("c", "c_a"))),
            ("an own taken twice", second(owns(&["c_old"], &[]))),
            (
                "taken by one and beside another",
                second(owns(&[], &["c_old"])),
            ),
            (
                "beside one and taken by another",
                second(owns(&["c_a"], &[])),
            ),
            (
                "another list",
                second({
                    let mut of = owns(&[], &["c_a"]);
                    of.own.pop();
                    of
                }),
            ),
            ("a rename", and(rename)),
            ("a parent's column altered", and(retype)),
            ("an own's parameters changed", and(parameters)),
        ];
        for (why, changes) in cases {
            assert!(
                validate(&plan_of(changes)).is_err_and(|e| e.contains("parent rehearsal")),
                "{why}"
            );
        }
        for other in ["mssql", "sqlite"] {
            let mut elsewhere = plan_of(added());
            elsewhere.dialect = other.into();
            assert!(validate(&elsewhere).is_err(), "{other}");
        }
        let dropping = Change::Constraint(ConstraintChange::SetExclusion {
            table: table("p"),
            name: "p_x".into(),
            from: Some(Box::new(crate::ExclusionConstraint {
                method: Default::default(),
                keys: Vec::new(),
                include: Vec::new(),
                filter: None,
                storage_parameters: Default::default(),
                timing: crate::ExclusionTiming::Immediate,
            })),
            to: None,
            rehearsal: Some(Box::new(AdditionRehearsal {
                partitions: vec![owns(&[], &["c_x"])],
            })),
        });
        assert!(validate(&plan_of(vec![dropping])).is_err_and(|e| e.contains("adds nothing")));
    }

    /// What apply holds a partition to before one addition: the record less
    /// what earlier changes drop and earlier additions take, then what earlier
    /// changes add, by name.
    #[test]
    fn what_stands_before_an_addition_follows_the_earlier_changes_of_that_partition() {
        let record = owns(&[], &[]).own;
        let rebuilt = added_to("c", "c_new");
        let changes = ChangeSet {
            changes: [
                drop_of("c", "c_a"),
                drop_of("other", "c_x"),
                add_index("p_m", Some(vec![owns(&["c_old"], &[])])),
                rebuilt,
                add_index("p_n", Some(vec![owns(&[], &["c_x"])])),
                drop_of("c", "c_x"),
            ]
            .into_iter()
            .map(crate::PlannedChange::new)
            .collect(),
        };
        let (kept, added) = standing_own(&changes, 4, &table("c"), &record);
        assert_eq!(
            kept.iter().map(|i| i.name.raw()).collect::<Vec<_>>(),
            ["c_x"]
        );
        assert_eq!(added, [ObjectLocalName::from("c_new")]);
        assert_eq!(
            standing_own(&changes, 0, &table("c"), &record),
            (record.clone(), Vec::new())
        );

        // Whatever an added one prints, its place and its name are held.
        let mut found = held(&kept);
        found.extend(held(&[index("c_new")]));
        found[1].index.index = "CREATE INDEX something else".into();
        assert_eq!(difference_after(&kept, &added, &found), None);
        let says = |found: &[HeldIndex]| difference_after(&kept, &added, found).unwrap();
        assert!(says(&found[..1]).contains("no longer has the index `c_new`"));
        let mut other = found.clone();
        other[1].index.name = "c_y".into();
        assert!(says(&other).contains("`c_new`") || says(&other).contains("`c_y`"));
        let mut extra = found.clone();
        extra.extend(held(&[index("c_z")]));
        assert!(says(&extra).contains("has the index `c_z`"));
        found[1].valid = false;
        assert!(says(&found).contains("`c_new` is invalid"));
    }
}
