//! The SQL Server target capture (#620 (a), #2141): what each object in scope
//! binds, the candidates of the names it resolves when it runs, and who owns
//! it, read from the catalog and sealed under the environment key. The design
//! is `docs/RESOLVER-MSSQL.md` §1–§4 (DEC-619.1, DEC-619.2); the reasons for
//! this module's own choices are DEC-2141.1.
//!
//! It mirrors `pbps-pg`'s capture where the engines agree, and departs where
//! #619 measured that they do not:
//!
//! - **Most references are resolved when a module runs**, not when it is
//!   created (§1). A one-part name in a body that is not schema-bound is
//!   *runtime-bound*: there is no binding to compare, so its module is
//!   recorded as runtime-bound and the name's candidates enter the scope
//!   instead (§2). Only id-bound references — schema-bound ones, the
//!   functions a computed column, DEFAULT or CHECK calls, a two-part name,
//!   and every type — are bindings.
//! - **No snapshot holds the catalog** (§3). Each component is one statement,
//!   read in turn; the capture is compared with the sealed one at the opening
//!   and the closing read (DEC-619.1), which is [`recapture`]'s job. A commit
//!   between two component reads either reaches a later one, and differs
//!   there, or touches only what was already read: the after-last-observation
//!   class SPEC §7.6 excludes.
//!
//! Scratch, separation, the externals comparison and assessment are #2142;
//! nothing here is wired into planning yet (#2143).

mod catalog;
mod columns;
mod manifest;
mod object_properties;
mod ownership;
mod read;
mod references;
mod select;
mod staged;
mod whole;

pub use manifest::CapturedInputs;
pub use object_properties::{OBJECT_PROPERTIES, PropertyDisposition, property_disposition};
pub use ownership::RecordedOwnership;
pub use pbps_db::resolver::capture::{CaptureError, ObjectIdentity, Uncovered};
pub use staged::{
    ExternalInputs, StageError, Staged, cascade_scope, compare_staged, compare_staged_declared,
    required_externals, required_externals_declared,
};

use std::collections::BTreeSet;

/// The canonical property rule every sealed SQL Server prerequisite uses. A
/// change to what a property means, or to which properties are read, is a
/// new rule: an old seal then reads as a version difference, never as a
/// match under different semantics.
pub const RULE: &str = "mssql-capture-v3";

/// The identity classes this capture produces, by the catalog each comes from.
pub mod class {
    /// A schema, by name.
    pub const SCHEMA: &str = "sys.schemas";
    /// A database principal (user or role), by name.
    pub const PRINCIPAL: &str = "sys.database_principals";
    /// Anything `sys.objects` lists, by schema and name. Every kind occupies
    /// a name in its schema (§2): a table, a view, a routine, a synonym and
    /// a constraint can never share one.
    pub const OBJECT: &str = "sys.objects";
    /// A column of an object, by name; its signature is the object.
    pub const COLUMN: &str = "sys.columns";
    /// A type, by schema and name: a user type in its own schema, a system
    /// type in `sys`.
    pub const TYPE: &str = "sys.types";
    /// An XML schema collection, by schema and name, as a module's typed
    /// `xml` variable binds it.
    pub const XML_COLLECTION: &str = "sys.xml_schema_collections";
    /// A partition function, by name (it has no schema), as `$PARTITION`
    /// binds it.
    pub const PARTITION_FUNCTION: &str = "sys.partition_functions";
}

/// Which name a candidate set asks about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum CandidateClass {
    /// Every object of the name in the schema, of any type.
    Object,
    /// Whether `sys` holds a procedure of the name. A one-part `EXEC` looks
    /// there first, so a system procedure is never captured by a user one
    /// (§2): the set's members prove the name is the engine's, its absence
    /// that the call's other candidates apply.
    SystemProcedure,
    /// Every type of the name in the schema. A one-part type in a module
    /// body, such as a local variable's, resolves like a one-part object
    /// name, in the module's schema and then `dbo`, again whenever it is
    /// read (measured on #2141).
    Type,
}

impl CandidateClass {
    fn catalog(self) -> &'static str {
        match self {
            Self::Object => "sys.objects",
            Self::SystemProcedure => "sys.system_objects",
            Self::Type => "sys.types",
        }
    }
}

/// One name in one schema. An empty membership is a required absence, never
/// an omitted read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct CandidateSet {
    pub class: CandidateClass,
    pub schema: String,
    pub name: String,
}

/// The objects to read and the names whose candidates to read. This is a
/// request, never a certificate that it is complete: the closure of what the
/// requested objects bind is added by the capture itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CaptureScope {
    pub retained: BTreeSet<ObjectIdentity>,
    pub candidates: BTreeSet<CandidateSet>,
}

pub use catalog::{CATALOG, Disposition, disposition};

