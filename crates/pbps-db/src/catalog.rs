//! What a catalog read returns, on every engine.
//!
//! The shapes here are the answers a connected command consumes: what a pull
//! found and could not say, what a row read could not read, how the engine
//! spells a declared value. Each engine fills them from its own catalog
//! queries — `pbps_mssql::catalog` and `pbps_pg::catalog` — and the SQL stays
//! there, beside the engine whose catalog it reads. Only the *meaning* of the
//! answer lives here, once, which is what lets `pbps-cli` ask one question of
//! whichever engine a connection is to (DECISIONS 417).
//!
//! The same split [`crate::ledger`] made first: the types and the errors are
//! dialect-agnostic, the statements are each dialect's. A field one engine
//! never fills is documented on that field — an empty inventory from an engine
//! that keeps no such inventory is not "nothing found", and the caller has to
//! be able to read the difference.

use pbps_model::{ObjectName, RowConflict, RowKey, Schema, TableName};

use crate::DbError;

/// The result of a pull: the schema, plus everything that could not be said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pulled {
    pub schema: Schema,
    /// Facts about the database the model cannot express, rendered, in the
    /// order a reader should see them. Never empty silence: the caller must
    /// show these, because each one is a difference that would otherwise
    /// surface as phantom drift or a destructive plan later.
    pub warnings: Vec<String>,
    /// Permissions the model cannot hold and a drift check must not call
    /// clean. A grant `WITH GRANT OPTION`, a DENY, a column-level grant, a
    /// permission outside the closed set, a grant on an object the model does
    /// not hold: each is left out of the role's set — folded in or merely
    /// warned about, `verify` compared the sets that remained equal and said
    /// "no drift" about a role that had changed — and reported here for the
    /// caller to put beside the other unexpressible differences
    /// (DECISIONS 95, 97).
    pub unexpressible: Vec<Unexpressible>,
    /// Unsupported facts associated with an object. Callers use its typed target
    /// to distinguish a limitation inside the managed set (unexpressible
    /// drift) from one on somebody else's object.
    ///
    /// The same argument as `unexpressible` above, on the other half of the
    /// model: one is about a role's permissions, the other about unsupported
    /// object features, and neither may be folded into the comparison or dropped
    /// from it.
    pub limitations: Vec<Limitation>,
    /// Modules the database has that pbps cannot manage: on SQL Server a CLR
    /// object, one created `WITH ENCRYPTION`, or one whose stored text does
    /// not have the shape the emitter can reproduce.
    ///
    /// Separate from `warnings` because these are not defects in the pull —
    /// they are an inventory of what is left alone, and the user needs the
    /// count and the names.
    ///
    /// PostgreSQL also inventories modules it omits, including unsupported
    /// catalog kinds and definitions or identities the declaration cannot hold.
    pub unmanaged_modules: Vec<UnmanagedModule>,
}

/// One permission the model cannot hold, and enough about it for the caller
/// to decide whether it is any of this project's business.
///
/// The securable travels with it. Filtered by role alone, a `DENY` or a
/// column-level grant on somebody else's table stopped every command — while
/// the *plain* grant on that same table was dropped by `scope`, whose recorded
/// reason is that it is that table's business (DECISIONS 176).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unexpressible {
    pub role: String,
    /// The securable, where the permission names one a declaration could.
    /// `None` for a permission on the database itself and for a class the
    /// model cannot name at all — neither is any object's business, and a
    /// role that gained one has changed (DECISIONS 105).
    pub target: Option<pbps_model::GrantTarget>,
    /// The difference, already rendered.
    pub what: String,
}

/// One fact the model cannot hold, with the namespace needed to scope it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Limitation {
    pub target: LimitationTarget,
    pub detail: String,
}

/// Relations share a namespace; routines include their signature and triggers
/// include their parent. A name-only table filter cannot distinguish them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LimitationTarget {
    Relation(TableName),
    /// A module in a shared object namespace, even when its declaration also
    /// carries a trigger parent. The unreadable body cannot supply that parent.
    SharedModule(ObjectName),
    Module(pbps_model::ModuleId),
    /// No declaration can name this identity. Keep the diagnostic, without
    /// attributing it to a different object that happens to share its name.
    UnnameableModule(ObjectName),
}

