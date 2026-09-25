//! Observed target bindings against desired bindings compiled on scratch
//! (ADR-0016 decision 2; issue #613).
//!
//! Both sides are captures by the same versioned rule, so a binding is a
//! logical identity on each and database-local numbers never meet. A surface
//! whose bindings agree is proven unaffected; one whose bindings differ is a
//! proven rebuild. Neither verdict holds unless the scratch namespace was a
//! faithful stand-in for the target's wherever those declarations can look:
//! every candidate sharing a name a surface bound, on its resolution path and
//! in the schema it bound into. Scratch holds only the engine's own objects
//! and the desired managed ones, so a target candidate that is neither a
//! built-in scratch also has nor one of the project's managed objects was not
//! reconstructed, and the surfaces that could have bound it are unresolved.
//! Reconstructing such retained external objects is gated on private-source
//! handling (#617); until then the analysis is managed-only by construction.
//!
//! Names come from what scratch bound. A declaration the plan leaves
//! unchanged has the same text on both sides, so it resolves the same names;
//! only what those names select can differ, and that difference is exactly
//! what the bindings carry. A declaration the plan changes is rebuilt anyway.

use super::{CandidateClass, CandidateSet, CaptureScope, CapturedInputs};
use pbps_db::resolver::capture::{Assessment, ObjectIdentity, Verdict};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The target's managed objects, by the names pbps's own model gives them.
/// Routines overload, so they are counted: every managed overload exists on
/// the target, and one more member of that name than the model holds is an
/// object nobody reconstructed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Managed {
    relations: BTreeSet<(String, String)>,
    routines: BTreeMap<(String, String), usize>,
}

impl Managed {
    /// `schema` is the managed side of the target: its tables, their named
    /// indexes and keys, and its modules.
    pub fn from_schema(schema: &pbps_model::Schema) -> Self {
        let mut managed = Self::default();
        for (name, table) in &schema.tables {
            managed
                .relations
                .insert((name.schema.clone(), name.name.clone()));
            let key = table.primary_key.as_ref().and_then(|pk| pk.name.clone());
            for index in table.indexes.keys().chain(table.unique.keys()).chain(&key) {
                managed
                    .relations
                    .insert((name.schema.clone(), index.clone()));
            }
        }
        for id in schema.modules.keys() {
            let key = (id.schema().to_owned(), id.name().to_owned());
            match id {
                pbps_model::ModuleId::Named(_) => {
                    managed.relations.insert(key);
                }
                pbps_model::ModuleId::Routine(_) => *managed.routines.entry(key).or_default() += 1,
                pbps_model::ModuleId::Trigger { .. } => {}
            }
        }
        managed
    }

    /// The scope that captures every managed object of these names with its
    /// bindings, and every same-named object beside it.
    fn scope(&self) -> BTreeSet<CandidateSet> {
        let set = |class, (schema, name): &(String, String)| CandidateSet {
            class,
            namespace: Some(schema.clone()),
            name: Some(name.clone()),
        };
        self.relations
            .iter()
            .map(|key| set(CandidateClass::Relation, key))
            .chain(
                self.routines
                    .keys()
                    .map(|key| set(CandidateClass::Routine, key)),
            )
            .collect()
    }

    fn holds(
        &self,
        class: CandidateClass,
        member: &ObjectIdentity,
        members: &BTreeSet<ObjectIdentity>,
    ) -> bool {
        let [schema, name] = member.name.as_slice() else {
            return false;
        };
        let key = |name: &str| (schema.clone(), name.to_owned());
        match class {
            CandidateClass::Relation => self.relations.contains(&key(name)),
            // A relation's row type has its name, and its array type is the
            // name with an underscore in front.
            CandidateClass::Type => {
                self.relations.contains(&key(name))
                    || name
                        .strip_prefix('_')
                        .is_some_and(|element| self.relations.contains(&key(element)))
            }
            CandidateClass::Routine => {
                let present = members
                    .iter()
                    .filter(|other| other.name == member.name)
                    .count();
                self.routines.get(&key(name)) == Some(&present)
            }
            CandidateClass::Operator
            | CandidateClass::Collation
            | CandidateClass::OperatorClass
            | CandidateClass::OperatorFamily
            | CandidateClass::Cast
            | CandidateClass::Extension => false,
        }
    }
}

