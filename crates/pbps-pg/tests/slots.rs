//! PostgreSQL's name slots (#1964): which namespace holds each kind of name,
//! and that two spellings are two names.

use pbps_dialect::slots::{Namespace, Same, SlotKey, TableSpace, same, slots};
use pbps_model::operation::{Address, NameKind, ObjectKey, module_key};
use pbps_model::{ModuleId, ModuleKind};
use pbps_pg::Postgres;

/// Any key stands for the table: the dialect only scopes by it.
fn table() -> ObjectKey {
    module_key(&"app.t".parse::<ModuleId>().unwrap())
}

fn address(kind: NameKind, name: &str, signature: Option<&str>) -> Address {
    Address {
        kind,
        schema: "app".into(),
        table: Some(table()),
        name: name.into(),
        signature: signature.map(Into::into),
    }
}

fn namespaces(kind: NameKind) -> Vec<Namespace> {
    slots(&Postgres::new(), &address(kind, "x", None))
        .into_iter()
        .map(|s| s.namespace)
        .collect()
}

#[test]
fn tables_views_and_indexes_share_the_schemas_relation_namespace() {
    let relations = Namespace::Relations("app".into());
    for kind in [
        NameKind::Table,
        NameKind::Module(ModuleKind::View),
        NameKind::Index,
    ] {
        assert_eq!(
            namespaces(kind),
            std::slice::from_ref(&relations),
            "{kind:?}"
        );
    }
    // A key is a constraint backed by an index: its name is in the relation
    // namespace and among its table's constraints.
    assert_eq!(
        namespaces(NameKind::Key),
        [
            relations,
            Namespace::Table(table(), TableSpace::Constraints)
        ]
    );
}

/// A table keeps its columns, its constraints and its triggers apart: a
/// column and a check may share a name.
#[test]
fn a_tables_columns_constraints_and_triggers_are_separate_namespaces() {
    for (kind, space) in [
        (NameKind::Constraint, TableSpace::Constraints),
        (NameKind::Column, TableSpace::Columns),
        (NameKind::Module(ModuleKind::Trigger), TableSpace::Triggers),
    ] {
        assert_eq!(
            namespaces(kind),
            [Namespace::Table(table(), space)],
            "{kind:?}"
        );
    }
}

#[test]
fn overloaded_routines_are_told_apart_by_their_signatures() {
    let f = |signature: &str| {
        slots(
            &Postgres::new(),
            &address(NameKind::Module(ModuleKind::Function), "f", Some(signature)),
        )
    };
    assert_ne!(f("app.f(integer)"), f("app.f(text)"));
    // Not a relation: a function may share a table's name.
    assert_eq!(
        f("app.f(integer)")[0].namespace,
        Namespace::Routines("app".into())
    );
}

#[test]
fn two_spellings_are_two_names_without_any_catalog_fact() {
    let key = |name: &str| SlotKey {
        namespace: Namespace::Relations("app".into()),
        name: name.into(),
    };
    assert_eq!(
        same(&Postgres::new(), None, &key("ab"), &key("Ab")),
        Same::No
    );
    assert_eq!(
        same(&Postgres::new(), None, &key("ab"), &key("ab")),
        Same::Yes
    );
}
