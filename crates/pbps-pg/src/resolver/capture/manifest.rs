//! Private complete input records and versioned cryptographic comparison.
//! No external verifier, source, or derived checksum is an ordinary report.

use super::{CaptureScope, Uncovered, bindings::Binding, properties, read, scope};
use pbps_db::fingerprint::FingerprintKey;
use pbps_db::resolver::capture::{CaptureDifference, InputChange, ObjectIdentity};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Input {
    pub(super) properties: BTreeMap<String, Value>,
    pub(super) bindings: Vec<Binding>,
}

/// Private in-memory catalog evidence. It intentionally has no Debug,
/// Serialize, digest getter, persistence constructor or verified flag.
/// Executable/environment qualification is still required by the lifecycle.
/// Receiving ordinary evidence does not grant its producer's native-input
/// capability (DEC-974.1). In particular, caller-chosen mappings cannot probe it:
/// ```compile_fail,E0624
/// use pbps_pg::resolver::capture::{CapturedInputs, NativeLibrary};
/// use std::{fs::File, path::Path};
/// fn guess(captured: &CapturedInputs, name: String) -> bool {
///     let inputs = captured.runtime_inputs().unwrap();
///     let resolved = inputs.resolve_native(Path::new("/bin/postgres"), Path::new("/"));
///     let root = File::open("/dev/null").unwrap();
///     (0..resolved.len()).any(|i| matches!(resolved.open(i, &root, &[name.clone()]),
///         Ok(NativeLibrary::Mapped { .. })))
/// }
/// ```
/// Neither crafted roots nor selected-candidate positions can reveal its names:
/// ```compile_fail,E0624
/// use pbps_pg::resolver::capture::{CapturedInputs, NativeLibrary};
/// use std::{fs::File, path::Path};
/// fn probe(captured: &CapturedInputs, crafted_root: &File) -> Option<(usize, Vec<u8>)> {
///     let inputs = captured.runtime_inputs().unwrap();
///     let resolved = inputs.resolve_native(Path::new("/bin/postgres"), Path::new("/"));
///     for i in 0..resolved.len() {
///         if let Ok(NativeLibrary::Candidate { candidate, reader }) = resolved.open(i, crafted_root, &[]) {
///             let mut content = Vec::new();
///             reader.read_to_end(&mut content).ok()?;
///             return Some((candidate, content));
///         }
///     }
///     None
/// }
/// ```
/// The capability cannot be recovered for later probing either:
/// ```compile_fail,E0624
/// use pbps_pg::resolver::capture::{CapturedInputs, RuntimeInputs};
/// fn acquire(captured: &CapturedInputs) -> RuntimeInputs {
///     captured.runtime_inputs().unwrap()
/// }
/// ```
/// A result consumer cannot select a known key and test guesses of private
/// properties. Only a fresh catalog read grants sealing authority:
/// ```compile_fail,E0624
/// use pbps_pg::resolver::capture::CapturedInputs;
/// use pbps_db::fingerprint::EnvironmentFingerprintKey;
/// fn probe(captured: &CapturedInputs, chosen: &EnvironmentFingerprintKey) {
///     let _ = captured.seal(chosen);
/// }
/// ```
pub struct CapturedInputs {
    baseline: super::baseline::Baseline,
    session: super::session::Facts,
    rule: &'static str,
    major: u32,
    scope: CaptureScope,
    pub(super) inputs: BTreeMap<ObjectIdentity, Input>,
    pub(super) candidates: BTreeMap<super::CandidateSet, BTreeSet<ObjectIdentity>>,
    pub(super) limitations: BTreeSet<ObjectIdentity>,
    /// What each requested dropped signature named in this snapshot.
    dropped: BTreeMap<super::DroppedSignature, Option<ObjectIdentity>>,
}

