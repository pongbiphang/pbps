//! SQL Server creation-time bindings, using the same verdicts as PostgreSQL.
//! Runtime-bound bodies have candidate sets, never invented static bindings.

use super::{CaptureScope, CapturedInputs, ObjectIdentity, Staged};
use pbps_db::resolver::capture::{Assessment, Verdict};
use pbps_model::resolver::ObjectOwnership;
use std::collections::{BTreeMap, BTreeSet};

fn own(
    capture: &CapturedInputs,
    object: &ObjectIdentity,
    roots: &BTreeSet<ObjectIdentity>,
) -> bool {
    let mut current = object.clone();
    let mut seen = BTreeSet::new();
    while seen.insert(current.clone()) {
        if roots.contains(&current) {
            return true;
        }
        let parent = current.signature.first().cloned().or_else(|| {
            capture
                .inputs
                .get(&current)
                .and_then(|i| i.properties.get("parent"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
        });
        match parent {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

pub(super) fn assess(
    target: &CapturedInputs,
    desired: &CapturedInputs,
    ownership: &BTreeMap<ObjectIdentity, ObjectOwnership>,
    managed: &CaptureScope,
    reconstruction: &crate::resolver::reconstruct::Reconstruction,
    staged: &Staged,
) -> Assessment {
    let mut result = Assessment::default();
    let made: BTreeSet<_> = reconstruction.created().into_keys().collect();
    let reproduced = |member: &ObjectIdentity| {
        own(target, member, &managed.retained)
            || desired
                .inputs
                .get(member)
                .is_some_and(|i| i.properties.get("engine") == Some(&serde_json::Value::Bool(true)))
            || (member.class == super::class::TYPE
                && desired.inputs.get(member).is_some_and(|i| {
                    i.properties.get("user_defined") == Some(&serde_json::Value::Bool(false))
                }))
            || (desired.inputs.contains_key(member) && staged.holds(member))
    };
    for (object, input) in &desired.inputs {
        if !ownership.contains_key(object) && !own(desired, object, &made) {
            continue;
        }
        let current = target.inputs.get(object);
        let missing_binding = input
            .bindings
            .iter()
            .chain(current.into_iter().flat_map(|i| &i.bindings))
            .map(|b| &b.target)
            .find(|member| target.inputs.contains_key(*member) && !reproduced(member));
        // A runtime candidate that exists only on the target is just as real
        // for a newly created module as for one already on both sides.
        let mut missing_candidate = None;
        let mut unread = false;
        if let Some(names) = desired.runtime.get(object) {
            let needed = super::select::runtime_candidates(
                &desired.inputs,
                &BTreeMap::from([(object.clone(), names.clone())]),
            );
            for set in needed {
                match (target.candidates.get(&set), desired.candidates.get(&set)) {
                    (Some(a), Some(_)) => {
                        if let Some(member) = a.iter().find(|member| !reproduced(member)) {
                            missing_candidate = Some(member);
                            break;
                        }
                    }
                    _ => unread = true,
                }
            }
        }
        let missing = missing_binding.or(missing_candidate);
        let verdict = if target.producer.key().id() != desired.producer.key().id() {
            Verdict::Unresolved {
                condition: "the captures use different environment keys".into(),
            }
        } else if let Some(member) = missing {
            Verdict::Unresolved {
                condition: format!(
                    "{member:?} on the target is not in the resolver's externals file, and {object:?} could bind it; add it to the externals file, or select no resolver and keep ADR-0013's conservative rebuild"
                ),
            }
        } else if unread {
            Verdict::Unresolved {
                condition: "a same-named candidate on the target was not reconstructed on scratch"
                    .into(),
            }
        } else if let Some(current) = current {
            if input.bindings == current.bindings {
                Verdict::Unaffected
            } else {
                Verdict::Rebuild
            }
        } else {
            Verdict::Created
        };
        result.surfaces.insert(object.clone(), verdict);
    }
    result
        .runtime_bound
        .extend(desired.runtime_bound().cloned());
    result
}
