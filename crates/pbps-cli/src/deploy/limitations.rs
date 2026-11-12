//! What the managed set leaves out or cannot express: limitations, unmanaged
//! and unreadable objects, permission advisories.

use super::*;

/// Every fact inside the managed set that the catalog projection could not
/// express: an unsupported feature on a managed table, or a module the set
/// names whose definition introspection cannot read back.
///
/// The second half is what keeps a recorded scope honest. [`managed_modules`]
/// rebuilds the set from the recorded schema's keys, so a name that schema does
/// not hold is a name every later command forgets. Left as a warning, a
/// declared encrypted procedure was exempted from `unmanaged: error` by the
/// `baseline` that recorded it and refused as an undeclared object by the
/// first `verify` after — a policy violation on a database nothing had touched.
/// A module pbps cannot read is inside the managed set by name and outside it
/// in fact; that is a partial schema, and the recorders already refuse one.
pub(crate) fn managed_limitations(
    pulled: &pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &std::collections::BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
) -> Vec<String> {
    let mut managed_tables: std::collections::BTreeSet<_> = ids.tables.values().collect();
    // A partition of a managed parent whose partitions are external (#1533)
    // is held only as its parent makes it: what the catalog read could not
    // take in, a hash index of its own say, is a part of its own the scope
    // cannot see, and is refused as one (#1833 review).
    managed_tables.extend(pulled.schema.tables.iter().filter_map(|(name, table)| {
        let of = table.partition_of.as_ref()?;
        (external.contains(&of.parent) && ids.tables.values().any(|t| t == &of.parent))
            .then_some(name)
    }));
    pulled
        .limitations
        .iter()
        .filter(|limitation| match &limitation.target {
            pbps_db::catalog::LimitationTarget::Type(name) => ids
                .types
                .values()
                .any(|t| t.schema() == name.schema && t.name() == name.name),
            // A text search object has no identity yet (#2728), so none is inside the
            // managed set: its omission is named by the census and not reported here.
            pbps_db::catalog::LimitationTarget::Search(_) => false,
            pbps_db::catalog::LimitationTarget::Relation(_)
            | pbps_db::catalog::LimitationTarget::SharedModule(_)
            | pbps_db::catalog::LimitationTarget::Module(_)
            | pbps_db::catalog::LimitationTarget::UnnameableModule(_) => {
                omission_is_managed(&limitation.target, &managed_tables, modules)
            }
        })
        .map(|limitation| limitation.detail.clone())
        .chain(
            pulled
                .unmanaged_modules
                .iter()
                .filter(|m| omission_is_managed(&m.target, &managed_tables, modules))
                // The catalog keeps both inventories for different consumers
                // (DECISIONS 426). Only this exact fact supersedes the fallback;
                // matching by target alone would discard another limitation.
                .filter(|m| {
                    !pulled.limitations.iter().any(|limitation| {
                        limitation.target == m.target && limitation.detail == m.why
                    })
                })
                .map(|m| {
                    format!(
                        "{} {} is in the managed set, but {}",
                        m.kind,
                        pbps_model::escape::Escaped(&m.target.to_string()),
                        m.why
                    )
                }),
        )
        .collect()
}

/// Omitted relations still belong to identity-file tables even when absent
/// from the pulled schema. Routine arguments and trigger parents retain their
/// own namespaces; a table identity must not capture either (DECISIONS 425).
pub(super) fn omission_is_managed(
    target: &pbps_db::catalog::LimitationTarget,
    tables: &BTreeSet<&TableName>,
    modules: &BTreeSet<ModuleId>,
) -> bool {
    matches!(target, pbps_db::catalog::LimitationTarget::Relation(name) if tables.contains(name))
        || modules.iter().any(|id| target.matches_module(id))
}

