//! SQL Server's name slots (#1964): which namespace holds each kind of name,
//! and that two spellings are a catalog fact.

use pbps_dialect::slots::{CatalogFacts, Namespace, Same, SlotKey, same, slot};
use pbps_model::operation::{Address, NameKind, ObjectKey, module_key};
use pbps_model::{ModuleId, ModuleKind};
use pbps_mssql::Mssql;

/// Any key stands for the table: the dialect only scopes by it.
fn table() -> ObjectKey {
    module_key(&"dbo.t".parse::<ModuleId>().unwrap())
}

fn address(kind: NameKind) -> Address {
    Address {
        kind,
        schema: "dbo".into(),
        table: Some(table()),
        name: "x".into(),
        signature: None,
    }
}

#[test]
fn every_object_and_constraint_shares_the_schema_and_an_index_is_its_tables() {
    let schema = Namespace::Relations("dbo".into());
    for kind in [
        NameKind::Table,
        NameKind::Module(ModuleKind::Procedure),
        NameKind::Module(ModuleKind::Trigger),
        NameKind::Key,
        NameKind::Constraint,
    ] {
        assert_eq!(slot(&Mssql, &address(kind)).namespace, schema, "{kind:?}");
    }
    for kind in [NameKind::Index, NameKind::Column] {
        assert_eq!(
            slot(&Mssql, &address(kind)).namespace,
            Namespace::Table(table()),
            "{kind:?}"
        );
    }
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
