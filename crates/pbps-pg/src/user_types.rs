//! The connected read of a user-defined type a column names, and its pin
//! (#1523, plan step 2).
//!
//! pbps never creates, alters or drops a type. A column may name one only by
//! reference (`app.mood`), so before a plan relies on that name it has to know
//! what the name is *now*, and a saved plan has to notice that it is no longer
//! the same type. This module does both reads and nothing else: it is not yet
//! called by `plan`, `apply`, introspection or emit.
//!
//! # Three outcomes that must not be confused
//!
//! A lookup ends in one of five results ([`TypeRead`]), and a caller can no
//! more take one for another than it can take a `Some` for a `None`:
//!
//! - **absent**: no `pg_type` row by that schema and name;
//! - **not defined**: a shell type (`typisdefined = false`), declared by
//!   `CREATE TYPE name;` and never given a body. An enum with no labels is
//!   present, not empty;
//! - **unreadable**: the deploying role has no `USAGE` on the type or its
//!   schema, or the read itself failed. The reason is kept, and a failed read
//!   is never reported as an absent type;
//! - **not admitted**: the type exists and is readable but is not an enum, a
//!   domain or a standalone composite (a base or extension type, a range, a
//!   pseudo-type, a table's or view's row type);
//! - **present**: an admitted type, read in full.
//!
//! # Lookup by name, never by cast
//!
//! `to_regtype('s.t')` and `'s.t'::regtype` raise *permission denied for
//! schema* when the role has no `USAGE` on `s`, and so does
//! `has_type_privilege('s.t'::regtype, …)` (measured on 16.15 and 18.6). The
//! `pg_type` row itself stays readable. So the lookup joins `pg_type` to
//! `pg_namespace` by name and asks about privilege by OID, which is the only
//! way to tell "cannot use it" from "not there".
//!
//! # Trust (DEC-1663.1)
//!
//! Nothing here casts a value to the type or evaluates it: a domain's CHECK
//! and DEFAULT are read as the catalog's text and no more. What a later
//! `ADD COLUMN` over the type would run is a different matter, because that
//! runs a domain's CHECK once per existing row as the deployer. So the read
//! names every role that wrote what the type is made of or calls: the type's
//! owner, the owner of every type nested in it, and the owner of every object
//! its `pg_depend` closure reaches. Each is marked trusted or not by the first
//! stage of DEC-1663.1's walk: a superuser, or a role that can `SET ROLE` to
//! the current role or to a superuser. Every other owner is untrusted (a role
//! that owns an object is always caught by the walk's later stages, and a
//! role below OID 16384 that is not a superuser is counted untrusted, which
//! errs towards refusal). Deciding what to refuse is the caller's.
//!
//! # The closure
//!
//! [`UserType::reached`] lists the objects the type depends on, transitively,
//! by `pg_depend`: the functions and operators a domain's CHECK and DEFAULT
//! call, the sequence a default reads, the table behind a row type nested in a
//! composite, and a function's own dependencies. It follows the edges from the
//! type, its array type, its composite relation and its domain constraints (the
//! parts the type owns) to what they reference, and stops at objects the
//! engine creates (OID below 16384) and at schemas. It does not follow what
//! depends *on* the type: a table with a column of the type is the point of
//! the type, not a reach. Which of the reached objects the plan manages is the
//! caller's to say ([`UserType::reaches_managed`]).
//!
//! # The pin
//!
//! [`Definition::pin`] is an **unkeyed** SHA-256 of the canonical definition
//! (DEC-319.1 pins routines under a key because a body may hold a secret; a
//! type's definition is not a routine body, #1523 decision 4). It covers the
//! OID and kind; an enum's labels in sort order; a domain's base, typmod,
//! collation, `NOT NULL`, default, and its constraints with `convalidated`; a
//! composite's attributes in order; the owner; and, for every type another
//! type is made of, the same again, recursively. It is built from chosen
//! columns and never a whole catalog row, so a column a later release adds
//! cannot move it, and a `NOT NULL` that 18 also lists as a constraint row
//! is read from `typnotnull` and read the same on 16. What it leaves out is
//! the ACL, which [`Unreadable`] answers, and a comment.
//!
//! The replay checks at DEC-319.1's three points, and the `type_pins` field
//! of a plan, are not here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pbps_db::{Conn, DbError, Param, Row};
use pbps_dialect::TransactionFraming;
use pbps_model::TypeRef;
use sha2::{Digest, Sha256};

use crate::pins::Within;

/// A catalog OID, read as `int8` because the driver has no unsigned type.
pub type Oid = i64;

/// The deepest chain of nested types read, and the most types read in all. A
/// composite may hold domains over composites over domains; the engine refuses
/// a type that contains itself, so these bound only an absurd catalog, and a
/// catalog beyond them is refused as unreadable instead of walked forever.
const MAX_DEPTH: usize = 64;
const MAX_TYPES: usize = 10_000;

/// Which rule produced a digest, hashed ahead of the definition. A change to the
/// encoding below is a new rule, never an edit.
const RULE: &str = "pbps/type-pin/v1";

// -- the result ------------------------------------------------------------

/// What one lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRead {
    /// An enum, domain or standalone composite, read in full.
    Present(Box<UserType>),
    /// No `pg_type` row by that schema and name.
    Absent,
    /// A shell type: named, and never defined.
    NotDefined,
    /// The deploying role cannot use the type, or the read failed. Never the
    /// same as [`TypeRead::Absent`].
    Unreadable(Unreadable),
    /// Present and readable, but not a kind pbps takes by reference.
    NotAdmitted(NotAdmitted),
}

impl TypeRead {
    /// Why a plan cannot name this type, or `None` when it can be used. Each
    /// refusal says which of the cases it is.
    #[must_use]
    pub fn refusal(&self, reference: &TypeRef) -> Option<String> {
        let name = reference.to_string();
        Some(match self {
            Self::Present(_) => return None,
            Self::Absent => format!(
                "the type `{name}` does not exist in the target database, so no column can \
                 have it"
            ),
            Self::NotDefined => format!(
                "the type `{name}` is a shell type (declared with CREATE TYPE and never \
                 defined), so no column can have it"
            ),
            Self::Unreadable(why) => why.describe(&name),
            Self::NotAdmitted(why) => why.describe(&name),
        })
    }
}

/// Why the type cannot be read. Each is a different thing to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// No `USAGE` on the schema. The type is there and cannot be named.
    SchemaUsage { schema: String },
    /// No `USAGE` on the type.
    TypeUsage,
    /// The read failed. What it found is unknown, which is not absence.
    Failed(String),
}

impl Unreadable {
    fn describe(&self, name: &str) -> String {
        match self {
            Self::SchemaUsage { schema } => format!(
                "the deploying role has no USAGE on schema `{schema}`, so it cannot use the \
                 type `{name}`"
            ),
            Self::TypeUsage => {
                format!("the deploying role has no USAGE on the type `{name}`")
            }
            Self::Failed(why) => {
                format!("the type `{name}` could not be read ({why}); this is not an absent type")
            }
        }
    }
}

/// The kinds of type that exist and are not taken by reference. Each is refused
/// by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotAdmitted {
    /// A base type, which includes an extension's (`citext`).
    Base {
        extension: Option<String>,
    },
    /// The array type of another (`_mood`); an array is written `type[]`.
    Array,
    Range,
    Multirange,
    /// `record`, `anyelement` and their kind.
    Pseudo,
    /// The row type of a relation that is not a composite type: the table,
    /// view, materialized view, partitioned table or foreign table `relkind`
    /// names (`r`, `v`, `m`, `p`, `f`).
    RowType {
        relkind: String,
    },
    /// A `typtype` this pbps does not know.
    Unknown {
        typtype: String,
    },
}

