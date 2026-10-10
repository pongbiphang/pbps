//! What a resolver baseline staged on scratch, compared with the target
//! (SPEC §9.3.2; #1673).
//!
//! Each object the baseline created is compared by what a creation-time
//! binding can read: its own class properties, and of a relation its
//! columns, primary-key and unique constraints and indexes, and parents.
//! Foreign keys, CHECKs, defaults, triggers, policies, rules, non-unique
//! indexes and the source of a routine's body or a view's query are not
//! compared: a binding reads an object's shape, not how it computes or
//! guards it, so a baseline may omit them, and a view may be staged as a
//! shape view. The target's complete fingerprints are still sealed and
//! rechecked; only this comparison is narrower.

use super::logical::Catalog;
use super::manifest::Input;
use super::{CaptureError, CaptureScope, CapturedInputs, Managed};
use crate::resolver::baseline::Object;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::transport::QueryConnection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The objects a baseline staged and the comparison accepted. Their target
/// fingerprints join the sealed capture, and the assessment takes them, and
/// what they carry, as reconstructed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staged {
    roots: BTreeSet<ObjectIdentity>,
}

impl Staged {
    pub fn roots(&self) -> &BTreeSet<ObjectIdentity> {
        &self.roots
    }

    pub(super) fn holds(&self, object: &ObjectIdentity) -> bool {
        self.roots.contains(object)
    }
}

/// Why a baseline's objects could not be accepted.
#[derive(Debug)]
pub enum StageError {
    /// A capture of either side failed.
    Capture(CaptureError),
    /// Each finding names an object.
    Refused(Vec<String>),
}

/// Compares what the baseline created on `scratch`, `created` as the
/// inventory found it there, with the same objects on `target`. `managed`
/// is every side of the managed set the plan touches.
pub async fn compare(
    scratch: &mut impl QueryConnection,
    target: &mut impl QueryConnection,
    created: &BTreeSet<Object>,
    managed: &[&Managed],
) -> Result<Staged, StageError> {
    let mut resolved: BTreeMap<ObjectIdentity, String> = BTreeMap::new();
    let mut refused = Vec::new();
    let staged = read(scratch, |catalog| {
        for object in created {
            match comparable(catalog, object) {
                Some(identity) => {
                    resolved.insert(identity, object.described.clone());
                }
                None => refused.push(format!(
                    "the baseline created {}, which the comparison with the target does not \
                     cover; leave it out of the baseline",
                    object.described
                )),
            }
        }
        resolved.keys().cloned().collect()
    })
    .await
    .map_err(StageError::Capture)?;
    for (identity, described) in &resolved {
        if managed.iter().any(|m| m.names(identity)) {
            refused.push(format!(
                "the baseline created {described}, which is in the managed set; leave it out of \
                 the baseline, which stages only objects outside it"
            ));
        }
    }
    if !refused.is_empty() {
        return Err(StageError::Refused(refused));
    }
    let mut absent = Vec::new();
    let on_target = read(target, |catalog| {
        let mut present = BTreeSet::new();
        for identity in resolved.keys() {
            if exists(catalog, identity) {
                present.insert(identity.clone());
            } else {
                absent.push(identity.clone());
            }
        }
        present
    })
    .await
    .map_err(StageError::Capture)?;
    let mut findings: Vec<String> = absent
        .iter()
        .map(|identity| {
            format!(
                "the baseline created {}, which the target does not have",
                resolved[identity]
            )
        })
        .collect();
    let roots: BTreeSet<ObjectIdentity> = resolved.keys().cloned().collect();
    // A root the target lacks is named once, not with each of its parts.
    let present: BTreeSet<ObjectIdentity> = roots
        .iter()
        .filter(|root| !absent.contains(root))
        .cloned()
        .collect();
    findings.extend(differences(
        &shapes(&on_target.inputs, &present),
        &shapes(&staged.inputs, &present),
        &resolved,
    ));
    if findings.is_empty() {
        Ok(Staged { roots })
    } else {
        Err(StageError::Refused(findings))
    }
}

/// One owned snapshot of `retained`, as `choose` picks it from the catalog.
async fn read(
    conn: &mut impl QueryConnection,
    choose: impl FnOnce(&Catalog) -> BTreeSet<ObjectIdentity>,
) -> Result<CapturedInputs, CaptureError> {
    let mut prepared = None;
    let mut chosen = None;
    let read = super::read::owned(conn, &BTreeSet::new(), |catalog, major| {
        let scope = CaptureScope {
            retained: choose(catalog),
            candidates: BTreeSet::new(),
        };
        let mut result = super::scope::prepare(catalog, major, &scope)?;
        let selection = std::mem::take(&mut result.render);
        prepared = Some(result);
        chosen = Some(scope);
        Ok(selection)
    })
    .await?;
    super::manifest::finish(
        read,
        prepared.ok_or(CaptureError::Incomplete)?,
        chosen.ok_or(CaptureError::Incomplete)?,
    )
    .map_err(CaptureError::Coverage)
}

