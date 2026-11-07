//! Engine-read authorization endpoints, in the target's logical domain (DEC-2143.1).

use super::*;

/// Only a bracketed target read and a mapped engine answer construct these
/// endpoints. The seal never projects SQL Server's permission semantics.
#[derive(Debug)]
pub struct Condition {
    before: AuthorizationContext,
    after: AuthorizationContext,
}

impl Condition {
    pub fn before(&self) -> &AuthorizationContext {
        &self.before
    }
    pub fn after(&self) -> &AuthorizationContext {
        &self.after
    }

    pub async fn recheck(&self, target: &mut impl QueryConnection) -> Result<bool, DbError> {
        Ok(target_context(target, &self.before).await? == self.before)
    }
}

/// Planned recipients are part of the question inventory even after a REVOKE
/// removes their last ACL row. Preserve their actual kind before reproduction:
/// a role cloned as a user cannot give a faithful post-plan catalog answer.
pub async fn read_planned_principals(
    conn: &mut impl QueryConnection,
    context: &mut AuthorizationContext,
    planned: &[pbps_model::RoleName],
) -> Result<(), DbError> {
    resolve_spellings(conn, context, planned).await?;
    for name in context
        .spellings
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        let rows = conn.query(&format!(
            "SELECT type,USER_NAME(owning_principal_id) AS owner FROM sys.database_principals WHERE name={};",
            literal(name.raw())
        )).await?;
        let row = match rows.as_slice() {
            [row] => row,
            // The deployer's catalog view hides a principal it holds no
            // permission on, while USER_ID still resolves it (measured by
            // recon726). Its kind is not part of what the deployer can
            // observe either, so it stays unrecorded, as before this read
            // existed. More than one row is a catalog that contradicts itself.
            [] => continue,
            _ => {
                return Err(DbError::Refused(format!(
                    "the kind of planned principal {} is unreadable",
                    name.shown()
                )));
            }
        };
        let kind = if required(row, "type", "a planned principal's kind")? == "R" {
            PrincipalKind::Role
        } else {
            PrincipalKind::User
        };
        if kind == PrincipalKind::Role {
            let owner = required(row, "owner", "a planned role's owner")?;
            if owner != "dbo" {
                context.principal_owners.insert(name.clone(), owner.into());
            }
        }
        context.principals.insert(name, kind);
    }
    Ok(())
}

fn owners(context: &AuthorizationContext) -> OwnerPartition {
    // These logical owners already passed the staged ownership partition.
    // read_staged_permissions uses them only to address its fixed questions;
    // it does not infer an owner from an effective permission or ACL row.
    OwnerPartition::for_authorization(context)
}

async fn target_context(
    conn: &mut impl QueryConnection,
    expected: &AuthorizationContext,
) -> Result<AuthorizationContext, DbError> {
    let mut got = reread(conn, expected).await?;
    read_planned_principals(
        conn,
        &mut got,
        &expected.spellings.keys().cloned().collect::<Vec<_>>(),
    )
    .await?;
    read_staged_permissions(conn, &mut got, &owners(expected)).await?;
    Ok(got)
}

/// Why the staged question inventory could not be built.
#[derive(Debug)]
pub enum InventoryError {
    Read(DbError),
    Capture(crate::resolver::capture::CaptureError),
    /// The partition of the externals' owners was refused (its `Debug` text).
    Owners(String),
}

