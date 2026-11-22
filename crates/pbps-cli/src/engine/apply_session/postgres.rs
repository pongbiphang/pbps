use super::*;

#[derive(Default)]
pub(crate) struct State {
    transaction: Transaction,
    retained: Option<pbps_pg::retained::Held>,
    verified: Option<pbps_pg::retained::Verified>,
}

impl ApplySession for pbps_pg::Postgres {
    type State = State;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction {
        &mut state.transaction
    }
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction {
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
    async fn history_name_occupants(
        &self,
        _conn: &mut Conn,
        _names: &[TableName],
    ) -> anyhow::Result<Option<Vec<pbps_mssql::catalog::NameOccupant>>> {
        Ok(None)
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
}
