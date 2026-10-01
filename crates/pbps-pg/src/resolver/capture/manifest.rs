//! Private complete input records and versioned cryptographic comparison.
//! No external verifier, source, or derived checksum is an ordinary report.

use super::{CaptureScope, Uncovered, bindings::Binding, logical, properties, read, scope};
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
    /// Same-snapshot pg_roles OIDs classify shared-dependency pinning on
    /// qualified PG16/18. Physical numbers never enter sealed properties.
    role_pinned: BTreeMap<ObjectIdentity, bool>,
    /// Raw attribute numbers prove physical children before the ordinal is
    /// erased from durable catalog properties. Never seal these positions.
    pub(super) attribute_numbers: BTreeMap<ObjectIdentity, i32>,
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

struct ParentColumnOrder {
    source: ObjectIdentity,
    columns: Vec<ObjectIdentity>,
    separate_table_mutation: bool,
    key_change: bool,
}

// A declaration map cannot predict the live order of an existing relation.
// Replay only approved vector operations against the opening UID-backed order;
// physical slots and the independently compiled scratch order are not answers.
fn projected_column_order(
    opening: &CapturedInputs,
    compiled: &CapturedInputs,
    changes: &pbps_model::ChangeSet,
    base_ids: &pbps_model::IdsFile,
    desired_ids: &pbps_model::IdsFile,
    table: &pbps_model::TableName,
) -> Result<Option<ParentColumnOrder>, pbps_model::resolver::ManifestError> {
    use pbps_model::Change;
    use pbps_model::resolver::ManifestError;
    if !changes.changes.iter().any(|step| {
        matches!(
            &step.change,
            Change::AddColumn { .. } | Change::DropColumn { .. } | Change::RenameColumn { .. }
        )
    }) {
        return Ok(None);
    }
    let mut recorded_tables = desired_ids.tables.iter().filter(|(_, name)| *name == table);
    let (table_uid, _) = recorded_tables.next().ok_or(ManifestError::Invalid)?;
    if recorded_tables.next().is_some() {
        return Err(ManifestError::Invalid);
    }
    let Some(prior) = base_ids.tables.get(table_uid) else {
        // Only an explicit CREATE explains the absence of an opening order.
        if changes.changes.iter().any(|step| {
            matches!(&step.change, Change::CreateTable { uid, .. } if uid == table_uid)
        }) {
            return Ok(None);
        }
        return Err(ManifestError::Invalid);
    };
    if base_ids
        .tables
        .values()
        .filter(|name| *name == prior)
        .count()
        != 1
    {
        return Err(ManifestError::Invalid);
    }
    let mut current = prior.clone();
    let mut affected = false;
    let mut separate = false;
    let mut key_change = false;
    for step in &changes.changes {
        if let Change::RenameTable { uid, from, to, .. } = &step.change
            && uid == table_uid
        {
            if &current != from {
                return Err(ManifestError::Invalid);
            }
            current = to.clone();
            separate = true;
        }
        if let Change::AddColumn { table, .. } | Change::RenameColumn { table, .. } = &step.change {
            affected |= table == &current;
        } else if let Change::DropColumn { column, .. } = &step.change {
            affected |= column.table == current;
        }
        if let Change::SetPrimaryKey { table, .. }
            | Change::AddUnique { table, .. }
            | Change::DropUnique { table, .. }
            | Change::AddForeignKey { table, .. }
            | Change::DropForeignKey { table, .. } = &step.change
        {
            separate |= table == &current;
            key_change |= table == &current;
        }
    }
    if !affected {
        return Ok(None);
    }
    if &current != table {
        return Err(ManifestError::Invalid);
    }
    let source = relation_identity(prior);
    let read_order = |capture: &CapturedInputs, relation: &ObjectIdentity| {
        let input = capture
            .inputs
            .get(relation)
            .ok_or(ManifestError::Incomplete)?;
        if !matches!(
            input.properties.get("relkind").and_then(Value::as_str),
            Some("r" | "p")
        ) {
            return Err(ManifestError::Invalid);
        }
        let order: Vec<ObjectIdentity> = serde_json::from_value(
            input
                .properties
                .get("column_order")
                .cloned()
                .ok_or(ManifestError::Incomplete)?,
        )
        .map_err(|_| ManifestError::Invalid)?;
        let mut seen = BTreeSet::new();
        for column in &order {
            if column.class != "column"
                || column.name.len() != 1
                || column.name[0].is_empty()
                || column.signature != [relation.clone()]
                || !seen.insert(column.clone())
                || !capture.inputs.contains_key(column)
            {
                return Err(ManifestError::Invalid);
            }
        }
        let mut live = BTreeMap::new();
        for (column, number) in &capture.attribute_numbers {
            if *number > 0
                && column.signature == [relation.clone()]
                && live.insert(*number, column.clone()).is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        if seen != live.values().cloned().collect() {
            return Err(ManifestError::Incomplete);
        }
        if order != live.into_values().collect::<Vec<_>>() {
            return Err(ManifestError::Invalid);
        }
        Ok(order)
    };
    let opening_order = read_order(opening, &source)?;
    let mut order = Vec::new();
    let mut used = BTreeSet::new();
    for column in opening_order {
        let reference = prior.column(&column.name[0]);
        let mut recorded = base_ids.columns.iter().filter(|(_, name)| *name == &reference);
        let (uid, _) = recorded.next().ok_or(ManifestError::Invalid)?;
        if recorded.next().is_some() {
            return Err(ManifestError::Invalid);
        }
        let uid = uid.clone();
        if !used.insert(uid.clone()) {
            return Err(ManifestError::Invalid);
        }
        order.push((uid, column));
    }
    current = prior.clone();
    for step in &changes.changes {
        if let Change::RenameTable { uid, from, to, .. } = &step.change
            && uid == table_uid
        {
            if &current != from {
                return Err(ManifestError::Invalid);
            }
            current = to.clone();
            for (_, column) in &mut order {
                column.signature = vec![relation_identity(&current)];
            }
        } else if let Change::DropTable { uid, .. } | Change::CreateTable { uid, .. } = &step.change
            && uid == table_uid
        {
            return Err(ManifestError::Invalid);
        } else if let Change::AddColumn { uid, table, name, .. } = &step.change
            && table == &current
        {
            if name.is_empty()
                || !used.insert(uid.clone())
                || order.iter().any(|(_, column)| column.name == [name.clone()])
            {
                return Err(ManifestError::Invalid);
            }
            order.push((
                uid.clone(),
                ObjectIdentity {
                    class: "column".into(),
                    name: vec![name.clone()],
                    signature: vec![relation_identity(&current)],
                },
            ));
        } else if let Change::DropColumn { uid, column, .. } = &step.change
            && column.table == current
        {
            let position = order
                .iter()
                .position(|(known, object)| known == uid && object.name == [column.name.clone()])
                .ok_or(ManifestError::Invalid)?;
            order.remove(position);
        } else if let Change::RenameColumn { uid, table, from, to, .. } = &step.change
            && table == &current
        {
            if to.is_empty() || order.iter().any(|(_, column)| column.name == [to.clone()]) {
                return Err(ManifestError::Invalid);
            }
            let (_, column) = order
                .iter_mut()
                .find(|(known, column)| known == uid && column.name == [from.clone()])
                .ok_or(ManifestError::Invalid)?;
            column.name = vec![to.clone()];
        }
    }
    let final_relation = relation_identity(table);
    let mut projected = Vec::new();
    for (uid, column) in order {
        let reference = table.column(&column.name[0]);
        if desired_ids.columns.get(&uid) != Some(&reference)
            || desired_ids.columns.values().filter(|name| *name == &reference).count() != 1
        {
            return Err(ManifestError::Invalid);
        }
        projected.push(column);
    }
    let desired_order = read_order(compiled, &final_relation)?;
    if projected.iter().collect::<BTreeSet<_>>() != desired_order.iter().collect::<BTreeSet<_>>() {
        return Err(ManifestError::Incomplete);
    }
    Ok(Some(ParentColumnOrder {
        source,
        columns: projected,
        separate_table_mutation: separate,
        key_change,
    }))
}

/// Validate explicit index creation against the unmodified scratch capture.
/// Table-rename projection changes the parent's owner later in the seal, so
/// comparing an index with that mutable parent would depend on surface order.
fn qualified_explicit_indexes(
    captured: &CapturedInputs,
    ownership: &BTreeMap<ObjectIdentity, pbps_model::resolver::ObjectOwnership>,
    surfaces: &BTreeSet<pbps_model::resolver::Surface>,
) -> Result<BTreeSet<ObjectIdentity>, pbps_model::resolver::ManifestError> {
    use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
    let mut indexes = BTreeSet::new();
    for surface in surfaces {
        let Surface::Index { table, name } = surface else {
            return Err(ManifestError::Invalid);
        };
        let index = ObjectIdentity {
            class: "pg_class".into(),
            name: vec![table.schema.clone(), name.clone()],
            signature: Vec::new(),
        };
        if ownership.get(&index) != Some(&ObjectOwnership::Surface(surface.clone())) {
            return Err(ManifestError::Incomplete);
        }
        let row = captured
            .inputs
            .get(&index)
            .ok_or(ManifestError::Incomplete)?;
        // The measured CREATE INDEX path produces ordinary indexes. A
        // partitioned index has a different relkind and is not inferred here.
        if row.properties.get("relkind").and_then(Value::as_str) != Some("i") {
            return Err(ManifestError::Invalid);
        }
        let metadata = ObjectIdentity {
            class: "pg_index".into(),
            name: Vec::new(),
            signature: vec![index.clone()],
        };
        let parent: ObjectIdentity = serde_json::from_value(
            captured
                .inputs
                .get(&metadata)
                .and_then(|input| input.properties.get("indrelid"))
                .cloned()
                .ok_or(ManifestError::Incomplete)?,
        )
        .map_err(|_| ManifestError::Invalid)?;
        if parent != relation_identity(table) {
            return Err(ManifestError::Invalid);
        }
        let parent_owner = captured
            .inputs
            .get(&parent)
            .and_then(|input| input.properties.get("relowner"))
            .ok_or(ManifestError::Incomplete)?;
        if row.properties.get("relowner") != Some(parent_owner)
            || captured.inputs.keys().any(|id| {
                id.class == "pg_shdepend"
                    && id.name == ["o"]
                    && id.signature.first() == Some(&index)
            })
        {
            return Err(ManifestError::Invalid);
        }
        indexes.insert(index);
    }
    Ok(indexes)
}

/// A constraint-backed or explicit index belongs to its table's owner,
/// even when scratch used a different creator. The recorded table UID selects
/// the opening owner across a rename; a newly created table uses this plan's
/// effective creator.
fn table_owner_for_index(
    opening: &CapturedInputs,
    base_ids: &pbps_model::IdsFile,
    desired_ids: &pbps_model::IdsFile,
    table: &pbps_model::TableName,
    creator: &ObjectIdentity,
) -> Result<ObjectIdentity, pbps_model::resolver::ManifestError> {
    use pbps_model::resolver::ManifestError;
    let uid = desired_ids.table_uid(table).ok_or(ManifestError::Invalid)?;
    let Some(prior) = base_ids.tables.get(uid) else {
        return Ok(creator.clone());
    };
    let prior = relation_identity(prior);
    let owner: ObjectIdentity = serde_json::from_value(
        opening
            .inputs
            .get(&prior)
            .and_then(|input| input.properties.get("relowner"))
            .cloned()
            .ok_or(ManifestError::Incomplete)?,
    )
    .map_err(|_| ManifestError::Invalid)?;
    if owner.class != "pg_authid" || !opening.role_pinned.contains_key(&owner) {
        return Err(ManifestError::Invalid);
    }
    Ok(owner)
}

// Target metadata inserted into a scratch capture has already been named in
// the opening catalog. Keep that provenance until sealing: an opening role
// must not be mistaken for the scratch server's native role of the same name,
// or mapped a second time as though it were a run-local role.
const TARGET_ROLE: &str = "resolver-target-authid";
const SCRATCH_ROLE: &str = "resolver-scratch-authid";

fn target_identity(object: &ObjectIdentity) -> ObjectIdentity {
    let mut result = object.clone();
    result.signature = object.signature.iter().map(target_identity).collect();
    if result.class == "pg_authid" {
        result.class = TARGET_ROLE.into();
    }
    result
}

fn target_value(value: &Value) -> Result<Value, pbps_model::resolver::ManifestError> {
    use pbps_model::resolver::ManifestError;
    if let Value::Object(map) = value {
        if map.get("class").and_then(Value::as_str) == Some("pg_authid")
            || map.get("class").and_then(Value::as_str) == Some(TARGET_ROLE)
        {
            let identity: ObjectIdentity =
                serde_json::from_value(value.clone()).map_err(|_| ManifestError::Invalid)?;
            return serde_json::to_value(target_identity(&identity))
                .map_err(|_| ManifestError::Invalid);
        }
        return Ok(Value::Object(
            map.iter()
                .map(|(name, member)| Ok((name.clone(), target_value(member)?)))
                .collect::<Result<_, ManifestError>>()?,
        ));
    }
    if let Value::Array(items) = value {
        return Ok(Value::Array(
            items.iter().map(target_value).collect::<Result<_, _>>()?,
        ));
    }
    Ok(value.clone())
}

/// A retained subject keeps only metadata the typed statement preserves.
/// The recorded change and its UID-backed transition locate the opening
/// subject; a reused spelling cannot serve as a substitute source.
fn retained_source(
    object: &ObjectIdentity,
    transition: &pbps_model::resolver::ObjectTransition,
    opening: &CapturedInputs,
    compiled: &CapturedInputs,
    base_ids: &pbps_model::IdsFile,
    desired_ids: &pbps_model::IdsFile,
) -> Option<ObjectIdentity> {
    use pbps_model::resolver::Surface;
    let final_table = match &transition.surface {
        Surface::Table(table) => table,
        Surface::Column(column) => &column.table,
        Surface::Namespace(_) | Surface::Module(_) => {
            return transition.before.contains(object).then(|| object.clone());
        }
        Surface::Default(_) | Surface::Check { .. } | Surface::Index { .. } => return None,
    };
    let prior_table = desired_ids
        .table_uid(final_table)
        .and_then(|uid| base_ids.tables.get(uid))?;
    let before_relation = relation_identity(prior_table);
    let before_column = |name: &str| {
        let uid = desired_ids.column_uid(&final_table.column(name))?;
        let prior = base_ids.columns.get(uid)?;
        (prior.table == *prior_table).then(|| ObjectIdentity {
            class: "column".into(),
            name: vec![prior.name.clone()],
            signature: vec![before_relation.clone()],
        })
    };
    let reference = |capture: &CapturedInputs, owner: &ObjectIdentity, field: &str| {
        capture
            .inputs
            .get(owner)?
            .properties
            .get(field)
            .and_then(|value| serde_json::from_value::<ObjectIdentity>(value.clone()).ok())
    };
    let source = match object.class.as_str() {
        "pg_class" if object.name == [final_table.schema.clone(), final_table.name.clone()] => {
            before_relation
        }
        "column" => before_column(object.name.first()?)?,
        "pg_type" => {
            // PostgreSQL renames a relation's row and array types with the
            // table. Match the qualified type references, never a guessed
            // `_`-prefixed name that could belong to a different object.
            let old_row = reference(opening, &before_relation, "reltype")?;
            let new_row = reference(compiled, &relation_identity(final_table), "reltype")?;
            if object == &new_row {
                old_row
            } else {
                let old_array = reference(opening, &old_row, "typarray")?;
                let new_array = reference(compiled, &new_row, "typarray")?;
                if object != &new_array {
                    return None;
                }
                old_array
            }
        }
        _ => return None,
    };
    transition.before.contains(&source).then_some(source)
}

// PostgreSQL 18 retains a NOT NULL constraint's name across an in-place
// table/column rename. The automatic dependency identifies the exact column
// that made it; the generated spelling is never evidence of that ownership.
fn not_null_child(
    captured: &CapturedInputs,
    column: &ObjectIdentity,
) -> Result<Option<ObjectIdentity>, pbps_model::resolver::ManifestError> {
    use pbps_model::resolver::ManifestError;
    let [relation] = column.signature.as_slice() else {
        return Err(ManifestError::Invalid);
    };
    let mut found = None;
    for (constraint, input) in captured.inputs.iter().filter(|(id, input)| {
        id.class == "pg_constraint"
            && input.properties.get("contype").and_then(Value::as_str) == Some("n")
            && id.signature.get(1) == Some(relation)
    }) {
        let keys: Vec<ObjectIdentity> = serde_json::from_value(
            input
                .properties
                .get("conkey")
                .cloned()
                .ok_or(ManifestError::Incomplete)?,
        )
        .map_err(|_| ManifestError::Invalid)?;
        if keys != [column.clone()] {
            continue;
        }
        if constraint.name.len() != 1
            || constraint.signature.len() != 3
            || input.properties.get("conname").and_then(Value::as_str)
                != constraint.name.first().map(String::as_str)
        {
            return Err(ManifestError::Invalid);
        }
        let edge = ObjectIdentity {
            class: "pg_depend".into(),
            name: vec!["a".into()],
            signature: vec![constraint.clone(), column.clone()],
        };
        if !captured.inputs.contains_key(&edge) {
            return Err(ManifestError::Incomplete);
        }
        if found.replace(constraint.clone()).is_some() {
            return Err(ManifestError::Invalid);
        }
    }
    Ok(found)
}

fn relocated_identity(
    object: &ObjectIdentity,
    names: &BTreeMap<ObjectIdentity, ObjectIdentity>,
) -> ObjectIdentity {
    if let Some(replaced) = names.get(object) {
        return replaced.clone();
    }
    let mut result = object.clone();
    result.signature = object
        .signature
        .iter()
        .map(|part| relocated_identity(part, names))
        .collect();
    result
}

// Catalog properties encode references as complete logical identities, while
// scalar strings and SQL definitions are not reference addresses.
fn relocated_value(
    value: &Value,
    names: &BTreeMap<ObjectIdentity, ObjectIdentity>,
) -> Result<Value, pbps_model::resolver::ManifestError> {
    use pbps_model::resolver::ManifestError;
    match value {
        Value::Object(members)
            if members.contains_key("class")
                && members.contains_key("name")
                && members.contains_key("signature") =>
        {
            let object: ObjectIdentity =
                serde_json::from_value(value.clone()).map_err(|_| ManifestError::Invalid)?;
            serde_json::to_value(relocated_identity(&object, names))
                .map_err(|_| ManifestError::Invalid)
        }
        Value::Object(members) => Ok(Value::Object(
            members
                .iter()
                .map(|(field, member)| Ok((field.clone(), relocated_value(member, names)?)))
                .collect::<Result<_, ManifestError>>()?,
        )),
        Value::Array(members) => Ok(Value::Array(
            members
                .iter()
                .map(|member| relocated_value(member, names))
                .collect::<Result<_, _>>()?,
        )),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(value.clone()),
    }
}

fn relocated_set(
    objects: &BTreeSet<ObjectIdentity>,
    names: &BTreeMap<ObjectIdentity, ObjectIdentity>,
) -> Result<BTreeSet<ObjectIdentity>, pbps_model::resolver::ManifestError> {
    let result: BTreeSet<_> = objects
        .iter()
        .map(|object| relocated_identity(object, names))
        .collect();
    if result.len() != objects.len() {
        return Err(pbps_model::resolver::ManifestError::Invalid);
    }
    Ok(result)
}

/// A one-shot fixed-key seal refusal. The public boundary conveys only a
/// safe category, never a captured property, chosen mapping or verifier.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error(transparent)]
    Manifest(#[from] pbps_model::resolver::ManifestError),
    #[error("a rebuilt routine's opening ACL is missing or unreadable")]
    OpeningAcl,
    #[error(
        "a rebuilt routine carries WITH GRANT OPTION, which the declarations cannot restore; revoke the grant option and plan again"
    )]
    GrantOption,
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

    // This consuming capability already holds the fixed key and both fresh
    // captures. Only recorded UIDs and typed in-place renames may relocate a
    // retained child; the caller cannot supply an identity mapping.
    fn retain_renamed_not_null(
        &mut self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        transitions: &[pbps_model::resolver::ObjectTransition],
        base_ids: &pbps_model::IdsFile,
        desired_ids: &pbps_model::IdsFile,
    ) -> Result<(), pbps_model::resolver::ManifestError> {
        use pbps_model::Change;
        use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
        base_ids.validate().map_err(|_| ManifestError::Invalid)?;
        desired_ids.validate().map_err(|_| ManifestError::Invalid)?;
        if opening.major != self.captured.major {
            return Err(ManifestError::Incomplete);
        }
        let mut names = BTreeMap::new();
        let mut retained = BTreeMap::new();
        for (uid, old) in &base_ids.columns {
            let Some(final_column) = desired_ids.columns.get(uid) else {
                continue;
            };
            if old == final_column {
                continue;
            }
            let table_uid = base_ids
                .table_uid(&old.table)
                .ok_or(ManifestError::Invalid)?;
            if desired_ids.table_uid(&final_column.table) != Some(table_uid) {
                return Err(ManifestError::Invalid);
            }
            if old.table != final_column.table
                && !changes.changes.iter().any(|step| {
                    matches!(
                        &step.change,
                        Change::RenameTable { uid: moved, from, to, .. }
                            if moved == table_uid && from == &old.table && to == &final_column.table
                    )
                })
            {
                return Err(ManifestError::Invalid);
            }
            if old.name != final_column.name
                && !changes.changes.iter().any(|step| {
                    matches!(
                        &step.change,
                        Change::RenameColumn { uid: moved, table, from, to, .. }
                            if moved == uid && table == &final_column.table
                                && from == &old.name && to == &final_column.name
                    )
                })
            {
                return Err(ManifestError::Invalid);
            }
            let old_column = ObjectIdentity {
                class: "column".into(),
                name: vec![old.name.clone()],
                signature: vec![relation_identity(&old.table)],
            };
            let new_column = ObjectIdentity {
                class: "column".into(),
                name: vec![final_column.name.clone()],
                signature: vec![relation_identity(&final_column.table)],
            };
            let old_not_null = opening
                .inputs
                .get(&old_column)
                .and_then(|input| input.properties.get("attnotnull"))
                .and_then(Value::as_bool)
                .ok_or(ManifestError::Incomplete)?;
            let new_not_null = self
                .captured
                .inputs
                .get(&new_column)
                .and_then(|input| input.properties.get("attnotnull"))
                .and_then(Value::as_bool)
                .ok_or(ManifestError::Incomplete)?;
            let mut nullable_after = None;
            for step in &changes.changes {
                let to_nullable = match &step.change {
                    Change::AlterColumnType {
                        uid: changed,
                        column,
                        from_nullable,
                        to_nullable,
                        ..
                    } if changed == uid => {
                        if column != final_column
                            || *from_nullable != !old_not_null
                            || *to_nullable != !new_not_null
                        {
                            return Err(ManifestError::Incomplete);
                        }
                        Some(*to_nullable)
                    }
                    Change::AlterColumnNullability {
                        uid: changed,
                        column,
                        to_nullable,
                        ..
                    } if changed == uid => {
                        if column != final_column || *to_nullable != !new_not_null {
                            return Err(ManifestError::Incomplete);
                        }
                        Some(*to_nullable)
                    }
                    Change::CreateTable { .. }
                    | Change::DropTable { .. }
                    | Change::RenameTable { .. }
                    | Change::AddColumn { .. }
                    | Change::DropColumn { .. }
                    | Change::RenameColumn { .. }
                    | Change::AlterColumnType { .. }
                    | Change::AlterColumnNullability { .. }
                    | Change::AlterColumnDefault { .. }
                    | Change::AlterColumnExpression { .. }
                    | Change::SetColumnDeprecated { .. }
                    | Change::SetPrimaryKey { .. }
                    | Change::AddUnique { .. }
                    | Change::DropUnique { .. }
                    | Change::AddForeignKey { .. }
                    | Change::DropForeignKey { .. }
                    | Change::AddCheck { .. }
                    | Change::DropCheck { .. }
                    | Change::AddIndex { .. }
                    | Change::DropIndex { .. }
                    | Change::InsertRow { .. }
                    | Change::UpdateRow { .. }
                    | Change::DeleteRow { .. }
                    | Change::SetDataMode { .. }
                    | Change::CreateModule { .. }
                    | Change::AlterModule { .. }
                    | Change::DropModule { .. }
                    | Change::CreateRole { .. }
                    | Change::DropRole { .. }
                    | Change::RenameRole { .. }
                    | Change::Grant { .. }
                    | Change::Revoke { .. }
                    | Change::PublicExecution { .. } => None,
                };
                if let Some(to_nullable) = to_nullable
                    && nullable_after.replace(to_nullable).is_some()
                {
                    return Err(ManifestError::Invalid);
                }
            }
            if nullable_after.is_none() && old_not_null != new_not_null {
                return Err(ManifestError::Incomplete);
            }
            let old_child = not_null_child(opening, &old_column)?;
            let new_child = not_null_child(&self.captured, &new_column)?;
            // A missing PG18 child is not evidence of a nullable column: the
            // captured attnotnull value and exact automatic edge must agree
            // on each side before any retained-name projection is allowed.
            match opening.major {
                18 if old_child.is_some() != old_not_null
                    || new_child.is_some() != new_not_null =>
                {
                    return Err(ManifestError::Incomplete);
                }
                16 if old_child.is_some() || new_child.is_some() => {
                    return Err(ManifestError::Incomplete);
                }
                16 | 18 => {}
                _ => return Err(ManifestError::Invalid),
            }
            let (old_child, new_child) = match (old_child, new_child) {
                (Some(old_child), Some(new_child)) if old_not_null && new_not_null => {
                    // A type rewrite recreates the physical PG18 row but
                    // retains its old name. Keep scratch structural properties.
                    (old_child, new_child)
                }
                (Some(_), None)
                    if old_not_null && !new_not_null && nullable_after == Some(true) =>
                {
                    continue;
                }
                (None, Some(_))
                    if !old_not_null && new_not_null && nullable_after == Some(false) =>
                {
                    continue;
                }
                // PostgreSQL 16 has no separate child; PG18 nullable columns
                // likewise have none on either side.
                (None, None) => continue,
                _ => return Err(ManifestError::Incomplete),
            };
            if !transitions.iter().any(|t| t.before.contains(&old_child))
                || !transitions.iter().any(|t| t.after.contains(&new_child))
                || self.ownership.get(&new_child)
                    != Some(&ObjectOwnership::Surface(Surface::Column(
                        final_column.clone(),
                    )))
            {
                return Err(ManifestError::Incomplete);
            }
            let mut projected = new_child.clone();
            projected.name.clone_from(&old_child.name);
            if names.insert(new_child.clone(), projected).is_some()
                || retained
                    .insert(new_child, old_child.name[0].clone())
                    .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        if names.is_empty() {
            return Ok(());
        }
        let destinations: BTreeSet<_> = names.values().collect();
        if destinations.len() != names.len()
            || names
                .iter()
                .any(|(from, to)| from != to && self.captured.inputs.contains_key(to))
        {
            return Err(ManifestError::Invalid);
        }
        for (object, name) in retained {
            let input = self
                .captured
                .inputs
                .get_mut(&object)
                .ok_or(ManifestError::Incomplete)?;
            input
                .properties
                .insert("conname".into(), Value::String(name));
        }
        let mut inputs = BTreeMap::new();
        for (object, mut input) in std::mem::take(&mut self.captured.inputs) {
            for value in input.properties.values_mut() {
                *value = relocated_value(value, &names)?;
            }
            for binding in &mut input.bindings {
                binding.target = relocated_identity(&binding.target, &names);
            }
            if inputs
                .insert(relocated_identity(&object, &names), input)
                .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        self.captured.inputs = inputs;
        let mut ownership = BTreeMap::new();
        for (object, owner) in std::mem::take(&mut self.ownership) {
            if ownership
                .insert(relocated_identity(&object, &names), owner)
                .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        self.ownership = ownership;
        let mut numbers = BTreeMap::new();
        for (object, number) in std::mem::take(&mut self.captured.attribute_numbers) {
            if numbers
                .insert(relocated_identity(&object, &names), number)
                .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        self.captured.attribute_numbers = numbers;
        for members in self.captured.candidates.values_mut() {
            *members = relocated_set(members, &names)?;
        }
        self.captured.scope.retained = relocated_set(&self.captured.scope.retained, &names)?;
        self.captured.limitations = relocated_set(&self.captured.limitations, &names)?;
        for object in self.captured.dropped.values_mut().flatten() {
            *object = relocated_identity(object, &names);
        }
        Ok(())
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

    /// A replacement cannot restore an opening routine ACL grant option:
    /// `Grant` expresses the privilege but carries no grant-option bit. The
    /// ordinary connected rebuild guard refuses the same shape (DEC-95,
    /// DEC-447). Check only final typed replacements, using their qualified
    /// opening transition and the retained capture that produced the verdict.
    /// The caller can only observe this check by consuming the seal below.
    fn admit_rebuilt_routine_grant_options(
        &self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        transitions: &[pbps_model::resolver::ObjectTransition],
    ) -> Result<(), SealError> {
        use pbps_model::resolver::Surface;
        use pbps_model::{Change, ModuleId};
        if opening.major != self.captured.major {
            return Err(SealError::OpeningAcl);
        }
        let dropped: BTreeSet<_> = changes
            .changes
            .iter()
            .filter_map(|step| match &step.change {
                Change::DropModule { id, .. } => Some(id.clone()),
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnExpression { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => None,
            })
            .collect();
        let rebuilt: BTreeSet<_> = changes
            .changes
            .iter()
            .filter_map(|step| match &step.change {
                Change::AlterModule { id, .. } => Some(id.clone()),
                Change::CreateModule { id, .. } if dropped.contains(id) => Some(id.clone()),
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnExpression { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => None,
            })
            .collect();
        for id in rebuilt {
            let ModuleId::Routine(_) = id else {
                continue;
            };
            let mut matching = transitions
                .iter()
                .filter(|transition| transition.surface == Surface::Module(id.clone()));
            let transition = matching.next().ok_or(SealError::OpeningAcl)?;
            if matching.next().is_some() {
                return Err(SealError::OpeningAcl);
            }
            let mut roots = transition
                .before
                .iter()
                .filter(|object| object.class == "pg_proc");
            let root = roots.next().ok_or(SealError::OpeningAcl)?;
            if roots.next().is_some() {
                return Err(SealError::OpeningAcl);
            }
            let acl = opening
                .inputs
                .get(root)
                .and_then(|input| input.properties.get("proacl"))
                .ok_or(SealError::OpeningAcl)?;
            match acl {
                Value::Null => {}
                Value::Array(entries) => {
                    for entry in entries {
                        match entry.get("grant_option").and_then(Value::as_bool) {
                            Some(true) => return Err(SealError::GrantOption),
                            Some(false) => {}
                            None => return Err(SealError::OpeningAcl),
                        }
                    }
                }
                Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Object(_) => {
                    return Err(SealError::OpeningAcl);
                }
            }
        }
        Ok(())
    }

    /// Consume the fixed-key producer once. This admission guard and the
    /// compiled seal observe the same opening capture and exact final plan;
    /// no separately callable capture probe escapes the producer.
    pub fn seal_for_plan(
        self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        transitions: &[pbps_model::resolver::ObjectTransition],
        base_ids: &pbps_model::IdsFile,
        desired_ids: &pbps_model::IdsFile,
        authorization: &crate::resolver::authorization::AuthorizationContext,
    ) -> Result<pbps_model::resolver::InputManifest, SealError> {
        self.admit_rebuilt_routine_grant_options(opening, changes, transitions)?;
        self.seal_for_plan_inner(
            opening,
            changes,
            transitions,
            base_ids,
            desired_ids,
            authorization,
        )
        .map_err(SealError::from)
    }

    /// Consume the fixed-key producer once the exact final typed sequence is
    /// known. Typed table/column renames retain measured opening metadata;
    /// rebuilt and newly created objects receive the opening target's
    /// effective creation defaults. No caller supplies a property mapping.
    fn seal_for_plan_inner(
        mut self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        transitions: &[pbps_model::resolver::ObjectTransition],
        base_ids: &pbps_model::IdsFile,
        desired_ids: &pbps_model::IdsFile,
        authorization: &crate::resolver::authorization::AuthorizationContext,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        let effective_creator = authorization.principal.effective.as_str();
        use pbps_model::Change;
        use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
        let added_indexes: BTreeSet<Surface> = changes
            .changes
            .iter()
            .filter_map(|step| match &step.change {
                Change::AddIndex { table, name, .. } => Some(Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                }),
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnExpression { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::DropIndex { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => None,
            })
            .collect();
        let verified_indexes =
            qualified_explicit_indexes(&self.captured, &self.ownership, &added_indexes)?;
        let lookup: BTreeMap<_, _> = self
            .captured
            .inputs
            .keys()
            .map(|raw| Ok((normalize_identity(raw, Some(&self.roles))?, raw.clone())))
            .collect::<Result<_, ManifestError>>()?;
        let mut column_names = BTreeMap::new();
        for (uid, before) in &base_ids.columns {
            if let Some(after) = desired_ids.columns.get(uid)
                && before != after
            {
                if !changes.changes.iter().any(|step| {
                    matches!(&step.change,
                        Change::RenameColumn { uid: renamed, .. } if renamed == uid)
                        || matches!(&step.change, Change::RenameTable { uid: renamed, .. }
                            if base_ids.tables.get(renamed) == Some(&before.table))
                }) {
                    return Err(ManifestError::Invalid);
                }
                let identity = |column: &pbps_model::ColumnRef| ObjectIdentity {
                    class: "column".into(),
                    name: vec![column.name.clone()],
                    signature: vec![relation_identity(&column.table)],
                };
                column_names.insert(identity(before), identity(after));
            }
        }
        let mut parent_orders = BTreeMap::new();
        for transition in transitions {
            if let Surface::Table(table) = &transition.surface
                && !transition.after.is_empty()
                && let Some(parent) = projected_column_order(
                    opening,
                    &self.captured,
                    changes,
                    base_ids,
                    desired_ids,
                    table,
                )?
            {
                let relation = relation_identity(table);
                if !transition.before.contains(&parent.source)
                    || !transition.after.contains(&relation)
                    || self.ownership.get(&relation)
                        != Some(&ObjectOwnership::Surface(transition.surface.clone()))
                {
                    return Err(ManifestError::Incomplete);
                }
                parent_orders.insert(transition.surface.clone(), parent);
            }
        }
        let mut preserved_dependencies = BTreeSet::new();
        let mut created_dependencies = BTreeSet::new();
        let mut created_owned_subjects = BTreeSet::new();
        let mut dependency_mappings = Vec::new();
        let mut dependencies_to_copy = Vec::new();
        let mut created_owner_edges = Vec::new();
        let mut updated_acl_subjects = BTreeSet::new();
        let creator = ObjectIdentity {
            class: "pg_authid".into(),
            name: vec![effective_creator.into()],
            signature: Vec::new(),
        };
        for transition in transitions {
            let created = transition.before.is_empty()
                || changes.changes.iter().any(|step| {
                    matches!(
                        &step.change,
                        Change::CreateModule { id, .. } | Change::AlterModule { id, .. }
                            if transition.surface == Surface::Module(id.clone())
                    )
                });
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
                // A transition may retain its table while replacing its
                // constraint or index. Preserve only records with a proved
                // opening counterpart, never the entire surface inventory.
                let parent = parent_orders.get(&transition.surface);
                let parent_relation = if let Surface::Table(table) = &transition.surface {
                    object == &relation_identity(table)
                } else {
                    false
                };
                let preserve_parent = parent.is_some_and(|parent| {
                    parent_relation || !parent.separate_table_mutation
                }) && owner == &ObjectOwnership::Surface(transition.surface.clone());
                let source = if preserve_parent {
                    let source = if parent_relation {
                        parent.ok_or(ManifestError::Invalid)?.source.clone()
                    } else {
                        object.clone()
                    };
                    if !transition.before.contains(&source) {
                        return Err(ManifestError::Incomplete);
                    }
                    Some(source)
                } else {
                    (!created)
                        .then(|| {
                            retained_source(
                                object,
                                transition,
                                opening,
                                &self.captured,
                                base_ids,
                                desired_ids,
                            )
                        })
                        .flatten()
                };
                if source.is_none() {
                    created_owned_subjects.insert(object.clone());
                }
                if let Some(source) = source {
                    let before = opening
                        .inputs
                        .get(&source)
                        .ok_or(ManifestError::Incomplete)?;
                    let mut fields: Vec<&str> = if preserve_parent {
                        before.properties.keys().map(String::as_str).collect()
                    } else {
                        match object.class.as_str() {
                            "pg_class" => vec!["relowner", "relacl"],
                            "pg_proc" => vec!["proowner", "proacl"],
                            "pg_namespace" => vec!["nspowner", "nspacl"],
                            "column" => vec!["attacl"],
                            "pg_type" => vec!["typowner"],
                            _ => Vec::new(),
                        }
                    };
                    if parent.is_some() && parent_relation && !fields.contains(&"column_order") {
                        fields.push("column_order");
                    }
                    let references_changed = preserve_parent
                        && (before.properties.values().any(|value| {
                            relocated_value(value, &column_names)
                                .is_ok_and(|mapped| mapped != *value)
                        }) || before.bindings.iter().any(|binding| {
                            relocated_identity(&binding.target, &column_names) != binding.target
                        }));
                    if !fields.is_empty() {
                        let mut values = Vec::new();
                        let mut changed_acl = None;
                        for field in &fields {
                            let original = before
                                .properties
                                .get(*field)
                                .ok_or(ManifestError::Incomplete)?;
                            let relocated = if preserve_parent {
                                relocated_value(original, &column_names)?
                            } else {
                                original.clone()
                            };
                            // A column operation preserves the existing table;
                            // only a separately approved rename/key operation
                            // may replace these other relation properties.
                            let renamed_relation = parent_relation && source != *object
                                && matches!(*field, "relname" | "relnamespace" | "reltype");
                            let changed_key = parent_relation && *field == "relreplident"
                                && parent.is_some_and(|parent| parent.key_change);
                            let projected = if renamed_relation || changed_key
                                || *field == "engine_definition" && references_changed
                            {
                                Some(
                                    self.captured
                                        .inputs
                                        .get(raw)
                                        .and_then(|input| input.properties.get(*field))
                                        .cloned()
                                        .ok_or(ManifestError::Incomplete)?,
                                )
                            } else if parent_relation && *field == "column_order" {
                                Some(
                                    serde_json::to_value(
                                        &parent.ok_or(ManifestError::Invalid)?.columns,
                                    )
                                    .map_err(|_| ManifestError::Invalid)?,
                                )
                            } else if matches!(*field, "relacl" | "proacl" | "nspacl") {
                                super::creation_acl::retained_acl_after_plan(
                                    opening,
                                    &self.captured,
                                    raw,
                                    &source,
                                    &transition.surface,
                                    changes,
                                    authorization,
                                )?
                            } else {
                                None
                            };
                            if let Some(acl) = &projected
                                && matches!(*field, "relacl" | "proacl" | "nspacl")
                            {
                                changed_acl = Some(acl.clone());
                            }
                            values.push((
                                (*field).to_owned(),
                                target_value(projected.as_ref().unwrap_or(&relocated))?,
                            ));
                        }
                        if let Some(acl) = changed_acl {
                            updated_acl_subjects.insert(object.clone());
                            let owner_field = match object.class.as_str() {
                                "pg_class" => "relowner",
                                "pg_proc" => "proowner",
                                "pg_namespace" => "nspowner",
                                _ => return Err(ManifestError::Invalid),
                            };
                            let owner: ObjectIdentity = serde_json::from_value(
                                before
                                    .properties
                                    .get(owner_field)
                                    .cloned()
                                    .ok_or(ManifestError::Incomplete)?,
                            )
                            .map_err(|_| ManifestError::Invalid)?;
                            let Value::Array(entries) = acl else {
                                return Err(ManifestError::Invalid);
                            };
                            for entry in entries {
                                for principal in ["grantor", "grantee"] {
                                    let role: ObjectIdentity = serde_json::from_value(
                                        entry
                                            .get(principal)
                                            .cloned()
                                            .ok_or(ManifestError::Invalid)?,
                                    )
                                    .map_err(|_| ManifestError::Invalid)?;
                                    if role.class == "pg_authid" && role != owner {
                                        dependencies_to_copy.push((
                                            object.clone(),
                                            role,
                                            transition.surface.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        let after = self
                            .captured
                            .inputs
                            .get_mut(raw)
                            .ok_or(ManifestError::Incomplete)?;
                        for (field, value) in values {
                            after.properties.insert(field, value);
                        }
                        if preserve_parent {
                            after.bindings = before
                                .bindings
                                .iter()
                                .map(|binding| Binding {
                                    node: binding.node.clone(),
                                    path: binding.path.clone(),
                                    target: target_identity(&relocated_identity(
                                        &binding.target,
                                        &column_names,
                                    )),
                                })
                                .collect();
                        }
                        preserved_dependencies.insert(object.clone());
                        dependency_mappings.push((
                            source,
                            object.clone(),
                            transition.surface.clone(),
                        ));
                    }
                } else if created || !transition.before.contains(object) {
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
                        let acl = if kind == "f" {
                            let Surface::Module(pbps_model::ModuleId::Routine(routine)) =
                                &transition.surface
                            else {
                                return Err(ManifestError::Incomplete);
                            };
                            super::creation_acl::routine_acl_after_plan(
                                opening,
                                &self.captured,
                                raw,
                                effective_creator,
                                namespace,
                                routine,
                                changes,
                            )?
                        } else {
                            super::creation_acl::creation_acl(
                                opening,
                                effective_creator,
                                namespace,
                                kind,
                            )?
                        };
                        let target_acl = target_value(&acl)?;
                        let after = self
                            .captured
                            .inputs
                            .get_mut(raw)
                            .ok_or(ManifestError::Incomplete)?;
                        after.properties.insert(acl_field.into(), target_acl);
                        after.properties.insert(
                            owner_field.into(),
                            serde_json::to_value(target_identity(&creator))
                                .map_err(|_| ManifestError::Invalid)?,
                        );
                        created_dependencies.insert(object.clone());
                        // A class without an owner edge on scratch has no
                        // edge to project. For classes that do, the scratch
                        // role must resolve to this plan's effective creator.
                        let mut observed = self.captured.inputs.keys().filter(|id| {
                            id.class == "pg_shdepend"
                                && id.name == ["o"]
                                && id.signature.first() == Some(raw)
                        });
                        if let Some(edge) = observed.next() {
                            let role = edge.signature.get(1).ok_or(ManifestError::Invalid)?;
                            if observed.next().is_some()
                                || normalize_identity(role, Some(&self.roles))? != creator
                            {
                                return Err(ManifestError::Invalid);
                            }
                            created_owner_edges.push((object.clone(), transition.surface.clone()));
                        }
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
                if let Surface::Table(table) = &transition.surface
                    && object.class == "pg_class"
                    && owner == &ObjectOwnership::Surface(transition.surface.clone())
                    && matches!(
                        self.captured
                            .inputs
                            .get(raw)
                            .and_then(|input| input.properties.get("relkind"))
                            .and_then(Value::as_str),
                        Some("i" | "I")
                    )
                {
                    // Only the exact internal dependency of a qualified key
                    // constraint may carry a table-owned index. An ordinary
                    // index is independently declared on Surface::Index.
                    let mut parents = self.captured.inputs.keys().filter(|id| {
                        id.class == "pg_depend"
                            && id.name == ["i"]
                            && id.signature.first() == Some(raw)
                            && id.signature.get(1).is_some_and(|constraint| {
                                constraint.class == "pg_constraint"
                                    && self.ownership.get(constraint)
                                        == Some(&ObjectOwnership::Surface(
                                            transition.surface.clone(),
                                        ))
                            })
                    });
                    let dependency = parents.next().ok_or(ManifestError::Incomplete)?;
                    if parents.next().is_some() {
                        return Err(ManifestError::Invalid);
                    }
                    let constraint = dependency.signature[1].clone();
                    let metadata = ObjectIdentity {
                        class: "pg_index".into(),
                        name: Vec::new(),
                        signature: vec![raw.clone()],
                    };
                    let parent = relation_identity(table);
                    if self.captured.inputs.get(&metadata).is_none_or(|row| {
                        row.properties.get("indrelid")
                            != serde_json::to_value(&parent).ok().as_ref()
                    }) || self.captured.inputs.get(&constraint).is_none_or(|row| {
                        !matches!(
                            row.properties.get("contype").and_then(Value::as_str),
                            Some("p" | "u")
                        ) || row.properties.get("conindid")
                            != serde_json::to_value(raw).ok().as_ref()
                    }) || self.captured.inputs.keys().any(|id| {
                        id.class == "pg_shdepend"
                            && id.name == ["o"]
                            && id.signature.first() == Some(raw)
                    }) {
                        return Err(ManifestError::Incomplete);
                    }
                    let target_owner =
                        table_owner_for_index(opening, base_ids, desired_ids, table, &creator)?;
                    self.captured
                        .inputs
                        .get_mut(raw)
                        .ok_or(ManifestError::Incomplete)?
                        .properties
                        .insert(
                            "relowner".into(),
                            serde_json::to_value(target_identity(&target_owner))
                                .map_err(|_| ManifestError::Invalid)?,
                        );
                }
                if let Surface::Index { table, .. } = &transition.surface
                    && added_indexes.contains(&transition.surface)
                    && verified_indexes.contains(raw)
                    && owner == &ObjectOwnership::Surface(transition.surface.clone())
                {
                    // PostgreSQL creates an explicit index under its table's
                    // owner, even when another role issues CREATE INDEX. The
                    // recorded table UID selects the opening owner across a
                    // rename; a newly created table has this plan's creator.
                    let target_owner =
                        table_owner_for_index(opening, base_ids, desired_ids, table, &creator)?;
                    self.captured
                        .inputs
                        .get_mut(raw)
                        .ok_or(ManifestError::Incomplete)?
                        .properties
                        .insert(
                            "relowner".into(),
                            serde_json::to_value(target_identity(&target_owner))
                                .map_err(|_| ManifestError::Invalid)?,
                        );
                }
            }
        }
        // Dependency rows are addressed by their owned subject. A typed
        // rename keeps the target's owner/ACL edges, but their catalog address
        // must use the final subject. Scratch's rows would report its transient
        // owner instead.
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
                            || created_dependencies.contains(&subject)
                                && (id.name == ["a"] || id.name == ["o"])
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for id in old_dependencies {
            self.captured.inputs.remove(&id);
            self.ownership.remove(&id);
        }
        // Auto-created children may have owner edges even though their
        // metadata needs no explicit owner rewrite above. The opening
        // target role's pin class decides whether each observed scratch edge
        // survives the same DDL on target.
        let scratch_owner_edges = self
            .captured
            .inputs
            .keys()
            .filter(|id| id.class == "pg_shdepend" && id.name == ["o"])
            .cloned()
            .collect::<Vec<_>>();
        for id in scratch_owner_edges {
            let [subject, role] = id.signature.as_slice() else {
                return Err(ManifestError::Invalid);
            };
            let subject = normalize_identity(subject, Some(&self.roles))?;
            if !created_owned_subjects.contains(&subject) {
                continue;
            }
            let role = normalize_identity(role, Some(&self.roles))?;
            let pinned = opening
                .role_pinned
                .get(&role)
                .ok_or(ManifestError::Incomplete)?;
            if *pinned {
                self.captured.inputs.remove(&id);
                self.ownership.remove(&id);
            }
        }
        for (source, destination, surface) in dependency_mappings {
            for (id, input) in &opening.inputs {
                if id.class != "pg_shdepend"
                    || id.signature.first() != Some(&source)
                    || id.name == ["a"] && updated_acl_subjects.contains(&destination)
                {
                    continue;
                }
                let mut final_id = id.clone();
                final_id.signature[0] = destination.clone();
                for role in final_id.signature.iter_mut().skip(1) {
                    *role = target_identity(role);
                }
                self.captured.inputs.insert(final_id.clone(), input.clone());
                self.ownership
                    .insert(final_id, ObjectOwnership::Surface(surface.clone()));
            }
        }
        if !created_owner_edges.is_empty() {
            // Scratch's owner is a run-local ordinary role. Re-addressing its
            // edge to a pinned target owner invents a dependency; dropping it
            // for an ordinary target owner loses a real one. Classify the
            // effective creator from the same opening pg_roles inventory.
            let pinned = opening
                .role_pinned
                .get(&creator)
                .ok_or(ManifestError::Incomplete)?;
            if !*pinned {
                for (subject, surface) in created_owner_edges {
                    let id = ObjectIdentity {
                        class: "pg_shdepend".into(),
                        name: vec!["o".into()],
                        signature: vec![subject, target_identity(&creator)],
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
            }
        }
        for (subject, role, surface) in dependencies_to_copy {
            // PostgreSQL records an ACL edge for each unpinned role. A final
            // GRANT may name a role absent from every opening default ACL,
            // so absence of an earlier edge cannot classify that role.
            let pinned = opening
                .role_pinned
                .get(&role)
                .ok_or(ManifestError::Incomplete)?;
            if *pinned {
                continue;
            }
            let id = ObjectIdentity {
                class: "pg_shdepend".into(),
                name: vec!["a".into()],
                signature: vec![subject, target_identity(&role)],
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
        self.retain_renamed_not_null(opening, changes, transitions, base_ids, desired_ids)?;
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
                let mut bindings: Vec<pbps_model::resolver::Binding> = input
                    .bindings
                    .iter()
                    .map(|binding| {
                        Ok(pbps_model::resolver::Binding {
                            node: binding.node.clone(),
                            path: binding.path.clone(),
                            target: normalize_identity(&binding.target, Some(&self.roles))?,
                        })
                    })
                    .collect::<Result<_, pbps_model::resolver::ManifestError>>()?;
                // The raw capture orders paths first; the sealed manifest
                // orders model Bindings after role normalization. Resolution
                // must carry that same order into BoundSurface.
                bindings.sort();
                Ok(BindingRecord {
                    object: normalize_identity(object, Some(&self.roles))?,
                    ownership: self
                        .ownership
                        .get(object)
                        .cloned()
                        .unwrap_or(pbps_model::resolver::ObjectOwnership::Unqualified),
                    bindings,
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
    if object.class == TARGET_ROLE {
        result.class = "pg_authid".into();
    } else if object.class == "pg_authid"
        && let Some(roles) = roles
    {
        let [name] = object.name.as_slice() else {
            return Err(ManifestError::Invalid);
        };
        if let Some(logical) = roles.logical_of(name) {
            result.name = vec![logical];
        } else {
            // The scratch server's own role is a separate observed
            // prerequisite, even if its spelling equals a target role.
            result.class = SCRATCH_ROLE.into();
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
        ("column", "attacl")
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
        if map.get("class").and_then(Value::as_str) == Some("pg_authid")
            || map.get("class").and_then(Value::as_str) == Some(TARGET_ROLE)
        {
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
                        if acl_field(&object.class, name)
                            && let Value::Array(entries) = &mut normalized
                        {
                            entries.sort_by_cached_key(Value::to_string);
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
    if !matches!(read.major, 16 | 18) {
        return Err(Uncovered::class(
            "pg_roles",
            "unsupported role pinning rule",
        ));
    }
    let mut role_pinned = BTreeMap::new();
    for row in read
        .catalog
        .rows
        .get("pg_roles")
        .ok_or_else(|| Uncovered::class("pg_roles", "role inventory is missing"))?
    {
        let role = read
            .catalog
            .identity("pg_roles", row)
            .map_err(|_| Uncovered::class("pg_roles", "role identity is unreadable"))?;
        let oid = logical::number(row, "oid")
            .map_err(|_| Uncovered::object(&role, "role OID is unreadable"))?;
        // PG16/18 IsPinnedObject uses FirstUnpinnedObjectId (12000), with
        // no pg_authid exception. A spelling or absent edge proves neither.
        if oid == 0 || role_pinned.insert(role.clone(), oid < 12_000).is_some() {
            return Err(Uncovered::object(
                &role,
                "role OID is invalid or duplicated",
            ));
        }
    }
    let mut inputs = BTreeMap::new();
    let mut attribute_numbers = BTreeMap::new();
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
        if object.class == "column" {
            if locator.class != "pg_attribute" {
                return Err(Uncovered::object(
                    &object,
                    "column provenance has the wrong catalog kind",
                ));
            }
            let number = logical::signed(row, "attnum")
                .map_err(|_| Uncovered::object(&object, "column number is unreadable"))?;
            if number == 0 || attribute_numbers.insert(object.clone(), number).is_some() {
                return Err(Uncovered::object(
                    &object,
                    "column number is not unique or valid",
                ));
            }
        }
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
        role_pinned,
        attribute_numbers,
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

#[cfg(all(test, unix))]
mod acl_order_tests;
#[cfg(all(test, unix))]
mod guard_tests;
#[cfg(all(test, unix))]
mod view_ownership_tests;
