//! PostgreSQL's name slots (#1964): which namespace holds each kind of name,
//! and that two spellings are two names.

use pbps_dialect::slots::{Namespace, Same, SlotKey, same, slot};
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

#[test]
fn tables_views_indexes_and_keys_share_the_schemas_relation_namespace() {
    let relations = Namespace::Relations("app".into());
    for kind in [
        NameKind::Table,
        NameKind::Module(ModuleKind::View),
        NameKind::Index,
        NameKind::Key,
    ] {
        assert_eq!(
            slot(&Postgres::new(), &address(kind, "x", None)).namespace,
            relations,
            "{kind:?}"
        );
    }
    // A check, a foreign key, a column and a trigger are the table's own.
    for kind in [
        NameKind::Constraint,
        NameKind::Column,
        NameKind::Module(ModuleKind::Trigger),
    ] {
        assert_eq!(
            slot(&Postgres::new(), &address(kind, "x", None)).namespace,
            Namespace::Table(table()),
            "{kind:?}"
        );
    }
}

#[test]
fn overloaded_routines_are_told_apart_by_their_signatures() {
    let f = |signature: &str| {
        slot(
            &Postgres::new(),
            &address(NameKind::Module(ModuleKind::Function), "f", Some(signature)),
        )
    };
    assert_ne!(f("app.f(integer)"), f("app.f(text)"));
    // Not a relation: a function may share a table's name.
    assert_eq!(
        f("app.f(integer)").namespace,
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
