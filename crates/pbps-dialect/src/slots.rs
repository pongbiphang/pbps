//! Name slots: the namespace that holds a name, and whether two spellings are
//! one name (#1776 slice S1b-ii, DEC-1964.1).
//!
//! `pbps_model::operation::names` lists the names each step claims and
//! releases, as addresses: a kind, a schema, a table, a name. Which addresses
//! compete is the engine's to say. PostgreSQL keeps tables, views, sequences
//! and indexes in one relation namespace per schema; SQL Server keeps every
//! object and constraint in one per schema, and an index name per table.
//! [`slot`] maps an address to its slot from the dialect's own answers, so no
//! namespace is hard-coded twice.
//!
//! Two spellings in one namespace are one name when the engine compares them
//! equal. PostgreSQL compares identifiers exactly. SQL Server compares them
//! under the database's collation, which a plan computed offline does not
//! know (SPEC 7.3): there the answer is a catalog fact, and without it the
//! check is skipped and reported, never read as "different names".

use std::collections::BTreeSet;

use pbps_model::ModuleKind;
use pbps_model::operation::{Address, NameKind, ObjectKey};

use crate::Dialect;

/// The namespace a name is unique in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Namespace {
    /// A schema's objects that share one namespace with its tables.
    Relations(String),
    /// A schema's routines, where they do not share the tables' namespace.
    Routines(String),
    /// One table's names, by the table's identity.
    Table(ObjectKey),
    /// Roles, which belong to no schema.
    Roles,
}

/// One name in one namespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotKey {
    pub namespace: Namespace,
    pub name: String,
}

impl std::fmt::Display for SlotKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Namespace::Relations(schema) => write!(f, "{schema}.{}", self.name),
            Namespace::Routines(schema) => write!(f, "{schema}.{} (routine)", self.name),
            Namespace::Table(table) => write!(f, "{} on {table:?}", self.name),
            Namespace::Roles => write!(f, "role {}", self.name),
        }
    }
}

/// Whether two slots are one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Same {
    Yes,
    No,
    /// The engine compares the two spellings under a rule this plan has no
    /// fact for.
    Unknown,
}

/// What a connected plan's catalog reads say about names: who holds a name
/// the declarations do not account for, and which spellings it compared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogFacts {
    pub occupants: Vec<Occupant>,
    /// Every slot the catalog was asked to compare.
    pub compared: BTreeSet<SlotKey>,
    /// The pairs among them it reports alike.
    pub alike: BTreeSet<(SlotKey, SlotKey)>,
}

/// A name the catalog holds for an object the declarations do not name: a
/// carried sequence or index, an adopted or fallback default constraint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Occupant {
    pub slot: SlotKey,
    /// The catalog's word for it, for the report.
    pub kind: String,
    /// The table it belongs to, by the baseline's name: it goes where that
    /// table goes and leaves when that table is dropped.
    pub owner: Option<pbps_model::TableName>,
    /// The owner's column it belongs to, by the baseline's name: an owned
    /// sequence, or a column's default constraint.
    pub column: Option<String>,
}

/// The slot `address` takes, by the dialect's namespace answers.
pub fn slot(dialect: &dyn Dialect, address: &Address) -> SlotKey {
    let schema = || Namespace::Relations(address.schema.clone());
    // A per-table name whose table no identity records has no table to be
    // scoped by; its schema is the narrowest scope there is.
    let table = || address.table.clone().map_or_else(schema, Namespace::Table);
    let mut name = address.name.clone();
    let namespace = match address.kind {
        NameKind::Table => schema(),
        NameKind::Module(kind) if dialect.shares_namespace_with_tables(kind) => schema(),
        NameKind::Module(ModuleKind::Trigger) => table(),
        NameKind::Module(kind) => {
            if dialect.overloads(kind)
                && let Some(signature) = &address.signature
            {
                name.clone_from(signature);
            }
            Namespace::Routines(address.schema.clone())
        }
        NameKind::Index if dialect.indexes_share_namespace_with_tables() => schema(),
        NameKind::Key
            if dialect.indexes_share_namespace_with_tables()
                || dialect.constraints_share_namespace_with_tables() =>
        {
            schema()
        }
        NameKind::Constraint if dialect.constraints_share_namespace_with_tables() => schema(),
        NameKind::Index | NameKind::Key | NameKind::Constraint | NameKind::Column => table(),
        NameKind::Role => Namespace::Roles,
    };
    SlotKey { namespace, name }
}

/// Whether `a` and `b` are one name: equal slots are; otherwise the engine's
/// rule, or the catalog's answer where the rule is a collation.
pub fn same(dialect: &dyn Dialect, facts: Option<&CatalogFacts>, a: &SlotKey, b: &SlotKey) -> Same {
    if a == b {
        return Same::Yes;
    }
    // Two tables' own namespaces are two namespaces, whatever their names.
    if matches!((&a.namespace, &b.namespace), (Namespace::Table(x), Namespace::Table(y)) if x != y)
        || std::mem::discriminant(&a.namespace) != std::mem::discriminant(&b.namespace)
    {
        return Same::No;
    }
    if dialect.compares_names_exactly() {
        return Same::No;
    }
    match facts {
        Some(facts) if facts.compared.contains(a) && facts.compared.contains(b) => {
            if facts.alike.contains(&(a.clone(), b.clone()))
                || facts.alike.contains(&(b.clone(), a.clone()))
            {
                Same::Yes
            } else {
                Same::No
            }
        }
        _ => Same::Unknown,
    }
}
