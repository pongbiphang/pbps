//! PostgreSQL's positions of a connected plan.
//!
//! The bodies are the `Driver::Postgres` arms of the functions of
//! [`super::super`] that the pipeline used to call one by one. Those functions
//! keep their other callers (bootstrap, apply, the pins) and reach these bodies
//! through the free functions here, so each body is written once.

use super::*;

/// PostgreSQL's own wording, kept as it was, not the default's.
pub(super) const DEPENDENCIES_REFUSAL: &str =
    "PostgreSQL does not support cross-database dependencies";

/// What PostgreSQL carries from one position of a plan to a later one.
#[derive(Default)]
pub(crate) struct PlanState {
    /// Whether a routine a declared type's input path calls still needs review
    /// (sealed at the start, read by the misspelling probe, which would
    /// otherwise call that routine on a value it parses). Neutral code never
    /// reads it, so it is state and not a return value.
    type_routines_need_review: bool,
    /// What the name read of the planning transaction found (#951), which the
    /// schedule may undo by planning the revision afresh: ordered and checked
    /// again over the same reads once it has run (#1767).
    name_facts: Option<NameFacts>,
    /// The verifier of a table rebuild the schedule expanded, run once every
    /// pass that orders the plan has (#1954).
    rebuild_verification: Option<RebuildVerification>,
}

pub(in crate::engine) async fn seal_type_pins(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> anyhow::Result<Prerequisites> {
    let pins = pbps_pg::type_refs::seal(conn, schemas, changes, trusted).await?;
    type_input_routines(conn, &pins, trusted, pin_routines, false).await?;
    let providers = pbps_pg::class_pins::seal(conn, schemas, changes).await?;
    Ok(Prerequisites {
        types: pins,
        providers,
    })
}

pub(in crate::engine) async fn seal_plan_type_pins(
    conn: &mut Conn,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> anyhow::Result<Prerequisites> {
    let pins = pbps_pg::type_refs::seal_sides(conn, base, desired, changes, trusted).await?;
    type_input_routines(conn, &pins, trusted, pin_routines, false).await?;
    let providers =
        pbps_pg::class_pins::seal(conn, &[base.schema, desired.schema], changes).await?;
    Ok(Prerequisites {
        types: pins,
        providers,
    })
}

pub(in crate::engine) async fn type_input_routines(
    conn: &mut Conn,
    pins: &[pbps_model::TypePin],
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
    inside: bool,
) -> anyhow::Result<BTreeSet<i64>> {
    let within = if inside {
        pbps_pg::pins::Within::CallersTransaction
    } else {
        pbps_pg::pins::Within::OwnTransaction
    };
    // Where each type is now: a placeholder names a type the plan itself
    // brings and is not read, and a type its package's move carried is
    // found under the name the move gave it (DEC-2447.12).
    let references = pbps_pg::type_refs::current_references(conn, within, pins).await?;
    let admission = pbps_pg::admission::Admission::new(trusted.iter().cloned(), pin_routines);
    Ok(pbps_pg::type_refs::input_routines(conn, within, &references, &admission).await?)
}

pub(in crate::engine) async fn external_role_renames(
    conn: &mut Conn,
    recorded: &IdsFile,
    declared: &IdsFile,
) -> anyhow::Result<ExternalRoleRenames> {
    let mut renames = BTreeMap::new();
    for (from, to) in recorded.role_renames(declared) {
        let evidence = pbps_pg::roles::rename_evidence(conn, from, to).await?;
        if let Some(error) = pbps_pg::roles::refuse_rename(from, to, evidence) {
            anyhow::bail!("rename_evidence (PostgreSQL): {error}");
        }
        renames.insert(from.clone(), to.clone());
    }
    let check = ConnectedCheck {
        name: "rename_evidence",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{} external cluster-role rename(s) confirmed complete",
            renames.len()
        ),
    };
    Ok(ExternalRoleRenames { renames, check })
}

/// Reconcile only incoming and renamed cluster roles against the same read
/// used for the baseline checksum. A second presence query could approve a
/// role that arrived after that read, while the saved baseline still lacks it.
pub(in crate::engine) fn reconcile_cluster_roles(
    recorded: &IdsFile,
    queried: &IdsFile,
    renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
    live: &Schema,
    expected: &mut Schema,
) -> anyhow::Result<ConnectedCheck> {
    let mut checked = 0;
    for to in queried.roles.values().filter(|name| {
        renames.values().any(|to| to == *name) || !recorded.roles.values().any(|old| old == *name)
    }) {
        let Some(role) = live.roles.get(to) else {
            anyhow::bail!(
                "missing_roles (PostgreSQL): declared cluster role `{to}` is missing; create or rename it first, then plan again",
                to = to.shown(),
            );
        };
        expected.roles.insert(to.clone(), role.clone());
        checked += 1;
    }
    Ok(ConnectedCheck {
        name: "missing_roles",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{checked} incoming or renamed cluster role(s) present in the queried baseline"
        ),
    })
}

