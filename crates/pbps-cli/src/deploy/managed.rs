//! Reading the managed state of a database: the catalog projection, the
//! baselines an apply measures itself against, and the refusals for a database
//! whose collation or time zone moved.

use super::*;

/// The command owns catalog diagnostics even when a later read fails. Keep
/// them beside errors, never inside an error or recorded schema (DEC-1739.1).
#[derive(Clone, Copy)]
pub(super) enum CatalogWarnings<'a> {
    Human,
    Findings(&'a std::cell::RefCell<Vec<crate::output::Finding>>),
}

impl<'a> CatalogWarnings<'a> {
    pub(super) fn for_output(
        json: bool,
        findings: &'a std::cell::RefCell<Vec<crate::output::Finding>>,
    ) -> Self {
        if json {
            Self::Findings(findings)
        } else {
            Self::Human
        }
    }

    pub(super) fn for_progress(progress: Option<&'a services::Progress>) -> Self {
        progress.map_or(Self::Human, |p| Self::Findings(&p.findings))
    }

    pub(super) fn emit(self, findings: impl IntoIterator<Item = crate::output::Finding>) {
        match self {
            Self::Human => {
                for finding in findings {
                    eprintln!("warning: {}", finding.message);
                }
            }
            Self::Findings(collected) => collected.borrow_mut().extend(findings),
        }
    }
}

/// A scoped live state, plus the declared modules the read could not express.
///
/// The second list cannot live in [`pbps_diff::Scoped`] — "pbps cannot read
/// this back" is a dialect fact, and the differ knows no dialect — but a caller
/// that has to decide whether an object is *there* needs both: a module the
/// catalog holds and introspection cannot reproduce is inside the managed set
/// by name and outside the scoped schema in fact.
pub struct Managed {
    /// The connected database's default collation, where the engine has
    /// column collations (#1175): the declarations are compared with a
    /// collation equal to it taken out ([`Schema::without_collation`]).
    pub database_collation: Option<String>,
    pub scoped: pbps_diff::Scoped,
    /// Catalog facts inside the managed set that the projection could not
    /// express: an unsupported feature on a managed table, or a module the set
    /// names that introspection cannot read back. They are drift findings,
    /// never ignorable warnings, and no recorder accepts a schema that has any
    /// (see [`managed_limitations`]).
    pub limitations: Vec<String>,
    /// Unsupported grants to unmanaged roles on managed targets. These are
    /// report-only context, never scoped state or saved-plan findings (DEC-2446.1).
    pub permission_advisories: Vec<String>,
    /// Report-only catalog objects; never attached to plan changes or state.
    pub catalog_notices: Vec<String>,
    /// The grants this read could see and this connection could not take
    /// away (#251): an entry a third role granted, where no `REVOKE` this
    /// connection runs would carry that grantor.
    ///
    /// Here rather than in [`pbps_diff::Scoped`] for the reason above — whose
    /// grantor a statement carries is a dialect fact, and the differ knows no
    /// dialect. They are **not** removed from the scoped schema: each is an
    /// ordinary grant, and a declaration that keeps one is satisfied by the
    /// database as it stands. Only a plan that revokes one is impossible, and
    /// that is what [`crate::engine::unrevocable_grants`] refuses.
    pub unrevocable: Vec<pbps_db::catalog::Unrevocable>,
    /// Every module or type in the database that introspection cannot express, by identity,
    /// with the reason already rendered.
    ///
    /// Returned in full rather than filtered to the managed set, because the
    /// caller that most needs it is asking about a name that has *never* been
    /// recorded: a newly declared module that collides with an encrypted one
    /// already standing there.
    pub unreadable: Vec<(pbps_db::catalog::LimitationTarget, String)>,
    /// The rows read back under the read scope the caller asked for. Not yet
    /// placed in the schema: `plan --db` projects two different views out of
    /// one read (see [`pbps_model::data::read_scopes`]).
    pub rows: ObservedRows,
    /// Names outside the managed set whose relation definitions were omitted.
    /// An unsupported temporal table still occupies its name (SPEC §7.6).
    pub unmanaged_relations: Vec<TableName>,
}

