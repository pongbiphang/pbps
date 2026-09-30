//! Creation ACLs derived from the opening catalog's default-ACL rows.
//!
//! The scratch role has no copy of the target's ALTER DEFAULT PRIVILEGES
//! state. This calculation stays inside the fixed-key producer, before its
//! compiled facts are sealed; it is never an independent target read.

use super::{CandidateClass, CandidateSet, CapturedInputs};
use crate::resolver::authorization::AuthorizationContext;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_model::resolver::{ManifestError, Surface};
use pbps_model::{Change, ChangeSet, GrantTarget, ModuleId, Permission, PublicAccess, RoutineId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AclEntry {
    grantor: ObjectIdentity,
    grantee: ObjectIdentity,
    privilege: String,
    grant_option: bool,
}

type AclKey = (ObjectIdentity, ObjectIdentity, String);

fn role(name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: "pg_authid".into(),
        name: vec![name.into()],
        signature: Vec::new(),
    }
}

fn public() -> ObjectIdentity {
    ObjectIdentity {
        class: "public-principal".into(),
        name: vec!["PUBLIC".into()],
        signature: Vec::new(),
    }
}

/// The exact grantor is a property of the effective principal and the ACL at
/// this typed step, not the subject's owner alone. Without verified target
/// role traversal order, multiple inherited full-option holders are not a
/// choice the producer can make from their logical names (DEC-483).
fn grantor_for(
    authorization: &AuthorizationContext,
    owner: &ObjectIdentity,
    acl: &BTreeMap<AclKey, bool>,
    privileges: &[&str],
    field: &str,
) -> Result<ObjectIdentity, ManifestError> {
    if privileges.is_empty() {
        return Err(ManifestError::Invalid);
    }
    let actor = role(&authorization.principal.effective);
    if authorization.principal.superuser || actor == *owner {
        return Ok(owner.clone());
    }
    let holds_all = |principal: &ObjectIdentity| {
        principal == owner
            || privileges.iter().all(|privilege| {
                acl.iter().any(|((_, grantee, held), option)| {
                    grantee == principal && held.as_str() == *privilege && *option
                })
            })
    };
    // The current role is the first eligible role in select_best_grantor.
    // This also wins over an inherited owner when it holds the full option.
    if holds_all(&actor) {
        // The separate schema-authorization projection still gives inherited
        // ownership precedence. Refuse only this conflicting schema shape;
        // sealing two different postconditions would record a false success.
        if field == "nspacl"
            && authorization
                .roles
                .get(&owner.name[0])
                .is_some_and(|attrs| attrs.inherits)
        {
            return Err(ManifestError::Incomplete);
        }
        return Ok(actor);
    }
    let mut inherited = authorization
        .roles
        .iter()
        .filter(|(name, attrs)| {
            attrs.inherits && name.as_str() != authorization.principal.effective.as_str()
        })
        .map(|(name, _)| role(name))
        .filter(holds_all);
    // Partial option masks can make the engine grant only some requested
    // privileges. That is not the complete typed change we may seal.
    let first = inherited.next().ok_or(ManifestError::Incomplete)?;
    if inherited.next().is_some() {
        // A logical-name sort is not PostgreSQL's role traversal order.
        return Err(ManifestError::Incomplete);
    }
    Ok(first)
}

fn defaults(
    captured: &CapturedInputs,
    owner: &ObjectIdentity,
    namespace: Option<&str>,
    kind: &str,
) -> Result<Option<Vec<AclEntry>>, ManifestError> {
    let scope = ObjectIdentity {
        class: "pg_namespace".into(),
        name: namespace.map_or_else(Vec::new, |name| vec![name.into()]),
        signature: Vec::new(),
    };
    let object = ObjectIdentity {
        class: "pg_default_acl".into(),
        name: vec![kind.into()],
        signature: vec![owner.clone(), scope],
    };
    let Some(input) = captured.inputs.get(&object) else {
        return Ok(None);
    };
    let entries: Vec<AclEntry> = serde_json::from_value(
        input
            .properties
            .get("defaclacl")
            .cloned()
            .ok_or(ManifestError::Incomplete)?,
    )
    .map_err(|_| ManifestError::Invalid)?;
    Ok(Some(entries))
}