/// Refuses a name this plan creates that a PostgreSQL relation-namespace
/// entry outside the catalog inventory already holds (#951). SQL Server's
/// `sys.objects` walk also orders the plan, so it runs after every pass that
/// changes the plan: [`order_created_object_names`]. On PostgreSQL it first
/// orders a cross-schema rename after the rename that moves a carried
/// object's holder away (#1767, DEC-1957.2). What it read is returned, for
/// [`NameFacts::order_and_refuse`] again once the module passes, which may
/// plan the revision afresh in the differ's order, have run.
/// Type occupants are retained here and refused only by that settled replay.
#[allow(clippy::wildcard_enum_match_arm)] // Only type creators/renames add pg_type queries.
pub(in crate::engine) async fn refuse_created_name_occupants<S: Clone>(
    conn: &mut Conn,
    cs: &mut PlanningPlan<S>,
    label: &str,
    plan: Planned<'_>,
) -> anyhow::Result<NameFacts> {
    // The names it creates, and those its table renames take on
    // their way (#1785 review).
    let mut asked = crate::deploy::created_relation_names(cs);
    asked.extend(crate::deploy::rename_claims(cs));
    let mut owners = crate::deploy::transferred_tables(cs);
    let mut detach_releases = std::collections::BTreeMap::new();
    let mut detach_retained = std::collections::BTreeMap::new();
    for change in &cs.changes {
        if let pbps_model::Change::DetachPartition {
            uid, parent, names, ..
        } = &change.change
        {
            let Some(table) = plan.base.ids.tables.get(uid) else {
                continue;
            };
            owners.push(table.clone());
            let Some(parent) = plan
                .desired
                .ids
                .tables
                .iter()
                .find(|(_, name)| *name == parent)
                .and_then(|(id, _)| plan.base.ids.tables.get(id))
            else {
                continue;
            };
            let clones = pbps_pg::catalog::detach_clone_names(conn, table, parent).await?;
            let slots = crate::deploy::detach_clone_slots(uid, names, &clones);
            detach_releases.extend(slots.detach_releases);
            detach_retained.extend(slots.detach_retained);
        }
    }
    let mut occupants = pbps_pg::catalog::relation_name_occupants(conn, &asked, &owners).await?;
    // What a cross-schema rename carries lands on names of its own,
    // which something else may hold there now (#1749), a table or a
    // view included (#1765).
    let carried = crate::deploy::carried_names(cs, &occupants);
    let landing: Vec<TableName> = carried
        .iter()
        .filter(|n| !occupants.iter().any(|o| &o.name == *n))
        .cloned()
        .collect();
    for found in pbps_pg::catalog::relation_name_occupants(conn, &landing, &[]).await? {
        if !occupants.contains(&found) {
            occupants.push(found);
        }
    }
    let mut at_names = carried;
    at_names.extend(crate::deploy::rename_claims(cs));
    at_names.extend(cs.changes.iter().flat_map(|p| {
        crate::deploy::sequence_names_claimed(&p.change)
            .into_iter()
            .map(|(name, _)| name)
    }));
    at_names.extend(
        cs.changes
            .iter()
            .filter(|p| matches!(p.change, pbps_model::Change::DetachPartition { .. }))
            .flat_map(|p| crate::deploy::relation_names_created(&p.change)),
    );
    let mut type_names = asked.clone();
    type_names.extend(cs.changes.iter().flat_map(|p| match &p.change {
        pbps_model::Change::CreateType { name, .. } => {
            vec![TableName::new(name.schema(), name.name())]
        }
        pbps_model::Change::RenameType { from, to, .. } => vec![
            TableName::new(to.schema(), to.name()),
            TableName::new(to.schema(), from.name()),
        ],
        _ => Vec::new(),
    }));
    let type_occupants = pbps_pg::catalog::type_names_at(conn, &type_names)
        .await?
        .into_iter()
        .filter(|name| {
            !plan.base.schema.tables.contains_key(name)
                && !plan.base.schema.modules.iter().any(|(id, m)| {
                    m.kind == pbps_model::ModuleKind::View && id.object_name() == *name
                })
                && !plan
                    .base
                    .schema
                    .types
                    .keys()
                    .any(|t| t.schema() == name.schema && t.name() == name.name)
        })
        .collect();
    let facts = NameFacts {
        type_occupants,
        relations: pbps_pg::catalog::relations_at(conn, &at_names).await?,
        occupants,
        detach_releases,
        detach_retained,
    };
    facts.order_relations_and_refuse(cs, label, plan)?;
    Ok(facts)
}

/// The PostgreSQL body of `engine::refuse_unlogged_partition_referencers`, which apply also calls.
pub(in crate::engine) async fn refuse_unlogged_partition_referencers(
    conn: &mut Conn,
    cs: &ChangeSet,
    ids: &pbps_model::IdsFile,
    staged: bool,
) -> anyhow::Result<()> {
    // Each parent under the name the catalog has it by: the partition names
    // it as declared, after a rename this plan may run first (#1613, #1690).
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let parents = crate::deploy::stored_unlogged_parents(&names);
    if parents.is_empty() {
        return Ok(());
    }
    let referencers = pbps_pg::catalog::permanent_referencers(conn, &parents)
        .await
        .map_err(|e| {
            anyhow::Error::new(e).context(
                "cannot read the foreign keys that reference a parent this plan adds an unlogged \
                 partition to",
            )
        })?;
    crate::deploy::refuse_permanent_referencers(cs, &names, &referencers, staged)
}