/// The deployer's question inventory over the target for a set of externals:
/// the captured authorization re-read in its own domain, the planned
/// principals' kinds, and the permissions on the externals the staging leaves
/// to authorization. Planning builds it to reproduce and seal; apply builds
/// it from the sealed manifest to recompute the seal (#2144). One derivation,
/// so a digest planning sealed is one apply can reproduce.
pub async fn staged_inventory(
    target: &mut impl QueryConnection,
    read: &AuthorizationContext,
    planned: &[pbps_model::RoleName],
    external: &crate::resolver::capture::ExternalInputs,
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<AuthorizationContext, InventoryError> {
    let mut context = reread(target, read).await.map_err(InventoryError::Read)?;
    read_planned_principals(target, &mut context, planned)
        .await
        .map_err(InventoryError::Read)?;
    let scope = crate::resolver::capture::CaptureScope {
        retained: external.roots().clone(),
        candidates: BTreeSet::new(),
    };
    let initial = crate::resolver::capture::capture_declared(target, &scope, declarations, key)
        .await
        .map_err(InventoryError::Capture)?;
    let owners = OwnerPartition::for_staging(&initial, &context, &external.excluded())
        .map_err(|e| InventoryError::Owners(format!("{e:?}")))?;
    read_staged_permissions(target, &mut context, &owners)
        .await
        .map_err(InventoryError::Read)?;
    Ok(context)
}

/// The deployer's authorization as the target answers it now, for the
/// questions `read` asks: the engine-read effective permissions on top of the
/// bracketed catalog read (#2144). A plan's sealed `before` and `after` are
/// digests of exactly this shape, so apply asks the same questions of the
/// target inside its transaction and compares digests.
pub async fn answered_context(
    target: &mut impl QueryConnection,
    read: &AuthorizationContext,
) -> Result<AuthorizationContext, DbError> {
    target_context(target, read).await
}

/// The reproduction has already been verified and apply_planned has run.
/// Capture its full after-state, translate every physical principal, and
/// bracket it with target reads so no lasting target change supplies half a seal.
pub async fn capture_condition(
    target: &mut impl QueryConnection,
    scratch: &mut impl QueryConnection,
    map: &PrincipalMap,
    expected: &AuthorizationContext,
) -> Result<Condition, DbError> {
    let before = target_context(target, expected).await?;
    if &before != expected {
        return Err(DbError::Refused(
            "the target authorization changed during scratch reproduction".into(),
        ));
    }
    let after = reproduced_context(scratch, map, expected).await?;
    if reproduced_context(scratch, map, expected).await? != after
        || target_context(target, expected).await? != before
    {
        return Err(DbError::Refused(
            "authorization changed while its before/after condition was read".into(),
        ));
    }
    Ok(Condition { before, after })
}

async fn reproduced_context(
    scratch: &mut impl QueryConnection,
    map: &PrincipalMap,
    expected: &AuthorizationContext,
) -> Result<AuthorizationContext, DbError> {
    let mut got = reread(scratch, expected).await?;
    let planned = expected
        .spellings
        .keys()
        .map(|name| map.run_local(name).ok_or_else(|| unmapped(name)))
        .collect::<Result<Vec<_>, _>>()?;
    read_planned_principals(scratch, &mut got, &planned).await?;
    read_staged_permissions(scratch, &mut got, &owners(expected)).await?;
    logical(map, expected, got)
}

fn unmapped(name: &pbps_model::RoleName) -> DbError {
    DbError::Refused(format!(
        "scratch principal {} cannot be mapped back to a target principal",
        name.shown()
    ))
}

fn logical(
    map: &PrincipalMap,
    target: &AuthorizationContext,
    mut got: AuthorizationContext,
) -> Result<AuthorizationContext, DbError> {
    let name = |physical: &pbps_model::RoleName| {
        map.logical_of(physical)
            .or_else(|| {
                (BUILT_IN.contains(&physical.raw()) || is_fixed_role(physical.raw()))
                    .then(|| physical.clone())
            })
            .ok_or_else(|| unmapped(physical))
    };
    let grant = |g: &mut Grant| -> Result<(), DbError> {
        g.grantee = name(&g.grantee)?;
        g.grantor = name(&g.grantor)?;
        Ok(())
    };
    got.principal.effective = name(&got.principal.effective)?;
    if got.principal.effective != target.principal.effective
        || got.principal.superuser != target.principal.superuser
    {
        return Err(DbError::Refused(
            "the scratch authorization reader is not the reproduced deployer".into(),
        ));
    }
    // ORIGINAL_LOGIN is the provisioning connection, not the EXECUTE AS
    // identity. The checked effective principal maps it to this target login.
    got.principal.login.clone_from(&target.principal.login);
    for g in &mut got.database_grants {
        grant(g)?;
    }
    got.database_grants.sort();
    for schema in got.schemas.values_mut() {
        schema.owner = name(&schema.owner)?;
        for g in &mut schema.grants {
            grant(g)?;
        }
        schema.grants.sort();
    }
    got.roles = got
        .roles
        .into_iter()
        .map(|(n, v)| Ok((name(&n)?, v)))
        .collect::<Result<_, DbError>>()?;
    got.impersonation = got
        .impersonation
        .iter()
        .map(name)
        .collect::<Result<_, _>>()?;
    for g in &mut got.principal_grants {
        g.on = name(&g.on)?;
        grant(&mut g.grant)?;
    }
    got.principal_grants.sort();
    got.principal_owners = got
        .principal_owners
        .into_iter()
        .map(|(n, o)| Ok((name(&n)?, name(&o)?)))
        .collect::<Result<_, DbError>>()?;
    got.principals = got
        .principals
        .into_iter()
        .map(|(n, k)| Ok((name(&n)?, k)))
        .collect::<Result<_, DbError>>()?;
    for (asked, expected) in &target.spellings {
        let physical = map.run_local(asked).ok_or_else(|| unmapped(asked))?;
        let answer = got
            .spellings
            .get(&physical)
            .ok_or_else(|| unmapped(&physical))?;
        let resolved = answer.as_ref().map(name).transpose()?;
        if &resolved != expected {
            return Err(DbError::Refused(format!(
                "the scratch spelling of planned principal {} differs from the target",
                asked.shown()
            )));
        }
    }
    got.spellings.clone_from(&target.spellings);
    if got.staged.keys().ne(target.staged.keys()) {
        return Err(DbError::Refused("a scratch authorization object cannot be mapped to the target's captured object inventory".into()));
    }
    for object in got.staged.values_mut() {
        // owner is already logical: supplied by the verified target partition
        // above. ACL principals remain independent and must map individually.
        for g in &mut object.grants {
            g.grantee = name(&g.grantee)?;
            g.grantor = name(&g.grantor)?;
        }
        object.grants.sort();
    }
    Ok(got)
}
