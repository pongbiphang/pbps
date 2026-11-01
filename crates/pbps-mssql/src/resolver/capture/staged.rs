//! Accepted external shapes, distinct from the full target fingerprints.
//! Bodies and designated guards may differ; every other fact is compared.

use super::select::Input;
use super::{CaptureError, CaptureScope, CapturedInputs, ObjectIdentity, class};
use pbps_db::transport::QueryConnection;
use pbps_model::{ReferentialAction, Schema};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

mod names;
mod target;

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
    pub(super) fn holds(&self, id: &ObjectIdentity) -> bool {
        self.accepted.contains_key(id)
    }

    /// Merge accepted comparisons without overwriting a target observation
    /// made earlier in the run (DEC-2376.1).
    pub fn extend(&mut self, other: Self) -> Result<(), Vec<String>> {
        if self.managed != other.managed {
            return Err(vec![
                "staged comparisons disagree on the managed set".into(),
            ]);
        }

        let changed = self
            .accepted
            .iter()
            .filter(|(id, properties)| other.accepted.get(*id).is_some_and(|p| p != *properties))
            .map(|(id, _)| {
                format!(
                    "the target changed {} between staged comparisons",
                    named(id)
                )
            })
            .collect::<Vec<_>>();
        if !changed.is_empty() {
            return Err(changed);
        }
        self.compared.extend(other.compared);
        self.accepted.extend(other.accepted);
        self.scope.retained.extend(other.scope.retained);
        self.scope.candidates.extend(other.scope.candidates);
        Ok(())
    }

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
            &shapes(
                &current.inputs,
                &self.compared,
                &relevant,
                &self.managed,
                Owners::Target,
            ),
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
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<Staged, StageError> {
    compare_staged_declared(scratch, target, created, managed, &Default::default(), key).await
}

