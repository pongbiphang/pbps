//! The row scopes a plan pins: cutting reference data to the declared scope,
//! and what was held before the plan.

use super::*;

/// Report the cut's diagnostics before its unmanaged policy can refuse it.
pub(super) fn cut_report(
    pulled: &pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    unmanaged: db::CatalogPolicy<'_>,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<pbps_diff::Scoped> {
    let (scoped, permissions) = cut(pulled, ids, modules, external, unmanaged);
    warnings.emit(permissions.into_iter().map(|message| {
        crate::output::Finding::warning("catalog.unexpressible-permission", message)
    }));
    report_unmanaged(&scoped, unreadable, ids, modules, unmanaged.unmanaged)?;
    Ok(scoped)
}

/// One cut of a pulled catalog down to a managed set. Pure.
/// Returns the same permission subset added to the scoped reasons, so warnings
/// cannot label partition limitations as permissions (DEC-1739.1).
pub(super) fn cut<'a>(
    pulled: &'a pbps_db::catalog::Pulled,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
) -> (pbps_diff::Scoped, Vec<&'a str>) {
    let mut scoped = pbps_diff::scope(&pulled.schema, ids, modules, external);
    // These exact roles belong to the verified owned run, not its declarations.
    // A similarly named role remains subject to the project's ordinary policy.
    scoped
        .unmanaged_roles
        .retain(|role| !unmanaged.infrastructure_roles.contains(role));
    // A managed role's grant WITH GRANT OPTION is wider than the plain grant
    // the declarations can spell, and was left out of the role's set rather
    // than folded in. Carried beside the comparison so `verify` reports it
    // as drift and `plan --db` refuses to plan over it; on an unmanaged role,
    // or on somebody else's object, it is not ours (DECISIONS 95, 176).
    let permissions = unexpressible_permissions(pulled, ids, modules);
    scoped
        .unexpressible
        .extend(permissions.iter().map(|what| (*what).to_owned()));
    // Carried beside the comparison, never into it, exactly as the line above
    // carries a `WITH GRANT OPTION` (DECISIONS 95, 371). Cut to the managed
    // modules for the same reason every other field here is: what `PUBLIC`
    // may run on somebody else's routine is not ours to have an opinion on.
    scoped.public_execute = pulled
        .public_execute
        .iter()
        .filter(|id| modules.contains(*id))
        .cloned()
        .collect();
    // Kept whole rather than cut: a declared grant's target is the project's
    // by construction, and the caller looks each one up by name. An owner
    // this map lacks is an object the read did not see, which is a different
    // finding from one whose owner is somebody else.
    scoped.owners = pulled.owners.clone();
    scoped.session_role = pulled.session_role.clone();
    scoped.baseline_evidence = pulled
        .baseline_evidence
        .as_ref()
        .map(|evidence| evidence.for_managed(&scoped.schema));
    (scoped, permissions)
}

