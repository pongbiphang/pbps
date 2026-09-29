//! Private complete input records and versioned cryptographic comparison.
//! No external verifier, source, or derived checksum is an ordinary report.

use super::{CaptureScope, Uncovered, bindings::Binding, properties, read, scope};
use pbps_db::fingerprint::FingerprintKey;
use pbps_db::resolver::capture::{CaptureDifference, InputChange, ObjectIdentity};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
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

/// Logical planning facts only. Private properties never leave the fixed-key
/// capture that produced them.
#[derive(Clone)]
pub struct BindingRecord {
    pub object: ObjectIdentity,
    pub ownership: pbps_model::resolver::ObjectOwnership,
    pub bindings: Vec<pbps_model::resolver::Binding>,
}

/// An in-progress producer, with its environment key selected before the
/// coherent read. It exposes no raw capture getter, callback or rekey setter.
/// The only outward observations are verdicts and logical binding identities.
pub struct CompiledCapture {
    captured: CapturedInputs,
    key: pbps_db::fingerprint::EnvironmentFingerprintKey,
    roles: crate::resolver::authorization::RoleMap,
    ownership: BTreeMap<ObjectIdentity, pbps_model::resolver::ObjectOwnership>,
}

fn relation_identity(table: &pbps_model::TableName) -> ObjectIdentity {
    ObjectIdentity {
        class: "pg_class".into(),
        name: vec![table.schema.clone(), table.name.clone()],
        signature: Vec::new(),
    }
}

/// An ALTER TABLE/COLUMN RENAME preserves only typed owner/ACL metadata.
/// The recorded change and its UID-backed transition locate the opening
/// subject; a reused spelling cannot serve as a substitute source.
fn rename_source(
    object: &ObjectIdentity,
    transition: &pbps_model::resolver::ObjectTransition,
    changes: &pbps_model::ChangeSet,
) -> Option<ObjectIdentity> {
    use pbps_model::Change;
    use pbps_model::resolver::Surface;
    let final_table = match &transition.surface {
        Surface::Table(table) => table,
        Surface::Column(column) => &column.table,
        _ => return None,
    };
    let prior_table = changes
        .changes
        .iter()
        .find_map(|step| match &step.change {
            Change::RenameTable { from, to, .. } if to == final_table => Some(from),
            _ => None,
        })
        .unwrap_or(final_table);
    let before_relation = relation_identity(prior_table);
    let before_column = |name: &str| {
        let prior = changes
            .changes
            .iter()
            .find_map(|step| match &step.change {
                Change::RenameColumn {
                    table, from, to, ..
                } if table == final_table && to == name => Some(from.as_str()),
                _ => None,
            })
            .unwrap_or(name);
        ObjectIdentity {
            class: "pg_attribute".into(),
            name: vec![prior.into()],
            signature: vec![before_relation.clone()],
        }
    };
    let source = match object.class.as_str() {
        "pg_class" if object.name == [final_table.schema.clone(), final_table.name.clone()] => {
            before_relation
        }
        "pg_attribute" => before_column(object.name.first()?),
        _ => return None,
    };
    transition.before.contains(&source).then_some(source)
}

impl CompiledCapture {
    pub(super) fn new(
        captured: CapturedInputs,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        roles: &crate::resolver::authorization::RoleMap,
        ownership: BTreeMap<ObjectIdentity, pbps_model::resolver::ObjectOwnership>,
    ) -> Self {
        Self {
            captured,
            key: key.clone(),
            roles: roles.clone(),
            ownership,
        }
    }

    pub fn assess(
        &self,
        target: &CapturedInputs,
        base: &super::Managed,
        paths: &super::Paths,
        reconstruction: &crate::resolver::reconstruct::Reconstruction,
    ) -> pbps_db::resolver::capture::Assessment {
        super::assess(target, &self.captured, base, paths, reconstruction)
    }

