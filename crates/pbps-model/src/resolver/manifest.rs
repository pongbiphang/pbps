use super::ObjectOwnership;
use std::collections::BTreeSet;

/// A logical address, independent of any particular database's object numbers.
/// Components stay separate: a dot inside an identifier is not qualification.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectIdentity {
    pub class: String,
    pub name: Vec<String>,
    pub signature: Vec<ObjectIdentity>,
}

/// One occurrence in an engine-observed binding tree. Keeping its node/path
/// distinguishes swapping two same-typed arguments from an unchanged target set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub node: String,
    pub path: Vec<String>,
    pub target: ObjectIdentity,
}

/// All overloads of a name, all names in a namespace, or the whole class.
/// An empty membership is a required absence, never an omitted predicate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSet {
    pub class: String,
    #[serde(deserialize_with = "required_option")]
    pub namespace: Option<String>,
    #[serde(deserialize_with = "required_option")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadScope {
    pub retained: BTreeSet<ObjectIdentity>,
    pub candidates: BTreeSet<CandidateSet>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Membership {
    pub predicate: CandidateSet,
    pub members: BTreeSet<ObjectIdentity>,
}

/// The binding of a DROP signature under its own write path, observed in the
/// same snapshot as candidates. This is a typed lookup, never executable SQL.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineLookup {
    pub signature: String,
    pub search_path: String,
    pub kind: String,
    #[serde(deserialize_with = "required_option")]
    pub resolved: Option<ObjectIdentity>,
}

fn required_option<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    <Option<T> as serde::Deserialize>::deserialize(d)
}

/// A closing record of an object the plan itself installs. Only its identity
/// and bindings are expected; its properties are the managed revalidation's
/// to check, so it carries no fingerprint (DEC-1274.1).
pub const MANAGED_CLOSING: &str = "managed-closing-v1";
const NO_FINGERPRINT: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Complete properties under `canonicalization`, including definitions and
/// literals, reduced by the adapter to an environment-keyed HMAC. Bindings are
/// logical addresses only. Neither a raw property map nor SQL can be stored here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prerequisite {
    pub object: ObjectIdentity,
    pub ownership: ObjectOwnership,
    pub canonicalization: String,
    pub properties: String,
    pub bindings: Vec<Binding>,
}

impl Prerequisite {
    /// The closing placeholder of a record the plan installs.
    pub fn managed_closing(&self) -> Self {
        Self {
            object: self.object.clone(),
            ownership: self.ownership.clone(),
            canonicalization: MANAGED_CLOSING.into(),
            properties: NO_FINGERPRINT.into(),
            bindings: self.bindings.clone(),
        }
    }

    pub fn is_managed_closing(&self) -> bool {
        self.canonicalization == MANAGED_CLOSING
    }
}

/// Required, complete catalog input for one coherent observation. Fields have
/// no serde defaults: unreadable and absent evidence cannot become an empty set.
/// Construction and deserialization apply the same structural checks; adapter
/// version support and scope derivation remain the engine reader's obligations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ManifestWire")]
pub struct InputManifest {
    version: u32,
    adapter: String,
    engine_major: u32,
    key_id: String,
    scope: ReadScope,
    baseline: String,
    session: String,
    prerequisites: Vec<Prerequisite>,
    membership: Vec<Membership>,
    runtime_bound: BTreeSet<ObjectIdentity>,
    identifications: Vec<RoutineLookup>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestWire {
    version: u32,
    adapter: String,
    engine_major: u32,
    key_id: String,
    scope: ReadScope,
    baseline: String,
    session: String,
    prerequisites: Vec<Prerequisite>,
    membership: Vec<Membership>,
    runtime_bound: BTreeSet<ObjectIdentity>,
    identifications: Vec<RoutineLookup>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("unsupported resolver manifest format")]
    Version,
    #[error("resolver evidence has an invalid identity or fingerprint")]
    Invalid,
    #[error("resolver evidence contains duplicate or unordered records")]
    Order,
    #[error("resolver evidence omits a required prerequisite or membership predicate")]
    Incomplete,
}

impl TryFrom<ManifestWire> for InputManifest {
    type Error = ManifestError;
    fn try_from(w: ManifestWire) -> Result<Self, Self::Error> {
        let result = Self {
            version: w.version,
            adapter: w.adapter,
            engine_major: w.engine_major,
            key_id: w.key_id,
            scope: w.scope,
            baseline: w.baseline,
            session: w.session,
            prerequisites: w.prerequisites,
            membership: w.membership,
            runtime_bound: w.runtime_bound,
            identifications: w.identifications,
        };
        result.validate()?;
        Ok(result)
    }
}