/// The scoped state alone, with the rows of every table in `scopes` read back
/// and placed under it — which is all any caller but `bootstrap` and
/// `plan --db` needs.
///
/// `reference` is the schema whose rows say how a cell at its default is
/// read ([`pbps_model::ObservedRow`]): the recorded snapshot for a drift
/// check, the declarations for a state recorded from them. A command that has
/// no rows of its own — `apply` records what it just wrote from the plan's
/// scope alone — passes an empty schema and gets the shortest true spelling.
/// The rows to read back for each table in `scopes`.
///
/// Its own function because three callers need it and one of them —
/// `verify` — cannot go through [`managed_state`]: it wants the raw
/// [`Managed`] so the unmanaged policy can be applied beside the report
/// rather than inside the read.
pub(super) fn rows_to_read(scopes: &DataScopes) -> BTreeMap<TableName, RowScope> {
    scopes
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn managed_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    scopes: &DataScopes,
    reference: &Schema,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<pbps_diff::Scoped> {
    let request = ManagedRead {
        ids,
        modules,
        external,
        unmanaged,
        scopes,
        reference,
        read,
        warnings,
    };
    managed_state_then(conn, &request, || std::future::ready(Ok(()))).await
}

pub(super) struct ManagedRead<'a> {
    pub(super) ids: &'a IdsFile,
    pub(super) modules: &'a BTreeSet<ModuleId>,
    pub(super) external: &'a BTreeSet<TableName>,
    pub(super) unmanaged: db::CatalogPolicy<'a>,
    pub(super) scopes: &'a DataScopes,
    pub(super) reference: &'a Schema,
    pub(super) read: crate::engine::Read,
    pub(super) warnings: CatalogWarnings<'a>,
}

impl ManagedRead<'_> {
    async fn capture(&self, conn: &mut Conn) -> anyhow::Result<pbps_diff::Scoped> {
        managed_state_once(
            conn,
            self.ids,
            self.modules,
            self.external,
            self.unmanaged,
            self.scopes,
            self.reference,
            self.read,
            self.warnings,
        )
        .await
    }
}