/// The PostgreSQL body of `engine::refuse_published_unlogged`, which apply also calls.
pub(in crate::engine) async fn refuse_published_unlogged(
    conn: &mut Conn,
    cs: &ChangeSet,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<()> {
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let made_unlogged = crate::deploy::tables_made_unlogged(&names);
    if made_unlogged.is_empty() {
        return Ok(());
    }
    let stored: Vec<TableName> = made_unlogged.iter().map(|(_, s)| s.clone()).collect();
    let naming = pbps_pg::catalog::publications_naming(conn, &stored)
        .await
        .map_err(|e| {
            anyhow::Error::new(e)
                .context("cannot read the publications of a table this plan makes unlogged")
        })?;
    crate::deploy::refuse_published_unlogged(&made_unlogged, &naming)
}

/// The modules a plan must rebuild around the enums it rebuilds and does not
/// yet, found in the planning transaction (DEC-1635.7).
///
/// What stands on a rebuilt enum comes down and goes back through the module
/// rebuild, and anything else is refused by name. Only PostgreSQL has enums
/// to rebuild; SQL Server has no module to add.
pub(in crate::engine) async fn enum_rebuild_dependents(
    conn: &mut Conn,
    changes: &ChangeSet,
    declared: Option<&Schema>,
    before: &IdsFile,
) -> anyhow::Result<BTreeSet<pbps_model::ModuleId>> {
    if !changes
        .changes
        .iter()
        .any(|p| matches!(p.change, pbps_model::Change::EnumRebuild { .. }))
    {
        return Ok(BTreeSet::new());
    }
    let census = pbps_pg::type_rebuild::census(conn, changes, declared, before, false)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(census.rebuild)
}

/// A table rebuild expands at the existing connected continuation.
pub(in crate::engine) async fn expand_table_rebuild<
    S: pbps_diff::order::RouteState<D>,
    D: crate::emission::Engine,
>(
    conn: &mut Conn,
    admission: &pbps_pg::admission::Admission,
    changes: PlanningPlan<S>,
    before: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &pbps_model::Hints,
    dialect: &D,
) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck, RebuildVerification)> {
    let (links, cap) = changes
        .changes
        .first()
        .and_then(|p| {
            if let pbps_model::Change::RebuildPartitionedTable(r) = &p.change {
                Some((r.earlier.len() + 1, r.cap()))
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("missing qualified rebuild root"))?;
    let (changes, observed, verification) =
        pbps_pg::rebuild::plan(conn, admission, changes, before, desired, hints, dialect).await?;
    Ok((
        changes,
        ConnectedCheck {
            name: "blocking_rewrite",
            engine: "PostgreSQL",
            status: if observed.is_some() {
                "checked"
            } else {
                "unchecked"
            },
            message: match observed {
                Some((rows, bytes)) => format!(
                    "blocking rewrite composed over {links} approved links: {rows} observed rows, max_rows {cap}, {bytes} total bytes; about twice the disk space while both trees exist; the full copy is written to WAL and can delay replicas",
                ),
                None => format!(
                    "blocking rewrite composed over {links} approved links, max_rows {cap}: the rows were not observed because a conversion routine awaits source review; apply observes them before copying; about twice the disk space while both trees exist; the full copy is written to WAL and can delay replicas",
                ),
            },
        },
        verification,
    ))
}

/// The planner's half of #314: reads what depends on every module this plan
/// drops and puts each dependent on the right side of that drop, in the plan
/// the approver sees (`dependents::weave`, which the schedule runs over the
/// dependents just read, ahead of the passes that read no catalog,
/// DEC-1955.1). Called inside the planning transaction, before
/// `check_module_rebuilds`, so that a view the plan now drops and recreates is
/// held to the same bar as one the declarations edit.
///
/// It is where a connected plan is scheduled, on either engine: SQL Server
/// supplies DEFAULT holders for explicit routine drops, along with the members
/// of the roles it drops, `role_members`, which the caller read when needed.
///
/// `rediff` plans the same revision again with the given unchanged modules
/// rebuilt (`pbps_diff::diff_rebuilding`): a module dependent the plan does
/// not touch has to be rebuilt around the module under it, and a rebuild is
/// its grants and its `PUBLIC` execute as well as its two statements, which
/// only the differ's own passes write.
// The eighth argument is the role members the caller read: the schedule
// takes every fact in one call (#1955).
#[allow(clippy::too_many_arguments)]
pub(in crate::engine) async fn account_for_module_dependents<
    S: pbps_diff::order::RouteState<D>,
    D: crate::emission::Engine,
>(
    conn: &mut Conn,
    mut changes: PlanningPlan<S>,
    declared: &pbps_model::Schema,
    sources: crate::dependents::Sources<'_>,
    deps: &pbps_model::object_ref::Annotations,
    role_members: Option<&crate::dependents::schedule::RoleMembers>,
    dialect: &D,
    rediff: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
    let (declarations, read) = defaults::scheduled_inputs(declared, sources, deps, role_members);
    // Version is observed once, before the pure expansion. Ordinary CLI
    // planning has no extra write-path schemas (DEC-1503.1).
    let pg = dialect.with_planning_version(pbps_pg::roles::server_version_num(conn).await?);
    let dialect = &pg;
    let mut found = module_dependents(conn, &changes).await?;
    let untouched = crate::dependents::untouched_module_dependents(&changes, &found, declared);
    if !untouched.is_empty() {
        changes = rediff(&untouched)?;
        found = module_dependents(conn, &changes).await?;
    }
    // Every reordering pass, in one place (#1955): `weave` over what the
    // catalog just said depends on the modules this plan drops, then the
    // passes that read nothing.
    let policies = policy_dependents(conn, &changes, sources.after, sources.before.schema).await?;
    // Only a plan that moves a partition while changing its parent's columns
    // reads stored column facts. The read stays in this transaction and call,
    // beside the dependents it is consulted with (DEC-2296.2).
    let tables = pbps_diff::order::boundary_catalog_tables(
        &changes,
        sources.before,
        pbps_diff::Side {
            schema: declared,
            ids: sources.after,
        },
    );
    let stored = if tables.is_empty() {
        None
    } else {
        Some(
            pbps_pg::modules::dependencies::references(
                conn,
                pbps_model::operation::Snapshot {
                    schema: sources.before.schema,
                    ids: sources.before.ids,
                },
                &tables,
            )
            .await?,
        )
    };
    let facts = read
        .with_module_dependents(&found)
        .with_policy_dependents(&policies);
    let facts = match &stored {
        Some(stored) => facts.with_membership_catalog(stored),
        None => facts,
    };
    let scheduled = crate::dependents::schedule::schedule(changes, &declarations, &facts, dialect)?;
    let changes = scheduled.changes;
    pbps_diff::order::row_security::check_releases(&changes, sources.after, &policies)?;
    let (added, split, moved) = (scheduled.added, scheduled.split, scheduled.moved);
    let partitions = crate::dependents::standing_partitions(declared, &changes);
    // On the final order: every pass above may move what an expression
    // names, or the expression (#1576).
    // Which name each unnamed key's index takes depends on what holds its
    // candidates now (#1645).
    let holders =
        pbps_pg::catalog::key_name_holders(conn, &crate::dependents::key_name_prefixes(&changes))
            .await
            .map_err(|e| {
                anyhow::anyhow!("module_dependents (PostgreSQL): reading what holds key names: {e}")
            })?;
    let mut later = Vec::new();
    for name in crate::dependents::names_a_later_relation(
        &changes,
        dialect,
        Some(&holders),
        sources.before.schema,
    ) {
        if !resolves_now(conn, &name).await? {
            later.push(name);
        }
    }
    crate::dependents::later_relation_refusal(&later)
        .map_err(|why| anyhow::anyhow!("module_dependents (PostgreSQL): {why}"))?;
    let left = crate::dependents::unaccounted(
        &changes,
        &found,
        &partitions,
        &sources
            .project(&changes.changes)
            .map_err(anyhow::Error::msg)?,
    );
    if !left.is_empty() {
        anyhow::bail!(
            "module_dependents (PostgreSQL): the plan still drops a module before what depends \
             on it; this is a bug in pbps, please report it:\n  {}",
            left.join("\n  ")
        );
    }
    let dependents: usize = found.values().map(Vec::len).sum();
    let check = ConnectedCheck {
        name: "module_dependents",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{dependents} dependent(s) of {} dropped or rebuilt module(s) removed before the drop; {added} change(s) added to the plan for them; {split} part(s) split out of new tables; {moved} addition(s) moved after the function(s) they call",
            found.len()
        ),
    };
    Ok((changes, check))
}

/// [`order_after_releases`], with the live edges of every table the plan
/// retypes or drops a column of. PostgreSQL only.
pub(in crate::engine) async fn release_generated_inputs<S: Clone>(
    conn: &mut Conn,
    changes: &mut PlanningPlan<S>,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
    dialect: &dyn pbps_dialect::Dialect,
    declared: &pbps_model::ModuleDeps,
) -> anyhow::Result<usize> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, after.ids)?;
    let mut dependences = BTreeMap::new();
    for p in &changes.changes {
        if let Some((table, ..)) = changed_column(&names, &p.change)
            && let std::collections::btree_map::Entry::Vacant(e) = dependences.entry(table)
        {
            let found = pbps_pg::generated::dependences(conn, e.key()).await?;
            e.insert(found);
        }
    }
    order_after_releases(changes, before, after, &dependences, dialect, declared)
        .map_err(|why| anyhow::anyhow!("generation_support (PostgreSQL): {why}"))
}