impl CapturedInputs {
    /// Persistable catalog facts, keyed by the target environment. This is
    /// still catalog evidence only: it does not confer runtime qualification.
    /// No property value, including an external definition's literals, crosses
    /// this boundary. The process-key comparison API cannot supply this key.
    /// Only the producer of a fresh read may seal private inputs. An ordinary
    /// recipient must not test guesses by supplying a known key (DEC-974.1).
    /// ```compile_fail,E0624
    /// use pbps_pg::resolver::capture::CapturedInputs;
    /// use pbps_db::fingerprint::FingerprintKey;
    /// fn persist(captured: &CapturedInputs) {
    ///     let _ = captured.seal(FingerprintKey::process());
    /// }
    /// ```
    pub(super) fn seal(
        &self,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        use pbps_model::resolver::{
            Binding, CandidateSet, InputManifest, Membership, Prerequisite, ReadScope,
            RoutineLookup,
        };
        let digest = |component: &str, bytes: Vec<u8>| -> String {
            key.fingerprint(self.rule, component, &bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let candidate = |q: &super::CandidateSet| CandidateSet {
            class: q.class.catalog().into(),
            namespace: q.namespace.clone(),
            name: q.name.clone(),
        };
        let mut membership: Vec<_> = self
            .candidates
            .iter()
            .map(|(q, members)| Membership {
                predicate: candidate(q),
                members: members.clone(),
            })
            .collect();
        membership.sort_by(|a, b| a.predicate.cmp(&b.predicate));
        InputManifest::new(
            self.rule.into(),
            self.major,
            key.id().as_str().into(),
            ReadScope {
                retained: self.scope.retained.clone(),
                candidates: self.scope.candidates.iter().map(candidate).collect(),
            },
            digest(
                "baseline",
                serde_json::to_vec(&self.baseline).expect("baseline serializes"),
            ),
            digest(
                "session",
                serde_json::to_vec(&self.session).expect("session serializes"),
            ),
            self.inputs
                .iter()
                .map(|(object, input)| {
                    let mut bindings: Vec<_> = input
                        .bindings
                        .iter()
                        .map(|b| Binding {
                            node: b.node.clone(),
                            path: b.path.clone(),
                            target: b.target.clone(),
                        })
                        .collect();
                    bindings.sort();
                    Prerequisite {
                        object: object.clone(),
                        canonicalization: self.rule.into(),
                        properties: digest(
                            "properties",
                            serde_json::to_vec(&input.properties)
                                .expect("canonical properties serialize"),
                        ),
                        bindings,
                    }
                })
                .collect(),
            membership,
            self.limitations.clone(),
            self.dropped
                .iter()
                .map(|(query, resolved)| RoutineLookup {
                    signature: query.spelled.clone(),
                    search_path: query.path.clone(),
                    kind: query.kind.into(),
                    resolved: resolved.clone(),
                })
                .collect(),
        )
    }

    /// What each requested dropped signature named in this capture's
    /// snapshot: the routine the plan's `DROP` of it would address.
    pub fn dropped(&self) -> &BTreeMap<super::DroppedSignature, Option<ObjectIdentity>> {
        &self.dropped
    }

    pub fn scope(&self) -> &CaptureScope {
        &self.scope
    }

    // Only the producer of a fresh read may obtain the source capability.
    // Making this public lets an ordinary result recipient guess names through
    // mapped equality, crafted-root opens and candidate positions (DEC-974.1).
    pub(super) fn runtime_inputs(&self) -> Result<super::RuntimeInputs, Uncovered> {
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
        if self.candidates != current.candidates
            || self.limitations != current.limitations
            || self.dropped != current.dropped
        {
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
    // Keyed (DEC-952.1): these digests are compared and dropped within the
    // process, so the process key serves, and no bare SHA-256 over a private
    // input exists to test guesses against. A digest that is ever kept must
    // be made under the environment's key instead (#614).
    FingerprintKey::process().fingerprint(
        rule,
        component,
        &serde_json::to_vec(input).expect("canonical private input serializes"),
    )
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
        dropped: read.dropped,
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
