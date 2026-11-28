//! A parent's new index or exclusion over a standing partition: what is asked,
//! what an answer changes, and what the plan says (DEC-1671.3).
use super::*;
use crate::identity::Context;
use crate::schema_diff::tests::Postgres;
use pbps_model::adoption::{AdditionAnswer, RelationIndex};
use pbps_model::{
    Column, ForeignKey, IndexColumn, Intent, PartitionBound, PartitionBy, PartitionOf,
};

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

fn name(table: &str) -> TableName {
    TableName::new("app", table)
}

/// The retirement of [`World::excluded`]'s `c_gone`.
fn retired() -> [Intent; 1] {
    [Intent::DropExclusion {
        table: name("c"),
        name: "c_gone".into(),
        reason: "the parent's holds it".parse().unwrap(),
    }]
}

fn names(list: &[&str]) -> BTreeSet<ObjectLocalName> {
    list.iter().map(|n| (*n).into()).collect()
}

fn read(index: &str) -> RelationIndex {
    RelationIndex {
        name: index.into(),
        index: format!("CREATE INDEX {index}").into(),
        constraint: None,
    }
}

fn context() -> Context {
    Context {
        operator: "leon".into(),
        today: "2026-11-28".into(),
    }
}

/// `app.p` partitioned, `app.c` under it holding `c_a` (declared) and `c_old`
/// (not), both over `n`, and the parent's new `p_n` over `n`.
struct World {
    base: Schema,
    declared: Schema,
    ids: IdsFile,
}

