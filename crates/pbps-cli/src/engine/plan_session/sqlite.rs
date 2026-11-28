//! SQLite's positions of a connected plan (#2950).
//!
//! Implements the 7 required methods of [`PlanSession`], the 4 non-catalog plan
//! gates (`require_transactional_rebuilds`, `vet_plan_sites`, `support_rows`,
//! `server_limits`), the 3 name-namespace methods
//! (`refuse_occupied_relation_names`, `check_names_against_catalog`,
//! `order_by_catalog`), connected view-dependent scheduling (`schedule`), and
//! the post-ordering drop-blocker and final-order checks (`check_final_order`).

use super::*;
use pbps_dialect::Dialect as _;
use pbps_sqlite::catalog::inventory::{self, Inventory};
use pbps_sqlite::dialect::Sqlite;

/// What SQLite carries from one position of a plan to a later one: the site
/// admission (empty because SQLite has no stored routines) and the raw catalog
/// inventory read in [`PlanSession::check_names_against_catalog`] for reuse by
/// [`PlanSession::order_by_catalog`].
#[derive(Default)]
pub(crate) struct PlanState {
    admission: RoutineAdmission,
    inventory: Option<Inventory>,
}

impl AsRef<RoutineAdmission> for PlanState {
    fn as_ref(&self) -> &RoutineAdmission {
        &self.admission
    }
}

impl AsMut<RoutineAdmission> for PlanState {
    fn as_mut(&mut self) -> &mut RoutineAdmission {
        &mut self.admission
    }
}

/// The SQLite connection inside `conn`, or a refusal: a session handed another
/// engine's connection is a routing bug and never an empty database.
pub(in crate::engine) fn sqlite_conn(
    conn: &mut Conn,
) -> Result<&mut pbps_db::sqlite::Connection, DbError> {
    conn.sqlite().ok_or_else(|| {
        DbError::Refused(
            "the SQLite session was given a connection that is not to a SQLite file".into(),
        )
    })
}

/// Why role-management questions refuse on SQLite rather than answer empty.
///
/// SQLite has no roles or users at all (`Dialect::manages_roles()` is `false`),
/// so its dialect never plans `CREATE ROLE`, `RENAME ROLE` or `DROP ROLE`. Each
/// caller asks these questions only to clear one of those statements; answering
/// empty would read as "owns nothing / no members / no conflict" for a plan
/// that was not built for SQLite.
pub(in crate::engine) fn sqlite_has_no_roles(question: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{question} is a question about a role this tool creates, renames or drops, and SQLite \
         has no roles: this dialect plans none of those statements, so a plan that asks this was \
         not built for SQLite"
    )
}

/// Shared connection, library, ledger and `strategy: online` preflight for
/// both [`PlanSession::server_limits`] (`Stage::Plan`) and
/// [`ApplySession::preflight_limits`] (`Stage::Apply`), so the two gates
/// cannot drift.
pub(in crate::engine) fn preflight_verdict(
    dialect: &Sqlite,
    conn: &mut Conn,
    changes: &ChangeSet,
    stage: pbps_sqlite::preflight::Stage,
) -> anyhow::Result<EditionVerdict> {
    let c = sqlite_conn(conn)?;
    let online: Vec<String> = changes
        .changes
        .iter()
        .filter(|p| p.strategy.online)
        .map(|p| pbps_model::escape::Escaped(&p.change.subject()).to_string())
        .collect();
    if !online.is_empty() {
        anyhow::bail!(
            "SQLite does not support `strategy: online` (requested for {}); remove \
             `strategy: online` and plan again",
            online.join(", ")
        );
    }
    let verdict = pbps_sqlite::preflight::before_apply(c, changes, dialect.floor(), stage)?;
    if !verdict.problems.is_empty() {
        anyhow::bail!("{}", pbps_sqlite::preflight::refusal(&verdict.problems));
    }
    Ok(EditionVerdict {
        refused_online: Vec::new(),
        runs: format!("SQLite {}", c.version()?),
        warnings: verdict.notes,
    })
}

/// Does not exist on this engine (partitioned-table rebuilds) plus SQLite's
/// staged-view-edit refusal: SQLite replaces a view with `DROP VIEW` followed
/// by `CREATE VIEW` rather than `CREATE OR ALTER`, so a staged run would leave
/// the view absent between two commits, while SQLite runs DDL in a transaction
/// so an unstaged apply is atomic. Shared with [`ApplySession::rebuild_mode`]
/// so plan and apply cannot drift.
pub(in crate::engine) fn require_transactional_rebuilds(
    changes: &ChangeSet,
    staged: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !changes.changes.iter().any(|p| matches!(
            p.change,
            pbps_model::Change::Partition(pbps_model::PartitionChange::RebuildPartitionedTable(_))
        )),
        "SQLite does not support partitioned-table rebuilds"
    );
    defaults::staged_rebuilds(changes, staged, || {
        anyhow::ensure!(
            !staged || rebuilt_modules(changes).is_empty(),
            "SQLite replaces a view with DROP VIEW and CREATE VIEW, so a staged run would leave \
             the view absent between two commits; SQLite runs DDL in a transaction, so apply \
             without --staged"
        );
        Ok(())
    })
}

/// Does not exist on this engine: SQLite has no system-versioned tables, so
/// a plan that creates one was not built for SQLite. Asked of the table
/// itself and not through SQL Server's `temporal::needs_the_question`, which
/// is also true of every cascading foreign key (a SQL Server 2016 limit) and
/// would refuse an ordinary SQLite table. Shared with
/// [`ApplySession::refuse_unsupported_temporal`] so plan and apply cannot
/// drift.
pub(in crate::engine) fn refuse_system_versioned(changes: &ChangeSet) -> anyhow::Result<()> {
    anyhow::ensure!(
        !changes.changes.iter().any(|p| matches!(
            &p.change,
            pbps_model::Change::Table(pbps_model::TableChange::CreateTable { table, .. })
                if table.system_time.is_some()
        )),
        "SQLite does not support system-versioned tables"
    );
    Ok(())
}

/// Collects every dropped or rebuilt view in `changes` (refusing non-view
/// modules on SQLite) and resolves its catalog readers from `inventory` through
/// [`inventory::view_dependents`]. Shared by [`PlanSession::schedule`] and
/// [`ApplySession::check_module_dependency_facts`].
pub(in crate::engine) fn module_dependents_from(
    inventory: &Inventory,
    changes: &ChangeSet,
    known_modules: &BTreeSet<pbps_model::ModuleId>,
) -> anyhow::Result<crate::dependents::schedule::ModuleDependents> {
    let dropped = crate::dependents::dropped_modules(changes);
    let mut views = Vec::with_capacity(dropped.len());
    for (id, kind) in dropped {
        match kind {
            pbps_model::ModuleKind::View => views.push(id),
            pbps_model::ModuleKind::Procedure => {
                anyhow::bail!("SQLite does not support procedures (`{}`)", id.shown())
            }
            pbps_model::ModuleKind::Function => {
                anyhow::bail!(
                    "SQLite does not support stored functions (`{}`)",
                    id.shown()
                )
            }
            pbps_model::ModuleKind::Trigger => {
                anyhow::bail!(
                    "SQLite does not support managed triggers yet (`{}`)",
                    id.shown()
                )
            }
        }
    }
    Ok(inventory::view_dependents(
        inventory,
        &views,
        known_modules,
    )?)
}

pub(in crate::engine) fn known_module_ids<'a>(
    changes: &ChangeSet,
    schemas: impl IntoIterator<Item = &'a Schema>,
) -> BTreeSet<pbps_model::ModuleId> {
    let mut known: BTreeSet<pbps_model::ModuleId> = schemas
        .into_iter()
        .flat_map(|s| s.modules.keys().cloned())
        .collect();
    for p in &changes.changes {
        if let pbps_model::Change::Module(
            pbps_model::ModuleChange::CreateModule { id, .. }
            | pbps_model::ModuleChange::AlterModule { id, .. }
            | pbps_model::ModuleChange::DropModule { id, .. },
        ) = &p.change
        {
            known.insert(id.clone());
        }
    }
    known
}

/// Runs SQLite's catalog drop-blocker check inside the caller's transaction;
/// shared by [`PlanSession::check_final_order`] and [`ApplySession::drop_blockers`].
pub(in crate::engine) fn check_drop_blockers(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &IdsFile,
    stored: &Schema,
    stored_ids: &IdsFile,
) -> anyhow::Result<ConnectedCheck> {
    let c = sqlite_conn(conn)?;
    if !inventory::has_drop_blocker_target(changes) {
        return Ok(defaults::not_applicable(
            "SQLite",
            "drop_blockers",
            "the plan drops and retypes nothing a dependency could block",
        ));
    }
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
    let sources = pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)?;
    let inv = inventory::read_for_drops(c, &sources)?;
    let reports = inventory::drop_blockers(&inv, &sources)?;
    refuse_drop_reports("SQLite", &reports)?;
    Ok(ConnectedCheck {
        name: "drop_blockers",
        engine: "SQLite",
        status: "passed",
        message: format!(
            "{} table/column/index drop(s) checked against current SQLite catalog dependencies",
            reports.len()
        ),
    })
}

fn ensure_inventory<'a>(
    state: &'a mut PlanState,
    conn: &mut Conn,
    changes: &ChangeSet,
    frame: Frame<'_>,
) -> anyhow::Result<&'a Inventory> {
    let c = sqlite_conn(conn)?;
    let needed = inventory::column_tables(changes, frame.recorded, frame.written);
    let covered = state.inventory.as_ref().is_some_and(|inv| {
        needed
            .iter()
            .all(|t| inv.columns.keys().any(|k| pbps_sqlite::ident::same(k, t)))
    });
    if !covered {
        let refs: Vec<&str> = needed.iter().map(String::as_str).collect();
        state.inventory = Some(inventory::read(c, &refs)?);
    }
    Ok(state.inventory.as_ref().expect("populated above"))
}

fn order_and_check_names<S: Clone>(
    dialect: &Sqlite,
    inventory: &Inventory,
    changes: &mut PlanningPlan<S>,
    frame: Frame<'_>,
) -> anyhow::Result<pbps_dialect::slots::CatalogFacts> {
    anyhow::ensure!(
        !crate::deploy::leaves_something_unlogged(changes),
        "SQLite does not support unlogged tables"
    );
    let cols = crate::object_order::renamed_column_names(changes);
    if !cols.is_empty() {
        let mut alike = Vec::new();
        for i in 0..cols.len() {
            for j in (i + 1)..cols.len() {
                if cols[i].0 == cols[j].0
                    && pbps_sqlite::ident::same(cols[i].1.raw(), cols[j].1.raw())
                {
                    alike.push((cols[i].clone(), cols[j].clone()));
                }
            }
        }
        crate::object_order::order_column_renames(changes, &alike, frame.label)?;
    }
    let facts =
        inventory::catalog_facts(inventory, changes, frame.recorded, frame.written, dialect)
            .map_err(anyhow::Error::msg)?;
    order_name_handovers(changes, frame, dialect, &facts)?;
    inventory::verify_names(
        inventory,
        changes,
        frame.recorded,
        frame.written,
        dialect,
        &facts,
        frame.label,
    )
    .map_err(anyhow::Error::msg)?;
    Ok(facts)
}

fn order_name_handovers<S: Clone>(
    changes: &mut PlanningPlan<S>,
    frame: Frame<'_>,
    dialect: &Sqlite,
    facts: &pbps_dialect::slots::CatalogFacts,
) -> anyhow::Result<()> {
    let mut schedule = pbps_diff::order::slots::handovers(
        &changes.changes,
        frame.recorded,
        frame.written,
        dialect,
        facts,
    )
    .map_err(anyhow::Error::msg)?;
    if schedule.constraints().is_empty() {
        return Ok(());
    }
    // Keep effect edges beside the slot handovers so settling respects table,
    // column and view prerequisites.
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, frame.written.ids)?;
    let owners =
        pbps_model::operation::PlanOwners::new(names, frame.recorded.ids, frame.written.ids);
    let ops = pbps_model::operation::Operations::of_plan(
        &changes.changes,
        if dialect.rebuilds_modules() {
            pbps_model::operation::ModuleEdits::Rebuilt
        } else {
            pbps_model::operation::ModuleEdits::InPlace
        },
    );
    let before = pbps_model::operation::Snapshot {
        schema: frame.recorded.schema,
        ids: frame.recorded.ids,
    };
    let after = pbps_model::operation::Snapshot {
        schema: frame.written.schema,
        ids: frame.written.ids,
    };
    let shadowed =
        pbps_diff::order::analyse_with_catalog(&ops, &owners, before, after, dialect, Some(facts))?;
    let fx_graph = {
        let step_effects = shadowed.effects().map_err(|pbps_diff::order::Unread { id, error }| {
            let label = pbps_model::escape::Escaped(frame.label);
            anyhow::anyhow!("SQLite cannot order these name handovers for `{label}`: unread step {id}: {error}")
        })?;
        pbps_diff::order::graph(
            ops.as_slice(),
            step_effects,
            &pbps_diff::order::Baseline::of(before, &owners)?,
            &frame.hints.annotations.modules(frame.written.schema),
            pbps_diff::order::Checks::ALL,
        )
    };
    for edge in &fx_graph.edges {
        schedule.edge(
            edge.before,
            edge.after,
            pbps_diff::order::Source::Effect,
            pbps_diff::order::schedule::reason(edge, ops.as_slice()),
        );
    }
    let settled = schedule.settle(changes.clone()).map_err(|refusal| {
        anyhow::anyhow!(
            "SQLite cannot order these name handovers for `{}`: {}",
            pbps_model::escape::Escaped(frame.label),
            refusal.describe(changes)
        )
    })?;
    *changes = settled.changes;
    Ok(())
}

impl PlanSession for Sqlite {
    type State = PlanState;

    /// SQLite has no partitions and no session-dependent date/time types, and
    /// declared reference-data rows are not built on SQLite yet. An empty
    /// [`Spellings`] is honest only when no table declares rows or partitions;
    /// any table that does is refused by name rather than answered empty.
    async fn misspelt(
        &self,
        _state: &PlanState,
        _conn: &mut Conn,
        schema: &Schema,
        _at: &CatalogNames,
    ) -> Result<Spellings, RowsError> {
        for (name, table) in &schema.tables {
            if table.data.is_some() {
                return Err(RowsError::Unreadable {
                    table: name.clone(),
                    why: "declared row spellings are not checked on SQLite yet".into(),
                });
            }
            if table.partition_of.is_some() || table.partition_by.is_some() {
                return Err(RowsError::Unreadable {
                    table: name.clone(),
                    why: "SQLite has no partitions, so partition bound spellings do not apply"
                        .into(),
                });
            }
        }
        Ok(Spellings::default())
    }

    /// SQLite's pure steps depend on the project's version floor, which `self`
    /// already carries, and not on a server setting; still verify the
    /// connection is a SQLite file and that its library meets the catalog
    /// reader's floor.
    async fn planning_dialect(&self, conn: &mut Conn) -> anyhow::Result<Self> {
        let c = sqlite_conn(conn)?;
        let problems = pbps_sqlite::preflight::library_problems(c, &ChangeSet::default())?;
        if !problems.is_empty() {
            anyhow::bail!("{}", pbps_sqlite::preflight::refusal(&problems));
        }
        Ok(*self)
    }