impl NotAdmitted {
    fn describe(&self, name: &str) -> String {
        let tail = "only an enum, a domain or a standalone composite type can be named";
        match self {
            Self::Base { extension: None } => {
                format!("the type `{name}` is a base type, and {tail}")
            }
            Self::Base {
                extension: Some(extension),
            } => format!(
                "the type `{name}` is a base type that belongs to the extension `{extension}`, \
                 and {tail}"
            ),
            Self::Array => format!(
                "the type `{name}` is the array type of another; write the element's name \
                 followed by `[]`, and {tail}"
            ),
            Self::Range => format!("the type `{name}` is a range type, and {tail}"),
            Self::Multirange => format!("the type `{name}` is a multirange type, and {tail}"),
            Self::Pseudo => format!("the type `{name}` is a pseudo-type, and {tail}"),
            Self::RowType { relkind } => format!(
                "the type `{name}` is the row type of {}, not a standalone composite type, \
                 and {tail}",
                relation_kind(relkind)
            ),
            Self::Unknown { typtype } => {
                format!("the type `{name}` is of kind `{typtype}`, which pbps does not know")
            }
        }
    }
}

fn relation_kind(relkind: &str) -> String {
    match relkind {
        "r" => "a table".to_owned(),
        "p" => "a partitioned table".to_owned(),
        "v" => "a view".to_owned(),
        "m" => "a materialized view".to_owned(),
        "f" => "a foreign table".to_owned(),
        other => format!("a relation of kind `{other}`"),
    }
}

// -- what is read -----------------------------------------------------------

/// The kinds of type pbps takes by reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Enum,
    Domain,
    Composite,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enum => "enum",
            Self::Domain => "domain",
            Self::Composite => "composite",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A role that owns something the type is made of or calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub oid: Oid,
    pub name: String,
    /// A superuser, or a role that can `SET ROLE` to the current role or to
    /// a superuser: DEC-1663.1's first stage. `false` for every other role.
    pub trusted: bool,
}

/// An admitted type, read in full.
///
/// Held privately so that the pin, the owners and the closure cannot disagree
/// with the definition they were read beside: this module is the only thing
/// that builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserType {
    reference: TypeRef,
    definition: Definition,
    owners: BTreeMap<Oid, Owner>,
    reached: Vec<Reached>,
    pin: Pin,
}

impl UserType {
    /// The name it was asked for by.
    #[must_use]
    pub fn reference(&self) -> &TypeRef {
        &self.reference
    }

    /// The canonical definition the pin covers.
    #[must_use]
    pub fn definition(&self) -> &Definition {
        &self.definition
    }

    #[must_use]
    pub fn oid(&self) -> Oid {
        self.definition.oid
    }

    #[must_use]
    pub fn kind(&self) -> Kind {
        self.definition.kind()
    }

    /// The type's own owner.
    #[must_use]
    pub fn owner(&self) -> &Owner {
        // `owners` is built from every owner the definition names, and a
        // missing one is a failed read.
        &self.owners[&self.definition.owner]
    }

    /// Every role that owns the type, a type nested in it, or an object its
    /// closure reaches, by OID.
    pub fn owners(&self) -> impl Iterator<Item = &Owner> {
        self.owners.values()
    }

    /// What the type depends on, transitively. Types are not listed: the
    /// types a type is made of are in [`UserType::definition`].
    #[must_use]
    pub fn reached(&self) -> &[Reached] {
        &self.reached
    }

    #[must_use]
    pub fn pin(&self) -> &Pin {
        &self.pin
    }

    /// The owners that are not trusted, by name. A domain's CHECK and
    /// DEFAULT run as the deployer when a column of it is added over existing
    /// rows, so a non-empty answer for a domain is what makes that
    /// `ADD COLUMN` unsafe (DEC-1663.1).
    #[must_use]
    pub fn untrusted_owners(&self) -> Vec<&Owner> {
        let mut out: Vec<&Owner> = self.owners.values().filter(|o| !o.trusted).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The first reached object `is_managed` says the plan manages, if any.
    /// Which objects are managed is the caller's, who holds the plan.
    pub fn reaches_managed(&self, is_managed: impl Fn(&Reached) -> bool) -> Option<&Reached> {
        self.reached.iter().find(|r| is_managed(r))
    }
}

/// One object a type's closure reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reached {
    pub oid: Oid,
    /// The system catalog that holds it: `pg_proc`, `pg_class`, `pg_operator`.
    pub catalog: String,
    /// The engine's word for it: `function`, `table`, `sequence`, `operator`,
    /// `view`, `extension`.
    pub kind: String,
    pub schema: Option<String>,
    /// A function's `proname`, an operator's `oprname`, a relation's name.
    pub name: Option<String>,
    /// The engine's identity for it, which for a function holds the argument
    /// types: `app.is_positive(integer)`.
    pub identity: String,
    pub owner: Option<Oid>,
}

/// The unkeyed digest of a [`Definition`], lowercase hexadecimal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pin(String);

impl Pin {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- the canonical definition -----------------------------------------------

/// A type as the pin sees it. Every field is a chosen catalog column, so a
/// column a later release adds changes nothing here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub owner: Oid,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// The labels in sort order. The sort key itself is not read: adding a
    /// value `BEFORE` another gives it a fraction, and only the order is a
    /// property of the type.
    Enum {
        labels: Vec<String>,
    },
    Domain(Domain),
    Composite(Composite),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    pub base: Component,
    pub typmod: i32,
    pub collation: Oid,
    pub not_null: bool,
    /// The default as the catalog deparses it, under an empty `search_path`.
    pub default: Option<String>,
    /// The constraints other than a `NOT NULL`, which `typnotnull` holds on
    /// every release and 18 also lists as a row.
    pub constraints: Vec<DomainConstraint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainConstraint {
    pub name: String,
    /// `pg_constraint.contype`, so a kind a later release adds still differs.
    pub contype: String,
    /// `convalidated`: `ADD CONSTRAINT … NOT VALID` and `VALIDATE CONSTRAINT`
    /// change what a column of the domain has been checked against.
    pub validated: bool,
    /// `pg_get_constraintdef`, under an empty `search_path`.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composite {
    /// `c` for a composite type. A table's row type nested in one is read the
    /// same way and keeps its `relkind` (`r`, `v`, …), but is never admitted
    /// by itself.
    pub relkind: String,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub ty: Component,
    pub typmod: i32,
    pub collation: Oid,
}

/// A type another type is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Component {
    /// `T[]`, with the element read in full.
    Array(Box<Component>),
    /// An enum, a domain or a composite (a table's row type included), read
    /// in full, since a change to it is a change to what this one means.
    Defined(Box<Definition>),
    /// Anything else (a built-in, an extension's type, a range), by OID and
    /// name: it cannot be changed in place.
    Other {
        oid: Oid,
        schema: String,
        name: String,
        typtype: String,
    },
}

