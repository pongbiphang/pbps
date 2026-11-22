//! Roles, memberships and grants.

use super::*;

/// How deep among the roles this plan drops each dropped role sits: a role
/// no other dropped role holds is 0, and each membership step adds one.
///
/// A connected plan removes a dropped role's members by name before its
/// `DROP ROLE`, and the engine refuses `ALTER ROLE [z] DROP MEMBER [a]` once
/// `a` is gone — measured, along with the fact that dropping `a` while it is
/// a member of `z` succeeds and takes the membership with it. So a role
/// holding another dropped role has to go *first*, and with every drop at
/// rank 0 the order was left to the name tiebreaker: dropping `a` before `z`
/// rolled the whole apply back (DECISIONS 127).
///
/// Wildcarded deliberately: no change other than a role drop can put a role
/// into this order, and one added later could not without also being a drop.
///
/// Membership among roles cannot be cyclic, and the walk is bounded by the
/// number of drops regardless, so a catalog that somehow held a cycle costs
/// a wrong order rather than a hang.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn member_depth(planned: &[PlannedChange]) -> BTreeMap<pbps_model::RoleName, usize> {
    let dropped: BTreeMap<&pbps_model::RoleName, &[pbps_model::RoleName]> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::Role(RoleChange::DropRole { name, members, .. }) => {
                Some((name, members.as_slice()))
            }
            _ => None,
        })
        .collect();
    let mut depth: BTreeMap<pbps_model::RoleName, usize> =
        dropped.keys().map(|n| ((*n).to_owned(), 0)).collect();
    for _ in 0..dropped.len() {
        let mut moved = false;
        for (holder, members) in &dropped {
            let above = depth[*holder];
            for member in *members {
                if let Some(d) = depth.get_mut(member)
                    && *d <= above
                {
                    *d = above + 1;
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
    depth
}

/// Roles are matched by **uid** (ADR-0005), and their grants by target after
/// the base side's object names have been brought forward through this plan's
/// table renames — a grant follows its object through `sp_rename`, so a
/// renamed table must not come out as a revoke on the old name plus a grant
/// on the new one.
///
/// A revoke on an object this same plan drops is not emitted: the drop takes
/// the permission with it, and a `REVOKE` that ran after it would fail on an
/// object that is gone (and one ordered before it would be noise).
pub(super) fn diff_roles(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    // Whether the *principal* is this tool's at all (ADR-0010 §3,
    // DECISIONS 211). A SQL Server database role lives in the one database
    // this connection is to, so its existence is managed here; a PostgreSQL
    // role is a cluster object, and a plan that created, renamed or dropped
    // one would reach every other database in the cluster.
    //
    // What a role *holds in this database* is managed either way, so only the
    // three identity changes are conditional and every `Grant` and `Revoke`
    // below is built the same on both engines.
    let manages_roles = dialect.manages_roles();
    // The table marker can stay equal while its set of sequences grows. Its
    // statement discovers that set after the column changes (DEC-1892.1).
    let sequence_creates: BTreeSet<_> = changes
        .iter()
        .flat_map(|change| {
            if let Change::Column(ColumnChange::AddColumn {
                table,
                name,
                column,
                ..
            }) = change
            {
                return matches!(column.value_source, ValueSource::Sequence(_))
                    .then(|| table.column(name))
                    .into_iter()
                    .collect::<Vec<_>>();
            }
            if let Change::Column(ColumnChange::AlterColumnNumbering {
                column, from, to, ..
            }) = change
            {
                return (matches!(to, ValueSource::Sequence(_))
                    && !matches!(from, ValueSource::Sequence(_)))
                .then(|| column.clone())
                .into_iter()
                .collect();
            }
            if let Change::Table(TableChange::CreateTable { name, table, .. }) = change {
                return table
                    .columns
                    .iter()
                    .filter(|(_, c)| matches!(c.value_source, ValueSource::Sequence(_)))
                    .map(|(column, _)| name.column(column))
                    .collect();
            }
            Vec::new()
        })
        .collect();
    let detaching: BTreeSet<_> = changes
        .iter()
        .filter_map(|change| {
            if let Change::Partition(PartitionChange::DetachPartition { uid, .. }) = change {
                Some(uid.clone())
            } else {
                None
            }
        })
        .collect();
    let effective = |role: &pbps_model::Role, schema: &Schema| {
        let mut grants = pbps_model::role::effective_grants(role, &schema.tables);
        // Keep the compact marker as the existing Grant statement. Individual
        // column USAGE entries below are the union used for comparisons.
        for (target, rights) in &role.grants {
            if rights.contains(&Permission::SerialSequences) {
                grants
                    .entry(target.clone())
                    .or_default()
                    .insert(Permission::SerialSequences);
            }
        }
        grants
    };
    let mut dropped: Vec<pbps_model::Dropped> = changes.iter().filter_map(Change::drops).collect();
    // `Change::drops` answers `None` for `AlterModule` — the object is
    // restated, not removed, and every other reader of it wants exactly that
    // (DECISIONS 158's caller, and the staged-run rewind in
    // `pbps-cli::deploy`, both ask "is this identity gone for good"). But a
    // dialect that rebuilds a module on every edit (`rebuilds_modules`,
    // ADR-0009 §3, #248) takes the object's grants with it too, precisely as
    // a `DropModule` does — so *this* comparison, and only this one, treats
    // an `AlterModule` the same way: a grant unchanged by the declarations is
    // still gone from the rebuilt object and has to be written into the plan
    // again, or it silently does not come back.
    if dialect.rebuilds_modules() {
        dropped.extend(changes.iter().filter_map(|c| {
            if let Change::Module(ModuleChange::AlterModule { id, .. }) = c {
                Some(pbps_model::Dropped::Module(id.clone()))
            } else {
                None
            }
        }));
    }
    // Base table name -> the name it has after this plan, by uid.
    let renamed: BTreeMap<&TableName, &TableName> = base
        .ids
        .tables
        .iter()
        .filter_map(|(uid, name)| declared.ids.tables.get(uid).map(|to| (name, to)))
        .filter(|(from, to)| from != to)
        .collect();
    // A name that passes to another table (DEC-1118.1). The baseline's grants
    // under it belonged to a table this plan drops, and went with it: the
    // occupant's own grants are the ones carried here by `forward`, so the
    // name is compared as the occupant's, not as a dropped object's.
    let changing_hands = names_changing_hands(base, declared);
    let base_uid: BTreeMap<&TableName, &Uid> = base
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    let detached_usage = |role: &pbps_model::Role, target: &GrantTarget| {
        let GrantTarget::Sequence(column) = target else {
            return false;
        };
        role.grants.iter().any(|(target, rights)| {
            let GrantTarget::Object(table) = target else {
                return false;
            };
            rights.contains(&Permission::SerialSequences)
                && base_uid
                    .get(table)
                    .is_some_and(|uid| detaching.contains(*uid))
                && pbps_model::role::serial_grant_columns(table, &base.schema.tables)
                    .contains(column)
        })
    };
    let dropped_with_its_table = |target: &GrantTarget| -> bool {
        match target {
            GrantTarget::Object(o) => base_uid
                .get(o)
                .is_some_and(|uid| !declared.ids.tables.contains_key(*uid)),
            GrantTarget::Sequence(c) => base.ids.columns.iter().any(|(uid, from)| {
                // Keeping the column UID does not keep its serial sequence:
                // changing to a default or identity removes the old sequence
                // and its grants, so no separate revoke is needed.
                from == c
                    && declared
                        .ids
                        .columns
                        .get(uid)
                        .and_then(|column| {
                            declared
                                .schema
                                .tables
                                .get(&column.table)
                                .and_then(|table| table.columns.get(&column.name))
                        })
                        .is_none_or(|column| {
                            !matches!(column.value_source, ValueSource::Sequence(_))
                        })
            }),
            GrantTarget::Routine(_) | GrantTarget::Schema(_) => false,
        }
    };
    let changing_hands_target = |target: &GrantTarget| match target {
        GrantTarget::Object(table) => changing_hands.contains(table),
        GrantTarget::Sequence(column) => changing_hands.contains(&column.table),
        GrantTarget::Routine(_) | GrantTarget::Schema(_) => false,
    };
    let forward = |target: &GrantTarget| -> GrantTarget {
        match target {
            GrantTarget::Object(o) => match renamed.get(o) {
                Some(to) => GrantTarget::Object((*to).clone()),
                None => target.clone(),
            },
            // A routine's identity moves with its name, so a rename of the
            // object carries the signature with it unchanged: only tables are
            // renamed by uid here, and a routine is not a table.
            GrantTarget::Sequence(column) => {
                let final_column = base
                    .ids
                    .columns
                    .iter()
                    .find(|(_, c)| *c == column)
                    .and_then(|(uid, _)| declared.ids.columns.get(uid))
                    .cloned()
                    .unwrap_or_else(|| {
                        let table = renamed.get(&column.table).copied().unwrap_or(&column.table);
                        table.column(&column.name)
                    });
                GrantTarget::Sequence(final_column)
            }
            GrantTarget::Routine(_) | GrantTarget::Schema(_) => target.clone(),
        }
    };

    for (uid, name) in &base.ids.roles {
        if declared.ids.roles.contains_key(uid) {
            continue;
        }
        if manages_roles {
            changes.push(Change::Role(RoleChange::DropRole {
                uid: uid.clone(),
                name: name.clone(),
                // Only a connected plan can know them; see `Change::DropRole`.
                members: Vec::new(),
            }));
            continue;
        }
        // The role stays; what it was granted **here** does not. Anything
        // else would be one of the two silent wrong answers: a plan that says
        // "drop role" and runs nothing leaves a principal still holding every
        // permission pbps was managing, and a plan that really dropped it
        // would take the role out of every other database in the cluster.
        //
        // The revokes are the same shape as a narrowing of a role that stays,
        // so the risk classification and the approval gate see them as what
        // they are: access being taken away.
        let Some(held) = base.schema.roles.get(name) else {
            continue;
        };
        for (target, permissions) in &pbps_model::role::effective_grants(held, &base.schema.tables)
        {
            if dropped_with_its_table(target) {
                continue;
            }
            let mut permissions = permissions.clone();
            // Detach removes the implication, not the root's sequence ACL.
            // Leave that access to the live read's explicit column target
            // (DEC-2199.1), rather than revoking through the departed partition.
            if detached_usage(held, target) {
                permissions.remove(&Permission::Usage);
            }
            let target = forward(target);
            // A `REVOKE` on an object this same plan drops would fail on an
            // object that is gone — the same rule the per-target comparison
            // below applies, and for the same reason.
            let target_dropped =
                dropped.iter().any(|d| d.takes(&target)) && !changing_hands_target(&target);
            if permissions.is_empty() || target_dropped {
                continue;
            }
            changes.push(Change::Permission(GrantChange::Revoke {
                role: name.clone(),
                target,
                permissions,
            }));
        }
    }

    for (uid, name) in &declared.ids.roles {
        let Some(role) = declared.schema.roles.get(name) else {
            continue;
        };
        let Some(base_name) = base.ids.roles.get(uid) else {
            // Where the principal is not this tool's, whether the cluster
            // actually has this role is a question only a connection can
            // answer, and it is asked there: `pbps_pg::roles::missing_roles`
            // refuses a declared role the cluster lacks with the `CREATE ROLE`
            // to run by hand. Emitting a `CreateRole` here instead would
            // refuse the ordinary case as well — a DBA creating the role and
            // the project then declaring it, which is the only way a role ever
            // comes under management on that engine.
            if manages_roles {
                changes.push(Change::Role(RoleChange::CreateRole {
                    uid: uid.clone(),
                    name: name.clone(),
                }));
            }
            for (target, permissions) in &role.grants {
                if !permissions.is_empty() {
                    changes.push(Change::Permission(GrantChange::Grant {
                        role: name.clone(),
                        target: target.clone(),
                        permissions: permissions.clone(),
                    }));
                }
            }
            continue;
        };
        // A rename, where the principal is the cluster's, is a rename a human
        // performed there — and on this engine an ACL entry holds the role's
        // oid rather than its name, so every grant followed it and nothing has
        // to be re-granted.
        //
        // **This elision is sound only behind the connected check**
        // (`pbps_pg::roles::rename_evidence`), and the check is not "does the
        // new name exist". If the *old* name is still there as well, the two
        // are two principals: emitting nothing would leave the old one holding
        // everything pbps was managing while the declared one holds nothing,
        // and the plan would then record the declared one as holding it all.
        // The evidence that makes the rename a rename is the old name's
        // **absence**; and if the old role was dropped and a new one created
        // instead, its grants went with it, the pull shows the new role
        // holding nothing, and every declared grant is planned here anyway.
        if base_name != name && manages_roles {
            changes.push(Change::Role(RoleChange::RenameRole {
                uid: uid.clone(),
                from: base_name.clone(),
                to: name.clone(),
            }));
        }
        // The base grants, keyed by the name the target has after this plan.
        let mut before: BTreeMap<GrantTarget, BTreeSet<Permission>> = BTreeMap::new();
        let mut detached = BTreeSet::new();
        if let Some(b) = base.schema.roles.get(base_name) {
            for (target, permissions) in &effective(b, base.schema) {
                if dropped_with_its_table(target) {
                    continue;
                }
                if detached_usage(b, target) {
                    detached.insert(forward(target));
                }
                before
                    .entry(forward(target))
                    .or_default()
                    .extend(permissions.iter().copied());
            }
        }
        let desired = effective(role, declared.schema);
        let targets: BTreeSet<&GrantTarget> = before.keys().chain(desired.keys()).collect();
        for target in targets {
            // By whole identity: where routines overload, the drop of one
            // takes its own grants and leaves the sibling's to be compared.
            let target_dropped =
                dropped.iter().any(|d| d.takes(target)) && !changing_hands_target(target);
            // A DROP takes the object's permissions with it. An object this
            // plan drops and creates again under the same name — a table
            // replaced by a new one, a module changing kind — therefore has
            // *no* grants after the DROP, whatever the base held, and every
            // declared permission on it is a GRANT to write after the CREATE.
            // Comparing the two grant sets as text called them equal and
            // left the role without its access until a later plan noticed.
            let b = if target_dropped {
                BTreeSet::new()
            } else {
                before.get(target).cloned().unwrap_or_default()
            };
            let d = desired.get(target).cloned().unwrap_or_default();
            let mut added: BTreeSet<Permission> = d.difference(&b).copied().collect();
            if d.contains(&Permission::SerialSequences)
                && matches!(target, GrantTarget::Object(table) if !pbps_model::role::serial_grant_columns(table, &declared.schema.tables).is_disjoint(&sequence_creates))
            {
                added.insert(Permission::SerialSequences);
            }
            if let GrantTarget::Sequence(column) = target {
                if sequence_creates.contains(column) {
                    added.extend(d.iter().copied());
                }
                if role
                    .grants
                    .iter()
                    .any(|(target, rights)| matches!(target, GrantTarget::Object(table)
                        if rights.contains(&Permission::SerialSequences)
                        && pbps_model::role::serial_grant_columns(table, &declared.schema.tables).contains(column)))
                {
                    // The table marker grants this same ACL entry once.
                    added.remove(&Permission::Usage);
                }
            }
            let mut removed: BTreeSet<Permission> = b.difference(&d).copied().collect();
            // A marker's narrowing is settled per column: retained explicit
            // USAGE must never be revoked by a table-wide statement.
            removed.remove(&Permission::SerialSequences);
            if detached.contains(target) {
                removed.remove(&Permission::Usage);
            }
            if !added.is_empty() {
                changes.push(Change::Permission(GrantChange::Grant {
                    role: name.clone(),
                    target: target.clone(),
                    permissions: added,
                }));
            }
            if !removed.is_empty() && !target_dropped {
                changes.push(Change::Permission(GrantChange::Revoke {
                    role: name.clone(),
                    target: target.clone(),
                    permissions: removed,
                }));
            }
        }
    }
}

/// Settles what happens to the engine's default `EXECUTE` to `PUBLIC` on
/// every routine this plan brings into being (ADR-0010 §5, DECISIONS 371).
///
/// # Why this is a separate pass and not part of `diff_modules`
///
/// Because "brings into being" has three spellings and they are produced in
/// three different places: a routine the declarations have newly added, a
/// routine whose *kind* changed and which `diff_modules` therefore emits as a
/// drop plus a create, and — on an engine that rebuilds modules (ADR-0009 §3)
/// — every `AlterModule`, including the ones `rebound_modules` synthesizes
/// after the declarations were compared. All three end with a `CREATE`, and a
/// `CREATE` is what restores the default. Reading the built change list is
/// the only place that sees all three; a hook inside the comparison would
/// have seen the first.
///
/// # Why the opt-in is a hint and the revoke is the default
///
/// `PUBLIC` is context, never a compared grant, so neither side of the
/// comparison holds it and the differ cannot be *asked* for convergence here
/// — it can only be told what to write. Told nothing, it writes the revoke:
/// the routine may be `SECURITY DEFINER`, in which case the engine default
/// means "any principal that can reach this schema may act as the owner", and
/// which routines are definer routines is inside a body this tool never
/// parses. A declaration that wants the default back says so by name, in a
/// line that goes through the merge request and into the plan's checksum like
/// everything else.
pub(super) fn revoke_public_execution(
    base: &Schema,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    changes: &mut Vec<Change>,
) {
    if !dialect.creates_public_executable_routines() {
        return;
    }
    // `AlterModule` is only a create on an engine that rebuilds: where the
    // edit is a `CREATE OR ALTER`, the object keeps the ACL it had and a
    // revoke here would take away a state this plan was not asked to change.
    let rebuilds = dialect.rebuilds_modules();
    let mut decided = Vec::new();
    for change in changes.iter() {
        // Written as `if let` rather than a match with a wildcard: every
        // other kind of change ends with no `CREATE`, so there is nothing to
        // enumerate and a list of them would need maintaining for nothing.
        let id = if let Change::Module(ModuleChange::CreateModule { id, .. }) = change {
            id
        } else if let Change::Module(ModuleChange::AlterModule { id, .. }) = change
            && rebuilds
        {
            id
        } else {
            continue;
        };
        let ModuleId::Routine(routine) = id else {
            continue;
        };
        // The declaration's answer, written into the plan either way. Saying
        // nothing where it asks for the default back would leave the
        // connected rebuild guard unable to tell that plan from one with no
        // opinion, and it refuses the missing default it was asked to restore
        // (ADR-0009 §3).
        let access = if hints.public_execute.contains(id) {
            PublicAccess::Kept
        } else {
            PublicAccess::Revoked
        };
        // Whether the object was there before, not which variant said so: a
        // kind change arrives here as a `CreateModule` and is still a routine
        // somebody may have been executing a moment ago.
        let origin = if base.modules.contains_key(id) {
            RoutineOrigin::Rebuilt
        } else {
            RoutineOrigin::Created
        };
        decided.push(Change::Permission(GrantChange::PublicExecution {
            routine: routine.clone(),
            access,
            origin,
        }));
    }
    changes.extend(decided);
}