/// The identity of a created object the comparison can read, or `None`: a
/// class the capture has no identity for (a text search configuration, a
/// large object, a subscription, a default privilege), or a foreign table,
/// whose properties are not qualified.
fn comparable(catalog: &Catalog, object: &Object) -> Option<ObjectIdentity> {
    let identity = catalog.object(&object.catalog, object.oid).ok()?;
    if object.catalog == "pg_class" {
        let row = catalog.row("pg_class", object.oid).ok()?;
        if super::logical::string(row, "relkind").ok()? == "f" {
            return None;
        }
    }
    Some(identity)
}

/// Whether the catalog holds an object of this identity.
fn exists(catalog: &Catalog, identity: &ObjectIdentity) -> bool {
    catalog
        .rows
        .get(identity.class.as_str())
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| catalog.identity(&identity.class, row).as_ref() == Ok(identity))
        })
}

/// What is compared of each root, keyed by identity: the root's own
/// properties, and the children a binding reads.
fn shapes(
    inputs: &BTreeMap<ObjectIdentity, Input>,
    roots: &BTreeSet<ObjectIdentity>,
) -> BTreeMap<ObjectIdentity, BTreeMap<String, Value>> {
    let mut shapes = BTreeMap::new();
    // Relations whose columns are compared: each root relation, a composite
    // type's relation, and a unique index.
    let mut relations: BTreeSet<ObjectIdentity> = BTreeSet::new();
    for root in roots {
        let Some(input) = inputs.get(root) else {
            continue;
        };
        shapes.insert(root.clone(), projected(root, &input.properties));
        match root.class.as_str() {
            "pg_class" => {
                relations.insert(root.clone());
            }
            "pg_type" => {
                if let Some(relation) = reference(&input.properties, "typrelid") {
                    relations.insert(relation);
                }
            }
            _ => {}
        }
    }
    let first = |object: &ObjectIdentity, at: usize| object.signature.get(at).cloned();
    let mut ranked: BTreeMap<ObjectIdentity, Vec<(f64, ObjectIdentity)>> = BTreeMap::new();
    for (object, input) in inputs {
        let properties = &input.properties;
        let owner = match object.class.as_str() {
            "pg_constraint" => first(object, 1).filter(|_| {
                matches!(
                    properties.get("contype").and_then(Value::as_str),
                    Some("p" | "u")
                )
            }),
            "pg_index" => reference(properties, "indrelid")
                .filter(|_| properties.get("indisunique") == Some(&Value::Bool(true))),
            "pg_inherits"
            | "pg_partitioned_table"
            | "pg_sequence"
            | "pg_range"
            | "pg_aggregate" => first(object, 0),
            "pg_enum" => {
                if let (Some(owner), Some(order)) = (
                    first(object, 0),
                    properties.get("enumsortorder").and_then(Value::as_f64),
                ) && roots.contains(&owner)
                {
                    ranked
                        .entry(owner)
                        .or_default()
                        .push((order, object.clone()));
                }
                continue;
            }
            _ => continue,
        };
        let Some(owner) = owner else {
            continue;
        };
        if !roots.contains(&owner) && !relations.contains(&owner) {
            continue;
        }
        shapes.insert(object.clone(), projected(object, properties));
        if object.class == "pg_index"
            && let Some(index) = first(object, 0)
        {
            if let Some(input) = inputs.get(&index) {
                shapes.insert(index.clone(), projected(&index, &input.properties));
            }
            relations.insert(index);
        }
    }
    // An enum's labels by their place, not the engine's sort keys, which
    // differ with the order labels were added in.
    for labels in ranked.values_mut() {
        labels.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (rank, (_, label)) in labels.iter().enumerate() {
            let mut properties = projected(label, &inputs[label].properties);
            properties.insert("enumsortorder".into(), Value::from(rank));
            shapes.insert(label.clone(), properties);
        }
    }
    for (object, input) in inputs {
        if object.class == "column"
            && object
                .signature
                .first()
                .is_some_and(|relation| relations.contains(relation))
        {
            shapes.insert(object.clone(), projected(object, &input.properties));
        }
    }
    shapes
}