/// The PostgreSQL body of `engine::check_module_rebuilds`, which apply also calls.
pub(in crate::engine) async fn check_module_rebuilds(
    conn: &mut Conn,
    changes: &ChangeSet,
    after: bool,
) -> anyhow::Result<ConnectedCheck> {
    let rebuilt = rebuilt_modules(changes);
    for (id, (before_kind, after_kind)) in &rebuilt {
        let found = pbps_pg::modules::before_a_rebuild(
            conn,
            id,
            if after { *after_kind } else { *before_kind },
            changes,
        )
        .await?;
        if let Some(reason) = found.refusal() {
            anyhow::bail!("before_a_rebuild (PostgreSQL): {reason}");
        }
        if !after && let pbps_pg::modules::Serialized::Not(reason) = found.serialized {
            eprintln!("warning: {id}: {reason}", id = id.shown());
        }
    }
    Ok(ConnectedCheck {
        name: "before_a_rebuild",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{} module rebuild(s) checked for catalog state a replacement cannot preserve",
            rebuilt.len()
        ),
    })
}

/// The PostgreSQL body of `engine::check_drop_blockers`, which apply also calls.
pub(in crate::engine) async fn check_drop_blockers(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    stored: &pbps_model::Schema,
    stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let sources = pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)?;
    let reports = pbps_pg::impact::drop_blockers(conn, &sources).await?;
    refuse_drop_reports("PostgreSQL", &reports)?;
    Ok(ConnectedCheck {
        name: "drop_blockers",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{} table/column/key/index drop(s) checked against current catalog dependencies",
            reports.len()
        ),
    })
}

