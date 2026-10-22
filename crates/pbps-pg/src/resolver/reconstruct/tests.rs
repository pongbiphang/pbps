use super::*;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_model::{
    CheckConstraint, Column, ForeignKey, GrantTarget, Index, IndexColumn, Module, Table, TableName,
    Uid, UidKind,
};

fn table() -> Table {
    let mut table = Table::default();
    let mut column = Column::new("numeric".parse().unwrap());
    column.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
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
            beside_default: None,
            moves_at_most: None,
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
    let reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap()).unwrap();
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
    let mut reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap()).unwrap();
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
            beside_default: None,
            moves_at_most: None,
        },
        Change::CreateTable {
            uid: Uid::generate(UidKind::Table),
            name: parent.clone(),
            table: Box::new(keyed),
            beside_default: None,
            moves_at_most: None,
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
    let reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap).unwrap();
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

/// Scratch creates the prefix and its plain index before the generation's
/// function, then adds the generated suffix (DEC-1503.1). Readers and foreign
/// keys wait for the columns they may need. A table generating from nothing
/// declared keeps its place; a generation function needing the whole table
/// closes a named cycle.
#[test]
fn generated_columns_and_readers_follow_functions_but_relation_names_precede_them() {
    use pbps_model::{Column, ForeignKey, Generated, Index, IndexColumn, IndexKey, Module, Schema};
    let generated = |expression: &str| {
        let mut column = Column::new("integer".parse().unwrap());
        column.value_source = pbps_model::ValueSource::Generated(Generated {
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
        .clone()
        .into_iter()
        .map(|planned| planned.change)
        .collect::<Vec<_>>()
    };
    let reconstruction =
        build_reconstruction(&crate::Postgres::new(), &bootstrap(&schema(false))).unwrap();
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
        at("table app.n") < at("function app.f(integer)")
            && at("function app.f(integer)") < at("column app.n.g"),
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
        at("column app.n.g") < key,
        "the key waits for the target's columns: {order:?}"
    );

    assert_eq!(
        order[at("table app.plain")],
        (Phase::Tables, "table app.plain")
    );
    let why = build_reconstruction(&crate::Postgres::new(), &bootstrap(&schema(true)))
        .unwrap_err()
        .to_string();
    assert!(why.contains("app.n") && why.contains("app.f"), "{why}");
}

/// A partition's own default is set on scratch in its own step after its
/// parent's default, which reaches every partition (measured on 16 and 18),
/// and so after the declared functions it calls; the partition's table step
/// keeps its place (#1578 review).
#[test]
fn a_partitions_own_default_is_set_after_its_parents_and_its_functions() {
    use pbps_model::{
        Column, Module, PartitionBound, PartitionBy, PartitionColumn, PartitionOf, Schema,
    };
    let mut schema = Schema::default();
    let mut parent = Table::default();
    let mut v = Column::new("integer".parse().unwrap());
    v.value_source = pbps_model::ValueSource::Default("0".into());
    parent.columns.insert("v".into(), v);
    parent.partition_by = Some(PartitionBy {
        external: false,
        columns: vec!["v".into()],
        strategy: Default::default(),
    });
    schema.tables.insert(TableName::new("app", "ev"), parent);
    let partition = |bound: PartitionBound, default: &str| Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", "ev"),
            bound,
            columns: [(
                "v".into(),
                PartitionColumn {
                    default: Some(default.to_owned()),
                    not_null: false,
                },
            )]
            .into(),
        }),
        ..Table::default()
    };
    schema.tables.insert(
        TableName::new("app", "ev_new"),
        partition(PartitionBound::Default, "app.f(1)"),
    );
    schema.tables.insert(
        TableName::new("app", "ev_plain"),
        partition(
            PartitionBound::Range {
                from: vec![pbps_model::BoundDatum::MinValue],
                to: vec![pbps_model::BoundDatum::Value("0".into())],
            },
            "7",
        ),
    );
    schema.modules.insert(
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
        },
    );
    let ids = pbps_diff::resolve(
        &schema,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "1578-test".into(),
            today: "2026-10-06".into(),
        },
    )
    .unwrap()
    .ids;
    let bootstrap: Vec<_> = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
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
    .clone()
    .into_iter()
    .map(|planned| planned.change)
    .collect();
    let reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap).unwrap();
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
    for table in ["app.ev", "app.ev_new", "app.ev_plain"] {
        let declaration = format!("table {table}");
        assert_eq!(order[at(&declaration)].0, Phase::Tables, "{order:?}");
    }
    for partition in ["app.ev_new", "app.ev_plain"] {
        let own = format!("the default of {partition}.v");
        assert_eq!(order[at(&own)].0, Phase::Expressions, "{order:?}");
        assert!(at("the default of app.ev.v") < at(&own), "{order:?}");
        assert!(at("function app.f(integer)") < at(&own), "{order:?}");
        // Negative: the table step itself no longer carries it.
        let sql = reconstruction.steps[at(&format!("table {partition}"))]
            .statements
            .join("\n");
        assert!(!sql.contains("SET DEFAULT"), "{sql}");
    }
}