fn builtin(owner: &ObjectIdentity, kind: &str, major: u32) -> Result<Vec<AclEntry>, ManifestError> {
    let privileges: &[&str] = match kind {
        "r" => {
            if major == 18 {
                &[
                    "INSERT",
                    "SELECT",
                    "UPDATE",
                    "DELETE",
                    "TRUNCATE",
                    "REFERENCES",
                    "TRIGGER",
                    "MAINTAIN",
                ]
            } else if major == 16 {
                &[
                    "INSERT",
                    "SELECT",
                    "UPDATE",
                    "DELETE",
                    "TRUNCATE",
                    "REFERENCES",
                    "TRIGGER",
                ]
            } else {
                return Err(ManifestError::Invalid);
            }
        }
        "S" => &["SELECT", "UPDATE", "USAGE"],
        "n" => &["CREATE", "USAGE"],
        "f" => &["EXECUTE"],
        "T" => &["USAGE"],
        _ => return Err(ManifestError::Invalid),
    };
    let mut acl = Vec::new();
    for privilege in privileges {
        acl.push(AclEntry {
            grantor: owner.clone(),
            grantee: owner.clone(),
            privilege: (*privilege).into(),
            grant_option: false,
        });
    }
    if matches!(kind, "f" | "T") {
        acl.push(AclEntry {
            grantor: owner.clone(),
            grantee: public(),
            privilege: if kind == "f" { "EXECUTE" } else { "USAGE" }.into(),
            grant_option: false,
        });
    }
    Ok(acl)
}

fn merge(entries: impl IntoIterator<Item = AclEntry>) -> BTreeMap<AclKey, bool> {
    let mut merged = BTreeMap::new();
    for entry in entries {
        let key = (entry.grantor, entry.grantee, entry.privilege);
        *merged.entry(key).or_insert(false) |= entry.grant_option;
    }
    merged
}

/// PostgreSQL uses the effective creator's global default in place of the
/// built-in ACL, then adds the schema default. `aclmerge` folds overlapping
/// grants with grant-option OR. An empty merge or one equal to the built-in
/// default is represented by SQL NULL in the created object's ACL column.
/// Auto row types and their arrays do not call this path; identity sequences
/// do, with catalog default-ACL kind `S` (SQL acldefault selector `s`).
pub(super) fn creation_acl(
    captured: &CapturedInputs,
    owner_name: &str,
    namespace: &str,
    kind: &str,
) -> Result<Value, ManifestError> {
    let owner = role(owner_name);
    let built_in = builtin(&owner, kind, captured.major())?;
    let global = defaults(captured, &owner, None, kind)?.unwrap_or_else(|| built_in.clone());
    let scoped = defaults(captured, &owner, Some(namespace), kind)?.unwrap_or_default();
    let merged = merge(global.into_iter().chain(scoped));
    if merged.is_empty() || merged == merge(built_in) {
        return Ok(Value::Null);
    }
    Ok(explicit(merged))
}

