use super::*;
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
/// index, measured from the first overload of a routine's name. The last
/// module has only the indexes after it, and a name no module has, nothing.
#[test]
fn later_names_are_what_became_nameable_after_the_module() {
    let reconstruction = Reconstruction::new(&crate::Postgres::new(), &bootstrap()).unwrap();
    assert_eq!(
        reconstruction.later_names("app", "f", true).unwrap(),
        ["v", "ix"].into_iter().collect()
    );
    assert_eq!(
        reconstruction.later_names("app", "v", false).unwrap(),
        ["ix"].into_iter().collect()
    );
    // A view is not a routine of the same name, and a table is not a module.
    assert!(reconstruction.later_names("app", "v", true).is_none());
    assert!(reconstruction.later_names("app", "t", false).is_none());
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