/// A plan that drops, renames or alters is not a bootstrap, and building a
/// namespace from one would reproduce neither side.
#[test]
fn a_change_no_bootstrap_contains_is_refused_by_kind() {
    let refused = build_reconstruction(
        &crate::Postgres::new(),
        &[Change::DropTable {
            uid: Uid::generate(UidKind::Table),
            name: TableName::new("app", "t"),
            detach_from: None,
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
        beside_default: None,
        moves_at_most: None,
    }];
    let reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap).unwrap();
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

/// Two tables calling different functions expose their prefixes before
/// either function (DEC-1503.1). A generated suffix of `app.b` can therefore
/// name `app.a` in an OID-alias literal even though `app.a`'s own generation
/// calls the later function.
#[test]
fn split_generated_tables_expose_names_before_either_function() {
    use pbps_model::{Column, Generated, Module, Schema};
    let generated = |expression: &str| {
        let mut column = Column::new("integer".parse().unwrap());
        column.value_source = pbps_model::ValueSource::Generated(Generated {
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
    .clone()
    .into_iter()
    .map(|planned| planned.change)
    .collect();
    let reconstruction = build_reconstruction(&crate::Postgres::new(), &changes).unwrap();
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
        at("table app.b") < at("function app.f1(integer)")
            && at("function app.f1(integer)") < at("column app.b.g"),
        "{order:?}"
    );
    assert!(
        at("table app.a") < at("function app.f2(integer)")
            && at("function app.f2(integer)") < at("column app.a.g"),
        "{order:?}"
    );
    assert!(at("table app.a") < at("table app.b"), "{order:?}");
}

/// #1597: only the server's judgement of a statement is a verdict on its
/// declaration. A terminated backend (`57P01`, measured), a lost connection
/// (no code), and the other classes about the server's state are not.
#[test]
fn a_compile_failure_is_a_verdict_only_when_the_server_judged_the_statement() {
    let failed = |code: Option<&str>| pbps_db::DbError::Driver {
        message: "failed".into(),
        code: code.map(Into::into),
    };
    for code in [
        "42P01", "42883", "42601", "0A000", "22023", "2BP01", "P0001", "25001",
    ] {
        assert!(judged_the_statement(&failed(Some(code))), "{code}");
    }
    for code in [
        None,
        Some("57P01"),
        Some("57014"),
        Some("25P03"),
        Some("25P04"),
        Some("08006"),
        Some("40P01"),
        Some("53200"),
        Some("58030"),
        Some("XX000"),
    ] {
        assert!(!judged_the_statement(&failed(code)), "{code:?}");
    }
    // A code wrapped in this tool's own context is still the server's.
    assert!(judged_the_statement(
        &failed(Some("42P01")).context("compiling app.t")
    ));
    assert!(!judged_the_statement(&pbps_db::DbError::Refused(
        "refused".into()
    )));
}

/// A fill's chained shapes wait for the last managed relation's complete
/// column shape. A split table exposes its prefix before the function and
/// finishes its generated suffix after it (DEC-1503.1, #1778).
#[test]
fn chained_shapes_wait_for_the_step_that_creates_what_they_name() {
    use pbps_model::{Column, Generated, Module, Schema};
    let mut generated = Column::new("integer".parse().unwrap());
    generated.value_source = pbps_model::ValueSource::Generated(Generated {
        expression: "app.f(id)".into(),
        stored: true,
    });
    let mut n = Table::default();
    n.columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    n.columns.insert("g".into(), generated);
    let mut plain = Table::default();
    plain
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut schema = Schema::default();
    schema.tables.insert(TableName::new("app", "n"), n);
    schema.tables.insert(TableName::new("app", "plain"), plain);
    schema.modules.insert(
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
        },
    );
    let ids = pbps_diff::resolve(
        &schema,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "1778-test".into(),
            today: "2026-10-17".into(),
        },
    )
    .unwrap()
    .ids;
    let empty = Schema::default();
    let bootstrap: Vec<_> = pbps_diff::diff(
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
    .clone()
    .into_iter()
    .map(|planned| planned.change)
    .collect();
    let reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap).unwrap();
    let at = |declaration: &str| {
        reconstruction
            .steps
            .iter()
            .position(|step| step.declaration == declaration)
            .unwrap()
    };
    let named = |relations: &[(&str, &str)]| {
        relations
            .iter()
            .map(|(schema, name)| ((*schema).to_owned(), (*name).to_owned()))
            .collect::<BTreeSet<_>>()
    };
    assert!(at("table app.n") < at("function app.f(integer)"));
    assert!(at("function app.f(integer)") < at("column app.n.g"));
    assert_eq!(
        reconstruction.after_creating(&named(&[("app", "n")])),
        Some(at("column app.n.g") + 1)
    );
    assert_eq!(
        reconstruction.after_creating(&named(&[("app", "plain"), ("app", "n")])),
        Some(at("column app.n.g") + 1),
        "the last of them"
    );
    assert_eq!(
        reconstruction.after_creating(&named(&[("app", "plain")])),
        Some(at("table app.plain") + 1)
    );
    assert_eq!(
        reconstruction.after_creating(&named(&[("app", "absent")])),
        None,
        "a relation no step creates"
    );
}

/// Names made resolvable between two steps, as a fill's chained shapes are,
/// count as later for a module compiled before them and not for one after
/// (#1778).
#[test]
fn names_created_between_steps_are_later_only_for_modules_before_them() {
    let mut reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap()).unwrap();
    let view = ObjectIdentity {
        class: "pg_class".into(),
        name: vec!["app".into(), "v".into()],
        signature: Vec::new(),
    };
    let shape = (Nameable::Type, "ext".to_owned(), "d".to_owned());
    let later = |reconstruction: &Reconstruction| {
        reconstruction
            .later_names(&view)
            .unwrap()
            .contains(&(Nameable::Type, "ext", "d"))
    };
    assert!(!later(&reconstruction), "nothing is recorded yet");
    let v = reconstruction
        .steps
        .iter()
        .position(|step| step.declaration == "view app.v")
        .unwrap();
    reconstruction.created_before(v, vec![shape.clone()]);
    assert!(!later(&reconstruction), "created before the view compiled");
    reconstruction.created_before(v + 1, vec![shape]);
    assert!(later(&reconstruction), "created after the view compiled");
}

