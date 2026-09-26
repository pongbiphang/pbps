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
/// Routines overload, so they are held by declared identity: a routine's
/// catalog identity is known once scratch has compiled its declaration
/// (DEC-613.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Managed {
    relations: BTreeSet<(String, String)>,
    routines: BTreeMap<(String, String), BTreeSet<pbps_model::ModuleId>>,
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
                pbps_model::ModuleId::Routine(_) => {
                    managed.routines.entry(key).or_default().insert(id.clone());
                }
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
        order: &crate::resolver::reconstruct::Reconstruction,
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
            // A declared overload the desired schema keeps was compiled on
            // scratch, which gives its catalog identity, and only that
            // identity is it: a count would let an unmanaged overload stand
            // in for a declared one the target lacks. An overload the plan
            // drops was never compiled, so those alone are counted, against
            // the target members no compiled identity accounts for.
            CandidateClass::Routine => {
                let Some(declared) = self.routines.get(&key(name)) else {
                    return false;
                };
                let known: BTreeSet<&ObjectIdentity> =
                    declared.iter().filter_map(|id| order.created(id)).collect();
                if known.contains(member) {
                    return true;
                }
                let dropped = declared
                    .iter()
                    .filter(|id| order.created(id).is_none())
                    .count();
                let unaccounted = members
                    .iter()
                    .filter(|other| other.name == member.name && !known.contains(other))
                    .count();
                unaccounted <= dropped
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

/// Where an unqualified name in each schema's surfaces is looked up.
///
/// The deployer's effective path, as the analysis scope measured and verified
/// it (DECISIONS 520): `pg_catalog`, then each schema of the write path the
/// deployer may use. A configured extra it has no `USAGE` on is not searched,
/// so an object there is no candidate. A schema whose effective path is not
/// known falls back to the whole configured write path, which only adds
/// candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paths {
    extras: Vec<String>,
    effective: BTreeMap<String, Vec<String>>,
}

impl Paths {
    /// `extras` is the write path after an object's own schema; `effective`
    /// the measured path per in-scope schema, `pg_catalog` first.
    pub fn new(extras: Vec<String>, effective: BTreeMap<String, Vec<String>>) -> Self {
        Self { extras, effective }
    }

    fn of(&self, schema: &str) -> Vec<String> {
        if let Some(path) = self.effective.get(schema) {
            return path.clone();
        }
        let mut path = vec!["pg_catalog".to_owned(), schema.to_owned()];
        for extra in &self.extras {
            if extra != "pg_temp" && !path.contains(extra) {
                path.push(extra.clone());
            }
        }
        path
    }
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
    paths: &Paths,
    routines: &BTreeMap<ObjectIdentity, super::manifest::Input>,
) -> BTreeSet<CandidateSet> {
    let mut sets = BTreeSet::from([CandidateSet {
        class: CandidateClass::Cast,
        namespace: None,
        name: None,
    }]);
    let Some(schema) = owner(object).name.first() else {
        return sets;
    };
    for binding in input
        .bindings
        .iter()
        .filter(|binding| resolved_by_name(binding))
    {
        let target = &binding.target;
        let (Some(class), [namespace, name]) =
            (candidate_class(&target.class), target.name.as_slice())
        else {
            continue;
        };
        // `t(x)` is either: a call when a routine fits exactly, a cast to
        // type `t` when one of that name exists and no exact match does, a
        // call to the best-matching routine otherwise. So a type binding may
        // be a call a same-named routine would now take, and a routine
        // binding one a same-named type would now take (measured on 16 and
        // 18: `foo('x')` calls foo(text) until a type `foo` exists). A cast
        // takes exactly one argument, so only a routine a one-argument call
        // can reach — its declared count, less defaults, variadic — can be
        // displaced by a type.
        let classes: &[CandidateClass] = match class {
            CandidateClass::Type => &[CandidateClass::Type, CandidateClass::Routine],
            CandidateClass::Routine if callable_with_one(routines, target) => {
                &[CandidateClass::Type, CandidateClass::Routine]
            }
            CandidateClass::Routine => &[CandidateClass::Routine],
            CandidateClass::Relation
            | CandidateClass::Operator
            | CandidateClass::Collation
            | CandidateClass::OperatorClass
            | CandidateClass::OperatorFamily
            | CandidateClass::Cast
            | CandidateClass::Extension => std::slice::from_ref(&class),
        };
        for space in paths.of(schema).into_iter().chain([namespace.clone()]) {
            for &class in classes {
                sets.insert(CandidateSet {
                    class,
                    namespace: Some(space.clone()),
                    name: Some(name.clone()),
                });
            }
        }
    }
    sets
}

