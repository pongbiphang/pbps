//! Accepted external shapes, distinct from the full target fingerprints.
//! Bodies and designated guards may differ; every other fact is compared.

use super::select::Input;
use super::{CaptureError, CaptureScope, CapturedInputs, ObjectIdentity, class};
use pbps_db::transport::QueryConnection;
use pbps_model::{ReferentialAction, Schema};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

mod names;

use names::Names;

type Shapes = BTreeMap<ObjectIdentity, BTreeMap<String, Value>>;

#[derive(Clone, PartialEq, Eq)]
pub struct Staged {
    compared: BTreeSet<ObjectIdentity>,
    accepted: Shapes,
    scope: CaptureScope,
    managed: Managed,
}

impl std::fmt::Debug for Staged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Staged")
            .field("compared", &self.compared)
            .finish_non_exhaustive()
    }
}

impl Staged {
    pub fn compared(&self) -> &BTreeSet<ObjectIdentity> {
        &self.compared
    }

    /// Include this scope in the final target read. Accepted properties never
    /// leave through this API; only identities do.
    pub fn scope(&self) -> &CaptureScope {
        &self.scope
    }

    /// The target read which will be sealed must still have the shapes scratch
    /// accepted. Otherwise compilation and its evidence describe different states.
    pub fn changed_since(&self, current: &CapturedInputs) -> Vec<String> {
        let relevant = barriers(&current.inputs, &self.managed);
        differences(
            &self.accepted,
            &shapes(&current.inputs, &self.compared, &relevant, &self.managed),
            "the target",
        )
    }
}

#[derive(Debug)]
pub enum StageError {
    Capture(CaptureError),
    Refused(Vec<String>),
}

/// Compare every root from the post-file inventory. Unknown classes and managed
/// overlaps refuse, including unreferenced roots. File execution/inventory and
/// rendering remain #2269; this function never runs a module body.
/// `managed` contains every side of the managed schema the plan touches.
pub async fn compare_staged(
    scratch: &mut impl QueryConnection,
    target: &mut impl QueryConnection,
    created: &BTreeSet<ObjectIdentity>,
    managed: &[&Schema],
) -> Result<Staged, StageError> {
    let managed: Vec<_> = managed.iter().map(|s| (*s).clone()).collect();
    let mut refused = Vec::new();
    for root in created {
        if ![class::OBJECT, class::TYPE, class::SCHEMA].contains(&root.class.as_str())
            || !root.signature.is_empty()
            || root.name.len() != if root.class == class::SCHEMA { 1 } else { 2 }
        {
            refused.push(format!(
                "the externals file created {}, which the comparison does not cover",
                named(root)
            ));
        }
    }
    if !refused.is_empty() {
        return Err(StageError::Refused(refused));
    }
    let target_managed = Managed::read(target, &managed)
        .await
        .map_err(StageError::Capture)?;
    let scratch_managed = Managed::read(scratch, &managed)
        .await
        .map_err(StageError::Capture)?;
    let created_names = Names::read(scratch, created)
        .await
        .map_err(StageError::Capture)?;
    for root in created {
        if created_names
            .get(root)
            .is_some_and(|id| scratch_managed.objects.contains(id))
        {
            refused.push(format!(
                "the externals file created {}, which is in the managed set",
                named(root)
            ));
        }
    }
    if !refused.is_empty() {
        return Err(StageError::Refused(refused));
    }
    let mut scope = cascade_scope_for(target, &target_managed)
        .await
        .map_err(StageError::Capture)?;
    scope.retained.extend(created.iter().cloned());
    let on_target = super::capture(target, &scope)
        .await
        .map_err(StageError::Capture)?;
    let staged = super::capture(scratch, &scope)
        .await
        .map_err(StageError::Capture)?;
    let requested: BTreeSet<_> = created
        .iter()
        .cloned()
        .chain(
            scope
                .retained
                .iter()
                .filter(|id| !target_managed.objects.contains(*id))
                .cloned(),
        )
        .collect();
    let target_names = Names::read(target, &requested)
        .await
        .map_err(StageError::Capture)?;
    let scratch_names = Names::read(scratch, &requested)
        .await
        .map_err(StageError::Capture)?;
    let mut roots = BTreeSet::new();
    for root in &requested {
        let a = target_names.get(root);
        let b = scratch_names.get(root);
        for (label, actual, inputs) in [
            ("target", a, &on_target.inputs),
            ("scratch", b, &staged.inputs),
        ] {
            match actual {
                None => refused.push(format!("the {label} does not have {}", named(root))),
                Some(id) if !inputs.contains_key(id) => {
                    return Err(StageError::Capture(CaptureError::Changed));
                }
                _ => {}
            }
        }
        if let (Some(a), Some(b)) = (a, b) {
            if a != b {
                refused.push(spelling(a, b));
            }
            roots.insert(a.clone());
        }
    }
    if !refused.is_empty() {
        return Err(StageError::Refused(refused));
    }
    let target_barriers = barriers(&on_target.inputs, &target_managed);
    let scratch_barriers = barriers(&staged.inputs, &scratch_managed);
    let accepted = shapes(&on_target.inputs, &roots, &target_barriers, &target_managed);
    let scratch_shapes = shapes(&staged.inputs, &roots, &scratch_barriers, &scratch_managed);
    let mut findings = spelling_differences(scratch, &accepted, &scratch_shapes)
        .await
        .map_err(StageError::Capture)?;
    if findings.is_empty() {
        findings = differences(&accepted, &scratch_shapes, "the externals file");
    }
    if !findings.is_empty() {
        return Err(StageError::Refused(findings));
    }
    Ok(Staged {
        compared: roots,
        accepted,
        scope,
        managed: target_managed,
    })
}