/// The unexpressible permissions that are this project's business.
///
/// A managed role's, first — an unmanaged role's grants are its own
/// (DECISIONS 95, 125). And then, on the securable: a `DENY`, a column-level
/// grant or a `WITH GRANT OPTION` on an object **outside the managed set** is
/// that object's business too, exactly as `pbps_diff::scope` says of the
/// *plain* grant beside it — "a grant on somebody else's table is that table's
/// business, and comparing it would have the next plan revoke a permission the
/// declarations were never allowed to name". Filtered by role alone, the plain
/// grant was dropped and the unsupported one stopped every command
/// (DECISIONS 176).
///
/// Membership is tested against the managed set as *declared*, not against the
/// cut schema: a managed module the catalog could not read back is absent from
/// the second and still ours (491edd9).
///
/// Schema targets and the targetless ones stay. A schema grant is declarable,
/// so a `DENY` on one is a difference the declarations cannot hold; a
/// permission on the database itself belongs to no object at all, and a role
/// that gained one has changed (DECISIONS 105).
pub(crate) fn unexpressible_permissions<'a>(
    pulled: &'a pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
) -> Vec<&'a str> {
    permissions_the_managed_set_keeps(&pulled.unexpressible, ids, modules)
}

/// The same cut, asked of a list on its own — by `init`, which has findings
/// and a generated identity file but no `Pulled` left to ask.
///
/// One function rather than two that have to agree: `init` tells the operator
/// which of these `baseline` will refuse over, and `baseline` is this filter
/// (#308).
pub(crate) fn permissions_the_managed_set_keeps<'a>(
    found: &'a [pbps_db::catalog::Unexpressible],
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
) -> Vec<&'a str> {
    let managed_tables: BTreeSet<&TableName> = ids.tables.values().collect();
    found
        .iter()
        .filter(|u| ids.roles.values().any(|managed| managed == &u.role))
        .filter(|u| {
            u.target
                .as_ref()
                .is_none_or(|target| permission_target_is_managed(target, &managed_tables, modules))
        })
        .map(|u| u.what.as_str())
        .collect()
}

/// One target cut for fatal permissions and report-only advisories. A routine
/// signature must not capture a relation of the same name (DECISIONS 176, 384).
pub(super) fn permission_target_is_managed(
    target: &pbps_model::GrantTarget,
    tables: &BTreeSet<&TableName>,
    modules: &BTreeSet<ModuleId>,
) -> bool {
    match target {
        pbps_model::GrantTarget::Object(o) => {
            tables.contains(o)
                || modules.iter().any(|id| {
                    !matches!(id, ModuleId::Routine(_)) && id.referenced_name().as_ref() == Some(o)
                })
        }
        pbps_model::GrantTarget::Routine(r) => modules
            .iter()
            .any(|id| matches!(id, ModuleId::Routine(other) if other == r)),
        pbps_model::GrantTarget::Sequence(c) => tables.contains(&c.table),
        // Schema grants are declarable independently of object identities;
        // preserve the fatal filter's existing schema scope (DEC-2446.1).
        pbps_model::GrantTarget::Schema(_) => true,
    }
}

pub(super) fn permission_advisories<'a>(
    found: &'a [pbps_db::catalog::Unexpressible],
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
) -> Vec<&'a str> {
    let tables = ids.tables.values().collect();
    found
        .iter()
        .filter(|u| !ids.roles.values().any(|managed| managed == &u.role))
        // A targetless finding cannot establish managed-object membership.
        .filter(|u| {
            u.target
                .as_ref()
                .is_some_and(|target| permission_target_is_managed(target, &tables, modules))
        })
        .map(|u| u.what.as_str())
        .collect()
}

pub(super) fn permission_advisory_findings(
    advisories: &[String],
    project: &Project,
) -> Vec<crate::output::Finding> {
    let id = pbps_policy::rules::UNMANAGED_GRANT;
    let policies = project.config.policies();
    let today = pbps_policy::civil::at(crate::unix_seconds(), 0).date();
    policies
        .effective(id)
        .severity
        .filter(|_| !policies.suppressed(id, None, &today))
        .into_iter()
        .flat_map(|severity| {
            advisories.iter().map(move |what| {
                crate::policy_finding(&pbps_model::Finding::new(
                    id,
                    severity,
                    format!("{what}; the grantee is unmanaged, so this permission is outside drift comparison"),
                    None,
                ))
            })
        })
        .collect()
}