impl Definition {
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self.body {
            Body::Enum { .. } => Kind::Enum,
            Body::Domain(_) => Kind::Domain,
            Body::Composite(_) => Kind::Composite,
        }
    }

    /// The digest of this definition. Two definitions that are equal as
    /// catalog facts give the same pin; any field changing gives another.
    #[must_use]
    pub fn pin(&self) -> Pin {
        let mut hasher = Sha256::new();
        hasher.update(RULE.as_bytes());
        hasher.update([0]);
        hasher.update(self.canonical().to_string().as_bytes());
        Pin(format!("{:x}", hasher.finalize()))
    }

    /// The encoding the pin hashes: JSON arrays with a fixed position for
    /// every field, so no key order is involved and no two definitions share a
    /// text. A domain's constraints are a set and are sorted by name; labels
    /// and attributes are sequences and keep their order.
    fn canonical(&self) -> serde_json::Value {
        use serde_json::{Value, json};
        let body = match &self.body {
            Body::Enum { labels } => json!(["enum", labels]),
            Body::Domain(d) => {
                let mut constraints: Vec<&DomainConstraint> = d.constraints.iter().collect();
                constraints.sort_by(|a, b| a.name.cmp(&b.name));
                json!([
                    "domain",
                    d.base.canonical(),
                    d.typmod,
                    d.collation,
                    d.not_null,
                    d.default,
                    constraints
                        .iter()
                        .map(|c| json!([c.name, c.contype, c.validated, c.definition]))
                        .collect::<Vec<Value>>(),
                ])
            }
            Body::Composite(c) => json!([
                "composite",
                c.relkind,
                c.attributes
                    .iter()
                    .map(|a| json!([a.name, a.ty.canonical(), a.typmod, a.collation]))
                    .collect::<Vec<Value>>(),
            ]),
        };
        json!([self.oid, self.schema, self.name, self.owner, body])
    }

    /// Every owner this definition and the types nested in it name.
    fn collect_owners(&self, out: &mut BTreeSet<Oid>) {
        out.insert(self.owner);
        let mut nested = |c: &Component| c.collect_owners(out);
        match &self.body {
            Body::Enum { .. } => {}
            Body::Domain(d) => nested(&d.base),
            Body::Composite(c) => c.attributes.iter().for_each(|a| nested(&a.ty)),
        }
    }
}

impl Component {
    fn canonical(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Array(element) => json!(["array", element.canonical()]),
            Self::Defined(definition) => json!(["type", definition.canonical()]),
            Self::Other {
                oid,
                schema,
                name,
                typtype,
            } => json!(["other", oid, schema, name, typtype]),
        }
    }

    fn collect_owners(&self, out: &mut BTreeSet<Oid>) {
        match self {
            Self::Array(element) => element.collect_owners(out),
            Self::Defined(definition) => definition.collect_owners(out),
            Self::Other { .. } => {}
        }
    }
}

// -- the read ----------------------------------------------------------------

/// Reads each reference, in order. In a transaction of its own that is one
/// snapshot of the catalog tables; in the caller's it is the caller's. A
/// reference has exactly one result, and a read that fails is [`Unreadable::Failed`] for every
/// reference: nothing was learned, and nothing is reported as absent.
///
/// Runs under an empty `search_path` of its own (`format_type` and the
/// deparsed CHECK and DEFAULT qualify a name only when the path does not show
/// it), set for the transaction and not left behind.
pub async fn read(conn: &mut Conn, within: Within, references: &[TypeRef]) -> Vec<TypeRead> {
    match read_all(conn, within, references).await {
        Ok(reads) => reads,
        Err(e) => references
            .iter()
            .map(|_| TypeRead::Unreadable(Unreadable::Failed(e.to_string())))
            .collect(),
    }
}

/// `REPEATABLE READ`, so the lookup, the definition and the closure of one
/// type come from one snapshot of the catalog tables.
const OWN_TRANSACTION: TransactionFraming = TransactionFraming {
    begin: std::borrow::Cow::Borrowed("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;"),
    commit: std::borrow::Cow::Borrowed("COMMIT;"),
    rollback: std::borrow::Cow::Borrowed("ROLLBACK;"),
};

const SAVEPOINT: &str = "pbps_type_reads";

