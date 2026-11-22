//! Apply's engine work at its existing positions (DEC-2798.2, DEC-2798.4).
//!
//! Positions keep source order and end wherever neutral code can refuse,
//! print, add a finding or change an input. Reads repeated today stay repeated;
//! a pin reader receives the original Check, never a guessed closing phase.
//! The service selects the concrete dialect with with_plan_dialect! before
//! opening one default state per attempt. Existing connected adapters remain
//! available to planning until its own route moves.
//! Staged apply, snapshot, baseline and bootstrap use the positions.
use super::*;
use crate::pins::{Approved, Check};
use anyhow::Context as _;
use pbps_dialect::routines::RoutineAdmission;

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

/// What a bootstrap's sealing step read of the prerequisites its declarations
/// rely on: the pins to replay, and the roots of the routines a declared type's
/// input path would run. Facts only: whether a root is refused is neutral code's
/// decision, because bootstrap's gate is stronger than planning's, which defers
/// the same roots to review.
pub(crate) struct SealedPrerequisites {
    pub pins: Prerequisites,
    pub input_roots: BTreeSet<i64>,
}

/// Evidence lives for this attempt, while closing verification is always fresh.
#[derive(Default)]
pub(crate) struct Transaction {
    pub data_guard: DataWriteGuard,
    pub defaults: BTreeMap<pbps_model::Uid, pbps_model::ManagedType>,
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
    fn retained_comparison<'s>(
        &self,
        state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison>;
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()>;
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget>;

