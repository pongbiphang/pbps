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

mod manifest;
mod ownership;
mod read;
mod select;

pub use manifest::CapturedInputs;
pub use ownership::RecordedOwnership;
pub use pbps_db::resolver::capture::{CaptureError, ObjectIdentity, Uncovered};

use std::collections::BTreeSet;

/// The canonical property rule every sealed SQL Server prerequisite uses. A
/// change to what a property means, or to which properties are read, is a
/// new rule: an old seal then reads as a version difference, never as a
/// match under different semantics.
pub const RULE: &str = "mssql-capture-v1";

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

/// The identity of the object `schema.name` in `sys.objects`.
pub fn object(schema: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class::OBJECT.into(),
        name: vec![schema.into(), name.into()],
        signature: Vec::new(),
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
    let catalog = read::read(connection, &select::lookups(scope)).await?;
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
    let captured = capture(connection, scope).await?;
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
    let current = capture(connection, previous.scope()).await?;
    let differences = previous.compare(&current);
    Ok((current, differences))
}

#[cfg(test)]
mod tests;