async fn read_all(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
) -> Result<Vec<TypeRead>, DbError> {
    match within {
        Within::OwnTransaction => {
            conn.begin(&OWN_TRANSACTION).await?;
            let result = read_each(conn, references).await;
            let closed = conn.rollback(&OWN_TRANSACTION).await;
            let reads = result?;
            closed?;
            Ok(reads)
        }
        Within::CallersTransaction => {
            conn.execute(&format!("SAVEPOINT {SAVEPOINT}")).await?;
            match read_each(conn, references).await {
                Ok(reads) => {
                    conn.execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                        .await?;
                    conn.execute(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                        .await?;
                    Ok(reads)
                }
                Err(e) => {
                    // Leave the caller's transaction usable if it can be: a
                    // failed statement aborts it until the savepoint is
                    // rolled back. The read's own error is the one reported.
                    let _ = conn
                        .execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                        .await;
                    let _ = conn
                        .execute(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                        .await;
                    Err(e)
                }
            }
        }
    }
}

async fn read_each(conn: &mut Conn, references: &[TypeRef]) -> Result<Vec<TypeRead>, DbError> {
    conn.query("SELECT pg_catalog.set_config('search_path', '', true) AS was_set")
        .await?;
    let mut reads = Vec::with_capacity(references.len());
    for reference in references {
        reads.push(read_one(conn, reference).await?);
    }
    Ok(reads)
}

/// The row a lookup by name returns.
const LOOKUP: &str = "
    SELECT t.oid::pg_catalog.int8 AS oid,
           t.typtype::pg_catalog.text AS typtype,
           t.typisdefined AS defined,
           t.typcategory::pg_catalog.text AS category,
           t.typelem::pg_catalog.int8 AS elem,
           c.relkind::pg_catalog.text AS relkind,
           n.nspname::pg_catalog.text AS schema,
           pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS schema_usage,
           pg_catalog.has_type_privilege(t.oid, 'USAGE') AS type_usage,
           (SELECT e.extname::pg_catalog.text
              FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_catalog.pg_type'::pg_catalog.regclass
               AND d.objid = t.oid
               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
               AND d.deptype = 'e'
             LIMIT 1) AS extension
      FROM pg_catalog.pg_type t
      JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
      LEFT JOIN pg_catalog.pg_class c ON c.oid = t.typrelid
     WHERE n.nspname::pg_catalog.text = $1
       AND t.typname::pg_catalog.text = $2";

async fn read_one(conn: &mut Conn, reference: &TypeRef) -> Result<TypeRead, DbError> {
    let rows = conn
        .query_with(
            LOOKUP,
            &[Param::Str(reference.schema()), Param::Str(reference.name())],
        )
        .await?;
    // `(typname, typnamespace)` is unique, so two rows is not a catalog.
    let row = match rows.as_slice() {
        [] => return Ok(TypeRead::Absent),
        [row] => row,
        _ => {
            return Err(DbError::BadRow(
                "a type name matched more than one `pg_type` row".to_owned(),
            ));
        }
    };
    if !need::<bool>(row, "schema_usage")? {
        return Ok(TypeRead::Unreadable(Unreadable::SchemaUsage {
            schema: need::<&str>(row, "schema")?.to_owned(),
        }));
    }
    if !need::<bool>(row, "type_usage")? {
        return Ok(TypeRead::Unreadable(Unreadable::TypeUsage));
    }
    if !need::<bool>(row, "defined")? {
        return Ok(TypeRead::NotDefined);
    }
    let oid = need::<i64>(row, "oid")?;
    let typtype = need::<&str>(row, "typtype")?;
    let relkind = row.try_get::<&str>("relkind")?;
    let category = need::<&str>(row, "category")?;
    if let Some(not_admitted) = classify(
        typtype,
        category,
        need::<i64>(row, "elem")?,
        relkind,
        row.try_get::<&str>("extension")?,
    ) {
        return Ok(TypeRead::NotAdmitted(not_admitted));
    }
    let definition = define(conn, oid).await?;
    let reached = closure(conn, oid).await?;
    let mut ids = BTreeSet::new();
    definition.collect_owners(&mut ids);
    ids.extend(reached.iter().filter_map(|r| r.owner));
    let owners = owners(conn, &ids).await?;
    let pin = definition.pin();
    Ok(TypeRead::Present(Box::new(UserType {
        reference: reference.clone(),
        definition,
        owners,
        reached,
        pin,
    })))
}

/// Why a type that exists is not one of the three kinds, or `None`. Checked in
/// full, with no catch-all: a `typtype` this does not list is refused by name
/// and not admitted.
fn classify(
    typtype: &str,
    category: &str,
    elem: i64,
    relkind: Option<&str>,
    extension: Option<&str>,
) -> Option<NotAdmitted> {
    match typtype {
        "e" | "d" => None,
        "c" => match relkind {
            Some("c") => None,
            Some(other) => Some(NotAdmitted::RowType {
                relkind: other.to_owned(),
            }),
            // A composite `typtype` with no relation behind it is not a
            // catalog the engine produces; refused, never admitted.
            None => Some(NotAdmitted::Unknown {
                typtype: "c".to_owned(),
            }),
        },
        "b" if category == "A" && elem != 0 => Some(NotAdmitted::Array),
        "b" => Some(NotAdmitted::Base {
            extension: extension.map(str::to_owned),
        }),
        "r" => Some(NotAdmitted::Range),
        "m" => Some(NotAdmitted::Multirange),
        "p" => Some(NotAdmitted::Pseudo),
        other => Some(NotAdmitted::Unknown {
            typtype: other.to_owned(),
        }),
    }
}

// -- the definition, from the catalog ----------------------------------------

/// A row of `pg_type` for a type the definition holds or names.
struct TypeRow {
    oid: Oid,
    schema: String,
    name: String,
    owner: Oid,
    typtype: String,
    category: String,
    elem: Oid,
    base: Oid,
    typmod: i32,
    not_null: bool,
    collation: Oid,
    default: Option<String>,
    relkind: Option<String>,
}

impl TypeRow {
    fn is_array(&self) -> bool {
        self.typtype == "b" && self.category == "A" && self.elem != 0
    }

    /// Whether the definition holds this type in full.
    fn is_defined_kind(&self) -> bool {
        matches!(self.typtype.as_str(), "e" | "d" | "c")
    }
}

struct AttributeRow {
    name: String,
    ty: Oid,
    typmod: i32,
    collation: Oid,
}

#[derive(Default)]
struct Catalog {
    types: BTreeMap<Oid, TypeRow>,
    labels: BTreeMap<Oid, Vec<String>>,
    constraints: BTreeMap<Oid, Vec<DomainConstraint>>,
    attributes: BTreeMap<Oid, Vec<AttributeRow>>,
}

/// An OID list as SQL. The values are integers this module read, never text.
fn oid_array(oids: &[Oid]) -> String {
    let list: Vec<String> = oids.iter().map(Oid::to_string).collect();
    format!("ARRAY[{}]::pg_catalog.oid[]", list.join(", "))
}

async fn define(conn: &mut Conn, top: Oid) -> Result<Definition, DbError> {
    let mut catalog = Catalog::default();
    let mut frontier = vec![top];
    let mut level = 0;
    while !frontier.is_empty() {
        level += 1;
        if level > MAX_DEPTH {
            return Err(DbError::BadRow(format!(
                "types are nested more than {MAX_DEPTH} deep"
            )));
        }
        let fetched = fetch_types(conn, &frontier).await?;
        let held: Vec<Oid> = fetched
            .iter()
            .filter(|t| t.is_defined_kind())
            .map(|t| t.oid)
            .collect();
        let mut wanted: Vec<Oid> = Vec::new();
        for t in &fetched {
            if t.is_array() {
                wanted.push(t.elem);
            } else if t.typtype == "d" {
                wanted.push(t.base);
            }
        }
        for t in fetched {
            catalog.types.insert(t.oid, t);
        }
        if !held.is_empty() {
            fetch_labels(conn, &held, &mut catalog).await?;
            fetch_constraints(conn, &held, &mut catalog).await?;
            fetch_attributes(conn, &held, &mut catalog).await?;
        }
        for oid in &held {
            for a in catalog.attributes.get(oid).into_iter().flatten() {
                wanted.push(a.ty);
            }
        }
        wanted.retain(|oid| !catalog.types.contains_key(oid));
        wanted.sort_unstable();
        wanted.dedup();
        if catalog.types.len() + wanted.len() > MAX_TYPES {
            return Err(DbError::BadRow(format!(
                "a type is made of more than {MAX_TYPES} types"
            )));
        }
        frontier = wanted;
    }
    let mut built = Built::default();
    built.definition(&catalog, top)
}

#[derive(Default)]
struct Built {
    stack: Vec<Oid>,
    nodes: usize,
}

impl Built {
    fn definition(&mut self, catalog: &Catalog, oid: Oid) -> Result<Definition, DbError> {
        if self.stack.contains(&oid) {
            return Err(DbError::BadRow(format!("type {oid} contains itself")));
        }
        self.nodes += 1;
        if self.nodes > MAX_TYPES * 10 {
            return Err(DbError::BadRow(
                "a type's definition is too large to pin".to_owned(),
            ));
        }
        self.stack.push(oid);
        let t = catalog
            .types
            .get(&oid)
            .ok_or_else(|| DbError::BadRow(format!("type {oid} is not in the catalog read")))?;
        let body = match t.typtype.as_str() {
            "e" => Body::Enum {
                labels: catalog.labels.get(&oid).cloned().unwrap_or_default(),
            },
            "d" => Body::Domain(Domain {
                base: self.component(catalog, t.base)?,
                typmod: t.typmod,
                collation: t.collation,
                not_null: t.not_null,
                default: t.default.clone(),
                constraints: catalog.constraints.get(&oid).cloned().unwrap_or_default(),
            }),
            "c" => {
                let mut attributes = Vec::new();
                for a in catalog.attributes.get(&oid).into_iter().flatten() {
                    attributes.push(Attribute {
                        name: a.name.clone(),
                        ty: self.component(catalog, a.ty)?,
                        typmod: a.typmod,
                        collation: a.collation,
                    });
                }
                Body::Composite(Composite {
                    relkind: t.relkind.clone().unwrap_or_default(),
                    attributes,
                })
            }
            other => {
                return Err(DbError::BadRow(format!(
                    "type {oid} is of kind `{other}`, which has no definition to pin"
                )));
            }
        };
        self.stack.pop();
        Ok(Definition {
            oid,
            schema: t.schema.clone(),
            name: t.name.clone(),
            owner: t.owner,
            body,
        })
    }

    fn component(&mut self, catalog: &Catalog, oid: Oid) -> Result<Component, DbError> {
        let t = catalog
            .types
            .get(&oid)
            .ok_or_else(|| DbError::BadRow(format!("type {oid} is not in the catalog read")))?;
        if t.is_array() {
            return Ok(Component::Array(Box::new(self.component(catalog, t.elem)?)));
        }
        if t.is_defined_kind() {
            return Ok(Component::Defined(Box::new(self.definition(catalog, oid)?)));
        }
        Ok(Component::Other {
            oid,
            schema: t.schema.clone(),
            name: t.name.clone(),
            typtype: t.typtype.clone(),
        })
    }
}

async fn fetch_types(conn: &mut Conn, oids: &[Oid]) -> Result<Vec<TypeRow>, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT t.oid::pg_catalog.int8 AS oid,
                    n.nspname::pg_catalog.text AS schema,
                    t.typname::pg_catalog.text AS name,
                    t.typowner::pg_catalog.int8 AS owner,
                    t.typtype::pg_catalog.text AS typtype,
                    t.typcategory::pg_catalog.text AS category,
                    t.typelem::pg_catalog.int8 AS elem,
                    t.typbasetype::pg_catalog.int8 AS base,
                    t.typtypmod AS typmod,
                    t.typnotnull AS not_null,
                    t.typcollation::pg_catalog.int8 AS collation,
                    pg_catalog.pg_get_expr(t.typdefaultbin, 0::pg_catalog.oid) AS default_expr,
                    c.relkind::pg_catalog.text AS relkind
               FROM pg_catalog.pg_type t
               JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
               LEFT JOIN pg_catalog.pg_class c ON c.oid = t.typrelid
              WHERE t.oid = ANY({})
              ORDER BY t.oid",
            oid_array(oids)
        ))
        .await?;
    let found: Vec<TypeRow> = rows
        .iter()
        .map(|row| {
            Ok(TypeRow {
                oid: need(row, "oid")?,
                schema: need::<&str>(row, "schema")?.to_owned(),
                name: need::<&str>(row, "name")?.to_owned(),
                owner: need(row, "owner")?,
                typtype: need::<&str>(row, "typtype")?.to_owned(),
                category: need::<&str>(row, "category")?.to_owned(),
                elem: need(row, "elem")?,
                base: need(row, "base")?,
                typmod: need(row, "typmod")?,
                not_null: need(row, "not_null")?,
                collation: need(row, "collation")?,
                default: row.try_get::<&str>("default_expr")?.map(str::to_owned),
                relkind: row.try_get::<&str>("relkind")?.map(str::to_owned),
            })
        })
        .collect::<Result<_, DbError>>()?;
    // A type another one names is a catalog row; one that is not there has
    // been dropped under the read, which is a failed read and not an absence.
    for oid in oids {
        if !found.iter().any(|t| t.oid == *oid) {
            return Err(DbError::BadRow(format!(
                "type {oid} disappeared while it was being read"
            )));
        }
    }
    Ok(found)
}