/// The logical replication slots that would decode the rows this plan moves
/// out of a DEFAULT partition (#1788), listed and never refused: a slot's
/// consumer may take a delete and an insert in its stride, and whether it
/// does is what the operator allowing the move answers. A publication is
/// another matter, refused by the pre-flight, since a subscriber is known to
/// lose the rows. Asked only of a plan that makes a partition beside a
/// DEFAULT; whether it moves any row is the apply's to count.
pub(in crate::engine) async fn logical_slots(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> anyhow::Result<ConnectedCheck> {
    let moving: Vec<String> = changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::CreateTable {
                name,
                beside_default: Some(default),
                ..
            } = &p.change
            {
                Some(format!(
                    "{default} into {name}",
                    default = default.shown(),
                    name = name.shown()
                ))
            } else {
                None
            }
        })
        .collect();
    if moving.is_empty() {
        return Ok(ConnectedCheck {
            name: "logical_slots",
            engine: "PostgreSQL",
            status: "not_applicable",
            message: "this plan makes no partition beside a DEFAULT, so it moves no rows"
                .to_owned(),
        });
    }
    let slots = pbps_pg::impact::logical_slots(conn).await?;
    Ok(slots_listed(&moving, &slots))
}

/// What logical decoding misses of a plan's identity or `serial` column
/// added to a table already there (#1890, DEC-1890.1), listed and never
/// refused. The engine numbers every row already there in the table's
/// rewrite, and measured on 18.6 that rewrite reaches no stream: a
/// `test_decoding` slot shows an empty transaction, a `pgoutput` slot nothing,
/// and a subscriber keeps the new column empty for those rows, while a row
/// inserted afterwards arrives with its number. So the publications over the
/// table and the database's logical slots are named, for whoever answers for
/// their consumers. A table the plan creates is empty when made: it has no
/// `AddColumn`, and its rows stream as they come.
pub(in crate::engine) async fn numbered_column_fills(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    use pbps_model::plan_names::At;
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let mut fills: Vec<(TableName, pbps_model::ColumnName)> = Vec::new();
    for (i, p) in changes.changes.iter().enumerate() {
        let pbps_model::Change::AddColumn {
            table,
            name,
            column,
            ..
        } = &p.change
        else {
            continue;
        };
        let numbered = match column.value_source {
            pbps_model::ValueSource::Identity(_) | pbps_model::ValueSource::Sequence(_) => true,
            pbps_model::ValueSource::None
            | pbps_model::ValueSource::Default(_)
            | pbps_model::ValueSource::Generated(_) => false,
        };
        // The table under the name the catalog has it by now, after a
        // rename this plan may run first; none for one the plan creates.
        if let (true, Some(live)) = (numbered, names.stored(table, At::Before(i))) {
            fills.push((live, name.clone()));
        }
    }
    if fills.is_empty() {
        return Ok(ConnectedCheck {
            name: "numbered_column_fills",
            engine: "PostgreSQL",
            status: "not_applicable",
            message: "this plan adds no identity or serial column to a table already there"
                .to_owned(),
        });
    }
    let mut publications = Vec::new();
    for (table, _) in &fills {
        if !publications
            .iter()
            .any(|(t, _): &(TableName, _)| t == table)
        {
            let found = pbps_pg::impact::publications_of(conn, table).await?;
            publications.push((table.clone(), found));
        }
    }
    let slots = pbps_pg::impact::logical_slots(conn).await?;
    Ok(fills_listed(&fills, &publications, &slots))
}