/// A generated expression's own installation step decides which names
/// arrived later, including when ADD COLUMN follows the prefix CREATE
/// (DEC-1503.1, #1778). A plain column or absent table has no such step.
#[test]
fn a_generated_columns_later_names_follow_its_expression_step() {
    use pbps_model::{Column, Generated, Module, Schema};
    let mut generated = Column::new("integer".parse().unwrap());
    generated.value_source = pbps_model::ValueSource::Generated(Generated {
        expression: "app.f(id)".into(),
        stored: true,
    });
    let mut n = Table::default();
    n.columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    n.columns.insert("g".into(), generated);
    let mut schema = Schema::default();
    schema.tables.insert(TableName::new("app", "n"), n);
    for (name, definition) in [
        (
            "app.f(integer)",
            "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
        ),
        ("app.v", "SELECT 1 AS x"),
    ] {
        let kind = if name.contains('(') {
            ModuleKind::Function
        } else {
            ModuleKind::View
        };
        schema.modules.insert(
            name.parse().unwrap(),
            Module {
                kind,
                description: None,
                definition: definition.into(),
            },
        );
    }
    let ids = pbps_diff::resolve(
        &schema,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "1778-test".into(),
            today: "2026-10-17".into(),
        },
    )
    .unwrap()
    .ids;
    let empty = Schema::default();
    let bootstrap: Vec<_> = pbps_diff::diff(
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
    .clone()
    .into_iter()
    .map(|planned| planned.change)
    .collect();
    let mut reconstruction = build_reconstruction(&crate::Postgres::new(), &bootstrap).unwrap();
    let expression = reconstruction
        .steps
        .iter()
        .position(|step| step.declaration == "column app.n.g")
        .unwrap();
    let later: Vec<_> = reconstruction.steps[expression + 1..]
        .iter()
        .flat_map(|step| step.names.clone())
        .collect();
    let names = reconstruction
        .later_names_generated("app", "n", "g")
        .unwrap();
    for (kind, schema, name) in &later {
        assert!(
            names.contains(&(*kind, schema.as_str(), name.as_str())),
            "{later:?}"
        );
    }
    assert!(
        reconstruction
            .later_names_generated("app", "n", "id")
            .is_none()
    );
    assert!(
        reconstruction
            .later_names_generated("app", "x", "g")
            .is_none()
    );
    reconstruction.created_before(
        expression + 1,
        vec![(Nameable::Type, "ext".into(), "d".into())],
    );
    assert!(
        reconstruction
            .later_names_generated("app", "n", "g")
            .unwrap()
            .contains(&(Nameable::Type, "ext", "d")),
        "a fill after the expression"
    );
}

