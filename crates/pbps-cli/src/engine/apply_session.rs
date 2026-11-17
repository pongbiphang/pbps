//! Apply's engine work at its existing positions (DEC-2798.2, DEC-2798.4).
//!
//! Positions keep source order and end wherever neutral code can refuse,
//! print, add a finding or change an input. Reads repeated today stay repeated;
//! a pin reader receives the original Check, never a guessed closing phase.
//! The service selects the concrete dialect with with_plan_dialect! before
//! opening one default state per attempt. Existing connected adapters remain
//! available to staged apply and bootstrap until their own routes move.
//! Staged apply now uses the positions; bootstrap and planning retain adapters.
use super::*;
use crate::pins::{Approved, Check};
use anyhow::Context as _;
use pbps_pg::admission::Admission;

pub(crate) mod extensions;
mod mssql;
mod postgres;

/// SQL execution has different owners; statement count cannot identify them.
#[derive(Clone, Copy)]
pub(crate) enum Execution<'a> {
    Transactional,
    StagedGuardedDdl { previous: &'a Schema },
    StagedRow,
}

/// Evidence lives for this attempt, while closing verification is always fresh.
#[derive(Default)]
pub(crate) struct Transaction {
    pub data_guard: DataWriteGuard,
    pub retained: Option<pbps_pg::retained::Held>,
    pub verified: Option<pbps_pg::retained::Verified>,
    pub defaults: pbps_pg::type_refs::declared::DomainDefaults,
}

pub(crate) struct Opening<'a> {
    pub approved: &'a Approved<'a>,
    pub baseline: &'a StateSnapshot,
    pub before: &'a Schema,
    pub scoped: &'a Schema,
    pub recorded_ids: &'a IdsFile,
    pub original_ids: &'a IdsFile,
}

pub(crate) trait ApplySession: crate::emission::Engine {
    type State: Default;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction;
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction;
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()>;
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget>;