/// The tables whose partitions are external (#1533), by identity: the
/// recorded state's and each the plan creates so. Built as
/// [`modules_after`] is, from the state and the plan alone. A uid's setting
/// cannot change within a plan (the differ refuses the switch), so the one
/// set holds for every read the apply takes. Each read names them through
/// the ids it scopes with, by [`externals_under`]: by name, a rename chain
/// that hands a declared parent's name to an external one would mark the
/// declared parent external before its rename ran (#1833 review).
pub(super) fn external_uids(
    recorded: &pbps_model::StateSnapshot,
    changes: &pbps_model::ChangeSet,
) -> BTreeSet<pbps_model::Uid> {
    let names = pbps_diff::external_parents(&recorded.schema);
    let mut set: BTreeSet<_> = recorded
        .ids
        .tables
        .iter()
        .filter(|(_, name)| names.contains(*name))
        .map(|(uid, _)| uid.clone())
        .collect();
    for p in &changes.changes {
        if let pbps_model::Change::CreateTable { uid, table, .. } = &p.change
            && table.partition_by.as_ref().is_some_and(|by| by.external)
        {
            set.insert(uid.clone());
        }
    }
    set
}

/// The names `ids` gives the tables of [`external_uids`].
pub(super) fn externals_under(
    uids: &BTreeSet<pbps_model::Uid>,
    ids: &IdsFile,
) -> BTreeSet<TableName> {
    ids.tables
        .iter()
        .filter(|(uid, _)| uids.contains(*uid))
        .map(|(_, name)| name.clone())
        .collect()
}

/// The modules a plan leaves the environment holding.
///
/// Built from the recorded state plus the plan's own changes rather than from
/// the declarations, so that `apply` still needs nothing but the plan file — the
/// same reason the plan carries its ids (SPEC §7.3, and constraint 23).
pub(super) fn modules_after(
    recorded: &pbps_model::StateSnapshot,
    changes: &pbps_model::ChangeSet,
    settled: Settled,
) -> BTreeSet<ModuleId> {
    let mut set: BTreeSet<_> = recorded.schema.modules.keys().cloned().collect();
    for p in &changes.changes {
        // Every module change names its module and nothing else does, so the
        // accessor is the whole classification; only the direction is left.
        let Some(id) = p.change.module_id() else {
            continue;
        };
        // A module the plan drops leaves the set only once the plan has run.
        // Removed from every mid-run read as well, a module whose `DROP` had
        // not happened yet was already outside the managed set at each
        // checkpoint — absent from the checkpoint's schema, and invisible to
        // `--resume`, which scopes the live side the same way. The remaining
        // `DROP` then ran against an object nobody had looked at since the
        // plan was approved (DECISIONS 164).
        //
        // Keeping it needs no knowledge of which statements have run: one
        // already dropped is simply absent from the catalog, which the read
        // records truthfully, and one still standing stays watched.
        if settled.whole() && matches!(p.change, pbps_model::Change::DropModule { .. }) {
            set.remove(id);
        } else {
            set.insert(id.clone());
        }
    }
    set
}

/// The dependency annotations that describe an intermediate staged state.
///
/// `plan.module_deps` is the complete post-plan annotation set, including the
/// meaningful absence of an edge that was removed. It is therefore right for
/// every module that survives the plan. A module scheduled for deletion may
/// still exist at an early checkpoint, though, and its declaration has already
/// disappeared; until its DROP runs, preserve the dependency information from
/// the recorded state. Edges to modules no longer present at this checkpoint
/// are discarded so the snapshot never claims an impossible dependency.
pub(super) fn dependency_hints_for_schema(
    recorded: &pbps_model::StateSnapshot,
    plan: &pbps_model::SavedPlan,
    schema: &Schema,
) -> pbps_model::ModuleDeps {
    let dropped: std::collections::BTreeSet<_> = plan
        .changes
        .changes
        .iter()
        .filter(|planned| matches!(planned.change, pbps_model::Change::DropModule { .. }))
        .filter_map(|planned| planned.change.module_id())
        .collect();
    let present: std::collections::BTreeSet<_> = schema.modules.keys().collect();
    let recorded_modules = recorded.object_deps.to_deps().modules();
    let mut dependencies = pbps_model::ModuleDeps::new();

    for name in schema.modules.keys() {
        let source = if dropped.contains(name) {
            recorded_modules.get(name)
        } else {
            plan.module_deps.get(name)
        };
        let Some(source) = source else {
            continue;
        };
        let filtered = source
            .iter()
            .filter(|dependency| present.contains(*dependency))
            .cloned()
            .collect();
        dependencies.insert(name.clone(), filtered);
    }
    dependencies.retain(|_, required| !required.is_empty());
    dependencies
}