/// An object's properties without what the comparison skips, and without
/// role names, which differ on scratch by construction.
fn projected(
    object: &ObjectIdentity,
    properties: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let skipped: &[&str] = match object.class.as_str() {
        // The body, in each of its spellings.
        "pg_proc" => &["prosrc", "probin", "prosqlbody", "engine_definition"],
        // A domain's default.
        "pg_type" => &["typdefault", "typdefaultbin"],
        // A column's default, and the value a later added one fills.
        "column" => &["atthasdef", "atthasmissing", "attmissingval"],
        _ => &[],
    };
    properties
        .iter()
        .filter(|(key, _)| !skipped.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), super::assess::without_roles(value)))
        .collect()
}

fn reference(properties: &BTreeMap<String, Value>, field: &str) -> Option<ObjectIdentity> {
    serde_json::from_value(properties.get(field)?.clone()).ok()
}

/// Each difference between the two sides' shapes, named.
fn differences(
    target: &BTreeMap<ObjectIdentity, BTreeMap<String, Value>>,
    scratch: &BTreeMap<ObjectIdentity, BTreeMap<String, Value>>,
    described: &BTreeMap<ObjectIdentity, String>,
) -> Vec<String> {
    let name = |object: &ObjectIdentity| {
        described
            .get(object)
            .cloned()
            .unwrap_or_else(|| named(object))
    };
    let mut findings = Vec::new();
    for (object, on_target) in target {
        match scratch.get(object) {
            None => findings.push(format!(
                "the target has {} and the baseline does not",
                name(object)
            )),
            Some(staged) if staged != on_target => {
                let fields: Vec<&str> = on_target
                    .keys()
                    .chain(staged.keys())
                    .filter(|key| on_target.get(*key) != staged.get(*key))
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                findings.push(format!(
                    "the baseline's {} differs from the target's in {}",
                    name(object),
                    fields.join(", ")
                ));
            }
            Some(_) => {}
        }
    }
    for object in scratch.keys() {
        if !target.contains_key(object) && described.get(object).is_none() {
            findings.push(format!(
                "the baseline has {} and the target does not",
                name(object)
            ));
        }
    }
    findings
}

