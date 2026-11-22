use super::*;

impl ApplySession for pbps_mssql::Mssql {
    type State = Transaction;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction {
        state
    }
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction {
        state
    }
    fn retained_comparison<'s>(
        &self,
        _state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison> {
        None
    }
    async fn hold_retained(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _authored: Option<&Schema>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            pbps_model::retained::expressions(changes).next().is_none(),
            "retained expressions require PostgreSQL"
        );
        Ok(())
    }
    async fn verify_retained(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        _after: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server's staged plans write no temporary CHECK.
    async fn standing_scan_checks(
        &self,
        _conn: &mut Conn,
        _ids: &IdsFile,
    ) -> anyhow::Result<Vec<pbps_dialect::ScanCheck>> {
        Ok(Vec::new())
    }
    async fn drop_scan_checks(
        &self,
        _conn: &mut Conn,
        checks: &[pbps_dialect::ScanCheck],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            checks.is_empty(),
            "SQL Server writes no temporary CHECK for a staged attach, so there is none to drop"
        );
        Ok(())
    }
    /// SQL Server records no catalog evidence for a baseline.
    async fn handover_catalog(
        &self,
        _conn: &mut Conn,
    ) -> Result<Option<pbps_model::baseline_evidence::BaselineCatalog>, DbError> {
        Ok(None)
    }
    async fn recording_absence(
        &self,
        conn: &mut Conn,
        missing: &[TableName],
        missing_roles: &[pbps_model::RoleName],
    ) -> anyhow::Result<Vec<pbps_model::RoleName>> {
        prove_tables_absent(conn, missing).await?;
        absent_roles(conn, missing_roles).await
    }
    async fn seal_prerequisites(
        &self,
        _conn: &mut Conn,
        _declared: &Schema,
        _changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<SealedPrerequisites> {
        let (pins, input_roots) =
            plan_session::mssql::seal_with_input_roots(trusted, pin_routines)?;
        Ok(SealedPrerequisites { pins, input_roots })
    }
    async fn recheck_prerequisites(
        &self,
        _conn: &mut Conn,
        expected: &Prerequisites,
        _changes: &ChangeSet,
        _trusted: &BTreeSet<pbps_model::RoleName>,
        _within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            expected.types.is_empty() && expected.providers.is_empty(),
            "SQL Server does not support type references"
        );
        Ok(())
    }
    /// SQL Server permissions are validated by its dialect.
    async fn permission_support(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _ids: &IdsFile,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn refuse_unsupported_temporal(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        match temporal_refusal(&plan_session::mssql::unsupported_temporal(conn, changes).await?) {
            Some(refusal) => Err(anyhow::anyhow!(refusal)),
            None => Ok(()),
        }
    }
    /// The owner's default entry is a PostgreSQL notion.
    fn owned_targets(
        &self,
        _changes: &ChangeSet,
        _ids: &IdsFile,
        _owners: &BTreeMap<pbps_model::GrantTarget, pbps_model::RoleName>,
        _session_role: &pbps_model::RoleName,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server selects no grantor a `REVOKE` could miss.
    fn unrevocable_grants(
        &self,
        _changes: &ChangeSet,
        _ids: &IdsFile,
        _unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// `SECURITY DEFINER` and its `search_path` are PostgreSQL's.
    async fn refuse_unsafe_definers(
        &self,
        _conn: &mut Conn,
        _routines: &[(pbps_model::ModuleId, pbps_model::ModuleKind)],
        _absent: Absent,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server takes an existing table whose columns match as the history,
    /// rows and all, where any other `CREATE` would be refused (measured on
    /// 17.0), so it is asked.
    async fn history_name_occupants(
        &self,
        conn: &mut Conn,
        names: &[TableName],
    ) -> anyhow::Result<Option<Vec<pbps_mssql::catalog::NameOccupant>>> {
        Ok(Some(
            pbps_mssql::catalog::object_name_occupants(conn, names, &[]).await?,
        ))
    }
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        plan_session::mssql::require_transactional_rebuilds(changes, staged)
    }
    /// Every rename in a plan, as the objects they are renamed *from* — the name
    /// the catalog still knows them by. A module rename is a drop plus a create,
    /// and SQL Server asks the drop side through the rename-impact query.
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        pbps_mssql::impact::rename_targets(names)
    }
    async fn generation_support(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _ids: &IdsFile,
    ) -> anyhow::Result<ConnectedCheck> {
        Ok(plan_session::mssql::generation_support())
    }
    async fn external_roles(
        &self,
        _conn: &mut Conn,
        _recorded: &IdsFile,
        _declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        Ok(plan_session::mssql::external_role_renames())
    }
    async fn drop_blockers(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        plan_session::mssql::check_drop_blockers(conn, &plan.changes, &plan.ids, stored, stored_ids)
            .await
            .map(|_| ())
    }
    async fn preflight_limits(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> Result<EditionVerdict, anyhow::Error> {
        // The edition, asked again. `plan --db` checked it, but nothing binds a
        // saved plan to an environment: the same file can be applied to a different
        // server, or to the same one after an edition change, and an `ONLINE = ON`
        // statement rejected halfway through an apply is precisely what the check
        // at plan time exists to prevent (ADR-0003).
        Ok(plan_session::mssql::edition_verdict(conn, &plan.changes).await?)
    }
    async fn baseline_collation(&self, conn: &mut Conn) -> Result<Option<String>, DbError> {
        pbps_mssql::catalog::database_collation(conn)
            .await
            .map(Some)
    }
    /// SQL Server binds no table expression this way, so none is left unjudged.
    async fn table_sites(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _changes: &[pbps_model::PlannedChange],
    ) -> bool {
        false
    }
    /// SQL Server has no declared-type pin, so there is nothing to check.
    async fn declared_type_replay(
        &self,
        _conn: &mut Conn,
        _approved: &Approved<'_>,
        _at: Check<'_>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server pins no routine (DEC-319.1).
    async fn replaceable_rows(
        &self,
        _conn: &mut Conn,
        _at: Check<'_>,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError> {
        Ok(None)
    }
    async fn requirement(
        &self,
        conn: &mut Conn,
        _admission: &RoutineAdmission,
        table: &TableName,
        predicate: &str,
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnName>> {
        Box::pin(pbps_mssql::drop_requirements::vet(conn, table, predicate))
            .await
            .map_err(anyhow::Error::msg)
    }
    async fn check_drop_requirement(
        &self,
        conn: &mut Conn,
        _admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
    ) -> Result<(), DbError> {
        let Some(guard) = &statement.drop_guard else {
            return Ok(());
        };
        Box::pin(pbps_mssql::drop_requirements::check(conn, guard))
            .await
            .map_err(DbError::Refused)
    }
    /// Missing database roles are created by the SQL Server typed plan, and no
    /// role of this engine belongs to a cluster.
    fn refuse_missing_cluster_roles(
        &self,
        _missing: &[pbps_model::RoleName],
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server's roles and users are the database's own, so there is no
    /// cluster to ask (DEC-2379.3).
    async fn absent_cluster_roles(
        &self,
        _conn: &mut Conn,
        _named: &BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<Vec<pbps_model::RoleName>> {
        Ok(Vec::new())
    }
    /// SQL Server has no packages: nothing is required of an empty set, and a
    /// declared one is refused by name rather than read as satisfied.
    async fn extension_prerequisites(
        &self,
        _conn: &mut Conn,
        required: &BTreeMap<pbps_model::ObjectLocalName, pbps_model::Extension>,
    ) -> anyhow::Result<Vec<String>> {
        if required.is_empty() {
            return Ok(Vec::new());
        }
        anyhow::bail!("PostgreSQL extension prerequisites are unsupported on SQL Server")
    }
    async fn role_drop_facts(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<(
        BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>,
        BTreeMap<pbps_model::RoleName, Vec<String>>,
    )> {
        let members = plan_session::mssql::role_members(conn)
            .await
            .context("cannot read the role memberships")?;
        let owned = pbps_mssql::catalog::role_owned_securables(conn)
            .await
            .context("cannot read what the roles own")?;
        Ok((members, owned))
    }
    async fn alike_role_names(
        &self,
        conn: &mut Conn,
        names: &[&str],
    ) -> anyhow::Result<Vec<(String, String)>> {
        Ok(pbps_mssql::catalog::names_alike(conn, names).await?)
    }
    async fn principal_name_holders(
        &self,
        conn: &mut Conn,
        names: &[&str],
        vacated: &[&str],
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        Ok(pbps_mssql::catalog::principals_holding(conn, names, vacated).await?)
    }
    async fn rename_report(
        &self,
        conn: &mut Conn,
        target: &RenameTarget,
    ) -> Result<ImpactReport, ImpactError> {
        pbps_mssql::impact::rename_impact(conn, target).await
    }
    /// SQL Server records no conversion expression to vet.
    async fn conversions(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _changes: &[pbps_model::PlannedChange],
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnRef>> {
        Ok(BTreeSet::new())
    }
    async fn removed_name_matches(
        &self,
        conn: &mut Conn,
        wanted: &[TableName],
        observed: &[TableName],
    ) -> anyhow::Result<Vec<TableName>> {
        Ok(pbps_mssql::catalog::matching_table_names(conn, wanted, observed).await?)
    }
    /// The lock-and-recertify window belongs to PostgreSQL's rebuild, which
    /// reads a dependency closure from its catalog; this engine has none.
    async fn rebuild_window(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _before: pbps_diff::Side<'_>,
        _changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server's `CREATE OR ALTER` preserves this state without a rebuild.
    async fn check_module_rebuilds(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _after: bool,
    ) -> anyhow::Result<ConnectedCheck> {
        Ok(plan_session::mssql::check_module_rebuilds())
    }
    /// SQL Server has no enum to rebuild.
    async fn check_enum_rebuild_census(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _stored: &IdsFile,
        _resuming: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// SQL Server has no domain.
    async fn check_domain_alterations(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _stored: &IdsFile,
    ) -> anyhow::Result<Option<ConnectedCheck>> {
        Ok(None)
    }
    /// Policy dependents are read from PostgreSQL's catalog bindings; this
    /// engine has no read of them to ask.
    async fn refuse_policy_dependents<D: ?Sized>(
        &self,
        _conn: &mut Conn,
        _plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
        _ids: &IdsFile,
        _managed: &Schema,
    ) -> anyhow::Result<()> {
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
        let found = plan_session::mssql::module_dependents(conn, changes).await?;
        refuse_unaccounted_dependents("SQL Server", changes, &found, names, stored, stored_ids)
    }
    /// SQL Server keeps its existing reference-data execution policy.
    async fn prepare_data_writes(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _changes: &ChangeSet,
        _baseline: &StateSnapshot,
        _planned_ids: &IdsFile,
    ) -> anyhow::Result<DataWriteGuard> {
        Ok(DataWriteGuard::default())
    }
    async fn check_data_write(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _statement: &pbps_dialect::Statement,
        _guard: &DataWriteGuard,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn settle_writes(&self, _conn: &mut Conn) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn replay_type_pins(
        &self,
        _conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        trusted: &BTreeSet<pbps_model::RoleName>,
        _within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            plan.type_pins.is_empty() && plan.provider_pins.is_empty() && trusted.is_empty(),
            "this SQL Server plan carries PostgreSQL type prerequisites"
        );
        Ok(())
    }
    async fn reviewed_input_roots(
        &self,
        _conn: &mut Conn,
        approved: &Approved<'_>,
        _at: Check<'_>,
    ) -> anyhow::Result<BTreeSet<i64>> {
        plan_session::mssql::type_input_routines(
            &approved.plan.type_pins,
            approved.plan.pin_type_routines,
        )
    }
}

/// Proves the tables a catalog read did not return are absent, not hidden,
/// before a state records them as missing (#1600). On SQL Server `sys.tables`
/// is filtered by metadata visibility, so a table under an effective `DENY
/// VIEW DEFINITION`, or in a schema this login cannot view, returns no row
/// and reads as missing. Recorded as missing, its uid would leave the state
/// and the table the managed set, silently. The proof is DEC-1192.1's.
async fn prove_tables_absent(conn: &mut Conn, missing: &[TableName]) -> anyhow::Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let mut schemas: Vec<String> = missing.iter().map(|t| t.schema.clone()).collect();
    schemas.sort();
    schemas.dedup();
    pbps_mssql::catalog::prove_schemas_visible(conn, &schemas)
        .await
        .map_err(|e| absence_unproven(missing, e))
}

/// The roles a recording may take out of the state: those the catalog read
/// did not return and the database proves it lacks (#1606). On SQL Server
/// `sys.database_principals` is filtered by metadata visibility, so a role
/// hidden from this login reads as missing; recorded as missing, its uid
/// would leave the state and the next plan create a role that exists. A name
/// the database still resolves refuses the command by name instead
/// (DEC-1606.1).
async fn absent_roles(
    conn: &mut Conn,
    missing: &[pbps_model::RoleName],
) -> anyhow::Result<Vec<pbps_model::RoleName>> {
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let held = pbps_mssql::catalog::held_principals(conn, missing).await?;
    if !held.is_empty() {
        anyhow::bail!(
            "cannot record role(s) {} as missing: the database holds a principal by that name \
             this login cannot see; record the state with a login that can see it, so a role \
             hidden from this one is not taken for one that is gone",
            held.iter()
                .map(|name| name.shown().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(missing.to_vec())
}