    /// SQLite has no roles; a plan that asks what a dropped role owns was not
    /// built for SQLite (refused rather than answered empty).
    async fn role_owned_securables(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<BTreeMap<pbps_model::RoleName, Vec<String>>> {
        Err(sqlite_has_no_roles("reading what the roles own"))
    }

    /// SQLite's only managed schema is `main` (`pbps_sqlite::validate::SCHEMA`),
    /// compared ASCII case-insensitively as SQLite does: `"main"` spells
    /// itself, `"MAIN"` spells `"main"` (so `refuse_wrongly_spelt` catches the
    /// case mismatch), and any other schema name is absent (`None`).
    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError> {
        let _ = sqlite_conn(conn)?;
        let schema = pbps_sqlite::validate::SCHEMA;
        Ok(names
            .iter()
            .map(|name| {
                let spelled = pbps_sqlite::ident::same(name, schema).then(|| schema.to_owned());
                (name.clone(), spelled)
            })
            .collect())
    }

    /// SQLite has no SQL Server computed-column expression edges to read, so
    /// the differ screens standing computed columns by text.
    fn computed_column_screen(&self) -> pbps_diff::Screen {
        pbps_diff::Screen::Text
    }

    /// Asked later, of the whole file: this position exists for an engine
    /// whose catalog read can miss an object, and it answers from what the
    /// model could not read. SQLite's `sqlite_schema` is readable whole, so
    /// [`PlanSession::check_names_against_catalog`], which runs next, sees
    /// every table, view and index there is, managed or not, and compares
    /// them by SQLite's own rule. A second answer here could only disagree
    /// with it. SQL Server answers the same way for the same reason.
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

    /// Does not exist on this engine (partitioned-table rebuilds) plus
    /// SQLite's staged-view-edit refusal: delegates to the helper shared with
    /// [`ApplySession::rebuild_mode`].
    fn require_transactional_rebuilds(
        &self,
        changes: &ChangeSet,
        staged: bool,
    ) -> anyhow::Result<()> {
        require_transactional_rebuilds(changes, staged)
    }

    /// Reads `main.sqlite_schema` and touched `table_xinfo` columns (without
    /// loading virtual-table modules), orders handovers, refuses whole-file
    /// collisions, and caches the inventory in [`PlanState`].
    async fn check_names_against_catalog<S: Clone>(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        frame: Frame<'_>,
        _staged: bool,
    ) -> anyhow::Result<()> {
        let inv = ensure_inventory(state, conn, changes, frame)?.clone();
        order_and_check_names(self, &inv, changes, frame)?;
        Ok(())
    }

    /// Schedules view edits and drops against `main.sqlite_schema`: managed
    /// views that may read an edited view are woven around it (dropped before,
    /// created after), while unmanaged views, triggers (including `INSTEAD OF`
    /// triggers on the view), and managed views kept over a dropped view refuse
    /// the plan naming SQLite, the view and the reader. Foreign role, enum and
    /// partitioned-table rebuild plans are refused by name.
    async fn schedule<S: pbps_diff::order::RouteState<Self>>(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        changes: PlanningPlan<S>,
        frame: Frame<'_>,
        role_members: Option<&RoleMembers>,
        _again: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
    ) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
        let _ = sqlite_conn(conn)?;
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                p.change,
                pbps_model::Change::Partition(
                    pbps_model::PartitionChange::RebuildPartitionedTable(_)
                )
            )),
            "SQLite does not support partitioned-table rebuilds"
        );
        if role_members.is_some()
            || changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::Role(_)))
        {
            return Err(sqlite_has_no_roles("scheduling role changes"));
        }
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                p.change,
                pbps_model::Change::TypeValue(pbps_model::TypeValueChange::EnumRebuild { .. })
            )),
            "SQLite has no enum types to rebuild"
        );
        let (declarations, read) = defaults::scheduled_inputs(
            frame.written.schema,
            crate::dependents::Sources {
                before: frame.recorded,
                after: frame.written.ids,
            },
            &frame.hints.annotations,
            None,
        );
        if crate::dependents::dropped_modules(&changes).is_empty() {
            let scheduled =
                crate::dependents::schedule::schedule(changes, &declarations, &read, self)?;
            return Ok((
                scheduled.changes,
                defaults::not_applicable(
                    "SQLite",
                    "module_dependents",
                    "the plan drops or rebuilds no view",
                ),
            ));
        }
        let inv = ensure_inventory(state, conn, &changes, frame)?.clone();
        let known = known_module_ids(&changes, [frame.written.schema, frame.recorded.schema]);
        let found = module_dependents_from(&inv, &changes, &known)?;
        let read = read.with_module_dependents(&found);
        let scheduled = crate::dependents::schedule::schedule(changes, &declarations, &read, self)?;
        let partitions =
            crate::dependents::standing_partitions(frame.written.schema, &scheduled.changes);
        let sources = crate::dependents::Sources {
            before: frame.recorded,
            after: frame.written.ids,
        };
        let left = crate::dependents::unaccounted(
            &scheduled.changes,
            &found,
            &partitions,
            &sources
                .project(&scheduled.changes.changes)
                .map_err(anyhow::Error::msg)?,
        );
        if !left.is_empty() {
            anyhow::bail!(
                "module_dependents (SQLite): the plan still drops a view before what depends \
                 on it; this is a bug in pbps, please report it:\n  {}",
                left.join("\n  ")
            );
        }
        let dependents: usize = found.values().map(Vec::len).sum();
        let check = ConnectedCheck {
            name: "module_dependents",
            engine: "SQLite",
            status: "passed",
            message: format!(
                "{dependents} dependent(s) of {} dropped or rebuilt view(s) accounted for; {} change(s) added to the plan for them",
                found.len(),
                scheduled.added
            ),
        };
        Ok((scheduled.changes, check))
    }

    /// Orders column renames and slot handovers over the cached inventory and
    /// returns [`pbps_dialect::slots::CatalogFacts`] computed in memory.
    async fn order_by_catalog<S: Clone>(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        frame: Frame<'_>,
    ) -> anyhow::Result<Ordered> {
        let inv = ensure_inventory(state, conn, changes, frame)?.clone();
        let facts = order_and_check_names(self, &inv, changes, frame)?;
        Ok(Ordered {
            verified: false,
            facts: Some(facts),
        })
    }

    /// Runs SQLite's four post-ordering checks inside the planning transaction:
    /// `rebuilds`, `drop_blockers` (shared with [`ApplySession::drop_blockers`]),
    /// `logical_slots` and `numbered_column_fills`.
    async fn check_final_order(
        &self,
        _state: &PlanState,
        conn: &mut Conn,
        changes: &ChangeSet,
        frame: Frame<'_>,
    ) -> anyhow::Result<FinalOrderChecks> {
        use pbps_model::{
            Change, ColumnChange, ModuleChange as MC, ModuleKind, ModuleOptions, TableChange,
            ValueSource,
        };
        let _ = sqlite_conn(conn)?;
        for p in &changes.changes {
            if let Change::Module(
                MC::AlterModule { id, module } | MC::CreateModule { id, module },
            ) = &p.change
            {
                let shown = id.shown();
                anyhow::ensure!(
                    module.kind == ModuleKind::View && module.options == ModuleOptions::default(),
                    "SQLite only supports plain views, not {} (`{shown}`)",
                    module.kind
                );
            } else if let Change::Module(MC::DropModule { id, kind }) = &p.change {
                anyhow::ensure!(
                    *kind == ModuleKind::View,
                    "SQLite only supports views, not {kind} (`{}`)",
                    id.shown()
                );
            }
        }
        let rebuilds = self.check_module_rebuilds(conn, changes, false).await?;
        let drop_blockers = check_drop_blockers(
            conn,
            changes,
            frame.written.ids,
            frame.recorded.schema,
            frame.recorded.ids,
        )?;
        let no_slots = "SQLite has no partitions or logical replication slots";
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                p.change,
                Change::Table(TableChange::CreateTable {
                    beside_default: Some(_),
                    ..
                }) | Change::Partition(_)
            )),
            "{no_slots}"
        );
        let no_num = "SQLite has no identity or sequence columns and no logical decoding";
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                &p.change,
                Change::Column(ColumnChange::AddColumn { column, .. })
                    if matches!(column.value_source, ValueSource::Identity(_) | ValueSource::Sequence(_))
            )),
            "{no_num}"
        );
        Ok(FinalOrderChecks {
            rebuilds,
            drop_blockers,
            logical_slots: defaults::not_applicable("SQLite", "logical_slots", no_slots),
            numbered_column_fills: defaults::not_applicable(
                "SQLite",
                "numbered_column_fills",
                no_num,
            ),
        })
    }

    /// Does not exist on this engine (stored routines and `USING` conversions)
    /// plus neutral drop-requirement vetting: SQLite has no stored routines a
    /// table expression can call, so a `DEFAULT`, a `CHECK` or a partial-index
    /// `WHERE` predicate has nothing to vet. A recorded `USING` conversion is
    /// refused by name via [`ApplySession::conversions`]. Any `drop_guard`
    /// runs through [`vet_drop_requirements`] and [`ApplySession::requirement`],
    /// binding each predicate through SQLite's authorizer and refusing plans
    /// that drop, rename or retype a column a requirement reads.
    async fn vet_plan_sites(
        &self,
        state: &PlanState,
        conn: &mut Conn,
        names: &pbps_model::plan_names::PlanNames<'_>,
        _on_note: &mut dyn FnMut(String),
    ) -> anyhow::Result<()> {
        let _ = sqlite_conn(conn)?;
        vet_drop_requirements(self, conn, &state.admission, names).await?;
        self.conversions(conn, &state.admission, names.changes())
            .await?;
        Ok(())
    }

    /// Does not exist on this engine (roles, grants, revokes, row-security
    /// policies, exclusion constraints) or not built yet (generated columns):
    /// delegates each feature guard to [`ApplySession`] so plan and apply
    /// refuse the same foreign plans, and answers all four rows with
    /// `not_applicable` naming SQLite. Unlike `defaults::support_rows`, a
    /// plain column drop or retype is not refused here.
    async fn support_rows(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
        reads: SupportReads<'_>,
    ) -> anyhow::Result<SupportRows> {
        let _ = sqlite_conn(conn)?;
        self.permission_support(conn, changes, ids).await?;
        let generation_support = self.generation_support(conn, changes, ids).await?;
        self.owned_targets(changes, ids, reads.owners, reads.session_role)?;
        self.unrevocable_grants(changes, ids, reads.unrevocable)?;
        Ok(SupportRows {
            permission_support: defaults::not_applicable(
                "SQLite",
                "permission_support",
                "SQLite has no permissions, grants or row-security policies",
            ),
            generation_support,
            owned_targets: defaults::not_applicable(
                "SQLite",
                "owned_targets",
                "SQLite has no roles, object owners or grants",
            ),
            unrevocable_grants: defaults::not_applicable(
                "SQLite",
                "unrevocable_grants",
                "SQLite has no grants or revokes",
            ),
        })
    }

    /// Exists in `pbps-sqlite` already plus "does not exist on this engine"
    /// for server editions and system-versioned tables: delegates to the
    /// helper shared with [`ApplySession::preflight_limits`] at `Stage::Plan`
    /// (skipping the file-writability probe), surfaces floor notes through
    /// `edition.warnings`, refuses `strategy: online` on any change, and
    /// refuses any system-versioned table by name. Ordinary cascading foreign
    /// keys and `NOT NULL DEFAULT` additions pass.
    async fn server_limits(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<ServerLimits> {
        let edition = preflight_verdict(self, conn, changes, pbps_sqlite::preflight::Stage::Plan)?;
        refuse_system_versioned(changes)?;
        Ok(ServerLimits {
            edition,
            temporal: Vec::new(),
        })
    }

    /// SQLite has no stored routines, so it pins none.
    async fn pinned_routines(
        &self,
        conn: &mut Conn,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError> {
        let _ = sqlite_conn(conn)?;
        Ok(None)
    }

    /// SQLite has no partitioned tables and no partition-maintenance tool.
    async fn maintained_partitions(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<Vec<MaintainedParent>> {
        let _ = sqlite_conn(conn)?;
        Ok(Vec::new())
    }

    async fn external_role_renames(
        &self,
        _conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        defaults::external_role_renames("SQLite", recorded, declared)
    }

    fn reconcile_cluster_roles(
        &self,
        recorded: &IdsFile,
        queried: &IdsFile,
        renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
        _live: &Schema,
        _expected: &mut Schema,
    ) -> anyhow::Result<ConnectedCheck> {
        defaults::cluster_roles("SQLite", recorded, queried, renames)
    }

    fn rehearsal_course(
        &self,
        _scratch_available: bool,
        switched_off: bool,
    ) -> crate::rehearsal::connected::Course {
        crate::rehearsal::connected::course(true, switched_off)
    }

    /// Runs the plan on an in-memory page copy of the SQLite file (DEC-2950.16).
    async fn rehearse_connected(
        &self,
        _entry: Option<&mut crate::resolution::Entry<'_>>,
        conn: &mut Conn,
        _target: &crate::db::Target,
        plan: &crate::rehearsal::connected::Plan<'_, Self>,
        on_note: &mut dyn FnMut(String),
    ) -> anyhow::Result<Option<crate::rehearsal::connected::Verdict>> {
        use crate::output::Finding;
        use crate::rehearsal::{Base, Execution, Rejected, connected::Verdict};
        use pbps_sqlite::rehearsal as sq;

        let c = sqlite_conn(conn)?;
        let library = format!("SQLite {}", c.version()?);
        let mut scratch = Conn::Sqlite(Box::new(sq::scratch(c, sq::SCRATCH_LIMIT)?));
        let before = sq::resolution(sqlite_conn(&mut scratch)?)?;
        let zone = pbps_model::SessionZone::utc();
        let statements = crate::rehearsal::all_statements(plan.statements);
        let base = Base::Target {
            statements: &[],
            ids: plan.base.ids,
            execution: Execution {
                zone: &zone,
                transactional: !plan.staged,
            },
            roles: None,
        };
        let opt_out = "fix the declarations or the plan, or turn the rehearsal off with `rehearse: false` or `--no-rehearse`";
        let findings = match crate::rehearsal::rehearse_in::<Sqlite>(
            &mut scratch,
            None,
            base,
            &statements,
            plan.declared,
            plan.desired.ids,
            self,
            plan.hints,
        )
        .await
        {
            Ok(findings) => findings,
            Err(error) => match error.downcast_ref::<Rejected>() {
                Some(r) => {
                    return Ok(Some(Verdict::Refused(
                        Finding::error("rehearsal.statement", r.to_string()).remedy(opt_out),
                    )));
                }
                None => return Err(error.context("the rehearsal could not be completed")),
            },
        };
        if !findings.converged() {
            let msg = format!(
                "the plan does not converge on the declarations: after applying it to a copy of the database, the scratch still differs:\n  {}",
                findings.structural.join("\n  ")
            );
            return Ok(Some(Verdict::Refused(
                Finding::error("rehearsal.structural", msg)
                    .remedy("fix the declarations or the plan; nothing was written"),
            )));
        }
        let after = sq::resolution(sqlite_conn(&mut scratch)?)?;
        let renamed = sq::Renamed::of(plan.ordered.changes());
        for line in before.failing() {
            on_note(format!(
                "pre-existing unresolved object on scratch copy: {line}"
            ));
        }
        for line in after.not_compared(&before, &renamed) {
            on_note(format!(
                "unresolved object on scratch copy (could not be asked before the plan): {line}"
            ));
        }
        let broken = after.new_since(&before, &renamed);
        if !broken.is_empty() {
            let msg = format!(
                "the plan leaves objects that no longer resolve on the scratch copy:\n  {}",
                broken.join("\n  ")
            );
            return Ok(Some(Verdict::Refused(
                Finding::error("rehearsal.resolution", msg).remedy(opt_out),
            )));
        }
        let spelling = findings
            .spelling
            .iter()
            .map(|s| pbps_model::Text::from(s.as_str()))
            .collect();
        let record = pbps_model::Rehearsal::InProcess {
            library: pbps_model::Text::from(library),
            spelling,
        };
        Ok(Some(Verdict::Sealed(
            record,
            crate::rehearsal::connected::Manifests::new(),
        )))
    }
}

#[cfg(test)]
pub(in crate::engine) mod test_support {
    use pbps_db::{Conn, Driver};
    use std::future::Future;
    use std::path::PathBuf;
    use std::task::{Context, Poll, Waker};

    /// Polls a future once: a SQLite session call runs in place on the calling
    /// thread and must be ready at the first poll (DEC-2950.17).
    pub(in crate::engine) fn now<T>(future: impl Future<Output = T>) -> T {
        let mut cx = Context::from_waker(Waker::noop());
        match std::pin::pin!(future).as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => {
                panic!("a SQLite call is made in place and is ready at the first poll")
            }
        }
    }

    /// A temporary SQLite database file removed on drop.
    pub(in crate::engine) struct TempDb {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempDb {
        pub(in crate::engine) fn create() -> (Self, Conn) {
            let dir = std::env::temp_dir().join(format!(
                "pbps-cli-sqlite-test-{}-{:032x}",
                std::process::id(),
                rand::random::<u128>()
            ));
            std::fs::create_dir_all(&dir).expect("a temporary directory");
            let path = dir.join("test.db");
            let conn = Conn::create(Driver::Sqlite, path.to_str().expect("UTF-8 path"))
                .expect("the SQLite file is created");
            (Self { dir, path }, conn)
        }

        pub(in crate::engine) fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TempDb, now};
    use super::*;

    fn plain_schema() -> Schema {
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "id".into(),
            pbps_model::Column::new("integer".parse().expect("type")),
        );
        let mut schema = Schema::default();
        schema.tables.insert(TableName::new("main", "items"), table);
        schema
    }

    #[test]
    fn planning_dialect_preserves_the_configured_version_floor_on_a_real_file() {
        let (_tmp, mut conn) = TempDb::create();
        let floor = pbps_sqlite::Version::new(3, 42, 0);
        let dialect = Sqlite::with_floor(floor);
        let planned = now(dialect.planning_dialect(&mut conn)).expect("library meets floor");
        assert_eq!(planned.floor(), floor);
        // Negative: the default floor stays distinct from the configured one.
        assert_ne!(planned, Sqlite::default());
    }

    #[test]
    fn schema_spellings_maps_main_case_insensitively_and_reports_other_schemas_absent() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let names: BTreeSet<String> = ["main", "MAIN", "Main", "other", "public"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let spelled = now(dialect.schema_spellings(&mut conn, &names)).expect("spelled");
        assert_eq!(spelled["main"], Some("main".to_owned()));
        assert_eq!(spelled["MAIN"], Some("main".to_owned()));
        assert_eq!(spelled["Main"], Some("main".to_owned()));
        // Negative: schemas other than `main` are absent, not spelt back.
        assert_eq!(spelled["other"], None);
        assert_eq!(spelled["public"], None);
    }

    #[test]
    fn misspelt_is_empty_for_plain_tables_and_refuses_declared_rows_and_partitions_by_table() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let state = PlanState::default();
        let at = CatalogNames::new();

        let ok = now(dialect.misspelt(&state, &mut conn, &plain_schema(), &at)).expect("plain");
        assert_eq!(ok, Spellings::default());

        // Negative: a table declaring reference data is refused naming the table and SQLite.
        let mut with_rows = plain_schema();
        let row_table = TableName::new("main", "items");
        with_rows.tables.get_mut(&row_table).expect("table").data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Ensure,
            rows: BTreeMap::new(),
        });
        match now(dialect.misspelt(&state, &mut conn, &with_rows, &at)) {
            Err(RowsError::Unreadable { table, why }) => {
                assert_eq!(table, row_table);
                assert!(why.contains("SQLite"), "{why}");
            }
            other => panic!("expected Unreadable for declared rows, got {other:?}"),
        }

        // Negative: a partitioned table is refused naming the table and SQLite.
        let mut with_partition = plain_schema();
        with_partition
            .tables
            .get_mut(&row_table)
            .expect("table")
            .partition_of = Some(pbps_model::PartitionOf {
            parent: TableName::new("main", "parent"),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        });
        match now(dialect.misspelt(&state, &mut conn, &with_partition, &at)) {
            Err(RowsError::Unreadable { table, why }) => {
                assert_eq!(table, row_table);
                assert!(why.contains("SQLite") && why.contains("partition"), "{why}");
            }
            other => panic!("expected Unreadable for partitioned table, got {other:?}"),
        }
    }

    #[test]
    fn role_owned_securables_refuses_naming_sqlite_rather_than_answering_empty() {
        let (_tmp, mut conn) = TempDb::create();
        let err = now(Sqlite::default().role_owned_securables(&mut conn)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("SQLite") && message.contains("role"),
            "{message}"
        );
    }

    #[test]
    fn sqlite_plan_session_constants_reflect_no_routines_no_partitions_and_text_screen() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        assert_eq!(dialect.computed_column_screen(), pbps_diff::Screen::Text);
        assert!(
            now(dialect.pinned_routines(&mut conn))
                .expect("ok")
                .is_none()
        );
        assert!(
            now(dialect.maintained_partitions(&mut conn))
                .expect("ok")
                .is_empty()
        );
        // `check_final_order` refuses a DropTable whose target is absent from
        // SQLite's catalog (never answering "no blockers").
        let drop_table = ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Table(
                pbps_model::TableChange::DropTable {
                    uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "main.t", 0),
                    name: TableName::new("main", "t"),
                    detach_from: None,
                },
            ))],
        };
        let empty = Schema::default();
        let empty_ids = IdsFile::default();
        let hints = pbps_model::Hints::default();
        let baseline = StateSnapshot::new(
            pbps_model::StateKind::Baseline,
            pbps_sqlite::types::DIALECT,
            empty.clone(),
            empty_ids.clone(),
            "test",
        );
        let side = pbps_diff::Side {
            schema: &empty,
            ids: &empty_ids,
        };
        let frame = Frame {
            label: "test.db",
            recorded: side,
            written: side,
            base: side,
            declared: side,
            hints: &hints,
            baseline: &baseline,
        };
        let state = PlanState::default();
        let refused =
            now(dialect.check_final_order(&state, &mut conn, &drop_table, frame)).unwrap_err();
        assert!(
            refused.to_string().contains("SQLite") && refused.to_string().contains("main.t"),
            "{refused}"
        );
    }

    fn uid(kind: pbps_model::UidKind, name: &str) -> pbps_model::Uid {
        pbps_model::Uid::derived(kind, name, 0)
    }

    fn change_set(changes: Vec<pbps_model::Change>) -> ChangeSet {
        ChangeSet {
            changes: changes
                .into_iter()
                .map(pbps_model::PlannedChange::new)
                .collect(),
        }
    }

    fn view_module(sql: &str) -> Box<pbps_model::Module> {
        Box::new(pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: sql.into(),
            options: Default::default(),
        })
    }

    #[test]
    fn support_rows_returns_not_applicable_for_plain_plans_including_column_drops_and_retypes() {
        use pbps_model::{
            Change, Column, ColumnChange, ConstraintChange, ExclusionConstraint, Generated,
            GrantChange, GrantTarget, IndexMethod, Permission, PolicyChange, RoleName, Table,
            TableChange, ValueSource,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let table = TableName::new("main", "items");
        let t_uid = uid(pbps_model::UidKind::Table, "main.items");
        let drop_uid = uid(pbps_model::UidKind::Column, "main.items.old_col");
        let retype_uid = uid(pbps_model::UidKind::Column, "main.items.qty");
        let ids_of = |changes: &ChangeSet| crate::plan_fixture::ids(changes);

        let owners = BTreeMap::new();
        let session_role = RoleName::new("");
        let reads = SupportReads {
            owners: &owners,
            session_role: &session_role,
            unrevocable: &[],
        };

        // Plain SQLite plan including column drop and column retype (both
        // refused by `defaults::support_rows` via `row_security::column_edit`).
        let plain = change_set(vec![
            Change::Column(ColumnChange::DropColumn {
                uid: drop_uid,
                column: table.column("old_col"),
            }),
            Change::Column(ColumnChange::AlterColumnType {
                uid: retype_uid,
                column: table.column("qty"),
                from: "integer".parse().expect("type"),
                to: "real".parse().expect("type"),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: None,
            }),
        ]);
        let rows = now(dialect.support_rows(&mut conn, &plain, &ids_of(&plain), reads))
            .expect("plain plan passes");
        for (row, name) in [
            (&rows.permission_support, "permission_support"),
            (&rows.generation_support, "generation_support"),
            (&rows.owned_targets, "owned_targets"),
            (&rows.unrevocable_grants, "unrevocable_grants"),
        ] {
            assert_eq!(row.name, name);
            assert_eq!(row.engine, "SQLite");
            assert_eq!(row.status, "not_applicable");
            assert!(row.message.contains("SQLite"), "{row:?}");
        }

        // Negative: foreign permission, policy, generated-column and exclusion
        // plans are refused naming SQLite and the unsupported feature.
        let grant = change_set(vec![Change::Permission(GrantChange::Grant {
            role: RoleName::new("app"),
            target: GrantTarget::Object(table.clone()),
            permissions: BTreeSet::from([Permission::Select]),
        })]);
        let grant_err =
            now(dialect.support_rows(&mut conn, &grant, &ids_of(&grant), reads)).unwrap_err();
        assert!(
            grant_err.to_string().contains("SQLite") && grant_err.to_string().contains("grants"),
            "{grant_err}"
        );

        let revoke = change_set(vec![Change::Permission(GrantChange::Revoke {
            role: RoleName::new("app"),
            target: GrantTarget::Object(table.clone()),
            permissions: BTreeSet::from([Permission::Select]),
        })]);
        let revoke_err =
            now(dialect.support_rows(&mut conn, &revoke, &ids_of(&revoke), reads)).unwrap_err();
        assert!(
            revoke_err.to_string().contains("SQLite")
                && revoke_err.to_string().contains("permissions"),
            "{revoke_err}"
        );

        let policy = change_set(vec![Change::Policy(PolicyChange::DropPolicy {
            table: table.clone(),
            name: "p".into(),
            policy: Default::default(),
        })]);
        let policy_err =
            now(dialect.support_rows(&mut conn, &policy, &ids_of(&policy), reads)).unwrap_err();
        assert!(
            policy_err.to_string().contains("SQLite")
                && policy_err.to_string().contains("policies"),
            "{policy_err}"
        );

        let unrevocable = [pbps_db::catalog::Unrevocable {
            role: RoleName::new("app"),
            target: GrantTarget::Object(table.clone()),
            permission: Permission::Select,
            why: "granted by another role".into(),
        }];
        let reads_with_unrevocable = SupportReads {
            owners: &owners,
            session_role: &session_role,
            unrevocable: &unrevocable,
        };
        let empty = ChangeSet::default();
        let unrev_err = now(dialect.support_rows(
            &mut conn,
            &empty,
            &IdsFile::default(),
            reads_with_unrevocable,
        ))
        .unwrap_err();
        assert!(
            unrev_err.to_string().contains("SQLite") && unrev_err.to_string().contains("revokes"),
            "{unrev_err}"
        );

        let alter_expr = change_set(vec![Change::Column(ColumnChange::AlterColumnExpression {
            uid: uid(pbps_model::UidKind::Column, "main.items.g"),
            column: table.column("g"),
            from: "a + 1".into(),
            to: "a + 2".into(),
        })]);
        let gen_err =
            now(dialect.support_rows(&mut conn, &alter_expr, &ids_of(&alter_expr), reads))
                .unwrap_err();
        assert!(
            gen_err.to_string().contains("SQLite")
                && gen_err.to_string().contains("generated columns"),
            "{gen_err}"
        );

        let mut gen_col = Column::new("integer".parse().expect("type"));
        gen_col.value_source = ValueSource::Generated(Generated {
            expression: "id + 1".into(),
            stored: false,
        });
        let add_gen = change_set(vec![Change::Column(ColumnChange::AddColumn {
            uid: uid(pbps_model::UidKind::Column, "main.items.g2"),
            table: table.clone(),
            name: "g2".into(),
            column: Box::new(gen_col.clone()),
        })]);
        let add_gen_err =
            now(dialect.support_rows(&mut conn, &add_gen, &ids_of(&add_gen), reads)).unwrap_err();
        assert!(
            add_gen_err.to_string().contains("SQLite")
                && add_gen_err.to_string().contains("generated columns"),
            "{add_gen_err}"
        );

        let mut gen_table = Table::default();
        gen_table.columns.insert("g".into(), gen_col);
        let create_gen = change_set(vec![Change::Table(TableChange::CreateTable {
            uid: t_uid,
            name: table.clone(),
            table: Box::new(gen_table),
            beside_default: None,
            moves_at_most: None,
        })]);
        let create_gen_err =
            now(dialect.support_rows(&mut conn, &create_gen, &ids_of(&create_gen), reads))
                .unwrap_err();
        assert!(
            create_gen_err.to_string().contains("SQLite")
                && create_gen_err.to_string().contains("generated columns"),
            "{create_gen_err}"
        );

        let exclusion = change_set(vec![Change::Constraint(ConstraintChange::SetExclusion {
            table,
            name: "ex".into(),
            from: None,
            to: Some(Box::new(ExclusionConstraint {
                method: IndexMethod::Gist,
                keys: Vec::new(),
                include: Vec::new(),
                filter: None,
                storage_parameters: BTreeMap::new(),
                timing: Default::default(),
            })),
        })]);
        let ex_err = now(dialect.support_rows(&mut conn, &exclusion, &ids_of(&exclusion), reads))
            .unwrap_err();
        assert!(
            ex_err.to_string().contains("SQLite") && ex_err.to_string().contains("exclusion"),
            "{ex_err}"
        );
    }

    #[test]
    fn server_limits_runs_stage_plan_preflight_and_refuses_online_and_temporal_plans() {
        use pbps_model::{
            Change, Column, ColumnChange, ForeignKey, PlannedChange, ReferentialAction, Strategy,
            SystemTime, Table, TableChange, ValueSource,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();

        // Plain plan carrying both a `NOT NULL DEFAULT` column addition and an
        // ordinary table with `ON DELETE CASCADE` (both refused by
        // `defaults::server_limits`).
        let mut required_default = Column::new("integer".parse().expect("type"));
        required_default.nullable = false;
        required_default.value_source = ValueSource::Default("0".into());

        let mut child = Table::default();
        child.columns.insert(
            "parent_id".into(),
            Column::new("integer".parse().expect("type")),
        );
        child.foreign_keys.insert(
            "fk_parent".into(),
            ForeignKey {
                columns: vec!["parent_id".into()],
                references_table: TableName::new("main", "parent"),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::Cascade,
                on_update: Default::default(),
            },
        );
        let plain = change_set(vec![
            Change::Column(ColumnChange::AddColumn {
                uid: uid(pbps_model::UidKind::Column, "main.t.n"),
                table: TableName::new("main", "t"),
                name: "n".into(),
                column: Box::new(required_default),
            }),
            Change::Table(TableChange::CreateTable {
                uid: uid(pbps_model::UidKind::Table, "main.child"),
                name: TableName::new("main", "child"),
                table: Box::new(child),
                beside_default: None,
                moves_at_most: None,
            }),
        ]);

        let limits = now(dialect.server_limits(&mut conn, &plain)).expect("plain plan passes");
        // The apply asks the same question and must not refuse what the plan
        // accepted: a cascading key is not a system-versioned table.
        now(dialect.refuse_unsupported_temporal(&mut conn, &plain))
            .expect("the apply accepts a cascading key too");
        assert!(limits.edition.refused_online.is_empty());
        assert!(
            limits.edition.runs.starts_with("SQLite 3."),
            "{}",
            limits.edition.runs
        );
        assert!(limits.temporal.is_empty());
        // Default floor 3.35.0 is older than the bundled 3.54.x library, so
        // the floor note surfaces through `edition.warnings`.
        assert_eq!(
            limits.edition.warnings.len(),
            1,
            "{:?}",
            limits.edition.warnings
        );
        assert!(
            limits.edition.warnings[0].contains("floor"),
            "{}",
            limits.edition.warnings[0]
        );

        // With a floor at or above the bundled library, no floor note is emitted.
        let high_floor = Sqlite::with_floor(pbps_sqlite::Version::new(3, 99, 0));
        let quiet = now(high_floor.server_limits(&mut conn, &plain)).expect("high floor passes");
        assert!(
            quiet.edition.warnings.is_empty(),
            "{:?}",
            quiet.edition.warnings
        );

        // Negative: a bad connection pragma is refused via `preflight::refusal`.
        now(conn.execute("PRAGMA legacy_alter_table = ON")).expect("set pragma");
        let pragma_err = now(dialect.server_limits(&mut conn, &plain)).unwrap_err();
        assert!(
            pragma_err.to_string().contains("legacy_alter_table")
                && pragma_err.to_string().contains("Nothing was changed"),
            "{pragma_err}"
        );
        now(conn.execute("PRAGMA legacy_alter_table = OFF")).expect("restore pragma");

        // Negative: `strategy: online` is refused naming SQLite (even on a
        // change SQL Server's `takes_online` ignores).
        let mut online = PlannedChange::new(Change::Table(TableChange::DropTable {
            uid: uid(pbps_model::UidKind::Table, "main.t"),
            name: TableName::new("main", "t"),
            detach_from: None,
        }));
        online.strategy = Strategy { online: true };
        let online_err = now(dialect.server_limits(
            &mut conn,
            &ChangeSet {
                changes: vec![online],
            },
        ))
        .unwrap_err();
        assert!(
            online_err.to_string().contains("SQLite") && online_err.to_string().contains("online"),
            "{online_err}"
        );

        // Negative: a system-versioned table is refused naming SQLite.
        let temporal_table = Table {
            system_time: Some(SystemTime {
                start: "valid_from".into(),
                end: "valid_to".into(),
                hidden: false,
                versioning: None,
            }),
            ..Default::default()
        };
        let temporal = change_set(vec![Change::Table(TableChange::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "main.hist"),
            name: TableName::new("main", "hist"),
            table: Box::new(temporal_table),
            beside_default: None,
            moves_at_most: None,
        })]);
        let temporal_err = now(dialect.server_limits(&mut conn, &temporal)).unwrap_err();
        assert!(
            temporal_err.to_string().contains("SQLite")
                && temporal_err.to_string().contains("system-versioned"),
            "{temporal_err}"
        );
        let at_apply = now(dialect.refuse_unsupported_temporal(&mut conn, &temporal)).unwrap_err();
        assert_eq!(at_apply.to_string(), temporal_err.to_string());
    }

    #[test]
    fn require_transactional_rebuilds_refuses_staged_view_edits_and_partition_rebuilds() {
        use pbps_model::{
            Change, ModuleChange, ModuleId, ModuleKind, PartitionChange, Rebuild,
            RebuildIdentities, RebuildPartitionedTable, TableChange,
        };

        let dialect = Sqlite::default();
        let view_id = ModuleId::Named(TableName::new("main", "v"));

        let alter_view = change_set(vec![Change::Module(ModuleChange::AlterModule {
            id: view_id.clone(),
            module: view_module("SELECT 1"),
        })]);
        dialect
            .require_transactional_rebuilds(&alter_view, false)
            .expect("unstaged view edit passes");

        // Plain non-rebuild change passes even when staged.
        let create_table = change_set(vec![Change::Table(TableChange::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "main.t"),
            name: TableName::new("main", "t"),
            table: Default::default(),
            beside_default: None,
            moves_at_most: None,
        })]);
        dialect
            .require_transactional_rebuilds(&create_table, true)
            .expect("staged create table passes");

        // Negative: staged view edit is refused naming SQLite and telling the
        // operator to apply without `--staged`, aligned with `ApplySession::rebuild_mode`.
        let staged_err = dialect
            .require_transactional_rebuilds(&alter_view, true)
            .unwrap_err();
        let staged_msg = staged_err.to_string();
        assert!(
            staged_msg.contains("SQLite")
                && staged_msg.contains("absent")
                && staged_msg.contains("without --staged"),
            "{staged_msg}"
        );
        let apply_msg = dialect
            .rebuild_mode(&alter_view, true)
            .unwrap_err()
            .to_string();
        assert_eq!(staged_msg, apply_msg);

        // Negative: drop+create view rebuild is also refused when staged.
        let drop_create = change_set(vec![
            Change::Module(ModuleChange::DropModule {
                id: view_id.clone(),
                kind: ModuleKind::View,
            }),
            Change::Module(ModuleChange::CreateModule {
                id: view_id,
                module: view_module("SELECT 2"),
            }),
        ]);
        let dc_err = dialect
            .require_transactional_rebuilds(&drop_create, true)
            .unwrap_err();
        assert!(
            dc_err.to_string().contains("SQLite")
                && dc_err.to_string().contains("without --staged"),
            "{dc_err}"
        );
        dialect
            .require_transactional_rebuilds(&drop_create, false)
            .expect("unstaged drop+create passes");

        // Negative: partitioned-table rebuild is refused both staged and
        // unstaged, naming SQLite.
        let part_rebuild = change_set(vec![Change::Partition(
            PartitionChange::RebuildPartitionedTable(Box::new(RebuildPartitionedTable {
                table: TableName::new("main", "p"),
                intent: Rebuild {
                    source: RebuildIdentities {
                        tables: BTreeMap::new(),
                        columns: BTreeMap::new(),
                    },
                    replacement: RebuildIdentities {
                        tables: BTreeMap::new(),
                        columns: BTreeMap::new(),
                    },
                    replacement_root: uid(pbps_model::UidKind::Table, "main.p_new"),
                    continues: BTreeMap::new(),
                    target_checksum: "0".repeat(64),
                    max_rows: 0,
                    allowed_at: "2026-01-01T00:00:00Z".into(),
                    reason: pbps_model::Reason::new("test").expect("reason"),
                    operator: "test".into(),
                    types: BTreeMap::new(),
                    conversions: BTreeMap::new(),
                    exclusions: BTreeMap::new(),
                },
                earlier: Vec::new(),
                before: BTreeMap::new(),
                after: BTreeMap::new(),
            })),
        )]);
        for staged in [false, true] {
            let err = dialect
                .require_transactional_rebuilds(&part_rebuild, staged)
                .unwrap_err();
            assert!(
                err.to_string().contains("SQLite") && err.to_string().contains("partitioned-table"),
                "staged={staged}: {err}"
            );
            assert_eq!(
                err.to_string(),
                dialect
                    .rebuild_mode(&part_rebuild, staged)
                    .unwrap_err()
                    .to_string()
            );
        }
    }

    #[test]
    fn vet_plan_sites_accepts_table_expressions_and_guarded_drops_and_refuses_using_and_referenced_column_edits()
     {
        use pbps_model::{
            Change, CheckConstraint, Column, ColumnChange, ConstraintChange, DropGuard,
            DropRequirement, Index, IndexChange, IndexColumn, PlannedChange, Table, TableChange,
            ValueSource,
        };

        let (_tmp, mut conn) = TempDb::create();
        now(conn.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, active INTEGER NOT NULL, note TEXT)",
        ))
        .expect("seed items");
        let dialect = Sqlite::default();
        let state = PlanState::default();
        let table_name = TableName::new("main", "items");
        let t_uid = uid(pbps_model::UidKind::Table, "main.items");

        let mut table = Table::default();
        let mut active = Column::new("integer".parse().expect("type"));
        active.value_source = ValueSource::Default("1".into());
        table.columns.insert("active".into(), active.clone());
        table.checks.insert(
            "ck_active".into(),
            CheckConstraint {
                expression: "active IN (0, 1)".into(),
            },
        );

        let expr_changes = vec![
            PlannedChange::new(Change::Table(TableChange::CreateTable {
                uid: t_uid.clone(),
                name: table_name.clone(),
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            })),
            PlannedChange::new(Change::Column(ColumnChange::AddColumn {
                uid: uid(pbps_model::UidKind::Column, "main.items.extra"),
                table: table_name.clone(),
                name: "extra".into(),
                column: Box::new(active),
            })),
            PlannedChange::new(Change::Constraint(ConstraintChange::AddCheck {
                table: table_name.clone(),
                name: "ck_extra".into(),
                constraint: CheckConstraint {
                    expression: "extra >= 0".into(),
                },
            })),
            PlannedChange::new(Change::Index(IndexChange::AddIndex {
                table: table_name.clone(),
                name: "ix_active".into(),
                index: Box::new(Index {
                    columns: vec![IndexColumn::column("active")],
                    include: Vec::new(),
                    unique: false,
                    filter: Some("active = 1".into()),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                    filegroup: None,
                }),
                clustered: false,
            })),
        ];
        let expr_cs = ChangeSet {
            changes: expr_changes,
        };
        let expr_ids = crate::plan_fixture::ids(&expr_cs);
        let expr_names =
            pbps_model::plan_names::PlanNames::of(&expr_cs.changes, &expr_ids).expect("names");
        let mut notes = Vec::new();
        now(
            dialect.vet_plan_sites(&state, &mut conn, &expr_names, &mut |note| {
                notes.push(note);
            }),
        )
        .expect("default, check and partial index pass");
        assert!(notes.is_empty(), "{notes:?}");

        // Negative: a recorded `USING` conversion is refused naming SQLite.
        let using_cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::Column(
                ColumnChange::AlterColumnType {
                    uid: uid(pbps_model::UidKind::Column, "main.items.active"),
                    column: table_name.column("active"),
                    from: "integer".parse().expect("type"),
                    to: "text".parse().expect("type"),
                    from_nullable: true,
                    to_nullable: true,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: Some("CAST(active AS text)".into()),
                },
            ))],
        };
        let using_ids = crate::plan_fixture::ids(&using_cs);
        let using_names =
            pbps_model::plan_names::PlanNames::of(&using_cs.changes, &using_ids).expect("names");
        let using_err =
            now(dialect.vet_plan_sites(&state, &mut conn, &using_names, &mut |_| {})).unwrap_err();
        assert!(
            using_err.to_string().contains("SQLite") && using_err.to_string().contains("USING"),
            "{using_err}"
        );

        // Positive: a valid guarded drop passes `vet_plan_sites`.
        let one_guard = |owner: &pbps_model::Uid, predicate: &str| DropGuard {
            table_uid: t_uid.clone(),
            table: table_name.clone(),
            detach_from: None,
            requirements: [(
                owner.clone(),
                DropRequirement {
                    table_uid: t_uid.clone(),
                    predicate: predicate.into(),
                    required_at: "2026-01-01T00:00:00Z".into(),
                    operator: "test".into(),
                },
            )]
            .into(),
        };
        let mut guarded = PlannedChange::new(Change::Table(TableChange::DropTable {
            uid: t_uid.clone(),
            name: table_name.clone(),
            detach_from: None,
        }));
        guarded.drop_guard = Some(one_guard(&t_uid, "active = 0"));
        let guard_cs = ChangeSet {
            changes: vec![guarded],
        };
        let guard_ids = crate::plan_fixture::ids(&guard_cs);
        let guard_names =
            pbps_model::plan_names::PlanNames::of(&guard_cs.changes, &guard_ids).expect("names");
        now(dialect.vet_plan_sites(&state, &mut conn, &guard_names, &mut |_| {}))
            .expect("valid guarded drop passes vet_plan_sites");

        // Negative: a guarded drop whose predicate reads a column this plan drops,
        // renames, or retypes is refused with the split-plan remedy (even when the
        // predicate spells the column in another ASCII case).
        let active_uid = uid(pbps_model::UidKind::Column, "main.items.active");
        let note_uid = uid(pbps_model::UidKind::Column, "main.items.note");
        let mut base_ids = IdsFile::default();
        base_ids.tables.insert(t_uid.clone(), table_name.clone());
        base_ids.columns.insert(
            uid(pbps_model::UidKind::Column, "main.items.id"),
            table_name.column("id"),
        );
        base_ids
            .columns
            .insert(active_uid.clone(), table_name.column("active"));
        base_ids
            .columns
            .insert(note_uid.clone(), table_name.column("note"));

        let mut drop_note = PlannedChange::new(Change::Column(ColumnChange::DropColumn {
            uid: note_uid.clone(),
            column: table_name.column("note"),
        }));
        drop_note.drop_guard = Some(one_guard(&note_uid, "ACTIVE = 0"));

        for (label, extra, after_ids) in [
            (
                "drop referenced column",
                Change::Column(ColumnChange::DropColumn {
                    uid: active_uid.clone(),
                    column: table_name.column("active"),
                }),
                {
                    let mut ids = base_ids.clone();
                    ids.columns.remove(&note_uid);
                    ids.columns.remove(&active_uid);
                    ids
                },
            ),
            (
                "rename referenced column",
                Change::Column(ColumnChange::RenameColumn {
                    uid: active_uid.clone(),
                    table: table_name.clone(),
                    from: "active".into(),
                    to: "is_active".into(),
                    table_was: None,
                }),
                {
                    let mut ids = base_ids.clone();
                    ids.columns.remove(&note_uid);
                    ids.columns
                        .insert(active_uid.clone(), table_name.column("is_active"));
                    ids
                },
            ),
            (
                "retype referenced column",
                Change::Column(ColumnChange::AlterColumnType {
                    uid: active_uid.clone(),
                    column: table_name.column("active"),
                    from: "integer".parse().expect("type"),
                    to: "text".parse().expect("type"),
                    from_nullable: false,
                    to_nullable: false,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: None,
                }),
                {
                    let mut ids = base_ids.clone();
                    ids.columns.remove(&note_uid);
                    ids
                },
            ),
        ] {
            let cs = ChangeSet {
                changes: vec![drop_note.clone(), PlannedChange::new(extra)],
            };
            let names =
                pbps_model::plan_names::PlanNames::of(&cs.changes, &after_ids).expect(label);
            let err = now(dialect.vet_plan_sites(&state, &mut conn, &names, &mut |_| {}))
                .expect_err(label)
                .to_string();
            assert!(
                err.contains("names a column changed by this plan; split the plan"),
                "{label}: {err}"
            );
        }
    }

    fn int_col() -> pbps_model::Column {
        pbps_model::Column::new("integer".parse().expect("type"))
    }

    fn table_with_cols(cols: &[&str]) -> pbps_model::Table {
        let mut table = pbps_model::Table::default();
        for &c in cols {
            table.columns.insert(c.into(), int_col());
        }
        table
    }

    fn table_id(ids: &mut IdsFile, name: &str) -> pbps_model::Uid {
        let tn = TableName::new("main", name);
        let u = uid(pbps_model::UidKind::Table, &format!("main.{name}"));
        ids.tables.insert(u.clone(), tn);
        u
    }

    fn col_id(ids: &mut IdsFile, table: &str, col: &str) -> pbps_model::Uid {
        let u = uid(pbps_model::UidKind::Column, &format!("main.{table}.{col}"));
        ids.columns
            .insert(u.clone(), TableName::new("main", table).column(col));
        u
    }

    fn scoped_of(schema: &Schema, ids: &IdsFile) -> pbps_diff::Scoped {
        let modules: BTreeSet<pbps_model::ModuleId> = schema.modules.keys().cloned().collect();
        pbps_diff::scope(schema, ids, &modules, &BTreeSet::new())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_name_methods(
        dialect: &Sqlite,
        conn: &mut Conn,
        plan: &mut PlanningPlan<pbps_diff::order::Draft>,
        base: &Schema,
        base_ids: &IdsFile,
        desired: &Schema,
        desired_ids: &IdsFile,
        unmanaged_relations: &[TableName],
    ) -> anyhow::Result<(PlanState, Ordered)> {
        let scoped = scoped_of(base, base_ids);
        dialect.refuse_occupied_relation_names(
            plan,
            &scoped,
            &[],
            unmanaged_relations,
            "test.db",
        )?;
        let hints = pbps_model::Hints::default();
        let baseline = StateSnapshot::new(
            pbps_model::StateKind::Baseline,
            pbps_sqlite::types::DIALECT,
            base.clone(),
            base_ids.clone(),
            "test",
        );
        let base_side = pbps_diff::Side {
            schema: base,
            ids: base_ids,
        };
        let desired_side = pbps_diff::Side {
            schema: desired,
            ids: desired_ids,
        };
        let frame = Frame {
            label: "test.db",
            recorded: base_side,
            written: desired_side,
            base: base_side,
            declared: desired_side,
            hints: &hints,
            baseline: &baseline,
        };
        let mut state = PlanState::default();
        now(dialect.check_names_against_catalog(&mut state, conn, plan, frame, false))?;
        let ordered = now(dialect.order_by_catalog(&mut state, conn, plan, frame))?;
        #[cfg(debug_assertions)]
        if !ordered.verified {
            pbps_diff::order::shadow::observe_scheduled(
                base_side,
                desired_side,
                dialect,
                &hints.annotations.modules(desired),
                plan,
                pbps_diff::order::shadow::Stage::Final,
                ordered.facts.as_ref(),
                &Default::default(),
            )?;
        }
        Ok((state, ordered))
    }

    #[test]
    fn creating_table_view_and_index_on_clean_file_passes_all_three_name_methods() {
        use pbps_model::{
            Change, Index, IndexChange, IndexColumn, ModuleChange, ModuleId, TableChange,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();

        let base = Schema::default();
        let base_ids = IdsFile::default();
        let mut desired = Schema::default();
        let mut desired_ids = IdsFile::default();
        let items = table_with_cols(&["id"]);
        let t_uid = table_id(&mut desired_ids, "items");
        col_id(&mut desired_ids, "items", "id");
        let idx = Index {
            columns: vec![IndexColumn::column("id")],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
            filegroup: None,
        };
        let mut items_with_idx = items.clone();
        items_with_idx
            .indexes
            .insert("ix_items".into(), idx.clone());
        desired
            .tables
            .insert(TableName::new("main", "items"), items_with_idx);
        let view_id = ModuleId::Named(TableName::new("main", "v_items"));
        desired
            .modules
            .insert(view_id.clone(), *view_module("SELECT id FROM items"));

        let mut plan = pbps_diff::order::propose(change_set(vec![
            Change::Table(TableChange::CreateTable {
                uid: t_uid,
                name: TableName::new("main", "items"),
                table: Box::new(items),
                beside_default: None,
                moves_at_most: None,
            }),
            Change::Index(IndexChange::AddIndex {
                table: TableName::new("main", "items"),
                name: "ix_items".into(),
                index: Box::new(idx),
                clustered: false,
            }),
            Change::Module(ModuleChange::CreateModule {
                id: view_id,
                module: view_module("SELECT id FROM items"),
            }),
        ]));

        let (state, ordered) = run_name_methods(
            &dialect,
            &mut conn,
            &mut plan,
            &base,
            &base_ids,
            &desired,
            &desired_ids,
            &[],
        )
        .expect("clean file passes all three methods and observe_scheduled");
        assert!(!ordered.verified);
        assert!(ordered.facts.is_some());
        assert!(state.inventory.is_some());
    }

    #[test]
    fn relation_collisions_across_managed_unmanaged_virtual_ledger_intermediate_and_case_folded_are_refused()
     {
        use pbps_model::{
            Change, Index, IndexColumn, ModuleChange, ModuleId, Strategy, TableChange,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();

        // Seed the real SQLite file with every kind of occupant:
        // - a managed table `ManagedTbl` with an explicit index `ManagedIdx`
        // - a managed view `ManagedView`
        // - an unmanaged table `UnmanagedTbl`, view `UnmanagedView`, expression index `UnmanagedExprIdx`
        // - a virtual FTS5 table `DocsFts` (creating shadow tables such as `DocsFts_content`)
        // - a missing-module virtual table `MissingMod` (via writable_schema)
        // - a ledger table `__pbps_state`
        // - a SQLite internal `sqlite_sequence` (via AUTOINCREMENT)
        // - a trigger `OnlyTrigger` (separate namespace: must NOT collide)
        // - a non-ASCII table `café` (`cafÉ` must NOT collide)
        for sql in [
            "CREATE TABLE ManagedTbl (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            "CREATE INDEX ManagedIdx ON ManagedTbl (id)",
            "CREATE VIEW ManagedView AS SELECT id FROM ManagedTbl",
            "CREATE TABLE UnmanagedTbl (id INTEGER)",
            "CREATE VIEW UnmanagedView AS SELECT 1 AS x",
            "CREATE INDEX UnmanagedExprIdx ON UnmanagedTbl (ABS(id))",
            "CREATE VIRTUAL TABLE DocsFts USING fts5(body)",
            "CREATE TABLE __pbps_state (id INTEGER PRIMARY KEY)",
            "CREATE TRIGGER OnlyTrigger AFTER INSERT ON ManagedTbl BEGIN SELECT 1; END",
            "CREATE TABLE \"café\" (id INTEGER)",
            "PRAGMA writable_schema = ON",
            "INSERT INTO sqlite_schema(type, name, tbl_name, rootpage, sql) VALUES \
             ('table', 'MissingMod', 'MissingMod', 0, \
              'CREATE VIRTUAL TABLE MissingMod USING no_such_module_ever()')",
            "PRAGMA writable_schema = OFF",
        ] {
            now(conn.execute(sql)).expect(sql);
        }

        // Baseline model holds ManagedTbl (with ManagedIdx) and ManagedView.
        let mut base = Schema::default();
        let mut base_ids = IdsFile::default();
        let mut managed_tbl = table_with_cols(&["id"]);
        managed_tbl.indexes.insert(
            "ManagedIdx".into(),
            Index {
                columns: vec![IndexColumn::column("id")],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        let managed_uid = table_id(&mut base_ids, "ManagedTbl");
        col_id(&mut base_ids, "ManagedTbl", "id");
        base.tables
            .insert(TableName::new("main", "ManagedTbl"), managed_tbl);
        base.modules.insert(
            ModuleId::Named(TableName::new("main", "ManagedView")),
            *view_module("SELECT id FROM ManagedTbl"),
        );
        // 2. Whole-file collisions (unmanaged table/view/index, virtual table
        //    including missing-module, FTS5 shadow table, ledger table, sqlite_sequence)
        //    refused by `check_names_against_catalog` naming SQLite and both names.
        for (attempt_name, existing_name) in [
            ("managedtbl", "ManagedTbl"),
            ("managedview", "ManagedView"),
            ("managedidx", "ManagedIdx"),
            ("unmanagedtbl", "UnmanagedTbl"),
            ("unmanagedview", "UnmanagedView"),
            ("unmanagedexpridx", "UnmanagedExprIdx"),
            ("docsfts", "DocsFts"),
            ("docsfts_content", "DocsFts_content"),
            ("missingmod", "MissingMod"),
            ("__PBPS_STATE", "__pbps_state"),
            ("SQLITE_SEQUENCE", "sqlite_sequence"),
        ] {
            let mut desired = base.clone();
            let mut desired_ids = base_ids.clone();
            let new_uid = table_id(&mut desired_ids, attempt_name);
            col_id(&mut desired_ids, attempt_name, "x");
            desired.tables.insert(
                TableName::new("main", attempt_name),
                table_with_cols(&["x"]),
            );
            let mut plan = pbps_diff::order::propose(change_set(vec![Change::Table(
                TableChange::CreateTable {
                    uid: new_uid,
                    name: TableName::new("main", attempt_name),
                    table: Box::new(table_with_cols(&["x"])),
                    beside_default: None,
                    moves_at_most: None,
                },
            )]));
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                base.clone(),
                base_ids.clone(),
                "test",
            );
            let base_side = pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            };
            let desired_side = pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded: base_side,
                written: desired_side,
                base: base_side,
                declared: desired_side,
                hints: &hints,
                baseline: &baseline,
            };
            let mut state = PlanState::default();
            let err =
                now(dialect
                    .check_names_against_catalog(&mut state, &mut conn, &mut plan, frame, false))
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("SQLite") && msg.contains(attempt_name) && msg.contains(existing_name),
                "expected SQLite + {attempt_name} + {existing_name}, got: {msg}"
            );
        }

        // 3. Stray table at case-only rename's intermediate name (case-folded)
        //    is refused naming SQLite and both names.
        let case_rename = Change::Table(TableChange::RenameTable {
            uid: managed_uid.clone(),
            from: TableName::new("main", "ManagedTbl"),
            to: TableName::new("main", "managedtbl"),
            defaults: Vec::new(),
        });
        let inter = dialect
            .intermediate_tables(&case_rename, Strategy::default())
            .expect("intermediate table");
        assert_eq!(inter.len(), 1);
        let stray_upper = inter[0].name.to_ascii_uppercase();
        now(conn.execute(&format!("CREATE TABLE \"{stray_upper}\" (x INTEGER)")))
            .expect("create stray intermediate");
        {
            let mut desired = base.clone();
            let tbl = desired
                .tables
                .remove(&TableName::new("main", "ManagedTbl"))
                .expect("table");
            desired
                .tables
                .insert(TableName::new("main", "managedtbl"), tbl);
            let mut desired_ids = base_ids.clone();
            desired_ids
                .tables
                .insert(managed_uid.clone(), TableName::new("main", "managedtbl"));
            desired_ids.columns.insert(
                uid(pbps_model::UidKind::Column, "main.ManagedTbl.id"),
                TableName::new("main", "managedtbl").column("id"),
            );
            let mut plan = pbps_diff::order::propose(change_set(vec![case_rename]));
            let err = run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .err()
            .expect("stray intermediate table must be refused");
            let msg = err.to_string();
            assert!(
                msg.contains("SQLite")
                    && msg.contains(&inter[0].name)
                    && msg.contains(&stray_upper),
                "expected SQLite + intermediate + stray, got: {msg}"
            );
        }
        now(conn.execute(&format!("DROP TABLE \"{stray_upper}\""))).expect("drop stray");

        // 4. Negative-of-collision: trigger name `onlytrigger` and non-ASCII
        //    `cafÉ` (distinct from `café`) are NOT collisions.
        for free_name in ["onlytrigger", "cafÉ"] {
            let mut desired = base.clone();
            let mut desired_ids = base_ids.clone();
            let new_uid = table_id(&mut desired_ids, free_name);
            col_id(&mut desired_ids, free_name, "x");
            desired
                .tables
                .insert(TableName::new("main", free_name), table_with_cols(&["x"]));
            let mut plan = pbps_diff::order::propose(change_set(vec![Change::Table(
                TableChange::CreateTable {
                    uid: new_uid,
                    name: TableName::new("main", free_name),
                    table: Box::new(table_with_cols(&["x"])),
                    beside_default: None,
                    moves_at_most: None,
                },
            )]));
            run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .unwrap_or_else(|e| panic!("{free_name} should not collide: {e}"));
        }

        // Also creating a view colliding with an unmanaged table fails.
        let mut desired_v = base.clone();
        let v_id = ModuleId::Named(TableName::new("main", "unmanagedtbl"));
        desired_v
            .modules
            .insert(v_id.clone(), *view_module("SELECT 1"));
        let mut plan_v = pbps_diff::order::propose(change_set(vec![Change::Module(
            ModuleChange::CreateModule {
                id: v_id,
                module: view_module("SELECT 1"),
            },
        )]));
        let err_v = run_name_methods(
            &dialect,
            &mut conn,
            &mut plan_v,
            &base,
            &base_ids,
            &desired_v,
            &base_ids,
            &[],
        )
        .err()
        .expect("view colliding with unmanaged table must be refused");
        let msg_v = err_v.to_string();
        assert!(
            msg_v.contains("SQLite")
                && msg_v.contains("unmanagedtbl")
                && msg_v.contains("UnmanagedTbl"),
            "{msg_v}"
        );
    }

    #[test]
    fn key_constraint_name_equal_to_existing_table_or_index_is_not_a_collision() {
        use pbps_model::{Change, PrimaryKey, TableChange, UniqueConstraint};

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        now(conn.execute("CREATE TABLE Existing (id INTEGER PRIMARY KEY)")).expect("seed");
        now(conn.execute("CREATE INDEX ExistingIdx ON Existing (id)")).expect("seed idx");

        let mut base = Schema::default();
        let mut base_ids = IdsFile::default();
        table_id(&mut base_ids, "Existing");
        col_id(&mut base_ids, "Existing", "id");
        base.tables
            .insert(TableName::new("main", "Existing"), table_with_cols(&["id"]));

        let mut desired = base.clone();
        let mut desired_ids = base_ids.clone();
        let child_uid = table_id(&mut desired_ids, "child");
        col_id(&mut desired_ids, "child", "id");
        col_id(&mut desired_ids, "child", "code");
        let mut child = table_with_cols(&["id", "code"]);
        child.primary_key = Some(PrimaryKey {
            name: Some("Existing".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        });
        child.unique.insert(
            "ExistingIdx".into(),
            UniqueConstraint {
                columns: vec!["code".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        desired
            .tables
            .insert(TableName::new("main", "child"), child.clone());

        let mut plan =
            pbps_diff::order::propose(change_set(vec![Change::Table(TableChange::CreateTable {
                uid: child_uid,
                name: TableName::new("main", "child"),
                table: Box::new(child),
                beside_default: None,
                moves_at_most: None,
            })]));

        run_name_methods(
            &dialect,
            &mut conn,
            &mut plan,
            &base,
            &base_ids,
            &desired,
            &desired_ids,
            &[],
        )
        .expect("PK/UNIQUE names sharing an existing table/index name must pass on SQLite");
    }

    #[test]
    fn column_collisions_with_ordinary_and_generated_columns_are_refused() {
        use pbps_model::{Change, ColumnChange};

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        now(conn.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, Qty INTEGER, \
             DoubleQty INTEGER GENERATED ALWAYS AS (Qty * 2) VIRTUAL, \
             StoredQty INTEGER GENERATED ALWAYS AS (Qty * 3) STORED)",
        ))
        .expect("seed");

        let mut base = Schema::default();
        let mut base_ids = IdsFile::default();
        table_id(&mut base_ids, "items");
        col_id(&mut base_ids, "items", "id");
        let qty_uid = col_id(&mut base_ids, "items", "Qty");
        base.tables.insert(
            TableName::new("main", "items"),
            table_with_cols(&["id", "Qty"]),
        );

        // Adding a column colliding (case-folded) with an existing ordinary,
        // virtual generated, or stored generated column is refused.
        for (attempt, existing) in [
            ("qty", "Qty"),
            ("doubleqty", "DoubleQty"),
            ("storedqty", "StoredQty"),
        ] {
            let mut desired = base.clone();
            let mut desired_ids = base_ids.clone();
            let c_uid = col_id(&mut desired_ids, "items", attempt);
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .insert(attempt.into(), int_col());
            let mut plan = pbps_diff::order::propose(change_set(vec![Change::Column(
                ColumnChange::AddColumn {
                    uid: c_uid,
                    table: TableName::new("main", "items"),
                    name: attempt.into(),
                    column: Box::new(int_col()),
                },
            )]));
            let err = run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .err()
            .expect("column collision must be refused");
            let msg = err.to_string();
            assert!(
                msg.contains("SQLite") && msg.contains(attempt) && msg.contains(existing),
                "expected SQLite + {attempt} + {existing}, got: {msg}"
            );
        }

        // Renaming a column onto a hidden/generated column is refused.
        {
            let mut desired = base.clone();
            let tbl = desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items");
            tbl.columns.shift_remove("Qty");
            tbl.columns.insert("doubleqty".into(), int_col());
            let mut desired_ids = base_ids.clone();
            desired_ids.columns.insert(
                qty_uid.clone(),
                TableName::new("main", "items").column("doubleqty"),
            );
            let mut plan = pbps_diff::order::propose(change_set(vec![Change::Column(
                ColumnChange::RenameColumn {
                    uid: qty_uid,
                    table: TableName::new("main", "items"),
                    from: "Qty".into(),
                    to: "doubleqty".into(),
                    table_was: None,
                },
            )]));
            let err = run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .err()
            .expect("rename onto generated column must be refused");
            let msg = err.to_string();
            assert!(
                msg.contains("SQLite") && msg.contains("doubleqty") && msg.contains("DoubleQty"),
                "{msg}"
            );
        }
    }

    #[test]
    fn sqlite_name_handovers_require_complete_effects() {
        use pbps_model::{
            Change, ConstraintChange, ModuleChange, ModuleId, ModuleKind, PlannedChange,
            TableChange,
        };

        let table = TableName::new("main", "items");
        let reader = ModuleId::Named(TableName::new("main", "reader"));
        let replacement = ModuleId::Named(table.clone());
        let module = view_module("CREATE VIEW items AS SELECT 1");
        let mut base = plain_schema();
        base.modules.insert(
            reader.clone(),
            *view_module("CREATE VIEW reader AS SELECT id FROM items"),
        );
        let mut desired = Schema::default();
        desired
            .modules
            .insert(replacement.clone(), (*module).clone());
        let mut ids = IdsFile::default();
        let uid = table_id(&mut ids, "items");
        col_id(&mut ids, "items", "id");
        let desired_ids = IdsFile::default();
        let recorded = pbps_diff::Side {
            schema: &base,
            ids: &ids,
        };
        let written = pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        let baseline = StateSnapshot::new(
            pbps_model::StateKind::Baseline,
            pbps_sqlite::types::DIALECT,
            base.clone(),
            ids.clone(),
            "test",
        );
        let frame = Frame {
            label: "test\n.db",
            recorded,
            written,
            base: recorded,
            declared: written,
            hints: &Default::default(),
            baseline: &baseline,
        };
        let mut changes = change_set(vec![
            Change::Module(ModuleChange::CreateModule {
                id: replacement,
                module,
            }),
            Change::Table(TableChange::DropTable {
                uid,
                name: table,
                detach_from: None,
            }),
            Change::Module(ModuleChange::DropModule {
                id: reader,
                kind: ModuleKind::View,
            }),
        ]);
        let dialect = Sqlite::default();
        let facts = Default::default();
        let mut plan = pbps_diff::order::propose(changes.clone());
        order_name_handovers(&mut plan, frame, &dialect, &facts)
            .expect("fully read handover settles");
        // The effect edge removes the dependent view before the slot handover.
        assert_eq!(
            plan.changes[..],
            [
                changes.changes[2].clone(),
                changes.changes[1].clone(),
                changes.changes[0].clone()
            ]
        );

        changes.changes.push(PlannedChange::new(Change::Constraint(
            ConstraintChange::DropCheck {
                table: TableName::new("main", "unknown"),
                name: "ck".into(),
            },
        )));
        let mut plan = pbps_diff::order::propose(changes);
        let error = order_name_handovers(&mut plan, frame, &dialect, &facts)
            .expect_err("an unread step refuses SQLite handovers");
        assert_eq!(
            error.to_string(),
            r"SQLite cannot order these name handovers for `test\n.db`: unread step 3.0: no managed object is recorded under `main.unknown` (before change 3)"
        );
    }

    #[test]
    fn drop_then_create_and_rename_into_freed_name_are_ordered_and_swap_cycle_is_refused() {
        use pbps_model::{Change, TableChange};

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        now(conn.execute("CREATE TABLE Alpha (id INTEGER PRIMARY KEY)")).expect("Alpha");
        now(conn.execute("CREATE INDEX IxAlpha ON Alpha (id)")).expect("IxAlpha");
        now(conn.execute("CREATE TABLE Beta (id INTEGER PRIMARY KEY)")).expect("Beta");

        let mut base = Schema::default();
        let mut base_ids = IdsFile::default();
        let alpha_uid = table_id(&mut base_ids, "Alpha");
        col_id(&mut base_ids, "Alpha", "id");
        let beta_uid = table_id(&mut base_ids, "Beta");
        col_id(&mut base_ids, "Beta", "id");
        base.tables
            .insert(TableName::new("main", "Alpha"), table_with_cols(&["id"]));
        base.tables
            .insert(TableName::new("main", "Beta"), table_with_cols(&["id"]));

        // 1. Out-of-order case-folded drop-then-create: CreateTable `alpha` listed
        //    BEFORE DropTable `Alpha` (plus CreateTable `ixalpha` freed when `Alpha`
        //    drops its unmanaged index `IxAlpha`).
        {
            let mut desired = Schema::default();
            let mut desired_ids = IdsFile::default();
            desired_ids
                .tables
                .insert(beta_uid.clone(), TableName::new("main", "Beta"));
            col_id(&mut desired_ids, "Beta", "id");
            desired
                .tables
                .insert(TableName::new("main", "Beta"), table_with_cols(&["id"]));
            let new_alpha_uid = uid(pbps_model::UidKind::Table, "main.alpha.new");
            desired_ids
                .tables
                .insert(new_alpha_uid.clone(), TableName::new("main", "alpha"));
            col_id(&mut desired_ids, "alpha", "x");
            desired
                .tables
                .insert(TableName::new("main", "alpha"), table_with_cols(&["x"]));
            let new_ix_uid = table_id(&mut desired_ids, "ixalpha");
            col_id(&mut desired_ids, "ixalpha", "y");
            desired
                .tables
                .insert(TableName::new("main", "ixalpha"), table_with_cols(&["y"]));

            let mut plan = pbps_diff::order::propose(change_set(vec![
                Change::Table(TableChange::CreateTable {
                    uid: new_alpha_uid,
                    name: TableName::new("main", "alpha"),
                    table: Box::new(table_with_cols(&["x"])),
                    beside_default: None,
                    moves_at_most: None,
                }),
                Change::Table(TableChange::CreateTable {
                    uid: new_ix_uid,
                    name: TableName::new("main", "ixalpha"),
                    table: Box::new(table_with_cols(&["y"])),
                    beside_default: None,
                    moves_at_most: None,
                }),
                Change::Table(TableChange::DropTable {
                    uid: alpha_uid.clone(),
                    name: TableName::new("main", "Alpha"),
                    detach_from: None,
                }),
            ]));

            run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .expect("drop-then-create reorders and passes");

            assert!(
                matches!(
                    &plan.changes[0].change,
                    Change::Table(TableChange::DropTable { name, .. }) if name.name == "Alpha"
                ),
                "DropTable Alpha must move first: {:?}",
                plan.changes
            );
        }

        // 2. Out-of-order rename chain: `Beta -> alpha` listed BEFORE `Alpha -> Gamma`.
        {
            let mut desired = Schema::default();
            let mut desired_ids = IdsFile::default();
            desired_ids
                .tables
                .insert(alpha_uid.clone(), TableName::new("main", "Gamma"));
            desired_ids
                .tables
                .insert(beta_uid.clone(), TableName::new("main", "alpha"));
            col_id(&mut desired_ids, "Gamma", "id");
            col_id(&mut desired_ids, "alpha", "id");
            desired
                .tables
                .insert(TableName::new("main", "Gamma"), table_with_cols(&["id"]));
            desired
                .tables
                .insert(TableName::new("main", "alpha"), table_with_cols(&["id"]));

            let mut plan = pbps_diff::order::propose(change_set(vec![
                Change::Table(TableChange::RenameTable {
                    uid: beta_uid.clone(),
                    from: TableName::new("main", "Beta"),
                    to: TableName::new("main", "alpha"),
                    defaults: Vec::new(),
                }),
                Change::Table(TableChange::RenameTable {
                    uid: alpha_uid.clone(),
                    from: TableName::new("main", "Alpha"),
                    to: TableName::new("main", "Gamma"),
                    defaults: Vec::new(),
                }),
            ]));

            run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .expect("rename chain reorders and passes");

            assert!(
                matches!(
                    &plan.changes[0].change,
                    Change::Table(TableChange::RenameTable { from, to, .. })
                        if from.name == "Alpha" && to.name == "Gamma"
                ),
                "Alpha -> Gamma must move before Beta -> alpha: {:?}",
                plan.changes
            );
        }

        // 3. Negative: a case-folded table-rename swap (`Alpha -> beta`, `Beta -> alpha`)
        //    is a cycle and is refused naming SQLite.
        {
            let mut desired = Schema::default();
            let mut desired_ids = IdsFile::default();
            desired_ids
                .tables
                .insert(alpha_uid.clone(), TableName::new("main", "beta"));
            desired_ids
                .tables
                .insert(beta_uid.clone(), TableName::new("main", "alpha"));
            col_id(&mut desired_ids, "beta", "id");
            col_id(&mut desired_ids, "alpha", "id");
            desired
                .tables
                .insert(TableName::new("main", "beta"), table_with_cols(&["id"]));
            desired
                .tables
                .insert(TableName::new("main", "alpha"), table_with_cols(&["id"]));

            let mut plan = pbps_diff::order::propose(change_set(vec![
                Change::Table(TableChange::RenameTable {
                    uid: alpha_uid,
                    from: TableName::new("main", "Alpha"),
                    to: TableName::new("main", "beta"),
                    defaults: Vec::new(),
                }),
                Change::Table(TableChange::RenameTable {
                    uid: beta_uid,
                    from: TableName::new("main", "Beta"),
                    to: TableName::new("main", "alpha"),
                    defaults: Vec::new(),
                }),
            ]));

            let err = run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .err()
            .expect("table rename swap must be refused");
            assert!(err.to_string().contains("SQLite"), "{err}");
        }
    }

    #[test]
    fn column_rename_chain_is_ordered_and_case_folded_column_swap_is_refused() {
        use pbps_model::{Change, ColumnChange};

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        now(conn.execute("CREATE TABLE items (a INTEGER, b INTEGER)")).expect("seed");

        let mut base = Schema::default();
        let mut base_ids = IdsFile::default();
        let t_uid = table_id(&mut base_ids, "items");
        let a_uid = col_id(&mut base_ids, "items", "a");
        let b_uid = col_id(&mut base_ids, "items", "b");
        base.tables.insert(
            TableName::new("main", "items"),
            table_with_cols(&["a", "b"]),
        );

        // 1. Out-of-order case-folded chain: `a -> B` listed before `b -> c`.
        {
            let mut desired = Schema::default();
            let mut desired_ids = IdsFile::default();
            desired_ids
                .tables
                .insert(t_uid.clone(), TableName::new("main", "items"));
            desired_ids
                .columns
                .insert(a_uid.clone(), TableName::new("main", "items").column("B"));
            desired_ids
                .columns
                .insert(b_uid.clone(), TableName::new("main", "items").column("c"));
            desired.tables.insert(
                TableName::new("main", "items"),
                table_with_cols(&["B", "c"]),
            );

            let mut plan = pbps_diff::order::propose(change_set(vec![
                Change::Column(ColumnChange::RenameColumn {
                    uid: a_uid.clone(),
                    table: TableName::new("main", "items"),
                    from: "a".into(),
                    to: "B".into(),
                    table_was: None,
                }),
                Change::Column(ColumnChange::RenameColumn {
                    uid: b_uid.clone(),
                    table: TableName::new("main", "items"),
                    from: "b".into(),
                    to: "c".into(),
                    table_was: None,
                }),
            ]));

            run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .expect("column rename chain orders and passes");

            assert!(
                matches!(
                    &plan.changes[0].change,
                    Change::Column(ColumnChange::RenameColumn { from, to, .. })
                        if from.raw() == "b" && to.raw() == "c"
                ),
                "b -> c must run before a -> B: {:?}",
                plan.changes
            );
        }

        // 2. Negative: a case-folded swap (`a -> B`, `b -> A`) is refused by
        //    `object_order::order_column_renames`.
        {
            let mut desired = Schema::default();
            let mut desired_ids = IdsFile::default();
            desired_ids
                .tables
                .insert(t_uid, TableName::new("main", "items"));
            desired_ids
                .columns
                .insert(a_uid.clone(), TableName::new("main", "items").column("B"));
            desired_ids
                .columns
                .insert(b_uid.clone(), TableName::new("main", "items").column("A"));
            desired.tables.insert(
                TableName::new("main", "items"),
                table_with_cols(&["A", "B"]),
            );

            let mut plan = pbps_diff::order::propose(change_set(vec![
                Change::Column(ColumnChange::RenameColumn {
                    uid: a_uid,
                    table: TableName::new("main", "items"),
                    from: "a".into(),
                    to: "B".into(),
                    table_was: None,
                }),
                Change::Column(ColumnChange::RenameColumn {
                    uid: b_uid,
                    table: TableName::new("main", "items"),
                    from: "b".into(),
                    to: "A".into(),
                    table_was: None,
                }),
            ]));

            let err = run_name_methods(
                &dialect,
                &mut conn,
                &mut plan,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &[],
            )
            .err()
            .expect("column swap must be refused");
            assert!(
                err.to_string().contains("cycle")
                    || err.to_string().contains("two separate plans")
                    || err.to_string().contains("rename"),
                "{err}"
            );
        }
    }

    #[test]
    fn unreadable_inventory_returns_error_and_never_empty() {
        use pbps_model::{Change, TableChange};

        let (tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        now(conn.execute("PRAGMA busy_timeout = 0")).expect("busy_timeout=0");

        let locker =
            pbps_db::sqlite::Connection::open(tmp.path(), pbps_db::sqlite::OpenMode::ExistingOnly)
                .expect("second connection");
        locker.execute("BEGIN EXCLUSIVE", &[]).expect("lock file");

        let base = Schema::default();
        let base_ids = IdsFile::default();
        let mut desired = Schema::default();
        let mut desired_ids = IdsFile::default();
        let t_uid = table_id(&mut desired_ids, "t");
        col_id(&mut desired_ids, "t", "id");
        desired
            .tables
            .insert(TableName::new("main", "t"), table_with_cols(&["id"]));
        let mut plan =
            pbps_diff::order::propose(change_set(vec![Change::Table(TableChange::CreateTable {
                uid: t_uid,
                name: TableName::new("main", "t"),
                table: Box::new(table_with_cols(&["id"])),
                beside_default: None,
                moves_at_most: None,
            })]));

        let hints = pbps_model::Hints::default();
        let baseline = StateSnapshot::new(
            pbps_model::StateKind::Baseline,
            pbps_sqlite::types::DIALECT,
            base.clone(),
            base_ids.clone(),
            "test",
        );
        let base_side = pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        };
        let desired_side = pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        let frame = Frame {
            label: "test.db",
            recorded: base_side,
            written: desired_side,
            base: base_side,
            declared: desired_side,
            hints: &hints,
            baseline: &baseline,
        };

        let mut state = PlanState::default();
        let check_err = now(
            dialect.check_names_against_catalog(&mut state, &mut conn, &mut plan, frame, false)
        )
        .unwrap_err();
        assert!(
            state.inventory.is_none(),
            "failed read must not cache an inventory: {check_err}"
        );

        let order_err = now(dialect.order_by_catalog(&mut state, &mut conn, &mut plan, frame))
            .expect_err("locked file must fail order_by_catalog too");
        assert!(state.inventory.is_none(), "{order_err}");

        locker.execute("ROLLBACK", &[]).expect("unlock");
    }

    #[test]
    fn sqlite_plan_session_positions_pass_for_every_supported_change_kind_and_pin_remaining_refusals()
     {
        use pbps_model::{
            Change, CheckConstraint, Column, ColumnChange, ConstraintChange, ForeignKey, Index,
            IndexChange, IndexColumn, ModuleChange, ModuleId, ModuleKind, PrimaryKey, RoleName,
            TableChange, UniqueConstraint, ValueSource,
        };

        let dialect = Sqlite::default();

        // Run every PlanSession position in `plan_db.rs` order and return
        // `Ok(())` or `Err((position_name, error_message))`.
        let run_all_planned = |seed_sql: &[&str],
                               base: &Schema,
                               base_ids: &IdsFile,
                               desired: &Schema,
                               desired_ids: &IdsFile,
                               changes: Vec<pbps_model::PlannedChange>|
         -> Result<(), (&'static str, String)> {
            let (_tmp, mut conn) = TempDb::create();
            for sql in seed_sql {
                now(conn.execute(sql)).expect(sql);
            }
            let scoped = scoped_of(base, base_ids);
            let mut plan = pbps_diff::order::propose(ChangeSet { changes });
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                base.clone(),
                base_ids.clone(),
                "test",
            );
            let base_side = pbps_diff::Side {
                schema: base,
                ids: base_ids,
            };
            let desired_side = pbps_diff::Side {
                schema: desired,
                ids: desired_ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded: base_side,
                written: desired_side,
                base: base_side,
                declared: desired_side,
                hints: &hints,
                baseline: &baseline,
            };
            let mut state = PlanState::default();

            dialect
                .refuse_occupied_relation_names(&plan, &scoped, &[], &[], "test.db")
                .map_err(|e| ("refuse_occupied_relation_names", e.to_string()))?;
            dialect
                .require_transactional_rebuilds(&plan, false)
                .map_err(|e| ("require_transactional_rebuilds", e.to_string()))?;
            now(dialect.check_names_against_catalog(&mut state, &mut conn, &mut plan, frame, false))
                .map_err(|e| ("check_names_against_catalog", e.to_string()))?;
            let reads = now(dialect.read_before_schedule(&mut conn, &plan, desired, base_ids))
                .map_err(|e| ("read_before_schedule", e.to_string()))?;
            let rediff =
                |_also: &BTreeSet<ModuleId>| -> anyhow::Result<PlanningPlan<pbps_diff::order::Draft>> {
                    anyhow::bail!("unexpected rediff")
                };
            let (mut plan, _deps) = now(dialect.schedule(
                &mut state,
                &mut conn,
                plan,
                frame,
                reads.role_members.as_ref(),
                &rediff,
            ))
            .map_err(|e| ("schedule", e.to_string()))?;
            let ordered = now(dialect.order_by_catalog(&mut state, &mut conn, &mut plan, frame))
                .map_err(|e| ("order_by_catalog", e.to_string()))?;
            #[cfg(debug_assertions)]
            if !ordered.verified {
                pbps_diff::order::shadow::observe_scheduled(
                    base_side,
                    desired_side,
                    &dialect,
                    &hints.annotations.modules(desired),
                    &plan,
                    pbps_diff::order::shadow::Stage::Final,
                    ordered.facts.as_ref(),
                    &Default::default(),
                )
                .map_err(|e| ("observe_scheduled", e.to_string()))?;
            }
            now(dialect.check_final_order(&state, &mut conn, &plan, frame))
                .map_err(|e| ("check_final_order", e.to_string()))?;
            let plan_names = pbps_model::plan_names::PlanNames::of(&plan.changes, desired_ids)
                .map_err(|e| ("plan_names", e.to_string()))?;
            now(dialect.vet_plan_sites(&state, &mut conn, &plan_names, &mut |_| {}))
                .map_err(|e| ("vet_plan_sites", e.to_string()))?;
            let _ = now(dialect.probe_stranded_rows(&state, &mut conn, &plan, frame, None))
                .map_err(|e| ("probe_stranded_rows", e.to_string()))?;
            let owners = BTreeMap::new();
            let role = RoleName::new("");
            now(dialect.support_rows(
                &mut conn,
                &plan,
                desired_ids,
                SupportReads {
                    owners: &owners,
                    session_role: &role,
                    unrevocable: &[],
                },
            ))
            .map_err(|e| ("support_rows", e.to_string()))?;
            now(dialect.server_limits(&mut conn, &plan))
                .map_err(|e| ("server_limits", e.to_string()))?;
            Ok(())
        };
        let run_all_positions = |seed_sql: &[&str],
                                 base: &Schema,
                                 base_ids: &IdsFile,
                                 desired: &Schema,
                                 desired_ids: &IdsFile,
                                 changes: Vec<Change>|
         -> Result<(), (&'static str, String)> {
            run_all_planned(
                seed_sql,
                base,
                base_ids,
                desired,
                desired_ids,
                change_set(changes).changes,
            )
        };

        // Shared baseline with parent & items tables.
        let mut base = Schema::default();
        let mut base_ids = IdsFile::default();
        let parent_uid = table_id(&mut base_ids, "parent");
        col_id(&mut base_ids, "parent", "id");
        let items_uid = table_id(&mut base_ids, "items");
        col_id(&mut base_ids, "items", "id");
        let qty_uid = col_id(&mut base_ids, "items", "qty");
        base.tables
            .insert(TableName::new("main", "parent"), table_with_cols(&["id"]));
        let mut items_tbl = table_with_cols(&["id", "qty"]);
        items_tbl.checks.insert(
            "ck_pos".into(),
            CheckConstraint {
                expression: "qty >= 0".into(),
            },
        );
        items_tbl.foreign_keys.insert(
            "fk_parent".into(),
            ForeignKey {
                columns: vec!["qty".into()],
                references_table: TableName::new("main", "parent"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        base.tables
            .insert(TableName::new("main", "items"), items_tbl);
        let seed = [
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
        ];

        // 1. Create table (with PK, UNIQUE, CHECK, DEFAULT, FK).
        {
            let mut desired = base.clone();
            let mut desired_ids = base_ids.clone();
            let u = table_id(&mut desired_ids, "orders");
            col_id(&mut desired_ids, "orders", "id");
            col_id(&mut desired_ids, "orders", "code");
            let mut t = table_with_cols(&["id", "code"]);
            t.primary_key = Some(PrimaryKey {
                name: Some("pk_orders".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            });
            t.unique.insert(
                "uq_orders".into(),
                UniqueConstraint {
                    columns: vec!["code".into()],
                    storage_parameters: Default::default(),
                    filegroup: None,
                },
            );
            t.checks.insert(
                "ck_code".into(),
                CheckConstraint {
                    expression: "code > 0".into(),
                },
            );
            desired
                .tables
                .insert(TableName::new("main", "orders"), t.clone());
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                vec![Change::Table(TableChange::CreateTable {
                    uid: u,
                    name: TableName::new("main", "orders"),
                    table: Box::new(t),
                    beside_default: None,
                    moves_at_most: None,
                })],
            )
            .expect("create table passes every PlanSession position");
        }

        // 2. Create view.
        {
            let mut desired = base.clone();
            let v = ModuleId::Named(TableName::new("main", "v_items"));
            desired
                .modules
                .insert(v.clone(), *view_module("SELECT id FROM items"));
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &base_ids,
                vec![Change::Module(ModuleChange::CreateModule {
                    id: v,
                    module: view_module("SELECT id FROM items"),
                })],
            )
            .expect("create view passes every PlanSession position");
        }

        // 3. Add index (with partial WHERE).
        {
            let mut desired = base.clone();
            let idx = Index {
                columns: vec![IndexColumn::column("qty")],
                include: Vec::new(),
                unique: false,
                filter: Some("qty > 0".into()),
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            };
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .indexes
                .insert("ix_qty".into(), idx.clone());
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &base_ids,
                vec![Change::Index(IndexChange::AddIndex {
                    table: TableName::new("main", "items"),
                    name: "ix_qty".into(),
                    index: Box::new(idx),
                    clustered: false,
                })],
            )
            .expect("add index passes every PlanSession position");
        }

        // 4. Add column with constant default.
        {
            let mut desired = base.clone();
            let mut desired_ids = base_ids.clone();
            let c_uid = col_id(&mut desired_ids, "items", "status");
            let mut col = Column::new("integer".parse().expect("type"));
            col.nullable = false;
            col.value_source = ValueSource::Default("0".into());
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .insert("status".into(), col.clone());
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                vec![Change::Column(ColumnChange::AddColumn {
                    uid: c_uid,
                    table: TableName::new("main", "items"),
                    name: "status".into(),
                    column: Box::new(col),
                })],
            )
            .expect("add column with constant default passes every PlanSession position");
        }

        // 5. Add check & drop check.
        {
            let mut desired = base.clone();
            let ck = CheckConstraint {
                expression: "qty < 100".into(),
            };
            let tbl = desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items");
            tbl.checks.remove("ck_pos");
            tbl.checks.insert("ck_max".into(), ck.clone());
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &base_ids,
                vec![
                    Change::Constraint(ConstraintChange::AddCheck {
                        table: TableName::new("main", "items"),
                        name: "ck_max".into(),
                        constraint: ck,
                    }),
                    Change::Constraint(ConstraintChange::DropCheck {
                        table: TableName::new("main", "items"),
                        name: "ck_pos".into(),
                    }),
                ],
            )
            .expect("add/drop check passes every PlanSession position");
        }

        // 6. Drop foreign key.
        {
            let mut desired = base.clone();
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .foreign_keys
                .remove("fk_parent");
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &base_ids,
                vec![Change::Constraint(ConstraintChange::DropForeignKey {
                    table: TableName::new("main", "items"),
                    name: "fk_parent".into(),
                })],
            )
            .expect("drop foreign key passes every PlanSession position");
        }

        // 7. Alter column nullability.
        {
            let mut desired = base.clone();
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .get_mut("qty")
                .expect("qty")
                .nullable = false;
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &base_ids,
                vec![Change::Column(ColumnChange::AlterColumnNullability {
                    uid: qty_uid.clone(),
                    column: TableName::new("main", "items").column("qty"),
                    ty: "integer".parse().expect("type"),
                    to_nullable: false,
                    collation: None,
                    through_external_partitions: false,
                })],
            )
            .expect("alter column nullability passes every PlanSession position");
        }

        // 8. Rename table (ordinary + case-only).
        for target_name in ["records", "ITEMS"] {
            let mut desired = base.clone();
            let tbl = desired
                .tables
                .remove(&TableName::new("main", "items"))
                .expect("items");
            desired
                .tables
                .insert(TableName::new("main", target_name), tbl);
            let mut desired_ids = base_ids.clone();
            desired_ids
                .tables
                .insert(items_uid.clone(), TableName::new("main", target_name));
            desired_ids.columns.insert(
                uid(pbps_model::UidKind::Column, "main.items.id"),
                TableName::new("main", target_name).column("id"),
            );
            desired_ids.columns.insert(
                qty_uid.clone(),
                TableName::new("main", target_name).column("qty"),
            );
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                vec![Change::Table(TableChange::RenameTable {
                    uid: items_uid.clone(),
                    from: TableName::new("main", "items"),
                    to: TableName::new("main", target_name),
                    defaults: Vec::new(),
                })],
            )
            .unwrap_or_else(|e| panic!("rename table to {target_name} should pass: {e:?}"));
        }

        // 9. Rename column.
        {
            let mut desired = base.clone();
            let tbl = desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items");
            let c = tbl.columns.shift_remove("qty").expect("qty");
            tbl.columns.insert("amount".into(), c);
            let mut desired_ids = base_ids.clone();
            desired_ids.columns.insert(
                qty_uid.clone(),
                TableName::new("main", "items").column("amount"),
            );
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                vec![Change::Column(ColumnChange::RenameColumn {
                    uid: qty_uid.clone(),
                    table: TableName::new("main", "items"),
                    from: "qty".into(),
                    to: "amount".into(),
                    table_was: None,
                })],
            )
            .expect("rename column passes every PlanSession position");
        }

        // 10. Drop view (alone and with a managed reader) now passes EVERY
        //     PlanSession position!
        {
            let mut base_v = base.clone();
            let v_id = ModuleId::Named(TableName::new("main", "v_old"));
            base_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT 1"));
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE VIEW v_old AS SELECT 1",
                ],
                &base_v,
                &base_ids,
                &base,
                &base_ids,
                vec![Change::Module(ModuleChange::DropModule {
                    id: v_id,
                    kind: ModuleKind::View,
                })],
            )
            .expect("drop view alone passes every PlanSession position");
        }
        {
            let mut base_v = base.clone();
            let v_id = ModuleId::Named(TableName::new("main", "v_old"));
            let u_id = ModuleId::Named(TableName::new("main", "u_reads_v"));
            base_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT id FROM items"));
            base_v
                .modules
                .insert(u_id.clone(), *view_module("SELECT id FROM v_old"));
            // Even given [DropModule(v_old), DropModule(u_reads_v)], `schedule`
            // reorders `u_reads_v` ahead of `v_old` and all positions pass.
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE VIEW v_old AS SELECT id FROM items",
                    "CREATE VIEW u_reads_v AS SELECT id FROM v_old",
                ],
                &base_v,
                &base_ids,
                &base,
                &base_ids,
                vec![
                    Change::Module(ModuleChange::DropModule {
                        id: v_id,
                        kind: ModuleKind::View,
                    }),
                    Change::Module(ModuleChange::DropModule {
                        id: u_id,
                        kind: ModuleKind::View,
                    }),
                ],
            )
            .expect("drop view with dropped managed reader passes every PlanSession position");
        }

        // 11. Drop table, drop column, drop index, and edit view (alone and
        //     with managed readers) now pass EVERY PlanSession position!
        // - DropTable alone
        {
            let mut desired = base.clone();
            desired.tables.remove(&TableName::new("main", "parent"));
            let mut desired_ids = base_ids.clone();
            desired_ids.tables.remove(&parent_uid);
            desired_ids
                .columns
                .remove(&uid(pbps_model::UidKind::Column, "main.parent.id"));
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                vec![Change::Table(TableChange::DropTable {
                    uid: parent_uid.clone(),
                    name: TableName::new("main", "parent"),
                    detach_from: None,
                })],
            )
            .expect("drop table alone passes every PlanSession position");
        }
        // - DropTable with managed reader view dropped first
        {
            let mut base_v = base.clone();
            let v_id = ModuleId::Named(TableName::new("main", "v_parent"));
            base_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT id FROM parent"));
            let mut desired = base.clone();
            desired.tables.remove(&TableName::new("main", "parent"));
            let mut desired_ids = base_ids.clone();
            desired_ids.tables.remove(&parent_uid);
            desired_ids
                .columns
                .remove(&uid(pbps_model::UidKind::Column, "main.parent.id"));
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE VIEW v_parent AS SELECT id FROM parent",
                ],
                &base_v,
                &base_ids,
                &desired,
                &desired_ids,
                vec![
                    Change::Module(ModuleChange::DropModule {
                        id: v_id,
                        kind: ModuleKind::View,
                    }),
                    Change::Table(TableChange::DropTable {
                        uid: parent_uid.clone(),
                        name: TableName::new("main", "parent"),
                        detach_from: None,
                    }),
                ],
            )
            .expect(
                "drop table with managed reader dropped first passes every PlanSession position",
            );
        }
        // - DropColumn alone
        {
            let mut desired = base.clone();
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .shift_remove("qty");
            let mut desired_ids = base_ids.clone();
            desired_ids
                .columns
                .remove(&uid(pbps_model::UidKind::Column, "main.items.qty"));
            run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                vec![Change::Column(ColumnChange::DropColumn {
                    uid: uid(pbps_model::UidKind::Column, "main.items.qty"),
                    column: TableName::new("main", "items").column("qty"),
                })],
            )
            .expect("drop column alone passes every PlanSession position");
        }
        // - DropColumn with managed reader view dropped first
        {
            let mut base_v = base.clone();
            let v_id = ModuleId::Named(TableName::new("main", "v_qty"));
            base_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT qty FROM items"));
            let mut desired = base.clone();
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .shift_remove("qty");
            let mut desired_ids = base_ids.clone();
            desired_ids
                .columns
                .remove(&uid(pbps_model::UidKind::Column, "main.items.qty"));
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE VIEW v_qty AS SELECT qty FROM items",
                ],
                &base_v,
                &base_ids,
                &desired,
                &desired_ids,
                vec![
                    Change::Module(ModuleChange::DropModule {
                        id: v_id,
                        kind: ModuleKind::View,
                    }),
                    Change::Column(ColumnChange::DropColumn {
                        uid: uid(pbps_model::UidKind::Column, "main.items.qty"),
                        column: TableName::new("main", "items").column("qty"),
                    }),
                ],
            )
            .expect(
                "drop column with managed reader dropped first passes every PlanSession position",
            );
        }
        // - DropIndex
        {
            let mut base_ix = base.clone();
            base_ix
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .indexes
                .insert(
                    "ix_qty".into(),
                    Index {
                        columns: vec![IndexColumn::column("qty")],
                        include: Vec::new(),
                        unique: false,
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                        filegroup: None,
                    },
                );
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE INDEX ix_qty ON items(qty)",
                ],
                &base_ix,
                &base_ids,
                &base,
                &base_ids,
                vec![Change::Index(IndexChange::DropIndex {
                    table: TableName::new("main", "items"),
                    name: "ix_qty".into(),
                })],
            )
            .expect("drop index passes every PlanSession position");
        }
        // - AlterModule (View) alone
        {
            let mut base_v = base.clone();
            let v_id = ModuleId::Named(TableName::new("main", "v_edit"));
            base_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT 1"));
            let mut desired_v = base.clone();
            desired_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT 2"));
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE VIEW v_edit AS SELECT 1",
                ],
                &base_v,
                &base_ids,
                &desired_v,
                &base_ids,
                vec![Change::Module(ModuleChange::AlterModule {
                    id: v_id,
                    module: view_module("SELECT 2"),
                })],
            )
            .expect("edit view alone passes every PlanSession position");
        }
        // - AlterModule (View) with managed reader woven by `schedule`
        {
            let mut base_v = base.clone();
            let v_id = ModuleId::Named(TableName::new("main", "v_edit"));
            let u_id = ModuleId::Named(TableName::new("main", "u_reader"));
            base_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT id FROM items"));
            base_v
                .modules
                .insert(u_id.clone(), *view_module("SELECT id FROM v_edit"));
            let mut desired_v = base_v.clone();
            desired_v
                .modules
                .insert(v_id.clone(), *view_module("SELECT id + 1 AS id FROM items"));
            run_all_positions(
                &[
                    "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                    "CREATE VIEW v_edit AS SELECT id FROM items",
                    "CREATE VIEW u_reader AS SELECT id FROM v_edit",
                ],
                &base_v,
                &base_ids,
                &desired_v,
                &base_ids,
                vec![Change::Module(ModuleChange::AlterModule {
                    id: v_id,
                    module: view_module("SELECT id + 1 AS id FROM items"),
                })],
            )
            .expect("edit view with managed reader passes every PlanSession position");
        }

        // 12. Pin what still refuses and at which position:
        // - Rebuild-requiring changes (AlterColumnType, DropUnique, SetPrimaryKey { from: Some(_) })
        //   -> `check_final_order` (drop_blockers).
        {
            let mut desired = base.clone();
            desired
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .get_mut("qty")
                .expect("qty")
                .ty = "text".parse().expect("type");
            let (pos, err) = run_all_positions(
                &seed,
                &base,
                &base_ids,
                &desired,
                &base_ids,
                vec![Change::Column(ColumnChange::AlterColumnType {
                    uid: uid(pbps_model::UidKind::Column, "main.items.qty"),
                    column: TableName::new("main", "items").column("qty"),
                    from: "integer".parse().expect("type"),
                    to: "text".parse().expect("type"),
                    from_nullable: true,
                    to_nullable: true,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: None,
                })],
            )
            .unwrap_err();
            assert_eq!(pos, "check_final_order", "{err}");
            assert!(err.contains("SQLite") && err.contains("rebuild"), "{err}");
        }
        // - Guarded drops (`drop-table` with one requirement, `drop-table` with several
        //   requirements, and `drop-column`) now pass every PlanSession position.
        {
            let mk_guard =
                |table_uid: &pbps_model::Uid, table: &str, reqs: &[(&pbps_model::Uid, &str)]| {
                    pbps_model::DropGuard {
                        table_uid: table_uid.clone(),
                        table: TableName::new("main", table),
                        detach_from: None,
                        requirements: reqs
                            .iter()
                            .map(|(u, pred)| {
                                (
                                    (*u).clone(),
                                    pbps_model::DropRequirement {
                                        table_uid: table_uid.clone(),
                                        predicate: (*pred).into(),
                                        required_at: "2026-01-01T00:00:00Z".into(),
                                        operator: "test".into(),
                                    },
                                )
                            })
                            .collect(),
                    }
                };
            let mut desired_drop_items = base.clone();
            desired_drop_items
                .tables
                .remove(&TableName::new("main", "items"));
            let mut after_drop_items_ids = base_ids.clone();
            after_drop_items_ids.tables.remove(&items_uid);
            after_drop_items_ids
                .columns
                .retain(|_, c| c.table != TableName::new("main", "items"));

            for (label, reqs) in [
                ("one requirement", vec![(&items_uid, "qty >= 0")]),
                (
                    "several requirements",
                    vec![
                        (&items_uid, "qty >= 0"),
                        (&qty_uid, "id > 0 AND qty IS NOT NULL"),
                    ],
                ),
            ] {
                let mut pc =
                    pbps_model::PlannedChange::new(Change::Table(TableChange::DropTable {
                        uid: items_uid.clone(),
                        name: TableName::new("main", "items"),
                        detach_from: None,
                    }));
                pc.drop_guard = Some(mk_guard(&items_uid, "items", &reqs));
                run_all_planned(
                    &seed,
                    &base,
                    &base_ids,
                    &desired_drop_items,
                    &after_drop_items_ids,
                    vec![pc],
                )
                .unwrap_or_else(|e| panic!("guarded drop-table ({label}) failed: {e:?}"));
            }

            let mut desired_drop_qty = base.clone();
            desired_drop_qty
                .tables
                .get_mut(&TableName::new("main", "items"))
                .expect("items")
                .columns
                .shift_remove("qty");
            let mut after_drop_qty_ids = base_ids.clone();
            after_drop_qty_ids.columns.remove(&qty_uid);
            let mut drop_col =
                pbps_model::PlannedChange::new(Change::Column(ColumnChange::DropColumn {
                    uid: qty_uid.clone(),
                    column: TableName::new("main", "items").column("qty"),
                }));
            drop_col.drop_guard = Some(mk_guard(
                &items_uid,
                "items",
                &[(&qty_uid, "id > 0 AND qty = 0")],
            ));
            run_all_planned(
                &seed,
                &base,
                &base_ids,
                &desired_drop_qty,
                &after_drop_qty_ids,
                vec![drop_col],
            )
            .expect("guarded drop-column passes every PlanSession position");

            // Negative: a guarded drop whose predicate calls a disallowed function fails at `vet_plan_sites`.
            let mut bad_guard =
                pbps_model::PlannedChange::new(Change::Table(TableChange::DropTable {
                    uid: items_uid.clone(),
                    name: TableName::new("main", "items"),
                    detach_from: None,
                }));
            bad_guard.drop_guard = Some(mk_guard(
                &items_uid,
                "items",
                &[(&items_uid, "random() > 0")],
            ));
            let (pos, err) = run_all_planned(
                &seed,
                &base,
                &base_ids,
                &desired_drop_items,
                &after_drop_items_ids,
                vec![bad_guard],
            )
            .unwrap_err();
            assert_eq!(pos, "vet_plan_sites", "{err}");
            assert!(err.contains("calls function `random`"), "{err}");
        }
    }

    #[test]
    fn schedule_passes_for_unreferenced_view_edit_and_drop_and_rebuilds_managed_readers_in_executable_order()
     {
        use pbps_model::{Change, ModuleChange, ModuleId, ModuleKind};

        let dialect = Sqlite::default();
        let rediff =
            |_also: &BTreeSet<ModuleId>| -> anyhow::Result<PlanningPlan<pbps_diff::order::Draft>> {
                anyhow::bail!("unexpected rediff")
            };

        let run_schedule = |conn: &mut Conn,
                            base: &Schema,
                            desired: &Schema,
                            changes: Vec<Change>|
         -> anyhow::Result<(
            PlanningPlan<pbps_diff::order::Draft>,
            ConnectedCheck,
        )> {
            let ids = IdsFile::default();
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                base.clone(),
                ids.clone(),
                "test",
            );
            let base_side = pbps_diff::Side {
                schema: base,
                ids: &ids,
            };
            let desired_side = pbps_diff::Side {
                schema: desired,
                ids: &ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded: base_side,
                written: desired_side,
                base: base_side,
                declared: desired_side,
                hints: &hints,
                baseline: &baseline,
            };
            let mut state = PlanState::default();
            let plan = pbps_diff::order::propose(change_set(changes));
            let (mut scheduled, check) =
                now(dialect.schedule(&mut state, conn, plan, frame, None, &rediff))?;
            let ordered = now(dialect.order_by_catalog(&mut state, conn, &mut scheduled, frame))?;
            #[cfg(debug_assertions)]
            if !ordered.verified {
                pbps_diff::order::shadow::observe_scheduled(
                    base_side,
                    desired_side,
                    &dialect,
                    &hints.annotations.modules(desired),
                    &scheduled,
                    pbps_diff::order::shadow::Stage::Final,
                    ordered.facts.as_ref(),
                    &Default::default(),
                )?;
            }
            Ok((scheduled, check))
        };

        // 1. Editing and dropping a view nothing reads both pass.
        {
            let (_tmp, mut conn) = TempDb::create();
            for sql in [
                "CREATE TABLE items (id INTEGER PRIMARY KEY)",
                "CREATE VIEW solo AS SELECT id FROM items",
            ] {
                now(conn.execute(sql)).expect(sql);
            }
            let solo = ModuleId::Named(TableName::new("main", "solo"));
            let mut base = Schema::default();
            base.modules
                .insert(solo.clone(), *view_module("SELECT id FROM items"));

            let mut edited = Schema::default();
            edited
                .modules
                .insert(solo.clone(), *view_module("SELECT id + 1 AS id FROM items"));
            let (plan, check) = run_schedule(
                &mut conn,
                &base,
                &edited,
                vec![Change::Module(ModuleChange::AlterModule {
                    id: solo.clone(),
                    module: view_module("SELECT id + 1 AS id FROM items"),
                })],
            )
            .expect("unreferenced edit passes");
            assert_eq!(check.engine, "SQLite");
            assert_eq!(check.status, "passed");
            assert_eq!(plan.changes.len(), 1);

            let (drop_plan, drop_check) = run_schedule(
                &mut conn,
                &base,
                &Schema::default(),
                vec![Change::Module(ModuleChange::DropModule {
                    id: solo,
                    kind: ModuleKind::View,
                })],
            )
            .expect("unreferenced drop passes");
            assert_eq!(drop_check.status, "passed");
            assert_eq!(drop_plan.changes.len(), 1);
        }

        // 2. Managed direct + transitive readers (`u -> w -> v`) are rebuilt
        //    around an edit of `v` in an order SQLite accepts and executes.
        {
            let (_tmp, mut conn) = TempDb::create();
            for sql in [
                "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
                "INSERT INTO items VALUES (1, 10)",
                "CREATE VIEW v AS SELECT id, qty FROM items",
                "CREATE VIEW w AS SELECT id, qty FROM v",
                "CREATE VIEW u AS SELECT qty FROM w",
            ] {
                now(conn.execute(sql)).expect(sql);
            }
            let v = ModuleId::Named(TableName::new("main", "v"));
            let w = ModuleId::Named(TableName::new("main", "w"));
            let u = ModuleId::Named(TableName::new("main", "u"));
            let mut base = Schema::default();
            base.modules
                .insert(v.clone(), *view_module("SELECT id, qty FROM items"));
            base.modules
                .insert(w.clone(), *view_module("SELECT id, qty FROM v"));
            base.modules
                .insert(u.clone(), *view_module("SELECT qty FROM w"));
            let mut desired = base.clone();
            desired.modules.insert(
                v.clone(),
                *view_module("SELECT id, qty * 2 AS qty FROM items"),
            );

            let ids = IdsFile::default();
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                base.clone(),
                ids.clone(),
                "test",
            );
            let base_side = pbps_diff::Side {
                schema: &base,
                ids: &ids,
            };
            let desired_side = pbps_diff::Side {
                schema: &desired,
                ids: &ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded: base_side,
                written: desired_side,
                base: base_side,
                declared: desired_side,
                hints: &hints,
                baseline: &baseline,
            };
            let diff_plan =
                pbps_diff::diff(base_side, desired_side, &dialect, &hints).expect("diff");
            let mut state = PlanState::default();
            let rediff_diff =
                |_also: &BTreeSet<ModuleId>| -> anyhow::Result<pbps_diff::order::DiffPlan> {
                    anyhow::bail!("unexpected rediff")
                };
            let (mut scheduled, check) =
                now(dialect.schedule(&mut state, &mut conn, diff_plan, frame, None, &rediff_diff))
                    .expect("managed transitive readers woven around edit");
            let ordered_check =
                now(dialect.order_by_catalog(&mut state, &mut conn, &mut scheduled, frame))
                    .expect("order_by_catalog");
            use pbps_diff::order::finish;
            let finished = finish::connected(
                scheduled,
                finish::Inputs {
                    before: base_side,
                    after: desired_side,
                    modules: &hints.annotations.modules(&desired),
                    captured: &Default::default(),
                },
                &dialect,
                finish::Catalog {
                    slots: ordered_check.facts.as_ref(),
                    verified: ordered_check.verified,
                },
            )
            .expect("the scheduled plan is observed");
            assert_eq!(check.engine, "SQLite");
            assert_eq!(check.status, "passed");
            let order: Vec<(&str, String)> = finished
                .changes
                .iter()
                .map(|p| {
                    let Change::Module(m) = &p.change else {
                        panic!("unexpected change {:?}", p.change);
                    };
                    match m {
                        ModuleChange::DropModule { id, .. } => ("drop", id.shown().to_string()),
                        ModuleChange::AlterModule { id, .. } => ("alter", id.shown().to_string()),
                        ModuleChange::CreateModule { id, .. } => ("create", id.shown().to_string()),
                    }
                })
                .collect();
            assert_eq!(
                order,
                vec![
                    ("drop", "main.u".to_owned()),
                    ("drop", "main.w".to_owned()),
                    ("alter", "main.v".to_owned()),
                    ("create", "main.w".to_owned()),
                    ("create", "main.u".to_owned()),
                ]
            );
            // Lower through the real certified emitter and execute every statement on the file.
            let certified = finished.ordered(pbps_model::PlanMode::Transactional, &dialect);
            let lowered = crate::emission::Engine::lower(&dialect, &certified).expect("lower");
            let c = sqlite_conn(&mut conn).expect("sqlite conn");
            for stmt in lowered.statements() {
                c.execute_batch(&stmt.sql).expect(&stmt.sql);
            }
            let rows = c.query("SELECT qty FROM u", &[]).expect("select u");
            assert_eq!(rows[0].integer(0).expect("qty"), 20);
        }
    }

    #[test]
    fn schedule_refuses_unmanaged_readers_transitive_readers_triggers_unreadable_file_and_foreign_plans()
     {
        use pbps_model::{
            Change, ModuleChange, ModuleId, ModuleKind, PartitionChange, Rebuild,
            RebuildIdentities, RebuildPartitionedTable, RoleChange, RoleName, TypeValueChange,
        };

        let dialect = Sqlite::default();
        let rediff =
            |_also: &BTreeSet<ModuleId>| -> anyhow::Result<PlanningPlan<pbps_diff::order::Draft>> {
                anyhow::bail!("unexpected rediff")
            };
        let v = ModuleId::Named(TableName::new("main", "v"));
        let w = ModuleId::Named(TableName::new("main", "w"));

        let run_err = |seed: &[&str],
                       base: &Schema,
                       desired: &Schema,
                       changes: Vec<Change>,
                       role_members: Option<&RoleMembers>|
         -> String {
            let (_tmp, mut conn) = TempDb::create();
            for sql in seed {
                now(conn.execute(sql)).expect(sql);
            }
            let ids = IdsFile::default();
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                base.clone(),
                ids.clone(),
                "test",
            );
            let base_side = pbps_diff::Side {
                schema: base,
                ids: &ids,
            };
            let desired_side = pbps_diff::Side {
                schema: desired,
                ids: &ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded: base_side,
                written: desired_side,
                base: base_side,
                declared: desired_side,
                hints: &hints,
                baseline: &baseline,
            };
            let mut state = PlanState::default();
            let plan = pbps_diff::order::propose(change_set(changes));
            now(dialect.schedule(&mut state, &mut conn, plan, frame, role_members, &rediff))
                .expect_err("must refuse")
                .to_string()
        };

        let mut base_v = Schema::default();
        base_v
            .modules
            .insert(v.clone(), *view_module("SELECT a FROM t"));
        let mut desired_v = Schema::default();
        desired_v
            .modules
            .insert(v.clone(), *view_module("SELECT a + 1 AS a FROM t"));
        let alter_v = vec![Change::Module(ModuleChange::AlterModule {
            id: v.clone(),
            module: view_module("SELECT a + 1 AS a FROM t"),
        })];
        let drop_v = vec![Change::Module(ModuleChange::DropModule {
            id: v.clone(),
            kind: ModuleKind::View,
        })];

        // 1. Direct unmanaged view reader refuses on both edit and drop.
        for (what, desired, changes) in [
            ("edit", &desired_v, alter_v.clone()),
            ("drop", &Schema::default(), drop_v.clone()),
        ] {
            let err = run_err(
                &[
                    "CREATE TABLE t(a INTEGER)",
                    "CREATE VIEW v AS SELECT a FROM t",
                    "CREATE VIEW ext_reader AS SELECT a FROM v",
                ],
                &base_v,
                desired,
                changes,
                None,
            );
            assert!(
                err.contains("SQLite") && err.contains("main.v") && err.contains("ext_reader"),
                "{what}: {err}"
            );
        }

        // 2. Transitive unmanaged view reader (`ext_top -> managed w -> v`).
        {
            let mut base_vw = base_v.clone();
            base_vw
                .modules
                .insert(w.clone(), *view_module("SELECT a FROM v"));
            let mut desired_vw = desired_v.clone();
            desired_vw
                .modules
                .insert(w.clone(), *view_module("SELECT a FROM v"));
            let err = run_err(
                &[
                    "CREATE TABLE t(a INTEGER)",
                    "CREATE VIEW v AS SELECT a FROM t",
                    "CREATE VIEW w AS SELECT a FROM v",
                    "CREATE VIEW ext_top AS SELECT a FROM w",
                ],
                &base_vw,
                &desired_vw,
                alter_v.clone(),
                None,
            );
            assert!(
                err.contains("SQLite") && err.contains("main.v") && err.contains("ext_top"),
                "{err}"
            );
        }

        // 3. Trigger reading `v` in its body refuses naming SQLite, `v` and trigger.
        {
            let err = run_err(
                &[
                    "CREATE TABLE t(a INTEGER)",
                    "CREATE VIEW v AS SELECT a FROM t",
                    "CREATE TRIGGER tr_body AFTER INSERT ON t BEGIN SELECT a FROM v; END",
                ],
                &base_v,
                &desired_v,
                alter_v.clone(),
                None,
            );
            assert!(
                err.contains("SQLite") && err.contains("main.v") && err.contains("tr_body"),
                "{err}"
            );
        }

        // 4. INSTEAD OF trigger on `v` refuses naming SQLite, `v` and trigger.
        {
            let err = run_err(
                &[
                    "CREATE TABLE t(a INTEGER)",
                    "CREATE VIEW v AS SELECT a FROM t",
                    "CREATE TRIGGER tr_instead INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES (NEW.a); END",
                ],
                &base_v,
                &Schema::default(),
                drop_v,
                None,
            );
            assert!(
                err.contains("SQLite") && err.contains("main.v") && err.contains("tr_instead"),
                "{err}"
            );
        }

        // 5. Foreign plans (roles, enum rebuild, partition rebuild, function drop) refuse naming SQLite.
        {
            let role_err = run_err(
                &[],
                &Schema::default(),
                &Schema::default(),
                vec![Change::Role(RoleChange::DropRole {
                    uid: uid(pbps_model::UidKind::Role, "app"),
                    name: RoleName::new("app"),
                    members: Vec::new(),
                })],
                None,
            );
            assert!(
                role_err.contains("SQLite") && role_err.contains("role"),
                "{role_err}"
            );

            let enum_err = run_err(
                &[],
                &Schema::default(),
                &Schema::default(),
                vec![Change::TypeValue(TypeValueChange::EnumRebuild {
                    uid: uid(pbps_model::UidKind::Type, "main.status"),
                    name: pbps_model::TypeRef::new("main", "status").expect("type"),
                    tmp: pbps_model::TypeRef::new("main", "status_tmp").expect("tmp"),
                    step: pbps_model::EnumRebuildStep::Create {
                        definition: Box::new(pbps_model::ManagedType::Enum {
                            values: vec!["a".into()],
                        }),
                        removed: Vec::new(),
                        ordinal: 0,
                        over: None,
                        why: None,
                    },
                })],
                None,
            );
            assert!(
                enum_err.contains("SQLite") && enum_err.contains("enum"),
                "{enum_err}"
            );

            let part_err = run_err(
                &[],
                &Schema::default(),
                &Schema::default(),
                vec![Change::Partition(PartitionChange::RebuildPartitionedTable(
                    Box::new(RebuildPartitionedTable {
                        table: TableName::new("main", "p"),
                        intent: Rebuild {
                            source: RebuildIdentities {
                                tables: BTreeMap::new(),
                                columns: BTreeMap::new(),
                            },
                            replacement: RebuildIdentities {
                                tables: BTreeMap::new(),
                                columns: BTreeMap::new(),
                            },
                            replacement_root: uid(pbps_model::UidKind::Table, "main.p_new"),
                            continues: BTreeMap::new(),
                            target_checksum: "0".repeat(64),
                            max_rows: 0,
                            allowed_at: "2026-01-01T00:00:00Z".into(),
                            reason: pbps_model::Reason::new("test").expect("reason"),
                            operator: "test".into(),
                            types: BTreeMap::new(),
                            conversions: BTreeMap::new(),
                            exclusions: BTreeMap::new(),
                        },
                        earlier: Vec::new(),
                        before: BTreeMap::new(),
                        after: BTreeMap::new(),
                    }),
                ))],
                None,
            );
            assert!(
                part_err.contains("SQLite") && part_err.contains("partitioned-table"),
                "{part_err}"
            );
        }

        // 6. Unreadable inventory (exclusive lock held elsewhere) is an error, never "no readers".
        {
            let (tmp, mut conn) = TempDb::create();
            now(conn.execute("CREATE TABLE t(a INTEGER)")).expect("table");
            now(conn.execute("CREATE VIEW v AS SELECT a FROM t")).expect("view");
            let locker = pbps_db::sqlite::Connection::open(
                tmp.path(),
                pbps_db::sqlite::OpenMode::ExistingOnly,
            )
            .expect("second connection");
            locker.execute("BEGIN EXCLUSIVE", &[]).expect("lock");
            let ids = IdsFile::default();
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                base_v.clone(),
                ids.clone(),
                "test",
            );
            let base_side = pbps_diff::Side {
                schema: &base_v,
                ids: &ids,
            };
            let desired_side = pbps_diff::Side {
                schema: &desired_v,
                ids: &ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded: base_side,
                written: desired_side,
                base: base_side,
                declared: desired_side,
                hints: &hints,
                baseline: &baseline,
            };
            let mut state = PlanState::default();
            let plan = pbps_diff::order::propose(change_set(alter_v));
            let err = now(dialect.schedule(&mut state, &mut conn, plan, frame, None, &rediff))
                .expect_err("locked file must fail schedule");
            assert!(
                state.inventory.is_none(),
                "failed read must not cache: {err}"
            );
            locker.execute("ROLLBACK", &[]).expect("unlock");
        }
    }

    #[test]
    fn check_final_order_passes_for_clean_drops_and_view_edits_and_refuses_blockers_and_foreign_parts()
     {
        use pbps_model::{
            Change, Column, ColumnChange, IndexChange, ModuleChange, ModuleId, ModuleKind,
            ModuleOptions, TableChange, ValueSource,
        };

        let (tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        for sql in [
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, k INTEGER)",
            "CREATE UNIQUE INDEX uq_k ON parent(k)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id), pk INTEGER REFERENCES parent(k))",
            "CREATE TABLE solo (id INTEGER PRIMARY KEY, extra INTEGER)",
            "CREATE INDEX ix_extra ON solo(extra)",
            "CREATE VIEW v_solo AS SELECT extra FROM solo",
        ] {
            now(conn.execute(sql)).expect(sql);
        }

        let mut stored = Schema::default();
        let mut stored_ids = IdsFile::default();
        for (name, cols) in [
            ("parent", &["id", "k"][..]),
            ("child", &["id", "pid", "pk"][..]),
            ("solo", &["id", "extra"][..]),
        ] {
            let tn = TableName::new("main", name);
            stored.tables.insert(tn.clone(), table_with_cols(cols));
            table_id(&mut stored_ids, name);
            for &col in cols {
                col_id(&mut stored_ids, name, col);
            }
        }
        let run = |conn: &mut Conn, changes: Vec<Change>| {
            let mut desired_ids = stored_ids.clone();
            for ch in &changes {
                if let Change::Table(TableChange::DropTable { uid, name, .. }) = ch {
                    desired_ids.tables.remove(uid);
                    desired_ids.columns.retain(|_, c| c.table != *name);
                } else if let Change::Column(ColumnChange::DropColumn { uid, .. }) = ch {
                    desired_ids.columns.remove(uid);
                }
            }
            let cs = change_set(changes);
            let hints = pbps_model::Hints::default();
            let baseline = StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                pbps_sqlite::types::DIALECT,
                stored.clone(),
                stored_ids.clone(),
                "test",
            );
            let recorded = pbps_diff::Side {
                schema: &stored,
                ids: &stored_ids,
            };
            let written = pbps_diff::Side {
                schema: &stored,
                ids: &desired_ids,
            };
            let frame = Frame {
                label: "test.db",
                recorded,
                written,
                base: recorded,
                declared: written,
                hints: &hints,
                baseline: &baseline,
            };
            now(dialect.check_final_order(&PlanState::default(), conn, &cs, frame))
        };

        // 1. View edit passes all four parts; rebuilds/slots/numbering/drop_blockers are not_applicable.
        let edit_ok = run(
            &mut conn,
            vec![Change::Module(ModuleChange::AlterModule {
                id: ModuleId::Named(TableName::new("main", "v_solo")),
                module: view_module("SELECT id FROM solo"),
            })],
        )
        .expect("view edit passes");
        assert_eq!(edit_ok.rebuilds.status, "not_applicable");
        assert_eq!(edit_ok.drop_blockers.status, "not_applicable");
        assert_eq!(edit_ok.logical_slots.status, "not_applicable");
        assert_eq!(edit_ok.numbered_column_fills.status, "not_applicable");

        // 2. Unreferenced index drop, child-before-parent table drops, and reader-before-column drop pass.
        let drops_ok = run(
            &mut conn,
            vec![
                Change::Index(IndexChange::DropIndex {
                    table: TableName::new("main", "solo"),
                    name: "ix_extra".into(),
                }),
                Change::Module(ModuleChange::DropModule {
                    id: ModuleId::Named(TableName::new("main", "v_solo")),
                    kind: ModuleKind::View,
                }),
                Change::Column(ColumnChange::DropColumn {
                    uid: uid(pbps_model::UidKind::Column, "main.solo.extra"),
                    column: TableName::new("main", "solo").column("extra"),
                }),
                Change::Table(TableChange::DropTable {
                    uid: uid(pbps_model::UidKind::Table, "main.child"),
                    name: TableName::new("main", "child"),
                    detach_from: None,
                }),
                Change::Table(TableChange::DropTable {
                    uid: uid(pbps_model::UidKind::Table, "main.parent"),
                    name: TableName::new("main", "parent"),
                    detach_from: None,
                }),
            ],
        )
        .expect("accounted drops pass");
        assert_eq!(drops_ok.drop_blockers.status, "passed");

        // 3. Permitted-but-broken blockers refuse naming SQLite and both objects.
        for (label, changes, expected) in [
            (
                "parent with child FK",
                vec![Change::Table(TableChange::DropTable {
                    uid: uid(pbps_model::UidKind::Table, "main.parent"),
                    name: TableName::new("main", "parent"),
                    detach_from: None,
                })],
                ("main.parent", "main.child"),
            ),
            (
                "sole unique index behind FK",
                vec![Change::Index(IndexChange::DropIndex {
                    table: TableName::new("main", "parent"),
                    name: "uq_k".into(),
                })],
                ("uq_k", "main.child"),
            ),
            (
                "column read by view",
                vec![Change::Column(ColumnChange::DropColumn {
                    uid: uid(pbps_model::UidKind::Column, "main.solo.extra"),
                    column: TableName::new("main", "solo").column("extra"),
                })],
                ("main.solo.extra", "v_solo"),
            ),
        ] {
            let err = run(&mut conn, changes).expect_err(label).to_string();
            assert!(
                err.contains("SQLite") && err.contains(expected.0) && err.contains(expected.1),
                "{label}: {err}"
            );
        }

        // 4. Foreign inputs across all four parts refuse naming SQLite.
        let mut opt_mod = view_module("SELECT 1");
        opt_mod.options = ModuleOptions {
            schemabinding: true,
            ..Default::default()
        };
        let mut id_col = Column::new("integer".parse().expect("ty"));
        id_col.value_source = ValueSource::Identity(pbps_model::Identity::new(1, 1));
        for (label, ch) in [
            (
                "non-view module",
                Change::Module(ModuleChange::DropModule {
                    id: ModuleId::Named(TableName::new("main", "f")),
                    kind: ModuleKind::Function,
                }),
            ),
            (
                "view options",
                Change::Module(ModuleChange::AlterModule {
                    id: ModuleId::Named(TableName::new("main", "v_solo")),
                    module: opt_mod,
                }),
            ),
            (
                "beside_default partition",
                Change::Table(TableChange::CreateTable {
                    uid: uid(pbps_model::UidKind::Table, "main.p1"),
                    name: TableName::new("main", "p1"),
                    table: Box::new(table_with_cols(&["id"])),
                    beside_default: Some(TableName::new("main", "p_def")),
                    moves_at_most: None,
                }),
            ),
            (
                "identity column",
                Change::Column(ColumnChange::AddColumn {
                    uid: uid(pbps_model::UidKind::Column, "main.solo.seq"),
                    table: TableName::new("main", "solo"),
                    name: "seq".into(),
                    column: Box::new(id_col),
                }),
            ),
        ] {
            let err = run(&mut conn, vec![ch]).expect_err(label).to_string();
            assert!(err.contains("SQLite"), "{label}: {err}");
        }

        // 5. Unreadable (exclusively locked) catalog is an error, never "no blockers".
        let locker =
            pbps_db::sqlite::Connection::open(tmp.path(), pbps_db::sqlite::OpenMode::ExistingOnly)
                .expect("second conn");
        locker.execute("BEGIN EXCLUSIVE", &[]).expect("lock");
        let lock_err = run(
            &mut conn,
            vec![Change::Index(IndexChange::DropIndex {
                table: TableName::new("main", "solo"),
                name: "ix_extra".into(),
            })],
        )
        .expect_err("locked file must error");
        assert!(!lock_err.to_string().is_empty(), "{lock_err}");
        locker.execute("ROLLBACK", &[]).expect("unlock");
    }
}
