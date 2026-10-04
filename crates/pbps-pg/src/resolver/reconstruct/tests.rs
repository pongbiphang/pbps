use super::*;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_model::{
    CheckConstraint, Column, ForeignKey, GrantTarget, Index, IndexColumn, Module, Table, TableName,
    Uid, UidKind,
};

fn table() -> Table {
    let mut table = Table::default();
    let mut column = Column::new("numeric".parse().unwrap());
    column.default = Some("app.f(1)".into());
    table.columns.insert("c".into(), column);
    table.checks.insert(
        "ck".into(),
        CheckConstraint {
            expression: "c > app.f(1)".into(),
        },
    );
    table.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column("c".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("c > app.f(1)".into()),
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    table.indexes.insert(
        "plain".into(),
        Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column("c".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    table.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["c".into()],
            references_table: TableName::new("app", "parent"),
            references_columns: vec!["c".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    table
}

fn module(id: &str, kind: ModuleKind, definition: &str) -> Change {
    Change::CreateModule {
        id: id.parse().unwrap(),
        module: Box::new(Module {
            kind,
            description: None,
            definition: definition.into(),
        }),
    }
}

fn bootstrap() -> Vec<Change> {
    vec![
        Change::CreateTable {
            uid: Uid::generate(UidKind::Table),
            name: TableName::new("app", "t"),
            table: Box::new(table()),
        },
        module(
            "app.f(integer)",
            ModuleKind::Function,
            "(integer) RETURNS numeric LANGUAGE sql RETURN $1",
        ),
        module(
            "app.t.audit",
            ModuleKind::Trigger,
            "AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.g()",
        ),
        module("app.v", ModuleKind::View, "SELECT app.f(1) AS x"),
        Change::Grant {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: Default::default(),
        },
    ]
}

/// A table is created bare; its keys and its indexes without a predicate
/// follow every table, the modules follow them in the differ's order, and
/// each expression, predicated index and trigger comes last, when every
/// routine it can name exists. A grant changes no binding and is not
/// reproduced.
#[test]
fn expressions_wait_for_every_module_and_modules_keep_the_differs_order() {
    let reconstruction = Reconstruction::new(&crate::Postgres::new(), &bootstrap()).unwrap();
    let phases: Vec<_> = reconstruction
        .steps
        .iter()
        .map(|step| (step.phase, step.declaration.as_str()))
        .collect();
    assert_eq!(
        phases,
        [
            (Phase::Tables, "table app.t"),
            (Phase::Keys, "table app.t"),
            (Phase::Keys, "index plain on app.t"),
            (Phase::Modules, "function app.f(integer)"),
            (Phase::Modules, "view app.v"),
            (Phase::Expressions, "the default of app.t.c"),
            (Phase::Expressions, "check ck on app.t"),
            (Phase::Expressions, "index ix on app.t"),
            (Phase::Expressions, "trigger app.t.audit"),
        ]
    );
    let table = reconstruction.steps[0].statements.join("\n");
    for expression in ["DEFAULT", "CHECK", "CREATE INDEX", "REFERENCES"] {
        assert!(!table.contains(expression), "{expression} in {table}");
    }
    let rest = reconstruction.steps[1..]
        .iter()
        .flat_map(|step| &step.statements)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    for expression in ["DEFAULT", "CHECK", "CREATE INDEX", "REFERENCES"] {
        assert!(
            rest.contains(expression),
            "{expression} missing from {rest}"
        );
    }
    assert!(!rest.contains("GRANT"), "{rest}");
}

/// What was made nameable after a module: every later module and every
/// index with a predicate; one without exists before any module. A view is found by name; a routine only by the overload its step
/// created, never by the first of its name. An object no compiled module is
/// has none.
#[test]
fn later_names_are_what_became_nameable_after_the_module() {
    let mut reconstruction = Reconstruction::new(&crate::Postgres::new(), &bootstrap()).unwrap();
    let routine = |argument: &str| ObjectIdentity {
        class: "pg_proc".into(),
        name: vec!["app".into(), "f".into()],
        signature: vec![ObjectIdentity {
            class: "pg_type".into(),
            name: vec!["pg_catalog".into(), argument.into()],
            signature: Vec::new(),
        }],
    };
    let relation = |name: &str| ObjectIdentity {
        class: "pg_class".into(),
        name: vec!["app".into(), name.into()],
        signature: Vec::new(),
    };
    // Nothing is known of a routine before its step has compiled.
    assert!(reconstruction.later_names(&routine("int4")).is_none());
    let step = reconstruction
        .steps
        .iter_mut()
        .find(|step| step.declaration == "function app.f(integer)")
        .unwrap();
    step.created = Some(routine("int4"));
    assert_eq!(
        reconstruction.later_names(&routine("int4")).unwrap(),
        [
            (Nameable::Relation, "app", "v"),
            (Nameable::Index, "app", "ix")
        ]
        .into_iter()
        .collect()
    );
    // Another overload of the same name was not compiled at that step.
    assert!(reconstruction.later_names(&routine("text")).is_none());
    assert_eq!(
        reconstruction.later_names(&relation("v")).unwrap(),
        [(Nameable::Index, "app", "ix")].into_iter().collect()
    );
    // A table is not a module.
    assert!(reconstruction.later_names(&relation("t")).is_none());
}

/// A later object could only have taken a binding it can be resolved as:
/// an index never a routine call, a routine never a relation, while a
/// relation's row type can take a one-argument call.
#[test]
fn a_later_object_shadows_only_what_it_could_be_resolved_as() {
    assert!(Nameable::Index.shadows("pg_class", false));
    assert!(!Nameable::Index.shadows("pg_proc", true));
    assert!(!Nameable::Index.shadows("pg_type", false));
    assert!(Nameable::Routine.shadows("pg_proc", false));
    assert!(Nameable::Routine.shadows("pg_type", false));
    assert!(!Nameable::Routine.shadows("pg_class", false));
    assert!(Nameable::Relation.shadows("pg_class", false));
    assert!(Nameable::Relation.shadows("pg_type", false));
    // A relation's row type takes a call only where a one-argument call can
    // reach the routine.
    assert!(Nameable::Relation.shadows("pg_proc", true));
    assert!(!Nameable::Relation.shadows("pg_proc", false));
    assert!(!Nameable::Relation.shadows("column", false));
    // A generated array type is a type only: never a relation, and a call
    // only as a cast.
    assert!(Nameable::Type.shadows("pg_type", false));
    assert!(!Nameable::Type.shadows("pg_class", false));
    assert!(Nameable::Type.shadows("pg_proc", true));
    assert!(!Nameable::Type.shadows("pg_proc", false));
}

/// The differ splits a new table's foreign keys out of its CREATE, so a
/// bootstrap of related tables carries each as its own change. It is a key
/// like one kept inline: it follows every table and precedes the modules.
#[test]
fn a_split_foreign_key_follows_every_table() {
    let parent = TableName::new("app", "p");
    let child = TableName::new("app", "c");
    let mut keyed = Table::default();
    keyed.columns.insert(
        "id".into(),
        pbps_model::Column::new("integer".parse().unwrap()).not_null(),
    );
    keyed.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("p_pk".into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    let mut referencing = Table::default();
    referencing.columns.insert(
        "id".into(),
        pbps_model::Column::new("integer".parse().unwrap()),
    );
    let bootstrap = vec![
        Change::CreateTable {
            uid: Uid::generate(UidKind::Table),
            name: child.clone(),
            table: Box::new(referencing),
        },
        Change::CreateTable {
            uid: Uid::generate(UidKind::Table),
            name: parent.clone(),
            table: Box::new(keyed),
        },
        Change::AddForeignKey {
            table: child,
            name: "c_fk".into(),
            constraint: Box::new(pbps_model::ForeignKey {
                columns: vec!["id".into()],
                references_table: parent,
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            }),
        },
        module(
            "app.f()",
            ModuleKind::Function,
            "() RETURNS integer LANGUAGE sql RETURN 1",
        ),
    ];
    let reconstruction = Reconstruction::new(&crate::Postgres::new(), &bootstrap).unwrap();
    let phases: Vec<_> = reconstruction
        .steps
        .iter()
        .map(|step| (step.phase, step.declaration.as_str()))
        .collect();
    assert_eq!(
        phases,
        [
            (Phase::Tables, "table app.c"),
            (Phase::Tables, "table app.p"),
            (Phase::Keys, "table app.c"),
            (Phase::Modules, "function app.f()"),
        ]
    );
    assert!(
        reconstruction.steps[2]
            .statements
            .join("\n")
            .contains("REFERENCES")
    );
}

/// A table whose generated column calls a declared function cannot have
/// the column split out without changing its column order, so the table
/// follows the function on scratch, as in the ordinary plan (DEC-1364.1):
/// its plain index with it, a view naming it after it, and a foreign key
/// naming it once every module exists. A table generating from nothing
/// declared keeps its place, and a function that needs the table it is
/// called from is refused by name.
#[test]
fn a_table_generating_from_a_declared_function_compiles_after_it() {
    use pbps_model::{Column, ForeignKey, Generated, Index, IndexColumn, IndexKey, Module, Schema};
    let generated = |expression: &str| {
        let mut column = Column::new("integer".parse().unwrap());
        column.generated = Some(Generated {
            expression: expression.into(),
            stored: true,
        });
        column
    };
    let schema = |reads_table: bool| {
        let mut n = Table::default();
        n.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        n.columns.insert("g".into(), generated("app.f(id)"));
        n.indexes.insert(
            "n_ix".into(),
            Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let mut plain = Table::default();
        plain
            .columns
            .insert("id".into(), Column::new("integer".parse().unwrap()));
        plain.columns.insert("g".into(), generated("id * 2"));
        plain.foreign_keys.insert(
            "plain_fk".into(),
            ForeignKey {
                columns: vec!["id".into()],
                references_table: TableName::new("app", "n"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        n.unique.insert(
            "n_id".into(),
            pbps_model::UniqueConstraint {
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            },
        );
        let mut schema = Schema::default();
        schema.tables.insert(TableName::new("app", "n"), n);
        schema.tables.insert(TableName::new("app", "plain"), plain);
        let body = if reads_table {
            "(integer) RETURNS integer LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT count(*)::integer FROM app.n; END"
        } else {
            "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1"
        };
        schema.modules.insert(
            "app.f(integer)".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: body.into(),
            },
        );
        schema.modules.insert(
            "app.v".parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT g FROM app.n".into(),
            },
        );
        // Names the table only inside an OID-alias literal.
        schema.modules.insert(
            "app.w".parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT 'app.n'::regclass AS r".into(),
            },
        );
        schema
    };
    let bootstrap = |schema: &Schema| {
        let ids = pbps_diff::resolve(
            schema,
            &pbps_model::IdsFile::default(),
            &[],
            &pbps_diff::Context {
                operator: "1274-test".into(),
                today: "2026-10-03".into(),
            },
        )
        .unwrap()
        .ids;
        let empty = Schema::default();
        pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &pbps_model::IdsFile::default(),
            },
            pbps_diff::Side { schema, ids: &ids },
            &crate::Postgres::new(),
            &pbps_model::Hints::default(),
        )
        .unwrap()
        .changes
        .into_iter()
        .map(|planned| planned.change)
        .collect::<Vec<_>>()
    };
    let reconstruction =
        Reconstruction::new(&crate::Postgres::new(), &bootstrap(&schema(false))).unwrap();
    let order: Vec<_> = reconstruction
        .steps
        .iter()
        .map(|step| (step.phase, step.declaration.as_str()))
        .collect();
    let at = |declaration: &str| {
        order
            .iter()
            .position(|(_, d)| *d == declaration)
            .unwrap_or_else(|| panic!("{declaration} missing from {order:?}"))
    };
    assert!(
        at("function app.f(integer)") < at("table app.n"),
        "{order:?}"
    );
    assert!(at("table app.n") < at("index n_ix on app.n"), "{order:?}");
    assert!(at("table app.n") < at("view app.v"), "{order:?}");
    assert!(at("table app.n") < at("view app.w"), "{order:?}");
    let key = order
        .iter()
        .rposition(|(_, d)| *d == "table app.plain")
        .unwrap();
    assert!(
        at("view app.v") < key,
        "the key waits for every module: {order:?}"
    );
    assert_eq!(order[key].0, Phase::Modules, "{order:?}");
    assert_eq!(
        order[at("table app.plain")],
        (Phase::Tables, "table app.plain")
    );
    assert_eq!(
        Reconstruction::new(&crate::Postgres::new(), &bootstrap(&schema(true))).unwrap_err(),
        ReconstructError::Cycle("table app.n".into())
    );
}

/// A plan that drops, renames or alters is not a bootstrap, and building a
/// namespace from one would reproduce neither side.
#[test]
fn a_change_no_bootstrap_contains_is_refused_by_kind() {
    let refused = Reconstruction::new(
        &crate::Postgres::new(),
        &[Change::DropTable {
            uid: Uid::generate(UidKind::Table),
            name: TableName::new("app", "t"),
        }],
    )
    .unwrap_err();
    assert_eq!(refused, ReconstructError::Unsupported("drop_table".into()));
}

/// A created table's identity on one of its indexes is a step after that
/// index, not part of the bare `CREATE`, which would name an index that is
/// not there yet (#1467 review).
#[test]
fn an_index_replica_identity_is_set_after_its_index() {
    let mut t = Table::default();
    t.columns.insert(
        "id".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    t.indexes.insert(
        "t_id".into(),
        Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: true,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    t.replica_identity = Some(pbps_model::ReplicaIdentity::Index("t_id".into()));
    let bootstrap = [Change::CreateTable {
        uid: Uid::derived(UidKind::Table, "app.t", 0),
        name: TableName::new("app", "t"),
        table: Box::new(t),
    }];
    let reconstruction = Reconstruction::new(&crate::Postgres::new(), &bootstrap).unwrap();
    let phases: Vec<_> = reconstruction
        .steps
        .iter()
        .map(|step| (step.phase, step.declaration.as_str()))
        .collect();
    assert_eq!(
        phases,
        [
            (Phase::Tables, "table app.t"),
            (Phase::Keys, "index t_id on app.t"),
            (Phase::Keys, "replica identity of app.t"),
        ]
    );
    let table = reconstruction.steps[0].statements.join("\n");
    assert!(!table.contains("REPLICA IDENTITY"), "{table}");
    let last = reconstruction.steps[2].statements.join("\n");
    assert!(last.contains("REPLICA IDENTITY USING INDEX"), "{last}");
}

/// Two tables held behind different functions keep the order between them:
/// `app.b` calls the earlier function and another of its generated columns
/// names `app.a` in an OID-alias literal, resolved as the table is created
/// (measured on 18), while `app.a` waits for the later function. Placing
/// `app.b` after its own function alone would create it before `app.a`.
#[test]
fn a_held_table_naming_another_held_table_follows_it() {
    use pbps_model::{Column, Generated, Module, Schema};
    let generated = |expression: &str| {
        let mut column = Column::new("integer".parse().unwrap());
        column.generated = Some(Generated {
            expression: expression.into(),
            stored: true,
        });
        column
    };
    let mut a = Table::default();
    a.columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    a.columns.insert("g".into(), generated("app.f2(id)"));
    let mut b = Table::default();
    b.columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    b.columns.insert("g".into(), generated("app.f1(id)"));
    let mut r = generated("'app.a'::regclass::oid::bigint");
    r.ty = "bigint".parse().unwrap();
    b.columns.insert("r".into(), r);
    let mut schema = Schema::default();
    schema.tables.insert(TableName::new("app", "a"), a);
    schema.tables.insert(TableName::new("app", "b"), b);
    for name in ["app.f1(integer)", "app.f2(integer)"] {
        schema.modules.insert(
            name.parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
            },
        );
    }
    let ids = pbps_diff::resolve(
        &schema,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "1274-test".into(),
            today: "2026-10-04".into(),
        },
    )
    .unwrap()
    .ids;
    let empty = Schema::default();
    let changes: Vec<_> = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &pbps_model::IdsFile::default(),
        },
        pbps_diff::Side {
            schema: &schema,
            ids: &ids,
        },
        &crate::Postgres::new(),
        &pbps_model::Hints::default(),
    )
    .unwrap()
    .changes
    .into_iter()
    .map(|planned| planned.change)
    .collect();
    let reconstruction = Reconstruction::new(&crate::Postgres::new(), &changes).unwrap();
    let order: Vec<_> = reconstruction
        .steps
        .iter()
        .map(|step| step.declaration.as_str())
        .collect();
    let at = |declaration: &str| {
        order
            .iter()
            .position(|d| *d == declaration)
            .unwrap_or_else(|| panic!("{declaration} missing from {order:?}"))
    };
    assert!(
        at("function app.f1(integer)") < at("table app.b"),
        "{order:?}"
    );
    assert!(
        at("function app.f2(integer)") < at("table app.a"),
        "{order:?}"
    );
    assert!(at("table app.a") < at("table app.b"), "{order:?}");
}