fn build_reconstruction(
    pg: &crate::Postgres,
    changes: &[pbps_model::Change],
) -> Result<Reconstruction, crate::resolver::reconstruct::ReconstructError> {
    Reconstruction::new(
        pg,
        changes,
        &crate::resolver::reconstruct::fixture_ids(changes),
    )
}

#[test]
fn placeholder_bindings_are_measured_at_the_real_expression_and_fill_uses_the_complete_shape() {
    let mut column = Column::new("integer".parse().unwrap());
    column.value_source = ValueSource::Generated(pbps_model::Generated {
        expression: "app.f(z)".into(),
        stored: true,
    });
    let mut table = Table::default();
    table.columns.insert("g".into(), column);
    table
        .columns
        .insert("z".into(), Column::new("integer".parse().unwrap()));
    let changes = vec![
        Change::CreateTable {
            uid: Uid::generate(UidKind::Table),
            name: TableName::new("app", "a"),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        },
        module(
            "app.f(integer)",
            ModuleKind::Function,
            "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
        ),
    ];
    let pg = crate::Postgres::new().with_server_version(180_000);
    let reconstruction = build_reconstruction(&pg, &changes).unwrap();
    let expression = reconstruction
        .steps
        .iter()
        .position(|s| s.declaration == "generation of app.a.g")
        .unwrap();
    assert_eq!(
        reconstruction
            .steps
            .iter()
            .filter(|s| s.generated.contains(&"g".into()))
            .count(),
        1
    );
    assert!(
        reconstruction
            .later_names_generated("app", "a", "g")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reconstruction.after_creating(&[("app".into(), "a".into())].into()),
        Some(expression + 1)
    );
    // With unknown capabilities the old whole-table hold still owns the
    // generated binding; no placeholder or SET EXPRESSION is emitted.
    let unknown = build_reconstruction(&crate::Postgres::new(), &changes).unwrap();
    assert!(
        !unknown
            .steps
            .iter()
            .any(|s| s.declaration == "generation of app.a.g")
    );
}

