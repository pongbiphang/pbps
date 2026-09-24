//! Private complete input records and versioned cryptographic comparison.
//! No external verifier, source, or derived checksum is an ordinary report.

use super::{CaptureScope, Uncovered, bindings::Binding, properties, read, scope};
use pbps_db::resolver::capture::{CaptureDifference, InputChange, ObjectIdentity};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

struct Input {
    properties: BTreeMap<String, Value>,
    bindings: Vec<Binding>,
}

/// Private in-memory catalog evidence. It intentionally has no Debug,
/// Serialize, digest getter, persistence constructor or verified flag.
/// Executable/environment qualification is still required by the lifecycle.
pub struct CapturedInputs {
    baseline: super::baseline::Baseline,
    session: super::session::Facts,
    rule: &'static str,
    major: u32,
    scope: CaptureScope,
    inputs: BTreeMap<ObjectIdentity, Input>,
    candidates: BTreeMap<super::CandidateSet, BTreeSet<ObjectIdentity>>,
    limitations: BTreeSet<ObjectIdentity>,
}

impl CapturedInputs {
    pub fn scope(&self) -> &CaptureScope {
        &self.scope
    }

    /// Names the native lifecycle must resolve against the connected backend,
    /// including non-extension C routines. No reported version certifies these
    /// executable bytes. This private transfer has no diagnostic serialization.
    pub fn runtime_inputs(&self) -> Result<super::RuntimeInputs, Uncovered> {
        let mut libraries = BTreeSet::new();
        for (object, input) in &self.inputs {
            if object.class == "pg_proc"
                && input
                    .properties
                    .get("prolang")
                    .and_then(|language| language.get("name"))
                    == Some(&serde_json::json!(["c"]))
            {
                let library = input
                    .properties
                    .get("probin")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        Uncovered::object(object, "required native library is unreadable")
                    })?;
                libraries.insert(library.to_owned());
            }
        }
        for setting in [
            "shared_preload_libraries",
            "session_preload_libraries",
            "local_preload_libraries",
        ] {
            let value = self.session.settings.get(setting).ok_or_else(|| {
                Uncovered::class(
                    "session-environment",
                    "required preload setting is unreadable",
                )
            })?;
            let names =
                pbps_db::resolver::environment::guc_list(&value.value).ok_or_else(|| {
                    Uncovered::class(
                        "session-environment",
                        "required preload list is unqualified",
                    )
                })?;
            for name in names {
                libraries.insert(
                    if setting == "local_preload_libraries" && !name.contains('/') {
                        format!("$libdir/plugins/{name}")
                    } else {
                        name
                    },
                );
            }
        }
        let dynamic_library_path = self
            .session
            .settings
            .get("dynamic_library_path")
            .ok_or_else(|| {
                Uncovered::class("session-environment", "library search path is unreadable")
            })?
            .value
            .clone();
        Ok(super::RuntimeInputs {
            libraries: libraries.into_iter().collect(),
            dynamic_library_path,
        })
    }

    pub fn objects(&self) -> impl Iterator<Item = &ObjectIdentity> {
        self.inputs.keys()
    }

    /// The observed creation-time targets of one catalog surface. A view's
    /// surface is its logical pg_rewrite rule, not a freshly bootstrapped view.
    pub fn bound_objects(
        &self,
        object: &ObjectIdentity,
    ) -> Option<impl Iterator<Item = &ObjectIdentity>> {
        self.inputs
            .get(object)
            .map(|input| input.bindings.iter().map(|binding| &binding.target))
    }

    /// Routines whose body is runtime-bound. Only their creation-time
    /// header/defaults are covered; this is never an empty body-binding proof.
    pub fn runtime_bound_bodies(&self) -> &BTreeSet<ObjectIdentity> {
        &self.limitations
    }

    pub fn compare(&self, current: &Self) -> Vec<CaptureDifference> {
        if self.rule != properties::RULE || current.rule != self.rule || self.major != current.major
        {
            return vec![CaptureDifference {
                object: None,
                change: InputChange::Version,
            }];
        }
        if self.scope != current.scope {
            return vec![CaptureDifference {
                object: None,
                change: InputChange::Scope,
            }];
        }
        let mut differences = Vec::new();
        if fingerprint(self.rule, "session", &self.session)
            != fingerprint(current.rule, "session", &current.session)
        {
            differences.push(CaptureDifference {
                object: None,
                change: InputChange::Environment,
            });
        }
        if fingerprint(self.rule, "baseline", &self.baseline)
            != fingerprint(current.rule, "baseline", &current.baseline)
        {
            differences.push(CaptureDifference {
                object: None,
                change: InputChange::Baseline,
            });
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
                    if fingerprint(self.rule, "properties", &old.properties)
                        != fingerprint(current.rule, "properties", &new.properties)
                    {
                        Some(InputChange::Properties)
                    } else if fingerprint(self.rule, "bindings", &old.bindings)
                        != fingerprint(current.rule, "bindings", &new.bindings)
                    {
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
        if self.candidates != current.candidates || self.limitations != current.limitations {
            differences.push(CaptureDifference {
                object: None,
                change: InputChange::Membership,
            });
        }
        differences
    }
}

fn fingerprint(rule: &str, component: &str, input: &impl serde::Serialize) -> [u8; 32] {
    // JSON encodes only normalized, deterministic maps/ordered arrays. This
    // is byte identity under a versioned rule, not guessed SQL equivalence.
    // Length framing keeps domain labels distinct from private input bytes.
    let mut hash = Sha256::new();
    for bytes in [rule.as_bytes(), component.as_bytes()] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hash.update(serde_json::to_vec(input).expect("canonical private input serializes"));
    hash.finalize().into()
}

pub(super) fn finish(
    read: read::Read,
    prepared: scope::Prepared,
    scope: CaptureScope,
) -> Result<CapturedInputs, Uncovered> {
    let mut inputs = BTreeMap::new();
    for (object, locator) in prepared.members {
        let row = &read.catalog.rows[locator.class][locator.index];
        // Rows in the two passes need not have the same iteration order.
        // Lookup by logical identity, not by array position from the raw pass.
        let row = if read.catalog.identity(locator.class, row).as_ref() == Ok(&object) {
            row
        } else {
            read.catalog.rows[locator.class]
                .iter()
                .find(|row| read.catalog.identity(locator.class, row).as_ref() == Ok(&object))
                .ok_or_else(|| {
                    Uncovered::object(&object, "selected object is missing from rendered input")
                })?
        };
        let properties = properties::normalize(&read.catalog, locator.class, row, read.major)
            .map_err(|_| Uncovered::object(&object, "incomplete canonical properties"))?;
        let bindings = prepared
            .bindings
            .get(&object)
            .cloned()
            .ok_or_else(|| Uncovered::object(&object, "missing binding coverage"))?;
        inputs.insert(
            object,
            Input {
                properties,
                bindings,
            },
        );
    }
    for (object, properties) in prepared.addresses {
        if inputs
            .insert(
                object.clone(),
                Input {
                    properties,
                    bindings: Vec::new(),
                },
            )
            .is_some()
        {
            return Err(Uncovered::object(&object, "duplicate dependency identity"));
        }
    }
    Ok(CapturedInputs {
        session: read.session,
        baseline: read.baseline,
        rule: properties::RULE,
        major: read.major,
        scope,
        inputs,
        candidates: prepared.candidates,
        limitations: prepared.limitations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_properties_and_their_rule_affect_private_verifiers() {
        let before = BTreeMap::from([("context", json!("assignment"))]);
        let after = BTreeMap::from([("context", json!("implicit"))]);
        assert!(
            fingerprint(properties::RULE, "properties", &before)
                != fingerprint(properties::RULE, "properties", &after)
        );
        assert!(
            fingerprint(properties::RULE, "properties", &before)
                != fingerprint("unsupported-rule", "properties", &before)
        );
        assert!(
            fingerprint(properties::RULE, "properties", &before)
                != fingerprint(properties::RULE, "bindings", &before)
        );
        assert!(
            fingerprint(properties::RULE, "properties", &before)
                == fingerprint(properties::RULE, "properties", &before)
        );
    }
}