/// The PostgreSQL body of `engine::prepare_data_writes`, which apply also calls.
pub(in crate::engine) async fn prepare_data_writes(
    conn: &mut Conn,
    admission: &pbps_pg::admission::Admission,
    changes: &ChangeSet,
    baseline: &StateSnapshot,
    planned_ids: &pbps_model::IdsFile,
) -> anyhow::Result<DataWriteGuard> {
    use pbps_dialect::{RowOperation, RowWrite};
    use pbps_model::Change;
    let mut writes = Vec::new();
    let mut dropped = pbps_pg::data_triggers::Dropped::default();
    // Asked once per row change and updated column: indexed once,
    // not scanned per question (#1870).
    let planned_names = planned_ids.index();
    let baseline_names = baseline.ids.index();
    // The plan's own name for a table, in the spelling the catalog
    // still holds: the guard runs before the renames (DECISIONS 445).
    let stored = |table: &pbps_model::TableName| {
        planned_names
            .table_uid(table)
            .or_else(|| baseline_names.table_uid(table))
            .and_then(|uid| baseline.ids.tables.get(uid))
            .cloned()
    };
    let stored_column = |column: &pbps_model::ColumnRef| {
        planned_names
            .column_uid(column)
            .or_else(|| baseline_names.column_uid(column))
            .and_then(|uid| baseline.ids.columns.get(uid))
            .map_or_else(
                || column.name.raw().to_owned(),
                |old| old.name.raw().to_owned(),
            )
    };
    for planned in &changes.changes {
        if let Change::DropModule { id, .. } = &planned.change {
            dropped.modules.insert(id.clone());
        }
        // Both are ordered ahead of every row change, so the
        // referential action they carry cannot write when the row
        // statement runs (DECISIONS 451).
        if let Change::DropForeignKey { table, name } = &planned.change
            && let Some(old) = stored(table)
        {
            dropped.foreign_keys.insert((old, name.clone()));
        }
        if let Change::DropTable { name, .. } = &planned.change {
            dropped
                .tables
                .insert(stored(name).unwrap_or_else(|| name.clone()));
        }
        // The expressions the plan removes or replaces before a row is
        // written or a table rewritten: `prepare` does not judge what
        // will be gone by then, and `check` judges whatever is not
        // (DEC-2563.9).
        dropped.note_removed_expressions(&planned.change, &stored, &stored_column);
        // A table the plan rewrites, scans or copies into runs its index
        // expressions as the deploying role even though no row of the
        // plan is written to it. One that did not exist has no stored
        // name and is judged at its own statement instead.
        if let Some(write) = pbps_pg::existing_expressions::rewrite_write(&planned.change)
            && let Some(old) = stored(&write.table)
        {
            writes.push(RowWrite {
                table: old,
                operation: write.operation,
            });
        }
        let (table, mut operation) = if let Change::InsertRow {
            table,
            key_column,
            row,
            ..
        } = &planned.change
        {
            // The columns the INSERT names: the guard judges the
            // DEFAULT of no other (#2748).
            (
                table,
                RowOperation::Insert {
                    columns: std::iter::once(key_column.clone())
                        .chain(row.columns().map(|(column, _)| column.clone()))
                        .collect(),
                },
            )
        } else if let Change::UpdateRow { table, columns, .. } = &planned.change {
            (
                table,
                RowOperation::Update {
                    columns: columns.keys().cloned().collect(),
                },
            )
        } else if let Change::DeleteRow { table, .. } = &planned.change {
            (table, RowOperation::Delete)
        } else {
            continue;
        };
        // The logical target has the plan's final spelling; locks are
        // acquired before any rename, through the stable table uid.
        // New tables receive no existing-trigger allowance.
        if let Some(old) = stored(table).as_ref() {
            // Like the table, an updated or inserted column may still
            // carry its pre-plan name while the guard authenticates
            // triggers.
            if let RowOperation::Update { columns } | RowOperation::Insert { columns } =
                &mut operation
            {
                *columns = columns
                    .iter()
                    .map(|name| {
                        let column = pbps_model::ColumnRef {
                            table: table.clone(),
                            name: name.clone(),
                        };
                        planned_names
                            .column_uid(&column)
                            .or_else(|| baseline_names.column_uid(&column))
                            .and_then(|uid| baseline.ids.columns.get(uid))
                            .map_or_else(|| name.clone(), |old| old.name.clone())
                    })
                    .collect();
            }
            writes.push(RowWrite {
                table: old.clone(),
                operation,
            });
        }
    }
    Ok(DataWriteGuard {
        postgres: Some(
            pbps_pg::data_triggers::prepare(conn, admission, &writes, &baseline.schema, &dropped)
                .await?,
        ),
    })
}

/// PostgreSQL's module rebuild keeps its catalog state only inside the one
/// transaction a staged run does not have (ADR-0009 §3).
pub(in crate::engine) fn require_transactional_rebuilds(
    changes: &ChangeSet,
    staged: bool,
) -> anyhow::Result<()> {
    defaults::staged_rebuilds(changes, staged, || {
        if staged && !rebuilt_modules(changes).is_empty() {
            anyhow::bail!(
                "PostgreSQL module rebuilds require a transaction to preserve their catalog state \
                 (ADR-0009 §3); remove --staged and plan again"
            );
        }
        Ok(())
    })
}

impl PlanSession for pbps_pg::Postgres {
    type State = PlanState;