#[test]
fn scratch_transitive_readers_share_lexical_and_declared_dependency_closures() {
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut column = Column::new("integer".parse().unwrap());
    column.value_source = ValueSource::Generated(pbps_model::Generated {
        expression: "app.f(id)".into(),
        stored: true,
    });
    table.columns.insert("g".into(), column);
    table
        .columns
        .insert("z".into(), Column::new("integer".parse().unwrap()));
    for declared in [false, true] {
        let changes = vec![
            Change::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: TableName::new("app", "a"),
                table: Box::new(table.clone()),
                beside_default: None,
                moves_at_most: None,
            },
            module(
                "app.g(integer)",
                ModuleKind::Function,
                "(integer) RETURNS integer LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT z FROM app.a LIMIT 1; SELECT $1 * 2; END",
            ),
            module(
                "app.f(integer)",
                ModuleKind::Function,
                if declared {
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2"
                } else {
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN app.g($1)"
                },
            ),
        ];
        let deps = if declared {
            [(
                "app.f(integer)".parse().unwrap(),
                ["app.g(integer)".parse().unwrap()].into(),
            )]
            .into()
        } else {
            Default::default()
        };
        let ids = crate::resolver::reconstruct::fixture_ids(&changes);
        let pg = crate::Postgres::new().with_server_version(180_000);
        let reconstruction = Reconstruction::with_dependencies(&pg, &changes, &ids, &deps).unwrap();
        let sql = reconstruction
            .steps
            .iter()
            .flat_map(|s| &s.statements)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sql.contains("SET EXPRESSION"), "{sql}");
        // An unrelated reader does not make the generation's callee a reader.
        if declared {
            let reconstruction =
                Reconstruction::with_dependencies(&pg, &changes, &ids, &Default::default())
                    .unwrap();
            assert!(
                reconstruction
                    .steps
                    .iter()
                    .flat_map(|s| &s.statements)
                    .all(|sql| !sql.contains("SET EXPRESSION"))
            );
        }
    }
}

#[test]
fn row_security_bootstraps_keep_the_namespace_without_claiming_policy_bindings() {
    use pbps_model::{PolicyRoles, RowPolicy, RowSecurity, RowSecurityOrigin};
    let dialect = crate::Postgres::new();
    let plain = build_reconstruction(&dialect, &bootstrap()).unwrap();
    let statements = |recipe: &Reconstruction| {
        recipe
            .steps
            .iter()
            .flat_map(|step| step.statements.clone())
            .collect::<Vec<_>>()
    };
    let expected = statements(&plain);
    for (enabled, forced, with_policy) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        let mut changes = bootstrap();
        let Change::CreateTable {
            uid, name, table, ..
        } = &mut changes[0]
        else {
            panic!("the fixture starts with a table");
        };
        let uid = uid.clone();
        let name = name.clone();
        let flags = RowSecurity { enabled, forced };
        table.row_security = flags;
        let policy = RowPolicy {
            to: PolicyRoles::Roles(["recipient_not_reproduced_on_scratch".into()].into()),
            using: Some("app.f(c::integer) > 0".into()),
            ..Default::default()
        };
        if with_policy {
            table.row_policies.insert("visible".into(), policy.clone());
            changes.push(Change::CreatePolicy {
                table: name.clone(),
                name: "visible".into(),
                policy,
                origin: RowSecurityOrigin::Created,
            });
        }
        if !flags.is_default() {
            changes.push(Change::SetRowSecurity {
                uid,
                table: name,
                from: RowSecurity::default(),
                to: flags,
                origin: RowSecurityOrigin::Created,
            });
        }
        let recipe = build_reconstruction(&dialect, &changes).unwrap();
        assert_eq!(statements(&recipe), expected);
        assert_eq!(recipe.len(), plain.len());
    }
}
