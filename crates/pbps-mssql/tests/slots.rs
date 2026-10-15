//! SQL Server's name slots (#1964): which namespace holds each kind of name,
//! and that two spellings are a catalog fact.

use pbps_dialect::slots::{CatalogFacts, Namespace, Same, SlotKey, TableSpace, same, slots};
use pbps_model::operation::{Address, NameKind, ObjectKey, module_key};
use pbps_model::{ModuleId, ModuleKind};
use pbps_mssql::Mssql;

/// Any key stands for the table: the dialect only scopes by it.
fn table() -> ObjectKey {
    module_key(&"dbo.t".parse::<ModuleId>().unwrap())
}

fn namespaces(kind: NameKind) -> Vec<Namespace> {
    let address = Address {
        kind,
        schema: "dbo".into(),
        table: Some(table()),
        name: "x".into(),
        signature: None,
    };
    slots(&Mssql, &address)
        .into_iter()
        .map(|s| s.namespace)
        .collect()
}

#[test]
fn every_object_and_constraint_shares_the_schema() {
    let schema = Namespace::Relations("dbo".into());
    for kind in [
        NameKind::Table,
        NameKind::Module(ModuleKind::Procedure),
        NameKind::Module(ModuleKind::Trigger),
        NameKind::Constraint,
    ] {
        assert_eq!(namespaces(kind), std::slice::from_ref(&schema), "{kind:?}");
    }
}

/// An index name is its table's, apart from its columns; a named key holds
/// its name among the schema's objects and among its table's indexes, which
/// is the index that backs it.
#[test]
fn an_index_is_its_tables_and_a_key_is_both() {
    assert_eq!(
        namespaces(NameKind::Index),
        [Namespace::Table(table(), TableSpace::Indexes)]
    );
    assert_eq!(
        namespaces(NameKind::Column),
        [Namespace::Table(table(), TableSpace::Columns)]
    );
    assert_eq!(
        namespaces(NameKind::Key),
        [
            Namespace::Table(table(), TableSpace::Indexes),
            Namespace::Relations("dbo".into())
        ]
    );
}

/// The collation decides, and only the catalog knows it: without the fact
/// two spellings are unknown, never "different".
#[test]
fn two_spellings_are_one_name_only_by_the_catalogs_word() {
    let key = |name: &str| SlotKey {
        namespace: Namespace::Relations("dbo".into()),
        name: name.into(),
    };
    let (a, b) = (key("Ab"), key("ab"));
    assert_eq!(same(&Mssql, None, &a, &b), Same::Unknown);
    let mut facts = CatalogFacts::default();
    facts.compared.extend([a.clone(), b.clone()]);
    assert_eq!(same(&Mssql, Some(&facts), &a, &b), Same::No);
    facts.alike.insert((a.clone(), b.clone()));
    assert_eq!(same(&Mssql, Some(&facts), &b, &a), Same::Yes);
}
