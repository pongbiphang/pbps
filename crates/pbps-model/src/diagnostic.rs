//! Rejected declaration sources travel beside the recovery schema, never in
//! checked columns or persisted state. Consumers still match `ValueSource`
//! exhaustively; this view only supplies the information recovery retained.

use crate::ColumnName;
use std::collections::BTreeMap;

use crate::{Column, TableName, ValueSource};

pub type SourceConflicts = BTreeMap<TableName, BTreeMap<ColumnName, Vec<ValueSource>>>;

/// A declaration owner, outside semantic equality and saved artifacts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Declaration {
    Extension(crate::ObjectLocalName),
    Table(TableName),
    Module(crate::ModuleId),
    Type(crate::TypeRef),
    Search(crate::SearchRef),
    Sequence(TableName),
    Role(crate::RoleName),
}

/// What every consumer asks of a declaration's owner, answered here once per
/// kind (#2878, DEC-2878.3). A consumer that matched on the kind to learn one
/// of these was a list someone had to remember; a kind added to the enum now
/// stops compiling in this block alone.
impl Declaration {
    /// The kind of identity this declaration holds and the name it spells in
    /// the identity tree, or `None` for a kind that holds no identity.
    pub fn identity(&self) -> Option<(crate::UidKind, String)> {
        use crate::UidKind;
        match self {
            Self::Table(n) => Some((UidKind::Table, n.raw().to_string())),
            Self::Role(n) => Some((UidKind::Role, n.raw().to_owned())),
            Self::Type(n) => Some((UidKind::Type, n.raw().to_string())),
            Self::Extension(n) => Some((UidKind::Extension, n.raw().to_owned())),
            Self::Search(n) => Some((UidKind::Search, n.raw().to_string())),
            // Sequences take identities with the part that creates one
            // (#2721); a module is identified by its name.
            Self::Module(_) | Self::Sequence(_) => None,
        }
    }

    /// The uid the identity file holds for what this declaration names.
    ///
    /// Through an [`IdsIndex`](crate::IdsIndex) and not a scan of the file:
    /// a caller asks once per declaration, and a scan per call made such a
    /// loop quadratic in the schema (#1870).
    pub fn uid_in<'a>(&self, ids: &crate::IdsIndex<'a>) -> Option<&'a crate::Uid> {
        match self {
            Self::Table(n) => ids.table_uid(n),
            Self::Role(n) => ids.role_uid(n),
            Self::Type(n) => ids.type_uid(n),
            Self::Extension(n) => ids.extension_uid(n),
            Self::Search(n) => ids.search_uid(n),
            Self::Module(_) | Self::Sequence(_) => None,
        }
    }

    /// The declaration as a message names it: what it is, then its name,
    /// escaped once.
    pub fn described(&self) -> String {
        match self {
            Self::Table(t) => format!("table {}", t.shown()),
            Self::Module(m) => format!("module {}", m.shown()),
            Self::Extension(e) => format!("extension {}", e.shown()),
            Self::Type(t) => format!("type {}", t.shown()),
            // A text search reference says which of the two it is itself.
            Self::Search(s) => s.shown().to_string(),
            Self::Sequence(s) => format!("sequence {}", s.shown()),
            Self::Role(r) => format!("role {}", r.shown()),
        }
    }
}

/// The sources actually supplied for each column of one diagnostic table.
/// With no recovery entry, the checked column supplies its sole source.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColumnSources<'a> {
    conflicts: Option<&'a BTreeMap<ColumnName, Vec<ValueSource>>>,
}

impl<'a> ColumnSources<'a> {
    pub fn new(conflicts: Option<&'a BTreeMap<ColumnName, Vec<ValueSource>>>) -> Self {
        Self { conflicts }
    }

    pub fn get<'b>(&'b self, name: &str, column: &'b Column) -> &'b [ValueSource] {
        self.conflicts
            .and_then(|conflicts| conflicts.get(name))
            .map(Vec::as_slice)
            .unwrap_or_else(|| std::slice::from_ref(&column.value_source))
    }