    async fn connection_support(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> anyhow::Result<()> {
        permission_support(conn, &plan.changes, &plan.ids).await?;
        generation_support(conn, &plan.changes, &plan.ids).await?;
        Ok(())
    }
    async fn history_before_intent(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        label: &str,
    ) -> anyhow::Result<()> {
        crate::deploy::refuse_taken_history_names(conn, changes, label).await?;
        // Nothing binds a saved plan to a server: asked again here (#1502).
        refuse_unsupported_temporal(conn, changes).await
    }
    async fn external_roles(
        &self,
        conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        external_role_renames(conn, recorded, declared).await
    }
    async fn baseline_collation(&self, conn: &mut Conn) -> Result<Option<String>, DbError> {
        database_collation(conn).await
    }
    fn baseline_grants(
        &self,
        driver: Driver,
        plan: &pbps_model::SavedPlan,
        scoped: &pbps_diff::Scoped,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()> {
        owned_targets(
            driver,
            &plan.changes,
            &plan.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        // And whether *this* connection can take back what the plan revokes,
        // which `plan` asked of the connection it planned over (#1057). A plan
        // made as the owner and applied as a least-privilege deployer otherwise
        // passes the baseline and fails at its `REVOKE`, after earlier statements
        // of a staged run are already committed.
        unrevocable_grants(driver, &plan.changes, &plan.ids, unrevocable)?;
        Ok(())
    }
    async fn drop_blockers(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        check_drop_blockers(conn, &plan.changes, &plan.ids, stored, stored_ids)
            .await
            .map(|_| ())
    }
    async fn preflight_limits(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> Result<EditionVerdict, anyhow::Error> {
        refuse_unlogged_partition_referencers(
            conn,
            &plan.changes,
            &plan.ids,
            plan.mode.is_staged(),
        )
        .await?;
        // A publication can list the table between the plan and this apply (#1489).
        refuse_published_unlogged(conn, &plan.changes, &plan.ids).await?;
        // The edition, asked again. `plan --db` checked it, but nothing binds a
        // saved plan to an environment: the same file can be applied to a different
        // server, or to the same one after an edition change, and an `ONLINE = ON`
        // statement rejected halfway through an apply is precisely what the check
        // at plan time exists to prevent (ADR-0003).
        Ok(edition_verdict(conn, &plan.changes).await?)
    }
    async fn role_drop_facts(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<(
        BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>,
        BTreeMap<pbps_model::RoleName, Vec<String>>,
    )> {
        let members = role_members(conn)
            .await
            .context("cannot read the role memberships")?;
        let owned = role_owned_securables(conn)
            .await
            .context("cannot read what the roles own")?;
        Ok((members, owned))
    }
    async fn alike_role_names(
        &self,
        conn: &mut Conn,
        names: &[&str],
    ) -> anyhow::Result<Vec<(String, String)>> {
        names_alike(conn, names).await
    }
    async fn principal_name_holders(
        &self,
        conn: &mut Conn,
        names: &[&str],
        vacated: &[&str],
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        principals_holding(conn, names, vacated).await
    }
    async fn rename_report(
        &self,
        conn: &mut Conn,
        target: &RenameTarget,
    ) -> Result<ImpactReport, ImpactError> {
        rename_impact(conn, target).await
    }
    fn rename_notes(&self, driver: Driver, report: &ImpactReport) -> Vec<String> {
        impact_notes(driver, report)
    }
    async fn conversions(
        &self,
        conn: &mut Conn,
        admission: &Admission,
        changes: &[pbps_model::PlannedChange],
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnRef>> {
        vet_conversions(conn, admission, changes).await
    }
    async fn requirement(
        &self,
        conn: &mut Conn,
        admission: &Admission,
        table: &TableName,
        predicate: &str,
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnName>> {
        vet_drop_requirement(conn, admission, table, predicate).await
    }
    async fn table_sites(
        &self,
        conn: &mut Conn,
        admission: &Admission,
        changes: &[pbps_model::PlannedChange],
    ) -> bool {
        table_expressions_left_unjudged(conn, admission, changes).await
    }
    async fn transaction_opened(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        plan: &pbps_diff::order::AdmittedPlan<Self>,
        opening: Opening<'_>,
    ) -> anyhow::Result<()> {
        let admission = opening.approved.admission();
        rebuild_window(
            conn,
            &admission,
            pbps_diff::Side {
                schema: &opening.baseline.schema,
                ids: opening.recorded_ids,
            },
            &plan.changes,
        )
        .await?;
        self.state(state).retained = self
            .hold_retained(
                conn,
                &plan.changes,
                Some(&opening.baseline.declared.overlay(opening.before)),
            )
            .await?;
        self.external_roles(conn, opening.original_ids, &plan.ids)
            .await?;
        check_module_dependents(conn, plan, opening.scoped, &plan.ids, opening.recorded_ids)
            .await?;
        check_module_rebuilds(conn, &plan.changes, false).await?;
        recheck_enum_rebuild_census(conn, &plan.changes, opening.recorded_ids, false).await?;
        check_domain_alterations(conn, &plan.changes, opening.recorded_ids).await?;
        crate::deploy::recheck_extension_creates(conn, plan).await?;
        self.drop_blockers(conn, plan, opening.scoped, opening.recorded_ids)
            .await?;
        self.state(state).data_guard = prepare_data_writes(
            conn,
            &opening.approved.admission(),
            &plan.changes,
            opening.baseline,
            &plan.ids,
        )
        .await?;
        Ok(())
    }
    async fn after_statements(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        original_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        check_module_rebuilds(conn, &plan.changes, true).await?;
        refuse_unsafe_definers(conn, &written_routines(&plan.changes), Absent::Refused).await?;
        self.external_roles(conn, original_ids, &plan.ids).await?;
        Ok(())
    }
    /// The census again, where the plan runs: a dependent that appeared on an
    /// enum after the plan was made is refused by name before the first statement
    /// (DEC-1635.7). Inside the apply's own transaction, after the lock.
    ///
    /// The tightened domains again, where the plan runs: a column of an array or
    /// a composite of the domain that appeared after the plan was made is refused
    /// by name before the first statement (DEC-1635.8).
    async fn staged_census(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &IdsFile,
        resuming: bool,
    ) -> anyhow::Result<()> {
        recheck_enum_rebuild_census(conn, &plan.changes, stored, resuming).await?;
        if !resuming {
            check_domain_alterations(conn, &plan.changes, stored).await?;
            // Every statement starts from a target that still allows it: a resume
            // only re-asks for the packages whose statement has not run.
            crate::deploy::recheck_extension_creates(conn, plan).await?;
        }
        Ok(())
    }
    async fn declared_type_replay(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<()> {
        check_declared_type_pins(conn, approved.project, approved.plan, approved.managed, at).await
    }
    async fn external_catalog(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<()> {
        check_type_pins(
            conn,
            approved.plan,
            &approved.project.config.trusted_roles,
            &approved.managed.references,
            matches!(at.within(), pbps_pg::pins::Within::CallersTransaction),
        )
        .await
    }
    async fn reviewed_input_roots(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<BTreeSet<i64>> {
        type_input_routines(
            conn,
            &approved.plan.type_pins,
            &approved.project.config.trusted_roles,
            approved.plan.pin_type_routines,
            matches!(at.within(), pbps_pg::pins::Within::CallersTransaction),
        )
        .await
    }
    async fn selected_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
        roots: &[i64],
    ) -> Result<(pbps_pg::pins::Read, Vec<pbps_model::TypeRoutine>), DbError> {
        pbps_pg::pins::selected(conn, at.within(), roots).await
    }
    async fn replaceable_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
    ) -> Result<Option<pbps_pg::pins::Read>, DbError> {
        read_pinned_routines(conn, at.within()).await
    }
    async fn before_transactional_statement(
        &self,
        conn: &mut Conn,
        admission: &Admission,
        stmt: &pbps_dialect::Statement,
        guard: &DataWriteGuard,
        plan: Option<&pbps_model::SavedPlan>,
    ) -> anyhow::Result<()> {
        before_statement(conn, admission, stmt, guard, plan).await
    }
    #[allow(clippy::too_many_arguments)]
    async fn before_staged_statement(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        approved: &Approved<'_>,
        stmt: &pbps_dialect::Statement,
        baseline: &StateSnapshot,
        previous: &Schema,
    ) -> anyhow::Result<()> {
        let plan = approved.plan;
        if stmt.row_write.is_some() {
            // The plan's own admission, so a routine its review pinned is
            // admitted here as it was at the start (DEC-2563.9).
            let admission = approved.admission();
            let guard =
                prepare_data_writes(conn, &admission, &plan.changes, baseline, &plan.ids).await?;
            check_data_write(conn, &admission, stmt, &guard).await?;
        } else {
            self.state(state).retained = self
                .hold_retained(
                    conn,
                    &plan.changes,
                    Some(&baseline.declared.overlay(previous)),
                )
                .await?;
            if let Some(extension) = &stmt.extension {
                crate::deploy::vet_before_extension(conn, extension, Some(plan)).await?;
            }
            if stmt.rewrites.is_some() {
                // The expressions the rewrite re-evaluates, held still by
                // the lock the guard takes, judged by the plan's own
                // admission (DEC-2563.9).
                let admission = approved.admission();
                let guard =
                    prepare_data_writes(conn, &admission, &plan.changes, baseline, &plan.ids)
                        .await?;
                check_data_write(conn, &admission, stmt, &guard).await?;
            }
            if let Some(change) = &stmt.introduces {
                // Judged by the plan's own admission before the statement
                // runs, with what the earlier steps created in place.
                let admission = approved.admission();
                pbps_pg::table_expressions::vet_change(
                    conn,
                    &admission,
                    pbps_pg::pins::Within::CallersTransaction,
                    pbps_pg::table_expressions::Moment::Statement,
                    change,
                )
                .await
                .and_then(pbps_pg::table_expressions::Vetted::admitted)
                .map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn after_statement(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        admission: &Admission,
        stmt: &pbps_dialect::Statement,
        plan: Option<&pbps_model::SavedPlan>,
        changes: &ChangeSet,
        executed: &[pbps_dialect::Statement],
        execution: Execution<'_>,
    ) -> anyhow::Result<()> {
        match execution {
            Execution::StagedRow => {
                // A deferred trigger would otherwise run at COMMIT, after the
                // check below, and a function replaced while an earlier one
                // held the commit would run unchecked. The step commits right
                // after, so settling here refuses nothing the commit would
                // have allowed (473).
                self.settle_writes(conn).await?;
                Ok(())
            }
            Execution::StagedGuardedDdl { previous } => {
                self.verify_retained(state, conn, previous).await?;
                after_statement(conn, admission, stmt, plan, changes, executed).await?;
                refuse_unsafe_definers(conn, &written_routines(changes), Absent::NotYetCreated)
                    .await
            }
            Execution::Transactional => {
                after_statement(conn, admission, stmt, plan, changes, executed).await
            }
        }
    }
    async fn staged_closing(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        self.state(state).retained = self.hold_retained(conn, changes, None).await?;
        Ok(())
    }
    /// Recover validation after a staged step's caller has rolled its transaction back.
    async fn recover_failed_validation(
        &self,
        conn: &mut Conn,
        statement: &pbps_dialect::Statement,
    ) -> Option<String> {
        let check = statement
            .scan_check
            .as_ref()
            .filter(|check| check.step == pbps_dialect::ScanCheckStep::Validate)?;
        match conn.driver() {
            // SQL Server does not emit a staged attach's temporary CHECK.
            Driver::Mssql => None,
            Driver::Postgres => Some(pbps_pg::staged::recover_failed_validation(conn, check).await),
        }
    }
    async fn capture_physical_defaults(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        at: Check<'_>,
    ) -> anyhow::Result<()> {
        self.state(state).defaults = pbps_pg::type_refs::declared::capture_defaults(
            conn,
            &plan.declared_type_pins,
            &plan.changes,
            at.within(),
        )
        .await?;
        Ok(())
    }
    async fn settle_writes(&self, conn: &mut Conn) -> anyhow::Result<bool> {
        settle_data_writes(conn).await
    }
    async fn hold_retained(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        authored: Option<&Schema>,
    ) -> anyhow::Result<Option<pbps_pg::retained::Held>> {
        hold_retained(conn, changes, authored).await
    }
    async fn verify_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        after: &Schema,
    ) -> anyhow::Result<()> {
        let transaction = self.state(state);
        transaction.verified = verify_retained(conn, transaction.retained.as_ref(), after).await?;
        Ok(())
    }
    async fn removed_name_matches(
        &self,
        conn: &mut Conn,
        wanted: &[TableName],
        observed: &[TableName],
    ) -> anyhow::Result<Vec<TableName>> {
        matching_table_names(conn, wanted, observed).await
    }
}

/// Preserved adapters have only a connection; the apply service already has
/// its concrete dialect and never selects again (DEC-2798.2).
pub(crate) fn selected(driver: Driver) -> crate::emission::SelectedDialect {
    match driver {
        Driver::Mssql => crate::emission::SelectedDialect::Mssql(pbps_mssql::Mssql),
        Driver::Postgres => crate::emission::SelectedDialect::Postgres(pbps_pg::Postgres::new()),
    }
}

// Bootstrap keeps its trusted-only admission, and staged keeps its own hooks.
// These adapters preserve the statement annotations' original reachability.
pub(crate) async fn before_statement(
    conn: &mut Conn,
    admission: &Admission,
    stmt: &pbps_dialect::Statement,
    guard: &DataWriteGuard,
    plan: Option<&pbps_model::SavedPlan>,
) -> anyhow::Result<()> {
    crate::engine::check_data_write(conn, admission, stmt, guard).await?;
    crate::engine::check_drop_requirement(conn, admission, stmt).await?;
    // The routines a table expression calls are judged just before the statement
    // that introduces it, with what the plan created earlier in place (DEC-2563.4).
    if let Some(change) = &stmt.introduces {
        pbps_pg::table_expressions::vet_change(
            conn,
            admission,
            pbps_pg::pins::Within::CallersTransaction,
            pbps_pg::table_expressions::Moment::Statement,
            change,
        )
        .await
        .and_then(pbps_pg::table_expressions::Vetted::admitted)
        .map_err(anyhow::Error::msg)?;
    }
    // A drop is vetted before it runs: what it would delete is judged while
    // it can still be refused, not after (DEC-2447.8).
    if let Some(extension) = &stmt.extension {
        crate::deploy::vet_before_extension(conn, extension, plan).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn after_statement(
    conn: &mut Conn,
    admission: &Admission,
    stmt: &pbps_dialect::Statement,
    plan: Option<&pbps_model::SavedPlan>,
    changes: &ChangeSet,
    executed: &[pbps_dialect::Statement],
) -> anyhow::Result<()> {
    if let Some(type_statement) = &stmt.declared_type {
        pbps_pg::type_refs::declared::vet(
            conn,
            type_statement,
            admission,
            plan.map_or(&[][..], |p| p.declared_type_pins.as_slice()),
            changes,
            executed,
        )
        .await?;
    }
    if let Some(extension) = &stmt.extension {
        crate::deploy::vet_after_extension(conn, extension, plan).await?;
    }
    Ok(())
}