impl InputManifest {
    /// Seal already-fingerprinted adapter output. The adapter must use the
    /// target environment's key, not a process-local comparison key.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        adapter: String,
        engine_major: u32,
        key_id: String,
        scope: ReadScope,
        baseline: String,
        session: String,
        prerequisites: Vec<Prerequisite>,
        membership: Vec<Membership>,
        runtime_bound: BTreeSet<ObjectIdentity>,
        identifications: Vec<RoutineLookup>,
    ) -> Result<Self, ManifestError> {
        Self::try_from(ManifestWire {
            version: 1,
            adapter,
            engine_major,
            key_id,
            scope,
            baseline,
            session,
            prerequisites,
            membership,
            runtime_bound,
            identifications,
        })
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }
    pub fn engine_major(&self) -> u32 {
        self.engine_major
    }
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
    pub fn scope(&self) -> &ReadScope {
        &self.scope
    }
    pub fn baseline(&self) -> &str {
        &self.baseline
    }
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn prerequisites(&self) -> &[Prerequisite] {
        &self.prerequisites
    }
    pub fn membership(&self) -> &[Membership] {
        &self.membership
    }
    pub fn runtime_bound(&self) -> &BTreeSet<ObjectIdentity> {
        &self.runtime_bound
    }

    pub fn identifications(&self) -> &[RoutineLookup] {
        &self.identifications
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.version != 1 {
            return Err(ManifestError::Version);
        }
        if self.adapter.is_empty()
            || self.engine_major == 0
            || !hex(&self.key_id, 16)
            || !hex(&self.baseline, 64)
            || !hex(&self.session, 64)
        {
            return Err(ManifestError::Invalid);
        }
        if !self
            .prerequisites
            .windows(2)
            .all(|p| p[0].object < p[1].object)
            || !self
                .membership
                .windows(2)
                .all(|p| p[0].predicate < p[1].predicate)
        {
            return Err(ManifestError::Order);
        }
        let objects: BTreeSet<_> = self.prerequisites.iter().map(|p| &p.object).collect();
        let predicates: BTreeSet<_> = self.membership.iter().map(|p| &p.predicate).collect();
        if predicates != self.scope.candidates.iter().collect()
            || !self.scope.retained.iter().all(|o| objects.contains(o))
            || !self.runtime_bound.iter().all(|o| objects.contains(o))
        {
            return Err(ManifestError::Incomplete);
        }
        if !self.identifications.windows(2).all(|p| {
            (&p[0].signature, &p[0].search_path, &p[0].kind)
                < (&p[1].signature, &p[1].search_path, &p[1].kind)
        }) {
            return Err(ManifestError::Order);
        }
        for q in &self.identifications {
            if q.signature.is_empty() || q.search_path.is_empty() || q.kind.is_empty() {
                return Err(ManifestError::Invalid);
            }
            if q.resolved.as_ref().is_some_and(|o| !objects.contains(o)) {
                return Err(ManifestError::Incomplete);
            }
        }
        for p in &self.prerequisites {
            let placeholder =
                p.canonicalization == MANAGED_CLOSING && p.properties == NO_FINGERPRINT;
            if !identity(&p.object)
                || (p.canonicalization != self.adapter && !placeholder)
                || !hex(&p.properties, 64)
                || p.bindings
                    .iter()
                    .any(|b| b.node.is_empty() || !identity(&b.target))
            {
                return Err(ManifestError::Invalid);
            }
            if !p.bindings.windows(2).all(|b| b[0] < b[1]) {
                return Err(ManifestError::Order);
            }
            if !p.bindings.iter().all(|b| objects.contains(&b.target)) {
                return Err(ManifestError::Incomplete);
            }
        }
        for m in &self.membership {
            if m.predicate.class.is_empty()
                || m.predicate.namespace.as_ref().is_some_and(String::is_empty)
                || m.predicate.name.as_ref().is_some_and(String::is_empty)
            {
                return Err(ManifestError::Invalid);
            }
            if !m.members.iter().all(|o| objects.contains(o)) {
                return Err(ManifestError::Incomplete);
            }
        }
        Ok(())
    }
}

pub(super) fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn identity(o: &ObjectIdentity) -> bool {
    !o.class.is_empty()
        && (!o.name.is_empty() || !o.signature.is_empty())
        && o.name.iter().all(|s| !s.is_empty())
        && o.signature.iter().all(component)
}

#[cfg(test)]
mod tests;

// A missing optional engine subobject is represented by its class and no name
// inside a composite address (for example a table CHECK's absent domain).
fn component(o: &ObjectIdentity) -> bool {
    !o.class.is_empty() && o.name.iter().all(|s| !s.is_empty()) && o.signature.iter().all(component)
}
