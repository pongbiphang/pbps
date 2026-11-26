//! What a resolver's rehearsal answered about the indexes an attach leaves to
//! the engine, and what apply holds the database to because of it (#1671,
//! DEC-1671.2).
//!
//! Which of a table's indexes `ATTACH PARTITION` takes as the clone of one of
//! its parent's depends on what their expressions bind to and on the order they
//! were made in. The model holds neither. A rehearsal asks an engine, and the
//! plan then rests on both facts staying as they were read: the lists here are
//! that record.

use crate::{
    Change, ChangeSet, ConstraintChange, IndexChange, ObjectLocalName, PartitionChange, SavedPlan,
    TableName, Text,
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

/// An index as a database holds it now. An invalid one is never taken by an
/// attach, and its definition does not show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldIndex {
    pub index: RelationIndex,
    pub valid: bool,
}

/// A relation as apply reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
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
    /// Each of the table's own indexes the plan leaves standing, and the
    /// parent's index the rehearsal's engine gave it to.
    pub kept: BTreeMap<ObjectLocalName, ObjectLocalName>,
}

/// Whether `change` takes the index `index` off `relation`: the forms a plan
/// removes an index in ahead of an attach.
fn drops(change: &Change, relation: &TableName, index: &RelationIndex) -> bool {
    if let Change::Index(IndexChange::DropIndex { table, name })
    | Change::Constraint(ConstraintChange::DropUnique { table, name })
    | Change::Constraint(ConstraintChange::SetExclusion {
        table,
        name,
        from: Some(_),
        to: None,
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
                || {
                    index
                        .constraint
                        .as_ref()
                        .is_some_and(|c| c.raw().starts_with("PRIMARY KEY"))
                },
                |name| *name == index.name,
            )
    } else {
        false
    }
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

/// Admission of what a saved plan records. The record is what apply compares the
/// database with and what a reviewer is told stays, so one that does not agree
/// with its own plan is refused.
pub fn validate(plan: &SavedPlan) -> Result<(), String> {
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
        let plain = |list: &'_ [RelationIndex], name: &ObjectLocalName| {
            list.iter()
                .find(|i| i.name == *name && i.constraint.is_none())
                .cloned()
        };
        let taken: BTreeSet<_> = rehearsal.kept.values().collect();
        let holds = plan.dialect == "postgres"
            && !rehearsal.kept.is_empty()
            && distinct(&rehearsal.parent)
            && distinct(&rehearsal.table)
            && taken.len() == rehearsal.kept.len()
            && rehearsal.kept.iter().all(|(own, theirs)| {
                plain(&rehearsal.parent, theirs).is_some()
                    && !shape.indexes.contains_key(own)
                    && plain(&rehearsal.table, own).is_some_and(|own| {
                        !plan
                            .changes
                            .changes
                            .iter()
                            .any(|p| drops(&p.change, table, &own))
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
        for (why, plan) in cases {
            assert_eq!(
                validate(&plan),
                Err("invalid attach rehearsal on app.c".to_owned()),
                "{why}"
            );
        }
    }
}