impl World {
    fn new() -> Self {
        let columns = || -> Table {
            Table {
                columns: ["id", "m", "n"]
                    .into_iter()
                    .map(|name| (name.into(), Column::new("int".parse().unwrap()).not_null()))
                    .collect(),
                ..Default::default()
            }
        };
        let mut parent = columns();
        parent.partition_by = Some(PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        });
        let mut partition = Table {
            partition_of: Some(PartitionOf {
                parent: name("p"),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        partition.indexes.insert("c_a".into(), plain("n"));
        partition.indexes.insert("c_old".into(), plain("n"));
        let mut base = Schema::default();
        base.tables.insert(name("p"), parent);
        base.tables.insert(name("c"), partition);
        base.tables.insert(name("other"), columns());
        let mut declared = base.clone();
        declared.tables.get_mut(&name("p")).unwrap().indexes = [("p_n".into(), plain("n"))].into();
        declared
            .tables
            .get_mut(&name("c"))
            .unwrap()
            .indexes
            .remove("c_old");
        let ids = crate::resolve(&base, &IdsFile::default(), &[], &context())
            .unwrap()
            .ids;
        Self {
            base,
            declared,
            ids,
        }
    }

    /// As [`World::new`] with exclusions for indexes: `app.c` holds `c_x`
    /// (declared) and `c_gone` (not) over `n`, and `c_far` over `m`, and the
    /// parent's new one is `p_x` over `n`.
    fn excluded() -> Self {
        let mut world = Self::new();
        for schema in [&mut world.base, &mut world.declared] {
            let partition = schema.tables.get_mut(&name("c")).unwrap();
            partition.indexes.clear();
            partition.exclude.insert("c_x".into(), excluding("n"));
            partition.exclude.insert("c_far".into(), excluding("m"));
        }
        let partition = world.base.tables.get_mut(&name("c")).unwrap();
        partition.exclude.insert("c_gone".into(), excluding("n"));
        world.parent().indexes.clear();
        world.parent().exclude.insert("p_x".into(), excluding("n"));
        world.ids = crate::resolve(&world.base, &IdsFile::default(), &[], &context())
            .unwrap()
            .ids;
        world
    }

    /// [`World::new`] with each of `app.c`'s owns, and the parent's new index,
    /// unique over the partition key.
    fn unique() -> Self {
        let mut world = Self::new();
        let over_id = || Index {
            unique: true,
            ..plain("id")
        };
        for schema in [&mut world.base, &mut world.declared] {
            for index in schema
                .tables
                .get_mut(&name("c"))
                .unwrap()
                .indexes
                .values_mut()
            {
                *index = over_id();
            }
        }
        world.parent().indexes.insert("p_n".into(), over_id());
        world
    }

    fn parent(&mut self) -> &mut Table {
        self.declared.tables.get_mut(&name("p")).unwrap()
    }

    fn sides<T>(&self, intents: &[Intent], with: impl FnOnce(Side<'_>, Side<'_>) -> T) -> T {
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
        with(
            baseline,
            Side {
                schema: &self.declared,
                ids: &declared,
            },
        )
    }

    fn questions_given(&self, intents: &[Intent]) -> Questions {
        self.sides(intents, |base, declared| {
            questions(base, declared, &Postgres, &Hints::default(), base.schema)
        })
    }

    fn questions(&self) -> Questions {
        self.questions_given(&[])
    }

    fn question(&self) -> AdditionQuestion {
        let found = self.questions();
        let [question] = &found.additions[..] else {
            panic!("one question: {found:?}")
        };
        question.clone()
    }

    fn planned(&self, intents: &[Intent], answers: Vec<AttachAnswer>) -> Vec<PlannedChange> {
        self.sides(intents, |base, declared| {
            let hints = Hints {
                attach_rehearsals: answers,
                ..Default::default()
            };
            crate::diff(base, declared, &Postgres, &hints)
                .expect("planned")
                .changes()
                .changes
                .clone()
        })
    }

    fn plan(&self, answers: Vec<AttachAnswer>) -> Vec<Change> {
        self.planned(&[], answers)
            .into_iter()
            .map(|p| p.change)
            .collect()
    }
}

/// What the engine would answer for [`World::new`]: `c_old`, made first, is the
/// copy, and `c_a` stands beside it.
fn answer(question: &AdditionQuestion) -> AdditionAnswer {
    AdditionAnswer {
        parent: question.parent_uid.clone(),
        uid: question.uid.clone(),
        additions: question.additions().map(|(n, _)| n.clone()).collect(),
        own: vec![read("c_old"), read("c_a")],
        adopted: [("c_old".into(), "p_n".into())].into(),
        beside: names(&["c_a"]),
    }
}

fn given(answer: AdditionAnswer) -> Vec<AttachAnswer> {
    vec![AttachAnswer::Addition(Box::new(answer))]
}

/// Every drop and add on `app.c`, as `-name` and `+name`.
fn on_partition(changes: &[Change]) -> Vec<String> {
    let mut found: Vec<_> = changes
        .iter()
        .filter_map(|change| {
            if let Some((name, _)) = dropped_from(change, &self::name("c")) {
                Some(format!("-{name}", name = name.shown()))
            } else {
                pbps_model::adoption::adds(change, &self::name("c"))
                    .map(|name| format!("+{name}", name = name.shown()))
            }
        })
        .collect();
    found.sort_unstable();
    found
}

fn recorded(changes: &[Change]) -> Vec<&AdditionRehearsal> {
    changes
        .iter()
        .filter_map(|change| pbps_model::adoption::record(change).map(|(_, r)| r))
        .collect()
}

#[test]
fn a_parents_new_index_asks_about_each_partitions_declared_and_retiring_own() {
    let world = World::new();
    let question = world.question();
    assert_eq!(question.table, name("c"));
    assert_eq!(question.parent, name("p"));
    assert_eq!(question.declared, names(&["c_a"]));
    assert_eq!(question.retiring, names(&["c_old"]));
    assert!(question.dropped.is_empty());
    assert_eq!(
        question.additions().collect::<Vec<_>>(),
        [(&"p_n".into(), IndexKind::Plain)]
    );
    assert!(world.questions().skipped.is_empty());
    world.sides(&[], |base, declared| {
        assert_eq!(
            candidates(base, declared),
            [name("p"), name("c")].into_iter().collect()
        );
    });

    // A parent whose indexes and exclusions do not differ reads nothing.
    let mut unchanged = World::new();
    unchanged.parent().indexes.clear();
    unchanged.sides(&[], |base, declared| {
        assert!(candidates(base, declared).is_empty());
    });
    assert!(unchanged.questions().is_empty());

    // Nothing that might be taken, nothing to ask or to say.
    let mut apart = World::new();
    apart.parent().indexes = [("p_m".into(), plain("m"))].into();
    assert!(apart.questions().is_empty(), "{:?}", apart.questions());
}

#[test]
fn without_a_resolver_each_declared_own_is_rebuilt_and_says_a_resolver_can_keep_it() {
    let world = World::new();
    let planned = world.planned(&[], Vec::new());
    let changes: Vec<_> = planned.iter().map(|p| p.change.clone()).collect();
    assert_eq!(on_partition(&changes), ["+c_a", "-c_a", "-c_old"]);
    assert!(recorded(&changes).is_empty());
    let said = |own: &str| {
        planned
            .iter()
            .filter(|p| {
                dropped_from(&p.change, &name("c")).is_some_and(|(n, _)| n.raw() == own)
                    || pbps_model::adoption::adds(&p.change, &name("c"))
                        .is_some_and(|n| n.raw() == own)
            })
            .map(|p| p.remarks.clone())
            .collect::<Vec<_>>()
    };
    let loose = Remark::Loose {
        by: vec!["p_n".into()],
        exclusions: false,
        new: true,
        again: true,
        resolver_could_keep: true,
    };
    assert_eq!(said("c_a"), [vec![loose.clone()], vec![loose]]);
    // The declarations' own removal is not the parent's to explain.
    assert_eq!(said("c_old"), [Vec::new()]);
    assert!(
        planned
            .iter()
            .filter(|p| p.change.table() == Some(&name("p")))
            .all(|p| p.remarks.is_empty())
    );
}

#[test]
fn an_answer_leaves_the_declared_own_beside_and_the_retiring_one_to_the_addition() {
    let world = World::new();
    let question = world.question();
    let without = world.plan(Vec::new());
    let planned = world.planned(&[], given(answer(&question)));
    let with: Vec<_> = planned.iter().map(|p| p.change.clone()).collect();
    assert_eq!(on_partition(&with), Vec::<String>::new());
    assert_eq!(with.len() + 3, without.len());
    let [record] = &recorded(&with)[..] else {
        panic!("one record: {with:?}")
    };
    assert_eq!(
        record.partitions,
        [PartitionOwns {
            table: name("c"),
            own: vec![read("c_old"), read("c_a")],
            adopted: names(&["c_old"]),
            beside: names(&["c_a"]),
        }]
    );
    assert!(planned.iter().all(|p| p.remarks.is_empty()));

    // An answer that takes the declared own too leaves it rebuilt, and says no
    // resolver is wanted for it.
    let mut one = answer(&question);
    one.beside.clear();
    let planned = world.planned(&[], given(one));
    let changes: Vec<_> = planned.iter().map(|p| p.change.clone()).collect();
    assert_eq!(on_partition(&changes), ["+c_a", "-c_a"]);
    assert!(
        planned
            .iter()
            .flat_map(|p| &p.remarks)
            .all(|remark| matches!(
                remark,
                Remark::Loose {
                    resolver_could_keep: false,
                    ..
                }
            ))
    );
}

#[test]
fn an_answer_that_claims_what_was_not_asked_plans_as_without_a_resolver() {
    let world = World::new();
    let question = world.question();
    let without = world.plan(Vec::new());
    let good = answer(&question);
    assert_ne!(world.plan(given(good.clone())), without);
    let wrong = |edit: &dyn Fn(&mut AdditionAnswer)| {
        let mut answer = good.clone();
        edit(&mut answer);
        given(answer)
    };
    let cases: Vec<(&str, Vec<AttachAnswer>)> = vec![
        (
            "another parent",
            wrong(&|a| a.parent = "t_zzzzzz".parse().unwrap()),
        ),
        (
            "another partition",
            wrong(&|a| a.uid = "t_zzzzzz".parse().unwrap()),
        ),
        (
            "other additions",
            wrong(&|a| a.additions = vec!["p_other".into()]),
        ),
        ("an own the base lacks", wrong(&|a| a.own.push(read("c_x")))),
        (
            "an own missing",
            wrong(&|a| a.own.retain(|i| i.name.raw() != "c_a")),
        ),
        ("a name twice", wrong(&|a| a.own.push(read("c_a")))),
        (
            "an own of another kind",
            wrong(&|a| a.own[1].constraint = Some("EXCLUDE USING btree (n WITH =)".into())),
        ),
        (
            "a declared own taken",
            wrong(&|a| {
                a.beside.clear();
                a.adopted.insert("c_a".into(), "p_n".into());
                a.adopted.remove(&ObjectLocalName::from("c_old"));
            }),
        ),
        (
            "an undeclared own left beside",
            wrong(&|a| {
                a.adopted.clear();
                a.beside.insert("c_old".into());
            }),
        ),
        (
            "a taker that is no addition",
            wrong(&|a| a.adopted = [("c_old".into(), "p_x".into())].into()),
        ),
        (
            "two answers",
            [given(good.clone()), given(good.clone())].concat(),
        ),
    ];
    for (why, answers) in cases {
        assert_eq!(world.plan(answers), without, "{why}");
    }
}

/// A column the plan adds to the parent is in the scratch's shells; any other
/// change to one of its columns, or to what a definition is read through, is
/// not, and the report says which.
#[test]
fn only_an_added_column_leaves_the_parents_partitions_askable() {
    let mut added = World::new();
    added
        .parent()
        .columns
        .insert("extra".into(), Column::new("int".parse().unwrap()));
    added
        .parent()
        .indexes
        .insert("p_extra".into(), plain("extra"));
    let question = added.question();
    assert!(matches!(
        question.changes[0],
        Change::Column(ColumnChange::AddColumn { .. })
    ));
    // The new column's index takes nothing of the partition's, and is not run.
    assert_eq!(
        question
            .additions()
            .map(|(n, _)| n.raw())
            .collect::<Vec<_>>(),
        ["p_n"]
    );
    let with = added.plan(given(answer(&question)));
    assert_eq!(on_partition(&with), Vec::<String>::new());
    assert_eq!(recorded(&with).len(), 1);

    let mut retyped = World::new();
    retyped.parent().columns.get_mut("m").unwrap().ty = "bigint".parse().unwrap();
    let mut nullable = World::new();
    nullable.parent().columns.get_mut("m").unwrap().nullable = true;
    let mut dropped = World::new();
    dropped.parent().columns.shift_remove("m");
    let drop = [Intent::DropColumn {
        column: "app.p.m".parse().unwrap(),
        reason: "unused".parse().unwrap(),
    }];
    let mut renamed = World::new();
    let column = renamed.parent().columns.shift_remove("m").unwrap();
    renamed.parent().columns.insert("moved".into(), column);
    let rename = [Intent::RenameColumn {
        table: name("p"),
        from: "m".into(),
        to: "moved".into(),
    }];
    // A column renamed on a table the partitions do not touch.
    let mut elsewhere = World::new();
    let other = elsewhere.declared.tables.get_mut(&name("other")).unwrap();
    let column = other.columns.shift_remove("m").unwrap();
    other.columns.insert("moved".into(), column);
    let rename_elsewhere = [Intent::RenameColumn {
        table: name("other"),
        from: "m".into(),
        to: "moved".into(),
    }];
    let refilled = |own: &str| {
        let mut world = World::excluded();
        world
            .declared
            .tables
            .get_mut(&name("c"))
            .unwrap()
            .exclude
            .get_mut(own)
            .unwrap()
            .storage_parameters
            .insert("fillfactor".into(), "70".into());
        world
    };
    let retired = retired();
    assert!(
        World::excluded()
            .questions_given(&retired)
            .skipped
            .is_empty()
    );
    let cases: [(World, &[Intent], Skipped); 7] = [
        (retyped, &[], Skipped::ParentColumns),
        (nullable, &[], Skipped::ParentColumns),
        (dropped, &drop, Skipped::ParentColumns),
        (renamed, &rename, Skipped::ParentColumns),
        (elsewhere, &rename_elsewhere, Skipped::Unsettles),
        (refilled("c_x"), &retired, Skipped::OwnParameters),
        (refilled("c_far"), &retired, Skipped::OwnParameters),
    ];
    for (world, intents, why) in cases {
        let found = world.questions_given(intents);
        assert!(found.additions.is_empty(), "{why:?}: {found:?}");
        let [(uid, said)] = &found.skipped[..] else {
            panic!("{why:?}: {found:?}")
        };
        assert_eq!(*said, why);
        // An answer made without the change is not used, and the reason is on the
        // parent's addition.
        let stale = [AttachAnswer::Skipped {
            table: uid.clone(),
            why: *said,
        }];
        let planned = world.planned(intents, stale.to_vec());
        let said: Vec<_> = planned
            .iter()
            .flat_map(|p| p.remarks.iter().map(move |r| (p, r)))
            .filter(|(_, r)| matches!(r, Remark::NotRehearsed { .. }))
            .collect();
        let [(on, remark)] = &said[..] else {
            panic!("{why:?}: {said:?}")
        };
        assert_eq!(on.change.table(), Some(&name("p")), "{why:?}");
        assert_eq!(
            **remark,
            Remark::NotRehearsed {
                partitions: vec![name("c")],
                why,
            }
        );
    }
}

#[test]
fn a_referenced_or_claimed_retiring_own_is_dropped_whatever_an_engine_says() {
    let key = || ForeignKey {
        columns: vec!["id".into()],
        references_table: name("c"),
        references_columns: vec!["id".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    };
    let retiring = |world: &World| {
        let found = world.questions();
        found
            .additions
            .iter()
            .flat_map(|question| question.retiring.clone())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(retiring(&World::unique()), names(&["c_old"]));

    // A key the base holds on the retiring own, dropped by the same plan.
    let mut held = World::unique();
    let other = held.base.tables.get_mut(&name("other")).unwrap();
    other.foreign_keys.insert("other_fk".into(), key());
    let question = held.question();
    assert_eq!(question.declared, names(&["c_a"]));
    assert_eq!(question.retiring, names(&[]));
    assert_eq!(question.dropped, names(&["c_old"]));

    // One the declarations add, which the engine could bind to the retiring own
    // while it stands.
    let mut added = World::unique();
    for schema in [&mut added.base, &mut added.declared] {
        schema
            .tables
            .get_mut(&name("c"))
            .unwrap()
            .indexes
            .remove("c_a");
    }
    assert_eq!(retiring(&added), names(&["c_old"]));
    let other = added.declared.tables.get_mut(&name("other")).unwrap();
    other.foreign_keys.insert("other_fk".into(), key());
    assert_eq!(retiring(&added), names(&[]));

    // A name the declarations give to something else.
    let mut claimed = World::new();
    claimed
        .declared
        .tables
        .get_mut(&name("other"))
        .unwrap()
        .indexes
        .insert("c_old".into(), plain("m"));
    let question = claimed.question();
    assert_eq!(question.retiring, names(&[]));
    assert_eq!(question.dropped, names(&["c_old"]));
}

#[test]
fn a_parents_new_exclusion_may_leave_a_declared_own_beside_and_take_a_retired_one() {
    let world = World::excluded();
    let retired = retired();
    let found = world.questions_given(&retired);
    let [question] = &found.additions[..] else {
        panic!("one question: {found:?}")
    };
    assert_eq!(question.declared, names(&["c_x"]));
    assert_eq!(question.retiring, names(&["c_gone"]));
    let exclusion = |n| RelationIndex {
        constraint: Some("EXCLUDE USING btree (id WITH =)".into()),
        ..read(n)
    };
    let answered = AdditionAnswer {
        parent: question.parent_uid.clone(),
        uid: question.uid.clone(),
        additions: vec!["p_x".into()],
        own: vec![exclusion("c_far"), exclusion("c_gone"), exclusion("c_x")],
        adopted: [("c_gone".into(), "p_x".into())].into(),
        beside: names(&["c_x"]),
    };
    let planned = world.planned(&retired, given(answered));
    let changes: Vec<_> = planned.iter().map(|p| p.change.clone()).collect();
    assert_eq!(on_partition(&changes), Vec::<String>::new());
    let [record] = &recorded(&changes)[..] else {
        panic!("one record: {changes:?}")
    };
    assert_eq!(record.partitions[0].adopted, names(&["c_gone"]));
    assert_eq!(record.partitions[0].beside, names(&["c_x"]));
    // Without an answer the declared own is rebuilt and says so.
    let planned = world.planned(&retired, Vec::new());
    let changes: Vec<_> = planned.iter().map(|p| p.change.clone()).collect();
    assert_eq!(on_partition(&changes), ["+c_x", "-c_gone", "-c_x"]);
    assert!(planned.iter().flat_map(|p| &p.remarks).any(|r| matches!(
        r,
        Remark::Loose {
            exclusions: true,
            new: true,
            again: true,
            ..
        }
    )));
}

/// Each of two additions records what it takes and what stands beside it, over
/// one list of the partition's, and neither takes two.
#[test]
fn each_addition_records_what_it_takes_and_what_stands_beside_it() {
    let mut world = World::new();
    for schema in [&mut world.base, &mut world.declared] {
        let partition = schema.tables.get_mut(&name("c")).unwrap();
        partition.indexes.insert("c_ma".into(), plain("m"));
    }
    let partition = world.base.tables.get_mut(&name("c")).unwrap();
    partition.indexes.insert("c_mo".into(), plain("m"));
    world.parent().indexes.insert("p_m".into(), plain("m"));
    let question = world.question();
    assert_eq!(question.declared, names(&["c_a", "c_ma"]));
    assert_eq!(question.retiring, names(&["c_mo", "c_old"]));
    let own = vec![read("c_old"), read("c_a"), read("c_mo"), read("c_ma")];
    let answered = AdditionAnswer {
        own: own.clone(),
        adopted: [
            ("c_old".into(), "p_n".into()),
            ("c_mo".into(), "p_m".into()),
        ]
        .into(),
        beside: names(&["c_a", "c_ma"]),
        ..answer(&question)
    };
    let with = world.plan(given(answered.clone()));
    assert_eq!(on_partition(&with), Vec::<String>::new());
    let found: BTreeMap<_, _> = with
        .iter()
        .filter_map(|change| {
            let (_, record) = pbps_model::adoption::record(change)?;
            let (_, addition) = pbps_model::adoption::addition(change)?;
            Some((addition.raw(), record.partitions.clone()))
        })
        .collect();
    let of = |adopted: &str, beside: &str| {
        vec![PartitionOwns {
            table: name("c"),
            own: own.clone(),
            adopted: names(&[adopted]),
            beside: names(&[beside]),
        }]
    };
    assert_eq!(
        found,
        [("p_m", of("c_mo", "c_ma")), ("p_n", of("c_old", "c_a"))].into()
    );

    let mut twice = answered;
    twice.adopted.insert("c_mo".into(), "p_n".into());
    assert_eq!(world.plan(given(twice)), world.plan(Vec::new()));
}