/// A readable name for an identity the engine did not describe.
fn named(object: &ObjectIdentity) -> String {
    let path = object.name.join(".");
    let of = |at: usize| object.signature.get(at).map(named).unwrap_or_default();
    match object.class.as_str() {
        "column" => format!("column {path} of {}", of(0)),
        "pg_class" => format!("relation {path}"),
        "pg_type" => format!("type {path}"),
        "pg_proc" => format!("routine {path}"),
        "pg_constraint" => format!("constraint {path} of {}", of(1)),
        "pg_index" => format!(
            "index {}",
            object
                .signature
                .first()
                .map_or(String::new(), |i| i.name.join("."))
        ),
        "pg_inherits" => format!("parent {} of {}", of(1), of(0)),
        "pg_enum" => format!("label {path} of {}", of(0)),
        "pg_range" => format!("range of {}", of(0)),
        "pg_sequence" => format!("sequence parameters of {}", of(0)),
        "pg_aggregate" => format!("aggregate parameters of {}", of(0)),
        "pg_partitioned_table" => format!("partition key of {}", of(0)),
        class => format!("{class} {path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(class: &str, name: &[&str], signature: Vec<ObjectIdentity>) -> ObjectIdentity {
        ObjectIdentity {
            class: class.into(),
            name: name.iter().map(|s| s.to_string()).collect(),
            signature,
        }
    }

    fn input(properties: Value) -> Input {
        Input {
            properties: serde_json::from_value(properties).unwrap(),
            bindings: Vec::new(),
        }
    }

    fn table() -> ObjectIdentity {
        id("pg_class", &["ext", "t"], vec![])
    }

    /// A table with a column, a primary key, a CHECK, a unique and a plain
    /// index, a default and a trigger, as a capture holds it.
    fn captured(check: &str, plain_index: bool) -> BTreeMap<ObjectIdentity, Input> {
        let t = table();
        let ns = id("pg_namespace", &["ext"], vec![]);
        let none = id("pg_type", &[], vec![]);
        let column = id("column", &["a"], vec![t.clone()]);
        let mut inputs = BTreeMap::from([
            (t.clone(), input(json!({"relkind": "r", "relname": "t"}))),
            (
                column.clone(),
                input(json!({"atttypid": "int4", "atthasdef": true})),
            ),
            (
                id(
                    "pg_constraint",
                    &["t_pkey"],
                    vec![ns.clone(), t.clone(), none.clone()],
                ),
                input(json!({"contype": "p"})),
            ),
            (
                id(
                    "pg_constraint",
                    &["t_a_check"],
                    vec![ns.clone(), t.clone(), none.clone()],
                ),
                input(json!({"contype": "c", "conbin": check})),
            ),
            (
                id(
                    "pg_index",
                    &[],
                    vec![id("pg_class", &["ext", "t_pkey"], vec![])],
                ),
                input(json!({"indisunique": true, "indrelid": t})),
            ),
            (
                id("pg_class", &["ext", "t_pkey"], vec![]),
                input(json!({"relkind": "i"})),
            ),
            (
                id("pg_attrdef", &[], vec![column.clone()]),
                input(json!({"adbin": "42"})),
            ),
            (
                id("pg_trigger", &["audit"], vec![t.clone()]),
                input(json!({"tgname": "audit"})),
            ),
        ]);
        if plain_index {
            inputs.insert(
                id(
                    "pg_index",
                    &[],
                    vec![id("pg_class", &["ext", "t_b_idx"], vec![])],
                ),
                input(json!({"indisunique": false, "indrelid": t})),
            );
            inputs.insert(
                id("pg_class", &["ext", "t_b_idx"], vec![]),
                input(json!({"relkind": "i"})),
            );
        }
        inputs
    }

    #[test]
    fn a_relation_is_compared_by_its_columns_and_keys_not_its_guards() {
        let roots = BTreeSet::from([table()]);
        let target = shapes(&captured("a > 0", true), &roots);
        let staged = shapes(&captured("a > 1", false), &roots);
        // A different CHECK, a missing plain index, defaults and triggers
        // are not compared.
        assert_eq!(target, staged);
        assert!(target.contains_key(&id("column", &["a"], vec![table()])));
        assert!(target.contains_key(&id("pg_class", &["ext", "t_pkey"], vec![])));
        assert!(!target.contains_key(&id("pg_class", &["ext", "t_b_idx"], vec![])));
        // A column's default flag is not compared either.
        assert!(!target[&id("column", &["a"], vec![table()])].contains_key("atthasdef"));
    }

    #[test]
    fn a_changed_column_or_a_missing_key_is_named() {
        let roots = BTreeSet::from([table()]);
        let target = captured("", false);
        let mut staged = target.clone();
        staged.insert(
            id("column", &["a"], vec![table()]),
            input(json!({"atttypid": "int8"})),
        );
        staged.retain(|object, _| object.class != "pg_index");
        let described = BTreeMap::from([(table(), "table ext.t".to_owned())]);
        let findings = differences(
            &shapes(&target, &roots),
            &shapes(&staged, &roots),
            &described,
        );
        assert_eq!(
            findings,
            [
                "the baseline's column a of relation ext.t differs from the target's in atttypid",
                "the target has relation ext.t_pkey and the baseline does not",
                "the target has index ext.t_pkey and the baseline does not",
            ]
        );
        // Negative: the same shape names nothing.
        assert!(
            differences(
                &shapes(&target, &roots),
                &shapes(&target, &roots),
                &described
            )
            .is_empty()
        );
    }

    #[test]
    fn a_routine_is_compared_by_its_header_not_its_body() {
        let f = id(
            "pg_proc",
            &["ext", "f"],
            vec![id("pg_type", &["pg_catalog", "int4"], vec![])],
        );
        let roots = BTreeSet::from([f.clone()]);
        let routine = |body: &str, volatile: &str| {
            BTreeMap::from([(
                f.clone(),
                input(json!({"prosrc": body, "engine_definition": body, "provolatile": volatile})),
            )])
        };
        assert_eq!(
            shapes(&routine("SELECT 1", "v"), &roots),
            shapes(&routine("SELECT 2", "v"), &roots)
        );
        // Negative: the header is compared.
        assert_ne!(
            shapes(&routine("SELECT 1", "v"), &roots),
            shapes(&routine("SELECT 1", "i"), &roots)
        );
    }

    #[test]
    fn an_enum_is_compared_by_the_order_of_its_labels() {
        let e = id("pg_type", &["ext", "e"], vec![]);
        let roots = BTreeSet::from([e.clone()]);
        let labels = |orders: [(f64, &str); 2]| {
            let mut inputs = BTreeMap::from([(e.clone(), input(json!({"typtype": "e"})))]);
            for (order, label) in orders {
                inputs.insert(
                    id("pg_enum", &[label], vec![e.clone()]),
                    input(json!({"enumsortorder": order, "enumlabel": label})),
                );
            }
            inputs
        };
        // `ALTER TYPE ... ADD VALUE ... BEFORE` gives a fractional key.
        assert_eq!(
            shapes(&labels([(1.0, "a"), (1.5, "b")]), &roots),
            shapes(&labels([(1.0, "a"), (2.0, "b")]), &roots)
        );
        // Negative: swapped labels differ.
        assert_ne!(
            shapes(&labels([(1.0, "a"), (2.0, "b")]), &roots),
            shapes(&labels([(2.0, "a"), (1.0, "b")]), &roots)
        );
    }
}
