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
// Every retained table needs this, not only one whose columns change: a key,
// constraint or grant runs in place and keeps the target's order, which can
// differ from the declaration order scratch created.
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
    let mut recorded_tables = desired_ids.tables.iter().filter(|(_, name)| *name == table);
    let (table_uid, _) = recorded_tables.next().ok_or(ManifestError::Invalid)?;
    if recorded_tables.next().is_some() {
        return Err(ManifestError::Invalid);
    }
    let Some(prior) = base_ids.tables.get(table_uid) else {
        // Only an explicit CREATE explains the absence of an opening order.
        if changes
            .changes
            .iter()
            .any(|step| matches!(&step.change, Change::CreateTable { uid, .. } if uid == table_uid))
        {
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
        let mut recorded = base_ids
            .columns
            .iter()
            .filter(|(_, name)| *name == &reference);
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
        } else if let Change::AddColumn {
            uid, table, name, ..
        } = &step.change
            && table == &current
        {
            if name.is_empty()
                || !used.insert(uid.clone())
                || order
                    .iter()
                    .any(|(_, column)| column.name == [name.clone()])
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
        } else if let Change::RenameColumn {
            uid,
            table,
            from,
            to,
            ..
        } = &step.change
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
            || desired_ids
                .columns
                .values()
                .filter(|name| *name == &reference)
                .count()
                != 1
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
        let retained = retained
            .into_iter()
            .map(|(object, name)| (object, "conname", name))
            .collect();
        self.relocate(&names, retained)
    }

    /// PostgreSQL names an unnamed primary key, its index, an identity
    /// sequence and a PG18 NOT NULL constraint after the table and column when
    /// it creates them, and a later rename keeps those names (measured on
    /// PG16/18). Scratch creates them under the final names. For a table the
    /// plan keeps, and a child the plan does not recreate, the target's name
    /// is the closing name, whether the rename is in this plan or an earlier
    /// deployment. Call before the transitions are derived.
    pub fn retain_auto_named_children(
        &mut self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        base_ids: &pbps_model::IdsFile,
        desired_ids: &pbps_model::IdsFile,
        desired: &pbps_model::Schema,
    ) -> Result<(), pbps_model::resolver::ManifestError> {
        use pbps_model::Change;
        use pbps_model::resolver::ManifestError;
        if opening.major != self.captured.major {
            return Err(ManifestError::Incomplete);
        }
        let mut names = BTreeMap::new();
        let mut retained = Vec::new();
        let mut map = |from: ObjectIdentity, to: ObjectIdentity, field: &'static str| {
            if from != to {
                let name = to.name.last().cloned().ok_or(ManifestError::Invalid)?;
                retained.push((from.clone(), field, name));
                if names.insert(from, to).is_some() {
                    return Err(ManifestError::Invalid);
                }
            }
            Ok(())
        };
        let reference = |capture: &CapturedInputs, object: &ObjectIdentity, field: &str| {
            capture
                .inputs
                .get(object)
                .and_then(|input| input.properties.get(field))
                .and_then(|value| serde_json::from_value::<ObjectIdentity>(value.clone()).ok())
        };
        // The one primary key, and each column's owned sequence, of a relation.
        let primary = |capture: &CapturedInputs, relation: &ObjectIdentity| {
            let mut keys = capture.inputs.iter().filter(|(id, input)| {
                id.class == "pg_constraint"
                    && id.signature.get(1) == Some(relation)
                    && input.properties.get("contype").and_then(Value::as_str) == Some("p")
            });
            match (keys.next(), keys.next()) {
                (Some((key, _)), None) => Ok(Some(key.clone())),
                (None, _) => Ok(None),
                (Some(_), Some(_)) => Err(ManifestError::Invalid),
            }
        };
        let sequence = |capture: &CapturedInputs, column: &ObjectIdentity| {
            let mut owned = capture
                .inputs
                .keys()
                .filter_map(|id| match id.signature.as_slice() {
                    [made, maker]
                        if id.class == "pg_depend"
                            && id.name == ["i"]
                            && maker == column
                            && capture
                                .inputs
                                .get(made)
                                .and_then(|input| input.properties.get("relkind"))
                                .and_then(Value::as_str)
                                == Some("S") =>
                    {
                        Some(made.clone())
                    }
                    _ => None,
                });
            match (owned.next(), owned.next()) {
                (Some(found), None) => Ok(Some(found)),
                (None, _) => Ok(None),
                (Some(_), Some(_)) => Err(ManifestError::Invalid),
            }
        };
        let column_identity = |column: &pbps_model::ColumnRef| ObjectIdentity {
            class: "column".into(),
            name: vec![column.name.clone()],
            signature: vec![relation_identity(&column.table)],
        };
        for (uid, final_table) in &desired_ids.tables {
            let Some(prior) = base_ids.tables.get(uid) else {
                continue;
            };
            let mut spellings = BTreeSet::from([prior.clone(), final_table.clone()]);
            let mut recreated = false;
            for step in &changes.changes {
                if let Change::CreateTable { uid: changed, .. }
                | Change::DropTable { uid: changed, .. } = &step.change
                    && changed == uid
                {
                    recreated = true;
                }
                if let Change::RenameTable {
                    uid: changed,
                    from,
                    to,
                    ..
                } = &step.change
                    && changed == uid
                {
                    spellings.insert(from.clone());
                    spellings.insert(to.clone());
                }
            }
            let Some(table) = desired.tables.get(final_table) else {
                return Err(ManifestError::Invalid);
            };
            if recreated {
                continue;
            }
            let opening_relation = relation_identity(prior);
            let final_relation = relation_identity(final_table);
            let key_changed = changes.changes.iter().any(|step| {
                matches!(&step.change,
                    Change::SetPrimaryKey { table, .. } if spellings.contains(table))
            });
            if !key_changed
                && table
                    .primary_key
                    .as_ref()
                    .is_some_and(|key| key.name.is_none())
                && let (Some(old_key), Some(new_key)) = (
                    primary(opening, &opening_relation)?,
                    primary(&self.captured, &final_relation)?,
                )
            {
                let mut kept = new_key.clone();
                kept.name.clone_from(&old_key.name);
                map(new_key.clone(), kept, "conname")?;
                if let (Some(old_index), Some(new_index)) = (
                    reference(opening, &old_key, "conindid"),
                    reference(&self.captured, &new_key, "conindid"),
                ) {
                    let mut kept = new_index.clone();
                    *kept.name.last_mut().ok_or(ManifestError::Invalid)? = old_index
                        .name
                        .last()
                        .cloned()
                        .ok_or(ManifestError::Invalid)?;
                    map(new_index, kept, "relname")?;
                }
            }
            for (column_uid, final_column) in &desired_ids.columns {
                if &final_column.table != final_table {
                    continue;
                }
                let Some(old_column) = base_ids.columns.get(column_uid) else {
                    continue;
                };
                let identity_kept = table
                    .columns
                    .get(&final_column.name)
                    .is_some_and(|column| column.identity.is_some());
                // Measured on PG16/18: a type change keeps the identity
                // sequence and the NOT NULL child with their names; only a
                // nullability change recreates the child, under the current
                // spelling, as scratch does.
                let column_recreated = changes.changes.iter().any(|step| {
                    matches!(&step.change,
                        Change::DropColumn { uid: changed, .. }
                        | Change::AddColumn { uid: changed, .. }
                            if changed == column_uid)
                });
                let nullability_changed = changes.changes.iter().any(|step| {
                    matches!(&step.change,
                        Change::AlterColumnNullability { uid: changed, .. }
                            if changed == column_uid)
                        || matches!(&step.change,
                            Change::AlterColumnType { uid: changed, from_nullable, to_nullable, .. }
                                if changed == column_uid && from_nullable != to_nullable)
                });
                if identity_kept
                    && !column_recreated
                    && let (Some(old_sequence), Some(new_sequence)) = (
                        sequence(opening, &column_identity(old_column))?,
                        sequence(&self.captured, &column_identity(final_column))?,
                    )
                {
                    let mut kept = new_sequence.clone();
                    *kept.name.last_mut().ok_or(ManifestError::Invalid)? = old_sequence
                        .name
                        .last()
                        .cloned()
                        .ok_or(ManifestError::Invalid)?;
                    map(new_sequence, kept, "relname")?;
                }
                // A renamed column's NOT NULL child is the rename path's
                // (retain_renamed_not_null); here only a column whose
                // address is unchanged but whose child name is historical.
                if old_column == final_column
                    && !column_recreated
                    && !nullability_changed
                    && let (Some(old_child), Some(new_child)) = (
                        not_null_child(opening, &column_identity(old_column))?,
                        not_null_child(&self.captured, &column_identity(final_column))?,
                    )
                {
                    let mut kept = new_child.clone();
                    kept.name.clone_from(&old_child.name);
                    map(new_child, kept, "conname")?;
                }
            }
        }
        self.relocate(&names, retained)
    }

    /// Re-address scratch records under the names the target keeps, with the
    /// given name properties, everywhere a record or reference can appear.
    fn relocate(
        &mut self,
        names: &BTreeMap<ObjectIdentity, ObjectIdentity>,
        retained: Vec<(ObjectIdentity, &'static str, String)>,
    ) -> Result<(), pbps_model::resolver::ManifestError> {
        use pbps_model::resolver::ManifestError;
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
        for (object, field, name) in retained {
            let input = self
                .captured
                .inputs
                .get_mut(&object)
                .ok_or(ManifestError::Incomplete)?;
            input.properties.insert(field.into(), Value::String(name));
        }
        let mut inputs = BTreeMap::new();
        for (object, mut input) in std::mem::take(&mut self.captured.inputs) {
            for value in input.properties.values_mut() {
                *value = relocated_value(value, names)?;
            }
            for binding in &mut input.bindings {
                binding.target = relocated_identity(&binding.target, names);
            }
            if inputs
                .insert(relocated_identity(&object, names), input)
                .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        self.captured.inputs = inputs;
        let mut ownership = BTreeMap::new();
        for (object, owner) in std::mem::take(&mut self.ownership) {
            if ownership
                .insert(relocated_identity(&object, names), owner)
                .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        self.ownership = ownership;
        let mut numbers = BTreeMap::new();
        for (object, number) in std::mem::take(&mut self.captured.attribute_numbers) {
            if numbers
                .insert(relocated_identity(&object, names), number)
                .is_some()
            {
                return Err(ManifestError::Invalid);
            }
        }
        self.captured.attribute_numbers = numbers;
        for members in self.captured.candidates.values_mut() {
            *members = relocated_set(members, names)?;
        }
        self.captured.scope.retained = relocated_set(&self.captured.scope.retained, names)?;
        self.captured.limitations = relocated_set(&self.captured.limitations, names)?;
        for object in self.captured.dropped.values_mut().flatten() {
            *object = relocated_identity(object, names);
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

    /// Consume the fixed-key producer once the exact final typed sequence is
    /// known. A transitioned record's closing properties are the compiled
    /// scratch record's, except for a retained table: every in-place change
    /// keeps its opening properties, with the approved column order, renames
    /// and key changes applied. Owners and
    /// ACLs are not fingerprinted (rule v2), so nothing here predicts them.
    /// No caller supplies a property mapping.
    pub fn seal_for_plan(
        mut self,
        opening: &CapturedInputs,
        changes: &pbps_model::ChangeSet,
        transitions: &[pbps_model::resolver::ObjectTransition],
        base_ids: &pbps_model::IdsFile,
        desired_ids: &pbps_model::IdsFile,
    ) -> Result<pbps_model::resolver::InputManifest, SealError> {
        use pbps_model::Change;
        use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
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
                    return Err(ManifestError::Invalid.into());
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
                    return Err(ManifestError::Incomplete.into());
                }
                parent_orders.insert(transition.surface.clone(), parent);
            }
        }
        for transition in transitions {
            let Some(parent) = parent_orders.get(&transition.surface) else {
                continue;
            };
            for object in &transition.after {
                let Some(raw) = lookup.get(object) else {
                    return Err(ManifestError::Incomplete.into());
                };
                let owner = self.ownership.get(raw).ok_or(ManifestError::Incomplete)?;
                if owner != &ObjectOwnership::Surface(transition.surface.clone()) {
                    continue;
                }
                let parent_relation = matches!(&transition.surface,
                    Surface::Table(table) if object == &relation_identity(table));
                // A transition may retain its table while replacing its
                // constraint or index. Preserve only records with a proved
                // opening counterpart, never the entire surface inventory.
                if !parent_relation && parent.separate_table_mutation {
                    continue;
                }
                let source = if parent_relation {
                    parent.source.clone()
                } else {
                    object.clone()
                };
                if !transition.before.contains(&source) {
                    return Err(ManifestError::Incomplete.into());
                }
                let before = opening
                    .inputs
                    .get(&source)
                    .ok_or(ManifestError::Incomplete)?;
                let references_changed = before.properties.values().any(|value| {
                    relocated_value(value, &column_names).is_ok_and(|mapped| mapped != *value)
                }) || before.bindings.iter().any(|binding| {
                    relocated_identity(&binding.target, &column_names) != binding.target
                });
                let mut values = Vec::new();
                for (field, original) in &before.properties {
                    // A column operation preserves the existing table; only a
                    // separately approved rename/key operation may replace
                    // these other relation properties.
                    let renamed_relation = parent_relation
                        && source != *object
                        && matches!(field.as_str(), "relname" | "relnamespace" | "reltype");
                    let changed_key =
                        parent_relation && field == "relreplident" && parent.key_change;
                    let value = if renamed_relation
                        || changed_key
                        || field == "engine_definition" && references_changed
                    {
                        self.captured
                            .inputs
                            .get(raw)
                            .and_then(|input| input.properties.get(field))
                            .cloned()
                            .ok_or(ManifestError::Incomplete)?
                    } else if parent_relation && field == "column_order" {
                        serde_json::to_value(&parent.columns).map_err(|_| ManifestError::Invalid)?
                    } else {
                        relocated_value(original, &column_names)?
                    };
                    values.push((field.clone(), target_value(&value)?));
                }
                if parent_relation && !before.properties.contains_key("column_order") {
                    return Err(ManifestError::Incomplete.into());
                }
                let after = self
                    .captured
                    .inputs
                    .get_mut(raw)
                    .ok_or(ManifestError::Incomplete)?;
                for (field, value) in values {
                    after.properties.insert(field, value);
                }
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
        }
        self.retain_renamed_not_null(opening, changes, transitions, base_ids, desired_ids)?;
        Ok(self
            .captured
            .seal_with_roles(&self.key, Some(&self.roles), Some(&self.ownership))?)
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
                    .map(|(name, value)| Ok((name.clone(), normalize_value(value, roles)?)))
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
mod guard_tests;
#[cfg(all(test, unix))]
mod view_ownership_tests;