    pub fn conflicted_columns(self) -> impl Iterator<Item = &'a ColumnName> {
        self.conflicts.into_iter().flat_map(|c| c.keys())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdsFile, ModuleId, SearchKind, SearchRef, TypeRef, Uid, UidKind};

    fn uid(text: &str) -> Uid {
        text.parse().unwrap()
    }

    /// One declaration of every kind, and an identity file that holds a uid
    /// for each of the kinds that can have one.
    fn every_kind() -> (Vec<Declaration>, IdsFile) {
        let table = TableName::new("app", "orders");
        let ty = TypeRef::new("app", "mood").unwrap();
        let search = SearchRef::new(SearchKind::Dictionary, "app.mood".parse().unwrap());
        let mut ids = IdsFile::default();
        ids.tables.insert(uid("t_aaaaaa"), table.clone());
        ids.roles
            .insert(uid("r_aaaaaa"), crate::RoleName::new("reader"));
        ids.types.insert(uid("y_aaaaaa"), ty.clone());
        ids.extensions
            .insert(uid("e_aaaaaa"), crate::ObjectLocalName::new("citext"));
        ids.search.insert(uid("s_aaaaaa"), search.clone());
        let declarations = vec![
            Declaration::Extension(crate::ObjectLocalName::new("citext")),
            Declaration::Table(table.clone()),
            Declaration::Module(ModuleId::Named(TableName::new("app", "v"))),
            Declaration::Type(ty.clone()),
            Declaration::Search(search),
            Declaration::Role(crate::RoleName::new("reader")),
            // A sequence of the table's name, so that finding none for it is
            // not just an empty map.
            Declaration::Sequence(table.clone()),
        ];
        (declarations, ids)
    }

    #[test]
    fn a_kind_that_holds_an_identity_is_found_in_the_file_and_one_that_does_not_is_not() {
        let (declarations, ids) = every_kind();
        let mut with_identity = 0;
        for declaration in &declarations {
            // The two answers are one fact: a kind names an identity exactly
            // when the file can hold a uid for it.
            assert_eq!(
                declaration.identity().is_some(),
                declaration.uid_in(&ids.index()).is_some(),
                "{declaration:?}"
            );
            with_identity += usize::from(declaration.identity().is_some());
        }
        assert_eq!(with_identity, 5);
        assert_eq!(declarations[4].uid_in(&ids.index()), Some(&uid("s_aaaaaa")));
        // Negative: a module and a sequence hold none, though a table of each
        // one's name is held (a sequence takes its identity with the part that
        // creates it, #2721).
        ids_hold_nothing_for(&declarations[2], &ids);
        ids_hold_nothing_for(&declarations[6], &ids);
    }

    fn ids_hold_nothing_for(declaration: &Declaration, ids: &IdsFile) {
        assert_eq!(declaration.identity(), None);
        assert_eq!(declaration.uid_in(&ids.index()), None);
    }

    #[test]
    fn an_identity_is_the_kind_and_the_name_as_the_tree_spells_it() {
        let (declarations, ids) = every_kind();
        assert_eq!(
            declarations[1].identity(),
            Some((UidKind::Table, "app.orders".to_owned()))
        );
        assert_eq!(declarations[1].uid_in(&ids.index()), Some(&uid("t_aaaaaa")));
        assert_eq!(
            declarations[0].identity(),
            Some((UidKind::Extension, "citext".to_owned()))
        );
        assert_eq!(declarations[0].uid_in(&ids.index()), Some(&uid("e_aaaaaa")));
        // Negative: a name the file does not hold has an identity to take and
        // no uid yet; a role's uid is not found under a table of its name.
        let unknown = Declaration::Table(TableName::new("app", "reader"));
        assert!(unknown.identity().is_some());
        assert_eq!(unknown.uid_in(&ids.index()), None);
    }

    #[test]
    fn a_declaration_is_described_by_what_it_is_and_its_name_once() {
        let (declarations, _) = every_kind();
        let described: Vec<String> = declarations.iter().map(Declaration::described).collect();
        assert_eq!(
            described,
            [
                "extension citext",
                "table app.orders",
                "module app.v",
                "type app.mood",
                "text search dictionary `app.mood`",
                "role reader",
                "sequence app.orders",
            ]
        );
        // Negative: the text search reference says what it is itself, so the
        // kind is not written a second time in front of it.
        assert!(!described[4].starts_with("search"));
    }
}
