//! What the plan says of each own the loose rule drops (DEC-1671.3).
use super::*;
use crate::identity::Context;
use crate::schema_diff::tests::Postgres;
use pbps_model::adoption::{AttachAnswer, AttachRehearsal, AttachedAnswer, Skipped};
use pbps_model::{Column, IndexColumn, Intent, PartitionBound, PartitionBy, PartitionOf};

fn plain(column: &str) -> Index {
    Index {
        columns: vec![IndexColumn::column(column)],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

/// An exclusion over the partition key, which a partitioned parent's must hold,
/// and `column`.
fn excluding(column: &str) -> pbps_model::ExclusionConstraint {
    let equal = |column: &str| pbps_model::ExclusionKey {
        key: IndexColumn::column(column),
        operator: pbps_model::OperatorRef::symbol("=").unwrap(),
    };
    pbps_model::ExclusionConstraint {
        method: pbps_model::IndexMethod::Btree,
        keys: vec![equal("id"), equal(column)],
        include: Vec::new(),
        filter: None,
        storage_parameters: Default::default(),
        timing: pbps_model::ExclusionTiming::Immediate,
    }
}

fn key() -> pbps_model::UniqueConstraint {
    pbps_model::UniqueConstraint {
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

fn columns() -> Table {
    Table {
        columns: ["id", "m", "n"]
            .into_iter()
            .map(|name| (name.into(), Column::new("int".parse().unwrap()).not_null()))
            .collect(),
        ..Default::default()
    }
}

fn name(table: &str) -> TableName {
    TableName::new("app", table)
}

fn under_p() -> Table {
    Table {
        partition_of: Some(PartitionOf {
            parent: name("p"),
            bound: PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    }
}

fn context() -> Context {
    Context {
        operator: "leon".into(),
        today: "2026-11-28".into(),
    }
}

/// `app.p` partitioned and `app.other` beside it, with what a test puts in
/// `app.c`.
struct World {
    base: Schema,
    declared: Schema,
    ids: IdsFile,
}

impl World {
    fn of(base: Schema, declared: Schema) -> Self {
        let ids = crate::resolve(&base, &IdsFile::default(), &[], &context())
            .unwrap()
            .ids;
        Self {
            base,
            declared,
            ids,
        }
    }

    fn partitioned() -> Schema {
        let mut parent = columns();
        parent.partition_by = Some(PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        });
        let mut schema = Schema::default();
        schema.tables.insert(name("p"), parent);
        schema.tables.insert(name("other"), columns());
        schema
    }

    /// `app.c` standing under `app.p`, holding `c_a` and `c_m`, which the
    /// declarations keep, and `c_old`, which they do not.
    fn standing() -> Self {
        let mut base = Self::partitioned();
        let mut partition = under_p();
        partition.indexes.insert("c_a".into(), plain("n"));
        partition.indexes.insert("c_m".into(), plain("m"));
        base.tables.insert(name("c"), partition);
        let mut declared = base.clone();
        base.tables
            .get_mut(&name("c"))
            .unwrap()
            .indexes
            .insert("c_old".into(), plain("n"));
        declared
            .tables
            .get_mut(&name("p"))
            .unwrap()
            .indexes
            .insert("p_n".into(), plain("n"));
        Self::of(base, declared)
    }

    /// `app.c` ordinary, declared a partition of `app.p`, whose `p_n` and
    /// unique `p_u` stand: `c_a` kept as it is, `c_r` declared otherwise,
    /// `c_old` and `c_m` not declared, and the unique constraint `c_k`.
    fn attaching() -> Self {
        let mut base = Self::partitioned();
        let parent = base.tables.get_mut(&name("p")).unwrap();
        parent.indexes.insert("p_n".into(), plain("n"));
        parent.indexes.insert(
            "p_u".into(),
            Index {
                unique: true,
                ..plain("id")
            },
        );
        parent.unique.insert("p_k".into(), key());
        let mut ordinary = columns();
        for index in ["c_a", "c_r", "c_old"] {
            ordinary.indexes.insert(index.into(), plain("n"));
        }
        ordinary.indexes.insert("c_m".into(), plain("m"));
        ordinary.unique.insert("c_k".into(), key());
        base.tables.insert(name("c"), ordinary);
        let mut declared = base.clone();
        let mut partition = under_p();
        partition.indexes.insert("c_a".into(), plain("n"));
        partition.indexes.insert("c_r".into(), plain("m"));
        declared.tables.insert(name("c"), partition);
        Self::of(base, declared)
    }

    fn planned(&self, intents: &[Intent], answers: Vec<AttachAnswer>) -> Vec<PlannedChange> {
        let baseline = Side {
            schema: &self.base,
            ids: &self.ids,
        };
        let declared = crate::resolve_with_annotations_against(
            &self.declared,
            &self.ids,
            intents,
            0,
            baseline,
            &context(),
        )
        .unwrap()
        .ids;
        let hints = Hints {
            attach_rehearsals: answers,
            ..Default::default()
        };
        let declared = Side {
            schema: &self.declared,
            ids: &declared,
        };
        crate::diff(baseline, declared, &Postgres, &hints)
            .expect("planned")
            .changes()
            .changes
            .clone()
    }
}

/// Each drop and add of `app.c`'s indexes, keys and exclusions as `-name` or
/// `+name`, with what the plan says of it.
fn said(planned: &[PlannedChange]) -> BTreeMap<String, Vec<Remark>> {
    let of = name("c");
    planned
        .iter()
        .filter_map(|p| {
            let (sign, table, part) = match &p.change {
                Change::Index(IndexChange::DropIndex { table, name })
                | Change::Constraint(ConstraintChange::DropUnique { table, name }) => {
                    ('-', table, name.raw().to_owned())
                }
                Change::Index(IndexChange::AddIndex { table, name, .. }) => {
                    ('+', table, name.raw().to_owned())
                }
                Change::Constraint(ConstraintChange::SetPrimaryKey {
                    table, to: None, ..
                }) => ('-', table, "(key)".to_owned()),
                Change::Constraint(ConstraintChange::SetExclusion {
                    table, name, to, ..
                }) => (
                    if to.is_some() { '+' } else { '-' },
                    table,
                    name.raw().to_owned(),
                ),
                _ => return None,
            };
            (*table == of).then(|| (format!("{sign}{part}"), p.remarks.clone()))
        })
        .collect()
}

fn loose(by: &str, exclusions: bool, new: bool, again: bool, hint: bool) -> Vec<Remark> {
    vec![Remark::Loose {
        by: vec![by.into()],
        exclusions,
        new,
        again,
        resolver_could_keep: hint,
    }]
}

#[test]
fn a_parents_new_index_marks_each_own_it_rebuilds_and_nothing_else() {
    let planned = World::standing().planned(&[], Vec::new());
    let found = said(&planned);
    let rebuilt = loose("p_n", false, true, true, true);
    assert_eq!(found["-c_a"], rebuilt, "{found:?}");
    assert_eq!(found["+c_a"], rebuilt, "{found:?}");
    // The declarations' own removal is not the parent's to explain.
    assert_eq!(found["-c_old"], Vec::new(), "{found:?}");
    // No key of the parent's reads this one, and it is not touched.
    assert!(!found.contains_key("-c_m"), "{found:?}");
    assert!(
        planned
            .iter()
            .filter(|p| p.change.table() == Some(&name("p")))
            .all(|p| p.remarks.is_empty())
    );
}

#[test]
fn a_parents_new_exclusion_marks_its_rebuild_as_an_exclusions() {
    let mut base = World::partitioned();
    let mut partition = under_p();
    partition.exclude.insert("c_x".into(), excluding("n"));
    base.tables.insert(name("c"), partition);
    let mut declared = base.clone();
    declared
        .tables
        .get_mut(&name("p"))
        .unwrap()
        .exclude
        .insert("p_x".into(), excluding("n"));
    let found = said(&World::of(base, declared).planned(&[], Vec::new()));
    let rebuilt = loose("p_x", true, true, true, true);
    assert_eq!(found["-c_x"], rebuilt, "{found:?}");
    assert_eq!(found["+c_x"], rebuilt, "{found:?}");
}

/// A declared name is never an attach's copy, so no resolver is offered for it;
/// one dropped for good could be kept by one. A declaration that differs makes
/// the drop its own.
#[test]
fn an_attach_marks_each_loose_drop_and_offers_a_resolver_only_where_one_could_keep_it() {
    let found = said(&World::attaching().planned(&[], Vec::new()));
    let rebuilt = loose("p_n", false, false, true, false);
    assert_eq!(found["-c_a"], rebuilt, "{found:?}");
    assert_eq!(found["+c_a"], rebuilt, "{found:?}");
    assert_eq!(
        found["-c_old"],
        loose("p_n", false, false, false, true),
        "{found:?}"
    );
    assert_eq!(
        found["-c_k"],
        loose("p_u", false, false, false, true),
        "{found:?}"
    );
    for own in ["-c_r", "+c_r", "-c_m"] {
        assert_eq!(found[own], Vec::new(), "{own}: {found:?}");
    }
}

#[test]
fn an_attach_dropping_a_retired_exclusion_marks_it_as_an_exclusions() {
    let mut world = World::attaching();
    for (table, exclusion) in [("p", "p_x"), ("c", "c_x")] {
        world
            .base
            .tables
            .get_mut(&name(table))
            .unwrap()
            .exclude
            .insert(exclusion.into(), excluding("m"));
    }
    world
        .declared
        .tables
        .get_mut(&name("p"))
        .unwrap()
        .exclude
        .insert("p_x".into(), excluding("m"));
    let world = World::of(world.base, world.declared);
    let retired = Intent::DropExclusion {
        table: name("c"),
        name: "c_x".into(),
        reason: "the parent's holds it".parse().unwrap(),
    };
    let found = said(&world.planned(&[retired], Vec::new()));
    assert_eq!(
        found["-c_x"],
        loose("p_x", true, false, false, true),
        "{found:?}"
    );
}

/// Every drop of the attach still says why, and none offers a resolver: one was
/// asked, or none would be.
#[test]
fn no_resolver_is_offered_where_one_was_asked_or_would_not_be() {
    let offered = |found: &BTreeMap<String, Vec<Remark>>| {
        let remarks: Vec<_> = found.values().flatten().collect();
        assert!(!remarks.is_empty(), "{found:?}");
        remarks.iter().any(|remark| {
            matches!(
                remark,
                Remark::Loose {
                    resolver_could_keep: true,
                    ..
                }
            )
        })
    };
    let world = World::attaching();
    assert!(offered(&said(&world.planned(&[], Vec::new()))));
    // An answer that keeps nothing: the question was put.
    let asked = AttachAnswer::Attach(AttachedAnswer {
        uid: world.ids.table_uid(&name("c")).unwrap().clone(),
        parent: name("p"),
        rehearsal: AttachRehearsal {
            parent: Vec::new(),
            table: Vec::new(),
            kept: Default::default(),
        },
    });
    assert!(!offered(&said(&world.planned(&[], vec![asked]))));

    // A column renamed on another table.
    let mut renamed = World::attaching();
    let other = renamed.declared.tables.get_mut(&name("other")).unwrap();
    let column = other.columns.shift_remove("m").unwrap();
    other.columns.insert("moved".into(), column);
    let rename = Intent::RenameColumn {
        table: name("other"),
        from: "m".into(),
        to: "moved".into(),
    };
    assert!(!offered(&said(&renamed.planned(&[rename], Vec::new()))));

    // A parent the same plan reshapes. This test dialect refuses such a plan
    // whole, as PostgreSQL does not, so the pass is asked directly.
    let directly = |declared: &Schema| {
        let mut planned = world.planned(&[], Vec::new());
        for p in &mut planned {
            p.remarks.clear();
        }
        let drop = planned
            .iter()
            .find(|p| {
                matches!(&p.change, Change::Index(IndexChange::DropIndex { name, .. })
                    if name.raw() == "c_old")
            })
            .map(|p| p.change.clone())
            .expect("the own's drop");
        let own = Loose::attached(
            &drop,
            &name("c"),
            Some(&"c_old".into()),
            &["p_n".into()],
            false,
            None,
        )
        .expect("a loose drop");
        let before = Side {
            schema: &world.base,
            ids: &world.ids,
        };
        say_why(before, declared, &Hints::default(), &[own], &mut planned);
        offered(&said(&planned))
    };
    assert!(directly(&world.declared));
    let mut reshaped = world.declared.clone();
    let parent = reshaped.tables.get_mut(&name("p")).unwrap();
    parent.columns.get_mut("m").unwrap().nullable = true;
    assert!(!directly(&reshaped));

    // A key no rehearsal could leave out for want of a name.
    let mut unnamed = World::attaching();
    let primary = pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    };
    for table in ["p", "c"] {
        unnamed
            .base
            .tables
            .get_mut(&name(table))
            .unwrap()
            .primary_key = Some(primary.clone());
    }
    unnamed
        .declared
        .tables
        .get_mut(&name("p"))
        .unwrap()
        .primary_key = Some(primary);
    let found = said(&World::of(unnamed.base, unnamed.declared).planned(&[], Vec::new()));
    assert_eq!(
        found["-(key)"],
        loose("p_u", false, false, false, false),
        "{found:?}"
    );
    assert!(!offered(&found));
}

/// A parent's new index may leave a declared own standing, so a resolver is
/// offered for it until one has said anything of the partition, and not where
/// the plan is one no rehearsal answers.
#[test]
fn a_resolver_is_offered_under_a_parents_new_index_only_where_it_would_be_asked() {
    let offered = |world: &World, answers| {
        let found = said(&world.planned(&[], answers));
        assert!(
            found.contains_key("-c_a"),
            "the own is dropped either way: {found:?}"
        );
        found["-c_a"] == loose("p_n", false, true, true, true)
    };
    let world = World::standing();
    assert!(offered(&world, Vec::new()));
    let partition = world.ids.table_uid(&name("c")).unwrap().clone();
    let skipped = |table| {
        vec![AttachAnswer::Skipped {
            table,
            why: Skipped::Scratch,
        }]
    };
    assert!(!offered(&world, skipped(partition)));
    // What is said of another table is not said of this one.
    let other = world.ids.table_uid(&name("other")).unwrap().clone();
    assert!(offered(&world, skipped(other)));

    let mut reshaped = World::standing();
    let parent = reshaped.declared.tables.get_mut(&name("p")).unwrap();
    parent.columns.get_mut("m").unwrap().nullable = true;
    assert!(!offered(&reshaped, Vec::new()));

    // A storage parameter the plan changes on the own it would leave standing,
    // which only an exclusion's can be on a partition.
    let mut base = World::partitioned();
    let mut partition = under_p();
    partition.exclude.insert("c_x".into(), excluding("n"));
    base.tables.insert(name("c"), partition);
    let mut declared = base.clone();
    declared
        .tables
        .get_mut(&name("p"))
        .unwrap()
        .exclude
        .insert("p_x".into(), excluding("n"));
    let hinted = |declared: Schema| {
        let found = said(&World::of(base.clone(), declared).planned(&[], Vec::new()));
        found["-c_x"] == loose("p_x", true, true, true, true)
    };
    assert!(hinted(declared.clone()));
    declared
        .tables
        .get_mut(&name("c"))
        .unwrap()
        .exclude
        .get_mut("c_x")
        .unwrap()
        .storage_parameters
        .insert("fillfactor".into(), "70".into());
    assert!(!hinted(declared));
}