// An internal test callback places a second connection's DDL between the two
// real reads. The production caller supplies no work and no user hook exists.
pub(super) async fn managed_state_then<F, Fut>(
    conn: &mut Conn,
    request: &ManagedRead<'_>,
    after_capture: F,
) -> anyhow::Result<pbps_diff::Scoped>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let captured = request.capture(conn).await?;
    if crate::engine::needs_readback_revalidation(conn.driver(), request.read) {
        after_capture().await?;
        let checked = request.capture(conn).await?;
        // An unrelated unmanaged object is outside SPEC §8.2. Compare the
        // facts that can enter this recording, not the ambient inventory.
        if captured.schema != checked.schema
            || captured.missing != checked.missing
            || captured.missing_roles != checked.missing_roles
            || captured.missing_types != checked.missing_types
            || captured.unexpressible != checked.unexpressible
        {
            bail!(
                "the managed state changed during the transactional read-back; refusing to record an unstable schema. Retry after the concurrent change settles"
            );
        }
        return Ok(checked);
    }
    Ok(captured)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn managed_state_once(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    scopes: &DataScopes,
    reference: &Schema,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<pbps_diff::Scoped> {
    let managed = managed_state_full(
        conn,
        ids,
        modules,
        external,
        unmanaged,
        &rows_to_read(scopes),
        read,
        warnings,
    )
    .await?;
    refuse_managed_limitations(&managed.limitations)?;
    let mut scoped = managed.scoped;
    // Definition projection must not manufacture absence. Keep the same
    // read's omitted relation names for final endpoint checks, without
    // recording an unsupported definition in the schema (SPEC §7.6).
    scoped.unmanaged.extend(managed.unmanaged_relations);
    scoped.unmanaged.sort();
    scoped.unmanaged.dedup();
    scoped.schema = scoped
        .schema
        .with_observed_rows(&managed.rows, scopes, reference)?;
    Ok(scoped)
}

/// Refuses to use a catalog projection that omitted facts inside the managed
/// set. Both recording and connected planning need the same guard: a partial
/// schema is neither an honest snapshot nor a safe baseline for an artifact.
pub(super) fn refuse_managed_limitations(limitations: &[String]) -> anyhow::Result<()> {
    if !limitations.is_empty() {
        bail!(
            "{} fact(s) inside the managed set cannot be represented:\n  {}\n\
             Refusing to compare, plan from, or record a partial schema.",
            limitations.len(),
            limitations.join("\n  ")
        );
    }
    Ok(())
}

/// The baseline an `apply` measures itself against, projected twice out of one
/// read.
///
/// The two projections answer two different questions and cannot be one.
/// The **checksum** has to be taken under the recorded state's spelling of a
/// cell at its default ([`pbps_model::ObservedRow`]) and under the union of
/// the recorded and declared scopes, because that is what the plan pinned
/// (DECISIONS 98). The **comparison** made after the statements is against a
/// read-back that has neither: `apply` records what it just wrote, from the
/// plan's scope and with no recorded state to spell it. So the second view is
/// projected exactly the way that read-back will be — the plan's scopes, no
/// reference — and any difference between the two is then a change, not a
/// difference of question.
///
/// One read and two projections, never two reads: two reads would ask the
/// engine the same thing twice and could get two answers, which is the very
/// thing the comparison exists to detect.
#[allow(clippy::too_many_arguments)]
pub(super) async fn baseline_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    scopes: &DataScopes,
    recorded: &Schema,
    as_the_apply_reads_it: &DataScopes,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<(
    pbps_diff::Scoped,
    Schema,
    Vec<pbps_db::catalog::Unrevocable>,
)> {
    // Before the plan's transaction opens: the baseline is the database as
    // it stands, not as this apply is about to leave it.
    // One read for both projections: the rows either one keeps.
    let managed = managed_state_full(
        conn,
        ids,
        modules,
        external,
        unmanaged,
        &rows_to_read(&pinned_scopes(scopes, as_the_apply_reads_it)),
        crate::engine::Read::Snapshot,
        warnings,
    )
    .await?;
    refuse_managed_limitations(&managed.limitations)?;
    let comparable = managed.scoped.schema.clone().with_observed_rows(
        &managed.rows,
        as_the_apply_reads_it,
        &Schema::default(),
    )?;
    let mut scoped = managed.scoped;
    scoped.schema = scoped
        .schema
        .with_observed_rows(&managed.rows, scopes, recorded)?;
    // The grants this connection could not revoke come from the same read:
    // like ownership, they are beside the checksum, and the apply rechecks
    // them against the connection that runs the statements (#1057).
    Ok((scoped, comparable, managed.unrevocable))
}

/// The same two projections for a staged apply, which needs a third
/// difference: a checkpoint watches every module the plan *names*, including
/// ones it has yet to create (DECISIONS 164), while the checksum has to be
/// taken over exactly the managed set the plan was pinned to or no plan would
/// validate at all. Two cuts of one read, not two reads.
///
/// It was two reads, with the preflight probes and the resume checks running
/// between them. Anything another session changed in that window was already
/// in the second read, so it became the baseline every later checkpoint was
/// measured against — never reported, and finally recorded as clean by the
/// closing ordinary snapshot, which is the state `verify` compares against
/// ever after. [`baseline_state`] states the rule this broke: two reads ask
/// the engine the same thing twice and can get two answers, which is the very
/// thing the comparison exists to detect (DECISIONS 174).
#[allow(clippy::too_many_arguments)]
pub(super) async fn staged_baseline(
    conn: &mut Conn,
    ids: &IdsFile,
    checked: &BTreeSet<ModuleId>,
    watched: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    checked_scopes: &DataScopes,
    recorded: &Schema,
    watched_scopes: &DataScopes,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<(
    pbps_diff::Scoped,
    Schema,
    Vec<pbps_db::catalog::Unrevocable>,
)> {
    let pulled = pull(conn, crate::engine::Read::Snapshot).await?;
    warnings.emit(
        pulled
            .warnings
            .iter()
            .map(|message| crate::output::Finding::warning("catalog.warning", message)),
    );
    let unreadable = unreadable_objects(&pulled);
    // Over the *watched* set, which contains the checked one: a module this
    // plan is about to write that the catalog cannot read back is a
    // limitation, and it is only in the wider set. The two reads this
    // replaces each refused over their own set, so refusing over the union is
    // what keeps that (491edd9, DECISIONS 174).
    debug_assert!(checked.is_subset(watched));
    refuse_managed_limitations(&managed_limitations(&pulled, ids, watched, external))?;

    let mut scoped = cut_report(
        &pulled,
        ids,
        checked,
        external,
        &unreadable,
        unmanaged,
        warnings,
    )?;
    // The watch cut is the one the row read is taken against, because it is
    // the wider of the two: a module the plan creates is in it and not in the
    // checksum's, and no table is in either alone. `Unmanaged::Ignore`
    // because the policy has already been applied, by the cut above — running
    // it twice would report the same object twice.
    let watching = cut_report(
        &pulled,
        ids,
        watched,
        external,
        &unreadable,
        db::CatalogPolicy {
            unmanaged: pbps_config::Unmanaged::Ignore,
            ..unmanaged
        },
        warnings,
    )?;
    let rows = crate::engine::read_rows(
        conn,
        &watching.schema,
        &rows_to_read(&pinned_scopes(checked_scopes, watched_scopes)),
        crate::engine::Read::Snapshot,
    )
    .await
    .context("cannot read the declared rows back")?;

    // Spelled two ways for the reason `baseline_state` gives: the checksum
    // under the recorded state's spelling of a cell at its default, the
    // comparison the way the checkpoint read that follows it will be.
    let previous = watching
        .schema
        .with_observed_rows(&rows, watched_scopes, &Schema::default())?;
    scoped.schema = scoped
        .schema
        .with_observed_rows(&rows, checked_scopes, recorded)?;
    Ok((scoped, previous, pulled.unrevocable.clone()))
}

/// Introspects, cuts the result down to the managed set, reads the rows of
/// every table in `read`, and reports whatever the read itself could not
/// express.
///
/// Every connected command starts here. Its catalog diagnostics go to the
/// command's collector before fallible projection and row reads, so a failure
/// cannot discard warnings that belong in its JSON envelope (DEC-1739.1).
///
/// The rows come from the catalog under a scope the caller supplies, because
/// which of a table's rows are declared is not something the database knows
/// (ADR-0004). A row read that fails is an error, not an empty answer: a
/// state recorded without the rows would say the table declares none.
#[allow(clippy::too_many_arguments)]
pub(super) async fn managed_state_full(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &BTreeSet<ModuleId>,
    external: &BTreeSet<TableName>,
    unmanaged: db::CatalogPolicy<'_>,
    rows: &BTreeMap<TableName, RowScope>,
    read: crate::engine::Read,
    warnings: CatalogWarnings<'_>,
) -> anyhow::Result<Managed> {
    let pulled = pull(conn, read).await?;
    warnings.emit(
        pulled
            .warnings
            .iter()
            .map(|message| crate::output::Finding::warning("catalog.warning", message)),
    );
    let unreadable = unreadable_objects(&pulled);
    let limitations = managed_limitations(&pulled, ids, modules, external);
    let permission_advisories = permission_advisories(&pulled.unexpressible, ids, modules)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let unmanaged_relations = pulled
        .limitations
        .iter()
        .filter_map(|limitation| match &limitation.target {
            pbps_db::catalog::LimitationTarget::Relation(name)
                if !ids.tables.values().any(|managed| managed == name)
                    && !modules.contains(&ModuleId::Named(name.clone())) =>
            {
                Some(name.clone())
            }
            pbps_db::catalog::LimitationTarget::Relation(_)
            | pbps_db::catalog::LimitationTarget::Type(_)
            | pbps_db::catalog::LimitationTarget::SharedModule(_)
            | pbps_db::catalog::LimitationTarget::Search(_)
            | pbps_db::catalog::LimitationTarget::Module(_)
            | pbps_db::catalog::LimitationTarget::UnnameableModule(_) => None,
        })
        .collect();
    let scoped = cut_report(
        &pulled,
        ids,
        modules,
        external,
        &unreadable,
        unmanaged,
        warnings,
    )?;
    let rows = crate::engine::read_rows(conn, &scoped.schema, rows, read)
        .await
        .context("cannot read the declared rows back")?;
    Ok(Managed {
        database_collation: pulled.database_collation.clone(),
        scoped,
        limitations,
        permission_advisories,
        catalog_notices: pulled
            .catalog_notices
            .iter()
            .cloned()
            .chain(
                pulled
                    .extension_notices
                    .iter()
                    .filter(|(name, _)| ids.extension_uid(name).is_none())
                    .map(|(_, message)| message.clone()),
            )
            .collect(),
        unrevocable: pulled.unrevocable.clone(),
        unreadable,
        rows,
        unmanaged_relations,
    })
}

/// The declarations as a connected plan compares them, on a database whose
/// default collation is `database_collation` (#1175): a declared collation
/// equal to it is a column with none, which is how the engine reads it back.
///
/// One function because three diffs depend on it agreeing: `plan --db`, the
/// `--dev` rehearsal, and `doctor`'s question of which keys survive the
/// deletes. A normalization one of them skipped would rebuild a column the
/// others leave alone, and in `doctor` that phantom rebuild withholds a demand
/// the apply needs (#1511, DEC-1511.1).
pub fn declarations_as_compared(declared: &Schema, database_collation: Option<&str>) -> Schema {
    match database_collation {
        Some(default) => declared.without_collation(default),
        None => declared.clone(),
    }
}

/// Refuses a plan whose target's default collation has changed since it was
/// computed (#1175). What a column declared with no collation means is that
/// default, and the checksum cannot see it move when no managed character
/// column sits under it (see [`pbps_model::PlanBaseline::database_collation`]).
/// A plan from an engine without column collations carries none and is not
/// asked.
pub(super) async fn refuse_moved_database_collation(
    conn: &mut Conn,
    plan: &pbps_model::SavedPlan,
    label: &str,
) -> anyhow::Result<()> {
    let Some(planned) = &plan.baseline.database_collation else {
        return Ok(());
    };
    let now = crate::engine::database_collation(conn)
        .await
        .context("cannot read the database's default collation")?;
    if now.as_deref() != Some(planned.as_str()) {
        bail!(
            "`{label}` has default collation {} now, and this plan was computed against `{planned}`.\n\
             A column it creates or alters without a collation would take the new default, \
             which is not what was approved. Recompute the plan with `pbps plan --db`.",
            now.map_or_else(|| "none readable".to_owned(), |n| format!("`{n}`")),
            label = pbps_model::escape::Escaped(label),
        );
    }
    Ok(())
}

/// Refuses a plan whose target reads offset-less date/time values in another
/// zone than the one it was computed in (DEC-1916.1). Every such value the plan
/// writes, and every pin it runs under, is the recorded zone's; in another
/// zone the same statements store other instants, and nothing in them looks
/// different (see [`pbps_model::PlanBaseline::time_zone`]).
pub(super) fn refuse_moved_time_zone(
    conn: &Conn,
    plan: &pbps_model::SavedPlan,
    label: &str,
) -> anyhow::Result<()> {
    match moved_time_zone(plan.baseline.time_zone.as_ref(), conn.session_zone(), label) {
        Some(refusal) => Err(anyhow::anyhow!(refusal)),
        None => Ok(()),
    }
}

/// [`refuse_moved_time_zone`]'s verdict, apart from the connection.
///
/// The zones compare by their exact text, as the engine reported them: two
/// spellings of one zone (`UTC`, `Etc/UTC`) are refused too, because which
/// spellings agree is a fact about the engine's tz database that this side
/// cannot see, and a spelling costs one `ALTER` where a wrong instant costs
/// every value written. Absent on one side only is refused as well: a plan
/// that records no zone for an engine that has one was not computed by this
/// build's `plan --db`.
pub(super) fn moved_time_zone(
    planned: Option<&SessionZone>,
    now: Option<&SessionZone>,
    label: &str,
) -> Option<String> {
    let remedy = "Recompute the plan with `pbps plan --db`, or have the zone set back \
                  (`ALTER DATABASE … SET timezone` or `ALTER ROLE … SET timezone`).";
    match (planned, now) {
        (None, None) => None,
        (Some(planned), Some(now)) if planned == now => None,
        (Some(planned), Some(now)) => Some(format!(
            "`{label}` reads offset-less date/time values in `{now}` now, and this plan \
             was computed in `{planned}`.\nDifferent spellings are treated as different zones; \
             their texts must match exactly (DEC-1916.1). A different zone could store any \
             `timestamptz` or `timetz` written without an offset at another instant than \
             the one approved. \
             {remedy}",
            label = pbps_model::escape::Escaped(label),
        )),
        (Some(planned), None) => Some(format!(
            "`{label}` reports no session time zone, and this plan was computed in \
             `{planned}`. {remedy}",
            label = pbps_model::escape::Escaped(label),
        )),
        (None, Some(now)) => Some(format!(
            "`{label}` reads offset-less date/time values in `{now}`, and this plan records \
             no time zone, so what it writes was not read in one. {remedy}",
            label = pbps_model::escape::Escaped(label),
        )),
    }
}

/// Text search objects have no identity yet, so a drift comparison cannot tell a project's
/// object from another's: one the database holds that the recorded state does not is not
/// drift (as an unmanaged type is not), and the differ would otherwise read it as a creation
/// it cannot express. Only the objects the record holds are compared, so one of them that
/// changed is still reported by name (DEC-2728.1).
pub(super) fn only_recorded_search_objects(recorded: &Schema, live: &mut Schema) {
    live.search
        .retain(|name, _| recorded.search.contains_key(name));
}

/// The same for standalone sequences (DEC-2721.1): a sequence the database holds that the
/// record does not is not drift, and one the record holds that changed is still reported by
/// name. The counter is in neither side, so advancing it is no difference at all.
pub(super) fn only_recorded_sequences(recorded: &Schema, live: &mut Schema) {
    live.sequences
        .retain(|name, _| recorded.sequences.contains_key(name));
}

/// The informational line that ends a plan when the database holds comments no declaration
/// manages (DEC-2729.1): a note, not a change.
pub(super) fn undeclared_comments_note(count: usize) -> String {
    let (are, comments) = if count == 1 {
        ("is", "comment in the database")
    } else {
        ("are", "comments in the database")
    };
    format!("{count} {comments} {are} not declared; pbps leaves them as they are.")
}

/// Both sides of a drift comparison without the comments of their tables and columns: a
/// comment is reconciled by the plan, never drift, and the differ would otherwise read one the
/// database holds and the record does not as a creation to plan (DEC-2729.1).
pub(super) fn without_comments(recorded: &mut Schema, live: &mut Schema) {
    recorded.clear_descriptions();
    live.clear_descriptions();
}

/// The catalog, read once.
///
/// Split out so a caller that needs two *cuts* of one database state can take
/// them from one read. Two reads would ask the engine the same thing twice
/// and could get two answers, which is the very thing a movement comparison
/// exists to detect (DECISIONS 174).
pub(super) async fn pull(
    conn: &mut Conn,
    read: crate::engine::Read,
) -> anyhow::Result<pbps_db::catalog::Pulled> {
    crate::engine::introspect(conn, read)
        .await
        .context("cannot read the database catalog")
}
