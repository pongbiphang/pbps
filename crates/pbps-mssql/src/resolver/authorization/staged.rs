//! Staged-object authority and effective-owner equality (DEC-2267.1).

use super::*;
use crate::resolver::capture::{CapturedInputs, ObjectIdentity, Uncovered, class};

pub(super) fn serialize<S: serde::Serializer>(
    staged: &BTreeMap<ObjectIdentity, StagedObject>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // JSON object keys cannot hold structured identities. Sorted entries
    // retain the complete identity without inventing a textual key codec.
    serde::Serialize::serialize(&staged.iter().collect::<Vec<_>>(), serializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum PermissionState {
    Grant,
    GrantWithGrantOption,
    Deny,
}

impl PermissionState {
    fn read(value: &str) -> Result<Self, DbError> {
        match value {
            "GRANT" => Ok(Self::Grant),
            "GRANT_WITH_GRANT_OPTION" => Ok(Self::GrantWithGrantOption),
            "DENY" => Ok(Self::Deny),
            _ => Err(DbError::BadRow(format!(
                "unsupported object permission state {value}"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Grant => "GRANT",
            Self::GrantWithGrantOption => "GRANT_WITH_GRANT_OPTION",
            Self::Deny => "DENY",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum ColumnPermissionState {
    Permission(PermissionState),
    /// A column exception to an object permission; never a DENY.
    Revoke,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum PermissionScope {
    Object(PermissionState),
    Column {
        name: String,
        state: ColumnPermissionState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct StagedGrant {
    pub grantee: pbps_model::RoleName,
    pub grantor: pbps_model::RoleName,
    pub permission: String,
    pub scope: PermissionScope,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StagedObject {
    pub owner: pbps_model::RoleName,
    /// The engine's effective answers, including each column's permissions.
    pub permissions: BTreeSet<(String, String)>,
    pub grants: Vec<StagedGrant>,
}

/// The anchors keep owner equality with the deployer and database owner, as
/// well as between every schema and staged object. Principal names are only
/// replay inputs; comparison uses the equivalence classes of these subjects.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Subject {
    Securable(ObjectIdentity),
    Deployer,
    DatabaseOwner,
}

#[derive(Debug, Clone)]
pub struct OwnerPartition {
    owners: BTreeMap<Subject, pbps_model::RoleName>,
}

impl OwnerPartition {
    /// Trusted renderer ownership only. File-supplied subjects are verified,
    /// never passed through an ownership repair.
    pub fn restricted_to(&self, objects: &BTreeSet<ObjectIdentity>) -> Self {
        Self {
            owners: self
                .owners
                .iter()
                .filter(|(subject, _)| match subject {
                    Subject::Securable(id) => {
                        matches!(id.class.as_str(), class::OBJECT | class::TYPE)
                            && objects.contains(id)
                    }
                    Subject::Deployer | Subject::DatabaseOwner => true,
                })
                .map(|(s, o)| (s.clone(), o.clone()))
                .collect(),
        }
    }
    pub fn from_capture(
        captured: &CapturedInputs,
        context: &AuthorizationContext,
    ) -> Result<Self, Uncovered> {
        let effective = captured.effective_owners()?;
        let mut held_schemas: BTreeSet<String> = context.schemas.keys().cloned().collect();
        for id in captured
            .objects()
            .filter(|id| id.class == class::OBJECT || id.class == class::TYPE)
        {
            if let Some(schema) = id.name.first() {
                held_schemas.insert(schema.clone());
            }
        }
        let mut owners: BTreeMap<_, _> = effective
            .into_iter()
            .filter(|(id, _)| {
                id.class == class::OBJECT
                    || id.name.first().is_some_and(|name| {
                        held_schemas.contains(name.as_str())
                            || SYSTEM_SCHEMAS.contains(&name.as_str())
                            || name == "dbo"
                            || is_fixed_role(name)
                    })
            })
            .map(|(id, owner)| (Subject::Securable(id), owner))
            .collect();
        owners.insert(Subject::Deployer, context.principal.effective.clone());
        owners.insert(Subject::DatabaseOwner, "dbo".into());
        Ok(Self { owners })
    }

    fn groups(&self) -> BTreeSet<BTreeSet<&Subject>> {
        let mut groups: BTreeMap<&pbps_model::RoleName, BTreeSet<&Subject>> = BTreeMap::new();
        for (subject, owner) in &self.owners {
            groups.entry(owner).or_default().insert(subject);
        }
        groups.into_values().collect()
    }

    /// An explicit file owner which changes the partition refuses here;
    /// comparison never repairs it or compares production principal names.
    pub fn verify(&self, current: &Self) -> Result<(), DbError> {
        if self.groups() != current.groups() {
            return Err(DbError::Refused("staged effective-owner equality differs from the target; file-supplied ownership is not repaired".into()));
        }
        Ok(())
    }
}

fn object_name(id: &ObjectIdentity) -> Result<String, DbError> {
    match (
        id.class.as_str(),
        id.name.as_slice(),
        id.signature.as_slice(),
    ) {
        (class::OBJECT, [schema, name], []) => Ok(format!("{}.{}", bracket(schema), bracket(name))),
        _ => Err(DbError::BadRow(
            "staged authorization requires a schema-qualified object identity".into(),
        )),
    }
}

/// Trusted setup only, before any ACL replay. A file's resulting owners use
/// `OwnerPartition::verify` instead. ALTER AUTHORIZATION clears object ACLs.
pub async fn reconstruct_owners(
    conn: &mut impl QueryConnection,
    map: &PrincipalMap,
    target: &OwnerPartition,
) -> Result<(), DbError> {
    // Schemas first, then types and objects. Children that share their parent's owner
    // inherit it, just as Index::owner resolves them on the target.
    for wanted_class in [class::SCHEMA, class::TYPE, class::OBJECT] {
        for (subject, owner) in &target.owners {
            let Subject::Securable(id) = subject else {
                continue;
            };
            if id.class != wanted_class {
                continue;
            }
            let on = if wanted_class == class::SCHEMA {
                let [name] = id.name.as_slice() else {
                    return Err(DbError::BadRow("invalid schema owner identity".into()));
                };
                if SYSTEM_SCHEMAS.contains(&name.as_str()) {
                    continue;
                }
                let local = map
                    .run_local(owner)
                    .ok_or_else(|| DbError::BadRow("unmapped schema owner".into()))?;
                let rows = conn
                    .query(&format!(
                        "SELECT USER_NAME(principal_id) AS owner FROM sys.schemas WHERE name={};",
                        literal(name)
                    ))
                    .await?;
                if rows.len() == 1 && required(&rows[0], "owner", "a schema owner")? == local.raw()
                {
                    // Database reconstruction set this before its schema
                    // ACL. Reassigning it now would clear that ACL too.
                    continue;
                }
                format!("SCHEMA::{}", bracket(name))
            } else if wanted_class == class::TYPE {
                let [schema, name] = id.name.as_slice() else {
                    return Err(DbError::BadRow("invalid type owner identity".into()));
                };
                if !id.signature.is_empty() {
                    return Err(DbError::BadRow("invalid type owner signature".into()));
                }
                format!("TYPE::{}.{}", bracket(schema), bracket(name))
            } else {
                let name = object_name(id)?;
                let rows = conn.query(&format!(
                    "SELECT OBJECT_SCHEMA_NAME(parent_object_id) AS parent_schema, \
                     OBJECT_NAME(parent_object_id) AS parent_name FROM sys.objects WHERE object_id=OBJECT_ID({});", literal(&name)
                )).await?;
                let [row] = rows.as_slice() else {
                    return Err(DbError::Refused(format!(
                        "staged object {name} is absent or unreadable"
                    )));
                };
                if let (Some(schema), Some(parent)) = (
                    row.try_get::<&str>("parent_schema")?,
                    row.try_get::<&str>("parent_name")?,
                ) {
                    let parent = crate::resolver::capture::object(schema, parent);
                    if target.owners.get(&Subject::Securable(parent)) == Some(owner) {
                        continue;
                    }
                }
                format!("OBJECT::{name}")
            };
            let local = map.run_local(owner).ok_or_else(|| {
                DbError::BadRow(format!("unmapped staged owner {}", owner.shown()))
            })?;
            conn.query(&format!(
                "ALTER AUTHORIZATION ON {on} TO {};",
                bracket(local.raw())
            ))
            .await?;
        }
    }
    Ok(())
}

/// Captures visible ACL rows and effective answers as the target deployer.
/// Role/public rows are included; object and column identities are names,
/// never database-local numeric IDs. This extends the principal map's input
/// before it is generated, so an ACL-only grantor is also reproduced.
pub async fn read_staged_permissions(
    conn: &mut impl QueryConnection,
    context: &mut AuthorizationContext,
    owners: &OwnerPartition,
) -> Result<(), DbError> {
    let missing: Vec<String> = owners
        .owners
        .keys()
        .filter_map(|subject| match subject {
            Subject::Securable(id) if id.class == class::SCHEMA => id
                .name
                .first()
                .filter(|name| !context.schemas.contains_key(*name))
                .cloned(),
            Subject::Securable(_) | Subject::Deployer | Subject::DatabaseOwner => None,
        })
        .collect();
    if !missing.is_empty() {
        let extra = read(conn, &missing).await?;
        context.schemas.extend(extra.schemas);
        context.principals.extend(extra.principals);
        context.principal_owners.extend(extra.principal_owners);
    }
    // An alias type's owner need not own a schema or appear in an object ACL.
    // Keep it in the same run-local principal map used for object owners.
    for (subject, owner) in &owners.owners {
        if matches!(subject, Subject::Securable(id) if id.class == class::TYPE) {
            context
                .principals
                .entry(owner.clone())
                .or_insert(PrincipalKind::User);
        }
    }
    let mut staged = BTreeMap::new();
    for (subject, owner) in &owners.owners {
        let Subject::Securable(id) = subject else {
            continue;
        };
        if id.class != class::OBJECT {
            continue;
        }
        let name = object_name(id)?;
        let rows = conn
            .query(&format!("SELECT OBJECT_ID({}) AS id;", literal(&name)))
            .await?;
        if rows.len() != 1 || rows[0].try_get::<i32>("id")?.is_none() {
            return Err(DbError::Refused(format!(
                "staged object {name} is absent or unreadable"
            )));
        }
        let mut effective = BTreeSet::new();
        for row in conn
            .query(&format!(
                "SELECT subentity_name AS column_name, permission_name AS permission \
            FROM fn_my_permissions({},N'OBJECT');",
                literal(&name)
            ))
            .await?
        {
            effective.insert((
                required(&row, "column_name", "a permission's column")?,
                required(&row, "permission", "an effective permission")?,
            ));
        }
        let mut held = Vec::new();
        for row in conn.query(&format!(
            "SELECT USER_NAME(p.grantee_principal_id) AS grantee,USER_NAME(p.grantor_principal_id) AS grantor,\
             p.permission_name AS permission,p.state_desc AS state,p.minor_id,\
             COL_NAME(p.major_id,p.minor_id) AS column_name FROM sys.database_permissions p \
             WHERE p.class=1 AND p.major_id=OBJECT_ID({});", literal(&name)
        )).await? {
            let state = required(&row, "state", "a staged permission state")?;
            let minor = row.try_get::<i32>("minor_id")?.ok_or_else(|| DbError::BadRow("unreadable permission column ID".into()))?;
            let scope = if minor == 0 {
                PermissionScope::Object(PermissionState::read(&state)?)
            } else {
                PermissionScope::Column {
                    name: required(&row, "column_name", "a staged permission column")?,
                    state: if state == "REVOKE" { ColumnPermissionState::Revoke }
                        else { ColumnPermissionState::Permission(PermissionState::read(&state)?) },
                }
            };
            held.push(StagedGrant {
                grantee: required(&row,"grantee","a staged grantee")?.into(),
                grantor: required(&row,"grantor","a staged grantor")?.into(),
                permission: required(&row,"permission","a staged permission")?,
                scope,
            });
        }
        held.sort();
        staged.insert(
            id.clone(),
            StagedObject {
                owner: owner.clone(),
                permissions: effective,
                grants: held,
            },
        );
    }
    *context.staged = staged;
    let named = named_principals(context, &[]);
    for row in conn.query("SELECT name,type,USER_NAME(owning_principal_id) AS owner FROM sys.database_principals;").await? {
        let name = pbps_model::RoleName::from(required(&row,"name","a staged principal")?);
        if named.contains(&name) {
            let kind = if required(&row,"type","a principal kind")? == "R" { PrincipalKind::Role } else { PrincipalKind::User };
            if kind == PrincipalKind::Role {
                let owner = required(&row,"owner","a role owner")?;
                if owner != "dbo" { context.principal_owners.insert(name.clone(), owner.into()); }
            }
            context.principals.insert(name, kind);
        }
    }
    Ok(())
}

/// Replays object rows first, then column rows and REVOKE exceptions, only
/// after ownership has been accepted. It cannot repair contradictory owners.
pub async fn replay_staged_permissions(
    conn: &mut impl QueryConnection,
    map: &PrincipalMap,
    context: &AuthorizationContext,
    target_owners: &OwnerPartition,
    current_owners: &OwnerPartition,
) -> Result<(), DbError> {
    target_owners.verify(current_owners)?;
    for (id, object) in context.staged.iter() {
        let name = object_name(id)?;
        let mut groups: BTreeMap<Option<&str>, Vec<Grant>> = BTreeMap::new();
        let mut revoked = Vec::new();
        for held in &object.grants {
            let (column, state) = match &held.scope {
                PermissionScope::Object(state) => (None, *state),
                PermissionScope::Column {
                    name,
                    state: ColumnPermissionState::Permission(state),
                } => (Some(name.as_str()), *state),
                PermissionScope::Column {
                    name,
                    state: ColumnPermissionState::Revoke,
                } => {
                    revoked.push((name, held));
                    continue;
                }
            };
            groups.entry(column).or_default().push(Grant {
                grantee: held.grantee.clone(),
                grantor: held.grantor.clone(),
                permission: held.permission.clone(),
                state: state.name().into(),
            });
        }
        let mut object_able = BTreeSet::new();
        for (column, grants) in &groups {
            // Object grant options also cover its columns. Each column starts
            // with those capabilities, never with a sibling column's options.
            let mut column_able = object_able.clone();
            let able = if column.is_none() {
                &mut object_able
            } else {
                &mut column_able
            };
            let column = column
                .map(|name| format!(" ({})", bracket(name)))
                .unwrap_or_default();
            replay(
                conn,
                map,
                &object.owner,
                grants,
                &format!(" ON OBJECT::{name}{column}"),
                able,
            )
            .await?;
        }
        for (column, held) in revoked {
            let grantee = map
                .run_local(&held.grantee)
                .ok_or_else(|| DbError::BadRow("unmapped column revoke grantee".into()))?;
            let grantor = map
                .run_local(&held.grantor)
                .ok_or_else(|| DbError::BadRow("unmapped column revoke grantor".into()))?;
            conn.query(&format!(
                "REVOKE {} ON OBJECT::{name} ({}) FROM {} AS {};",
                held.permission,
                bracket(column),
                bracket(grantee.raw()),
                bracket(grantor.raw())
            ))
            .await?;
        }
    }
    Ok(())
}

/// Re-reads the complete database and staged-object context as the entered
/// deployer, including what a file might have added to its authority.
pub async fn verify_staged_authorization(
    conn: &mut impl QueryConnection,
    map: &PrincipalMap,
    target: &AuthorizationContext,
    schemas: &[String],
    target_owners: &OwnerPartition,
    current_owners: &OwnerPartition,
) -> Result<Vec<String>, DbError> {
    target_owners.verify(current_owners)?;
    let mut got = read(conn, schemas).await?;
    read_staged_permissions(conn, &mut got, current_owners).await?;
    // The partition already compared schema and object ownership. Its owner
    // aliases cannot normalize ACL principals: an owner may also hold a grant
    // under a different logical identity in the unchanged principal map.
    let mut found = differences_except_schema_owners(map, target, &got);
    let logical =
        |name: &pbps_model::RoleName| map.logical_of(name).unwrap_or_else(|| name.clone());
    for object in got.staged.values_mut() {
        for grant in &mut object.grants {
            grant.grantee = logical(&grant.grantee);
            grant.grantor = logical(&grant.grantor);
        }
        object.grants.sort();
    }
    if !target
        .staged
        .iter()
        .map(|(id, object)| (id, &object.permissions, &object.grants))
        .eq(got
            .staged
            .iter()
            .map(|(id, object)| (id, &object.permissions, &object.grants)))
    {
        found.push("staged:permissions".into());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_rendered_ownership_never_repairs_file_schemas() {
        let object = crate::resolver::capture::object("dbo", "rendered");
        let schema = ObjectIdentity {
            class: class::SCHEMA.into(),
            name: vec!["dbo".into()],
            signature: vec![],
        };
        let partition = OwnerPartition {
            owners: BTreeMap::from([
                (Subject::Securable(object.clone()), "dbo".into()),
                (Subject::Securable(schema.clone()), "dbo".into()),
                (Subject::Deployer, "dbo".into()),
            ]),
        };
        let narrowed = partition.restricted_to(&BTreeSet::from([schema.clone(), object.clone()]));
        assert!(narrowed.owners.contains_key(&Subject::Securable(object)));
        assert!(!narrowed.owners.contains_key(&Subject::Securable(schema)));
        assert!(narrowed.owners.contains_key(&Subject::Deployer));
    }

    #[test]
    fn owner_groups_compare_relationships_and_keep_distinct_owners_apart() {
        let schema = |name: &str| {
            Subject::Securable(ObjectIdentity {
                class: class::SCHEMA.into(),
                name: vec![name.into()],
                signature: vec![],
            })
        };
        let partition = |left: &str, right: &str| OwnerPartition {
            owners: BTreeMap::from([(schema("a"), left.into()), (schema("b"), right.into())]),
        };
        assert!(
            partition("one", "one")
                .verify(&partition("local", "local"))
                .is_ok()
        );
        assert!(partition("one", "two").verify(&partition("x", "y")).is_ok());
        assert!(
            partition("one", "two")
                .verify(&partition("x", "x"))
                .is_err()
        );
        assert!(
            partition("one", "one")
                .verify(&partition("x", "y"))
                .is_err()
        );
    }

    #[test]
    fn revoke_is_a_column_exception_and_never_an_object_permission() {
        assert!(PermissionState::read("REVOKE").is_err());
        assert!(PermissionState::read("unknown").is_err());
        assert_eq!(
            PermissionState::read("DENY").unwrap(),
            PermissionState::Deny
        );
    }
}