async fn fetch_labels(conn: &mut Conn, oids: &[Oid], into: &mut Catalog) -> Result<(), DbError> {
    let rows = conn
        .query(&format!(
            "SELECT enumtypid::pg_catalog.int8 AS typ, enumlabel::pg_catalog.text AS label
               FROM pg_catalog.pg_enum
              WHERE enumtypid = ANY({})
              ORDER BY enumtypid, enumsortorder",
            oid_array(oids)
        ))
        .await?;
    for row in &rows {
        into.labels
            .entry(need(row, "typ")?)
            .or_default()
            .push(need::<&str>(row, "label")?.to_owned());
    }
    Ok(())
}

/// The `NOT NULL` row 18 keeps for a domain is left out (`contype = 'n'`):
/// `typnotnull` holds it on every release, and 16 has no such row.
async fn fetch_constraints(
    conn: &mut Conn,
    oids: &[Oid],
    into: &mut Catalog,
) -> Result<(), DbError> {
    let rows = conn
        .query(&format!(
            "SELECT contypid::pg_catalog.int8 AS typ,
                    conname::pg_catalog.text AS name,
                    contype::pg_catalog.text AS contype,
                    convalidated AS validated,
                    pg_catalog.pg_get_constraintdef(oid) AS definition
               FROM pg_catalog.pg_constraint
              WHERE contypid = ANY({}) AND contype <> 'n'
              ORDER BY contypid, conname::pg_catalog.text COLLATE \"C\"",
            oid_array(oids)
        ))
        .await?;
    for row in &rows {
        into.constraints
            .entry(need(row, "typ")?)
            .or_default()
            .push(DomainConstraint {
                name: need::<&str>(row, "name")?.to_owned(),
                contype: need::<&str>(row, "contype")?.to_owned(),
                validated: need(row, "validated")?,
                definition: need::<&str>(row, "definition")?.to_owned(),
            });
    }
    Ok(())
}

async fn fetch_attributes(
    conn: &mut Conn,
    oids: &[Oid],
    into: &mut Catalog,
) -> Result<(), DbError> {
    let rows = conn
        .query(&format!(
            "SELECT t.oid::pg_catalog.int8 AS typ,
                    a.attname::pg_catalog.text AS name,
                    a.atttypid::pg_catalog.int8 AS ty,
                    a.atttypmod AS typmod,
                    a.attcollation::pg_catalog.int8 AS collation
               FROM pg_catalog.pg_type t
               JOIN pg_catalog.pg_attribute a ON a.attrelid = t.typrelid
              WHERE t.oid = ANY({}) AND t.typtype = 'c'
                AND a.attnum > 0 AND NOT a.attisdropped
              ORDER BY t.oid, a.attnum",
            oid_array(oids)
        ))
        .await?;
    for row in &rows {
        into.attributes
            .entry(need(row, "typ")?)
            .or_default()
            .push(AttributeRow {
                name: need::<&str>(row, "name")?.to_owned(),
                ty: need(row, "ty")?,
                typmod: need(row, "typmod")?,
                collation: need(row, "collation")?,
            });
    }
    Ok(())
}

// -- the closure and the owners -----------------------------------------------

