//! SQL Server's positions of a connected plan.
//!
//! The bodies are the `Driver::Mssql` arms of the functions of [`super::super`]
//! that the pipeline used to call one by one. Those functions keep their other
//! callers (bootstrap, apply, the pins) and reach these bodies through the free
//! functions here, so each body is written once.

use super::*;

/// SQL Server has no `trusted_roles`; its wording is its own, not the
/// default's.
pub(super) const TRUSTED_ROLES_REFUSAL: &str = "trusted_roles is supported only on PostgreSQL";
pub(super) const PIN_ROUTINES_REFUSAL: &str = "pin_type_routines is supported only on PostgreSQL";
/// Only the PostgreSQL differ plans a table rebuild.
pub(super) const TABLE_REBUILD_REFUSAL: &str = "this engine does not support table rebuilds";

pub(in crate::engine) fn seal_type_pins(
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> anyhow::Result<Prerequisites> {
    if trusted.is_empty() {
        Ok(Prerequisites::default())
    } else {
        anyhow::bail!("{TRUSTED_ROLES_REFUSAL}")
    }
}

pub(in crate::engine) fn type_input_routines(
    pins: &[pbps_model::TypePin],
    pin_routines: bool,
) -> anyhow::Result<BTreeSet<i64>> {
    if !pin_routines && pins.is_empty() {
        Ok(BTreeSet::new())
    } else {
        anyhow::bail!("{PIN_ROUTINES_REFUSAL}")
    }
}

pub(in crate::engine) fn external_role_renames() -> ExternalRoleRenames {
    ExternalRoleRenames {
        renames: BTreeMap::new(),
        check: ConnectedCheck {
            name: "rename_evidence",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server renames database roles through the typed plan; external cluster-role rename evidence does not apply".into(),
        },
    }
}

pub(in crate::engine) async fn refuse_unknown_collations(
    conn: &mut Conn,
    schema: &Schema,
) -> anyhow::Result<()> {
    let names: Vec<String> = schema
        .tables
        .values()
        .flat_map(|t| t.columns.values())
        .filter_map(|c| c.collation.as_ref().map(|c| c.as_str().to_owned()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let unknown = pbps_mssql::catalog::unknown_collations(conn, &names).await?;
    if !unknown.is_empty() {
        anyhow::bail!(
            "this server has no collation named {}; a `COLLATE` naming it would be refused \
             mid-apply",
            unknown
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Refuses a declared filegroup the target cannot place the object on, by
/// name and before any statement runs (#2813, DEC-2813.1): one it does not
/// hold, one that is read-only, and one that is the database's default where
/// the declaration would read back as none, which no plan could converge on.
/// pbps never creates a filegroup, so none of these is something a later step
/// of the plan supplies.
pub(in crate::engine) async fn refuse_unknown_filegroups(
    conn: &mut Conn,
    schema: &Schema,
) -> anyhow::Result<()> {
    let declared = declared_filegroups(schema);
    if declared.is_empty() {
        return Ok(());
    }
    // Each distinct declared name is resolved by the engine, under the
    // database's collation, so that what is accepted here is what `ON [fg]`
    // will accept.
    let mut names: Vec<&str> = declared.iter().map(|(_, fg, _)| fg.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let found = pbps_mssql::catalog::filegroups_named(conn, &names).await?;
    let held: std::collections::BTreeMap<&str, pbps_mssql::catalog::FilegroupRow> = names
        .iter()
        .copied()
        .zip(found)
        .filter_map(|(n, row)| Some((n, row?)))
        .collect();
    let problems = filegroup_problems(&declared, &held);
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("\n"));
    }
    Ok(())
}

/// Every declared filegroup with where it is declared, and whether that
/// object sits where its table does when it names none (`follows`): an object
/// on the default filegroup reads back as none unless its table is elsewhere.
pub(in crate::engine) fn declared_filegroups(
    schema: &Schema,
) -> Vec<(String, &pbps_model::Filegroup, bool)> {
    let mut out = Vec::new();
    for (name, table) in &schema.tables {
        let table_name = name.shown();
        if let Some(fg) = &table.filegroup {
            out.push((format!("table `{table_name}`"), fg, false));
        }
        let beside = table.filegroup.is_some();
        if let Some(fg) = table
            .primary_key
            .as_ref()
            .and_then(|k| k.filegroup.as_ref())
        {
            out.push((format!("the primary key of `{table_name}`"), fg, beside));
        }
        for (n, u) in &table.unique {
            if let Some(fg) = &u.filegroup {
                out.push((
                    format!("unique constraint `{}` of `{table_name}`", n.shown()),
                    fg,
                    beside,
                ));
            }
        }
        for (n, ix) in &table.indexes {
            if let Some(fg) = &ix.filegroup {
                out.push((
                    format!("index `{}` of `{table_name}`", n.shown()),
                    fg,
                    beside,
                ));
            }
        }
    }
    out
}

fn filegroup_problems(
    declared: &[(String, &pbps_model::Filegroup, bool)],
    held: &std::collections::BTreeMap<&str, pbps_mssql::catalog::FilegroupRow>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (what, fg, beside) in declared {
        let name = pbps_model::escape::Escaped(fg.as_str());
        let Some(found) = held.get(fg.as_str()) else {
            problems.push(format!(
                "{what} names the filegroup `{name}`, which this database does not have; pbps \
                 never creates a filegroup, so add it (with its files) first"
            ));
            continue;
        };
        // The engine's lookup is by its collation, which may fold accents,
        // widths or kana the model does not: the model's equality decides, and
        // the lookup only confirms that the filegroup is there. A filegroup
        // found under another spelling would read back as a change forever.
        let stored = pbps_model::Filegroup::new(found.name.as_str());
        if stored != **fg {
            problems.push(format!(
                "{what} names the filegroup `{name}`, but the database's filegroup is `{}`; \
                 declare it as stored",
                pbps_model::escape::Escaped(stored.as_str())
            ));
        } else if found.alike > 1 {
            problems.push(format!(
                "{what} names the filegroup `{name}`, but this database holds several that differ \
                 only in case; pbps compares filegroup names without case and cannot tell them \
                 apart, so rename all but one"
            ));
        } else if found.is_read_only {
            problems.push(format!(
                "{what} names the filegroup `{name}`, which is read-only here, so nothing can \
                 be built on it"
            ));
        } else if found.is_default && !beside {
            problems.push(format!(
                "{what} names `{name}`, the database's default filegroup, which a pull writes \
                 as no `filegroup`: remove the line"
            ));
        }
    }
    problems
}

pub(in crate::engine) async fn refuse_history_references_alike(
    conn: &mut Conn,
    schema: &Schema,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<()> {
    let histories: Vec<&TableName> = schema
        .tables
        .values()
        .filter_map(|t| t.system_time.as_ref()?.versioning.as_ref())
        .map(|v| &v.history)
        .collect();
    // A key to a history as written is the offline check's, already refused.
    let referenced: BTreeSet<&TableName> = schema
        .tables
        .values()
        .flat_map(|t| t.foreign_keys.values())
        .map(|fk| &fk.references_table)
        .filter(|r| !histories.contains(r))
        .collect();
    if histories.is_empty() || referenced.is_empty() {
        return Ok(());
    }
    let names: Vec<TableName> = histories
        .iter()
        .copied()
        .chain(referenced)
        .cloned()
        .collect();
    let alike = pbps_mssql::catalog::object_names_alike(conn, &names).await?;
    let problems = pbps_dialect::history_references_under(schema, dialect, |a, b| {
        a == b
            || alike
                .iter()
                .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    });
    if !problems.is_empty() {
        anyhow::bail!(
            "the declarations have a foreign key the engine refuses (13565):\n  {}",
            problems.join("\n  ")
        );
    }
    Ok(())
}

pub(in crate::engine) fn reconcile_cluster_roles() -> ConnectedCheck {
    ConnectedCheck {
        name: "missing_roles",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server creates and renames database roles through the typed plan; the external cluster-role presence check does not apply".into(),
    }
}

/// The SQL Server body of `engine::account_for_module_dependents`: DEFAULT
/// holders for explicit routine drops, which is all that SQL Server's `CREATE
/// OR ALTER` leaves to schedule, along with the members of the roles the plan
/// drops. It never plans again, so it has no use for `_rediff`.
#[allow(clippy::too_many_arguments)]
pub(in crate::engine) async fn account_for_module_dependents<
    S: pbps_diff::order::RouteState<D>,
    D: crate::emission::Engine,
>(
    conn: &mut Conn,
    changes: PlanningPlan<S>,
    declared: &pbps_model::Schema,
    sources: crate::dependents::Sources<'_>,
    deps: &pbps_model::object_ref::Annotations,
    role_members: Option<&crate::dependents::schedule::RoleMembers>,
    dialect: &D,
    _rediff: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
    let (declarations, read) = defaults::scheduled_inputs(declared, sources, deps, role_members);
    let found = module_dependents(conn, &changes).await?;
    let read = read.with_module_dependents(&found);
    let scheduled = crate::dependents::schedule::schedule(changes, &declarations, &read, dialect)?;
    Ok((
        scheduled.changes,
        ConnectedCheck {
            name: "module_dependents",
            engine: "SQL Server",
            status: if found.values().any(|v| !v.is_empty()) {
                "passed"
            } else {
                "not_applicable"
            },
            message: if found.is_empty() {
                "SQL Server uses CREATE OR ALTER, so a module change drops nothing that depends on it".into()
            } else {
                format!(
                    "{} DEFAULT holder(s) of {} dropped routine(s) accounted for; {} change(s) added",
                    found.values().map(Vec::len).sum::<usize>(),
                    found.len(),
                    scheduled.added
                )
            },
        },
    ))
}

/// The SQL Server body of `engine::check_drop_blockers`, which apply also calls.
pub(in crate::engine) async fn check_drop_blockers(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    _stored: &pbps_model::Schema,
    _stored_ids: &pbps_model::IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let reports = pbps_mssql::impact::key_drop_blockers(conn, &names).await?;
    refuse_drop_reports("SQL Server", &reports)?;
    // Two kinds of question now, and the count says which: a key this
    // plan drops, and a column it retypes while a key stands on it
    // (DECISIONS 515). Classified by the change each report points at
    // rather than by reading its target text, which belongs to the
    // engine that wrote it.
    let retypes = reports
        .iter()
        .filter(|r| {
            matches!(
                changes.changes.get(r.change_index).map(|p| &p.change),
                Some(pbps_model::Change::AlterColumnType { .. })
            )
        })
        .count();
    Ok(ConnectedCheck {
        name: "drop_blockers",
        engine: "SQL Server",
        status: "unavailable",
        message: format!(
            "{} unique-key drop(s) and {retypes} column retype(s) checked; the table/column drop dependency reader is not implemented for SQL Server",
            reports.len() - retypes
        ),
    })
}

/// SQL Server has no partition this model moves rows into.
pub(in crate::engine) fn logical_slots() -> ConnectedCheck {
    ConnectedCheck {
        name: "logical_slots",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server has no partition this model moves rows into".to_owned(),
    }
}

/// SQL Server has no logical decoding this model reads.
pub(in crate::engine) fn numbered_column_fills() -> ConnectedCheck {
    ConnectedCheck {
        name: "numbered_column_fills",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server has no logical decoding this model reads".to_owned(),
    }
}

/// Orders a SQL Server plan's function drops after the computed columns that
/// call them, with what each is schema-bound to after it, and refuses what
/// the catalog's expression edges say the engine will not do (#1431,
/// DEC-1431.1). Reads the edges of the tables and modules the plan touches,
/// under the names the catalog has them by. PostgreSQL has no computed
/// columns, and its generated ones are `release_generated_inputs`'s.
///
/// Returns which drop waits for which by those edges, for the rename search
/// that runs after it ([`order_created_object_names`], #1680): empty where
/// no edge was read.
#[allow(clippy::wildcard_enum_match_arm)]
pub(in crate::engine) async fn order_computed_by_edges<S: Clone>(
    conn: &mut Conn,
    cs: &mut PlanningPlan<S>,
    ids: &pbps_model::IdsFile,
) -> anyhow::Result<Vec<(pbps_model::Change, pbps_model::Change)>> {
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    let catalog = |at, table: &TableName| computed_catalog_table(&names, table, at);
    // The tables and modules whose expression edges are worth reading.
    let objects = computed_edge_objects(&names, cs.changes());
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    // The edges refuse or move only these: a column a computed column of its
    // own table reads, a function one calls, a computed column a module is
    // bound to, and drops `release` orders, which takes two. A plan with none
    // of them, such as one adding a computed column or dropping one view,
    // needs no edge read and so no grant that makes one complete (#1643
    // review).
    let mut inputs: Vec<pbps_model::TableName> = Vec::new();
    let mut drops = 0;
    let mut decides = false;
    for (at, p) in cs.changes.iter().enumerate() {
        match &p.change {
            pbps_model::Change::RenameColumn { table, .. } => inputs.push(catalog(at, table)),
            pbps_model::Change::DropColumn { column, .. }
            | pbps_model::Change::AlterColumnType { column, .. }
            | pbps_model::Change::AlterColumnNullability { column, .. } => {
                inputs.push(catalog(at, &column.table));
            }
            pbps_model::Change::DropComputedColumn { .. } => decides = true,
            pbps_model::Change::AlterModule { module, .. } => {
                decides |= module.kind == pbps_model::ModuleKind::Function;
            }
            pbps_model::Change::DropModule { kind, .. } => {
                decides |= *kind == pbps_model::ModuleKind::Function;
                drops += 1;
            }
            pbps_model::Change::DropTable { .. } => drops += 1,
            _ => {}
        }
    }
    let modules_dropped = cs
        .changes
        .iter()
        .any(|p| matches!(p.change, pbps_model::Change::DropModule { .. }));
    decides |= modules_dropped && drops > 1;
    if !decides {
        decides = pbps_mssql::catalog::may_hold_computed_columns(conn, &inputs)
            .await
            .map_err(|e| {
                anyhow::Error::new(e)
                    .context("cannot read whether a changed column's table has computed columns")
            })?;
    }
    if !decides {
        return Ok(Vec::new());
    }
    // An empty read is "no edge" only where no referrer the pass decides by
    // can be hidden (#1462).
    let mut targets = pbps_mssql::catalog::ReferrerTargets::default();
    for (at, p) in cs.changes.iter().enumerate() {
        match &p.change {
            pbps_model::Change::AlterModule { id, module }
                if module.kind == pbps_model::ModuleKind::Function =>
            {
                targets.functions.push(id.object_name());
            }
            pbps_model::Change::DropModule {
                id,
                kind: pbps_model::ModuleKind::Function,
            } => targets.functions.push(id.object_name()),
            pbps_model::Change::DropComputedColumn { table, name, .. } => {
                targets
                    .computed_columns
                    .push((catalog(at, table), name.clone()));
            }
            _ => {}
        }
    }
    pbps_mssql::catalog::prove_referrers_visible(conn, &targets)
        .await
        .map_err(|e| {
            anyhow::Error::new(e).context(
                "cannot prove the catalog shows everything that calls what this plan changes",
            )
        })?;
    let edges = pbps_mssql::catalog::expression_edges(conn, &objects).await?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    // One group: whether two strings are one name is the collation's, not
    // the kind of object they name.
    let spellings: Vec<String> = crate::computed_order::spellings(cs, &edges)
        .into_iter()
        .collect();
    let grouped: Vec<(usize, String)> = spellings.iter().map(|s| (0, s.clone())).collect();
    let alike = crate::computed_order::Alike::from_pairs(
        pbps_mssql::catalog::column_names_alike(conn, &grouped)
            .await?
            .into_iter()
            .map(|(a, b)| (spellings[a].clone(), spellings[b].clone())),
    );
    crate::computed_order::order_by_edges(cs, ids, &edges, &alike)
        .map_err(|why| anyhow::anyhow!("computed_dependencies (SQL Server): {why}"))?;
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, ids)?;
    Ok(crate::computed_order::drop_precedence(
        cs, &names, &edges, &alike,
    ))
}

/// The SQL Server body of `engine::refuse_added_computed_calls`.
pub(in crate::engine) async fn refuse_added_computed_calls<S>(
    conn: &mut Conn,
    cs: &pbps_diff::order::PlanningPlan<S>,
    declared: &pbps_model::Schema,
) -> anyhow::Result<()> {
    let dialect = pbps_mssql::Mssql;
    let spellings: Vec<String> =
        crate::computed_order::added_call_spellings(cs, declared, &dialect)
            .into_iter()
            .collect();
    if spellings.is_empty() {
        return Ok(());
    }
    let grouped: Vec<(usize, String)> = spellings.iter().map(|s| (0, s.clone())).collect();
    let alike = crate::computed_order::Alike::from_pairs(
        pbps_mssql::catalog::column_names_alike(conn, &grouped)
            .await?
            .into_iter()
            .map(|(a, b)| (spellings[a].clone(), spellings[b].clone()))
            // Successful catalog comparison covers every supplied spelling;
            // the query reports distinct pairs only, not reflexive rows.
            .chain(spellings.iter().map(|s| (s.clone(), s.clone()))),
    );
    crate::computed_order::refuse_added_calls_in(cs, declared, &dialect, &alike)
        .map_err(|why| anyhow::anyhow!("computed_dependencies (SQL Server): {why}"))
}

/// The SQL Server body of `engine::order_created_object_names`.
pub(in crate::engine) async fn order_created_object_names<S: Clone>(
    conn: &mut Conn,
    cs: &mut PlanningPlan<S>,
    label: &str,
    precedence: &[(pbps_model::Change, pbps_model::Change)],
    declared: &crate::dependents::schedule::Declarations<'_>,
) -> anyhow::Result<pbps_dialect::slots::CatalogFacts> {
    // Column renames first: each one may move a generated default, which the
    // `sys.objects` walk below follows in the order they run (DEC-1366.3).
    let columns = crate::object_order::renamed_column_names(cs);
    if !columns.is_empty() {
        let tables: Vec<&TableName> = columns.iter().map(|(t, _)| t).collect();
        let grouped: Vec<(usize, String)> = columns
            .iter()
            .map(|(t, name)| {
                let group = tables.iter().position(|u| *u == t).unwrap_or_default();
                (group, name.raw().to_owned())
            })
            .collect();
        let alike: Vec<_> = pbps_mssql::catalog::column_names_alike(conn, &grouped)
            .await?
            .into_iter()
            .map(|(a, b)| (columns[a].clone(), columns[b].clone()))
            .collect();
        crate::object_order::order_column_renames(cs, &alike, label)?;
    }
    let index = pbps_model::plan_names::PlanNames::of(&cs.changes, declared.sources.after)?;
    let (mut names, parents) = crate::deploy::object_reads(cs, &index);
    let mut occupants = pbps_mssql::catalog::object_name_occupants(conn, &names, &parents).await?;
    // Where a transfer carries each child of a moved table: a name the
    // first read could not know to ask until it found the child (#1366).
    let carried: Vec<TableName> = crate::deploy::carried_destinations(cs, &occupants)
        .into_iter()
        .filter(|n| !names.contains(n))
        .collect();
    if !carried.is_empty() {
        occupants.extend(pbps_mssql::catalog::object_name_occupants(conn, &carried, &[]).await?);
        names.extend(carried);
    }
    // An absent row is a free name only where nothing can be hidden
    // from this login (#1192), so only the names the read found no
    // row at need the proof. A name holds one object per schema, so
    // one this login sees leaves no room for a hidden one: a table
    // dropped and recreated at its own name is not asked. Nor are
    // `parents`, read for what the walk moves or removes with a
    // table, so a drop-only plan claims nothing to prove (review of
    // #1360).
    let mut schemas: Vec<String> = names
        .iter()
        .filter(|n| !occupants.iter().any(|o| &o.wanted == *n))
        .map(|n| n.schema.clone())
        .collect();
    schemas.sort();
    schemas.dedup();
    pbps_mssql::catalog::prove_schemas_visible(conn, &schemas)
        .await
        // As context, never interpolated: a driver frame has to reach
        // `ledger_safe_reason` intact to be redacted (DECISIONS 455).
        .map_err(|e| {
            anyhow::Error::new(e).context("cannot prove the names this plan creates are free")
        })?;
    // Which of the plan's own names are one under the database's
    // collation (#1215).
    let mut candidates = crate::deploy::alike_candidates(cs, &names, &occupants);
    // Computed-column reads may be skipped on a module-only plan. Extend this
    // existing comparison, whose answers the final observer already carries.
    candidates.extend(pbps_mssql::schema_bound_call_names(
        cs,
        declared.sources.before.schema,
        declared.schema,
    ));
    candidates.sort();
    candidates.dedup();
    let alike = pbps_mssql::catalog::object_names_alike(conn, &candidates).await?;
    let facts = object_slots(&occupants, &candidates, &alike);
    // Name equality can reveal a dependency that the earlier lexical passes
    // could not schedule (#2221). Settle it before the final name walk and
    // observer, using the same comparison and existing release mechanism.
    *cs = pbps_diff::order::catalog_requirements(
        cs.clone(),
        declared.sources.before,
        pbps_diff::Side {
            schema: declared.schema,
            ids: declared.sources.after,
        },
        &pbps_mssql::Mssql,
        &declared.module_deps(),
        &facts,
        precedence,
    )
    .map_err(anyhow::Error::msg)?;
    crate::object_order::order_occupied_objects_under(cs, &occupants, &alike, label, precedence)?;
    Ok(facts)
}

/// SQL Server keeps a replaced module's state with `CREATE OR ALTER`, so a
/// staged run needs no transaction for it.
pub(in crate::engine) fn require_transactional_rebuilds(
    changes: &ChangeSet,
    staged: bool,
) -> anyhow::Result<()> {
    defaults::staged_rebuilds(changes, staged, || Ok(()))
}

/// SQL Server's `CREATE OR ALTER` retains module state, so the PostgreSQL
/// drop and recreate check has nothing to ask.
pub(in crate::engine) fn check_module_rebuilds() -> ConnectedCheck {
    ConnectedCheck {
        name: "before_a_rebuild",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server uses CREATE OR ALTER to retain module state; the PostgreSQL drop/recreate check does not apply".into(),
    }
}

pub(in crate::engine) async fn role_members(
    conn: &mut Conn,
) -> anyhow::Result<BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>> {
    Ok(pbps_mssql::catalog::role_members(conn).await?)
}

/// The SQL Server body of `operational_cost`.
pub(in crate::engine) async fn operational_cost(
    conn: &mut Conn,
    cs: &ChangeSet,
    ids: &IdsFile,
) -> crate::cost::CostReport {
    use crate::cost::{ChangeCost, CostReport, Rows};
    let names = match pbps_model::plan_names::PlanNames::of(&cs.changes, ids) {
        Ok(names) => names,
        Err(error) => {
            return CostReport::Unavailable {
                engine: profile::profile(conn.driver()).cost_report_engine,
                reason: format!("operational_cost could not establish plan identities: {error}"),
            };
        }
    };
    use pbps_mssql::estimate as ms;
    let mut estimates = ms::planned_estimates(&names).into_iter().peekable();
    if estimates.peek().is_none() && !cs.changes.is_empty() {
        return CostReport::Unavailable {
            engine: "sqlserver",
            reason: "operational_cost currently measures SQL Server column type and nullability changes and clustered index changes; this plan contains none".to_owned(),
        };
    }
    let mut changes = Vec::with_capacity(cs.changes.len());
    for change_index in 0..cs.changes.len() {
        let Some((_, mut e)) = estimates.next_if(|(index, _)| *index == change_index) else {
            changes.push(ChangeCost::Unavailable {
                change_index,
                reason: "operational_cost has no measurement for this SQL Server change".to_owned(),
            });
            continue;
        };
        if let Err(error) = ms::against(conn, &mut e).await {
            changes.push(ChangeCost::Unavailable {
                change_index,
                reason: format!(
                    "operational_cost could not read SQL Server catalog context: {error}"
                ),
            });
            continue;
        }
        changes.push(ChangeCost::Available {
            change_index,
            about: e.about,
            table: e.table.raw().to_string(),
            rewrite: e.rewrite.into(),
            reads: e.reads.into(),
            lock: e.lock.to_owned(),
            blocks: e.blocks.to_owned(),
            also_locks: Vec::new(),
            rows: match e.rows {
                Some(count) => Rows::Estimated { count },
                None => Rows::Unknown {
                    reason: e
                        .rows_unknown
                        .unwrap_or_else(|| "no catalog row estimate is available".to_owned()),
                },
            },
        });
    }
    CostReport::Available {
        engine: "sqlserver",
        changes,
    }
}

/// SQL Server permissions are validated by its dialect; the PostgreSQL
/// permission-version check does not apply.
pub(in crate::engine) fn permission_support() -> ConnectedCheck {
    ConnectedCheck {
        name: "permission_support",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server permissions are validated by its dialect; the PostgreSQL \
                  permission-version check does not apply"
            .to_owned(),
    }
}

/// This model holds generated columns for PostgreSQL only.
pub(in crate::engine) fn generation_support() -> ConnectedCheck {
    ConnectedCheck {
        name: "generation_support",
        engine: "SQL Server",
        status: "not_applicable",
        message: "this model holds generated columns for PostgreSQL only".to_owned(),
    }
}

/// SQL Server grants to an object's owner like any other principal, and its
/// reader carries no object owner. The owner map is empty on a reader that
/// carries none, so the loop would find nothing anyway — named so that "this
/// engine was not asked" reads differently from "this engine was asked and
/// said no".
pub(in crate::engine) fn owned_targets() -> ConnectedCheck {
    ConnectedCheck {
        name: "owned_targets",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server grants to an object's owner like any other principal, \
                  and this reader carries no object owner; the PostgreSQL check for a \
                  grant no `REVOKE` could ever undo does not apply"
            .to_owned(),
    }
}

/// SQL Server's `REVOKE` names its grantor with `AS`, and its reader carries
/// no unrevocable grant.
pub(in crate::engine) fn unrevocable_grants() -> ConnectedCheck {
    ConnectedCheck {
        name: "unrevocable_grants",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server's `REVOKE` names its grantor with `AS`, and this reader \
                  carries no unrevocable grant; the PostgreSQL check for a grant this \
                  connection could not take away does not apply"
            .to_owned(),
    }
}

/// The SQL Server body of `edition_verdict`, which apply also calls.
pub(in crate::engine) async fn edition_verdict(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> Result<EditionVerdict, DbError> {
    let edition = pbps_mssql::edition::edition(conn).await?;
    Ok(EditionVerdict {
        refused_online: pbps_mssql::edition::online_not_supported(changes, &edition),
        runs: edition.name().to_owned(),
        warnings: pbps_mssql::edition::size_of_data_warnings(changes, &edition),
    })
}

/// The SQL Server body of `unsupported_temporal`.
pub(in crate::engine) async fn unsupported_temporal(
    conn: &mut Conn,
    changes: &ChangeSet,
) -> anyhow::Result<Vec<String>> {
    if !pbps_mssql::temporal::needs_the_question(changes) {
        return Ok(Vec::new());
    }
    if pbps_mssql::temporal::has_history_retention(conn).await? {
        return Ok(Vec::new());
    }
    let versioned = if pbps_mssql::temporal::needs_the_catalog(changes) {
        pbps_mssql::temporal::system_versioned_tables(conn).await?
    } else {
        Default::default()
    };
    Ok(pbps_mssql::temporal::refused_without_retention(
        changes, &versioned,
    ))
}

/// What SQL Server carries from one position of a plan to a later one: only the
/// site admission every engine's state holds, which its sites never admit
/// anything into. Its misspelling probe asks nothing of a type-routine review.
#[derive(Default)]
pub(crate) struct PlanState {
    admission: SiteAdmission,
}

impl AsRef<SiteAdmission> for PlanState {
    fn as_ref(&self) -> &SiteAdmission {
        &self.admission
    }
}

impl AsMut<SiteAdmission> for PlanState {
    fn as_mut(&mut self) -> &mut SiteAdmission {
        &mut self.admission
    }
}

impl PlanSession for pbps_mssql::Mssql {
    type State = PlanState;

    async fn compare_dependencies(
        &self,
        conn: &mut Conn,
        declarations: &pbps_model::dependency::Dependencies,
    ) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
        Ok(pbps_mssql::dependencies::compare(conn, declarations).await?)
    }

    async fn seal_declared_type_pins(
        &self,
        _state: &mut PlanState,
        _conn: &mut Conn,
        _declared: &Schema,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<()> {
        // The sealing step, then the routine read it feeds: the order of the
        // two refusals is the order of the two steps.
        let pins = seal_type_pins(trusted)?;
        type_input_routines(&pins.types, pin_routines)?;
        Ok(())
    }

    async fn external_role_renames(
        &self,
        _conn: &mut Conn,
        _recorded: &IdsFile,
        _declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        Ok(external_role_renames())
    }

    async fn misspelt(
        &self,
        _state: &PlanState,
        conn: &mut Conn,
        schema: &Schema,
        at: &CatalogNames,
    ) -> Result<Spellings, RowsError> {
        pbps_mssql::catalog::misspelt(conn, schema, at).await
    }

    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError> {
        pbps_mssql::catalog::schema_spellings(conn, names).await
    }

    async fn refuse_unknown_collations(
        &self,
        conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        refuse_unknown_collations(conn, declared).await
    }

    async fn refuse_history_references_alike(
        &self,
        conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        refuse_history_references_alike(conn, declared, self).await
    }

    async fn refuse_unknown_filegroups(
        &self,
        conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        refuse_unknown_filegroups(conn, declared).await
    }

    fn reconcile_cluster_roles(
        &self,
        _recorded: &IdsFile,
        _queried: &IdsFile,
        _renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
        _live: &Schema,
        _expected: &mut Schema,
    ) -> anyhow::Result<ConnectedCheck> {
        Ok(reconcile_cluster_roles())
    }

    /// Adds to `hints` the facts about the target that planning a type or an
    /// extension change needs and the differ cannot see.
    ///
    /// SQL Server has no such types or extensions, so it adds nothing.
    async fn type_rebuild_hints(
        &self,
        _conn: &mut Conn,
        _base: &Schema,
        _declared: &Schema,
        _recorded_ids: &IdsFile,
        _resolved_ids: &IdsFile,
        _hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn computed_column_screen(&self) -> pbps_diff::Screen {
        computed_column_screen(Driver::Mssql)
    }

    /// Reads the `NOT VALID` checks of the domains standing in the database.
    ///
    /// SQL Server has no domains, so it answers none.
    async fn standing_not_valid_domain_checks(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<Vec<StandingNotValid>> {
        Ok(Vec::new())
    }

    async fn seal_plan_type_review(
        &self,
        _conn: &mut Conn,
        _base: pbps_diff::Side<'_>,
        _desired: pbps_diff::Side<'_>,
        _changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<bool> {
        let pins = seal_type_pins(trusted)?;
        Ok(!type_input_routines(&pins.types, pin_routines)?.is_empty())
    }

    /// SQL Server's namespace is asked after the passes of the connected
    /// plan instead, with the walk that also orders it (#1077).
    fn refuse_occupied_relation_names(
        &self,
        _changes: &ChangeSet,
        _scoped: &pbps_diff::Scoped,
        _unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
        _unmanaged_relations: &[TableName],
        _label: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn require_transactional_rebuilds(
        &self,
        changes: &ChangeSet,
        staged: bool,
    ) -> anyhow::Result<()> {
        require_transactional_rebuilds(changes, staged)
    }

    /// SQL Server has no `UNLOGGED` table and no relation namespace this
    /// position reads: its names are walked once the plan is ordered
    /// ([`Self::order_by_catalog`]).
    async fn check_names_against_catalog<S: Clone>(
        &self,
        _state: &mut PlanState,
        _conn: &mut Conn,
        _changes: &mut PlanningPlan<S>,
        _frame: Frame<'_>,
        _staged: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn read_before_schedule(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        _declared: &Schema,
        _recorded_ids: &IdsFile,
    ) -> anyhow::Result<ScheduleReads> {
        // A role this plan drops still has this environment's members, which
        // the engine will not drop it over. They are read here, for the
        // schedule to write into the plan by name, so the artifact the gate
        // approves lists exactly who is removed (ADR-0005) — never found
        // again at apply time, where nobody would have reviewed them.
        // Without a role drop there is nothing to ask. SQL Server has no
        // enum to rebuild.
        let role_members = if defaults::drops_a_role(changes) {
            Some(
                role_members(conn)
                    .await
                    .map_err(|e| e.context("cannot read the role memberships"))?,
            )
        } else {
            None
        };
        Ok(ScheduleReads {
            role_members,
            enum_rebuild: BTreeSet::new(),
        })
    }

    async fn schedule<S: pbps_diff::order::RouteState<Self>>(
        &self,
        _state: &mut PlanState,
        conn: &mut Conn,
        changes: PlanningPlan<S>,
        frame: Frame<'_>,
        role_members: Option<&RoleMembers>,
        again: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
    ) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
        anyhow::ensure!(
            !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::RebuildPartitionedTable(_))),
            "{TABLE_REBUILD_REFUSAL}"
        );
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

    async fn order_by_catalog<S: Clone>(
        &self,
        _state: &mut PlanState,
        conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        frame: Frame<'_>,
    ) -> anyhow::Result<Ordered> {
        // A computed column's function drops after it, by the catalog's
        // edges (DEC-1431.1): before the rename walk, which moves a module
        // drop only between the drops these edges order it among (#1680).
        let precedence = order_computed_by_edges(conn, changes, frame.written.ids).await?;
        // What an added computed column calls has no edge yet; its names are
        // compared under the collation instead (#1459).
        refuse_added_computed_calls(conn, changes, frame.declared.schema).await?;
        // Last of the passes that order the plan, so the order it settles
        // from the catalog is the one checked below and saved (#1366).
        let facts = order_created_object_names(
            conn,
            changes,
            frame.label,
            &precedence,
            &crate::dependents::schedule::Declarations {
                schema: frame.written.schema,
                sources: crate::dependents::Sources {
                    before: frame.recorded,
                    after: frame.written.ids,
                },
                annotations: &frame.hints.annotations,
            },
        )
        .await?;
        Ok(Ordered {
            verified: false,
            facts: Some(facts),
        })
    }

    async fn check_final_order(
        &self,
        _state: &PlanState,
        conn: &mut Conn,
        changes: &ChangeSet,
        frame: Frame<'_>,
    ) -> anyhow::Result<FinalOrderChecks> {
        let rebuilds = check_module_rebuilds();
        let drop_blockers = check_drop_blockers(
            conn,
            changes,
            frame.written.ids,
            frame.recorded.schema,
            frame.recorded.ids,
        )
        .await?;
        // SQL Server keeps its existing reference-data execution policy, so
        // no guard is prepared for the rows a plan writes.
        Ok(FinalOrderChecks {
            rebuilds,
            drop_blockers,
            logical_slots: logical_slots(),
            numbered_column_fills: numbered_column_fills(),
        })
    }

    async fn vet_plan_sites(
        &self,
        state: &PlanState,
        conn: &mut Conn,
        names: &pbps_model::plan_names::PlanNames<'_>,
        _on_note: &mut dyn FnMut(String),
    ) -> anyhow::Result<()> {
        // SQL Server vets the requirement a drop is guarded by. It records no
        // conversion's expression and binds no table expression this way.
        vet_drop_requirements(conn, &state.admission, names).await
    }

    /// SQL Server has neither enums nor domains, so no row is stranded.
    async fn probe_stranded_rows(
        &self,
        _state: &PlanState,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _frame: Frame<'_>,
        _progress: Option<&crate::deploy::services::Progress>,
    ) -> anyhow::Result<Option<anyhow::Result<()>>> {
        Ok(None)
    }

    async fn support_rows(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _ids: &IdsFile,
        _reads: SupportReads<'_>,
    ) -> anyhow::Result<SupportRows> {
        Ok(SupportRows {
            permission_support: permission_support(),
            generation_support: generation_support(),
            owned_targets: owned_targets(),
            unrevocable_grants: unrevocable_grants(),
        })
    }

    async fn server_limits(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<ServerLimits> {
        // The edition is a connection-time fact, and it is the only place the
        // two edition-dependent questions of ADR-0003 can be answered
        // honestly: whether ONLINE will be accepted at all, and whether an
        // addition that is metadata-only on Enterprise rewrites every row
        // here. An offline plan has to assume the conservative answer.
        let edition = edition_verdict(conn, changes).await?;
        // A server fact like the edition: 2016 has no history retention and
        // no cascading key from a system-versioned table (#1502). Answered,
        // it is a finding, as the edition's is; a failure to ask stays an
        // error (#1630).
        let temporal = unsupported_temporal(conn, changes).await?;
        Ok(ServerLimits { edition, temporal })
    }

    async fn operational_cost(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> crate::cost::CostReport {
        operational_cost(conn, changes, ids).await
    }

    async fn seal_plan_facts(
        &self,
        state: &PlanState,
        conn: &mut Conn,
        project: &pbps_config::Project,
        target: &crate::db::Target,
        _frame: Frame<'_>,
        _changes: &ChangeSet,
        managed: &BTreeSet<pbps_model::ModuleId>,
    ) -> anyhow::Result<SealedFacts> {
        // The sealing step refuses `trusted_roles`; the routine pins that
        // follow refuse `pin_type_routines` in their own words.
        let Prerequisites {
            types: type_pins,
            providers: provider_pins,
        } = seal_type_pins(&project.config.trusted_roles)?;
        let reviewed =
            crate::pins::seal_selected(conn, project, target, &type_pins, &state.admission).await?;
        let routine_pins = crate::pins::seal(conn, project, target, managed).await?;
        Ok(SealedFacts {
            type_pins,
            provider_pins,
            reviewed,
            routine_pins,
        })
    }

    /// SQL Server has no domains.
    async fn declared_domain_code(
        &self,
        _scratch: Option<&mut crate::resolution::Entry<'_>>,
        _conn: &mut Conn,
        _target: &crate::db::Target,
        _project: &pbps_config::Project,
        _desired: pbps_diff::Side<'_>,
        _changes: &ChangeSet,
    ) -> anyhow::Result<DomainCode> {
        Ok(DomainCode::NotAsked)
    }
}

#[cfg(test)]
mod filegroup_tests {
    use super::*;
    use pbps_mssql::catalog::FilegroupRow;
    use std::collections::BTreeMap;

    /// What the engine answers for a declared name, which it matches under
    /// the database's collation: a name it does not match is simply absent.
    fn held(answers: &[(&'static str, bool, bool, usize)]) -> BTreeMap<&'static str, FilegroupRow> {
        answers
            .iter()
            .map(|&(asked, is_default, is_read_only, alike)| {
                let name = if asked.eq_ignore_ascii_case("primary") {
                    "PRIMARY"
                } else {
                    asked
                };
                (
                    asked,
                    FilegroupRow {
                        name: name.to_owned(),
                        is_default,
                        is_read_only,
                        alike,
                    },
                )
            })
            .collect()
    }

    fn problems_with(schema: &Schema, answers: &[(&'static str, bool, bool, usize)]) -> String {
        filegroup_problems(&declared_filegroups(schema), &held(answers)).join("\n")
    }

    fn problems(schema: &Schema) -> String {
        problems_with(
            schema,
            &[
                ("PRIMARY", true, false, 1),
                ("primary", true, false, 1),
                ("fgx", false, false, 1),
                ("FGX", false, false, 1),
                ("frozen", false, true, 1),
            ],
        )
    }

    fn table_with(table: Option<&str>, index: Option<&str>) -> Schema {
        let mut t = pbps_model::Table {
            filegroup: table.map(pbps_model::Filegroup::new),
            ..Default::default()
        };
        t.indexes.insert(
            "ix".into(),
            pbps_model::Index {
                columns: Vec::new(),
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: index.map(pbps_model::Filegroup::new),
            },
        );
        let mut schema = Schema::default();
        schema.tables.insert("dbo.t".parse().unwrap(), t);
        schema
    }

    /// A declared filegroup must be one the database holds, writable, and not
    /// the default where it would read back as none (#2813, DEC-2813.1).
    #[test]
    fn a_filegroup_the_database_lacks_or_cannot_write_is_refused_by_name() {
        assert_eq!(problems(&table_with(Some("fgx"), Some("FGX"))), "");
        assert_eq!(problems(&table_with(None, None)), "");
        let said = problems(&table_with(Some("nope"), Some("frozen")));
        assert!(
            said.contains(
                "table `dbo.t` names the filegroup `nope`, which this database does not have"
            ),
            "{said}"
        );
        assert!(
            said.contains("index `ix` of `dbo.t` names the filegroup `frozen`, which is read-only"),
            "{said}"
        );
        // Negative: nothing is said of a filegroup that is fine.
        assert!(!problems(&table_with(Some("nope"), Some("fgx"))).contains("`fgx`"));
    }

    /// The default filegroup reads back as none, so naming it on the table, or
    /// on an object whose table is on the default too, never converges; under
    /// a table elsewhere it is the only way to say where the object is.
    #[test]
    fn the_default_filegroup_is_refused_unless_the_table_is_elsewhere() {
        let said = problems(&table_with(Some("PRIMARY"), None));
        assert!(said.contains("the database's default filegroup"), "{said}");
        let said = problems(&table_with(None, Some("primary")));
        assert!(
            said.contains("index `ix` of `dbo.t` names `primary`, the database's default"),
            "{said}"
        );
        assert_eq!(problems(&table_with(Some("fgx"), Some("PRIMARY"))), "");
    }

    /// The engine matches the name under the database's collation, so a name
    /// it does not return is missing here even when it differs from a held
    /// one only in case (a case-sensitive database refuses `ON [fgx]` for
    /// `FGX`, measured), and one it does return is accepted (a
    /// case-insensitive database accepts it). The live test pins the engine's
    /// side of this.
    #[test]
    fn a_name_the_engine_does_not_match_is_missing_whatever_its_case() {
        let case_sensitive = problems_with(
            &table_with(Some("fgx"), Some("FGX")),
            &[("FGX", false, false, 1)],
        );
        assert!(
            case_sensitive.contains(
                "table `dbo.t` names the filegroup `fgx`, which this database does not have"
            ),
            "{case_sensitive}"
        );
        assert!(!case_sensitive.contains("`FGX`"), "{case_sensitive}");
        // Negative: the same declaration, matched, is accepted.
        let case_insensitive = problems_with(
            &table_with(Some("fgx"), Some("FGX")),
            &[("fgx", false, false, 1), ("FGX", false, false, 1)],
        );
        assert_eq!(case_insensitive, "");
    }

    /// The engine's lookup follows the database's collation, which may fold
    /// accents or widths the model does not, so a filegroup it returns under
    /// another spelling is refused by both names; the model's equality (ASCII
    /// case only) decides, and the lookup only confirms.
    #[test]
    fn a_filegroup_the_engine_found_under_another_spelling_is_refused_as_stored() {
        let stored = |asked: &'static str, name: &str| {
            let mut held = BTreeMap::new();
            held.insert(
                asked,
                FilegroupRow {
                    name: name.to_owned(),
                    is_default: false,
                    is_read_only: false,
                    alike: 1,
                },
            );
            filegroup_problems(&declared_filegroups(&table_with(Some(asked), None)), &held)
                .join("\n")
        };
        let said = stored("cafe", "café");
        assert!(
            said.contains(
                "table `dbo.t` names the filegroup `cafe`, but the database's filegroup is `café`; declare it as stored"
            ),
            "{said}"
        );
        // Negative: the same spelling, or one that differs only in ASCII case,
        // is the model's own equality and passes.
        assert_eq!(stored("café", "café"), "");
        assert_eq!(stored("FGX", "fgx"), "");
        // Negative: a non-ASCII case difference is a different name to the model.
        assert!(stored("éast", "Éast").contains("declare it as stored"));
    }

    /// A case-sensitive database may hold `fgx` and `FGX`; the model, which
    /// compares names without case, cannot tell an object on one from an
    /// object on the other, so naming either is refused rather than left to
    /// go unseen at the next change.
    #[test]
    fn filegroups_differing_only_in_case_are_refused_by_name() {
        let said = problems_with(&table_with(Some("fgx"), None), &[("fgx", false, false, 2)]);
        assert!(
            said.contains("names the filegroup `fgx`, but this database holds several that differ only in case"),
            "{said}"
        );
        // Negative: a filegroup that is alone under its name is fine.
        assert_eq!(
            problems_with(&table_with(Some("fgx"), None), &[("fgx", false, false, 1)]),
            ""
        );
    }
}