pub async fn compare_staged_declared(
    scratch: &mut impl QueryConnection,
    target: &mut impl QueryConnection,
    created: &BTreeSet<ObjectIdentity>,
    managed: &[&Schema],
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<Staged, StageError> {
    compare_staged_pending(
        scratch,
        target,
        created,
        managed,
        declarations,
        key,
        &BTreeSet::new(),
    )
    .await
}

/// Pending roots are compared by a later native reconstruction step. They
/// cannot contribute an accepted shape before their prerequisites exist.
pub async fn compare_staged_pending(
    scratch: &mut impl QueryConnection,
    target: &mut impl QueryConnection,
    created: &BTreeSet<ObjectIdentity>,
    managed: &[&Schema],
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    pending: &BTreeSet<ObjectIdentity>,
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
    let canonical_created = if created.is_empty() {
        created.clone()
    } else {
        super::capture_declared(
            scratch,
            &CaptureScope {
                retained: created.clone(),
                candidates: BTreeSet::new(),
            },
            declarations,
            key,
        )
        .await
        .map_err(StageError::Capture)?
        .scope()
        .retained
        .clone()
    };
    let created = &canonical_created;
    let mut scope = cascade_scope_for(target, &target_managed, key)
        .await
        .map_err(StageError::Capture)?;
    scope.retained.extend(created.iter().cloned());
    scope.retained.retain(|id| !pending.contains(id));
    let mut on_target = super::capture_declared(target, &scope, declarations, key)
        .await
        .map_err(StageError::Capture)?;
    scope = on_target.scope().clone();
    on_target.inputs.retain(|id, _| !pending.contains(id));
    let held_managed = Names::read(scratch, &target_managed.objects)
        .await
        .map_err(StageError::Capture)?;
    let missing_managed = target_managed
        .objects
        .iter()
        .filter(|id| held_managed.get(id).is_none())
        .cloned()
        .collect();
    let waiting = managed_dependencies(&on_target, &scope.retained, &missing_managed);
    if !waiting.is_empty() {
        return Err(StageError::Refused(waiting));
    }
    let mut staged = super::capture_declared(scratch, &scope, declarations, key)
        .await
        .map_err(StageError::Capture)?;
    staged.inputs.retain(|id, _| !pending.contains(id));
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
    let spelled = requested
        .iter()
        .filter(|id| !id.class.starts_with("pbps-unnamed-"))
        .cloned()
        .collect();
    let target_names = Names::read(target, &spelled)
        .await
        .map_err(StageError::Capture)?;
    let scratch_names = Names::read(scratch, &spelled)
        .await
        .map_err(StageError::Capture)?;
    let mut roots = BTreeSet::new();
    for root in &requested {
        let a = if root.class.starts_with("pbps-unnamed-") {
            on_target.inputs.get_key_value(root).map(|(id, _)| id)
        } else {
            target_names.get(root)
        };
        let b = if root.class.starts_with("pbps-unnamed-") {
            staged.inputs.get_key_value(root).map(|(id, _)| id)
        } else {
            scratch_names.get(root)
        };
        for (label, actual, inputs) in [
            ("target", a, &on_target.inputs),
            ("scratch", b, &staged.inputs),
        ] {
            match actual {
                None => {
                    let waiting = if label == "scratch" {
                        managed_dependencies(
                            &on_target,
                            &BTreeSet::from([root.clone()]),
                            &target_managed.objects,
                        )
                    } else {
                        vec![]
                    };
                    if waiting.is_empty() {
                        refused.push(format!("the {label} does not have {}", named(root)));
                    } else {
                        refused.extend(waiting);
                    }
                }
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
    let accepted = shapes(
        &on_target.inputs,
        &roots,
        &target_barriers,
        &target_managed,
        Owners::Stage,
    );
    // A sys.objects row alone does not cover a kind's dedicated catalogs.
    // The capture deliberately skips Broker, for example; an unreferenced
    // file-created queue must therefore refuse even when both names exist.
    for (id, properties) in &accepted {
        if id.class == class::OBJECT
            && !matches!(
                properties.get("type").and_then(Value::as_str),
                Some(
                    "U" | "V"
                        | "P"
                        | "FN"
                        | "IF"
                        | "TF"
                        | "SO"
                        | "SN"
                        | "C"
                        | "D"
                        | "F"
                        | "PK"
                        | "UQ"
                        | "TR"
                        | "SP"
                        | "EC"
                )
            )
        {
            refused.push(format!(
                "{} has an object kind the staged comparison does not cover",
                named(id)
            ));
        }
    }
    if !refused.is_empty() {
        return Err(StageError::Refused(refused));
    }
    let scratch_shapes = shapes(
        &staged.inputs,
        &roots,
        &scratch_barriers,
        &scratch_managed,
        Owners::Stage,
    );
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
        accepted: shapes(
            &on_target.inputs,
            &roots,
            &target_barriers,
            &target_managed,
            Owners::Target,
        ),
        compared: roots,
        scope,
        managed: target_managed,
    })
}

/// A target survey for automatic staging, retaining the exact catalog names
/// resolved under its collation. This is not accepted scratch evidence.
pub struct ExternalInputs {
    pub(crate) captured: CapturedInputs,
    pub(crate) roots: BTreeSet<ObjectIdentity>,
    pub(crate) managed: BTreeSet<ObjectIdentity>,
}

impl ExternalInputs {
    pub(in crate::resolver) fn module_names(
        module: &pbps_model::Module,
    ) -> Result<target::CreationNames, &'static str> {
        target::CreationNames::module(module)
    }
    pub(in crate::resolver) fn statement_names(
        text: &str,
    ) -> Result<target::CreationNames, &'static str> {
        target::CreationNames::statement(text)
    }

    pub fn managed(&self) -> &BTreeSet<ObjectIdentity> {
        &self.managed
    }
    pub fn captured(&self) -> &CapturedInputs {
        &self.captured
    }
    pub fn roots(&self) -> &BTreeSet<ObjectIdentity> {
        &self.roots
    }
    pub fn prerequisites(&self) -> BTreeMap<ObjectIdentity, BTreeSet<ObjectIdentity>> {
        prerequisites(&self.captured, &self.roots, &self.managed)
    }
    pub fn external_prerequisites(&self) -> BTreeMap<ObjectIdentity, BTreeSet<ObjectIdentity>> {
        // DEC-2376.4: managed providers carry their own prerequisites in the
        // settled program. Crossing their children here invents external cycles.
        let boundaries = self.roots.union(&self.managed).cloned().collect();
        self.roots
            .iter()
            .map(|root| {
                let required = references(&self.captured, root, &boundaries);
                (
                    root.clone(),
                    required.intersection(&self.roots).cloned().collect(),
                )
            })
            .collect()
    }
    pub fn waiting(&self) -> Vec<String> {
        managed_dependencies(&self.captured, &self.roots, &self.managed)
    }
}