/// Where an unqualified name in a surface of `schema` is looked up: the
/// write path the emitter gives every statement, after `pg_catalog`.
fn path(schema: &str, extras: &[String]) -> Vec<String> {
    let mut path = vec!["pg_catalog".to_owned(), schema.to_owned()];
    for extra in extras {
        if extra != "pg_temp" && !path.contains(extra) {
            path.push(extra.clone());
        }
    }
    path
}

fn candidate_class(class: &str) -> Option<CandidateClass> {
    Some(match class {
        "pg_class" => CandidateClass::Relation,
        "pg_proc" => CandidateClass::Routine,
        "pg_type" => CandidateClass::Type,
        "pg_operator" => CandidateClass::Operator,
        "pg_collation" => CandidateClass::Collation,
        "pg_opclass" => CandidateClass::OperatorClass,
        "pg_opfamily" => CandidateClass::OperatorFamily,
        _ => return None,
    })
}

/// The relation or routine a surface belongs to: itself, or the object its
/// catalog row hangs off (a rule's view, a default's column's table).
fn owner(object: &ObjectIdentity) -> &ObjectIdentity {
    match object.class.as_str() {
        "pg_class" | "pg_proc" | "pg_type" => object,
        "pg_constraint" => object
            .signature
            .iter()
            .find(|part| part.class == "pg_class" && !part.name.is_empty())
            .unwrap_or(object),
        _ => object.signature.first().map_or(object, owner),
    }
}

fn is_system(schema: &str) -> bool {
    super::super::authorization::is_system_schema(schema)
}

/// The surfaces worth comparing: an object in a user schema that carries
/// bindings on scratch. Scratch's user schemas hold only desired managed
/// objects, so these are the managed declarations and their parts.
fn surfaces(
    desired: &CapturedInputs,
) -> impl Iterator<Item = (&ObjectIdentity, &super::manifest::Input)> {
    desired.inputs.iter().filter(|(object, input)| {
        !input.bindings.is_empty()
            && owner(object)
                .name
                .first()
                .is_some_and(|schema| !is_system(schema))
    })
}

/// The candidate sets a surface depends on, from what it bound on scratch,
/// and every cast: resolution consults them without a name, so a changed
/// or added cast can change any call's or coercion's answer.
fn derived(
    object: &ObjectIdentity,
    input: &super::manifest::Input,
    extras: &[String],
) -> BTreeSet<CandidateSet> {
    let mut sets = BTreeSet::from([CandidateSet {
        class: CandidateClass::Cast,
        namespace: None,
        name: None,
    }]);
    let Some(schema) = owner(object).name.first() else {
        return sets;
    };
    for binding in &input.bindings {
        let target = &binding.target;
        let (Some(class), [namespace, name]) =
            (candidate_class(&target.class), target.name.as_slice())
        else {
            continue;
        };
        for space in path(schema, extras).into_iter().chain([namespace.clone()]) {
            sets.insert(CandidateSet {
                class,
                namespace: Some(space),
                name: Some(name.clone()),
            });
        }
    }
    sets
}

/// What to capture on each side: the managed objects of both and every
/// candidate their scratch bindings depend on.
pub fn scope(desired: &CapturedInputs, managed: &[&Managed], extras: &[String]) -> CaptureScope {
    let mut candidates: BTreeSet<CandidateSet> = managed.iter().flat_map(|m| m.scope()).collect();
    for (object, input) in surfaces(desired) {
        candidates.extend(derived(object, input, extras));
    }
    CaptureScope {
        retained: BTreeSet::new(),
        candidates,
    }
}

/// The first capture of scratch: its managed objects and their bindings,
/// from which [`scope`] derives what both sides must capture.
pub fn managed_scope(desired: &Managed) -> CaptureScope {
    CaptureScope {
        retained: BTreeSet::new(),
        candidates: desired.scope(),
    }
}

