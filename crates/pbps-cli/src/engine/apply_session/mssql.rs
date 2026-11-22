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
        require_transactional_rebuilds(Driver::Mssql, changes, staged)
    }
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        super::super::rename_targets(Driver::Mssql, names)
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