    /// Consume the fixed-key producer once the exact final typed sequence is
    /// known. Typed table/column renames retain measured opening metadata;
    /// rebuilt and newly created objects receive the opening target's
    /// effective creation defaults. No caller supplies a property mapping.
    pub fn seal_for_plan(
        mut self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        transitions: &[pbps_model::resolver::ObjectTransition],
        effective_creator: &str,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        use pbps_model::Change;
        use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
        let inplace: BTreeSet<Surface> = changes
            .changes
            .iter()
            .filter_map(|step| match &step.change {
                Change::RenameTable { to, .. } => Some(Surface::Table(to.clone())),
                Change::RenameColumn { table, to, .. } => Some(Surface::Column(table.column(to))),
                _ => None,
            })
            .collect();
        let lookup: BTreeMap<_, _> = self
            .captured
            .inputs
            .keys()
            .map(|raw| Ok((normalize_identity(raw, Some(&self.roles))?, raw.clone())))
            .collect::<Result<_, ManifestError>>()?;
        let mut preserved_dependencies = BTreeSet::new();
        let mut created_dependencies = BTreeSet::new();
        let mut dependency_mappings = Vec::new();
        let mut dependencies_to_copy = Vec::new();
        for transition in transitions {
            let preserved = inplace.contains(&transition.surface);
            let created = transition.before.is_empty()
                || changes.changes.iter().any(|step| matches!(
                    &step.change,
                    Change::CreateModule { id, .. } if transition.surface == Surface::Module(id.clone())
                ));
            for object in &transition.after {
                if object.class == "pg_shdepend" {
                    continue;
                }
                let Some(raw) = lookup.get(object) else {
                    return Err(ManifestError::Incomplete);
                };
                let owner = self.ownership.get(raw).ok_or(ManifestError::Incomplete)?;
                if !matches!(owner, ObjectOwnership::Surface(_)) {
                    continue;
                }
                if preserved {
                    let source = rename_source(object, transition, changes);
                    let before = source
                        .as_ref()
                        .and_then(|source| opening.inputs.get(source));
                    let fields: &[&str] = match object.class.as_str() {
                        "pg_class" => &["relowner", "relacl"],
                        "pg_proc" => &["proowner", "proacl"],
                        "pg_attribute" if source.is_some() => &["attacl"],
                        "pg_type" => &["typowner"],
                        _ => &[],
                    };
                    if !fields.is_empty() {
                        let source = source.ok_or(ManifestError::Incomplete)?;
                        let before = before.ok_or(ManifestError::Incomplete)?;
                        let after = self
                            .captured
                            .inputs
                            .get_mut(raw)
                            .ok_or(ManifestError::Incomplete)?;
                        for field in fields {
                            let value = before
                                .properties
                                .get(*field)
                                .ok_or(ManifestError::Incomplete)?;
                            after.properties.insert((*field).into(), value.clone());
                        }
                        preserved_dependencies.insert(object.clone());
                        dependency_mappings.push((
                            source,
                            object.clone(),
                            transition.surface.clone(),
                        ));
                    }
                } else if created {
                    let kind = match object.class.as_str() {
                        "pg_proc" => Some(("f", "proacl", "proowner")),
                        "pg_class" => match self
                            .captured
                            .inputs
                            .get(raw)
                            .and_then(|input| input.properties.get("relkind"))
                            .and_then(Value::as_str)
                        {
                            Some("r" | "p" | "v") => Some(("r", "relacl", "relowner")),
                            Some("S") => Some(("S", "relacl", "relowner")),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some((kind, acl_field, owner_field)) = kind {
                        let namespace = object.name.first().ok_or(ManifestError::Invalid)?;
                        let acl = super::creation_acl::creation_acl(
                            opening,
                            effective_creator,
                            namespace,
                            kind,
                        )?;
                        let after = self
                            .captured
                            .inputs
                            .get_mut(raw)
                            .ok_or(ManifestError::Incomplete)?;
                        after.properties.insert(acl_field.into(), acl.clone());
                        after.properties.insert(
                            owner_field.into(),
                            serde_json::to_value(ObjectIdentity {
                                class: "pg_authid".into(),
                                name: vec![effective_creator.into()],
                                signature: Vec::new(),
                            })
                            .map_err(|_| ManifestError::Invalid)?,
                        );
                        created_dependencies.insert(object.clone());
                        if let Value::Array(entries) = acl {
                            for entry in entries {
                                for principal in ["grantor", "grantee"] {
                                    let role: ObjectIdentity = serde_json::from_value(
                                        entry
                                            .get(principal)
                                            .cloned()
                                            .ok_or(ManifestError::Invalid)?,
                                    )
                                    .map_err(|_| ManifestError::Invalid)?;
                                    if role.class == "pg_authid" && role.name != [effective_creator]
                                    {
                                        dependencies_to_copy.push((
                                            object.clone(),
                                            role,
                                            transition.surface.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Dependency rows are addressed by their owned subject. For in-place
        // replacement the target's owner/ACL rows survive exactly; scratch's
        // new-object dependencies would falsely report its transient owner.
        let old_dependencies = self
            .captured
            .inputs
            .keys()
            .filter(|id| id.class == "pg_shdepend")
            .filter(|id| {
                id.signature
                    .first()
                    .and_then(|subject| normalize_identity(subject, Some(&self.roles)).ok())
                    .is_some_and(|subject| {
                        preserved_dependencies.contains(&subject)
                            || created_dependencies.contains(&subject) && id.name == ["a"]
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for id in old_dependencies {
            self.captured.inputs.remove(&id);
            self.ownership.remove(&id);
        }
        for transition in transitions {
            if !inplace.contains(&transition.surface) {
                continue;
            }
            for (id, input) in &opening.inputs {
                if id.class == "pg_shdepend"
                    && id
                        .signature
                        .first()
                        .is_some_and(|subject| transition.before.contains(subject))
                {
                    self.captured.inputs.insert(id.clone(), input.clone());
                    self.ownership.insert(
                        id.clone(),
                        ObjectOwnership::Surface(transition.surface.clone()),
                    );
                }
            }
        }
        for (subject, role, surface) in dependencies_to_copy {
            // A pinned role has no shared dependency. The opening default ACL
            // itself proves whether this role is dependency-recorded; never
            // infer that from a spelling or from the scratch role map.
            let recorded = opening.inputs.keys().any(|id| {
                id.class == "pg_shdepend"
                    && id.name == ["a"]
                    && id.signature.get(1) == Some(&role)
                    && id
                        .signature
                        .first()
                        .is_some_and(|owner| owner.class == "pg_default_acl")
            });
            if !recorded {
                continue;
            }
            let id = ObjectIdentity {
                class: "pg_shdepend".into(),
                name: vec!["a".into()],
                signature: vec![subject, role],
            };
            self.captured.inputs.insert(
                id.clone(),
                Input {
                    properties: BTreeMap::new(),
                    bindings: Vec::new(),
                },
            );
            self.ownership.insert(id, ObjectOwnership::Surface(surface));
        }
        self.captured
            .seal_with_roles(&self.key, Some(&self.roles), Some(&self.ownership))
    }

    pub fn planning_records(
        &self,
    ) -> Result<Vec<BindingRecord>, pbps_model::resolver::ManifestError> {
        self.captured
            .inputs
            .iter()
            .map(|(object, input)| {
                Ok(BindingRecord {
                    object: normalize_identity(object, Some(&self.roles))?,
                    ownership: self
                        .ownership
                        .get(object)
                        .cloned()
                        .unwrap_or(pbps_model::resolver::ObjectOwnership::Unqualified),
                    bindings: input
                        .bindings
                        .iter()
                        .map(|binding| {
                            Ok(pbps_model::resolver::Binding {
                                node: binding.node.clone(),
                                path: binding.path.clone(),
                                target: normalize_identity(&binding.target, Some(&self.roles))?,
                            })
                        })
                        .collect::<Result<_, pbps_model::resolver::ManifestError>>()?,
                })
            })
            .collect()
    }
}

fn normalize_identity(
    object: &ObjectIdentity,
    roles: Option<&crate::resolver::authorization::RoleMap>,
) -> Result<ObjectIdentity, pbps_model::resolver::ManifestError> {
    use pbps_model::resolver::ManifestError;
    let mut result = object.clone();
    result.signature = object
        .signature
        .iter()
        .map(|id| normalize_identity(id, roles))
        .collect::<Result<_, _>>()?;
    if let Some(roles) = roles {
        if object.class == "pg_authid" {
            let [name] = object.name.as_slice() else {
                return Err(ManifestError::Invalid);
            };
            if let Some(logical) = roles.logical_of(name) {
                result.name = vec![logical];
            }
        }
    }
    Ok(result)
}

/// Only the ACL arrays qualified by the catalog layout are sets. Their
/// source order is by raw role spelling, which changes under the run-local
/// map; other property arrays may encode meaningful SQL order.
fn acl_field(class: &str, name: &str) -> bool {
    matches!(
        (class, name),
        ("pg_attribute", "attacl")
            | ("pg_class", "relacl")
            | ("pg_database", "datacl")
            | ("pg_default_acl", "defaclacl")
            | ("pg_init_privs", "privileges")
            | ("pg_language", "lanacl")
            | ("pg_namespace", "nspacl")
            | ("pg_parameter_acl", "paracl")
            | ("pg_proc", "proacl")
            | ("pg_tablespace", "spcacl")
            | ("pg_type", "typacl")
    )
}

fn normalize_value(
    value: &Value,
    roles: Option<&crate::resolver::authorization::RoleMap>,
) -> Result<Value, pbps_model::resolver::ManifestError> {
    if let Value::Object(map) = value {
        if map.get("class").and_then(Value::as_str) == Some("pg_authid") {
            let identity: ObjectIdentity = serde_json::from_value(value.clone())
                .map_err(|_| pbps_model::resolver::ManifestError::Invalid)?;
            return serde_json::to_value(normalize_identity(&identity, roles)?)
                .map_err(|_| pbps_model::resolver::ManifestError::Invalid);
        }
        return Ok(Value::Object(
            map.iter()
                .map(|(name, member)| Ok((name.clone(), normalize_value(member, roles)?)))
                .collect::<Result<_, pbps_model::resolver::ManifestError>>()?,
        ));
    }
    if let Value::Array(items) = value {
        return Ok(Value::Array(
            items
                .iter()
                .map(|item| normalize_value(item, roles))
                .collect::<Result<_, _>>()?,
        ));
    }
    Ok(value.clone())
}

impl CapturedInputs {
    pub(super) fn major(&self) -> u32 {
        self.major
    }

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
        self.seal_with_roles(key, None, None)
    }

    /// The mapped variant is used only during the scratch read owned by the
    /// qualified run. Every principal position is normalized before hashing;
    /// neither NULL ACLs nor grant options are collapsed.
    pub(super) fn seal_with_roles(
        &self,
        key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
        roles: Option<&crate::resolver::authorization::RoleMap>,
        ownership: Option<&BTreeMap<ObjectIdentity, pbps_model::resolver::ObjectOwnership>>,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        use pbps_model::resolver::{
            Binding, CandidateSet, InputManifest, ManifestError, Membership, Prerequisite,
            ReadScope, RoutineLookup,
        };
        let digest = |component: &str, bytes: Vec<u8>| -> String {
            key.fingerprint(self.rule, component, &bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let normalized = |object: &ObjectIdentity| normalize_identity(object, roles);
        let candidate = |q: &super::CandidateSet| CandidateSet {
            class: q.class.catalog().into(),
            namespace: q.namespace.clone(),
            name: q.name.clone(),
        };
        let mut membership: Vec<_> = self
            .candidates
            .iter()
            .map(|(q, members)| {
                Ok(Membership {
                    predicate: candidate(q),
                    members: members.iter().map(&normalized).collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<_, ManifestError>>()?;
        membership.sort_by(|a, b| a.predicate.cmp(&b.predicate));
        let mut prerequisites: Vec<Prerequisite> = self
            .inputs
            .iter()
            .map(|(object, input)| {
                let mut bindings: Vec<Binding> = input
                    .bindings
                    .iter()
                    .map(|binding| {
                        Ok(Binding {
                            node: binding.node.clone(),
                            path: binding.path.clone(),
                            target: normalized(&binding.target)?,
                        })
                    })
                    .collect::<Result<_, ManifestError>>()?;
                bindings.sort();
                let properties: BTreeMap<String, Value> = input
                    .properties
                    .iter()
                    .map(|(name, value)| {
                        let mut normalized = normalize_value(value, roles)?;
                        if acl_field(&object.class, name) {
                            if let Value::Array(entries) = &mut normalized {
                                entries.sort_by_cached_key(Value::to_string);
                            }
                        }
                        Ok((name.clone(), normalized))
                    })
                    .collect::<Result<_, ManifestError>>()?;
                Ok(Prerequisite {
                    object: normalized(object)?,
                    // An ordinary read still proves no ownership. The qualified
                    // producer assigns it only from the recorded managed UID.
                    ownership: ownership
                        .and_then(|owners| owners.get(object))
                        .cloned()
                        .unwrap_or(pbps_model::resolver::ObjectOwnership::Unqualified),
                    canonicalization: self.rule.into(),
                    properties: digest(
                        "properties",
                        serde_json::to_vec(&properties).expect("canonical properties serialize"),
                    ),
                    bindings,
                })
            })
            .collect::<Result<_, ManifestError>>()?;
        prerequisites.sort_by(|a, b| a.object.cmp(&b.object));
        InputManifest::new(
            self.rule.into(),
            self.major,
            key.id().as_str().into(),
            ReadScope {
                retained: self
                    .scope
                    .retained
                    .iter()
                    .map(&normalized)
                    .collect::<Result<_, _>>()?,
                candidates: self.scope.candidates.iter().map(candidate).collect(),
            },
            digest(
                "baseline",
                serde_json::to_vec(&self.baseline).expect("baseline serializes"),
            ),
            // Projection retains the opening target session; the scratch
            // admin's session hash is not used as an approved postcondition.
            digest(
                "session",
                serde_json::to_vec(&self.session).expect("session serializes"),
            ),
            prerequisites,
            membership,
            self.limitations
                .iter()
                .map(&normalized)
                .collect::<Result<_, _>>()?,
            self.dropped
                .iter()
                .map(|(query, resolved)| {
                    Ok(RoutineLookup {
                        signature: query.spelled.clone(),
                        search_path: query.path.clone(),
                        kind: query.kind.into(),
                        resolved: resolved.as_ref().map(&normalized).transpose()?,
                    })
                })
                .collect::<Result<_, ManifestError>>()?,
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