/// The modules the declarations name and the rows they declare, for the
/// commands that record a state.
///
/// `snapshot` and `baseline` write down what the environment is; a module the
/// declarations manage, or a table whose rows they declare, has to be in that
/// record, or the next `verify` would not be watching it. The ids file is
/// already required by both, so requiring the declarations to parse as well
/// changes nothing about when they can run.
/// The whole `Loaded` travels back, not just its schema: the state these two
/// commands record carries the module dependency annotations beside it, and
/// those live in the hints rather than in the model (constraint 8).
pub(super) fn declared_scope(
    project: &Project,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<(BTreeSet<ModuleId>, DataScopes, pbps_load::Loaded)> {
    let loaded = crate::load(project, dialect)?;
    Ok((
        managed_modules(None, Some(&loaded.schema)),
        loaded.schema.data_scopes(),
        loaded,
    ))
}

/// The scopes a plan's baseline is pinned under: the recorded ones and the
/// plan's own, table by table, each the union of the two where both cover it.
///
/// A table gaining its first `data:` block has rows the differ measured
/// against and the recorded state knows nothing of; a table whose `ensure`
/// block gains a key, or switches to `exact`, has rows the differ measured
/// that the recorded scope alone would not look at again. The baseline
/// checksum and `apply`'s check both read this union, so a row that appears
/// or changes in such a table between plan and apply is a mismatch, not a
/// row the approved changes silently missed or overwrote (DECISIONS 98).
pub(super) fn pinned_scopes(recorded: &DataScopes, planned: &DataScopes) -> DataScopes {
    let mut out = recorded.clone();
    for (name, scope) in planned {
        let pinned = match out.remove(name) {
            Some(recorded) => recorded.union(scope.clone()),
            None => scope.clone(),
        };
        out.insert(name.clone(), pinned);
    }
    out
}

/// The plan's data scopes under the names the catalog has *now*.
///
/// `plan.data` is keyed by each table's final name. Halfway through a staged
/// rename that moves both the schema and the name, the table stands at an
/// intermediate name, and a scope keyed by the final one would make the
/// checkpoint read no rows for it — so a hand-made change to those rows while
/// the deployment is paused would go unseen by `--resume`. The scopes follow
/// the same mapping the checkpoint's ids follow: final name -> uid -> the
/// name `live_ids` has for it. A table the plan has not created yet keeps its
/// final name and is simply not there to read.
pub(super) fn scopes_at(plan: &pbps_model::SavedPlan, live_ids: &IdsFile) -> DataScopes {
    scopes_under(&plan.data, &plan.ids, live_ids)
}

/// The state read under the widened scope, kept to the rows the plan still
/// manages once its renames have run (DEC-2346.2): see
/// [`pbps_diff::enum_labels::trim_to_the_final_scope`].
pub(super) fn trim_to_the_planned_scope(
    plan: &pbps_model::SavedPlan,
    ids: &IdsFile,
    scopes: &DataScopes,
    state: &mut Schema,
) {
    pbps_diff::enum_labels::trim_to_the_final_scope(
        state,
        scopes,
        (&TypeResolver::new(ids, &plan.ids), TypeSide::Base),
        &pbps_diff::enum_labels::label_renames(plan.changes.changes.iter().map(|p| &p.change)),
    );
}

/// `scopes` with the keys of the tables an enum keys also named by the labels
/// they hold before this plan's label renames (DEC-2346.2). The engine renames
/// a label in the rows that hold it, so a read taken before the rename finds
/// the row only under the label it still has, and one taken after finds it
/// under the new one; the baseline of every apply, staged or not, asks both.
/// `schema` and `ids` are the observed side's, whose type references resolve
/// against the plan's.
pub(super) fn held_before_the_plan(
    plan: &pbps_model::SavedPlan,
    schema: &Schema,
    ids: &IdsFile,
    scopes: &DataScopes,
) -> DataScopes {
    held_before(&plan.changes, &plan.ids, schema, ids, scopes)
}

/// [`held_before_the_plan`] on its parts, for the side that has the changes
/// and the ids but not yet a saved plan.
pub(super) fn held_before(
    changes: &pbps_model::ChangeSet,
    plan_ids: &IdsFile,
    schema: &Schema,
    ids: &IdsFile,
    scopes: &DataScopes,
) -> DataScopes {
    pbps_diff::enum_labels::scopes_before_the_plan(
        scopes,
        schema,
        (&TypeResolver::new(ids, plan_ids), TypeSide::Base),
        &pbps_diff::enum_labels::label_renames(changes.changes.iter().map(|p| &p.change)),
    )
}

/// The scopes a plan's baseline checksum is taken over, on both sides of the
/// pin: [`pinned_scopes`] of the recorded scopes and the planned ones, with the
/// planned ones widened by the labels their keys hold before the plan's renames
/// (DEC-2346.2). A row the plan depends on only under the label it has now,
/// the takeover of an unmanaged `sad` renamed to `blue`, is in what the
/// checksum pins, so a change to it between `plan` and `apply` is drift. The
/// plan computes it and the apply recomputes it from the same inputs through
/// this one function; with no rename of a label a key holds it is
/// [`pinned_scopes`] exactly.
pub(super) fn pinned_baseline_scopes(
    recorded: &DataScopes,
    planned: &DataScopes,
    changes: &pbps_model::ChangeSet,
    plan_ids: &IdsFile,
    (schema, ids): (&Schema, &IdsFile),
) -> DataScopes {
    pinned_scopes(
        recorded,
        &held_before(changes, plan_ids, schema, ids, planned),
    )
}

/// The declared schema with its tables keyed by the names `live_ids` gives
/// their uids — the names the database has before this plan runs — so that
/// a scope, a row reference or a row spelling looked up by the live name
/// finds the declaration of the same table. Everything inside a table is
/// left as declared; only the map keys move.
pub(super) fn tables_under(schema: &Schema, final_ids: &IdsFile, live_ids: &IdsFile) -> Schema {
    let mut out = schema.clone();
    out.tables = schema
        .tables
        .iter()
        .map(|(final_name, table)| (final_ids.resolved_in(final_name, live_ids), table.clone()))
        .collect();
    out
}

/// [`scopes_at`] on its parts: `data` keyed by the names `final_ids` gives
/// each table, re-keyed by the names `live_ids` gives the same uids.
///
/// The lookup itself is [`IdsFile::resolved_in`] — shared with `doctor`'s
/// readiness questions (DECISIONS 439), which resolve a declared object to
/// the name an environment currently has it under for the same reason this
/// does: a name only the declarations have moved to is not yet a name the
/// database answers to.
pub(super) fn scopes_under(
    data: &DataScopes,
    final_ids: &IdsFile,
    live_ids: &IdsFile,
) -> DataScopes {
    data.iter()
        .map(|(final_name, scope)| (final_ids.resolved_in(final_name, live_ids), scope.clone()))
        .collect()
}