/// The modules a command is answerable for.
///
/// Two callers, two questions (see [`pbps_diff::scope`]): `verify` asks whether
/// this environment has moved since it was recorded, so the recorded state's
/// modules are the set; everything that plans or records asks what the state
/// should be, so the declarations count too.
///
/// The recorded set is the recorded schema's keys and nothing more. That is
/// sound only because no recorder writes a snapshot whose managed set names a
/// module the schema does not hold — [`managed_limitations`] turns such a
/// module into a refusal before `record` is reached.
pub(crate) fn managed_modules(
    recorded: Option<&pbps_model::StateSnapshot>,
    declared: Option<&pbps_model::Schema>,
) -> BTreeSet<ModuleId> {
    let mut set = BTreeSet::new();
    if let Some(s) = recorded {
        set.extend(s.schema.modules.keys().cloned());
    }
    if let Some(s) = declared {
        set.extend(s.modules.keys().cloned());
    }
    set
}

/// Says so when the deployment lock could not be released after a command that
/// had already failed.
///
/// The command's own error is what the caller returns — a cleanup failure must
/// not replace the reason the deployment stopped — but dropping the unlock
/// result on that path left `__pbps_lock` held with no word about it, and the
/// retry that should have fixed the environment failed as "locked" instead.
pub(super) fn warn_unreleased(label: &str, released: &Result<bool, pbps_db::DbError>) {
    if let Err(e) = released {
        eprintln!(
            "warning: the deployment lock on `{label}` was not released: {e}\n\
             Release it with `pbps unlock` before the next attempt.",
            label = pbps_model::escape::Escaped(label),
        );
    }
}

/// The `unmanaged: error` policy, refused.
///
/// A distinct type because `verify` has to tell it apart from every other way
/// its work can fail. The database was reached, the catalog was read, and the
/// command found something the project's own policy calls a problem — that is
/// an answer (exit 2, the schema owner's), not a failure to answer (exit 1,
/// CI's). Folded into the catch-all it came back as `environment.unreachable`
/// about a database that had just been read successfully.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnmanagedPolicy(String);

/// Converts unreadable modules and type limitations into the common identity
/// and description form used by connected commands.
pub(crate) fn unreadable_objects(
    pulled: &pbps_db::catalog::Pulled,
) -> Vec<(pbps_db::catalog::LimitationTarget, String)> {
    pulled
        .unmanaged_modules
        .iter()
        .map(|module| {
            (
                module.target.clone(),
                format!(
                    "{} {} ({})",
                    module.kind,
                    pbps_model::escape::Escaped(&module.target.to_string()),
                    module.why
                ),
            )
        })
        // An omitted type has no definition to scope, but remains a catalog
        // object subject to the unmanaged policy (SPEC §4.8, DEC-1635.1).
        .chain(
            pulled
                .limitations
                .iter()
                .filter(|limitation| {
                    matches!(
                        limitation.target,
                        pbps_db::catalog::LimitationTarget::Type(_)
                    )
                })
                .map(|limitation| {
                    (
                        limitation.target.clone(),
                        format!(
                            "type {} ({})",
                            pbps_model::escape::Escaped(&limitation.target.to_string()),
                            limitation.detail
                        ),
                    )
                }),
        )
        .collect()
}

/// Names every representable or unreadable catalog object outside the managed
/// set — tables, types, modules and roles alike — for applying the `unmanaged:`
/// policy of SPEC §8.2.
pub(crate) fn unmanaged_objects(
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    ids: &IdsFile,
    managed_modules: &std::collections::BTreeSet<ModuleId>,
) -> Vec<String> {
    let managed_tables = ids.tables.values().collect();
    scoped
        .unmanaged
        .iter()
        .map(|name| name.shown().to_string())
        .chain(
            scoped
                .unmanaged_types
                .iter()
                .map(|name| format!("type {}", name.shown())),
        )
        .chain(
            scoped
                .unmanaged_modules
                .iter()
                .map(|id| id.shown().to_string()),
        )
        // A role is a principal rather than an object, so it has no
        // `TableName` to be listed under and needs its own spelling — but the
        // policy is about everything in the database this project does not
        // declare, and an undeclared role is one of those (ADR-0005).
        .chain(
            scoped
                .unmanaged_roles
                .iter()
                .map(|r| format!("role {}", r.shown())),
        )
        // Unreadable modules and types are absent from `scoped.schema`, but they are
        // still catalog objects. If their target is outside the supplied managed
        // set, the unmanaged policy applies exactly as it does to a readable
        // module. Without this half, encryption was an accidental escape from
        // `unmanaged: error`.
        .chain(
            unreadable
                .iter()
                .filter(|(target, _)| match target {
                    pbps_db::catalog::LimitationTarget::Type(name) => !ids
                        .types
                        .values()
                        .any(|t| t.schema() == name.schema && t.name() == name.name),
                    // Unreadable, and with no identity to be managed by (#2728): outside.
                    pbps_db::catalog::LimitationTarget::Search(_) => true,
                    pbps_db::catalog::LimitationTarget::Relation(_)
                    | pbps_db::catalog::LimitationTarget::SharedModule(_)
                    | pbps_db::catalog::LimitationTarget::Module(_)
                    | pbps_db::catalog::LimitationTarget::UnnameableModule(_) => {
                        !omission_is_managed(target, &managed_tables, managed_modules)
                    }
                })
                .map(|(_, description)| description.clone()),
        )
        .collect()
}