    async fn connection_support(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> anyhow::Result<()> {
        self.permission_support(conn, &plan.changes, &plan.ids)
            .await?;
        generation_support(conn, &plan.changes, &plan.ids).await?;
        Ok(())
    }
    async fn history_before_intent(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        label: &str,
    ) -> anyhow::Result<()> {
        crate::deploy::refuse_taken_history_names(self, conn, changes, label).await?;
        // Nothing binds a saved plan to a server: asked again here (#1502).
        self.refuse_unsupported_temporal(conn, changes).await
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
        plan: &pbps_model::SavedPlan,
        scoped: &pbps_diff::Scoped,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()> {
        self.owned_targets(
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
        self.unrevocable_grants(&plan.changes, &plan.ids, unrevocable)?;
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
        admission: &RoutineAdmission,
        changes: &[pbps_model::PlannedChange],
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnRef>> {
        vet_conversions(conn, admission, changes).await
    }
    async fn requirement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        table: &TableName,
        predicate: &str,
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnName>> {
        vet_drop_requirement(conn, admission, table, predicate).await
    }
    async fn table_sites(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
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
        self.hold_retained(
            state,
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
        self.refuse_unsafe_definers(conn, &written_routines(&plan.changes), Absent::Refused)
            .await?;
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
            matches!(at.within(), pbps_db::TransactionOwner::CallersTransaction),
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
            matches!(at.within(), pbps_db::TransactionOwner::CallersTransaction),
        )
        .await
    }
    async fn selected_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
        roots: &[i64],
    ) -> Result<(pbps_dialect::routines::Read, Vec<pbps_model::TypeRoutine>), DbError> {
        pbps_pg::pins::selected(conn, at.within(), roots).await
    }
    async fn replaceable_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError> {
        read_pinned_routines(conn, at.within()).await
    }
    async fn before_transactional_statement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
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
            self.hold_retained(
                state,
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
                    pbps_db::TransactionOwner::CallersTransaction,
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
        admission: &RoutineAdmission,
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
                self.refuse_unsafe_definers(conn, &written_routines(changes), Absent::NotYetCreated)
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
        self.hold_retained(state, conn, changes, None).await?;
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
        owner: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        self.state(state).defaults = pbps_pg::type_refs::declared::capture_defaults(
            conn,
            &plan.declared_type_pins,
            &plan.changes,
            owner,
        )
        .await?;
        Ok(())
    }
    async fn settle_writes(&self, conn: &mut Conn) -> anyhow::Result<bool> {
        settle_data_writes(conn).await
    }
    async fn hold_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        changes: &ChangeSet,
        authored: Option<&Schema>,
    ) -> anyhow::Result<()>;
    async fn verify_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        after: &Schema,
    ) -> anyhow::Result<()>;
    /// What the database holds, for a recording that takes missing objects out
    /// of the state (#1600, #1606): proves the tables the catalog read did not
    /// return are absent and not hidden, then returns the roles it proves
    /// absent. Neutral code narrows the held identities with the returned
    /// roles afterwards.
    ///
    /// The tables come first, as the recordings asked them: a refusal about a
    /// table is the one an operator reads when both a table and a role are
    /// hidden from the login.
    ///
    /// Required, and not one shared body: the two engines share nothing but the
    /// order. Whether a missing object can be hidden from the login is a fact
    /// about the engine's catalog views, and an answer from an engine that was
    /// never asked would record a hidden table as one that is gone.
    async fn recording_absence(
        &self,
        conn: &mut Conn,
        missing: &[TableName],
        missing_roles: &[pbps_model::RoleName],
    ) -> anyhow::Result<Vec<pbps_model::RoleName>>;
    /// The temporary CHECKs a staged attach left on its tables, which
    /// `baseline` takes out before it takes the database as it stands (#1638,
    /// DEC-1638.1).
    ///
    /// Only under the exact name a table's identity in the project's ids file
    /// gives: never by a prefix. Not from the ledger: `baseline` is the remedy
    /// for an entry this build cannot read (DECISIONS 203), and a run that dies
    /// between the `ADD` and the `VALIDATE`'s checkpoint records nothing, so no
    /// entry's kind or ids can be asked first. No declared constraint can hold
    /// the name, the database cannot be read while a `NOT VALID` one stands, and
    /// a validated one is pbps's own.
    ///
    /// Required: whether an engine's staged plans write such a CHECK is a fact
    /// about the engine, and an empty answer from an engine that was never
    /// asked would let `baseline` read a database with one standing.
    async fn standing_scan_checks(
        &self,
        conn: &mut Conn,
        ids: &IdsFile,
    ) -> anyhow::Result<Vec<pbps_dialect::ScanCheck>>;
    /// Drops `checks`, inside the transaction `baseline` records in (#2127).
    /// Neutral code decides there is something to drop and names each for the
    /// record.
    async fn drop_scan_checks(
        &self,
        conn: &mut Conn,
        checks: &[pbps_dialect::ScanCheck],
    ) -> anyhow::Result<()>;
    /// The opaque catalog evidence `baseline` asks before it hands a held
    /// partition over (DEC-1852.1, ADR-0018): whether the engine records one
    /// at all, and what it read. Neutral code keeps the refusal, and keeps the
    /// retained parent-and-bound refusal when the answer is `None`.
    ///
    /// Required, for the reason of [`Self::standing_scan_checks`]: `None` is
    /// the engine's statement that it records no such evidence.
    async fn handover_catalog(
        &self,
        conn: &mut Conn,
    ) -> Result<Option<pbps_model::baseline_evidence::BaselineCatalog>, DbError>;
    /// Seals the type and provider prerequisites of `declared` and reads the
    /// roots of the routines a declared type's input path would run, as
    /// bootstrap does at its probe and on its execution connection. One
    /// implementation for the sealing and the read, so the two cannot disagree
    /// about what was sealed; the returned facts are bootstrap's to refuse.
    ///
    /// Required: whether an engine has type prerequisites at all is a fact about
    /// it, and an empty answer from an engine that was never asked would let a
    /// bootstrap build over types nothing sealed.
    async fn seal_prerequisites(
        &self,
        conn: &mut Conn,
        declared: &Schema,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<SealedPrerequisites>;
    /// Reads the prerequisites again and refuses what changed since `expected`
    /// was sealed: before the transaction begins, when it opens, before every
    /// statement and before the recording. `within` says who owns the
    /// transaction the reads run in.
    ///
    /// Required, for the reason of [`Self::seal_prerequisites`].
    async fn recheck_prerequisites(
        &self,
        conn: &mut Conn,
        expected: &Prerequisites,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()>;
    /// Refuses a plan the server's version or the deployer's ownership cannot
    /// carry (a permission the server lacks, a row-security table the deployer
    /// does not own), before the first statement. Asked alone by bootstrap and
    /// beside the generation support by a saved plan's connection.
    ///
    /// Required: an engine that was never asked must not read as one that has
    /// nothing to refuse.
    async fn permission_support(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> anyhow::Result<()>;
    /// Refuses what this server cannot take of a system-versioned table, before
    /// the first statement (#1502). A plan with nothing of the kind asks
    /// nothing. Required, for the reason of [`Self::permission_support`].
    async fn refuse_unsupported_temporal(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<()>;
    /// Refuses a grant on what the connected role will own (#261), which the
    /// engine writes only as the owner's default entry. Apart from
    /// [`Self::unrevocable_grants`] because bootstrap asks only this one: it
    /// revokes nothing. Required, for the reason of [`Self::permission_support`].
    fn owned_targets(
        &self,
        changes: &ChangeSet,
        ids: &IdsFile,
        owners: &BTreeMap<pbps_model::GrantTarget, pbps_model::RoleName>,
        session_role: &pbps_model::RoleName,
    ) -> anyhow::Result<()>;
    /// Refuses a plan that revokes a grant no `REVOKE` from this connection
    /// could take away (#251). Required, for the reason of
    /// [`Self::permission_support`].
    fn unrevocable_grants(
        &self,
        changes: &ChangeSet,
        ids: &IdsFile,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()>;
    /// Refuses a `SECURITY DEFINER` routine the plan wrote that is not in the
    /// engine's safe form (#322), read back inside the transaction that wrote
    /// it. Apart from the module-rebuild and external-role questions
    /// [`Self::after_statements`] asks beside it, because bootstrap asks only
    /// this one. Required, for the reason of [`Self::permission_support`].
    async fn refuse_unsafe_definers(
        &self,
        conn: &mut Conn,
        routines: &[(pbps_model::ModuleId, pbps_model::ModuleKind)],
        absent: Absent,
    ) -> anyhow::Result<()>;
    /// The objects the database already holds under the history-table names a
    /// plan would create (#1176): `None` when the engine has no system-versioned
    /// table, so no name can be taken. Neutral code keeps the refusal.
    /// Required, so an engine never asked is not read as one with no occupant.
    async fn history_name_occupants(
        &self,
        conn: &mut Conn,
        names: &[TableName],
    ) -> anyhow::Result<Option<Vec<pbps_mssql::catalog::NameOccupant>>>;
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
    admission: &RoutineAdmission,
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
            pbps_db::TransactionOwner::CallersTransaction,
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
    admission: &RoutineAdmission,
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