/// What to capture on each side: the managed objects of both and every
/// candidate their scratch bindings depend on.
pub fn scope(desired: &CapturedInputs, managed: &[&Managed], paths: &Paths) -> CaptureScope {
    let mut candidates: BTreeSet<CandidateSet> = managed.iter().flat_map(|m| m.scope()).collect();
    for (object, input) in surfaces(desired) {
        candidates.extend(derived(object, input, paths, &desired.inputs));
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
    paths: &Paths,
    order: &crate::resolver::reconstruct::Reconstruction,
) -> Assessment {
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
            // Scratch's user schemas hold only the declarations and what
            // creating them made — an identity column's sequence, a key's
            // index, a relation's row type — so a member scratch has too was
            // reproduced, whatever the model calls it. What only the target
            // has must be one of the project's own objects the plan drops.
            !on_target.contains(member)
                || on_scratch.contains(member)
                || managed.holds(set.class, member, on_target, order)
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
        } else if !derived(object, input, paths, &desired.inputs)
            .iter()
            .all(&faithful)
        {
            Verdict::Unresolved {
                condition: "a same-named candidate on the target was not reconstructed on scratch",
            }
        } else if compiled_early(object, input, order, &desired.inputs) {
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

/// Node fields whose object follows from other parts of the tree, never
/// from looking a name up: an operator's implementation from the operator; a
/// call's, operator's or aggregate's result and transition types from the
/// routine; a column reference's type from the column; the common type of a
/// CASE, COALESCE, GREATEST/LEAST or array from its operands; a subscript's
/// container and element types from its input; a parameter's and a
/// placeholder's type from the declaration they stand for; a collation from
/// its inputs. What decided each is compared in its own right, and a
/// same-named object cannot displace these.
///
/// Not here: a constant's type and a coercion's or row constructor's result,
/// which can be a name written in the source (`'x'::t`, `ROW(..)::t`) and
/// are stored the same way when they were not (#1062).
const DETERMINED: &[&str] = &[
    "opfuncid",
    "funcresulttype",
    "opresulttype",
    "aggtype",
    "wintype",
    "aggtranstype",
    "aggargtypes",
    "vartype",
    "casetype",
    "coalescetype",
    "minmaxtype",
    "array_typeid",
    "element_typeid",
    "refcontainertype",
    "refelemtype",
    "refrestype",
    "paramtype",
    "typeId",
];

/// Whether a binding's object was selected by name resolution, so that a
/// same-named object could have been selected instead. The field is the
/// binding path's last named step; a trailing number is a list position.
fn resolved_by_name(binding: &super::bindings::Binding) -> bool {
    let Some(field) = binding
        .path
        .iter()
        .rev()
        .find(|step| step.parse::<usize>().is_err())
    else {
        return true;
    };
    let collation = field.ends_with("collid") || field == "collation";
    !collation && !DETERMINED.contains(&field.as_str())
}

/// Whether a call with exactly one argument can reach `routine`: its
/// declared argument count less its defaults is at most one, and it takes at
/// least one or is variadic. The count at the call site is not in the
/// binding, so every routine such a call could reach counts; one whose
/// properties are unreadable counts too.
fn callable_with_one(
    routines: &BTreeMap<ObjectIdentity, super::manifest::Input>,
    routine: &ObjectIdentity,
) -> bool {
    let Some(input) = routines.get(routine) else {
        return true;
    };
    let number = |field: &str| input.properties.get(field).and_then(Value::as_u64);
    let (Some(arguments), Some(defaults)) = (number("pronargs"), number("pronargdefaults")) else {
        return true;
    };
    let variadic = input
        .properties
        .get("provariadic")
        .is_none_or(|value| !value.is_null());
    arguments.saturating_sub(defaults) <= 1 && (arguments >= 1 || variadic)
}

/// Whether the module this surface belongs to bound a name that only came
/// into being later in the reconstruction.
fn compiled_early(
    object: &ObjectIdentity,
    input: &super::manifest::Input,
    order: &crate::resolver::reconstruct::Reconstruction,
    routines: &BTreeMap<ObjectIdentity, super::manifest::Input>,
) -> bool {
    let Some(later) = order.later_names(owner(object)) else {
        return false;
    };
    // Only an object that could have been resolved instead counts: an index
    // named like a routine a view calls could not have taken that call.
    input
        .bindings
        .iter()
        .filter(|binding| resolved_by_name(binding))
        .any(|binding| {
            binding.target.name.last().is_some_and(|bound| {
                later.iter().any(|(kind, name)| {
                    *name == bound
                        && kind.shadows(
                            &binding.target.class,
                            binding.target.class == "pg_proc"
                                && callable_with_one(routines, &binding.target),
                        )
                })
            })
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

    /// A relation's name covers its row type and array type. An overload
    /// the plan drops is accounted for by count, so one more member of that
    /// name on the target is an object nobody reconstructed.
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
        // Nothing compiled: every declared overload counts as one the plan
        // drops. The compiled-identity half is pinned on real engines.
        let order = crate::resolver::reconstruct::Reconstruction::new(&crate::Postgres::new(), &[])
            .unwrap();
        let relation = id("pg_class", &["app", "t"], Vec::new());
        assert!(managed.holds(CandidateClass::Relation, &relation, &none, &order));
        assert!(managed.holds(
            CandidateClass::Type,
            &id("pg_type", &["app", "t"], Vec::new()),
            &none,
            &order
        ));
        assert!(managed.holds(
            CandidateClass::Type,
            &id("pg_type", &["app", "_t"], Vec::new()),
            &none,
            &order
        ));
        assert!(!managed.holds(
            CandidateClass::Relation,
            &id("pg_class", &["other", "t"], Vec::new()),
            &none,
            &order
        ));
        assert!(!managed.holds(
            CandidateClass::Operator,
            &id("pg_operator", &["app", "t"], Vec::new()),
            &none,
            &order
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
        assert!(managed.holds(CandidateClass::Routine, &integer, &one, &order));
        assert!(!managed.holds(CandidateClass::Routine, &integer, &two, &order));
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
                bound(id(
                    "pg_proc",
                    &["util", "f"],
                    vec![id("pg_type", &["pg_catalog", "text"], Vec::new())],
                )),
                bound(id(
                    "column",
                    &["x"],
                    vec![id("pg_class", &["app", "t"], Vec::new())],
                )),
            ],
        };
        let paths = Paths::new(
            vec!["shared".to_owned(), "pg_temp".to_owned()],
            BTreeMap::new(),
        );
        let sets = derived(&view, &input, &paths, &BTreeMap::new());
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
                set(CandidateClass::Type, Some("pg_catalog"), Some("f")),
                set(CandidateClass::Type, Some("app"), Some("f")),
                set(CandidateClass::Type, Some("shared"), Some("f")),
                set(CandidateClass::Type, Some("util"), Some("f")),
            ])
        );
        // A call no one-argument call can reach cannot be a cast, so it
        // derives no type sets; a routine a single argument reaches through
        // its defaults does.
        let routine = |arguments: u64, defaults: u64| {
            let target = id("pg_proc", &["util", "f"], Vec::new());
            let properties = BTreeMap::from([
                ("pronargs".to_owned(), json!(arguments)),
                ("pronargdefaults".to_owned(), json!(defaults)),
                ("provariadic".to_owned(), Value::Null),
            ]);
            let call = Input {
                properties: BTreeMap::new(),
                bindings: vec![bound(target.clone())],
            };
            let catalog = BTreeMap::from([(
                target,
                Input {
                    properties,
                    bindings: Vec::new(),
                },
            )]);
            derived(&view, &call, &paths, &catalog)
                .iter()
                .any(|set| set.class == CandidateClass::Type)
        };
        assert!(!routine(0, 0), "a nullary call is not a cast");
        assert!(!routine(2, 0), "two required arguments are not a cast");
        assert!(routine(1, 0));
        assert!(routine(2, 1), "a defaulted second argument leaves one");
        // An operator's implementation follows from the operator, and a
        // column reference's type from the column: neither was looked up
        // by name, so neither derives a candidate.
        let determined = Input {
            properties: BTreeMap::new(),
            bindings: ["opfuncid", "vartype", "inputcollid", "coalescetype"]
                .into_iter()
                .map(|field| Binding {
                    path: vec!["ev_action".into(), "0".into(), field.into()],
                    target: id("pg_proc", &["pg_catalog", "int4pl"], Vec::new()),
                })
                .collect(),
        };
        assert_eq!(
            derived(&view, &determined, &paths, &BTreeMap::new()),
            BTreeSet::from([set(CandidateClass::Cast, None, None)])
        );
        // An extra the deployer cannot use is not on its measured path, so
        // nothing in it is a candidate.
        let measured = Paths::new(
            vec!["shared".to_owned()],
            BTreeMap::from([(
                "app".to_owned(),
                vec!["pg_catalog".to_owned(), "app".to_owned()],
            )]),
        );
        let visible = derived(&view, &input, &measured, &BTreeMap::new());
        assert!(
            !visible
                .iter()
                .any(|set| set.namespace.as_deref() == Some("shared"))
        );
        assert!(visible.contains(&set(CandidateClass::Routine, Some("util"), Some("f"))));
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