    async fn compare_dependencies(
        &self,
        _conn: &mut Conn,
        declarations: &pbps_model::dependency::Dependencies,
    ) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
        if declarations.is_empty() {
            Ok(Default::default())
        } else {
            anyhow::bail!("{DEPENDENCIES_REFUSAL}")
        }
    }

    async fn seal_declared_type_pins(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        declared: &Schema,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<()> {
        let initial = seal_type_pins(
            conn,
            &[declared],
            &Default::default(),
            trusted,
            pin_routines,
        )
        .await?;
        state.type_routines_need_review =
            !type_input_routines(conn, &initial.types, trusted, pin_routines, false)
                .await?
                .is_empty();
        Ok(())
    }

    async fn external_role_renames(
        &self,
        conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        external_role_renames(conn, recorded, declared).await
    }

    async fn misspelt(
        &self,
        state: &PlanState,
        conn: &mut Conn,
        schema: &Schema,
        at: &CatalogNames,
    ) -> Result<Spellings, RowsError> {
        pbps_pg::catalog::misspelt(conn, schema, at, state.type_routines_need_review).await
    }

    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError> {
        pbps_pg::catalog::schema_spellings(conn, names).await
    }

    /// PostgreSQL declares no column collation: `validate` refuses the field
    /// there, so there is nothing to ask the server.
    async fn refuse_unknown_collations(
        &self,
        _conn: &mut Conn,
        _declared: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// PostgreSQL has no system-versioned table.
    async fn refuse_history_references_alike(
        &self,
        _conn: &mut Conn,
        _declared: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Reconcile only incoming and renamed cluster roles against the same read
    /// used for the baseline checksum. A second presence query could approve a
    /// role that arrived after that read, while the saved baseline still lacks
    /// it.
    fn reconcile_cluster_roles(
        &self,
        recorded: &IdsFile,
        queried: &IdsFile,
        renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
        live: &Schema,
        expected: &mut Schema,
    ) -> anyhow::Result<ConnectedCheck> {
        reconcile_cluster_roles(recorded, queried, renames, live, expected)
    }

    /// A staged plan's table-local name facts, read beside its schema facts.
    /// Identities map a renamed attaching table back to its current catalog
    /// name. Preserve the project's dialect and ordinary planning's unknown
    /// server version.
    async fn staged_attach_dialect(
        &self,
        conn: &mut Conn,
        before: pbps_diff::Side<'_>,
        after: pbps_diff::Side<'_>,
    ) -> anyhow::Result<Self> {
        let attaching = after
            .ids
            .tables
            .iter()
            .filter_map(|(uid, name)| {
                let table = after.schema.tables.get(name)?;
                table.partition_of.as_ref()?;
                let live = before.ids.tables.get(uid)?;
                let standing = before.schema.tables.get(live)?;
                standing
                    .partition_of
                    .is_none()
                    .then(|| (uid.clone(), live.clone()))
            })
            .collect();
        Ok(crate::emission::Engine::with_attach_constraint_names(
            self,
            pbps_pg::staged::attach_constraint_names(conn, &attaching).await?,
        ))
    }

    /// Adds to `hints` the facts about the target that planning a type or an
    /// extension change needs and the differ cannot see: a rebuilt enum or
    /// domain keeps its comment, which `CREATE TYPE` does not copy, and which
    /// domains an array holds, which PostgreSQL refuses a new check on in place
    /// and a rebuild takes instead (DEC-1635.7, DEC-1635.9), and the extension
    /// facts (#2447). All are facts about the target, read before the differ
    /// writes the swap that carries them.
    async fn type_rebuild_hints(
        &self,
        conn: &mut Conn,
        base: &Schema,
        declared: &Schema,
        recorded_ids: &IdsFile,
        resolved_ids: &IdsFile,
        hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        let types: Vec<_> = base.types.keys().cloned().collect();
        hints.enum_comments = pbps_pg::type_rebuild::comments(conn, &types).await?;
        let domains: Vec<_> = base
            .types
            .iter()
            .filter(|(_, ty)| matches!(ty, pbps_model::ManagedType::Domain { .. }))
            .filter_map(|(name, _)| Some((recorded_ids.type_uid(name)?.clone(), name.clone())))
            .collect();
        hints.domain_array_holders = pbps_pg::domain_alter::array_holders(conn, &domains).await?;
        hints.extension_requires =
            crate::deploy::extension_creates(conn, base, declared, resolved_ids).await?;
        hints.extension_updates =
            crate::deploy::extension_update_facts(conn, base, declared, recorded_ids, resolved_ids)
                .await?;
        hints.extension_moves =
            crate::deploy::extension_move_facts(conn, base, declared, recorded_ids, resolved_ids)
                .await?;
        hints.extension_drops = crate::deploy::extension_drop_facts(
            conn,
            &crate::deploy::retired_extensions(base, declared, recorded_ids, resolved_ids),
        )
        .await?;
        Ok(())
    }

    fn computed_column_screen(&self) -> pbps_diff::Screen {
        computed_column_screen(Driver::Postgres)
    }

    /// Asks the resolver's scratch to compare the expressions a plan retains,
    /// which the engine accepts by parse tree, and records the answers in
    /// `hints`.
    ///
    /// Only PostgreSQL retains expressions this way. Without a scratch entry
    /// there is nobody to ask, and a database that cannot be asked either is
    /// left unanswered.
    async fn retained_expression_hints(
        &self,
        scratch: Option<&mut crate::resolution::Entry<'_>>,
        conn: &mut Conn,
        target: &crate::db::Target,
        before: pbps_diff::Side<'_>,
        after: pbps_diff::Side<'_>,
        read_back: &Schema,
        hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        if let Some(entry) = scratch {
            let requests = pbps_diff::retained::candidates(before, after, hints);
            if !requests.is_empty() && pbps_pg::retained::available(conn, &requests).await {
                // Paired binding carries the resolver lifecycle. Keep its future
                // separate from the connected planner's other binding phases.
                hints.retained_expressions = Box::pin(crate::resolution::retained::compare(
                    entry, conn, target, before, after, read_back, hints, &requests,
                ))
                .await?;
            }
        }
        Ok(())
    }

    /// Reads the `NOT VALID` checks of the domains standing in the database.
    ///
    /// PostgreSQL has them.
    async fn standing_not_valid_domain_checks(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<Vec<StandingNotValid>> {
        Ok(pbps_pg::domain_alter::not_valid(conn)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .map(|found| StandingNotValid {
                domain: found.domain,
                check: found.check,
                remedy: found.remedy,
            })
            .collect())
    }

    async fn seal_plan_type_review(
        &self,
        conn: &mut Conn,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<bool> {
        let pins = seal_plan_type_pins(conn, base, desired, changes, trusted, pin_routines).await?;
        Ok(
            !type_input_routines(conn, &pins.types, trusted, pin_routines, false)
                .await?
                .is_empty(),
        )
    }

    fn refuse_occupied_relation_names(
        &self,
        changes: &ChangeSet,
        scoped: &pbps_diff::Scoped,
        unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
        unmanaged_relations: &[TableName],
        label: &str,
    ) -> anyhow::Result<()> {
        crate::deploy::refuse_occupied_names(
            changes,
            scoped,
            unreadable,
            unmanaged_relations,
            label,
        )
    }

    fn require_transactional_rebuilds(
        &self,
        changes: &ChangeSet,
        staged: bool,
    ) -> anyhow::Result<()> {
        require_transactional_rebuilds(changes, staged)
    }

    async fn check_names_against_catalog<S: Clone>(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        frame: Frame<'_>,
        staged: bool,
    ) -> anyhow::Result<()> {
        // In the read-only planning transaction: the names this plan creates,
        // asked of PostgreSQL's whole relation namespace, not only of what
        // the inventory reports (#951).
        let planned = Planned {
            base: frame.recorded,
            desired: frame.written,
            dialect: self,
        };
        state.name_facts =
            Some(refuse_created_name_occupants(conn, changes, frame.label, planned).await?);
        refuse_unlogged_partition_referencers(conn, changes, frame.written.ids, staged).await?;
        refuse_published_unlogged(conn, changes, frame.written.ids).await?;
        Ok(())
    }

    async fn read_before_schedule(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        declared: &Schema,
        recorded_ids: &IdsFile,
    ) -> anyhow::Result<ScheduleReads> {
        // PostgreSQL plans no role drop (a role is the cluster's): the
        // question refuses, as it always did, rather than answer "no members".
        if defaults::drops_a_role(changes) {
            return Err(roles_are_the_clusters("reading the role memberships")
                .context("cannot read the role memberships"));
        }
        let enum_rebuild =
            enum_rebuild_dependents(conn, changes, Some(declared), recorded_ids).await?;
        Ok(ScheduleReads {
            role_members: None,
            enum_rebuild,
        })
    }

    async fn schedule<S: pbps_diff::order::RouteState<Self>>(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        admission: &SiteAdmission,
        changes: PlanningPlan<S>,
        frame: Frame<'_>,
        role_members: Option<&RoleMembers>,
        again: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
    ) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
        if changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::RebuildPartitionedTable(_)))
        {
            let (changes, check, verification) = expand_table_rebuild(
                conn,
                admission,
                changes,
                frame.base,
                frame.declared,
                frame.hints,
                self,
            )
            .await?;
            state.rebuild_verification = Some(verification);
            Ok((changes, check))
        } else {
            account_for_module_dependents(
                conn,
                changes,
                frame.written.schema,
                crate::dependents::Sources {
                    before: frame.recorded,
                    after: frame.written.ids,
                },
                &frame.hints.annotations,
                role_members,
                self,
                again,
            )
            .await
        }
    }

    async fn order_by_catalog<S: Clone>(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        frame: Frame<'_>,
    ) -> anyhow::Result<Ordered> {
        // A retype or drop of a column a generated column reads goes after
        // the plan's change to that expression which stops reading it
        // (DEC-1316.1): after the module passes, which may move that change,
        // and before the checks that read the order. PostgreSQL has no
        // computed columns and no `sys.objects` walk, so nothing else orders
        // the plan here.
        release_generated_inputs(
            conn,
            changes,
            frame.recorded,
            frame.written,
            self,
            &frame.hints.annotations.modules(frame.written.schema),
        )
        .await?;
        // The module passes may plan the revision again in the differ's
        // order (`rediff`), undoing what was ordered: the carried moves are
        // ordered and checked again, over the same reads, as the plan now
        // stands (#1767 review).
        if let Some(facts) = &state.name_facts {
            let planned = Planned {
                base: frame.recorded,
                desired: frame.written,
                dialect: self,
            };
            facts.order_and_refuse(changes, frame.label, planned)?;
        }
        // Every pass that orders the plan has run: the verifier checks the
        // order that is saved and applied, in tests and debug builds
        // (#1954, DEC-1954.6).
        let verified = if let Some(verification) = &state.rebuild_verification {
            verification
                .verify(frame.base, frame.declared, frame.hints, changes, self)
                .map_err(|why| anyhow::anyhow!("{why}"))?;
            true
        } else {
            false
        };
        Ok(Ordered {
            verified,
            facts: state.name_facts.as_ref().map(NameFacts::slots),
        })
    }

    async fn check_final_order(
        &self,
        conn: &mut Conn,
        admission: &SiteAdmission,
        changes: &ChangeSet,
        frame: Frame<'_>,
    ) -> anyhow::Result<FinalOrderChecks> {
        let rebuilds = check_module_rebuilds(conn, changes, false).await?;
        let drop_blockers = check_drop_blockers(
            conn,
            changes,
            frame.written.ids,
            frame.recorded.schema,
            frame.recorded.ids,
        )
        .await?;
        let logical_slots = logical_slots(conn, changes).await?;
        let numbered_column_fills = numbered_column_fills(conn, changes, frame.written.ids).await?;
        prepare_data_writes(conn, admission, changes, frame.baseline, frame.written.ids).await?;
        Ok(FinalOrderChecks {
            rebuilds,
            drop_blockers,
            logical_slots,
            numbered_column_fills,
        })
    }
}