pub async fn required_externals(
    target: &mut impl QueryConnection,
    managed: &[&Schema],
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<ExternalInputs, CaptureError> {
    required_externals_declared(target, managed, &Default::default(), key).await
}

pub async fn required_externals_declared(
    target: &mut impl QueryConnection,
    managed: &[&Schema],
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<ExternalInputs, CaptureError> {
    let schemas: Vec<_> = managed.iter().map(|s| (*s).clone()).collect();
    target::prove(target, &schemas).await?;
    let managed = Managed::read(target, &schemas).await?;
    let mut scope = cascade_scope_for(target, &managed, key).await?;
    scope.retained.extend(managed.objects.iter().cloned());
    let captured =
        target::prepare(target, scope, &schemas, &managed.objects, declarations, key).await?;
    let roots = captured
        .inputs
        .iter()
        .filter(|(id, input)| {
            !managed.objects.contains(*id)
                && input.properties.get("engine") != Some(&Value::Bool(true))
                && ((id.class == class::TYPE
                    && input.properties.get("user_defined") == Some(&Value::Bool(true)))
                    || ((id.class == class::OBJECT || id.class.starts_with("pbps-unnamed-"))
                        && (reference(input, "parent").is_none()
                            || captured.scope().retained.contains(*id))))
        })
        .map(|(id, _)| id.clone())
        .collect();
    Ok(ExternalInputs {
        captured,
        roots,
        managed: managed.objects,
    })
}

fn managed_dependencies(
    captured: &CapturedInputs,
    roots: &BTreeSet<ObjectIdentity>,
    managed: &BTreeSet<ObjectIdentity>,
) -> Vec<String> {
    prerequisites(captured, roots, managed)
        .into_iter()
        .flat_map(|(root, managed)| {
            managed.into_iter().map(move |id| {
                format!(
                    "external {} depends on managed {}; it waits for chained staging (#2376)",
                    named(&root),
                    named(&id)
                )
            })
        })
        .collect()
}

fn prerequisites(
    captured: &CapturedInputs,
    roots: &BTreeSet<ObjectIdentity>,
    managed: &BTreeSet<ObjectIdentity>,
) -> BTreeMap<ObjectIdentity, BTreeSet<ObjectIdentity>> {
    roots
        .iter()
        .filter(|root| !managed.contains(*root))
        .filter_map(|root| {
            let required = references(captured, root, managed);
            (!required.is_empty()).then(|| (root.clone(), required))
        })
        .collect()
}

fn references(
    captured: &CapturedInputs,
    root: &ObjectIdentity,
    providers: &BTreeSet<ObjectIdentity>,
) -> BTreeSet<ObjectIdentity> {
    let mut required = BTreeSet::new();
    let mut pending = vec![root.clone()];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if id != *root && providers.contains(&id) {
            required.insert(id);
            continue;
        }
        pending.extend(id.signature.iter().cloned());
        // A relation's typed columns are structural prerequisites; its
        // independently staged guards are not prerequisites of the relation.
        pending.extend(
            captured
                .inputs
                .keys()
                .filter(|child| {
                    child.class == class::COLUMN && child.signature.first() == Some(&id)
                })
                .cloned(),
        );
        if let Some(input) = captured.inputs.get(&id) {
            pending.extend(input.bindings.iter().map(|b| b.target.clone()));
            pending.extend(reference(input, "parent"));
        }
    }
    required
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
            for name in schema.types.keys() {
                schemas.insert(name.schema().into());
                objects.insert(ObjectIdentity {
                    class: class::TYPE.into(),
                    name: vec![name.schema().into(), name.name().into()],
                    signature: vec![],
                });
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
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<CaptureScope, CaptureError> {
    let managed: Vec<_> = managed.iter().map(|s| (*s).clone()).collect();
    let managed = Managed::read(target, &managed).await?;
    cascade_scope_for(target, &managed, key).await
}

async fn cascade_scope_for(
    target: &mut impl QueryConnection,
    managed: &Managed,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<CaptureScope, CaptureError> {
    let mut scope = CaptureScope::default();
    // Capturing the seeds follows their children and every incoming cascade,
    // so paths not directly adjacent to a managed table are present too.
    for (a, b, _) in &managed.seeds {
        scope.retained.extend([a.clone(), b.clone()]);
    }
    let catalog = super::read::read(target, &super::select::lookups(&scope)).await?;
    let selected = super::select::select(&catalog, &scope, &super::identity::Producer::new(key))?;
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

/// The staged skip list also bounds authorization subjects. A child omitted
/// from shape comparison must not become mandatory through owner equality.
/// Explicit roots (including cascade barriers) are retained by the caller.
pub(super) fn skipped_child(input: &Input) -> bool {
    reference(input, "parent").is_some()
        && matches!(
            input.properties.get("type").and_then(Value::as_str),
            Some("C" | "D" | "F" | "TR")
        )
}

// A stage compares owner equality across engines. Private target observations
// retain its stable identity: the partition domain grows during chained staging.
#[derive(Clone, Copy)]
enum Owners {
    Stage,
    Target,
}

fn shapes(
    inputs: &BTreeMap<ObjectIdentity, Input>,
    roots: &BTreeSet<ObjectIdentity>,
    barriers: &BTreeSet<ObjectIdentity>,
    managed: &Managed,
    owners_as: Owners,
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
            let designated = skipped_child(input);
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
                        match owners_as {
                            Owners::Target => json!(owner),
                            Owners::Stage => json!(
                                owners
                                    .iter()
                                    .filter(|(_, o)| *o == owner)
                                    .map(|(id, _)| id)
                                    .collect::<Vec<_>>()
                            ),
                        },
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
    // These views inherit sys.objects' owner field. ALTER AUTHORIZATION can
    // make a schema-inherited owner explicit without changing its equality
    // group; retaining a duplicate would undo the logical owner projection.
    for key in [
        "sys.objects",
        "sys.tables",
        "sys.views",
        "sys.procedures",
        "sys.synonyms",
        "sys.sequences",
        "sys.check_constraints",
        "sys.default_constraints",
        "sys.key_constraints",
        "sys.foreign_keys",
        "sys.triggers",
        "sys.security_policies",
        "sys.edge_constraints",
        "sys.types",
        "sys.table_types",
    ] {
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
    if let Some(name) = id.unnamed_diagnostic() {
        return name;
    }
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
    let mut out: BTreeSet<_> = shapes
        .keys()
        .filter(|id| !id.class.starts_with("pbps-unnamed-"))
        .cloned()
        .collect();
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
    #[test]
    fn extending_staging_keeps_prior_shapes_and_rejects_a_changed_target() {
        use super::*;
        let id = super::super::object("ext", "first");
        let make = |root: ObjectIdentity, value| Staged {
            compared: BTreeSet::from([root.clone()]),
            accepted: BTreeMap::from([(
                root.clone(),
                BTreeMap::from([("shape".into(), json!(value))]),
            )]),
            scope: CaptureScope {
                retained: BTreeSet::from([root]),
                candidates: Default::default(),
            },
            managed: Managed::default(),
        };
        let mut staged = make(id.clone(), 1);
        let original = staged.clone();
        assert!(staged.extend(make(id.clone(), 2)).is_err());
        assert_eq!(staged, original);
        let next = super::super::object("ext", "next");
        staged.extend(make(next.clone(), 3)).unwrap();
        assert_eq!(
            staged.compared(),
            &BTreeSet::from([id.clone(), next.clone()])
        );
        assert_eq!(staged.accepted[&id]["shape"], json!(1));
        assert!(staged.scope().retained.contains(&next));
    }

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
        let target = shapes(
            &inputs,
            &roots,
            &BTreeSet::new(),
            &Managed::default(),
            Owners::Stage,
        );
        assert!(target.contains_key(&policy));
        let mut changed = inputs.clone();
        changed
            .get_mut(&policy)
            .unwrap()
            .properties
            .insert("sys.security_policies".into(), json!({"is_enabled":"0"}));
        assert_ne!(
            target,
            shapes(
                &changed,
                &roots,
                &BTreeSet::new(),
                &Managed::default(),
                Owners::Stage
            )
        );
        // A module's ordinary reference is not ownership of its callee.
        inputs.get_mut(&table).unwrap().bindings.clear();
        assert!(
            !shapes(
                &inputs,
                &roots,
                &BTreeSet::new(),
                &Managed::default(),
                Owners::Stage
            )
            .contains_key(&policy)
        );
    }
}
