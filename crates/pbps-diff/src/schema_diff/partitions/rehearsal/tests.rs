use super::*;
use crate::identity::Context;
use crate::schema_diff::tests::Postgres;
use pbps_dialect::MinimalDialect;
use pbps_model::adoption::{AttachedAnswer, RelationIndex, Skipped};
use pbps_model::{
    Column, IndexColumn, Module, ModuleKind, PartitionBound, PartitionBy, PartitionOf,
};
use std::collections::BTreeMap;

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

fn columns() -> Table {
    Table {
        columns: ["id", "m", "n"]
            .into_iter()
            .map(|name| {
                let column = Column::new("int".parse().unwrap()).not_null();
                (name.into(), column)
            })
            .collect(),
        ..Default::default()
    }
}

/// `app.t` ordinary with two indexes a parent's `p_n` might take, declared a
/// partition of `app.p`; `app.other` stands beside them.
struct World {
    base: Schema,
    declared: Schema,
    /// Minted once, so an answer made from one plan names the same table in
    /// the next.
    ids: IdsFile,
}

impl World {
    fn new() -> Self {
        let mut parent = columns();
        parent.partition_by = Some(PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        });
        parent.indexes.insert("p_n".into(), plain("n"));
        let mut ordinary = columns();
        ordinary.indexes.insert("c_z".into(), plain("n"));
        ordinary.indexes.insert("c_a".into(), plain("n"));
        let mut base = Schema::default();
        base.tables.insert(name("p"), parent);
        base.tables.insert(name("t"), ordinary);
        base.tables.insert(name("other"), columns());
        let mut declared = base.clone();
        declared.tables.insert(
            name("t"),
            Table {
                partition_of: Some(PartitionOf {
                    parent: name("p"),
                    bound: PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
        let ids = crate::resolve(&base, &IdsFile::default(), &[], &context())
            .unwrap()
            .ids;
        Self {
            base,
            declared,
            ids,
        }
    }

    fn table(&mut self) -> &mut Table {
        self.declared.tables.get_mut(&name("t")).unwrap()
    }

    /// Runs `with` on the two sides, the declared identities carried over
    /// from the base's through `intents`.
    fn sides<T>(
        &self,
        intents: &[pbps_model::Intent],
        with: impl FnOnce(Side<'_>, Side<'_>) -> T,
    ) -> T {
        let baseline = Side {
            schema: &self.base,
            ids: &self.ids,
        };
        let declared_ids = crate::resolve_with_annotations_against(
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
            Side {
                schema: &self.base,
                ids: &self.ids,
            },
            Side {
                schema: &self.declared,
                ids: &declared_ids,
            },
        )
    }

    fn questions(&self) -> Vec<Question> {
        self.questions_given(&[])
    }

    fn questions_given(&self, intents: &[pbps_model::Intent]) -> Vec<Question> {
        self.all_questions(intents).attaches
    }

    fn all_questions(&self, intents: &[pbps_model::Intent]) -> Questions {
        self.sides(intents, |base, declared| {
            questions(
                base,
                declared,
                &MinimalDialect,
                &Hints::default(),
                base.schema,
            )
        })
    }

    fn plan(&self, answers: Vec<AttachAnswer>) -> Vec<Change> {
        self.plan_given(&[], answers)
    }

    fn plan_given(
        &self,
        intents: &[pbps_model::Intent],
        answers: Vec<AttachAnswer>,
    ) -> Vec<Change> {
        self.sides(intents, |base, declared| {
            let hints = Hints {
                attach_rehearsals: answers,
                ..Default::default()
            };
            crate::diff(base, declared, &MinimalDialect, &hints)
                .expect("the attach is planned")
                .changes()
                .changes
                .iter()
                .map(|p| p.change.clone())
                .collect()
        })
    }
}

fn context() -> Context {
    Context {
        operator: "leon".into(),
        today: "2026-11-26".into(),
    }
}

fn name(table: &str) -> TableName {
    TableName::new("app", table)
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

/// The answer the engine would give for [`World::new`]: `c_z`, made first, is
/// taken as `p_n`'s clone.
fn answer(world: &World) -> AttachAnswer {
    answer_to(world.questions().pop().expect("a question"))
}

fn answer_to(question: Question) -> AttachAnswer {
    AttachAnswer::Attach(AttachedAnswer {
        uid: question.uid,
        parent: question.parent,
        rehearsal: AttachRehearsal {
            parent: vec![read("p_n")],
            table: vec![read("c_z"), read("c_a")],
            kept: [("c_z".into(), "p_n".into())].into(),
        },
    })
}

fn inner(answer: &AttachAnswer) -> &AttachedAnswer {
    let AttachAnswer::Attach(inner) = answer else {
        panic!("an attach's answer: {answer:?}")
    };
    inner
}

/// `answer` with its attach's fields edited.
fn edited(answer: &AttachAnswer, edit: &dyn Fn(&mut AttachedAnswer)) -> AttachAnswer {
    let mut inner = inner(answer).clone();
    edit(&mut inner);
    AttachAnswer::Attach(inner)
}

fn dropped(changes: &[Change]) -> Vec<&str> {
    changes
        .iter()
        .filter_map(|change| {
            if let Change::Index(IndexChange::DropIndex { name, .. }) = change {
                Some(name.raw())
            } else {
                None
            }
        })
        .collect()
}

fn recorded(changes: &[Change]) -> Option<&AttachRehearsal> {
    changes
        .iter()
        .find_map(|change| {
            if let Change::Partition(PartitionChange::AttachPartition { rehearsal, .. }) = change {
                Some(rehearsal.as_deref())
            } else {
                None
            }
        })
        .expect("an attach")
}

#[test]
fn every_plain_index_the_plan_would_drop_is_asked_about() {
    let world = World::new();
    let [question] = &world.questions()[..] else {
        panic!("one question: {:?}", world.questions())
    };
    assert_eq!(question.table, name("t"));
    assert_eq!(question.parent, name("p"));
    assert_eq!(question.owns, names(&["c_a", "c_z"]));
    assert!(question.dropped.is_empty());
    world.sides(&[], |base, declared| {
        assert_eq!(
            candidates(base, declared),
            [name("p"), name("t")].into_iter().collect()
        );
    });
}

/// A key the plan releases is asked about with the indexes. What no parent's
/// index might take is not, and is left out of the rehearsal with the other
/// drops; a certain index is left to the attach as it is.
#[test]
fn a_released_key_is_asked_about_and_an_untaken_index_and_a_certain_one_are_not() {
    let mut world = World::new();
    let key = pbps_model::UniqueConstraint {
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    };
    let parent = world.base.tables.get_mut(&name("p")).unwrap();
    parent.unique.insert("p_k".into(), key.clone());
    parent.indexes.insert(
        "p_u".into(),
        Index {
            unique: true,
            ..plain("id")
        },
    );
    let table = world.base.tables.get_mut(&name("t")).unwrap();
    table.unique.insert("t_u".into(), key);
    table.indexes.insert("c_m".into(), plain("m"));
    world
        .declared
        .tables
        .insert(name("p"), world.base.tables[&name("p")].clone());
    assert_eq!(
        taken_off(&world.plan(Vec::new())),
        ["c_a", "c_m", "c_z", "t_u"]
    );
    let [question] = &world.questions()[..] else {
        panic!("one question: {:?}", world.questions())
    };
    assert_eq!(question.owns, names(&["c_a", "c_z", "t_u"]));
    assert_eq!(question.dropped, names(&["c_m"]));

    // A lone candidate equal to the parent's index is already left to it.
    let mut certain = World::new();
    certain
        .base
        .tables
        .get_mut(&name("t"))
        .unwrap()
        .indexes
        .remove("c_a");
    assert_eq!(certain.questions(), []);

    // No attach, no question and nothing to read ahead of the diff.
    let mut standing = World::new();
    standing.declared = standing.base.clone();
    assert_eq!(standing.questions(), []);
    standing.sides(&[], |base, declared| {
        assert!(candidates(base, declared).is_empty());
    });
}

#[test]
fn an_index_under_a_name_the_declarations_give_something_is_dropped_whatever_the_engine_says() {
    // The partition's own index, check or foreign key.
    let mut declares = World::new();
    declares.table().indexes.insert("c_z".into(), plain("m"));
    let mut checks = World::new();
    checks.table().checks.insert(
        "c_z".into(),
        pbps_model::CheckConstraint {
            expression: "m > 0".into(),
        },
    );
    // Another table's index, or a table, of the same schema.
    let mut elsewhere = World::new();
    elsewhere
        .declared
        .tables
        .get_mut(&name("other"))
        .unwrap()
        .indexes
        .insert("c_z".into(), plain("m"));
    let mut table = World::new();
    table.declared.tables.insert(name("c_z"), columns());
    for world in [declares, checks, elsewhere, table] {
        let [question] = &world.questions()[..] else {
            panic!("one question: {:?}", world.questions())
        };
        assert_eq!(question.owns, names(&["c_a"]));
        assert_eq!(question.dropped, names(&["c_z"]));
    }

    // Another schema's relation of that name is no claim on it.
    let mut other_schema = World::new();
    other_schema
        .declared
        .tables
        .insert(TableName::new("lib", "c_z"), columns());
    assert_eq!(other_schema.questions()[0].owns, names(&["c_a", "c_z"]));

    // With every candidate claimed there is nothing to ask.
    let mut all = World::new();
    for index in ["c_a", "c_z"] {
        all.table().indexes.insert(index.into(), plain("m"));
    }
    assert_eq!(all.questions(), []);
}

#[test]
fn a_plan_that_can_move_a_definition_before_the_attach_asks_nothing() {
    use pbps_model::Intent;
    // The parent reshaped beside the attach.
    let mut reshaped = World::new();
    reshaped
        .declared
        .tables
        .get_mut(&name("p"))
        .unwrap()
        .columns
        .get_mut("m")
        .unwrap()
        .nullable = true;
    assert_eq!(reshaped.questions(), []);
    // The reason is said where the plan itself stands: an engine that plans the
    // two together.
    let meeting = crate::schema_diff::meeting_shape_tests::Engine {
        enabled: true,
        label: "postgres",
    };
    let found = reshaped.sides(&[], |base, declared| {
        questions(base, declared, &meeting, &Hints::default(), base.schema)
    });
    assert_eq!(found.attaches, []);
    let [(_, why)] = &found.skipped[..] else {
        panic!("one reason: {found:?}")
    };
    assert_eq!(*why, Skipped::ParentColumns);

    // A column renamed on a table the attach does not touch.
    let mut renamed = World::new();
    let other = renamed.declared.tables.get_mut(&name("other")).unwrap();
    let column = other.columns.shift_remove("m").unwrap();
    other.columns.insert("moved".into(), column);
    let intent = [Intent::RenameColumn {
        table: name("other"),
        from: "m".into(),
        to: "moved".into(),
    }];
    renamed.sides(&intent, |base, declared| {
        assert!(
            crate::diff(base, declared, &MinimalDialect, &Hints::default())
                .expect("planned")
                .changes()
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::Column(ColumnChange::RenameColumn { .. })))
        );
        let found = questions(
            base,
            declared,
            &MinimalDialect,
            &Hints::default(),
            base.schema,
        );
        assert_eq!(found.attaches, []);
        let [(_, why)] = &found.skipped[..] else {
            panic!("one reason: {found:?}")
        };
        assert_eq!(*why, Skipped::Unsettles);
    });
}

/// An update can reprint, or bind anew, an operator class or a function an
/// index names.
#[test]
fn a_plan_that_updates_an_extension_asks_nothing() {
    let package = |version: &str| pbps_model::Extension {
        schema: "public".into(),
        version: version.into(),
    };
    let mut world = World::new();
    for schema in [&mut world.base, &mut world.declared] {
        schema
            .extensions
            .insert("btree_gist".into(), package("1.6"));
    }
    world.ids = crate::resolve(&world.base, &IdsFile::default(), &[], &context())
        .unwrap()
        .ids;
    let asked = |world: &World| {
        world.sides(&[], |base, declared| {
            questions(base, declared, &Postgres, &Hints::default(), base.schema).attaches
        })
    };
    // The package standing as it was does not stop the question.
    let good = answer_to(asked(&world).pop().expect("a question"));

    world
        .declared
        .extensions
        .insert("btree_gist".into(), package("1.7"));
    let hints = Hints {
        extension_updates: [("btree_gist".into(), Default::default())].into(),
        attach_rehearsals: vec![good],
        ..Default::default()
    };
    let plan: Vec<_> = world.sides(&[], |base, declared| {
        assert_eq!(
            questions(base, declared, &Postgres, &hints, base.schema).attaches,
            []
        );
        crate::diff(base, declared, &Postgres, &hints)
            .expect("the update and the attach are planned")
            .changes()
            .changes
            .iter()
            .map(|p| p.change.clone())
            .collect()
    });
    assert!(plan.iter().any(|change| matches!(
        change,
        Change::Extension(ExtensionChange::UpdateExtension { .. })
    )));
    assert_eq!(dropped(&plan), ["c_a", "c_z"]);
    assert_eq!(recorded(&plan), None);
}

#[test]
fn only_the_changes_that_can_move_a_definition_unsettle_an_answer() {
    let routine = ModuleId::Routine("app.f(integer)".parse().unwrap());
    let view = ModuleId::Named(name("v"));
    let module = |kind| {
        Box::new(Module {
            kind,
            description: None,
            options: Default::default(),
            definition: "x".into(),
        })
    };
    let uid: Uid = "t_aaaaaa".parse().unwrap();
    let rename = Change::Table(TableChange::RenameTable {
        uid: uid.clone(),
        from: name("a"),
        to: name("b"),
        defaults: Default::default(),
    });
    assert!(unsettles(&rename));
    assert!(unsettles(&Change::Module(ModuleChange::AlterModule {
        id: routine.clone(),
        module: module(ModuleKind::Function),
    })));
    assert!(unsettles(&Change::Module(ModuleChange::DropModule {
        id: routine.clone(),
        kind: ModuleKind::Function,
    })));
    for settled in [
        Change::Module(ModuleChange::CreateModule {
            id: routine,
            module: module(ModuleKind::Function),
        }),
        Change::Module(ModuleChange::DropModule {
            id: view,
            kind: ModuleKind::View,
        }),
        Change::Index(IndexChange::DropIndex {
            table: name("t"),
            name: "c_a".into(),
        }),
        Change::Table(TableChange::DropTable {
            uid,
            name: name("other"),
            detach_from: None,
        }),
    ] {
        assert!(!unsettles(&settled), "{settled:?}");
    }
}

#[test]
fn an_answer_leaves_its_index_to_the_attach_and_changes_nothing_else() {
    let world = World::new();
    let without = world.plan(Vec::new());
    assert_eq!(dropped(&without), ["c_a", "c_z"]);
    assert_eq!(recorded(&without), None);

    let answer = answer(&world);
    let with = world.plan(vec![answer.clone()]);
    assert_eq!(dropped(&with), ["c_a"]);
    assert_eq!(recorded(&with), Some(&inner(&answer).rehearsal));
    // The same changes otherwise, in the same order, and none added.
    let expected: Vec<_> = without
        .iter()
        .filter(|c| {
            !matches!(c, Change::Index(IndexChange::DropIndex { name, .. }) if name.raw() == "c_z")
        })
        .cloned()
        .map(|mut change| {
            if let Change::Partition(PartitionChange::AttachPartition { rehearsal, .. }) =
                &mut change
            {
                *rehearsal = Some(Box::new(inner(&answer).rehearsal.clone()));
            }
            change
        })
        .collect();
    assert_eq!(with, expected);
}

#[test]
fn an_answer_that_does_not_agree_with_its_question_plans_as_without_a_resolver() {
    let world = World::new();
    let without = world.plan(Vec::new());
    let good = answer(&world);
    let wrong = |edit: &dyn Fn(&mut AttachedAnswer)| vec![edited(&good, edit)];
    let keyed = |mut index: RelationIndex| {
        index.constraint = Some("UNIQUE (n)".into());
        index
    };
    let cases: Vec<(&str, Vec<AttachAnswer>)> = vec![
        (
            "another table's identity",
            wrong(&|a| a.uid = "t_zzzzzz".parse().unwrap()),
        ),
        ("another parent", wrong(&|a| a.parent = name("other"))),
        (
            "a candidate the read did not hold",
            wrong(&|a| a.rehearsal.table.retain(|i| i.name.raw() != "c_a")),
        ),
        (
            "an index that was not asked about",
            wrong(&|a| {
                a.rehearsal.table.push(read("c_x"));
                a.rehearsal.kept = [("c_x".into(), "p_n".into())].into();
            }),
        ),
        (
            "one parent index taking two",
            wrong(&|a| {
                a.rehearsal.kept.insert("c_a".into(), "p_n".into());
            }),
        ),
        (
            "a taker the parent does not hold",
            wrong(&|a| a.rehearsal.kept = [("c_z".into(), "p_x".into())].into()),
        ),
        (
            "a constraint's index left to the attach",
            wrong(&|a| a.rehearsal.table[0] = keyed(read("c_z"))),
        ),
        (
            "a taker that is a constraint's",
            wrong(&|a| {
                a.rehearsal.parent.push(keyed(read("p_u")));
                a.rehearsal.kept = [("c_z".into(), "p_u".into())].into();
            }),
        ),
        (
            "a parent that has gained an index",
            wrong(&|a| a.rehearsal.parent.push(read("p_new"))),
        ),
        (
            "a parent that has lost one",
            wrong(&|a| a.rehearsal.parent.clear()),
        ),
        ("nothing kept", wrong(&|a| a.rehearsal.kept.clear())),
        ("two answers", vec![good.clone(), good.clone()]),
    ];
    for (why, answers) in cases {
        assert_eq!(world.plan(answers), without, "{why}");
    }
}

/// A function rebuilt beside the attach takes down what calls it, so the
/// answer made without that rebuild is not used.
#[test]
fn an_answer_is_not_used_in_a_plan_that_rebuilds_a_function() {
    let mut world = World::new();
    let good = answer(&world);
    let id = ModuleId::Routine("app.f()".parse().unwrap());
    let function = |body: &str| Module {
        kind: ModuleKind::Function,
        description: None,
        options: Default::default(),
        definition: body.into(),
    };
    world.base.modules.insert(
        id.clone(),
        function("returns int language sql as 'select 1'"),
    );
    world
        .declared
        .modules
        .insert(id, function("returns int language sql as 'select 2'"));
    assert_eq!(world.questions(), []);
    let plan = world.plan(vec![good]);
    assert_eq!(dropped(&plan), ["c_a", "c_z"]);
    assert_eq!(recorded(&plan), None);
}

/// Every index, key and exclusion the plan drops, by name, in order of name.
fn taken_off(changes: &[Change]) -> Vec<&str> {
    let mut found: Vec<_> = changes
        .iter()
        .filter_map(|change| {
            if let Change::Index(IndexChange::DropIndex { name, .. })
            | Change::Constraint(ConstraintChange::DropUnique { name, .. })
            | Change::Constraint(ConstraintChange::SetExclusion { name, to: None, .. }) = change
            {
                Some(name.raw())
            } else if let Change::Constraint(ConstraintChange::SetPrimaryKey {
                from: Some(key),
                to: None,
                ..
            }) = change
            {
                Some(key.name.as_ref().map_or("(unnamed)", |name| name.raw()))
            } else {
                None
            }
        })
        .collect();
    found.sort_unstable();
    found
}

fn on_id() -> pbps_model::UniqueConstraint {
    pbps_model::UniqueConstraint {
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

fn primary_key(name: Option<&str>) -> pbps_model::PrimaryKey {
    pbps_model::PrimaryKey {
        name: name.map(Into::into),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

/// [`World::new`] with a key and a unique constraint on both sides, and a
/// unique index of the parent's that might take either, so the plan releases
/// both.
fn keyed_world() -> World {
    let mut world = World::new();
    let parent = world.base.tables.get_mut(&name("p")).unwrap();
    parent.primary_key = Some(primary_key(Some("p_pk")));
    parent.unique.insert("p_u".into(), on_id());
    parent.indexes.insert(
        "p_ui".into(),
        Index {
            unique: true,
            ..plain("id")
        },
    );
    let table = world.base.tables.get_mut(&name("t")).unwrap();
    table.primary_key = Some(primary_key(Some("t_pk")));
    table.unique.insert("t_u".into(), on_id());
    let parent = world.base.tables[&name("p")].clone();
    world.declared.tables.insert(name("p"), parent);
    world
}

fn under(name: &str, constraint: &str) -> RelationIndex {
    RelationIndex {
        constraint: Some(constraint.into()),
        ..read(name)
    }
}

/// An answer for [`keyed_world`] that keeps both keys and one index.
fn keyed_answer(world: &World) -> AttachAnswer {
    let question = world.questions().pop().expect("a question");
    AttachAnswer::Attach(AttachedAnswer {
        uid: question.uid,
        parent: question.parent,
        rehearsal: AttachRehearsal {
            parent: vec![
                read("p_n"),
                read("p_ui"),
                under("p_u", "UNIQUE (id)"),
                under("p_pk", "PRIMARY KEY (id)"),
            ],
            table: vec![
                read("c_z"),
                read("c_a"),
                under("t_u", "UNIQUE (id)"),
                under("t_pk", "PRIMARY KEY (id)"),
            ],
            kept: [
                ("c_z".into(), "p_n".into()),
                ("t_u".into(), "p_u".into()),
                ("t_pk".into(), "p_pk".into()),
            ]
            .into(),
        },
    })
}

#[test]
fn a_kept_key_and_unique_constraint_lose_their_drops_and_nothing_else_changes() {
    let world = keyed_world();
    let [question] = &world.questions()[..] else {
        panic!("one question: {:?}", world.questions())
    };
    assert_eq!(question.owns, names(&["c_a", "c_z", "t_pk", "t_u"]));
    assert!(question.dropped.is_empty());
    let without = world.plan(Vec::new());
    assert_eq!(taken_off(&without), ["c_a", "c_z", "t_pk", "t_u"]);

    let answer = keyed_answer(&world);
    let with = world.plan(vec![answer.clone()]);
    assert_eq!(taken_off(&with), ["c_a"]);
    assert_eq!(recorded(&with), Some(&inner(&answer).rehearsal));
    assert_eq!(with.len() + 3, without.len());
    assert!(with.iter().all(|change| {
        matches!(
            change,
            Change::Partition(PartitionChange::AttachPartition { .. })
        ) || without.contains(change)
    }));
}

/// One of the table's is left to the attach only under one of the parent's of
/// its own kind, and only where each list is what the plan found.
#[test]
fn an_answer_pairing_two_kinds_or_reading_another_plans_as_without_a_resolver() {
    let world = keyed_world();
    let without = world.plan(Vec::new());
    let good = keyed_answer(&world);
    assert_ne!(world.plan(vec![good.clone()]), without);
    let wrong = |edit: &dyn Fn(&mut AttachRehearsal)| {
        vec![edited(&good, &|answer| edit(&mut answer.rehearsal))]
    };
    let keep = |own: &str, by: &str| -> BTreeMap<ObjectLocalName, ObjectLocalName> {
        [(own.into(), by.into())].into()
    };
    let cases = [
        (
            "a constraint under a plain index",
            wrong(&|r| r.kept = keep("t_u", "p_ui")),
        ),
        (
            "a unique constraint under the key",
            wrong(&|r| r.kept = keep("t_u", "p_pk")),
        ),
        (
            "the key under a unique constraint",
            wrong(&|r| r.kept = keep("t_pk", "p_u")),
        ),
        (
            "an index under a constraint",
            wrong(&|r| r.kept = keep("c_z", "p_u")),
        ),
        (
            "an own read as another kind than the plan holds",
            wrong(&|r| {
                r.table[2] = read("t_u");
                r.kept = keep("c_z", "p_n");
            }),
        ),
        (
            "an own whose constraint is of no known kind",
            wrong(&|r| {
                r.table[2] = under("t_u", "CHECK (id > 0)");
                r.kept = keep("c_z", "p_n");
            }),
        ),
        (
            "a parent that has lost a constraint",
            wrong(&|r| {
                r.parent.retain(|i| i.name.raw() != "p_u");
                r.kept = keep("c_z", "p_n");
            }),
        ),
        (
            "a parent's constraint read as another kind",
            wrong(&|r| {
                r.parent[2] = under("p_u", "EXCLUDE USING btree (id WITH =)");
                r.kept = keep("c_z", "p_n");
            }),
        ),
    ];
    for (why, answers) in cases {
        assert_eq!(world.plan(answers), without, "{why}");
    }
}

/// A key under a declared name is dropped whatever the engine says, and a key
/// the base holds no name for cannot be left out of a rehearsal, so nothing is
/// asked.
#[test]
fn a_key_under_a_declared_name_is_not_asked_about_and_an_unnamed_one_stops_the_question() {
    let mut declared = keyed_world();
    declared
        .declared
        .tables
        .get_mut(&name("other"))
        .unwrap()
        .indexes
        .insert("t_u".into(), plain("m"));
    let [question] = &declared.questions()[..] else {
        panic!("one question: {:?}", declared.questions())
    };
    assert_eq!(question.owns, names(&["c_a", "c_z", "t_pk"]));
    assert_eq!(question.dropped, names(&["t_u"]));

    let mut unnamed = keyed_world();
    let good = keyed_answer(&unnamed);
    let table = unnamed.base.tables.get_mut(&name("t")).unwrap();
    table.primary_key = Some(primary_key(None));
    assert_eq!(unnamed.questions(), []);
    let plan = unnamed.plan(vec![good]);
    assert_eq!(taken_off(&plan), ["(unnamed)", "c_a", "c_z", "t_u"]);
    assert_eq!(recorded(&plan), None);
}

fn excluding(columns: &[&str]) -> pbps_model::ExclusionConstraint {
    pbps_model::ExclusionConstraint {
        method: pbps_model::IndexMethod::Btree,
        keys: columns
            .iter()
            .map(|column| pbps_model::ExclusionKey {
                key: IndexColumn::column(*column),
                operator: pbps_model::OperatorRef::symbol("=").unwrap(),
            })
            .collect(),
        include: Vec::new(),
        filter: None,
        storage_parameters: Default::default(),
        timing: pbps_model::ExclusionTiming::Immediate,
    }
}

/// [`World::new`] with an exclusion of the parent's and one of the table's it
/// might take, and the record that retires the table's.
fn excluding_world() -> (World, [pbps_model::Intent; 1]) {
    let mut world = World::new();
    for (table, exclusion) in [("p", "p_x"), ("t", "t_x")] {
        let held = world.base.tables.get_mut(&name(table)).unwrap();
        held.exclude.insert(exclusion.into(), excluding(&["id"]));
    }
    let parent = world.base.tables[&name("p")].clone();
    world.declared.tables.insert(name("p"), parent);
    let retired = pbps_model::Intent::DropExclusion {
        table: name("t"),
        name: "t_x".into(),
        reason: "the parent's holds it".parse().unwrap(),
    };
    (world, [retired])
}

fn excluding_answer(question: Question) -> AttachAnswer {
    let exclusion = |name| under(name, "EXCLUDE USING btree (id WITH =)");
    AttachAnswer::Attach(AttachedAnswer {
        uid: question.uid,
        parent: question.parent,
        rehearsal: AttachRehearsal {
            parent: vec![read("p_n"), exclusion("p_x")],
            table: vec![read("c_z"), read("c_a"), exclusion("t_x")],
            kept: [("t_x".into(), "p_x".into())].into(),
        },
    })
}

#[test]
fn a_retired_exclusion_the_parents_might_take_is_asked_about_and_may_be_kept() {
    let (world, retired) = excluding_world();
    let [question] = &world.questions_given(&retired)[..] else {
        panic!("one question")
    };
    assert_eq!(question.owns, names(&["c_a", "c_z", "t_x"]));
    let without = world.plan_given(&retired, Vec::new());
    assert_eq!(taken_off(&without), ["c_a", "c_z", "t_x"]);
    let answer = excluding_answer(question.clone());
    let with = world.plan_given(&retired, vec![answer.clone()]);
    assert_eq!(taken_off(&with), ["c_a", "c_z"]);
    assert_eq!(recorded(&with), Some(&inner(&answer).rehearsal));
    // Under another kind of the parent's it is dropped as without an answer.
    let crossed = edited(&answer, &|a| {
        a.rehearsal.kept = [("t_x".into(), "p_n".into())].into();
    });
    assert_eq!(world.plan_given(&retired, vec![crossed]), without);
}

#[test]
fn an_exclusion_that_is_declared_unretired_or_untaken_is_never_asked_about() {
    // Declared: dropped and added back, whatever the engine says.
    let (mut declared, _) = excluding_world();
    declared
        .table()
        .exclude
        .insert("t_x".into(), excluding(&["id"]));
    let [question] = &declared.questions()[..] else {
        panic!("one question: {:?}", declared.questions())
    };
    assert_eq!(question.owns, names(&["c_a", "c_z"]));
    assert_eq!(question.dropped, names(&["t_x"]));

    // Neither declared nor retired: the plan is refused, and nothing is asked.
    let (unretired, _) = excluding_world();
    let side = Side {
        schema: &unretired.declared,
        ids: &unretired.ids,
    };
    let base = Side {
        schema: &unretired.base,
        ids: &unretired.ids,
    };
    assert!(crate::diff(base, side, &MinimalDialect, &Hints::default()).is_err());
    // A refused plan asks nothing, and gives no reason either.
    assert!(questions(base, side, &MinimalDialect, &Hints::default(), base.schema).is_empty());

    // Retired, and over another number of keys than any of the parent's: its
    // drop is the retirement's, and the engine has no say.
    let (mut untaken, retired) = excluding_world();
    let table = untaken.base.tables.get_mut(&name("t")).unwrap();
    table.exclude.insert("t_x".into(), excluding(&["id", "m"]));
    let [question] = &untaken.questions_given(&retired)[..] else {
        panic!("one question")
    };
    assert_eq!(question.owns, names(&["c_a", "c_z"]));
    assert_eq!(question.dropped, names(&["t_x"]));
}
