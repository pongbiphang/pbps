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
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        require_transactional_rebuilds(Driver::Mssql, changes, staged)
    }
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        super::super::rename_targets(Driver::Mssql, names)
    }
}
