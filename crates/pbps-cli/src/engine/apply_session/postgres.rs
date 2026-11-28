use super::*;

#[derive(Default)]
pub(crate) struct State {
    transaction: Transaction<pbps_pg::Postgres>,
    retained: Option<pbps_pg::retained::Held>,
    verified: Option<pbps_pg::retained::Verified>,
}

impl ApplySession for pbps_pg::Postgres {
    type State = State;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction<Self> {
        &mut state.transaction
    }
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction<Self> {
        &state.transaction
    }
    fn retained_comparison<'s>(
        &self,
        state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison> {
        state
            .verified
            .as_ref()
            .map(|verified| verified as &dyn pbps_dialect::RetainedComparison)
    }
    /// A staged attach's temporary CHECK is validated in a step of its own; when
    /// that step fails, what the rolled-back transaction held is read again to
    /// say why.
    async fn recover_failed_validation(
        &self,
        conn: &mut Conn,
        statement: &pbps_dialect::Statement,
    ) -> Option<String> {
        let check = statement
            .scan_check
            .as_ref()
            .filter(|check| check.step == pbps_dialect::ScanCheckStep::Validate)?;
        Some(pbps_pg::staged::recover_failed_validation(conn, check).await)
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
    async fn vet_introduced(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        change: &pbps_model::Change,
    ) -> anyhow::Result<()> {
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
        Ok(())
    }
    async fn vet_declared_type(
        &self,
        conn: &mut Conn,
        statement: &pbps_dialect::DeclaredTypeStatement,
        admission: &RoutineAdmission,
        pins: &[pbps_model::DeclaredTypePin],
        changes: &ChangeSet,
        executed: &[pbps_dialect::Statement],
    ) -> anyhow::Result<()> {
        pbps_pg::type_refs::declared::vet(conn, statement, admission, pins, changes, executed)
            .await?;
        Ok(())
    }
    async fn check_rehearsed(&self, conn: &mut Conn, changes: &ChangeSet) -> anyhow::Result<()> {
        Ok(pbps_pg::adoption::check(conn, changes).await?)
    }
    async fn hold_rehearsed(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        pinned: &pbps_model::Change,
    ) -> anyhow::Result<()> {
        Ok(pbps_pg::adoption::hold(conn, changes, pinned).await?)
    }
    /// Retention is a connected engine guard, never a second plan emitter.
    async fn hold_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        changes: &ChangeSet,
        authored: Option<&Schema>,
    ) -> anyhow::Result<()> {
        state.retained = if pbps_model::retained::expressions(changes).next().is_none() {
            None
        } else {
            anyhow::ensure!(
                conn.driver() == pbps_db::Driver::Postgres,
                "retained expressions require PostgreSQL"
            );
            Some(
                pbps_pg::retained::hold(conn, changes, authored)
                    .await
                    .map_err(anyhow::Error::msg)?,
            )
        };
        Ok(())
    }
    async fn verify_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        after: &Schema,
    ) -> anyhow::Result<()> {
        state.verified = match state.retained.as_ref() {
            Some(held) => Some(
                held.verify(conn, &pbps_pg::Postgres::new(), after)
                    .await
                    .map_err(anyhow::Error::msg)?,
            ),
            None => None,
        };
        Ok(())
    }
    async fn standing_scan_checks(
        &self,
        conn: &mut Conn,
        ids: &IdsFile,
    ) -> anyhow::Result<Vec<pbps_dialect::ScanCheck>> {
        Ok(pbps_pg::staged::standing_scan_checks(conn, &ids.tables).await?)
    }
    async fn drop_scan_checks(
        &self,
        conn: &mut Conn,
        checks: &[pbps_dialect::ScanCheck],
    ) -> anyhow::Result<()> {
        Ok(pbps_pg::staged::drop_scan_checks(conn, checks).await?)
    }
    async fn handover_catalog(
        &self,
        conn: &mut Conn,
    ) -> Result<Option<pbps_model::baseline_evidence::BaselineCatalog>, DbError> {
        pbps_pg::catalog::baseline_catalog(conn).await
    }
    async fn recording_absence(
        &self,
        _conn: &mut Conn,
        _missing: &[TableName],
        _missing_roles: &[pbps_model::RoleName],
    ) -> anyhow::Result<Vec<pbps_model::RoleName>> {
        // A role reads every row of `pg_class`, so no table is hidden from it. A
        // missing role is the cluster's, which a plan refuses rather than
        // creates, so taking its uid out of the state would change no plan.
        Ok(Vec::new())
    }
    async fn seal_prerequisites(
        &self,
        conn: &mut Conn,
        declared: &Schema,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<SealedPrerequisites> {
        let (pins, input_roots) = plan_session::postgres::seal_with_input_roots(
            conn,
            declared,
            changes,
            trusted,
            pin_routines,
        )
        .await?;
        Ok(SealedPrerequisites { pins, input_roots })
    }
    async fn recheck_prerequisites(
        &self,
        conn: &mut Conn,
        expected: &Prerequisites,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        pbps_pg::type_refs::check(conn, within, &expected.types, &[], changes).await?;
        pbps_pg::class_pins::check(conn, within, &expected.providers, changes).await?;
        plan_session::postgres::type_input_routines(
            conn,
            &expected.types,
            trusted,
            false,
            matches!(within, pbps_db::TransactionOwner::CallersTransaction),
        )
        .await?;
        Ok(())
    }
    async fn permission_support(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> anyhow::Result<()> {
        plan_session::postgres::permission_support(conn, changes, ids)
            .await
            .map(|_| ())
    }
    /// PostgreSQL has no system-versioned table.
    async fn refuse_unsupported_temporal(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn owned_targets(
        &self,
        changes: &ChangeSet,
        ids: &IdsFile,
        owners: &BTreeMap<pbps_model::GrantTarget, pbps_model::RoleName>,
        session_role: &pbps_model::RoleName,
    ) -> anyhow::Result<()> {
        plan_session::postgres::owned_targets(changes, ids, owners, session_role).map(|_| ())
    }
    fn unrevocable_grants(
        &self,
        changes: &ChangeSet,
        ids: &IdsFile,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()> {
        plan_session::postgres::unrevocable_grants(changes, ids, unrevocable).map(|_| ())
    }
    async fn refuse_unsafe_definers(
        &self,
        conn: &mut Conn,
        routines: &[(pbps_model::ModuleId, pbps_model::ModuleKind)],
        absent: Absent,
    ) -> anyhow::Result<()> {
        use pbps_pg::modules::DefinerPath;
        if routines.is_empty() {
            return Ok(());
        }
        let mut unsafe_ = Vec::new();
        for (id, kind) in routines {
            match pbps_pg::modules::definer_path(conn, id, *kind).await? {
                DefinerPath::Safe => {}
                DefinerPath::Absent if absent == Absent::NotYetCreated => {}
                DefinerPath::Absent => anyhow::bail!(
                    "{id}: this plan writes the routine, and the catalog holds no routine by \
                     that identity after its statements ran",
                    id = id.shown()
                ),
                DefinerPath::Unsafe(why) => unsafe_.push(format!("{id}: {why}", id = id.shown())),
            }
        }
        if !unsafe_.is_empty() {
            anyhow::bail!(
                "a SECURITY DEFINER routine this plan writes is not in PostgreSQL's safe form:\n  {}\n\
                 Give each one `SET search_path = <its schemas>, pg_temp` in its declaration, with \
                 pg_temp last, and plan again (DEC-322.1). Nothing has been applied; the \
                 transaction was rolled back.",
                unsafe_.join("\n  ")
            );
        }
        Ok(())
    }
    /// PostgreSQL has no system-versioned table, so no history name can be taken.
    async fn taken_history_names(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _names: &[TableName],
    ) -> anyhow::Result<Vec<TakenHistoryName>> {
        Ok(Vec::new())
    }
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        plan_session::postgres::require_transactional_rebuilds(changes, staged)
    }
    /// Every rename in a plan, as the objects they are renamed *from* — the name
    /// the catalog still knows them by. A module rename is a drop plus a create,
    /// and only SQL Server asks the drop side through the rename-impact query.
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        pbps_pg::impact::rename_targets(names)
    }
    async fn generation_support(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> anyhow::Result<ConnectedCheck> {
        plan_session::postgres::generation_support(conn, changes, ids).await
    }
    async fn external_roles(
        &self,
        conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        plan_session::postgres::external_role_renames(conn, recorded, declared).await
    }
    async fn drop_blockers(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        plan_session::postgres::check_drop_blockers(
            conn,
            &plan.changes,
            &plan.ids,
            stored,
            stored_ids,
        )
        .await
        .map(|_| ())
    }
    async fn preflight_limits(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> Result<EditionVerdict, anyhow::Error> {
        plan_session::postgres::refuse_unlogged_partition_referencers(
            conn,
            &plan.changes,
            &plan.ids,
            plan.mode.is_staged(),
        )
        .await?;
        // A publication can list the table between the plan and this apply (#1489).
        plan_session::postgres::refuse_published_unlogged(conn, &plan.changes, &plan.ids).await?;
        Ok(plan_session::postgres::edition_verdict())
    }
    /// PostgreSQL declares no column collation (`validate` refuses the field),
    /// so there is no default to compare.
    async fn baseline_collation(&self, _conn: &mut Conn) -> Result<Option<String>, DbError> {
        Ok(None)
    }
    async fn table_sites(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &[pbps_model::PlannedChange],
    ) -> bool {
        let mut table_unjudged = false;
        for planned in changes {
            match vet_table_expressions(
                conn,
                admission,
                pbps_db::TransactionOwner::OwnTransaction,
                pbps_pg::table_expressions::Moment::Early,
                &planned.change,
            )
            .await
            {
                Ok(pbps_pg::table_expressions::Vetted::Admitted) => {}
                // Not judged, however it came about: refused, or unbound here.
                Ok(pbps_pg::table_expressions::Vetted::Unjudged(_)) | Err(_) => {
                    table_unjudged |= pbps_pg::table_expressions::probed(&planned.change);
                }
            }
        }
        table_unjudged
    }
    /// PostgreSQL requires a pin for every declared type and compares each with
    /// the database.
    async fn declared_type_replay(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<()> {
        let Approved {
            project,
            plan,
            managed,
            ..
        } = *approved;
        crate::pins::require_declared_types(
            &managed.declared_types,
            &plan.declared_type_pins,
            &plan.changes,
        )?;
        pbps_pg::type_refs::declared::check(
            conn,
            &plan.declared_type_pins,
            &crate::pins::applying(project, plan),
            at.within(),
            matches!(
                at,
                Check::BeforeRecording { .. }
                    | Check::BeforeCommit { .. }
                    | Check::BeforeClosing { .. }
            ),
            &plan.changes,
            at.executed(),
        )
        .await?;
        Ok(())
    }
    /// The routines are read from `pg_proc`.
    async fn selected_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
        roots: &[i64],
    ) -> Result<(pbps_dialect::routines::Read, Vec<pbps_model::TypeRoutine>), DbError> {
        pbps_pg::pins::selected(conn, at.within(), roots).await
    }
    /// The routines are read from `pg_proc`.
    async fn replaceable_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError> {
        pbps_pg::pins::read(conn, at.within()).await.map(Some)
    }
    async fn requirement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        table: &TableName,
        predicate: &str,
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnName>> {
        pbps_pg::input_path::vet_requirement(
            conn,
            admission,
            pbps_db::TransactionOwner::OwnTransaction,
            table,
            predicate,
        )
        .await
        .map_err(anyhow::Error::msg)
    }
    async fn check_drop_requirement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
    ) -> Result<(), DbError> {
        let Some(guard) = &statement.drop_guard else {
            return Ok(());
        };
        pbps_pg::drop_requirements::check(conn, admission, guard)
            .await
            .map_err(DbError::Refused)
    }
    /// PostgreSQL's roles belong to the cluster, so they are asked.
    async fn absent_cluster_roles(
        &self,
        conn: &mut Conn,
        named: &BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<Vec<pbps_model::RoleName>> {
        Ok(pbps_pg::roles::missing_roles(conn, &named.iter().collect()).await?)
    }
    fn refuse_missing_cluster_roles(&self, missing: &[pbps_model::RoleName]) -> anyhow::Result<()> {
        if !missing.is_empty() {
            anyhow::bail!(
                "{}",
                missing
                    .iter()
                    .map(|name| pbps_pg::roles::refuse_missing(name).to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        Ok(())
    }
    async fn extension_prerequisites(
        &self,
        conn: &mut Conn,
        required: &BTreeMap<pbps_model::ObjectLocalName, pbps_model::Extension>,
    ) -> anyhow::Result<Vec<String>> {
        Ok(pbps_pg::resolver::scratch::extension_prerequisites(conn, required).await?)
    }
    async fn role_drop_facts(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<(
        BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>,
        BTreeMap<pbps_model::RoleName, Vec<String>>,
    )> {
        // A PostgreSQL role is the cluster's, so a plan for it drops none.
        Err(roles_are_the_clusters("reading the role memberships"))
            .context("cannot read the role memberships")
    }
    async fn alike_role_names(
        &self,
        _conn: &mut Conn,
        _names: &[&str],
    ) -> anyhow::Result<Vec<(String, String)>> {
        Err(roles_are_the_clusters("comparing the declared role names"))
    }
    async fn principal_name_holders(
        &self,
        _conn: &mut Conn,
        _names: &[&str],
        _vacated: &[&str],
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        Err(roles_are_the_clusters(
            "reading which principals hold a name",
        ))
    }
    async fn rename_report(
        &self,
        conn: &mut Conn,
        target: &RenameTarget,
    ) -> Result<ImpactReport, ImpactError> {
        pbps_pg::impact::rename_impact(conn, target).await
    }
    async fn conversions(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &[pbps_model::PlannedChange],
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnRef>> {
        plan_session::postgres::vet_conversions(conn, admission, changes).await
    }
    async fn removed_name_matches(
        &self,
        _conn: &mut Conn,
        wanted: &[TableName],
        observed: &[TableName],
    ) -> anyhow::Result<Vec<TableName>> {
        Ok(pbps_pg::catalog::matching_table_names(wanted, observed))
    }
    async fn rebuild_window(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        before: pbps_diff::Side<'_>,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        pbps_pg::rebuild::window(conn, admission, before, changes).await?;
        Ok(())
    }
    async fn check_module_rebuilds(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        after: bool,
    ) -> anyhow::Result<ConnectedCheck> {
        plan_session::postgres::check_module_rebuilds(conn, changes, after).await
    }
    async fn check_enum_rebuild_census(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        stored: &IdsFile,
        resuming: bool,
    ) -> anyhow::Result<()> {
        if !changes.changes.iter().any(|p| {
            matches!(
                p.change,
                pbps_model::Change::TypeValue(pbps_model::TypeValueChange::EnumRebuild { .. })
            )
        }) {
            return Ok(());
        }
        pbps_pg::type_rebuild::census(conn, changes, None, stored, resuming)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
    async fn check_domain_alterations(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        stored: &IdsFile,
    ) -> anyhow::Result<Option<ConnectedCheck>> {
        plan_session::postgres::check_domain_alterations(conn, changes, stored).await
    }
    async fn refuse_policy_dependents<D: ?Sized>(
        &self,
        conn: &mut Conn,
        plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
        ids: &IdsFile,
        managed: &Schema,
    ) -> anyhow::Result<()> {
        let changes = pbps_dialect::PlanPayload::changes(plan);
        let found = policy_dependents(conn, changes, ids, managed).await?;
        crate::row_security::check_policy_releases(plan, ids, &found)?;
        Ok(())
    }
    async fn check_module_dependency_facts(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        names: pbps_model::plan_names::PlanNames<'_>,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let found = plan_session::postgres::module_dependents(conn, changes).await?;
        refuse_unaccounted_dependents("PostgreSQL", changes, &found, names, stored, stored_ids)
    }
    async fn replay_type_pins(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        trusted: &BTreeSet<pbps_model::RoleName>,
        within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        pbps_pg::type_refs::trusted_roles(conn, trusted).await?;
        pbps_pg::type_refs::check(
            conn,
            within,
            &plan.type_pins,
            &plan.declared_type_pins,
            &plan.changes,
        )
        .await?;
        pbps_pg::class_pins::check(conn, within, &plan.provider_pins, &plan.changes).await?;
        Ok(())
    }
    async fn reviewed_input_roots(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<BTreeSet<i64>> {
        plan_session::postgres::type_input_routines(
            conn,
            &approved.plan.type_pins,
            &approved.project.config.trusted_roles,
            approved.plan.pin_type_routines,
            matches!(at.within(), pbps_db::TransactionOwner::CallersTransaction),
        )
        .await
    }
}
