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
                name: "c".into(),
                descending: false,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("c > app.f(1)".into()),
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

/// A table is created bare; its keys follow every table, the modules follow
/// the keys in the differ's order, and each expression and trigger comes
/// last, when every routine it can name exists. A grant changes no binding
/// and is not reproduced.
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
/// index. A view is found by name; a routine only by the overload its step
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
        [(Nameable::Relation, "v"), (Nameable::Index, "ix")]
            .into_iter()
            .collect()
    );
    // Another overload of the same name was not compiled at that step.
    assert!(reconstruction.later_names(&routine("text")).is_none());
    assert_eq!(
        reconstruction.later_names(&relation("v")).unwrap(),
        [(Nameable::Index, "ix")].into_iter().collect()
    );
    // A table is not a module.
    assert!(reconstruction.later_names(&relation("t")).is_none());
}

/// A later object could only have taken a binding it can be resolved as:
/// an index never a routine call, a routine never a relation, while a
/// relation's row type can take a one-argument call.
#[test]
fn a_later_object_shadows_only_what_it_could_be_resolved_as() {
    let target = |class: &str, arguments: usize| ObjectIdentity {
        class: class.into(),
        name: vec!["app".into(), "f".into()],
        signature: (0..arguments)
            .map(|_| ObjectIdentity {
                class: "pg_type".into(),
                name: vec!["pg_catalog".into(), "text".into()],
                signature: Vec::new(),
            })
            .collect(),
    };
    assert!(Nameable::Index.shadows(&target("pg_class", 0)));
    assert!(!Nameable::Index.shadows(&target("pg_proc", 1)));
    assert!(!Nameable::Index.shadows(&target("pg_type", 0)));
    assert!(Nameable::Routine.shadows(&target("pg_proc", 0)));
    assert!(Nameable::Routine.shadows(&target("pg_type", 0)));
    assert!(!Nameable::Routine.shadows(&target("pg_class", 0)));
    assert!(Nameable::Relation.shadows(&target("pg_class", 0)));
    assert!(Nameable::Relation.shadows(&target("pg_type", 0)));
    // A relation's row type can take a one-argument call as a cast, never a
    // call of another arity.
    assert!(Nameable::Relation.shadows(&target("pg_proc", 1)));
    assert!(!Nameable::Relation.shadows(&target("pg_proc", 0)));
    assert!(!Nameable::Relation.shadows(&target("pg_proc", 2)));
    assert!(!Nameable::Relation.shadows(&target("column", 0)));
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