/// Properties with every role reference removed. The engine's own objects
/// are owned by the bootstrap superuser, whose name is the installation's,
/// and neither ownership nor ACL decides what a name binds to.
fn without_roles(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.get("class") == Some(&Value::from("pg_authid")) => Value::Null,
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), without_roles(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_roles).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// Compares every surface the captures share. `target` must have been
/// captured with the scope [`scope`] built from `desired`, and `order` must
/// be the reconstruction that produced `desired`.
pub fn assess(
    target: &CapturedInputs,
    desired: &CapturedInputs,
    managed: &Managed,
    extras: &[String],
    order: &crate::resolver::reconstruct::Reconstruction,
) -> Assessment {
    let empty = BTreeSet::new();
    // A set is faithful when scratch reproduced every target member that
    // could win: the engine's own objects identically, the managed ones as
    // declared. Missing from either capture is not faithful: an uncaptured
    // set is not an empty one.
    let faithful = |set: &CandidateSet| -> bool {
        let (Some(on_target), Some(on_scratch)) =
            (target.candidates.get(set), desired.candidates.get(set))
        else {
            return false;
        };
        on_target.union(on_scratch).all(|member| {
            let system = set.class == CandidateClass::Cast
                || member.name.first().is_some_and(|schema| is_system(schema));
            if system {
                return match (target.inputs.get(member), desired.inputs.get(member)) {
                    (Some(left), Some(right)) => {
                        on_target.contains(member)
                            && on_scratch.contains(member)
                            && properties_equal(&left.properties, &right.properties)
                    }
                    _ => false,
                };
            }
            !on_target.contains(member) || managed.holds(set.class, member, on_target)
        })
    };
    let mut assessment = Assessment::default();
    for (object, input) in surfaces(desired) {
        let Some(current) = target.inputs.get(object) else {
            continue;
        };
        let verdict = if input
            .bindings
            .iter()
            .chain(&current.bindings)
            .any(|binding| binding.target.class == "pg_authid")
        {
            Verdict::Unresolved {
                condition: "a binding names a role, which scratch reproduces under another name",
            }
        } else if !derived(object, input, extras).iter().all(&faithful) {
            Verdict::Unresolved {
                condition: "a same-named candidate on the target was not reconstructed on scratch",
            }
        } else if compiled_early(object, input, order, &empty) {
            Verdict::Unresolved {
                condition: "the declaration was compiled before an object sharing a name it bound",
            }
        } else if input.bindings == current.bindings {
            Verdict::Unaffected
        } else {
            Verdict::Rebuild
        };
        assessment.surfaces.insert(object.clone(), verdict);
    }
    // A runtime-bound routine may have no creation-time bindings at all, so
    // it is named whether or not it has a surface to compare.
    assessment.runtime_bound = desired
        .limitations
        .iter()
        .filter(|object| {
            target.inputs.contains_key(*object)
                && object.name.first().is_some_and(|schema| !is_system(schema))
        })
        .cloned()
        .collect();
    assessment
}

fn properties_equal(left: &BTreeMap<String, Value>, right: &BTreeMap<String, Value>) -> bool {
    let strip = |map: &BTreeMap<String, Value>| -> BTreeMap<String, Value> {
        map.iter()
            .map(|(key, value)| (key.clone(), without_roles(value)))
            .collect()
    };
    strip(left) == strip(right)
}

