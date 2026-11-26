use super::*;
use crate::identity::Context;
use crate::schema_diff::tests::Postgres;
use pbps_dialect::MinimalDialect;
use pbps_model::adoption::RelationIndex;
use pbps_model::{
    Column, IndexColumn, Module, ModuleKind, PartitionBound, PartitionBy, PartitionOf,
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
        let declared_ids = crate::resolve(&self.declared, &self.ids, intents, &context())
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
        self.sides(&[], |base, declared| {
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
        self.sides(&[], |base, declared| {
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
    AttachAnswer {
        uid: question.uid,
        parent: question.parent,
        rehearsal: AttachRehearsal {
            parent: vec![read("p_n")],
            table: vec![read("c_z"), read("c_a")],
            kept: [("c_z".into(), "p_n".into())].into(),
        },
    }
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

/// Neither a key nor an exclusion is ever asked about, and what no parent's
/// index might take is not either.
#[test]
fn a_key_an_untaken_index_and_a_certain_one_are_not_asked_about() {
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
    // The key is dropped ahead of the attach, and is not a candidate.
    assert!(world.plan(Vec::new()).iter().any(|change| matches!(
        change,
        Change::Constraint(ConstraintChange::DropUnique { name, .. }) if name.raw() == "t_u"
    )));
    let [question] = &world.questions()[..] else {
        panic!("one question: {:?}", world.questions())
    };
    assert_eq!(question.owns, names(&["c_a", "c_z"]));

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
        assert_eq!(
            questions(
                base,
                declared,
                &MinimalDialect,
                &Hints::default(),
                base.schema
            ),
            []
        );
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
            questions(base, declared, &Postgres, &Hints::default(), base.schema)
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
            questions(base, declared, &Postgres, &hints, base.schema),
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
    assert_eq!(recorded(&with), Some(&answer.rehearsal));
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
                *rehearsal = Some(Box::new(answer.rehearsal.clone()));
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
    let wrong = |edit: &dyn Fn(&mut AttachAnswer)| {
        let mut answer = good.clone();
        edit(&mut answer);
        vec![answer]
    };
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
