//! Creation ACLs derived from the opening catalog's default-ACL rows.
//!
//! The scratch role has no copy of the target's ALTER DEFAULT PRIVILEGES
//! state. This calculation stays inside the fixed-key producer, before its
//! compiled facts are sealed; it is never an independent target read.

use super::CapturedInputs;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_model::resolver::ManifestError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
    let mut entries = merged
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
    Ok(Value::Array(entries))
}