// Every identity from YAML is resolved in the catalog where it will be used.
// A declaration not present yet retains its requested identity, not a guessed
// fold; the engine may create it later. The accepted handoff keeps target names.
#[derive(Clone, Default, PartialEq, Eq)]
struct Managed {
    objects: BTreeSet<ObjectIdentity>,
    seeds: Vec<(ObjectIdentity, ObjectIdentity, &'static str)>,
    schemas: BTreeSet<String>,
}

impl Managed {
    async fn read(
        conn: &mut impl QueryConnection,
        managed: &[Schema],
    ) -> Result<Self, CaptureError> {
        let mut objects = BTreeSet::new();
        let mut schemas = BTreeSet::from(["dbo".to_owned()]);
        for schema in managed {
            for (table, t) in &schema.tables {
                schemas.insert(table.schema.clone());
                objects.insert(super::object(&table.schema, &table.name));
                objects.extend(
                    t.foreign_keys
                        .keys()
                        .chain(t.unique.keys())
                        .chain(t.primary_key.iter().filter_map(|p| p.name.as_ref()))
                        .map(|name| super::object(&table.schema, name.raw())),
                );
            }
            for name in schema.modules.keys() {
                schemas.insert(name.schema().into());
                objects.insert(super::object(name.schema(), name.name()));
            }
        }
        let seeds = seeds(managed);
        let schema_ids: BTreeSet<_> = schemas
            .iter()
            .map(|name| ObjectIdentity {
                class: class::SCHEMA.into(),
                name: vec![name.clone()],
                signature: vec![],
            })
            .collect();
        let requested = objects
            .iter()
            .cloned()
            .chain(schema_ids.iter().cloned())
            .chain(seeds.iter().flat_map(|(a, b, _)| [a.clone(), b.clone()]))
            .collect();
        let names = Names::read(conn, &requested).await?;
        let resolve = |id: ObjectIdentity| names.get(&id).cloned().unwrap_or(id);
        Ok(Self {
            objects: objects.into_iter().map(resolve).collect(),
            seeds: seeds
                .into_iter()
                .map(|(a, b, action)| (resolve(a), resolve(b), action))
                .collect(),
            schemas: schema_ids
                .into_iter()
                .map(resolve)
                .map(|id| id.name[0].clone())
                .collect(),
        })
    }
}

/// Identities a renderer must reproduce for cascade legality. Traverse external
/// CASCADE edges in both directions, separately for DELETE and UPDATE. SET NULL,
/// SET DEFAULT and NO ACTION do not enter this closure (measured on 17.0).
/// Returned tables supply the keys and column mapping; the engine judges legality.
pub async fn cascade_scope(
    target: &mut impl QueryConnection,
    managed: &[&Schema],
) -> Result<CaptureScope, CaptureError> {
    let managed: Vec<_> = managed.iter().map(|s| (*s).clone()).collect();
    let managed = Managed::read(target, &managed).await?;
    cascade_scope_for(target, &managed).await
}

async fn cascade_scope_for(
    target: &mut impl QueryConnection,
    managed: &Managed,
) -> Result<CaptureScope, CaptureError> {
    let mut scope = CaptureScope::default();
    // Capturing the seeds follows their children and every incoming cascade,
    // so paths not directly adjacent to a managed table are present too.
    for (a, b, _) in &managed.seeds {
        scope.retained.extend([a.clone(), b.clone()]);
    }
    let catalog = super::read::read(target, &super::select::lookups(&scope)).await?;
    let selected = super::select::select(&catalog, &scope)?;
    let relevant = barriers(&selected.inputs, managed);
    scope.retained.extend(relevant);
    for id in scope.retained.clone() {
        if let Some(input) = selected.inputs.get(&id) {
            if let Some(parent) = reference(input, "parent") {
                scope.retained.insert(parent);
            }
            if let Some(parent) = input
                .properties
                .get("foreign_key")
                .and_then(|v| v.get("references"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
            {
                scope.retained.insert(parent);
            }
        }
    }
    Ok(scope)
}

fn seeds(managed: &[Schema]) -> Vec<(ObjectIdentity, ObjectIdentity, &'static str)> {
    let mut out = Vec::new();
    for schema in managed {
        for (name, table) in &schema.tables {
            for fk in table.foreign_keys.values() {
                for (action, value) in [("on_delete", fk.on_delete), ("on_update", fk.on_update)] {
                    if value == ReferentialAction::Cascade {
                        out.push((
                            super::object(&name.schema, &name.name),
                            super::object(&fk.references_table.schema, &fk.references_table.name),
                            action,
                        ));
                    }
                }
            }
        }
    }
    out
}

fn reference(input: &Input, key: &str) -> Option<ObjectIdentity> {
    serde_json::from_value(input.properties.get(key)?.clone()).ok()
}

fn barriers(
    inputs: &BTreeMap<ObjectIdentity, Input>,
    managed: &Managed,
) -> BTreeSet<ObjectIdentity> {
    let mut kept = BTreeSet::new();
    for action in ["on_delete", "on_update"] {
        let mut nodes: BTreeSet<_> = managed
            .seeds
            .iter()
            .filter(|(_, _, a)| *a == action)
            .cloned()
            .flat_map(|(a, b, _)| [a, b])
            .collect();
        let mut referencing: BTreeSet<_> = managed
            .seeds
            .iter()
            .filter(|(_, _, a)| *a == action)
            .cloned()
            .map(|(a, _, _)| a)
            .collect();
        loop {
            let before = nodes.len();
            for (id, input) in inputs {
                let Some(fk) = input.properties.get("foreign_key") else {
                    continue;
                };
                if fk.get(action).and_then(Value::as_str) != Some("CASCADE")
                    || managed.objects.contains(id)
                {
                    continue;
                }
                let (Some(child), Some(parent)) = (
                    reference(input, "parent"),
                    fk.get("references")
                        .and_then(|v| serde_json::from_value::<ObjectIdentity>(v.clone()).ok()),
                ) else {
                    continue;
                };
                if nodes.contains(&child) || nodes.contains(&parent) {
                    nodes.extend([child.clone(), parent]);
                    referencing.insert(child);
                    kept.insert(id.clone());
                }
            }
            if before == nodes.len() {
                break;
            }
        }
        let event = if action == "on_delete" {
            "DELETE"
        } else {
            "UPDATE"
        };
        for (id, input) in inputs {
            if input.properties.get("instead_of") == Some(&json!(true))
                && reference(input, "parent").is_some_and(|p| referencing.contains(&p))
                && input
                    .properties
                    .get("sys.trigger_events")
                    .and_then(Value::as_array)
                    .is_some_and(|events| {
                        events
                            .iter()
                            .any(|e| e.get("type_desc").and_then(Value::as_str) == Some(event))
                    })
            {
                kept.insert(id.clone());
            }
        }
    }
    kept
}

fn shapes(
    inputs: &BTreeMap<ObjectIdentity, Input>,
    roots: &BTreeSet<ObjectIdentity>,
    barriers: &BTreeSet<ObjectIdentity>,
    managed: &Managed,
) -> Shapes {
    let mut included = roots.clone();
    loop {
        let before = included.len();
        for (id, input) in inputs {
            let parent = if id.class == class::COLUMN {
                id.signature.first().cloned()
            } else {
                reference(input, "parent")
            };
            if included.contains(id) {
                // A security policy hangs off the table through its predicate
                // binding, not parent_object_id. It is not a skipped guard.
                included.extend(
                    input
                        .bindings
                        .iter()
                        .filter(|b| b.node == "sys.security_predicates")
                        .map(|b| b.target.clone()),
                );
            }
            let designated = matches!(
                input.properties.get("type").and_then(Value::as_str),
                Some("C" | "D" | "F" | "TR")
            );
            if parent.is_some_and(|p| included.contains(&p))
                && (!designated || barriers.contains(id))
            {
                included.insert(id.clone());
            }
        }
        if included.len() == before {
            break;
        }
    }
    included.extend(barriers.iter().cloned());
    // Owner names differ by construction. Preserve the equality partition over
    // held schemas and objects, including dbo as the common anchor.
    let schemas: BTreeSet<_> = included
        .iter()
        .filter(|i| i.class == class::OBJECT || i.class == class::TYPE)
        .filter_map(|i| i.name.first().cloned())
        .chain(managed.schemas.iter().cloned())
        .collect();
    included.extend(
        inputs
            .keys()
            .filter(|i| {
                i.class == class::SCHEMA && i.name.first().is_some_and(|n| schemas.contains(n))
            })
            .cloned(),
    );
    let owners: BTreeMap<_, _> = included
        .iter()
        .filter_map(|id| {
            inputs
                .get(id)
                .and_then(|input| {
                    reference(input, "owner").or_else(|| {
                        if id.class != class::TYPE {
                            return None;
                        }
                        // Types carry the same explicit-or-schema ownership rule
                        // in their whole catalog row rather than an object row.
                        input
                            .properties
                            .get("sys.types")
                            .and_then(|row| row.get("principal_id"))
                            .and_then(|value| serde_json::from_value(value.clone()).ok())
                            .or_else(|| {
                                inputs
                                    .get(&ObjectIdentity {
                                        class: class::SCHEMA.into(),
                                        name: vec![id.name[0].clone()],
                                        signature: vec![],
                                    })
                                    .and_then(|schema| reference(schema, "owner"))
                            })
                    })
                })
                .map(|owner| (id.clone(), owner))
        })
        .collect();
    included
        .iter()
        .filter_map(|id| {
            inputs.get(id).map(|input| {
                let mut properties = projected(&input.properties);
                if let Some(owner) = owners.get(id) {
                    properties.insert(
                        "owner_group".into(),
                        json!(
                            owners
                                .iter()
                                .filter(|(_, o)| *o == owner)
                                .map(|(id, _)| id)
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                (id.clone(), properties)
            })
        })
        .collect()
}

fn projected(properties: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut out = properties.clone();
    out.remove("definition");
    out.remove("owner");
    for key in ["sys.objects", "sys.types", "sys.table_types"] {
        if let Some(row) = out.get_mut(key).and_then(Value::as_object_mut) {
            row.remove("principal_id");
        }
    }
    if let Some(row) = out
        .get_mut("sys.sql_modules")
        .and_then(Value::as_object_mut)
    {
        row.remove("definition");
    }
    if let Some(row) = out.get_mut("sys.tables").and_then(Value::as_object_mut) {
        row.remove("lock_on_bulk_load");
    }
    if let Some(row) = out
        .get_mut("OBJECTPROPERTYEX")
        .and_then(Value::as_object_mut)
    {
        use super::object_properties::{PropertyDisposition as P, property_disposition};
        row.retain(|name, _| {
            !matches!(
                property_disposition(name),
                Some(P::ChildSummary | P::Data | P::Owner)
            )
        });
    }
    // Defaults have both a child object and a reference in the column/type row.
    for key in ["sys.columns", "sys.types"] {
        if let Some(row) = out.get_mut(key).and_then(Value::as_object_mut) {
            row.remove("default_object_id");
        }
    }
    project_indexes(&mut out);
    if let Some(columns) = out.get_mut("columns").and_then(Value::as_array_mut) {
        for column in columns {
            if let Some(row) = column.get_mut("sys.columns").and_then(Value::as_object_mut) {
                row.remove("default_object_id");
            }
        }
    }
    if let Some(structure) = out.get_mut("structure").and_then(Value::as_object_mut) {
        structure.remove("checks");
        if let Some(columns) = structure.get_mut("columns").and_then(Value::as_array_mut) {
            for column in columns {
                if let Some(row) = column.as_object_mut() {
                    row.remove("default");
                }
            }
        }
    }
    out
}

fn project_indexes(properties: &mut BTreeMap<String, Value>) {
    if let Some(indexes) = properties
        .get_mut("sys.indexes")
        .and_then(Value::as_array_mut)
    {
        indexes.retain(|row| {
            row.get("index")
                .and_then(|r| r.get("is_unique"))
                .and_then(Value::as_str)
                == Some("1")
        });
    }
}

fn named(id: &ObjectIdentity) -> String {
    let parent = id
        .signature
        .first()
        .map(|p| format!(" of {}", named(p)))
        .unwrap_or_default();
    format!("{} {:?}{parent}", id.class, id.name)
}

fn spelling(target: &ObjectIdentity, scratch: &ObjectIdentity) -> String {
    format!(
        "the externals file spells {} as {}",
        named(target),
        named(scratch)
    )
}

fn shape_names(shapes: &Shapes) -> BTreeSet<ObjectIdentity> {
    let mut out: BTreeSet<_> = shapes.keys().cloned().collect();
    for (id, properties) in shapes {
        if let Some(indexes) = properties.get("sys.indexes").and_then(Value::as_array) {
            for index in indexes {
                if let Some(name) = index.get("name").and_then(Value::as_str) {
                    out.insert(names::member(names::INDEX, id, name));
                }
            }
        }
        if id.class == class::TYPE
            && let Some(columns) = properties.get("columns").and_then(Value::as_array)
        {
            for column in columns {
                if let Some(name) = column
                    .get("sys.columns")
                    .and_then(|c| c.get("name"))
                    .and_then(Value::as_str)
                {
                    out.insert(names::member(class::COLUMN, id, name));
                }
            }
        }
    }
    out
}

async fn spelling_differences(
    scratch: &mut impl QueryConnection,
    target: &Shapes,
    staged: &Shapes,
) -> Result<Vec<String>, CaptureError> {
    if target == staged {
        return Ok(vec![]);
    }
    let a = shape_names(target);
    let b = shape_names(staged);
    let requested = a.difference(&b).cloned().collect();
    let resolved = Names::read(scratch, &requested).await?;
    Ok(a.difference(&b)
        .filter_map(|id| {
            resolved
                .get(id)
                .filter(|actual| b.contains(*actual))
                .map(|actual| spelling(id, actual))
        })
        .collect())
}

fn differences(target: &Shapes, staged: &Shapes, label: &str) -> Vec<String> {
    target
        .keys()
        .chain(staged.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|id| match (target.get(id), staged.get(id)) {
            (Some(a), Some(b)) if a != b => {
                let fields: BTreeSet<_> = a
                    .keys()
                    .chain(b.keys())
                    .filter(|k| a.get(*k) != b.get(*k))
                    .collect();
                Some(format!(
                    "{label}'s {} differs in {}",
                    named(id),
                    fields
                        .into_iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            (Some(_), None) => Some(format!("{label} lacks {}", named(id))),
            (None, Some(_)) => Some(format!("{label} gained {}", named(id))),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_scalar_catalog_property_is_compared() {
        let original = BTreeMap::from([(
            "sys.tables".into(),
            json!({"future_scalar": "1", "lock_on_bulk_load": "0"}),
        )]);
        let mut changed = original.clone();
        changed.get_mut("sys.tables").unwrap()["future_scalar"] = json!("2");
        assert_ne!(projected(&original), projected(&changed));
        changed = original.clone();
        changed.get_mut("sys.tables").unwrap()["lock_on_bulk_load"] = json!("1");
        assert_eq!(projected(&original), projected(&changed));
    }

    #[test]
    fn module_text_is_removed_from_both_representations() {
        let properties = |text| {
            BTreeMap::from([
                ("definition".into(), json!(text)),
                (
                    "sys.sql_modules".into(),
                    json!({"definition": text, "is_schema_bound": "1"}),
                ),
            ])
        };
        assert_eq!(projected(&properties("one")), projected(&properties("two")));
        let mut changed = properties("one");
        changed.get_mut("sys.sql_modules").unwrap()["is_schema_bound"] = json!("0");
        assert_ne!(projected(&properties("one")), projected(&changed));
    }
    #[test]
    fn table_type_defaults_are_skipped_in_each_nested_representation() {
        let properties = |value| {
            BTreeMap::from([
                (
                    "structure".into(),
                    json!({"columns":[{"name":"a","default":value,"nullable":true}],"checks":[value]}),
                ),
                (
                    "columns".into(),
                    json!([{"sys.columns":{"name":"a","default_object_id":value,"is_nullable":"1"}}]),
                ),
            ])
        };
        assert_eq!(projected(&properties("one")), projected(&properties("two")));
        let mut changed = properties("one");
        changed.get_mut("columns").unwrap()[0]["sys.columns"]["is_nullable"] = json!("0");
        assert_ne!(projected(&properties("one")), projected(&changed));
    }

    #[test]
    fn retained_semantic_properties_and_unknown_names_are_never_prefix_skipped() {
        for name in [
            "IsDeterministic",
            "IsPrecise",
            "UserDataAccess",
            "SystemDataAccess",
            "TableFullTextChangeTrackingOn",
            "TableFullTextFutureConfiguration",
        ] {
            let properties =
                |value| BTreeMap::from([("OBJECTPROPERTYEX".into(), json!({name:value}))]);
            assert_ne!(
                projected(&properties(0)),
                projected(&properties(1)),
                "{name}"
            );
            assert_eq!(projected(&properties(0)), projected(&properties(0)));
        }
    }

    #[test]
    fn staged_debug_exposes_identities_without_accepted_catalog_values() {
        let id = super::super::object("dbo", "t");
        let staged = Staged {
            compared: BTreeSet::from([id.clone()]),
            accepted: BTreeMap::from([(id, BTreeMap::from([("private".into(), json!("secret"))]))]),
            scope: CaptureScope::default(),
            managed: Managed::default(),
        };
        let shown = format!("{staged:?}");
        assert!(shown.contains("compared"));
        assert!(!shown.contains("private"));
        assert!(!shown.contains("secret"));
    }
    #[test]
    fn a_security_policy_is_compared_through_its_table_binding() {
        let table = super::super::object("dbo", "t");
        let policy = super::super::object("dbo", "policy");
        let input = |properties| Input {
            properties,
            bindings: BTreeSet::new(),
        };
        let mut inputs = BTreeMap::from([
            (
                table.clone(),
                input(BTreeMap::from([("type".into(), json!("U"))])),
            ),
            (
                policy.clone(),
                input(BTreeMap::from([(
                    "sys.security_policies".into(),
                    json!({"is_enabled":"1"}),
                )])),
            ),
        ]);
        inputs
            .get_mut(&table)
            .unwrap()
            .bindings
            .insert(super::super::select::Binding {
                node: "sys.security_predicates".into(),
                path: vec![],
                target: policy.clone(),
            });
        let roots = BTreeSet::from([table.clone()]);
        let target = shapes(&inputs, &roots, &BTreeSet::new(), &Managed::default());
        assert!(target.contains_key(&policy));
        let mut changed = inputs.clone();
        changed
            .get_mut(&policy)
            .unwrap()
            .properties
            .insert("sys.security_policies".into(), json!({"is_enabled":"0"}));
        assert_ne!(
            target,
            shapes(&changed, &roots, &BTreeSet::new(), &Managed::default())
        );
        // A module's ordinary reference is not ownership of its callee.
        inputs.get_mut(&table).unwrap().bindings.clear();
        assert!(
            !shapes(&inputs, &roots, &BTreeSet::new(), &Managed::default()).contains_key(&policy)
        );
    }
}