/// Applies the `unmanaged:` policy to both representable and unreadable
/// catalog objects outside the managed set.
pub(super) fn report_unmanaged(
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    ids: &IdsFile,
    managed_modules: &std::collections::BTreeSet<ModuleId>,
    policy: pbps_config::Unmanaged,
) -> anyhow::Result<()> {
    let names = unmanaged_objects(scoped, unreadable, ids, managed_modules);
    apply_unmanaged_policy(&names, policy)
}

pub(super) fn apply_unmanaged_policy(
    names: &[String],
    policy: pbps_config::Unmanaged,
) -> anyhow::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let names_shown = names.join(", ");
    match policy {
        pbps_config::Unmanaged::Ignore => {}
        pbps_config::Unmanaged::Warn => eprintln!(
            "warning: {} object(s) in this database are not declared and are left alone: {}",
            names.len(),
            names_shown
        ),
        pbps_config::Unmanaged::Error => {
            return Err(UnmanagedPolicy(format!(
                "`unmanaged: error` in pbps.yml, and {} object(s) here are not declared: {}.\n\
                 Declare them (`pbps pull` reverse-generates them) or relax the setting.",
                names.len(),
                names_shown
            ))
            .into());
        }
    }
    Ok(())
}

/// A declared table the database does not have is worth saying out loud
/// wherever it turns up: it is either a hand-dropped table or an identity file
/// that describes a different database.
pub(super) fn report_missing(scoped: &pbps_diff::Scoped) {
    if !scoped.missing.is_empty() {
        let names: Vec<String> = scoped
            .missing
            .iter()
            .map(|name| name.shown().to_string())
            .collect();
        eprintln!(
            "warning: the identity file names {} table(s) this database does not have: {}",
            names.len(),
            names.join(", ")
        );
    }
    if !scoped.missing_types.is_empty() {
        eprintln!(
            "warning: the identity file names {} type(s) this database does not have: {}",
            scoped.missing_types.len(),
            scoped
                .missing_types
                .iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !scoped.missing_roles.is_empty() {
        eprintln!(
            "warning: the identity file names {} role(s) this database does not have: {}",
            scoped.missing_roles.len(),
            scoped
                .missing_roles
                .iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Refuses a command that would record or plan over a live permission the
/// declarations cannot hold.
///
/// A managed role's grant `WITH GRANT OPTION`, a column-level grant, a `DENY`:
/// `managed_state` leaves each out of `scoped.schema` and names it here
/// instead (DECISIONS 95, 97, 105). A state recorded from that schema is one
/// `verify` reports as drift the moment it is written, and a plan built on it
/// restates the plain `GRANT` on every run while the engine keeps the option
/// — so `snapshot`, `baseline` and `plan --db` all stop here rather than
/// write down a state that can never be clean (DECISIONS 110).
///
/// `snapshot --force` is not an escape. It answers "record a state that
/// differs from the recorded one", which is a different question from
/// "record a state pbps cannot express at all"; the second has no right
/// answer to force.
pub(super) fn refuse_unexpressible(
    scoped: &pbps_diff::Scoped,
    label: &str,
    then: &str,
) -> anyhow::Result<()> {
    if scoped.unexpressible.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` holds what the declarations cannot express:\n  {}\n\
         Resolve it by hand, then {then}.",
        scoped.unexpressible.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}
