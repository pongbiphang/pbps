//! The captured inputs, their comparison, and their seal under the
//! environment's key, in the shape of `pbps-pg`'s capture manifest.

use super::read::{Baseline, Catalog, Session};
use super::select::{Input, RuntimeName, Selected};
use super::{CandidateSet, CaptureScope, ObjectIdentity, RULE};
use pbps_db::fingerprint::FingerprintKey;
use pbps_db::resolver::capture::{CaptureDifference, InputChange};
use std::collections::{BTreeMap, BTreeSet};

/// Private in-memory catalog evidence. Like `pbps-pg`'s, it has no Debug,
/// Serialize or digest getter: a definition read here leaves only as a keyed
/// fingerprint, and only its producer seals it.
pub struct CapturedInputs {
    major: u32,
    session: Session,
    baseline: Baseline,
    scope: CaptureScope,
    pub(super) inputs: BTreeMap<ObjectIdentity, Input>,
    candidates: BTreeMap<CandidateSet, BTreeSet<ObjectIdentity>>,
    runtime: BTreeMap<ObjectIdentity, BTreeSet<RuntimeName>>,
}

impl CapturedInputs {
    /// Engine-private rendering facts. Definitions remain inside the engine;
    /// the renderer consumes structure and never reads a module's source.
    pub(crate) fn properties(
        &self,
        id: &ObjectIdentity,
    ) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
        self.inputs.get(id).map(|input| &input.properties)
    }
    /// Effective owners already resolved by `select::Index::owner`: explicit,
    /// then parent, then schema. Only authorization metadata leaves here.
    pub(crate) fn effective_owners(
        &self,
    ) -> Result<BTreeMap<ObjectIdentity, pbps_model::RoleName>, super::Uncovered> {
        self.inputs
            .iter()
            .filter(|(id, input)| {
                [super::class::SCHEMA, super::class::OBJECT].contains(&id.class.as_str())
                    && input.properties.get("engine") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(id, input)| {
                let owner: ObjectIdentity = input
                    .properties
                    .get("owner")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .ok_or_else(|| {
                        super::Uncovered::object(id, "the effective owner was not captured")
                    })?;
                match (owner.class.as_str(), owner.name.as_slice()) {
                    (super::class::PRINCIPAL, [name]) => Ok((id.clone(), name.clone().into())),
                    _ => Err(super::Uncovered::object(
                        id,
                        "the effective owner is not a principal",
                    )),
                }
            })
            .collect()
    }

    pub(super) fn new(catalog: Catalog, selected: Selected, scope: CaptureScope) -> Self {
        Self {
            major: catalog.major,
            session: catalog.session,
            baseline: catalog.baseline,
            scope,
            inputs: selected.inputs,
            candidates: selected.candidates,
            runtime: selected.runtime,
        }
    }

    pub fn scope(&self) -> &CaptureScope {
        &self.scope
    }

    pub fn objects(&self) -> impl Iterator<Item = &ObjectIdentity> {
        self.inputs.keys()
    }

    /// What one object's id-bound references name, in the order sealed.
    pub fn bound_objects(
        &self,
        object: &ObjectIdentity,
    ) -> Option<impl Iterator<Item = &ObjectIdentity>> {
        self.inputs
            .get(object)
            .map(|input| input.bindings.iter().map(|binding| &binding.target))
    }

    /// The modules with a name resolved only when they run. Their id-bound
    /// references and properties are covered; such a name is not a binding
    /// any rebuild could pin (§2), so its candidates stand for it.
    pub fn runtime_bound(&self) -> impl Iterator<Item = &ObjectIdentity> {
        self.runtime.keys()
    }

    /// The members of one requested candidate set, if it was requested.
    pub fn members(&self, candidate: &CandidateSet) -> Option<&BTreeSet<ObjectIdentity>> {
        self.candidates.get(candidate)
    }

    /// This scope, with the candidate sets every captured module's
    /// runtime-bound names need. A capture of the target under the result
    /// reads what each name would find there.
    pub fn runtime_scope(&self) -> CaptureScope {
        let mut scope = self.scope.clone();
        scope.candidates.extend(super::select::runtime_candidates(
            &self.inputs,
            &self.runtime,
        ));
        scope
    }

    /// Refuse a capture whose scope lacks a set its own runtime-bound names
    /// need. Sealed so, a later recapture would reuse the short scope, and an
    /// object arriving under the omitted name would rebind the module with no
    /// difference to show. A set asked and found empty is a required absence;
    /// one never asked is no answer at all.
    pub(super) fn require_runtime_candidates(&self) -> Result<(), super::Uncovered> {
        for (module, names) in &self.runtime {
            let one = BTreeMap::from([(module.clone(), names.clone())]);
            let needed = super::select::runtime_candidates(&self.inputs, &one);
            if !needed.is_subset(&self.scope.candidates) {
                return Err(super::Uncovered::object(
                    module,
                    "a runtime-bound name's candidates are not in the scope; capture under runtime_scope() first",
                ));
            }
        }
        Ok(())
    }

    pub fn compare(&self, current: &Self) -> Vec<CaptureDifference> {
        let whole = |change| CaptureDifference {
            object: None,
            change,
        };
        if self.major != current.major {
            return vec![whole(InputChange::Version)];
        }
        if self.scope != current.scope {
            return vec![whole(InputChange::Scope)];
        }
        let mut differences = Vec::new();
        if fingerprint("session", &self.session) != fingerprint("session", &current.session) {
            differences.push(whole(InputChange::Environment));
        }
        if fingerprint("baseline", &self.baseline) != fingerprint("baseline", &current.baseline) {
            differences.push(whole(InputChange::Baseline));
        }
        for object in self
            .inputs
            .keys()
            .chain(current.inputs.keys())
            .collect::<BTreeSet<_>>()
        {
            let change = match (self.inputs.get(object), current.inputs.get(object)) {
                (None, Some(_)) => Some(InputChange::Added),
                (Some(_), None) => Some(InputChange::Removed),
                (Some(old), Some(new)) => {
                    if fingerprint("properties", &old.properties)
                        != fingerprint("properties", &new.properties)
                    {
                        Some(InputChange::Properties)
                    } else if old.bindings != new.bindings {
                        Some(InputChange::Bindings)
                    } else {
                        None
                    }
                }
                (None, None) => unreachable!("union of input keys"),
            };
            if let Some(change) = change {
                differences.push(CaptureDifference {
                    object: Some(object.clone()),
                    change,
                });
            }
        }
        if self.candidates != current.candidates || self.runtime != current.runtime {
            differences.push(whole(InputChange::Membership));
        }
        differences
    }

    /// Persistable catalog facts under the target environment's key. No
    /// property value, a definition included, crosses this boundary: each
    /// object's properties leave as one keyed digest.
    pub(super) fn seal(
        &self,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        ownership: &BTreeMap<ObjectIdentity, pbps_model::resolver::ObjectOwnership>,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        use pbps_model::resolver::{
            Binding, CandidateSet as Predicate, InputManifest, Membership, ObjectOwnership,
            Prerequisite, ReadScope,
        };
        let digest = |component: &str, bytes: Vec<u8>| -> String {
            key.fingerprint(RULE, component, &bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let predicate = |q: &CandidateSet| Predicate {
            class: q.class.catalog().into(),
            namespace: Some(q.schema.clone()),
            name: Some(q.name.clone()),
        };
        let mut membership: Vec<Membership> = self
            .candidates
            .iter()
            .map(|(q, members)| Membership {
                predicate: predicate(q),
                members: members.clone(),
            })
            .collect();
        membership.sort_by(|a, b| a.predicate.cmp(&b.predicate));
        let prerequisites: Vec<Prerequisite> = self
            .inputs
            .iter()
            .map(|(object, input)| Prerequisite {
                object: object.clone(),
                ownership: ownership
                    .get(object)
                    .cloned()
                    .unwrap_or(ObjectOwnership::Unqualified),
                canonicalization: RULE.into(),
                properties: digest(
                    "properties",
                    serde_json::to_vec(&input.properties).expect("canonical properties serialize"),
                ),
                // A BTreeSet: already in the order the manifest requires.
                bindings: input
                    .bindings
                    .iter()
                    .map(|binding| Binding {
                        node: binding.node.clone(),
                        path: binding.path.clone(),
                        target: binding.target.clone(),
                    })
                    .collect(),
            })
            .collect();
        InputManifest::new(
            RULE.into(),
            self.major,
            key.id().as_str().into(),
            ReadScope {
                retained: self.scope.retained.clone(),
                candidates: self.scope.candidates.iter().map(predicate).collect(),
            },
            digest(
                "baseline",
                serde_json::to_vec(&self.baseline).expect("baseline serializes"),
            ),
            digest(
                "session",
                serde_json::to_vec(&self.session).expect("session serializes"),
            ),
            prerequisites,
            membership,
            self.runtime.keys().cloned().collect(),
            Vec::new(),
        )
    }
}

fn fingerprint(component: &str, input: &impl serde::Serialize) -> [u8; 32] {
    // Compared and dropped within the process, so the process key serves
    // (DEC-952.1), and no bare digest of a private input exists to test
    // guesses against.
    FingerprintKey::process().fingerprint(
        RULE,
        component,
        &serde_json::to_vec(input).expect("canonical private input serializes"),
    )
}