/// Apply only the final ordered ACL statements for a retained, independently
/// qualified subject. Scratch never received the target's object ACL, so its
/// final ACL cannot stand in for either the opening entries or a typed delta.
/// Return None when no statement addresses this subject.
pub(super) fn retained_acl_after_plan(
    opening: &CapturedInputs,
    compiled: &CapturedInputs,
    compiled_object: &ObjectIdentity,
    source: &ObjectIdentity,
    surface: &Surface,
    field: &str,
    changes: &ChangeSet,
    authorization: &AuthorizationContext,
) -> Result<Option<Value>, ManifestError> {
    let row = opening
        .inputs
        .get(source)
        .ok_or(ManifestError::Incomplete)?;
    let original = row.properties.get(field).ok_or(ManifestError::Incomplete)?;
    let owner_field = match field {
        "relacl" => "relowner",
        "proacl" => "proowner",
        "nspacl" => "nspowner",
        _ => return Err(ManifestError::Invalid),
    };
    let owner: ObjectIdentity = serde_json::from_value(
        row.properties
            .get(owner_field)
            .cloned()
            .ok_or(ManifestError::Incomplete)?,
    )
    .map_err(|_| ManifestError::Invalid)?;
    if owner.class != "pg_authid" || owner.name.len() != 1 {
        return Err(ManifestError::Invalid);
    }
    let kind = match field {
        "relacl" => "r",
        "proacl" => "f",
        "nspacl" => "n",
        _ => return Err(ManifestError::Invalid),
    };
    let mut acl = if original.is_null() {
        merge(builtin(&owner, kind, opening.major())?)
    } else {
        let entries: Vec<AclEntry> =
            serde_json::from_value(original.clone()).map_err(|_| ManifestError::Invalid)?;
        merge(entries)
    };
    let mut changed = false;
    for step in &changes.changes {
        match &step.change {
            Change::PublicExecution {
                routine, access, ..
            } if matches!(surface, Surface::Module(ModuleId::Routine(named)) if named == routine) =>
            {
                changed = true;
                let grantor = grantor_for(authorization, &owner, &acl, &["EXECUTE"], field)?;
                let key = (grantor, public(), "EXECUTE".into());
                match access {
                    PublicAccess::Kept => {
                        acl.entry(key).or_insert(false);
                    }
                    PublicAccess::Revoked => {
                        acl.remove(&key);
                    }
                }
            }
            Change::Grant {
                role: grantee,
                target,
                permissions,
            }
            | Change::Revoke {
                role: grantee,
                target,
                permissions,
            } if match (surface, target) {
                (Surface::Namespace(name), GrantTarget::Schema(target)) => name == target,
                (Surface::Table(table), GrantTarget::Object(target)) => {
                    table == target && !permissions.contains(&Permission::Execute)
                }
                (Surface::Module(ModuleId::Named(name)), GrantTarget::Object(target)) => {
                    name == target && !permissions.contains(&Permission::Execute)
                }
                (Surface::Module(ModuleId::Routine(routine)), target) => {
                    addresses_routine(compiled, compiled_object, target, permissions, routine)?
                }
                _ => false,
            } =>
            {
                changed = true;
                let privileges = permissions
                    .iter()
                    .map(privilege)
                    .collect::<Result<Vec<_>, _>>()?;
                // PostgreSQL selects one grantor for a GRANT statement's whole
                // privilege set; the emitter writes each REVOKE separately.
                let grantor = if matches!(&step.change, Change::Grant { .. }) {
                    Some(grantor_for(
                        authorization,
                        &owner,
                        &acl,
                        &privileges,
                        field,
                    )?)
                } else {
                    None
                };
                for privilege in privileges {
                    let selected = if let Some(grantor) = &grantor {
                        grantor.clone()
                    } else {
                        grantor_for(authorization, &owner, &acl, &[privilege], field)?
                    };
                    let key = (selected, role(grantee), privilege.into());
                    if grantor.is_some() {
                        acl.entry(key).or_insert(false);
                    } else {
                        // A revoke touches only the selected grantor's ACL
                        // row. A second grantor's matching entry survives.
                        acl.remove(&key);
                    }
                }
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
            | Change::PublicExecution { .. } => {}
        }
    }
    Ok(changed.then(|| explicit(acl)))
}

fn privilege(permission: &Permission) -> Result<&'static str, ManifestError> {
    match permission {
        Permission::Select => Ok("SELECT"),
        Permission::Insert => Ok("INSERT"),
        Permission::Update => Ok("UPDATE"),
        Permission::Delete => Ok("DELETE"),
        Permission::References => Ok("REFERENCES"),
        Permission::Execute => Ok("EXECUTE"),
        Permission::Usage => Ok("USAGE"),
        Permission::Create => Ok("CREATE"),
        Permission::Truncate => Ok("TRUNCATE"),
        Permission::Trigger => Ok("TRIGGER"),
        Permission::Maintain => Ok("MAINTAIN"),
        Permission::Alter | Permission::ViewDefinition => Err(ManifestError::Invalid),
    }
}