/// Whether the module this surface belongs to bound a name that only came
/// into being later in the reconstruction.
fn compiled_early(
    object: &ObjectIdentity,
    input: &super::manifest::Input,
    order: &crate::resolver::reconstruct::Reconstruction,
    empty: &BTreeSet<&str>,
) -> bool {
    let owner = owner(object);
    let [schema, name] = owner.name.as_slice() else {
        return false;
    };
    let routine = owner.class == "pg_proc";
    let later = order.later_names(schema, name, routine);
    let later = later.as_ref().unwrap_or(empty);
    input.bindings.iter().any(|binding| {
        candidate_class(&binding.target.class).is_some()
            && binding
                .target
                .name
                .last()
                .is_some_and(|bound| later.contains(bound.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::super::bindings::Binding;
    use super::super::manifest::Input;
    use super::*;
    use serde_json::json;

    fn id(class: &str, name: &[&str], signature: Vec<ObjectIdentity>) -> ObjectIdentity {
        ObjectIdentity {
            class: class.into(),
            name: name.iter().map(|&part| part.to_owned()).collect(),
            signature,
        }
    }

    /// The engine's objects are owned by the bootstrap superuser, whose name
    /// is the installation's. A role, wherever it appears, decides nothing;
    /// any other property still does.
    #[test]
    fn a_built_in_is_the_same_whoever_owns_it_and_not_when_a_property_moves() {
        let owner = |name: &str| json!(id("pg_authid", &[name], Vec::new()));
        let properties = |role: &str, context: &str| {
            BTreeMap::from([
                ("castowner".to_owned(), owner(role)),
                (
                    "acl".to_owned(),
                    json!([{"grantor": owner(role), "grantee": owner("pg_monitor"), "privilege": "USAGE"}]),
                ),
                ("castcontext".to_owned(), json!(context)),
            ])
        };
        assert!(properties_equal(
            &properties("postgres", "i"),
            &properties("admin", "i")
        ));
        assert!(!properties_equal(
            &properties("postgres", "i"),
            &properties("postgres", "a")
        ));
    }

    /// A relation's name covers its row type and array type; a routine's
    /// name holds only as many overloads as the model declares, so one more
    /// on the target is an object nobody reconstructed.
    #[test]
    fn managed_names_hold_their_own_objects_and_nothing_beside_them() {
        let mut schema = pbps_model::Schema::default();
        schema
            .tables
            .insert("app.t".parse().unwrap(), pbps_model::Table::default());
        schema.modules.insert(
            "app.f(integer)".parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: String::new(),
            },
        );
        let managed = Managed::from_schema(&schema);
        let none = BTreeSet::new();
        let relation = id("pg_class", &["app", "t"], Vec::new());
        assert!(managed.holds(CandidateClass::Relation, &relation, &none));
        assert!(managed.holds(
            CandidateClass::Type,
            &id("pg_type", &["app", "t"], Vec::new()),
            &none
        ));
        assert!(managed.holds(
            CandidateClass::Type,
            &id("pg_type", &["app", "_t"], Vec::new()),
            &none
        ));
        assert!(!managed.holds(
            CandidateClass::Relation,
            &id("pg_class", &["other", "t"], Vec::new()),
            &none
        ));
        assert!(!managed.holds(
            CandidateClass::Operator,
            &id("pg_operator", &["app", "t"], Vec::new()),
            &none
        ));
        let integer = id(
            "pg_proc",
            &["app", "f"],
            vec![id("pg_type", &["pg_catalog", "int4"], Vec::new())],
        );
        let text = id(
            "pg_proc",
            &["app", "f"],
            vec![id("pg_type", &["pg_catalog", "text"], Vec::new())],
        );
        let one = BTreeSet::from([integer.clone()]);
        let two = BTreeSet::from([integer.clone(), text]);
        assert!(managed.holds(CandidateClass::Routine, &integer, &one));
        assert!(!managed.holds(CandidateClass::Routine, &integer, &two));
    }

    /// A surface depends on every name it bound, looked up along its own
    /// write path and in the schema it bound into, and on every cast. A
    /// column binding names no candidate: its relation already is one.
    #[test]
    fn a_surface_depends_on_its_bound_names_along_its_path_and_on_every_cast() {
        let view = id(
            "pg_rewrite",
            &["_RETURN"],
            vec![id("pg_class", &["app", "v"], Vec::new())],
        );
        let bound = |target| Binding {
            path: Vec::new(),
            target,
        };
        let input = Input {
            properties: BTreeMap::new(),
            bindings: vec![
                bound(id("pg_proc", &["util", "f"], Vec::new())),
                bound(id(
                    "column",
                    &["x"],
                    vec![id("pg_class", &["app", "t"], Vec::new())],
                )),
            ],
        };
        let extras = ["shared".to_owned(), "pg_temp".to_owned()];
        let sets = derived(&view, &input, &extras);
        let set = |class, namespace: Option<&str>, name: Option<&str>| CandidateSet {
            class,
            namespace: namespace.map(str::to_owned),
            name: name.map(str::to_owned),
        };
        assert_eq!(
            sets,
            BTreeSet::from([
                set(CandidateClass::Cast, None, None),
                set(CandidateClass::Routine, Some("pg_catalog"), Some("f")),
                set(CandidateClass::Routine, Some("app"), Some("f")),
                set(CandidateClass::Routine, Some("shared"), Some("f")),
                set(CandidateClass::Routine, Some("util"), Some("f")),
            ])
        );
        assert_eq!(owner(&view).name, ["app", "v"]);
        let default = id(
            "pg_attrdef",
            &[],
            vec![id(
                "column",
                &["c"],
                vec![id("pg_class", &["app", "t"], Vec::new())],
            )],
        );
        assert_eq!(owner(&default).name, ["app", "t"]);
    }
}
