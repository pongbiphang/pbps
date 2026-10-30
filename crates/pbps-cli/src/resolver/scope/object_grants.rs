//! Selected grant answers from the ordinary canonical catalog read. ACLs
//! remain outside binding inputs; no permission decoder or ACL simulator is
//! duplicated here (DEC-1454.1).

use pbps_db::catalog::Pulled;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_dialect::Dialect;
use pbps_model::plan_names::{At, Holder, PlanNames};
use pbps_model::resolver::{
    AuthorizationGrant, GrantPhase, GrantPrincipal, GrantTargetState, GrantWrite,
};
use pbps_model::{Change, ChangeSet, GrantTarget, IdsFile, ModuleId, Permission, RoutineId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum End {
    Before,
    After,
}

impl End {
    fn at(self) -> At {
        match self {
            Self::Before => At::Start,
            Self::After => At::End,
        }
    }
    fn phase(self, grant: &AuthorizationGrant) -> &GrantPhase {
        match self {
            Self::Before => &grant.before,
            Self::After => &grant.after,
        }
    }
}

type Effective = BTreeMap<pbps_model::RoleName, BTreeMap<GrantTarget, BTreeSet<Permission>>>;

#[derive(Default)]
struct ModuleCuts {
    created: BTreeMap<ModuleId, usize>,
    dropped: BTreeMap<ModuleId, usize>,
    type_moves: bool,
}

impl ModuleCuts {
    fn of(changes: &ChangeSet) -> Self {
        let mut cuts = Self::default();
        for (i, p) in changes.changes.iter().enumerate() {
            if let Change::CreateModule { id, .. } = &p.change {
                cuts.created.entry(id.clone()).or_insert(i);
            }
            if let Change::DropModule { id, .. } = &p.change {
                cuts.dropped.insert(id.clone(), i);
            }
            cuts.type_moves |= matches!(p.change, Change::RenameType { .. });
        }
        cuts
    }
}

#[derive(serde::Serialize)]
struct Answer {
    target_present: bool,
    principal_present: bool,
    permissions: BTreeSet<Permission>,
}

pub(crate) async fn capture(conn: &mut pbps_db::Conn) -> Result<Pulled, pbps_db::DbError> {
    if pbps_pg::catalog::in_transaction(conn).await? {
        pbps_pg::catalog::introspect_within_transaction(conn).await
    } else {
        pbps_pg::catalog::introspect(conn).await
    }
}

fn fingerprint(
    key: &EnvironmentFingerprintKey,
    index: usize,
    write: &GrantWrite,
    phase: &GrantPhase,
    answer: &Answer,
    end: End,
) -> String {
    let bytes = serde_json::to_vec(&(end, index, write, &phase.target, &phase.grantee, answer))
        .expect("typed grant observation serialises");
    crate::resolver::sealing::hex(key.fingerprint(
        "pbps/object-grant-condition/v1",
        "grant",
        &bytes,
    ))
}

/// All permissions in a question are explicitly written by the plan. The
/// final writer supplies each bit; untouched ACL rows are never predicted.
pub(crate) fn seal(
    key: &EnvironmentFingerprintKey,
    changes: &ChangeSet,
    ids: &IdsFile,
    observed: Option<&Pulled>,
) -> Result<Vec<AuthorizationGrant>, String> {
    let writes = AuthorizationGrant::writes(changes);
    if writes.is_empty() {
        return Ok(Vec::new());
    }
    let observed = observed.ok_or("object grant evidence has no qualified target read")?;
    let names = PlanNames::of(&changes.changes, ids).map_err(|e| e.to_string())?;
    let cuts = ModuleCuts::of(changes);
    let mut grants = Vec::new();
    for (&change, write) in &writes {
        let target = canonical_target(observed, changes, change, write)?;
        let phase = |end| -> Result<GrantPhase, String> {
            Ok(GrantPhase {
                target: target_at(changes, &names, &cuts, change, &target, end)?,
                grantee: principal_at(&names, change, &write.grantee, end),
                fingerprint: String::new(),
            })
        };
        grants.push(AuthorizationGrant {
            change,
            before: phase(End::Before)?,
            after: phase(End::After)?,
        });
    }
    let mut effective = Effective::new();
    for grant in &mut grants {
        let write = &writes[&grant.change];
        let before = answer(observed, &grant.before, &write.permissions, &mut effective)?;
        grant.before.fingerprint = fingerprint(
            key,
            grant.change,
            write,
            &grant.before,
            &before,
            End::Before,
        );
    }
    // Calculate after answers before mutating their fingerprints. The same
    // endpoint may be written several times, including grant then revoke.
    let mut program = BTreeMap::<_, BTreeSet<_>>::new();
    for grant in &grants {
        let permissions = program
            .entry((grant.after.target.clone(), grant.after.grantee.clone()))
            .or_default();
        let write = &writes[&grant.change];
        for permission in &write.permissions {
            if write.grant {
                permissions.insert(*permission);
            } else {
                permissions.remove(permission);
            }
        }
    }
    let after: Vec<_> = grants
        .iter()
        .map(|grant| {
            let write = &writes[&grant.change];
            let mut permissions = program
                [&(grant.after.target.clone(), grant.after.grantee.clone())]
                .intersection(&write.permissions)
                .copied()
                .collect::<BTreeSet<_>>();
            let target_present = matches!(grant.after.target, GrantTargetState::Present { .. });
            let principal_present = !matches!(grant.after.grantee, GrantPrincipal::Absent);
            if !target_present || !principal_present {
                permissions.clear();
            }
            fingerprint(
                key,
                grant.change,
                write,
                &grant.after,
                &Answer {
                    target_present,
                    principal_present,
                    permissions,
                },
                End::After,
            )
        })
        .collect();
    for (grant, digest) in grants.iter_mut().zip(after) {
        grant.after.fingerprint = digest;
    }
    Ok(grants)
}

pub(crate) fn recheck(
    key: &EnvironmentFingerprintKey,
    changes: &ChangeSet,
    grants: &[AuthorizationGrant],
    observed: &Pulled,
    end: End,
) -> Result<(Vec<String>, Vec<String>), String> {
    let writes = AuthorizationGrant::writes(changes);
    let mut fingerprints = Vec::new();
    let mut changed = Vec::new();
    let mut effective = Effective::new();
    for grant in grants {
        let write = writes
            .get(&grant.change)
            .ok_or("object grant question is outside the approved plan")?;
        let phase = end.phase(grant);
        let digest = fingerprint(
            key,
            grant.change,
            write,
            phase,
            &answer(observed, phase, &write.permissions, &mut effective)?,
            end,
        );
        if digest != phase.fingerprint {
            changed.push(format!(
                "the object grant postcondition for {}",
                write.target.shown()
            ));
        }
        fingerprints.push(digest);
    }
    Ok((fingerprints, changed))
}

fn answer(
    read: &Pulled,
    phase: &GrantPhase,
    mask: &BTreeSet<Permission>,
    cache: &mut Effective,
) -> Result<Answer, String> {
    let target = match &phase.target {
        GrantTargetState::Absent => None,
        GrantTargetState::Present { target } => Some(target),
    };
    let target_present = target.is_some_and(|target| match target {
        GrantTarget::Sequence(column) => read
            .schema
            .tables
            .get(&column.table)
            .and_then(|table| table.columns.get(&column.name))
            .is_some_and(|column| {
                matches!(column.value_source, pbps_model::ValueSource::Sequence(_))
            }),
        GrantTarget::Object(_) | GrantTarget::Routine(_) | GrantTarget::Schema(_) => {
            read.owners.contains_key(target)
        }
    });
    let principal_present = match &phase.grantee {
        GrantPrincipal::Absent => false,
        GrantPrincipal::Public => true,
        GrantPrincipal::Role { name } => read.schema.roles.contains_key(name),
    };
    let mut permissions = BTreeSet::new();
    if let Some(target) = target
        && target_present
        && principal_present
    {
        match &phase.grantee {
            GrantPrincipal::Role { name } => {
                if read
                    .unexpressible
                    .iter()
                    .any(|g| &g.role == name && g.target.as_ref().is_none_or(|t| t == target))
                {
                    return Err(format!(
                        "cannot read the declared grant of {} on {}",
                        name.shown(),
                        target.shown()
                    ));
                }
                let role = &read.schema.roles[name];
                // The ordinary differ uses this same expansion for compact
                // serial markers and individual owned-sequence grants.
                let effective = cache.entry(name.clone()).or_insert_with(|| {
                    pbps_model::role::effective_grants(role, &read.schema.tables)
                });
                if let Some(rights) = effective.get(target) {
                    permissions.extend(rights.intersection(mask));
                }
                if mask.contains(&Permission::SerialSequences)
                    && role
                        .grants
                        .get(target)
                        .is_some_and(|p| p.contains(&Permission::SerialSequences))
                {
                    permissions.insert(Permission::SerialSequences);
                }
            }
            GrantPrincipal::Public => {
                if let GrantTarget::Routine(routine) = target
                    && read
                        .public_execute
                        .contains(&ModuleId::Routine(routine.clone()))
                {
                    permissions.insert(Permission::Execute);
                }
            }
            GrantPrincipal::Absent => {}
        }
    }
    Ok(Answer {
        target_present,
        principal_present,
        permissions,
    })
}

fn principal_at(
    names: &PlanNames<'_>,
    index: usize,
    principal: &GrantPrincipal,
    end: End,
) -> GrantPrincipal {
    if let GrantPrincipal::Role { name } = principal
        && let Holder::Tracked(uid) = names.role_holder(name, At::Before(index))
    {
        return names
            .role_at(uid, end.at())
            .cloned()
            .map_or(GrantPrincipal::Absent, |name| GrantPrincipal::Role { name });
    }
    principal.clone()
}

fn target_at(
    changes: &ChangeSet,
    names: &PlanNames<'_>,
    cuts: &ModuleCuts,
    index: usize,
    target: &GrantTarget,
    end: End,
) -> Result<GrantTargetState, String> {
    let present = |target| GrantTargetState::Present { target };
    match target {
        GrantTarget::Object(table) => match names.holder(table, At::Before(index)) {
            Holder::Tracked(uid) => Ok(names
                .table_at(uid, end.at())
                .cloned()
                .map_or(GrantTargetState::Absent, |name| {
                    present(GrantTarget::Object(name))
                })),
            Holder::Unmanaged => {
                module_at(changes, cuts, index, ModuleId::Named(table.clone()), end)
            }
            Holder::Vacant => Err(format!(
                "the grant target {} has no managed holder at its change",
                table.shown()
            )),
        },
        GrantTarget::Sequence(column) => match names.column_holder(column, At::Before(index)) {
            Holder::Tracked(uid) => Ok(names
                .column_at(uid, end.at())
                .and_then(|(table, column)| {
                    names
                        .table_at(table, end.at())
                        .map(|table| present(GrantTarget::Sequence(table.column(column))))
                })
                .unwrap_or(GrantTargetState::Absent)),
            Holder::Unmanaged => Ok(present(target.clone())),
            Holder::Vacant => Err(format!(
                "the grant target {} has no managed holder at its change",
                column.shown()
            )),
        },
        GrantTarget::Routine(routine) => module_at(
            changes,
            cuts,
            index,
            ModuleId::Routine(routine.clone()),
            end,
        ),
        GrantTarget::Schema(_) => Err("a schema grant is not an object grant question".into()),
    }
}

fn module_at(
    changes: &ChangeSet,
    cuts: &ModuleCuts,
    index: usize,
    mut id: ModuleId,
    end: End,
) -> Result<GrantTargetState, String> {
    // The usual plan needs two indexed boundary checks, not a scan per
    // routine. Only an actual type move needs the ordered spelling walk.
    if !cuts.type_moves || matches!(id, ModuleId::Named(_)) {
        let absent = match end {
            End::Before => cuts.created.get(&id).is_some_and(|&at| at < index),
            End::After => cuts.dropped.get(&id).is_some_and(|&at| at > index),
        };
        if absent {
            return Ok(GrantTargetState::Absent);
        }
        let target = match id {
            ModuleId::Named(name) => GrantTarget::Object(name),
            ModuleId::Routine(routine) => GrantTarget::Routine(routine),
            ModuleId::Trigger { .. } => return Err("a trigger has no object ACL".into()),
        };
        return Ok(GrantTargetState::Present { target });
    }
    let steps: Box<dyn Iterator<Item = &pbps_model::PlannedChange> + '_> = match end {
        End::Before => Box::new(changes.changes[..index].iter().rev()),
        End::After => Box::new(changes.changes[index + 1..].iter()),
    };
    for p in steps {
        if matches!((end, &p.change), (End::Before, Change::CreateModule { id: added, .. }) if added == &id)
            || matches!((end, &p.change), (End::After, Change::DropModule { id: dropped, .. }) if dropped == &id)
        {
            return Ok(GrantTargetState::Absent);
        }
        if let Change::RenameType { from, to, .. } = &p.change
            && let ModuleId::Routine(routine) = &mut id
        {
            let (from, to) = match end {
                End::Before => (to, from),
                End::After => (from, to),
            };
            move_type(routine, from, to)?;
        }
    }
    let target = match id {
        ModuleId::Named(name) => GrantTarget::Object(name),
        ModuleId::Routine(routine) => GrantTarget::Routine(routine),
        ModuleId::Trigger { .. } => return Err("a trigger has no object ACL".into()),
    };
    Ok(GrantTargetState::Present { target })
}