/// What the type depends on. One recursive term, which the engine requires,
/// carries both directions: the forward edges of every node, and, from a type
/// only, what it owns (`a`uto and `i`nternal dependents: its array type, its
/// composite relation, a domain's constraints). Owned parts are flagged and
/// left out of the answer, and so are types and constraints themselves; what
/// they reference is in it.
const CLOSURE: &str = "
    WITH RECURSIVE reach(classid, objid, part) AS (
        SELECT 'pg_catalog.pg_type'::pg_catalog.regclass::pg_catalog.oid,
               ($1::pg_catalog.int8)::pg_catalog.oid, true
      UNION
        SELECT n.classid, n.objid, n.part
          FROM reach r
         CROSS JOIN LATERAL (
            SELECT d.refclassid, d.refobjid, false
              FROM pg_catalog.pg_depend d
             WHERE d.classid = r.classid AND d.objid = r.objid
               AND d.refobjid >= 16384::pg_catalog.oid
               AND d.refclassid <> 'pg_catalog.pg_namespace'::pg_catalog.regclass
            UNION ALL
            SELECT d.classid, d.objid, true
              FROM pg_catalog.pg_depend d
             WHERE r.classid = 'pg_catalog.pg_type'::pg_catalog.regclass
               AND d.refclassid = r.classid AND d.refobjid = r.objid
               AND d.deptype IN ('a', 'i')
               AND d.classid IN ('pg_catalog.pg_type'::pg_catalog.regclass,
                                 'pg_catalog.pg_constraint'::pg_catalog.regclass,
                                 'pg_catalog.pg_class'::pg_catalog.regclass)
         ) n(classid, objid, part)
    ),
    nodes AS (
        SELECT classid, objid, pg_catalog.bool_or(part) AS part
          FROM reach GROUP BY classid, objid
    )
    SELECT c.relname::pg_catalog.text AS catalog,
           n.objid::pg_catalog.int8 AS oid,
           COALESCE(i.type, 'object') AS kind,
           i.schema AS schema,
           CASE n.classid
             WHEN 'pg_catalog.pg_proc'::pg_catalog.regclass
               THEN (SELECT p.proname::pg_catalog.text FROM pg_catalog.pg_proc p WHERE p.oid = n.objid)
             WHEN 'pg_catalog.pg_operator'::pg_catalog.regclass
               THEN (SELECT o.oprname::pg_catalog.text FROM pg_catalog.pg_operator o WHERE o.oid = n.objid)
             ELSE i.name
           END AS name,
           COALESCE(i.identity, n.objid::pg_catalog.text) AS identity,
           CASE n.classid
             WHEN 'pg_catalog.pg_proc'::pg_catalog.regclass
               THEN (SELECT p.proowner::pg_catalog.int8 FROM pg_catalog.pg_proc p WHERE p.oid = n.objid)
             WHEN 'pg_catalog.pg_class'::pg_catalog.regclass
               THEN (SELECT k.relowner::pg_catalog.int8 FROM pg_catalog.pg_class k WHERE k.oid = n.objid)
             WHEN 'pg_catalog.pg_operator'::pg_catalog.regclass
               THEN (SELECT o.oprowner::pg_catalog.int8 FROM pg_catalog.pg_operator o WHERE o.oid = n.objid)
             WHEN 'pg_catalog.pg_collation'::pg_catalog.regclass
               THEN (SELECT k.collowner::pg_catalog.int8 FROM pg_catalog.pg_collation k WHERE k.oid = n.objid)
             WHEN 'pg_catalog.pg_extension'::pg_catalog.regclass
               THEN (SELECT x.extowner::pg_catalog.int8 FROM pg_catalog.pg_extension x WHERE x.oid = n.objid)
           END AS owner
      FROM nodes n
      JOIN pg_catalog.pg_class c ON c.oid = n.classid
     CROSS JOIN LATERAL pg_catalog.pg_identify_object(n.classid, n.objid, 0) i
     WHERE NOT n.part
       AND n.classid NOT IN ('pg_catalog.pg_type'::pg_catalog.regclass,
                             'pg_catalog.pg_constraint'::pg_catalog.regclass)
     ORDER BY 1, 4, 6";

async fn closure(conn: &mut Conn, oid: Oid) -> Result<Vec<Reached>, DbError> {
    let rows = conn.query_with(CLOSURE, &[Param::I64(oid)]).await?;
    rows.iter()
        .map(|row| {
            Ok(Reached {
                oid: need(row, "oid")?,
                catalog: need::<&str>(row, "catalog")?.to_owned(),
                kind: need::<&str>(row, "kind")?.to_owned(),
                schema: row.try_get::<&str>("schema")?.map(str::to_owned),
                name: row.try_get::<&str>("name")?.map(str::to_owned),
                identity: need::<&str>(row, "identity")?.to_owned(),
                owner: row.try_get::<i64>("owner")?,
            })
        })
        .collect()
}

/// Each owner and whether DEC-1663.1's first stage trusts it: a superuser, or
/// a role that can `SET ROLE` to the current role or to a superuser. `SET`
/// is a `pg_has_role` privilege from 16, the oldest release supported.
async fn owners(conn: &mut Conn, ids: &BTreeSet<Oid>) -> Result<BTreeMap<Oid, Owner>, DbError> {
    let wanted: Vec<Oid> = ids.iter().copied().collect();
    let rows = conn
        .query(&format!(
            "SELECT r.oid::pg_catalog.int8 AS oid,
                    r.rolname::pg_catalog.text AS name,
                    (r.rolsuper
                     OR pg_catalog.pg_has_role(r.oid, (
                            SELECT m.oid FROM pg_catalog.pg_roles m
                             WHERE m.rolname OPERATOR(pg_catalog.=) CURRENT_USER), 'SET')
                     OR EXISTS (SELECT FROM pg_catalog.pg_roles su
                                 WHERE su.rolsuper
                                   AND pg_catalog.pg_has_role(r.oid, su.oid, 'SET'))) AS trusted
               FROM pg_catalog.pg_roles r
              WHERE r.oid = ANY({})",
            oid_array(&wanted)
        ))
        .await?;
    let mut out = BTreeMap::new();
    for row in &rows {
        let oid = need(row, "oid")?;
        out.insert(
            oid,
            Owner {
                oid,
                name: need::<&str>(row, "name")?.to_owned(),
                trusted: need(row, "trusted")?,
            },
        );
    }
    // An owner the catalog cannot show is a read that failed, and is not
    // counted trusted by being missing.
    if let Some(missing) = wanted.iter().find(|oid| !out.contains_key(oid)) {
        return Err(DbError::BadRow(format!(
            "the owning role {missing} is not in `pg_roles`"
        )));
    }
    Ok(out)
}