impl LimitationTarget {
    pub fn module(id: pbps_model::ModuleId) -> Self {
        match id {
            pbps_model::ModuleId::Named(name) => Self::Relation(name),
            id @ (pbps_model::ModuleId::Routine(_) | pbps_model::ModuleId::Trigger { .. }) => {
                Self::Module(id)
            }
        }
    }

    /// Whether a declaration names this module, without discarding overloads
    /// or trigger parents. An unnameable identity cannot be declared.
    pub fn matches_module(&self, id: &pbps_model::ModuleId) -> bool {
        match self {
            Self::Relation(name) => matches!(id, pbps_model::ModuleId::Named(n) if n == name),
            Self::SharedModule(name) => *name == id.object_name(),
            Self::Module(module) => module == id,
            Self::UnnameableModule(_) => false,
        }
    }

    pub fn object_name(&self) -> ObjectName {
        match self {
            Self::Relation(name) | Self::SharedModule(name) | Self::UnnameableModule(name) => {
                name.clone()
            }
            Self::Module(id) => id.object_name(),
        }
    }
}

impl std::fmt::Display for LimitationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relation(name) | Self::SharedModule(name) | Self::UnnameableModule(name) => {
                name.fmt(f)
            }
            Self::Module(id) => id.fmt(f),
        }
    }
}

/// One module the database has and `pbps` does not manage.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnmanagedModule {
    pub kind: &'static str,
    /// Kept structured, including routine arguments and trigger parent, so a
    /// same-name object cannot hide this entry from the unmanaged policy.
    /// SQL Server modules use `SharedModule` for its shared object namespace.
    pub target: LimitationTarget,
    /// Why it is not managed, in the operator's words.
    pub why: String,
}

/// Why a table's rows could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum RowsError {
    #[error("{table}: its rows cannot be read back — {why}")]
    Unreadable { table: TableName, why: String },

    #[error("{table}: reading its rows back failed: {source}")]
    Read {
        table: TableName,
        // Boxed so the error is not larger than every `Ok` it travels beside.
        #[source]
        source: Box<DbError>,
    },

    /// The engine sent a value the mapping cannot hold — an integer column
    /// whose text does not parse, say. A bug in the engine's row reader, not
    /// bad data.
    #[error("{table}.{column}: the engine sent `{text}`, which is not a {kind}")]
    BadValue {
        table: TableName,
        column: String,
        text: String,
        kind: &'static str,
    },

    #[error(transparent)]
    Dialect(#[from] pbps_dialect::DialectError),
}

/// What the catalog calls a table and its key column *now*, where the plan
/// about to be checked renames them.
///
/// The spelling checks run before a statement of the plan has run, so the
/// database still has the old names — and the one query there that names an
/// object rather than converting a literal, the key column's collation, found
/// nothing under the declared name and fell back to the database default
/// without saying so. Absent entries mean "as declared", which is right for
/// every table a plan does not rename and for one it has yet to create
/// (DECISIONS 148).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogued {
    pub table: Option<TableName>,
    pub key_column: Option<String>,
    /// The key column's collation, schema and name, where it has one that is
    /// not the database default. `None` is not "the default collation" as a
    /// value — it is "nothing to write", which leaves the comparison to
    /// whatever the expression's own collation is, and that is the right
    /// answer for a table this plan is about to create: the emitter writes no
    /// `COLLATE`, so its column will be created with the database's default.
    ///
    /// Read from the catalog by the PostgreSQL spelling check itself, under
    /// the names the other two fields carry; SQL Server asks its collation
    /// question inside the query and leaves this `None`.
    pub key_collation: Option<(String, String)>,
}

/// Every declared table's catalog names, keyed by the declared table name.
pub type CatalogNames = std::collections::BTreeMap<TableName, Catalogued>;

/// One declared spelling the engine reads back differently, or cannot read
/// at all (DECISIONS 101).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misspelt {
    pub table: TableName,
    pub key: RowKey,
    /// `None` for the key itself.
    pub column: Option<String>,
    pub declared: String,
    /// The column's type, as the engine was asked to read the text.
    pub ty: String,
    /// What the engine reads back; `None` when it cannot convert the text.
    pub canonical: Option<String>,
}

/// What the engine says about the declared spellings of every table that
/// declares rows: the ones it would not read back as written, and the keys
/// it reads as one row.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spellings {
    /// Declared texts the engine reads back differently, or not at all.
    pub misspelt: Vec<Misspelt>,
    /// Two declared keys the engine reads as one row (DECISIONS 106).
    pub conflicts: Vec<RowConflict>,
}