fn move_type(
    routine: &mut RoutineId,
    from: &pbps_model::TypeRef,
    to: &pbps_model::TypeRef,
) -> Result<(), String> {
    let renames =
        pbps_pg::type_refs::renames::TypeRenames(BTreeMap::from([(from.clone(), to.clone())]));
    let pg = pbps_pg::Postgres::new();
    for arg in &mut routine.args {
        let Ok(mut ty) = arg.raw().parse::<pbps_model::ColumnType>() else {
            continue;
        };
        if ty.type_ref() != Some(from) {
            continue;
        }
        renames.column_type(&mut ty);
        let reference = ty.type_ref().expect("reference retained");
        let qualified = [
            pg.quote_ident(reference.schema())
                .map_err(|e| e.to_string())?,
            pg.quote_ident(reference.name())
                .map_err(|e| e.to_string())?,
        ]
        .join(".");
        let text = format!("{qualified}{}", if ty.is_array() { "[]" } else { "" });
        *arg = pg
            .normalize_routine_arg(
                &text
                    .parse()
                    .map_err(|_| "renamed routine argument is not a type")?,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn canonical_target(
    read: &Pulled,
    changes: &ChangeSet,
    index: usize,
    write: &GrantWrite,
) -> Result<GrantTarget, String> {
    let GrantTarget::Object(name) = &write.target else {
        return Ok(write.target.clone());
    };
    if !write.permissions.contains(&Permission::Execute) {
        return Ok(write.target.clone());
    }
    // PostgreSQL permits a bare routine only when it has one overload. The
    // catalog's already decoded argument rows retain quoted comma-bearing
    // type names; no signature string is split or reinterpreted here.
    let mut candidates: BTreeSet<_> = read
        .owners
        .keys()
        .filter_map(|target| {
            if let GrantTarget::Routine(routine) = target {
                (&routine.name == name).then(|| routine.clone())
            } else {
                None
            }
        })
        .collect();
    for p in &changes.changes[..index] {
        if let Change::CreateModule {
            id: ModuleId::Routine(routine),
            ..
        } = &p.change
            && &routine.name == name
        {
            candidates.insert(routine.clone());
        }
        if let Change::DropModule {
            id: ModuleId::Routine(routine),
            ..
        } = &p.change
        {
            candidates.remove(routine);
        }
        if let Change::RenameType { from, to, .. } = &p.change {
            candidates = candidates
                .into_iter()
                .map(|mut routine| {
                    move_type(&mut routine, from, to)?;
                    Ok(routine)
                })
                .collect::<Result<_, String>>()?;
        }
    }
    if candidates.len() != 1 {
        return Err(format!(
            "the object grant on {} does not identify one routine overload",
            name.shown()
        ));
    }
    Ok(GrantTarget::Routine(
        candidates.into_iter().next().expect("one candidate"),
    ))
}

#[cfg(all(test, unix))]
#[path = "object_grants_tests.rs"]
mod tests;