/// A column that is never NULL where it is read. A NULL is a failed read.
fn need<'a, T: pbps_db::FromColumn<'a>>(row: &'a Row, column: &str) -> Result<T, DbError> {
    row.try_get::<T>(column)?
        .ok_or_else(|| DbError::BadRow(format!("column `{column}` is unexpectedly NULL")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(oid: Oid, name: &str) -> Component {
        Component::Other {
            oid,
            schema: "pg_catalog".to_owned(),
            name: name.to_owned(),
            typtype: "b".to_owned(),
        }
    }

    fn enumeration(labels: &[&str]) -> Definition {
        Definition {
            oid: 40001,
            schema: "app".to_owned(),
            name: "mood".to_owned(),
            owner: 10,
            body: Body::Enum {
                labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            },
        }
    }

    fn constraint(name: &str, validated: bool, definition: &str) -> DomainConstraint {
        DomainConstraint {
            name: name.to_owned(),
            contype: "c".to_owned(),
            validated,
            definition: definition.to_owned(),
        }
    }

    fn domain() -> Definition {
        Definition {
            oid: 40002,
            schema: "app".to_owned(),
            name: "positive".to_owned(),
            owner: 10,
            body: Body::Domain(Domain {
                base: leaf(23, "int4"),
                typmod: -1,
                collation: 0,
                not_null: false,
                default: Some("1".to_owned()),
                constraints: vec![
                    constraint("positive_check", true, "CHECK ((VALUE > 0))"),
                    constraint("small", true, "CHECK ((VALUE < 100))"),
                ],
            }),
        }
    }

    fn composite() -> Definition {
        Definition {
            oid: 40003,
            schema: "app".to_owned(),
            name: "pair".to_owned(),
            owner: 10,
            body: Body::Composite(Composite {
                relkind: "c".to_owned(),
                attributes: vec![
                    Attribute {
                        name: "a".to_owned(),
                        ty: Component::Defined(Box::new(domain())),
                        typmod: -1,
                        collation: 0,
                    },
                    Attribute {
                        name: "b".to_owned(),
                        ty: Component::Array(Box::new(leaf(25, "text"))),
                        typmod: -1,
                        collation: 100,
                    },
                ],
            }),
        }
    }

    fn domain_of(edit: impl FnOnce(&mut Domain)) -> Definition {
        let mut d = domain();
        if let Body::Domain(inner) = &mut d.body {
            edit(inner);
        }
        d
    }

    fn composite_of(edit: impl FnOnce(&mut Composite)) -> Definition {
        let mut d = composite();
        if let Body::Composite(inner) = &mut d.body {
            edit(inner);
        }
        d
    }

    /// The encoding is a rule, and a change to it silently re-pins every saved
    /// plan. Pinned to the text and digest of three definitions (the digests
    /// computed outside this crate, from the text), so an edit to the encoding
    /// fails here and must become a new rule (`RULE`).
    #[test]
    fn the_pin_encoding_is_the_one_the_rule_names() {
        assert_eq!(RULE, "pbps/type-pin/v1");
        let mood = enumeration(&["happy", "sad"]);
        assert_eq!(
            mood.canonical().to_string(),
            r#"[40001,"app","mood",10,["enum",["happy","sad"]]]"#
        );
        assert_eq!(
            mood.pin().as_str(),
            "cc7101ae9beff864c124e4bdcb99acaee6d1a1ec2639d8f697d6f72efb8a30f9"
        );
        assert_eq!(
            domain().canonical().to_string(),
            r#"[40002,"app","positive",10,["domain",["other",23,"pg_catalog","int4","b"],-1,0,false,"1",[["positive_check","c",true,"CHECK ((VALUE > 0))"],["small","c",true,"CHECK ((VALUE < 100))"]]]]"#
        );
        assert_eq!(
            domain().pin().as_str(),
            "3fd0c79846be9b817c248ca1cc309910d905e632b7779bf919e06f4f257636f8"
        );
        assert_eq!(
            composite().pin().as_str(),
            "11c269673400967ab091c94f1d211f03dfba0fc431e603dfc340c0caf0985738"
        );
    }

    #[test]
    fn equal_definitions_give_equal_pins() {
        assert_eq!(composite().pin(), composite().pin());
        assert_eq!(domain().pin(), domain().clone().pin());
        assert_eq!(
            enumeration(&["a", "b"]).pin(),
            enumeration(&["a", "b"]).pin()
        );
    }

    #[test]
    fn an_enums_labels_are_a_sequence_and_their_order_changes_the_pin() {
        let base = enumeration(&["a", "b", "c"]);
        for other in [
            enumeration(&["b", "a", "c"]),
            enumeration(&["a", "b"]),
            enumeration(&["a", "b", "c", "d"]),
            enumeration(&["a", "b", "z"]),
            // The text of two labels is not the text of one.
            enumeration(&["a", "bc"]),
            enumeration(&["ab", "c"]),
            enumeration(&[]),
        ] {
            assert_ne!(base.pin(), other.pin(), "{other:?}");
        }
        // Negative: an empty enum is a definition, with a pin of its own.
        assert_eq!(enumeration(&[]).pin(), enumeration(&[]).pin());
    }

    #[test]
    fn a_domain_constraint_set_does_not_depend_on_the_order_it_was_read_in() {
        let swapped = domain_of(|d| d.constraints.reverse());
        assert_eq!(domain().pin(), swapped.pin());
        // Negative: the constraints themselves do change it.
        let fewer = domain_of(|d| {
            d.constraints.pop();
        });
        assert_ne!(domain().pin(), fewer.pin());
    }

    #[test]
    fn every_field_of_a_domain_changes_its_pin() {
        let base = domain().pin();
        let edits: Vec<(&str, Definition)> = vec![
            ("base type", domain_of(|d| d.base = leaf(20, "int8"))),
            ("typmod", domain_of(|d| d.typmod = 14)),
            ("collation", domain_of(|d| d.collation = 950)),
            ("not null", domain_of(|d| d.not_null = true)),
            ("default", domain_of(|d| d.default = Some("2".to_owned()))),
            ("no default", domain_of(|d| d.default = None)),
            (
                "constraint validated",
                domain_of(|d| d.constraints[0].validated = false),
            ),
            (
                "constraint text",
                domain_of(|d| d.constraints[0].definition = "CHECK ((VALUE > 1))".to_owned()),
            ),
            (
                "constraint name",
                domain_of(|d| d.constraints[0].name = "renamed".to_owned()),
            ),
            (
                "constraint kind",
                domain_of(|d| d.constraints[0].contype = "x".to_owned()),
            ),
            (
                "constraint added",
                domain_of(|d| {
                    d.constraints
                        .push(constraint("extra", false, "CHECK (true)"))
                }),
            ),
            (
                "oid",
                Definition {
                    oid: 40099,
                    ..domain()
                },
            ),
            (
                "schema",
                Definition {
                    schema: "other".to_owned(),
                    ..domain()
                },
            ),
            (
                "name",
                Definition {
                    name: "negative".to_owned(),
                    ..domain()
                },
            ),
            (
                "owner",
                Definition {
                    owner: 11,
                    ..domain()
                },
            ),
        ];
        for (what, edited) in edits {
            assert_ne!(base, edited.pin(), "{what} does not change the pin");
        }
    }

    #[test]
    fn a_composites_attributes_are_a_sequence_and_each_field_changes_the_pin() {
        let base = composite().pin();
        let edits: Vec<(&str, Definition)> = vec![
            ("order", composite_of(|c| c.attributes.reverse())),
            (
                "an attribute dropped",
                composite_of(|c| {
                    c.attributes.pop();
                }),
            ),
            (
                "an attribute name",
                composite_of(|c| c.attributes[0].name = "z".to_owned()),
            ),
            (
                "an attribute type",
                composite_of(|c| c.attributes[1].ty = leaf(25, "text")),
            ),
            (
                "an attribute typmod",
                composite_of(|c| c.attributes[1].typmod = 14),
            ),
            (
                "an attribute collation",
                composite_of(|c| c.attributes[1].collation = 950),
            ),
            ("relkind", composite_of(|c| c.relkind = "r".to_owned())),
            (
                "an array's element",
                composite_of(|c| {
                    c.attributes[1].ty = Component::Array(Box::new(leaf(1043, "varchar")));
                }),
            ),
        ];
        for (what, edited) in edits {
            assert_ne!(base, edited.pin(), "{what} does not change the pin");
        }
    }

    /// A type a composite holds is read in full, so a change to the nested
    /// domain moves the outer pin, however deep it sits.
    #[test]
    fn a_change_inside_a_nested_type_changes_the_outer_pin() {
        let nested_domain_not_null = composite_of(|c| {
            if let Component::Defined(inner) = &mut c.attributes[0].ty
                && let Body::Domain(d) = &mut inner.body
            {
                d.not_null = true;
            }
        });
        assert_ne!(composite().pin(), nested_domain_not_null.pin());

        // Two levels: the domain's base is an enum, whose label changes.
        let over_enum = |labels: &[&str]| Definition {
            body: Body::Domain(Domain {
                base: Component::Array(Box::new(Component::Defined(Box::new(enumeration(labels))))),
                ..match domain().body {
                    Body::Domain(d) => d,
                    Body::Enum { .. } | Body::Composite(_) => unreachable!(),
                }
            }),
            ..domain()
        };
        assert_ne!(over_enum(&["a"]).pin(), over_enum(&["a", "b"]).pin());
        assert_eq!(over_enum(&["a"]).pin(), over_enum(&["a"]).pin());
    }

    /// An array of a type is not the type, and a type held in full is not
    /// the same type named opaquely.
    #[test]
    fn the_shape_of_a_component_is_part_of_the_pin() {
        let with = |ty: Component| composite_of(|c| c.attributes[1].ty = ty).pin();
        let element = leaf(25, "text");
        let pins = [
            with(element.clone()),
            with(Component::Array(Box::new(element.clone()))),
            with(Component::Array(Box::new(Component::Array(Box::new(
                element.clone(),
            ))))),
            with(Component::Defined(Box::new(enumeration(&["x"])))),
            with(Component::Array(Box::new(Component::Defined(Box::new(
                enumeration(&["x"]),
            ))))),
        ];
        let distinct: BTreeSet<&Pin> = pins.iter().collect();
        assert_eq!(distinct.len(), pins.len(), "{pins:?}");
    }

    #[test]
    fn the_kind_is_part_of_the_pin() {
        let as_enum = Definition {
            body: Body::Enum { labels: Vec::new() },
            ..composite()
        };
        let as_composite = Definition {
            body: Body::Composite(Composite {
                relkind: "c".to_owned(),
                attributes: Vec::new(),
            }),
            ..composite()
        };
        assert_ne!(as_enum.pin(), as_composite.pin());
        assert_eq!(as_enum.kind(), Kind::Enum);
        assert_eq!(domain().kind(), Kind::Domain);
        assert_eq!(as_composite.kind(), Kind::Composite);
    }

    #[test]
    fn owners_are_collected_from_every_nested_type() {
        let mut nested = domain();
        nested.owner = 20;
        let mut outer = composite_of(|c| c.attributes[0].ty = Component::Defined(Box::new(nested)));
        outer.owner = 10;
        let mut ids = BTreeSet::new();
        outer.collect_owners(&mut ids);
        assert_eq!(ids, BTreeSet::from([10, 20]));
        // Negative: an enum nests nothing, and a leaf has no owner to name.
        let mut ids = BTreeSet::new();
        enumeration(&["a"]).collect_owners(&mut ids);
        assert_eq!(ids, BTreeSet::from([10]));
    }

    fn row_of(typtype: &str) -> Option<NotAdmitted> {
        classify(typtype, "X", 0, None, None)
    }

    #[test]
    fn the_three_kinds_are_admitted_and_every_other_is_refused_by_name() {
        assert_eq!(row_of("e"), None);
        assert_eq!(row_of("d"), None);
        assert_eq!(classify("c", "C", 0, Some("c"), None), None);
        assert_eq!(row_of("r"), Some(NotAdmitted::Range));
        assert_eq!(row_of("m"), Some(NotAdmitted::Multirange));
        assert_eq!(row_of("p"), Some(NotAdmitted::Pseudo));
        assert_eq!(
            classify("b", "S", 0, None, Some("citext")),
            Some(NotAdmitted::Base {
                extension: Some("citext".to_owned())
            })
        );
        assert_eq!(
            classify("b", "N", 0, None, None),
            Some(NotAdmitted::Base { extension: None })
        );
        assert_eq!(classify("b", "A", 23, None, None), Some(NotAdmitted::Array));
        for relkind in ["r", "p", "v", "m", "f"] {
            assert_eq!(
                classify("c", "C", 0, Some(relkind), None),
                Some(NotAdmitted::RowType {
                    relkind: relkind.to_owned()
                }),
                "{relkind}"
            );
        }
        // A composite with no relation, and a kind nobody lists, are refused.
        assert!(matches!(
            classify("c", "C", 0, None, None),
            Some(NotAdmitted::Unknown { .. })
        ));
        assert!(matches!(
            row_of("z"),
            Some(NotAdmitted::Unknown { typtype }) if typtype == "z"
        ));
    }

    #[test]
    fn each_outcome_has_its_own_message() {
        let reference = TypeRef::new("app", "mood").unwrap();
        let reads = [
            TypeRead::Absent,
            TypeRead::NotDefined,
            TypeRead::Unreadable(Unreadable::SchemaUsage {
                schema: "app".to_owned(),
            }),
            TypeRead::Unreadable(Unreadable::TypeUsage),
            TypeRead::Unreadable(Unreadable::Failed("connection reset".to_owned())),
            TypeRead::NotAdmitted(NotAdmitted::Range),
            TypeRead::NotAdmitted(NotAdmitted::RowType {
                relkind: "v".to_owned(),
            }),
        ];
        let messages: Vec<String> = reads
            .iter()
            .map(|r| r.refusal(&reference).expect("a refusal"))
            .collect();
        let distinct: BTreeSet<&String> = messages.iter().collect();
        assert_eq!(distinct.len(), messages.len(), "{messages:#?}");
        for message in &messages {
            assert!(message.contains("app.mood"), "{message}");
        }
        // A failed read says it is not an absence; an absence says nothing of
        // the sort.
        assert!(
            messages[4].contains("not an absent type"),
            "{}",
            messages[4]
        );
        assert!(
            !messages[0].contains("could not be read"),
            "{}",
            messages[0]
        );
        assert!(messages[2].contains("USAGE on schema `app`"));
        assert!(messages[3].contains("USAGE on the type"));
        assert!(messages[6].contains("view"));
        assert!(messages[1].contains("shell type"));
    }

    #[test]
    fn a_present_type_is_not_refused() {
        let definition = enumeration(&["a"]);
        let pin = definition.pin();
        let present = TypeRead::Present(Box::new(UserType {
            reference: TypeRef::new("app", "mood").unwrap(),
            owners: BTreeMap::from([(
                10,
                Owner {
                    oid: 10,
                    name: "postgres".to_owned(),
                    trusted: true,
                },
            )]),
            definition,
            reached: Vec::new(),
            pin,
        }));
        assert_eq!(present.refusal(&TypeRef::new("app", "mood").unwrap()), None);
    }

    #[test]
    fn untrusted_owners_and_managed_reach_are_for_the_caller_to_ask() {
        let owner = |oid, name: &str, trusted| Owner {
            oid,
            name: name.to_owned(),
            trusted,
        };
        let definition = domain();
        let pin = definition.pin();
        let user = UserType {
            reference: TypeRef::new("app", "positive").unwrap(),
            owners: BTreeMap::from([
                (10, owner(10, "postgres", true)),
                (30, owner(30, "zed", false)),
                (20, owner(20, "amy", false)),
            ]),
            definition,
            reached: vec![
                Reached {
                    oid: 1,
                    catalog: "pg_proc".to_owned(),
                    kind: "function".to_owned(),
                    schema: Some("app".to_owned()),
                    name: Some("is_positive".to_owned()),
                    identity: "app.is_positive(integer)".to_owned(),
                    owner: Some(20),
                },
                Reached {
                    oid: 2,
                    catalog: "pg_class".to_owned(),
                    kind: "table".to_owned(),
                    schema: Some("app".to_owned()),
                    name: Some("t".to_owned()),
                    identity: "app.t".to_owned(),
                    owner: Some(10),
                },
            ],
            pin,
        };
        assert_eq!(user.kind(), Kind::Domain);
        assert_eq!(user.owner().name, "postgres");
        let untrusted: Vec<&str> = user
            .untrusted_owners()
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(untrusted, ["amy", "zed"]);
        let table = user
            .reaches_managed(|r| r.kind == "table" && r.name.as_deref() == Some("t"))
            .expect("the table is reached");
        assert_eq!(table.identity, "app.t");
        // Negative: nothing the caller does not manage is reported.
        assert!(user.reaches_managed(|_| false).is_none());
        assert!(
            user.reaches_managed(|r| r.name.as_deref() == Some("other"))
                .is_none()
        );
    }

    #[test]
    fn a_list_of_oids_is_an_array_literal_of_integers_only() {
        assert_eq!(oid_array(&[1, 22]), "ARRAY[1, 22]::pg_catalog.oid[]");
        assert_eq!(oid_array(&[]), "ARRAY[]::pg_catalog.oid[]");
    }
}