fn explicit(acl: BTreeMap<AclKey, bool>) -> Value {
    let mut entries = acl
        .into_iter()
        .map(|((grantor, grantee, privilege), grant_option)| {
            json!(AclEntry {
                grantor,
                grantee,
                privilege,
                grant_option
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by_cached_key(Value::to_string);
    Value::Array(entries)
}

fn addresses_routine(
    compiled: &CapturedInputs,
    object: &ObjectIdentity,
    target: &GrantTarget,
    permissions: &BTreeSet<Permission>,
    routine: &RoutineId,
) -> Result<bool, ManifestError> {
    match target {
        GrantTarget::Routine(named) => Ok(named == routine),
        GrantTarget::Object(name)
            if name == &routine.name && permissions.contains(&Permission::Execute) =>
        {
            // PostgreSQL accepts bare ON ROUTINE only when this name has one
            // overload. Use the complete qualified compiled candidate set,
            // including unmanaged overloads, rather than guessing from the
            // declaration or allowing its name to choose a signature.
            let set = CandidateSet {
                class: CandidateClass::Routine,
                namespace: Some(name.schema.clone()),
                name: Some(name.name.clone()),
            };
            let members = compiled
                .candidates
                .get(&set)
                .ok_or(ManifestError::Incomplete)?;
            if members.len() != 1 || !members.contains(object) {
                return Err(ManifestError::Incomplete);
            }
            Ok(true)
        }
        GrantTarget::Object(_) | GrantTarget::Schema(_) => Ok(false),
    }
}

/// Finish a newly created routine's ACL under the immutable producer key.
/// PostgreSQL materializes even a NULL creation ACL on the first explicit
/// GRANT/REVOKE, starting from acldefault('f', owner). This is also true
/// when an empty global default had made creation store NULL.
pub(super) fn routine_acl_after_plan(
    captured: &CapturedInputs,
    compiled: &CapturedInputs,
    compiled_routine: &ObjectIdentity,
    owner_name: &str,
    namespace: &str,
    routine: &RoutineId,
    changes: &ChangeSet,
) -> Result<Value, ManifestError> {
    let owner = role(owner_name);
    let created = creation_acl(captured, owner_name, namespace, "f")?;
    let mut creates = changes
        .changes
        .iter()
        .enumerate()
        .filter_map(|(index, step)| {
            matches!(
                &step.change,
                Change::CreateModule { id: ModuleId::Routine(r), .. }
                    | Change::AlterModule { id: ModuleId::Routine(r), .. }
                    if r == routine
            )
            .then_some(index)
        });
    let create = creates.next().ok_or(ManifestError::Incomplete)?;
    if creates.next().is_some() {
        return Err(ManifestError::Invalid);
    }
    let mut acl = if created.is_null() {
        merge(builtin(&owner, "f", captured.major())?)
    } else {
        let entries: Vec<AclEntry> =
            serde_json::from_value(created).map_err(|_| ManifestError::Invalid)?;
        if entries.iter().any(|entry| entry.grantor != owner) {
            return Err(ManifestError::Incomplete);
        }
        merge(entries)
    };
    let mut public_decision = false;
    for step in changes.changes.iter().skip(create + 1) {
        match &step.change {
            Change::PublicExecution {
                routine: named,
                access,
                ..
            } if named == routine => {
                if public_decision {
                    return Err(ManifestError::Invalid);
                }
                public_decision = true;
                let key = (owner.clone(), public(), "EXECUTE".into());
                match access {
                    PublicAccess::Kept => {
                        acl.entry(key).or_insert(false);
                    }
                    PublicAccess::Revoked => {
                        acl.remove(&key);
                    }
                }
            }
            Change::Grant {
                role: grantee,
                target,
                permissions,
            }
            | Change::Revoke {
                role: grantee,
                target,
                permissions,
            } if addresses_routine(compiled, compiled_routine, target, permissions, routine)? => {
                if permissions != &BTreeSet::from([Permission::Execute]) {
                    return Err(ManifestError::Invalid);
                }
                let key = (owner.clone(), role(grantee), "EXECUTE".into());
                if matches!(&step.change, Change::Grant { .. }) {
                    acl.entry(key).or_insert(false);
                } else {
                    acl.remove(&key);
                }
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
            | Change::PublicExecution { .. } => {}
        }
    }
    if !public_decision {
        return Err(ManifestError::Incomplete);
    }
    Ok(explicit(acl))
}