/// The catalog views the capture reads whole (#2153), by name under `sys`.
pub const WHOLE_VIEWS: &[&str] = columns::VIEWS;

/// Whether the capture knows what to do with `column` of `sys.<view>`: a
/// rule names it, or it may be compared as the engine reports it. An id
/// column no rule names is the one it does not, and a capture that meets one
/// refuses; the coverage test asks this of every column a build reports.
pub fn classifies(view: &str, column: &str) -> bool {
    columns::rule(view, column).is_some() || columns::comparable_raw(column)
}

/// The identity of the object `schema.name` in `sys.objects`.
pub fn object(schema: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class::OBJECT.into(),
        name: vec![schema.into(), name.into()],
        signature: Vec::new(),
    }
}

/// The roots the managed declarations of each side name: its tables, modules
/// and types, by the schema and name they are created under. A name one side
/// lacks is simply absent when read; the capture adds each root's children
/// and everything it binds.
pub fn managed_scope(sides: &[&pbps_model::Schema]) -> CaptureScope {
    let mut retained = BTreeSet::new();
    for side in sides {
        retained.extend(side.tables.keys().map(|t| object(&t.schema, &t.name)));
        retained.extend(side.modules.keys().map(|id| object(id.schema(), id.name())));
        retained.extend(side.types.keys().map(|t| ObjectIdentity {
            class: class::TYPE.into(),
            name: vec![t.schema().to_owned(), t.name().to_owned()],
            signature: Vec::new(),
        }));
    }
    CaptureScope {
        retained,
        candidates: BTreeSet::new(),
    }
}

/// Read the objects in `scope`, everything they bind, and the candidates of
/// every name the scope asks about, on the caller's connection.
///
/// The connection's own principal reads: what it may not see, it refuses
/// rather than reads as absent (`docs/RESOLVER-MSSQL.md` §4).
pub async fn capture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
) -> Result<CapturedInputs, CaptureError> {
    capture_declared(connection, scope, &Default::default()).await
}

/// Capture with the explicitly declared cross-database reference boundary.
/// This admits names, not proof of the other database's contents (DEC-2323.1).
pub async fn capture_declared(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    declarations: &pbps_model::dependency::Dependencies,
) -> Result<CapturedInputs, CaptureError> {
    let mut catalog = read::read(connection, &select::lookups(scope)).await?;
    catalog.declarations = declarations.clone();
    let selected = select::select(&catalog, scope)?;
    // Only reached objects are asked about: an unrelated broken module must
    // not turn a scoped capture into a database-wide refusal.
    let schemas: std::collections::BTreeMap<_, _> = catalog
        .schemas
        .iter()
        .map(|s| (s.id, s.name.as_str()))
        .collect();
    let objects: Vec<_> = catalog
        .objects
        .iter()
        .filter(|o| {
            schemas
                .get(&o.schema)
                .is_some_and(|schema| selected.inputs.contains_key(&object(schema, &o.name)))
        })
        .collect();
    catalog.object_properties = object_properties::read(connection, &objects).await?;
    let selected = select::select(&catalog, scope)?;
    Ok(CapturedInputs::new(catalog, selected, scope.clone()))
}

/// [`capture`], then the qualified ownership of each declared object, and the
/// manifest sealed under the environment's key. The key and the recorded
/// identities are fixed here, at the producer of the fresh read.
pub async fn capture_qualified(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    recorded: &RecordedOwnership<'_>,
) -> Result<(CapturedInputs, pbps_model::resolver::InputManifest), CaptureError> {
    capture_qualified_declared(connection, scope, &Default::default(), key, recorded).await
}

/// [`capture_qualified`] with the declared cross-database reference
/// boundary (DEC-2323.1), so a module naming a declared database captures
/// as the declaration admits it and not as an uncovered name.
pub async fn capture_qualified_declared(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &CaptureScope,
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    recorded: &RecordedOwnership<'_>,
) -> Result<(CapturedInputs, pbps_model::resolver::InputManifest), CaptureError> {
    let captured = capture_declared(connection, scope, declarations).await?;
    captured
        .require_runtime_candidates()
        .map_err(CaptureError::Coverage)?;
    let ownership = ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    let manifest = captured
        .seal(key, &ownership)
        .map_err(|_| CaptureError::Incomplete)?;
    Ok((captured, manifest))
}

/// A fresh capture of `previous`'s scope, and how it differs. Cached rows
/// cannot answer whether a candidate arrived or a property changed, so every
/// recheck reads again (DEC-619.1).
pub async fn recapture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    previous: &CapturedInputs,
) -> Result<
    (
        CapturedInputs,
        Vec<pbps_db::resolver::capture::CaptureDifference>,
    ),
    CaptureError,
> {
    let current = capture_declared(connection, previous.scope(), &previous.declarations).await?;
    let differences = previous.compare(&current);
    Ok((current, differences))
}

#[cfg(test)]
mod tests;
